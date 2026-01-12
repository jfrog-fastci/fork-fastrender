#![cfg(all(
  runtime_native_has_statepoint_fixture,
  target_os = "linux",
  target_arch = "x86_64"
))]

use std::sync::Once;

use runtime_native::abi::RtShapeDescriptor;
use runtime_native::abi::RtShapeId;
use runtime_native::shape_table::rt_register_shape_table;
use runtime_native::test_util::TestRuntimeGuard;

// Linked via `build.rs` (copied into OUT_DIR and made PIE-friendly).
#[link(name = ":statepoint_fixture.o", kind = "static")]
extern "C" {
  fn test(a: *mut u8, b: *mut u8) -> *mut u8;
}

const SHAPE: RtShapeId = RtShapeId(1);
const PAYLOAD_SIZE: usize = 32;
const OBJ_SIZE: usize = runtime_native::gc::OBJ_HEADER_SIZE + PAYLOAD_SIZE;

static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: OBJ_SIZE as u32,
  align: 16,
  flags: 0,
  ptr_offsets: std::ptr::null(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table_registered() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn is_in_young(ptr: *mut u8) -> bool {
  let mut start = std::ptr::null_mut();
  let mut end = std::ptr::null_mut();
  unsafe {
    runtime_native::rt_gc_get_young_range(&mut start, &mut end);
  }
  let ptr = ptr as usize;
  ptr >= start as usize && ptr < end as usize
}

// `statepoint_fixture.o` declares an external `callee()` symbol that is invoked from the statepoint.
// Provide it here and trigger a stop-the-world collection while the GC roots are live in the
// statepoint's `"gc-live"` operand bundle.
#[no_mangle]
extern "C" fn callee() {
  runtime_native::rt_gc_collect();
}

#[test]
fn statepoint_fixture_relocates_root_slots_across_rt_gc_collect() {
  let _rt = TestRuntimeGuard::new();
  ensure_shape_table_registered();

  let obj = runtime_native::rt_alloc(OBJ_SIZE, SHAPE);
  assert!(!obj.is_null());
  assert!(is_in_young(obj), "expected rt_alloc allocation in nursery");

  // Initialize payload (just after ObjHeader) with a deterministic pattern.
  unsafe {
    let payload =
      std::slice::from_raw_parts_mut(obj.add(runtime_native::gc::OBJ_HEADER_SIZE), PAYLOAD_SIZE);
    for (i, b) in payload.iter_mut().enumerate() {
      *b = (i as u8).wrapping_mul(3).wrapping_add(1);
    }
  }

  // Preserve the original pointer value. Debug-mode tests can enable conservative stack scanning;
  // keep an obfuscated copy that won't be treated as a candidate pointer.
  let obj_before_gc = (obj as usize) ^ 1;

  // The fixture calls `rt_gc_collect` from an LLVM statepoint and returns the relocated pointer.
  let moved = unsafe { test(obj, obj) };
  assert!(!moved.is_null());

  let obj_before_gc = ((obj_before_gc) ^ 1) as *mut u8;
  assert_ne!(
    moved, obj_before_gc,
    "major GC should evacuate nursery objects to old-gen"
  );
  assert!(
    !is_in_young(moved),
    "relocated pointer should point outside the nursery"
  );

  // Verify payload survived evacuation.
  unsafe {
    let payload =
      std::slice::from_raw_parts(moved.add(runtime_native::gc::OBJ_HEADER_SIZE), PAYLOAD_SIZE);
    for (i, &b) in payload.iter().enumerate() {
      let expected = (i as u8).wrapping_mul(3).wrapping_add(1);
      assert_eq!(b, expected, "payload byte {i} changed after GC");
    }
  }
}
