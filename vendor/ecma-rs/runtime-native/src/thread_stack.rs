#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackBounds {
  pub low: usize,
  pub high: usize,
}

impl StackBounds {
  pub fn new(low: usize, high: usize) -> Result<Self, Error> {
    if low >= high {
      return Err(Error::InvalidStackBounds { low, high });
    }
    Ok(Self { low, high })
  }

  #[inline]
  pub fn contains(&self, addr: usize) -> bool {
    addr >= self.low && addr < self.high
  }

  #[inline]
  pub fn contains_range(&self, addr: usize, len: usize) -> bool {
    if addr < self.low {
      return false;
    }
    let Some(end) = addr.checked_add(len) else {
      return false;
    };
    end <= self.high
  }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error("unsupported target for stack bounds discovery: os={os} arch={arch}")]
  UnsupportedTarget { os: &'static str, arch: &'static str },

  #[error("{func} failed with error code {code}")]
  Pthread { func: &'static str, code: i32 },

  #[error("stack bounds arithmetic overflow: low={low:#x} size={size:#x}")]
  StackBoundsOverflow { low: usize, size: usize },

  #[error("stack bounds arithmetic underflow: high={high:#x} size={size:#x}")]
  StackBoundsUnderflow { high: usize, size: usize },

  #[error("invalid stack bounds (low >= high): low={low:#x} high={high:#x}")]
  InvalidStackBounds { low: usize, high: usize },
}

/// Returns the stack bounds for the current OS thread.
///
/// The returned range is half-open: `[low, high)`.
pub fn current_thread_stack_bounds() -> Result<StackBounds, Error> {
  current_thread_stack_bounds_impl()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_thread_stack_bounds_impl() -> Result<StackBounds, Error> {
  // glibc/bionic extension.
  //
  // `pthread_getattr_np` fills in a `pthread_attr_t` for the running thread.
  // `pthread_attr_getstack` returns:
  //   - stackaddr: low address of the stack allocation
  //   - stacksize: size in bytes
  // Stack range: [stackaddr, stackaddr + stacksize)
  unsafe {
    let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    let rc = libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr());
    if rc != 0 {
      return Err(Error::Pthread {
        func: "pthread_getattr_np",
        code: rc,
      });
    }
    let mut attr = attr.assume_init();

    let mut stack_addr: *mut libc::c_void = std::ptr::null_mut();
    let mut stack_size: usize = 0;
    let rc = libc::pthread_attr_getstack(&attr, &mut stack_addr, &mut stack_size);

    // On Linux/Android, pthread stacks typically have a guard region at the low
    // end of the allocation that is mapped as PROT_NONE. Excluding it from the
    // returned bounds prevents stack walkers from accidentally reading into the
    // guard page (e.g. if the FP chain is corrupted).
    let mut guard_size: usize = 0;
    let rc_guard = libc::pthread_attr_getguardsize(&attr, &mut guard_size);

    // Always destroy the attr to avoid leaking resources, even if getstack failed.
    let rc_destroy = libc::pthread_attr_destroy(&mut attr);
    if rc != 0 {
      return Err(Error::Pthread {
        func: "pthread_attr_getstack",
        code: rc,
      });
    }
    if rc_guard != 0 {
      return Err(Error::Pthread {
        func: "pthread_attr_getguardsize",
        code: rc_guard,
      });
    }
    if rc_destroy != 0 {
      return Err(Error::Pthread {
        func: "pthread_attr_destroy",
        code: rc_destroy,
      });
    }

    let mut low = stack_addr as usize;
    let high = low
      .checked_add(stack_size)
      .ok_or(Error::StackBoundsOverflow {
        low,
        size: stack_size,
      })?;

    if guard_size != 0 {
      low = low
        .checked_add(guard_size)
        .ok_or(Error::StackBoundsOverflow {
          low,
          size: guard_size,
        })?;
    }

    StackBounds::new(low, high)
  }
}

#[cfg(target_os = "macos")]
fn current_thread_stack_bounds_impl() -> Result<StackBounds, Error> {
  // macOS provides non-portable helpers:
  // - pthread_get_stackaddr_np returns the *high* address (top of stack).
  // - pthread_get_stacksize_np returns the size in bytes.
  //
  // Stack range: [high - size, high)
  unsafe {
    let thread = libc::pthread_self();
    let high = libc::pthread_get_stackaddr_np(thread) as usize;
    let size = libc::pthread_get_stacksize_np(thread) as usize;
    let low = high
      .checked_sub(size)
      .ok_or(Error::StackBoundsUnderflow { high, size })?;
    StackBounds::new(low, high)
  }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn current_thread_stack_bounds_impl() -> Result<StackBounds, Error> {
  Err(Error::UnsupportedTarget {
    os: std::env::consts::OS,
    arch: std::env::consts::ARCH,
  })
}
