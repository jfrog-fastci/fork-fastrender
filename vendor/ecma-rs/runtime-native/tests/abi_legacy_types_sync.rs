use core::mem::{align_of, size_of};
use std::sync::Once;

use runtime_native::abi::{LegacyPromiseRef, PromiseResolveInput, RtShapeDescriptor, RtShapeId};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseRef;

// Ensure we can allocate a GC-managed promise via `rt_alloc` in this test binary.
static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: size_of::<PromiseHeader>() as u32,
      align: align_of::<PromiseHeader>() as u16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    runtime_native::shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

#[test]
fn legacy_abi_struct_layout_matches_runtime_native_abi() {
  assert_eq!(
    size_of::<runtime_native::abi::PromiseResolveInput>(),
    size_of::<runtime_native_abi::PromiseResolveInput>()
  );
  assert_eq!(
    align_of::<runtime_native::abi::PromiseResolveInput>(),
    align_of::<runtime_native_abi::PromiseResolveInput>()
  );

  assert_eq!(
    size_of::<runtime_native::abi::RtCoroutineHeader>(),
    size_of::<runtime_native_abi::RtCoroutineHeader>()
  );
  assert_eq!(
    align_of::<runtime_native::abi::RtCoroutineHeader>(),
    align_of::<runtime_native_abi::RtCoroutineHeader>()
  );
}

#[test]
fn promise_resolve_input_promise_requires_legacy_promise_ref() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table();

  // Compile-time regression check: `PromiseResolveInput::promise` must take a `LegacyPromiseRef`,
  // preventing accidental use of a native async-ABI `PromiseRef` (header-only).
  let _ctor: fn(LegacyPromiseRef) -> PromiseResolveInput = PromiseResolveInput::promise;

  // Construct a native async-ABI promise allocated in the GC heap.
  let obj = runtime_native::rt_alloc(size_of::<PromiseHeader>(), RtShapeId(1));
  assert!(!obj.is_null());
  let native_promise = PromiseRef(obj.cast());
  unsafe {
    runtime_native::rt_promise_init(native_promise);
  }

  // This should remain possible for non-promise inputs.
  let _value_input: PromiseResolveInput = PromiseResolveInput::value(core::ptr::null_mut());

  let _ = (native_promise, _value_input);
}
