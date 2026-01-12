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
            Err(format!(
              "failed to initialize Rayon global thread pool: {err}"
            ))
          }
        }
      }
    })
    .clone()
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::sync::{Mutex, MutexGuard, OnceLock};

  fn env_var_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
  }

  impl EnvVarGuard {
    fn unset(key: &'static str) -> Self {
      let previous = std::env::var_os(key);
      std::env::remove_var(key);
      Self { key, previous }
    }
  }

  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      match self.previous.take() {
        Some(value) => std::env::set_var(self.key, value),
        None => std::env::remove_var(self.key),
      }
    }
  }

  fn expected_rayon_threads() -> usize {
    crate::system::cpu_budget()
      .max(1)
      .min(crate::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
      .max(1)
  }

  #[test]
  fn rayon_global_pool_is_capped_when_env_unset() {
    let _lock = env_var_test_lock();
    let _guard = EnvVarGuard::unset(super::RAYON_NUM_THREADS_ENV);

    crate::rayon_global::ensure_global_pool()
      .expect("Rayon global pool should initialise when env var is unset");

    assert_eq!(rayon::current_num_threads(), expected_rayon_threads());
  }

  #[test]
  fn rayon_global_pool_is_capped_for_renderer_pool() {
    let _lock = env_var_test_lock();
    let _guard = EnvVarGuard::unset(super::RAYON_NUM_THREADS_ENV);

    let pool = crate::FastRenderPool::new().expect("pool should build");
    pool
      .with_renderer(|_| Ok(()))
      .expect("pool should build a renderer");

    assert_eq!(rayon::current_num_threads(), expected_rayon_threads());
  }
}
