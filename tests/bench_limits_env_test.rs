#[path = "../benches/common.rs"]
#[allow(dead_code)]
mod bench_common;

use std::ffi::OsString;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
  let _lock = ENV_LOCK.lock().unwrap();

  let _verbose = EnvGuard::set("FASTR_BENCH_VERBOSE", "1");
  assert!(bench_common::bench_verbose());

  let _max_threads = EnvGuard::set("FASTR_BENCH_MAX_THREADS", "0");
  let _max_dom = EnvGuard::set("FASTR_BENCH_MAX_DOM_NODES", "10_000");
  let _max_items = EnvGuard::set("FASTR_BENCH_MAX_DISPLAY_LIST_ITEMS", "2000");
  let _max_depth = EnvGuard::set("FASTR_BENCH_MAX_DEPTH", "64");
  let _max_fixture = EnvGuard::set("FASTR_BENCH_MAX_FIXTURE_BYTES", "1MiB");

  let limits = bench_common::BenchLimits::from_env();
  assert_eq!(limits.max_threads, 1, "max_threads should clamp to at least 1");
  assert_eq!(limits.max_dom_nodes, 10_000);
  assert_eq!(limits.max_display_list_items, 2000);
  assert_eq!(limits.max_depth, 64);
  assert_eq!(limits.max_fixture_bytes, 1024 * 1024);

  // Invalid values fall back to defaults.
  let _invalid_fixture = EnvGuard::set("FASTR_BENCH_MAX_FIXTURE_BYTES", "nope");
  let limits = bench_common::BenchLimits::from_env();
  assert_eq!(limits.max_fixture_bytes, 8 * 1024 * 1024);
}
