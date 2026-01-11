//! StackMap-driven root scanning helpers.
//!
//! This module provides a small, architecture-independent helper that:
//! - looks up the [`crate::stackmaps::CallSite`] for a stopped thread's current PC, and
//! - enumerates the `(base, derived)` relocation pairs corresponding to LLVM `gc.relocate` uses.
//!
//! Moving GCs must not treat the derived pointer as an independent root. Instead, relocate the
//! base pointer and recompute the derived pointer relative to it (see
//! [`crate::reloc::relocate_derived_pair`] / [`crate::relocate_derived_pairs`]).
//!
//! The key observation (LLVM 18, empirically) is that GC pointers are typically described as
//! `Indirect [DWARF_REG + offset]` locations, where the base DWARF register is the caller-frame SP
//! at the statepoint return address. This means the address of the spill slot is simply
//! `reg_value + offset`.

use crate::stackmaps::{Location, StackMaps};
use crate::statepoints::{StatepointError, StatepointRecord};
use stackmap_context::{ThreadContext, DWARF_REG_IP};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
  #[error("missing instruction pointer (DWARF reg {dwarf_reg}) in ThreadContext")]
  MissingInstructionPointer { dwarf_reg: u16 },

  #[error("missing DWARF register {dwarf_reg} while evaluating location {loc:?}")]
  MissingRegister { dwarf_reg: u16, loc: Location },

  #[error("address overflow while computing {kind} location address: base=0x{base:x} offset={offset}")]
  AddressOverflow {
    kind: &'static str,
    base: u64,
    offset: i32,
  },

  #[error("unsupported relocation location (expected Indirect spill slot), got {loc:?}")]
  UnsupportedLocation { loc: Location },

  #[error(
    "failed to decode statepoint stackmap record at callsite pc=0x{ip:x} (patchpoint_id=0x{patchpoint_id:x})"
  )]
  InvalidStatepoint {
    ip: u64,
    patchpoint_id: u64,
    #[source]
    source: StatepointError,
  },
}

/// A visitor for GC root slots discovered while scanning native frames.
pub trait RootVisitor {
  /// Visit a plain GC root slot.
  ///
  /// `slot` points to a word containing either `0` (null) or a pointer to the start of a
  /// GC-managed object.
  fn visit_root(&mut self, slot: *mut usize);

  /// Visit an LLVM `gc.relocate` (base, derived) pair.
  ///
  /// `base_slot` contains the base object pointer and must be treated as the GC root.
  /// `derived_slot` is an interior pointer derived from that base.
  ///
  /// # Important
  ///
  /// Moving GCs must not treat the derived slot as an independent root. Instead, relocate the base
  /// pointer and recompute the derived pointer relative to it (see
  /// [`crate::reloc::relocate_derived_pair`]).
  ///
  /// Note: LLVM stackmaps may reuse the same `base_slot` across multiple pairs; relocating pairs
  /// one-by-one is therefore incorrect. Collect all pairs first and relocate them in a batch using
  /// [`crate::relocate_derived_pairs`].
  fn visit_derived_pair(&mut self, base_slot: *mut usize, derived_slot: *mut usize);
}

/// Like [`scan_reloc_pairs`], but dispatches to a [`RootVisitor`].
///
/// This treats relocation pairs that alias the same spill slot (`base_slot == derived_slot`) as a
/// plain root (only one slot to update). All other pairs are reported via
/// [`RootVisitor::visit_derived_pair`], including cases where `base == derived` but the two values
/// are stored in distinct slots (LLVM may emit duplicates).
pub fn scan_roots(
  thread_ctx: &ThreadContext,
  stackmaps: &StackMaps,
  visitor: &mut impl RootVisitor,
) -> Result<(), ScanError> {
  for (base_slot, derived_slot) in scan_reloc_pairs(thread_ctx, stackmaps)? {
    if base_slot == derived_slot {
      visitor.visit_root(base_slot);
    } else {
      visitor.visit_derived_pair(base_slot, derived_slot);
    }
  }
  Ok(())
}

/// Enumerate `(base_slot, derived_slot)` relocation pairs at the current callsite.
///
/// - `thread_ctx` provides the stopped thread's DWARF register values.
/// - `stackmaps` is the parsed `.llvm_stackmaps` index for the current process/module.
///
/// Returns the address of each *pointer-sized spill slot* as a `(base_slot, derived_slot)` tuple.
///
/// Notes:
/// - For now, this helper only supports `Indirect` locations, which is the common LLVM 18 output
///   for statepoint roots after `rewrite-statepoints-for-gc`.
/// - `Register` and `Direct` locations are treated as unsupported and return an error.
pub fn scan_reloc_pairs(
  thread_ctx: &ThreadContext,
  stackmaps: &StackMaps,
) -> Result<Vec<(*mut usize, *mut usize)>, ScanError> {
  let ip = thread_ctx
    .get_dwarf_reg_u64(DWARF_REG_IP)
    .ok_or(ScanError::MissingInstructionPointer {
      dwarf_reg: DWARF_REG_IP,
    })?;

  let Some(callsite) = stackmaps.lookup(ip) else {
    // No stackmap record for this PC.
    return Ok(Vec::new());
  };

  // Detect LLVM `gc.statepoint` record layout by structure, not by `patchpoint_id`:
  // LLVM allows overriding the statepoint ID (`"statepoint-id"` attribute).
  let looks_like_statepoint = callsite.record.locations.len()
    >= crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS
    && callsite.record.locations[..crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS]
      .iter()
      .all(|loc| matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }));
  if !looks_like_statepoint {
    return Ok(Vec::new());
  }

  let statepoint = StatepointRecord::new(callsite.record).map_err(|source| ScanError::InvalidStatepoint {
    ip,
    patchpoint_id: callsite.record.patchpoint_id,
    source,
  })?;

  let mut pairs: Vec<(*mut usize, *mut usize)> = Vec::with_capacity(statepoint.gc_pair_count());
  for pair in statepoint.gc_pairs() {
    let base_slot = slot_addr(thread_ctx, &pair.base)?;
    let derived_slot = slot_addr(thread_ctx, &pair.derived)?;
    pairs.push((base_slot, derived_slot));
  }

  Ok(pairs)
}

fn slot_addr(ctx: &ThreadContext, loc: &Location) -> Result<*mut usize, ScanError> {
  match *loc {
    Location::Indirect {
      dwarf_reg, offset, ..
    } => {
      let base = ctx.get_dwarf_reg_u64(dwarf_reg).ok_or(ScanError::MissingRegister {
        dwarf_reg,
        loc: loc.clone(),
      })?;
      let addr = add_i32(base, offset).ok_or(ScanError::AddressOverflow {
        kind: "Indirect",
        base,
        offset,
      })?;
      Ok(addr as usize as *mut usize)
    }

    // `Direct` locations in LLVM stackmaps mean "value is (reg + offset)" (no memory indirection).
    //
    // `scan_reloc_pairs` currently only supports addressable stack slots (i.e. `Indirect` spill
    // slots). If we ever need to support `Direct` and `Register` roots, this API likely needs to
    // return a richer "root slot" abstraction (stack slot vs register) instead of raw pointers.
    Location::Direct { .. } | Location::Register { .. } | Location::Constant { .. } | Location::ConstIndex { .. } => {
      Err(ScanError::UnsupportedLocation { loc: loc.clone() })
    }
  }
}

fn add_i32(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}
