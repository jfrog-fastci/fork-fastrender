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
use std::ptr;

use crate::stack_walk::{FrameView, StackWalker};
use crate::stackmaps::{CallSite, Location, StackMapRecord, StackMaps};

/// A `(base, derived)` relocation pair.
///
/// `base_slot` and `derived_slot` are pointers to machine-word slots (usually stack slots or
/// spilled-register slots) that contain pointer-sized values.
///
/// A value of `0` in either slot is treated as a null pointer.
#[derive(Clone, Copy, Debug)]
pub struct RelocPair {
  pub base_slot: *mut usize,
  pub derived_slot: *mut usize,
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
  pub unsafe fn update_derived_after_base_moved(&self, old_base: usize, new_base: usize) {
    let old_derived = self.derived_slot.read();

    // Preserve nulls to avoid underflow and to treat null pointers as non-roots.
    if old_base == 0 || old_derived == 0 || new_base == 0 {
      self.derived_slot.write(0);
      return;
    }

    let offset = old_derived.wrapping_sub(old_base);
    let new_derived = new_base.wrapping_add(offset);
    self.derived_slot.write(new_derived);
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
/// Callers must ensure that every `base_slot` and `derived_slot` in `pairs` is a valid, writable
/// pointer to a `usize` slot for the duration of the call.
pub fn relocate_reloc_pairs_in_place(
  pairs: impl IntoIterator<Item = RelocPair>,
  mut relocate: impl FnMut(usize) -> usize,
) {
  // Phase 1: snapshot all old slot values without mutating anything.
  //
  // Cache at least `old_base` per unique `base_slot` so repeated base slots don't observe our own
  // in-place writes.
  let mut old_base_by_slot: HashMap<*mut usize, usize> = HashMap::new();
  let mut old_derived_by_slot: HashMap<*mut usize, usize> = HashMap::new();
  let mut pairs_vec: Vec<RelocPair> = Vec::new();

  for pair in pairs {
    if pair.base_slot.is_null() || pair.derived_slot.is_null() {
      // Invalid slot pointers are a caller bug; skip to avoid UB in release builds.
      debug_assert!(
        false,
        "relocate_reloc_pairs_in_place received null base_slot/derived_slot pointer"
      );
      continue;
    }

    // SAFETY: caller guarantees slots are valid; use unaligned access to avoid imposing alignment
    // constraints on stackmap encodings.
    let old_base = unsafe { ptr::read_unaligned(pair.base_slot) };
    let old_derived = unsafe { ptr::read_unaligned(pair.derived_slot) };

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
  let mut new_base_by_slot: HashMap<*mut usize, usize> =
    HashMap::with_capacity(old_base_by_slot.len());
  for (&slot, &old_base) in &old_base_by_slot {
    let new_base = if old_base == 0 { 0 } else { relocate(old_base) };
    new_base_by_slot.insert(slot, new_base);
  }

  // Phase 3a: write relocated base pointers.
  for (&slot, &new_base) in &new_base_by_slot {
    // SAFETY: caller guarantees slots are valid and writable.
    unsafe {
      ptr::write_unaligned(slot, new_base);
    }
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

    // SAFETY: caller guarantees slots are valid and writable.
    unsafe {
      ptr::write_unaligned(pair.derived_slot, new_derived);
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
  /// `top_callee_fp` is typically the frame pointer of the runtime safepoint function.
  ///
  /// Notes/assumptions:
  /// - We currently assume LLVM 18 statepoint lowering, where the stackmap record's `locations`
  ///   are: a prefix of constant header entries (metadata), followed by `(base, derived)` pairs for
  ///   each `gc.relocate` in the frame.
  /// - Only `Location::Indirect` is supported for slot addressing (with DWARF reg RSP/RBP).
  pub fn visit_reloc_pairs(&self, top_callee_fp: usize, mut f: impl FnMut(RelocPair)) {
    unsafe {
      let mut walker = StackWalker::new(top_callee_fp);
      while let Some(frame) = walker.next_frame() {
        let Some(callsite) = self.stackmaps.lookup(frame.return_address as u64) else {
          // We likely reached an unmanaged/native frame (no stackmap entry). Stop.
          break;
        };

        visit_callsite_reloc_pairs(callsite, &frame, &mut f);
      }
    }
  }
}

fn visit_callsite_reloc_pairs(callsite: CallSite<'_>, frame: &FrameView, f: &mut dyn FnMut(RelocPair)) {
  let record: &StackMapRecord = callsite.record;
  let non_const: Vec<&Location> = record
    .locations
    .iter()
    .filter(|loc| !matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }))
    .collect();

  assert!(
    non_const.len() % 2 == 0,
    "stackmap record at return_address=0x{:x} has odd number of non-constant locations ({})",
    frame.return_address,
    non_const.len()
  );

  // LLVM 18 statepoint lowering emits locations in (base, derived) order for each gc.relocate.
  for chunk in non_const.chunks_exact(2) {
    let base_slot = location_to_slot(frame, chunk[0]);
    let derived_slot = location_to_slot(frame, chunk[1]);
    f(RelocPair {
      base_slot,
      derived_slot,
    });
  }
}

fn location_to_slot(frame: &FrameView, loc: &Location) -> *mut usize {
  match *loc {
    Location::Indirect { dwarf_reg, offset, size } => {
      assert!(
        size as usize == std::mem::size_of::<usize>(),
        "unsupported stackmap slot size {size} (expected pointer-sized)"
      );
      let base = match dwarf_reg {
        // x86_64 DWARF reg numbers.
        7 => frame.caller_sp, // RSP
        6 => frame.caller_fp, // RBP
        _ => panic!("unsupported DWARF register {dwarf_reg} for stack slot"),
      };
      let addr = (base as isize).wrapping_add(offset as isize) as usize;
      assert!(
        addr % std::mem::align_of::<usize>() == 0,
        "stackmap slot address 0x{addr:x} is not word-aligned"
      );
      addr as *mut usize
    }
    _ => panic!("unsupported stackmap location for mutable slot: {loc:?}"),
  }
}

