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
/// # Stack pointer semantics (important!)
///
/// LLVM StackMaps encode stack slots as `Indirect [SP + off]`, where `SP` is the
/// *caller's* stack pointer value at the stackmap record PC (i.e. the
/// instruction **after** the call returns).
///
/// When a thread is stopped *inside* the safepoint callee, the callee-entry SP
/// differs from the stackmap SP on some architectures:
///
/// - **x86_64 SysV**: `call` pushes the 8-byte return address.
///   - `sp_entry` points at the return address.
///   - `sp` is the **post-call** SP expected by stackmaps: `sp = sp_entry + 8`.
/// - **AArch64**: `bl` does not push a return address.
///   - `sp_entry == sp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SafepointContext {
  pub sp_entry: usize,
  /// Stack pointer value used by LLVM stackmaps (`SP` at the safepoint PC).
  pub sp: usize,
  pub fp: usize,
  pub ip: usize,
}

extern "C" {
  fn rt_capture_safepoint_context(out: *mut SafepointContext);
}

/// Capture a [`SafepointContext`] describing the *callsite* that entered a runtime
/// helper just before the current thread will block / remain quiescent.
///
/// # Captured frame contract
/// The returned context's `fp` and `ip` are **not** for this Rust function nor for
/// the internal assembly helper. Instead, they are captured from the *outer*
/// caller frame so the published context stays live even after the runtime helper
/// returns.
///
/// Concretely, when called from a runtime helper like:
/// ```text
/// outer() -> runtime_helper() -> arch::capture_safepoint_context()
/// ```
/// we capture:
/// - `ctx.fp`: the frame pointer for `outer()`
/// - `ctx.ip`: the return address in `outer()` after the call to `runtime_helper()`
///
/// This ensures `SafepointContext` always refers to a **stable** frame that remains
/// on the stack while the thread is stopped / NativeSafe.
///
/// `sp_entry`/`sp` are best-effort and may not correspond to the exact
/// callsite stack pointer when this function is not invoked as a leaf; current
/// stackmap-based scanning relies on `fp`/`ip`.
#[inline(never)]
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
    assert_eq!(offset_of!(SafepointContext, sp), WORD_SIZE);
    assert_eq!(offset_of!(SafepointContext, fp), WORD_SIZE * 2);
    assert_eq!(offset_of!(SafepointContext, ip), WORD_SIZE * 3);
  }
}
