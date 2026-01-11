use runtime_native::gc::roots::GlobalRootSet;
use runtime_native::gc::ObjHeader;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::GcHeap;
use runtime_native::TypeDescriptor;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const MAGIC: u64 = 0x0123_4567_89AB_CDEF;

const HEADER_SIZE: usize = std::mem::size_of::<ObjHeader>();
const MAGIC_OFFSET: usize = HEADER_SIZE;
const SEEN_OFFSET: usize = HEADER_SIZE + std::mem::size_of::<u64>();

static NO_PTR_OFFSETS: [u32; 0] = [];
static TEST_OBJ_DESC: TypeDescriptor = TypeDescriptor::new(
  HEADER_SIZE + std::mem::size_of::<u64>() + std::mem::size_of::<AtomicU64>(),
  &NO_PTR_OFFSETS,
);

unsafe fn init_test_obj(heap: &mut GcHeap) -> *mut u8 {
  let obj = heap.alloc_young(&TEST_OBJ_DESC);
  (obj.add(MAGIC_OFFSET) as *mut u64).write(MAGIC);
  (obj.add(SEEN_OFFSET) as *mut AtomicU64).write(AtomicU64::new(0));
  obj
}

unsafe fn seen_magic_slot(obj: *mut u8) -> &'static AtomicU64 {
  &*(obj.add(SEEN_OFFSET) as *const AtomicU64)
}

extern "C" fn record_magic_io(_events: u32, data: *mut u8) {
  unsafe {
    let magic = (data.add(MAGIC_OFFSET) as *const u64).read();
    let seen = &*(data.add(SEEN_OFFSET) as *const AtomicU64);
    seen.store(magic, Ordering::Release);
  }
}

fn collect_major(heap: &mut GcHeap) {
  let mut roots = GlobalRootSet::new();
  let mut remembered = SimpleRememberedSet::new();
  let _ = heap.collect_major(&mut roots, &mut remembered);
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    if flags < 0 {
      return Err(io::Error::last_os_error());
    }
    if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
      return Err(io::Error::last_os_error());
    }
  }
  Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  set_nonblocking(fds[0])?;
  set_nonblocking(fds[1])?;
  // Safety: `pipe` returns new, owned file descriptors.
  let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((rfd, wfd))
}

fn write_byte(fd: RawFd) {
  let byte: u8 = 1;
  let rc = unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
  assert_eq!(rc, 1, "write failed: {}", io::Error::last_os_error());
}

struct WeakHandleGuard(u64);

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    if self.0 != 0 {
      runtime_native::rt_weak_remove(self.0);
      self.0 = 0;
    }
  }
}

#[test]
fn io_watcher_rooted_keeps_gc_object_alive_and_relocates_pointer() {
  let _rt = TestRuntimeGuard::new();

  let (rfd, wfd) = pipe().unwrap();

  let mut heap = GcHeap::new();
  let obj = unsafe { init_test_obj(&mut heap) };
  let weak = runtime_native::rt_weak_add(obj);
  let _weak_guard = WeakHandleGuard(weak);

  let watcher = runtime_native::rt_io_register_rooted(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_magic_io,
    obj,
  );
  assert_ne!(watcher, 0);

  // Move/collect while the watcher is registered but before any readiness is delivered.
  collect_major(&mut heap);

  let after_gc = runtime_native::rt_weak_get(weak);
  assert!(!after_gc.is_null());
  assert!(!heap.is_in_nursery(after_gc));

  write_byte(wfd.as_raw_fd());

  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    runtime_native::rt_async_poll_legacy();
    let ptr = runtime_native::rt_weak_get(weak);
    assert!(!ptr.is_null());
    let seen = unsafe { seen_magic_slot(ptr) }.load(Ordering::Acquire);
    if seen != 0 {
      assert_eq!(seen, MAGIC);
      break;
    }
    assert!(Instant::now() < deadline, "rooted I/O watcher did not run in time");
    std::thread::yield_now();
  }

  // The watcher still holds a root, so the object must remain alive across GC.
  collect_major(&mut heap);
  assert!(!runtime_native::rt_weak_get(weak).is_null());

  runtime_native::rt_io_unregister(watcher);

  // Drain any queued reactor tasks so rooted callback state can drop promptly.
  let deadline = Instant::now() + Duration::from_secs(2);
  while runtime_native::rt_async_poll_legacy() {
    assert!(
      Instant::now() < deadline,
      "runtime did not become idle after unregistering watcher"
    );
    std::thread::yield_now();
  }

  // After unregistration, the root must be released and the object should become collectible.
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    collect_major(&mut heap);
    if runtime_native::rt_weak_get(weak).is_null() {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "object stayed alive after rooted I/O watcher was unregistered (root not released?)"
    );
    std::thread::yield_now();
  }
}
