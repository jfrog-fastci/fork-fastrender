//! Verifier for LLVM `gc.statepoint` stackmap conventions.
//!
//! Our GC design currently assumes that all GC roots at statepoints are spilled
//! into addressable stack slots (SP-relative `Indirect` locations). LLVM *can*
//! legally encode roots in registers, so we keep a runtime verifier to fail fast
//! if codegen or LLVM changes break that assumption.

use crate::stackmaps::{Location, StackMap, StackMapError, StackMapRecord, StackSizeRecord};
use crate::statepoints::{StatepointError, StatepointRecord, AARCH64_DWARF_REG_SP, X86_64_DWARF_REG_SP};
use std::error::Error;
use std::fmt;

/// Empirically observed in LLVM 18 output for `@llvm.experimental.gc.statepoint`.
///
/// This is **not** a contract of LLVM itself; it's a convention for our codegen
/// to mark statepoints so the runtime can cheaply identify which stackmap
/// records should follow the statepoint layout.
pub const LLVM_STATEPOINT_PATCHPOINT_ID: u64 = 0xABCDEF00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwarfArch {
  X86_64,
  AArch64,
}

impl DwarfArch {
  pub fn stack_pointer_dwarf_reg(self) -> u16 {
    match self {
      DwarfArch::X86_64 => X86_64_DWARF_REG_SP,
      DwarfArch::AArch64 => AARCH64_DWARF_REG_SP,
    }
  }

  pub fn pointer_size(self) -> u16 {
    // We currently only support 64-bit targets for native codegen.
    8
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
  /// Only verify stack map records that look like LLVM statepoints.
  StatepointsOnly,
  /// Verify all stack map records as if they were statepoints.
  AllRecords,
}

#[derive(Debug, Clone, Copy)]
pub struct VerifyStatepointOptions {
  pub arch: DwarfArch,
  pub mode: VerifyMode,
}

#[derive(Debug, Clone, Copy)]
pub struct LocationSummary {
  pub kind: &'static str,
  pub dwarf_reg: u16,
  pub offset: i64,
  pub size: u16,
}

impl LocationSummary {
  fn from_location(loc: &Location) -> Self {
    match *loc {
      Location::Register {
        size,
        dwarf_reg,
        offset,
      } => Self {
        kind: "Register",
        dwarf_reg,
        offset: offset as i64,
        size,
      },
      Location::Direct {
        size,
        dwarf_reg,
        offset,
      } => Self {
        kind: "Direct",
        dwarf_reg,
        offset: offset as i64,
        size,
      },
      Location::Indirect {
        size,
        dwarf_reg,
        offset,
      } => Self {
        kind: "Indirect",
        dwarf_reg,
        offset: offset as i64,
        size,
      },
      Location::Constant { size, value } => Self {
        kind: "Constant",
        dwarf_reg: 0,
        offset: value as i64,
        size,
      },
      Location::ConstIndex {
        size,
        index: _,
        value,
      } => Self {
        kind: "ConstIndex",
        dwarf_reg: 0,
        offset: value as i64,
        size,
      },
    }
  }
}

#[derive(Debug, Clone)]
pub struct VerifyError {
  pub callsite_address: u64,
  pub patchpoint_id: u64,
  pub message: String,
  pub location_index: Option<usize>,
  pub location: Option<LocationSummary>,
}

impl VerifyError {
  fn new_record(callsite_address: u64, patchpoint_id: u64, message: String) -> Self {
    Self {
      callsite_address,
      patchpoint_id,
      message,
      location_index: None,
      location: None,
    }
  }

  fn new_location(
    callsite_address: u64,
    patchpoint_id: u64,
    location_index: usize,
    location: &Location,
    message: String,
  ) -> Self {
    Self {
      callsite_address,
      patchpoint_id,
      message,
      location_index: Some(location_index),
      location: Some(LocationSummary::from_location(location)),
    }
  }
}

impl fmt::Display for VerifyError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "statepoint stackmap verification failed for return address {:#x} (patchpoint_id={:#x})",
      self.callsite_address, self.patchpoint_id
    )?;

    if let (Some(idx), Some(loc)) = (self.location_index, self.location) {
      write!(
        f,
        ": location[{idx}] {msg} (kind={kind}, dwarf_reg={dwarf_reg}, offset={offset}, size={size})",
        idx = idx,
        msg = self.message,
        kind = loc.kind,
        dwarf_reg = loc.dwarf_reg,
        offset = loc.offset,
        size = loc.size,
      )
    } else {
      write!(f, ": {}", self.message)
    }
  }
}

impl Error for VerifyError {}

#[derive(Debug)]
pub enum LoadError {
  Parse(StackMapError),
  Verify(VerifyError),
}

impl fmt::Display for LoadError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      LoadError::Parse(err) => write!(f, "{err}"),
      LoadError::Verify(err) => write!(f, "{err}"),
    }
  }
}

impl Error for LoadError {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      LoadError::Parse(err) => Some(err),
      LoadError::Verify(err) => Some(err),
    }
  }
}

/// Parse an LLVM `.llvm_stackmaps` section and (optionally) verify our statepoint invariants.
///
/// In debug builds (or when compiled with the `verify-statepoints` feature), this enforces the
/// spill-to-stack convention for all statepoint records.
pub fn load_stackmap(section: &[u8], arch: DwarfArch) -> Result<StackMap, LoadError> {
  let stackmap = StackMap::parse(section).map_err(LoadError::Parse)?;

  #[cfg(any(debug_assertions, feature = "verify-statepoints"))]
  {
    verify_statepoint_stackmap(
      &stackmap,
      VerifyStatepointOptions {
        arch,
        mode: VerifyMode::StatepointsOnly,
      },
    )
    .map_err(LoadError::Verify)?;
  }

  #[cfg(not(any(debug_assertions, feature = "verify-statepoints")))]
  {
    let _ = arch;
  }

  Ok(stackmap)
}

pub fn verify_statepoint_stackmap(
  stackmap: &StackMap,
  opts: VerifyStatepointOptions,
) -> Result<(), VerifyError> {
  let sp_reg = opts.arch.stack_pointer_dwarf_reg();
  let ptr_size = opts.arch.pointer_size();

  let mut record_index = 0usize;
  for func in &stackmap.functions {
    let record_count = usize::try_from(func.record_count).unwrap_or(usize::MAX);
    for _ in 0..record_count {
      let rec = stackmap.records.get(record_index).ok_or_else(|| {
        VerifyError::new_record(
          func.address,
          0,
          format!(
            "stackmap function record_count overflowed records list: record_index={record_index} records.len()={}",
            stackmap.records.len()
          ),
        )
      })?;
      record_index += 1;

      let is_statepoint = rec.patchpoint_id == LLVM_STATEPOINT_PATCHPOINT_ID;
      if opts.mode == VerifyMode::StatepointsOnly && !is_statepoint {
        continue;
      }

      verify_statepoint_record(stackmap, func, rec, sp_reg, ptr_size)?;
    }
  }

  Ok(())
}

fn verify_statepoint_record(
  _stackmap: &StackMap,
  func: &StackSizeRecord,
  rec: &StackMapRecord,
  sp_reg: u16,
  ptr_size: u16,
) -> Result<(), VerifyError> {
  let callsite = func.address.wrapping_add(rec.instruction_offset as u64);

  let sp = StatepointRecord::new(rec).map_err(|err| match err {
    StatepointError::NonConstantHeader { index } => VerifyError::new_location(
      callsite,
      rec.patchpoint_id,
      index,
      &rec.locations[index],
      "expected Constant/ConstIndex statepoint header".to_string(),
    ),
    other => VerifyError::new_record(callsite, rec.patchpoint_id, other.to_string()),
  })?;

  // Enforce our current runtime policy for statepoints:
  // - callconv must be 0
  // - flags must be a 2-bit mask (0..3) and must be 0 for now
  // - deopt operands are currently not supported (deopt_count must be 0)
  let header = sp.header();
  if header.callconv != 0 {
    return Err(VerifyError::new_location(
      callsite,
      rec.patchpoint_id,
      0,
      &rec.locations[0],
      format!(
        "expected gc.statepoint callconv=0 header (locations[0]), got {}",
        header.callconv
      ),
    ));
  }
  if header.flags > 3 {
    return Err(VerifyError::new_location(
      callsite,
      rec.patchpoint_id,
      1,
      &rec.locations[1],
      format!(
        "expected gc.statepoint flags to be a 2-bit mask (0..3) (locations[1]), got {}",
        header.flags
      ),
    ));
  }
  if header.flags != 0 {
    return Err(VerifyError::new_location(
      callsite,
      rec.patchpoint_id,
      1,
      &rec.locations[1],
      format!(
        "expected gc.statepoint flags=0 header (locations[1]), got {}",
        header.flags
      ),
    ));
  }
  if header.deopt_count != 0 {
    return Err(VerifyError::new_location(
      callsite,
      rec.patchpoint_id,
      2,
      &rec.locations[2],
      format!(
        "gc.statepoint deopt operands are not supported (locations[2] deopt_count={})",
        header.deopt_count
      ),
    ));
  }

  let start = sp.gc_pairs_start();
  for (pair_idx, (base, derived)) in sp.gc_pairs().enumerate() {
    let base_idx = start + pair_idx * 2;
    verify_indirect_sp_slot(
      callsite,
      rec.patchpoint_id,
      func.stack_size,
      sp_reg,
      ptr_size,
      base_idx,
      base,
    )?;
    verify_indirect_sp_slot(
      callsite,
      rec.patchpoint_id,
      func.stack_size,
      sp_reg,
      ptr_size,
      base_idx + 1,
      derived,
    )?;
  }

  Ok(())
}

fn verify_indirect_sp_slot(
  callsite: u64,
  patchpoint_id: u64,
  stack_size: u64,
  sp_reg: u16,
  ptr_size: u16,
  location_index: usize,
  loc: &Location,
) -> Result<(), VerifyError> {
  let (size, dwarf_reg, offset) = match *loc {
    Location::Indirect {
      size,
      dwarf_reg,
      offset,
    } => (size, dwarf_reg, offset),
    _ => {
      return Err(VerifyError::new_location(
        callsite,
        patchpoint_id,
        location_index,
        loc,
        "expected Indirect spill slot".to_string(),
      ))
    }
  };

  if size != ptr_size {
    return Err(VerifyError::new_location(
      callsite,
      patchpoint_id,
      location_index,
      loc,
      format!("expected pointer-sized slot (size={ptr_size})"),
    ));
  }

  if dwarf_reg != sp_reg {
    return Err(VerifyError::new_location(
      callsite,
      patchpoint_id,
      location_index,
      loc,
      format!("expected base register SP (DWARF reg {sp_reg})"),
    ));
  }

  // Conservative bound: LLVM records stack slots as offsets from SP. We require a non-negative
  // offset that doesn't exceed the total frame size.
  let offset64 = offset as i64;
  if offset64 < 0 || (offset64 as u64) > stack_size {
    return Err(VerifyError::new_location(
      callsite,
      patchpoint_id,
      location_index,
      loc,
      format!("expected offset within [0, {stack_size}]"),
    ));
  }

  Ok(())
}
