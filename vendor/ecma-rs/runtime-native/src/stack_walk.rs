use std::mem::align_of;

use crate::stackwalk::StackBounds;

/// A view of a caller frame as observed from its callee (i.e. the frame we're currently in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameView {
  /// The caller's frame pointer (saved at `[callee_fp + 0]`).
  pub caller_fp: usize,
  /// The caller's DWARF call-frame address (CFA) at the call site.
  ///
  /// This is computed as `callee_fp + 16` on both x86_64 SysV and AArch64 when walking via frame
  /// pointers.
  ///
  /// Note: LLVM StackMaps `Indirect [SP + off]` locations for statepoints are based on the caller's
  /// stack pointer at the safepoint call site. When walking via frame pointers, this matches the
  /// caller CFA value derived from the callee frame pointer.
  pub caller_cfa: usize,
  /// The return address into the caller (saved at `[callee_fp + 8]`).
  pub return_address: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum FpWalkError {
  #[error("frame-pointer walk exceeded max depth ({max_depth}), possible cycle or corruption")]
  MaxDepthExceeded { max_depth: usize },

  #[error("frame pointer {fp:#x} is misaligned (expected {alignment}-byte alignment)")]
  FramePointerMisaligned { fp: usize, alignment: usize },

  #[error(
    "frame pointer {fp:#x} is outside stack bounds [{lo:#x}, {hi:#x}) (need {record_size} bytes)"
  )]
  FramePointerOutOfBounds {
    fp: usize,
    lo: u64,
    hi: u64,
    record_size: u64,
  },

  #[error("frame pointer chain is not monotonically increasing: fp={fp:#x} prev_fp={prev_fp:#x}")]
  NonMonotonicFramePointer { fp: usize, prev_fp: usize },

  #[error("return address {return_address:#x} at fp={fp:#x} is not canonical")]
  ReturnAddressNonCanonical { fp: usize, return_address: usize },

  #[error("caller SP computation overflowed for fp={fp:#x}")]
  CallerSpOverflow { fp: usize },

  #[error("caller SP {caller_sp:#x} is outside stack bounds (hi={hi:#x})")]
  CallerSpOutOfBounds { caller_sp: usize, hi: u64 },
}

/// Frame-pointer stack walker for x86_64 SysV.
///
/// Assumes the program is compiled with frame pointers enabled for all code we want to walk.
/// Layout:
/// - `[fp + 0]` saved caller fp
/// - `[fp + 8]` return address
/// - caller's `rsp` at callsite is `fp + 16`
pub struct StackWalker {
  next_callee_fp: usize,
  prev_fp: usize,
  depth: usize,
  max_depth: usize,
  bounds: Option<StackBounds>,
}

impl StackWalker {
  pub const DEFAULT_MAX_DEPTH: usize = 1024;
  const FP_RECORD_SIZE: u64 = 16;
  const FP_ALIGN: usize = 16;
  const CALLER_CFA_OFFSET: usize = 16;

  /// # Safety
  /// `top_callee_fp` must be a valid frame pointer for the current thread.
  pub unsafe fn new(top_callee_fp: usize, bounds: Option<StackBounds>) -> Self {
    Self {
      next_callee_fp: top_callee_fp,
      prev_fp: 0,
      depth: 0,
      max_depth: Self::DEFAULT_MAX_DEPTH,
      bounds,
    }
  }

  pub fn with_max_depth(mut self, max_depth: usize) -> Self {
    self.max_depth = max_depth;
    self
  }

  /// Produces the next caller frame view. Returns `None` when reaching the end of the FP chain
  /// and returns an error when sanity checks fail.
  ///
  /// # Safety
  /// Walks raw stack memory.
  pub unsafe fn next_frame(&mut self) -> Result<Option<FrameView>, FpWalkError> {
    if self.next_callee_fp == 0 {
      return Ok(None);
    }
    if self.depth >= self.max_depth {
      return Err(FpWalkError::MaxDepthExceeded {
        max_depth: self.max_depth,
      });
    }

    let callee_fp = self.next_callee_fp;

    // Basic alignment sanity check.
    if callee_fp % Self::FP_ALIGN != 0 || callee_fp % align_of::<usize>() != 0 {
      return Err(FpWalkError::FramePointerMisaligned {
        fp: callee_fp,
        alignment: Self::FP_ALIGN,
      });
    }

    if let Some(bounds) = self.bounds {
      if !bounds.contains_range(callee_fp as u64, Self::FP_RECORD_SIZE) {
        return Err(FpWalkError::FramePointerOutOfBounds {
          fp: callee_fp,
          lo: bounds.lo,
          hi: bounds.hi,
          record_size: Self::FP_RECORD_SIZE,
        });
      }
    }

    // Ensure the FP chain is monotonic (stack grows down; walking "up" should increase addresses).
    if self.prev_fp != 0 && callee_fp <= self.prev_fp {
      return Err(FpWalkError::NonMonotonicFramePointer {
        fp: callee_fp,
        prev_fp: self.prev_fp,
      });
    }

    let callee_fp_ptr = callee_fp as *const usize;

    let caller_fp = callee_fp_ptr.read();
    let return_address = callee_fp_ptr.add(1).read();

    if caller_fp == 0 {
      return Ok(None);
    }

    if caller_fp <= callee_fp {
      return Err(FpWalkError::NonMonotonicFramePointer {
        fp: caller_fp,
        prev_fp: callee_fp,
      });
    }

    if caller_fp % Self::FP_ALIGN != 0 || caller_fp % align_of::<usize>() != 0 {
      return Err(FpWalkError::FramePointerMisaligned {
        fp: caller_fp,
        alignment: Self::FP_ALIGN,
      });
    }

    if let Some(bounds) = self.bounds {
      if !bounds.contains_range(caller_fp as u64, Self::FP_RECORD_SIZE) {
        return Err(FpWalkError::FramePointerOutOfBounds {
          fp: caller_fp,
          lo: bounds.lo,
          hi: bounds.hi,
          record_size: Self::FP_RECORD_SIZE,
        });
      }
    }

    if !is_canonical_pc(return_address) {
      return Err(FpWalkError::ReturnAddressNonCanonical {
        fp: callee_fp,
        return_address,
      });
    }

    let caller_cfa = callee_fp
      .checked_add(Self::CALLER_CFA_OFFSET)
      .ok_or(FpWalkError::CallerSpOverflow { fp: callee_fp })?;
    if let Some(bounds) = self.bounds {
      if caller_cfa as u64 > bounds.hi {
        return Err(FpWalkError::CallerSpOutOfBounds {
          caller_sp: caller_cfa,
          hi: bounds.hi,
        });
      }
    }

    self.prev_fp = callee_fp;
    self.next_callee_fp = caller_fp;
    self.depth += 1;

    Ok(Some(FrameView {
      caller_fp,
      caller_cfa,
      return_address,
    }))
  }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn is_canonical_pc(pc: usize) -> bool {
  // Canonical addresses are sign-extended from bit 47 (SysV x86_64).
  let pc = pc as u64;
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
fn is_canonical_pc(_pc: usize) -> bool {
  true
}
