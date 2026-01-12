use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::layout::determinism::fragment_tree_fingerprint;
use fastrender::style::media::MediaType;
use fastrender::{
  snapshot_pipeline, FastRender, FontConfig, PipelineSnapshot, RenderArtifactRequest,
  RenderDiagnostics, RenderOptions, ResourcePolicy,
};
use rayon::ThreadPoolBuilder;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture<'a> {
  name: &'a str,
  html: &'a str,
}

const FIXTURES: &[Fixture<'static>] = &[
  Fixture {
    name: "float_inline_text",
    html: r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      body { font-family: sans-serif; font-size: 16px; line-height: 1.2; }
      .wrap { width: 320px; padding: 6px; }
      .float { float: left; width: 64px; height: 48px; margin: 4px; background: #ddd; border: 1px solid #999; }
      .float.r { float: right; width: 56px; height: 40px; margin: 3px; background: #eee; }
      p { margin: 0; }
      .marker { display: inline-block; padding: 0 2px; border: 1px solid #ccc; }
    </style>
  </head>
  <body>
    <div class="wrap">
      <div class="float"></div>
      <div class="float r"></div>
      <div class="float"></div>
      <p>
        <span class="marker">A</span>
        This paragraph contains enough words to force multiple line breaks while avoiding floats.
        supercalifragilisticexpialidocious
        pack my box with five dozen liquor jugs
        sphinx of black quartz judge my vow.
      </p>
      <p>
        More text follows after the initial floats to exercise float clearance interactions and
        inline layout. aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
      </p>
    </div>
  </body>
</html>"#,
  },
  Fixture {
    name: "flex_grid_intrinsic_wrap",
    html: r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      body { font-family: sans-serif; font-size: 14px; }
      .root { width: 360px; padding: 8px; }
      .flex {
        display: flex;
        flex-wrap: wrap;
        border: 1px solid #999;
        padding: 4px;
      }
      .item {
        flex: 0 1 auto; /* forces intrinsic measurement in many engines */
        border: 1px solid #ccc;
        padding: 4px 6px;
        margin: 2px;
        background: #f6f6f6;
      }
      .item.long { font-weight: 600; }
      .grid {
        display: grid;
        grid-template-columns: auto auto auto;
        border: 1px solid #999;
        margin-top: 8px;
      }
      .cell {
        border: 1px solid #ddd;
        padding: 4px;
      }
    </style>
  </head>
  <body>
    <div class="root">
      <div class="flex">
        <div class="item">a</div>
        <div class="item long">longer-item</div>
        <div class="item">iii</div>
        <div class="item">WWWWW</div>
        <div class="item">wrap wrap wrap</div>
        <div class="item long">supercalifragilisticexpialidocious</div>
        <div class="item">end</div>
      </div>
      <div class="grid">
        <div class="cell">alpha</div>
        <div class="cell">beta beta</div>
        <div class="cell">gamma</div>
        <div class="cell">delta-delta-delta</div>
        <div class="cell">epsilon</div>
        <div class="cell">zeta zeta zeta</div>
      </div>
    </div>
  </body>
</html>"#,
  },
];

const DEFAULT_DPR: f32 = 1.0;
const BASE_URL: &str = "https://example.com/";

const ENV_VIEWPORT: &str = "FASTR_FRAGMENT_FINGERPRINT_DETERMINISM_VIEWPORT";
const ENV_SCHEDULE: &str = "FASTR_FRAGMENT_FINGERPRINT_DETERMINISM_SCHEDULE";
const ENV_HEAVY: &str = "FASTR_FRAGMENT_FINGERPRINT_DETERMINISM_HEAVY";
const ENV_ALLOW_HEAVY: &str = "FASTR_FRAGMENT_FINGERPRINT_DETERMINISM_ALLOW_HEAVY";

// CI-friendly defaults.
const FAST_VIEWPORT: (u32, u32) = (420, 360);
const FAST_SCHEDULE: &[usize] = &[1, 4, 1, 8];

const THREAD_POOL_STACK_SIZE: usize = 8 * 1024 * 1024;

// Heavier settings (opt-in) for local debugging / chasing subtle nondeterminism.
const HEAVY_VIEWPORT: (u32, u32) = (900, 720);
const HEAVY_SCHEDULE: &[usize] = &[1, 8, 1, 8];

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
    other => Err(format!(
      "expected 1/0/true/false/yes/no/on/off, got '{other}'"
    )),
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
    return Err("schedule cannot be empty (expected e.g. 1,4,1,8)".to_string());
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
      .stack_size(THREAD_POOL_STACK_SIZE)
      .build()
      .map_err(|e| format!("Failed to create {threads}-thread pool: {e}"))?;
    pools.insert(threads, pool);
  }
  Ok(pools)
}

fn diff_dir_for_fixture(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("target/determinism_diffs/fragment_fingerprint_in_process")
    .join(name)
}

fn render_fragment_fingerprint(
  renderer: &mut FastRender,
  html: &str,
  options: &RenderOptions,
) -> Result<fastrender::layout::determinism::LayoutFingerprint, String> {
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      BASE_URL,
      options.clone(),
      RenderArtifactRequest {
        fragment_tree: true,
        ..RenderArtifactRequest::none()
      },
    )
    .map_err(|e| format!("Render failed: {:?}", e))?;

  let fragment_tree = report
    .artifacts
    .fragment_tree
    .as_ref()
    .ok_or_else(|| "missing fragment tree artifact".to_string())?;

  Ok(fragment_tree_fingerprint(fragment_tree))
}

fn capture_snapshot_with_png(
  renderer: &mut FastRender,
  html: &str,
  options: &RenderOptions,
) -> Result<(PipelineSnapshot, RenderDiagnostics, Vec<u8>), String> {
  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      BASE_URL,
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

  let png = encode_image(&report.pixmap, OutputFormat::Png).map_err(|e| format!("{e:?}"))?;

  Ok((
    snapshot_pipeline(dom, styled, box_tree, fragment_tree, display_list),
    report.diagnostics,
    png,
  ))
}

fn write_json_pretty(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
  let json = serde_json::to_string_pretty(value)
    .map_err(|e| format!("serialize {}: {e}", path.display()))?;
  fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

fn run_diff_snapshots(before_dir: &Path, after_dir: &Path, out_dir: &Path) -> Result<(), String> {
  let json_path = out_dir.join("diff_snapshots.json");
  let html_path = out_dir.join("diff_snapshots.html");
  let status = Command::new(env!("CARGO_BIN_EXE_diff_snapshots"))
    .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    .args([
      "--before",
      before_dir
        .to_str()
        .ok_or_else(|| "before dir not utf-8".to_string())?,
      "--after",
      after_dir
        .to_str()
        .ok_or_else(|| "after dir not utf-8".to_string())?,
      "--json",
      json_path
        .to_str()
        .ok_or_else(|| "json path not utf-8".to_string())?,
      "--html",
      html_path
        .to_str()
        .ok_or_else(|| "html path not utf-8".to_string())?,
    ])
    .status()
    .map_err(|e| format!("spawn diff_snapshots: {e}"))?;

  if status.success() {
    return Ok(());
  }

  Err(format!("diff_snapshots failed with status {status}"))
}

fn run_fixture(
  fixture: &Fixture<'_>,
  config: &DeterminismConfig,
  pools: &HashMap<usize, rayon::ThreadPool>,
  options: &RenderOptions,
) -> Result<(), String> {
  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(false)
    .allow_data(true);

  let mut renderer = FastRender::builder()
    .base_url(BASE_URL.to_string())
    .font_sources(FontConfig::bundled_only())
    .resource_policy(policy)
    .build()
    .map_err(|e| format!("Failed to create renderer: {:?}", e))?;

  let mut expected: Option<fastrender::layout::determinism::LayoutFingerprint> = None;
  let mut expected_threads: Option<usize> = None;

  for (idx, &threads) in config.schedule.iter().enumerate() {
    let pool = pools
      .get(&threads)
      .ok_or_else(|| format!("Missing thread pool for {threads} threads"))?;
    let fp = pool.install(|| render_fragment_fingerprint(&mut renderer, fixture.html, options))?;

    if let Some(expected_fp) = expected {
      if fp != expected_fp {
        let output_dir = diff_dir_for_fixture(fixture.name);
        let snapshot_root = output_dir.join("snapshots");
        let before_dir = snapshot_root.join("baseline");
        let after_dir = snapshot_root.join(format!("run_{idx}_threads_{threads}"));

        let expected_threads = expected_threads.unwrap_or(config.schedule[0]);
        let expected_pool = pools
          .get(&expected_threads)
          .ok_or_else(|| format!("Missing thread pool for expected {expected_threads} threads"))?;

        let mut snapshot_error = None::<String>;
        let before_capture =
          expected_pool.install(|| capture_snapshot_with_png(&mut renderer, fixture.html, options));
        let after_capture =
          pool.install(|| capture_snapshot_with_png(&mut renderer, fixture.html, options));

        match (before_capture, after_capture) {
          (
            Ok((before_snapshot, before_diag, before_png)),
            Ok((after_snapshot, after_diag, after_png)),
          ) => {
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
            } else if let Err(err) = fs::write(before_dir.join("render.png"), &before_png)
              .map_err(|e| format!("write render.png: {e}"))
            {
              snapshot_error = Some(err);
            } else if let Err(err) = fs::write(after_dir.join("render.png"), &after_png)
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

        let mut message = String::new();
        message.push_str("Fragment-tree fingerprint mismatch detected.\n");
        message.push_str(&format!("  fixture: {}\n", fixture.name));
        message.push_str(&format!(
          "  baseline: threads={} fingerprint={}\n",
          expected_threads, expected_fp
        ));
        message.push_str(&format!(
          "  failing:  threads={} fingerprint={}\n",
          threads, fp
        ));

        message.push_str("\nSnapshot artifacts:");
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
      expected = Some(fp);
      expected_threads = Some(threads);
    }
  }

  Ok(())
}

#[test]
fn fragment_fingerprint_determinism_in_process() {
  let config =
    DeterminismConfig::from_env().unwrap_or_else(|e| panic!("Invalid determinism config: {e}"));

  let is_heavy_settings = config.viewport == HEAVY_VIEWPORT && config.schedule == HEAVY_SCHEDULE;
  let is_slow_platform = cfg!(target_os = "windows") || cfg!(target_os = "macos");
  let allow_heavy =
    env_flag(ENV_ALLOW_HEAVY).unwrap_or_else(|e| panic!("Invalid {ENV_ALLOW_HEAVY} value: {e}"));
  if is_heavy_settings && is_slow_platform && !allow_heavy {
    eprintln!(
      "Skipping heavy fragment-fingerprint determinism run on this platform (viewport {}x{}, schedule {:?}).\n\
Set {ENV_ALLOW_HEAVY}=1 to run anyway.",
      config.viewport.0, config.viewport.1, config.schedule
    );
    return;
  }

  let options = config.render_options();
  let pools = build_thread_pools(&config.schedule)
    .unwrap_or_else(|e| panic!("Thread pool setup failed: {e}"));

  for fixture in FIXTURES {
    run_fixture(fixture, &config, &pools, &options).unwrap_or_else(|e| {
      panic!(
        "Fixture '{}' failed fragment-tree determinism check (viewport {}x{}, schedule {:?}).\n\
Override via {ENV_VIEWPORT}=<wxh>, {ENV_SCHEDULE}=<csv>, or {ENV_HEAVY}=1 (heavy preset).\n\n{}",
        fixture.name, config.viewport.0, config.viewport.1, config.schedule, e
      )
    });
  }
}
