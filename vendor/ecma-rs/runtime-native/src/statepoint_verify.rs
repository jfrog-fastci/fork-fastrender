//! Verifier for LLVM `gc.statepoint` stackmap conventions.
//!
//! Our GC design currently assumes that all GC roots at statepoints are spilled
//! into addressable stack slots (SP-relative `Indirect` locations). LLVM *can*
//! legally encode roots in registers, so we keep a runtime verifier to fail fast
//! if codegen or LLVM changes break that assumption.

use crate::stackmaps::{
  parse_all_stackmaps, Location, StackMap, StackMapError, StackMapRecord, StackSize, StackSizeRecord,
  STACKMAP_VERSION,
};
use crate::statepoints::{
  StatepointError, StatepointRecord, AARCH64_DWARF_REG_FP, AARCH64_DWARF_REG_SP, X86_64_DWARF_REG_FP,
  X86_64_DWARF_REG_SP,
};
use std::error::Error;
use std::fmt;

/// Default `gc.statepoint` ID used by LLVM 18 when `rewrite-statepoints-for-gc` is
/// run without an explicit `"statepoint-id"` override.
///
/// The StackMap record stores this value in its `patchpoint_id` field.
///
/// LLVM uses this constant ID when callsites are not annotated with the
/// `"statepoint-id"` directive attribute. When directives are used, the StackMap
/// record's `patchpoint_id` is no longer a reliable discriminator for
/// identifying statepoints.
///
/// The runtime verifier identifies statepoints by attempting to decode the
/// record layout as a `gc.statepoint` (see
/// [`crate::statepoints::StatepointRecord`]).
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

  pub fn frame_pointer_dwarf_reg(self) -> u16 {
    match self {
      DwarfArch::X86_64 => X86_64_DWARF_REG_FP,
      DwarfArch::AArch64 => AARCH64_DWARF_REG_FP,
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
  ///
  /// Detection is layout-based: the record must have the 3 leading constant
  /// header locations (`callconv`, `flags`, `deopt_count`).
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
      "statepoint stackmap verification failed for callsite return address {:#x} (patchpoint_id={:#x})",
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
  let stackmaps = parse_all_stackmaps(section).map_err(LoadError::Parse)?;
  let stackmap = merge_stackmap_tables(stackmaps).map_err(LoadError::Parse)?;

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

fn merge_stackmap_tables(mut tables: Vec<StackMap>) -> Result<StackMap, StackMapError> {
  if tables.is_empty() {
    return Err(StackMapError::UnexpectedEof);
  }
  if tables.len() == 1 {
    return Ok(tables.pop().unwrap());
  }

  let total_functions: usize = tables.iter().map(|t| t.functions.len()).sum();
  let total_constants: usize = tables.iter().map(|t| t.constants.len()).sum();
  let total_records: usize = tables.iter().map(|t| t.records.len()).sum();

  let mut out = StackMap {
    version: STACKMAP_VERSION,
    functions: Vec::with_capacity(total_functions),
    constants: Vec::with_capacity(total_constants),
    records: Vec::with_capacity(total_records),
  };

  for mut table in tables {
    debug_assert_eq!(table.version, STACKMAP_VERSION);

    let const_base = out.constants.len();
    out.constants.extend(table.constants);

    if const_base != 0 {
      for rec in &mut table.records {
        for loc in &mut rec.locations {
          if let Location::ConstIndex { index, .. } = loc {
            let new_index = (const_base as u64) + (*index as u64);
            let new_index = u32::try_from(new_index).map_err(|_| StackMapError::UnexpectedEof)?;
            *index = new_index;
          }
        }
      }
    }

    out.functions.extend(table.functions);
    out.records.extend(table.records);
  }

  Ok(out)
}

pub fn verify_statepoint_stackmap(
  stackmap: &StackMap,
  opts: VerifyStatepointOptions,
) -> Result<(), VerifyError> {
  let sp_reg = opts.arch.stack_pointer_dwarf_reg();
  let fp_reg = opts.arch.frame_pointer_dwarf_reg();
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

      if opts.mode == VerifyMode::StatepointsOnly && StatepointRecord::new(rec).is_err() {
        // In "statepoints only" mode, ignore any record that doesn't match the
        // statepoint layout. This includes patchpoints and any other stackmap
        // users.
        continue;
      }

      verify_statepoint_record(stackmap, func, rec, sp_reg, fp_reg, ptr_size)?;
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::load_stackmap;
  use super::DwarfArch;
  use crate::stackmaps::Location;

  fn push_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
  }

  fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
  }

  fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
  }

  fn push_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
  }

  fn push_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_le_bytes());
  }

  fn align_to_8(buf: &mut Vec<u8>) {
    while buf.len() % 8 != 0 {
      buf.push(0);
    }
  }

  fn build_one_table(patchpoint_id: u64, constant: u64) -> Vec<u8> {
    let mut bytes = Vec::new();

    // Header.
    push_u8(&mut bytes, 3);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, 1); // num_functions
    push_u32(&mut bytes, 1); // num_constants
    push_u32(&mut bytes, 1); // num_records

    // Function record.
    push_u64(&mut bytes, 0x1000);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 1); // record_count

    // Constants table.
    push_u64(&mut bytes, constant);

    // Record header.
    push_u64(&mut bytes, patchpoint_id);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1); // num_locations

    // Location: ConstIndex[0].
    push_u8(&mut bytes, 5); // kind
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8); // size
    push_u16(&mut bytes, 0); // dwarf reg (unused)
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0); // constants[0]

    align_to_8(&mut bytes);
    // Live-out header.
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0); // num_liveouts
    align_to_8(&mut bytes);

    bytes
  }

  #[test]
  fn load_stackmap_merges_concatenated_tables_and_rewrites_const_indices() {
    let mut bytes = Vec::new();
    bytes.extend(build_one_table(111, 0xAAAA));
    bytes.extend([0u8; 8]);
    bytes.extend(build_one_table(222, 0xBBBB));

    let merged = load_stackmap(&bytes, DwarfArch::X86_64).expect("load stackmap");
    assert_eq!(merged.constants, vec![0xAAAA, 0xBBBB]);
    assert_eq!(merged.functions.len(), 2);
    assert_eq!(merged.records.len(), 2);

    match &merged.records[0].locations[0] {
      Location::ConstIndex { index, value, .. } => {
        assert_eq!(*index, 0);
        assert_eq!(*value, 0xAAAA);
      }
      other => panic!("expected ConstIndex, got {other:?}"),
    }

    match &merged.records[1].locations[0] {
      Location::ConstIndex { index, value, .. } => {
        assert_eq!(*index, 1);
        assert_eq!(*value, 0xBBBB);
      }
      other => panic!("expected ConstIndex, got {other:?}"),
    }
  }
}

fn verify_statepoint_record(
  _stackmap: &StackMap,
  func: &StackSizeRecord,
  rec: &StackMapRecord,
  sp_reg: u16,
  fp_reg: u16,
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

  // Statepoint header sanity:
  // - `flags` is defined by LLVM as a 2-bit mask (0..3).
  //
  // `callconv` and `deopt_count` are currently opaque to the runtime. They are
  // still decoded/validated structurally by `StatepointRecord::new`, but we do
  // not enforce specific values here.
  let header = sp.header();
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

  let start = sp.gc_pairs_start();
  for (pair_idx, pair) in sp.gc_pairs().iter().enumerate() {
    let base_idx = start + pair_idx * 2;
    let base = &pair.base;
    let derived = &pair.derived;
    verify_indirect_root_slot(
      callsite,
      rec.patchpoint_id,
      func.stack_size,
      sp_reg,
      fp_reg,
      ptr_size,
      base_idx,
      base,
    )?;
    verify_indirect_root_slot(
      callsite,
      rec.patchpoint_id,
      func.stack_size,
      sp_reg,
      fp_reg,
      ptr_size,
      base_idx + 1,
      derived,
    )?;
  }

  Ok(())
}

fn verify_indirect_root_slot(
  callsite: u64,
  patchpoint_id: u64,
  stack_size: StackSize,
  sp_reg: u16,
  fp_reg: u16,
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
    Location::Register { .. } => {
      return Err(VerifyError::new_location(
        callsite,
        patchpoint_id,
        location_index,
        loc,
        "GC root is held in a register, but runtime-native currently only supports stack slots; \
ensure LLVM codegen disables register roots at statepoints (e.g. \
`--fixup-allow-gcptr-in-csr=false` or `--fixup-max-csr-statepoints=0`)."
          .to_string(),
      ))
    }
    _ => {
      return Err(VerifyError::new_location(
        callsite,
        patchpoint_id,
        location_index,
        loc,
        "expected Indirect SP-relative spill slot for GC root".to_string(),
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

  if dwarf_reg == sp_reg {
    // Conservative bound: LLVM records stack slots as offsets from SP. We require a non-negative
    // offset. If the function stack size is known, additionally ensure it doesn't exceed the
    // total frame size.
    let offset64 = offset as i64;
    if offset64 < 0 {
      return Err(VerifyError::new_location(
        callsite,
        patchpoint_id,
        location_index,
        loc,
        "expected non-negative SP offset".to_string(),
      ));
    }
    if let StackSize::Known(stack_size) = stack_size {
      if (offset64 as u64) > stack_size {
        return Err(VerifyError::new_location(
          callsite,
          patchpoint_id,
          location_index,
          loc,
          format!("expected offset within [0, {stack_size}]"),
        ));
      }
    }
  } else if dwarf_reg != fp_reg {
    return Err(VerifyError::new_location(
      callsite,
      patchpoint_id,
      location_index,
      loc,
      format!("expected base register SP (DWARF reg {sp_reg}) or FP (DWARF reg {fp_reg})"),
    ));
  }

  Ok(())
}
