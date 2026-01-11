use crate::context::UnsupportedDwarfRegister;

/// Placeholder: unsupported target.
///
/// The `stackmap-context` crate is currently implemented for Linux `x86_64` and
/// Linux `aarch64`. This stub exists so the crate can still be built on other
/// targets in the workspace.
pub const DWARF_REG_SP: u16 = 0;
/// Placeholder: unsupported target.
pub const DWARF_REG_IP: u16 = 0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadContext;

impl ThreadContext {
  pub fn get_dwarf_reg_u64(&self, _reg: u16) -> Option<u64> {
    None
  }

  pub fn set_dwarf_reg_u64(&mut self, reg: u16, _val: u64) -> Result<(), UnsupportedDwarfRegister> {
    Err(UnsupportedDwarfRegister(reg))
  }

  #[cfg(target_os = "linux")]
  pub unsafe fn from_ucontext(_uc: *const libc::ucontext_t) -> ThreadContext {
    ThreadContext
  }
}
