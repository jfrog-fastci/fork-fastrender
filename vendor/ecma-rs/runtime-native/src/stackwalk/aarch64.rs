// AArch64 frame layout with frame pointers:
// Typical prologue:
//   stp x29, x30, [sp, #-16]!
//   mov x29, sp
//
// Frame record layout:
//   [FP + 0] = saved FP (caller frame pointer)
//   [FP + 8] = saved LR (return address)
// Caller SP at the return address is `FP + 16`.

pub(crate) const FRAME_POINTER_ALIGNMENT: u64 = 16;
pub(crate) const FRAME_RECORD_SIZE: u64 = 16;

pub(crate) const RETURN_ADDRESS_OFFSET: u64 = 8;
pub(crate) const CALLER_SP_OFFSET: u64 = 16;

// DWARF register numbers (AArch64 ELF).
pub(crate) const DWARF_SP_REG: u16 = 31;
pub(crate) const DWARF_FP_REG: u16 = 29;

/// Offset from the frame pointer (x29) to the stack pointer at function entry.
///
/// AArch64 CALL instructions do not push a return address onto the stack, so the entry SP equals
/// the caller's SP. With the standard frame-pointer prologue saving `{x29, x30}`:
///   `entry_sp = fp + 16`.
pub(crate) const FP_TO_ENTRY_SP_OFFSET: u64 = 16;

/// Compute the stack pointer value used as the base for SP-relative stackmap locations at a
/// safepoint within this frame.
///
/// Empirically (LLVM 18), the stackmap `stack_size` for AArch64 is the total SP delta, including
/// the fixed 16-byte `{fp, lr}` save area. Therefore:
///   `sp_at_safepoint = (fp + 16) - stack_size`
///
/// Note: `stack_size` does not account for per-callsite SP adjustments (e.g. outgoing stack
/// arguments), so this reconstruction is only valid when the callsite SP matches the function's
/// fixed frame size.
pub(crate) fn compute_sp(fp: u64, stack_size: u64) -> Option<u64> {
  fp.checked_add(FP_TO_ENTRY_SP_OFFSET)?.checked_sub(stack_size)
}
