use crate::stackmaps::{Location, StackMaps, STACKMAP_VERSION};

#[cfg(target_arch = "aarch64")]
use crate::stackmaps::{AARCH64_DWARF_REG_FP, AARCH64_DWARF_REG_SP};
#[cfg(target_arch = "x86_64")]
use crate::stackmaps::{X86_64_DWARF_REG_RBP, X86_64_DWARF_REG_RSP};

/// Validation errors for stackmap conformance against the runtime stack scanner assumptions.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
  #[error("unsupported stackmap version {found} (expected {expected})")]
  UnsupportedVersion { found: u8, expected: u8 },

  #[error(
    "callsite pc=0x{pc:x} patchpoint_id={patchpoint_id} instruction_offset={instruction_offset}: odd number of non-constant locations after filtering Constant/ConstIndex: {count}"
  )]
  OddLocationCount {
    pc: u64,
    patchpoint_id: u64,
    instruction_offset: u32,
    count: usize,
  },

  #[error(
    "callsite pc=0x{pc:x} patchpoint_id={patchpoint_id} instruction_offset={instruction_offset}: unsupported location kind {kind} at filtered location #{location_index}"
  )]
  UnsupportedLocationKind {
    pc: u64,
    patchpoint_id: u64,
    instruction_offset: u32,
    location_index: usize,
    kind: &'static str,
  },

  #[error(
    "callsite pc=0x{pc:x} patchpoint_id={patchpoint_id} instruction_offset={instruction_offset}: pointer location #{location_index} has size {size}, expected pointer size {ptr_size}"
  )]
  UnexpectedPointerSize {
    pc: u64,
    patchpoint_id: u64,
    instruction_offset: u32,
    location_index: usize,
    size: u16,
    ptr_size: u16,
  },

  #[error(
    "callsite pc=0x{pc:x} patchpoint_id={patchpoint_id} instruction_offset={instruction_offset}: pointer location #{location_index} uses unsupported DWARF base reg {dwarf_reg} (allowed: {allowed:?})"
  )]
  UnsupportedBaseReg {
    pc: u64,
    patchpoint_id: u64,
    instruction_offset: u32,
    location_index: usize,
    dwarf_reg: u16,
    allowed: &'static [u16],
  },

  #[error(
    "callsite pc=0x{pc:x} patchpoint_id={patchpoint_id} instruction_offset={instruction_offset}: pointer location #{location_index} has unaligned offset {offset} (ptr size {ptr_size})"
  )]
  UnalignedOffset {
    pc: u64,
    patchpoint_id: u64,
    instruction_offset: u32,
    location_index: usize,
    offset: i32,
    ptr_size: i32,
  },
}

/// Validate that parsed stackmaps match the invariants assumed by our runtime stack scanner.
///
/// This is a **deterministic** structural check intended for tests and CI, guarding against LLVM
/// changing stackmap emission in ways that would break in-place root relocation.
pub fn validate_stackmaps(maps: &StackMaps) -> Result<(), ValidationError> {
  for raw in maps.raws() {
    if raw.version != STACKMAP_VERSION {
      return Err(ValidationError::UnsupportedVersion {
        found: raw.version,
        expected: STACKMAP_VERSION,
      });
    }
  }

  let allowed_base_regs = allowed_base_regs_for_target();
  let ptr_size = std::mem::size_of::<usize>() as u16;

  for (pc, callsite) in maps.iter() {
    let record = callsite.record;
    let patchpoint_id = record.patchpoint_id;
    let instruction_offset = record.instruction_offset;

    let filtered: Vec<&Location> = record
      .locations
      .iter()
      .filter(|loc| !matches!(loc, Location::Constant { .. } | Location::ConstIndex { .. }))
      .collect();

    if filtered.len() % 2 != 0 {
      return Err(ValidationError::OddLocationCount {
        pc,
        patchpoint_id,
        instruction_offset,
        count: filtered.len(),
      });
    }

    for (location_index, loc) in filtered.iter().enumerate() {
      // Start strict: only stack-addressable slots that can be updated in-place are supported by
      // the runtime scanner. (`Register` / `Direct` roots are currently rejected.)
      let (size, dwarf_reg, offset) = match **loc {
        Location::Indirect {
          size,
          dwarf_reg,
          offset,
        } => (size, dwarf_reg, offset),
        Location::Register { .. } => {
          return Err(ValidationError::UnsupportedLocationKind {
            pc,
            patchpoint_id,
            instruction_offset,
            location_index,
            kind: "Register",
          })
        }
        Location::Direct { .. } => {
          return Err(ValidationError::UnsupportedLocationKind {
            pc,
            patchpoint_id,
            instruction_offset,
            location_index,
            kind: "Direct",
          })
        }
        Location::Constant { .. } => {
          return Err(ValidationError::UnsupportedLocationKind {
            pc,
            patchpoint_id,
            instruction_offset,
            location_index,
            kind: "Constant",
          })
        }
        Location::ConstIndex { .. } => {
          return Err(ValidationError::UnsupportedLocationKind {
            pc,
            patchpoint_id,
            instruction_offset,
            location_index,
            kind: "ConstIndex",
          })
        }
      };

      if size != ptr_size {
        return Err(ValidationError::UnexpectedPointerSize {
          pc,
          patchpoint_id,
          instruction_offset,
          location_index,
          size,
          ptr_size,
        });
      }

      if !allowed_base_regs.contains(&dwarf_reg) {
        return Err(ValidationError::UnsupportedBaseReg {
          pc,
          patchpoint_id,
          instruction_offset,
          location_index,
          dwarf_reg,
          allowed: allowed_base_regs,
        });
      }

      let ptr_size_i32 = ptr_size as i32;
      if offset.rem_euclid(ptr_size_i32) != 0 {
        return Err(ValidationError::UnalignedOffset {
          pc,
          patchpoint_id,
          instruction_offset,
          location_index,
          offset,
          ptr_size: ptr_size_i32,
        });
      }
    }
  }

  Ok(())
}

fn allowed_base_regs_for_target() -> &'static [u16] {
  #[cfg(target_arch = "x86_64")]
  {
    &[X86_64_DWARF_REG_RSP, X86_64_DWARF_REG_RBP]
  }
  #[cfg(target_arch = "aarch64")]
  {
    &[AARCH64_DWARF_REG_SP, AARCH64_DWARF_REG_FP]
  }
  #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
  {
    &[]
  }
}
