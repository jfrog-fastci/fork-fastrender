use crate::context::UnsupportedDwarfRegister;

/// DWARF register number for the stack pointer (RSP).
pub const DWARF_REG_SP: u16 = 7;
/// DWARF register number for the instruction pointer (RIP).
pub const DWARF_REG_IP: u16 = 16;

/// Linux `x86_64` general purpose register state.
///
/// The field set intentionally matches the DWARF GPR set used by LLVM stackmaps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadContext {
  pub rax: u64,
  pub rdx: u64,
  pub rcx: u64,
  pub rbx: u64,
  pub rsi: u64,
  pub rdi: u64,
  pub rbp: u64,
  pub rsp: u64,
  pub r8: u64,
  pub r9: u64,
  pub r10: u64,
  pub r11: u64,
  pub r12: u64,
  pub r13: u64,
  pub r14: u64,
  pub r15: u64,
  pub rip: u64,
}

impl ThreadContext {
  /// Returns the value of a DWARF register number, or `None` if unsupported.
  pub fn get_dwarf_reg_u64(&self, reg: u16) -> Option<u64> {
    Some(match reg {
      0 => self.rax,
      1 => self.rdx,
      2 => self.rcx,
      3 => self.rbx,
      4 => self.rsi,
      5 => self.rdi,
      6 => self.rbp,
      7 => self.rsp,
      8 => self.r8,
      9 => self.r9,
      10 => self.r10,
      11 => self.r11,
      12 => self.r12,
      13 => self.r13,
      14 => self.r14,
      15 => self.r15,
      16 => self.rip,
      _ => return None,
    })
  }

  /// Sets the value of a DWARF register number.
  pub fn set_dwarf_reg_u64(&mut self, reg: u16, val: u64) -> Result<(), UnsupportedDwarfRegister> {
    match reg {
      0 => self.rax = val,
      1 => self.rdx = val,
      2 => self.rcx = val,
      3 => self.rbx = val,
      4 => self.rsi = val,
      5 => self.rdi = val,
      6 => self.rbp = val,
      7 => self.rsp = val,
      8 => self.r8 = val,
      9 => self.r9 = val,
      10 => self.r10 = val,
      11 => self.r11 = val,
      12 => self.r12 = val,
      13 => self.r13 = val,
      14 => self.r14 = val,
      15 => self.r15 = val,
      16 => self.rip = val,
      _ => return Err(UnsupportedDwarfRegister(reg)),
    }
    Ok(())
  }

  /// Builds a [`ThreadContext`] from a Linux `ucontext_t` (e.g. from a signal handler).
  #[cfg(target_os = "linux")]
  pub unsafe fn from_ucontext(uc: *const libc::ucontext_t) -> ThreadContext {
    debug_assert!(!uc.is_null());
    let gregs = (*uc).uc_mcontext.gregs;

    ThreadContext {
      r8: gregs[libc::REG_R8 as usize] as u64,
      r9: gregs[libc::REG_R9 as usize] as u64,
      r10: gregs[libc::REG_R10 as usize] as u64,
      r11: gregs[libc::REG_R11 as usize] as u64,
      r12: gregs[libc::REG_R12 as usize] as u64,
      r13: gregs[libc::REG_R13 as usize] as u64,
      r14: gregs[libc::REG_R14 as usize] as u64,
      r15: gregs[libc::REG_R15 as usize] as u64,
      rdi: gregs[libc::REG_RDI as usize] as u64,
      rsi: gregs[libc::REG_RSI as usize] as u64,
      rbp: gregs[libc::REG_RBP as usize] as u64,
      rbx: gregs[libc::REG_RBX as usize] as u64,
      rdx: gregs[libc::REG_RDX as usize] as u64,
      rax: gregs[libc::REG_RAX as usize] as u64,
      rcx: gregs[libc::REG_RCX as usize] as u64,
      rsp: gregs[libc::REG_RSP as usize] as u64,
      rip: gregs[libc::REG_RIP as usize] as u64,
    }
  }

  /// Writes this [`ThreadContext`] back into a Linux `ucontext_t`.
  ///
  /// This is required when a runtime (or test) wants to **rewrite registers** for a
  /// stopped thread (e.g. updating register-located GC roots) before resuming
  /// execution from a signal handler.
  #[cfg(target_os = "linux")]
  pub unsafe fn write_to_ucontext(&self, uc: *mut libc::ucontext_t) {
    debug_assert!(!uc.is_null());
    let gregs = &mut (*uc).uc_mcontext.gregs;

    gregs[libc::REG_R8 as usize] = self.r8 as libc::greg_t;
    gregs[libc::REG_R9 as usize] = self.r9 as libc::greg_t;
    gregs[libc::REG_R10 as usize] = self.r10 as libc::greg_t;
    gregs[libc::REG_R11 as usize] = self.r11 as libc::greg_t;
    gregs[libc::REG_R12 as usize] = self.r12 as libc::greg_t;
    gregs[libc::REG_R13 as usize] = self.r13 as libc::greg_t;
    gregs[libc::REG_R14 as usize] = self.r14 as libc::greg_t;
    gregs[libc::REG_R15 as usize] = self.r15 as libc::greg_t;
    gregs[libc::REG_RDI as usize] = self.rdi as libc::greg_t;
    gregs[libc::REG_RSI as usize] = self.rsi as libc::greg_t;
    gregs[libc::REG_RBP as usize] = self.rbp as libc::greg_t;
    gregs[libc::REG_RBX as usize] = self.rbx as libc::greg_t;
    gregs[libc::REG_RDX as usize] = self.rdx as libc::greg_t;
    gregs[libc::REG_RAX as usize] = self.rax as libc::greg_t;
    gregs[libc::REG_RCX as usize] = self.rcx as libc::greg_t;
    gregs[libc::REG_RSP as usize] = self.rsp as libc::greg_t;
    gregs[libc::REG_RIP as usize] = self.rip as libc::greg_t;
  }
}

#[cfg(test)]
mod tests {
  use super::ThreadContext;
  use crate::context::UnsupportedDwarfRegister;
  use core::mem::MaybeUninit;

  #[test]
  fn dwarf_register_mapping_round_trips() {
    let mut ctx = ThreadContext {
      rax: 0x00,
      rdx: 0x01,
      rcx: 0x02,
      rbx: 0x03,
      rsi: 0x04,
      rdi: 0x05,
      rbp: 0x06,
      rsp: 0x07,
      r8: 0x08,
      r9: 0x09,
      r10: 0x0a,
      r11: 0x0b,
      r12: 0x0c,
      r13: 0x0d,
      r14: 0x0e,
      r15: 0x0f,
      rip: 0x10,
    };

    for (reg, expected) in [
      (0, ctx.rax),
      (1, ctx.rdx),
      (2, ctx.rcx),
      (3, ctx.rbx),
      (4, ctx.rsi),
      (5, ctx.rdi),
      (6, ctx.rbp),
      (7, ctx.rsp),
      (8, ctx.r8),
      (9, ctx.r9),
      (10, ctx.r10),
      (11, ctx.r11),
      (12, ctx.r12),
      (13, ctx.r13),
      (14, ctx.r14),
      (15, ctx.r15),
      (16, ctx.rip),
    ] {
      assert_eq!(ctx.get_dwarf_reg_u64(reg).unwrap(), expected);
    }

    assert_eq!(ctx.get_dwarf_reg_u64(17), None);

    ctx.set_dwarf_reg_u64(6, 0xdead_beef).unwrap();
    assert_eq!(ctx.rbp, 0xdead_beef);
    assert_eq!(ctx.get_dwarf_reg_u64(6), Some(0xdead_beef));

    assert_eq!(
      ctx.set_dwarf_reg_u64(999, 0),
      Err(UnsupportedDwarfRegister(999))
    );
  }

  #[test]
  #[cfg(target_os = "linux")]
  fn write_to_ucontext_updates_gregs() {
    unsafe {
      let mut uc = MaybeUninit::<libc::ucontext_t>::uninit();
      assert_eq!(libc::getcontext(uc.as_mut_ptr()), 0);
      let mut uc = uc.assume_init();

      let mut ctx = ThreadContext::from_ucontext(&uc);
      ctx.rax = 0xdead_beef;
      ctx.rdi = 0xfeed_face;
      ctx.write_to_ucontext(&mut uc);

      let ctx2 = ThreadContext::from_ucontext(&uc);
      assert_eq!(ctx2.rax, 0xdead_beef);
      assert_eq!(ctx2.rdi, 0xfeed_face);
    }
  }
}
