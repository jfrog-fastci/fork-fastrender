use crate::{
  discover_tests, BackendSelection, RunError, RunOutcome, Runner, RunnerConfig, TestCase, TestKind,
  WptFs, WptReport,
};
use anyhow::{anyhow, bail, Context, Result};
use conformance_harness::{AppliedExpectation, ExpectationKind, Expectations, FailOn, Shard};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
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
      if let Ok(glob) = Glob::new(raw) {
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let set = builder
          .build()
          .map_err(|err| anyhow!("invalid glob: {err}"))?;
        return Ok(Filter::Glob(set));
      }

      let regex = Regex::new(raw).map_err(|err| anyhow!("invalid regex: {err}"))?;
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
  let fs = WptFs::new(&config.wpt_root).with_context(|| {
    format!(
      "create WPT fs rooted at {}",
      config.wpt_root.as_path().display()
    )
  })?;

  let mut discovered =
    discover_tests(fs.tests_root()).context("discover WPT DOM tests from corpus")?;
  discovered.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.kind.cmp(&b.kind)));

  let filter = build_filter(config.filter.as_deref())?;
  let mut selected: Vec<TestCase> = discovered
    .into_iter()
    .filter(|t| filter.matches(&t.id))
    .collect();

  if selected.is_empty() {
    bail!("suite selected zero tests");
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

  let expectations = Expectations::from_path(&config.manifest_path)
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

  let mut results: Vec<TestResult> = selected
    .iter()
    .map(|test| run_single(&runner, test, expectations.lookup(&test.id)))
    .collect();

  results.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.kind.cmp(&b.kind)));

  let summary = summarize(&results);

  Ok(Report {
    schema_version: REPORT_SCHEMA_VERSION,
    summary,
    results,
  })
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
