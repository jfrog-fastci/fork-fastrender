//! Guard against stale/malformed pageset JS failure reports.
//!
//! `progress/js/pageset_js_failures.*` is a committed summary derived from the pageset scoreboard
//! (`progress/pages/*.json`). Keep it in sync so planners always have a current, deterministic view
//! of JS-engine failure hotspots.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn count_progress_pages(root: &PathBuf) -> usize {
  let dir = root.join("progress/pages");
  fs::read_dir(&dir)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
    .filter_map(|entry| entry.ok())
    .filter(|entry| {
      entry
        .path()
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext == "json")
    })
    .count()
}

fn read_progress_pages(root: &PathBuf) -> Vec<(PathBuf, Value)> {
  let dir = root.join("progress/pages");
  let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
    .filter_map(|entry| entry.ok().map(|e| e.path()))
    .filter(|path| {
      path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext == "json")
    })
    .collect();
  paths.sort();

  paths
    .into_iter()
    .map(|path| {
      let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
      let json: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
      (path, json)
    })
    .collect()
}

fn value_u64(value: Option<&Value>) -> u64 {
  match value {
    None | Some(Value::Null) => 0,
    Some(v) => v
      .as_u64()
      .unwrap_or_else(|| panic!("expected u64, got {v:?}")),
  }
}

fn js_object(progress: &Value) -> Option<&serde_json::Map<String, Value>> {
  progress
    .get("diagnostics")?
    .get("stats")?
    .get("js")?
    .as_object()
}

#[derive(Default, Debug)]
struct AggregatedJs {
  pages_with_js: usize,
  scripts_executed: u64,
  exceptions_thrown: u64,
  terminations_observed: u64,
  termination_out_of_fuel: u64,
  termination_deadline_exceeded: u64,
  termination_interrupted: u64,
  termination_stack_overflow: u64,
  termination_out_of_memory: u64,
  top_unimplemented: Vec<(String, u64)>,
  top_exceptions: Vec<(String, String, u64)>,
}

fn aggregate_js(progress_pages: &[(PathBuf, Value)], top_n: usize) -> AggregatedJs {
  let mut agg = AggregatedJs::default();
  let mut unimplemented: HashMap<String, u64> = HashMap::new();
  let mut exceptions: HashMap<(String, String), u64> = HashMap::new();

  for (path, progress) in progress_pages {
    let Some(js) = js_object(progress) else {
      continue;
    };
    agg.pages_with_js += 1;

    agg.scripts_executed = agg
      .scripts_executed
      .saturating_add(value_u64(js.get("scripts_executed")));
    agg.exceptions_thrown = agg
      .exceptions_thrown
      .saturating_add(value_u64(js.get("exceptions_thrown")));
    agg.terminations_observed = agg
      .terminations_observed
      .saturating_add(value_u64(js.get("terminations_observed")));

    if let Some(term) = js.get("termination").and_then(Value::as_object) {
      agg.termination_out_of_fuel = agg
        .termination_out_of_fuel
        .saturating_add(value_u64(term.get("out_of_fuel")));
      agg.termination_deadline_exceeded = agg
        .termination_deadline_exceeded
        .saturating_add(value_u64(term.get("deadline_exceeded")));
      agg.termination_interrupted = agg
        .termination_interrupted
        .saturating_add(value_u64(term.get("interrupted")));
      agg.termination_stack_overflow = agg
        .termination_stack_overflow
        .saturating_add(value_u64(term.get("stack_overflow")));
      agg.termination_out_of_memory = agg
        .termination_out_of_memory
        .saturating_add(value_u64(term.get("out_of_memory")));
    }

    if let Some(items) = js.get("top_unimplemented").and_then(Value::as_array) {
      for item in items {
        let item = item.as_object().unwrap_or_else(|| {
          panic!(
            "expected js.top_unimplemented entry to be object in {}",
            path.display()
          )
        });
        let message = item
          .get("message")
          .and_then(Value::as_str)
          .unwrap_or_else(|| panic!("top_unimplemented.message missing in {}", path.display()));
        let count = value_u64(item.get("count"));
        unimplemented
          .entry(message.to_string())
          .and_modify(|c| *c = c.saturating_add(count))
          .or_insert(count);
      }
    }

    if let Some(items) = js.get("top_exceptions").and_then(Value::as_array) {
      for item in items {
        let item = item.as_object().unwrap_or_else(|| {
          panic!(
            "expected js.top_exceptions entry to be object in {}",
            path.display()
          )
        });
        let type_ = item
          .get("type")
          .and_then(Value::as_str)
          .unwrap_or_else(|| panic!("top_exceptions.type missing in {}", path.display()));
        let message = item
          .get("message")
          .and_then(Value::as_str)
          .unwrap_or_else(|| panic!("top_exceptions.message missing in {}", path.display()));
        let count = value_u64(item.get("count"));
        exceptions
          .entry((type_.to_string(), message.to_string()))
          .and_modify(|c| *c = c.saturating_add(count))
          .or_insert(count);
      }
    }
  }

  let mut top_unimplemented: Vec<(String, u64)> = unimplemented.into_iter().collect();
  top_unimplemented.sort_by(|(a_msg, a_count), (b_msg, b_count)| {
    b_count.cmp(a_count).then_with(|| a_msg.cmp(b_msg))
  });
  top_unimplemented.truncate(top_n);
  agg.top_unimplemented = top_unimplemented;

  let mut top_exceptions: Vec<(String, String, u64)> = exceptions
    .into_iter()
    .map(|((type_, message), count)| (type_, message, count))
    .collect();
  top_exceptions.sort_by(|(a_type, a_msg, a_count), (b_type, b_msg, b_count)| {
    b_count
      .cmp(a_count)
      .then_with(|| a_type.cmp(b_type))
      .then_with(|| a_msg.cmp(b_msg))
  });
  top_exceptions.truncate(top_n);
  agg.top_exceptions = top_exceptions;

  agg
}

fn assert_sorted_top_unimplemented(entries: &[Value]) {
  for pair in entries.windows(2) {
    let a = &pair[0];
    let b = &pair[1];
    let a_count = a
      .get("count")
      .and_then(Value::as_u64)
      .expect("top_unimplemented.count is u64");
    let b_count = b
      .get("count")
      .and_then(Value::as_u64)
      .expect("top_unimplemented.count is u64");
    let a_msg = a
      .get("message")
      .and_then(Value::as_str)
      .expect("top_unimplemented.message is str");
    let b_msg = b
      .get("message")
      .and_then(Value::as_str)
      .expect("top_unimplemented.message is str");

    assert!(
      a_count > b_count || (a_count == b_count && a_msg <= b_msg),
      "top_unimplemented must be sorted by count desc, then message asc: ({a_count}, {a_msg:?}) then ({b_count}, {b_msg:?})"
    );
  }
}

fn assert_sorted_top_exceptions(entries: &[Value]) {
  for pair in entries.windows(2) {
    let a = &pair[0];
    let b = &pair[1];
    let a_count = a
      .get("count")
      .and_then(Value::as_u64)
      .expect("top_exceptions.count is u64");
    let b_count = b
      .get("count")
      .and_then(Value::as_u64)
      .expect("top_exceptions.count is u64");
    let a_type = a
      .get("type")
      .and_then(Value::as_str)
      .expect("top_exceptions.type is str");
    let b_type = b
      .get("type")
      .and_then(Value::as_str)
      .expect("top_exceptions.type is str");
    let a_msg = a
      .get("message")
      .and_then(Value::as_str)
      .expect("top_exceptions.message is str");
    let b_msg = b
      .get("message")
      .and_then(Value::as_str)
      .expect("top_exceptions.message is str");

    assert!(
      (a_count > b_count)
        || (a_count == b_count && (a_type < b_type || (a_type == b_type && a_msg <= b_msg))),
      "top_exceptions must be sorted by count desc, then type asc, then message asc: ({a_count}, {a_type:?}, {a_msg:?}) then ({b_count}, {b_type:?}, {b_msg:?})"
    );
  }
}

#[test]
fn pageset_js_failure_report_is_present_and_in_sync() {
  let root = repo_root();

  let pages_total_expected = count_progress_pages(&root);
  let progress_pages = read_progress_pages(&root);

  let json_path = root.join("progress/js/pageset_js_failures.json");
  let raw = fs::read_to_string(&json_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", json_path.display()));
  let report: Value = serde_json::from_str(&raw)
    .unwrap_or_else(|err| panic!("failed to parse {}: {err}", json_path.display()));

  let top_n = report
    .get("top_n")
    .and_then(Value::as_u64)
    .expect("pageset_js_failures.json must include top_n");
  assert_eq!(
    top_n, 32,
    "pageset_js_failures.json top_n must match the repo-wide triage setting"
  );

  let pages_total = report
    .get("pages_total")
    .and_then(Value::as_u64)
    .expect("pages_total is u64") as usize;
  assert_eq!(
    pages_total, pages_total_expected,
    "pages_total must match number of committed progress/pages/*.json files"
  );

  let expected = aggregate_js(&progress_pages, top_n as usize);

  let pages_with_js = report
    .get("pages_with_js")
    .and_then(Value::as_u64)
    .expect("pages_with_js is u64") as usize;
  assert_eq!(
    pages_with_js, expected.pages_with_js,
    "pages_with_js must match diagnostics.stats.js presence across progress/pages/*.json"
  );

  let js = report.get("js").expect("js object is present");
  let js_obj = js.as_object().expect("js is object");
  for key in [
    "scripts_executed",
    "exceptions_thrown",
    "terminations_observed",
    "termination",
    "top_unimplemented",
    "top_exceptions",
  ] {
    assert!(
      js_obj.contains_key(key),
      "js object must contain key {key:?}"
    );
  }

  assert_eq!(
    value_u64(js.get("scripts_executed")),
    expected.scripts_executed,
    "scripts_executed mismatch vs progress/pages aggregation"
  );
  assert_eq!(
    value_u64(js.get("exceptions_thrown")),
    expected.exceptions_thrown,
    "exceptions_thrown mismatch vs progress/pages aggregation"
  );
  assert_eq!(
    value_u64(js.get("terminations_observed")),
    expected.terminations_observed,
    "terminations_observed mismatch vs progress/pages aggregation"
  );

  let term = js
    .get("termination")
    .and_then(Value::as_object)
    .expect("js.termination is object");
  assert_eq!(
    value_u64(term.get("out_of_fuel")),
    expected.termination_out_of_fuel,
    "termination.out_of_fuel mismatch"
  );
  assert_eq!(
    value_u64(term.get("deadline_exceeded")),
    expected.termination_deadline_exceeded,
    "termination.deadline_exceeded mismatch"
  );
  assert_eq!(
    value_u64(term.get("interrupted")),
    expected.termination_interrupted,
    "termination.interrupted mismatch"
  );
  assert_eq!(
    value_u64(term.get("stack_overflow")),
    expected.termination_stack_overflow,
    "termination.stack_overflow mismatch"
  );
  assert_eq!(
    value_u64(term.get("out_of_memory")),
    expected.termination_out_of_memory,
    "termination.out_of_memory mismatch"
  );

  let top_unimplemented = js
    .get("top_unimplemented")
    .and_then(Value::as_array)
    .expect("js.top_unimplemented is array");
  assert!(
    top_unimplemented.len() <= top_n as usize,
    "top_unimplemented must be truncated to top_n"
  );
  assert_sorted_top_unimplemented(top_unimplemented);

  let report_unimplemented: Vec<(String, u64)> = top_unimplemented
    .iter()
    .map(|entry| {
      let entry = entry.as_object().expect("top_unimplemented entry object");
      let msg = entry
        .get("message")
        .and_then(Value::as_str)
        .expect("top_unimplemented.message");
      let count = value_u64(entry.get("count"));
      (msg.to_string(), count)
    })
    .collect();
  assert_eq!(
    report_unimplemented, expected.top_unimplemented,
    "top_unimplemented mismatch vs aggregated telemetry"
  );

  let top_exceptions = js
    .get("top_exceptions")
    .and_then(Value::as_array)
    .expect("js.top_exceptions is array");
  assert!(
    top_exceptions.len() <= top_n as usize,
    "top_exceptions must be truncated to top_n"
  );
  assert_sorted_top_exceptions(top_exceptions);

  let report_exceptions: Vec<(String, String, u64)> = top_exceptions
    .iter()
    .map(|entry| {
      let entry = entry.as_object().expect("top_exceptions entry object");
      let type_ = entry
        .get("type")
        .and_then(Value::as_str)
        .expect("top_exceptions.type");
      let msg = entry
        .get("message")
        .and_then(Value::as_str)
        .expect("top_exceptions.message");
      let count = value_u64(entry.get("count"));
      (type_.to_string(), msg.to_string(), count)
    })
    .collect();
  assert_eq!(
    report_exceptions, expected.top_exceptions,
    "top_exceptions mismatch vs aggregated telemetry"
  );

  let md_path = root.join("progress/js/pageset_js_failures.md");
  let md = fs::read_to_string(&md_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", md_path.display()));
  assert!(
    md.contains("# pageset_progress JavaScript failure report"),
    "markdown report missing title header"
  );
  assert!(
    md.contains("## Terminations"),
    "markdown report must include Terminations breakdown"
  );
  assert!(
    md.contains("## Top unimplemented"),
    "markdown report must include Top unimplemented section"
  );
  assert!(
    md.contains("## Top exceptions"),
    "markdown report must include Top exceptions section"
  );

  // Keep the committed report small (it's intended to be read/reviewed in diffs).
  let md_size = md.len();
  assert!(
    md_size <= 32 * 1024,
    "markdown report is unexpectedly large ({md_size} bytes)"
  );
  let json_size = raw.len();
  assert!(
    json_size <= 32 * 1024,
    "json report is unexpectedly large ({json_size} bytes)"
  );
}
