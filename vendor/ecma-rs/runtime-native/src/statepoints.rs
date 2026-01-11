//! LLVM `gc.statepoint` decoding on top of parsed stackmaps.
//!
//! LLVM encodes safepoint information in the `.llvm_stackmaps` section. For
//! statepoints (LLVM 18, empirically), the callsite record's locations are laid
//! out as:
//!
//! - 3 leading non-root constants (ignore for GC root scanning)
//! - followed by 2 entries per gc-live value: `(base, derived)` pairs
//!
//! The tests in `tests/statepoint_layout.rs` intentionally lock this down to
//! catch regressions if LLVM changes its encoding.

use crate::stackmaps::{Location, StackMapRecord};
use thiserror::Error;

pub const LLVM18_STATEPOINT_HEADER_CONSTANTS: usize = 3;

// DWARF register number helpers used by tests and documentation.
pub const X86_64_DWARF_REG_SP: u16 = 7;
pub const X86_64_DWARF_REG_FP: u16 = 6;
pub const AARCH64_DWARF_REG_SP: u16 = 31;
pub const AARCH64_DWARF_REG_FP: u16 = 29;

#[derive(Debug, Error)]
pub enum StatepointError {
  #[error(
    "invalid statepoint stackmap layout: expected at least {LLVM18_STATEPOINT_HEADER_CONSTANTS} locations and (n-{LLVM18_STATEPOINT_HEADER_CONSTANTS}) even, got {num_locations}"
  )]
  InvalidLayout { num_locations: usize },

  #[error("expected locations[{index}] to be Constant/ConstIndex header")]
  NonConstantHeader { index: usize },

  #[error("missing DWARF register {dwarf_reg} in provided register file")]
  MissingRegister { dwarf_reg: u16 },

  #[error("address computation overflow: base=0x{base:x} offset={offset}")]
  AddressOverflow { base: u64, offset: i32 },
}

#[derive(Debug, Clone, Copy)]
pub struct GcLocationPair<'a> {
  pub base: &'a Location,
  pub derived: &'a Location,
}

/// A view of a [`StackMapRecord`] interpreted as an LLVM `gc.statepoint`
/// safepoint.
pub struct StatepointRecord<'a> {
  record: &'a StackMapRecord,
  gc_pairs: Vec<GcLocationPair<'a>>,
}

impl<'a> StatepointRecord<'a> {
  pub fn new(record: &'a StackMapRecord) -> Result<Self, StatepointError> {
    let num_locations = record.locations.len();
    if num_locations < LLVM18_STATEPOINT_HEADER_CONSTANTS
      || (num_locations - LLVM18_STATEPOINT_HEADER_CONSTANTS) % 2 != 0
    {
      return Err(StatepointError::InvalidLayout { num_locations });
    }

    // LLVM 18 observed encoding: the first three locations are non-root
    // constants used by the lowering, not actual GC roots.
    for idx in 0..LLVM18_STATEPOINT_HEADER_CONSTANTS {
      match record.locations[idx] {
        Location::Constant { .. } | Location::ConstIndex { .. } => {}
        _ => return Err(StatepointError::NonConstantHeader { index: idx }),
      }
    }

    let mut gc_pairs = Vec::with_capacity((num_locations - LLVM18_STATEPOINT_HEADER_CONSTANTS) / 2);
    let mut i = LLVM18_STATEPOINT_HEADER_CONSTANTS;
    while i < num_locations {
      gc_pairs.push(GcLocationPair {
        base: &record.locations[i],
        derived: &record.locations[i + 1],
      });
      i += 2;
    }

    Ok(Self { record, gc_pairs })
  }

  pub fn gc_pairs(&self) -> &[GcLocationPair<'a>] {
    &self.gc_pairs
  }

  pub fn record(&self) -> &'a StackMapRecord {
    self.record
  }
}

/// A minimal register-value provider used for evaluating stackmap locations.
pub trait RegFile {
  fn get(&self, dwarf_reg: u16) -> Option<u64>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSlot {
  /// An addressable stack slot containing the pointer value.
  Stack { addr: *mut u8 },
  /// The pointer value lives in a register (rare in observed statepoint output).
  Register { dwarf_reg: u16 },
  /// A non-addressable value (constant or computed).
  Const { value: u64 },
}

pub fn eval_location(loc: &Location, regs: &impl RegFile) -> Result<RootSlot, StatepointError> {
  match *loc {
    Location::Indirect {
      dwarf_reg, offset, ..
    } => eval_stack_indirect(dwarf_reg, offset, regs),

    Location::Direct {
      dwarf_reg, offset, ..
    } => {
      // LLVM StackMaps `Direct` semantics are "value is reg + offset" (no memory indirection).
      //
      // This is different from `Indirect`, which identifies an addressable stack slot (where the
      // pointer value lives). A `Direct` value is not an addressable root slot, so we surface it as
      // an immediate value.
      let value = eval_reg_plus_offset(dwarf_reg, offset, regs)?;
      Ok(RootSlot::Const { value })
    }

    Location::Register { dwarf_reg, .. } => Ok(RootSlot::Register { dwarf_reg }),

    Location::Constant { value, .. } => Ok(RootSlot::Const { value }),
    Location::ConstIndex { value, .. } => Ok(RootSlot::Const { value }),
  }
}

fn eval_stack_indirect(
  dwarf_reg: u16,
  offset: i32,
  regs: &impl RegFile,
) -> Result<RootSlot, StatepointError> {
  let addr = eval_reg_plus_offset(dwarf_reg, offset, regs)?;
  Ok(RootSlot::Stack {
    addr: addr as *mut u8,
  })
}

fn eval_reg_plus_offset(
  dwarf_reg: u16,
  offset: i32,
  regs: &impl RegFile,
) -> Result<u64, StatepointError> {
  let base = regs
    .get(dwarf_reg)
    .ok_or(StatepointError::MissingRegister { dwarf_reg })?;

  let addr = (base as i128) + (offset as i128);
  if !(0..=u64::MAX as i128).contains(&addr) {
    return Err(StatepointError::AddressOverflow { base, offset });
  }
  Ok(addr as u64)
}
