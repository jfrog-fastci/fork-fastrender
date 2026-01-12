use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::render_control::{DeadlineGuard, RenderDeadline};
use fastrender::{
  parse_stylesheet_with_errors, FastRender, FastRenderConfig, FontConfig, LayoutParallelism,
  PaintParallelism, RenderOptions, ResourcePolicy,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const VIEWPORT: u32 = 256;
const DPR: f32 = 1.0;
const STYLESHEET_LIMIT: usize = 16;
const CSS_PARSE_TIMEOUT: Duration = Duration::from_millis(200);
const HTML_RENDER_TIMEOUT: Duration = Duration::from_millis(1500);
const HTML_RENDER_TIMEOUT_STRESS: Duration = Duration::from_millis(3000);

const REQUIRED_CORPUS_FILES: &[&str] = &[
  "render_pipeline_minimal.html",
  "render_pipeline_stress.html",
  "animations.css",
  "bootstrap_snippet.css",
  "media_queries.css",
  "complex_selectors.css",
  "svg_filters.svg",
];

fn corpus_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fuzz_corpus")
}

fn build_renderer() -> FastRender {
  let policy = ResourcePolicy::new()
    .allow_http(false)
    .allow_https(false)
    .allow_file(false)
    .allow_data(true)
    // Defend against accidental payload growth (even though the checked-in corpus is small).
    .with_max_response_bytes(256 * 1024)
    // Redirects only matter for HTTP(S), which is disabled above.
    .with_max_redirects(1);

  // Don't let runtime behavior drift based on the host environment.
  let runtime_toggles = RuntimeToggles::from_map(HashMap::new());

  let mut config = FastRenderConfig::new()
    .with_default_viewport(VIEWPORT, VIEWPORT)
    .with_device_pixel_ratio(DPR)
    .with_resource_policy(policy)
    .with_max_iframe_depth(1)
    .with_runtime_toggles(runtime_toggles)
    // Prefer bundled fixture fonts for stable render output (no host font discovery).
    .with_font_sources(FontConfig::bundled_only())
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  // Avoid pixmap blow-ups from pathological layout bounds.
  config.fit_canvas_to_content = false;
  // Don't cache fetched resources across corpus cases (keeps memory bounded + deterministic).
  config.resource_cache = None;

  FastRender::with_config(config).expect("failed to construct FastRender for fuzz corpus smoke test")
}

fn base_render_options(timeout: Duration) -> RenderOptions {
  RenderOptions::new()
    .with_viewport(VIEWPORT, VIEWPORT)
    .with_device_pixel_ratio(DPR)
    .with_fit_canvas_to_content(false)
    .with_timeout(Some(timeout))
    // Avoid pathological "link rel=stylesheet" storms (corpus inputs are curated, but this is a
    // cheap extra defense).
    .with_stylesheet_limit(Some(STYLESHEET_LIMIT))
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled())
}

fn run_html_case(renderer: &mut FastRender, file_name: &str, html: &str, timeout: Duration) {
  let options = base_render_options(timeout);
  match renderer.render_html_with_options(html, options) {
    Ok(_) => {}
    Err(err) => {
      assert!(
        !err.is_timeout(),
        "HTML corpus case {file_name} exceeded timeout ({:?}): {err:?}",
        timeout
      );
      eprintln!("HTML corpus case {file_name} returned error (allowed): {err:?}");
    }
  }
}

fn run_css_case(file_name: &str, css: &str) {
  let deadline = RenderDeadline::new(Some(CSS_PARSE_TIMEOUT), None);
  let _deadline_guard = DeadlineGuard::install(Some(&deadline));

  match parse_stylesheet_with_errors(css) {
    Ok(_result) => {}
    Err(err) => {
      assert!(
        !err.is_timeout(),
        "CSS corpus case {file_name} exceeded timeout ({:?}): {err:?}",
        CSS_PARSE_TIMEOUT
      );
      eprintln!("CSS corpus case {file_name} returned error (allowed): {err:?}");
    }
  }
}

fn run_svg_case(renderer: &mut FastRender, file_name: &str, svg_content: &[u8]) {
  let encoded = BASE64_STANDARD.encode(svg_content);
  // Drive SVG parsing through the normal HTML render pipeline by rendering an `<img>` whose source
  // is the corpus entry. This hits FastRender's `data:` URL resource handling and SVG decode path
  // (resvg).
  let html = format!(
    r#"<!doctype html>
<meta charset="utf-8">
<title>svg_corpus_smoke</title>
<style>
  body {{ margin: 0; background: #fff; }}
  img {{ width: 128px; height: 128px; }}
</style>
<body>
  <img src="data:image/svg+xml;base64,{encoded}">
</body>
"#
  );
 
  run_html_case(renderer, file_name, &html, HTML_RENDER_TIMEOUT);
}

fn should_skip_stress_html(file_name: &str) -> bool {
  if file_name != "render_pipeline_stress.html" {
    return false;
  }
  cfg!(debug_assertions) && std::env::var_os("FUZZ_CORPUS_SMOKE_IN_DEBUG").is_none()
}
 
fn corpus_file_name(path: &Path) -> String {
  path
    .file_name()
    .unwrap_or_default()
    .to_string_lossy()
    .to_string()
}

#[test]
fn fuzz_corpus_smoke_test() {
  // Keep smoke runs deterministic and avoid spawning a large Rayon pool in CI.
  //
  // `fastrender` also disables layout/paint parallelism via per-render options, but initializing
  // Rayon to a single thread keeps incidental parallel work (and stack usage) bounded.
  crate::common::init_rayon_for_tests(1);

  let corpus_dir = corpus_dir();
  for file in REQUIRED_CORPUS_FILES {
    let path = corpus_dir.join(file);
    assert!(
      path.exists(),
      "Required fuzz corpus entry missing from repository: {}",
      path.display()
    );
  }

  let mut renderer = build_renderer();

  let mut entries = fs::read_dir(&corpus_dir)
    .unwrap_or_else(|e| panic!("failed to read corpus dir {}: {e}", corpus_dir.display()))
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.path())
    .filter(|path| path.is_file())
    .collect::<Vec<_>>();
  entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

  for path in entries {
    let file_name = corpus_file_name(&path);
    let ext = path.extension().and_then(|ext| ext.to_str());

    match ext {
      Some("css") => {
        let css = fs::read_to_string(&path)
          .unwrap_or_else(|e| panic!("failed to read CSS corpus file {}: {e}", path.display()));
        run_css_case(&file_name, &css);
      }
      Some("html") => {
        if should_skip_stress_html(&file_name) {
          eprintln!(
            "Skipping {file_name} in debug (set FUZZ_CORPUS_SMOKE_IN_DEBUG=1 to run)."
          );
          continue;
        }

        let html = fs::read_to_string(&path)
          .unwrap_or_else(|e| panic!("failed to read HTML corpus file {}: {e}", path.display()));
        let timeout = if file_name == "render_pipeline_stress.html" {
          HTML_RENDER_TIMEOUT_STRESS
        } else {
          HTML_RENDER_TIMEOUT
        };
        run_html_case(&mut renderer, &file_name, &html, timeout);
      }
      Some("svg") => {
        let svg = fs::read(&path)
          .unwrap_or_else(|e| panic!("failed to read SVG corpus file {}: {e}", path.display()));
        run_svg_case(&mut renderer, &file_name, &svg);
      }
      _ => {}
    }
  }
}
