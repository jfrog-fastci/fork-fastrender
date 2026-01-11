#![cfg(any(
  target_os = "linux",
  target_os = "macos",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd",
  target_os = "dragonfly"
))]

use runtime_native::gc::ObjHeader;
use runtime_native::gc::TypeDescriptor;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::GcHeap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};

static OBSERVED: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_ptr(data: *mut u8) {
  OBSERVED.store(data as usize, Ordering::SeqCst);
}

extern "C" fn record_ptr_io(_events: u32, data: *mut u8) {
  record_ptr(data);
}

#[repr(C)]
struct Leaf {
  _header: ObjHeader,
}

static LEAF_DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<Leaf>(), &[]);

fn pipe_nonblocking() -> (OwnedFd, OwnedFd) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());

  for &fd in &fds {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(flags, -1, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_ne!(rc, -1, "fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
  }

  // Safety: `pipe` returns new, owned file descriptors.
  unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn simulate_relocation(old_ptr: *mut u8, new_ptr: *mut u8) {
  let mut updated = 0usize;
  threading::safepoint::with_world_stopped(|epoch| {
    threading::safepoint::for_each_root_slot_world_stopped(epoch, |slot| unsafe {
      if *slot == old_ptr {
        *slot = new_ptr;
        updated += 1;
      }
    })
    .expect("root enumeration should succeed");
  });

  assert_eq!(updated, 1, "expected exactly one persistent-handle slot update");
}

#[test]
fn queue_microtask_handle_reloads_userdata_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  runtime_native::rt_queue_microtask_handle(record_ptr, h);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the microtask runs"
  );

  threading::unregister_current_thread();
}

#[test]
fn set_timeout_handle_reloads_userdata_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  let _timer = runtime_native::rt_set_timeout_handle(record_ptr, h, 0);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the timeout fires"
  );

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_reloads_userdata_from_persistent_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_nonblocking();

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  let watcher = runtime_native::rt_io_register_handle(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h,
  );
  assert_ne!(watcher, 0, "rt_io_register_handle should succeed");

  simulate_relocation(obj1, obj2);

  let byte = [0x2au8];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), byte.as_ptr().cast(), 1) };
  assert_eq!(rc, 1, "write to pipe failed: {}", std::io::Error::last_os_error());

  runtime_native::rt_async_poll_legacy();

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);

  runtime_native::rt_io_unregister(watcher);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the watcher is unregistered"
  );

  // Drain any wakeups triggered by unregistering the watcher.
  while runtime_native::rt_async_poll_legacy() {}

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}
