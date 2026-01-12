//! HTML fixture golden regression suite.
//!
//! This harness renders every top-level HTML fixture under `tests/fixtures/html/*.html` and
//! compares the output against checked-in PNG goldens under `tests/fixtures/golden/`.
//!
//! ## Filtering
//!
//! The suite is intentionally auto-discovered so adding new fixtures does not require updating the
//! harness. To keep iterations fast, you can select a subset via environment variables:
//!
//! - `FIXTURES_FIXTURE=<name>`
//! - `FIXTURES_FILTER=<comma,separated,names>`
//!
//! ## Golden updates
//!
//! Set `UPDATE_GOLDEN=1` to rewrite goldens in-place:
//!
//! ```bash
//! UPDATE_GOLDEN=1 bash scripts/cargo_agent.sh test -p fastrender --test integration fixtures::runner::fixtures_regression_suite -- --exact
//! ```

use crate::common::{init_rayon_for_tests, with_large_stack};
use crate::r#ref::compare::load_png_from_bytes;
use crate::r#ref::image_compare::{compare_config_from_env, compare_pngs, CompareEnvVars};
use fastrender::api::RenderDiagnostics;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::image_output::OutputFormat;
use fastrender::{FastRender, FontConfig, RenderOptions, ResourcePolicy};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

/// Fallback viewport when no golden exists yet.
const DEFAULT_VIEWPORT: (u32, u32) = (600, 800);

/// Rendering "shots" for a single fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixtureShot {
  /// Default DPR=1 golden: `<name>.png`
  Default,
  /// Optional DPR=2 golden: `<name>_dpr2.png`
  Dpr2,
}

impl FixtureShot {
  fn dpr(self) -> u32 {
    match self {
      Self::Default => 1,
      Self::Dpr2 => 2,
    }
  }

  fn golden_name(self, fixture_name: &str) -> String {
    match self {
      Self::Default => fixture_name.to_string(),
      Self::Dpr2 => format!("{fixture_name}_dpr2"),
    }
  }
}

#[derive(Debug, Clone)]
struct Fixture {
  name: String,
  html_path: PathBuf,
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn html_dir() -> PathBuf {
  fixtures_dir().join("html")
}

fn golden_dir() -> PathBuf {
  fixtures_dir().join("golden")
}

fn fixtures_diff_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/fixtures_diffs")
}

fn golden_path(golden_name: &str) -> PathBuf {
  golden_dir().join(format!("{golden_name}.png"))
}

fn should_update_goldens() -> bool {
  std::env::var_os("UPDATE_GOLDEN").is_some()
}

fn fixture_filter() -> Option<Vec<String>> {
  let raw = std::env::var("FIXTURES_FILTER")
    .ok()
    .or_else(|| std::env::var("FIXTURES_FIXTURE").ok())?;
  let parts = raw
    .split(',')
    .map(|part| part.trim().to_string())
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>();
  (!parts.is_empty()).then_some(parts)
}

fn discover_fixtures() -> Result<Vec<Fixture>, String> {
  let dir = html_dir();
  let entries = fs::read_dir(&dir)
    .map_err(|e| format!("Failed to read HTML fixtures dir {}: {}", dir.display(), e))?;
  let mut fixtures = Vec::new();

  for entry in entries {
    let entry = entry
      .map_err(|e| format!("Failed to read entry in {}: {}", dir.display(), e))?;
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension() != Some(OsStr::new("html")) {
      continue;
    }
    let stem = path
      .file_stem()
      .and_then(|s| s.to_str())
      .ok_or_else(|| format!("Fixture filename is not valid UTF-8: {}", path.display()))?;
    fixtures.push(Fixture {
      name: stem.to_string(),
      html_path: path,
    });
  }

  fixtures.sort_by(|a, b| a.name.cmp(&b.name));
  Ok(fixtures)
}

fn base_url_for(html_path: &Path) -> Result<String, String> {
  let dir = html_path
    .parent()
    .ok_or_else(|| format!("No parent directory for {}", html_path.display()))?;
  Url::from_directory_path(dir)
    .map_err(|_| format!("Failed to build file:// base URL for {}", dir.display()))
    .map(|url| url.to_string())
}

fn offline_resource_policy() -> ResourcePolicy {
  ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true)
}

fn infer_viewport_from_golden(
  golden_png: &[u8],
  dpr: u32,
  golden_path: &Path,
) -> Result<(u32, u32), String> {
  let pixmap = load_png_from_bytes(golden_png).map_err(|e| {
    format!(
      "Failed to decode golden PNG {}: {}",
      golden_path.display(),
      e
    )
  })?;
  let (width, height) = (pixmap.width(), pixmap.height());
  if dpr == 0 {
    return Err("DPR must be > 0".to_string());
  }
  if width % dpr != 0 || height % dpr != 0 {
    return Err(format!(
      "Golden {} has dimensions {}x{} which are not divisible by dpr={}",
      golden_path.display(),
      width,
      height,
      dpr
    ));
  }
  Ok((width / dpr, height / dpr))
}

fn render_png(
  renderer: &mut FastRender,
  html: &str,
  viewport: (u32, u32),
  dpr: f32,
) -> Result<(Vec<u8>, RenderDiagnostics), String> {
  let options = RenderOptions::new()
    .with_viewport(viewport.0, viewport.1)
    .with_device_pixel_ratio(dpr);
  let rendered = renderer
    .render_html_with_diagnostics(html, options)
    .map_err(|e| format!("Render failed: {:?}", e))?;
  rendered
    .encode(OutputFormat::Png)
    .map_err(|e| format!("PNG encode failed: {:?}", e))
}

fn run_fixture(
  fixture: &Fixture,
  compare_config: &crate::r#ref::compare::CompareConfig,
) -> Result<(), String> {
  let html = fs::read_to_string(&fixture.html_path).map_err(|e| {
    format!(
      "Failed to read fixture {}: {}",
      fixture.html_path.display(),
      e
    )
  })?;
  let base_url = base_url_for(&fixture.html_path)?;
  let policy = offline_resource_policy();

  let mut renderer = FastRender::builder()
    .base_url(base_url)
    .font_sources(FontConfig::bundled_only())
    // Avoid host `FASTR_*` env vars affecting deterministic fixture renders.
    .runtime_toggles(RuntimeToggles::default())
    .resource_policy(policy)
    .build()
    .map_err(|e| format!("Failed to create renderer: {:?}", e))?;

  let mut shots = vec![FixtureShot::Default];
  let dpr2_path = golden_path(&FixtureShot::Dpr2.golden_name(&fixture.name));
  if dpr2_path.exists() {
    shots.push(FixtureShot::Dpr2);
  }

  for shot in shots {
    let golden_name = shot.golden_name(&fixture.name);
    let golden_path = golden_path(&golden_name);
    let golden_bytes = fs::read(&golden_path).ok();

    let viewport = match golden_bytes.as_deref() {
      Some(bytes) => infer_viewport_from_golden(bytes, shot.dpr(), &golden_path)?,
      None => DEFAULT_VIEWPORT,
    };

    let (rendered, diagnostics) = render_png(&mut renderer, &html, viewport, shot.dpr() as f32)?;

    if should_update_goldens() {
      fs::create_dir_all(golden_dir()).map_err(|e| {
        format!(
          "Failed to create golden dir {}: {}",
          golden_dir().display(),
          e
        )
      })?;
      fs::write(&golden_path, &rendered)
        .map_err(|e| format!("Failed to write golden {}: {}", golden_path.display(), e))?;
      eprintln!("Updated golden for {}", golden_name);
      continue;
    }

    if let Some(golden_bytes) = golden_bytes {
      if let Err(err) = compare_pngs(
        &golden_name,
        &rendered,
        &golden_bytes,
        compare_config,
        &fixtures_diff_dir(),
      ) {
        let mut message = err;
        if !diagnostics.fetch_errors.is_empty() || !diagnostics.blocked_fetch_errors.is_empty() {
          message.push_str("\n\nRender diagnostics:");
          if !diagnostics.fetch_errors.is_empty() {
            message.push_str("\nFetch errors:");
            for error in &diagnostics.fetch_errors {
              message.push_str(&format!(
                "\n- {:?} {}: {}",
                error.kind, error.url, error.message
              ));
            }
          }
          if !diagnostics.blocked_fetch_errors.is_empty() {
            message.push_str("\nBlocked fetch errors:");
            for error in &diagnostics.blocked_fetch_errors {
              message.push_str(&format!(
                "\n- {:?} {}: {}",
                error.kind, error.url, error.message
              ));
            }
          }
        }
        return Err(message);
      }
    } else {
      // No golden exists yet; at least validate that we produced a syntactically valid PNG.
      load_png_from_bytes(&rendered)
        .map_err(|e| format!("Invalid PNG output for {}: {}", golden_name, e))?;
      eprintln!(
        "Warning: Missing golden for {} (expected at {}). Run with UPDATE_GOLDEN=1 to create.",
        golden_name,
        golden_path.display()
      );
    }
  }

  Ok(())
}

#[test]
fn fixture_discovery_finds_many_html_fixtures_and_excludes_js_subtree() {
  let fixtures = discover_fixtures().expect("fixture discovery should succeed");
  assert!(
    fixtures.len() > 80,
    "Expected > 80 HTML fixtures; found {}",
    fixtures.len()
  );

  assert!(
    fixtures.iter().any(|f| f.name == "block_simple"),
    "Expected discovery to include block_simple"
  );

  // Ensure we are not recursively walking `tests/fixtures/html/js/**`.
  let js_fixture = html_dir().join("js/base_url_timing.html");
  assert!(
    js_fixture.exists(),
    "Expected test fixture to exist for js subtree exclusion test: {}",
    js_fixture.display()
  );
  assert!(
    !fixtures.iter().any(|f| f.html_path == js_fixture),
    "Discovery should ignore html/js/** entries, but included {}",
    js_fixture.display()
  );
}

#[test]
fn fixtures_regression_suite() {
  let filter = fixture_filter();
  init_rayon_for_tests(2);
  with_large_stack(move || {
    let compare_config =
      compare_config_from_env(CompareEnvVars::fixtures()).expect("invalid compare config");
    let fixtures = discover_fixtures().expect("fixture discovery failed");

    let mut failures = Vec::new();
    for fixture in fixtures {
      if let Some(filter) = filter.as_ref() {
        if !filter.iter().any(|name| name == &fixture.name) {
          continue;
        }
      }
      if let Err(err) = run_fixture(&fixture, &compare_config) {
        failures.push(format!("Fixture '{}' failed: {}", fixture.name, err));
      }
    }

    if !failures.is_empty() {
      panic!(
        "{} fixture(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
      );
    }
  });
}

