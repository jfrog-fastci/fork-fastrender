use lru::LruCache;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::borrow::Cow;
use std::sync::{Arc, LazyLock, Mutex};

const PAINT_THREADS_ENV: &str = "FASTR_PAINT_THREADS";

static PAINT_THREAD_POOLS: LazyLock<Mutex<LruCache<usize, Result<Arc<ThreadPool>, String>>>> =
  LazyLock::new(|| Mutex::new(LruCache::unbounded()));

fn paint_thread_pool_state(threads: usize) -> Result<Arc<ThreadPool>, String> {
  #[cfg(test)]
  let _test_lock = crate::thread_pool_cache::thread_pool_cache_test_lock();

  let threads = crate::thread_pool_cache::clamp_thread_count(threads);
  if threads <= 1 {
    return Err("thread pool thread count must be > 1".to_string());
  }

  let cache_max = crate::thread_pool_cache::thread_pool_cache_max();
  if cache_max > 0 {
    {
      let mut guard = PAINT_THREAD_POOLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = guard.get(&threads) {
        return existing.clone();
      }
    }
  }

  let built = ThreadPoolBuilder::new()
    .num_threads(threads)
    .build()
    .map(Arc::new)
    .map_err(|err| err.to_string());

  if cache_max == 0 {
    return built;
  }

  let mut evicted = Vec::new();
  {
    let mut guard = PAINT_THREAD_POOLS
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.put(threads, built.clone());
    while guard.len() > cache_max {
      if let Some((_key, value)) = guard.pop_lru() {
        evicted.push(value);
      } else {
        break;
      }
    }
  }
  drop(evicted);
  built
}

#[derive(Debug)]
pub(crate) struct PaintPoolSelection {
  /// Thread pool to install before running paint-related Rayon work.
  ///
  /// `None` means we should run Rayon work in the current/global pool.
  pub(crate) pool: Option<Arc<ThreadPool>>,
  /// Thread count available for parallel paint work.
  pub(crate) threads: usize,
  /// If no dedicated pool is selected, describes why.
  pub(crate) dedicated_fallback: Option<Cow<'static, str>>,
}

fn parse_paint_threads_env() -> Result<Option<usize>, String> {
  // Prefer runtime toggles so library callers/tests can override `FASTR_PAINT_THREADS` without
  // mutating the process environment.
  let toggles = crate::debug::runtime::runtime_toggles();
  match toggles.get(PAINT_THREADS_ENV) {
    Some(raw) => {
      let raw = raw.trim();
      if raw.is_empty() {
        return Err(format!("{PAINT_THREADS_ENV} is set but empty"));
      }
      raw
        .parse::<usize>()
        .map(Some)
        .map_err(|_| format!("{PAINT_THREADS_ENV}={raw:?} is not a valid positive integer"))
    }
    None => Ok(None),
  }
}

/// Select the Rayon thread pool that should be used for paint work.
///
/// When `FASTR_PAINT_THREADS` is set to a value greater than 1, a lazily-initialised dedicated
/// thread pool is returned. Otherwise, callers should use the current/global Rayon pool.
pub(crate) fn paint_pool() -> PaintPoolSelection {
  crate::rayon_init::ensure_global_rayon_pool();
  // Rayon may still observe the host CPU count inside cgroup-quotad containers. Clamp the reported
  // pool size by our process CPU budget so default paint fan-out doesn't oversubscribe CI runs.
  let current_threads = rayon::current_num_threads().max(1);
  let current_threads = current_threads.min(crate::system::cpu_budget().max(1));

  match parse_paint_threads_env() {
    Ok(None) => PaintPoolSelection {
      pool: None,
      threads: current_threads,
      dedicated_fallback: Some(Cow::Borrowed(
        "dedicated paint pool disabled (set FASTR_PAINT_THREADS>1 to enable)",
      )),
    },
    Ok(Some(threads)) if threads <= 1 => PaintPoolSelection {
      pool: None,
      threads: current_threads,
      dedicated_fallback: Some(Cow::Owned(format!(
        "dedicated paint pool disabled ({PAINT_THREADS_ENV} must be >1, got {threads})"
      ))),
    },
    Ok(Some(threads)) => {
      let threads = crate::thread_pool_cache::clamp_thread_count(threads);
      if threads <= 1 {
        return PaintPoolSelection {
          pool: None,
          threads: current_threads,
          dedicated_fallback: Some(Cow::Owned(format!(
            "dedicated paint pool disabled ({PAINT_THREADS_ENV} clamped to {threads})"
          ))),
        };
      }
      match paint_thread_pool_state(threads) {
        Ok(pool) => PaintPoolSelection {
          pool: Some(pool),
          threads: threads.max(1),
          dedicated_fallback: None,
        },
        Err(err) => PaintPoolSelection {
          pool: None,
          threads: current_threads,
          dedicated_fallback: Some(Cow::Owned(format!(
            "dedicated paint pool unavailable: {err}"
          ))),
        },
      }
    }
    Err(reason) => PaintPoolSelection {
      pool: None,
      threads: current_threads,
      dedicated_fallback: Some(Cow::Owned(format!(
        "dedicated paint pool disabled ({reason})"
      ))),
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::Arc;

  fn clear_cache() {
    let mut guard = PAINT_THREAD_POOLS
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();
  }

  fn cache_len() -> usize {
    PAINT_THREAD_POOLS
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .len()
  }

  #[test]
  fn paint_thread_pool_cache_is_bounded() {
    let _lock = crate::thread_pool_cache::thread_pool_cache_test_lock();
    clear_cache();

    let mut raw = HashMap::new();
    raw.insert(
      crate::thread_pool_cache::THREAD_POOL_CACHE_MAX_ENV.to_string(),
      "2".to_string(),
    );
    let toggles = Arc::new(RuntimeToggles::from_map(raw));

    with_thread_runtime_toggles(toggles, || {
      for threads in 2..10 {
        let _ = paint_thread_pool_state(threads);
        assert!(
          cache_len() <= 2,
          "paint thread pool cache should stay within configured bound"
        );
      }
    });

    // Avoid leaving dedicated pools alive for the remainder of the test suite.
    clear_cache();
  }
}
