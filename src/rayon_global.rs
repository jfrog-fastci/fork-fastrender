use rayon::ThreadPoolBuilder;
use std::sync::OnceLock;

const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";

fn env_var_is_nonempty(key: &str) -> bool {
  std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

static GLOBAL_POOL_STATUS: OnceLock<Result<(), String>> = OnceLock::new();

/// Ensure the Rayon global thread pool is initialised with a conservative default.
///
/// Rayon initialises its global pool lazily on first use. In constrained environments (CI runners,
/// containers with PID limits, etc) `std::thread::available_parallelism()` can report very high
/// core counts while the process cannot spawn that many worker threads, causing the first Rayon
/// call to panic with:
///
/// `ThreadPoolBuildError { kind: IOError(.. WouldBlock ..) }`
///
/// To keep FastRender's public API panic-free by default, we eagerly initialise the global pool
/// with a bounded thread count whenever `RAYON_NUM_THREADS` is not set.
pub(crate) fn ensure_global_pool() -> Result<(), String> {
  GLOBAL_POOL_STATUS
    .get_or_init(|| {
      if env_var_is_nonempty(RAYON_NUM_THREADS_ENV) {
        return Ok(());
      }

      // Match the default parallelism cap used by auto layout fan-out. This avoids large fan-out
      // on hosts where `available_parallelism()` sees dozens/hundreds of CPUs.
      let threads = crate::system::cpu_budget()
        .max(1)
        .min(crate::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
        .max(1);

      match ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|idx| format!("fastr-rayon-{idx}"))
        .build_global()
      {
        Ok(()) => Ok(()),
        Err(err) => {
          // `ThreadPoolBuildError` does not currently expose its internal kind publicly. The only
          // non-fatal error case is when another crate has already initialised the global pool.
          // Detect that by checking whether querying the current pool succeeds without panicking.
          let already_initialized =
            std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok();
          if already_initialized {
            Ok(())
          } else {
            Err(format!("failed to initialize Rayon global thread pool: {err}"))
          }
        }
      }
    })
    .clone()
}
