use crate::gc::ObjHeader;
use std::ptr::NonNull;

#[cfg(unix)]
use std::ffi::c_void;

#[derive(Debug)]
struct LosEntry {
  base: NonNull<u8>,
  mmap_size: usize,
  obj_size: usize,
}

/// Large Object Space (LOS).
///
/// LOS allocations are:
/// - mmap-backed (1 mapping per object)
/// - non-moving (good for FFI / pinning)
/// - traced and reclaimed via mark/sweep
///
/// The caller (GC) is responsible for initializing the object header and updating mark bits.
/// Unmarked objects are unmapped during [`LargeObjectSpace::sweep`].
pub(crate) struct LargeObjectSpace {
  entries: Vec<LosEntry>,
}

impl Default for LargeObjectSpace {
  fn default() -> Self {
    Self::new()
  }
}

impl LargeObjectSpace {
  pub fn new() -> Self {
    Self { entries: Vec::new() }
  }

  /// Allocates an uninitialized object of `size` bytes (including the [`ObjHeader`]).
  ///
  /// Returns a pointer to the start of the object header.
  pub(crate) fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
    debug_assert!(align.is_power_of_two());
    debug_assert!(align <= page_size());
    debug_assert!(size > 0);

    let mmap_size = round_up_to_page_size(size);

    let base_ptr = unsafe { os_alloc(mmap_size) };
    let base = NonNull::new(base_ptr).expect("mmap returned null");

    self.entries.push(LosEntry {
      base,
      mmap_size,
      obj_size: size,
    });
    base.as_ptr()
  }

  /// Iterates over all currently-tracked LOS objects.
  ///
  /// The callback receives `(object_ptr, object_size)`.
  pub(crate) fn for_each_object(&self, mut f: impl FnMut(*mut u8, usize)) {
    for entry in &self.entries {
      f(entry.base.as_ptr(), entry.obj_size);
    }
  }

  /// Sweeps the LOS, freeing (munmap'ing) any object that is not marked for `current_epoch`.
  ///
  /// Returns the number of bytes unmapped.
  pub(crate) fn sweep(&mut self, current_epoch: u8) -> usize {
    let mut freed = 0usize;
    self.entries.retain(|entry| unsafe {
      let hdr = &*(entry.base.as_ptr() as *const ObjHeader);
      if hdr.is_marked(current_epoch) {
        true
      } else {
        os_free(entry.base.as_ptr(), entry.mmap_size);
        freed += entry.mmap_size;
        false
      }
    });
    freed
  }

  pub(crate) fn live_bytes(&self, current_epoch: u8) -> usize {
    let mut live = 0usize;
    self.for_each_object(|obj, size| unsafe {
      let hdr = &*(obj as *const ObjHeader);
      if hdr.is_marked(current_epoch) {
        live += size;
      }
    });
    live
  }

  pub(crate) fn contains(&self, ptr: *mut u8) -> bool {
    let mut found = false;
    self.for_each_object(|obj, _size| {
      if obj == ptr {
        found = true;
      }
    });
    found
  }

  pub(crate) fn object_count(&self) -> usize {
    self.entries.len()
  }

  pub(crate) fn committed_bytes(&self) -> usize {
    self.entries.iter().map(|e| e.mmap_size).sum()
  }
}

impl Drop for LargeObjectSpace {
  fn drop(&mut self) {
    for entry in self.entries.drain(..) {
      unsafe {
        os_free(entry.base.as_ptr(), entry.mmap_size);
      }
    }
  }
}

fn round_up_to_page_size(size: usize) -> usize {
  let page = page_size();
  let rem = size % page;
  if rem == 0 {
    size
  } else {
    size + (page - rem)
  }
}

fn page_size() -> usize {
  #[cfg(unix)]
  unsafe {
    let ps = libc::sysconf(libc::_SC_PAGESIZE);
    if ps <= 0 {
      4096
    } else {
      ps as usize
    }
  }

  #[cfg(not(unix))]
  {
    4096
  }
}

#[cfg(unix)]
unsafe fn os_alloc(size: usize) -> *mut u8 {
  let ptr = libc::mmap(
    std::ptr::null_mut(),
    size,
    libc::PROT_READ | libc::PROT_WRITE,
    libc::MAP_PRIVATE | libc::MAP_ANON,
    -1,
    0,
  );
  if ptr == libc::MAP_FAILED {
    panic!("mmap failed: {}", std::io::Error::last_os_error());
  }
  ptr as *mut u8
}

#[cfg(unix)]
unsafe fn os_free(ptr: *mut u8, size: usize) {
  let rc = libc::munmap(ptr as *mut c_void, size);
  if rc != 0 {
    panic!("munmap failed: {}", std::io::Error::last_os_error());
  }
}

#[cfg(not(unix))]
unsafe fn os_alloc(size: usize) -> *mut u8 {
  let layout = std::alloc::Layout::from_size_align(size, 4096).expect("invalid layout");
  let ptr = std::alloc::alloc(layout);
  if ptr.is_null() {
    std::alloc::handle_alloc_error(layout);
  }
  ptr
}

#[cfg(not(unix))]
unsafe fn os_free(ptr: *mut u8, size: usize) {
  let layout = std::alloc::Layout::from_size_align(size, 4096).expect("invalid layout");
  std::alloc::dealloc(ptr, layout);
}
