#[inline]
pub(crate) fn abort_on_panic<T>(f: impl FnOnce() -> T) -> T {
  #[cfg(panic = "unwind")]
  {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
      Ok(value) => value,
      Err(_) => std::process::abort(),
    }
  }

  #[cfg(not(panic = "unwind"))]
  {
    f()
  }
}

