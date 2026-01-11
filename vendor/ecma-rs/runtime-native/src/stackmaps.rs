//! Parser for LLVM's `.llvm_stackmaps` section (StackMap v3) plus callsite lookup.
//!
//! LLVM emits stackmap metadata for patchpoints and GC statepoints. The runtime
//! uses this to locate safepoints and enumerate GC roots.
//!
//! This module keeps two layers:
//! - [`StackMap`]: a direct parse of the section (tables + records).
//! - [`StackMaps`]: a runtime-friendly view indexed by absolute callsite return
//!   address (PC).
//!
//! Note: for LLVM `gc.statepoint`, the record key is the *return address* of the
//! statepoint callsite. When `patch_bytes > 0`, LLVM 18 reserves a patchable
//! region (x86_64: a NOP sled) and the recorded return address points to the end
//! of that reserved region, not to the byte after a literal `call` instruction.
//!
//! Format reference: LLVM `StackMaps` / `StackMaps.cpp` (version 3).

#[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
use anyhow::Context;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use thiserror::Error;

pub const STACKMAP_VERSION: u8 = 3;
const STACKMAP_HEADER_SIZE: usize = 16;

/// x86_64 SysV DWARF register number for RBP.
pub const X86_64_DWARF_REG_RBP: u16 = 6;
/// x86_64 SysV DWARF register number for RSP.
pub const X86_64_DWARF_REG_RSP: u16 = 7;
/// AArch64 DWARF register number for the frame pointer (X29).
pub const AARCH64_DWARF_REG_FP: u16 = 29;
/// AArch64 DWARF register number for the stack pointer (SP).
pub const AARCH64_DWARF_REG_SP: u16 = 31;

#[derive(Debug, Error)]
pub enum StackMapError {
  #[error("unsupported stackmap version {0} (expected {STACKMAP_VERSION})")]
  UnsupportedVersion(u8),

  #[error("unexpected EOF while reading stackmap section")]
  UnexpectedEof,

  #[error("invalid location kind {0}")]
  InvalidLocationKind(u8),

  #[error("ConstIndex location refers to constants[{index}], but constants.len()={constants_len}")]
  InvalidConstIndex { index: u32, constants_len: usize },

  #[error("non-zero bytes after stackmap blob at section offset {offset}")]
  TrailingNonZeroBytes { offset: usize },

  #[error("overflow while summing stackmap per-function record counts")]
  RecordCountOverflow,

  #[error("stackmap record count mismatch: functions expect {expected}, section has {actual}")]
  RecordCountMismatch { expected: u64, actual: usize },

  #[error("stackmap function record_count {record_count} does not fit in usize")]
  RecordCountTooLarge { record_count: u64 },

  #[error(
    "callsite address overflow: function_address=0x{function_address:x} instruction_offset=0x{instruction_offset:x}"
  )]
  CallSiteAddressOverflow {
    function_address: u64,
    instruction_offset: u32,
  },

  #[error("duplicate stackmap record for callsite pc=0x{pc:x}")]
  DuplicateCallSite { pc: u64 },

  #[error(
    "gc root base register dwarf_reg={dwarf_reg} is unsupported (expected SP or FP: x86_64 RSP/RBP, aarch64 SP/X29)"
  )]
  UnsupportedGcBaseRegister { dwarf_reg: u16 },

  #[error("unsupported GC root location {loc:?}")]
  UnsupportedGcLocation { loc: Location },

  #[error(
    "stackmap stack_size {stack_size} is smaller than the arch frame record size {frame_record_size}"
  )]
  StackSizeTooSmall {
    stack_size: u64,
    frame_record_size: u64,
  },

  #[error(
    "stack slot offset overflow computing FP-relative offset for stack_size={stack_size} off={off}"
  )]
  StackSlotOffsetOverflow { stack_size: u64, off: i32 },

  #[error(transparent)]
  StatepointVerify(#[from] crate::statepoint_verify::VerifyError),

  #[error(transparent)]
  StatepointDecode(#[from] crate::statepoints::StatepointError),
}

#[derive(Debug, Clone)]
pub struct StackMap {
  pub version: u8,
  pub functions: Vec<StackSizeRecord>,
  pub constants: Vec<u64>,
  pub records: Vec<StackMapRecord>,
}

impl StackMap {
  pub fn parse(section: &[u8]) -> Result<Self, StackMapError> {
    let (map, len) = Self::parse_with_len(section)?;
    // A `StackMap` models a single StackMap v3 blob. If the caller accidentally
    // passes the entire `.llvm_stackmaps` output section (which may contain
    // multiple concatenated blobs), fail fast instead of silently dropping the
    // trailing records.
    if let Some(tail) = section.get(len..) {
      // Some toolchains may leave a short non-zero tail (< header size) due to
      // alignment noise. Such a tail cannot contain another StackMap v3 header,
      // so ignore it. Any longer non-zero tail likely indicates the caller
      // passed a linker-concatenated section containing multiple blobs.
      if tail.len() >= STACKMAP_HEADER_SIZE && tail.iter().any(|&b| b != 0) {
        return Err(StackMapError::TrailingNonZeroBytes { offset: len });
      }
    }
    Ok(map)
  }

  pub(crate) fn parse_with_len(section: &[u8]) -> Result<(Self, usize), StackMapError> {
    let mut c = Cursor::new(section);

    let version = c.read_u8()?;
    let _reserved0 = c.read_u8()?;
    let _reserved1 = c.read_u16()?;
    if version != STACKMAP_VERSION {
      return Err(StackMapError::UnsupportedVersion(version));
    }

    let num_functions = c.read_u32()? as usize;
    let num_constants = c.read_u32()? as usize;
    let num_records = c.read_u32()? as usize;

    // Defensively validate count fields against the remaining buffer so malformed inputs don't
    // trigger enormous allocations (e.g. `Vec::with_capacity(u32::MAX)`).
    if num_functions > c.remaining() / StackSizeRecord::BYTE_SIZE {
      return Err(StackMapError::UnexpectedEof);
    }

    let mut functions = Vec::with_capacity(num_functions);
    for _ in 0..num_functions {
      functions.push(StackSizeRecord {
        address: c.read_u64()?,
        stack_size: c.read_u64()?,
        record_count: c.read_u64()?,
      });
    }

    if num_constants > c.remaining() / 8 {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut constants = Vec::with_capacity(num_constants);
    for _ in 0..num_constants {
      constants.push(c.read_u64()?);
    }

    // Each record is at least 24 bytes, even with 0 locations and 0 live-outs.
    const MIN_RECORD_SIZE: usize = 24;
    if num_records > c.remaining() / MIN_RECORD_SIZE {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut records = Vec::with_capacity(num_records);
    for _ in 0..num_records {
      let patchpoint_id = c.read_u64()?;
      let instruction_offset = c.read_u32()?;
      let _reserved = c.read_u16()?;
      let num_locations = c.read_u16()? as usize;

      if num_locations > c.remaining() / Location::BYTE_SIZE {
        return Err(StackMapError::UnexpectedEof);
      }
      let mut locations = Vec::with_capacity(num_locations);
      for _ in 0..num_locations {
        let kind = c.read_u8()?;
        let _reserved0 = c.read_u8()?;
        let size = c.read_u16()?;
        let dwarf_reg = c.read_u16()?;
        let _reserved1 = c.read_u16()?;
        let offset_or_small_const = c.read_i32()?;

        locations.push(parse_location(
          kind,
          size,
          dwarf_reg,
          offset_or_small_const,
          &constants,
        )?);
      }

      // StackMap v3 pads so the live-out header begins on an 8-byte boundary.
      c.align_to(8)?;
      let _padding = c.read_u16()?;
      let num_live_outs = c.read_u16()? as usize;
      if num_live_outs > c.remaining() / LiveOut::BYTE_SIZE {
        return Err(StackMapError::UnexpectedEof);
      }
      let mut live_outs = Vec::with_capacity(num_live_outs);
      for _ in 0..num_live_outs {
        let dwarf_reg = c.read_u16()?;
        let _reserved = c.read_u8()?;
        let size = c.read_u8()?;
        live_outs.push(LiveOut { dwarf_reg, size });
      }

      // Records are 8-byte aligned after the live-out array.
      c.align_to(8)?;

      records.push(StackMapRecord {
        patchpoint_id,
        instruction_offset,
        locations,
        live_outs,
      });
    }

    let len = c.off;
    Ok((
      Self {
        version,
        functions,
        constants,
        records,
      },
      len,
    ))
  }
}

/// Parse all linker-concatenated StackMap v3 blobs within a `.llvm_stackmaps` section.
///
/// ELF linkers (`ld`, `lld`) concatenate input section payloads and may insert
/// alignment padding between them. Each input object contributes a complete
/// StackMap v3 blob, starting with the `version=3` header.
pub fn parse_all_stackmaps(bytes: &[u8]) -> Result<Vec<StackMap>, StackMapError> {
  let mut out: Vec<StackMap> = Vec::new();
  let mut off: usize = 0;

  while off < bytes.len() {
    // Linkers may insert padding between concatenated input sections to satisfy alignment
    // constraints. Skip any 0-filled padding to find the next `version=3` blob header.
    //
    // If the remaining tail is shorter than a StackMap v3 header (16 bytes), it cannot start
    // another blob; ignore it (some toolchains leave short non-zero alignment noise).
    //
    // Some toolchains have also been observed to leave short non-zero padding bytes between blobs.
    // For safety we treat those bytes as ignorable *only* if we can recover a plausible StackMap
    // v3 header shortly after (see the scan below).
    while off < bytes.len() && bytes[off] == 0 {
      off += 1;
    }
    if off >= bytes.len() || bytes.len() - off < STACKMAP_HEADER_SIZE {
      break;
    }

    if bytes[off] != STACKMAP_VERSION {
      // If we land on non-zero padding, try to recover by searching for the
      // next plausible `version=3` header.
      //
      // We limit the scan range to keep us from accidentally "resynchronizing"
      // into the middle of an otherwise-valid blob if our offset accounting is
      // wrong.
      const MAX_PADDING_SCAN: usize = 256;
      let scan_end = (off + MAX_PADDING_SCAN).min(bytes.len().saturating_sub(STACKMAP_HEADER_SIZE));

      let mut found: Option<usize> = None;
      for i in off + 1..=scan_end {
        if bytes[i] == STACKMAP_VERSION && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 0 {
          found = Some(i);
          break;
        }
      }

      if let Some(i) = found {
        off = i;
        continue;
      }

      return Err(StackMapError::TrailingNonZeroBytes { offset: off });
    }

    let (map, len) = StackMap::parse_with_len(&bytes[off..])?;
    if len == 0 {
      return Err(StackMapError::UnexpectedEof);
    }

    out.push(map);
    off = off.checked_add(len).ok_or(StackMapError::UnexpectedEof)?;
    // Linkers align concatenated input sections; `.llvm_stackmaps` uses (at least) 8-byte
    // alignment.
    //
    // Note: some linkers may insert padding beyond the minimum required for alignment (e.g. due to
    // input section alignment). The next iteration will skip any 0-filled padding until the next
    // `version=3` header.
    let aligned = off.checked_add(7).ok_or(StackMapError::UnexpectedEof)? & !7;
    if aligned > bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    off = aligned;
  }

  Ok(out)
}

#[derive(Debug, Clone)]
pub struct StackSizeRecord {
  pub address: u64,
  pub stack_size: u64,
  pub record_count: u64,
}

impl StackSizeRecord {
  const BYTE_SIZE: usize = 24;
}

#[derive(Debug, Clone)]
pub struct StackMapRecord {
  pub patchpoint_id: u64,
  pub instruction_offset: u32,
  pub locations: Vec<Location>,
  pub live_outs: Vec<LiveOut>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
  // Note: `offset` is semantically unused for `Register` locations, but the field is present in
  // the on-disk encoding for all location kinds. Keeping it enables better diagnostics when
  // verifying stackmap invariants.
  Register {
    size: u16,
    dwarf_reg: u16,
    offset: i32,
  },
  Direct {
    size: u16,
    dwarf_reg: u16,
    offset: i32,
  },
  Indirect {
    size: u16,
    dwarf_reg: u16,
    offset: i32,
  },
  Constant {
    size: u16,
    value: u64,
  },
  ConstIndex {
    size: u16,
    index: u32,
    value: u64,
  },
}

impl Location {
  const BYTE_SIZE: usize = 12;
}

impl Location {
  pub fn size(&self) -> u16 {
    match *self {
      Location::Register { size, .. }
      | Location::Direct { size, .. }
      | Location::Indirect { size, .. }
      | Location::Constant { size, .. }
      | Location::ConstIndex { size, .. } => size,
    }
  }
}

#[derive(Debug, Clone)]
pub struct LiveOut {
  pub dwarf_reg: u16,
  pub size: u8,
}

impl LiveOut {
  const BYTE_SIZE: usize = 4;
}

fn parse_location(
  kind: u8,
  size: u16,
  dwarf_reg: u16,
  offset_or_small_const: i32,
  constants: &[u64],
) -> Result<Location, StackMapError> {
  const KIND_REGISTER: u8 = 0x1;
  const KIND_DIRECT: u8 = 0x2;
  const KIND_INDIRECT: u8 = 0x3;
  const KIND_CONSTANT: u8 = 0x4;
  const KIND_CONST_INDEX: u8 = 0x5;

  Ok(match kind {
    KIND_REGISTER => Location::Register {
      size,
      dwarf_reg,
      offset: offset_or_small_const,
    },
    KIND_DIRECT => Location::Direct {
      size,
      dwarf_reg,
      offset: offset_or_small_const,
    },
    KIND_INDIRECT => Location::Indirect {
      size,
      dwarf_reg,
      offset: offset_or_small_const,
    },
    KIND_CONSTANT => Location::Constant {
      size,
      value: (offset_or_small_const as i64) as u64,
    },
    KIND_CONST_INDEX => {
      if offset_or_small_const < 0 {
        return Err(StackMapError::InvalidConstIndex {
          index: offset_or_small_const as u32,
          constants_len: constants.len(),
        });
      }
      let index = offset_or_small_const as u32;
      let value = *constants
        .get(index as usize)
        .ok_or(StackMapError::InvalidConstIndex {
          index,
          constants_len: constants.len(),
        })?;
      Location::ConstIndex { size, index, value }
    }
    other => return Err(StackMapError::InvalidLocationKind(other)),
  })
}

struct Cursor<'a> {
  bytes: &'a [u8],
  off: usize,
}

impl<'a> Cursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, off: 0 }
  }

  fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.off)
  }

  fn read_u8(&mut self) -> Result<u8, StackMapError> {
    if self.off + 1 > self.bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    let v = self.bytes[self.off];
    self.off += 1;
    Ok(v)
  }

  fn read_u16(&mut self) -> Result<u16, StackMapError> {
    let bytes = self.read_exact::<2>()?;
    Ok(u16::from_le_bytes(bytes))
  }

  fn read_u32(&mut self) -> Result<u32, StackMapError> {
    let bytes = self.read_exact::<4>()?;
    Ok(u32::from_le_bytes(bytes))
  }

  fn read_u64(&mut self) -> Result<u64, StackMapError> {
    let bytes = self.read_exact::<8>()?;
    Ok(u64::from_le_bytes(bytes))
  }

  fn read_i32(&mut self) -> Result<i32, StackMapError> {
    let bytes = self.read_exact::<4>()?;
    Ok(i32::from_le_bytes(bytes))
  }

  fn align_to(&mut self, align: usize) -> Result<(), StackMapError> {
    debug_assert!(align.is_power_of_two());
    let add = self
      .off
      .checked_add(align - 1)
      .ok_or(StackMapError::UnexpectedEof)?;
    let new_off = add & !(align - 1);
    if new_off > self.bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    self.off = new_off;
    Ok(())
  }

  fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], StackMapError> {
    let end = self
      .off
      .checked_add(N)
      .ok_or(StackMapError::UnexpectedEof)?;
    if end > self.bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&self.bytes[self.off..end]);
    self.off = end;
    Ok(out)
  }
}
/// A parsed `.llvm_stackmaps` section with a callsite-address index.
///
/// Note: ELF linkers concatenate `.llvm_stackmaps` sections from multiple input
/// object files. This means a final binary may contain multiple back-to-back
/// StackMap v3 blobs. [`StackMaps::parse`] handles this and builds a single
/// callsite index across all blobs.
///
/// This type is the "runtime-friendly" view for safepoint/GC stack walking.
#[derive(Debug, Clone)]
pub struct StackMaps {
  raws: Vec<StackMap>,
  callsites: Vec<CallsiteEntry>,
}

/// Entry in the callsite index, keyed by `pc` (absolute return address).
#[derive(Debug, Clone, Copy)]
pub struct CallsiteEntry {
  pub pc: u64,
  pub function_address: u64,
  /// Fixed stack frame size for the function containing this callsite, as reported by LLVM.
  ///
  /// This is **not** the caller's runtime `SP` at an arbitrary callsite: it excludes dynamic
  /// adjustments like outgoing stack argument pushes. Runtimes must not reconstruct callsite `SP`
  /// from `stack_size`; instead derive it from the callee frame pointer when frame pointers are
  /// enforced (see `stackwalk_fp`).
  pub stack_size: u64,
  pub stackmap_index: usize,
  pub record_index: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct CallSite<'a> {
  /// See [`CallsiteEntry::stack_size`].
  pub stack_size: u64,
  pub record: &'a StackMapRecord,
}

/// A `(base, derived)` relocation pair as emitted by LLVM for `gc.relocate` uses.
///
/// - When `base == derived`, LLVM still emits two locations (often identical).
/// - When `base != derived`, `derived` is a derived/interior pointer that must be
///   relocated relative to `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocPair {
  pub base: Location,
  pub derived: Location,
}

impl<'a> CallSite<'a> {
  /// Iterate over `(base, derived)` pairs used for GC relocation at this callsite.
  ///
  /// LLVM statepoints encode GC relocation pairs after a small prefix:
  /// - 3 constant header locations (callconv, flags, deopt_count)
  /// - `deopt_count` deopt operand locations (not GC roots)
  /// - `(base, derived)` pairs corresponding to `gc.relocate` uses
  ///
  /// This helper uses [`crate::statepoints::StatepointRecord`] as the canonical decoder and
  /// re-exports the decoded `(base, derived)` tail as [`RelocPair`]s.
  ///
  /// For non-statepoint records, it yields an empty iterator.
  pub fn reloc_pairs(&self) -> impl Iterator<Item = RelocPair> + '_ {
    enum RelocPairsIter<'a> {
      Empty,
      Pairs(core::slice::Iter<'a, crate::statepoints::GcLocationPair>),
    }

    impl<'a> Iterator for RelocPairsIter<'a> {
      type Item = RelocPair;

      fn next(&mut self) -> Option<Self::Item> {
        match self {
          RelocPairsIter::Empty => None,
          RelocPairsIter::Pairs(iter) => iter.next().map(|pair| RelocPair {
            base: pair.base.clone(),
            derived: pair.derived.clone(),
          }),
        }
      }
    }

    // `gc.relocate` pairing is only meaningful for LLVM statepoints. Detect statepoints by their
    // structural prefix (3 constant header locations), not by `patchpoint_id`: LLVM allows overriding
    // the ID (via `"statepoint-id"`) and does not guarantee a fixed constant. Misclassifying a
    // statepoint as a generic patchpoint would cause us to treat deopt operands as roots or skip
    // relocation pairs entirely.
    let looks_like_statepoint = self.record.locations.len()
      >= crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS
      && self.record.locations[..crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS]
        .iter()
        .all(|loc| matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }));
    if !looks_like_statepoint {
      return RelocPairsIter::Empty;
    }

    let statepoint = match crate::statepoints::StatepointRecord::new(self.record) {
      Ok(sp) => sp,
      Err(err) => {
        // Some non-statepoint patchpoints might (coincidentally) start with constant-like operands;
        // treat decode failures as fatal only when the record uses LLVM's default statepoint id.
        if self.record.patchpoint_id == crate::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID {
          debug_assert!(
            false,
            "failed to decode statepoint stackmap record for reloc_pairs (err={err:?} record={:?})",
            self.record
          );
        }
        return RelocPairsIter::Empty;
      }
    };
    RelocPairsIter::Pairs(statepoint.gc_pairs().iter())
  }

  /// Return a deduplicated list of GC root stack slots as offsets from the frame pointer (RBP/x29).
  ///
  /// This is a statepoint-oriented helper: it decodes the LLVM `gc.statepoint`
  /// record layout and enumerates only the `(base, derived)` GC root pairs (the
  /// leading header constants and any deopt operands are ignored).
  ///
  /// Note: for derived pointers where `base != derived`, this returns **only the base slots**.
  /// Derived slots are not GC roots and require special relocation based on the corresponding base
  /// (handled by `stackwalk_fp` / `gc.relocate` pairing).
  ///
  /// For non-statepoint records (those without the 3-constant prefix), this
  /// falls back to scanning all locations and treating `Indirect` stack slots as
  /// GC roots.
  ///
  /// Normalization (requires frame pointers):
  /// - `Indirect [SP + off]` becomes `fp_off = frame_record_size - stack_size + off`
  ///   - x86_64: `frame_record_size = 8` (saved RBP; return address is outside `stack_size`)
  ///   - aarch64: `frame_record_size = 16` (saved X29 + X30)
  /// - `Indirect [FP + off]` becomes `fp_off = off`
  ///
  /// Note: this conversion uses `stack_size` to relate `SP` and `FP`. That is only valid when the
  /// caller's `SP` at the callsite matches the function's fixed frame size (i.e. no outgoing-argument
  /// pushes/stack adjustments at that specific callsite). Runtime stack walking must instead use
  /// per-callsite `SP` derived from the callee frame pointer (see `stackwalk_fp`).
  pub fn gc_root_rbp_offsets_strict(&self) -> Result<Vec<i32>, StackMapError> {
    let mut out: Vec<i32> = Vec::new();
    // Detect LLVM `gc.statepoint` record layout by structure, not by `patchpoint_id`.
    //
    // A statepoint record begins with the fixed-length LLVM 18 header prefix, which is a sequence
    // of constant-like locations. Our codegen currently uses `LLVM_STATEPOINT_PATCHPOINT_ID` as a
    // convention, but LLVM does not contractually guarantee that value (and external/older
    // stackmaps may differ). Misclassifying a statepoint as a generic patchpoint would cause us to
    // treat deopt operands as GC roots.
    let looks_like_statepoint = self.record.locations.len()
      >= crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS
      && self.record.locations[..crate::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS]
        .iter()
        .all(|loc| matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }));

    let sp_relative_to_fp_relative =
      |frame_record_size: u64, offset: i32| -> Result<i32, StackMapError> {
      if self.stack_size < frame_record_size {
        return Err(StackMapError::StackSizeTooSmall {
          stack_size: self.stack_size,
          frame_record_size,
        });
      }

      let fp_off = (frame_record_size as i128) - (self.stack_size as i128) + (offset as i128);
      if !(i32::MIN as i128..=i32::MAX as i128).contains(&fp_off) {
        return Err(StackMapError::StackSlotOffsetOverflow {
          stack_size: self.stack_size,
          off: offset,
        });
      }
      Ok(fp_off as i32)
    };

    let location_fp_offset = |dwarf_reg: u16, offset: i32| -> Result<i32, StackMapError> {
      match dwarf_reg {
        X86_64_DWARF_REG_RBP | AARCH64_DWARF_REG_FP => Ok(offset),
        X86_64_DWARF_REG_RSP => sp_relative_to_fp_relative(8, offset),
        AARCH64_DWARF_REG_SP => sp_relative_to_fp_relative(16, offset),
        other => Err(StackMapError::UnsupportedGcBaseRegister { dwarf_reg: other }),
      }
    };

    if looks_like_statepoint {
      // If the record is marked with the (codegen-convention) statepoint patchpoint id, treat decode
      // errors as fatal. Otherwise, fall back to the non-statepoint scanning path (some patchpoints
      // may also start with constant operands).
      let statepoint = match crate::statepoints::StatepointRecord::new(self.record) {
        Ok(sp) => Some(sp),
        Err(err) => {
          if self.record.patchpoint_id == crate::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID {
            return Err(err.into());
          }
          None
        }
      };

      if let Some(statepoint) = statepoint {
        for pair in statepoint.gc_pairs() {
          let base = &pair.base;
          let derived = &pair.derived;

          // Strict mode: even though we only return the base slot, validate the derived location too
          // so callers catch unsupported register roots / base registers early.
          match *derived {
            Location::Indirect {
              dwarf_reg, offset, ..
            } => {
              let _ = location_fp_offset(dwarf_reg, offset)?;
            }
            _ => return Err(StackMapError::UnsupportedGcLocation { loc: derived.clone() }),
          }

          let fp_off = match *base {
            Location::Indirect {
              dwarf_reg, offset, ..
            } => location_fp_offset(dwarf_reg, offset)?,

            // Strict mode: reject roots in registers / direct expressions / constants.
            _ => return Err(StackMapError::UnsupportedGcLocation { loc: base.clone() }),
          };

          out.push(fp_off);
        }
      } else {
        for loc in &self.record.locations {
          match *loc {
            Location::Indirect {
              dwarf_reg, offset, ..
            } => {
              out.push(location_fp_offset(dwarf_reg, offset)?);
            }

            // Ignore constants (used by statepoint headers and patchpoints).
            Location::Constant { .. } | Location::ConstIndex { .. } => {}

            // Strict mode: reject roots in registers / direct expressions.
            _ => return Err(StackMapError::UnsupportedGcLocation { loc: loc.clone() }),
          }
        }
      }
    } else {
      for loc in &self.record.locations {
        match *loc {
          Location::Indirect {
            dwarf_reg, offset, ..
          } => {
            out.push(location_fp_offset(dwarf_reg, offset)?);
          }

          // Ignore constants (used by statepoint headers and patchpoints).
          Location::Constant { .. } | Location::ConstIndex { .. } => {}

          // Strict mode: reject roots in registers / direct expressions.
          _ => return Err(StackMapError::UnsupportedGcLocation { loc: loc.clone() }),
        }
      }
    }

    out.sort_unstable();
    out.dedup();
    Ok(out)
  }

  pub fn gc_root_slots(&self) -> Result<Vec<i32>, StackMapError> {
    self.gc_root_rbp_offsets_strict()
  }
}

impl StackMaps {
  pub fn parse(section: &[u8]) -> Result<Self, StackMapError> {
    let raws = parse_all_stackmaps(section)?;
    if raws.is_empty() {
      return Err(StackMapError::UnexpectedEof);
    }

    // Fail fast if LLVM/codegen start emitting statepoint roots in registers or
    // otherwise violate our spill-to-stack assumptions.
    #[cfg(any(debug_assertions, feature = "verify-statepoints"))]
    {
      use crate::statepoint_verify::{
        verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
      };

      #[cfg(target_arch = "x86_64")]
      let arch = DwarfArch::X86_64;
      #[cfg(target_arch = "aarch64")]
      let arch = DwarfArch::AArch64;
      #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
      compile_error!("statepoint stackmap verification only supports x86_64 and aarch64");

      for raw in &raws {
        verify_statepoint_stackmap(
          raw,
          VerifyStatepointOptions {
            arch,
            mode: VerifyMode::StatepointsOnly,
          },
        )?;
      }
    }

    let mut callsites: Vec<CallsiteEntry> = Vec::new();

    for (stackmap_index, raw) in raws.iter().enumerate() {
      let mut expected: u64 = 0;
      for f in &raw.functions {
        expected = expected
          .checked_add(f.record_count)
          .ok_or(StackMapError::RecordCountOverflow)?;
      }

      if expected != raw.records.len() as u64 {
        return Err(StackMapError::RecordCountMismatch {
          expected,
          actual: raw.records.len(),
        });
      }

      callsites.reserve(raw.records.len());

      let mut record_index: usize = 0;
      for f in &raw.functions {
        let record_count =
          usize::try_from(f.record_count).map_err(|_| StackMapError::RecordCountTooLarge {
            record_count: f.record_count,
          })?;

        for _ in 0..record_count {
          let record = &raw.records[record_index];
          let pc = f
            .address
            .checked_add(record.instruction_offset as u64)
            .ok_or(StackMapError::CallSiteAddressOverflow {
              function_address: f.address,
              instruction_offset: record.instruction_offset,
            })?;

          callsites.push(CallsiteEntry {
            pc,
            function_address: f.address,
            stack_size: f.stack_size,
            stackmap_index,
            record_index,
          });

          record_index += 1;
        }
      }
    }

    callsites.sort_by_key(|e| e.pc);

    // A malformed section could contain two records for the same callsite PC.
    // Reject this to avoid ambiguous GC root enumeration.
    for window in callsites.windows(2) {
      if let [a, b] = window {
        if a.pc == b.pc {
          return Err(StackMapError::DuplicateCallSite { pc: a.pc });
        }
      }
    }

    Ok(Self { raws, callsites })
  }

  #[inline]
  pub fn lookup(&self, callsite_return_addr: u64) -> Option<CallSite<'_>> {
    let idx = self
      .callsites
      .binary_search_by_key(&callsite_return_addr, |e| e.pc)
      .ok()?;
    let entry = &self.callsites[idx];
    let raw = self.raws.get(entry.stackmap_index)?;
    Some(CallSite {
      stack_size: entry.stack_size,
      record: raw.records.get(entry.record_index)?,
    })
  }

  #[inline]
  pub fn lookup_entry(&self, callsite_return_addr: u64) -> Option<&CallsiteEntry> {
    self
      .callsites
      .binary_search_by_key(&callsite_return_addr, |e| e.pc)
      .ok()
      .map(|idx| &self.callsites[idx])
  }

  /// Convenience overload for callers using `usize` PCs on 64-bit runtimes.
  #[inline]
  pub fn lookup_return_address(&self, pc: usize) -> Option<CallSite<'_>> {
    self.lookup(pc as u64)
  }

  pub fn callsites(&self) -> &[CallsiteEntry] {
    &self.callsites
  }

  pub fn iter(&self) -> impl Iterator<Item = (u64, CallSite<'_>)> + '_ {
    self.callsites.iter().map(|entry| {
      let raw = &self.raws[entry.stackmap_index];
      (
        entry.pc,
        CallSite {
          stack_size: entry.stack_size,
          record: &raw.records[entry.record_index],
        },
      )
    })
  }

  /// Return the first parsed StackMap blob.
  ///
  /// Most callers should prefer [`StackMaps::raws`]. This accessor exists for
  /// backwards compatibility with older code that assumed `.llvm_stackmaps`
  /// contained a single blob.
  pub fn raw(&self) -> &StackMap {
    &self.raws[0]
  }

  /// Return all parsed StackMap blobs from the input section.
  pub fn raws(&self) -> &[StackMap] {
    &self.raws
  }

  /// Parse the in-memory stackmaps section using linker-defined boundary symbols.
  ///
  /// This requires the final linked binary to contain a stackmaps section
  /// (`.llvm_stackmaps` or `.data.rel.ro.llvm_stackmaps`) and define:
  ///
  /// - `__start_llvm_stackmaps`
  /// - `__stop_llvm_stackmaps`
  ///
  /// These symbols are provided by `runtime-native/link/stackmaps.ld` (and are
  /// `KEEP`ed so `--gc-sections` does not discard the section).
  ///
  /// The linker script also defines compatibility aliases:
  /// - `__stackmaps_{start,end}`
  /// - `__fastr_stackmaps_{start,end}`
  /// - `__llvm_stackmaps_{start,end}`
  ///
  /// For a higher-level API that discovers stackmaps for the current process
  /// without relying on linker-defined symbols, see [`crate::load_stackmaps_from_self`]
  /// / [`crate::stackmaps_loader::try_load_stackmaps_from_self_linux_phdr`].
  #[cfg(all(target_os = "linux", feature = "llvm_stackmaps_linker"))]
  pub fn parse_from_linker_symbols() -> Result<Self, StackMapError> {
    let bytes = crate::stackmaps_loader::load_llvm_stackmaps_via_symbols().unwrap_or(&[]);
    Self::parse(bytes)
  }

  /// Load the stackmaps section for the current process (Linux 64-bit little-endian).
  ///
  /// This is PIE/ASLR-safe because it reads from *mapped memory* (relocations already applied)
  /// rather than from the on-disk bytes.
  #[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
  pub fn load_self() -> anyhow::Result<Self> {
    let bytes = crate::stackmaps_loader::stackmaps_section();
    if bytes.is_empty() {
      anyhow::bail!("main executable is missing a stackmaps section");
    }
    Ok(Self::parse(bytes).context("parse stackmaps section")?)
  }

  #[cfg(not(all(target_os = "linux", target_pointer_width = "64", target_endian = "little")))]
  pub fn load_self() -> anyhow::Result<Self> {
    anyhow::bail!("StackMaps::load_self is only supported on Linux 64-bit little-endian");
  }
}

// -----------------------------------------------------------------------------
// Global stackmap registry (multi-module / dlopen / JIT)
// -----------------------------------------------------------------------------

/// A handle to a callsite entry stored in a [`StackMapRegistry`].
///
/// This keeps the underlying parsed [`StackMaps`] alive via an [`Arc`], so it
/// can be used after dropping the registry lock.
#[derive(Clone, Debug)]
pub struct CallSiteHandle {
  pc: u64,
  stack_size: u64,
  stackmaps: Arc<StackMaps>,
  stackmap_index: usize,
  record_index: usize,
}

impl CallSiteHandle {
  #[inline]
  pub fn pc(&self) -> u64 {
    self.pc
  }

  #[inline]
  pub fn stack_size(&self) -> u64 {
    self.stack_size
  }

  #[inline]
  pub fn record(&self) -> &StackMapRecord {
    // Safety/invariants:
    // - `stackmaps` is kept alive by `Arc`.
    // - `stackmap_index`/`record_index` are derived from `StackMaps::callsites()`.
    &self.stackmaps.raws()[self.stackmap_index].records[self.record_index]
  }

  #[inline]
  pub fn as_call_site(&self) -> CallSite<'_> {
    CallSite {
      stack_size: self.stack_size,
      record: self.record(),
    }
  }
}

#[derive(Debug, Error)]
pub enum StackMapRegistryError {
  #[error("stackmaps register/unregister received a null pointer")]
  NullPointer,

  #[error("invalid stackmaps byte range: end < start")]
  InvalidRange,

  #[error("stackmaps provider already registered with a different byte range")]
  AlreadyRegisteredDifferentRange,

  #[error(transparent)]
  Parse(#[from] StackMapError),
}

#[derive(Debug, Default)]
pub struct StackMapRegistry {
  providers: HashMap<usize, Provider>,
  callsites: HashMap<u64, CallSiteHandle>,
}

#[derive(Debug)]
struct Provider {
  end: usize,
  callsites: Vec<u64>,
}

impl StackMapRegistry {
  #[inline]
  pub fn lookup(&self, callsite_return_addr: u64) -> Option<CallSiteHandle> {
    self.callsites.get(&callsite_return_addr).cloned()
  }

  #[inline]
  pub fn lookup_return_address(&self, pc: usize) -> Option<CallSiteHandle> {
    self.lookup(pc as u64)
  }

  /// Register a stackmaps provider, identified by its `[start, end)` byte range.
  ///
  /// The range is parsed as a `.llvm_stackmaps` output section (which may contain
  /// multiple concatenated StackMap v3 blobs).
  ///
  /// When a module is `dlclose`d, call [`StackMapRegistry::unregister`] with the
  /// same `start` pointer to remove its callsite mappings.
  pub fn register(
    &mut self,
    start: *const u8,
    end: *const u8,
  ) -> Result<(), StackMapRegistryError> {
    if start.is_null() || end.is_null() {
      return Err(StackMapRegistryError::NullPointer);
    }

    let start_addr = start as usize;
    let end_addr = end as usize;
    if end_addr < start_addr {
      return Err(StackMapRegistryError::InvalidRange);
    }

    if let Some(existing) = self.providers.get(&start_addr) {
      if existing.end == end_addr {
        // Idempotent registration.
        return Ok(());
      }
      return Err(StackMapRegistryError::AlreadyRegisteredDifferentRange);
    }

    let len = end_addr - start_addr;
    if len == 0 {
      self.providers.insert(
        start_addr,
        Provider {
          end: end_addr,
          callsites: Vec::new(),
        },
      );
      return Ok(());
    }

    // Safety: caller promises `[start, end)` is a valid readable range.
    let bytes = unsafe { std::slice::from_raw_parts(start, len) };
    let stackmaps = Arc::new(StackMaps::parse(bytes)?);

    let mut provider = Provider {
      end: end_addr,
      callsites: Vec::new(),
    };

    for entry in stackmaps.callsites() {
      let pc = entry.pc;
      if self.callsites.contains_key(&pc) {
        // Deterministic rule: first wins.
        #[cfg(debug_assertions)]
        eprintln!("runtime-native: duplicate stackmap for callsite pc=0x{pc:x}; keeping first");
        continue;
      }

      self.callsites.insert(
        pc,
        CallSiteHandle {
          pc,
          stack_size: entry.stack_size,
          stackmaps: stackmaps.clone(),
          stackmap_index: entry.stackmap_index,
          record_index: entry.record_index,
        },
      );
      provider.callsites.push(pc);
    }

    self.providers.insert(start_addr, provider);
    Ok(())
  }

  /// Unregister a previously registered provider.
  pub fn unregister(&mut self, start: *const u8) -> bool {
    if start.is_null() {
      return false;
    }
    let start_addr = start as usize;
    let Some(provider) = self.providers.remove(&start_addr) else {
      return false;
    };

    for pc in provider.callsites {
      self.callsites.remove(&pc);
    }
    true
  }

  /// Discover `.llvm_stackmaps` sections in all currently loaded ELF images and register them.
  ///
  /// This is a best-effort Linux fallback for environments where modules cannot
  /// auto-register (e.g. missing constructors/symbols).
  #[cfg(target_os = "linux")]
  pub fn load_all_loaded_modules(&mut self) -> anyhow::Result<usize> {
    let sections = crate::stackmap_loader::load_all_llvm_stackmaps()?;

    let mut newly_registered = 0usize;
    for section in sections {
      let start = section.as_ptr();
      let end = unsafe { section.as_ptr().add(section.len()) };

      let before = self.providers.len();
      self
        .register(start, end)
        .map_err(|err| anyhow::Error::new(err))?;
      if self.providers.len() > before {
        newly_registered += 1;
      }
    }

    Ok(newly_registered)
  }
}

static GLOBAL_STACKMAP_REGISTRY: OnceLock<RwLock<StackMapRegistry>> = OnceLock::new();

/// Access the global merged stackmap registry.
pub fn global_stackmap_registry() -> &'static RwLock<StackMapRegistry> {
  GLOBAL_STACKMAP_REGISTRY.get_or_init(|| RwLock::new(StackMapRegistry::default()))
}

/// Convenience helper: look up a callsite in the global registry by return address.
pub fn lookup(callsite_pc: usize) -> Option<CallSiteHandle> {
  global_stackmap_registry()
    .read()
    .lookup_return_address(callsite_pc)
}

/// Register a `.llvm_stackmaps` byte range into the global registry.
///
/// This is the public ABI for dynamically loaded modules (`dlopen`) and JITs.
#[no_mangle]
pub extern "C" fn rt_stackmaps_register(start: *const u8, end: *const u8) -> bool {
  crate::ffi::abort_on_panic(|| global_stackmap_registry().write().register(start, end).is_ok())
}

/// Unregister a previously registered provider from the global registry.
#[no_mangle]
pub extern "C" fn rt_stackmaps_unregister(start: *const u8) -> bool {
  crate::ffi::abort_on_panic(|| global_stackmap_registry().write().unregister(start))
}

/// Emit an ELF `.init_array` constructor which registers the current module's
/// `__llvm_stackmaps_start/end` range.
///
/// The containing module must define `__llvm_stackmaps_start` /
/// `__llvm_stackmaps_end`, typically via the linker script in
/// `runtime-native/link/stackmaps.ld`.
#[macro_export]
macro_rules! rt_stackmaps_init_array {
  () => {
    #[cfg(target_os = "linux")]
    mod __rt_stackmaps_init_array {
      extern "C" {
        static __llvm_stackmaps_start: u8;
        static __llvm_stackmaps_end: u8;
      }

      unsafe extern "C" fn __rt_stackmaps_ctor() {
        let start = core::ptr::addr_of!(__llvm_stackmaps_start) as *const u8;
        let end = core::ptr::addr_of!(__llvm_stackmaps_end) as *const u8;
        let _ = $crate::stackmaps::rt_stackmaps_register(start, end);
      }

      #[used]
      #[link_section = ".init_array"]
      static __RT_STACKMAPS_INIT: unsafe extern "C" fn() = __rt_stackmaps_ctor;
    }
  };
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::process::Command;

  use tempfile::TempDir;

  use super::Location;
  use super::StackMap;
  use super::StackMapError;
  use super::StackMaps;
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

  fn align_to_8_with(buf: &mut Vec<u8>, byte: u8) {
    while buf.len() % 8 != 0 {
      buf.push(byte);
    }
  }

  fn build_header(buf: &mut Vec<u8>, num_functions: u32, num_constants: u32, num_records: u32) {
    push_u8(buf, 3); // version
    push_u8(buf, 0); // rsv0
    push_u16(buf, 0); // rsv1
    push_u32(buf, num_functions);
    push_u32(buf, num_constants);
    push_u32(buf, num_records);
  }

  fn minimal_blob(function_addr: u64, patchpoint_id: u64, instruction_offset: u32) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    // Function record.
    push_u64(&mut bytes, function_addr);
    push_u64(&mut bytes, 32);
    push_u64(&mut bytes, 1);

    // Record (no locations / no liveouts).
    push_u64(&mut bytes, patchpoint_id);
    push_u32(&mut bytes, instruction_offset);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    align_to_8_with(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    align_to_8_with(&mut bytes, 0);

    bytes
  }

  #[test]
  fn parse_minimal_valid_stackmaps_index() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    // Function record.
    push_u64(&mut bytes, 0x1000); // addr
    push_u64(&mut bytes, 32); // stack_size
    push_u64(&mut bytes, 1); // record_count

    // Record.
    push_u64(&mut bytes, 1); // patchpoint_id
    push_u32(&mut bytes, 0x10); // instruction_offset
    push_u16(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 1); // num_locations

    // Location: Indirect [RSP + 16], size 8.
    push_u8(&mut bytes, 3); // kind = Indirect
    push_u8(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 8); // size
    push_u16(&mut bytes, crate::stackwalk::DWARF_SP_REG); // dwarf_reg
    push_u16(&mut bytes, 0); // reserved2
    push_i32(&mut bytes, 16); // offset

    // StackMap v3 pads so the live-out header begins on an 8-byte boundary.
    align_to_8_with(&mut bytes, 0);

    // No liveouts.
    push_u16(&mut bytes, 0); // padding
    push_u16(&mut bytes, 0); // num_liveouts
    align_to_8_with(&mut bytes, 0);

    let sm = StackMaps::parse(&bytes).unwrap();
    let callsite = sm.lookup(0x1010).unwrap();
    assert_eq!(callsite.stack_size, 32);

    let fp_to_entry_sp = crate::stackwalk::FP_TO_ENTRY_SP_OFFSET as i32;
    assert_eq!(
      callsite.gc_root_rbp_offsets_strict().unwrap(),
      vec![fp_to_entry_sp - 32 + 16]
    );
  }

  #[test]
  fn derived_pointers_return_base_offsets() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    // Function record.
    push_u64(&mut bytes, 0x1000); // addr
    push_u64(&mut bytes, 40); // stack_size
    push_u64(&mut bytes, 1); // record_count

    // Record (statepoint-style).
    //
    // Note: LLVM's `rewrite-statepoints-for-gc` supports overriding the statepoint ID via callsite
    // directives (`"statepoint-id"="..."`). The patchpoint ID in the stackmap record therefore
    // must not be used as a statepoint-layout discriminator.
    push_u64(&mut bytes, 42); // patchpoint_id
    push_u32(&mut bytes, 0x10); // instruction_offset
    push_u16(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 5); // num_locations = 3 header + 1 (base,derived) pair

    // Statepoint header constants (callconv, flags, deopt_count).
    for _ in 0..3 {
      push_u8(&mut bytes, 4); // kind = Constant
      push_u8(&mut bytes, 0); // reserved
      push_u16(&mut bytes, 8); // size
      push_u16(&mut bytes, 0); // dwarf_reg
      push_u16(&mut bytes, 0); // reserved
      push_i32(&mut bytes, 0); // constant value
    }

    // base: Indirect [RSP + 0]
    push_u8(&mut bytes, 3); // kind = Indirect
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, crate::stackwalk::DWARF_SP_REG);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // derived: Indirect [RSP + 8] (different spill slot => derived pointer)
    push_u8(&mut bytes, 3); // kind = Indirect
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, crate::stackwalk::DWARF_SP_REG);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 8);

    // LLVM stackmap v3 aligns the live-out header to 8 bytes after the locations array.
    align_to_8_with(&mut bytes, 0);

    // Live-out header: u16 Padding; u16 NumLiveOuts (none).
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    align_to_8_with(&mut bytes, 0);

    let sm = StackMaps::parse(&bytes).unwrap();
    let callsite = sm.lookup(0x1010).unwrap();
    let fp_to_entry_sp = crate::stackwalk::FP_TO_ENTRY_SP_OFFSET as i32;
    assert_eq!(
      callsite.gc_root_rbp_offsets_strict().unwrap(),
      vec![fp_to_entry_sp - 40]
    );
  }

  #[test]
  fn parse_all_supports_concatenated_stackmap_blobs() {
    let blob_a = minimal_blob(0x1000, 1, 0x10);
    let blob_b = minimal_blob(0x2000, 2, 0x20);
    let mut concat = blob_a.clone();
    concat.extend_from_slice(&blob_b);

    let sm = StackMaps::parse(&concat).unwrap();
    assert_eq!(sm.raws().len(), 2);

    assert_eq!(sm.raws()[0].records[0].patchpoint_id, 1);
    assert_eq!(sm.raws()[1].records[0].patchpoint_id, 2);

    // Ensure the per-blob callsite indexes are still correct.
    assert!(sm.lookup(0x1010).is_some());
    assert!(sm.lookup(0x2020).is_some());
  }

  #[test]
  fn stackmap_parse_rejects_concatenated_blobs() {
    let blob_a = minimal_blob(0x1000, 1, 0x10);
    let blob_b = minimal_blob(0x2000, 2, 0x20);
    let mut concat = blob_a.clone();
    // Simulate linker padding between input sections.
    concat.extend_from_slice(&[0u8; 8]);
    concat.extend_from_slice(&blob_b);

    let err = StackMap::parse(&concat).unwrap_err();
    assert!(
      matches!(err, StackMapError::TrailingNonZeroBytes { offset } if offset == blob_a.len()),
      "expected TrailingNonZeroBytes at offset {}, got {err:?}",
      blob_a.len()
    );
  }

  #[test]
  fn parse_all_stackmaps_skips_non_zero_padding_between_blobs() {
    let blob_a = minimal_blob(0x1000, 1, 0x10);
    let blob_b = minimal_blob(0x2000, 2, 0x20);
    let mut concat = blob_a.clone();

    // Some toolchains have been observed to leave non-zero bytes in linker padding; treat them
    // as ignorable if we can recover the next `version=3` header.
    concat.extend_from_slice(&[0xAA; 8]);
    concat.extend_from_slice(&blob_b);

    let sm = StackMaps::parse(&concat).unwrap();
    assert_eq!(sm.raws().len(), 2);

    // Ensure the callsite indexes are still correct.
    assert!(sm.lookup(0x1010).is_some());
    assert!(sm.lookup(0x2020).is_some());
  }

  #[test]
  fn parse_all_stackmaps_ignores_short_trailing_bytes() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    // Function record.
    push_u64(&mut bytes, 0x1000); // addr
    push_u64(&mut bytes, 32); // stack_size
    push_u64(&mut bytes, 1); // record_count

    // Record.
    push_u64(&mut bytes, 1); // patchpoint_id
    push_u32(&mut bytes, 0x10); // instruction_offset
    push_u16(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 1); // num_locations

    // Location: Indirect [RSP + 16], size 8.
    push_u8(&mut bytes, 3); // kind = Indirect
    push_u8(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 8); // size
    push_u16(&mut bytes, crate::stackwalk::DWARF_SP_REG); // dwarf_reg
    push_u16(&mut bytes, 0); // reserved2
    push_i32(&mut bytes, 16); // offset

    // LLVM stackmap v3 aligns the live-out header to 8 bytes after the locations array.
    align_to_8_with(&mut bytes, 0);

    // No liveouts.
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    align_to_8_with(&mut bytes, 0);

    // Short non-zero tail (e.g. section alignment noise from a non-standard toolchain).
    bytes.extend_from_slice(&[0xAA; 8]);

    let blobs = super::parse_all_stackmaps(&bytes).unwrap();
    assert_eq!(blobs.len(), 1);

    // The callsite index should still build successfully.
    let idx = StackMaps::parse(&bytes).unwrap();
    assert!(idx.lookup(0x1010).is_some());
  }

  #[test]
  fn parse_all_location_kinds() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 1, 1);

    // Function record.
    push_u64(&mut bytes, 0x2000);
    push_u64(&mut bytes, 64);
    push_u64(&mut bytes, 1);

    // Constants table (1 entry).
    push_u64(&mut bytes, 0xdead_beef_dead_beef);

    // Record.
    push_u64(&mut bytes, 7);
    push_u32(&mut bytes, 0x20);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 5); // 5 locations

    // Register
    push_u8(&mut bytes, 1);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0); // rax
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // Direct [RSP + 32]
    push_u8(&mut bytes, 2);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, crate::stackwalk::DWARF_SP_REG);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 32);

    // Indirect [RBP - 16]
    push_u8(&mut bytes, 3);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, crate::stackwalk::DWARF_FP_REG);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, -16);

    // Constant
    push_u8(&mut bytes, 4);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 1234);

    // ConstantIndex (0)
    push_u8(&mut bytes, 5);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // StackMap v3 pads so the live-out header begins on an 8-byte boundary.
    align_to_8_with(&mut bytes, 0);

    push_u16(&mut bytes, 0); // padding
    push_u16(&mut bytes, 0); // num_liveouts
    align_to_8_with(&mut bytes, 0);

    let sm = StackMap::parse(&bytes).unwrap();
    assert_eq!(sm.records.len(), 1);
    let locs = &sm.records[0].locations;
    assert_eq!(locs.len(), 5);
    assert!(matches!(locs[0], Location::Register { .. }));
    assert!(matches!(locs[1], Location::Direct { .. }));
    assert!(matches!(locs[2], Location::Indirect { .. }));
    assert!(matches!(locs[3], Location::Constant { .. }));
    assert!(matches!(locs[4], Location::ConstIndex { index: 0, .. }));
  }

  #[test]
  fn parse_concatenated_stackmap_blobs_from_multiple_objects() {
    // When linking multiple object files without full LTO, `.llvm_stackmaps`
    // commonly contains multiple independent StackMap v3 blobs concatenated with
    // zero padding between them.

    fn build_min_blob(func_addr: u64, patchpoint_id: u64, inst_offset: u32) -> Vec<u8> {
      let mut bytes = Vec::new();
      build_header(&mut bytes, 1, 0, 1);

      // Function record.
      push_u64(&mut bytes, func_addr);
      push_u64(&mut bytes, 32); // stack_size
      push_u64(&mut bytes, 1); // record_count

      // Record with 0 locations / 0 live-outs.
      push_u64(&mut bytes, patchpoint_id);
      push_u32(&mut bytes, inst_offset);
      push_u16(&mut bytes, 0);
      push_u16(&mut bytes, 0); // num_locations

      // Align to live-out header.
      align_to_8_with(&mut bytes, 0);
      push_u16(&mut bytes, 0); // padding
      push_u16(&mut bytes, 0); // num_liveouts
      align_to_8_with(&mut bytes, 0);

      bytes
    }

    let blob_a = build_min_blob(0x1000, 1, 0x10);
    let blob_b = build_min_blob(0x2000, 2, 0x20);

    let mut section = Vec::new();
    section.extend_from_slice(&blob_a);
    section.extend_from_slice(&[0u8; 16]); // linker alignment padding
    section.extend_from_slice(&blob_b);
    section.extend_from_slice(&[0u8; 8]); // trailing padding

    let sm = StackMaps::parse(&section).unwrap();
    assert_eq!(sm.raws().len(), 2);
    assert_eq!(sm.raws()[0].functions.len(), 1);
    assert_eq!(sm.raws()[0].records.len(), 1);
    assert_eq!(sm.raws()[1].functions.len(), 1);
    assert_eq!(sm.raws()[1].records.len(), 1);
    assert_eq!(sm.callsites().len(), 2);
    assert_eq!(sm.lookup(0x1010).unwrap().record.patchpoint_id, 1);
    assert_eq!(sm.lookup(0x2020).unwrap().record.patchpoint_id, 2);
  }

  #[test]
  fn record_padding_is_respected() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 2);

    // Function record.
    push_u64(&mut bytes, 0x3000);
    push_u64(&mut bytes, 16);
    push_u64(&mut bytes, 2);

    // Record 0: include an even number of liveouts so the record-end padding path is exercised.
    push_u64(&mut bytes, 100);
    push_u32(&mut bytes, 0x10);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1); // num_locations

    // Register location.
    push_u8(&mut bytes, 1);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // Pad so the live-out header begins 8-byte aligned.
    // Fill with non-zero bytes to validate the parser skips it.
    align_to_8_with(&mut bytes, 0xAB);

    // Live-out header:
    //   u16 Padding;
    //   u16 NumLiveOuts;
    // Keep the padding field non-zero to validate the parser ignores its content.
    // Include an even number of liveouts so the record-end padding path is exercised.
    push_u16(&mut bytes, 0xABAB); // padding (ignored)
    push_u16(&mut bytes, 2); // num_liveouts

    // LiveOut: reg=0,reserved=0,size=8.
    push_u16(&mut bytes, 0);
    push_u8(&mut bytes, 0);
    push_u8(&mut bytes, 8);

    // LiveOut[1]: reg=1,reserved=0,size=8.
    push_u16(&mut bytes, 1);
    push_u8(&mut bytes, 0);
    push_u8(&mut bytes, 8);

    // Pad with non-zero to validate we skip, not validate content.
    align_to_8_with(&mut bytes, 0xCD);

    // Record 1: no locations, no liveouts.
    push_u64(&mut bytes, 200);
    push_u32(&mut bytes, 0x20);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0); // no locations
    align_to_8_with(&mut bytes, 0); // align before live-out header (no-op here)
    push_u16(&mut bytes, 0); // padding
    push_u16(&mut bytes, 0); // num_liveouts
    align_to_8_with(&mut bytes, 0);

    let sm = StackMap::parse(&bytes).unwrap();
    assert_eq!(sm.records.len(), 2);
    assert_eq!(sm.records[0].patchpoint_id, 100);
    assert_eq!(sm.records[1].patchpoint_id, 200);
    assert_eq!(sm.records[0].live_outs.len(), 2);
    assert_eq!(sm.records[0].live_outs[0].size, 8);
    assert_eq!(sm.records[0].live_outs[1].size, 8);
  }

  #[test]
  fn constant_index_out_of_range_errors() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    push_u64(&mut bytes, 0x4000);
    push_u64(&mut bytes, 0);
    push_u64(&mut bytes, 1);

    push_u64(&mut bytes, 1);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);

    // ConstantIndex but no constants table.
    push_u8(&mut bytes, 5);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // StackMap v3 pads so the live-out header begins on an 8-byte boundary.
    align_to_8_with(&mut bytes, 0);

    push_u16(&mut bytes, 0); // padding
    push_u16(&mut bytes, 0); // num_liveouts
    align_to_8_with(&mut bytes, 0);

    let err = StackMap::parse(&bytes).unwrap_err();
    assert!(matches!(err, StackMapError::InvalidConstIndex { .. }));
  }

  #[test]
  fn invalid_location_kind_errors() {
    let mut bytes: Vec<u8> = Vec::new();
    build_header(&mut bytes, 1, 0, 1);

    // Function record.
    push_u64(&mut bytes, 0x5000); // addr
    push_u64(&mut bytes, 0); // stack_size
    push_u64(&mut bytes, 1); // record_count

    // Record.
    push_u64(&mut bytes, 1); // patchpoint_id
    push_u32(&mut bytes, 0); // instruction_offset
    push_u16(&mut bytes, 0); // reserved
    push_u16(&mut bytes, 1); // num_locations

    // Invalid kind byte.
    push_u8(&mut bytes, 0xFF);
    push_u8(&mut bytes, 0);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_i32(&mut bytes, 0);

    // LLVM stackmap v3 aligns the live-out header to 8 bytes after the locations array.
    align_to_8_with(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    align_to_8_with(&mut bytes, 0);

    let err = StackMap::parse(&bytes).unwrap_err();
    assert!(matches!(err, StackMapError::InvalidLocationKind(0xFF)));
  }

  fn read_elf64_le_section<'a>(file: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if file.len() < 0x40 {
      return None;
    }
    if &file[0..4] != b"\x7fELF" {
      return None;
    }
    if file[4] != 2 || file[5] != 1 {
      return None;
    }

    let e_shoff = u64::from_le_bytes(file[0x28..0x30].try_into().ok()?);
    let e_shentsize = u16::from_le_bytes(file[0x3A..0x3C].try_into().ok()?);
    let e_shnum = u16::from_le_bytes(file[0x3C..0x3E].try_into().ok()?);
    let e_shstrndx = u16::from_le_bytes(file[0x3E..0x40].try_into().ok()?);

    let shoff = usize::try_from(e_shoff).ok()?;
    let shentsize = usize::from(e_shentsize);
    let shnum = usize::from(e_shnum);
    let shstrndx = usize::from(e_shstrndx);

    if shoff.checked_add(shentsize.checked_mul(shnum)?)? > file.len() {
      return None;
    }

    let sh_at = |idx: usize| -> Option<&[u8]> {
      let start = shoff.checked_add(idx.checked_mul(shentsize)?)?;
      let end = start.checked_add(shentsize)?;
      file.get(start..end)
    };

    let shstr = sh_at(shstrndx)?;
    let shstr_off = u64::from_le_bytes(shstr[0x18..0x20].try_into().ok()?);
    let shstr_size = u64::from_le_bytes(shstr[0x20..0x28].try_into().ok()?);
    let shstr_off = usize::try_from(shstr_off).ok()?;
    let shstr_size = usize::try_from(shstr_size).ok()?;
    let strtab = file.get(shstr_off..shstr_off.checked_add(shstr_size)?)?;

    for i in 0..shnum {
      let sh = sh_at(i)?;
      if sh.len() < 0x40 {
        return None;
      }
      let sh_name = u32::from_le_bytes(sh[0..4].try_into().ok()?);
      let name_off = usize::try_from(sh_name).ok()?;
      let rest = strtab.get(name_off..)?;
      let nul = rest.iter().position(|&b| b == 0)?;
      let sec_name = std::str::from_utf8(&rest[..nul]).ok()?;
      if sec_name == name {
        let off = u64::from_le_bytes(sh[0x18..0x20].try_into().ok()?);
        let size = u64::from_le_bytes(sh[0x20..0x28].try_into().ok()?);
        let off = usize::try_from(off).ok()?;
        let size = usize::try_from(size).ok()?;
        return file.get(off..off.checked_add(size)?);
      }
    }
    None
  }

  #[test]
  #[ignore]
  fn llvm18_stackmap_roundtrip_smoke() {
    // This test requires LLVM 18 tools (`llc-18`, `llvm-readobj-18`) to be installed.
    let tmp = TempDir::new().unwrap();
    let ll_path = tmp.path().join("smoke.ll");
    let obj_path = tmp.path().join("smoke.o");

    // Generate enough arguments so some are passed on the stack (Indirect), plus an alloca
    // pointer (often Direct), plus a few values passed in registers (Register).
    let ll = r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @smoke(i64 %a0, i64 %a1, i64 %a2, i64 %a3, i64 %a4, i64 %a5, i64 %a6, i64 %a7) {
entry:
  %slot = alloca i64, align 8
  call void (i64, i32, ...) @llvm.experimental.stackmap(
    i64 1, i32 0,
    ptr %slot,
    i64 %a0, i64 %a1, i64 %a2, i64 %a3, i64 %a4, i64 %a5, i64 %a6, i64 %a7
  )
  ret void
}
"#;
    fs::write(&ll_path, ll).unwrap();

    let status = Command::new("llc-18")
      .arg("-filetype=obj")
      .arg(&ll_path)
      .arg("-o")
      .arg(&obj_path)
      .status();
    let Ok(status) = status else {
      return;
    };
    if !status.success() {
      return;
    }

    let obj = fs::read(&obj_path).unwrap();
    let section =
      read_elf64_le_section(&obj, ".llvm_stackmaps").expect("missing .llvm_stackmaps section");

    let sm = StackMap::parse(section).unwrap();
    assert_eq!(sm.version, 3);

    let out = Command::new("llvm-readobj-18")
      .arg("--stackmap")
      .arg(&obj_path)
      .output()
      .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("StackMap Version: 3"));

    assert!(stdout.contains("Register"));
    assert!(stdout.contains("Indirect"));
    assert!(stdout.contains("Direct"));

    let mut seen_reg = false;
    let mut seen_indirect = false;
    let mut seen_direct = false;
    for rec in &sm.records {
      for loc in &rec.locations {
        match loc {
          Location::Register { .. } => seen_reg = true,
          Location::Indirect { .. } => seen_indirect = true,
          Location::Direct { .. } => seen_direct = true,
          _ => {}
        }
      }
    }
    assert!(seen_reg);
    assert!(seen_indirect);
    assert!(seen_direct);
  }
}
