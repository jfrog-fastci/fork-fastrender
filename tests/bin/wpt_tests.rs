//! Standalone WPT runner entry point for scoped CI invocations.
//!
//! This is a `harness = false` Cargo test target so it can accept arbitrary CLI arguments (e.g.
//! `cargo test --test wpt_tests -- layout/floats`) without being interpreted by Rust's built-in
//! libtest argument parser.

#[path = "../wpt/mod.rs"]
mod wpt;

use std::path::{Path, PathBuf};
use std::sync::Once;

use wpt::{DiscoveryMode, HarnessConfig, TestStatus, WptRunner};

static SET_BUNDLED_FONTS: Once = Once::new();

fn ensure_bundled_fonts() {
  SET_BUNDLED_FONTS.call_once(|| {
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
    // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
    // can exceed CI sandbox thread budgets and also makes pixel output nondeterministic. If the
    // caller hasn't pinned the pool size already, clamp it to a deterministic default.
    if std::env::var("RAYON_NUM_THREADS").is_err() {
      std::env::set_var("RAYON_NUM_THREADS", "1");
    }
  });
}

fn create_test_renderer() -> fastrender::FastRender {
  ensure_bundled_fonts();
  fastrender::FastRender::builder()
    .resource_policy(
      fastrender::ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .build()
    .expect("build renderer")
}

fn main() {
  let renderer = create_test_renderer();

  let mut config = HarnessConfig::default();
  config.discovery_mode = DiscoveryMode::ManifestOnly;
  config.expected_dir = std::env::var_os("WPT_EXPECTED_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("tests/wpt/expected"));
  config.update_expected = std::env::var_os("UPDATE_WPT_EXPECTED").is_some();

  let filter = std::env::args().nth(1);
  if let Some(filter) = filter {
    config.filter = Some(filter);
  }

  let mut runner = WptRunner::with_config(renderer, config);
  let results = runner.run_suite(Path::new("tests/wpt/tests"));

  let ran = results
    .iter()
    .filter(|r| r.status != TestStatus::Skip)
    .count();
  if ran == 0 {
    eprintln!("wpt_tests: filter matched no runnable tests");
    std::process::exit(1);
  }

  let mut failed = false;
  for result in &results {
    if result.status.is_failure() {
      failed = true;
      eprintln!("{} failed with status {:?}", result.metadata.id, result.status);
      if let Some(msg) = result.message.as_deref() {
        eprintln!("  {msg}");
      }
    }
  }

  if failed {
    std::process::exit(1);
  }
}

