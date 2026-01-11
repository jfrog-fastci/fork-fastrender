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
use std::sync::atomic::AtomicU64;

static OBSERVED: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicUsize = AtomicUsize::new(0);
static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static TIMER_ID: AtomicU64 = AtomicU64::new(0);
static WATCHER_ID: AtomicU64 = AtomicU64::new(0);

extern "C" fn record_ptr(data: *mut u8) {
  OBSERVED.store(data as usize, Ordering::SeqCst);
}

extern "C" fn record_drop(data: *mut u8) {
  DROP_COUNT.fetch_add(1, Ordering::SeqCst);
  DROPPED.store(data as usize, Ordering::SeqCst);
}

extern "C" fn record_ptr_io(_events: u32, data: *mut u8) {
  record_ptr(data);
}

extern "C" fn record_ptr_io_and_unregister(_events: u32, data: *mut u8) {
  record_ptr(data);
  let id = WATCHER_ID.load(Ordering::SeqCst);
  runtime_native::rt_io_unregister(id);
}

extern "C" fn record_ptr_and_clear_timer(data: *mut u8) {
  record_ptr(data);
  let id = TIMER_ID.load(Ordering::SeqCst);
  runtime_native::rt_clear_timer(id);
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

fn pipe_blocking() -> (OwnedFd, OwnedFd) {
  let mut fds = [0i32; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
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
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  runtime_native::rt_queue_microtask_handle(record_ptr, h);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the microtask runs"
  );
  assert_eq!(
    DROPPED.load(Ordering::SeqCst),
    0,
    "microtask without drop hook must not invoke drop_data"
  );
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

  threading::unregister_current_thread();
}

#[test]
fn queue_microtask_handle_with_drop_invokes_drop_hook() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  runtime_native::rt_queue_microtask_handle_with_drop(record_ptr, h, record_drop);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the microtask runs"
  );

  threading::unregister_current_thread();
}

#[test]
fn queue_microtask_handle_with_drop_stale_handle_is_noop() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  runtime_native::rt_queue_microtask_handle_with_drop(record_ptr, h, record_drop);

  // Simulate ABI misuse: the embedding frees the handle even though the runtime now owns it. This
  // should not crash; callbacks should treat the stale handle as a no-op.
  runtime_native::rt_handle_free(h);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
  assert!(runtime_native::rt_handle_load(h).is_null());

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
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let _timer = runtime_native::rt_set_timeout_handle(record_ptr, h, 0);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the timeout fires"
  );
  assert_eq!(
    DROPPED.load(Ordering::SeqCst),
    0,
    "timeout without drop hook must not invoke drop_data"
  );
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

  threading::unregister_current_thread();
}

#[test]
fn clear_timeout_handle_with_drop_invokes_drop_hook_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let timer = runtime_native::rt_set_timeout_handle_with_drop(record_ptr, h, record_drop, 60_000);

  simulate_relocation(obj1, obj2);

  runtime_native::rt_clear_timer(timer);

  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the timeout is cleared"
  );

  // Ensure the cleared timeout does not fire later.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);

  threading::unregister_current_thread();
}

#[test]
fn set_timeout_handle_with_drop_stale_handle_is_noop() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let _timer = runtime_native::rt_set_timeout_handle_with_drop(record_ptr, h, record_drop, 0);

  // Simulate ABI misuse: the embedding frees the handle even though the runtime now owns it.
  runtime_native::rt_handle_free(h);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
  assert!(runtime_native::rt_handle_load(h).is_null());

  threading::unregister_current_thread();
}

#[test]
fn set_interval_handle_keeps_userdata_rooted_until_cleared() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let timer = runtime_native::rt_set_interval_handle(record_ptr, h, 0);

  simulate_relocation(obj1, obj2);

  // One poll turn should fire the interval immediately (`interval_ms == 0`).
  let pending = runtime_native::rt_async_poll_legacy();
  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert!(
    !runtime_native::rt_handle_load(h).is_null(),
    "runtime must keep the consumed handle alive while the interval is active"
  );
  assert_eq!(
    DROPPED.load(Ordering::SeqCst),
    0,
    "interval without drop hook must not invoke drop_data"
  );
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

  // There should still be pending work since the interval reschedules itself.
  assert!(pending);

  runtime_native::rt_clear_timer(timer);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the interval is cleared"
  );

  while runtime_native::rt_async_poll_legacy() {}

  threading::unregister_current_thread();
}

#[test]
fn clear_interval_handle_with_drop_inside_callback_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let timer = runtime_native::rt_set_interval_handle_with_drop(record_ptr_and_clear_timer, h, record_drop, 0);
  TIMER_ID.store(timer, Ordering::SeqCst);

  simulate_relocation(obj1, obj2);

  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the interval clears itself"
  );

  threading::unregister_current_thread();
}

#[test]
fn clear_interval_handle_with_drop_invokes_drop_hook_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let timer = runtime_native::rt_set_interval_handle_with_drop(record_ptr, h, record_drop, 60_000);

  simulate_relocation(obj1, obj2);

  runtime_native::rt_clear_timer(timer);

  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the interval is cleared"
  );

  // Ensure the cleared interval does not fire later.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);

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

#[test]
fn io_register_handle_invalid_interests_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let watcher = runtime_native::rt_io_register_handle(0, 0, record_ptr_io, h);
  assert_eq!(watcher, 0);
  assert_eq!(runtime_native::rt_io_debug_take_last_error(), runtime_native::rt_io_debug::ERR_INVALID_INTERESTS);

  assert!(runtime_native::rt_handle_load(h).is_null());
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_invalid_interests_calls_drop_and_frees_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let h = runtime_native::rt_handle_alloc(obj1);

  simulate_relocation(obj1, obj2);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let watcher = runtime_native::rt_io_register_handle_with_drop(0, 0, record_ptr_io, h, record_drop);
  assert_eq!(watcher, 0);
  assert_eq!(runtime_native::rt_io_debug_take_last_error(), runtime_native::rt_io_debug::ERR_INVALID_INTERESTS);

  assert!(runtime_native::rt_handle_load(h).is_null());
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_rejects_already_registered_fd_and_drops_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);
  let obj3 = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_nonblocking();

  let h1 = runtime_native::rt_handle_alloc(obj1);
  let watcher1 = runtime_native::rt_io_register_handle(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h1,
  );
  assert_ne!(watcher1, 0, "rt_io_register_handle should succeed");

  let h2 = runtime_native::rt_handle_alloc(obj2);
  simulate_relocation(obj2, obj3);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let watcher2 = runtime_native::rt_io_register_handle_with_drop(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h2,
    record_drop,
  );
  assert_eq!(watcher2, 0);
  assert_eq!(
    runtime_native::rt_io_debug_take_last_error(),
    runtime_native::rt_io_debug::ERR_ALREADY_REGISTERED
  );

  assert!(runtime_native::rt_handle_load(h2).is_null());
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj3 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

  runtime_native::rt_io_unregister(watcher1);
  assert!(runtime_native::rt_handle_load(h1).is_null());

  // Drain any wakeups triggered by unregistering the watcher.
  while runtime_native::rt_async_poll_legacy() {}

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_rejects_blocking_fd_and_drops_handle() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_blocking();

  let h = runtime_native::rt_handle_alloc(obj1);
  simulate_relocation(obj1, obj2);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let watcher = runtime_native::rt_io_register_handle_with_drop(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h,
    record_drop,
  );
  assert_eq!(watcher, 0);
  assert_eq!(runtime_native::rt_io_debug_take_last_error(), runtime_native::rt_io_debug::ERR_FD_NOT_NONBLOCKING);

  assert!(runtime_native::rt_handle_load(h).is_null());
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_stale_handle_is_noop() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_nonblocking();

  let h = runtime_native::rt_handle_alloc(obj);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let watcher = runtime_native::rt_io_register_handle_with_drop(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h,
    record_drop,
  );
  assert_ne!(watcher, 0);

  // Simulate ABI misuse: handle freed while watcher still registered. The runtime should treat this
  // as a no-op (no callback and no drop hook invocation).
  runtime_native::rt_handle_free(h);

  let byte = [0x2au8];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), byte.as_ptr().cast(), 1) };
  assert_eq!(rc, 1, "write to pipe failed: {}", std::io::Error::last_os_error());

  assert!(
    runtime_native::rt_async_poll_legacy(),
    "reactor should process readiness events even when userdata handle is stale"
  );

  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);

  runtime_native::rt_io_unregister(watcher);

  assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
  assert!(runtime_native::rt_handle_load(h).is_null());

  // Drain any wakeups triggered by unregistering.
  while runtime_native::rt_async_poll_legacy() {}

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_invokes_drop_hook_on_unregister() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_nonblocking();

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);
  let watcher = runtime_native::rt_io_register_handle_with_drop(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io,
    h,
    record_drop,
  );
  assert_ne!(watcher, 0, "rt_io_register_handle_with_drop should succeed");

  simulate_relocation(obj1, obj2);

  runtime_native::rt_io_unregister(watcher);

  // No poll => no readiness callbacks should have run.
  assert_eq!(OBSERVED.load(Ordering::SeqCst), 0);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after the watcher is unregistered"
  );

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}

#[test]
fn io_register_handle_with_drop_can_unregister_inside_callback() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let mut heap = GcHeap::new();
  let obj1 = heap.alloc_pinned(&LEAF_DESC);
  let obj2 = heap.alloc_pinned(&LEAF_DESC);

  let (rfd, wfd) = pipe_nonblocking();

  let h = runtime_native::rt_handle_alloc(obj1);

  OBSERVED.store(0, Ordering::SeqCst);
  DROPPED.store(0, Ordering::SeqCst);
  DROP_COUNT.store(0, Ordering::SeqCst);

  let watcher = runtime_native::rt_io_register_handle_with_drop(
    rfd.as_raw_fd(),
    runtime_native::abi::RT_IO_READABLE,
    record_ptr_io_and_unregister,
    h,
    record_drop,
  );
  assert_ne!(watcher, 0);
  WATCHER_ID.store(watcher, Ordering::SeqCst);

  simulate_relocation(obj1, obj2);

  let byte = [0x2au8];
  let rc = unsafe { libc::write(wfd.as_raw_fd(), byte.as_ptr().cast(), 1) };
  assert_eq!(rc, 1, "write to pipe failed: {}", std::io::Error::last_os_error());

  runtime_native::rt_async_poll_legacy();

  assert_eq!(OBSERVED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROPPED.load(Ordering::SeqCst), obj2 as usize);
  assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
  assert!(
    runtime_native::rt_handle_load(h).is_null(),
    "runtime must free the consumed handle after unregistering from within the callback"
  );

  // Drain any wakeups triggered by unregistering.
  while runtime_native::rt_async_poll_legacy() {}

  drop(rfd);
  drop(wfd);

  threading::unregister_current_thread();
}
