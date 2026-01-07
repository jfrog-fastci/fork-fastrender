//! Rayon global thread-pool initialization helpers.
//!
//! FastRender uses Rayon throughout layout/paint. Rayon will lazily initialize its global thread
//! pool on first use (e.g. `rayon::current_num_threads()` or a parallel iterator). In CI / sandboxed
//! environments the detected CPU count can be very large while address-space limits are tight,
//! causing the default global pool initialization (one worker per CPU) to fail with
//! `Resource temporarily unavailable`.
//!
//! To keep the library reliable under the repo's required resource caps (`scripts/run_limited.sh`)
//! we proactively initialize the global pool with a conservative thread count when the caller
//! hasn't explicitly set `RAYON_NUM_THREADS`.

use std::sync::OnceLock;

const DEFAULT_GLOBAL_RAYON_MAX_THREADS: usize = 16;

fn desired_global_rayon_threads() -> usize {
  if let Ok(raw) = std::env::var("RAYON_NUM_THREADS") {
    if let Ok(threads) = raw.parse::<usize>() {
      if threads > 0 {
        return threads;
      }
    }
  }

  crate::system::cpu_budget()
    .min(DEFAULT_GLOBAL_RAYON_MAX_THREADS)
    .max(1)
}

pub(crate) fn ensure_global_rayon_pool() {
  static INIT: OnceLock<()> = OnceLock::new();
  INIT.get_or_init(|| {
    let threads = desired_global_rayon_threads();
    // Ignore the already-initialized case (callers may have explicitly configured Rayon).
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(threads)
      .build_global();
  });
}

