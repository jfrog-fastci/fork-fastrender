use crate::context::UnsupportedDwarfRegister;

/// DWARF register number for the stack pointer (SP).
pub const DWARF_REG_SP: u16 = 31;
/// DWARF register number for the program counter (PC).
pub const DWARF_REG_IP: u16 = 32;

/// Linux `aarch64` general purpose register state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadContext {
  /// X0..X30.
  pub x: [u64; 31],
  pub sp: u64,
  pub pc: u64,
}

impl Default for ThreadContext {
  fn default() -> Self {
    Self {
      x: [0u64; 31],
      sp: 0,
      pc: 0,
    }
  }
}

impl ThreadContext {
  /// Returns the value of a DWARF register number, or `None` if unsupported.
  pub fn get_dwarf_reg_u64(&self, reg: u16) -> Option<u64> {
    match reg {
      0..=30 => Some(self.x[reg as usize]),
      31 => Some(self.sp),
      32 => Some(self.pc),
      _ => None,
    }
  }

  /// Sets the value of a DWARF register number.
  pub fn set_dwarf_reg_u64(&mut self, reg: u16, val: u64) -> Result<(), UnsupportedDwarfRegister> {
    match reg {
      0..=30 => self.x[reg as usize] = val,
      31 => self.sp = val,
      32 => self.pc = val,
      _ => return Err(UnsupportedDwarfRegister(reg)),
    }
    Ok(())
  }

  /// Builds a [`ThreadContext`] from a Linux `ucontext_t` (e.g. from a signal handler).
  #[cfg(target_os = "linux")]
  pub unsafe fn from_ucontext(uc: *const libc::ucontext_t) -> ThreadContext {
    debug_assert!(!uc.is_null());
    let mctx = &(*uc).uc_mcontext;
    ThreadContext {
      x: mctx.regs,
      sp: mctx.sp,
      pc: mctx.pc,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};
  use crate::context::UnsupportedDwarfRegister;

  #[test]
  fn constants_match_abi() {
    assert_eq!(DWARF_REG_SP, 31);
    assert_eq!(DWARF_REG_IP, 32);
  }

  #[test]
  fn dwarf_register_mapping_round_trips() {
    let mut ctx = ThreadContext::default();
    for i in 0..31 {
      ctx.x[i] = 0x1000 + i as u64;
    }
    ctx.sp = 0x2000;
    ctx.pc = 0x3000;

    for i in 0..31u16 {
      assert_eq!(ctx.get_dwarf_reg_u64(i), Some(0x1000 + i as u64));
    }
    assert_eq!(ctx.get_dwarf_reg_u64(31), Some(0x2000));
    assert_eq!(ctx.get_dwarf_reg_u64(32), Some(0x3000));
    assert_eq!(ctx.get_dwarf_reg_u64(33), None);

    ctx.set_dwarf_reg_u64(0, 0xaaaa).unwrap();
    assert_eq!(ctx.x[0], 0xaaaa);
    assert_eq!(
      ctx.set_dwarf_reg_u64(999, 0),
      Err(UnsupportedDwarfRegister(999))
    );
  }
}
