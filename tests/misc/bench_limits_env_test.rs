#[path = "../../benches/common.rs"]
#[allow(dead_code)]
mod bench_common;

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

  assert!(bench_common::bench_verbose_from_lookup(|name| env.get(name).copied()));

  let limits = bench_common::BenchLimits::from_lookup(|name| env.get(name).copied());
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
  let limits = bench_common::BenchLimits::from_lookup(|name| env.get(name).copied());
  assert_eq!(limits.max_fixture_bytes, 8 * 1024 * 1024);
}
