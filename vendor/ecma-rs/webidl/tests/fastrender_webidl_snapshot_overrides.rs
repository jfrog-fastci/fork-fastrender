use std::path::PathBuf;

fn repo_root() -> PathBuf {
  // `CARGO_MANIFEST_DIR` points at `vendor/ecma-rs/webidl`. Walk up to the workspace root so this
  // test can validate FastRender's committed WebIDL snapshot.
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

#[test]
fn fastrender_webidl_snapshot_includes_observer_overrides() {
  let snapshot_path = repo_root().join("src/webidl/generated/mod.rs");
  let snapshot = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|err| {
    panic!(
      "failed to read WebIDL snapshot at {}: {err}",
      snapshot_path.display()
    )
  });

  // These interfaces live in separate specs (Intersection Observer / Resize Observer) and are
  // provided in FastRender via `tools/webidl/overrides/*.idl`. Keep a regression test here so
  // snapshot regeneration continues to include them.
  for name in [
    "IntersectionObserver",
    "IntersectionObserverEntry",
    "IntersectionObserverInit",
    "IntersectionObserverCallback",
    "ResizeObserver",
    "ResizeObserverEntry",
    "ResizeObserverSize",
    "ResizeObserverOptions",
    "ResizeObserverBoxOptions",
    "ResizeObserverCallback",
  ] {
    let needle = format!("name: \"{name}\"");
    assert!(
      snapshot.contains(&needle),
      "expected WebIDL snapshot to contain {needle}"
    );
  }
}

