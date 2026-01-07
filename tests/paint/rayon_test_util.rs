use std::sync::Once;

/// Initialize the Rayon global pool with a conservative thread count for tests.
///
/// Many CI runners report very high CPU counts, but run tests under strict thread/address-space
/// limits (e.g. `scripts/run_limited.sh`). Rayon defaults to spawning one worker per CPU when the
/// global pool is first used, which can fail with `EAGAIN` and panic. Pre-initializing the global
/// pool keeps paint regression tests stable under those constraints.
pub fn init_rayon_for_tests(num_threads: usize) {
  static INIT: Once = Once::new();
  let num_threads = num_threads.max(1);

  INIT.call_once(|| {
    // Set the env var too so any incidental global-pool initialization (inside dependencies) uses
    // the same cap.
    if !std::env::var_os("RAYON_NUM_THREADS").is_some_and(|value| !value.is_empty()) {
      std::env::set_var("RAYON_NUM_THREADS", num_threads.to_string());
    }
    if let Err(err) = rayon::ThreadPoolBuilder::new()
      .num_threads(num_threads)
      .build_global()
    {
      // `ThreadPoolBuildError` does not expose its underlying kind publicly; the only non-fatal
      // failure is when another test has already initialized the global pool.
      //
      // Detect that case by checking whether querying the pool succeeds without panicking.
      let already_initialized = std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok();
      if !already_initialized {
        panic!("failed to initialize Rayon global pool for tests: {err}");
      }
    }
  });
}
