use clap::{ArgAction, Parser};
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use url::Url;

const UI_PERF_SMOKE_SCHEMA_VERSION: u32 = 1;

const DEFAULT_OUTPUT_PATH: &str = "target/ui_perf_smoke.json";

const DEFAULT_VIEWPORT_CSS: (u32, u32) = (800, 600);
const DEFAULT_DPR: f32 = 1.0;

const DEFAULT_THRESHOLD: f64 = 0.05;

const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

const SCROLL_WARMUP: usize = 5;
const SCROLL_SAMPLES: usize = 40;
const SCROLL_DELTA_CSS: f32 = 140.0;

const RESIZE_WARMUP: usize = 3;
const RESIZE_SAMPLES: usize = 20;

const INPUT_WARMUP: usize = 3;
const INPUT_CYCLES: usize = 20;

#[derive(Parser)]
#[command(about = "Headless browser UI responsiveness harness (scroll/resize/input latency)")]
struct Args {
  /// Write the JSON summary to this path (also printed to stdout).
  #[arg(long, default_value = DEFAULT_OUTPUT_PATH)]
  output: PathBuf,

  /// Optional baseline JSON to compare against.
  #[arg(long)]
  baseline: Option<PathBuf>,

  /// Relative regression threshold (0.05 = 5%).
  #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
  threshold: f64,

  /// Exit with a non-zero status when any tracked metric regresses beyond `--threshold`.
  #[arg(long)]
  fail_on_regression: bool,

  /// Only run these scenarios (comma-separated).
  #[arg(long, value_delimiter = ',')]
  only: Option<Vec<String>>,

  /// Exit with a non-zero status when any scenario fails (status=error|timeout).
  ///
  /// Defaults to enabled when the `CI` environment variable is set, otherwise disabled.
  #[arg(long)]
  fail_on_failure: bool,

  /// Disable the default `CI` behavior that enables `--fail-on-failure`.
  #[arg(long = "no-fail-on-failure", conflicts_with = "fail_on_failure")]
  no_fail_on_failure: bool,

  /// Print more debug information to stderr.
  #[arg(long, action = ArgAction::SetTrue)]
  verbose: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ScenarioStatus {
  Ok,
  Error,
  Timeout,
}

impl Default for ScenarioStatus {
  fn default() -> Self {
    Self::Ok
  }
}

#[derive(Clone, Serialize, Deserialize)]
struct ScenarioSummary {
  name: String,
  url: String,
  viewport_css: (u32, u32),
  dpr: f32,
  #[serde(default)]
  status: ScenarioStatus,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  error: Option<String>,
  #[serde(default)]
  samples_ms: Vec<f64>,
  #[serde(default)]
  metrics_ms: BTreeMap<String, f64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct UiPerfSmokeSummary {
  schema_version: u32,
  scenarios: Vec<ScenarioSummary>,
}

struct FrameInfo {
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_css: (f32, f32),
  scroll_bounds_css: fastrender::scroll::ScrollBounds,
  scroll_content_css: (f32, f32),
}

#[derive(Clone)]
struct Regression {
  scenario: String,
  metric: String,
  baseline: f64,
  latest: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();
  if args.threshold < 0.0 {
    return Err("--threshold must be non-negative".into());
  }
  if args.fail_on_regression && args.baseline.is_none() {
    return Err("--fail-on-regression requires --baseline".into());
  }

  if std::env::var_os("FASTR_USE_BUNDLED_FONTS").is_none() {
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  }
  ensure_single_thread_rayon_pool();

  let scenario_names = selected_scenarios(args.only.as_deref())?;

  let baseline = if let Some(path) = args.baseline.as_ref() {
    Some(read_summary(path)?)
  } else {
    None
  };
  if let Some(base) = baseline.as_ref() {
    if base.schema_version != UI_PERF_SMOKE_SCHEMA_VERSION {
      return Err(
        format!(
          "baseline schema_version {} does not match current schema_version {} (regenerate the baseline with the current ui_perf_smoke)",
          base.schema_version, UI_PERF_SMOKE_SCHEMA_VERSION
        )
        .into(),
      );
    }
  }

  let fail_on_failure = resolve_fail_on_failure(&args);

  let (tx, rx, join) = fastrender::ui::spawn_browser_ui_worker("fastr-ui-perf-smoke-worker")?;

  let mut scenarios = Vec::new();
  for name in &scenario_names {
    let summary = run_named_scenario(name, &tx, &rx, args.verbose);
    let failed = summary.status != ScenarioStatus::Ok;
    scenarios.push(summary);
    if failed && fail_on_failure {
      break;
    }
  }

  scenarios.sort_by(|a, b| a.name.cmp(&b.name));
  let summary = UiPerfSmokeSummary {
    schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
    scenarios: scenarios.clone(),
  };

  if let Some(parent) = args.output.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent)?;
    }
  }

  let json = serde_json::to_string_pretty(&summary)?;
  std::fs::write(&args.output, &json)?;
  println!("{json}");

  let mut exit_code = 0;

  if fail_on_failure {
    let failures: Vec<&ScenarioSummary> = scenarios
      .iter()
      .filter(|s| s.status != ScenarioStatus::Ok)
      .collect();
    if !failures.is_empty() {
      eprintln!("Scenario failures ({}):", failures.len());
      for scenario in failures {
        let message = scenario.error.as_deref().unwrap_or("scenario failed");
        eprintln!(
          "  {:<16} {:<7} {}",
          scenario.name,
          format_status(scenario.status),
          message
        );
      }
      exit_code = 1;
    }
  }

  if let Some(base) = baseline.as_ref() {
    let regressions = find_regressions(&summary, base, args.threshold);
    if regressions.is_empty() {
      eprintln!(
        "No regressions detected vs baseline (threshold {:.1}%).",
        args.threshold * 100.0
      );
    } else {
      eprintln!(
        "Regressions detected vs baseline ({} over threshold {:.1}%):",
        regressions.len(),
        args.threshold * 100.0
      );
      for reg in &regressions {
        eprintln!(
          "  {:<16} {:<24} baseline={:.3} latest={:.3} ({:+.1}%)",
          reg.scenario,
          reg.metric,
          reg.baseline,
          reg.latest,
          reg.percent_delta() * 100.0
        );
      }
      if args.fail_on_regression {
        exit_code = 1;
      }
    }
  }

  drop(tx);
  // Best-effort: don't hang indefinitely waiting for the worker thread to exit.
  let join_result = join_with_timeout(join, Duration::from_secs(5));
  if let Err(err) = join_result {
    eprintln!("Warning: failed to join UI worker thread: {err}");
  }

  if exit_code != 0 {
    std::process::exit(exit_code);
  }
  Ok(())
}

fn resolve_fail_on_failure(args: &Args) -> bool {
  if args.no_fail_on_failure {
    return false;
  }
  if args.fail_on_failure {
    return true;
  }
  std::env::var_os("CI").is_some()
}

fn ensure_single_thread_rayon_pool() {
  // Avoid mutating process environment variables. Instead, eagerly initialize Rayon's global pool.
  //
  // This matches the approach used by `browser --headless-smoke` so perf harness results are less
  // sensitive to the host CPU count (and avoids the global-pool race panic).
  if !std::env::var_os("RAYON_NUM_THREADS").is_some_and(|value| !value.is_empty()) {
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build_global();
  }
}

fn selected_scenarios(only: Option<&[String]>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
  let all = [
    "ttfp_newtab",
    "scroll_fixture",
    "resize_fixture",
    "input_text",
  ];

  match only {
    None => Ok(all.iter().map(|s| s.to_string()).collect()),
    Some(list) => {
      let wanted: BTreeSet<String> = list
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
      if wanted.is_empty() {
        return Err("--only was provided but no scenario names were specified".into());
      }
      for name in &wanted {
        if !all.contains(&name.as_str()) {
          return Err(
            format!(
              "unknown scenario {name:?}; valid values: {}",
              all.join(", ")
            )
            .into(),
          );
        }
      }
      Ok(wanted.into_iter().collect())
    }
  }
}

fn format_status(status: ScenarioStatus) -> &'static str {
  match status {
    ScenarioStatus::Ok => "ok",
    ScenarioStatus::Error => "error",
    ScenarioStatus::Timeout => "timeout",
  }
}

fn run_named_scenario(
  name: &str,
  tx: &Sender<UiToWorker>,
  rx: &Receiver<WorkerToUi>,
  verbose: bool,
) -> ScenarioSummary {
  match name {
    "ttfp_newtab" => run_ttfp_newtab(tx, rx, verbose),
    "scroll_fixture" => run_scroll_fixture(tx, rx, verbose),
    "resize_fixture" => run_resize_fixture(tx, rx, verbose),
    "input_text" => run_input_text_fixture(tx, rx, verbose),
    other => ScenarioSummary {
      name: other.to_string(),
      url: String::new(),
      viewport_css: DEFAULT_VIEWPORT_CSS,
      dpr: DEFAULT_DPR,
      status: ScenarioStatus::Error,
      error: Some(format!("unsupported scenario {other:?}")),
      samples_ms: Vec::new(),
      metrics_ms: BTreeMap::new(),
    },
  }
}

fn run_ttfp_newtab(
  tx: &Sender<UiToWorker>,
  rx: &Receiver<WorkerToUi>,
  verbose: bool,
) -> ScenarioSummary {
  let url = fastrender::ui::about_pages::ABOUT_NEWTAB.to_string();
  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "ttfp_newtab".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  let start = Instant::now();
  match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    Ok(_frame) => {
      let ttfp_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
      summary.samples_ms.push(ttfp_ms);
      summary.metrics_ms.insert("ttfp_ms".to_string(), ttfp_ms);
      if verbose {
        eprintln!("ttfp_newtab: {:.3} ms", ttfp_ms);
      }
    }
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
    }
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_scroll_fixture(
  tx: &Sender<UiToWorker>,
  rx: &Receiver<WorkerToUi>,
  verbose: bool,
) -> ScenarioSummary {
  let fixture_path = Path::new("tests/pages/fixtures/ui_perf_smoke/index.html");
  let url = match file_url(fixture_path) {
    Ok(url) => url,
    Err(err) => {
      return ScenarioSummary {
        name: "scroll_fixture".to_string(),
        url: fixture_path.display().to_string(),
        viewport_css: DEFAULT_VIEWPORT_CSS,
        dpr: DEFAULT_DPR,
        status: ScenarioStatus::Error,
        error: Some(err.to_string()),
        samples_ms: Vec::new(),
        metrics_ms: BTreeMap::new(),
      };
    }
  };

  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "scroll_fixture".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  let mut frame = match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    Ok(frame) => frame,
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
      let _ = tx.send(UiToWorker::CloseTab { tab_id });
      return summary;
    }
  };

  if frame.scroll_bounds_css.max_y <= frame.scroll_bounds_css.min_y + 1.0 {
    summary.status = ScenarioStatus::Error;
    summary.error = Some("fixture did not produce a scrollable document".to_string());
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  let mut scroll_y = frame.scroll_css.1;
  let mut bounds = frame.scroll_bounds_css;
  let mut direction: f32 = 1.0;

  let mut measured = Vec::new();
  for i in 0..(SCROLL_WARMUP + SCROLL_SAMPLES) {
    let mut target = scroll_y + direction * SCROLL_DELTA_CSS;
    if target > bounds.max_y {
      target = bounds.max_y;
    }
    if target < bounds.min_y {
      target = bounds.min_y;
    }
    if (target - scroll_y).abs() < 0.5 {
      direction *= -1.0;
      continue;
    }

    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::ScrollTo {
      tab_id,
      pos_css: (0.0, target),
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send ScrollTo: {err}"));
      break;
    }

    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(next) => {
        frame = next;
        scroll_y = frame.scroll_css.1;
        bounds = frame.scroll_bounds_css;
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        if i >= SCROLL_WARMUP {
          measured.push(dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }
  }

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("scroll_latency", &measured);
    if verbose {
      let p50 = summary
        .metrics_ms
        .get("scroll_latency_p50_ms")
        .copied()
        .unwrap_or(0.0);
      let p95 = summary
        .metrics_ms
        .get("scroll_latency_p95_ms")
        .copied()
        .unwrap_or(0.0);
      eprintln!(
        "scroll_fixture: bounds_y=[{:.1},{:.1}] content_h={:.1} p50={:.3}ms p95={:.3}ms",
        frame.scroll_bounds_css.min_y,
        frame.scroll_bounds_css.max_y,
        frame.scroll_content_css.1,
        p50,
        p95
      );
    }
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_resize_fixture(
  tx: &Sender<UiToWorker>,
  rx: &Receiver<WorkerToUi>,
  verbose: bool,
) -> ScenarioSummary {
  let fixture_path = Path::new("tests/pages/fixtures/ui_perf_smoke/index.html");
  let url = match file_url(fixture_path) {
    Ok(url) => url,
    Err(err) => {
      return ScenarioSummary {
        name: "resize_fixture".to_string(),
        url: fixture_path.display().to_string(),
        viewport_css: DEFAULT_VIEWPORT_CSS,
        dpr: DEFAULT_DPR,
        status: ScenarioStatus::Error,
        error: Some(err.to_string()),
        samples_ms: Vec::new(),
        metrics_ms: BTreeMap::new(),
      };
    }
  };

  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "resize_fixture".to_string(),
    url: url.clone(),
    viewport_css: DEFAULT_VIEWPORT_CSS,
    dpr,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, DEFAULT_VIEWPORT_CSS, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  if let Err(err) = wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  let small = DEFAULT_VIEWPORT_CSS;
  let large = (1_000, 700);
  let mut measured = Vec::new();

  for i in 0..(RESIZE_WARMUP + RESIZE_SAMPLES) {
    let viewport = if i % 2 == 0 { large } else { small };
    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: viewport,
      dpr,
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send ViewportChanged: {err}"));
      break;
    }

    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        if i >= RESIZE_WARMUP {
          measured.push(dt_ms);
        }
        if verbose {
          eprintln!(
            "resize_fixture: viewport_css={}x{} -> frame viewport_css={}x{} dt={:.3}ms",
            viewport.0, viewport.1, frame.viewport_css.0, frame.viewport_css.1, dt_ms
          );
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }
  }

  if summary.status == ScenarioStatus::Ok {
    summary.viewport_css = small;
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("resize_latency", &measured);
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_input_text_fixture(
  tx: &Sender<UiToWorker>,
  rx: &Receiver<WorkerToUi>,
  verbose: bool,
) -> ScenarioSummary {
  let fixture_path = Path::new("tests/pages/fixtures/ui_perf_smoke/index.html");
  let url = match file_url(fixture_path) {
    Ok(url) => url,
    Err(err) => {
      return ScenarioSummary {
        name: "input_text".to_string(),
        url: fixture_path.display().to_string(),
        viewport_css: DEFAULT_VIEWPORT_CSS,
        dpr: DEFAULT_DPR,
        status: ScenarioStatus::Error,
        error: Some(err.to_string()),
        samples_ms: Vec::new(),
        metrics_ms: BTreeMap::new(),
      };
    }
  };

  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "input_text".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  if let Err(err) = wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  // Ensure we are scrolled to the top so the input field is visible.
  let _ = tx.send(UiToWorker::ScrollTo {
    tab_id,
    pos_css: (0.0, 0.0),
  });
  let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);

  // Click the fixed input in the header at a deterministic coordinate.
  let input_pos_css = (32.0, 24.0);
  let modifiers = PointerModifiers::NONE;

  let _ = tx.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::None,
    modifiers,
  });
  let _ = tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::Primary,
    modifiers,
    click_count: 1,
  });
  let _ = tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::Primary,
    modifiers,
  });

  let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);

  let mut measured = Vec::new();

  // Warm up: insert + delete a character a few times.
  for _ in 0..INPUT_WARMUP {
    let _ = tx.send(UiToWorker::TextInput {
      tab_id,
      text: "a".to_string(),
    });
    let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);
    let _ = tx.send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Backspace,
    });
    let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);
  }

  for cycle in 0..INPUT_CYCLES {
    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::TextInput {
      tab_id,
      text: "a".to_string(),
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send TextInput: {err}"));
      break;
    }
    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        measured.push(dt_ms);
        if verbose {
          eprintln!("input_text: insert cycle={} dt={:.3}ms", cycle, dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }

    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Backspace,
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send Backspace: {err}"));
      break;
    }
    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        measured.push(dt_ms);
        if verbose {
          eprintln!("input_text: delete cycle={} dt={:.3}ms", cycle, dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }
  }

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("input_latency", &measured);
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn create_and_navigate_tab(
  tx: &Sender<UiToWorker>,
  tab_id: TabId,
  viewport_css: (u32, u32),
  dpr: f32,
  url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: CancelGens::new(),
  })?;
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css,
    dpr,
  })?;
  tx.send(UiToWorker::SetActiveTab { tab_id })?;
  tx.send(UiToWorker::Navigate {
    tab_id,
    url: url.to_string(),
    reason: NavigationReason::TypedUrl,
  })?;
  Ok(())
}

struct WaitError {
  status: ScenarioStatus,
  message: String,
}

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> Result<FrameInfo, WaitError> {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        let metrics = frame.scroll_metrics;
        return Ok(FrameInfo {
          viewport_css: frame.viewport_css,
          dpr: frame.dpr,
          scroll_css: metrics.scroll_css,
          scroll_bounds_css: metrics.bounds_css,
          scroll_content_css: metrics.content_css,
        });
      }
      Ok(_) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        return Err(WaitError {
          status: ScenarioStatus::Timeout,
          message: format!("timed out after {timeout:?} waiting for FrameReady"),
        });
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        return Err(WaitError {
          status: ScenarioStatus::Error,
          message: "UI worker disconnected before FrameReady".to_string(),
        });
      }
    }
  }
}

fn latency_metrics(prefix: &str, samples: &[f64]) -> BTreeMap<String, f64> {
  let mut out = BTreeMap::new();
  if samples.is_empty() {
    return out;
  }
  let mut sorted = samples.to_vec();
  sorted.sort_by(|a, b| a.total_cmp(b));
  let p50 = percentile_sorted(&sorted, 0.50);
  let p95 = percentile_sorted(&sorted, 0.95);
  let max = *sorted.last().unwrap();
  out.insert(format!("{prefix}_p50_ms"), round_ms(p50));
  out.insert(format!("{prefix}_p95_ms"), round_ms(p95));
  out.insert(format!("{prefix}_max_ms"), round_ms(max));
  out
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
  if sorted.is_empty() {
    return 0.0;
  }
  if sorted.len() == 1 {
    return sorted[0];
  }
  let clamped = p.clamp(0.0, 1.0);
  let idx = (clamped * ((sorted.len() - 1) as f64)).round() as usize;
  sorted[idx.min(sorted.len() - 1)]
}

fn read_summary(path: &Path) -> Result<UiPerfSmokeSummary, Box<dyn std::error::Error>> {
  let data = std::fs::read_to_string(path)?;
  Ok(serde_json::from_str(&data)?)
}

fn find_regressions(
  latest: &UiPerfSmokeSummary,
  baseline: &UiPerfSmokeSummary,
  threshold: f64,
) -> Vec<Regression> {
  const MIN_DELTA_MS: f64 = 1.0;
  let baseline_map = baseline
    .scenarios
    .iter()
    .map(|s| (s.name.as_str(), s))
    .collect::<BTreeMap<_, _>>();

  let mut regressions = Vec::new();
  for scenario in &latest.scenarios {
    let Some(base) = baseline_map.get(scenario.name.as_str()) else {
      continue;
    };
    if scenario.status != ScenarioStatus::Ok || base.status != ScenarioStatus::Ok {
      continue;
    }
    for (metric, latest_value) in &scenario.metrics_ms {
      let Some(base_value) = base.metrics_ms.get(metric) else {
        continue;
      };
      if *base_value <= 0.0 {
        continue;
      }
      let delta = latest_value - base_value;
      if delta > MIN_DELTA_MS && (delta / base_value) > threshold {
        regressions.push(Regression {
          scenario: scenario.name.clone(),
          metric: metric.clone(),
          baseline: *base_value,
          latest: *latest_value,
        });
      }
    }
  }

  regressions.sort_by(|a, b| {
    (a.scenario.as_str(), a.metric.as_str()).cmp(&(b.scenario.as_str(), b.metric.as_str()))
  });
  regressions
}

impl Regression {
  fn percent_delta(&self) -> f64 {
    (self.latest - self.baseline) / self.baseline
  }
}

fn round_ms(value: f64) -> f64 {
  let rounded = (value * 1000.0).round() / 1000.0;
  if rounded == 0.0 {
    0.0
  } else {
    rounded
  }
}

fn file_url(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
  let absolute = if path.is_absolute() {
    path.to_path_buf()
  } else {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    repo_root.join(path)
  };
  Ok(
    Url::from_file_path(&absolute)
      .map_err(|_| format!("could not convert {} to a file:// URL", absolute.display()))?
      .to_string(),
  )
}

fn join_with_timeout(join: std::thread::JoinHandle<()>, timeout: Duration) -> Result<(), String> {
  let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
  std::thread::spawn(move || {
    let _ = done_tx.send(join.join());
  });
  match done_rx.recv_timeout(timeout) {
    Ok(Ok(())) => Ok(()),
    Ok(Err(_)) => Err("UI worker thread panicked".to_string()),
    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
      "timed out after {timeout:?} waiting for UI worker join"
    )),
    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
      Err("UI worker join helper thread disconnected".to_string())
    }
  }
}
