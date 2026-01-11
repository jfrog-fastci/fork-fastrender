//! Stack walking / unwinding for GC-managed threads.
//!
//! ## Why this exists
//! LLVM statepoint stackmaps are keyed by the *return address* of the safepoint callsite and stack
//! slots are typically described as `Indirect [SP/FP + off]`.
//!
//! For `Indirect [SP + off]`, `SP` is the *caller* frame's stack pointer at that return address.
//!
//! If a thread is stopped inside the safepoint callee (common for stop-the-world GC), we must
//! recover the caller's `SP` (and the return address) for each frame.
//!
//! ## Unwinding strategy (first milestone)
//! We use **frame-pointer walking**. All code that can run on GC-managed threads must be compiled
//! with frame pointers enabled:
//! - LLVM codegen: `llc -frame-pointer=all` (or equivalent target options).
//! - Rust runtime: `-C force-frame-pointers=yes` (see `scripts/cargo_llvm.sh`).

use core::fmt;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod unsupported;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
use unsupported as arch;
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;

/// DWARF register number for the stack pointer used by LLVM StackMaps for this architecture.
pub const DWARF_SP_REG: u16 = arch::DWARF_SP_REG;

/// DWARF register number for the frame pointer used by LLVM StackMaps for this architecture.
pub const DWARF_FP_REG: u16 = arch::DWARF_FP_REG;

/// A cursor into the nearest managed frame.
///
/// When the current thread is executing inside runtime-native Rust code (e.g. an allocator slow
/// path calls `rt_gc_collect`), the current return address is *not* a managed callsite and will not
/// appear in the LLVM stackmaps index. The nearest managed frame is suspended at the callsite into
/// the outermost runtime frame, and *that* return address is present in stackmaps.
///
/// This cursor records that managed callsite as `(fp, pc, sp)` so root enumeration can start from
/// the correct frame even when GC is triggered from within runtime code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCursor {
  /// Managed caller frame pointer (saved at `[runtime_fp + 0]`).
  pub fp: u64,
  /// Return address into the managed caller (saved at `[runtime_fp + 8]`).
  pub pc: u64,
  /// Managed caller stack pointer at the callsite into the runtime frame.
  ///
  /// Under the forced-frame-pointer ABI contract this is `runtime_fp + 16` on both x86_64 and
  /// aarch64.
  pub sp: Option<u64>,
}

/// Return the current function's frame pointer (`rbp` on x86_64, `x29` on aarch64).
#[inline(always)]
pub fn current_frame_pointer() -> u64 {
  #[cfg(target_arch = "x86_64")]
  unsafe {
    let fp: u64;
    core::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack));
    fp
  }

  #[cfg(target_arch = "aarch64")]
  unsafe {
    let fp: u64;
    core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack));
    fp
  }

  #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
  {
    0
  }
}

const MAX_CURSOR_DEPTH: usize = 1024;

fn find_nearest_managed_cursor_with(
  start_fp: u64,
  mut has_stackmap: impl FnMut(u64) -> bool,
) -> Option<FrameCursor> {
  if start_fp == 0 {
    return None;
  }

  // When walking raw frame pointers, bound reads to the current thread's stack range when possible.
  // This avoids potentially crashing by dereferencing bogus `fp_caller` values if the FP chain is
  // corrupted.
  let bounds = StackBounds::current_thread().ok();

  // Skip frames until we find a return address that belongs to managed code (has a stackmap entry).
  let mut fp_cur = start_fp;
  for _ in 0..MAX_CURSOR_DEPTH {
    if fp_cur == 0 {
      return None;
    }

    // Basic sanity: frame pointers must obey alignment.
    if fp_cur % arch::FRAME_POINTER_ALIGNMENT != 0 {
      return None;
    }

    if let Some(bounds) = bounds {
      if !bounds.contains_range(fp_cur, arch::FRAME_RECORD_SIZE) {
        return None;
      }
    }

    // SAFETY: Caller supplies a valid frame pointer chain.
    let fp_caller = unsafe { (fp_cur as *const u64).read() };
    let pc_return = unsafe { ((fp_cur + arch::RETURN_ADDRESS_OFFSET) as *const u64).read() };

    if fp_caller == 0 || pc_return == 0 {
      return None;
    }

    if fp_caller <= fp_cur {
      return None;
    }

    if let Some(bounds) = bounds {
      if !bounds.contains_range(fp_caller, arch::FRAME_RECORD_SIZE) {
        return None;
      }
    }

    if has_stackmap(pc_return) {
      let sp_callsite = fp_cur.checked_add(arch::CALLER_SP_OFFSET)?;
      if let Some(bounds) = bounds {
        if sp_callsite > bounds.hi {
          return None;
        }
      }
      return Some(FrameCursor {
        fp: fp_caller,
        pc: pc_return,
        sp: Some(sp_callsite),
      });
    }

    fp_cur = fp_caller;
  }

  None
}

/// Walk the frame-pointer chain starting at `start_fp` and return the nearest managed callsite
/// cursor.
pub fn find_nearest_managed_cursor(start_fp: u64, stackmaps: &crate::StackMaps) -> Option<FrameCursor> {
  find_nearest_managed_cursor_with(start_fp, |pc| stackmaps.lookup(pc).is_some())
}

/// Convenience wrapper over [`find_nearest_managed_cursor`] for the current thread.
///
/// Note: this intentionally starts from the *caller* frame of this helper, not this helper's own
/// frame, so runtime code can call it and recover the nearest managed callsite without accidentally
/// selecting a stackmap record inside this helper itself.
#[inline(never)]
pub fn find_nearest_managed_cursor_from_here(stackmaps: &crate::StackMaps) -> Option<FrameCursor> {
  let fp = current_frame_pointer();
  if fp == 0 {
    return None;
  }

  // SAFETY: under the forced-frame-pointer ABI, `fp` points at a valid frame record:
  // `[fp + 0]` is the saved caller FP.
  let caller_fp = unsafe { (fp as *const u64).read() };
  find_nearest_managed_cursor(caller_fp, stackmaps)
}

/// Captured CPU context for a stopped thread.
///
/// This is intentionally minimal: stack walking via frame pointers only needs the current stack
/// pointer (`sp`) and frame pointer (`fp`). `ip` is included for diagnostics and future unwind
/// strategies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadContext {
  pub sp: u64,
  pub fp: u64,
  pub ip: u64,
}

impl ThreadContext {
  pub const fn new(sp: u64, fp: u64, ip: u64) -> Self {
    Self { sp, fp, ip }
  }
}

/// Address range (half-open) that bounds valid stack memory: `[lo, hi)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackBounds {
  pub lo: u64,
  pub hi: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackBoundsError {
  InvalidRange,
  UnsupportedPlatform,
  OsError(i32),
}

impl StackBounds {
  pub const fn new(lo: u64, hi: u64) -> Result<Self, StackBoundsError> {
    if lo >= hi {
      return Err(StackBoundsError::InvalidRange);
    }
    Ok(Self { lo, hi })
  }

  pub const fn contains_range(self, addr: u64, len: u64) -> bool {
    if addr < self.lo {
      return false;
    }
    let Some(end) = addr.checked_add(len) else {
      return false;
    };
    end <= self.hi
  }

  /// Returns the stack bounds for the current pthread.
  #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
  pub fn current_thread() -> Result<Self, StackBoundsError> {
    match crate::thread_stack::current_thread_stack_bounds() {
      Ok(b) => StackBounds::new(b.low as u64, b.high as u64).map_err(|_| StackBoundsError::InvalidRange),
      Err(crate::thread_stack::Error::UnsupportedTarget { .. }) => Err(StackBoundsError::UnsupportedPlatform),
      Err(crate::thread_stack::Error::Pthread { code, .. }) => Err(StackBoundsError::OsError(code)),
      Err(_) => Err(StackBoundsError::InvalidRange),
    }
  }

  /// Returns the stack bounds for the current thread (unsupported on this platform).
  #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
  pub fn current_thread() -> Result<Self, StackBoundsError> {
    Err(StackBoundsError::UnsupportedPlatform)
  }
}

/// A single stack frame edge.
///
/// This frame represents the currently executing function (the *callee*), and contains:
/// - `return_address`: the address in the caller right after the call that created this frame.
/// - `caller_sp`: the caller's stack pointer value at that return address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackFrame {
  pub return_address: u64,
  pub caller_sp: u64,
  pub frame_pointer: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackWalkError {
  UnsupportedArch,
  FramePointerIsNull,
  FramePointerMisaligned { fp: u64 },
  FramePointerOutOfBounds { fp: u64, bounds: StackBounds },
  CallerSpOutOfBounds { caller_sp: u64, bounds: StackBounds },
  ReturnAddressIsNull { fp: u64 },
  ReturnAddressNonCanonical { fp: u64, return_address: u64 },
  NonMonotonicFramePointer { fp: u64, next_fp: u64 },
  MaxDepthExceeded { max_depth: usize },
  UnalignedRead { addr: u64 },
  AddressOverflow,
}

impl fmt::Display for StackWalkError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      StackWalkError::UnsupportedArch => write!(f, "unsupported architecture"),
      StackWalkError::FramePointerIsNull => write!(f, "frame pointer is null"),
      StackWalkError::FramePointerMisaligned { fp } => {
        write!(f, "frame pointer {fp:#x} is misaligned")
      }
      StackWalkError::FramePointerOutOfBounds { fp, bounds } => write!(
        f,
        "frame pointer {fp:#x} is outside stack bounds [{:#x}, {:#x})",
        bounds.lo, bounds.hi
      ),
      StackWalkError::CallerSpOutOfBounds { caller_sp, bounds } => write!(
        f,
        "caller SP {caller_sp:#x} is outside stack bounds [{:#x}, {:#x})",
        bounds.lo, bounds.hi
      ),
      StackWalkError::ReturnAddressIsNull { fp } => {
        write!(f, "return address at {fp:#x}+8 is null")
      }
      StackWalkError::ReturnAddressNonCanonical { fp, return_address } => write!(
        f,
        "return address {return_address:#x} at {fp:#x}+8 is not canonical"
      ),
      StackWalkError::NonMonotonicFramePointer { fp, next_fp } => write!(
        f,
        "frame pointer chain is non-monotonic: fp={fp:#x}, next_fp={next_fp:#x}"
      ),
      StackWalkError::MaxDepthExceeded { max_depth } => {
        write!(f, "max stack depth exceeded ({max_depth})")
      }
      StackWalkError::UnalignedRead { addr } => write!(f, "attempted unaligned read at {addr:#x}"),
      StackWalkError::AddressOverflow => write!(f, "address computation overflowed"),
    }
  }
}

/// Frame-pointer-based stack walker.
#[derive(Clone, Debug)]
pub struct StackWalker {
  fp: u64,
  bounds: StackBounds,
  depth: usize,
  max_depth: usize,
  done: bool,
}

impl StackWalker {
  pub fn new(ctx: ThreadContext, bounds: StackBounds) -> Result<Self, StackWalkError> {
    if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
      return Err(StackWalkError::UnsupportedArch);
    }
    if ctx.fp == 0 {
      return Err(StackWalkError::FramePointerIsNull);
    }

    Ok(Self {
      fp: ctx.fp,
      bounds,
      depth: 0,
      max_depth: 1024,
      done: false,
    })
  }

  pub fn with_max_depth(mut self, max_depth: usize) -> Self {
    self.max_depth = max_depth;
    self
  }

  #[inline]
  fn validate_fp(&self, fp: u64) -> Result<(), StackWalkError> {
    if fp % arch::FRAME_POINTER_ALIGNMENT != 0 {
      return Err(StackWalkError::FramePointerMisaligned { fp });
    }
    // We must be able to read:
    //   [fp + 0]  => previous fp
    //   [fp + 8]  => return address / saved LR
    if !self.bounds.contains_range(fp, arch::FRAME_RECORD_SIZE) {
      return Err(StackWalkError::FramePointerOutOfBounds {
        fp,
        bounds: self.bounds,
      });
    }
    Ok(())
  }

  #[inline]
  unsafe fn read_u64_aligned(addr: u64) -> Result<u64, StackWalkError> {
    if addr % 8 != 0 {
      return Err(StackWalkError::UnalignedRead { addr });
    }
    // SAFETY: Callers must ensure the address is valid to read (stack bounds checks).
    Ok(unsafe { (addr as *const u64).read() })
  }
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

impl Iterator for StackWalker {
  type Item = Result<StackFrame, StackWalkError>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.done {
      return None;
    }
    if self.depth >= self.max_depth {
      self.done = true;
      return Some(Err(StackWalkError::MaxDepthExceeded {
        max_depth: self.max_depth,
      }));
    }

    let fp = self.fp;
    if fp == 0 {
      self.done = true;
      return None;
    }

    if let Err(err) = self.validate_fp(fp) {
      self.done = true;
      return Some(Err(err));
    }

    let prev_fp_addr = fp;
    let ret_addr_addr = fp.checked_add(arch::RETURN_ADDRESS_OFFSET);
    let caller_sp = fp.checked_add(arch::CALLER_SP_OFFSET);
    let (Some(ret_addr_addr), Some(caller_sp)) = (ret_addr_addr, caller_sp) else {
      self.done = true;
      return Some(Err(StackWalkError::AddressOverflow));
    };

    if caller_sp > self.bounds.hi {
      self.done = true;
      return Some(Err(StackWalkError::CallerSpOutOfBounds {
        caller_sp,
        bounds: self.bounds,
      }));
    }

    // SAFETY: we validated `fp` is within bounds for `FRAME_RECORD_SIZE`.
    let prev_fp = unsafe { Self::read_u64_aligned(prev_fp_addr) };
    let ret_addr = unsafe { Self::read_u64_aligned(ret_addr_addr) };
    let (prev_fp, ret_addr) = match (prev_fp, ret_addr) {
      (Ok(prev_fp), Ok(ret_addr)) => (prev_fp, ret_addr),
      (Err(e), _) | (_, Err(e)) => {
        self.done = true;
        return Some(Err(e));
      }
    };

    if ret_addr == 0 {
      self.done = true;
      return Some(Err(StackWalkError::ReturnAddressIsNull { fp }));
    }

    if !is_canonical_pc(ret_addr) {
      self.done = true;
      return Some(Err(StackWalkError::ReturnAddressNonCanonical {
        fp,
        return_address: ret_addr,
      }));
    }

    if prev_fp != 0 && prev_fp <= fp {
      self.done = true;
      return Some(Err(StackWalkError::NonMonotonicFramePointer {
        fp,
        next_fp: prev_fp,
      }));
    }

    self.fp = prev_fp;
    self.depth += 1;

    Some(Ok(StackFrame {
      return_address: ret_addr,
      caller_sp,
      frame_pointer: fp,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[repr(align(16))]
  struct AlignedStack<const N: usize>([u8; N]);

  #[test]
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  fn walk_two_frames_ok() {
    let mut mem = AlignedStack([0u8; 128]);
    let base = mem.0.as_mut_ptr() as u64;
    let hi = base + mem.0.len() as u64;

    let fp0 = base + 0x20;
    let fp1 = base + 0x40;
    assert_eq!(fp0 % 16, 0);
    assert_eq!(fp1 % 16, 0);

    unsafe {
      (fp0 as *mut u64).write(fp1);
      ((fp0 + 8) as *mut u64).write(0x1111);

      (fp1 as *mut u64).write(0);
      ((fp1 + 8) as *mut u64).write(0x2222);
    }

    let ctx = ThreadContext::new(0, fp0, 0);
    let bounds = StackBounds::new(base, hi).unwrap();
    let frames: Vec<StackFrame> = StackWalker::new(ctx, bounds)
      .unwrap()
      .map(Result::unwrap)
      .collect();

    assert_eq!(frames.len(), 2);
    assert_eq!(
      frames[0],
      StackFrame {
        return_address: 0x1111,
        caller_sp: fp0 + 16,
        frame_pointer: fp0,
      }
    );
    assert_eq!(
      frames[1],
      StackFrame {
        return_address: 0x2222,
        caller_sp: fp1 + 16,
        frame_pointer: fp1,
      }
    );
  }

  #[test]
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  fn walk_rejects_misaligned_fp() {
    let mut mem = AlignedStack([0u8; 64]);
    let base = mem.0.as_mut_ptr() as u64;
    let hi = base + mem.0.len() as u64;
    let fp = base + 8;
    assert_ne!(fp % 16, 0);

    let ctx = ThreadContext::new(0, fp, 0);
    let bounds = StackBounds::new(base, hi).unwrap();
    let mut walker = StackWalker::new(ctx, bounds).unwrap();
    let err = walker.next().unwrap().unwrap_err();
    assert!(matches!(err, StackWalkError::FramePointerMisaligned { .. }));
    assert!(walker.next().is_none());
  }

  #[test]
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  fn walk_rejects_non_monotonic_fp() {
    let mut mem = AlignedStack([0u8; 128]);
    let base = mem.0.as_mut_ptr() as u64;
    let hi = base + mem.0.len() as u64;

    let fp0 = base + 0x20;
    unsafe {
      // Make the chain loop back to itself.
      (fp0 as *mut u64).write(fp0);
      ((fp0 + 8) as *mut u64).write(0x1234);
    }

    let ctx = ThreadContext::new(0, fp0, 0);
    let bounds = StackBounds::new(base, hi).unwrap();
    let mut walker = StackWalker::new(ctx, bounds).unwrap();
    let err = walker.next().unwrap().unwrap_err();
    assert!(matches!(
      err,
      StackWalkError::NonMonotonicFramePointer { .. }
    ));
    assert!(walker.next().is_none());
  }

  #[test]
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  fn find_nearest_managed_cursor_skips_runtime_frames() {
    let mut mem = AlignedStack([0u8; 256]);
    let base = mem.0.as_mut_ptr() as u64;
    let hi = base + mem.0.len() as u64;

    let fp_managed = base + 0x80;
    let fp_rt_alloc = base + 0x60;
    let fp_rt_collect = base + 0x40;
    assert_eq!(fp_managed % 16, 0);
    assert_eq!(fp_rt_alloc % 16, 0);
    assert_eq!(fp_rt_collect % 16, 0);

    unsafe {
      // rt_gc_collect frame -> rt_alloc (return address is runtime, no stackmap)
      (fp_rt_collect as *mut u64).write(fp_rt_alloc);
      ((fp_rt_collect + 8) as *mut u64).write(0x1111);

      // rt_alloc frame -> managed (return address has stackmap)
      (fp_rt_alloc as *mut u64).write(fp_managed);
      ((fp_rt_alloc + 8) as *mut u64).write(0x2222);

      // managed frame -> end
      (fp_managed as *mut u64).write(0);
      ((fp_managed + 8) as *mut u64).write(0x3333);
    }

    let cursor =
      find_nearest_managed_cursor_with(fp_rt_collect, |pc| pc == 0x2222).expect("cursor");
    assert_eq!(
      cursor,
      FrameCursor {
        fp: fp_managed,
        pc: 0x2222,
        sp: Some(fp_rt_alloc + 16),
      }
    );

    // Sanity: our synthetic memory bounds are large enough for the fp+16 computation.
    assert!(StackBounds::new(base, hi).unwrap().contains_range(fp_rt_alloc + 16, 0));
  }
}
