//! LLVM `gc.statepoint` decoding on top of parsed stackmaps.
//!
//! LLVM encodes safepoint information in the `.llvm_stackmaps` section. For
//! statepoints (LLVM 18, empirically), the callsite record's locations are laid
//! out as:
//!
//! - 3 leading constant header locations:
//!   - `locations[0]`: `callconv` (call convention ID)
//!   - `locations[1]`: `flags` (2-bit mask, 0..3)
//!   - `locations[2]`: `deopt_count` (number of deopt operand locations)
//! - followed by `deopt_count` deopt operand locations (not GC roots)
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
    "invalid statepoint stackmap layout: expected at least {LLVM18_STATEPOINT_HEADER_CONSTANTS} header locations; expected deopt_count locations after the header and an even number of remaining (base,derived) locations, got {num_locations}"
  )]
  InvalidLayout { num_locations: usize },

  #[error("expected locations[{index}] to be Constant/ConstIndex header")]
  NonConstantHeader { index: usize },

  #[error("missing DWARF register {dwarf_reg} in provided register file")]
  MissingRegister { dwarf_reg: u16 },

  #[error("address computation overflow: base=0x{base:x} offset={offset}")]
  AddressOverflow { base: u64, offset: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatepointHeader {
  pub callconv: u64,
  pub flags: u64,
  pub deopt_count: u64,
}

/// A view of a [`StackMapRecord`] interpreted as an LLVM `gc.statepoint`
/// safepoint.
pub struct StatepointRecord<'a> {
  record: &'a StackMapRecord,
  header: StatepointHeader,
  gc_pairs_start: usize,
}

impl<'a> StatepointRecord<'a> {
  pub fn new(record: &'a StackMapRecord) -> Result<Self, StatepointError> {
    let header = decode_statepoint_header(&record.locations)?;
    let gc_pairs_start = statepoint_gc_pairs_start(header.deopt_count, record.locations.len())?;

    let remaining = record.locations.len() - gc_pairs_start;
    if remaining % 2 != 0 {
      return Err(StatepointError::InvalidLayout {
        num_locations: record.locations.len(),
      });
    }

    Ok(Self {
      record,
      header,
      gc_pairs_start,
    })
  }

  #[inline]
  pub fn header(&self) -> StatepointHeader {
    self.header
  }

  #[inline]
  pub fn deopt_locations(&self) -> &'a [Location] {
    &self.record.locations[LLVM18_STATEPOINT_HEADER_CONSTANTS..self.gc_pairs_start]
  }

  #[inline]
  pub fn gc_pairs_start(&self) -> usize {
    self.gc_pairs_start
  }

  #[inline]
  pub fn gc_pair_count(&self) -> usize {
    (self.record.locations.len() - self.gc_pairs_start) / 2
  }

  pub fn gc_pairs(&self) -> impl ExactSizeIterator<Item = (&'a Location, &'a Location)> {
    let locs = &self.record.locations[self.gc_pairs_start..];
    debug_assert_eq!(locs.len() % 2, 0);
    locs
      .chunks_exact(2)
      .map(|pair| (&pair[0], &pair[1]))
  }

  pub fn record(&self) -> &'a StackMapRecord {
    self.record
  }
}

fn decode_statepoint_header(locs: &[Location]) -> Result<StatepointHeader, StatepointError> {
  if locs.len() < LLVM18_STATEPOINT_HEADER_CONSTANTS {
    return Err(StatepointError::InvalidLayout {
      num_locations: locs.len(),
    });
  }

  let callconv = decode_statepoint_header_constant(&locs[0], 0)?;
  let flags = decode_statepoint_header_constant(&locs[1], 1)?;
  let deopt_count = decode_statepoint_header_constant(&locs[2], 2)?;

  Ok(StatepointHeader {
    callconv,
    flags,
    deopt_count,
  })
}

fn decode_statepoint_header_constant(loc: &Location, index: usize) -> Result<u64, StatepointError> {
  match *loc {
    Location::Constant { value, .. } | Location::ConstIndex { value, .. } => Ok(value),
    _ => Err(StatepointError::NonConstantHeader { index }),
  }
}

fn statepoint_gc_pairs_start(
  deopt_count: u64,
  num_locations: usize,
) -> Result<usize, StatepointError> {
  let deopt_count = usize::try_from(deopt_count).map_err(|_| StatepointError::InvalidLayout {
    num_locations,
  })?;

  let gc_pairs_start = LLVM18_STATEPOINT_HEADER_CONSTANTS
    .checked_add(deopt_count)
    .ok_or(StatepointError::InvalidLayout { num_locations })?;

  if gc_pairs_start > num_locations {
    return Err(StatepointError::InvalidLayout { num_locations });
  }

  Ok(gc_pairs_start)
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
