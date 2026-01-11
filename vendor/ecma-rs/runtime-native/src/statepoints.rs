//! LLVM `gc.statepoint` decoding on top of parsed stackmaps.
//!
//! LLVM encodes safepoint information in the `.llvm_stackmaps` section. For
//! statepoints (LLVM 18, empirically), the callsite record's locations are laid
//! out as:
//!
//! - 3 leading non-root constants:
//!   - `locations[1]` (the second constant) corresponds to the IR `gc.statepoint`
//!     `flags` immarg on LLVM 18.
//!   - The runtime currently assumes `flags = 0` (see `statepoint_verify`).
//! - followed by 2 entries per gc-live value: `(base, derived)` pairs
//!
//! The tests in `tests/statepoint_layout.rs` intentionally lock this down to
//! catch regressions if LLVM changes its encoding.

use crate::stackmaps::{Location, StackMapRecord};
use stackmap_context::ThreadContext;
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

impl RegFile for ThreadContext {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    self.get_dwarf_reg_u64(dwarf_reg)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSlot {
  /// An addressable stack slot containing the pointer value.
  StackAddr(*mut u8),
  /// The pointer value lives in a register.
  Reg { dwarf_reg: u16 },
}

impl RootSlot {
  pub fn read_u64(&self, ctx: &ThreadContext) -> u64 {
    match *self {
      RootSlot::StackAddr(addr) => unsafe { (addr as *const u64).read_unaligned() },
      RootSlot::Reg { dwarf_reg } => ctx.get_dwarf_reg_u64(dwarf_reg).unwrap_or_else(|| {
        panic!("missing DWARF register {dwarf_reg} in ThreadContext when reading RootSlot")
      }),
    }
  }

  pub fn write_u64(&self, ctx: &mut ThreadContext, val: u64) {
    match *self {
      RootSlot::StackAddr(addr) => unsafe { (addr as *mut u64).write_unaligned(val) },
      RootSlot::Reg { dwarf_reg } => {
        ctx.set_dwarf_reg_u64(dwarf_reg, val)
          .unwrap_or_else(|err| panic!("failed to write DWARF register {dwarf_reg}: {err}"))
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationValue {
  Slot(RootSlot),
  Const { value: u64 },
}

pub fn eval_location(loc: &Location, regs: &impl RegFile) -> Result<LocationValue, StatepointError> {
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
      Ok(LocationValue::Const { value })
    }

    Location::Register { dwarf_reg, .. } => Ok(LocationValue::Slot(RootSlot::Reg { dwarf_reg })),

    Location::Constant { value, .. } => Ok(LocationValue::Const { value }),
    Location::ConstIndex { value, .. } => Ok(LocationValue::Const { value }),
  }
}

fn eval_stack_indirect(
  dwarf_reg: u16,
  offset: i32,
  regs: &impl RegFile,
) -> Result<LocationValue, StatepointError> {
  let addr = eval_reg_plus_offset(dwarf_reg, offset, regs)?;
  Ok(LocationValue::Slot(RootSlot::StackAddr(addr as *mut u8)))
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

#[cfg(test)]
mod tests {
  use super::{RootSlot, ThreadContext};

  #[test]
  fn root_slot_stack_addr_read_write_u64() {
    let mut slot: u64 = 0x1111_2222_3333_4444;
    let slot_addr = (&mut slot as *mut u64).cast::<u8>();
    let root = RootSlot::StackAddr(slot_addr);
    let mut ctx = ThreadContext::default();

    assert_eq!(root.read_u64(&ctx), 0x1111_2222_3333_4444);

    root.write_u64(&mut ctx, 0xaaaa_bbbb_cccc_dddd);
    assert_eq!(slot, 0xaaaa_bbbb_cccc_dddd);
  }

  #[test]
  fn root_slot_register_read_write_u64() {
    let mut ctx = ThreadContext::default();
    // DWARF reg 0 is X86_64 RAX / AArch64 X0.
    ctx.set_dwarf_reg_u64(0, 0x1234_5678_9abc_def0).unwrap();

    let root = RootSlot::Reg { dwarf_reg: 0 };
    assert_eq!(root.read_u64(&ctx), 0x1234_5678_9abc_def0);

    root.write_u64(&mut ctx, 0x0fed_cba9_8765_4321);
    assert_eq!(
      ctx.get_dwarf_reg_u64(0).unwrap(),
      0x0fed_cba9_8765_4321
    );
  }
}
