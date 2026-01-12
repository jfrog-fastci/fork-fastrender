use crate::{
  discover_tests, BackendKind, BackendSelection, RunError, RunOutcome, Runner, RunnerConfig,
  TestCase, TestKind, WptFs, WptReport,
};
use anyhow::{anyhow, bail, Context, Result};
use conformance_harness::{AppliedExpectation, ExpectationKind, Expectations, FailOn, Shard};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
  Passed,
  Failed,
  TimedOut,
  Errored,
  Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectationOutcome {
  pub expectation: ExpectationKind,
  #[serde(default)]
  pub expected: bool,
  #[serde(default)]
  pub from_manifest: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tracking_issue: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MismatchSummary {
  pub expected: usize,
  pub unexpected: usize,
  pub flaky: usize,
}

impl MismatchSummary {
  pub fn total(&self) -> usize {
    self.expected + self.unexpected + self.flaky
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Summary {
  pub total: usize,
  pub passed: usize,
  pub failed: usize,
  pub timed_out: usize,
  pub errored: usize,
  pub skipped: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mismatches: Option<MismatchSummary>,
}

impl Summary {
  pub fn should_fail(&self, fail_on: FailOn) -> bool {
    let mismatches = self.mismatches.as_ref().map(|m| m.total()).unwrap_or(0);
    let unexpected = self.mismatches.as_ref().map(|m| m.unexpected).unwrap_or(0);
    fail_on.should_fail(unexpected, mismatches)
  }
}

pub fn should_fail(summary: &Summary, fail_on: FailOn) -> bool {
  summary.should_fail(fail_on)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestResult {
  pub id: String,
  pub path: String,
  pub kind: TestKind,
  pub outcome: TestOutcome,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub skip_reason: Option<String>,
  pub expectation: ExpectationOutcome,
  #[serde(default, skip_serializing_if = "is_false")]
  pub mismatched: bool,
  #[serde(default, skip_serializing_if = "is_false")]
  pub expected_mismatch: bool,
  #[serde(default, skip_serializing_if = "is_false")]
  pub flaky: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub wpt_report: Option<WptReport>,
}

fn is_false(value: &bool) -> bool {
  !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
  pub schema_version: u32,
  pub summary: Summary,
  pub results: Vec<TestResult>,
}

#[derive(Debug, Clone)]
pub struct SuiteConfig {
  pub wpt_root: PathBuf,
  pub manifest_path: PathBuf,
  pub shard: Option<Shard>,
  pub filter: Option<String>,
  pub timeout: Duration,
  pub long_timeout: Duration,
  pub fail_on: FailOn,
  /// Which JS backend to execute tests with.
  pub backend: BackendSelection,
}

#[derive(Debug, Clone)]
enum Filter {
  All,
  Glob(GlobSet),
  Regex(Regex),
}

fn build_filter(pattern: Option<&str>) -> Result<Filter> {
  match pattern {
    None => Ok(Filter::All),
    Some(raw) => {
      let raw = raw.trim();
      if raw.is_empty() {
        return Ok(Filter::All);
      }

      // Prefer regex semantics for patterns that are unambiguously regex-like. In particular, `|`
      // is not supported as alternation in glob patterns, but is commonly used for "OR" filters
      // (e.g. `--filter "documentURI|compatMode"`). Without this, such filters are parsed as globs
      // and end up selecting zero tests.
      if raw.contains('|') {
        let regex = Regex::new(raw).map_err(|err| anyhow!("invalid regex: {err}"))?;
        return Ok(Filter::Regex(regex));
      }

      // Support a comma-separated list of glob patterns for suite presets that want a union of
      // directories (e.g. `dom/**,event_loop/**,events/**`).
      //
      // Each segment is treated as an independent glob. If any segment fails to parse as a glob we
      // fall back to the legacy `glob or regex` behavior below.
      if raw.contains(',') {
        let parts: Vec<&str> = raw
          .split(',')
          .map(str::trim)
          .filter(|part| !part.is_empty())
          .collect();
        if !parts.is_empty() {
          let mut builder = GlobSetBuilder::new();
          let mut ok = true;
          for part in parts {
            match Glob::new(part) {
              Ok(glob) => {
                builder.add(glob);
              }
              Err(_) => {
                ok = false;
                break;
              }
            }
          }
          if ok {
            let set = builder
              .build()
              .map_err(|err| anyhow!("invalid glob: {err}"))?;
            return Ok(Filter::Glob(set));
          }
        }
      }

      // Developer ergonomics: treat patterns without glob meta characters as substring globs.
      //
      // The CLI advertises `--filter` as "glob or regex". However, patterns like "range_" parse as a
      // valid glob that matches only the exact id "range_", which is almost never what a caller
      // intends. Mapping plain strings to `*{pattern}*` matches common expectations ("run tests whose
      // id contains this substring") while keeping wildcard-heavy globs (e.g. `dom/**`) unchanged.
      //
      // Note: If the user provided a comma-separated list we intentionally do *not* apply this
      // rewriting; each list segment has its own glob semantics above.
      let raw = if raw.contains(',') || raw.contains('*') || raw.contains('?') || raw.contains('[') {
        raw.to_string()
      } else {
        format!("*{raw}*")
      };

      if let Ok(glob) = Glob::new(raw.as_str()) {
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let set = builder
          .build()
          .map_err(|err| anyhow!("invalid glob: {err}"))?;
        return Ok(Filter::Glob(set));
      }

      let regex = Regex::new(raw.as_str()).map_err(|err| anyhow!("invalid regex: {err}"))?;
      Ok(Filter::Regex(regex))
    }
  }
}

impl Filter {
  fn matches(&self, id: &str) -> bool {
    match self {
      Filter::All => true,
      Filter::Glob(set) => set.is_match(id),
      Filter::Regex(re) => re.is_match(id),
    }
  }
}

pub fn run_suite(config: &SuiteConfig) -> Result<Report> {
  let trace_tests = std::env::var("FASTERENDER_WPT_DOM_TRACE_TESTS")
    .ok()
    .is_some_and(|v| !v.trim().is_empty() && v.trim() != "0");

  let fs = WptFs::new(&config.wpt_root).with_context(|| {
    format!(
      "create WPT fs rooted at {}",
      config.wpt_root.as_path().display()
    )
  })?;

  let mut discovered =
    discover_tests(fs.tests_root()).context("discover WPT DOM tests from corpus")?;
  discovered.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.kind.cmp(&b.kind)));

  let filter_pattern = config.filter.as_deref();
  let filter = build_filter(filter_pattern)?;
  let mut selected: Vec<TestCase> = discovered
    .iter()
    .filter(|t| filter.matches(&t.id))
    .cloned()
    .collect();

  if selected.is_empty() {
    // `--filter` is documented as "glob or regex" (with glob tried first). This means that a bare
    // substring like `--filter assert_throws` is treated as a glob that only matches the *entire*
    // test id, which is surprising and easy to trip over.
    //
    // If the glob matched zero tests, fall back to treating the pattern as a regex. This keeps the
    // exact-id glob behavior working when there *is* an exact match while enabling the more common
    // "substring match" workflow for bare filters.
    if let (Some(raw), Filter::Glob(_)) = (filter_pattern, &filter) {
      let raw = raw.trim();
      if !raw.is_empty() {
        if let Ok(re) = Regex::new(raw) {
          selected = discovered
            .iter()
            .filter(|t| re.is_match(&t.id))
            .cloned()
            .collect();
        }
      }
    }
    if selected.is_empty() {
      bail!("suite selected zero tests");
    }
  }

  selected.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.kind.cmp(&b.kind)));

  if let Some(shard) = config.shard {
    let total = selected.len();
    selected = conformance_harness::apply_shard(selected, shard);
    if selected.is_empty() {
      bail!(
        "shard {}/{} matched no tests out of {total}",
        shard.index + 1,
        shard.total
      );
    }
  }

  let backend_kind = resolve_suite_backend(config.backend)?;
  let expectations = load_expectations_filtered(&config.manifest_path, backend_kind)
    .with_context(|| format!("load expectations {}", config.manifest_path.display()))?;

  let runner = Runner::new(
    fs,
    RunnerConfig {
      default_timeout: config.timeout,
      long_timeout: config.long_timeout,
      backend: config.backend,
      ..RunnerConfig::default()
    },
  );

  let mut results: Vec<TestResult> = Vec::with_capacity(selected.len());
  for (idx, test) in selected.iter().enumerate() {
    if trace_tests {
      eprintln!("[{}/{}] {}", idx + 1, selected.len(), test.id);
      let _ = std::io::stderr().flush();
    }
    results.push(run_single(&runner, test, expectations.lookup(&test.id)));
  }

  results.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.kind.cmp(&b.kind)));

  let summary = summarize(&results);

  Ok(Report {
    schema_version: REPORT_SCHEMA_VERSION,
    summary,
    results,
  })
}

fn resolve_suite_backend(selection: BackendSelection) -> Result<BackendKind> {
  // Mirror the backend resolution logic in `Runner` so expectation filtering stays in sync with the
  // actual backend used for test execution.
  let selection = if selection == BackendSelection::Auto {
    BackendSelection::from_env()
      .map_err(|err| anyhow!(err))?
      .unwrap_or(BackendSelection::Auto)
  } else {
    selection
  };
  Ok(selection.resolve())
}

#[derive(Debug, Clone, Deserialize)]
struct RawManifest {
  #[serde(default)]
  expectations: Vec<RawEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawEntry {
  id: Option<String>,
  glob: Option<String>,
  regex: Option<String>,
  #[serde(alias = "expectation")]
  status: Option<ExpectationKind>,
  reason: Option<String>,
  tracking_issue: Option<String>,
  #[serde(alias = "backends")]
  backend: Option<BackendConstraint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BackendConstraint {
  One(String),
  Many(Vec<String>),
}

impl BackendConstraint {
  fn matches(&self, backend: BackendKind) -> Result<bool> {
    let matches_one = |raw: &str| -> Result<bool> {
      let normalized = raw.trim().to_ascii_lowercase();
      let kind = match normalized.as_str() {
        "vmjs" | "vm-js" | "vm_js" => BackendKind::VmJs,
        "vmjs-rendered" | "vm-js-rendered" | "vm_js_rendered" | "vmjs_rendered" | "vmjsrendered" => {
          BackendKind::VmJsRendered
        }
        "quickjs" | "quick-js" | "quick_js" => BackendKind::QuickJs,
        other => {
          bail!("unknown backend selector {other:?} (expected vmjs|vmjs-rendered|quickjs)");
        }
      };
      Ok(kind == backend)
    };

    match self {
      BackendConstraint::One(s) => matches_one(s),
      BackendConstraint::Many(values) => {
        for value in values {
          if matches_one(value)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
    }
  }
}

fn load_expectations_filtered(
  path: &std::path::Path,
  backend: BackendKind,
) -> Result<Expectations> {
  let raw =
    std::fs::read_to_string(path).with_context(|| format!("read manifest {}", path.display()))?;

  let manifest = match toml::from_str::<RawManifest>(&raw) {
    Ok(manifest) => manifest,
    Err(toml_err) => serde_json::from_str::<RawManifest>(&raw).map_err(|json_err| {
      anyhow!("failed to parse manifest as TOML ({toml_err}) or JSON ({json_err})")
    })?,
  };

  let mut filtered = Vec::new();
  for entry in manifest.expectations {
    let keep = match &entry.backend {
      None => true,
      Some(constraint) => constraint
        .matches(backend)
        .with_context(|| "invalid backend expectation selector")?,
    };
    if !keep {
      continue;
    }

    filtered.push(json!({
      "id": entry.id,
      "glob": entry.glob,
      "regex": entry.regex,
      "status": entry.status,
      "reason": entry.reason,
      "tracking_issue": entry.tracking_issue,
    }));
  }

  let json_manifest = serde_json::to_string(&json!({ "expectations": filtered }))
    .context("serialize filtered expectations manifest")?;
  Expectations::from_str(&json_manifest).map_err(|err| anyhow!("{}: {err}", path.display()))
}

fn run_single(runner: &Runner, test: &TestCase, expectation: AppliedExpectation) -> TestResult {
  if expectation.expectation.kind == ExpectationKind::Skip {
    return finalize_result(
      test,
      TestOutcome::Skipped,
      None,
      expectation.expectation.reason.clone(),
      expectation,
      None,
    );
  }

  match runner.run_test(test) {
    Ok(run) => match run.outcome {
      RunOutcome::Pass => finalize_result(
        test,
        TestOutcome::Passed,
        None,
        None,
        expectation,
        run.wpt_report,
      ),
      RunOutcome::Fail(msg) => finalize_result(
        test,
        TestOutcome::Failed,
        Some(msg),
        None,
        expectation,
        run.wpt_report,
      ),
      RunOutcome::Timeout => finalize_result(
        test,
        TestOutcome::TimedOut,
        Some("timeout".to_string()),
        None,
        expectation,
        run.wpt_report,
      ),
      RunOutcome::Error(msg) => finalize_result(
        test,
        TestOutcome::Errored,
        Some(msg),
        None,
        expectation,
        run.wpt_report,
      ),
      RunOutcome::Skip(reason) => finalize_result(
        test,
        TestOutcome::Skipped,
        None,
        Some(reason),
        expectation,
        run.wpt_report,
      ),
    },
    Err(err) => finalize_result(
      test,
      TestOutcome::Errored,
      Some(run_error_string(err)),
      None,
      expectation,
      None,
    ),
  }
}

fn run_error_string(err: RunError) -> String {
  err.to_string()
}

fn finalize_result(
  test: &TestCase,
  outcome: TestOutcome,
  error: Option<String>,
  skip_reason: Option<String>,
  expectation: AppliedExpectation,
  wpt_report: Option<WptReport>,
) -> TestResult {
  let mismatched = match outcome {
    TestOutcome::Passed | TestOutcome::Skipped => false,
    TestOutcome::Failed | TestOutcome::TimedOut | TestOutcome::Errored => true,
  };

  let expectation_out = expectation_outcome(expectation, mismatched);

  TestResult {
    id: test.id.clone(),
    path: format!("tests/{}", test.id),
    kind: test.kind,
    outcome,
    error,
    skip_reason,
    expectation: expectation_out.clone(),
    mismatched,
    expected_mismatch: mismatched && expectation_out.expectation == ExpectationKind::Xfail,
    flaky: mismatched && expectation_out.expectation == ExpectationKind::Flaky,
    wpt_report,
  }
}

fn expectation_outcome(expectation: AppliedExpectation, mismatched: bool) -> ExpectationOutcome {
  ExpectationOutcome {
    expected: expectation.matches(mismatched),
    expectation: expectation.expectation.kind,
    from_manifest: expectation.from_manifest,
    reason: expectation.expectation.reason,
    tracking_issue: expectation.expectation.tracking_issue,
  }
}

fn summarize(results: &[TestResult]) -> Summary {
  let mut summary = Summary::default();
  let mut mismatches = MismatchSummary::default();

  for result in results {
    summary.total += 1;
    match result.outcome {
      TestOutcome::Passed => summary.passed += 1,
      TestOutcome::Failed => summary.failed += 1,
      TestOutcome::TimedOut => summary.timed_out += 1,
      TestOutcome::Errored => summary.errored += 1,
      TestOutcome::Skipped => summary.skipped += 1,
    }

    if result.mismatched {
      if result.flaky {
        mismatches.flaky += 1;
      } else if result.expected_mismatch {
        mismatches.expected += 1;
      } else {
        mismatches.unexpected += 1;
      }
    }
  }

  if mismatches.total() > 0 {
    summary.mismatches = Some(mismatches);
  }

  summary
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn filter_globset_supports_comma_separated_globs() {
    let filter = build_filter(Some("dom/**,events/**")).expect("parse filter");
    assert!(filter.matches("dom/element_matches_closest.window.js"));
    assert!(filter.matches("events/eventtarget.window.js"));
    assert!(!filter.matches("smoke/sync-pass.html"));
  }

  #[test]
  fn filter_globset_trims_whitespace_and_ignores_empty_segments() {
    let filter = build_filter(Some(" dom/** , , events/** ,")).expect("parse filter");
    assert!(filter.matches("dom/element_query_selector.window.js"));
    assert!(filter.matches("events/eventtarget_order.window.js"));
    assert!(!filter.matches("event_loop/settimeout_args.window.js"));
  }
}
