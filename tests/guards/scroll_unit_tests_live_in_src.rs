//! Guard to keep scroll unit tests out of `tests/scroll/`.
//!
//! `tests/scroll/` is reserved for integration tests that exercise the public `FastRender` API.
//! Unit-style tests that import internal modules must live under `src/` (e.g. `src/scroll/tests/**`)
//! so they run under `cargo test --lib` and don't create extra integration-test-only coupling.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn scroll_unit_tests_do_not_live_under_tests_scroll() {
  let root = repo_root();

  let forbidden = [
    root.join("tests/scroll/overflow_clipping_test.rs"),
    root.join("tests/scroll/offset_translates_promoted_fragments_test.rs"),
  ];

  let mut offenders = Vec::new();
  for path in forbidden {
    if path.exists() {
      offenders.push(path.strip_prefix(&root).unwrap_or(&path).display().to_string());
    }
  }

  assert!(
    offenders.is_empty(),
    "scroll unit tests must live under src/ (run via `cargo test --lib`), not tests/scroll:\n{}",
    offenders.join("\n")
  );
}

