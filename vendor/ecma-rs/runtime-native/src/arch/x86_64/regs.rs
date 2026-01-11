use crate::arch::RegContext;

/// DWARF register number for the frame pointer (RBP).
pub const DWARF_REG_FP: u16 = 6;
/// DWARF register number for the stack pointer (RSP).
pub const DWARF_REG_SP: u16 = 7;
/// DWARF register number for the instruction pointer (RIP).
pub const DWARF_REG_IP: u16 = 16;

/// Returns `Some(kind)` if `dwarf_reg` is not a valid register for a GC root.
///
/// Under our frame-pointer stack walking policy, SP/FP/IP are never treated as GC roots:
/// - SP/FP are used to address stack slots.
/// - IP is the return PC / stackmap lookup key.
#[inline]
pub fn forbidden_gc_root_reg(dwarf_reg: u16) -> Option<&'static str> {
  match dwarf_reg {
    DWARF_REG_SP => Some("SP"),
    DWARF_REG_FP => Some("FP"),
    DWARF_REG_IP => Some("IP"),
    _ => None,
  }
}

/// Maps a DWARF register number to a mutable pointer-sized slot inside `RegContext`.
///
/// Returns `None` if `dwarf_reg` is not part of the supported DWARF GPR set.
///
/// Note: this does **not** apply the GC-root forbidden-reg filter; callers should use
/// [`forbidden_gc_root_reg`] to reject SP/FP/IP.
#[inline]
pub unsafe fn reg_slot_ptr(regs: *mut RegContext, dwarf_reg: u16) -> Option<*mut usize> {
  let regs = regs.as_mut()?;
  Some(match dwarf_reg {
    0 => (&mut regs.rax as *mut u64).cast::<usize>(),
    1 => (&mut regs.rdx as *mut u64).cast::<usize>(),
    2 => (&mut regs.rcx as *mut u64).cast::<usize>(),
    3 => (&mut regs.rbx as *mut u64).cast::<usize>(),
    4 => (&mut regs.rsi as *mut u64).cast::<usize>(),
    5 => (&mut regs.rdi as *mut u64).cast::<usize>(),
    6 => (&mut regs.rbp as *mut u64).cast::<usize>(),
    7 => (&mut regs.rsp as *mut u64).cast::<usize>(),
    8 => (&mut regs.r8 as *mut u64).cast::<usize>(),
    9 => (&mut regs.r9 as *mut u64).cast::<usize>(),
    10 => (&mut regs.r10 as *mut u64).cast::<usize>(),
    11 => (&mut regs.r11 as *mut u64).cast::<usize>(),
    12 => (&mut regs.r12 as *mut u64).cast::<usize>(),
    13 => (&mut regs.r13 as *mut u64).cast::<usize>(),
    14 => (&mut regs.r14 as *mut u64).cast::<usize>(),
    15 => (&mut regs.r15 as *mut u64).cast::<usize>(),
    16 => (&mut regs.rip as *mut u64).cast::<usize>(),
    _ => return None,
  })
}

