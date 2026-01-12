//! Architecture-specific helpers for GC/safepoint integration.
//!
//! The eventual precise GC integration uses LLVM statepoint-generated stack maps.
//! For each thread parked in a stop-the-world safepoint we need a stable snapshot
//! of the *caller* state at the safepoint call site (stack pointer + return
//! address) so the GC can locate the correct stack map record.

use std::mem::MaybeUninit;

/// Pointer-sized word width for the current target.
pub const WORD_SIZE: usize = std::mem::size_of::<usize>();

/// Register context captured at safepoints for stackmap evaluation and register-located roots.
///
/// This is an architecture-specific DWARF register file (GPRs + IP + SP) used by the stackmap
/// scanner.
///
/// Important: stackmap semantics define `DWARF_REG_SP`/`DWARF_REG_IP` in terms of the *callsite*
/// (i.e. the return address PC recorded in the stackmap), which may differ from the callee-entry
/// machine state (notably on x86_64 where `call` pushes the return address).
pub type RegContext = stackmap_context::ThreadContext;

/// Minimal execution context recorded for a thread parked in a safepoint.
///
/// The values here intentionally represent the state at the *call site* that
/// entered the runtime safepoint slow path.
///
/// # Stack pointer semantics (important!)
///
/// LLVM StackMaps encode stack slots as `Indirect [SP/FP + off]`.
///
/// For `Indirect [SP + off]`, `SP` is the *caller's* stack pointer value at the stackmap record PC
/// (i.e. the instruction **after** the call returns). FP-relative locations are evaluated from the
/// frame pointer chain.
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
  /// Pointer to the saved register file at the safepoint callsite.
  ///
  /// - This points into memory owned by the stopped thread (currently the stack frame created by
  ///   `rt_gc_safepoint_slow`).
  /// - It is only valid while the thread remains parked in the safepoint slow path.
  /// - GC root scanning treats `LocationKind::Register` locations as mutable lvalues inside this
  ///   register file.
  pub regs: *mut RegContext,
}

// SAFETY: `SafepointContext` is a POD snapshot of a thread's call-site machine state plus an
// optional raw pointer into that thread's saved register file.
//
// The pointer is only meaningful while the thread is parked in a stop-the-world safepoint. The GC
// already requires external synchronization (world stopped) before dereferencing it. Making the
// context `Send`/`Sync` allows the runtime to publish it in the global thread registry.
unsafe impl Send for SafepointContext {}
unsafe impl Sync for SafepointContext {}

extern "C" {
  fn runtime_native_capture_safepoint_context(out: *mut SafepointContext);
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
    runtime_native_capture_safepoint_context(out.as_mut_ptr());
    out.assume_init()
  }
}

#[cfg(target_arch = "x86_64")]
mod x86_64;

// AArch64 helpers are compiled on all targets so x86_64 tests can exercise the
// DWARF register mapping and stack-pointer reconstruction logic. Any
// architecture-specific assembly is gated inside the module.
pub mod aarch64;

/// Architecture-specific DWARF register helpers used by stack scanning.
///
/// This is a small shim over `stackmap-context` that:
/// - rejects SP/FP/IP as GC root registers, and
/// - provides pointer-to-slot access for register-located roots (`LocationKind::Register`).
pub mod regs {
  #[cfg(target_arch = "aarch64")]
  pub use super::aarch64::regs::*;
  #[cfg(target_arch = "x86_64")]
  pub use super::x86_64::regs::*;
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("runtime-native safepoint context capture is only supported on x86_64 and aarch64");

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn safepoint_context_layout_is_stable() {
    use std::mem::offset_of;

    assert_eq!(std::mem::size_of::<SafepointContext>(), 5 * WORD_SIZE);
    assert_eq!(offset_of!(SafepointContext, sp_entry), 0);
    assert_eq!(offset_of!(SafepointContext, sp), WORD_SIZE);
    assert_eq!(offset_of!(SafepointContext, fp), WORD_SIZE * 2);
    assert_eq!(offset_of!(SafepointContext, ip), WORD_SIZE * 3);
    assert_eq!(offset_of!(SafepointContext, regs), WORD_SIZE * 4);
  }
}
