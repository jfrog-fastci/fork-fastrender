//! Helpers for relocating pointers during moving GC.
//!
//! LLVM statepoint stackmaps represent `gc.relocate` results as **(base,
//! derived)** pairs. The derived pointer may point into the middle of an object,
//! so it must be updated relative to the relocated base:
//!
//! ```text
//! derived' = relocate(base) + (derived - base)
//! ```

pub use crate::gc_roots::relocate_derived_pairs;

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
  if base_slot.is_null() || derived_slot.is_null() {
    debug_assert!(false, "relocate_derived_pair received null slot pointer");
    return;
  }

  // SAFETY: callers must provide valid, writable slots.
  unsafe {
    let base = base_slot.read_unaligned();
    let derived = derived_slot.read_unaligned();

    if base == 0 {
      base_slot.write_unaligned(0);
      derived_slot.write_unaligned(0);
      return;
    }

    let new_base = relocate_base(base);

    base_slot.write_unaligned(new_base);

    // Preserve null derived values, and keep the derived slot null if the base becomes null.
    if derived == 0 || new_base == 0 {
      derived_slot.write_unaligned(0);
      return;
    }

    // Derived relocation is defined as: `new_derived = new_base + (derived_old - base_old)`.
    //
    // Use wrapping arithmetic so this is safe even if the interior-pointer delta would not fit in
    // `isize` (debug overflow checks) or if the derived pointer happens to be below the base.
    let delta = derived.wrapping_sub(base);
    derived_slot.write_unaligned(new_base.wrapping_add(delta));
  }
}
