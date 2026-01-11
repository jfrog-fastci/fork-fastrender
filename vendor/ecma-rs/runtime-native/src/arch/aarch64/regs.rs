/// AArch64 general purpose register snapshot captured at a safepoint.
///
/// The layout is intentionally simple so the assembly safepoint stub can spill
/// registers directly into this struct using fixed offsets.
///
/// Notes:
/// - LLVM stackmaps use **DWARF register numbers**. On AArch64 these are:
///   - `0..=30` for `x0..x30`
///   - `31` for `sp`
///   - `32` for `pc` (not currently exposed via [`RegContext::reg_slot_ptr`])
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RegContext {
  /// `x0..x30`.
  pub x: [u64; 31],
  pub sp: u64,
  pub pc: u64,
}

impl RegContext {
  /// Returns a mutable pointer to the storage slot for a DWARF register.
  ///
  /// Mapping:
  /// - `0..=30` => `x0..x30`
  /// - `31` => `sp`
  ///
  /// (DWARF `32` => `pc` is intentionally omitted for now; stackmap GC roots are
  /// expected to be described in GP registers and stack slots.)
  pub fn reg_slot_ptr(&mut self, dwarf_reg: u16) -> Option<*mut u64> {
    match dwarf_reg {
      0..=30 => Some(self.x.as_mut_ptr().wrapping_add(dwarf_reg as usize)),
      31 => Some(&mut self.sp as *mut u64),
      _ => None,
    }
  }
}

/// DWARF register number for the frame pointer (X29).
pub const DWARF_REG_FP: u16 = 29;
/// DWARF register number for the stack pointer (SP).
pub const DWARF_REG_SP: u16 = 31;
/// DWARF register number for the program counter (PC).
pub const DWARF_REG_IP: u16 = 32;

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

/// Maps a DWARF register number to a mutable pointer-sized slot inside the runtime's saved register
/// file (`stackmap_context::ThreadContext`).
///
/// Returns `None` if `dwarf_reg` is not part of the supported DWARF GPR set.
///
/// Note: this does **not** apply the GC-root forbidden-reg filter; callers should use
/// [`forbidden_gc_root_reg`] to reject SP/FP/IP.
#[cfg(target_arch = "aarch64")]
#[inline]
pub unsafe fn reg_slot_ptr(regs: *mut crate::arch::RegContext, dwarf_reg: u16) -> Option<*mut usize> {
  let regs = regs.as_mut()?;
  Some(match dwarf_reg {
    0..=30 => regs.x.as_mut_ptr().add(dwarf_reg as usize).cast::<usize>(),
    31 => (&mut regs.sp as *mut u64).cast::<usize>(),
    32 => (&mut regs.pc as *mut u64).cast::<usize>(),
    _ => return None,
  })
}
