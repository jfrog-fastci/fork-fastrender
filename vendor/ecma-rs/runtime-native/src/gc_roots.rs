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
//! [`relocate_reloc_pairs_in_place`] implements a two-phase algorithm that is robust to this.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use crate::stack_walk::{FrameView, StackWalker};
use crate::stackmaps::{CallSite, Location, StackMapRecord, StackMaps};
use crate::stackwalk::StackBounds;
use crate::statepoints::{eval_location, RegFile, RootSlot, StatepointRecord};
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

    let offset = old_derived.wrapping_sub(old_base);
    let new_derived = new_base.wrapping_add(offset);
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

  // Phase 2: relocate each unique base slot once.
  let mut new_base_by_slot: HashMap<RootSlot, usize> = HashMap::with_capacity(old_base_by_slot.len());
  for (&slot, &old_base) in &old_base_by_slot {
    let new_base = if old_base == 0 { 0 } else { relocate(old_base) };
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
    let new_derived = if old_base == 0 || old_derived == 0 {
      0
    } else {
      let delta = old_derived.wrapping_sub(old_base);
      new_base.wrapping_add(delta)
    };

    pair.derived_slot.write_u64(ctx, new_derived as u64);
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
  /// `top_callee_fp` is typically the frame pointer of the runtime safepoint function.
  ///
  /// Notes/assumptions:
  /// - We currently assume LLVM 18 statepoint lowering, where the stackmap record's `locations`
  ///   are: a prefix of constant header entries (metadata), followed by `(base, derived)` pairs for
  ///   each `gc.relocate` in the frame. Deopt operands (if any) are skipped.
  /// - Root locations may be either stack slots (`Location::Indirect`) or registers (`Location::Register`).
  pub fn visit_reloc_pairs(
    &self,
    top_callee_fp: usize,
    bounds: Option<StackBounds>,
    mut f: impl FnMut(RelocPair),
  ) {
    unsafe {
      let mut walker = StackWalker::new(top_callee_fp, bounds);
      while let Some(frame) = walker.next_frame() {
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
  }
}

fn visit_callsite_reloc_pairs(
  callsite: CallSite<'_>,
  frame: &FrameView,
  bounds: Option<StackBounds>,
  f: &mut dyn FnMut(RelocPair),
) -> bool {
  let record: &StackMapRecord = callsite.record;
  let statepoint = StatepointRecord::new(record).unwrap_or_else(|err| {
    panic!(
      "failed to decode statepoint stackmap record at return_address=0x{:x} (patchpoint_id=0x{:x}): {err}",
      frame.return_address, record.patchpoint_id
    )
  });

  // LLVM 18 statepoint lowering emits locations in (base, derived) order for each `gc.relocate`
  // call. `gc_pairs()` is already offset past the 3-entry statepoint header and any deopt operands.
  for pair in statepoint.gc_pairs() {
    let Some(base_slot) = location_to_root_slot(frame, &pair.base, bounds) else {
      return false;
    };
    let Some(derived_slot) = location_to_root_slot(frame, &pair.derived, bounds) else {
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

#[cfg(target_arch = "x86_64")]
const DWARF_REG_SP: u16 = 7;
#[cfg(target_arch = "x86_64")]
const DWARF_REG_FP: u16 = 6;
#[cfg(target_arch = "aarch64")]
const DWARF_REG_SP: u16 = 31;
#[cfg(target_arch = "aarch64")]
const DWARF_REG_FP: u16 = 29;

impl RegFile for FrameRegs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    match dwarf_reg {
      DWARF_REG_SP => Some(self.sp),
      DWARF_REG_FP => Some(self.fp),
      _ => None,
    }
  }
}

fn location_to_root_slot(
  frame: &FrameView,
  loc: &Location,
  bounds: Option<StackBounds>,
) -> Option<RootSlot> {
  let ptr_size = std::mem::size_of::<usize>() as u16;
  if loc.size() != ptr_size {
    return None;
  }

  let regs = FrameRegs {
    sp: frame.caller_sp as u64,
    fp: frame.caller_fp as u64,
  };

  let slot = eval_location(loc, &regs).ok()?;
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
    RootSlot::Reg { .. } => Some(slot),
  }
}
