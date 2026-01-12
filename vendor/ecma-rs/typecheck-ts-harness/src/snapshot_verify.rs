use crate::directives::{DirectiveParseOptions, HarnessOptions, IgnoredDirectiveSummary};
use crate::discover::{
  discover_conformance_test_paths, Filter, Shard, TestCasePath, DEFAULT_EXTENSIONS,
};
use crate::runner::{HarnessFileSet, SnapshotStore, TscPoolError, TscRunnerPool};
use crate::tsc::{node_available, typescript_available};
use crate::{HarnessError, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct VerifySnapshotsOptions {
  pub root: PathBuf,
  pub filter: Filter,
  pub shard: Option<Shard>,
  pub extensions: Vec<String>,
  pub node_path: PathBuf,
  pub timeout: Duration,
  pub jobs: usize,
  pub trace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySnapshotsReport {
  pub suite_root: String,
  pub suite_name: String,
  pub summary: VerifySnapshotsSummary,
  #[serde(default, skip_serializing_if = "IgnoredDirectiveSummary::is_empty")]
  pub directives: IgnoredDirectiveSummary,
  pub cases: Vec<VerifySnapshotsCase>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifySnapshotsSummary {
  pub total: usize,
  pub ok: usize,
  pub missing_snapshot: usize,
  pub drift: usize,
  pub tsc_crashed: usize,
  pub timeout: usize,
}

impl VerifySnapshotsSummary {
  pub fn has_failures(&self) -> bool {
    self.missing_snapshot > 0 || self.drift > 0 || self.tsc_crashed > 0 || self.timeout > 0
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySnapshotsCase {
  pub id: String,
  pub path: String,
  pub status: VerifySnapshotsStatus,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub detail: Option<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifySnapshotsStatus {
  Ok,
  MissingSnapshot,
  Drift,
  TscCrashed,
  Timeout,
}

pub fn verify_snapshots(opts: VerifySnapshotsOptions) -> Result<VerifySnapshotsReport> {
  init_tracing(opts.trace);

  if !cfg!(feature = "with-node") {
    return Err(HarnessError::Typecheck(
      "verify-snapshots requires the `with-node` feature (build with default features)".to_string(),
    ));
  }

  if !node_available(&opts.node_path) {
    return Err(HarnessError::Typecheck(format!(
      "verify-snapshots requires Node.js (not available at {}); install Node.js or pass --node <path>",
      opts.node_path.display()
    )));
  }

  if !typescript_available(&opts.node_path) {
    return Err(HarnessError::Typecheck(
      "verify-snapshots requires the `typescript` npm package; run `cd typecheck-ts-harness && npm ci` (or set TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR)"
        .to_string(),
    ));
  }

  let extensions: Vec<String> = if opts.extensions.is_empty() {
    DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect()
  } else {
    opts.extensions.clone()
  };

  let mut cases = discover_conformance_test_paths(&opts.root, &opts.filter, &extensions)?;
  if cases.is_empty() {
    return Err(HarnessError::Typecheck(format!(
      "verify-snapshots: no conformance tests discovered under '{}' (extensions: {}); ensure the suite exists (for upstream, run `git submodule update --init --recursive parse-js/tests/TypeScript`) or pass --root <path>",
      opts.root.display(),
      extensions.join(",")
    )));
  }

  if let Some(shard) = opts.shard {
    cases = cases
      .into_iter()
      .enumerate()
      .filter(|(idx, _)| shard.includes(*idx))
      .map(|(_, case)| case)
      .collect();
  }

  let job_count = opts.jobs.max(1);
  let tsc_pool = TscRunnerPool::new(opts.node_path.clone(), job_count);
  let snapshot_store = SnapshotStore::new(&opts.root);

  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(job_count)
    .build()
    .map_err(|err| HarnessError::Typecheck(format!("create thread pool: {err}")))?;

  let timeout = opts.timeout;
  let snapshot_store_ref = &snapshot_store;
  let tsc_pool_ref = &tsc_pool;
  let mut results: Vec<VerifySnapshotsCaseResult> = pool.install(|| {
    cases
      .into_par_iter()
      .map(|case| verify_case(case, snapshot_store_ref, tsc_pool_ref, timeout))
      .collect()
  });
  results.sort_by(|a, b| a.case.id.cmp(&b.case.id));

  let mut summary = VerifySnapshotsSummary::default();
  summary.total = results.len();
  for case in &results {
    match case.case.status {
      VerifySnapshotsStatus::Ok => summary.ok += 1,
      VerifySnapshotsStatus::MissingSnapshot => summary.missing_snapshot += 1,
      VerifySnapshotsStatus::Drift => summary.drift += 1,
      VerifySnapshotsStatus::TscCrashed => summary.tsc_crashed += 1,
      VerifySnapshotsStatus::Timeout => summary.timeout += 1,
    }
  }

  let directives =
    IgnoredDirectiveSummary::from_harness_options(results.iter().map(|r| &r.harness_options));
  let cases = results.into_iter().map(|r| r.case).collect();

  Ok(VerifySnapshotsReport {
    suite_root: opts.root.display().to_string(),
    suite_name: snapshot_store.suite_name().to_string(),
    summary,
    directives,
    cases,
  })
}

#[derive(Debug, Clone)]
struct VerifySnapshotsCaseResult {
  case: VerifySnapshotsCase,
  harness_options: HarnessOptions,
}

fn init_tracing(enabled: bool) {
  if !enabled {
    return;
  }
  let _ = tracing_subscriber::fmt()
    .with_writer(std::io::stderr)
    .with_max_level(tracing::Level::DEBUG)
    .json()
    .with_ansi(false)
    .try_init();
}

fn verify_case(
  case: TestCasePath,
  snapshot_store: &SnapshotStore,
  tsc_pool: &TscRunnerPool,
  timeout: Duration,
) -> VerifySnapshotsCaseResult {
  let TestCasePath { id, path } = case;
  let path_display = path.display().to_string();

  let snapshot = snapshot_store.load(&id);
  let inputs = build_case_inputs(&path);

  let mut harness_options = HarnessOptions::default();
  let mut notes = Vec::new();
  let mut input_error: Option<String> = None;
  let (file_set, options) = match inputs {
    Ok(inputs) => {
      harness_options = inputs.harness_options;
      notes = inputs.notes;
      (Some(inputs.file_set), Some(inputs.options))
    }
    Err(err) => {
      input_error = Some(err.clone());
      notes.push(err);
      (None, None)
    }
  };

  let make_case = |status: VerifySnapshotsStatus, detail: Option<String>| VerifySnapshotsCase {
    id: id.clone(),
    path: path_display.clone(),
    status,
    detail,
    notes: notes.clone(),
  };

  let snapshot = match snapshot {
    Ok(snapshot) => snapshot,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      return VerifySnapshotsCaseResult {
        case: make_case(VerifySnapshotsStatus::MissingSnapshot, Some(err.to_string())),
        harness_options,
      };
    }
    Err(err) => {
      return VerifySnapshotsCaseResult {
        case: make_case(
          VerifySnapshotsStatus::Drift,
          Some(format!("failed to load snapshot: {err}")),
        ),
        harness_options,
      };
    }
  };

  let (file_set, options) = match (file_set, options) {
    (Some(file_set), Some(options)) => (file_set, options),
    _ => {
      return VerifySnapshotsCaseResult {
        case: make_case(VerifySnapshotsStatus::Drift, input_error),
        harness_options,
      };
    }
  };

  let deadline = Instant::now() + timeout;
  let live = match tsc_pool.run(&file_set, &options, deadline) {
    Ok(diags) => diags,
    Err(TscPoolError::Timeout) => {
      return VerifySnapshotsCaseResult {
        case: make_case(
          VerifySnapshotsStatus::Timeout,
          Some(format!("timed out after {}s", timeout.as_secs())),
        ),
        harness_options,
      };
    }
    Err(TscPoolError::Crashed(err)) => {
      return VerifySnapshotsCaseResult {
        case: make_case(VerifySnapshotsStatus::TscCrashed, Some(err)),
        harness_options,
      };
    }
  };

  let mut expected = snapshot;
  expected.canonicalize_for_baseline();
  let mut actual = live;
  actual.canonicalize_for_baseline();

  let expected_value = match serde_json::to_value(&expected) {
    Ok(value) => value,
    Err(err) => {
      return VerifySnapshotsCaseResult {
        case: make_case(
          VerifySnapshotsStatus::Drift,
          Some(format!("failed to serialize snapshot payload: {err}")),
        ),
        harness_options,
      };
    }
  };
  let actual_value = match serde_json::to_value(&actual) {
    Ok(value) => value,
    Err(err) => {
      return VerifySnapshotsCaseResult {
        case: make_case(
          VerifySnapshotsStatus::TscCrashed,
          Some(format!("failed to serialize live tsc payload: {err}")),
        ),
        harness_options,
      };
    }
  };

  if expected_value == actual_value {
    return VerifySnapshotsCaseResult {
      case: make_case(VerifySnapshotsStatus::Ok, None),
      harness_options,
    };
  }

  let detail = diff_payloads(&expected_value, &actual_value);
  VerifySnapshotsCaseResult {
    case: make_case(VerifySnapshotsStatus::Drift, Some(detail)),
    harness_options,
  }
}

struct CaseInputs {
  file_set: HarnessFileSet,
  options: serde_json::Map<String, Value>,
  harness_options: HarnessOptions,
  notes: Vec<String>,
}

fn build_case_inputs(path: &Path) -> std::result::Result<CaseInputs, String> {
  let content =
    crate::read_utf8_file(path).map_err(|err| format!("failed to read test input: {err}"))?;
  let split = crate::split_test_file(path, &content);
  let parsed = HarnessOptions::from_directives_with_options(
    &split.directives,
    DirectiveParseOptions::from_env(),
  );
  let mut notes = split.notes;
  notes.extend(parsed.notes);
  if let Some(note) = parsed.options.directives.ignored_directives_note() {
    notes.push(note);
  }
  let options = parsed.options.to_tsc_options_map();
  let file_set = HarnessFileSet::new(&split.files);
  Ok(CaseInputs {
    file_set,
    options,
    harness_options: parsed.options,
    notes,
  })
}

fn diff_payloads(expected: &Value, actual: &Value) -> String {
  let mut path = Vec::new();
  match diff_value(expected, actual, &mut path) {
    Some(detail) => detail,
    None => "payload differs".to_string(),
  }
}

fn diff_value(expected: &Value, actual: &Value, path: &mut Vec<String>) -> Option<String> {
  if expected == actual {
    return None;
  }

  match (expected, actual) {
    (Value::Object(expected), Value::Object(actual)) => {
      let mut expected_keys: Vec<_> = expected.keys().collect();
      expected_keys.sort();
      for key in expected_keys {
        if !actual.contains_key(key) {
          path.push(key.to_string());
          let pointer = json_pointer(path);
          path.pop();
          return Some(format!("{pointer}: missing from live output"));
        }
        path.push(key.to_string());
        if let Some(detail) = diff_value(&expected[key], &actual[key], path) {
          return Some(detail);
        }
        path.pop();
      }

      let mut actual_keys: Vec<_> = actual.keys().collect();
      actual_keys.sort();
      for key in actual_keys {
        if expected.contains_key(key) {
          continue;
        }
        path.push(key.to_string());
        let pointer = json_pointer(path);
        path.pop();
        return Some(format!("{pointer}: unexpected in live output"));
      }

      Some(format!("{}: payload differs", json_pointer(path)))
    }
    (Value::Array(expected), Value::Array(actual)) => {
      let min = expected.len().min(actual.len());
      for idx in 0..min {
        path.push(idx.to_string());
        if let Some(detail) = diff_value(&expected[idx], &actual[idx], path) {
          return Some(detail);
        }
        path.pop();
      }
      if expected.len() != actual.len() {
        return Some(format!(
          "{}: array length mismatch (snapshot={}, live={})",
          json_pointer(path),
          expected.len(),
          actual.len()
        ));
      }
      Some(format!("{}: array differs", json_pointer(path)))
    }
    _ => Some(format!(
      "{}: snapshot={} live={}",
      json_pointer(path),
      short_value(expected),
      short_value(actual)
    )),
  }
}

fn json_pointer(segments: &[String]) -> String {
  if segments.is_empty() {
    return "/".to_string();
  }
  let encoded: Vec<String> = segments.iter().map(|s| escape_pointer_segment(s)).collect();
  format!("/{}", encoded.join("/"))
}

fn escape_pointer_segment(segment: &str) -> String {
  segment.replace('~', "~0").replace('/', "~1")
}

fn short_value(value: &Value) -> String {
  let rendered = value.to_string();
  truncate(&rendered, 160)
}

fn truncate(input: &str, max_chars: usize) -> String {
  let mut chars = input.chars();
  let mut out = String::new();
  for _ in 0..max_chars {
    match chars.next() {
      Some(ch) => out.push(ch),
      None => return out,
    }
  }
  if chars.next().is_some() {
    out.push_str("...");
  }
  out
}
