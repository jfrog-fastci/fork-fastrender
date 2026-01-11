//! Parser for LLVM's `.llvm_stackmaps` section.
//!
//! This module parses the raw section bytes emitted by LLVM for stackmaps (v3).
//! It's intentionally small and self-contained so higher-level layers (like
//! statepoint GC root decoding) can depend on it without needing to parse object
//! files.
//!
//! Format reference: LLVM `StackMaps` / `StackMaps.cpp` (version 3).

use thiserror::Error;

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
}

pub const STACKMAP_VERSION: u8 = 3;

#[derive(Debug, Clone)]
pub struct StackMap {
  pub version: u8,
  pub functions: Vec<StackSizeRecord>,
  pub constants: Vec<u64>,
  pub records: Vec<StackMapRecord>,
}

impl StackMap {
  pub fn parse(section: &[u8]) -> Result<Self, StackMapError> {
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

    let mut functions = Vec::with_capacity(num_functions);
    for _ in 0..num_functions {
      functions.push(StackSizeRecord {
        address: c.read_u64()?,
        stack_size: c.read_u64()?,
        record_count: c.read_u64()?,
      });
    }

    let mut constants = Vec::with_capacity(num_constants);
    for _ in 0..num_constants {
      constants.push(c.read_u64()?);
    }

    let mut records = Vec::with_capacity(num_records);
    for _ in 0..num_records {
      let patchpoint_id = c.read_u64()?;
      let instruction_offset = c.read_u32()?;
      let _reserved = c.read_u16()?;
      let num_locations = c.read_u16()? as usize;

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

      // Records are 8-byte aligned after the locations array.
      c.align_to(8)?;

      let num_live_outs = c.read_u16()? as usize;
      let _reserved = c.read_u16()?;
      let mut live_outs = Vec::with_capacity(num_live_outs);
      for _ in 0..num_live_outs {
        live_outs.push(LiveOut {
          dwarf_reg: c.read_u16()?,
          size: c.read_u8()?,
        });
        let _reserved = c.read_u8()?;
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

    Ok(Self {
      version,
      functions,
      constants,
      records,
    })
  }
}

#[derive(Debug, Clone)]
pub struct StackSizeRecord {
  pub address: u64,
  pub stack_size: u64,
  pub record_count: u64,
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
  Register { size: u16, dwarf_reg: u16 },
  Direct { size: u16, dwarf_reg: u16, offset: i32 },
  Indirect { size: u16, dwarf_reg: u16, offset: i32 },
  Constant { size: u16, value: u64 },
  ConstIndex { size: u16, index: u32, value: u64 },
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
      let index = offset_or_small_const as u32;
      let value = *constants.get(index as usize).ok_or(StackMapError::InvalidConstIndex {
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
    let new_off = (self.off + (align - 1)) & !(align - 1);
    if new_off > self.bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    self.off = new_off;
    Ok(())
  }

  fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], StackMapError> {
    if self.off + N > self.bytes.len() {
      return Err(StackMapError::UnexpectedEof);
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&self.bytes[self.off..self.off + N]);
    self.off += N;
    Ok(out)
  }
}

