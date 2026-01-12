//! Stackmap/statepoint GC root relocation helpers.
//!
//! LLVM `gc.statepoint` encodes GC "live" pointers as `(base, derived)` pairs. When `base !=
//! derived`, the derived value is an *interior pointer* into the base object.
//!
//! A relocating GC must:
//! - relocate the base pointer, and
//! - update the derived pointer to preserve the interior offset:
//!   `new_derived = new_base + (old_derived - old_base)`.
//!
//! ## Why this module exists
//!
//! Stackmaps can contain **repeated base slots**:
//! - a base pointer can appear as its own `(base, derived)` pair (`base == derived`), and
//! - multiple derived pointers can reference the same base slot.
//!
//! Updating slots in-place in a single pass is incorrect if the same `base_slot` is encountered
//! multiple times: once `*base_slot` is overwritten, subsequent pairs would compute the derived
//! offset against the *new* base rather than the original one.
//!
//! [`relocate_reloc_pairs_in_place`] (and the raw-slot helper [`relocate_derived_pairs`]) implement
//! a two-phase algorithm that is robust to this.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use crate::stack_walk::{FpWalkError, FrameView, StackWalker};
use crate::stackmaps::{CallSite, Location, StackMaps};
use crate::stackwalk::{StackBounds, DWARF_FP_REG, DWARF_SP_REG};
use crate::statepoints::{eval_location, RegFile, RootSlot};
use stackmap_context::ThreadContext;

/// A `(base, derived)` relocation pair.
///
/// `base_slot` and `derived_slot` identify mutable locations that contain pointer-sized values:
/// either addressable stack slots or registers (DWARF register numbers).
///
/// A value of `0` in either slot is treated as a null pointer.
#[derive(Clone, Copy, Debug)]
pub struct RelocPair {
  pub base_slot: RootSlot,
  pub derived_slot: RootSlot,
}

impl RelocPair {
  /// Updates the derived slot after the base slot has been updated to `new_base`.
  ///
  /// This preserves the interior-pointer offset:
  /// `offset = old_derived - old_base`
  /// `new_derived = new_base + offset`
  ///
  /// Note: if the same `base_slot` can appear in multiple relocation pairs, callers must ensure
  /// that `old_base` is the *original* base value (before any in-place updates). If you do not
  /// have that guarantee, use [`relocate_reloc_pairs_in_place`] instead.
  ///
  /// # Safety
  /// The slots must be valid and writable.
  pub unsafe fn update_derived_after_base_moved(
    &self,
    ctx: &mut ThreadContext,
    old_base: usize,
    new_base: usize,
  ) {
    let old_derived = self.derived_slot.read_u64(ctx) as usize;

    // Preserve nulls to avoid underflow and to treat null pointers as non-roots.
    if old_base == 0 || old_derived == 0 || new_base == 0 {
      self.derived_slot.write_u64(ctx, 0);
      return;
    }

    let delta = old_derived.wrapping_sub(old_base);
    let new_derived = new_base.wrapping_add(delta);
    self.derived_slot.write_u64(ctx, new_derived as u64);
  }
}

/// Apply stackmap-derived relocation pairs with correct handling for repeated base slots.
///
/// This is intended to be used by a moving GC when updating roots in-place. `relocate` is the GC
/// primitive that maps an *old* object pointer to its *new* object pointer.
///
/// ## Null pointers
/// A slot value of `0` is treated as null and is not passed to `relocate`.
///
/// ## Safety
/// Callers must ensure that every `RootSlot::StackAddr` in `pairs` is a valid, writable pointer for
/// the duration of the call, and that `ctx` contains a complete register file for any
/// `RootSlot::Reg` entries.
pub fn relocate_reloc_pairs_in_place(
  ctx: &mut ThreadContext,
  pairs: impl IntoIterator<Item = RelocPair>,
  mut relocate: impl FnMut(usize) -> usize,
) {
  // Phase 1: snapshot all old slot values without mutating anything.
  //
  // Cache at least `old_base` per unique `base_slot` so repeated base slots don't observe our own
  // in-place writes.
  let mut old_base_by_slot: HashMap<RootSlot, usize> = HashMap::new();
  let mut old_derived_by_slot: HashMap<RootSlot, usize> = HashMap::new();
  let mut pairs_vec: Vec<RelocPair> = Vec::new();

  for pair in pairs {
    // Invalid stack slot pointers are a caller bug; skip to avoid UB.
    if matches!(pair.base_slot, RootSlot::StackAddr(p) if p.is_null())
      || matches!(pair.derived_slot, RootSlot::StackAddr(p) if p.is_null())
    {
      debug_assert!(false, "relocate_reloc_pairs_in_place received null stack slot pointer");
      continue;
    }

    let old_base = pair.base_slot.read_u64(ctx) as usize;
    let old_derived = pair.derived_slot.read_u64(ctx) as usize;

    match old_base_by_slot.entry(pair.base_slot) {
      Entry::Vacant(e) => {
        e.insert(old_base);
      }
      Entry::Occupied(e) => {
        debug_assert_eq!(
          *e.get(),
          old_base,
          "base_slot value changed while snapshotting relocation pairs"
        );
      }
    }

    match old_derived_by_slot.entry(pair.derived_slot) {
      Entry::Vacant(e) => {
        e.insert(old_derived);
      }
      Entry::Occupied(e) => {
        debug_assert_eq!(
          *e.get(),
          old_derived,
          "derived_slot value changed while snapshotting relocation pairs"
        );
      }
    }

    pairs_vec.push(pair);
  }

  // Phase 2: relocate each unique base value once.
  //
  // Even though stackmaps can contain repeated *base slots*, they can also contain duplicated
  // *values* across distinct slots (e.g. if LLVM spills the same pointer into multiple stack slots).
  // Cache by value to avoid redundant relocation work.
  let mut new_base_by_value: HashMap<usize, usize> = HashMap::new();
  let mut new_base_by_slot: HashMap<RootSlot, usize> = HashMap::with_capacity(old_base_by_slot.len());
  for (&slot, &old_base) in &old_base_by_slot {
    let new_base = if old_base == 0 {
      0
    } else {
      match new_base_by_value.entry(old_base) {
        Entry::Occupied(e) => *e.get(),
        Entry::Vacant(e) => {
          let new_base = relocate(old_base);
          e.insert(new_base);
          new_base
        }
      }
    };
    new_base_by_slot.insert(slot, new_base);
  }

  // Phase 3a: write relocated base pointers.
  for (&slot, &new_base) in &new_base_by_slot {
    slot.write_u64(ctx, new_base as u64);
  }

  // Phase 3b: write relocated derived pointers using the *snapshotted* old values.
  for pair in pairs_vec {
    let old_base = *old_base_by_slot
      .get(&pair.base_slot)
      .expect("base_slot missing from snapshot map");
    let new_base = *new_base_by_slot
      .get(&pair.base_slot)
      .expect("base_slot missing from relocated map");
    let old_derived = *old_derived_by_slot
      .get(&pair.derived_slot)
      .expect("derived_slot missing from snapshot map");

    // Null convention: treat 0 as null, skip relocation.
    //
    // Note: `relocate` should never return `0` for a live (non-null) base pointer, but keep the
    // derived slot consistent with the base slot if it does.
    let new_derived = if old_base == 0 || old_derived == 0 || new_base == 0 {
      0
    } else {
      let delta = old_derived.wrapping_sub(old_base);
      new_base.wrapping_add(delta)
    };

    pair.derived_slot.write_u64(ctx, new_derived as u64);
  }
}

/// Relocate stack-slot `(base_slot, derived_slot)` pairs in a single batch.
///
/// This is the raw-pointer version of [`relocate_reloc_pairs_in_place`]. It exists for scanners
/// that only report addressable stack slots (no register roots).
///
/// LLVM stackmaps encode `gc.relocate` results as `(base, derived)` pairs. The same *base slot*
/// may appear in multiple pairs when multiple derived pointers share a base (and when the base
/// itself is relocated via a `base == derived` pair). Derived relocation must therefore be done
/// in a batch: snapshot all old values first, then write updates.
///
/// ## Null pointers
/// - If the base slot's old value is `0`, the derived slot is written as `0`.
/// - If the derived slot's old value is `0`, it remains `0`.
///
/// ## Safety
/// The caller must ensure `base_slot` and `derived_slot` pointers are valid and writable for the
/// duration of the call.
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

  // Phase 1: snapshot all old values without mutating any slots.
  let mut pair_snapshots: Vec<PairSnapshot> = Vec::with_capacity(pairs.len());
  let mut base_map: HashMap<*mut usize, BaseInfo> = HashMap::new();
  let mut relocated_by_value: HashMap<usize, usize> = HashMap::new();

  for &(base_slot, derived_slot) in pairs {
    debug_assert!(
      !base_slot.is_null(),
      "relocate_derived_pairs received null base_slot pointer"
    );
    debug_assert!(
      !derived_slot.is_null(),
      "relocate_derived_pairs received null derived_slot pointer"
    );

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
        let base_new = if base_old == 0 {
          0
        } else {
          match relocated_by_value.entry(base_old) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
              let base_new = relocate_base(base_old);
              e.insert(base_new);
              base_new
            }
          }
        };
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

  // Phase 2: write relocated base pointers.
  for (&base_slot, base_info) in &base_map {
    unsafe {
      base_slot.write_unaligned(base_info.base_new);
    }
  }

  // Phase 3: write relocated derived pointers using the snapshotted values.
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

    unsafe {
      pair.derived_slot.write_unaligned(derived_new);
    }
  }
}

pub struct StackRootEnumerator<'a> {
  stackmaps: &'a StackMaps,
}

impl<'a> StackRootEnumerator<'a> {
  pub fn new(stackmaps: &'a StackMaps) -> Self {
    Self { stackmaps }
  }

  /// Walk the stack from the given callee frame pointer and invoke `f` for each
  /// base/derived relocation slot pair.
  ///
  /// This is a convenience helper when stack bounds are not available.
  pub fn visit_reloc_pairs_unbounded(
    &self,
    top_callee_fp: usize,
    f: impl FnMut(RelocPair),
  ) -> Result<(), FpWalkError> {
    self.visit_reloc_pairs(top_callee_fp, None, f)
  }

  /// Walk the stack from the given callee frame pointer and invoke `f` for each
  /// base/derived relocation slot pair.
  ///
  /// `top_callee_fp` is typically the frame pointer of the runtime safepoint function.
  ///
  /// Notes/assumptions:
  /// - We currently assume LLVM 18 statepoint lowering, where the stackmap record's `locations`
  ///   are: a prefix of constant header entries (metadata), followed by `(base, derived)` pairs for
  ///   each `gc.relocate` in the frame. Deopt operands (if any) are skipped.
  /// - Root locations must be addressable stack slots (`Location::Indirect`) relative to SP/FP
  ///   (no `Register` / `Direct` roots). This is enforced by the runtime statepoint verifier.
  pub fn visit_reloc_pairs(
    &self,
    top_callee_fp: usize,
    bounds: Option<StackBounds>,
    mut f: impl FnMut(RelocPair),
  ) -> Result<(), FpWalkError> {
    unsafe {
      let mut walker = StackWalker::new(top_callee_fp, bounds);
      loop {
        let Some(frame) = walker.next_frame()? else {
          break;
        };
        let Some(callsite) = self.stackmaps.lookup(frame.return_address as u64) else {
          // We likely reached an unmanaged/native frame (no stackmap entry). Stop.
          break;
        };

        if !visit_callsite_reloc_pairs(callsite, &frame, bounds, &mut f) {
          // Either a stack slot was out-of-bounds, or the record used an unsupported
          // location kind. Treat this as the end of the managed stack to avoid
          // potentially unsafe memory access.
          break;
        }
      }
    }
    Ok(())
  }
}

fn visit_callsite_reloc_pairs(
  callsite: CallSite<'_>,
  frame: &FrameView,
  bounds: Option<StackBounds>,
  f: &mut dyn FnMut(RelocPair),
) -> bool {
  // LLVM StackMaps `Indirect [SP + off]` locations are based on the caller's stack pointer at the
  // stackmap record PC (return address after the call, before any cleanup). Stackmaps may also
  // contain `Indirect [FP + off]` locations, which are evaluated directly from the caller frame
  // pointer.
  //
  // Under our forced-frame-pointer ABI contract this callsite SP is recoverable from the callee
  // frame pointer as `callee_fp + 16` (exposed by `FrameView::caller_sp`).
  //
  // Do *not* try to reconstruct callsite `SP` from the stackmap function record's `stack_size`:
  // `stack_size` is a fixed frame size and does not reliably account for per-call outgoing argument
  // pushes/stack adjustments, while `caller_sp` does.
  let Ok(caller_fp) = u64::try_from(frame.caller_fp) else {
    return false;
  };
  let Ok(caller_sp) = u64::try_from(frame.caller_sp) else {
    return false;
  };

  // LLVM 18 statepoint lowering emits `gc-live` locations in (base, derived) order for each
  // `gc.relocate` use. `CallSite::reloc_pairs` skips the statepoint header + any deopt operands and
  // yields an empty iterator for non-statepoint stackmap records.
  let regs = FrameRegs {
    sp: caller_sp,
    fp: caller_fp,
  };
  for pair in callsite.reloc_pairs() {
    let Some(base_slot) = location_to_root_slot(&regs, &pair.base, bounds) else {
      return false;
    };
    let Some(derived_slot) = location_to_root_slot(&regs, &pair.derived, bounds) else {
      return false;
    };
    f(RelocPair { base_slot, derived_slot });
  }

  true
}

#[derive(Clone, Copy)]
struct FrameRegs {
  sp: u64,
  fp: u64,
}

impl RegFile for FrameRegs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    match dwarf_reg {
      DWARF_SP_REG => Some(self.sp),
      DWARF_FP_REG => Some(self.fp),
      _ => None,
    }
  }
}

fn location_to_root_slot(regs: &FrameRegs, loc: &Location, bounds: Option<StackBounds>) -> Option<RootSlot> {
  let ptr_size = std::mem::size_of::<usize>() as u16;
  if loc.size() != ptr_size {
    return None;
  }

  let slot = eval_location(loc, regs).ok()?;
  match slot {
    RootSlot::Const { .. } => None,
    RootSlot::StackAddr(addr) => {
      if addr.is_null() {
        return None;
      }
      let addr_usize = addr as usize;
      if addr_usize % std::mem::align_of::<usize>() != 0 {
        return None;
      }
      if let Some(bounds) = bounds {
        if !bounds.contains_range(addr_usize as u64, ptr_size as u64) {
          return None;
        }
      }
      Some(slot)
    }
    RootSlot::Reg { dwarf_reg } => {
      // Register roots must never treat SP/FP/IP as GC pointers under our frame-pointer policy.
      if crate::arch::regs::forbidden_gc_root_reg(dwarf_reg).is_some() {
        return None;
      }
      Some(slot)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{relocate_derived_pairs, relocate_reloc_pairs_in_place, RelocPair};
  use crate::statepoints::RootSlot;
  use stackmap_context::ThreadContext;

  #[test]
  fn shared_base_slot_two_derived_slots() {
    let mut base: usize = 1000;
    let mut derived1: usize = base + 8;
    let mut derived2: usize = base + 16;

    let pairs = [
      (&mut base as *mut usize, &mut derived1 as *mut usize),
      (&mut base as *mut usize, &mut derived2 as *mut usize),
    ];

    relocate_derived_pairs(&pairs, |old| old + 100);

    assert_eq!(base, 1100);
    assert_eq!(derived1, 1108);
    assert_eq!(derived2, 1116);
  }

  #[test]
  fn shared_base_slot_order_independent() {
    let mut base: usize = 1000;
    let mut derived1: usize = base + 8;
    let mut derived2: usize = base + 16;

    // Reverse the input order to ensure the result is independent of pair ordering.
    let pairs = [
      (&mut base as *mut usize, &mut derived2 as *mut usize),
      (&mut base as *mut usize, &mut derived1 as *mut usize),
    ];

    relocate_derived_pairs(&pairs, |old| old + 100);

    assert_eq!(base, 1100);
    assert_eq!(derived1, 1108);
    assert_eq!(derived2, 1116);
  }

  #[test]
  fn base_itself_relocated_and_shared_with_derived() {
    let mut base: usize = 1000;
    let mut derived1: usize = base + 8;
    let mut derived2: usize = base + 16;

    // Include the base relocation pair (base_slot == derived_slot), plus two derived pointers
    // that share the same base slot.
    let pairs = [
      (&mut base as *mut usize, &mut derived1 as *mut usize),
      (&mut base as *mut usize, &mut base as *mut usize),
      (&mut base as *mut usize, &mut derived2 as *mut usize),
    ];

    relocate_derived_pairs(&pairs, |old| old + 100);

    assert_eq!(base, 1100);
    assert_eq!(derived1, 1108);
    assert_eq!(derived2, 1116);
  }

  #[test]
  fn null_base_forces_null_derived() {
    let mut base: usize = 0;
    let mut derived: usize = 12345;

    let pairs = [(&mut base as *mut usize, &mut derived as *mut usize)];
    relocate_derived_pairs(&pairs, |old| old + 100);

    assert_eq!(base, 0);
    assert_eq!(derived, 0);
  }

  #[test]
  fn relocated_base_to_null_forces_null_derived() {
    let mut base: usize = 1000;
    let mut derived: usize = base + 8;

    let pair = RelocPair {
      base_slot: RootSlot::StackAddr((&mut base as *mut usize).cast::<u8>()),
      derived_slot: RootSlot::StackAddr((&mut derived as *mut usize).cast::<u8>()),
    };

    let mut ctx = ThreadContext::default();
    relocate_reloc_pairs_in_place(&mut ctx, [pair], |_old| 0);

    assert_eq!(base, 0);
    assert_eq!(derived, 0);
  }
}
