//! Helpers for relocating pointers during moving GC.
//!
//! LLVM statepoint stackmaps represent `gc.relocate` results as **(base,
//! derived)** pairs. The derived pointer may point into the middle of an object,
//! so it must be updated relative to the relocated base:
//!
//! ```text
//! derived' = relocate(base) + (derived - base)
//! ```

use std::collections::hash_map::Entry;
use std::collections::HashMap;

/// Relocate LLVM statepoint-derived `(base_slot, derived_slot)` pairs in a single batch.
///
/// LLVM stackmaps can reuse the same **base spill slot** across multiple pairs when multiple
/// derived pointers share a common base. Relocating pairs one-by-one is therefore incorrect: once
/// `*base_slot` is overwritten, later pairs would compute the derived delta from the relocated base
/// rather than the original base.
///
/// This helper snapshots all old values first, computes `new_base` once per unique base slot, then
/// writes back the relocated base and derived values.
pub fn relocate_derived_pairs(
  pairs: &[(
    /* base_slot */ *mut usize,
    /* derived_slot */ *mut usize,
  )],
  mut relocate_base: impl FnMut(usize) -> usize,
) {
  #[derive(Clone, Copy)]
  struct PairSnapshot {
    base_slot: *mut usize,
    derived_slot: *mut usize,
    base_old: usize,
    derived_old: usize,
  }

  #[derive(Clone, Copy)]
  struct BaseInfo {
    base_old: usize,
    base_new: usize,
  }

  let mut pair_snapshots: Vec<PairSnapshot> = Vec::with_capacity(pairs.len());
  let mut base_map: HashMap<*mut usize, BaseInfo> = HashMap::new();

  // Phase 1: snapshot all old values.
  for &(base_slot, derived_slot) in pairs {
    debug_assert!(
      !base_slot.is_null(),
      "relocate_derived_pairs received null base_slot pointer"
    );
    debug_assert!(
      !derived_slot.is_null(),
      "relocate_derived_pairs received null derived_slot pointer"
    );

    // SAFETY: callers must provide valid, writable slots.
    let base_old = unsafe { base_slot.read_unaligned() };
    let derived_old = unsafe { derived_slot.read_unaligned() };

    pair_snapshots.push(PairSnapshot {
      base_slot,
      derived_slot,
      base_old,
      derived_old,
    });

    match base_map.entry(base_slot) {
      Entry::Vacant(e) => {
        let base_new = if base_old == 0 { 0 } else { relocate_base(base_old) };
        e.insert(BaseInfo { base_old, base_new });
      }
      Entry::Occupied(e) => {
        debug_assert_eq!(
          e.get().base_old,
          base_old,
          "base_slot value changed while snapshotting relocation pairs"
        );
      }
    }
  }

  // Phase 2: write relocated bases.
  for (&base_slot, base_info) in &base_map {
    unsafe { base_slot.write_unaligned(base_info.base_new) };
  }

  // Phase 3: write relocated derived values using snapshotted deltas.
  for pair in pair_snapshots {
    let base_new = base_map
      .get(&pair.base_slot)
      .expect("base_slot missing from relocation map")
      .base_new;

    let derived_new = if pair.base_old == 0 || pair.derived_old == 0 || base_new == 0 {
      0
    } else {
      let delta = pair.derived_old.wrapping_sub(pair.base_old);
      base_new.wrapping_add(delta)
    };

    unsafe { pair.derived_slot.write_unaligned(derived_new) };
  }
}

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
