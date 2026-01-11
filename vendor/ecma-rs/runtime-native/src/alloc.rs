//! Milestone-1 bump allocator (linear, leak-only, no GC integration).
//!
//! Design goals:
//! - Extremely fast allocations for the proof-of-concept stage.
//! - No freeing / no GC integration yet (allocations leak for process lifetime).
//! - Thread-safe with low contention: per-thread local bump buffers that refill from a
//!   process-global bump pointer.
//!
//! Linux implementation:
//! - Reserve one large virtual address region with `mmap`.
//! - Maintain an atomic global bump *offset* into that region.
//! - Each thread keeps a local `[cur, end)` allocation range; on exhaustion it grabs a
//!   new chunk from the global bump offset via `fetch_add`.
//!
//! Non-Linux fallback:
//! - Uses `std::alloc` directly (still abort-on-OOM).

use core::ptr::NonNull;

#[cfg(target_os = "linux")]
mod imp {
  use core::cell::Cell;
  use core::ptr::NonNull;
  use core::sync::atomic::AtomicUsize;
  use core::sync::atomic::Ordering;

  use once_cell::sync::Lazy;

  use crate::trap;

  const ENV_ARENA_SIZE: &str = "RUNTIME_NATIVE_BUMP_ARENA_SIZE";
  const ENV_CHUNK_SIZE: &str = "RUNTIME_NATIVE_BUMP_CHUNK_SIZE";

  const DEFAULT_ARENA_SIZE: usize = 4 * 1024 * 1024 * 1024;
  const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

  /// Minimum alignment for chunk reservations / global bump increments.
  const CHUNK_ALIGN: usize = 16;

  #[inline]
  fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value
      .checked_add(align - 1)
      .map(|v| v & !(align - 1))
  }

  fn parse_env_size(name: &str) -> Option<usize> {
    let raw = std::env::var(name).ok()?;
    let s = raw.trim();
    if s.is_empty() {
      return None;
    }

    let (num, mul) = match s.as_bytes().last().copied() {
      Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1024usize),
      Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1024usize * 1024),
      Some(b'G') | Some(b'g') => (&s[..s.len() - 1], 1024usize * 1024 * 1024),
      _ => (s, 1usize),
    };

    let n: usize = num.parse().ok()?;
    n.checked_mul(mul)
  }

  struct Arena {
    base: usize,
    size: usize,
    chunk_size: usize,
    bump: AtomicUsize,
  }

  impl Arena {
    fn new() -> Self {
      let chunk_size = parse_env_size(ENV_CHUNK_SIZE).unwrap_or(DEFAULT_CHUNK_SIZE);
      let chunk_size = align_up(chunk_size.max(CHUNK_ALIGN), CHUNK_ALIGN)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("chunk size overflow"));

      let arena_env = parse_env_size(ENV_ARENA_SIZE);
      let mut arena_size = arena_env.unwrap_or(DEFAULT_ARENA_SIZE);
      arena_size = align_up(arena_size.max(chunk_size).max(CHUNK_ALIGN), CHUNK_ALIGN)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("arena size overflow"));

      // `check_runtime_native_abi.sh` runs a C smoke test under a tight `RLIMIT_AS`
      // (virtual memory) cap. When the arena size is not explicitly configured,
      // degrade gracefully by trying smaller arenas before treating it as fatal.
      let mut attempt = arena_size;
      let ptr = loop {
        let ptr = unsafe {
          libc::mmap(
            core::ptr::null_mut(),
            attempt,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
          )
        };
        if ptr != libc::MAP_FAILED && !ptr.is_null() {
          arena_size = attempt;
          break ptr;
        }

        if arena_env.is_some() {
          trap::rt_trap_oom(attempt, "mmap bump arena");
        }

        // Fall back to a smaller reservation.
        let next = attempt / 2;
        let next = align_up(next.max(chunk_size).max(CHUNK_ALIGN), CHUNK_ALIGN)
          .unwrap_or_else(|| trap::rt_trap_invalid_arg("arena size overflow"));
        if next >= attempt {
          trap::rt_trap_oom(attempt, "mmap bump arena");
        }
        attempt = next;
      };

      Self {
        base: ptr as usize,
        size: arena_size,
        chunk_size,
        bump: AtomicUsize::new(0),
      }
    }

    #[inline]
    fn reserve(&self, bytes: usize, context: &str) -> (usize, usize) {
      debug_assert!(bytes > 0);
      let bytes = align_up(bytes, CHUNK_ALIGN)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

      let start_off = self.bump.fetch_add(bytes, Ordering::Relaxed);
      let end_off = start_off
        .checked_add(bytes)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

      if end_off > self.size {
        trap::rt_trap_oom(bytes, context);
      }

      (self.base + start_off, self.base + end_off)
    }
  }

  static ARENA: Lazy<Arena> = Lazy::new(Arena::new);

  #[derive(Clone, Copy, Debug)]
  struct LocalBump {
    cur: usize,
    end: usize,
  }

  impl LocalBump {
    const fn empty() -> Self {
      Self { cur: 0, end: 0 }
    }

    #[inline]
    fn try_alloc(&mut self, size: usize, align: usize) -> Option<usize> {
      if self.cur == 0 {
        return None;
      }

      let start = align_up(self.cur, align)?;
      let end = start.checked_add(size)?;
      if end <= self.end {
        self.cur = end;
        Some(start)
      } else {
        None
      }
    }

    #[inline]
    fn refill(&mut self, min_bytes: usize, align: usize, context: &str) {
      let needed = min_bytes
        .checked_add(align - 1)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

      let bytes = ARENA.chunk_size.max(needed);
      let (start, end) = ARENA.reserve(bytes, context);
      self.cur = start;
      self.end = end;
    }
  }

  thread_local! {
    static LOCAL_BUMP: Cell<LocalBump> = Cell::new(LocalBump::empty());
  }

  #[inline]
  pub(super) fn alloc_bytes(size: usize, align: usize, context: &str) -> *mut u8 {
    if size == 0 {
      return NonNull::<u8>::dangling().as_ptr();
    }
    let align = align.max(1);
    if !align.is_power_of_two() {
      trap::rt_trap_invalid_arg("allocation alignment must be a power of two");
    }

    let size = align_up(size, align).unwrap_or_else(|| trap::rt_trap_invalid_arg("allocation size overflow"));

    LOCAL_BUMP.with(|cell| {
      let mut local = cell.get();

      if let Some(ptr) = local.try_alloc(size, align) {
        cell.set(local);
        return ptr as *mut u8;
      }

      local.refill(size, align, context);
      let ptr = local
        .try_alloc(size, align)
        .expect("refill must provide enough space for allocation");
      cell.set(local);
      ptr as *mut u8
    })
  }

  #[inline]
  pub(super) fn alloc_bytes_zeroed(size: usize, align: usize, context: &str) -> *mut u8 {
    alloc_bytes(size, align, context)
  }
}

#[cfg(not(target_os = "linux"))]
mod imp {
  use core::ptr::NonNull;

  use crate::trap;

  #[inline]
  pub(super) fn alloc_bytes(size: usize, align: usize, context: &str) -> *mut u8 {
    if size == 0 {
      return NonNull::<u8>::dangling().as_ptr();
    }
    let align = align.max(1);
    if !align.is_power_of_two() {
      trap::rt_trap_invalid_arg("allocation alignment must be a power of two");
    }

    let layout =
      std::alloc::Layout::from_size_align(size, align).unwrap_or_else(|_| trap::rt_trap_invalid_arg("invalid layout"));
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
      trap::rt_trap_oom(size, context);
    }
    ptr
  }

  #[inline]
  pub(super) fn alloc_bytes_zeroed(size: usize, align: usize, context: &str) -> *mut u8 {
    if size == 0 {
      return NonNull::<u8>::dangling().as_ptr();
    }
    let align = align.max(1);
    if !align.is_power_of_two() {
      trap::rt_trap_invalid_arg("allocation alignment must be a power of two");
    }

    let layout =
      std::alloc::Layout::from_size_align(size, align).unwrap_or_else(|_| trap::rt_trap_invalid_arg("invalid layout"));
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
      trap::rt_trap_oom(size, context);
    }
    ptr
  }
}

/// Allocate `size` bytes with the requested alignment.
///
/// Notes:
/// - The allocator is leak-only for now (no frees).
/// - `align` must be a power of two. `align = 1` is allowed.
pub(crate) fn alloc_bytes(size: usize, align: usize, context: &str) -> *mut u8 {
  imp::alloc_bytes(size, align, context)
}

/// Allocate `size` bytes with the requested alignment and return zero-initialized memory.
///
/// Notes:
/// - The allocator is leak-only for now (no frees).
/// - `align` must be a power of two. `align = 1` is allowed.
pub(crate) fn alloc_bytes_zeroed(size: usize, align: usize, context: &str) -> *mut u8 {
  if size == 0 {
    return NonNull::<u8>::dangling().as_ptr();
  }
  imp::alloc_bytes_zeroed(size, align, context)
}
