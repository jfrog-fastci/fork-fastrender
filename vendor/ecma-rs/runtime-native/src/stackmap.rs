//! LLVM StackMap v3 parser + statepoint GC-root extraction.
//!
//! ## Derived pointers (interior pointers)
//! LLVM statepoints encode GC "live" pointers as `(base, derived)` pairs after a
//! 3-location constant prefix. When `base != derived`, the statepoint is
//! describing a derived (interior) pointer that must be relocated using the base
//! object identity.
//!
//! `runtime-native` v1 does **not** implement derived-pointer relocation yet. To
//! prevent silent GC corruption, we **fail fast** when we observe `base !=
//! derived` pairs.

use core::fmt;
use std::sync::OnceLock;

static STACKMAPS_INDEX: OnceLock<crate::stackmaps::StackMaps> = OnceLock::new();

/// Lazily parse and index the process' in-memory `.llvm_stackmaps` section.
///
/// This is intended for runtime stack walking / GC root enumeration. It panics if stackmaps are
/// unavailable or malformed.
pub fn stackmaps() -> &'static crate::stackmaps::StackMaps {
  STACKMAPS_INDEX.get_or_init(|| {
    let bytes = crate::stackmaps_section();
    if bytes.is_empty() {
      panic!(
        "missing .llvm_stackmaps section: on Linux, build with feature `llvm_stackmaps_linker`; on macOS, ensure LLVM emitted `__LLVM_STACKMAPS,__llvm_stackmaps`"
      );
    }

    crate::stackmaps::StackMaps::parse(bytes).unwrap_or_else(|err| {
      panic!("failed to parse .llvm_stackmaps section: {err}");
    })
  })
}

/// Parsed `.llvm_stackmaps` section (StackMap v3).
#[derive(Debug, Clone)]
pub struct StackMap {
  pub version: u8,
  pub functions: Vec<FunctionInfo>,
  pub records: Vec<StackMapRecord>,
  record_index_by_safepoint: Vec<(u64, usize)>,
}

impl StackMap {
  pub fn parse(section: &[u8]) -> Result<Self, StackMapError> {
    let mut r = Reader::new(section);

    let version = r.read_u8()?;
    if version != 3 {
      return Err(StackMapError::UnsupportedVersion(version));
    }

    let _reserved0 = r.read_u8()?;
    let _reserved1 = r.read_u16()?;
    let num_functions = r.read_u32()? as usize;
    let num_constants = r.read_u32()? as usize;
    let num_records = r.read_u32()? as usize;

    // Defensively validate count fields against the remaining buffer so malformed inputs don't
    // trigger enormous allocations (e.g. `Vec::with_capacity(u32::MAX)`).
    if num_functions > r.remaining() / 24 {
      return Err(StackMapError::UnexpectedEof);
    }

    let mut functions = Vec::with_capacity(num_functions);
    for _ in 0..num_functions {
      let function_address = r.read_u64()?;
      let stack_size = r.read_u64()?;
      let record_count = r.read_u64()? as usize;
      functions.push(FunctionInfo {
        function_address,
        stack_size,
        record_count,
      });
    }

    if num_constants > r.remaining() / 8 {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut constants = Vec::with_capacity(num_constants);
    for _ in 0..num_constants {
      constants.push(r.read_u64()?);
    }

    let mut expected_records: usize = 0;
    for f in &functions {
      expected_records = expected_records
        .checked_add(f.record_count)
        .ok_or(StackMapError::RecordCountOverflow)?;
    }
    if expected_records != num_records {
      return Err(StackMapError::RecordCountMismatch {
        header: num_records,
        parsed: expected_records,
      });
    }

    // Each record is at least 24 bytes, even with 0 locations and 0 live-outs.
    const MIN_RECORD_SIZE: usize = 24;
    if num_records > r.remaining() / MIN_RECORD_SIZE {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut records = Vec::with_capacity(num_records);
    for f in &functions {
      for _ in 0..f.record_count {
        records.push(StackMapRecord::parse_for_function(&mut r, f, &constants)?);
      }
    }

    if records.len() != num_records {
      return Err(StackMapError::RecordCountMismatch {
        header: num_records,
        parsed: records.len(),
      });
    }

    let mut record_index_by_safepoint = Vec::with_capacity(records.len());
    for (idx, rec) in records.iter().enumerate() {
      record_index_by_safepoint.push((rec.safepoint_address(), idx));
    }
    record_index_by_safepoint.sort_by_key(|&(addr, _)| addr);

    Ok(Self {
      version,
      functions,
      records,
      record_index_by_safepoint,
    })
  }

  /// Find a parsed record by its safepoint address (`FunctionAddress + InstructionOffset`).
  pub fn record_for_safepoint(&self, safepoint_addr: u64) -> Option<&StackMapRecord> {
    let idx = self
      .record_index_by_safepoint
      .binary_search_by_key(&safepoint_addr, |&(addr, _)| addr)
      .ok()
      .map(|i| self.record_index_by_safepoint[i].1)?;
    self.records.get(idx)
  }
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
  pub function_address: u64,
  pub stack_size: u64,
  pub record_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
  Register {
    size: u16,
    dwarf_reg_num: u16,
  },
  Direct {
    size: u16,
    dwarf_reg_num: u16,
    offset: i32,
  },
  Indirect {
    size: u16,
    dwarf_reg_num: u16,
    offset: i32,
  },
  Constant {
    size: u16,
    value: i64,
  },
}

impl Location {
  fn same_storage_as(&self, other: &Self) -> bool {
    use Location::*;
    match (self, other) {
      (Register { dwarf_reg_num: a, .. }, Register { dwarf_reg_num: b, .. }) => a == b,
      (
        Direct {
          dwarf_reg_num: a_reg,
          offset: a_off,
          ..
        },
        Direct {
          dwarf_reg_num: b_reg,
          offset: b_off,
          ..
        },
      ) => a_reg == b_reg && a_off == b_off,
      (
        Indirect {
          dwarf_reg_num: a_reg,
          offset: a_off,
          ..
        },
        Indirect {
          dwarf_reg_num: b_reg,
          offset: b_off,
          ..
        },
      ) => a_reg == b_reg && a_off == b_off,
      (Constant { value: a, .. }, Constant { value: b, .. }) => a == b,
      _ => false,
    }
  }
}

impl fmt::Display for Location {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    use Location::*;
    match self {
      Register {
        size,
        dwarf_reg_num,
      } => write!(f, "Register(reg={dwarf_reg_num}, size={size})"),
      Direct {
        size,
        dwarf_reg_num,
        offset,
      } => write!(f, "Direct(reg={dwarf_reg_num}, offset={offset}, size={size})"),
      Indirect {
        size,
        dwarf_reg_num,
        offset,
      } => write!(f, "Indirect(reg={dwarf_reg_num}, offset={offset}, size={size})"),
      Constant { size, value } => write!(f, "Constant(value={value}, size={size})"),
    }
  }
}

/// One record in a StackMap v3 section.
#[derive(Debug, Clone)]
pub struct StackMapRecord {
  pub patchpoint_id: u64,
  pub instruction_offset: u32,
  pub function_address: u64,
  pub stack_size: u64,

  locations_prefix: [Location; 3],
  locations_pairs: Vec<(Location, Location)>,
}

pub struct StatepointLocations<'a> {
  pub prefix: &'a [Location],
  pub pairs: &'a [(Location, Location)],
}

impl StackMapRecord {
  fn parse_for_function(
    r: &mut Reader<'_>,
    f: &FunctionInfo,
    constants: &[u64],
  ) -> Result<Self, StackMapError> {
    let patchpoint_id = r.read_u64()?;
    let instruction_offset = r.read_u32()?;
    let _reserved = r.read_u16()?;
    let num_locations = r.read_u16()? as usize;

    // Location entries are 12 bytes each.
    if num_locations > r.remaining() / 12 {
      return Err(StackMapError::UnexpectedEof);
    }

    if num_locations < 3 {
      return Err(StackMapError::InvalidStatepointLocations {
        patchpoint_id,
        reason: InvalidStatepointLocationsReason::TooFewLocations { num_locations },
      });
    }
    if (num_locations - 3) % 2 != 0 {
      return Err(StackMapError::InvalidStatepointLocations {
        patchpoint_id,
        reason: InvalidStatepointLocationsReason::OddLocationRemainder { num_locations },
      });
    }

    let mut locations = Vec::with_capacity(num_locations);
    for _ in 0..num_locations {
      locations.push(parse_location(r, constants)?);
    }

    // After `Locations[]`, StackMap v3 pads so the live-out header starts on an
    // 8-byte boundary.
    //
    // The live-out header itself is:
    //   u16 Padding;
    //   u16 NumLiveOuts;
    r.pad_to_align(8)?;
    let _padding = r.read_u16()?;
    let num_live_outs = r.read_u16()? as usize;

    // Live-out entries are 4 bytes each (u16 reg, u8 reserved, u8 size).
    if num_live_outs > r.remaining() / 4 {
      return Err(StackMapError::UnexpectedEof);
    }
    for _ in 0..num_live_outs {
      let _dwarf_reg_num = r.read_u16()?;
      let _reserved = r.read_u8()?;
      let _size = r.read_u8()?;
    }
    r.pad_to_align(8)?;

    let mut iter = locations.into_iter();
    let prefix0 = iter.next().unwrap();
    let prefix1 = iter.next().unwrap();
    let prefix2 = iter.next().unwrap();
    let locations_prefix = [prefix0, prefix1, prefix2];

    let mut locations_pairs = Vec::with_capacity((num_locations - 3) / 2);
    while let Some(base) = iter.next() {
      let derived = iter.next().ok_or(StackMapError::InvalidStatepointLocations {
        patchpoint_id,
        reason: InvalidStatepointLocationsReason::OddLocationRemainder { num_locations },
      })?;
      locations_pairs.push((base, derived));
    }

    Ok(Self {
      patchpoint_id,
      instruction_offset,
      function_address: f.function_address,
      stack_size: f.stack_size,
      locations_prefix,
      locations_pairs,
    })
  }

  pub fn safepoint_address(&self) -> u64 {
    self.function_address + self.instruction_offset as u64
  }

  /// Interpret this record as an LLVM statepoint stack map record.
  ///
  /// LLVM encodes statepoint records as:
  /// - a 3-location constant prefix (each should be `Constant(0)`), followed by
  /// - `(base, derived)` pairs for each GC-live pointer.
  ///
  /// See: <https://llvm.org/docs/Statepoints.html>
  pub fn statepoint_locations(&self) -> Result<StatepointLocations<'_>, StackMapError> {
    // The statepoint prefix is expected to be three `Constant(0)` entries.
    // We validate the value, not just the location kind.
    for (idx, loc) in self.locations_prefix.iter().enumerate() {
      match loc {
        Location::Constant { value: 0, .. } => {}
        other => {
          return Err(StackMapError::InvalidStatepointLocations {
            patchpoint_id: self.patchpoint_id,
            reason: InvalidStatepointLocationsReason::BadPrefix {
              index: idx,
              found: other.clone(),
            },
          });
        }
      }
    }

    Ok(StatepointLocations {
      prefix: &self.locations_prefix,
      pairs: &self.locations_pairs,
    })
  }

  /// Extract GC root stack slots from a statepoint record, with strict derived-pointer handling.
  ///
  /// Returns offsets relative to the frame pointer (`RBP`) for each unique GC root slot.
  ///
  /// - Only supports stack slots (`Location::Indirect`) based on `RSP` or `RBP`.
  /// - Rejects derived pointers: if any `(base, derived)` pair differs in storage,
  ///   returns [`StackMapError::DerivedPointerNotSupported`].
  pub fn gc_root_rbp_offsets_strict(&self) -> Result<Vec<i32>, StackMapError> {
    use std::collections::BTreeSet;

    let sp = self.statepoint_locations()?;

    let mut seen = BTreeSet::<i32>::new();
    let mut out = Vec::new();

    for (base, derived) in sp.pairs {
      if !base.same_storage_as(derived) {
        return Err(StackMapError::DerivedPointerNotSupported {
          base: base.clone(),
          derived: derived.clone(),
        });
      }

      let rbp_off = self.location_rbp_offset(base)?;
      if seen.insert(rbp_off) {
        out.push(rbp_off);
      }
    }

    Ok(out)
  }

  fn location_rbp_offset(&self, loc: &Location) -> Result<i32, StackMapError> {
    match loc {
      Location::Indirect {
        dwarf_reg_num,
        offset,
        ..
      } => {
        // DWARF register numbers for x86_64 SysV:
        //   6 = RBP, 7 = RSP
        #[cfg(target_arch = "x86_64")]
        match *dwarf_reg_num {
          6 => Ok(*offset),
          7 => {
            let stack_size_i64 =
              i64::try_from(self.stack_size).map_err(|_| StackMapError::StackSizeOverflow {
                stack_size: self.stack_size,
              })?;
            let rbp_off = i64::from(*offset) - stack_size_i64;
            i32::try_from(rbp_off).map_err(|_| StackMapError::RbpOffsetOverflow {
              stack_size: self.stack_size,
              rsp_offset: *offset,
              rbp_offset: rbp_off,
            })
          }
          other => Err(StackMapError::UnsupportedStackSlotBaseRegister {
            dwarf_reg_num: other,
          }),
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
          Err(StackMapError::UnsupportedStackSlotBaseRegister {
            dwarf_reg_num: *dwarf_reg_num,
          })
        }
      }
      other => Err(StackMapError::UnsupportedRootLocation(other.clone())),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackMapError {
  UnexpectedEof,
  UnsupportedVersion(u8),
  RecordCountOverflow,
  RecordCountMismatch { header: usize, parsed: usize },
  InvalidLocationKind { kind: u8 },
  ConstantIndexOutOfRange { index: usize, constants_len: usize },
  NegativeConstIndex { index: i32 },
  ConstantValueOutOfRange { value: u64 },
  NonZeroPaddingByte { offset: usize, byte: u8 },

  InvalidStatepointLocations {
    patchpoint_id: u64,
    reason: InvalidStatepointLocationsReason,
  },

  /// A `(base, derived)` pair in the statepoint describes a derived pointer.
  /// The runtime does not currently support relocating derived pointers.
  DerivedPointerNotSupported { base: Location, derived: Location },

  UnsupportedRootLocation(Location),
  UnsupportedStackSlotBaseRegister { dwarf_reg_num: u16 },
  StackSizeOverflow { stack_size: u64 },
  RbpOffsetOverflow {
    stack_size: u64,
    rsp_offset: i32,
    rbp_offset: i64,
  },
}

impl fmt::Display for StackMapError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    use StackMapError::*;
    match self {
      UnexpectedEof => write!(f, "unexpected EOF while parsing LLVM stackmap"),
      UnsupportedVersion(v) => write!(f, "unsupported LLVM stackmap version {v} (expected 3)"),
      RecordCountOverflow => write!(f, "stackmap record count overflow while summing functions"),
      RecordCountMismatch { header, parsed } => write!(
        f,
        "stackmap record count mismatch: header says {header}, parsed {parsed}"
      ),
      InvalidLocationKind { kind } => write!(f, "invalid stackmap location kind {kind}"),
      ConstantIndexOutOfRange {
        index,
        constants_len,
      } => write!(
        f,
        "stackmap ConstIndex out of range: index={index}, constants_len={constants_len}"
      ),
      NegativeConstIndex { index } => write!(f, "stackmap ConstIndex is negative: {index}"),
      ConstantValueOutOfRange { value } => {
        write!(f, "stackmap constant value out of i64 range: {value}")
      }
      NonZeroPaddingByte { offset, byte } => {
        write!(f, "stackmap padding byte at offset {offset} was non-zero: {byte:#04x}")
      }
      InvalidStatepointLocations {
        patchpoint_id,
        reason,
      } => write!(f, "invalid statepoint locations for patchpoint {patchpoint_id}: {reason}"),
      DerivedPointerNotSupported { base, derived } => write!(
        f,
        "derived pointers are not supported (base={base}, derived={derived})"
      ),
      UnsupportedRootLocation(loc) => write!(f, "unsupported GC root location: {loc}"),
      UnsupportedStackSlotBaseRegister { dwarf_reg_num } => write!(
        f,
        "unsupported stack slot base register (DWARF reg {dwarf_reg_num}); only RSP/RBP are supported"
      ),
      StackSizeOverflow { stack_size } => write!(f, "stack size overflows i64: {stack_size}"),
      RbpOffsetOverflow {
        stack_size,
        rsp_offset,
        rbp_offset,
      } => write!(
        f,
        "RBP offset overflows i32 (stack_size={stack_size}, rsp_offset={rsp_offset}, rbp_offset={rbp_offset})"
      ),
    }
  }
}

impl std::error::Error for StackMapError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidStatepointLocationsReason {
  TooFewLocations { num_locations: usize },
  OddLocationRemainder { num_locations: usize },
  BadPrefix { index: usize, found: Location },
}

impl fmt::Display for InvalidStatepointLocationsReason {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      InvalidStatepointLocationsReason::TooFewLocations { num_locations } => write!(
        f,
        "expected at least 3 locations (prefix), got {num_locations}"
      ),
      InvalidStatepointLocationsReason::OddLocationRemainder { num_locations } => write!(
        f,
        "expected (locations.len() - 3) to be even; got num_locations={num_locations}"
      ),
      InvalidStatepointLocationsReason::BadPrefix { index, found } => {
        write!(f, "invalid prefix at index {index}: {found}")
      }
    }
  }
}

fn parse_location(r: &mut Reader<'_>, constants: &[u64]) -> Result<Location, StackMapError> {
  let kind = r.read_u8()?;
  let _reserved0 = r.read_u8()?;
  let size = r.read_u16()?;
  let dwarf_reg_num = r.read_u16()?;
  let _reserved1 = r.read_u16()?;
  let offset_or_small_constant = r.read_i32()?;

  match kind {
    1 => Ok(Location::Register {
      size,
      dwarf_reg_num,
    }),
    2 => Ok(Location::Direct {
      size,
      dwarf_reg_num,
      offset: offset_or_small_constant,
    }),
    3 => Ok(Location::Indirect {
      size,
      dwarf_reg_num,
      offset: offset_or_small_constant,
    }),
    4 => Ok(Location::Constant {
      size,
      value: i64::from(offset_or_small_constant),
    }),
    5 => {
      if offset_or_small_constant < 0 {
        return Err(StackMapError::NegativeConstIndex {
          index: offset_or_small_constant,
        });
      }
      let idx = offset_or_small_constant as usize;
      let value = *constants.get(idx).ok_or(StackMapError::ConstantIndexOutOfRange {
        index: idx,
        constants_len: constants.len(),
      })?;
      let value_i64 = i64::try_from(value).map_err(|_| StackMapError::ConstantValueOutOfRange {
        value,
      })?;
      Ok(Location::Constant {
        size,
        value: value_i64,
      })
    }
    other => Err(StackMapError::InvalidLocationKind { kind: other }),
  }
}

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }

  fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.pos)
  }

  fn read_u8(&mut self) -> Result<u8, StackMapError> {
    let b = *self.bytes.get(self.pos).ok_or(StackMapError::UnexpectedEof)?;
    self.pos += 1;
    Ok(b)
  }

  fn read_u16(&mut self) -> Result<u16, StackMapError> {
    let bytes = self
      .bytes
      .get(self.pos..self.pos + 2)
      .ok_or(StackMapError::UnexpectedEof)?;
    self.pos += 2;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
  }

  fn read_u32(&mut self) -> Result<u32, StackMapError> {
    let bytes = self
      .bytes
      .get(self.pos..self.pos + 4)
      .ok_or(StackMapError::UnexpectedEof)?;
    self.pos += 4;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
  }

  fn read_u64(&mut self) -> Result<u64, StackMapError> {
    let bytes = self
      .bytes
      .get(self.pos..self.pos + 8)
      .ok_or(StackMapError::UnexpectedEof)?;
    self.pos += 8;
    Ok(u64::from_le_bytes([
      bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
  }

  fn read_i32(&mut self) -> Result<i32, StackMapError> {
    let bytes = self
      .bytes
      .get(self.pos..self.pos + 4)
      .ok_or(StackMapError::UnexpectedEof)?;
    self.pos += 4;
    Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
  }

  fn pad_to_align(&mut self, align: usize) -> Result<(), StackMapError> {
    while self.pos % align != 0 {
      let offset = self.pos;
      let b = self.read_u8()?;
      if b != 0 {
        return Err(StackMapError::NonZeroPaddingByte { offset, byte: b });
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
  }
  fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
  }

  fn push_location_constant(out: &mut Vec<u8>, size: u16, value: i32) {
    push_u8(out, 4); // Constant
    push_u8(out, 0);
    push_u16(out, size);
    push_u16(out, 0);
    push_u16(out, 0);
    push_i32(out, value);
  }

  fn push_location_indirect(out: &mut Vec<u8>, size: u16, dwarf_reg_num: u16, offset: i32) {
    push_u8(out, 3); // Indirect
    push_u8(out, 0);
    push_u16(out, size);
    push_u16(out, dwarf_reg_num);
    push_u16(out, 0);
    push_i32(out, offset);
  }

  fn stackmap_bytes_with_pair(base_off: i32, derived_off: i32) -> Vec<u8> {
    let mut out = Vec::new();

    // Header
    push_u8(&mut out, 3); // Version
    push_u8(&mut out, 0); // Reserved0
    push_u16(&mut out, 0); // Reserved1
    push_u32(&mut out, 1); // NumFunctions
    push_u32(&mut out, 0); // NumConstants
    push_u32(&mut out, 1); // NumRecords

    // Function
    push_u64(&mut out, 0x1000); // FunctionAddress
    push_u64(&mut out, 32); // StackSize
    push_u64(&mut out, 1); // RecordCount

    // Record
    push_u64(&mut out, 1); // PatchPointID
    push_u32(&mut out, 0); // InstructionOffset
    push_u16(&mut out, 0); // Reserved
    push_u16(&mut out, 5); // NumLocations = 3 prefix + 1 pair

    for _ in 0..3 {
      push_location_constant(&mut out, 8, 0);
    }
    // DWARF x86_64: 7 = RSP
    push_location_indirect(&mut out, 8, 7, base_off);
    push_location_indirect(&mut out, 8, 7, derived_off);

    // StackMap v3 aligns the live-out header (u16 Padding + u16 NumLiveOuts) to an 8-byte boundary
    // after the locations array. This parser requires the padding bytes be zero.
    while out.len() % 8 != 0 {
      push_u8(&mut out, 0);
    }

    // Live-out header: u16 Padding; u16 NumLiveOuts.
    push_u16(&mut out, 0); // Padding
    push_u16(&mut out, 0); // NumLiveOuts = 0

    // Records are 8-byte aligned after the live-out array too (even when empty).
    while out.len() % 8 != 0 {
      push_u8(&mut out, 0);
    }
    out
  }

  #[test]
  fn statepoint_base_equals_derived_ok() {
    let bytes = stackmap_bytes_with_pair(8, 8);
    let sm = StackMap::parse(&bytes).expect("parse");
    assert_eq!(sm.records.len(), 1);
    let rec = &sm.records[0];

    let sp = rec.statepoint_locations().expect("statepoint_locations");
    assert_eq!(sp.prefix.len(), 3);
    assert_eq!(sp.pairs.len(), 1);

    let roots = rec.gc_root_rbp_offsets_strict().expect("gc roots");
    assert_eq!(roots, vec![-24]); // 8 - stack_size(32)
  }

  #[test]
  fn statepoint_derived_pointer_pair_errors() {
    let bytes = stackmap_bytes_with_pair(8, 16);
    let sm = StackMap::parse(&bytes).expect("parse");
    let rec = &sm.records[0];

    let err = rec
      .gc_root_rbp_offsets_strict()
      .expect_err("expected derived pointer error");

    match err {
      StackMapError::DerivedPointerNotSupported { base, derived } => {
        assert_ne!(base, derived);
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }
}
