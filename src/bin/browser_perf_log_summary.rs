//! Summarize `FASTR_PERF_LOG` JSONL captures.
//!
//! The `browser` binary can emit newline-delimited JSON (JSONL) perf events when perf logging is
//! enabled (see `fastrender::browser_perf_log`). Raw event streams are useful for deep dives but
//! hard to compare without ad-hoc scripts; this tool provides stable percentile summaries.

use clap::{ArgAction, Parser};
use fastrender::browser_perf_log::BrowserPerfLogEvent;
use serde::Serialize;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
  name = "browser_perf_log_summary",
  version,
  about = "Summarize FASTR_PERF_LOG JSONL captures into percentile stats"
)]
struct Cli {
  /// Read perf log JSONL from the given file instead of stdin.
  #[arg(long, value_name = "PATH")]
  input: Option<PathBuf>,

  /// Emit machine-readable JSON instead of the human-readable table.
  #[arg(long, action = ArgAction::SetTrue)]
  json: bool,
}

#[derive(Debug, Default)]
struct Samples {
  ui_frame_time_ms: Vec<f64>,
  ttfp_ms: Vec<f64>,
  scroll_latency_ms: Vec<f64>,
  resize_latency_ms: Vec<f64>,
  input_latency_ms: Vec<f64>,
  tab_switch_latency_ms: Vec<f64>,
  cpu_percent: Vec<f64>,
  rss_bytes: Vec<u64>,
}

#[derive(Serialize)]
struct Summary {
  meta: MetaSummary,
  ui_frame_time_ms: TimeStats,
  ttfp_ms: TimeStats,
  scroll_latency_ms: TimeStats,
  resize_latency_ms: TimeStats,
  input_latency_ms: TimeStats,
  tab_switch_latency_ms: TimeStats,
  cpu_percent: ScalarStats,
  rss_bytes: RssStats,
}

#[derive(Serialize)]
struct MetaSummary {
  lines_total: u64,
  events_parsed: u64,
  parse_errors: u64,
  unknown_events: u64,
}

#[derive(Serialize)]
struct TimeStats {
  count: u64,
  mean: Option<f64>,
  p50: Option<f64>,
  p95: Option<f64>,
  max: Option<f64>,
}

#[derive(Serialize)]
struct ScalarStats {
  count: u64,
  min: Option<f64>,
  max: Option<f64>,
  mean: Option<f64>,
}

#[derive(Serialize)]
struct RssStats {
  count: u64,
  min: Option<u64>,
  max: Option<u64>,
  mean: Option<f64>,
}

fn percentile(sorted: &[f64], percentile: f64) -> Option<f64> {
  if sorted.is_empty() {
    return None;
  }
  let n = sorted.len();
  if n == 1 {
    return Some(sorted[0]);
  }
  let rank = (percentile / 100.0) * (n.saturating_sub(1)) as f64;
  let lower = rank.floor() as usize;
  let upper = rank.ceil() as usize;
  if lower >= n || upper >= n {
    return Some(*sorted.last().expect("non-empty percentile input"));
  }
  if lower == upper {
    return Some(sorted[lower]);
  }
  let weight = rank - (lower as f64);
  Some(sorted[lower] + (sorted[upper] - sorted[lower]) * weight)
}

fn mean_f64(values: &[f64]) -> Option<f64> {
  if values.is_empty() {
    return None;
  }
  Some(values.iter().sum::<f64>() / (values.len() as f64))
}

fn time_stats(values: &mut Vec<f64>) -> TimeStats {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  let count = values.len() as u64;
  TimeStats {
    count,
    mean: mean_f64(values),
    p50: percentile(values, 50.0),
    p95: percentile(values, 95.0),
    max: values.last().copied(),
  }
}

fn scalar_stats(values: &mut Vec<f64>) -> ScalarStats {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  let count = values.len() as u64;
  ScalarStats {
    count,
    min: values.first().copied(),
    max: values.last().copied(),
    mean: mean_f64(values),
  }
}

fn rss_stats(values: &mut Vec<u64>) -> RssStats {
  values.sort_unstable();
  let count = values.len() as u64;
  let mean = if values.is_empty() {
    None
  } else {
    Some(values.iter().map(|v| *v as f64).sum::<f64>() / (values.len() as f64))
  };
  RssStats {
    count,
    min: values.first().copied(),
    max: values.last().copied(),
    mean,
  }
}

fn fmt_opt_f64(value: Option<f64>, decimals: usize) -> String {
  match value {
    Some(v) => format!("{v:.decimals$}"),
    None => "-".to_string(),
  }
}

fn fmt_opt_u64(value: Option<u64>) -> String {
  match value {
    Some(v) => v.to_string(),
    None => "-".to_string(),
  }
}

fn print_table(summary: &Summary) {
  println!(
    "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
    "metric", "count", "mean", "p50", "p95", "max"
  );

  let rows: [(&str, &TimeStats); 6] = [
    ("ui_frame_time_ms", &summary.ui_frame_time_ms),
    ("ttfp_ms", &summary.ttfp_ms),
    ("scroll_latency_ms", &summary.scroll_latency_ms),
    ("resize_latency_ms", &summary.resize_latency_ms),
    ("input_latency_ms", &summary.input_latency_ms),
    ("tab_switch_latency_ms", &summary.tab_switch_latency_ms),
  ];

  for (name, stats) in rows {
    println!(
      "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
      name,
      stats.count,
      fmt_opt_f64(stats.mean, 2),
      fmt_opt_f64(stats.p50, 2),
      fmt_opt_f64(stats.p95, 2),
      fmt_opt_f64(stats.max, 2),
    );
  }

  println!();
  println!(
    "{:<22} {:>7} {:>12} {:>12} {:>12}",
    "metric", "count", "min", "mean", "max"
  );
  println!(
    "{:<22} {:>7} {:>12} {:>12} {:>12}",
    "cpu_percent",
    summary.cpu_percent.count,
    fmt_opt_f64(summary.cpu_percent.min, 2),
    fmt_opt_f64(summary.cpu_percent.mean, 2),
    fmt_opt_f64(summary.cpu_percent.max, 2),
  );
  println!(
    "{:<22} {:>7} {:>12} {:>12} {:>12}",
    "rss_bytes",
    summary.rss_bytes.count,
    fmt_opt_u64(summary.rss_bytes.min),
    fmt_opt_f64(summary.rss_bytes.mean, 2),
    fmt_opt_u64(summary.rss_bytes.max),
  );

  println!();
  println!(
    "lines={} parsed={} parse_errors={} unknown_events={}",
    summary.meta.lines_total,
    summary.meta.events_parsed,
    summary.meta.parse_errors,
    summary.meta.unknown_events
  );
}

fn main() {
  let cli = Cli::parse();
  if let Err(err) = run(cli) {
    eprintln!("browser_perf_log_summary failed: {err}");
    std::process::exit(1);
  }
}

fn run(cli: Cli) -> Result<(), String> {
  let mut input_reader: Box<dyn BufRead> = match cli.input {
    Some(path) if path.as_os_str() == "-" => Box::new(BufReader::new(io::stdin())),
    Some(path) => {
      let file = File::open(&path).map_err(|err| format!("open {}: {err}", path.display()))?;
      Box::new(BufReader::new(file))
    }
    None => Box::new(BufReader::new(io::stdin())),
  };

  let mut raw = String::new();
  let mut lines_total = 0u64;
  let mut events_parsed = 0u64;
  let mut parse_errors = 0u64;
  let mut unknown_events = 0u64;
  let mut samples = Samples::default();

  loop {
    raw.clear();
    match input_reader.read_line(&mut raw) {
      Ok(0) => break,
      Ok(_) => {
        lines_total = lines_total.saturating_add(1);
      }
      Err(err) => return Err(format!("read input: {err}")),
    }

    let line = raw.trim();
    if line.is_empty() {
      continue;
    }

    match serde_json::from_str::<BrowserPerfLogEvent>(line) {
      Ok(event) => {
        events_parsed = events_parsed.saturating_add(1);
        match event {
          BrowserPerfLogEvent::UiFrameTime { frame_time_ms, .. } => {
            if frame_time_ms.is_finite() {
              samples.ui_frame_time_ms.push(frame_time_ms);
            }
          }
          BrowserPerfLogEvent::TimeToFirstPaint { ttfp_ms, .. } => {
            if ttfp_ms.is_finite() {
              samples.ttfp_ms.push(ttfp_ms);
            }
          }
          BrowserPerfLogEvent::Latency {
            kind, latency_ms, ..
          } => {
            if !latency_ms.is_finite() {
              continue;
            }
            match kind.to_ascii_lowercase().as_str() {
              "scroll" => samples.scroll_latency_ms.push(latency_ms),
              "resize" => samples.resize_latency_ms.push(latency_ms),
              "input" => samples.input_latency_ms.push(latency_ms),
              "tab_switch" | "tabswitch" | "tab-switch" => {
                samples.tab_switch_latency_ms.push(latency_ms)
              }
              _ => {}
            }
          }
          BrowserPerfLogEvent::ResourceSample {
            cpu_percent,
            rss_bytes,
            ..
          } => {
            if let Some(cpu) = cpu_percent {
              if cpu.is_finite() {
                samples.cpu_percent.push(cpu);
              }
            }
            if let Some(rss) = rss_bytes {
              samples.rss_bytes.push(rss);
            }
          }
          BrowserPerfLogEvent::Unknown => {
            unknown_events = unknown_events.saturating_add(1);
          }
        }
      }
      Err(err) => {
        parse_errors = parse_errors.saturating_add(1);
        eprintln!("warn: failed to parse perf log line {lines_total}: {err}");
      }
    }
  }

  let summary = Summary {
    meta: MetaSummary {
      lines_total,
      events_parsed,
      parse_errors,
      unknown_events,
    },
    ui_frame_time_ms: time_stats(&mut samples.ui_frame_time_ms),
    ttfp_ms: time_stats(&mut samples.ttfp_ms),
    scroll_latency_ms: time_stats(&mut samples.scroll_latency_ms),
    resize_latency_ms: time_stats(&mut samples.resize_latency_ms),
    input_latency_ms: time_stats(&mut samples.input_latency_ms),
    tab_switch_latency_ms: time_stats(&mut samples.tab_switch_latency_ms),
    cpu_percent: scalar_stats(&mut samples.cpu_percent),
    rss_bytes: rss_stats(&mut samples.rss_bytes),
  };

  if cli.json {
    let json = serde_json::to_string_pretty(&summary).map_err(|err| err.to_string())?;
    println!("{json}");
  } else {
    print_table(&summary);
  }

  Ok(())
}
