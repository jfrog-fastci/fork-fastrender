use std::sync::Once;

/// Initialize the Rayon global pool with a conservative thread count for tests.
///
/// Many CI runners report very high CPU counts, but run tests under strict thread/address-space
/// limits (e.g. `scripts/run_limited.sh`). Rayon defaults to spawning one worker per CPU when the
/// global pool is first used, which can fail with `EAGAIN` and panic. Pre-initializing the global
/// pool keeps paint regression tests stable under those constraints.
pub(crate) fn init_rayon_for_tests(num_threads: usize) {
  static INIT: Once = Once::new();
  let num_threads = num_threads.max(1);

  INIT.call_once(|| {
    // Do not mutate process environment variables here.
    //
    // The Rust test harness runs tests in parallel by default, and `std::env::set_var` is not
    // thread-safe with concurrent `std::env::*` access. Setting `RAYON_NUM_THREADS` could race with
    // other tests (or FastRender initialization) that read environment variables, leading to flaky
    // behavior.
    let mut threads = num_threads;
    loop {
      match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
      {
        Ok(()) => break,
        Err(err) => {
          // `ThreadPoolBuildError` does not expose its underlying kind publicly; the only non-fatal
          // failure is when another test has already initialized the global pool.
          //
          // Detect that case by checking whether querying the pool succeeds without panicking.
          let already_initialized =
            std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok();
          if already_initialized {
            break;
          }

           // If initialization fails due to OS thread-spawn limits (EAGAIN/WouldBlock), retry with a
           // smaller pool size. This keeps unit tests stable under constrained CI.
           if threads <= 1 {
            std::panic::panic_any(format!(
              "failed to initialize Rayon global pool for tests: {err}"
            ));
           }
           threads = (threads / 2).max(1);
         }
       }
     }
  });
}
