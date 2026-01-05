mod r#ref;

use fastrender::debug::runtime::RuntimeToggles;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::{FastRender, RenderOptions};
use r#ref::image_compare::{compare_config_from_env, compare_pngs, CompareEnvVars};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/fixtures")
}

fn golden_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/golden")
}

fn diff_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/content_visibility_diffs")
}

fn golden_path(name: &str) -> PathBuf {
  golden_dir().join(format!("{name}.png"))
}

fn should_update_goldens() -> bool {
  std::env::var("UPDATE_PAGES_GOLDEN").is_ok()
}

fn base_url_for(html_path: &Path) -> Result<String, String> {
  let dir = html_path
    .parent()
    .ok_or_else(|| format!("No parent directory for {}", html_path.display()))?;
  Url::from_directory_path(dir)
    .map_err(|_| format!("Failed to build file:// base URL for {}", dir.display()))
    .map(|url| url.to_string())
}

fn content_visibility_runtime_toggles() -> RuntimeToggles {
  // Keep the regression stable even when developers have FASTR_* env vars set locally (e.g.
  // activation margin experiments).
  RuntimeToggles::from_map(HashMap::from([(
    "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
    "0".to_string(),
  )]))
}

fn run_fixture_with_options(options: RenderOptions) -> Vec<u8> {
  let options = options.with_runtime_toggles(content_visibility_runtime_toggles());
  let html_path = fixtures_dir().join("content_visibility/index.html");
  let html =
    fs::read_to_string(&html_path).unwrap_or_else(|e| panic!("Failed to read fixture: {e}"));
  let base_url = base_url_for(&html_path).expect("failed to build base url");

  let mut renderer = FastRender::builder()
    .base_url(base_url)
    .build()
    .expect("renderer should build");
  let pixmap = renderer
    .render_html_with_options(&html, options)
    .expect("render should succeed");
  encode_image(&pixmap, OutputFormat::Png).expect("encode should succeed")
}

fn assert_matches_golden(
  golden_name: &str,
  rendered: &[u8],
  compare_config: &r#ref::CompareConfig,
) {
  let golden_path = golden_path(golden_name);
  if should_update_goldens() {
    fs::create_dir_all(golden_dir()).expect("failed to create golden dir");
    fs::write(&golden_path, rendered).expect("failed to write golden");
    eprintln!("Updated golden for {golden_name}");
    return;
  }

  let golden = fs::read(&golden_path).unwrap_or_else(|e| {
    panic!(
      "Missing golden {} ({}). Set UPDATE_PAGES_GOLDEN=1 to regenerate. Error: {}",
      golden_name,
      golden_path.display(),
      e
    )
  });

  compare_pngs(golden_name, rendered, &golden, compare_config, &diff_dir())
    .unwrap_or_else(|e| panic!("Comparison failed: {e}"));
}

#[test]
fn content_visibility_regression() {
  let mut compare_config =
    compare_config_from_env(CompareEnvVars::pages()).expect("invalid comparison configuration");
  // Keep the test stable across platforms by allowing a small amount of per-pixel drift.
  compare_config.max_different_percent = compare_config.max_different_percent.max(0.05);

  let rendered = run_fixture_with_options(RenderOptions::new().with_viewport(420, 260));
  assert_matches_golden("content_visibility", &rendered, &compare_config);

  // Also validate behavior when rendering at a non-zero scroll offset (both layout- and paint-time
  // visibility decisions should respect the scroll position).
  let rendered_scrolled = run_fixture_with_options(
    RenderOptions::new()
      .with_viewport(420, 260)
      .with_scroll(0.0, 150.0),
  );
  assert_matches_golden(
    "content_visibility_scrolled",
    &rendered_scrolled,
    &compare_config,
  );
}
