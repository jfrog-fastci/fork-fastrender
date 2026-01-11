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
