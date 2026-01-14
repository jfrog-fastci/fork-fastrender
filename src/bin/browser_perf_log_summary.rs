//! Summarize `FASTR_PERF_LOG` JSONL captures.
//!
//! The `browser` binary can emit newline-delimited JSON (JSONL) perf events when perf logging is
//! enabled (see `fastrender::browser_perf_log`). Raw event streams are useful for deep dives but
//! hard to compare without ad-hoc scripts; this tool provides stable percentile summaries.

use clap::{ArgAction, Parser};
use fastrender::browser_perf_log::{
  BrowserPerfLogEvent, BrowserPerfLogEventV1, BrowserPerfLogEventV2, InputKind,
};
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

  /// Only include events at or after this timestamp (ms since process start).
  #[arg(long, value_name = "MS")]
  from_ms: Option<u64>,

  /// Only include events at or before this timestamp (ms since process start).
  #[arg(long, value_name = "MS")]
  to_ms: Option<u64>,

  /// Only include events of the given kind.
  ///
  /// Common values:
  /// - `frame` (UI frame samples)
  /// - `input` (keyboard + scroll latency)
  /// - `scroll` (scroll latency only)
  /// - `resize`
  /// - `ttfp`
  /// - `cpu_summary`
  /// - `memory_summary`
  /// - `frame_upload`
  /// - `idle_sample` (legacy alias: `idle_summary`)
  #[arg(long, value_name = "EVENT")]
  only_event: Option<String>,

  /// Emit machine-readable JSON instead of the human-readable table.
  #[arg(long, action = ArgAction::SetTrue)]
  json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventFilter {
  Frame,
  Input,
  Scroll,
  Resize,
  Ttfp,
  CpuSummary,
  MemorySummary,
  FrameUpload,
  IdleSample,
  TabSwitch,
}

fn parse_event_filter(raw: &str) -> Result<EventFilter, String> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Err("--only-event must not be empty".to_string());
  }
  match raw.to_ascii_lowercase().as_str() {
    "frame" => Ok(EventFilter::Frame),
    "input" => Ok(EventFilter::Input),
    "scroll" => Ok(EventFilter::Scroll),
    "resize" => Ok(EventFilter::Resize),
    "ttfp" => Ok(EventFilter::Ttfp),
    "cpu_summary" | "cpu" => Ok(EventFilter::CpuSummary),
    "memory_summary" | "memory" => Ok(EventFilter::MemorySummary),
    "frame_upload" | "upload" => Ok(EventFilter::FrameUpload),
    "idle_sample" | "idle_summary" => Ok(EventFilter::IdleSample),
    "tab_switch" | "tabswitch" | "tab-switch" => Ok(EventFilter::TabSwitch),
    other => Err(format!(
      "unknown --only-event {other:?}; expected frame|input|scroll|resize|ttfp|cpu_summary|memory_summary|frame_upload|idle_sample|tab_switch"
    )),
  }
}

fn f64_timestamp_ms_to_u64(ts_ms: Option<f64>) -> Option<u64> {
  let ts_ms = ts_ms?;
  if !ts_ms.is_finite() || ts_ms < 0.0 {
    return None;
  }
  let capped = ts_ms.min(u64::MAX as f64);
  Some(capped as u64)
}

fn event_timestamp_ms(event: &BrowserPerfLogEvent) -> Option<u64> {
  match event {
    BrowserPerfLogEvent::V2(event) => match event {
      BrowserPerfLogEventV2::Frame { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::Input { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::Resize { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::Ttfp { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::CpuSummary { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::IdleSample { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::FrameUpload { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::MemorySummary { t_ms, ts_ms, .. } => (*t_ms).or(*ts_ms),
      BrowserPerfLogEventV2::Unknown => None,
    },
    BrowserPerfLogEvent::V1(event) => match event {
      BrowserPerfLogEventV1::UiFrameTime { ts_ms, .. }
      | BrowserPerfLogEventV1::TimeToFirstPaint { ts_ms, .. }
      | BrowserPerfLogEventV1::Latency { ts_ms, .. }
      | BrowserPerfLogEventV1::ResourceSample { ts_ms, .. } => f64_timestamp_ms_to_u64(*ts_ms),
      BrowserPerfLogEventV1::Unknown => None,
    },
    BrowserPerfLogEvent::Unknown(_) => None,
  }
}

fn matches_event_filter(event: &BrowserPerfLogEvent, filter: EventFilter) -> bool {
  match event {
    BrowserPerfLogEvent::V2(event) => match event {
      BrowserPerfLogEventV2::Frame { .. } => filter == EventFilter::Frame,
      BrowserPerfLogEventV2::Input { input_kind, .. } => match filter {
        EventFilter::Input => true,
        EventFilter::Scroll => input_kind.unwrap_or(InputKind::Unknown) == InputKind::MouseWheel,
        _ => false,
      },
      BrowserPerfLogEventV2::Resize { .. } => filter == EventFilter::Resize,
      BrowserPerfLogEventV2::Ttfp { .. } => filter == EventFilter::Ttfp,
      BrowserPerfLogEventV2::CpuSummary { .. } => filter == EventFilter::CpuSummary,
      BrowserPerfLogEventV2::MemorySummary { .. } => filter == EventFilter::MemorySummary,
      BrowserPerfLogEventV2::FrameUpload { .. } => filter == EventFilter::FrameUpload,
      BrowserPerfLogEventV2::IdleSample { .. } => filter == EventFilter::IdleSample,
      BrowserPerfLogEventV2::Unknown => false,
    },
    BrowserPerfLogEvent::V1(event) => match event {
      BrowserPerfLogEventV1::UiFrameTime { .. } => filter == EventFilter::Frame,
      BrowserPerfLogEventV1::TimeToFirstPaint { .. } => filter == EventFilter::Ttfp,
      BrowserPerfLogEventV1::Latency { kind, .. } => match kind.to_ascii_lowercase().as_str() {
        "scroll" => filter == EventFilter::Scroll,
        "resize" => filter == EventFilter::Resize,
        "input" => filter == EventFilter::Input,
        "tab_switch" | "tabswitch" | "tab-switch" => filter == EventFilter::TabSwitch,
        _ => false,
      },
      BrowserPerfLogEventV1::ResourceSample { .. } => {
        matches!(filter, EventFilter::CpuSummary | EventFilter::MemorySummary)
      }
      BrowserPerfLogEventV1::Unknown => false,
    },
    BrowserPerfLogEvent::Unknown(_) => false,
  }
}

#[derive(Debug, Default)]
struct Samples {
  ui_frame_time_ms: Vec<f64>,
  ttfp_ms: Vec<f64>,
  scroll_latency_ms: Vec<f64>,
  resize_latency_ms: Vec<f64>,
  input_latency_ms: Vec<f64>,
  tab_switch_latency_ms: Vec<f64>,
  upload_total_ms: Vec<f64>,
  upload_last_ms: Vec<f64>,
  coalesced_frames: Vec<f64>,
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
  upload_total_ms: TimeStats,
  upload_last_ms: TimeStats,
  coalesced_frames: ScalarStats,
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
    return Some(*sorted.last().expect("non-empty percentile input")); // fastrender-allow-unwrap
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

  let rows: [(&str, &TimeStats); 8] = [
    ("ui_frame_time_ms", &summary.ui_frame_time_ms),
    ("ttfp_ms", &summary.ttfp_ms),
    ("scroll_latency_ms", &summary.scroll_latency_ms),
    ("resize_latency_ms", &summary.resize_latency_ms),
    ("input_latency_ms", &summary.input_latency_ms),
    ("tab_switch_latency_ms", &summary.tab_switch_latency_ms),
    ("upload_total_ms", &summary.upload_total_ms),
    ("upload_last_ms", &summary.upload_last_ms),
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
    "coalesced_frames",
    summary.coalesced_frames.count,
    fmt_opt_f64(summary.coalesced_frames.min, 2),
    fmt_opt_f64(summary.coalesced_frames.mean, 2),
    fmt_opt_f64(summary.coalesced_frames.max, 2),
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
  if let (Some(from), Some(to)) = (cli.from_ms, cli.to_ms) {
    if from > to {
      return Err(format!(
        "--from-ms ({from}) must be less than or equal to --to-ms ({to})"
      ));
    }
  }

  let only_event = match cli.only_event.as_deref() {
    Some(raw) => Some(parse_event_filter(raw)?),
    None => None,
  };

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

        if let Some(filter) = only_event {
          if !matches_event_filter(&event, filter) {
            continue;
          }
        }
        if cli.from_ms.is_some() || cli.to_ms.is_some() {
          if let Some(ts_ms) = event_timestamp_ms(&event) {
            if cli.from_ms.is_some_and(|from| ts_ms < from) {
              continue;
            }
            if cli.to_ms.is_some_and(|to| ts_ms > to) {
              continue;
            }
          }
        }

        match event {
          BrowserPerfLogEvent::V2(event) => match event {
            BrowserPerfLogEventV2::Frame { ui_frame_ms, .. } => {
              if let Some(ms) = ui_frame_ms.filter(f64::is_finite) {
                samples.ui_frame_time_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Ttfp { ttfp_ms, .. } => {
              if let Some(ms) = ttfp_ms.filter(f64::is_finite) {
                samples.ttfp_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Resize {
              resize_to_present_ms,
              ..
            } => {
              if let Some(ms) = resize_to_present_ms.filter(f64::is_finite) {
                samples.resize_latency_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Input {
              input_kind,
              input_to_present_ms,
              ..
            } => {
              let Some(ms) = input_to_present_ms.filter(f64::is_finite) else {
                continue;
              };
              match input_kind.unwrap_or(InputKind::Unknown) {
                InputKind::Keyboard => samples.input_latency_ms.push(ms),
                InputKind::MouseWheel => samples.scroll_latency_ms.push(ms),
                _ => {}
              }
            }
            BrowserPerfLogEventV2::CpuSummary {
              cpu_percent_recent, ..
            } => {
              if let Some(cpu) = cpu_percent_recent.filter(f64::is_finite) {
                samples.cpu_percent.push(cpu);
              }
            }
            BrowserPerfLogEventV2::FrameUpload {
              upload_last_ms,
              upload_total_ms,
              overwritten_frames,
              ..
            } => {
              if let Some(ms) = upload_total_ms.filter(f64::is_finite) {
                samples.upload_total_ms.push(ms);
              }
              if let Some(ms) = upload_last_ms.filter(f64::is_finite) {
                samples.upload_last_ms.push(ms);
              }
              if let Some(frames) = overwritten_frames {
                samples.coalesced_frames.push(frames as f64);
              }
            }
            BrowserPerfLogEventV2::MemorySummary { rss_bytes, .. } => {
              if let Some(rss) = rss_bytes {
                samples.rss_bytes.push(rss);
              }
            }
            BrowserPerfLogEventV2::IdleSample { .. } | BrowserPerfLogEventV2::Unknown => {
              unknown_events = unknown_events.saturating_add(1);
            }
          },
          BrowserPerfLogEvent::V1(event) => match event {
            fastrender::browser_perf_log::BrowserPerfLogEventV1::UiFrameTime {
              frame_time_ms, ..
            } => {
              if frame_time_ms.is_finite() {
                samples.ui_frame_time_ms.push(frame_time_ms);
              }
            }
            fastrender::browser_perf_log::BrowserPerfLogEventV1::TimeToFirstPaint { ttfp_ms, .. } => {
              if ttfp_ms.is_finite() {
                samples.ttfp_ms.push(ttfp_ms);
              }
            }
            fastrender::browser_perf_log::BrowserPerfLogEventV1::Latency {
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
            fastrender::browser_perf_log::BrowserPerfLogEventV1::ResourceSample {
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
            fastrender::browser_perf_log::BrowserPerfLogEventV1::Unknown => {
              unknown_events = unknown_events.saturating_add(1);
            }
          },
          BrowserPerfLogEvent::Unknown(_) => {
            unknown_events = unknown_events.saturating_add(1);
          }
        };
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
    upload_total_ms: time_stats(&mut samples.upload_total_ms),
    upload_last_ms: time_stats(&mut samples.upload_last_ms),
    coalesced_frames: scalar_stats(&mut samples.coalesced_frames),
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
