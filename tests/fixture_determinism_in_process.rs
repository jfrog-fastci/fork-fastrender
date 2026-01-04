mod r#ref;

use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::style::media::MediaType;
use fastrender::{
  snapshot_pipeline, FastRender, FontConfig, Pixmap, PipelineSnapshot, RenderArtifactRequest,
  RenderDiagnostics, RenderOptions, ResourcePolicy,
};
use rayon::ThreadPoolBuilder;
use r#ref::image_compare::compare_pngs;
use r#ref::CompareConfig;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use url::Url;

struct Fixture<'a> {
  name: &'a str,
  html: &'a str,
}

const FIXTURES: &[Fixture<'static>] = &[
  Fixture {
    name: "preserve_3d_stack",
    html: "preserve_3d_stack/index.html",
  },
  Fixture {
    name: "filter_backdrop_scene",
    html: "filter_backdrop_scene/index.html",
  },
];

const DEFAULT_DPR: f32 = 1.0;

const ENV_VIEWPORT: &str = "FASTR_IN_PROCESS_DETERMINISM_VIEWPORT";
const ENV_SCHEDULE: &str = "FASTR_IN_PROCESS_DETERMINISM_SCHEDULE";
const ENV_HEAVY: &str = "FASTR_IN_PROCESS_DETERMINISM_HEAVY";
const ENV_ALLOW_HEAVY: &str = "FASTR_IN_PROCESS_DETERMINISM_ALLOW_HEAVY";

// CI-friendly defaults:
// - a smaller viewport to keep debug builds snappy (especially on Windows/macOS runners)
// - a short schedule that still flips Rayon thread count to exercise parallel vs serial paths
const FAST_VIEWPORT: (u32, u32) = (600, 600);
const FAST_SCHEDULE: &[usize] = &[4, 1];

// Heavier settings (opt-in) for local debugging / chasing subtle nondeterminism.
const HEAVY_VIEWPORT: (u32, u32) = (1040, 1240);
const HEAVY_SCHEDULE: &[usize] = &[4, 4, 1];

#[derive(Debug, Clone)]
struct DeterminismConfig {
  viewport: (u32, u32),
  schedule: Vec<usize>,
}

impl DeterminismConfig {
  fn from_env() -> Result<Self, String> {
    let heavy = env_flag(ENV_HEAVY)?;

    let viewport = match env::var(ENV_VIEWPORT) {
      Ok(value) => parse_viewport(&value).map_err(|e| format!("{ENV_VIEWPORT}: {e}"))?,
      Err(env::VarError::NotPresent) => {
        if heavy {
          HEAVY_VIEWPORT
        } else {
          FAST_VIEWPORT
        }
      }
      Err(env::VarError::NotUnicode(_)) => {
        return Err(format!("{ENV_VIEWPORT} must be valid UTF-8"));
      }
    };

    let schedule = match env::var(ENV_SCHEDULE) {
      Ok(value) => parse_schedule(&value).map_err(|e| format!("{ENV_SCHEDULE}: {e}"))?,
      Err(env::VarError::NotPresent) => {
        if heavy {
          HEAVY_SCHEDULE.to_vec()
        } else {
          FAST_SCHEDULE.to_vec()
        }
      }
      Err(env::VarError::NotUnicode(_)) => {
        return Err(format!("{ENV_SCHEDULE} must be valid UTF-8"));
      }
    };

    if schedule.len() < 2 {
      return Err(format!(
        "render schedule must include at least 2 runs to detect nondeterminism (got {schedule:?})"
      ));
    }

    Ok(Self { viewport, schedule })
  }

  fn render_options(&self) -> RenderOptions {
    RenderOptions::new()
      .with_viewport(self.viewport.0, self.viewport.1)
      .with_device_pixel_ratio(DEFAULT_DPR)
      .with_media_type(MediaType::Screen)
  }
}

fn env_flag(name: &str) -> Result<bool, String> {
  let value = match env::var(name) {
    Ok(value) => value,
    Err(env::VarError::NotPresent) => return Ok(false),
    Err(env::VarError::NotUnicode(_)) => return Err(format!("{name} must be valid UTF-8")),
  };

  let value = value.trim().to_ascii_lowercase();
  if value.is_empty() {
    return Ok(true);
  }
  match value.as_str() {
    "1" | "true" | "yes" | "on" => Ok(true),
    "0" | "false" | "no" | "off" => Ok(false),
    other => Err(format!("expected 1/0/true/false/yes/no/on/off, got '{other}'")),
  }
}

fn parse_viewport(value: &str) -> Result<(u32, u32), String> {
  let value = value.trim();
  let (w, h) = value
    .split_once('x')
    .or_else(|| value.split_once('X'))
    .ok_or_else(|| "expected format <width>x<height> (e.g. 600x600)".to_string())?;

  let w: u32 = w
    .trim()
    .parse()
    .map_err(|_| format!("invalid width '{w}'"))?;
  let h: u32 = h
    .trim()
    .parse()
    .map_err(|_| format!("invalid height '{h}'"))?;
  if w == 0 || h == 0 {
    return Err("width/height must be > 0".to_string());
  }
  Ok((w, h))
}

fn parse_schedule(value: &str) -> Result<Vec<usize>, String> {
  let value = value.trim();
  if value.is_empty() {
    return Err("schedule cannot be empty (expected e.g. 4,1 or 4,4,1)".to_string());
  }

  let mut schedule = Vec::new();
  for part in value.split(',') {
    let part = part.trim();
    if part.is_empty() {
      return Err(format!("invalid schedule '{value}': empty entry"));
    }

    let threads: usize = part
      .parse()
      .map_err(|_| format!("invalid thread count '{part}'"))?;
    if threads == 0 {
      return Err("thread count must be > 0".to_string());
    }
    schedule.push(threads);
  }
  Ok(schedule)
}

fn build_thread_pools(schedule: &[usize]) -> Result<HashMap<usize, rayon::ThreadPool>, String> {
  let mut unique = BTreeSet::new();
  unique.extend(schedule.iter().copied());
  let mut pools = HashMap::new();
  for threads in unique {
    let pool = ThreadPoolBuilder::new()
      .num_threads(threads)
      .build()
      .map_err(|e| format!("Failed to create {threads}-thread pool: {e}"))?;
    pools.insert(threads, pool);
  }
  Ok(pools)
}

fn fixtures_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/pages/fixtures")
}

fn diff_dir_for_fixture(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("target/determinism_diffs/in_process")
    .join(name)
}

fn base_url_for(html_path: &Path) -> Result<String, String> {
  let dir = html_path
    .parent()
    .ok_or_else(|| format!("No parent directory for {}", html_path.display()))?;
  Url::from_directory_path(dir)
    .map_err(|_| format!("Failed to build file:// base URL for {}", dir.display()))
    .map(|url| url.to_string())
}

fn render_pixmap(renderer: &mut FastRender, html: &str, options: &RenderOptions) -> Result<Pixmap, String> {
  renderer
    .render_html_with_options(html, options.clone())
    .map_err(|e| format!("Render failed: {:?}", e))
}

fn capture_snapshot(
  renderer: &mut FastRender,
  html: &str,
  base_url: &str,
  options: &RenderOptions,
) -> Result<(PipelineSnapshot, RenderDiagnostics), String> {
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      base_url,
      options.clone(),
      RenderArtifactRequest::summary(),
    )
    .map_err(|e| format!("Render with artifacts failed: {:?}", e))?;

  let dom = report
    .artifacts
    .dom
    .as_ref()
    .ok_or_else(|| "missing DOM artifact".to_string())?;
  let styled = report
    .artifacts
    .styled_tree
    .as_ref()
    .ok_or_else(|| "missing styled tree artifact".to_string())?;
  let box_tree = report
    .artifacts
    .box_tree
    .as_ref()
    .ok_or_else(|| "missing box tree artifact".to_string())?;
  let fragment_tree = report
    .artifacts
    .fragment_tree
    .as_ref()
    .ok_or_else(|| "missing fragment tree artifact".to_string())?;
  let display_list = report
    .artifacts
    .display_list
    .as_ref()
    .ok_or_else(|| "missing display list artifact".to_string())?;

  Ok((
    snapshot_pipeline(dom, styled, box_tree, fragment_tree, display_list),
    report.diagnostics,
  ))
}

fn write_json_pretty(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
  let json =
    serde_json::to_string_pretty(value).map_err(|e| format!("serialize {}: {e}", path.display()))?;
  fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

fn run_diff_snapshots(before_dir: &Path, after_dir: &Path, out_dir: &Path) -> Result<(), String> {
  let json_path = out_dir.join("diff_snapshots.json");
  let html_path = out_dir.join("diff_snapshots.html");
  let status = Command::new(env!("CARGO_BIN_EXE_diff_snapshots"))
    .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    .args([
      "--before",
      before_dir.to_str().ok_or_else(|| "before dir not utf-8".to_string())?,
      "--after",
      after_dir.to_str().ok_or_else(|| "after dir not utf-8".to_string())?,
      "--json",
      json_path.to_str().ok_or_else(|| "json path not utf-8".to_string())?,
      "--html",
      html_path.to_str().ok_or_else(|| "html path not utf-8".to_string())?,
    ])
    .status()
    .map_err(|e| format!("spawn diff_snapshots: {e}"))?;

  if status.success() {
    return Ok(());
  }

  Err(format!("diff_snapshots failed with status {status}"))
}

fn pixmap_to_straight_rgba(pixmap: &Pixmap) -> Vec<u8> {
  let mut rgba = Vec::with_capacity(pixmap.data().len());
  for chunk in pixmap.data().chunks_exact(4) {
    let r = chunk[0];
    let g = chunk[1];
    let b = chunk[2];
    let a = chunk[3];

    // Convert premultiplied RGBA (tiny-skia) to straight RGBA for stable, byte-for-byte comparisons
    // that match the bytes written by `encode_image(OutputFormat::Png)`.
    let (r, g, b) = if a > 0 {
      let alpha = a as f32 / 255.0;
      (
        ((r as f32 / alpha).min(255.0)) as u8,
        ((g as f32 / alpha).min(255.0)) as u8,
        ((b as f32 / alpha).min(255.0)) as u8,
      )
    } else {
      (0, 0, 0)
    };

    rgba.extend_from_slice(&[r, g, b, a]);
  }
  rgba
}

fn pixmap_matches_straight_rgba(pixmap: &Pixmap, expected_rgba: &[u8]) -> bool {
  if pixmap.data().len() != expected_rgba.len() {
    return false;
  }

  for (expected, chunk) in expected_rgba.chunks_exact(4).zip(pixmap.data().chunks_exact(4)) {
    let r = chunk[0];
    let g = chunk[1];
    let b = chunk[2];
    let a = chunk[3];
    let (r, g, b) = if a > 0 {
      let alpha = a as f32 / 255.0;
      (
        ((r as f32 / alpha).min(255.0)) as u8,
        ((g as f32 / alpha).min(255.0)) as u8,
        ((b as f32 / alpha).min(255.0)) as u8,
      )
    } else {
      (0, 0, 0)
    };

    if expected[0] != r || expected[1] != g || expected[2] != b || expected[3] != a {
      return false;
    }
  }

  true
}

fn run_fixture(
  fixture: &Fixture<'_>,
  compare_config: &CompareConfig,
  config: &DeterminismConfig,
  pools: &HashMap<usize, rayon::ThreadPool>,
  options: &RenderOptions,
) -> Result<(), String> {
  let html_path = fixtures_dir().join(fixture.html);
  let html = fs::read_to_string(&html_path)
    .map_err(|e| format!("Failed to read {}: {}", html_path.display(), e))?;
  let base_url = base_url_for(&html_path)?;

  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut renderer = FastRender::builder()
    .base_url(base_url.clone())
    .font_sources(FontConfig::bundled_only())
    .resource_policy(policy)
    .build()
    .map_err(|e| format!("Failed to create renderer: {:?}", e))?;

  let mut expected: Option<Pixmap> = None;
  let mut expected_rgba: Option<Vec<u8>> = None;
  let mut expected_threads: Option<usize> = None;
  let output_dir = diff_dir_for_fixture(fixture.name);

  for (idx, &threads) in config.schedule.iter().enumerate() {
    let pool = pools
      .get(&threads)
      .ok_or_else(|| format!("Missing thread pool for {threads} threads"))?;

    let rendered = pool.install(|| render_pixmap(&mut renderer, &html, options))?;

    if let (Some(expected_pixmap), Some(expected_rgba)) = (expected.as_ref(), expected_rgba.as_ref())
    {
      if !pixmap_matches_straight_rgba(&rendered, expected_rgba) {
        let label = format!("run_{idx}_threads_{threads}");
        let expected_png =
          encode_image(expected_pixmap, OutputFormat::Png).map_err(|e| format!("{e:?}"))?;
        let rendered_png =
          encode_image(&rendered, OutputFormat::Png).map_err(|e| format!("{e:?}"))?;
        let mut message =
          compare_pngs(&label, &rendered_png, &expected_png, compare_config, &output_dir)
            .unwrap_err();

        // If the pixel diff is due to nondeterminism, make it actionable by capturing pipeline
        // snapshots (DOM/styled/box/fragment/display-list) for both variants and running
        // `diff_snapshots` to produce a stage-level report.
        let snapshot_root = output_dir.join("snapshots");
        let before_dir = snapshot_root.join("run1").join(fixture.name);
        let after_dir = snapshot_root.join("run2").join(fixture.name);

        let mut snapshot_error = None::<String>;
        let expected_threads = expected_threads.unwrap_or(config.schedule[0]);
        let expected_pool = pools
          .get(&expected_threads)
          .ok_or_else(|| format!("Missing thread pool for expected {expected_threads} threads"))?;

        let before_capture = expected_pool.install(|| {
          capture_snapshot(&mut renderer, &html, &base_url, options)
        });
        let after_capture =
          pool.install(|| capture_snapshot(&mut renderer, &html, &base_url, options));

        match (before_capture, after_capture) {
          (Ok((before_snapshot, before_diag)), Ok((after_snapshot, after_diag))) => {
            fs::create_dir_all(&before_dir)
              .map_err(|e| format!("create {}: {e}", before_dir.display()))?;
            fs::create_dir_all(&after_dir)
              .map_err(|e| format!("create {}: {e}", after_dir.display()))?;

            if let Err(err) = write_json_pretty(&before_dir.join("snapshot.json"), &before_snapshot)
            {
              snapshot_error = Some(err);
            } else if let Err(err) =
              write_json_pretty(&after_dir.join("snapshot.json"), &after_snapshot)
            {
              snapshot_error = Some(err);
            } else if let Err(err) =
              write_json_pretty(&before_dir.join("diagnostics.json"), &before_diag)
            {
              snapshot_error = Some(err);
            } else if let Err(err) =
              write_json_pretty(&after_dir.join("diagnostics.json"), &after_diag)
            {
              snapshot_error = Some(err);
            } else if let Err(err) = fs::write(before_dir.join("render.png"), &expected_png)
              .map_err(|e| format!("write render.png: {e}"))
            {
              snapshot_error = Some(err);
            } else if let Err(err) = fs::write(after_dir.join("render.png"), &rendered_png)
              .map_err(|e| format!("write render.png: {e}"))
            {
              snapshot_error = Some(err);
            } else if let Err(err) = run_diff_snapshots(&before_dir, &after_dir, &output_dir) {
              snapshot_error = Some(err);
            }
          }
          (Err(err), _) => snapshot_error = Some(err),
          (_, Err(err)) => snapshot_error = Some(err),
        }

        message.push_str("\n\nSnapshot artifacts:");
        message.push_str(&format!("\n  before: {}", before_dir.display()));
        message.push_str(&format!("\n  after:  {}", after_dir.display()));
        message.push_str(&format!(
          "\n  diff_snapshots: {}",
          output_dir.join("diff_snapshots.html").display()
        ));
        if let Some(err) = snapshot_error {
          message.push_str(&format!("\n\nSnapshot capture failed:\n{err}"));
        }

        return Err(message);
      }
    } else {
      expected_rgba = Some(pixmap_to_straight_rgba(&rendered));
      expected = Some(rendered);
      expected_threads = Some(threads);
    }
  }

  Ok(())
}

#[test]
fn fixture_determinism_in_process() {
  let config =
    DeterminismConfig::from_env().unwrap_or_else(|e| panic!("Invalid determinism config: {e}"));

  let is_heavy_settings = config.viewport == HEAVY_VIEWPORT && config.schedule == HEAVY_SCHEDULE;
  let is_slow_platform = cfg!(target_os = "windows") || cfg!(target_os = "macos");
  let allow_heavy = env_flag(ENV_ALLOW_HEAVY)
    .unwrap_or_else(|e| panic!("Invalid {ENV_ALLOW_HEAVY} value: {e}"));
  if is_heavy_settings && is_slow_platform && !allow_heavy {
    eprintln!(
      "Skipping heavy in-process determinism run on this platform (viewport {}x{}, schedule {:?}).\n\
Set {ENV_ALLOW_HEAVY}=1 to run anyway.",
      config.viewport.0, config.viewport.1, config.schedule
    );
    return;
  }

  let options = config.render_options();
  let pools =
    build_thread_pools(&config.schedule).unwrap_or_else(|e| panic!("Thread pool setup failed: {e}"));

  let compare_config = CompareConfig::strict();
  for fixture in FIXTURES {
    run_fixture(fixture, &compare_config, &config, &pools, &options).unwrap_or_else(|e| {
      panic!(
        "Fixture '{}' failed determinism check (viewport {}x{}, schedule {:?}).\n\
Override via {ENV_VIEWPORT}=<wxh>, {ENV_SCHEDULE}=<csv>, or {ENV_HEAVY}=1 (heavy preset).\n\n{}",
        fixture.name, config.viewport.0, config.viewport.1, config.schedule, e
      )
    });
  }
}
