#[cfg(not(unix))]
use std::alloc::Layout;
use std::mem;
use std::ptr;

/// Fixed-capacity stack for GC object pointers.
///
/// GC tracing must not allocate via the Rust global allocator. `WorkStack` is backed by an mmap
/// region on Unix so pushing never allocates.
pub(crate) struct WorkStack {
  ptr: *mut *mut u8,
  len: usize,
  cap: usize,
  map_len: usize,
  #[cfg(not(unix))]
  layout: Layout,
}

impl WorkStack {
  const ENV_MB: &'static str = "ECMA_RS_RUNTIME_NATIVE_GC_WORK_STACK_MB";
  #[cfg(unix)]
  const ENV_MB_CSTR: &'static [u8] = b"ECMA_RS_RUNTIME_NATIVE_GC_WORK_STACK_MB\0";
  const DEFAULT_MB: usize = 64;

  pub(crate) fn new() -> Self {
    let bytes = work_stack_bytes_from_env().unwrap_or(Self::DEFAULT_MB * 1024 * 1024);

    // Round up to pointer alignment and compute element capacity.
    let elem_size = mem::size_of::<*mut u8>();
    let cap = bytes / elem_size;
    if cap == 0 {
      // A zero-capacity stack is unusable; fall back to a single element so overflow handling
      // triggers deterministically.
      return Self::with_capacity_elems(1);
    }
    Self::with_capacity_elems(cap)
  }

  fn with_capacity_elems(cap: usize) -> Self {
    let elem_size = mem::size_of::<*mut u8>();
    let bytes = cap
      .checked_mul(elem_size)
      .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("GC work stack size overflow"));

    // `mmap` length must be page-aligned.
    let page = page_size();
    let map_len = bytes
      .checked_add(page - 1)
      .map(|v| v & !(page - 1))
      .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("GC work stack size overflow"));

    #[cfg(unix)]
    unsafe {
      let ptr = libc::mmap(
        ptr::null_mut(),
        map_len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANON,
        -1,
        0,
      );
      if ptr == libc::MAP_FAILED || ptr.is_null() {
        crate::trap::rt_trap_oom(map_len, "GC work stack mmap");
      }
      Self {
        ptr: ptr.cast::<*mut u8>(),
        len: 0,
        cap,
        map_len,
      }
    }

    #[cfg(not(unix))]
    {
      // Best-effort fallback for non-Unix platforms: allocate once up front and never grow.
      let layout = Layout::array::<*mut u8>(cap)
        .unwrap_or_else(|_| crate::trap::rt_trap_invalid_arg("GC work stack size overflow"));
      // SAFETY: layout is non-zero and valid.
      let ptr = unsafe { std::alloc::alloc(layout) }.cast::<*mut u8>();
      if ptr.is_null() {
        crate::trap::rt_trap_oom(layout.size(), "GC work stack alloc");
      }
      Self {
        ptr: ptr.cast::<*mut u8>(),
        len: 0,
        cap,
        map_len,
        layout,
      }
    }
  }

  #[inline]
  pub(crate) fn clear(&mut self) {
    self.len = 0;
  }

  #[inline]
  #[allow(dead_code)]
  pub(crate) fn len(&self) -> usize {
    self.len
  }

  #[inline]
  pub(crate) fn push(&mut self, obj: *mut u8) {
    if self.len >= self.cap {
      eprintln!(
        "runtime-native: GC work stack overflow (capacity={} pointers, env {} to increase)",
        self.cap,
        Self::ENV_MB
      );
      std::process::abort();
    }
    // SAFETY: bounds checked above.
    unsafe {
      self.ptr.add(self.len).write(obj);
    }
    self.len += 1;
  }

  #[inline]
  pub(crate) fn pop(&mut self) -> Option<*mut u8> {
    if self.len == 0 {
      return None;
    }
    self.len -= 1;
    // SAFETY: `len` was > 0 so new index is in-bounds.
    let obj = unsafe { self.ptr.add(self.len).read() };
    Some(obj)
  }
}

impl Drop for WorkStack {
  fn drop(&mut self) {
    #[cfg(unix)]
    unsafe {
      if !self.ptr.is_null() && self.map_len != 0 {
        libc::munmap(self.ptr.cast::<libc::c_void>(), self.map_len);
      }
    }

    #[cfg(not(unix))]
    unsafe {
      if !self.ptr.is_null() && self.layout.size() != 0 {
        std::alloc::dealloc(self.ptr.cast::<u8>(), self.layout);
      }
    }
  }
}

#[cfg(unix)]
fn page_size() -> usize {
  // SAFETY: sysconf is thread-safe.
  let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if sz <= 0 { 4096 } else { sz as usize }
}

#[cfg(not(unix))]
fn page_size() -> usize {
  4096
}

#[cfg(unix)]
fn work_stack_bytes_from_env() -> Option<usize> {
  unsafe {
    let cstr = libc::getenv(WorkStack::ENV_MB_CSTR.as_ptr().cast());
    if cstr.is_null() {
      return None;
    }
    parse_usize_cstr(cstr).and_then(|mb| mb.checked_mul(1024 * 1024))
  }
}

#[cfg(not(unix))]
fn work_stack_bytes_from_env() -> Option<usize> {
  std::env::var(WorkStack::ENV_MB)
    .ok()
    .and_then(|v| v.parse::<usize>().ok())
    .and_then(|mb| mb.checked_mul(1024 * 1024))
}

#[cfg(unix)]
unsafe fn parse_usize_cstr(mut p: *const libc::c_char) -> Option<usize> {
  if p.is_null() {
    return None;
  }

  let mut value: usize = 0;
  let mut saw_digit = false;
  loop {
    let b = *p as u8;
    if b == 0 {
      break;
    }
    if !(b'0'..=b'9').contains(&b) {
      return None;
    }
    saw_digit = true;
    value = value.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    p = p.add(1);
  }
  if !saw_digit { None } else { Some(value) }
}
