use std::mem::align_of;

/// A view of a caller frame as observed from its callee (i.e. the frame we're currently in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameView {
  /// The caller's frame pointer (saved at `[callee_fp + 0]`).
  pub caller_fp: usize,
  /// The caller's stack pointer at the call site (computed as `callee_fp + 16` on x86_64 SysV).
  pub caller_sp: usize,
  /// The return address into the caller (saved at `[callee_fp + 8]`).
  pub return_address: usize,
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
}

impl StackWalker {
  pub const DEFAULT_MAX_DEPTH: usize = 1024;

  /// # Safety
  /// `top_callee_fp` must be a valid frame pointer for the current thread.
  pub unsafe fn new(top_callee_fp: usize) -> Self {
    Self {
      next_callee_fp: top_callee_fp,
      prev_fp: 0,
      depth: 0,
      max_depth: Self::DEFAULT_MAX_DEPTH,
    }
  }

  pub fn with_max_depth(mut self, max_depth: usize) -> Self {
    self.max_depth = max_depth;
    self
  }

  /// Produces the next caller frame view. Returns `None` when reaching the end of the FP chain
  /// or when sanity checks fail.
  ///
  /// # Safety
  /// Walks raw stack memory.
  pub unsafe fn next_frame(&mut self) -> Option<FrameView> {
    if self.next_callee_fp == 0 || self.depth >= self.max_depth {
      return None;
    }

    let callee_fp = self.next_callee_fp;

    // Basic alignment sanity check.
    if callee_fp % align_of::<usize>() != 0 {
      return None;
    }

    // Ensure the FP chain is monotonic (stack grows down; walking "up" should increase addresses).
    if self.prev_fp != 0 && callee_fp <= self.prev_fp {
      return None;
    }

    let callee_fp_ptr = callee_fp as *const usize;

    let caller_fp = callee_fp_ptr.read();
    let return_address = callee_fp_ptr.add(1).read();

    if caller_fp == 0 || return_address == 0 {
      return None;
    }

    if caller_fp <= callee_fp {
      return None;
    }

    let caller_sp = callee_fp + 16;

    self.prev_fp = callee_fp;
    self.next_callee_fp = caller_fp;
    self.depth += 1;

    Some(FrameView {
      caller_fp,
      caller_sp,
      return_address,
    })
  }
}

