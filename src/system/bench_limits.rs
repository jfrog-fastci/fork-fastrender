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
    let max_fixture_bytes = std::env::var("FASTR_BENCH_MAX_FIXTURE_BYTES").ok();
    let max_threads = std::env::var("FASTR_BENCH_MAX_THREADS").ok();
    let max_dom_nodes = std::env::var("FASTR_BENCH_MAX_DOM_NODES").ok();
    let max_display_list_items = std::env::var("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS").ok();
    let max_depth = std::env::var("FASTR_BENCH_MAX_DEPTH").ok();

    Self::from_lookup(|name| match name {
      "FASTR_BENCH_MAX_FIXTURE_BYTES" => max_fixture_bytes.as_deref(),
      "FASTR_BENCH_MAX_THREADS" => max_threads.as_deref(),
      "FASTR_BENCH_MAX_DOM_NODES" => max_dom_nodes.as_deref(),
      "FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS" => max_display_list_items.as_deref(),
      "FASTR_BENCH_MAX_DEPTH" => max_depth.as_deref(),
      _ => None,
    })
  }

  pub fn from_lookup<'a>(mut get: impl FnMut(&str) -> Option<&'a str>) -> Self {
    Self {
      max_fixture_bytes: lookup_byte_limit(&mut get, "FASTR_BENCH_MAX_FIXTURE_BYTES")
        .unwrap_or(8 * 1024 * 1024),
      max_threads: lookup_usize(&mut get, "FASTR_BENCH_MAX_THREADS")
        .map(|v| v.max(1))
        .unwrap_or(8),
      max_dom_nodes: lookup_usize(&mut get, "FASTR_BENCH_MAX_DOM_NODES").unwrap_or(100_000),
      max_display_list_items: lookup_usize(&mut get, "FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS")
        .unwrap_or(200_000),
      max_depth: lookup_usize(&mut get, "FASTR_BENCH_MAX_DEPTH").unwrap_or(256),
    }
  }
}

pub fn bench_limits() -> &'static BenchLimits {
  static LIMITS: OnceLock<BenchLimits> = OnceLock::new();
  LIMITS.get_or_init(BenchLimits::from_env)
}

pub fn bench_verbose() -> bool {
  let verbose = std::env::var("FASTR_BENCH_VERBOSE").ok();
  bench_verbose_from_lookup(|name| match name {
    "FASTR_BENCH_VERBOSE" => verbose.as_deref(),
    _ => None,
  })
}

pub fn bench_verbose_from_lookup<'a>(mut get: impl FnMut(&str) -> Option<&'a str>) -> bool {
  lookup_flag(&mut get, "FASTR_BENCH_VERBOSE")
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
    .map(|value| parse_flag_value(&value))
    .unwrap_or(false)
}

pub fn env_usize(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  parse_usize_value(&raw)
}

pub fn env_byte_limit(name: &str) -> Option<usize> {
  let raw = std::env::var(name).ok()?;
  parse_byte_size(raw.trim())
}

fn lookup_flag<'a>(get: &mut impl FnMut(&str) -> Option<&'a str>, name: &str) -> bool {
  get(name)
    .map(|value| parse_flag_value(value))
    .unwrap_or(false)
}

fn lookup_usize<'a>(get: &mut impl FnMut(&str) -> Option<&'a str>, name: &str) -> Option<usize> {
  let raw = get(name)?;
  parse_usize_value(raw)
}

fn lookup_byte_limit<'a>(
  get: &mut impl FnMut(&str) -> Option<&'a str>,
  name: &str,
) -> Option<usize> {
  let raw = get(name)?;
  parse_byte_size(raw.trim())
}

fn parse_flag_value(raw: &str) -> bool {
  let trimmed = raw.trim();
  !(trimmed.is_empty()
    || trimmed == "0"
    || trimmed.eq_ignore_ascii_case("false")
    || trimmed.eq_ignore_ascii_case("no"))
}

fn parse_usize_value(raw: &str) -> Option<usize> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let cleaned: String = trimmed.chars().filter(|ch| *ch != '_').collect();
  cleaned.parse().ok()
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
  use std::collections::HashMap;

  #[test]
  fn bench_limits_parse_env_and_apply_defaults() {
    let env = HashMap::from([
      ("FASTR_BENCH_VERBOSE", "1"),
      ("FASTR_BENCH_MAX_THREADS", "0"),
      ("FASTR_BENCH_MAX_DOM_NODES", "10_000"),
      ("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS", "2000"),
      ("FASTR_BENCH_MAX_DEPTH", "64"),
      ("FASTR_BENCH_MAX_FIXTURE_BYTES", "1MiB"),
    ]);

    assert!(bench_verbose_from_lookup(|name| env.get(name).copied()));

    let limits = BenchLimits::from_lookup(|name| env.get(name).copied());
    assert_eq!(
      limits.max_threads, 1,
      "max_threads should clamp to at least 1"
    );
    assert_eq!(limits.max_dom_nodes, 10_000);
    assert_eq!(limits.max_display_list_items, 2000);
    assert_eq!(limits.max_depth, 64);
    assert_eq!(limits.max_fixture_bytes, 1024 * 1024);

    // Invalid values fall back to defaults.
    let env = HashMap::from([
      ("FASTR_BENCH_MAX_THREADS", "0"),
      ("FASTR_BENCH_MAX_DOM_NODES", "10_000"),
      ("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS", "2000"),
      ("FASTR_BENCH_MAX_DEPTH", "64"),
      ("FASTR_BENCH_MAX_FIXTURE_BYTES", "nope"),
    ]);
    let limits = BenchLimits::from_lookup(|name| env.get(name).copied());
    assert_eq!(limits.max_fixture_bytes, 8 * 1024 * 1024);
  }
}
