#![cfg(all(
  target_os = "linux",
  target_arch = "x86_64",
  runtime_native_has_stackmap_test_artifact
))]

use runtime_native::stackwalk::{
  StackBounds, StackFrame, StackWalkError, StackWalker, ThreadContext,
};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

include!(env!("RUNTIME_NATIVE_STACKMAP_TEST_DATA_RS"));

extern "C" {
  fn test_fn(p: *mut u8) -> *mut u8;
}

static EXPECTED_PTR: AtomicU64 = AtomicU64::new(0);
static CAPTURE: OnceLock<Capture> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct Capture {
  fp: u64,
  sp: u64,
  expected_return_address: u64,
  expected_ptr: u64,
  frame: Option<StackFrame>,
  stackwalk_error: Option<StackWalkError>,
  slot_addr: Option<u64>,
  slot_value: Option<u64>,
  ok: bool,
}

// `test_fn` (compiled from an LLVM statepoint module in build.rs) calls into this
// symbol. It captures FP/SP and delegates to the Rust helper below.
core::arch::global_asm!(
  r#"
  .text
  .globl safepoint
  .type safepoint,@function
safepoint:
  push rbp
  mov rbp, rsp
  mov rdi, rbp
  mov rsi, rsp
  call runtime_native_test_safepoint_impl
  pop rbp
  ret
"#
);

#[no_mangle]
extern "C" fn runtime_native_test_safepoint_impl(fp: u64, sp: u64) {
  let expected_ptr = EXPECTED_PTR.load(Ordering::SeqCst);
  let expected_return_address =
    (test_fn as usize as u64).wrapping_add(STACKMAP_INSTRUCTION_OFFSET as u64);

  let mut capture = Capture {
    fp,
    sp,
    expected_return_address,
    expected_ptr,
    frame: None,
    stackwalk_error: None,
    slot_addr: None,
    slot_value: None,
    ok: false,
  };

  let Ok(bounds) = StackBounds::current_thread() else {
    let _ = CAPTURE.set(capture);
    return;
  };

  let ctx = ThreadContext::new(sp, fp, 0);

  let walker = match StackWalker::new(ctx, bounds) {
    Ok(w) => w,
    Err(e) => {
      capture.stackwalk_error = Some(e);
      let _ = CAPTURE.set(capture);
      return;
    }
  };

  let mut it = walker;
  let frame = match it.next() {
    Some(Ok(f)) => f,
    Some(Err(e)) => {
      capture.stackwalk_error = Some(e);
      let _ = CAPTURE.set(capture);
      return;
    }
    None => {
      capture.stackwalk_error = Some(StackWalkError::FramePointerIsNull);
      let _ = CAPTURE.set(capture);
      return;
    }
  };

  capture.frame = Some(frame);

  if frame.return_address != expected_return_address {
    let _ = CAPTURE.set(capture);
    return;
  }

  // Stackmap locations are `Indirect [SP + off]`, where `SP` is the *caller* SP.
  let slot_addr = match (STACKMAP_SP_OFFSET >= 0, STACKMAP_SP_OFFSET) {
    (true, off) => frame.caller_sp.checked_add(off as u64),
    (false, off) => frame.caller_sp.checked_sub(off.unsigned_abs() as u64),
  };
  let Some(slot_addr) = slot_addr else {
    capture.stackwalk_error = Some(StackWalkError::AddressOverflow);
    let _ = CAPTURE.set(capture);
    return;
  };
  capture.slot_addr = Some(slot_addr);

  if !bounds.contains_range(slot_addr, 8) {
    capture.stackwalk_error = Some(StackWalkError::CallerSpOutOfBounds {
      caller_sp: slot_addr,
      bounds,
    });
    let _ = CAPTURE.set(capture);
    return;
  }

  // SAFETY: `slot_addr` is within the current thread's stack bounds.
  let slot_value = unsafe { (slot_addr as *const u64).read() };
  capture.slot_value = Some(slot_value);
  capture.ok = slot_value == expected_ptr;

  let _ = CAPTURE.set(capture);
}

#[test]
fn statepoint_stackmap_indirect_sp_slot_matches() {
  let _rt = TestRuntimeGuard::new();
  let mut obj = 0u64;
  let ptr = core::ptr::addr_of_mut!(obj).cast::<u8>() as u64;
  EXPECTED_PTR.store(ptr, Ordering::SeqCst);

  // Triggers `safepoint` during which we validate:
  //  - `StackWalker` returns the callsite return address (`test_fn + inst_offset`)
  //  - the stackmap's `Indirect [SP + off]` slot contains our spilled pointer.
  let ret = unsafe { test_fn(core::ptr::addr_of_mut!(obj).cast::<u8>()) };
  assert_eq!(ret as u64, ptr);

  let cap = CAPTURE.get().copied().expect("safepoint not reached");
  assert!(
    cap.ok,
    "stackmap slot mismatch:\n\
     {cap:#?}\n\
     STACKMAP_INSTRUCTION_OFFSET={STACKMAP_INSTRUCTION_OFFSET}\n\
     STACKMAP_SP_OFFSET={STACKMAP_SP_OFFSET}"
  );
}
