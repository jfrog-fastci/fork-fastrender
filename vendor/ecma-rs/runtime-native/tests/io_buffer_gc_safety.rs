#![cfg(target_os = "linux")]

use runtime_native::gc::{RootHandle, RootStack, SimpleRememberedSet, TypeDescriptor, OBJ_HEADER_SIZE};
use runtime_native::io::{UringCancellationToken as CancellationToken, UringIoError as IoError, UringDriver};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{ArrayBuffer, GcHeap, Uint8Array};
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

// GC type descriptors for embedding `ArrayBuffer`/`Uint8Array` structs in the GC heap for tests.
static ARRAY_BUFFER_DESC: TypeDescriptor = TypeDescriptor::new(
  OBJ_HEADER_SIZE + core::mem::size_of::<ArrayBuffer>(),
  &[],
);
static UINT8_ARRAY_PTR_OFFSETS: [u32; 1] = [OBJ_HEADER_SIZE as u32];
static UINT8_ARRAY_DESC: TypeDescriptor = TypeDescriptor::new(
  OBJ_HEADER_SIZE + core::mem::size_of::<Uint8Array>(),
  &UINT8_ARRAY_PTR_OFFSETS,
);
static DUMMY_DESC: TypeDescriptor = TypeDescriptor::new(OBJ_HEADER_SIZE, &[]);

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];
  let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  // Safety: `pipe` returns new, owned file descriptors.
  let rfd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let wfd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((rfd, wfd))
}

fn write_all(fd: RawFd, bytes: &[u8]) {
  let mut written = 0usize;
  while written < bytes.len() {
    let rc = unsafe {
      libc::write(
        fd,
        bytes[written..].as_ptr() as *const libc::c_void,
        bytes.len() - written,
      )
    };
    assert!(rc >= 0, "write failed: {}", io::Error::last_os_error());
    written += rc as usize;
  }
}

struct FlagWake {
  flag: Arc<AtomicBool>,
}

impl Wake for FlagWake {
  fn wake(self: Arc<Self>) {
    self.flag.store(true, Ordering::SeqCst);
  }

  fn wake_by_ref(self: &Arc<Self>) {
    self.flag.store(true, Ordering::SeqCst);
  }
}

fn flag_waker(flag: Arc<AtomicBool>) -> Waker {
  Waker::from(Arc::new(FlagWake { flag }))
}

fn block_on<F: Future>(fut: F, timeout: Duration) -> F::Output {
  let woke = Arc::new(AtomicBool::new(false));
  let waker = flag_waker(woke.clone());
  let mut cx = Context::from_waker(&waker);
  let mut fut = Box::pin(fut);
  let deadline = Instant::now() + timeout;

  loop {
    match fut.as_mut().poll(&mut cx) {
      Poll::Ready(out) => return out,
      Poll::Pending => {
        while !woke.swap(false, Ordering::SeqCst) {
          if Instant::now() > deadline {
            panic!("timed out waiting for future");
          }
          std::thread::yield_now();
        }
      }
    }
  }
}

fn alloc_array_buffer(heap: &mut GcHeap, byte_len: usize) -> *mut u8 {
  // Allocate the ArrayBuffer header in old gen so it does not move during nursery evacuation. The
  // backing store itself is always non-moving.
  let obj = heap.alloc_old(&ARRAY_BUFFER_DESC);
  let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer };
  let header = ArrayBuffer::new_zeroed(byte_len).unwrap();
  unsafe {
    payload.write(header);
  }
  obj
}

fn alloc_uint8_array(heap: &mut GcHeap, buffer: *mut u8, byte_offset: usize, length: usize) -> *mut u8 {
  let obj = heap.alloc_young(&UINT8_ARRAY_DESC);
  let payload = unsafe { obj.add(OBJ_HEADER_SIZE) as *mut Uint8Array };
  let buffer_payload = unsafe { &*(buffer.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
  let view = Uint8Array::view(buffer_payload, byte_offset, length).unwrap();
  unsafe {
    payload.write(view);
  }
  obj
}

fn alloc_dummy(heap: &mut GcHeap) -> *mut u8 {
  heap.alloc_young(&DUMMY_DESC)
}

fn finalize_array_buffer(_heap: &mut GcHeap, buffer_obj: *mut u8) {
  // Safety: payload layout matches `ArrayBuffer`.
  let buf = unsafe { &mut *(buffer_obj.add(OBJ_HEADER_SIZE) as *mut ArrayBuffer) };
  buf.finalize();
}

fn pin_count(_heap: &GcHeap, buffer_obj: *mut u8) -> u32 {
  let buf = unsafe { &*(buffer_obj.add(OBJ_HEADER_SIZE) as *const ArrayBuffer) };
  buf.pin_count()
}

fn root_get(heap: &GcHeap, h: RootHandle) -> *mut u8 {
  heap.root_get(h).expect("root handle missing")
}

#[test]
fn read_survives_moving_gc_while_in_flight() {
  let _rt = TestRuntimeGuard::new();

  let heap = Arc::new(Mutex::new(GcHeap::new()));
  let driver = UringDriver::new(64).unwrap();
  let (rfd, wfd) = pipe().unwrap();

  // Allocate GC-managed ArrayBuffer + Uint8Array headers (backing store lives outside the GC heap).
  let (array_obj, buffer_obj, promise_obj, array_root, buffer_root) = {
    let mut heap = heap.lock().unwrap();
    let buffer_obj = alloc_array_buffer(&mut heap, 16);
    let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 5);
    let promise_obj = alloc_dummy(&mut heap);
    let array_root = heap.root_add(array_obj);
    let buffer_root = heap.root_add(buffer_obj);
    (array_obj, buffer_obj, promise_obj, array_root, buffer_root)
  };

  let array_ptr_before = {
    let heap = heap.lock().unwrap();
    root_get(&heap, array_root)
  };

  // Compute the stable byte pointer once; the backing store is non-moving.
  let bytes_ptr = unsafe {
    let view = &*(array_obj.add(OBJ_HEADER_SIZE) as *const Uint8Array);
    view.as_ptr_range().unwrap().0 as *const u8
  };

  assert_eq!(pin_count(&heap.lock().unwrap(), buffer_obj), 0);

  let read_fut = driver
    .read_into_uint8_array(Arc::clone(&heap), rfd.as_raw_fd(), array_obj, promise_obj, None)
    .unwrap();

  // Creating the op must pin the ArrayBuffer backing store guard immediately.
  assert!(pin_count(&heap.lock().unwrap(), buffer_obj) > 0);

  // Trigger at least one moving GC cycle while the read is in flight (no data yet).
  {
    let mut heap = heap.lock().unwrap();
    let mut roots = RootStack::new();
    let mut remembered = SimpleRememberedSet::new();
    heap.collect_minor(&mut roots, &mut remembered);
  }

  let array_ptr_after = {
    let heap = heap.lock().unwrap();
    root_get(&heap, array_root)
  };
  assert_ne!(array_ptr_after, array_ptr_before, "Uint8Array should have been evacuated");

  write_all(wfd.as_raw_fd(), b"hello");

  let n = block_on(read_fut, Duration::from_secs(2)).unwrap();
  assert_eq!(n, 5);

  let got = unsafe { std::slice::from_raw_parts(bytes_ptr, n) };
  assert_eq!(got, b"hello");

  // Completion should unpin.
  assert_eq!(pin_count(&heap.lock().unwrap(), buffer_obj), 0);

  // Cleanup: remove test roots and free external backing store.
  {
    let mut heap = heap.lock().unwrap();
    heap.root_remove(array_root);
    heap.root_remove(buffer_root);
    finalize_array_buffer(&mut heap, buffer_obj);
  }
}

#[test]
fn cancel_before_submission_never_submits() {
  let _rt = TestRuntimeGuard::new();

  let heap = Arc::new(Mutex::new(GcHeap::new()));
  let driver = UringDriver::new(8).unwrap();
  let (rfd, _wfd) = pipe().unwrap();

  let (array_obj, buffer_obj, promise_obj) = {
    let mut heap = heap.lock().unwrap();
    let buffer_obj = alloc_array_buffer(&mut heap, 8);
    let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 8);
    let promise_obj = alloc_dummy(&mut heap);
    (array_obj, buffer_obj, promise_obj)
  };

  let token = CancellationToken::new();
  token.cancel();

  let res = driver.read_into_uint8_array(Arc::clone(&heap), rfd.as_raw_fd(), array_obj, promise_obj, Some(token));
  assert!(matches!(res, Err(IoError::Cancelled)));
  assert_eq!(pin_count(&heap.lock().unwrap(), buffer_obj), 0);

  // Free external backing store.
  finalize_array_buffer(&mut heap.lock().unwrap(), buffer_obj);
}

#[test]
fn cancel_after_submission_cleans_up_pin() {
  let _rt = TestRuntimeGuard::new();

  let heap = Arc::new(Mutex::new(GcHeap::new()));
  let driver = UringDriver::new(64).unwrap();
  let (rfd, _wfd) = pipe().unwrap();

  let (array_obj, buffer_obj, promise_obj) = {
    let mut heap = heap.lock().unwrap();
    let buffer_obj = alloc_array_buffer(&mut heap, 8);
    let array_obj = alloc_uint8_array(&mut heap, buffer_obj, 0, 8);
    let promise_obj = alloc_dummy(&mut heap);
    (array_obj, buffer_obj, promise_obj)
  };

  let token = CancellationToken::new();
  let fut = driver
    .read_into_uint8_array(Arc::clone(&heap), rfd.as_raw_fd(), array_obj, promise_obj, Some(token.clone()))
    .unwrap();

  assert!(pin_count(&heap.lock().unwrap(), buffer_obj) > 0);

  token.cancel();
  let res = block_on(fut, Duration::from_secs(2));
  assert!(matches!(res, Err(IoError::Cancelled)));
  assert_eq!(pin_count(&heap.lock().unwrap(), buffer_obj), 0);

  finalize_array_buffer(&mut heap.lock().unwrap(), buffer_obj);
}
