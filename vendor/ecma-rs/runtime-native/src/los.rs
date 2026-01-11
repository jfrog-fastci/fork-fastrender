use std::ptr::NonNull;

#[cfg(unix)]
use std::ffi::c_void;

#[derive(Debug)]
struct LosEntry {
  map_base: NonNull<u8>,
  obj_base: NonNull<u8>,
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
    assert!(
      align != 0 && align.is_power_of_two(),
      "LOS alloc alignment must be a non-zero power of two"
    );
    debug_assert!(size > 0);

    let needed = size.checked_add(align - 1).expect("LOS alloc size overflow");
    let mmap_size = round_up_to_page_size(needed);

    let map_base_ptr = unsafe { os_alloc(mmap_size) };
    let map_base = NonNull::new(map_base_ptr).expect("mmap returned null");
    let obj_addr = align_up(map_base.as_ptr() as usize, align);
    let obj_base = NonNull::new(obj_addr as *mut u8).expect("LOS aligned pointer is null");
    debug_assert!(obj_addr + size <= (map_base.as_ptr() as usize) + mmap_size);

    self.entries.push(LosEntry {
      map_base,
      obj_base,
      mmap_size,
      obj_size: size,
    });
    obj_base.as_ptr()
  }

  /// Iterates over all currently-tracked LOS objects.
  ///
  /// The callback receives `(object_ptr, object_size)`.
  pub(crate) fn for_each_object(&self, mut f: impl FnMut(*mut u8, usize)) {
    for entry in &self.entries {
      f(entry.obj_base.as_ptr(), entry.obj_size);
    }
  }

  /// Sweeps the LOS, freeing (munmap'ing) any object that is not marked for `current_epoch`.
  ///
  /// Returns the number of bytes unmapped.
  pub(crate) fn sweep(&mut self, current_epoch: u8) -> usize {
    let mut freed = 0usize;
    self.entries.retain(|entry| unsafe {
      // Avoid creating long-lived `&ObjHeader` references here: sweeping may need
      // to mutate the header (to clear the card table pointer) before freeing.
      let hdr_ptr = crate::gc::header_from_obj(entry.obj_base.as_ptr());
      if (*hdr_ptr).is_marked(current_epoch) {
        return true;
      }

      let card_table = (*hdr_ptr).card_table_ptr();
      if !card_table.is_null() {
        // Clear the header pointer before unmapping the object so other GC
        // bookkeeping (e.g. the card table registry) can't accidentally try
        // to free it twice.
        (&mut *hdr_ptr).set_card_table_ptr(core::ptr::null_mut());
        crate::gc::free_card_table(card_table, entry.obj_size);
      }
      os_free(entry.map_base.as_ptr(), entry.mmap_size);
      freed += entry.mmap_size;
      false
    });
    freed
  }

  pub(crate) fn live_bytes(&self, current_epoch: u8) -> usize {
    let mut live = 0usize;
    self.for_each_object(|obj, size| unsafe {
      let hdr = &*crate::gc::header_from_obj(obj);
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
        os_free(entry.map_base.as_ptr(), entry.mmap_size);
      }
    }
  }
}

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align != 0 && align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
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
  let ptr = loop {
    let ptr = libc::mmap(
      std::ptr::null_mut(),
      size,
      libc::PROT_READ | libc::PROT_WRITE,
      libc::MAP_PRIVATE | libc::MAP_ANON,
      -1,
      0,
    );
    if ptr == libc::MAP_FAILED {
      let err = std::io::Error::last_os_error();
      if err.kind() == std::io::ErrorKind::Interrupted {
        continue;
      }
      panic!("mmap failed: {err}");
    }
    if ptr.is_null() {
      // Mapping at address 0 is unexpected; unmap and treat as fatal.
      let _ = libc::munmap(ptr, size);
      panic!("mmap returned null");
    }
    break ptr;
  };
  ptr as *mut u8
}

#[cfg(unix)]
unsafe fn os_free(ptr: *mut u8, size: usize) {
  loop {
    let rc = libc::munmap(ptr as *mut c_void, size);
    if rc == 0 {
      return;
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::Interrupted {
      continue;
    }
    panic!("munmap failed: {err}");
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
