use std::mem::{align_of, size_of};

use runtime_native::abi::{PromiseRef, RtCoroutineHeader, RtCoroStatus, TaskId};

#[test]
fn abi_layout_smoke() {
  assert_eq!(size_of::<TaskId>(), 8);
  assert_eq!(align_of::<TaskId>(), align_of::<u64>());

  assert_eq!(size_of::<PromiseRef>(), size_of::<usize>());
  assert_eq!(align_of::<PromiseRef>(), align_of::<usize>());

  assert_eq!(size_of::<RtCoroStatus>(), 4);
  assert_eq!(align_of::<RtCoroStatus>(), 4);

  let ptr_size = size_of::<usize>();
  let ptr_align = align_of::<usize>();

  assert_eq!(align_of::<RtCoroutineHeader>(), ptr_align);

  // `RtCoroutineHeader` layout:
  // - 4 pointer-sized fields (resume fn ptr, promise, await_value, await_error)
  // - 2 u32 fields (state, await_is_error)
  let raw_size = (4 * ptr_size) + (2 * size_of::<u32>());
  let expected_size = (raw_size + (ptr_align - 1)) & !(ptr_align - 1);
  assert_eq!(size_of::<RtCoroutineHeader>(), expected_size);
}

#[test]
fn rt_async_poll_smoke() {
  let _ = runtime_native::rt_async_poll_legacy();
}
