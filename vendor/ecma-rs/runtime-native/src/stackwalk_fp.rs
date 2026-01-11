use crate::stackmaps::{CallSite, Location, StackMaps};

#[cfg(target_arch = "x86_64")]
mod arch {
  pub const WORD: u64 = 8;

  pub const FP_LINK_OFFSET: u64 = 0;
  pub const RA_OFFSET: u64 = WORD;

  /// Number of bytes in the architecture's "frame record" stored in the
  /// function's stack frame at entry.
  ///
  /// - x86_64: saved RBP only (return address is pushed by the CALL instruction,
  ///   outside the callee's stack size)
  pub const FP_RECORD_SIZE: u64 = 8;

  /// DWARF register number for the stack pointer (RSP).
  pub const DWARF_SP: u16 = 7;
  /// DWARF register number for the frame pointer (RBP).
  pub const DWARF_FP: u16 = 6;

  pub const FP_ALIGN: u64 = 8;
}

#[cfg(target_arch = "aarch64")]
mod arch {
  pub const WORD: u64 = 8;

  pub const FP_LINK_OFFSET: u64 = 0;
  pub const RA_OFFSET: u64 = WORD;

  /// - AArch64: saved X29 + X30 (FP + LR)
  pub const FP_RECORD_SIZE: u64 = 16;

  /// DWARF register number for the stack pointer (SP).
  pub const DWARF_SP: u16 = 31;
  /// DWARF register number for the frame pointer (X29).
  pub const DWARF_FP: u16 = 29;

  // AArch64 mandates 16-byte SP alignment at all public ABI boundaries, and LLVM
  // maintains this for FP too when frame pointers are enabled.
  pub const FP_ALIGN: u64 = 16;
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("runtime-native stack walking currently supports only x86_64 and aarch64");

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
  #[error("start frame pointer is null")]
  NullStartFp,
  #[error("frame pointer chain exceeded max depth ({max_depth})")]
  MaxDepth { max_depth: usize },
  #[error("frame pointer {fp:#x} is not aligned to {alignment} bytes")]
  MisalignedFramePointer { fp: u64, alignment: u64 },
  #[error("frame pointer chain is not monotonically increasing: cur_fp={cur_fp:#x} caller_fp={caller_fp:#x}")]
  NonMonotonicFp { cur_fp: u64, caller_fp: u64 },
  #[error(
    "stackmap function record for return address {return_addr:#x} reports stack_size={stack_size}, smaller than FP_RECORD_SIZE={fp_record_size}"
  )]
  InvalidStackSize {
    return_addr: u64,
    stack_size: u64,
    fp_record_size: u64,
  },
  #[error(
    "stack pointer underflow while computing caller SP: caller_fp={caller_fp:#x} stack_size={stack_size} fp_record_size={fp_record_size}"
  )]
  StackPointerUnderflow {
    caller_fp: u64,
    stack_size: u64,
    fp_record_size: u64,
  },
  #[error("failed to decode statepoint record at return address {return_addr:#x}")]
  InvalidStatepoint {
    return_addr: u64,
    #[source]
    source: crate::statepoints::StatepointError,
  },
  #[error("unsupported GC root location {loc:?} at return address {return_addr:#x}")]
  UnsupportedGcLocation { return_addr: u64, loc: Location },
  #[error(
    "unsupported stackmap root base register dwarf_reg={dwarf_reg} at return address {return_addr:#x}"
  )]
  UnsupportedBaseRegister { return_addr: u64, dwarf_reg: u16 },
  #[error("stackmap root address overflow: base={base:#x} offset={offset}")]
  RootAddressOverflow { base: u64, offset: i32 },
}

const MAX_FRAMES: usize = 100_000;

/// Walk the frame-pointer chain and enumerate GC root slots using LLVM stackmaps.
///
/// ## Assumptions / requirements
///
/// - Managed code **must** be compiled with frame pointers enabled.
///   - LLVM: `-frame-pointer=all`
///   - Rust: `-C force-frame-pointers=yes`
/// - The stack grows downwards and FP values increase as we walk toward older
///   callers (Linux x86_64/AArch64).
/// - GC roots are described by LLVM stackmaps statepoints as spilled stack slots
///   (i.e. `Indirect [SP + off]`).
///
/// ## Statepoint-oriented walking
///
/// This walker is statepoint-oriented: the return address stored in the current
/// frame identifies the *caller's* safepoint callsite. Therefore we use
/// `(caller_fp, caller_ra)` extracted from the current frame, and enumerate
/// roots in the *caller* frame.
pub unsafe fn walk_gc_roots_from_fp(
  start_fp: u64,
  stackmaps: &StackMaps,
  mut visit: impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  let mut cur_fp = start_fp;
  for depth in 0..MAX_FRAMES {
    check_fp_alignment(cur_fp)?;

    // Frame layout:
    // [FP + 0] = previous FP
    // [FP + 8] = return address into caller
    let caller_fp = read_u64(cur_fp + arch::FP_LINK_OFFSET);
    let caller_ra = read_u64(cur_fp + arch::RA_OFFSET);

    if caller_fp == 0 {
      return Ok(());
    }

    check_fp_alignment(caller_fp)?;
    if caller_fp <= cur_fp {
      return Err(WalkError::NonMonotonicFp { cur_fp, caller_fp });
    }

    if let Some(callsite) = stackmaps.lookup(caller_ra) {
      enumerate_roots_for_frame(caller_fp, caller_ra, callsite, &mut visit)?;
    }

    cur_fp = caller_fp;

    if depth + 1 == MAX_FRAMES {
      break;
    }
  }

  Err(WalkError::MaxDepth {
    max_depth: MAX_FRAMES,
  })
}

fn enumerate_roots_for_frame(
  caller_fp: u64,
  caller_ra: u64,
  callsite: CallSite<'_>,
  visit: &mut impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  let stack_size = callsite.stack_size;
  if stack_size < arch::FP_RECORD_SIZE {
    return Err(WalkError::InvalidStackSize {
      return_addr: caller_ra,
      stack_size,
      fp_record_size: arch::FP_RECORD_SIZE,
    });
  }

  let locals_size = stack_size - arch::FP_RECORD_SIZE;
  let caller_sp = caller_fp
    .checked_sub(locals_size)
    .ok_or(WalkError::StackPointerUnderflow {
      caller_fp,
      stack_size,
      fp_record_size: arch::FP_RECORD_SIZE,
    })?;

  let statepoint = crate::statepoints::StatepointRecord::new(callsite.record).map_err(|source| {
    WalkError::InvalidStatepoint {
      return_addr: caller_ra,
      source,
    }
  })?;

  // Collect + dedup within this frame to avoid double-visiting the same slot
  // (LLVM can emit duplicated locations for relocated values).
  let mut slots: Vec<u64> = Vec::with_capacity(statepoint.gc_pairs().len() * 2);
  for pair in statepoint.gc_pairs() {
    slots.push(eval_root_location(caller_fp, caller_sp, caller_ra, pair.base)?);
    slots.push(eval_root_location(caller_fp, caller_sp, caller_ra, pair.derived)?);
  }
  slots.sort_unstable();
  slots.dedup();

  for slot_addr in slots {
    visit(slot_addr as *mut u8);
  }

  Ok(())
}

fn eval_root_location(
  caller_fp: u64,
  caller_sp: u64,
  caller_ra: u64,
  loc: &Location,
) -> Result<u64, WalkError> {
  match *loc {
    Location::Indirect { dwarf_reg, offset, .. } => {
      let base = match dwarf_reg {
        arch::DWARF_SP => caller_sp,
        arch::DWARF_FP => caller_fp,
        other => {
          return Err(WalkError::UnsupportedBaseRegister {
            return_addr: caller_ra,
            dwarf_reg: other,
          });
        }
      };

      add_signed_u64(base, offset).ok_or(WalkError::RootAddressOverflow { base, offset })
    }

    // Statepoint GC roots should always be spilled, addressable slots. Treat
    // register roots or direct-address expressions as a hard error with context.
    _ => Err(WalkError::UnsupportedGcLocation {
      return_addr: caller_ra,
      loc: loc.clone(),
    }),
  }
}

fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}

#[inline]
unsafe fn read_u64(addr: u64) -> u64 {
  // FP slots are naturally aligned, but use unaligned reads so synthetic tests
  // don't have to care.
  (addr as *const u64).read_unaligned()
}

#[inline]
fn check_fp_alignment(fp: u64) -> Result<(), WalkError> {
  if fp % arch::FP_ALIGN != 0 {
    return Err(WalkError::MisalignedFramePointer {
      fp,
      alignment: arch::FP_ALIGN,
    });
  }
  Ok(())
}

