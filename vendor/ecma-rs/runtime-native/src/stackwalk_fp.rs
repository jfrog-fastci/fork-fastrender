use crate::stackmaps::{CallSite, Location, StackMaps};
use crate::stackwalk::StackBounds;

#[cfg(feature = "conservative_roots")]
use crate::roots::{conservative_scan_words, HeapRange};
#[cfg(feature = "conservative_roots")]
use crate::gc::YOUNG_SPACE;
#[cfg(feature = "conservative_roots")]
use std::sync::atomic::Ordering;

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

  // With a standard prologue (`push rbp; mov rbp, rsp`), the SysV ABI guarantees
  // the frame pointer is 16-byte aligned.
  pub const FP_ALIGN: u64 = 16;
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
  #[error("frame pointer {fp:#x} is outside stack bounds [{lo:#x}, {hi:#x})")]
  FramePointerOutOfBounds { fp: u64, lo: u64, hi: u64 },
  #[error("return address is null in frame record at fp={fp:#x}")]
  ReturnAddressIsNull { fp: u64 },
  #[error("return address {return_addr:#x} in frame record at fp={fp:#x} is not canonical")]
  ReturnAddressNonCanonical { fp: u64, return_addr: u64 },
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
  #[error("caller stack pointer {caller_sp:#x} is outside stack bounds [{lo:#x}, {hi:#x})")]
  StackPointerOutOfBounds { caller_sp: u64, lo: u64, hi: u64 },
  #[error(
    "GC root slot address {slot_addr:#x} is not aligned to {alignment} bytes at return address {return_addr:#x}"
  )]
  MisalignedRootSlot {
    return_addr: u64,
    slot_addr: u64,
    alignment: u64,
  },
  #[error(
    "GC root slot address {slot_addr:#x} is outside stack bounds [{lo:#x}, {hi:#x}) at return address {return_addr:#x}"
  )]
  RootSlotOutOfBounds {
    return_addr: u64,
    slot_addr: u64,
    lo: u64,
    hi: u64,
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

  #[error(
    "derived pointer delta overflow at return address {return_addr:#x} (base=0x{base_val:x} derived=0x{derived_val:x})"
  )]
  DerivedPointerDeltaOverflow {
    return_addr: u64,
    base_val: u64,
    derived_val: u64,
  },

  #[error(
    "derived pointer relocation overflow at return address {return_addr:#x} (relocated_base=0x{base_val:x} delta={delta})"
  )]
  DerivedPointerRelocationOverflow {
    return_addr: u64,
    base_val: u64,
    delta: i64,
  },
  #[error("missing stackmap entry for return address {return_addr:#x}")]
  MissingStackMap { return_addr: u64 },
}

const MAX_FRAMES: usize = 100_000;
#[cfg(feature = "conservative_roots")]
const MAX_CONSERVATIVE_SCAN_WORDS: usize = 4096;

#[cfg(feature = "conservative_roots")]
fn heap_range_for_conservative_roots() -> HeapRange {
  let start = YOUNG_SPACE.start.load(Ordering::Acquire) as *const u8;
  let end = YOUNG_SPACE.end.load(Ordering::Acquire) as *const u8;
  HeapRange::with_object_start_check(start, end, is_probably_young_object_start)
}

#[cfg(feature = "conservative_roots")]
fn is_probably_young_object_start(candidate: *const u8) -> bool {
  use crate::gc::ObjHeader;

  // Conservative scanning is a fallback: be strict about only reporting slots
  // that *look like* real object headers so a moving GC doesn't corrupt memory
  // by evacuating a false-positive "pointer" into the nursery.
  if (candidate as usize) % core::mem::align_of::<ObjHeader>() != 0 {
    return false;
  }

  // Safety: `candidate` is within the active young-space range, which is a
  // mapped RW region. Reading a header-sized prefix is safe; validation is
  // performed via the type-descriptor registry.
  let header = unsafe { &*(candidate as *const ObjHeader) };
  crate::gc::is_known_type_descriptor(header.type_desc)
}

#[cfg(feature = "conservative_roots")]
fn conservative_scan_frame_words(
  start_addr: u64,
  end_addr: u64,
  visit: &mut impl FnMut(*mut u8),
) {
  let start = start_addr as usize;
  let end = end_addr as usize;
  if end <= start {
    return;
  }

  let max_bytes = MAX_CONSERVATIVE_SCAN_WORDS * core::mem::size_of::<usize>();
  let bounded_end = start.saturating_add(max_bytes).min(end);
  let range = (start as *const usize)..(bounded_end as *const usize);
  let heap = heap_range_for_conservative_roots();
  conservative_scan_words(range, heap, |slot| visit(slot as *mut u8));
}

/// Walk the frame-pointer chain and enumerate GC root slots using LLVM stackmaps.
///
/// ## Assumptions / requirements
///
/// - Managed code **must** be compiled with frame pointers enabled.
///   - LLVM: `-frame-pointer=all`
///   - Rust: `-C force-frame-pointers=yes`
/// - Tail calls must be disabled for managed code (frame-pointer walking assumes
///   a complete call chain).
///   - LLVM: `disable-tail-calls="true"`
/// - The stack grows downwards and FP values increase as we walk toward older
///   callers (Linux x86_64/AArch64).
/// - In practice, LLVM statepoints *often* spill GC roots into addressable stack
///   slots (`Indirect [SP + off]`). However, the stackmap format also supports
///   register locations (`Register R#N`), which require rewriting the stopped
///   thread's register file when resuming (see `statepoints::RootSlot` and the
///   `stackmap-context` crate).
/// - Derived pointers (statepoint `(base, derived)` pairs) are supported. The
///   walker visits only the **base** root slots (for tracing) and then updates
///   each derived slot in-place after the base slot has potentially been
///   relocated:
///
///   `derived_new = relocated_base + (derived_old - base_old)`
///
/// ## Statepoint-oriented walking
///
/// This walker is statepoint-oriented: the return address stored in the current
/// frame identifies the *caller's* safepoint callsite. Therefore we use
/// `(caller_fp, caller_ra)` extracted from the current frame, and enumerate
/// roots in the *caller* frame.
///
/// For patchable statepoints (`gc.statepoint` with `patch_bytes > 0`), LLVM 18
/// records the return address as the byte *after the reserved patchable region*.
/// Any runtime patcher must ensure the actual call return address matches that
/// end-of-region address (so the stackmap lookup key matches).
pub unsafe fn walk_gc_roots_from_fp(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  mut visit: impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  let mut cur_fp = start_fp;
  for depth in 0..MAX_FRAMES {
    check_fp_alignment(cur_fp)?;
    if let Some(bounds) = bounds {
      check_fp_bounds(cur_fp, bounds)?;
    }

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

    if let Some(bounds) = bounds {
      check_fp_bounds(caller_fp, bounds)?;
    }

    if caller_ra == 0 {
      return Err(WalkError::ReturnAddressIsNull { fp: cur_fp });
    }

    if !is_canonical_pc(caller_ra) {
      return Err(WalkError::ReturnAddressNonCanonical {
        fp: cur_fp,
        return_addr: caller_ra,
      });
    }

    match stackmaps.lookup(caller_ra) {
      Some(callsite) => {
        enumerate_roots_for_frame(caller_fp, caller_ra, callsite, bounds, &mut visit)?;
      }
      None => {
        #[cfg(feature = "conservative_roots")]
        {
          conservative_scan_frame_words(cur_fp, caller_fp, &mut visit);
        }

        #[cfg(not(feature = "conservative_roots"))]
        {
          return Err(WalkError::MissingStackMap { return_addr: caller_ra });
        }
      }
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

/// Walk GC root slots for a thread parked in a stop-the-world safepoint.
///
/// This is the entry point used by the STW GC: after [`crate::rt_gc_wait_for_world_stopped`]
/// returns, the GC can read each thread's published [`crate::arch::SafepointContext`] (captured
/// when the mutator entered the safepoint slow path) and call this function to enumerate precise
/// stack roots for that parked thread.
///
/// The callback is invoked with the address of each *stack slot* that contains a managed pointer.
/// A relocating GC should treat the slot as `*mut *mut u8` and may update it in-place.
///
/// ## Statepoint-oriented walking
///
/// Unlike [`walk_gc_roots_from_fp`] (which expects to start from a runtime frame and uses the
/// current frame's saved return address to identify the managed caller), this function starts
/// directly from the captured call-site state:
///
/// - `ctx.fp` is treated as the managed caller's frame pointer (`caller_fp`).
/// - `ctx.ip` is treated as the managed caller's return address at the safepoint call site
///   (`caller_ra`).
///
/// Roots for that top managed frame are enumerated first (if a matching stackmap record exists),
/// then older frames are walked by delegating to [`walk_gc_roots_from_fp`] starting at `ctx.fp`.
/// This avoids double-enumerating the top frame.
///
/// # Safety
///
/// The caller must ensure the target thread is stopped and its stack is not concurrently modified
/// while walking. The supplied `ctx` must have been captured for a frame compiled with frame
/// pointers enabled, and `stackmaps` must correspond to the code being walked.
pub unsafe fn walk_gc_roots_from_safepoint_context(
  ctx: &crate::arch::SafepointContext,
  bounds: Option<StackBounds>,
  stackmaps: &crate::StackMaps,
  mut visit: impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  let caller_fp = ctx.fp as u64;
  if caller_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  check_fp_alignment(caller_fp)?;
  if let Some(bounds) = bounds {
    check_fp_bounds(caller_fp, bounds)?;
  }

  let caller_ra = ctx.ip as u64;
  if caller_ra == 0 {
    return Err(WalkError::ReturnAddressIsNull { fp: caller_fp });
  }
  if !is_canonical_pc(caller_ra) {
    return Err(WalkError::ReturnAddressNonCanonical {
      fp: caller_fp,
      return_addr: caller_ra,
    });
  }
  match stackmaps.lookup(caller_ra) {
    Some(callsite) => {
      enumerate_roots_for_frame(caller_fp, caller_ra, callsite, bounds, &mut visit)?;
    }
    None => {
      #[cfg(feature = "conservative_roots")]
      {
        conservative_scan_frame_words(ctx.sp_before_call as u64, caller_fp, &mut visit);
      }

      #[cfg(not(feature = "conservative_roots"))]
      {
        return Err(WalkError::MissingStackMap { return_addr: caller_ra });
      }
    }
  }

  // Continue walking older frames. Starting from the managed frame pointer means the delegated
  // walker will enumerate roots in the *caller* frame, i.e. it won't double-enumerate the top
  // managed frame we just handled above.
  walk_gc_roots_from_fp(caller_fp, bounds, stackmaps, visit)
}

#[inline]
fn check_fp_bounds(fp: u64, bounds: StackBounds) -> Result<(), WalkError> {
  // We must be able to read:
  //   [fp + FP_LINK_OFFSET] => caller fp
  //   [fp + RA_OFFSET]      => caller return address / LR
  let record_size = arch::RA_OFFSET + arch::WORD;
  if !bounds.contains_range(fp, record_size) {
    return Err(WalkError::FramePointerOutOfBounds {
      fp,
      lo: bounds.lo,
      hi: bounds.hi,
    });
  }
  Ok(())
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn is_canonical_pc(pc: u64) -> bool {
  // Canonical addresses are sign-extended from bit 47 (SysV x86_64).
  let sign = (pc >> 47) & 1;
  let top = pc >> 48;
  if sign == 0 {
    top == 0
  } else {
    top == 0xffff
  }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn is_canonical_pc(_pc: u64) -> bool {
  true
}

fn enumerate_roots_for_frame(
  caller_fp: u64,
  caller_ra: u64,
  callsite: CallSite<'_>,
  bounds: Option<StackBounds>,
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
  let caller_sp_checked = caller_fp
    .checked_sub(locals_size)
    .ok_or(WalkError::StackPointerUnderflow {
      caller_fp,
      stack_size,
      fp_record_size: arch::FP_RECORD_SIZE,
    })?;

  // LLVM StackMaps v3 (LLVM 18) frequently use DWARF RSP (R#7) as the base register even when
  // frame pointers are enabled (`-frame-pointer=all`). In that case, the reported `stack_size`
  // includes the pushed old RBP but *not* the return address pushed by `call`.
  //
  // For a canonical prologue:
  //   push rbp
  //   mov  rbp, rsp
  //   sub  rsp, locals
  //
  // LLVM reports:
  //   stack_size = locals + 8  (includes `push rbp`)
  //
  // So the caller's stack pointer value at the callsite return address is:
  //   rsp_at_callsite = rbp - locals = rbp + 8 - stack_size
  #[cfg(target_arch = "x86_64")]
  let caller_sp = {
    let sp = compute_rsp_x86_64(caller_fp as usize, stack_size) as u64;
    debug_assert_eq!(sp, caller_sp_checked);
    sp
  };
  #[cfg(target_arch = "aarch64")]
  let caller_sp = caller_sp_checked;

  if let Some(bounds) = bounds {
    if caller_sp < bounds.lo || caller_sp > bounds.hi {
      return Err(WalkError::StackPointerOutOfBounds {
        caller_sp,
        lo: bounds.lo,
        hi: bounds.hi,
      });
    }
  }

  let statepoint = crate::statepoints::StatepointRecord::new(callsite.record).map_err(|source| {
    WalkError::InvalidStatepoint {
      return_addr: caller_ra,
      source,
    }
  })?;

  // Collect + dedup within this frame to avoid double-visiting the same slot
  // (LLVM can emit duplicated locations for relocated values).
  //
  // For each `(base, derived)` pair, we always visit the base root slot. If the
  // derived value is stored in a distinct slot, we compute its byte delta from
  // the base value and update it after the base has been relocated by the
  // callback.
  let mut base_slots: Vec<u64> = Vec::with_capacity(statepoint.gc_pair_count());
  let mut derived_fixups: Vec<(u64, u64, i64)> = Vec::new(); // (base_slot, derived_slot, delta)
  for pair in statepoint.gc_pairs() {
    let base = &pair.base;
    let derived = &pair.derived;
    let base_slot = eval_root_location(caller_fp, caller_sp, caller_ra, base)?;
    let derived_slot = eval_root_location(caller_fp, caller_sp, caller_ra, derived)?;
    validate_root_slot(base_slot, bounds, caller_ra)?;
    validate_root_slot(derived_slot, bounds, caller_ra)?;
    base_slots.push(base_slot);

    if base_slot != derived_slot {
      let base_val = unsafe { read_u64(base_slot) };
      let derived_val = unsafe { read_u64(derived_slot) };
      let delta_i128 = (derived_val as i128) - (base_val as i128);
      let delta = i64::try_from(delta_i128).map_err(|_| WalkError::DerivedPointerDeltaOverflow {
        return_addr: caller_ra,
        base_val,
        derived_val,
      })?;
      derived_fixups.push((base_slot, derived_slot, delta));
    }
  }
  base_slots.sort_unstable();
  base_slots.dedup();

  // Deterministic ordering + avoid double-updating the same derived slot.
  derived_fixups.sort_unstable_by_key(|(base, derived, _)| (*base, *derived));
  derived_fixups.dedup_by_key(|(base, derived, _)| (*base, *derived));

  let mut fixup_idx = 0usize;
  for base_slot in base_slots {
    visit(base_slot as *mut u8);

    // Apply any derived fixups that use this base slot.
    while fixup_idx < derived_fixups.len() && derived_fixups[fixup_idx].0 < base_slot {
      fixup_idx += 1;
    }
    if fixup_idx >= derived_fixups.len() || derived_fixups[fixup_idx].0 != base_slot {
      continue;
    }

    let relocated_base_val = unsafe { read_u64(base_slot) };
    while fixup_idx < derived_fixups.len() && derived_fixups[fixup_idx].0 == base_slot {
      let (_base_slot, derived_slot, delta) = derived_fixups[fixup_idx];
      let new_derived =
        add_signed_u64_i64(relocated_base_val, delta).ok_or(WalkError::DerivedPointerRelocationOverflow {
          return_addr: caller_ra,
          base_val: relocated_base_val,
          delta,
        })?;
      unsafe { write_u64(derived_slot, new_derived) };
      fixup_idx += 1;
    }
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
unsafe fn write_u64(addr: u64, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

fn add_signed_u64_i64(base: u64, offset: i64) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    let abs = offset.checked_abs()? as u64;
    base.checked_sub(abs)
  }
}

#[inline]
fn validate_root_slot(slot_addr: u64, bounds: Option<StackBounds>, return_addr: u64) -> Result<(), WalkError> {
  if slot_addr % arch::WORD != 0 {
    return Err(WalkError::MisalignedRootSlot {
      return_addr,
      slot_addr,
      alignment: arch::WORD,
    });
  }

  if let Some(bounds) = bounds {
    if !bounds.contains_range(slot_addr, arch::WORD) {
      return Err(WalkError::RootSlotOutOfBounds {
        return_addr,
        slot_addr,
        lo: bounds.lo,
        hi: bounds.hi,
      });
    }
  }

  Ok(())
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

#[cfg(target_arch = "x86_64")]
fn compute_rsp_x86_64(fp: usize, stack_size: u64) -> usize {
  // See the derivation in `enumerate_roots_for_frame`: with frame pointers enabled, LLVM's
  // `stack_size` includes the pushed old RBP, so the caller-frame RSP at the statepoint callsite
  // return address is `RBP + 8 - stack_size`.
  fp + (arch::WORD as usize) - (stack_size as usize)
}
