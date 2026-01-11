pub(crate) const FRAME_POINTER_ALIGNMENT: u64 = 1;
pub(crate) const FRAME_RECORD_SIZE: u64 = 16;

pub(crate) const RETURN_ADDRESS_OFFSET: u64 = 8;
pub(crate) const CALLER_SP_OFFSET: u64 = 16;

pub(crate) const DWARF_SP_REG: u16 = 0;
pub(crate) const DWARF_FP_REG: u16 = 0;
pub(crate) const FP_TO_ENTRY_SP_OFFSET: u64 = 0;

pub(crate) fn compute_sp(_fp: u64, _stack_size: u64) -> Option<u64> {
  None
}
