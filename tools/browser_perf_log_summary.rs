//! Summarize windowed `browser` JSONL perf-log captures.
//!
//! The windowed UI can emit newline-delimited JSON (JSONL) perf events (`browser --perf-log`).
//! This tool consumes both:
//! - the **current** `event=...` schema (v2), and
//! - the **legacy** `type=...` schema (v1),
//! and emits headline percentile stats for common responsiveness metrics.
//!
//! Output:
//! - A machine-readable JSON summary is always written to **stdout** (pretty-printed).
//! - A human-readable summary is written to **stderr** (unless `--json` is passed).

use clap::{ArgAction, Parser};
use fastrender::browser_perf_log::{
  BrowserPerfLogEvent, BrowserPerfLogEventV1, BrowserPerfLogEventV2, InputKind,
};
use fastrender::memory::BYTES_PER_MIB;
use serde::Serialize;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
  about = "Summarize FASTR_PERF_LOG JSONL output into headline responsiveness metrics",
  disable_version_flag = true,
  color = clap::ColorChoice::Never,
  term_width = 90
)]
struct Args {
  /// Read JSONL input from a file (defaults to stdin). Use "-" to force stdin.
  #[arg(long)]
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
  /// - `tab_switch`
  /// - `resize`
  /// - `ttfp`
  /// - `cpu_summary`
  /// - `memory_summary`
  /// - `frame_upload`
  /// - `idle_sample` (legacy alias: `idle_summary`)
  #[arg(long, value_name = "EVENT")]
  only_event: Option<String>,

  /// Backward-compatible alias: some callers historically used `--json` to request JSON output.
  ///
  /// This tool now always writes JSON to stdout; `--json` suppresses the human-readable stderr
  /// summary for scripts that want a quiet mode.
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
      BrowserPerfLogEventV2::RunStart { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::RunEnd { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::Frame { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::Input { t_ms, ts_ms, .. }
      | BrowserPerfLogEventV2::TabSwitch { t_ms, ts_ms, .. }
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
      BrowserPerfLogEventV2::RunStart { .. } | BrowserPerfLogEventV2::RunEnd { .. } => false,
      BrowserPerfLogEventV2::Frame { .. } => filter == EventFilter::Frame,
      BrowserPerfLogEventV2::Input { input_kind, .. } => match filter {
        EventFilter::Input => true,
        EventFilter::Scroll => input_kind.unwrap_or(InputKind::Unknown) == InputKind::MouseWheel,
        _ => false,
      },
      BrowserPerfLogEventV2::TabSwitch { .. } => filter == EventFilter::TabSwitch,
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
  tab_switch_cached_latency_ms: Vec<f64>,
  tab_switch_uncached_latency_ms: Vec<f64>,
  upload_total_ms: Vec<f64>,
  upload_last_ms: Vec<f64>,
  coalesced_frames: Vec<f64>,
  cpu_percent: Vec<f64>,
  idle_fps: Vec<f64>,
  rss_bytes: Vec<u64>,
  rss_first_bytes: Option<u64>,
  rss_last_bytes: Option<u64>,
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
  tab_switch_cached_latency_ms: TimeStats,
  tab_switch_uncached_latency_ms: TimeStats,
  upload_total_ms: TimeStats,
  upload_last_ms: TimeStats,
  coalesced_frames: ScalarStats,
  cpu_percent: ScalarStats,
  idle_fps: ScalarStats,
  rss_bytes: RssStats,
  rss_mb: ScalarStats,
  rss_first_mb: Option<f64>,
  rss_last_mb: Option<f64>,
  rss_delta_mb: Option<f64>,
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
    return Some(*sorted.last()?);
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

fn print_human_summary(summary: &Summary) {
  eprintln!(
    "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
    "metric", "count", "mean", "p50", "p95", "max"
  );

  let rows: [(&str, &TimeStats); 10] = [
    ("ui_frame_time_ms", &summary.ui_frame_time_ms),
    ("ttfp_ms", &summary.ttfp_ms),
    ("scroll_latency_ms", &summary.scroll_latency_ms),
    ("resize_latency_ms", &summary.resize_latency_ms),
    ("input_latency_ms", &summary.input_latency_ms),
    ("tab_switch_latency_ms", &summary.tab_switch_latency_ms),
    (
      "tab_switch_cached_latency_ms",
      &summary.tab_switch_cached_latency_ms,
    ),
    (
      "tab_switch_uncached_latency_ms",
      &summary.tab_switch_uncached_latency_ms,
    ),
    ("upload_total_ms", &summary.upload_total_ms),
    ("upload_last_ms", &summary.upload_last_ms),
  ];

  for (name, stats) in rows {
    eprintln!(
      "{:<22} {:>7} {:>9} {:>9} {:>9} {:>9}",
      name,
      stats.count,
      fmt_opt_f64(stats.mean, 2),
      fmt_opt_f64(stats.p50, 2),
      fmt_opt_f64(stats.p95, 2),
      fmt_opt_f64(stats.max, 2),
    );
  }

  eprintln!();
  eprintln!(
    "{:<22} {:>7} {:>12} {:>12} {:>12}",
    "metric", "count", "min", "mean", "max"
  );

  for (name, stats) in [
    ("coalesced_frames", &summary.coalesced_frames),
    ("cpu_percent", &summary.cpu_percent),
    ("idle_fps", &summary.idle_fps),
    ("rss_mb", &summary.rss_mb),
  ] {
    eprintln!(
      "{:<22} {:>7} {:>12} {:>12} {:>12}",
      name,
      stats.count,
      fmt_opt_f64(stats.min, 2),
      fmt_opt_f64(stats.mean, 2),
      fmt_opt_f64(stats.max, 2),
    );
  }

  eprintln!(
    "{:<22} {:>7} {:>12} {:>12} {:>12}",
    "rss_bytes",
    summary.rss_bytes.count,
    fmt_opt_u64(summary.rss_bytes.min),
    fmt_opt_f64(summary.rss_bytes.mean, 2),
    fmt_opt_u64(summary.rss_bytes.max),
  );

  for (name, value) in [
    ("rss_first_mb", summary.rss_first_mb),
    ("rss_last_mb", summary.rss_last_mb),
    ("rss_delta_mb", summary.rss_delta_mb),
  ] {
    let count = u64::from(value.is_some());
    eprintln!(
      "{:<22} {:>7} {:>12} {:>12} {:>12}",
      name,
      count,
      fmt_opt_f64(value, 2),
      fmt_opt_f64(value, 2),
      fmt_opt_f64(value, 2),
    );
  }

  eprintln!();
  eprintln!(
    "lines={} parsed={} parse_errors={} unknown_events={}",
    summary.meta.lines_total,
    summary.meta.events_parsed,
    summary.meta.parse_errors,
    summary.meta.unknown_events
  );
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();

  if let (Some(from), Some(to)) = (args.from_ms, args.to_ms) {
    if from > to {
      return Err(format!(
        "--from-ms ({from}) must be less than or equal to --to-ms ({to})"
      )
      .into());
    }
  }

  let only_event = match args.only_event.as_deref() {
    Some(raw) => Some(parse_event_filter(raw)?),
    None => None,
  };

  let mut reader: Box<dyn BufRead> = match args.input {
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
    match reader.read_line(&mut raw) {
      Ok(0) => break,
      Ok(_) => {
        lines_total = lines_total.saturating_add(1);
      }
      Err(err) => return Err(format!("read input: {err}").into()),
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
        if args.from_ms.is_some() || args.to_ms.is_some() {
          let Some(ts_ms) = event_timestamp_ms(&event) else {
            // When a time window is specified we can only make a decision for events that carry a
            // timestamp; skip any unknown/legacy events without one.
            continue;
          };
          if args.from_ms.is_some_and(|from| ts_ms < from) {
            continue;
          }
          if args.to_ms.is_some_and(|to| ts_ms > to) {
            continue;
          }
        }

        match event {
          BrowserPerfLogEvent::V2(event) => match event {
            BrowserPerfLogEventV2::RunStart { .. } | BrowserPerfLogEventV2::RunEnd { .. } => {}
            BrowserPerfLogEventV2::Frame { ui_frame_ms, .. } => {
              if let Some(ms) = ui_frame_ms.filter(|v| v.is_finite()) {
                samples.ui_frame_time_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::TabSwitch {
              latency_ms,
              switch_to_present_ms,
              had_cached_texture,
              cached,
              ..
            } => {
              let ms = switch_to_present_ms
                .filter(|v| v.is_finite())
                .or_else(|| latency_ms.map(|ms| ms as f64));
              let Some(ms) = ms else {
                continue;
              };
              samples.tab_switch_latency_ms.push(ms);

              let cached_switch = had_cached_texture.or(cached).unwrap_or(false);
              if cached_switch {
                samples.tab_switch_cached_latency_ms.push(ms);
              } else {
                samples.tab_switch_uncached_latency_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Ttfp { ttfp_ms, .. } => {
              if let Some(ms) = ttfp_ms.filter(|v| v.is_finite()) {
                samples.ttfp_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Resize {
              resize_to_present_ms,
              ..
            } => {
              if let Some(ms) = resize_to_present_ms.filter(|v| v.is_finite()) {
                samples.resize_latency_ms.push(ms);
              }
            }
            BrowserPerfLogEventV2::Input {
              input_kind,
              input_to_present_ms,
              ..
            } => {
              let Some(ms) = input_to_present_ms.filter(|v| v.is_finite()) else {
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
              if let Some(cpu) = cpu_percent_recent.filter(|v| v.is_finite()) {
                samples.cpu_percent.push(cpu);
              }
            }
            BrowserPerfLogEventV2::IdleSample { idle_fps, .. } => {
              if let Some(fps) = idle_fps.filter(|v| v.is_finite()) {
                samples.idle_fps.push(f64::from(fps));
              }
            }
            BrowserPerfLogEventV2::FrameUpload {
              upload_last_ms,
              upload_total_ms,
              overwritten_frames,
              ..
            } => {
              if let Some(ms) = upload_total_ms.filter(|v| v.is_finite()) {
                samples.upload_total_ms.push(ms);
              }
              if let Some(ms) = upload_last_ms.filter(|v| v.is_finite()) {
                samples.upload_last_ms.push(ms);
              }
              if let Some(frames) = overwritten_frames {
                samples.coalesced_frames.push(frames as f64);
              }
            }
            BrowserPerfLogEventV2::MemorySummary { rss_bytes, .. } => {
              if let Some(rss) = rss_bytes {
                samples.rss_bytes.push(rss);
                if samples.rss_first_bytes.is_none() {
                  samples.rss_first_bytes = Some(rss);
                }
                samples.rss_last_bytes = Some(rss);
              }
            }
            BrowserPerfLogEventV2::Unknown => {
              unknown_events = unknown_events.saturating_add(1);
            }
          },
          BrowserPerfLogEvent::V1(event) => match event {
            BrowserPerfLogEventV1::UiFrameTime { frame_time_ms, .. } => {
              if frame_time_ms.is_finite() {
                samples.ui_frame_time_ms.push(frame_time_ms);
              }
            }
            BrowserPerfLogEventV1::TimeToFirstPaint { ttfp_ms, .. } => {
              if ttfp_ms.is_finite() {
                samples.ttfp_ms.push(ttfp_ms);
              }
            }
            BrowserPerfLogEventV1::Latency {
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
                  samples.tab_switch_latency_ms.push(latency_ms);
                }
                _ => {}
              }
            }
            BrowserPerfLogEventV1::ResourceSample {
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
                if samples.rss_first_bytes.is_none() {
                  samples.rss_first_bytes = Some(rss);
                }
                samples.rss_last_bytes = Some(rss);
              }
            }
            BrowserPerfLogEventV1::Unknown => {
              unknown_events = unknown_events.saturating_add(1);
            }
          },
          BrowserPerfLogEvent::Unknown(_) => {
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

  let rss_bytes = rss_stats(&mut samples.rss_bytes);
  let rss_mb = ScalarStats {
    count: rss_bytes.count,
    min: rss_bytes
      .min
      .map(|bytes| bytes as f64 / BYTES_PER_MIB as f64),
    max: rss_bytes
      .max
      .map(|bytes| bytes as f64 / BYTES_PER_MIB as f64),
    mean: rss_bytes.mean.map(|bytes| bytes / BYTES_PER_MIB as f64),
  };
  let rss_first_mb = samples
    .rss_first_bytes
    .map(|bytes| bytes as f64 / BYTES_PER_MIB as f64);
  let rss_last_mb = samples
    .rss_last_bytes
    .map(|bytes| bytes as f64 / BYTES_PER_MIB as f64);
  let rss_delta_mb = match (samples.rss_first_bytes, samples.rss_last_bytes) {
    (Some(first), Some(last)) => Some((last as f64 - first as f64) / BYTES_PER_MIB as f64),
    _ => None,
  };

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
    tab_switch_cached_latency_ms: time_stats(&mut samples.tab_switch_cached_latency_ms),
    tab_switch_uncached_latency_ms: time_stats(&mut samples.tab_switch_uncached_latency_ms),
    upload_total_ms: time_stats(&mut samples.upload_total_ms),
    upload_last_ms: time_stats(&mut samples.upload_last_ms),
    coalesced_frames: scalar_stats(&mut samples.coalesced_frames),
    cpu_percent: scalar_stats(&mut samples.cpu_percent),
    idle_fps: scalar_stats(&mut samples.idle_fps),
    rss_bytes,
    rss_mb,
    rss_first_mb,
    rss_last_mb,
    rss_delta_mb,
  };

  if !args.json {
    print_human_summary(&summary);
  }

  serde_json::to_writer_pretty(io::stdout(), &summary)?;
  println!();

  Ok(())
}

fn main() {
  if let Err(err) = run() {
    eprintln!("browser_perf_log_summary: {err}");
    std::process::exit(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_event_filter_accepts_aliases() {
    assert_eq!(
      parse_event_filter("idle_summary").unwrap(),
      EventFilter::IdleSample
    );
    assert_eq!(
      parse_event_filter("idle_sample").unwrap(),
      EventFilter::IdleSample
    );
    assert_eq!(parse_event_filter("cpu").unwrap(), EventFilter::CpuSummary);
    assert_eq!(
      parse_event_filter("cpu_summary").unwrap(),
      EventFilter::CpuSummary
    );
    assert_eq!(
      parse_event_filter("memory").unwrap(),
      EventFilter::MemorySummary
    );
    assert_eq!(
      parse_event_filter("memory_summary").unwrap(),
      EventFilter::MemorySummary
    );
    assert_eq!(
      parse_event_filter("tab-switch").unwrap(),
      EventFilter::TabSwitch
    );
  }

  #[test]
  fn matches_event_filter_scroll_matches_mouse_wheel_input() {
    let input_json =
      r#"{"event":"input","t_ms":10,"input_kind":"mouse_wheel","input_to_present_ms":3.0}"#;
    let event: BrowserPerfLogEvent = serde_json::from_str(input_json).expect("parse input");

    assert!(matches_event_filter(&event, EventFilter::Input));
    assert!(matches_event_filter(&event, EventFilter::Scroll));
    assert!(!matches_event_filter(&event, EventFilter::Resize));
  }
}
