use core::mem::align_of;
use core::mem::size_of;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicUsize;

use memoffset::offset_of;

use runtime_native::async_abi::Coroutine;
use runtime_native::async_abi::CoroutineRef;
use runtime_native::async_abi::CoroutineVTable;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::async_abi::PromiseRef;

#[repr(C)]
struct Promise<T> {
  header: PromiseHeader,
  payload: core::mem::MaybeUninit<T>,
}

#[repr(C)]
struct CoroutineFrame<Locals> {
  coroutine: Coroutine,
  locals: Locals,
}

#[test]
fn promise_header_layout_is_stable() {
  assert_eq!(align_of::<PromiseHeader>(), 8);

  assert_eq!(offset_of!(PromiseHeader, state), 0);
  assert_eq!(offset_of!(PromiseHeader, waiters), size_of::<usize>());
  assert_eq!(offset_of!(PromiseHeader, flags), size_of::<usize>() * 2);

  assert_eq!(size_of::<PromiseHeader>(), size_of::<usize>() * 2 + 8);
  assert_eq!(size_of::<PromiseHeader>() % align_of::<PromiseHeader>(), 0);
}

#[test]
fn coroutine_layout_is_stable() {
  let ptr_size = size_of::<usize>();
  let ptr_align = align_of::<usize>();

  assert_eq!(offset_of!(Coroutine, vtable), 0);
  assert_eq!(offset_of!(Coroutine, promise), ptr_size);
  assert_eq!(offset_of!(Coroutine, next_waiter), ptr_size * 2);
  assert_eq!(offset_of!(Coroutine, flags), ptr_size * 3);

  let raw_size = (3 * ptr_size) + size_of::<u32>();
  let expected_size = (raw_size + (ptr_align - 1)) & !(ptr_align - 1);
  assert_eq!(align_of::<Coroutine>(), ptr_align);
  assert_eq!(size_of::<Coroutine>(), expected_size);
}

#[test]
fn coroutine_vtable_layout_is_stable() {
  let ptr = size_of::<usize>();
  let u32_ = size_of::<u32>();

  assert_eq!(offset_of!(CoroutineVTable, resume), 0);
  assert_eq!(offset_of!(CoroutineVTable, destroy), ptr);

  assert_eq!(offset_of!(CoroutineVTable, promise_size), ptr * 2);
  assert_eq!(offset_of!(CoroutineVTable, promise_align), ptr * 2 + u32_);
  assert_eq!(offset_of!(CoroutineVTable, promise_shape_id), ptr * 2 + u32_ * 2);
  assert_eq!(offset_of!(CoroutineVTable, abi_version), ptr * 2 + u32_ * 3);
  assert_eq!(offset_of!(CoroutineVTable, reserved), ptr * 2 + u32_ * 4);

  assert_eq!(size_of::<CoroutineVTable>(), ptr * 6 + 16);
}

#[test]
fn promise_ref_round_trip_casts() {
  let mut p: Promise<u64> = Promise {
    header: PromiseHeader {
      state: AtomicU8::new(PromiseHeader::PENDING),
      waiters: AtomicUsize::new(0),
      flags: AtomicU8::new(0),
    },
    payload: core::mem::MaybeUninit::uninit(),
  };

  let header_ptr: *mut PromiseHeader = &mut p.header;
  let promise_ptr: *mut Promise<u64> = &mut p;

  let r: PromiseRef = header_ptr;
  assert_eq!(r, header_ptr);

  let round_trip: *mut Promise<u64> = r.cast();
  assert_eq!(round_trip, promise_ptr);
}

#[test]
fn coroutine_ref_round_trip_casts() {
  let mut frame: CoroutineFrame<u32> = CoroutineFrame {
    coroutine: Coroutine {
      vtable: core::ptr::null(),
      promise: core::ptr::null_mut(),
      next_waiter: core::ptr::null_mut(),
      flags: 0,
    },
    locals: 123,
  };

  let coro_ptr: *mut Coroutine = &mut frame.coroutine;
  let frame_ptr: *mut CoroutineFrame<u32> = &mut frame;

  let r: CoroutineRef = coro_ptr;
  assert_eq!(r, coro_ptr);

  let round_trip: *mut CoroutineFrame<u32> = r.cast();
  assert_eq!(round_trip, frame_ptr);
}
