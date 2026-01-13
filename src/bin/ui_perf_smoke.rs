use clap::{ArgAction, Parser};
use fastrender::ui::{CancelGens, NavigationReason, TabId, UiToWorker, WorkerToUi};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "Headless browser UI perf smoke harness")]
struct Args {
  /// Number of measured iterations per scenario.
  ///
  /// Warmup iterations (see `--warmup`) run in addition to this count.
  #[arg(long, default_value_t = 3)]
  iterations: usize,

  /// Number of warmup iterations per scenario.
  ///
  /// Warmup iterations are executed but excluded from reported statistics.
  #[arg(long, default_value_t = 0)]
  warmup: usize,

  /// Run each scenario in its own fresh UI worker thread instance.
  ///
  /// This reduces cross-scenario cache effects but increases total runtime.
  #[arg(long, action = ArgAction::SetTrue)]
  isolate: bool,

  /// Disable per-scenario isolation (overrides `--isolate` and any future defaults).
  #[arg(long, action = ArgAction::SetTrue)]
  no_isolate: bool,

  /// Only run scenarios matching these names (comma-separated).
  #[arg(long, value_delimiter = ',')]
  only: Option<Vec<String>>,

  /// Write JSON summary to this path (always printed to stdout).
  #[arg(long, default_value = "target/ui_perf_smoke.json")]
  output: PathBuf,
}

#[derive(Clone, Copy)]
struct Scenario {
  name: &'static str,
  url: &'static str,
}

const SCENARIOS: &[Scenario] = &[
  Scenario {
    name: "about_newtab",
    url: fastrender::ui::about_pages::ABOUT_NEWTAB,
  },
  Scenario {
    name: "about_test_scroll",
    url: fastrender::ui::about_pages::ABOUT_TEST_SCROLL,
  },
  Scenario {
    name: "about_test_heavy",
    url: fastrender::ui::about_pages::ABOUT_TEST_HEAVY,
  },
];

const UI_PERF_SMOKE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Serialize)]
struct RunConfig {
  iterations: usize,
  warmup: usize,
  isolate: bool,
}

#[derive(Clone, Serialize)]
struct TimingStats {
  samples: usize,
  min_ms: f64,
  max_ms: f64,
  mean_ms: f64,
  p50_ms: f64,
  p95_ms: f64,
}

#[derive(Clone, Serialize)]
struct ScenarioSummary {
  name: String,
  url: String,
  samples_ms: Vec<f64>,
  stats_ms: TimingStats,
}

#[derive(Clone, Serialize)]
struct UiPerfSmokeSummary {
  schema_version: u32,
  run_config: RunConfig,
  scenarios: Vec<ScenarioSummary>,
  total_ms: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();
  if args.iterations == 0 {
    return Err("--iterations must be positive".into());
  }

  let isolate_default = false;
  let isolate = if args.no_isolate {
    false
  } else {
    args.isolate || isolate_default
  };

  let scenarios = filter_scenarios(SCENARIOS, args.only.as_deref())?;
  if scenarios.is_empty() {
    return Err("no scenarios selected to run".into());
  }

  let start = Instant::now();

  let mut summaries = Vec::with_capacity(scenarios.len());
  if isolate {
    for scenario in &scenarios {
      let worker = spawn_worker(format!("ui-perf-{}", scenario.name))?;
      let summary = run_scenario(worker, scenario, args.warmup, args.iterations)?;
      summaries.push(summary);
    }
  } else {
    let worker = spawn_worker("ui-perf".to_string())?;
    for scenario in &scenarios {
      let summary = run_scenario_reuse(&worker, scenario, args.warmup, args.iterations)?;
      summaries.push(summary);
    }
    shutdown_worker(worker);
  }

  summaries.sort_by(|a, b| a.name.cmp(&b.name));

  let summary = UiPerfSmokeSummary {
    schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
    run_config: RunConfig {
      iterations: args.iterations,
      warmup: args.warmup,
      isolate,
    },
    scenarios: summaries,
    total_ms: round_ms(start.elapsed().as_secs_f64() * 1000.0),
  };

  if let Some(parent) = args.output.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent)?;
    }
  }
  let json = serde_json::to_string_pretty(&summary)?;
  fs::write(&args.output, &json)?;
  println!("{json}");
  Ok(())
}

fn filter_scenarios(
  all: &[Scenario],
  only: Option<&[String]>,
) -> Result<Vec<Scenario>, Box<dyn std::error::Error>> {
  let Some(only) = only else {
    return Ok(all.to_vec());
  };

  let only: std::collections::HashSet<&str> = only.iter().map(|s| s.as_str()).collect();
  let out: Vec<Scenario> = all
    .iter()
    .copied()
    .filter(|scenario| only.contains(scenario.name))
    .collect();
  Ok(out)
}

fn spawn_worker(
  name: String,
) -> Result<fastrender::ui::BrowserWorkerHandle, Box<dyn std::error::Error>> {
  Ok(fastrender::ui::spawn_browser_worker_with_name(name)?)
}

fn shutdown_worker(worker: fastrender::ui::BrowserWorkerHandle) {
  drop(worker.tx);
  let _ = worker.join.join();
}

fn run_scenario(
  worker: fastrender::ui::BrowserWorkerHandle,
  scenario: &Scenario,
  warmup: usize,
  iterations: usize,
) -> Result<ScenarioSummary, Box<dyn std::error::Error>> {
  let summary = run_scenario_reuse(&worker, scenario, warmup, iterations)?;
  shutdown_worker(worker);
  Ok(summary)
}

fn run_scenario_reuse(
  worker: &fastrender::ui::BrowserWorkerHandle,
  scenario: &Scenario,
  warmup: usize,
  iterations: usize,
) -> Result<ScenarioSummary, Box<dyn std::error::Error>> {
  for _ in 0..warmup {
    measure_navigation_iteration(worker, scenario.url)?;
  }

  let mut samples_ms = Vec::with_capacity(iterations);
  for _ in 0..iterations {
    let ms = measure_navigation_iteration(worker, scenario.url)?;
    samples_ms.push(ms);
  }

  let stats_ms = compute_stats_ms(&samples_ms)?;
  Ok(ScenarioSummary {
    name: scenario.name.to_string(),
    url: scenario.url.to_string(),
    samples_ms,
    stats_ms,
  })
}

fn measure_navigation_iteration(
  worker: &fastrender::ui::BrowserWorkerHandle,
  url: &str,
) -> Result<f64, Box<dyn std::error::Error>> {
  const VIEWPORT_CSS: (u32, u32) = (1000, 800);
  const DPR: f32 = 1.0;
  const TIMEOUT: Duration = Duration::from_secs(30);

  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker.tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: cancel.clone(),
  })?;
  worker.tx.send(UiToWorker::SetActiveTab { tab_id })?;
  worker.tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: VIEWPORT_CSS,
    dpr: DPR,
  })?;

  let start = Instant::now();
  worker.tx.send(UiToWorker::Navigate {
    tab_id,
    url: url.to_string(),
    reason: NavigationReason::TypedUrl,
  })?;
  wait_for_frame(worker, tab_id, TIMEOUT)?;
  let elapsed_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);

  // Best-effort cleanup; ignore errors if the worker is already gone.
  let _ = worker.tx.send(UiToWorker::CloseTab { tab_id });

  Ok(elapsed_ms)
}

fn wait_for_frame(
  worker: &fastrender::ui::BrowserWorkerHandle,
  tab_id: TabId,
  timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    if now >= deadline {
      return Err(format!("timed out waiting for frame for tab {tab_id:?}").into());
    }
    let remaining = deadline - now;
    let msg = worker
      .rx
      .recv_timeout(remaining)
      .map_err(|_| format!("timed out waiting for frame for tab {tab_id:?}"))?;
    match msg {
      WorkerToUi::FrameReady { tab_id: id, .. } if id == tab_id => {
        return Ok(());
      }
      WorkerToUi::NavigationFailed { tab_id: id, error, .. } if id == tab_id => {
        return Err(format!("navigation failed for {tab_id:?}: {error}").into());
      }
      _ => {}
    }
  }
}

fn compute_stats_ms(samples_ms: &[f64]) -> Result<TimingStats, Box<dyn std::error::Error>> {
  if samples_ms.is_empty() {
    return Err("no samples provided".into());
  }
  let mut sorted = samples_ms.to_vec();
  sorted.sort_by(|a, b| a.total_cmp(b));
  let samples = sorted.len();
  let min_ms = sorted[0];
  let max_ms = sorted[samples - 1];
  let mean_ms = round_ms(sorted.iter().sum::<f64>() / samples as f64);
  let p50_ms = round_ms(percentile_sorted(&sorted, 0.50));
  let p95_ms = round_ms(percentile_sorted(&sorted, 0.95));
  Ok(TimingStats {
    samples,
    min_ms,
    max_ms,
    mean_ms,
    p50_ms,
    p95_ms,
  })
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
  debug_assert!(!sorted.is_empty());
  debug_assert!(p >= 0.0 && p <= 1.0);
  if sorted.len() == 1 {
    return sorted[0];
  }
  let p = p.clamp(0.0, 1.0);
  let n = sorted.len() as f64;
  let rank = p * (n - 1.0);
  let lo = rank.floor() as usize;
  let hi = rank.ceil() as usize;
  if lo == hi {
    return sorted[lo];
  }
  let lo_v = sorted[lo];
  let hi_v = sorted[hi];
  lo_v + (hi_v - lo_v) * (rank - lo as f64)
}

fn round_ms(value: f64) -> f64 {
  let rounded = (value * 1000.0).round() / 1000.0;
  if rounded == 0.0 {
    0.0
  } else {
    rounded
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn stats_with_warmup(samples: &[f64], warmup: usize) -> TimingStats {
    let measured = samples
      .get(warmup..)
      .expect("warmup exceeds sample length");
    compute_stats_ms(measured).expect("stats")
  }

  #[test]
  fn warmup_samples_are_excluded_from_stats() {
    // Warmup sample is an extreme outlier and should not influence the reported stats.
    let samples = [1000.0, 10.0, 20.0, 30.0];
    let stats = stats_with_warmup(&samples, 1);
    assert_eq!(stats.samples, 3);
    assert_eq!(stats.min_ms, 10.0);
    assert_eq!(stats.max_ms, 30.0);
    assert_eq!(stats.mean_ms, 20.0);
    assert_eq!(stats.p50_ms, 20.0);
    assert!(
      (stats.p95_ms - 29.0).abs() < 1e-9,
      "expected p95=29ms for [10,20,30], got {}",
      stats.p95_ms
    );
  }

  #[test]
  fn warmup_zero_uses_all_samples() {
    let samples = [10.0, 20.0, 30.0];
    let stats = stats_with_warmup(&samples, 0);
    assert_eq!(stats.samples, 3);
    assert_eq!(stats.min_ms, 10.0);
    assert_eq!(stats.max_ms, 30.0);
  }
}

