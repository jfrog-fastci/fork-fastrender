// x86_64 SysV frame layout with frame pointers:
//   [RBP + 0] = saved RBP (caller frame pointer)
//   [RBP + 8] = return address
// Caller SP at the return address is `RBP + 16`.

pub(crate) const FRAME_POINTER_ALIGNMENT: u64 = 16;
pub(crate) const FRAME_RECORD_SIZE: u64 = 16;

pub(crate) const RETURN_ADDRESS_OFFSET: u64 = 8;
pub(crate) const CALLER_SP_OFFSET: u64 = 16;

// DWARF register numbers (SysV x86_64).
pub(crate) const DWARF_SP_REG: u16 = 7;
pub(crate) const DWARF_FP_REG: u16 = 6;

/// Offset from the frame pointer to the stack pointer at function entry.
///
/// For x86_64 this is the stack pointer after the CALL instruction pushes the return address, but
/// before `push rbp` in the callee prologue.
pub(crate) const FP_TO_ENTRY_SP_OFFSET: u64 = 8;

/// Compute the stack pointer value used as the base for SP-relative stackmap locations at a
/// safepoint within this frame.
///
/// LLVM's stackmap `stack_size` is the total SP delta from function entry. With frame pointers
/// enabled on x86_64:
///   `sp_at_safepoint = (fp + FP_TO_ENTRY_SP_OFFSET) - stack_size`
pub(crate) fn compute_sp(fp: u64, stack_size: u64) -> Option<u64> {
  fp.checked_add(FP_TO_ENTRY_SP_OFFSET)?.checked_sub(stack_size)
}
