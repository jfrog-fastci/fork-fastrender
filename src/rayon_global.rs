use rayon::ThreadPoolBuilder;
use std::sync::OnceLock;

const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";

fn parse_rayon_num_threads(raw: Option<&str>) -> Option<usize> {
  let raw = raw?.trim();
  if raw.is_empty() {
    return None;
  }
  raw.parse::<usize>().ok().filter(|threads| *threads > 0)
}

fn capped_global_pool_threads(cpu_budget: usize) -> usize {
  cpu_budget
    .max(1)
    .min(crate::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
    .max(1)
}

fn desired_global_pool_threads(cpu_budget: usize, env_value: Option<&str>) -> usize {
  parse_rayon_num_threads(env_value).unwrap_or_else(|| capped_global_pool_threads(cpu_budget))
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
/// To keep FastRender's public API panic-free by default, we eagerly initialise the global pool.
///
/// When `RAYON_NUM_THREADS` is set to a valid positive integer we honor it; otherwise we cap the
/// pool size using [`crate::system::cpu_budget`] and
/// [`crate::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS`].
pub(crate) fn ensure_global_pool() -> Result<(), String> {
  GLOBAL_POOL_STATUS
    .get_or_init(|| {
      // Match the default parallelism cap used by auto layout fan-out. This avoids large fan-out
      // on hosts where `available_parallelism()` sees dozens/hundreds of CPUs.
      let cpu_budget = crate::system::cpu_budget().max(1);
      let env_value = std::env::var(RAYON_NUM_THREADS_ENV).ok();
      let mut threads = desired_global_pool_threads(cpu_budget, env_value.as_deref()).max(1);

      loop {
        match ThreadPoolBuilder::new()
          .num_threads(threads)
          .thread_name(|idx| format!("fastr-rayon-{idx}"))
          .build_global()
        {
          Ok(()) => return Ok(()),
          Err(err) => {
            // `ThreadPoolBuildError` does not currently expose its internal kind publicly. The only
            // non-fatal error case is when another crate has already initialised the global pool.
            //
            // Detect that by checking whether querying the current pool succeeds without panicking.
            let already_initialized =
              std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok();
            if already_initialized {
              return Ok(());
            }

            // If initialization fails due to OS thread-spawn limits (EAGAIN/WouldBlock), retry with
            // fewer threads. If we still cannot initialize a 1-thread pool, surface the failure to
            // the caller so they can fall back to sequential code paths.
            if threads <= 1 {
              return Err(format!("failed to initialize Rayon global thread pool: {err}"));
            }
            threads = (threads / 2).max(1);
          }
        }
      }
    })
    .clone()
}

#[cfg(test)]
mod tests {
  #[test]
  fn parse_rayon_num_threads_rejects_empty_and_invalid() {
    assert_eq!(super::parse_rayon_num_threads(None), None);
    assert_eq!(super::parse_rayon_num_threads(Some("")), None);
    assert_eq!(super::parse_rayon_num_threads(Some("   ")), None);
    assert_eq!(super::parse_rayon_num_threads(Some("0")), None);
    assert_eq!(super::parse_rayon_num_threads(Some("nope")), None);
  }

  #[test]
  fn desired_global_pool_threads_uses_env_override_when_valid() {
    assert_eq!(super::desired_global_pool_threads(8, Some("4")), 4);
    assert_eq!(super::desired_global_pool_threads(1, Some("2")), 2);
    assert_eq!(super::desired_global_pool_threads(32, Some("  7 ")), 7);
  }

  #[test]
  fn desired_global_pool_threads_falls_back_to_capped_cpu_budget() {
    let cap = crate::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS;
    assert_eq!(super::desired_global_pool_threads(1, None), 1);
    assert_eq!(super::desired_global_pool_threads(4, Some("0")), 4);
    assert_eq!(super::desired_global_pool_threads(4, Some("nope")), 4);
    assert_eq!(super::desired_global_pool_threads(cap + 100, None), cap);
  }
}
