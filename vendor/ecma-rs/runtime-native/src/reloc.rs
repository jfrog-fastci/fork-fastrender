//! Helpers for relocating pointers during moving GC.
//!
//! LLVM statepoint stackmaps represent `gc.relocate` results as **(base,
//! derived)** pairs. The derived pointer may point into the middle of an object,
//! so it must be updated relative to the relocated base:
//!
//! ```text
//! derived' = relocate(base) + (derived - base)
//! ```

/// Relocate an interior/derived pointer described by LLVM's stackmaps.
///
/// `base_slot` and `derived_slot` point to stack slots (or register spill slots)
/// containing the *current* base/derived pointers.
///
/// If the base pointer is null (`0`), both slots are set to `0`.
///
/// # Derived-pointer rule
///
/// Moving GCs must **not** treat the derived pointer as an independent root.
/// Instead, relocate the base pointer and recompute the derived pointer by
/// applying the original offset.
pub fn relocate_derived_pair(
  base_slot: *mut usize,
  derived_slot: *mut usize,
  mut relocate_base: impl FnMut(usize) -> usize,
) {
  // SAFETY: This function is a low-level runtime helper. Callers must pass
  // valid, writable slots.
  //
  // We keep this as a safe function because the runtime (and stackmap walking)
  // is already fundamentally unsafe; the raw-pointer signature matches the
  // stackmap representation.
  unsafe {
    let base = *base_slot;
    let derived = *derived_slot;

    if base == 0 {
      *base_slot = 0;
      *derived_slot = 0;
      return;
    }

    let delta = (derived as isize) - (base as isize);
    let new_base = relocate_base(base);

    *base_slot = new_base;
    *derived_slot = (new_base as isize + delta) as usize;
  }
}

