#![cfg(target_os = "macos")]

use fastrender::api::{FastRender, FastRenderConfig};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::sandbox;
use fastrender::text::font_db::FontConfig;
use fastrender::Rgba;
use std::process::Command;
use std::sync::Arc;

#[derive(Debug)]
struct NoNetworkFetcher;

impl ResourceFetcher for NoNetworkFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    panic!("sandbox smoke test attempted unexpected fetch for {url}");
  }
}

#[test]
fn sandboxed_render_smoke_seatbelt_profile() {
  const CHILD_ENV: &str = "FASTR_TEST_SEATBELT_RENDER_SMOKE_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    // Apply the strictest built-in profile (`pure-computation`) so this smoke test fails if the
    // renderer starts depending on filesystem/network access inside the sandbox.
    sandbox::apply_pure_computation_sandbox().expect("apply Seatbelt pure-computation sandbox");

    let mut config = FastRenderConfig::new();
    config.background_color = Rgba::WHITE;
    config.font_config = FontConfig::bundled_only();
    // Avoid any optional caches even when `--all-features` enables disk_cache.
    config.resource_cache = None;

    let mut renderer =
      FastRender::with_config_and_fetcher(config, Some(Arc::new(NoNetworkFetcher)))
        .expect("build FastRender under Seatbelt");

    let pixmap = renderer
      .render_html(
        "<html><body style=\"margin:0\">Hello</body></html>",
        64,
        32,
      )
      .expect("render HTML under Seatbelt");

    let data = pixmap.data();
    assert!(!data.is_empty(), "expected non-empty image buffer");
    assert!(
      data.iter().any(|b| *b != 0),
      "expected rendered image to contain non-zero bytes"
    );

    // Background defaults to white; ensure some non-white pixels were painted so we know text
    // rendering executed inside the sandbox.
    let mut has_non_white = false;
    for px in data.chunks_exact(4) {
      if px[0] != 255 || px[1] != 255 || px[2] != 255 {
        has_non_white = true;
        break;
      }
    }
    assert!(has_non_white, "expected rendered output to contain some non-white pixels");
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandboxed_render_smoke_seatbelt_profile";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Keep test harness output deterministic under strict sandboxing.
    .arg("--test-threads=1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child test process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
