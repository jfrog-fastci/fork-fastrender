#![cfg(all(
  target_os = "linux",
  target_arch = "x86_64",
  runtime_native_has_stackmap_test_artifact
))]

use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;

include!(env!("RUNTIME_NATIVE_STACKMAP_TEST_DATA_RS"));

extern "C" {
  fn test_fn(p: *mut u8) -> *mut u8;
}

// Override the weak `safepoint` symbol from `build.rs`' generated stackmap test
// module. We deliberately trigger `rt_gc_collect` from *within* a runtime frame
// so the GC initiator must recover the nearest managed callsite by walking the
// frame-pointer chain.
core::arch::global_asm!(
  r#"
  .text
  .globl safepoint
  .type safepoint,@function
safepoint:
  push rbp
  mov rbp, rsp
  call rt_gc_collect
  pop rbp
  ret
"#
);

#[test]
fn gc_collect_recovers_managed_callsite_from_runtime_frames() {
  let _rt = TestRuntimeGuard::new();

  threading::register_current_thread(ThreadKind::Worker);
  struct Unregister;
  impl Drop for Unregister {
    fn drop(&mut self) {
      threading::unregister_current_thread();
    }
  }
  let _unregister = Unregister;

  let mut obj = 0u64;
  let ptr = core::ptr::addr_of_mut!(obj).cast::<u8>();
  let ret = unsafe { test_fn(ptr) };
  assert_eq!(ret, ptr);

  let state = threading::registry::current_thread_state().expect("current thread state");
  let ctx = state
    .safepoint_context()
    .expect("expected rt_gc_collect to publish a safepoint context for the initiator");

  let expected_ip =
    (test_fn as usize as u64).wrapping_add(STACKMAP_INSTRUCTION_OFFSET as u64) as usize;
  assert_eq!(
    ctx.ip, expected_ip,
    "expected initiator ctx.ip to match the managed safepoint return address; \
     this requires an intact frame-pointer chain across runtime frames"
  );

  let bounds = state.stack_bounds().expect("thread stack bounds");
  assert!(
    ctx.fp >= bounds.lo && ctx.fp < bounds.hi,
    "expected ctx.fp={:#x} to be within stack bounds [{:#x}, {:#x})",
    ctx.fp,
    bounds.lo,
    bounds.hi
  );
  assert!(
    ctx.sp >= bounds.lo && ctx.sp < bounds.hi,
    "expected ctx.sp={:#x} to be within stack bounds [{:#x}, {:#x})",
    ctx.sp,
    bounds.lo,
    bounds.hi
  );
}
