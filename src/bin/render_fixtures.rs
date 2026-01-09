//! Render offline fixtures under `tests/pages/fixtures/*` to PNGs.
//!
//! This binary is intended for deterministic, offline rendering of imported page fixtures.
//! Network access is denied via `ResourcePolicy` (http/https disabled) and the renderer defaults
//! to bundled fonts.

use fastrender::cli_utils as common;

use clap::Parser;
use common::args::{default_jobs, parse_shard, parse_viewport, AnimationTimeArgs, MediaTypeArg};
use common::prng;
use common::render_pipeline::{
  apply_test_render_delay, compute_soft_timeout_ms, format_error_with_chain, CLI_RENDER_STACK_SIZE,
};
use fastrender::api::{FastRenderPool, FastRenderPoolConfig, RenderArtifactRequest, RenderOptions};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::resource::ResourcePolicy;
use fastrender::text::font_db::FontConfig;
use fastrender::{snapshot_pipeline, PipelineSnapshot, RenderArtifacts};
use image::ImageFormat;
use rustc_hash::FxHasher;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::hash::Hasher;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tiny_skia::{IntSize, Pixmap};
use url::Url;
use walkdir::WalkDir;

const DEFAULT_FIXTURES_DIR: &str = "tests/pages/fixtures";
const DEFAULT_OUT_DIR: &str = "target/fixture_renders";

#[derive(Parser, Debug, Clone)]
#[command(name = "render_fixtures", version, about)]
struct Cli {
  /// Directory containing fixture subdirectories.
  ///
  /// Each fixture is a directory containing an `index.html` entrypoint.
  #[arg(long, default_value = DEFAULT_FIXTURES_DIR)]
  fixtures_dir: PathBuf,

  /// Output directory for PNG renders and logs.
  #[arg(long, default_value = DEFAULT_OUT_DIR)]
  out_dir: PathBuf,

  /// Render only a subset of fixtures (comma-separated stems).
  #[arg(long, value_delimiter = ',')]
  fixtures: Option<Vec<String>>,

  /// Process only a deterministic shard of fixtures (index/total, 0-based).
  #[arg(long, value_parser = parse_shard)]
  shard: Option<(usize, usize)>,

  /// Number of parallel fixture renders.
  #[arg(long, short, default_value_t = default_jobs())]
  jobs: usize,

  /// Viewport size as WxH (e.g., 1040x1240).
  #[arg(long, value_parser = parse_viewport, default_value = "1040x1240")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset.
  #[arg(long, default_value = "1.0")]
  dpr: f32,

  #[command(flatten)]
  animation_time: AnimationTimeArgs,

  /// Media type for evaluating media queries.
  #[arg(long, value_enum, default_value_t = MediaTypeArg::Screen)]
  media: MediaTypeArg,

  /// Expand the paint canvas to fit the laid-out content bounds.
  ///
  /// This is particularly useful for paginated print fixtures, where pages are stacked in the
  /// fragment tree and would otherwise be clipped by the viewport-sized output canvas.
  #[arg(long)]
  fit_canvas_to_content: bool,

  /// Hard per-fixture timeout in seconds.
  #[arg(long, default_value_t = 10)]
  timeout: u64,

  /// Also write `<out-dir>/<stem>/snapshot.json` + diagnostics for later `diff_snapshots`.
  #[arg(long)]
  write_snapshot: bool,

  /// Additional deterministic font directories to load (can be repeated).
  #[arg(long, value_name = "DIR")]
  font_dir: Vec<PathBuf>,

  /// Render each selected fixture N times to detect nondeterminism.
  ///
  /// When `N > 1`, additional repeats run through the same thread pool to surface scheduling-
  /// dependent paint nondeterminism.
  #[arg(long, default_value_t = 1)]
  repeat: usize,

  /// Deterministically shuffle fixture order between repeats (requires `--repeat > 1`).
  #[arg(long)]
  shuffle: bool,

  /// Seed used for deterministic shuffling.
  ///
  /// Has no effect unless `--shuffle` is enabled.
  #[arg(long, default_value_t = 0)]
  seed: u64,

  /// Exit non-zero if any fixture produces different pixels across repeats.
  ///
  /// Requires `--repeat > 1`.
  #[arg(long)]
  fail_on_nondeterminism: bool,

  /// When nondeterminism is detected, save each distinct pixel output as a PNG under:
  /// `<out-dir>/<fixture>/nondeterminism/<k>.png`, plus a small report.
  ///
  /// Requires `--repeat > 1`.
  #[arg(long)]
  save_variants: bool,

  /// Reset paint/filter thread-local scratch buffers before each repeat.
  ///
  /// This is useful when bisecting paint nondeterminism suspected to come from scheduling-dependent
  /// reuse of per-thread scratch buffers.
  #[arg(long)]
  reset_paint_scratch: bool,

  /// Patch fixture HTML before rendering to align with the Chrome baseline harness.
  ///
  /// This injects the same tags used by `xtask chrome-baseline-fixtures`:
  /// - force `color-scheme: light` + white background on `html, body` (for determinism),
  /// - inject a strict Content-Security-Policy (offline invariant),
  /// - disable CSS animations/transitions.
  ///
  /// This flag is primarily intended for `xtask fixture-chrome-diff` (FastRender vs Chrome fixture
  /// diffs). Most other uses of `render_fixtures` should render the raw fixture HTML.
  #[arg(long)]
  patch_html_for_chrome_baseline: bool,

  /// Force a light color scheme + white page background.
  ///
  /// This is useful when diffing against Chrome screenshots captured via `xtask chrome-baseline-fixtures`,
  /// which forces a white background + light color scheme by default (unless `--allow-dark-mode` is set).
  #[arg(long)]
  force_light_mode: bool,
}

#[derive(Clone)]
struct FixtureEntry {
  stem: String,
  index_path: PathBuf,
}

#[derive(Clone)]
struct RenderShared {
  render_pool: FastRenderPool,
  base_options: RenderOptions,
  hard_timeout: Duration,
  timeout_secs: u64,
  media: MediaTypeArg,
  font_config: FontConfig,
  write_snapshot: bool,
  patch_html_for_chrome_baseline: bool,
  out_dir: PathBuf,
  force_light_mode: bool,
}

#[derive(Clone)]
enum Status {
  Ok,
  Crash(String),
  Error(String),
  Timeout(String),
}

#[derive(Clone)]
struct FixtureResult {
  stem: String,
  status: Status,
  time_ms: u128,
  size: Option<usize>,
}

struct RenderOutcome {
  png: Option<Vec<u8>>,
  diagnostics: fastrender::RenderDiagnostics,
  artifacts: RenderArtifacts,
}

#[derive(Clone)]
struct RenderRunOptions {
  write_outputs: bool,
  write_snapshot: bool,
  quiet: bool,
  determinism: Option<DeterminismRun>,
}

#[derive(Clone)]
struct DeterminismRun {
  repeat_idx: usize,
  save_variants: bool,
  determinism: Arc<Mutex<HashMap<String, FixtureDeterminism>>>,
}

#[derive(Clone)]
struct VariantRecord {
  hash_hi: u64,
  hash_lo: u64,
  width: u32,
  height: u32,
  count: usize,
  diff_pixels_vs_baseline: Option<u64>,
  first_mismatch_vs_baseline: Option<(u32, u32)>,
  first_mismatch_rgba_vs_baseline: Option<([u8; 4], [u8; 4])>,
  /// Premultiplied pixel bytes for this variant.
  ///
  /// In repeat mode we avoid storing the baseline pixmap bytes for every fixture, because that can
  /// be multiple gigabytes for a full fixture set. We only keep bytes for non-baseline variants
  /// when `--save-variants` is enabled so we can write them out as PNGs at the end.
  data: Option<Vec<u8>>,
}

struct FixtureDeterminism {
  stem: String,
  variants: Vec<VariantRecord>,
  baseline_rgba: Option<Vec<u8>>,
}

struct RepeatFailure {
  stem: String,
  repeat_idx: usize,
  status: Status,
}

#[derive(Debug, Serialize)]
struct RenderMetadataFile {
  fixture: String,
  viewport: (u32, u32),
  dpr: f32,
  media: MediaMetadata,
  fit_canvas_to_content: bool,
  patch_html_for_chrome_baseline: bool,
  timeout_secs: u64,
  /// SHA-256 hash of `<fixture>/index.html` bytes (computed before any renderer work).
  #[serde(skip_serializing_if = "Option::is_none")]
  input_sha256: Option<String>,
  /// Deterministic SHA-256 hash over all regular files under the fixture directory.
  ///
  /// See `hash_fixture_dir_sha256` for the hashing algorithm.
  #[serde(skip_serializing_if = "Option::is_none")]
  fixture_dir_sha256: Option<String>,
  bundled_fonts: bool,
  font_dirs: Vec<PathBuf>,
  status: &'static str,
  elapsed_ms: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  blocked_network_urls: Option<BlockedNetworkUrlsMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum MediaMetadata {
  Screen,
  Print,
}

impl MediaMetadata {
  fn from_arg(media: MediaTypeArg) -> Self {
    match media {
      MediaTypeArg::Screen => Self::Screen,
      MediaTypeArg::Print => Self::Print,
    }
  }
}

#[derive(Debug, Serialize)]
struct BlockedNetworkUrlsMetadata {
  count: usize,
  sample: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DiagnosticsFile {
  fixture: String,
  status: String,
  error: Option<String>,
  time_ms: u128,
  png_size: Option<usize>,
  diagnostics: fastrender::RenderDiagnostics,
}

fn main() {
  let cli = Cli::parse();
  if let Err(err) = run(cli) {
    eprintln!("{err}");
    std::process::exit(1);
  }
}

fn fixture_runtime_toggles() -> RuntimeToggles {
  let mut raw = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<_, _>>();
  raw
    .entry("FASTR_DETERMINISTIC_PAINT".to_string())
    .or_insert_with(|| "1".to_string());
  // Fixture diffs compare against a Chrome snapshot that has had time to load local web fonts.
  // Wait briefly so `font-display: swap` faces become active deterministically for the render.
  raw
    .entry("FASTR_WEB_FONT_WAIT_MS".to_string())
    .or_insert_with(|| "500".to_string());
  RuntimeToggles::from_map(raw)
}

fn run(cli: Cli) -> io::Result<()> {
  if cli.jobs == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "jobs must be > 0",
    ));
  }
  if cli.timeout == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "timeout must be > 0",
    ));
  }
  if cli.repeat == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "repeat must be >= 1",
    ));
  }
  if !cli.dpr.is_finite() || cli.dpr <= 0.0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "dpr must be a finite number > 0",
    ));
  }
  if cli.repeat == 1 {
    if cli.shuffle {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "shuffle requires --repeat > 1",
      ));
    }
    if cli.fail_on_nondeterminism {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "fail-on-nondeterminism requires --repeat > 1",
      ));
    }
    if cli.save_variants {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "save-variants requires --repeat > 1",
      ));
    }
  }

  fs::create_dir_all(&cli.out_dir)?;

  let mut fixtures = discover_fixtures(&cli.fixtures_dir)?;
  if fixtures.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!(
        "No fixtures found under {} (expected directories containing index.html)",
        cli.fixtures_dir.display()
      ),
    ));
  }

  fixtures.sort_by(|a, b| a.stem.cmp(&b.stem));

  if let Some(selected) = &cli.fixtures {
    let wanted: HashSet<String> = selected.iter().map(|s| s.trim().to_string()).collect();
    let mut missing: Vec<String> = wanted
      .iter()
      .filter(|stem| !fixtures.iter().any(|f| &f.stem == *stem))
      .cloned()
      .collect();
    missing.sort();
    if !missing.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Unknown fixtures: {}", missing.join(", ")),
      ));
    }
    fixtures.retain(|f| wanted.contains(&f.stem));
  }

  if let Some((idx, total)) = cli.shard {
    fixtures = fixtures
      .into_iter()
      .enumerate()
      .filter(|(i, _)| i % total == idx)
      .map(|(_, f)| f)
      .collect();
  }

  if fixtures.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      "No fixtures selected after filtering/sharding",
    ));
  }

  let hard_timeout = Duration::from_secs(cli.timeout);
  let soft_timeout_ms = compute_soft_timeout_ms(hard_timeout, None);

  let font_config = {
    let mut config = FontConfig::bundled_only();
    if !cli.font_dir.is_empty() {
      config = config.with_font_dirs(cli.font_dir.clone());
    }
    config
  };

  let resource_policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false);
  let render_config = fastrender::api::FastRenderConfig::new()
    .with_default_viewport(cli.viewport.0, cli.viewport.1)
    .with_device_pixel_ratio(cli.dpr)
    .with_meta_viewport(true)
    .with_resource_policy(resource_policy)
    .with_font_sources(font_config.clone())
    .with_runtime_toggles(fixture_runtime_toggles());

  let render_pool = FastRenderPool::with_config(
    FastRenderPoolConfig::new()
      .with_renderer_config(render_config)
      .with_pool_size(cli.jobs),
  )
  .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

  let mut base_options = RenderOptions::new()
    .with_viewport(cli.viewport.0, cli.viewport.1)
    .with_device_pixel_ratio(cli.dpr)
    .with_media_type(cli.media.as_media_type());
  if let Some(time_ms) = cli.animation_time.animation_time_ms() {
    base_options = base_options.with_animation_time(time_ms);
  }
  if cli.fit_canvas_to_content {
    base_options = base_options.with_fit_canvas_to_content(true);
  }
  if let Some(ms) = soft_timeout_ms {
    if ms > 0 {
      base_options.timeout = Some(Duration::from_millis(ms));
    }
  }

  let shared = RenderShared {
    render_pool,
    base_options,
    hard_timeout,
    timeout_secs: cli.timeout,
    media: cli.media,
    font_config,
    write_snapshot: cli.write_snapshot,
    patch_html_for_chrome_baseline: cli.patch_html_for_chrome_baseline,
    out_dir: cli.out_dir.clone(),
    force_light_mode: cli.force_light_mode,
  };

  println!(
    "Rendering {} fixtures ({} parallel) to {}",
    fixtures.len(),
    cli.jobs,
    cli.out_dir.display()
  );
  if let Some((idx, total)) = cli.shard {
    println!("Shard: {idx}/{total}");
  }
  println!(
    "Viewport: {}x{} dpr={} media={:?} fit_canvas_to_content={} timeout={}s",
    cli.viewport.0,
    cli.viewport.1,
    cli.dpr,
    cli.media.as_media_type(),
    cli.fit_canvas_to_content,
    cli.timeout
  );
  if cli.repeat > 1 {
    println!(
      "Determinism: repeat={} shuffle={}{} reset_paint_scratch={}",
      cli.repeat,
      cli.shuffle,
      if cli.shuffle {
        format!(" seed={}", cli.seed)
      } else {
        String::new()
      },
      cli.reset_paint_scratch
    );
  }
  println!();

  let start = Instant::now();
  let thread_pool = rayon::ThreadPoolBuilder::new()
    .num_threads(cli.jobs)
    .build()
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

  let mut results: Vec<FixtureResult>;
  let mut determinism: HashMap<String, FixtureDeterminism> = HashMap::new();
  let mut repeat_failures: Vec<RepeatFailure> = Vec::new();

  if cli.repeat == 1 {
    if cli.reset_paint_scratch {
      reset_paint_scratch_for_pools(&thread_pool);
    }
    let results_mutex: Mutex<Vec<FixtureResult>> = Mutex::new(Vec::new());
    thread_pool.scope(|s| {
      for entry in fixtures {
        let shared = shared.clone();
        let results = &results_mutex;
        s.spawn(move |_| {
          let run = render_fixture(
            &shared,
            &entry,
            RenderRunOptions {
              write_outputs: true,
              write_snapshot: shared.write_snapshot,
              quiet: false,
              determinism: None,
            },
          );
          lock_mutex(results).push(run);
        });
      }
    });
    results = match results_mutex.into_inner() {
      Ok(results) => results,
      Err(poisoned) => poisoned.into_inner(),
    };
    results.sort_by(|a, b| a.stem.cmp(&b.stem));
  } else {
    // Render repeat 0 with full outputs (PNG/log/snapshot), then run additional repeats through the
    // same rayon pool to surface any scheduling-dependent nondeterminism.
    let results_mutex: Mutex<Vec<FixtureResult>> = Mutex::new(Vec::new());
    let repeat_failures_mutex: Mutex<Vec<RepeatFailure>> = Mutex::new(Vec::new());
    let determinism_mutex: Arc<Mutex<HashMap<String, FixtureDeterminism>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let skip_mutex: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    for repeat_idx in 0..cli.repeat {
      if cli.reset_paint_scratch {
        reset_paint_scratch_for_pools(&thread_pool);
      }
      let mut ordered = fixtures.clone();
      if repeat_idx > 0 {
        // If the baseline run for a fixture failed we cannot compare pixel outputs, so skip it in
        // later repeats. Additionally, if a fixture times out in any repeat, avoid spawning more
        // render worker threads for it; timed-out workers continue running and would otherwise
        // accumulate across repeats.
        let skipped = lock_mutex(&skip_mutex);
        if !skipped.is_empty() {
          ordered.retain(|entry| !skipped.contains(&entry.stem));
        }
      }
      if cli.shuffle && repeat_idx > 0 {
        let seed = cli
          .seed
          .wrapping_add((repeat_idx as u64).wrapping_mul(0x9E3779B97F4A7C15));
        prng::shuffle(&mut ordered, seed);
      }

      if repeat_idx > 0 {
        println!("--- Repeat {}/{} ---", repeat_idx + 1, cli.repeat);
      }

      let write_outputs = repeat_idx == 0;
      let write_snapshot = write_outputs && shared.write_snapshot;

      thread_pool.scope(|s| {
        for entry in ordered {
          let shared = shared.clone();
          let results = &results_mutex;
          let repeat_failures = &repeat_failures_mutex;
          let determinism = Arc::clone(&determinism_mutex);
          let skip = Arc::clone(&skip_mutex);
          let save_variants = cli.save_variants;
          s.spawn(move |_| {
            let run = render_fixture(
              &shared,
              &entry,
              RenderRunOptions {
                write_outputs,
                write_snapshot,
                quiet: !write_outputs,
                determinism: Some(DeterminismRun {
                  repeat_idx,
                  save_variants,
                  determinism,
                }),
              },
            );
            if write_outputs {
              lock_mutex(results).push(run.clone());
            }

            if write_outputs {
              if !matches!(run.status, Status::Ok) {
                lock_mutex(&skip).insert(run.stem.clone());
              }
            } else if matches!(run.status, Status::Timeout(_)) {
              lock_mutex(&skip).insert(run.stem.clone());
            }

            if repeat_idx > 0 && !matches!(run.status, Status::Ok) {
              lock_mutex(repeat_failures).push(RepeatFailure {
                stem: run.stem.clone(),
                repeat_idx,
                status: run.status.clone(),
              });
            }
          });
        }
      });
    }

    results = match results_mutex.into_inner() {
      Ok(results) => results,
      Err(poisoned) => poisoned.into_inner(),
    };
    results.sort_by(|a, b| a.stem.cmp(&b.stem));
    // Do not `try_unwrap` here: timeouts can leave the worker thread alive (and still holding an
    // `Arc` clone) even after the harness marks the fixture as timed out.
    determinism = {
      let mut guard = lock_mutex(&determinism_mutex);
      std::mem::take(&mut *guard)
    };
    repeat_failures = match repeat_failures_mutex.into_inner() {
      Ok(failures) => failures,
      Err(poisoned) => poisoned.into_inner(),
    };
    repeat_failures.sort_by(|a, b| {
      a.stem
        .cmp(&b.stem)
        .then_with(|| a.repeat_idx.cmp(&b.repeat_idx))
    });
  }

  // We only need the decoded baseline PNG while collecting variants. Drop it before building the
  // summary / saving variant outputs to keep peak memory use low when debugging large fixtures.
  for state in determinism.values_mut() {
    state.baseline_rgba = None;
  }

  let total_elapsed = start.elapsed();

  let pass = results
    .iter()
    .filter(|r| matches!(r.status, Status::Ok))
    .count();
  let timeout = results
    .iter()
    .filter(|r| matches!(r.status, Status::Timeout(_)))
    .count();
  let crash = results
    .iter()
    .filter(|r| matches!(r.status, Status::Crash(_)))
    .count();
  let error = results
    .iter()
    .filter(|r| matches!(r.status, Status::Error(_)))
    .count();

  let mut nondeterministic_stems: Vec<String> = Vec::new();
  if cli.repeat > 1 {
    nondeterministic_stems = determinism
      .values()
      .filter(|state| state.variants.len() > 1)
      .map(|state| state.stem.clone())
      .collect();
    nondeterministic_stems.sort();
  }
  let nondeterministic_count = nondeterministic_stems.len();
  let repeat_failure_count = repeat_failures.len();

  let summary_path = cli.out_dir.join("_summary.log");
  let mut summary = String::new();
  let _ = writeln!(summary, "=== Fixture Render Summary ===");
  let _ = writeln!(summary, "Total time: {:.1}s", total_elapsed.as_secs_f64());
  let _ = writeln!(
    summary,
    "Fixtures: {} total, {} pass, {} timeout, {} crash, {} error\n",
    results.len(),
    pass,
    timeout,
    crash,
    error
  );
  let _ = writeln!(
    summary,
    "{:<40} {:>8} {:>10} STATUS",
    "FIXTURE", "TIME", "SIZE"
  );
  let _ = writeln!(summary, "{}", "-".repeat(75));
  for r in &results {
    let status_str = match &r.status {
      Status::Ok => "OK".to_string(),
      Status::Crash(msg) => format!("CRASH: {}", msg.chars().take(30).collect::<String>()),
      Status::Error(msg) => format!("ERROR: {}", msg.chars().take(30).collect::<String>()),
      Status::Timeout(msg) => format!("TIMEOUT: {}", msg.chars().take(30).collect::<String>()),
    };
    let size_str = r
      .size
      .map(|s| format!("{s}b"))
      .unwrap_or_else(|| "-".to_string());
    let _ = writeln!(
      summary,
      "{:<40} {:>6}ms {:>10} {}",
      r.stem, r.time_ms, size_str, status_str
    );
  }
  let _ = writeln!(summary, "\n{}", "-".repeat(75));
  let _ = writeln!(summary, "Total: {:.1}s", total_elapsed.as_secs_f64());

  if cli.repeat > 1 {
    let _ = writeln!(summary, "\n=== Determinism Check ===");
    let _ = writeln!(
      summary,
      "repeat={} shuffle={}{} reset_paint_scratch={}",
      cli.repeat,
      cli.shuffle,
      if cli.shuffle {
        format!(" seed={}", cli.seed)
      } else {
        String::new()
      },
      cli.reset_paint_scratch
    );

    if repeat_failure_count > 0 {
      let _ = writeln!(summary, "Repeat failures: {repeat_failure_count}");
      let _ = writeln!(summary, "{:<40} {:>8} STATUS", "FIXTURE", "REPEAT");
      let _ = writeln!(summary, "{}", "-".repeat(65));
      for failure in &repeat_failures {
        let status_str = match &failure.status {
          Status::Ok => "OK".to_string(),
          Status::Crash(msg) => format!("CRASH: {}", msg.chars().take(30).collect::<String>()),
          Status::Error(msg) => format!("ERROR: {}", msg.chars().take(30).collect::<String>()),
          Status::Timeout(msg) => format!("TIMEOUT: {}", msg.chars().take(30).collect::<String>()),
        };
        let _ = writeln!(
          summary,
          "{:<40} {:>8} {}",
          failure.stem, failure.repeat_idx, status_str
        );
      }
    } else {
      let _ = writeln!(summary, "Repeat failures: 0");
    }

    if nondeterministic_count > 0 {
      let _ = writeln!(
        summary,
        "Nondeterministic fixtures: {nondeterministic_count}"
      );
      let _ = writeln!(
        summary,
        "{:<40} {:>10} {:>12}",
        "FIXTURE", "VARIANTS", "MAX_DIFF_PX"
      );
      let _ = writeln!(summary, "{}", "-".repeat(70));
      for stem in &nondeterministic_stems {
        if let Some(state) = determinism.get(stem) {
          let max_diff = if state
            .variants
            .iter()
            .skip(1)
            .any(|v| v.diff_pixels_vs_baseline.is_none())
          {
            "-".to_string()
          } else {
            state
              .variants
              .iter()
              .filter_map(|v| v.diff_pixels_vs_baseline)
              .max()
              .unwrap_or(0)
              .to_string()
          };
          let _ = writeln!(
            summary,
            "{:<40} {:>10} {:>12}",
            stem,
            state.variants.len(),
            max_diff
          );
        }
      }
    } else {
      let _ = writeln!(summary, "Nondeterministic fixtures: 0");
    }
  }

  let _ = fs::write(&summary_path, &summary);

  println!();
  println!(
    "Done in {:.1}s: ✓{} pass, ⏱{} timeout, ✗{} crash, ✗{} error",
    total_elapsed.as_secs_f64(),
    pass,
    timeout,
    crash,
    error
  );
  if cli.repeat > 1 {
    println!(
      "Determinism: {} repeat failures, {} nondeterministic fixtures",
      repeat_failure_count, nondeterministic_count
    );
  }
  println!("Summary: {}", summary_path.display());
  println!("Renders:  {}/<fixture>.png", cli.out_dir.display());
  println!("Logs:     {}/<fixture>.log", cli.out_dir.display());
  println!("Metadata: {}/<fixture>.json", cli.out_dir.display());
  if cli.write_snapshot {
    println!(
      "Snapshots:{}/<fixture>/snapshot.json",
      cli.out_dir.display()
    );
  }

  if cli.repeat > 1 && nondeterministic_count > 0 {
    println!("\n=== Nondeterminism Summary ===");
    println!("{:<40} {:>10} {:>12}", "FIXTURE", "VARIANTS", "MAX_DIFF_PX");
    println!("{}", "-".repeat(70));
    for stem in &nondeterministic_stems {
      if let Some(state) = determinism.get(stem) {
        let max_diff = if state
          .variants
          .iter()
          .skip(1)
          .any(|v| v.diff_pixels_vs_baseline.is_none())
        {
          "-".to_string()
        } else {
          state
            .variants
            .iter()
            .filter_map(|v| v.diff_pixels_vs_baseline)
            .max()
            .unwrap_or(0)
            .to_string()
        };
        println!("{:<40} {:>10} {:>12}", stem, state.variants.len(), max_diff);
      }
    }
  }

  let mut wrote_variants_ok = true;
  if cli.repeat > 1 && cli.save_variants && nondeterministic_count > 0 {
    for stem in &nondeterministic_stems {
      let Some(state) = determinism.get_mut(stem) else {
        continue;
      };
      if let Err(err) = write_nondeterminism_outputs(&cli.out_dir, stem, state, &cli) {
        eprintln!("Failed to save variants for {stem}: {err}");
        wrote_variants_ok = false;
      }
    }
  }

  if timeout > 0
    || crash > 0
    || error > 0
    || repeat_failure_count > 0
    || !wrote_variants_ok
    || (cli.fail_on_nondeterminism && nondeterministic_count > 0)
  {
    std::process::exit(1);
  }

  Ok(())
}

fn discover_fixtures(fixtures_dir: &Path) -> io::Result<Vec<FixtureEntry>> {
  let mut fixtures = Vec::new();
  for entry in fs::read_dir(fixtures_dir)? {
    let entry = entry?;
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let index_path = path.join("index.html");
    if !index_path.is_file() {
      continue;
    }
    let stem = entry.file_name().to_string_lossy().into_owned();
    fixtures.push(FixtureEntry { stem, index_path });
  }
  Ok(fixtures)
}

fn log_path_for(out_dir: &Path, stem: &str) -> PathBuf {
  out_dir.join(format!("{stem}.log"))
}

fn output_path_for(out_dir: &Path, stem: &str) -> PathBuf {
  out_dir.join(format!("{stem}.png"))
}

fn metadata_path_for(out_dir: &Path, stem: &str) -> PathBuf {
  out_dir.join(format!("{stem}.json"))
}

fn snapshot_dir_for(out_dir: &Path, stem: &str) -> PathBuf {
  out_dir.join(stem)
}

fn panic_to_string(panic: Box<dyn std::any::Any + Send + 'static>) -> String {
  panic
    .downcast_ref::<&str>()
    .map(|s| s.to_string())
    .or_else(|| panic.downcast_ref::<String>().cloned())
    .unwrap_or_else(|| "unknown panic".to_string())
}

fn status_label(status: &Status) -> &'static str {
  match status {
    Status::Ok => "ok",
    Status::Crash(_) => "crash",
    Status::Error(_) => "error",
    Status::Timeout(_) => "timeout",
  }
}

fn status_error(status: &Status) -> Option<&str> {
  match status {
    Status::Crash(msg) | Status::Error(msg) | Status::Timeout(msg) => Some(msg.as_str()),
    Status::Ok => None,
  }
}

fn reset_paint_scratch_for_pools(harness_pool: &rayon::ThreadPool) {
  // This is best-effort. Paint/filter code can run on:
  // - the calling thread (serial paths),
  // - the global Rayon thread pool (default for parallel paint),
  // - a dedicated paint pool (when `FASTR_PAINT_THREADS>1` is set),
  // - or a custom harness pool used by CLI tools.
  //
  // We reset scratch on the calling thread, the global pool, the dedicated paint pool, and the
  // fixture harness pool.
  fastrender::paint::scratch::reset_paint_scratch_best_effort();
  harness_pool.install(|| {
    rayon::broadcast(|_| {
      fastrender::paint::scratch::reset_thread_local_scratch();
    });
  });
}

fn hash64_with_salt(bytes: &[u8], salt: u64) -> u64 {
  let mut hasher = FxHasher::default();
  hasher.write_u64(salt);
  hasher.write(bytes);
  hasher.finish()
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn hash128(bytes: &[u8]) -> (u64, u64) {
  (hash64_with_salt(bytes, 0), hash64_with_salt(bytes, 1))
}

fn diff_premultiplied_against_rgba_baseline(
  baseline_rgba: &[u8],
  other_premultiplied: &[u8],
  width: u32,
) -> (u64, Option<(u32, u32)>, Option<([u8; 4], [u8; 4])>) {
  if baseline_rgba.len() != other_premultiplied.len() || width == 0 {
    return (0, None, None);
  }

  let mut diff_pixels = 0u64;
  let mut first_mismatch = None;
  let mut first_mismatch_rgba = None;

  for (idx, (baseline_px, other_px)) in baseline_rgba
    .chunks_exact(4)
    .zip(other_premultiplied.chunks_exact(4))
    .enumerate()
  {
    let r = other_px[0];
    let g = other_px[1];
    let b = other_px[2];
    let a = other_px[3];

    // tiny-skia stores premultiplied RGBA bytes. Convert to straight RGBA to match the baseline PNG
    // decode.
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
    let other_rgba = [r, g, b, a];
    let baseline_arr = [
      baseline_px[0],
      baseline_px[1],
      baseline_px[2],
      baseline_px[3],
    ];

    if baseline_arr != other_rgba {
      diff_pixels += 1;
      if first_mismatch.is_none() {
        let pixel_idx = idx as u32;
        let xy = (pixel_idx % width, pixel_idx / width);
        first_mismatch = Some(xy);
        first_mismatch_rgba = Some((baseline_arr, other_rgba));
      }
    }
  }

  (diff_pixels, first_mismatch, first_mismatch_rgba)
}

fn record_variant(
  state: &mut FixtureDeterminism,
  width: u32,
  height: u32,
  hash_hi: u64,
  hash_lo: u64,
  premultiplied: &[u8],
  out_dir: &Path,
  save_variant_bytes: bool,
) {
  for variant in &mut state.variants {
    if variant.hash_hi == hash_hi
      && variant.hash_lo == hash_lo
      && variant.width == width
      && variant.height == height
    {
      // The hashes are our primary key, but confirm equality with the stored bytes when available
      // (e.g. when `--save-variants` is enabled). This keeps the variant tracker effectively exact
      // while still avoiding retaining baseline pixmap bytes for every fixture.
      if let Some(existing) = variant.data.as_deref() {
        if existing == premultiplied {
          variant.count += 1;
          return;
        }
        // Hash collision (vanishingly unlikely); fall through and treat this as a new variant.
        continue;
      }

      variant.count += 1;
      return;
    }
  }

  let Some(baseline) = state.variants.first() else {
    // If we don't have a baseline variant yet (e.g. baseline run failed, or state was reset),
    // treat this as the baseline so we can continue collecting deterministic diagnostics without
    // crashing the whole run.
    state.variants.push(VariantRecord {
      hash_hi,
      hash_lo,
      width,
      height,
      count: 1,
      diff_pixels_vs_baseline: None,
      first_mismatch_vs_baseline: None,
      first_mismatch_rgba_vs_baseline: None,
      data: if save_variant_bytes {
        Some(premultiplied.to_vec())
      } else {
        None
      },
    });
    return;
  };

  let mut diff_pixels = None;
  let mut first_mismatch = None;
  let mut first_mismatch_rgba = None;

  if baseline.width == width && baseline.height == height {
    // Compare against the baseline PNG output so we don't need to hold the baseline pixmap bytes in
    // memory for every fixture in repeat mode.
    if state.baseline_rgba.is_none() {
      let baseline_png = output_path_for(out_dir, &state.stem);
      let decoded = fs::read(&baseline_png).ok().and_then(|png_bytes| {
        let img = image::load_from_memory_with_format(&png_bytes, ImageFormat::Png).ok()?;
        let img = img.to_rgba8();
        if img.width() != width || img.height() != height {
          return None;
        }
        Some(img.into_raw())
      });
      if let Some(bytes) = decoded {
        state.baseline_rgba = Some(bytes);
      }
    }

    if let Some(baseline_rgba) = state.baseline_rgba.as_deref() {
      if baseline_rgba.len() == premultiplied.len() && width > 0 {
        let (d, mismatch, mismatch_rgba) =
          diff_premultiplied_against_rgba_baseline(baseline_rgba, premultiplied, width);
        diff_pixels = Some(d);
        first_mismatch = mismatch;
        first_mismatch_rgba = mismatch_rgba;
      }
    }
  }

  state.variants.push(VariantRecord {
    hash_hi,
    hash_lo,
    width,
    height,
    count: 1,
    diff_pixels_vs_baseline: diff_pixels,
    first_mismatch_vs_baseline: first_mismatch,
    first_mismatch_rgba_vs_baseline: first_mismatch_rgba,
    data: if save_variant_bytes {
      Some(premultiplied.to_vec())
    } else {
      None
    },
  });
}

fn write_nondeterminism_outputs(
  out_dir: &Path,
  stem: &str,
  state: &mut FixtureDeterminism,
  cli: &Cli,
) -> io::Result<()> {
  if state.variants.len() > 2 {
    // Variants are discovered in parallel across fixture jobs, so their insertion order can depend
    // on scheduling. Sort them for stable `nondeterminism/<k>.png` numbering and report output.
    state.variants[1..].sort_by(|a, b| {
      a.hash_hi
        .cmp(&b.hash_hi)
        .then_with(|| a.hash_lo.cmp(&b.hash_lo))
        .then_with(|| a.width.cmp(&b.width))
        .then_with(|| a.height.cmp(&b.height))
    });
  }

  let fixture_dir = snapshot_dir_for(out_dir, stem);
  let nondet_dir = fixture_dir.join("nondeterminism");
  // Clear any existing nondeterminism artifacts so reruns don't leave behind stale variant files
  // (e.g. a previous run found more variants than the current run).
  if nondet_dir.exists() {
    fs::remove_dir_all(&nondet_dir)?;
  }
  fs::create_dir_all(&nondet_dir)?;

  // Variant 0 is the baseline output already written by the main render pass.
  let baseline_png = output_path_for(out_dir, stem);
  if !baseline_png.is_file() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!("baseline PNG not found at {}", baseline_png.display()),
    ));
  }
  fs::copy(&baseline_png, nondet_dir.join("0.png"))?;

  let mut report = String::new();
  let _ = writeln!(report, "fixture: {stem}");
  let _ = writeln!(report, "repeat: {}", cli.repeat);
  let _ = writeln!(report, "jobs: {}", cli.jobs);
  let _ = writeln!(report, "shuffle: {}", cli.shuffle);
  if cli.shuffle {
    let _ = writeln!(report, "seed: {}", cli.seed);
  }
  let _ = writeln!(report, "variants: {}", state.variants.len());
  let _ = writeln!(report);

  for (idx, variant) in state.variants.iter().enumerate() {
    let _ = writeln!(
      report,
      "variant {idx}: hash=0x{hash_hi:016x}{hash_lo:016x} count={count} dims={w}x{h}",
      hash_hi = variant.hash_hi,
      hash_lo = variant.hash_lo,
      count = variant.count,
      w = variant.width,
      h = variant.height
    );
    if idx == 0 {
      continue;
    }
    if let Some(diff) = variant.diff_pixels_vs_baseline {
      let _ = writeln!(report, "  diff_pixels_vs_baseline={diff}");
    }
    if let Some((x, y)) = variant.first_mismatch_vs_baseline {
      let _ = writeln!(report, "  first_mismatch_vs_baseline=({x}, {y})");
      if let Some((b, v)) = variant.first_mismatch_rgba_vs_baseline {
        let _ = writeln!(
          report,
          "  baseline_rgba=[{}, {}, {}, {}] variant_rgba=[{}, {}, {}, {}]",
          b[0], b[1], b[2], b[3], v[0], v[1], v[2], v[3]
        );
      }
    }
  }

  let report_path = nondet_dir.join("report.txt");
  fs::write(&report_path, report)?;

  for idx in 1..state.variants.len() {
    let variant = &mut state.variants[idx];
    let Some(bytes) = variant.data.take() else {
      continue;
    };
    let size = IntSize::from_wh(variant.width, variant.height).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "invalid pixmap size {}x{} for {stem} variant {idx}",
          variant.width, variant.height
        ),
      )
    })?;
    let pixmap = Pixmap::from_vec(bytes, size).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid pixmap bytes for {stem} variant {idx}"),
      )
    })?;
    let png = encode_image(&pixmap, OutputFormat::Png)
      .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(nondet_dir.join(format!("{idx}.png")), png)?;
  }

  Ok(())
}

fn render_fixture(
  shared: &RenderShared,
  entry: &FixtureEntry,
  opts: RenderRunOptions,
) -> FixtureResult {
  let stem = entry.stem.clone();
  let log_path = log_path_for(&shared.out_dir, &stem);
  let output_path = output_path_for(&shared.out_dir, &stem);
  let metadata_path = metadata_path_for(&shared.out_dir, &stem);
  let snapshot_dir = snapshot_dir_for(&shared.out_dir, &stem);

  let mut log = String::new();
  if opts.write_outputs || opts.write_snapshot {
    log = format!(
      "=== {stem} ===
"
    );
    let _ = writeln!(log, "Entrypoint: {}", entry.index_path.display());
    let viewport = shared.base_options.viewport.unwrap_or((0, 0));
    let _ = writeln!(log, "Viewport: {}x{}", viewport.0, viewport.1);
    let _ = writeln!(
      log,
      "DPR: {}",
      shared.base_options.device_pixel_ratio.unwrap_or(1.0)
    );
  }

  let base_url = match canonical_file_url(&entry.index_path) {
    Ok(url) => url,
    Err(err) => {
      let status = Status::Error(format!("base_url: {err}"));
      if opts.write_outputs {
        let _ = writeln!(log, "Base URL error: {err}");
        let _ = write_render_metadata_file(
          &metadata_path,
          &stem,
          shared,
          &status,
          0,
          None,
          None,
          None,
          &mut log,
        );
        let _ = fs::write(&log_path, log);
      }
      return FixtureResult {
        stem,
        status,
        time_ms: 0,
        size: None,
      };
    }
  };
  if !log.is_empty() {
    let _ = writeln!(log, "Base URL: {base_url}");
  }

  let mut html_bytes = match fs::read(&entry.index_path) {
    Ok(html) => html,
    Err(err) => {
      let status = Status::Error(format!("read: {err}"));
      if opts.write_outputs {
        let _ = writeln!(log, "Read error: {err}");
        let _ = write_render_metadata_file(
          &metadata_path,
          &stem,
          shared,
          &status,
          0,
          None,
          None,
          None,
          &mut log,
        );
        let _ = fs::write(&log_path, log);
      }
      return FixtureResult {
        stem,
        status,
        time_ms: 0,
        size: None,
      };
    }
  };

  let mut input_sha256: Option<String> = None;
  let mut fixture_dir_sha256: Option<String> = None;

  if opts.write_outputs {
    input_sha256 = Some(sha256_hex(&html_bytes));

    fixture_dir_sha256 = match entry.index_path.parent() {
      Some(dir) => match hash_fixture_dir_sha256(dir) {
        Ok(hash) => Some(hash),
        Err(err) => {
          let status = Status::Error(format!("fixture_dir_hash: {err}"));
          let _ = writeln!(log, "Fixture dir hash error: {err}");
          let _ = write_render_metadata_file(
            &metadata_path,
            &stem,
            shared,
            &status,
            0,
            None,
            input_sha256.as_deref(),
            None,
            &mut log,
          );
          let _ = fs::write(&log_path, log);
          return FixtureResult {
            stem,
            status,
            time_ms: 0,
            size: None,
          };
        }
      },
      None => {
        let status = Status::Error("fixture_dir_hash: missing fixture dir parent".to_string());
        let _ = writeln!(
          log,
          "Fixture dir hash error: missing fixture directory parent"
        );
        let _ = write_render_metadata_file(
          &metadata_path,
          &stem,
          shared,
          &status,
          0,
          None,
          input_sha256.as_deref(),
          None,
          &mut log,
        );
        let _ = fs::write(&log_path, log);
        return FixtureResult {
          stem,
          status,
          time_ms: 0,
          size: None,
        };
      }
    };

    if let Some(hash) = input_sha256.as_deref() {
      let _ = writeln!(log, "Input SHA-256: {hash}");
    }
    if let Some(hash) = fixture_dir_sha256.as_deref() {
      let _ = writeln!(log, "Fixture dir SHA-256: {hash}");
    }
  }

  if shared.patch_html_for_chrome_baseline {
    if !log.is_empty() {
      let _ = writeln!(log, "HTML patch: chrome_baseline");
    }
    // Match `xtask chrome-baseline-fixtures`: enforce a deterministic light color scheme and
    // offline CSP in the input HTML so FastRender/Chrome diffs don't get dominated by theme
    // variance (e.g. non-white root backgrounds on some sites).
    html_bytes = common::fixture_html_patch::patch_html_bytes(
      &html_bytes,
      Some(&base_url),
      true,  // disable JS
      true,  // disable animations
      false, // force light mode
    );
  }

  let html = match String::from_utf8(html_bytes) {
    Ok(html) => html,
    Err(err) => {
      let status = Status::Error(format!("decode_utf8: {err}"));
      if opts.write_outputs {
        let _ = writeln!(log, "UTF-8 decode error: {err}");
        let _ = write_render_metadata_file(
          &metadata_path,
          &stem,
          shared,
          &status,
          0,
          None,
          input_sha256.as_deref(),
          fixture_dir_sha256.as_deref(),
          &mut log,
        );
        let _ = fs::write(&log_path, log);
      }
      return FixtureResult {
        stem,
        status,
        time_ms: 0,
        size: None,
      };
    }
  };

  let html = if shared.force_light_mode {
    fastrender::css::loader::inject_css_into_html(
      &html,
      "html, body { background: white !important; color-scheme: light !important; forced-color-adjust: none !important; }",
    )
  } else {
    html
  };

  let page_start = Instant::now();

  let render_pool = shared.render_pool.clone();
  let options = shared.base_options.clone();
  let artifact_request = if opts.write_snapshot {
    RenderArtifactRequest::summary()
  } else {
    RenderArtifactRequest::none()
  };
  let encode_png = opts.write_outputs;
  let determinism = opts.determinism.clone();
  let stem_for_determinism = stem.clone();
  let out_dir_for_determinism = shared.out_dir.clone();

  let render_work = move || -> Result<RenderOutcome, fastrender::Error> {
    apply_test_render_delay(Some(&stem_for_determinism));
    let report = render_pool.with_renderer(|renderer| {
      renderer.render_html_with_stylesheets_report(&html, &base_url, options, artifact_request)
    })?;

    if let Some(determinism) = determinism.as_ref() {
      if blocked_network_urls(&report.diagnostics).is_empty() {
        let width = report.pixmap.width();
        let height = report.pixmap.height();
        let data = report.pixmap.data();
        let (hash_hi, hash_lo) = hash128(data);

        let mut determinism_map = lock_mutex(&determinism.determinism);
        if determinism.repeat_idx == 0 {
          determinism_map
            .entry(stem_for_determinism.clone())
            .or_insert_with(|| FixtureDeterminism {
              stem: stem_for_determinism.clone(),
              variants: vec![VariantRecord {
                hash_hi,
                hash_lo,
                width,
                height,
                count: 1,
                diff_pixels_vs_baseline: Some(0),
                first_mismatch_vs_baseline: None,
                first_mismatch_rgba_vs_baseline: None,
                data: None,
              }],
              baseline_rgba: None,
            });
        } else if let Some(state) = determinism_map.get_mut(&stem_for_determinism) {
          record_variant(
            state,
            width,
            height,
            hash_hi,
            hash_lo,
            data,
            &out_dir_for_determinism,
            determinism.save_variants,
          );
        }
      }
    }

    let png = if encode_png {
      Some(encode_image(&report.pixmap, OutputFormat::Png)?)
    } else {
      None
    };

    Ok(RenderOutcome {
      png,
      diagnostics: report.diagnostics,
      artifacts: report.artifacts,
    })
  };

  let (tx, rx) = channel();
  let worker_name = stem.clone();
  let spawn_result = thread::Builder::new()
    .name(format!("render-fixtures-worker-{worker_name}"))
    .stack_size(CLI_RENDER_STACK_SIZE)
    .spawn(move || {
      let result = std::panic::catch_unwind(AssertUnwindSafe(render_work));
      let _ = tx.send(result);
    });

  let result = match spawn_result {
    Ok(_handle) => match rx.recv_timeout(shared.hard_timeout) {
      Ok(Ok(outcome)) => outcome.map_err(|e| Status::Error(format_error_with_chain(&e, false))),
      Ok(Err(panic)) => Err(Status::Crash(panic_to_string(panic))),
      Err(RecvTimeoutError::Timeout) => Err(Status::Timeout(format!(
        "render timed out after {:.2}s",
        shared.hard_timeout.as_secs_f64()
      ))),
      Err(RecvTimeoutError::Disconnected) => {
        Err(Status::Crash("render worker disconnected".to_string()))
      }
    },
    Err(err) => Err(Status::Crash(format!("spawn render worker: {err}"))),
  };

  let elapsed = page_start.elapsed();
  let time_ms = elapsed.as_millis();

  let mut captured_artifacts: Option<RenderArtifacts> = None;
  let mut diagnostics = fastrender::RenderDiagnostics::default();
  let mut blocked_urls: Option<Vec<String>> = None;

  let (mut status, size) = match result {
    Ok(outcome) => {
      diagnostics = outcome.diagnostics;

      let mut status = Status::Ok;

      let urls = blocked_network_urls(&diagnostics);
      if !urls.is_empty() {
        if opts.write_outputs {
          blocked_urls = Some(urls.clone());
        }
        status = Status::Error(format!("blocked http/https resources: {}", urls.join(", ")));
      }

      if opts.write_outputs || opts.write_snapshot {
        common::render_pipeline::log_diagnostics(&diagnostics, |line| {
          let _ = writeln!(log, "{line}");
        });
        let _ = writeln!(log, "Time: {time_ms}ms");
      }

      if opts.write_outputs {
        let Some(png) = outcome.png else {
          if !log.is_empty() {
            let _ = writeln!(log, "PNG encode missing despite write_outputs=true");
          }
          return FixtureResult {
            stem,
            status: Status::Error("internal: missing PNG".to_string()),
            time_ms,
            size: None,
          };
        };

        let size = png.len();
        if !log.is_empty() {
          let _ = writeln!(log, "PNG size: {size} bytes");
        }

        match &status {
          Status::Ok => {
            if !log.is_empty() {
              log.push_str(
                "Status: OK
",
              );
            }
          }
          Status::Error(msg) => {
            if !log.is_empty() {
              log.push_str(
                "Status: ERROR
",
              );
              let _ = writeln!(log, "Error: {msg}");
            }
          }
          Status::Crash(_) | Status::Timeout(_) => {}
        }

        if let Err(err) = fs::write(&output_path, &png) {
          if !log.is_empty() {
            let _ = writeln!(log, "Write error: {err}");
          }
          (Status::Error(format!("write: {err}")), None)
        } else {
          captured_artifacts = Some(outcome.artifacts);
          (status, Some(size))
        }
      } else {
        (status, None)
      }
    }
    Err(status) => {
      if opts.write_outputs || opts.write_snapshot {
        let _ = writeln!(log, "Time: {time_ms}ms");
        match &status {
          Status::Error(msg) => {
            let _ = writeln!(log, "Status: ERROR");
            let _ = writeln!(log, "Error: {msg}");
          }
          Status::Crash(msg) => {
            let _ = writeln!(log, "Status: CRASH");
            let _ = writeln!(log, "Panic: {msg}");
          }
          Status::Timeout(msg) => {
            let _ = writeln!(log, "Status: TIMEOUT");
            let _ = writeln!(log, "Timeout: {msg}");
          }
          Status::Ok => {}
        }
      }
      (status, None)
    }
  };

  if opts.write_snapshot {
    if let Some(artifacts) = captured_artifacts.as_ref() {
      if let Err(err) = write_snapshot_outputs(&snapshot_dir, artifacts, &mut log) {
        let _ = writeln!(log, "Snapshot write error: {err}");
      }
    } else {
      let _ = writeln!(log, "Snapshot requested but artifacts were not captured");
    }

    let diag_report = DiagnosticsFile {
      fixture: stem.clone(),
      status: status_label(&status).to_string(),
      error: status_error(&status).map(str::to_string),
      time_ms,
      png_size: size,
      diagnostics: diagnostics.clone(),
    };
    let _ = write_diagnostics_file(&snapshot_dir, &diag_report, &mut log, true);
  }

  if opts.write_outputs {
    let _ = writeln!(log, "Metadata: {}", metadata_path.display());
    if let Err(err) = write_render_metadata_file(
      &metadata_path,
      &stem,
      shared,
      &status,
      time_ms,
      blocked_urls,
      input_sha256.as_deref(),
      fixture_dir_sha256.as_deref(),
      &mut log,
    ) {
      let _ = writeln!(log, "Metadata write error: {err}");
      if matches!(status, Status::Ok) {
        status = Status::Error(format!("write_metadata: {err}"));
      }
    }

    let _ = fs::write(&log_path, &log);
  }

  if !opts.quiet {
    match &status {
      Status::Ok => {
        if let Some(size) = size {
          println!("✓ {stem} ({size}b, {time_ms}ms)");
        } else {
          println!("✓ {stem} ({time_ms}ms)");
        }
      }
      Status::Error(msg) => println!("✗ {stem} ERROR: {msg} ({time_ms}ms)"),
      Status::Crash(msg) => {
        let short: String = msg.chars().take(50).collect();
        println!("✗ {stem} CRASH: {short} ({time_ms}ms)");
      }
      Status::Timeout(msg) => println!("✗ {stem} TIMEOUT: {msg} ({time_ms}ms)"),
    }
  }

  FixtureResult {
    stem,
    status,
    time_ms,
    size,
  }
}

fn write_render_metadata_file(
  path: &Path,
  stem: &str,
  shared: &RenderShared,
  status: &Status,
  elapsed_ms: u128,
  blocked_urls: Option<Vec<String>>,
  input_sha256: Option<&str>,
  fixture_dir_sha256: Option<&str>,
  log: &mut String,
) -> io::Result<()> {
  let viewport = shared.base_options.viewport.unwrap_or((0, 0));
  let dpr = shared.base_options.device_pixel_ratio.unwrap_or(1.0);
  let fit_canvas_to_content = shared.base_options.fit_canvas_to_content.unwrap_or(false);
  let blocked_network_urls = blocked_urls.map(|urls| {
    let count = urls.len();
    let sample = urls.into_iter().take(3).collect::<Vec<_>>();
    BlockedNetworkUrlsMetadata { count, sample }
  });
  let metadata = RenderMetadataFile {
    fixture: stem.to_string(),
    viewport,
    dpr,
    media: MediaMetadata::from_arg(shared.media),
    fit_canvas_to_content,
    patch_html_for_chrome_baseline: shared.patch_html_for_chrome_baseline,
    timeout_secs: shared.timeout_secs,
    input_sha256: input_sha256.map(|value| value.to_string()),
    fixture_dir_sha256: fixture_dir_sha256.map(|value| value.to_string()),
    bundled_fonts: shared.font_config.use_bundled_fonts,
    font_dirs: shared.font_config.font_dirs.clone(),
    status: status_label(status),
    elapsed_ms: elapsed_ms.min(u64::MAX as u128) as u64,
    blocked_network_urls,
  };
  let json = serde_json::to_vec(&metadata)
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
  fs::write(path, json).map_err(|e| {
    let _ = writeln!(log, "Failed to write {}: {e}", path.display());
    e
  })
}

fn blocked_network_urls(diagnostics: &fastrender::RenderDiagnostics) -> Vec<String> {
  let mut seen = HashSet::<String>::new();
  for entry in diagnostics
    .fetch_errors
    .iter()
    .chain(diagnostics.blocked_fetch_errors.iter())
  {
    if entry.url.starts_with("http://") || entry.url.starts_with("https://") {
      seen.insert(entry.url.clone());
    }
  }
  let mut urls: Vec<String> = seen.into_iter().collect();
  urls.sort();
  urls
}

fn canonical_file_url(path: &Path) -> io::Result<String> {
  let abs = fs::canonicalize(path)?;
  let url = Url::from_file_path(&abs)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file path for URL"))?;
  Ok(url.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
  let digest = Sha256::digest(bytes);
  digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn normalize_rel_path(path: &Path) -> String {
  path
    .components()
    .map(|c| c.as_os_str().to_string_lossy())
    .collect::<Vec<_>>()
    .join("/")
}

fn hash_fixture_dir_sha256(dir: &Path) -> io::Result<String> {
  // Keep this hashing algorithm in sync with `xtask fixture-chrome-diff` staleness checks.
  //
  // Deterministic digest over all regular files in the fixture directory:
  // - collect relative paths (with `/` separators), sort lexicographically
  // - hash `path_bytes + 0x00 + file_bytes` for each file in order.
  let mut files: Vec<(String, PathBuf)> = Vec::new();
  for entry in WalkDir::new(dir).follow_links(false) {
    let entry = entry.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    if !entry.file_type().is_file() {
      continue;
    }
    let rel = entry
      .path()
      .strip_prefix(dir)
      .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    files.push((normalize_rel_path(rel), entry.path().to_path_buf()));
  }
  files.sort_by(|a, b| a.0.cmp(&b.0));

  let mut hasher = Sha256::new();
  for (rel, path) in files {
    hasher.update(rel.as_bytes());
    hasher.update([0u8]);
    hasher.update(fs::read(path)?);
  }
  Ok(
    hasher
      .finalize()
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect(),
  )
}

fn write_snapshot_outputs(
  dir: &Path,
  artifacts: &RenderArtifacts,
  log: &mut String,
) -> io::Result<()> {
  fs::create_dir_all(dir)?;

  let snapshot = build_snapshot(artifacts)?;
  let snapshot_path = dir.join("snapshot.json");
  let _ = writeln!(log, "Snapshot: {}", snapshot_path.display());
  write_json_pretty(&snapshot_path, &snapshot, log)?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;
  use tiny_skia::{IntSize, Pixmap};

  fn pixmap_bytes_rgba(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
      bytes.extend_from_slice(&rgba);
    }
    bytes
  }

  #[test]
  fn lock_mutex_is_poison_tolerant() {
    let mutex = Mutex::new(0u32);
    let _ = std::panic::catch_unwind(|| {
      let _guard = mutex.lock().unwrap();
      panic!("poison");
    });

    let mut guard = lock_mutex(&mutex);
    *guard += 1;
    assert_eq!(*guard, 1);
  }

  #[test]
  fn record_variant_increments_baseline_without_storing_bytes() {
    let temp = tempdir().expect("tempdir");
    let out_dir = temp.path();

    let baseline_bytes = pixmap_bytes_rgba(1, 1, [10, 20, 30, 255]);
    let (hash_hi, hash_lo) = hash128(&baseline_bytes);

    let mut state = FixtureDeterminism {
      stem: "fixture".to_string(),
      variants: vec![VariantRecord {
        hash_hi,
        hash_lo,
        width: 1,
        height: 1,
        count: 1,
        diff_pixels_vs_baseline: Some(0),
        first_mismatch_vs_baseline: None,
        first_mismatch_rgba_vs_baseline: None,
        data: None,
      }],
      baseline_rgba: None,
    };

    record_variant(
      &mut state,
      1,
      1,
      hash_hi,
      hash_lo,
      &baseline_bytes,
      out_dir,
      false,
    );

    assert_eq!(state.variants.len(), 1);
    assert_eq!(state.variants[0].count, 2);
    assert!(
      state.variants[0].data.is_none(),
      "baseline record should not retain pixmap bytes"
    );
  }

  #[test]
  fn record_variant_reads_baseline_png_and_reports_first_mismatch_rgba() {
    let temp = tempdir().expect("tempdir");
    let out_dir = temp.path();

    let stem = "fixture";

    // Baseline pixmap bytes are premultiplied RGBA (alpha=255 keeps values unchanged).
    let baseline_bytes = pixmap_bytes_rgba(1, 1, [255, 0, 0, 255]);
    let (baseline_hi, baseline_lo) = hash128(&baseline_bytes);
    let size = IntSize::from_wh(1, 1).expect("size");
    let baseline_pixmap = Pixmap::from_vec(baseline_bytes.clone(), size).expect("baseline pixmap");
    let baseline_png = encode_image(&baseline_pixmap, OutputFormat::Png).expect("encode baseline");
    fs::write(output_path_for(out_dir, stem), baseline_png).expect("write baseline png");

    let mut state = FixtureDeterminism {
      stem: stem.to_string(),
      variants: vec![VariantRecord {
        hash_hi: baseline_hi,
        hash_lo: baseline_lo,
        width: 1,
        height: 1,
        count: 1,
        diff_pixels_vs_baseline: Some(0),
        first_mismatch_vs_baseline: None,
        first_mismatch_rgba_vs_baseline: None,
        data: None,
      }],
      baseline_rgba: None,
    };

    let variant_bytes = pixmap_bytes_rgba(1, 1, [0, 255, 0, 255]);
    let (variant_hi, variant_lo) = hash128(&variant_bytes);

    record_variant(
      &mut state,
      1,
      1,
      variant_hi,
      variant_lo,
      &variant_bytes,
      out_dir,
      true,
    );

    assert_eq!(state.variants.len(), 2);
    let variant = &state.variants[1];
    assert_eq!(variant.hash_hi, variant_hi);
    assert_eq!(variant.hash_lo, variant_lo);
    assert_eq!(variant.count, 1);
    assert_eq!(variant.diff_pixels_vs_baseline, Some(1));
    assert_eq!(variant.first_mismatch_vs_baseline, Some((0, 0)));
    assert_eq!(
      variant.first_mismatch_rgba_vs_baseline,
      Some(([255, 0, 0, 255], [0, 255, 0, 255]))
    );
    assert_eq!(variant.data.as_deref(), Some(variant_bytes.as_slice()));

    // When variant bytes are stored (because `--save-variants` is enabled), the tracker should
    // confirm equality before incrementing the count.
    record_variant(
      &mut state,
      1,
      1,
      variant_hi,
      variant_lo,
      &variant_bytes,
      out_dir,
      true,
    );

    assert_eq!(state.variants.len(), 2);
    let variant = &state.variants[1];
    assert_eq!(variant.count, 2);
    assert_eq!(variant.diff_pixels_vs_baseline, Some(1));
    assert_eq!(variant.first_mismatch_vs_baseline, Some((0, 0)));
  }

  #[test]
  fn record_variant_does_not_store_variant_bytes_when_disabled() {
    let temp = tempdir().expect("tempdir");
    let out_dir = temp.path();

    let stem = "fixture";
    let baseline_bytes = pixmap_bytes_rgba(1, 1, [255, 0, 0, 255]);
    let (baseline_hi, baseline_lo) = hash128(&baseline_bytes);
    let size = IntSize::from_wh(1, 1).expect("size");
    let baseline_pixmap = Pixmap::from_vec(baseline_bytes.clone(), size).expect("baseline pixmap");
    let baseline_png = encode_image(&baseline_pixmap, OutputFormat::Png).expect("encode baseline");
    fs::write(output_path_for(out_dir, stem), baseline_png).expect("write baseline png");

    let mut state = FixtureDeterminism {
      stem: stem.to_string(),
      variants: vec![VariantRecord {
        hash_hi: baseline_hi,
        hash_lo: baseline_lo,
        width: 1,
        height: 1,
        count: 1,
        diff_pixels_vs_baseline: Some(0),
        first_mismatch_vs_baseline: None,
        first_mismatch_rgba_vs_baseline: None,
        data: None,
      }],
      baseline_rgba: None,
    };

    let variant_bytes = pixmap_bytes_rgba(1, 1, [0, 255, 0, 255]);
    let (variant_hi, variant_lo) = hash128(&variant_bytes);
    record_variant(
      &mut state,
      1,
      1,
      variant_hi,
      variant_lo,
      &variant_bytes,
      out_dir,
      false,
    );

    assert_eq!(state.variants.len(), 2);
    assert!(
      state.variants[1].data.is_none(),
      "variant bytes should only be retained when --save-variants is enabled"
    );
  }

  #[test]
  fn record_variant_caches_baseline_rgba_for_multiple_variants() {
    let temp = tempdir().expect("tempdir");
    let out_dir = temp.path();

    let stem = "fixture";
    let baseline_bytes = pixmap_bytes_rgba(1, 1, [255, 0, 0, 255]);
    let (baseline_hi, baseline_lo) = hash128(&baseline_bytes);
    let size = IntSize::from_wh(1, 1).expect("size");
    let baseline_pixmap = Pixmap::from_vec(baseline_bytes.clone(), size).expect("baseline pixmap");
    let baseline_png = encode_image(&baseline_pixmap, OutputFormat::Png).expect("encode baseline");
    let baseline_png_path = output_path_for(out_dir, stem);
    fs::write(&baseline_png_path, baseline_png).expect("write baseline png");

    let mut state = FixtureDeterminism {
      stem: stem.to_string(),
      variants: vec![VariantRecord {
        hash_hi: baseline_hi,
        hash_lo: baseline_lo,
        width: 1,
        height: 1,
        count: 1,
        diff_pixels_vs_baseline: Some(0),
        first_mismatch_vs_baseline: None,
        first_mismatch_rgba_vs_baseline: None,
        data: None,
      }],
      baseline_rgba: None,
    };

    let variant_bytes = pixmap_bytes_rgba(1, 1, [0, 255, 0, 255]);
    let (variant_hi, variant_lo) = hash128(&variant_bytes);
    record_variant(
      &mut state,
      1,
      1,
      variant_hi,
      variant_lo,
      &variant_bytes,
      out_dir,
      false,
    );
    assert!(
      state.baseline_rgba.is_some(),
      "expected baseline RGBA to be cached"
    );

    // If the baseline PNG disappears, subsequent variant diffs should still work because the
    // baseline decode is cached in memory.
    fs::remove_file(&baseline_png_path).expect("remove baseline png");

    let variant_bytes = pixmap_bytes_rgba(1, 1, [0, 0, 255, 255]);
    let (variant_hi, variant_lo) = hash128(&variant_bytes);
    record_variant(
      &mut state,
      1,
      1,
      variant_hi,
      variant_lo,
      &variant_bytes,
      out_dir,
      false,
    );

    assert_eq!(state.variants.len(), 3);
    assert_eq!(state.variants[2].diff_pixels_vs_baseline, Some(1));
    assert_eq!(state.variants[2].first_mismatch_vs_baseline, Some((0, 0)));
    assert_eq!(
      state.variants[2].first_mismatch_rgba_vs_baseline,
      Some(([255, 0, 0, 255], [0, 0, 255, 255]))
    );
  }

  #[test]
  fn diff_premultiplied_against_rgba_baseline_handles_alpha() {
    // A premultiplied pixel whose corresponding straight RGBA value is not perfectly representable
    // via the unpremultiply math (due to rounding) should still compare equal against the baseline
    // PNG, because both code paths use the same unpremultiply logic.
    let premultiplied = vec![50u8, 75u8, 100u8, 128u8];
    let size = IntSize::from_wh(1, 1).expect("size");
    let pixmap = Pixmap::from_vec(premultiplied.clone(), size).expect("pixmap");
    let png = encode_image(&pixmap, OutputFormat::Png).expect("encode png");
    let baseline_rgba = image::load_from_memory_with_format(&png, ImageFormat::Png)
      .expect("decode png")
      .to_rgba8()
      .into_raw();

    let (diff, mismatch, mismatch_rgba) =
      diff_premultiplied_against_rgba_baseline(&baseline_rgba, &premultiplied, 1);
    assert_eq!(diff, 0);
    assert_eq!(mismatch, None);
    assert_eq!(mismatch_rgba, None);

    // Small perturbation should register as a diff.
    let mut different = premultiplied.clone();
    different[0] = different[0].saturating_add(1);
    let (diff, mismatch, mismatch_rgba) =
      diff_premultiplied_against_rgba_baseline(&baseline_rgba, &different, 1);
    assert_eq!(diff, 1);
    assert_eq!(mismatch, Some((0, 0)));
    assert!(
      mismatch_rgba.is_some(),
      "expected baseline/variant RGBA details for first mismatch"
    );
  }

  #[test]
  fn write_nondeterminism_outputs_sorts_variants_by_hash() {
    let temp = tempdir().expect("tempdir");
    let out_dir = temp.path();

    let stem = "fixture";

    let baseline_bytes = pixmap_bytes_rgba(1, 1, [0, 0, 0, 255]);
    let (baseline_hi, baseline_lo) = hash128(&baseline_bytes);
    let size = IntSize::from_wh(1, 1).expect("size");
    let baseline_pixmap = Pixmap::from_vec(baseline_bytes.clone(), size).expect("baseline pixmap");
    let baseline_png = encode_image(&baseline_pixmap, OutputFormat::Png).expect("encode baseline");
    fs::write(output_path_for(out_dir, stem), baseline_png).expect("write baseline png");

    let v1_bytes = pixmap_bytes_rgba(1, 1, [1, 0, 0, 255]);
    let v2_bytes = pixmap_bytes_rgba(1, 1, [2, 0, 0, 255]);
    let (v1_hi, v1_lo) = hash128(&v1_bytes);
    let (v2_hi, v2_lo) = hash128(&v2_bytes);

    let ((small_hi, small_lo, small_bytes), (large_hi, large_lo, large_bytes)) =
      if (v1_hi, v1_lo) <= (v2_hi, v2_lo) {
        ((v1_hi, v1_lo, v1_bytes), (v2_hi, v2_lo, v2_bytes))
      } else {
        ((v2_hi, v2_lo, v2_bytes), (v1_hi, v1_lo, v1_bytes))
      };

    // Insert variants in reverse hash order (large then small) so we can validate sorting.
    let mut state = FixtureDeterminism {
      stem: stem.to_string(),
      variants: vec![
        VariantRecord {
          hash_hi: baseline_hi,
          hash_lo: baseline_lo,
          width: 1,
          height: 1,
          count: 1,
          diff_pixels_vs_baseline: Some(0),
          first_mismatch_vs_baseline: None,
          first_mismatch_rgba_vs_baseline: None,
          data: None,
        },
        VariantRecord {
          hash_hi: large_hi,
          hash_lo: large_lo,
          width: 1,
          height: 1,
          count: 1,
          diff_pixels_vs_baseline: Some(1),
          first_mismatch_vs_baseline: Some((0, 0)),
          first_mismatch_rgba_vs_baseline: None,
          data: Some(large_bytes),
        },
        VariantRecord {
          hash_hi: small_hi,
          hash_lo: small_lo,
          width: 1,
          height: 1,
          count: 1,
          diff_pixels_vs_baseline: Some(1),
          first_mismatch_vs_baseline: Some((0, 0)),
          first_mismatch_rgba_vs_baseline: None,
          data: Some(small_bytes),
        },
      ],
      baseline_rgba: None,
    };

    let cli = Cli {
      fixtures_dir: PathBuf::new(),
      out_dir: out_dir.to_path_buf(),
      fixtures: None,
      shard: None,
      jobs: 1,
      viewport: (1, 1),
      dpr: 1.0,
      media: MediaTypeArg::Screen,
      fit_canvas_to_content: false,
      timeout: 1,
      write_snapshot: false,
      font_dir: Vec::new(),
      repeat: 2,
      shuffle: false,
      seed: 0,
      fail_on_nondeterminism: false,
      save_variants: true,
      reset_paint_scratch: false,
    };

    write_nondeterminism_outputs(out_dir, stem, &mut state, &cli).expect("write variants");

    let report_path = snapshot_dir_for(out_dir, stem)
      .join("nondeterminism")
      .join("report.txt");
    let report = fs::read_to_string(report_path).expect("read report");

    let small_tag = format!("hash=0x{small_hi:016x}{small_lo:016x}");
    let large_tag = format!("hash=0x{large_hi:016x}{large_lo:016x}");
    let small_pos = report
      .find(&small_tag)
      .expect("expected report to mention small hash");
    let large_pos = report
      .find(&large_tag)
      .expect("expected report to mention large hash");
    assert!(
      small_pos < large_pos,
      "expected variants to be sorted by hash in report; got:\n{report}"
    );
  }
}

fn build_snapshot(artifacts: &RenderArtifacts) -> io::Result<PipelineSnapshot> {
  let dom = artifacts
    .dom
    .as_ref()
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing DOM artifact"))?;
  let styled = artifacts
    .styled_tree
    .as_ref()
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing styled tree artifact"))?;
  let box_tree = artifacts
    .box_tree
    .as_ref()
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing box tree artifact"))?;
  let fragment_tree = artifacts
    .fragment_tree
    .as_ref()
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing fragment tree artifact"))?;
  let display_list = artifacts
    .display_list
    .as_ref()
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing display list artifact"))?;
  Ok(snapshot_pipeline(
    dom,
    styled,
    box_tree,
    fragment_tree,
    display_list,
  ))
}

fn write_diagnostics_file(
  snapshot_dir: &Path,
  diag: &DiagnosticsFile,
  log: &mut String,
  enabled: bool,
) -> io::Result<()> {
  if !enabled {
    return Ok(());
  }
  fs::create_dir_all(snapshot_dir)?;
  let path = snapshot_dir.join("diagnostics.json");
  write_json_pretty(&path, diag, log)
}

fn write_json_pretty(path: &Path, value: &impl Serialize, log: &mut String) -> io::Result<()> {
  let json = serde_json::to_string_pretty(value)
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
  fs::write(path, json).map_err(|e| {
    let _ = writeln!(log, "Failed to write {}: {e}", path.display());
    e
  })
}
