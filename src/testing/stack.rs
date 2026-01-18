/// Default stack size used by [`run_with_large_stack`].
pub(crate) const LARGE_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Run `f` on a freshly spawned thread with a custom stack size.
///
/// The main motivation is tests that exercise deep recursion / large stack frames in layout/paint
/// without requiring global `RUST_MIN_STACK` configuration.
pub(crate) fn run_with_stack_size<R>(
  bytes: usize,
  f: impl FnOnce() -> R + Send + 'static,
) -> R
where
  R: Send + 'static,
{
  let handle = std::thread::Builder::new()
    .stack_size(bytes)
    .spawn(f)
    .unwrap_or_else(|e| {
      std::panic::panic_any(format!(
        "failed to spawn test thread with stack size {bytes}: {e}"
      ));
    });

  match handle.join() {
    Ok(value) => value,
    Err(payload) => std::panic::resume_unwind(payload),
  }
}

/// Convenience wrapper for tests that need "a bit more stack" than the default.
pub(crate) fn run_with_large_stack<R>(f: impl FnOnce() -> R + Send + 'static) -> R
where
  R: Send + 'static,
{
  run_with_stack_size(LARGE_STACK_BYTES, f)
}
