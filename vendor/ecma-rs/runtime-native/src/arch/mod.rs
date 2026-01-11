//! Architecture-specific helpers for GC/safepoint integration.
//!
//! The eventual precise GC integration uses LLVM statepoint-generated stack maps.
//! For each thread parked in a stop-the-world safepoint we need a stable snapshot
//! of the *caller* state at the safepoint call site (stack pointer + return
//! address) so the GC can locate the correct stack map record.

use std::mem::MaybeUninit;

/// Pointer-sized word width for the current target.
pub const WORD_SIZE: usize = std::mem::size_of::<usize>();

/// Minimal execution context recorded for a thread parked in a safepoint.
///
/// The values here intentionally represent the state at the *call site* that
/// entered the runtime safepoint slow path.
///
/// `sp_entry` is the stack pointer as observed by the callee on entry.
///
/// `sp_before_call` is stored as `sp_entry + WORD_SIZE` to hedge against stackmap
/// base semantics (some unwinders/stackmap formats refer to "SP before pushing
/// the return address").
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SafepointContext {
  pub sp_entry: usize,
  pub sp_before_call: usize,
  pub fp: usize,
  pub ip: usize,
}

extern "C" {
  fn rt_capture_safepoint_context(out: *mut SafepointContext);
}

/// Capture a [`SafepointContext`] for the current call frame.
///
/// This is implemented in per-architecture assembly to ensure the captured
/// stack pointer and return address correspond to the callee's entry state (i.e.
/// before any Rust prologue can adjust the stack/frame pointers).
pub fn capture_safepoint_context() -> SafepointContext {
  let mut out = MaybeUninit::<SafepointContext>::uninit();
  // Safety: `rt_capture_safepoint_context` initializes the struct by writing
  // all fields.
  unsafe {
    rt_capture_safepoint_context(out.as_mut_ptr());
    out.assume_init()
  }
}

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
mod aarch64;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("runtime-native safepoint context capture is only supported on x86_64 and aarch64");

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn safepoint_context_layout_is_stable() {
    use std::mem::offset_of;

    assert_eq!(std::mem::size_of::<SafepointContext>(), 4 * WORD_SIZE);
    assert_eq!(offset_of!(SafepointContext, sp_entry), 0);
    assert_eq!(offset_of!(SafepointContext, sp_before_call), WORD_SIZE);
    assert_eq!(offset_of!(SafepointContext, fp), WORD_SIZE * 2);
    assert_eq!(offset_of!(SafepointContext, ip), WORD_SIZE * 3);
  }
}

