/// Run a closure on a dedicated thread with a large stack.
///
/// Some integration tests run very deep recursive layout/paint code and can overflow the default
/// per-thread stack in debug builds. Running them on a larger stack keeps the test harness stable.
pub(crate) fn with_large_stack<F, R>(f: F) -> R
where
  F: FnOnce() -> R + Send + 'static,
  R: Send + 'static,
{
  const STACK_SIZE: usize = 64 * 1024 * 1024;
  let handle = std::thread::Builder::new()
    .stack_size(STACK_SIZE)
    .spawn(f)
    .expect("failed to spawn test thread");
  match handle.join() {
    Ok(result) => result,
    Err(payload) => std::panic::resume_unwind(payload),
  }
}

