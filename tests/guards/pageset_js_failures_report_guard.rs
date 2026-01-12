//! Guard against stale/malformed pageset JS failure reports.
//!
//! `progress/js/pageset_js_failures.*` is a committed summary derived from the pageset scoreboard
//! (`progress/pages/*.json`). Keep it in sync so planners always have a current, deterministic view
//! of JS-engine failure hotspots.

use serde_json::Value;
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

  let pages_with_js = report
    .get("pages_with_js")
    .and_then(Value::as_u64)
    .expect("pages_with_js is u64") as usize;
  assert!(
    pages_with_js <= pages_total,
    "pages_with_js must be <= pages_total"
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

  let top_unimplemented = js
    .get("top_unimplemented")
    .and_then(Value::as_array)
    .expect("js.top_unimplemented is array");
  assert!(
    top_unimplemented.len() <= top_n as usize,
    "top_unimplemented must be truncated to top_n"
  );
  assert_sorted_top_unimplemented(top_unimplemented);

  let top_exceptions = js
    .get("top_exceptions")
    .and_then(Value::as_array)
    .expect("js.top_exceptions is array");
  assert!(
    top_exceptions.len() <= top_n as usize,
    "top_exceptions must be truncated to top_n"
  );
  assert_sorted_top_exceptions(top_exceptions);

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

