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
