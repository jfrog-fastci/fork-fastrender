//! StackMap-driven root scanning helpers.
//!
//! This module provides small helpers that:
//! - look up the [`crate::stackmaps::CallSite`] for a stopped thread's current PC, and
//! - enumerate the `(base, derived)` relocation pairs corresponding to LLVM `gc.relocate` uses.
//!
//! Moving GCs must not treat the derived pointer as an independent root. Instead, relocate the
//! base pointer and recompute the derived pointer relative to it (see
//! [`crate::reloc::relocate_derived_pair`] / [`crate::relocate_derived_pairs`]).
//!
//! The key observation (LLVM 18, empirically) is that GC pointers are typically described as
//! `Indirect [DWARF_REG + offset]` locations, where the base DWARF register is the caller-frame SP
//! at the statepoint return address. This means the address of the spill slot is simply
//! `reg_value + offset`.

use crate::stackmaps::{Location, StackMaps, StackSize};
#[cfg(target_arch = "x86_64")]
use crate::stackmaps::{X86_64_DWARF_REG_RBP, X86_64_DWARF_REG_RSP};
use crate::statepoints::StatepointRecord;
use stackmap_context::{ThreadContext, DWARF_REG_IP};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
  #[error("missing instruction pointer (DWARF reg {dwarf_reg}) in ThreadContext")]
  MissingInstructionPointer { dwarf_reg: u16 },

  #[error("missing DWARF register {dwarf_reg} while evaluating location {loc:?}")]
  MissingRegister { dwarf_reg: u16, loc: Location },

  #[error("stackmap register root uses forbidden DWARF register {dwarf_reg} ({kind}) in location {loc:?}")]
  ForbiddenRegisterRoot {
    dwarf_reg: u16,
    kind: &'static str,
    loc: Location,
  },

  #[error("stackmap register root uses unsupported DWARF register {dwarf_reg} in location {loc:?}")]
  UnsupportedRegisterRoot { dwarf_reg: u16, loc: Location },

  #[error("address overflow while computing {kind} location address: base=0x{base:x} offset={offset}")]
  AddressOverflow {
    kind: &'static str,
    base: u64,
    offset: i32,
  },

  #[error("failed to reconstruct RSP from FP for a non-top frame: fp=0x{fp:x} stack_size={stack_size}")]
  RspReconstructionFailed { fp: u64, stack_size: u64 },

  #[error("stackmap function stack_size is unknown, cannot evaluate RSP-based location {loc:?} in a non-top frame")]
  UnknownStackSizeForRspBasedLocation { loc: Location },

  #[error("unsupported relocation location (expected Indirect spill slot), got {loc:?}")]
  UnsupportedLocation { loc: Location },

  // -----------------------------------------------------------------------------
  // Thread stack scanning (stop-the-world GC root enumeration)
  // -----------------------------------------------------------------------------

  #[error("missing published safepoint context for thread_id={thread_id} (os_tid={os_tid})")]
  MissingSafepointContext { thread_id: u64, os_tid: u64 },

  #[error("missing stack bounds for thread_id={thread_id} (os_tid={os_tid})")]
  MissingStackBounds { thread_id: u64, os_tid: u64 },

  #[error(
    "invalid stack bounds for thread_id={thread_id} (os_tid={os_tid}): lo=0x{lo:x} hi=0x{hi:x}"
  )]
  InvalidStackBounds {
    thread_id: u64,
    os_tid: u64,
    lo: usize,
    hi: usize,
  },

  #[error(
    "failed to scan stack roots for thread_id={thread_id} (os_tid={os_tid}) at fp=0x{fp:x} ip=0x{ip:x}"
  )]
  StackWalkFailed {
    thread_id: u64,
    os_tid: u64,
    fp: usize,
    ip: usize,
    #[source]
    source: crate::WalkError,
  },
}

/// Scan stack roots for a stopped thread.
///
/// This is intended for stop-the-world GC root enumeration: the caller must ensure the thread is
/// parked at a safepoint and its stack is stable for the duration of the scan.
pub fn scan_thread_roots(
  thread: &crate::threading::ThreadState,
  stackmaps: &StackMaps,
  visit: &mut dyn FnMut(*mut *mut u8),
) -> Result<(), ScanError> {
  let thread_id = thread.id().get();
  let os_tid = thread.os_thread_id();

  let ctx = thread
    .safepoint_context()
    .ok_or(ScanError::MissingSafepointContext { thread_id, os_tid })?;

  let bounds = thread
    .stack_bounds()
    .ok_or(ScanError::MissingStackBounds { thread_id, os_tid })?;

  let bounds = crate::stackwalk::StackBounds::new(bounds.lo as u64, bounds.hi as u64).map_err(|_| {
    ScanError::InvalidStackBounds {
      thread_id,
      os_tid,
      lo: bounds.lo,
      hi: bounds.hi,
    }
  })?;

  // Safety: caller guarantees the thread is stopped and its stack is not concurrently modified.
  unsafe {
    crate::stackwalk_fp::walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), stackmaps, |slot_addr| {
      visit(slot_addr as *mut *mut u8);
    })
    .map_err(|source| ScanError::StackWalkFailed {
      thread_id,
      os_tid,
      fp: ctx.fp,
      ip: ctx.ip,
      source,
    })?;
  }

  Ok(())
}

/// A visitor for GC root slots discovered while scanning native frames.
pub trait RootVisitor {
  /// Visit a plain GC root slot.
  ///
  /// `slot` points to a word containing either `0` (null) or a pointer to the start of a
  /// GC-managed object.
  fn visit_root(&mut self, slot: *mut usize);

  /// Visit an LLVM `gc.relocate` (base, derived) pair.
  ///
  /// `base_slot` contains the base object pointer and must be treated as the GC root.
  /// `derived_slot` is an interior pointer derived from that base.
  ///
  /// # Important
  ///
  /// Moving GCs must not treat the derived slot as an independent root. Instead, relocate the base
  /// pointer and recompute the derived pointer relative to it (see
  /// [`crate::reloc::relocate_derived_pair`]).
  ///
  /// Note: LLVM stackmaps may reuse the same `base_slot` across multiple pairs; relocating pairs
  /// one-by-one is therefore incorrect. Collect all pairs first and relocate them in a batch using
  /// [`crate::relocate_derived_pairs`].
  fn visit_derived_pair(&mut self, base_slot: *mut usize, derived_slot: *mut usize);
}

/// Like [`scan_reloc_pairs`], but dispatches to a [`RootVisitor`].
///
/// This treats relocation pairs that alias the same spill slot (`base_slot == derived_slot`) as a
/// plain root (only one slot to update). All other pairs are reported via
/// [`RootVisitor::visit_derived_pair`], including cases where `base == derived` but the two values
/// are stored in distinct slots (LLVM may emit duplicates).
pub fn scan_roots(
  thread_ctx: &mut ThreadContext,
  stackmaps: &StackMaps,
  visitor: &mut impl RootVisitor,
) -> Result<(), ScanError> {
  for (base_slot, derived_slot) in scan_reloc_pairs(thread_ctx, stackmaps)? {
    if base_slot == derived_slot {
      visitor.visit_root(base_slot);
    } else {
      visitor.visit_derived_pair(base_slot, derived_slot);
    }
  }
  Ok(())
}

/// Evaluate an `Indirect [base_reg + offset]` location for a single x86_64 frame.
///
/// This helper exists for stack walking when we only have a frame pointer (RBP)
/// for older frames:
/// - FP-based locations (`[RBP + off]`) can always be evaluated.
/// - SP-based locations (`[RSP + off]`) require either:
///   - the *captured* top-frame register context (`top_regs.rsp`), or
///   - a known fixed frame size (`stack_size`) so we can reconstruct callsite RSP as
///     `rsp = fp + 8 - stack_size` (LLVM 18 convention).
///
/// Note: this `stack_size`-based reconstruction does **not** account for per-callsite SP
/// adjustments (e.g. outgoing stack arguments). It is only valid when the callsite SP matches the
/// function's fixed frame size. Runtime stack walking should prefer deriving callsite SP from the
/// callee frame pointer (`callee_fp + 16`) when walking a full FP chain (see `stackwalk_fp`).
#[cfg(target_arch = "x86_64")]
pub fn slot_addr_x86_64(
  fp: u64,
  stack_size: StackSize,
  is_top_frame: bool,
  top_regs: Option<&ThreadContext>,
  loc: &Location,
) -> Result<*mut usize, ScanError> {
  let Location::Indirect {
    dwarf_reg, offset, ..
  } = *loc
  else {
    return Err(ScanError::UnsupportedLocation { loc: loc.clone() });
  };

  let base = match dwarf_reg {
    X86_64_DWARF_REG_RBP => fp,
    X86_64_DWARF_REG_RSP => {
      if is_top_frame {
        let regs = top_regs.ok_or(ScanError::MissingRegister {
          dwarf_reg,
          loc: loc.clone(),
        })?;
        regs.get_dwarf_reg_u64(dwarf_reg).ok_or(ScanError::MissingRegister {
          dwarf_reg,
          loc: loc.clone(),
        })?
      } else {
        let StackSize::Known(stack_size) = stack_size else {
          return Err(ScanError::UnknownStackSizeForRspBasedLocation { loc: loc.clone() });
        };
        fp.checked_add(8)
          .and_then(|v| v.checked_sub(stack_size))
          .ok_or(ScanError::RspReconstructionFailed { fp, stack_size })?
      }
    }
    other => {
      return Err(ScanError::MissingRegister {
        dwarf_reg: other,
        loc: loc.clone(),
      });
    }
  };

  let addr = add_i32(base, offset).ok_or(ScanError::AddressOverflow {
    kind: "Indirect",
    base,
    offset,
  })?;
  Ok(addr as usize as *mut usize)
}

#[cfg(not(target_arch = "x86_64"))]
pub fn slot_addr_x86_64(
  _fp: u64,
  _stack_size: StackSize,
  _is_top_frame: bool,
  _top_regs: Option<&ThreadContext>,
  loc: &Location,
) -> Result<*mut usize, ScanError> {
  Err(ScanError::UnsupportedLocation { loc: loc.clone() })
}

/// Enumerate `(base_slot, derived_slot)` relocation pairs at the current callsite.
///
/// - `thread_ctx` provides the stopped thread's DWARF register values.
/// - `stackmaps` is the parsed `.llvm_stackmaps` index for the current process/module.
///
/// Returns the address of each *pointer-sized spill slot* as a `(base_slot, derived_slot)` tuple.
///
/// Notes:
/// - This helper supports:
///   - `Indirect` spill slots (the common LLVM 18 output after `rewrite-statepoints-for-gc`), and
///   - `Register` roots (by returning lvalue pointers into `thread_ctx`).
/// - `Register` locations are supported by returning a pointer into `thread_ctx`'s saved register
///   file. This allows a moving GC to rewrite register-located roots by mutating `thread_ctx`
///   in-place while the thread is stopped.
/// - `Direct` locations are immediate values (reg + offset) and are not addressable, so they are
///   treated as unsupported and return an error.
pub fn scan_reloc_pairs(
  thread_ctx: &mut ThreadContext,
  stackmaps: &StackMaps,
) -> Result<Vec<(*mut usize, *mut usize)>, ScanError> {
  let ip = thread_ctx
    .get_dwarf_reg_u64(DWARF_REG_IP)
    .ok_or(ScanError::MissingInstructionPointer {
      dwarf_reg: DWARF_REG_IP,
    })?;

  let Some(callsite) = stackmaps.lookup(ip) else {
    // No stackmap record for this PC.
    return Ok(Vec::new());
  };

  // Note: `patchpoint_id` is not a reliable marker for LLVM `gc.statepoint` records (it can be
  // overridden via the `"statepoint-id"` callsite attribute and is not globally unique). Detect
  // statepoints by decoding the StackMap record layout.
  //
  // If decode fails, treat it as a non-statepoint record and return an empty result.
  if !crate::statepoints::looks_like_statepoint_record(callsite.record) {
    return Ok(Vec::new());
  }
  let statepoint = match StatepointRecord::new(callsite.record) {
    Ok(sp) => sp,
    Err(_) => return Ok(Vec::new()),
  };

  let mut pairs: Vec<(*mut usize, *mut usize)> = Vec::with_capacity(statepoint.gc_pair_count());
  for pair in statepoint.gc_pairs() {
    let base_slot = slot_addr(thread_ctx, &pair.base)?;
    let derived_slot = slot_addr(thread_ctx, &pair.derived)?;
    pairs.push((base_slot, derived_slot));
  }

  Ok(pairs)
}

fn slot_addr(ctx: &mut ThreadContext, loc: &Location) -> Result<*mut usize, ScanError> {
  match *loc {
    Location::Indirect {
      dwarf_reg, offset, ..
    } => {
      let base = ctx.get_dwarf_reg_u64(dwarf_reg).ok_or(ScanError::MissingRegister {
        dwarf_reg,
        loc: loc.clone(),
      })?;
      let addr = add_i32(base, offset).ok_or(ScanError::AddressOverflow {
        kind: "Indirect",
        base,
        offset,
      })?;
      Ok(addr as usize as *mut usize)
    }

    Location::Register { dwarf_reg, .. } => {
      if let Some(kind) = crate::arch::regs::forbidden_gc_root_reg(dwarf_reg) {
        return Err(ScanError::ForbiddenRegisterRoot {
          dwarf_reg,
          kind,
          loc: loc.clone(),
        });
      }
      let Some(slot) = (unsafe { crate::arch::regs::reg_slot_ptr(ctx as *mut ThreadContext, dwarf_reg) }) else {
        return Err(ScanError::UnsupportedRegisterRoot {
          dwarf_reg,
          loc: loc.clone(),
        });
      };
      Ok(slot)
    }

    // `Direct` locations in LLVM stackmaps mean "value is (reg + offset)" (no memory indirection).
    //
    // `scan_reloc_pairs` currently only supports addressable stack slots (i.e. `Indirect` spill
    // slots) and register-located roots (treated as lvalues inside `thread_ctx`). `Direct` values
    // are not addressable, so they are rejected.
    Location::Direct { .. } | Location::Constant { .. } | Location::ConstIndex { .. } => {
      Err(ScanError::UnsupportedLocation { loc: loc.clone() })
    }
  }
}

fn add_i32(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}

/// Compute the caller-frame stack pointer value used as the base for LLVM stackmap
/// `Indirect [SP + off]` locations on AArch64.
///
/// Empirically on LLVM 18 with frame pointers enabled (`-frame-pointer=all`), a typical prologue:
/// - saves `(FP, LR)` as a 16-byte pair and sets `FP` to point at that pair, and
/// - reports a fixed `stack_size` equal to the full frame size (including that 16-byte record).
///
/// When the callsite `SP` matches the function's fixed frame size (i.e. no outgoing-argument pushes
/// or other per-callsite adjustments), this lets us derive the stackmap `SP` base from `FP` and
/// `stack_size`:
///
/// ```text
/// sp_callsite = fp + 16 - stack_size
/// ```
///
/// Note: this `stack_size`-based reconstruction is not reliable for arbitrary callsites. Frame-
/// pointer stack walking in `runtime-native` uses `caller_sp_callsite = callee_fp + 16` instead.
pub fn compute_sp_aarch64(fp: usize, stack_size: u64) -> usize {
  let stack_size: usize = stack_size
    .try_into()
    .expect("stack_size does not fit in usize");
  let fp_plus_record = fp.checked_add(16).expect("fp overflow");
  fp_plus_record
    .checked_sub(stack_size)
    .expect("stack_size underflow while computing AArch64 SP from FP")
}
