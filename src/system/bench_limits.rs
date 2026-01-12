use std::fs::File;
use std::io::{self, Read};
use std::sync::{Once, OnceLock};

#[derive(Debug, Clone, Copy)]
pub struct BenchLimits {
  /// Maximum bytes benches will read from an on-disk fixture.
  pub max_fixture_bytes: usize,
  /// Maximum number of threads benchmarks should use for parallel workloads.
  pub max_threads: usize,
  /// Maximum DOM nodes/HTML elements synthetic generators should create by default.
  pub max_dom_nodes: usize,
  /// Maximum number of display list items a synthetic generator should create by default.
  pub max_display_list_items: usize,
  /// Maximum recursion depth for synthetic tree builders.
  pub max_depth: usize,
}

impl BenchLimits {
  pub fn from_env() -> Self {
    Self {
      max_fixture_bytes: env_byte_limit("FASTR_BENCH_MAX_FIXTURE_BYTES").unwrap_or(8 * 1024 * 1024),
      max_threads: env_usize("FASTR_BENCH_MAX_THREADS")
        .map(|v| v.max(1))
        .unwrap_or(8),
      max_dom_nodes: env_usize("FASTR_BENCH_MAX_DOM_NODES").unwrap_or(100_000),
      max_display_list_items: env_usize("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS").unwrap_or(200_000),
      max_depth: env_usize("FASTR_BENCH_MAX_DEPTH").unwrap_or(256),
    }
  }
}

pub fn bench_limits() -> &'static BenchLimits {
  static LIMITS: OnceLock<BenchLimits> = OnceLock::new();
  LIMITS.get_or_init(BenchLimits::from_env)
}

pub fn bench_verbose() -> bool {
  env_flag("FASTR_BENCH_VERBOSE")
}

pub fn bench_print_config_once(bench_name: &str, extras: &[(&str, String)]) {
  if !bench_verbose() {
    return;
  }
  static PRINTED: Once = Once::new();
  PRINTED.call_once(|| {
    let limits = bench_limits();
    let mut msg = format!(
      "bench safety {bench_name}: max_dom_nodes={} max_display_list_items={} max_fixture_bytes={} max_threads={} max_depth={}",
      limits.max_dom_nodes,
      limits.max_display_list_items,
      limits.max_fixture_bytes,
      limits.max_threads,
      limits.max_depth
    );
    for (key, value) in extras {
      msg.push(' ');
      msg.push_str(key);
      msg.push('=');
      msg.push_str(value);
    }
    eprintln!("{msg}");
  });
}

/// Read a fixture file, failing if it exceeds `max_bytes`.
pub fn read_fixture_bytes_skip(
  path: impl AsRef<std::path::Path>,
  max_bytes: usize,
) -> io::Result<Vec<u8>> {
  let max_plus_one = max_bytes.saturating_add(1);
  let file = File::open(path.as_ref())?;
  let mut buf = Vec::new();
  file.take(max_plus_one as u64).read_to_end(&mut buf)?;
  if buf.len() > max_bytes {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      format!(
        "fixture {} exceeds FASTR_BENCH_MAX_FIXTURE_BYTES ({max_bytes} bytes)",
        path.as_ref().display()
      ),
    ));
  }
  Ok(buf)
}

/// Read up to `max_bytes` from a fixture file, truncating deterministically.
pub fn read_fixture_bytes_truncate(
  path: impl AsRef<std::path::Path>,
  max_bytes: usize,
) -> io::Result<Vec<u8>> {
  let file = File::open(path.as_ref())?;
  let mut buf = Vec::new();
  file.take(max_bytes as u64).read_to_end(&mut buf)?;
  Ok(buf)
}

pub fn env_flag(name: &str) -> bool {
  std::env::var(name)
    .ok()
    .map(|value| {
      let trimmed = value.trim();
      !(trimmed.is_empty()
        || trimmed == "0"
        || trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no"))
    })
    .unwrap_or(false)
}

pub fn env_usize(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let cleaned: String = trimmed.chars().filter(|ch| *ch != '_').collect();
  cleaned.parse().ok()
}

pub fn env_byte_limit(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  parse_byte_size(raw.trim())
}

pub fn parse_byte_size(raw: &str) -> Option<usize> {
  if raw.is_empty() {
    return None;
  }
  let s = raw.trim().to_ascii_lowercase();
  let unit_start = s
    .find(|c: char| c.is_ascii_alphabetic())
    .unwrap_or_else(|| s.len());
  let (num, unit) = s.split_at(unit_start);
  let cleaned: String = num.chars().filter(|ch| *ch != '_').collect();
  let value: u64 = cleaned.parse().ok()?;
  let factor: u64 = match unit {
    "" | "b" => 1,
    "k" | "kb" | "kib" => 1024,
    "m" | "mb" | "mib" => 1024 * 1024,
    "g" | "gb" | "gib" => 1024 * 1024 * 1024,
    "t" | "tb" | "tib" => 1024_u64.pow(4),
    _ => return None,
  };
  let bytes = value.checked_mul(factor)?;
  usize::try_from(bytes).ok()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::OsString;
  use std::sync::{Mutex, MutexGuard};

  fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  struct EnvGuard {
    name: &'static str,
    prev: Option<OsString>,
  }

  impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
      let prev = std::env::var_os(name);
      std::env::set_var(name, value);
      Self { name, prev }
    }
  }

  impl Drop for EnvGuard {
    fn drop(&mut self) {
      if let Some(value) = self.prev.take() {
        std::env::set_var(self.name, value);
      } else {
        std::env::remove_var(self.name);
      }
    }
  }

  #[test]
  fn bench_limits_parse_env_and_apply_defaults() {
    let _lock = test_lock();

    let _verbose = EnvGuard::set("FASTR_BENCH_VERBOSE", "1");
    assert!(bench_verbose());

    let _max_threads = EnvGuard::set("FASTR_BENCH_MAX_THREADS", "0");
    let _max_dom = EnvGuard::set("FASTR_BENCH_MAX_DOM_NODES", "10_000");
    let _max_items = EnvGuard::set("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS", "2000");
    let _max_depth = EnvGuard::set("FASTR_BENCH_MAX_DEPTH", "64");
    let _max_fixture = EnvGuard::set("FASTR_BENCH_MAX_FIXTURE_BYTES", "1MiB");

    let limits = BenchLimits::from_env();
    assert_eq!(
      limits.max_threads, 1,
      "max_threads should clamp to at least 1"
    );
    assert_eq!(limits.max_dom_nodes, 10_000);
    assert_eq!(limits.max_display_list_items, 2000);
    assert_eq!(limits.max_depth, 64);
    assert_eq!(limits.max_fixture_bytes, 1024 * 1024);

    // Invalid values fall back to defaults.
    let _invalid_fixture = EnvGuard::set("FASTR_BENCH_MAX_FIXTURE_BYTES", "nope");
    let limits = BenchLimits::from_env();
    assert_eq!(limits.max_fixture_bytes, 8 * 1024 * 1024);
  }
}
