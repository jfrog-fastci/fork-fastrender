use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

const SUPPORTED_SCHEMA_VERSIONS: &[u64] = &[1, 2];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OnlyEvent {
  Frame,
  Input,
  Resize,
  Ttfp,
  #[value(name = "idle_summary", alias = "idle-summary")]
  IdleSummary,
  #[value(name = "cpu_summary", alias = "cpu-summary")]
  CpuSummary,
}

impl OnlyEvent {
  fn as_str(self) -> &'static str {
    match self {
      OnlyEvent::Frame => "frame",
      OnlyEvent::Input => "input",
      OnlyEvent::Resize => "resize",
      OnlyEvent::Ttfp => "ttfp",
      OnlyEvent::IdleSummary => "idle_summary",
      OnlyEvent::CpuSummary => "cpu_summary",
    }
  }
}

#[derive(Parser, Debug)]
#[command(
  about = "Summarize FASTR_PERF_LOG JSONL output into headline responsiveness metrics",
  disable_version_flag = true,
  color = clap::ColorChoice::Never,
  term_width = 90
)]
struct Args {
  /// Read JSONL input from a file (defaults to stdin)
  #[arg(long)]
  input: Option<PathBuf>,

  /// Only include events at or after this timestamp (ms)
  #[arg(long)]
  from_ms: Option<f64>,

  /// Only include events at or before this timestamp (ms)
  #[arg(long)]
  to_ms: Option<f64>,

  /// Only include a single event type
  #[arg(long)]
  only_event: Option<OnlyEvent>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct SeriesStats {
  count: usize,
  mean: f64,
  p50: f64,
  p95: f64,
  max: f64,
}

#[derive(Debug, Default)]
struct Series {
  values: Vec<f64>,
}

impl Series {
  fn push(&mut self, value: f64) {
    if value.is_finite() {
      self.values.push(value);
    }
  }

  fn stats(&self) -> Option<SeriesStats> {
    if self.values.is_empty() {
      return None;
    }

    let mut sorted = self.values.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let count = sorted.len();
    let sum: f64 = sorted.iter().copied().sum();
    let mean = sum / (count as f64);
    let max = *sorted.last().unwrap();
    let p50 = percentile_nearest_rank_sorted(&sorted, 50.0);
    let p95 = percentile_nearest_rank_sorted(&sorted, 95.0);

    Some(SeriesStats {
      count,
      mean,
      p50,
      p95,
      max,
    })
  }
}

fn percentile_nearest_rank_sorted(sorted: &[f64], pct: f64) -> f64 {
  assert!(!sorted.is_empty());
  let pct = pct.clamp(0.0, 100.0);
  if pct <= 0.0 {
    return sorted[0];
  }
  if pct >= 100.0 {
    return sorted[sorted.len() - 1];
  }

  // Nearest-rank percentile:
  //   https://en.wikipedia.org/wiki/Percentile#The_nearest-rank_method
  //
  // rank is 1-indexed; we convert to an index. This is deterministic and avoids interpolation.
  let rank = ((pct / 100.0) * (sorted.len() as f64)).ceil() as usize;
  let idx = rank.saturating_sub(1).min(sorted.len() - 1);
  sorted[idx]
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Summary {
  source_schema_version: u64,
  filters: Filters,
  frames: Option<FrameSummary>,
  input: Option<InputSummary>,
  resize: Option<ResizeSummary>,
  ttfp: Option<TtfpSummary>,
  idle_summary: Option<IdleSummary>,
  cpu_summary: Option<CpuSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Filters {
  from_ms: Option<f64>,
  to_ms: Option<f64>,
  only_event: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct FrameSummary {
  ui_frame_ms: SeriesStats,
  fps: Option<SeriesStats>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct InputSummary {
  input_to_present_ms: SeriesStats,
  by_kind: BTreeMap<String, SeriesStats>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct ResizeSummary {
  resize_to_present_ms: SeriesStats,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct TtfpSummary {
  ttfp_ms: SeriesStats,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct IdleSummary {
  idle_frames: SeriesStats,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct CpuSummary {
  cpu_percent_recent: SeriesStats,
  cpu_time_ms_total: Option<SeriesStats>,
}

#[derive(Debug, Clone, Copy)]
struct WindowFilter {
  from_ms: Option<f64>,
  to_ms: Option<f64>,
  only_event: Option<OnlyEvent>,
}

fn parse_required_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<u64, String> {
  let Some(value) = obj.get(key) else {
    return Err(format!("missing required field {key:?}"));
  };
  value
    .as_u64()
    .ok_or_else(|| format!("expected {key:?} to be an integer, got {value}"))
}

fn parse_optional_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
  let Some(value) = obj.get(key) else {
    return Ok(None);
  };
  let Some(value) = value.as_u64() else {
    return Err(format!("expected {key:?} to be an integer, got {value}"));
  };
  Ok(Some(value))
}

fn parse_required_str<'a>(
  obj: &'a serde_json::Map<String, Value>,
  key: &str,
) -> Result<&'a str, String> {
  let Some(value) = obj.get(key) else {
    return Err(format!("missing required field {key:?}"));
  };
  value
    .as_str()
    .ok_or_else(|| format!("expected {key:?} to be a string, got {value}"))
}

fn parse_required_ms(obj: &serde_json::Map<String, Value>, key: &str) -> Result<f64, String> {
  let Some(value) = obj.get(key) else {
    return Err(format!("missing required field {key:?}"));
  };
  parse_value_as_f64(value).ok_or_else(|| format!("expected {key:?} to be a number, got {value}"))
}

fn parse_optional_ms(obj: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
  obj.get(key).and_then(parse_value_as_f64)
}

fn parse_value_as_f64(value: &Value) -> Option<f64> {
  if let Some(v) = value.as_f64() {
    return Some(v);
  }
  value.as_u64().map(|v| v as f64)
}

fn parse_timestamp_ms(obj: &serde_json::Map<String, Value>) -> Option<f64> {
  // Current browser perf log schema uses `ts_ms`.
  // Older/auxiliary emitters used `t_ms`; accept it as a legacy alias for robustness.
  parse_optional_ms(obj, "ts_ms").or_else(|| parse_optional_ms(obj, "t_ms"))
}

fn should_include_timestamp(t_ms: f64, filter: WindowFilter) -> bool {
  if let Some(from) = filter.from_ms {
    if t_ms < from {
      return false;
    }
  }
  if let Some(to) = filter.to_ms {
    if t_ms > to {
      return false;
    }
  }
  true
}

fn summarize_reader<R: BufRead>(reader: R, filter: WindowFilter) -> Result<Summary, String> {
  let mut frame_ms = Series::default();
  let mut fps = Series::default();
  let mut input_overall = Series::default();
  let mut input_by_kind: BTreeMap<String, Series> = BTreeMap::new();
  let mut resize_ms = Series::default();
  let mut ttfp_ms = Series::default();
  let mut idle_frames = Series::default();
  let mut cpu_percent_recent = Series::default();
  let mut cpu_time_ms_total = Series::default();

  let mut schema_version_seen: Option<u64> = None;

  for (idx, line) in reader.lines().enumerate() {
    let line_no = idx + 1;
    let line = line.map_err(|err| format!("failed to read line {line_no}: {err}"))?;
    let line = line.trim();
    if line.is_empty() {
      continue;
    }

    let parsed: Value =
      serde_json::from_str(line).map_err(|err| format!("line {line_no}: invalid JSON: {err}"))?;
    let Some(obj) = parsed.as_object() else {
      return Err(format!("line {line_no}: expected JSON object per line"));
    };

    // `schema_version` is present on most structured events, but some auxiliary browser logs
    // (historical/diagnostic) may omit it. Treat missing as "unknown but assumed supported" so the
    // summary tool stays resilient to mixed streams.
    let schema_version = parse_optional_u64(obj, "schema_version")
      .map_err(|err| format!("line {line_no}: {err}"))?;
    if let Some(schema_version) = schema_version {
      if !SUPPORTED_SCHEMA_VERSIONS.contains(&schema_version) {
        return Err(format!(
          "line {line_no}: unknown FASTR_PERF_LOG schema_version {schema_version} (supported: {:?})",
          SUPPORTED_SCHEMA_VERSIONS
        ));
      }
      match schema_version_seen {
        Some(seen) if seen != schema_version => {
          return Err(format!(
            "line {line_no}: mixed FASTR_PERF_LOG schema_version {schema_version} (previously saw {seen})"
          ));
        }
        None => {
          schema_version_seen = Some(schema_version);
        }
        _ => {}
      }
    }

    let event = parse_required_str(obj, "event").map_err(|err| format!("line {line_no}: {err}"))?;
    let Some(t_ms) = parse_timestamp_ms(obj) else {
      // If we don't have a timestamp we can't apply filters; ignore the line.
      continue;
    };

    if !should_include_timestamp(t_ms, filter) {
      continue;
    }

    if let Some(only) = filter.only_event {
      if event != only.as_str() {
        continue;
      }
    }

    match event {
      "frame" => {
        let ui_frame_ms =
          parse_required_ms(obj, "ui_frame_ms").map_err(|err| format!("line {line_no}: {err}"))?;
        frame_ms.push(ui_frame_ms);
        if let Some(fps_value) = parse_optional_ms(obj, "fps") {
          fps.push(fps_value);
        } else if ui_frame_ms.is_finite() && ui_frame_ms > 0.0 {
          // Legacy fallback: older logs did not include an explicit FPS measurement, so estimate it
          // from CPU frame time.
          fps.push(1000.0 / ui_frame_ms);
        }
      }
      "input" => {
        let input_to_present_ms = parse_required_ms(obj, "input_to_present_ms")
          .map_err(|err| format!("line {line_no}: {err}"))?;
        input_overall.push(input_to_present_ms);

        let kind = obj
          .get("input_kind")
          .and_then(|v| v.as_str())
          .or_else(|| obj.get("kind").and_then(|v| v.as_str()))
          .unwrap_or("unknown");

        input_by_kind
          .entry(kind.to_string())
          .or_insert_with(Series::default)
          .push(input_to_present_ms);
      }
      "resize" => {
        let resize_to_present_ms = parse_required_ms(obj, "resize_to_present_ms")
          .map_err(|err| format!("line {line_no}: {err}"))?;
        resize_ms.push(resize_to_present_ms);
      }
      "ttfp" => {
        let ttfp =
          parse_required_ms(obj, "ttfp_ms").map_err(|err| format!("line {line_no}: {err}"))?;
        ttfp_ms.push(ttfp);
      }
      "idle_summary" => {
        let value = parse_optional_ms(obj, "idle_frames_per_sec")
          .or_else(|| parse_optional_ms(obj, "idle_frames"))
          .or_else(|| parse_optional_ms(obj, "idle_frame_count"))
          .or_else(|| parse_optional_ms(obj, "idle_frames_total"))
          .ok_or_else(|| {
            format!(
              "line {line_no}: idle_summary event missing numeric field \"idle_frames_per_sec\" (or legacy \"idle_frames\"/\"idle_frame_count\"/\"idle_frames_total\")"
            )
          })?;
        idle_frames.push(value);
      }
      "cpu_summary" => {
        let percent = parse_required_ms(obj, "cpu_percent_recent")
          .map_err(|err| format!("line {line_no}: {err}"))?;
        cpu_percent_recent.push(percent);
        if let Some(total) = parse_optional_ms(obj, "cpu_time_ms_total") {
          cpu_time_ms_total.push(total);
        }
      }
      other => {
        // Unknown events are ignored so the tool stays forward-compatible with extra event types.
        let _ = other;
      }
    }
  }

  let schema_version_seen = schema_version_seen
    .or_else(|| SUPPORTED_SCHEMA_VERSIONS.last().copied())
    .unwrap_or(0);

  let fps_stats = fps.stats();
  let frames = frame_ms.stats().map(|ui_frame_ms| FrameSummary {
    ui_frame_ms,
    fps: fps_stats,
  });

  let input = if let Some(input_to_present_ms) = input_overall.stats() {
    let mut by_kind: BTreeMap<String, SeriesStats> = BTreeMap::new();
    for (kind, series) in input_by_kind {
      if let Some(stats) = series.stats() {
        by_kind.insert(kind, stats);
      }
    }
    Some(InputSummary {
      input_to_present_ms,
      by_kind,
    })
  } else {
    None
  };

  let resize = resize_ms.stats().map(|resize_to_present_ms| ResizeSummary {
    resize_to_present_ms,
  });

  let ttfp = ttfp_ms.stats().map(|ttfp_ms| TtfpSummary { ttfp_ms });

  let idle_summary = idle_frames
    .stats()
    .map(|idle_frames| IdleSummary { idle_frames });

  let cpu_summary = cpu_percent_recent.stats().map(|cpu_percent_recent| {
    let cpu_time_ms_total = cpu_time_ms_total.stats();
    CpuSummary {
      cpu_percent_recent,
      cpu_time_ms_total,
    }
  });

  Ok(Summary {
    source_schema_version: schema_version_seen,
    filters: Filters {
      from_ms: filter.from_ms,
      to_ms: filter.to_ms,
      only_event: filter.only_event.map(|e| e.as_str().to_string()),
    },
    frames,
    input,
    resize,
    ttfp,
    idle_summary,
    cpu_summary,
  })
}

fn fmt_ms(value: f64) -> String {
  if !value.is_finite() {
    return "NaN".to_string();
  }
  format!("{value:.2}ms")
}

fn fmt_fps(value: f64) -> String {
  if !value.is_finite() {
    return "NaN".to_string();
  }
  format!("{value:.2}fps")
}

fn fmt_pct(value: f64) -> String {
  if !value.is_finite() {
    return "NaN".to_string();
  }
  format!("{value:.2}%")
}

fn print_series(label: &str, stats: &SeriesStats, unit: &str) {
  let fmt = |v| match unit {
    "ms" => fmt_ms(v),
    "fps" => fmt_fps(v),
    "pct" => fmt_pct(v),
    _ => format!("{v:.2}"),
  };

  eprintln!(
    "{label}: n={} mean={} p50={} p95={} max={}",
    stats.count,
    fmt(stats.mean),
    fmt(stats.p50),
    fmt(stats.p95),
    fmt(stats.max)
  );
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();

  if let (Some(from), Some(to)) = (args.from_ms, args.to_ms) {
    if from > to {
      return Err(format!("--from-ms ({from}) must be <= --to-ms ({to})").into());
    }
  }

  let filter = WindowFilter {
    from_ms: args.from_ms,
    to_ms: args.to_ms,
    only_event: args.only_event,
  };

  let summary = if let Some(path) = args.input.as_ref() {
    let file = File::open(path)?;
    summarize_reader(BufReader::new(file), filter).map_err(|err| format!("{path:?}: {err}"))?
  } else {
    let stdin = io::stdin();
    summarize_reader(stdin.lock(), filter).map_err(|err| format!("stdin: {err}"))?
  };

  // Human-readable summary to stderr.
  if let Some(frames) = summary.frames.as_ref() {
    print_series("frames.ui_frame_ms", &frames.ui_frame_ms, "ms");
    if let Some(fps) = frames.fps.as_ref() {
      print_series("frames.fps", fps, "fps");
    }
  }
  if let Some(input) = summary.input.as_ref() {
    print_series(
      "input.input_to_present_ms",
      &input.input_to_present_ms,
      "ms",
    );
    for (kind, stats) in &input.by_kind {
      print_series(
        &format!("input.by_kind.{kind}.input_to_present_ms"),
        stats,
        "ms",
      );
    }
  }
  if let Some(resize) = summary.resize.as_ref() {
    print_series(
      "resize.resize_to_present_ms",
      &resize.resize_to_present_ms,
      "ms",
    );
  }
  if let Some(ttfp) = summary.ttfp.as_ref() {
    print_series("ttfp.ttfp_ms", &ttfp.ttfp_ms, "ms");
  }
  if let Some(idle) = summary.idle_summary.as_ref() {
    // This is a count-like metric in many logs, but we still report it via the generic stats
    // structure so p50/p95/max are available.
    print_series("idle_summary.idle_frames", &idle.idle_frames, "");
  }
  if let Some(cpu) = summary.cpu_summary.as_ref() {
    print_series(
      "cpu_summary.cpu_percent_recent",
      &cpu.cpu_percent_recent,
      "pct",
    );
    if let Some(total) = cpu.cpu_time_ms_total.as_ref() {
      print_series("cpu_summary.cpu_time_ms_total", total, "ms");
    }
  }

  // JSON summary to stdout.
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
  fn percentile_nearest_rank_behaviour() {
    let sorted = vec![1.0, 2.0, 3.0, 4.0];
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 0.0), 1.0);
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 50.0), 2.0);
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 95.0), 4.0);
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 100.0), 4.0);

    let sorted = vec![10.0, 20.0];
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 50.0), 10.0);
    assert_eq!(percentile_nearest_rank_sorted(&sorted, 95.0), 20.0);
  }

  #[test]
  fn summarize_synthetic_jsonl_log() {
    let log = r#"
{"schema_version":1,"event":"frame","ts_ms":0,"window_id":"WindowId(1)","ui_frame_ms":10}
{"schema_version":1,"event":"frame","ts_ms":16,"window_id":"WindowId(1)","ui_frame_ms":20}
{"schema_version":1,"event":"input","ts_ms":30,"window_id":"WindowId(1)","input_kind":"keyboard","input_to_present_ms":40}
{"schema_version":1,"event":"input","ts_ms":40,"window_id":"WindowId(1)","input_kind":"mouse","input_to_present_ms":50}
{"schema_version":1,"event":"resize","ts_ms":50,"window_id":"WindowId(1)","resize_to_present_ms":60}
{"schema_version":1,"event":"ttfp","ts_ms":70,"window_id":"WindowId(1)","ttfp_ms":80}
{"event":"idle_summary","t_ms":90,"window_id":"WindowId(1)","idle_frames_per_sec":100}
{"schema_version":1,"event":"cpu_summary","ts_ms":100,"window_id":"process","cpu_time_ms_total":1234,"cpu_percent_recent":1.5}
"#;

    let summary = summarize_reader(
      BufReader::new(log.as_bytes()),
      WindowFilter {
        from_ms: None,
        to_ms: None,
        only_event: None,
      },
    )
    .expect("summary should succeed");

    assert_eq!(summary.source_schema_version, 1);

    let frames = summary.frames.expect("expected frame stats");
    assert_eq!(frames.ui_frame_ms.count, 2);
    assert_eq!(frames.ui_frame_ms.mean, 15.0);
    assert_eq!(frames.ui_frame_ms.p50, 10.0);
    assert_eq!(frames.ui_frame_ms.p95, 20.0);
    assert_eq!(frames.ui_frame_ms.max, 20.0);

    let fps = frames.fps.expect("expected fps stats");
    assert_eq!(fps.count, 2);
    assert!((fps.mean - 75.0).abs() < 1e-6);
    assert!((fps.p50 - 50.0).abs() < 1e-6);
    assert!((fps.p95 - 100.0).abs() < 1e-6);
    assert!((fps.max - 100.0).abs() < 1e-6);

    let input = summary.input.expect("expected input stats");
    assert_eq!(input.input_to_present_ms.count, 2);
    assert_eq!(input.input_to_present_ms.mean, 45.0);
    assert_eq!(input.by_kind.get("keyboard").unwrap().mean, 40.0);
    assert_eq!(input.by_kind.get("mouse").unwrap().mean, 50.0);

    let resize = summary.resize.expect("expected resize stats");
    assert_eq!(resize.resize_to_present_ms.count, 1);
    assert_eq!(resize.resize_to_present_ms.mean, 60.0);

    let ttfp = summary.ttfp.expect("expected ttfp stats");
    assert_eq!(ttfp.ttfp_ms.count, 1);
    assert_eq!(ttfp.ttfp_ms.mean, 80.0);

    let idle = summary.idle_summary.expect("expected idle stats");
    assert_eq!(idle.idle_frames.count, 1);
    assert_eq!(idle.idle_frames.mean, 100.0);

    let cpu = summary.cpu_summary.expect("expected cpu stats");
    assert_eq!(cpu.cpu_percent_recent.count, 1);
    assert_eq!(cpu.cpu_percent_recent.mean, 1.5);
    let total = cpu.cpu_time_ms_total.expect("expected cpu_time_ms_total stats");
    assert_eq!(total.count, 1);
    assert_eq!(total.mean, 1234.0);
  }

  #[test]
  fn summarize_with_time_window_filter() {
    let log = r#"
{"schema_version":1,"event":"frame","t_ms":0,"ui_frame_ms":10}
{"schema_version":1,"event":"frame","t_ms":16,"ui_frame_ms":20}
{"schema_version":1,"event":"frame","t_ms":32,"ui_frame_ms":30}
"#;

    let summary = summarize_reader(
      BufReader::new(log.as_bytes()),
      WindowFilter {
        from_ms: Some(10.0),
        to_ms: Some(20.0),
        only_event: Some(OnlyEvent::Frame),
      },
    )
    .expect("summary should succeed");

    let frames = summary.frames.expect("expected frame stats");
    assert_eq!(frames.ui_frame_ms.count, 1);
    assert_eq!(frames.ui_frame_ms.mean, 20.0);
  }
}
