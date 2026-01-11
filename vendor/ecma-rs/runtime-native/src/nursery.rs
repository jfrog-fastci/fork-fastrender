use std::io;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default nursery size used by [`NurserySpace::new_default`].
///
/// Note: the nursery is a bump-allocated young generation and is expected to be
/// reset wholesale after a minor collection, so it is typically sized in the
/// tens of MiB.
pub const DEFAULT_NURSERY_SIZE_BYTES: usize = 32 * 1024 * 1024;

/// Default size of a thread-local allocation buffer (TLAB).
///
/// A TLAB amortizes contention on the global bump pointer by handing each
/// thread a chunk of nursery memory, then letting it perform bump allocation on
/// that chunk without synchronization.
pub const TLAB_SIZE: usize = 32 * 1024;

const TLAB_ALIGN: usize = 16;

#[inline]
fn is_pow2(v: usize) -> bool {
    v != 0 && v.is_power_of_two()
}

#[inline]
fn align_up_usize(value: usize, align: usize) -> usize {
    debug_assert!(is_pow2(align));
    (value + (align - 1)) & !(align - 1)
}

#[cfg(unix)]
fn page_size() -> usize {
    // SAFETY: sysconf is thread-safe.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz <= 0 {
        4096
    } else {
        sz as usize
    }
}

#[cfg(not(unix))]
fn page_size() -> usize {
    4096
}

/// A contiguous young-generation (nursery) space backed by a single mmap region.
///
/// Allocation strategy:
/// - A global atomic bump pointer hands out chunks to threads (TLABs).
/// - Each thread bump-allocates within its own chunk without synchronization.
///
/// Reset strategy:
/// - The entire nursery is reset in one operation after a minor collection.
pub struct NurserySpace {
    start: *mut u8,
    size_bytes: usize,
    bump_offset: AtomicUsize,
    #[cfg(not(unix))]
    layout: std::alloc::Layout,
}

// SAFETY: The nursery region is a raw byte buffer shared between threads. The
// allocator metadata (`bump_offset`) is atomic. Callers are responsible for
// synchronizing access to the objects they place in the nursery; the allocator
// itself only guarantees non-overlapping allocation.
unsafe impl Send for NurserySpace {}
unsafe impl Sync for NurserySpace {}

impl NurserySpace {
    /// Reserve a nursery of `size_bytes` bytes.
    ///
    /// On Unix this uses `mmap` to reserve a contiguous region.
    pub fn new(size_bytes: usize) -> io::Result<Self> {
        if size_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nursery size must be non-zero",
            ));
        }

        let page = page_size();
        let size_bytes = align_up_usize(size_bytes, page);

        #[cfg(unix)]
        unsafe {
            let ptr = libc::mmap(
                ptr::null_mut(),
                size_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                start: ptr.cast::<u8>(),
                size_bytes,
                bump_offset: AtomicUsize::new(0),
            })
        }

        #[cfg(not(unix))]
        {
            let layout = std::alloc::Layout::from_size_align(size_bytes, page).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid nursery layout")
            })?;
            // SAFETY: layout is non-zero and well-formed.
            let start = unsafe { std::alloc::alloc_zeroed(layout) };
            if start.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "failed to allocate nursery",
                ));
            }

            Ok(Self {
                start,
                size_bytes,
                bump_offset: AtomicUsize::new(0),
                layout,
            })
        }
    }

    /// Construct a nursery using [`DEFAULT_NURSERY_SIZE_BYTES`].
    pub fn new_default() -> io::Result<Self> {
        Self::new(DEFAULT_NURSERY_SIZE_BYTES)
    }

    #[inline]
    pub fn start(&self) -> *mut u8 {
        self.start
    }

    #[inline]
    pub fn end(&self) -> *mut u8 {
        // SAFETY: `self.start..self.start.add(self.size_bytes)` is valid as the
        // region was reserved with that size.
        unsafe { self.start.add(self.size_bytes) }
    }

    #[inline]
    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    #[inline]
    pub fn contains(&self, ptr: *const u8) -> bool {
        let ptr = ptr as usize;
        ptr >= self.start as usize && ptr < self.end() as usize
    }

    /// Bytes allocated/reserved since the last reset.
    ///
    /// This tracks the global bump pointer, i.e. TLAB reservations plus any
    /// alignment padding, not exact per-object usage within each TLAB.
    #[inline]
    pub fn allocated_bytes(&self) -> usize {
        self.bump_offset.load(Ordering::Acquire)
    }

    /// Reset the nursery to its initial empty state.
    ///
    /// # Safety
    ///
    /// Must only be called during a stop-the-world (STW) phase where no threads
    /// are allocating from, or mutating, nursery memory. All outstanding TLABs
    /// become invalid after this call.
    pub unsafe fn reset(&self) {
        self.bump_offset.store(0, Ordering::Release);
    }

    #[inline]
    fn reserve_chunk(&self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(size != 0);
        debug_assert!(is_pow2(align));

        let align = align.max(1);

        loop {
            let current = self.bump_offset.load(Ordering::Relaxed);
            let aligned = align_up_usize(current, align);
            let new = aligned.checked_add(size)?;
            if new > self.size_bytes {
                return None;
            }
            if self
                .bump_offset
                .compare_exchange(current, new, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // SAFETY: `aligned + size <= size_bytes`.
                return Some(unsafe { self.start.add(aligned) });
            }
        }
    }

    /// Reserve a contiguous range in the nursery with the given alignment.
    ///
    /// The returned pointer is aligned to `align` and the region is `size` bytes
    /// long.
    #[inline]
    pub fn alloc_raw(&self, size: usize, align: usize) -> Option<*mut u8> {
        if size == 0 {
            return None;
        }
        if !is_pow2(align) {
            return None;
        }
        self.reserve_chunk(size, align)
    }

    /// Return a snapshot of nursery usage stats.
    #[inline]
    pub fn stats(&self) -> NurseryStats {
        NurseryStats {
            reserved_bytes: self.size_bytes,
            allocated_bytes: self.allocated_bytes(),
        }
    }
}

impl Drop for NurserySpace {
    fn drop(&mut self) {
        unsafe {
            #[cfg(unix)]
            {
                libc::munmap(self.start.cast::<libc::c_void>(), self.size_bytes);
            }

            #[cfg(not(unix))]
            {
                std::alloc::dealloc(self.start, self.layout);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NurseryStats {
    pub reserved_bytes: usize,
    pub allocated_bytes: usize,
}

/// Per-thread nursery allocator state (TLAB).
#[derive(Debug)]
pub struct ThreadNursery {
    pub cursor: *mut u8,
    pub limit: *mut u8,
}

impl Default for ThreadNursery {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadNursery {
    #[inline]
    pub const fn new() -> Self {
        Self {
            cursor: ptr::null_mut(),
            limit: ptr::null_mut(),
        }
    }

    #[inline(always)]
    fn try_alloc_fast(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(size != 0);
        debug_assert!(is_pow2(align));

        if self.cursor.is_null() {
            return None;
        }

        let cursor_addr = self.cursor as usize;
        let limit_addr = self.limit as usize;
        let aligned_addr = align_up_usize(cursor_addr, align);
        let new_cursor_addr = aligned_addr.checked_add(size)?;

        if new_cursor_addr <= limit_addr {
            let padding = aligned_addr - cursor_addr;
            let step = new_cursor_addr - cursor_addr;
            // SAFETY: `padding` and `step` keep the pointers within the current
            // TLAB reservation (`new_cursor <= limit` checked above).
            unsafe {
                let aligned_ptr = self.cursor.add(padding);
                self.cursor = self.cursor.add(step);
                Some(aligned_ptr)
            }
        } else {
            None
        }
    }

    /// Allocate `size` bytes with `align` alignment from the nursery.
    ///
    /// Returns `None` when the nursery is exhausted. This layer does not
    /// trigger GC.
    #[inline]
    pub fn alloc(
        &mut self,
        size: usize,
        align: usize,
        nursery: &NurserySpace,
    ) -> Option<*mut u8> {
        if size == 0 || !is_pow2(align) {
            return None;
        }

        // Fast path: bump within the thread-local TLAB.
        if let Some(ptr) = self.try_alloc_fast(size, align) {
            return Some(ptr);
        }

        // Large allocation: bypass the TLAB and allocate directly from the
        // global bump pointer, preserving any remaining TLAB space for future
        // small allocations.
        if size > TLAB_SIZE {
            return nursery.alloc_raw(size, align.max(TLAB_ALIGN));
        }

        // Slow path: refill the TLAB from the global bump pointer.
        let chunk_align = align.max(TLAB_ALIGN);
        let chunk_start = nursery.reserve_chunk(TLAB_SIZE, chunk_align)?;
        self.cursor = chunk_start;
        // SAFETY: `chunk_start` points to `TLAB_SIZE` bytes in the nursery.
        self.limit = unsafe { chunk_start.add(TLAB_SIZE) };

        // This allocation must now succeed: `chunk_start` is aligned to at
        // least `align`, and the chunk is at least `size` bytes long.
        self.try_alloc_fast(size, align)
    }

    /// Clear the current TLAB.
    ///
    /// Useful after a nursery reset; existing cursor/limit ranges become
    /// invalid once the nursery is reset.
    #[inline]
    pub fn clear(&mut self) {
        self.cursor = ptr::null_mut();
        self.limit = ptr::null_mut();
    }
}
