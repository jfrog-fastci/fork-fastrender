use core::fmt;

/// Returned by [`ThreadContext::set_dwarf_reg_u64`] when the DWARF register
/// number is not supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedDwarfRegister(pub u16);

impl fmt::Display for UnsupportedDwarfRegister {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "unsupported DWARF register number: {}", self.0)
  }
}

impl std::error::Error for UnsupportedDwarfRegister {}

#[cfg(target_arch = "aarch64")]
#[path = "arch_aarch64.rs"]
mod arch_aarch64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[path = "arch_unsupported.rs"]
mod arch_unsupported;
#[cfg(target_arch = "x86_64")]
#[path = "arch_x86_64.rs"]
mod arch_x86_64;

#[cfg(target_arch = "aarch64")]
pub use arch_aarch64::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub use arch_unsupported::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};
#[cfg(target_arch = "x86_64")]
pub use arch_x86_64::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};
