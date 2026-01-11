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

