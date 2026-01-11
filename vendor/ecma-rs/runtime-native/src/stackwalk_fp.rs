use crate::gc_roots::RelocPair;
use crate::stackmaps::{CallSite, Location, StackMaps};
use crate::stackwalk::StackBounds;
use crate::statepoints::RootSlot;

use std::cell::{Cell, UnsafeCell};

#[cfg(any(debug_assertions, feature = "conservative_roots"))]
use crate::roots::{conservative_scan_words, HeapRange};
#[cfg(any(debug_assertions, feature = "conservative_roots"))]
use crate::gc::YOUNG_SPACE;
#[cfg(any(debug_assertions, feature = "conservative_roots"))]
use std::sync::atomic::Ordering;

#[cfg(target_arch = "x86_64")]
mod arch {
  pub const WORD: u64 = 8;

  pub const FP_LINK_OFFSET: u64 = 0;
  pub const RA_OFFSET: u64 = WORD;

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
  #[error("missing stack bounds for frame-pointer stack walk")]
  MissingStackBounds,
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
  #[error("stack pointer overflow while deriving callsite SP from callee FP: callee_fp={callee_fp:#x}")]
  CallerSpOverflow { callee_fp: u64 },
  #[error(
    "missing stackmap SP for SP-relative GC root locations at return address {return_addr:#x} (SafepointContext.sp and .sp_entry are both 0)"
  )]
  MissingStackmapSp { return_addr: u64 },
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
  #[error(
    "missing saved RegContext while evaluating register root dwarf_reg={dwarf_reg} at return address {return_addr:#x}"
  )]
  MissingRegContext { return_addr: u64, dwarf_reg: u16 },
  #[error(
    "register root uses forbidden DWARF reg {dwarf_reg} ({kind}) at return address {return_addr:#x}"
  )]
  ForbiddenRegisterRoot {
    return_addr: u64,
    dwarf_reg: u16,
    kind: &'static str,
  },
  #[error(
    "unsupported register root DWARF reg {dwarf_reg} at return address {return_addr:#x}"
  )]
  UnsupportedRegisterRoot { return_addr: u64, dwarf_reg: u16 },
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

  #[error(
    "statepoint record at return address {return_addr:#x} has gc_pair_count={gc_pair_count}, exceeding preallocated scratch capacity={scratch_capacity}"
  )]
  GcPairScratchCapacityExceeded {
    return_addr: u64,
    gc_pair_count: usize,
    scratch_capacity: usize,
  },
}

const MAX_FRAMES_CAP: usize = 1_000_000;
#[cfg(any(debug_assertions, feature = "conservative_roots"))]
const MAX_CONSERVATIVE_SCAN_WORDS: usize = 4096;

#[inline]
fn max_frames_for_bounds(bounds: StackBounds) -> usize {
  // On supported targets, the smallest possible "frame record" we will read is
  // two words (saved FP + return address / LR).
  let min_frame_bytes = (arch::RA_OFFSET + arch::WORD) as usize;
  debug_assert!(min_frame_bytes > 0);
  let stack_bytes = bounds.hi.saturating_sub(bounds.lo) as usize;
  let by_stack = stack_bytes / min_frame_bytes;
  // Clamp so corrupted FP chains can't loop forever even if stack bounds are
  // huge (e.g. custom thread stacks).
  by_stack.clamp(1, MAX_FRAMES_CAP)
}

#[derive(Clone, Copy, Debug)]
struct GcPairScratchEntry {
  base_slot: u64,
  derived_slot: u64,
  /// `(derived_old - base_old)` computed from pre-relocation values.
  delta: i64,
  /// Preserve `null` semantics: if either old pointer was `0`, force the derived slot to `0`
  /// after relocation.
  force_null: bool,
}

#[derive(Debug)]
struct StackwalkScratch {
  entries: Vec<GcPairScratchEntry>,
  root_pairs: Vec<(*mut usize, *mut usize)>,
}

impl StackwalkScratch {
  fn new() -> Self {
    Self {
      entries: Vec::new(),
      root_pairs: Vec::new(),
    }
  }

  #[inline]
  fn ensure_capacity(&mut self, max_gc_pairs_per_frame: usize) {
    if self.entries.capacity() < max_gc_pairs_per_frame {
      self
        .entries
        .reserve_exact(max_gc_pairs_per_frame.saturating_sub(self.entries.len()));
    }
    if self.root_pairs.capacity() < max_gc_pairs_per_frame {
      self
        .root_pairs
        .reserve_exact(max_gc_pairs_per_frame.saturating_sub(self.root_pairs.len()));
    }
  }
}

thread_local! {
  static TLS_STACKWALK_SCRATCH: UnsafeCell<StackwalkScratch> = UnsafeCell::new(StackwalkScratch::new());
  static TLS_STACKWALK_SCRATCH_IN_USE: Cell<bool> = const { Cell::new(false) };
}

fn with_stackwalk_scratch<R>(f: impl FnOnce(&mut StackwalkScratch) -> R) -> R {
  TLS_STACKWALK_SCRATCH_IN_USE.with(|in_use| {
    let already = in_use.replace(true);
    assert!(
      !already,
      "stackwalk scratch buffer is already borrowed (reentrant stack walking on the same thread)"
    );
    struct Reset<'a>(&'a Cell<bool>);
    impl Drop for Reset<'_> {
      fn drop(&mut self) {
        self.0.set(false);
      }
    }
    let _reset = Reset(in_use);

    TLS_STACKWALK_SCRATCH.with(|scratch| {
      // SAFETY: thread-local + `TLS_STACKWALK_SCRATCH_IN_USE` ensures exclusive access.
      let scratch = unsafe { &mut *scratch.get() };
      f(scratch)
    })
  })
}

/// Ensure the current thread's stack-walker scratch buffers can hold at least `max_gc_pairs_per_frame` entries.
///
/// This may allocate and should therefore be called outside stop-the-world GC. The runtime calls
/// this during thread registration so stack root scanning does not allocate while the world is
/// stopped.
#[doc(hidden)]
pub fn ensure_stackwalk_scratch_capacity(max_gc_pairs_per_frame: usize) {
  with_stackwalk_scratch(|scratch| scratch.ensure_capacity(max_gc_pairs_per_frame));
}

#[cfg(any(debug_assertions, feature = "conservative_roots"))]
fn heap_range_for_conservative_roots() -> HeapRange {
  let start = YOUNG_SPACE.start.load(Ordering::Acquire) as *const u8;
  let end = YOUNG_SPACE.end.load(Ordering::Acquire) as *const u8;
  HeapRange::with_object_start_check(start, end, is_probably_young_object_start)
}

#[cfg(any(debug_assertions, feature = "conservative_roots"))]
fn is_probably_young_object_start(candidate: *const u8) -> bool {
  use crate::gc::ObjHeader;

  // Conservative scanning is a fallback: be strict about only reporting slots
  // that *look like* real object headers so a moving GC doesn't corrupt memory
  // by evacuating a false-positive "pointer" into the nursery.
  if (candidate as usize) % crate::gc::OBJ_ALIGN != 0 {
    return false;
  }

  // Ensure we never dereference an ObjHeader that straddles the young-space end.
  let end = YOUNG_SPACE.end.load(Ordering::Acquire) as usize;
  let hdr_size = core::mem::size_of::<ObjHeader>();
  let last_valid = end.saturating_sub(hdr_size);
  if (candidate as usize) > last_valid {
    return false;
  }

  // Safety: `candidate` is within the active young-space range, which is a
  // mapped RW region. Reading a header-sized prefix is safe; validation is
  // performed via the type-descriptor registry.
  let header = unsafe { &*crate::gc::header_from_obj(candidate.cast_mut()) };
  crate::gc::is_known_type_descriptor(header.type_desc)
}

#[cfg(any(debug_assertions, feature = "conservative_roots"))]
fn conservative_scan_frame_words(
  start_addr: u64,
  end_addr: u64,
  bounds: StackBounds,
  visit: &mut impl FnMut(*mut u8),
) {
  let mut start = start_addr.max(bounds.lo);
  let mut end = end_addr.min(bounds.hi);

  if end <= start {
    return;
  }

  // Align to machine-word boundaries.
  let align = core::mem::align_of::<usize>() as u64;
  debug_assert!(align.is_power_of_two());
  let align_mask = align - 1;
  start = start.saturating_add(align_mask) & !align_mask;
  end &= !align_mask;

  if end <= start {
    return;
  }

  let max_bytes = MAX_CONSERVATIVE_SCAN_WORDS * core::mem::size_of::<usize>();
  let bounded_end = (start as usize).saturating_add(max_bytes).min(end as usize);
  let range = (start as usize as *const usize)..(bounded_end as *const usize);
  let heap = heap_range_for_conservative_roots();
  conservative_scan_words(range, heap, |slot| visit(slot as *mut u8));
}

/// Base/derived slot pair for a GC pointer reported by an LLVM `gc.statepoint`.
///
/// LLVM stackmaps record *two* locations for every live GC pointer at a safepoint:
/// the base pointer and the derived (possibly interior) pointer.
///
/// For non-interior pointers, LLVM typically emits duplicate locations where
/// `base == derived`.
#[derive(Clone, Copy, Debug)]
pub struct StatepointRootPair {
  pub base_slot: *mut usize,
  pub derived_slot: *mut usize,
}

/// Relocate a (base, derived) pointer pair after a moving GC.
///
/// `relocate_base` must return the new (relocated) address for `base_old` (or
/// `0` for `0`).
///
/// If both old pointers are non-null, the derived pointer is preserved relative
/// to the base pointer:
///
/// `derived_new = relocated_base + (derived_old - base_old)`
///
/// ## Warning
/// LLVM stackmaps may reuse the same `base_slot` across multiple pairs in a single frame when
/// multiple derived pointers share a base. Relocating pairs one-by-one can therefore be incorrect:
/// once `*base_slot` is overwritten, subsequent pairs will compute the delta against the relocated
/// base rather than the original base.
///
/// Prefer [`crate::relocate_derived_pairs`] on a per-frame batch of pairs.
pub unsafe fn relocate_pair(pair: StatepointRootPair, relocate_base: impl FnOnce(usize) -> usize) {
  // Read the old values first (before we overwrite either slot).
  let base_old = std::ptr::read_unaligned(pair.base_slot);
  let derived_old = std::ptr::read_unaligned(pair.derived_slot);

  let relocated_base = relocate_base(base_old);

  // Preserve interior-pointer offset only when we have both a base and a derived.
  // This keeps `null` pointers `null`, and avoids underflow for weird inputs.
  let relocated_derived = if base_old != 0 && derived_old != 0 && relocated_base != 0 {
    let delta = derived_old.wrapping_sub(base_old);
    relocated_base.wrapping_add(delta)
  } else {
    0
  };

  std::ptr::write_unaligned(pair.base_slot, relocated_base);
  std::ptr::write_unaligned(pair.derived_slot, relocated_derived);
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
/// - In practice, LLVM statepoints *often* spill GC roots into addressable stack slots
///   (`Indirect [SP/FP + off]`).
///
///   LLVM StackMaps can also describe roots in registers (`Register R#N`).
///   - When a saved `RegContext` is available (stop-the-world safepoint scanning), `runtime-native`
///     treats register roots as mutable lvalues inside that saved register file so a moving GC can
///     rewrite them in-place.
///   - When walking from a raw frame pointer without a register context (`walk_gc_roots_from_fp`),
///     register roots are not currently supported and will return an error.
/// - Derived pointers (statepoint `(base, derived)` pairs where `base != derived`) are supported.
///   The walker visits only the **base** root slots (derived slots are not traced as independent
///   roots). If the callback relocates a base slot in-place, the walker updates any derived slots
///   that reference it to preserve the interior offset.
///
///   For a pair-oriented API (useful for moving collectors that want to apply relocation with
///   [`crate::gc_roots::relocate_reloc_pairs_in_place`]), see
///   [`walk_gc_root_pairs_from_safepoint_context`] / [`walk_gc_reloc_pairs_from_safepoint_context`].
///
///   `derived_new = relocated_base + (derived_old - base_old)`
///
///   Null convention: if either `base_old` or `derived_old` is `0` (or if the GC
///   relocates `base` to `0`), the derived slot is written as `0`.
///
/// ## Statepoint-oriented walking
///
/// This walker is statepoint-oriented: the return address stored in the current
/// frame identifies the *caller's* safepoint callsite. Therefore we use
/// `(caller_fp, caller_ra)` extracted from the current frame, and enumerate
/// roots in the *caller* frame.
///
/// ### Callsite SP derivation (critical correctness note)
///
/// Stackmap locations are typically `Indirect [SP/FP + off]`.
///
/// For `Indirect [SP + off]`, `SP` is the *caller* frame's stack pointer value at the callsite return
/// address.
///
/// For correctness we must interpret the `SP` used by the stackmap record, not the callee-entry
/// stack pointer.
///
/// Under the forced-frame-pointer ABI contract on x86_64 SysV and AArch64, the caller's stackmap
/// `SP` is recoverable from the *callee* frame pointer:
///
/// `caller_sp_callsite = callee_fp + 16`
///
/// This is robust even when the callsite performs per-call stack adjustments (e.g. outgoing stack
/// arguments), and therefore must be preferred over stackmap function-record `stack_size`.
///
/// This behavior is validated by the LLVM-backed regression test
/// `runtime-native/tests/stackmap_callframe_adjust.rs`.
///
/// For patchable statepoints (`gc.statepoint` with `patch_bytes > 0`), LLVM 18
/// records the return address as the byte *after the reserved patchable region*.
/// Any runtime patcher must ensure the actual call return address matches that
/// end-of-region address (so the stackmap lookup key matches).
pub unsafe fn walk_gc_roots_from_fp(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  visit: impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  walk_gc_roots_from_fp_with_reg_context(start_fp, bounds, stackmaps, core::ptr::null_mut(), visit)
}

unsafe fn walk_gc_roots_from_fp_with_reg_context(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  reg_ctx: *mut crate::arch::RegContext,
  mut visit: impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }
  let bounds = bounds.ok_or(WalkError::MissingStackBounds)?;
  with_stackwalk_scratch(|scratch| {
    walk_gc_roots_from_fp_inner(start_fp, bounds, stackmaps, reg_ctx, &mut scratch.entries, &mut visit)
  })
}

unsafe fn walk_gc_roots_from_fp_inner(
  start_fp: u64,
  bounds: StackBounds,
  stackmaps: &StackMaps,
  reg_ctx: *mut crate::arch::RegContext,
  scratch_entries: &mut Vec<GcPairScratchEntry>,
  visit: &mut impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  let mut cur_fp = start_fp;
  let max_frames = max_frames_for_bounds(bounds);
  for depth in 0..max_frames {
    check_fp_alignment(cur_fp)?;
    check_fp_bounds(cur_fp, bounds)?;

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

    check_fp_bounds(caller_fp, bounds)?;

    if caller_ra == 0 {
      return Err(WalkError::ReturnAddressIsNull { fp: cur_fp });
    }

    if !is_canonical_pc(caller_ra) {
      return Err(WalkError::ReturnAddressNonCanonical {
        fp: cur_fp,
        return_addr: caller_ra,
      });
    }

    let caller_sp = caller_sp_callsite_from_callee_fp(cur_fp)?;
    match stackmaps.lookup(caller_ra) {
      Some(callsite) => {
        enumerate_roots_for_frame(
          caller_fp,
          caller_ra,
          callsite,
          bounds,
          Some(caller_sp),
          reg_ctx,
          scratch_entries,
          visit,
        )?;
      }
      None => {
        #[cfg(any(debug_assertions, feature = "conservative_roots"))]
        {
          conservative_scan_frame_words(caller_sp, caller_fp, bounds, visit);
        }
        // No stackmap record for the caller's return address. In a fully-instrumented managed
        // stack this indicates we've crossed into unmanaged/runtime frames (which are not scanned
        // precisely). Stop walking rather than failing the entire root enumeration.
        return Ok(());
      }
    }

    cur_fp = caller_fp;

    if depth + 1 == max_frames {
      break;
    }
  }

  Err(WalkError::MaxDepth {
    max_depth: max_frames,
  })
}

/// Walk GC root slots for a thread parked in a stop-the-world safepoint.
///
/// This is the entry point used by the STW GC: after [`crate::rt_gc_wait_for_world_stopped`]
/// returns, the GC can read each thread's published [`crate::arch::SafepointContext`] (captured
/// when the mutator entered the safepoint slow path) and call this function to enumerate precise
/// stack roots for that parked thread.
///
/// The callback is invoked with the address of each **base** root slot (the `base` half of each
/// statepoint `(base, derived)` pair). A relocating GC should treat the slot as `*mut *mut u8` and
/// may update it in-place.
///
/// If `base != derived`, this walker updates the derived slot in-place after the base slot has
/// potentially been relocated:
/// `derived_new = relocated_base + (derived_old - base_old)`.
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
  let bounds = bounds.ok_or(WalkError::MissingStackBounds)?;

  check_fp_alignment(caller_fp)?;
  check_fp_bounds(caller_fp, bounds)?;

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
  with_stackwalk_scratch(|scratch| {
    match stackmaps.lookup(caller_ra) {
      Some(callsite) => {
        // Prefer the captured stackmap-semantics SP for the top managed frame.
        //
        // If `ctx.sp` is missing, we can still derive the stackmap base from `ctx.sp_entry`:
        // - x86_64: `sp = sp_entry + 8` (return address pushed by `call`)
        // - aarch64: `sp = sp_entry` (`bl` does not push a return address)
        let caller_sp = caller_sp_override_from_safepoint_ctx(ctx);
        enumerate_roots_for_frame(
          caller_fp,
          caller_ra,
          callsite,
          bounds,
          caller_sp,
          ctx.regs,
          &mut scratch.entries,
          &mut visit,
        )?;
      }
      None => {
        #[cfg(any(debug_assertions, feature = "conservative_roots"))]
        {
          let scan_start = if ctx.sp != 0 {
            ctx.sp as u64
          } else {
            ctx.sp_entry as u64
          };
          if scan_start != 0 {
            let scan_end = bounds.hi;
            conservative_scan_frame_words(scan_start, scan_end, bounds, &mut visit);
          }
        }
      }
    }

    // Continue walking older frames. Starting from the managed frame pointer means the delegated
    // walker will enumerate roots in the *caller* frame, i.e. it won't double-enumerate the top
    // managed frame we just handled above.
    walk_gc_roots_from_fp_inner(caller_fp, bounds, stackmaps, ctx.regs, &mut scratch.entries, &mut visit)
  })
}

/// Walk the frame-pointer chain and enumerate GC relocation pairs (`(base, derived)` slots).
///
/// This is the primary API for moving collectors: it yields the exact stack slots LLVM recorded for
/// each `gc.relocate` use at every statepoint callsite, including interior/derived pointers where
/// `base != derived`.
///
/// Note: this uses the same stack-walking strategy as [`walk_gc_root_pairs_from_fp`], and stops
/// walking when it encounters a return address that is not present in `stackmaps` (treated as the
/// boundary into unmanaged/runtime frames).
pub unsafe fn walk_gc_reloc_pairs_from_fp(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  mut visit: impl FnMut(RelocPair),
) -> Result<(), WalkError> {
  walk_gc_root_pairs_from_fp(start_fp, bounds, stackmaps, |_return_addr, pairs| {
    for &(base_slot, derived_slot) in pairs {
      visit(RelocPair {
        base_slot: RootSlot::StackAddr(base_slot.cast::<u8>()),
        derived_slot: RootSlot::StackAddr(derived_slot.cast::<u8>()),
      });
    }
  })
}

/// Walk the frame-pointer chain and enumerate `(base_slot, derived_slot)` pairs using LLVM stackmaps.
///
/// This is a lower-level API than [`walk_gc_roots_from_fp`]: it reports the raw base/derived slot
/// pairs as encoded in LLVM statepoint stackmaps.
///
/// Callers should generally prefer [`walk_gc_roots_from_fp`] for tracing, because it visits base
/// root slots and updates derived slots automatically.
pub unsafe fn walk_gc_root_pairs_from_fp(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  visit_frame_reloc_pairs: impl FnMut(u64, &[(*mut usize, *mut usize)]),
) -> Result<(), WalkError> {
  walk_gc_root_pairs_from_fp_with_reg_context(
    start_fp,
    bounds,
    stackmaps,
    core::ptr::null_mut(),
    visit_frame_reloc_pairs,
  )
}

unsafe fn walk_gc_root_pairs_from_fp_with_reg_context(
  start_fp: u64,
  bounds: Option<StackBounds>,
  stackmaps: &StackMaps,
  reg_ctx: *mut crate::arch::RegContext,
  mut visit_frame_reloc_pairs: impl FnMut(u64, &[(*mut usize, *mut usize)]),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }
  let bounds = bounds.ok_or(WalkError::MissingStackBounds)?;
  with_stackwalk_scratch(|scratch| {
    walk_gc_root_pairs_from_fp_inner(
      start_fp,
      bounds,
      stackmaps,
      reg_ctx,
      &mut scratch.root_pairs,
      &mut visit_frame_reloc_pairs,
    )
  })
}

unsafe fn walk_gc_root_pairs_from_fp_inner(
  start_fp: u64,
  bounds: StackBounds,
  stackmaps: &StackMaps,
  reg_ctx: *mut crate::arch::RegContext,
  scratch_pairs: &mut Vec<(*mut usize, *mut usize)>,
  visit_frame_reloc_pairs: &mut impl FnMut(u64, &[(*mut usize, *mut usize)]),
) -> Result<(), WalkError> {
  if start_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  let mut cur_fp = start_fp;
  let max_frames = max_frames_for_bounds(bounds);
  for depth in 0..max_frames {
    check_fp_alignment(cur_fp)?;
    check_fp_bounds(cur_fp, bounds)?;

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

    check_fp_bounds(caller_fp, bounds)?;

    if caller_ra == 0 {
      return Err(WalkError::ReturnAddressIsNull { fp: cur_fp });
    }

    if !is_canonical_pc(caller_ra) {
      return Err(WalkError::ReturnAddressNonCanonical {
        fp: cur_fp,
        return_addr: caller_ra,
      });
    }

    let Some(callsite) = stackmaps.lookup(caller_ra) else {
      // No stackmap record for this return address. Treat this as the end of the managed stack and
      // stop walking; we intentionally do not traverse arbitrary unmanaged frames (libc, sysroot,
      // etc.) since they may not preserve a valid frame-pointer chain.
      return Ok(());
    };

    let caller_sp = caller_sp_callsite_from_callee_fp(cur_fp)?;
    let pairs = enumerate_root_pairs_for_frame(
      caller_fp,
      caller_ra,
      callsite,
      bounds,
      Some(caller_sp),
      reg_ctx,
      scratch_pairs,
    )?;
    if !pairs.is_empty() {
      visit_frame_reloc_pairs(caller_ra, pairs);
    }

    cur_fp = caller_fp;

    if depth + 1 == max_frames {
      break;
    }
  }

  Err(WalkError::MaxDepth {
    max_depth: max_frames,
  })
}

/// Walk GC root base/derived pairs for a thread parked in a stop-the-world safepoint.
///
/// This is the pair-oriented variant of [`walk_gc_roots_from_safepoint_context`].
pub unsafe fn walk_gc_root_pairs_from_safepoint_context(
  ctx: &crate::arch::SafepointContext,
  bounds: Option<StackBounds>,
  stackmaps: &crate::StackMaps,
  mut visit_frame_reloc_pairs: impl FnMut(u64, &[(*mut usize, *mut usize)]),
) -> Result<(), WalkError> {
  let caller_fp = ctx.fp as u64;
  if caller_fp == 0 {
    return Err(WalkError::NullStartFp);
  }

  let bounds = bounds.ok_or(WalkError::MissingStackBounds)?;
  check_fp_alignment(caller_fp)?;
  check_fp_bounds(caller_fp, bounds)?;

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
  with_stackwalk_scratch(|scratch| {
    if let Some(callsite) = stackmaps.lookup(caller_ra) {
      // Prefer the captured stackmap-semantics SP for the top managed frame (see the note in
      // `walk_gc_roots_from_safepoint_context`).
      let caller_sp = caller_sp_override_from_safepoint_ctx(ctx);
      let pairs = enumerate_root_pairs_for_frame(
        caller_fp,
        caller_ra,
        callsite,
        bounds,
        caller_sp,
        ctx.regs,
        &mut scratch.root_pairs,
      )?;
      if !pairs.is_empty() {
        visit_frame_reloc_pairs(caller_ra, pairs);
      }
    }

    walk_gc_root_pairs_from_fp_inner(
      caller_fp,
      bounds,
      stackmaps,
      ctx.regs,
      &mut scratch.root_pairs,
      &mut visit_frame_reloc_pairs,
    )
  })
}

/// Walk GC relocation pairs for a thread parked in a stop-the-world safepoint.
///
/// This is a [`RelocPair`]-typed wrapper around [`walk_gc_root_pairs_from_safepoint_context`].
/// It is intended for moving collectors that want to apply relocation in-place using
/// [`crate::gc_roots::relocate_reloc_pairs_in_place`].
///
/// For base pointers, `base_slot == derived_slot`. For interior pointers, the derived slot must be
/// updated relative to the base slot by preserving the interior offset.
///
/// # Safety
/// Same as [`walk_gc_roots_from_safepoint_context`].
pub unsafe fn walk_gc_reloc_pairs_from_safepoint_context(
  ctx: &crate::arch::SafepointContext,
  bounds: Option<StackBounds>,
  stackmaps: &crate::StackMaps,
  mut visit_pair: impl FnMut(RelocPair),
) -> Result<(), WalkError> {
  walk_gc_root_pairs_from_safepoint_context(ctx, bounds, stackmaps, |_return_addr, pairs| {
    for &(base_slot, derived_slot) in pairs {
      visit_pair(RelocPair {
        base_slot: RootSlot::StackAddr(base_slot.cast::<u8>()),
        derived_slot: RootSlot::StackAddr(derived_slot.cast::<u8>()),
      })
    }
  })
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

fn location_uses_sp(loc: &Location) -> bool {
  matches!(
    *loc,
    Location::Indirect {
      dwarf_reg: arch::DWARF_SP,
      ..
    }
  )
}

#[inline]
fn caller_sp_override_from_safepoint_ctx(ctx: &crate::arch::SafepointContext) -> Option<u64> {
  if ctx.sp != 0 {
    return Some(ctx.sp as u64);
  }

  if ctx.sp_entry == 0 {
    return None;
  }

  let sp_entry = ctx.sp_entry as u64;
  #[cfg(target_arch = "x86_64")]
  {
    // x86_64 `call` pushes the return address, so callee-entry RSP points at the return address and
    // the stackmap base is `RSP + 8`.
    Some(sp_entry.saturating_add(arch::WORD))
  }
  #[cfg(target_arch = "aarch64")]
  {
    // AArch64 `bl` does not push a return address; SP is unchanged.
    Some(sp_entry)
  }
}

fn ensure_scratch_capacity<T>(
  scratch: &mut Vec<T>,
  needed: usize,
  return_addr: u64,
) -> Result<(), WalkError> {
  if scratch.capacity() >= needed {
    return Ok(());
  }

  // Root enumeration must not allocate while a stop-the-world epoch is active. The global epoch is
  // a conservative signal: odd values mean a stop-the-world request is in progress (and may already
  // have stopped the world).
  if crate::threading::safepoint::current_epoch() & 1 == 1 {
    return Err(WalkError::GcPairScratchCapacityExceeded {
      return_addr,
      gc_pair_count: needed,
      scratch_capacity: scratch.capacity(),
    });
  }

  scratch.reserve_exact(needed.saturating_sub(scratch.len()));
  Ok(())
}

fn enumerate_roots_for_frame(
  caller_fp: u64,
  caller_ra: u64,
  callsite: CallSite<'_>,
  bounds: StackBounds,
  caller_sp_override: Option<u64>,
  reg_ctx: *mut crate::arch::RegContext,
  scratch_entries: &mut Vec<GcPairScratchEntry>,
  visit: &mut impl FnMut(*mut u8),
) -> Result<(), WalkError> {
  // `.llvm_stackmaps` may contain records other than GC statepoints (e.g. from
  // `llvm.experimental.stackmap`). Detect statepoints by attempting to decode the
  // callsite record using the LLVM 18 `gc.statepoint` layout.
  //
  // If decode fails, treat this record as a non-statepoint and skip it.
  let statepoint = match crate::statepoints::StatepointRecord::new(callsite.record) {
    Ok(sp) => sp,
    Err(_) => return Ok(()),
  };

  let needs_sp = statepoint
    .gc_pairs()
    .iter()
    .any(|pair| location_uses_sp(&pair.base) || location_uses_sp(&pair.derived));
  let caller_sp = if needs_sp {
    caller_sp_override.ok_or(WalkError::MissingStackmapSp { return_addr: caller_ra })?
  } else {
    0
  };

  if needs_sp {
    if caller_sp < bounds.lo || caller_sp > bounds.hi {
      return Err(WalkError::StackPointerOutOfBounds {
        caller_sp,
        lo: bounds.lo,
        hi: bounds.hi,
      });
    }
  }

  let gc_pair_count = statepoint.gc_pair_count();
  scratch_entries.clear();
  ensure_scratch_capacity(scratch_entries, gc_pair_count, caller_ra)?;

  // Collect + dedup within this frame to avoid double-visiting the same slot (LLVM can emit
  // duplicated locations for relocated values).
  //
  // We process the `(base, derived)` pairs in deterministic `(base_slot, derived_slot)` order:
  // - Visit each unique base slot once (roots are base slots).
  // - Apply derived-pointer fixups after the base slot has potentially been relocated by `visit`.
  //
  // Derived-pointer deltas are computed from the *pre-relocation* pointer values and stored in this
  // scratch array.
  for pair in statepoint.gc_pairs() {
    let base_slot = eval_root_location(caller_fp, caller_sp, caller_ra, reg_ctx, &pair.base)?;
    let derived_slot = eval_root_location(caller_fp, caller_sp, caller_ra, reg_ctx, &pair.derived)?;
    validate_root_slot(base_slot, bounds, caller_ra)?;
    validate_root_slot(derived_slot, bounds, caller_ra)?;

    let (delta, force_null) = if base_slot != derived_slot {
      let base_val = unsafe { read_u64(base_slot) };
      let derived_val = unsafe { read_u64(derived_slot) };
      if base_val == 0 || derived_val == 0 {
        (0, true)
      } else {
        let delta_i128 = (derived_val as i128) - (base_val as i128);
        let delta = i64::try_from(delta_i128).map_err(|_| WalkError::DerivedPointerDeltaOverflow {
          return_addr: caller_ra,
          base_val,
          derived_val,
        })?;
        (delta, false)
      }
    } else {
      (0, false)
    };

    scratch_entries.push(GcPairScratchEntry {
      base_slot,
      derived_slot,
      delta,
      force_null,
    });
  }

  scratch_entries.sort_unstable_by_key(|e| (e.base_slot, e.derived_slot));
  scratch_entries.dedup_by(|a, b| a.base_slot == b.base_slot && a.derived_slot == b.derived_slot);
  let entries = scratch_entries.as_slice();

  let mut cur_base: Option<u64> = None;
  let mut relocated_base_val: u64 = 0;
  for e in entries {
    if cur_base != Some(e.base_slot) {
      cur_base = Some(e.base_slot);
      visit(e.base_slot as *mut u8);
      relocated_base_val = unsafe { read_u64(e.base_slot) };
    }

    if e.derived_slot == e.base_slot {
      continue;
    }

    // Preserve `null` derived values:
    // - `derived_old == 0` must remain `0` after relocation (not `new_base + (0 - old_base)`),
    // - `base_old == 0` implies the derived pointer is meaningless (treat as null),
    // - if the GC writes `0` into the relocated base slot (should not happen, but stay safe),
    //   derived becomes null too.
    if e.force_null || relocated_base_val == 0 {
      unsafe { write_u64(e.derived_slot, 0) };
      continue;
    }

    let new_derived = add_signed_u64_i64(relocated_base_val, e.delta).ok_or(
      WalkError::DerivedPointerRelocationOverflow {
        return_addr: caller_ra,
        base_val: relocated_base_val,
        delta: e.delta,
      },
    )?;
    unsafe { write_u64(e.derived_slot, new_derived) };
  }

  Ok(())
}

fn enumerate_root_pairs_for_frame<'a>(
  caller_fp: u64,
  caller_ra: u64,
  callsite: CallSite<'_>,
  bounds: StackBounds,
  caller_sp_override: Option<u64>,
  reg_ctx: *mut crate::arch::RegContext,
  scratch_pairs: &'a mut Vec<(*mut usize, *mut usize)>,
) -> Result<&'a [(*mut usize, *mut usize)], WalkError> {
  scratch_pairs.clear();
  // Only statepoint callsites contribute GC root relocation pairs.
  let statepoint = match crate::statepoints::StatepointRecord::new(callsite.record) {
    Ok(sp) => sp,
    Err(_) => return Ok(scratch_pairs.as_slice()),
  };

  let needs_sp = statepoint
    .gc_pairs()
    .iter()
    .any(|pair| location_uses_sp(&pair.base) || location_uses_sp(&pair.derived));
  let caller_sp = if needs_sp {
    caller_sp_override.ok_or(WalkError::MissingStackmapSp { return_addr: caller_ra })?
  } else {
    0
  };

  if needs_sp {
    if caller_sp < bounds.lo || caller_sp > bounds.hi {
      return Err(WalkError::StackPointerOutOfBounds {
        caller_sp,
        lo: bounds.lo,
        hi: bounds.hi,
      });
    }
  }

  let gc_pair_count = statepoint.gc_pair_count();
  ensure_scratch_capacity(scratch_pairs, gc_pair_count, caller_ra)?;

  for pair in statepoint.gc_pairs() {
    let base_slot = eval_root_location(caller_fp, caller_sp, caller_ra, reg_ctx, &pair.base)?;
    let derived_slot = eval_root_location(caller_fp, caller_sp, caller_ra, reg_ctx, &pair.derived)?;
    validate_root_slot(base_slot, bounds, caller_ra)?;
    validate_root_slot(derived_slot, bounds, caller_ra)?;

    scratch_pairs.push((
      base_slot as usize as *mut usize,
      derived_slot as usize as *mut usize,
    ));
  }

  // Deterministic ordering and dedup.
  scratch_pairs.sort_unstable_by_key(|&(base, derived)| (base as usize, derived as usize));
  scratch_pairs.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

  Ok(scratch_pairs.as_slice())
}

fn eval_root_location(
  caller_fp: u64,
  caller_sp: u64,
  caller_ra: u64,
  reg_ctx: *mut crate::arch::RegContext,
  loc: &Location,
) -> Result<u64, WalkError> {
  match *loc {
    Location::Indirect {
      dwarf_reg, offset, ..
    } => {
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

    Location::Register { dwarf_reg, .. } => {
      if reg_ctx.is_null() {
        return Err(WalkError::MissingRegContext {
          return_addr: caller_ra,
          dwarf_reg,
        });
      }
      if let Some(kind) = crate::arch::regs::forbidden_gc_root_reg(dwarf_reg) {
        return Err(WalkError::ForbiddenRegisterRoot {
          return_addr: caller_ra,
          dwarf_reg,
          kind,
        });
      }
      let Some(slot) = (unsafe { crate::arch::regs::reg_slot_ptr(reg_ctx, dwarf_reg) }) else {
        return Err(WalkError::UnsupportedRegisterRoot {
          return_addr: caller_ra,
          dwarf_reg,
        });
      };
      Ok(slot as u64)
    }

    // Treat direct-address expressions or constants as hard errors with context.
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
fn validate_root_slot(slot_addr: u64, bounds: StackBounds, return_addr: u64) -> Result<(), WalkError> {
  if slot_addr % arch::WORD != 0 {
    return Err(WalkError::MisalignedRootSlot {
      return_addr,
      slot_addr,
      alignment: arch::WORD,
    });
  }

  if !bounds.contains_range(slot_addr, arch::WORD) {
    return Err(WalkError::RootSlotOutOfBounds {
      return_addr,
      slot_addr,
      lo: bounds.lo,
      hi: bounds.hi,
    });
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

#[inline]
fn caller_sp_callsite_from_callee_fp(callee_fp: u64) -> Result<u64, WalkError> {
  // With frame pointers enabled on both x86_64 SysV and AArch64:
  // - the call instruction reserves 8 bytes for the return address (x86_64 only),
  // - the callee prologue saves FP (and LR on AArch64) in a 16-byte frame record,
  //
  // so `callee_fp = caller_sp_callsite - 16`.
  callee_fp
    .checked_add(arch::WORD * 2)
    .ok_or(WalkError::CallerSpOverflow { callee_fp })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stackmaps::StackSize;

  #[test]
  fn derives_callsite_sp_from_callee_fp() {
    let callee_fp = 0x1000_u64;
    let caller_sp = caller_sp_callsite_from_callee_fp(callee_fp).unwrap();
    assert_eq!(caller_sp, 0x1010_u64);
  }

  #[test]
  fn skips_non_statepoint_stackmap_records() {
    // `enumerate_roots_for_frame` should ignore non-statepoint stackmap records
    // (e.g. those emitted by `llvm.experimental.stackmap` / patchpoints).
    //
    // Regression test: the FP walker previously attempted to decode every
    // `.llvm_stackmaps` record as a `gc.statepoint`, causing root enumeration to
    // fail when a non-statepoint record was present.
    let record = crate::stackmaps::StackMapRecord {
      patchpoint_id: 1,
      instruction_offset: 0,
      locations: vec![
        Location::Register {
          size: 8,
          dwarf_reg: 0,
          offset: 0,
        },
        Location::Register {
          size: 8,
          dwarf_reg: 0,
          offset: 0,
        },
        Location::Register {
          size: 8,
          dwarf_reg: 0,
          offset: 0,
        },
      ],
      live_outs: vec![],
    };

    let callsite = CallSite {
      stack_size: StackSize::Known(0),
      record: &record,
    };

    let bounds = StackBounds { lo: 0, hi: 0x10_000 };
    let mut visited = false;
    let mut scratch_entries: Vec<GcPairScratchEntry> = Vec::new();
    enumerate_roots_for_frame(
      0x1000,
      0x2000,
      callsite,
      bounds,
      None,
      core::ptr::null_mut(),
      &mut scratch_entries,
      &mut |_| {
      visited = true;
      },
    )
    .unwrap();
    assert!(!visited, "non-statepoint records must not contribute GC roots");
  }
}
