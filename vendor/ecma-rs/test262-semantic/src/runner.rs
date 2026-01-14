use crate::discover::{read_utf8_file, DiscoveredTest};
use crate::executor::{ExecError, Executor, JsError};
use crate::frontmatter::{parse_test_source, Frontmatter};
use crate::harness::{assemble_source, HarnessMode};
use crate::report::{
  ExpectationOutcome, ExpectedOutcome, MismatchSummary, Summary, TestOutcome, TestResult, Variant,
};
use anyhow::{anyhow, bail, Context, Result};
use conformance_harness::{
  AppliedExpectation, Expectation, ExpectationKind, Expectations, Shard, TimeoutManager,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TestCase {
  pub id: String,
  pub path: PathBuf,
  pub variant: Variant,
  pub expected: ExpectedOutcome,
  pub metadata: Frontmatter,
  pub body: String,
}

#[derive(Debug, Clone)]
pub enum Filter {
  All,
  Glob(GlobSet),
  Regex(Regex),
}

pub fn build_filter(pattern: Option<&str>) -> Result<Filter> {
  match pattern {
    None => Ok(Filter::All),
    Some(raw) => {
      // Convenience: if a filter looks like a directory prefix (no glob metacharacters and does not
      // end in `.js`), treat it as `<prefix>/**` so callers can write:
      //   --filter built-ins/String/prototype/matchAll
      // instead of:
      //   --filter built-ins/String/prototype/matchAll/**
      //
      // This keeps the UX closer to substring/prefix filtering while still allowing explicit glob
      // patterns when needed.
      let raw = raw.trim();
      let mut normalized = raw.to_string();
      let has_glob_chars = raw.contains('*') || raw.contains('?') || raw.contains('[') || raw.contains('{');
      if !has_glob_chars && !raw.ends_with(".js") {
        if raw.ends_with('/') {
          normalized.push_str("**");
        } else {
          normalized.push_str("/**");
        }
      }

      if let Ok(glob) = Glob::new(&normalized) {
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
  pub fn matches(&self, id: &str) -> bool {
    match self {
      Filter::All => true,
      Filter::Glob(set) => set.is_match(id),
      Filter::Regex(re) => re.is_match(id),
    }
  }
}

pub fn expand_cases(selected: &[DiscoveredTest], filter: &Filter) -> Result<Vec<TestCase>> {
  let mut cases = Vec::new();
  for test in selected {
    if !filter.matches(&test.id) {
      continue;
    }
    let raw = read_utf8_file(&test.path)?;
    let parsed = parse_test_source(&raw).with_context(|| format!("parse {}", test.id))?;
    let metadata = parsed.frontmatter.unwrap_or_default();
    let expected = expected_outcome(&metadata);

    for variant in variants_for(&metadata) {
      cases.push(TestCase {
        id: test.id.clone(),
        path: test.path.clone(),
        variant,
        expected: expected.clone(),
        metadata: metadata.clone(),
        body: parsed.body.clone(),
      });
    }
  }

  if cases.is_empty() {
    bail!("no test cases selected");
  }

  cases.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.variant.cmp(&b.variant)));
  Ok(cases)
}

fn expected_outcome(metadata: &Frontmatter) -> ExpectedOutcome {
  match &metadata.negative {
    Some(negative) => ExpectedOutcome::Negative {
      phase: negative.phase.clone(),
      typ: negative.typ.clone(),
    },
    None => ExpectedOutcome::Pass,
  }
}

fn variants_for(metadata: &Frontmatter) -> Vec<Variant> {
  let flags: std::collections::HashSet<&str> = metadata.flags.iter().map(|s| s.as_str()).collect();

  if flags.contains("module") {
    return vec![Variant::Module];
  }

  if flags.contains("raw") {
    return vec![Variant::NonStrict];
  }

  if flags.contains("onlyStrict") {
    return vec![Variant::Strict];
  }
  if flags.contains("noStrict") {
    return vec![Variant::NonStrict];
  }

  vec![Variant::NonStrict, Variant::Strict]
}

pub fn apply_shard(cases: Vec<TestCase>, shard: Option<Shard>) -> Result<Vec<TestCase>> {
  let Some(shard) = shard else {
    return Ok(cases);
  };

  let total = cases.len();
  let filtered = conformance_harness::apply_shard(cases, shard);

  if filtered.is_empty() {
    bail!(
      "shard {}/{} matched no tests out of {total}",
      shard.index + 1,
      shard.total
    );
  }

  Ok(filtered)
}

pub fn run_cases(
  test262_dir: &Path,
  harness_mode: HarnessMode,
  cases: &[TestCase],
  expectations: &Expectations,
  executor: &dyn Executor,
  trace_cases: bool,
  timeout: Duration,
  timeout_manager: &TimeoutManager,
) -> Vec<TestResult> {
  cases
    .iter()
    .map(|case| {
      let mut expectation = expectations.lookup(&case.id);
      if expectation.expectation.kind != ExpectationKind::Skip {
        if let Some(reason) = auto_skip_reason(case) {
          expectation = AppliedExpectation {
            expectation: Expectation {
              kind: ExpectationKind::Skip,
              reason: Some(reason),
              tracking_issue: None,
            },
            from_manifest: false,
          };
        }
      }
      run_single_case(
        test262_dir,
        harness_mode,
        case,
        expectation,
        executor,
        trace_cases,
        timeout,
        timeout_manager,
      )
    })
    .collect()
}

/// Return a deterministic skip reason when `case` requires test262 features that `vm-js` does not
/// implement yet.
///
/// `test262-semantic` is primarily used to track `vm-js` progress. When a test declares required
/// features via YAML frontmatter (`features: [...]`), it's expected that harnesses will *skip* the
/// test when the engine does not implement those features, rather than running the test and
/// reporting a `ReferenceError` for missing built-ins.
fn auto_skip_reason(case: &TestCase) -> Option<String> {
  const UNSUPPORTED_FEATURES: &[&str] = &[
    // Atomics + SharedArrayBuffer are part of ECMA-262, but `vm-js` doesn't implement them yet.
    "Atomics",
    "SharedArrayBuffer",
    // Temporal is a stage-3 proposal and is not implemented by `vm-js` yet.
    "Temporal",
    // Proposals / staged features not implemented yet.
    "ShadowRealm",
    "source-phase-imports",
  ];
  const UNSUPPORTED_FEATURE_PREFIXES: &[&str] = &[
    // `vm-js` does not implement ECMA-402 Internationalization APIs yet.
    "Intl.",
  ];

  // `intl402/` is a separate test262 suite for ECMA-402 Internationalization APIs. Tests in this
  // directory do not consistently declare `features: [Intl.*]` metadata, so skip them explicitly
  // when Intl is not implemented.
  if case.id.starts_with("intl402/") {
    return Some("unsupported test262 suite: intl402 (Intl APIs not implemented)".to_string());
  }

  if case.metadata.features.is_empty() {
    return None;
  }

  let mut unsupported: Vec<&str> = Vec::new();
  for feature in &case.metadata.features {
    if UNSUPPORTED_FEATURES.contains(&feature.as_str())
      || UNSUPPORTED_FEATURE_PREFIXES
        .iter()
        .any(|prefix| feature.starts_with(prefix))
    {
      unsupported.push(feature);
    }
  }

  if unsupported.is_empty() {
    None
  } else {
    Some(format!(
      "unsupported test262 feature(s): {}",
      unsupported.join(", ")
    ))
  }
}

fn run_single_case(
  test262_dir: &Path,
  harness_mode: HarnessMode,
  case: &TestCase,
  expectation: AppliedExpectation,
  executor: &dyn Executor,
  trace_cases: bool,
  timeout: Duration,
  timeout_manager: &TimeoutManager,
) -> TestResult {
  if expectation.expectation.kind == ExpectationKind::Skip {
    return TestResult {
      id: case.id.clone(),
      path: format!("test/{}", case.id),
      variant: case.variant,
      expected: case.expected.clone(),
      outcome: TestOutcome::Skipped,
      error: None,
      skip_reason: expectation.expectation.reason.clone(),
      expectation: expectation_outcome(expectation, false),
      metadata: case.metadata.clone(),
      mismatched: false,
      expected_mismatch: false,
      flaky: false,
    };
  }

  let harness_mode = if case.metadata.flags.iter().any(|flag| flag == "raw") {
    HarnessMode::None
  } else {
    harness_mode
  };

  let source = match assemble_source(
    test262_dir,
    &case.metadata,
    case.variant,
    &case.body,
    harness_mode,
  ) {
    Ok(src) => src,
    Err(err) => {
      let mismatched = true;
      let expectation_out = expectation_outcome(expectation.clone(), mismatched);
      return TestResult {
        id: case.id.clone(),
        path: format!("test/{}", case.id),
        variant: case.variant,
        expected: case.expected.clone(),
        outcome: TestOutcome::Failed,
        error: Some(err.to_string()),
        skip_reason: None,
        expectation: expectation_out.clone(),
        metadata: case.metadata.clone(),
        mismatched,
        expected_mismatch: mismatched && expectation_out.expectation == ExpectationKind::Xfail,
        flaky: mismatched && expectation_out.expectation == ExpectationKind::Flaky,
      };
    }
  };

  let cancel = Arc::new(AtomicBool::new(false));
  let deadline = Instant::now() + timeout;
  let _timeout_guard = timeout_manager.register(deadline, Arc::clone(&cancel));

  if trace_cases {
    eprintln!("RUN {}#{}", case.id, case.variant);
    let _ = std::io::stderr().flush();
  }

  let executed = executor.execute(case, &source, &cancel);

  // `actual_outcome` describes what happened when attempting to run the test case, independent of
  // test262 metadata expectations (e.g. negative tests).
  //
  // The `TestResult::outcome` field, however, represents the *test verdict* (passed/failed) after
  // applying the test's expected outcome (including `negative:` metadata). This keeps summaries
  // and report comparisons aligned with test262's definition of "passing", where negative tests
  // pass by failing with the expected error.
  let (actual_outcome, mut error, skip_reason, js_error) = match executed {
    Ok(()) => (TestOutcome::Passed, None, None, None),
    Err(ExecError::Cancelled) => (
      TestOutcome::TimedOut,
      Some(format!("timeout after {} seconds", timeout.as_secs())),
      None,
      None,
    ),
    Err(ExecError::Skipped(reason)) => (TestOutcome::Skipped, None, Some(reason), None),
    Err(ExecError::Js(err)) => (
      TestOutcome::Failed,
      Some(err.to_report_string()),
      None,
      Some(err),
    ),
  };

  let (mismatched, mismatch_error) = mismatched(
    &case.expected,
    actual_outcome,
    js_error.as_ref(),
    expectation.expectation.kind,
  );
  if let Some(mismatch_error) = mismatch_error {
    error = Some(match error {
      Some(original) => format!("{mismatch_error}\n\n{original}"),
      None => mismatch_error,
    });
  }
  let expectation_out = expectation_outcome(expectation.clone(), mismatched);

  // Convert `actual_outcome` into a final verdict outcome:
  // - skipped stays skipped
  // - timeouts stay timeouts
  // - otherwise, any expectation mismatch is a failure
  // - matched expectations are a pass (including negative tests)
  let outcome = match actual_outcome {
    TestOutcome::Skipped => TestOutcome::Skipped,
    TestOutcome::TimedOut => TestOutcome::TimedOut,
    _ if mismatched => TestOutcome::Failed,
    _ => TestOutcome::Passed,
  };

  // Only attach an error when the test verdict is non-passing.
  if matches!(outcome, TestOutcome::Passed | TestOutcome::Skipped) {
    error = None;
  }

  TestResult {
    id: case.id.clone(),
    path: format!("test/{}", case.id),
    variant: case.variant,
    expected: case.expected.clone(),
    outcome,
    error,
    skip_reason,
    expectation: expectation_out.clone(),
    metadata: case.metadata.clone(),
    mismatched,
    expected_mismatch: mismatched && expectation_out.expectation == ExpectationKind::Xfail,
    flaky: mismatched && expectation_out.expectation == ExpectationKind::Flaky,
  }
}

fn mismatched(
  expected: &ExpectedOutcome,
  actual: TestOutcome,
  js_error: Option<&JsError>,
  expectation: ExpectationKind,
) -> (bool, Option<String>) {
  if expectation == ExpectationKind::Skip && actual == TestOutcome::Skipped {
    return (false, None);
  }

  match expected {
    ExpectedOutcome::Pass => (actual != TestOutcome::Passed, None),
    ExpectedOutcome::Negative {
      phase: expected_phase,
      typ: expected_typ,
    } => {
      // A negative test is only considered matched when it fails (not times out) with the expected
      // phase and error type. Treat unknown error types as mismatches to avoid masking
      // misclassifications in the executor/engine.
      if actual != TestOutcome::Failed {
        return (
          true,
          Some(format!(
            "negative expectation mismatch: expected {expected_phase} {expected_typ}, got {actual}"
          )),
        );
      }

      let Some(js_error) = js_error else {
        return (
          true,
          Some(format!(
            "negative expectation mismatch: expected {expected_phase} {expected_typ}, got non-JS failure"
          )),
        );
      };

      if !expected_phase.eq_ignore_ascii_case(js_error.phase.as_str()) {
        return (
          true,
          Some(format!(
            "negative expectation mismatch: expected {expected_phase} {expected_typ}, got {} {}",
            js_error.phase,
            js_error.typ.as_deref().unwrap_or("<unknown error type>"),
          )),
        );
      }

      let Some(actual_typ) = js_error.typ.as_deref() else {
        return (
          true,
          Some(format!(
            "negative expectation mismatch: expected {expected_phase} {expected_typ}, got {} <unknown error type>",
            js_error.phase
          )),
        );
      };

      if actual_typ != expected_typ {
        return (
          true,
          Some(format!(
            "negative expectation mismatch: expected {expected_phase} {expected_typ}, got {} {actual_typ}",
            js_error.phase
          )),
        );
      }

      (false, None)
    }
  }
}

pub fn summarize(results: &[TestResult]) -> Summary {
  let mut summary = Summary::default();
  let mut mismatches = MismatchSummary::default();

  for result in results {
    summary.total += 1;
    match result.outcome {
      TestOutcome::Passed => summary.passed += 1,
      TestOutcome::Failed => summary.failed += 1,
      TestOutcome::TimedOut => summary.timed_out += 1,
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

fn expectation_outcome(expectation: AppliedExpectation, mismatched: bool) -> ExpectationOutcome {
  ExpectationOutcome {
    expected: expectation.matches(mismatched),
    expectation: expectation.expectation.kind,
    from_manifest: expectation.from_manifest,
    reason: expectation.expectation.reason,
    tracking_issue: expectation.expectation.tracking_issue,
  }
}

pub fn select_all(discovered: &[DiscoveredTest]) -> Vec<DiscoveredTest> {
  let mut out = discovered.to_vec();
  out.sort_by(|a, b| a.id.cmp(&b.id));
  out
}

pub fn select_by_ids(discovered: &[DiscoveredTest], ids: &[String]) -> Result<Vec<DiscoveredTest>> {
  let mut map: std::collections::HashMap<&str, &DiscoveredTest> = std::collections::HashMap::new();
  for test in discovered {
    map.insert(test.id.as_str(), test);
  }

  let mut out = Vec::new();
  for id in ids {
    let found = map
      .get(id.as_str())
      .ok_or_else(|| anyhow!("selected id `{id}` was not discovered"))?;
    out.push((*found).clone());
  }
  out.sort_by(|a, b| a.id.cmp(&b.id));
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::executor::ExecPhase;
  use conformance_harness::{Expectations, TimeoutManager};
  use serde_json::Value;
  use std::fs;
  use std::sync::atomic::Ordering;
  use std::sync::Arc;
  use tempfile::tempdir;

  #[test]
  fn build_filter_directory_prefix_matches_descendants() {
    let filter = build_filter(Some("built-ins/RegExp/prototype/flags")).unwrap();
    assert!(filter.matches("built-ins/RegExp/prototype/flags/get-order.js"));
    assert!(filter.matches("built-ins/RegExp/prototype/flags/return-order.js"));
    assert!(!filter.matches("built-ins/RegExp/prototype/flag/get-order.js"));
  }

  #[test]
  fn build_filter_directory_prefix_with_trailing_slash_matches_descendants() {
    let filter = build_filter(Some("built-ins/RegExp/prototype/flags/")).unwrap();
    assert!(filter.matches("built-ins/RegExp/prototype/flags/get-order.js"));
    assert!(!filter.matches("built-ins/RegExp/prototype/flag/get-order.js"));
  }

  #[test]
  fn manifest_prefers_exact_then_glob_then_regex() {
    let manifest = r#"
[[expectations]]
glob = "a/**"
status = "xfail"

[[expectations]]
regex = "a/.*"
status = "flaky"

[[expectations]]
id = "a/b/c.js"
status = "skip"
    "#;

    let expectations = Expectations::from_str(manifest).expect("manifest parsed");
    let applied = expectations.lookup("a/b/c.js");
    assert_eq!(applied.expectation.kind, ExpectationKind::Skip);
  }

  #[test]
  fn report_serializes_stably() {
    let result = TestResult {
      id: "language/example.js".to_string(),
      path: "test/language/example.js".to_string(),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      outcome: TestOutcome::Passed,
      error: None,
      skip_reason: None,
      expectation: ExpectationOutcome {
        expectation: ExpectationKind::Pass,
        expected: true,
        from_manifest: false,
        reason: None,
        tracking_issue: None,
      },
      metadata: Frontmatter::default(),
      mismatched: false,
      expected_mismatch: false,
      flaky: false,
    };

    let summary = summarize(std::slice::from_ref(&result));
    let report = crate::report::Report {
      schema_version: crate::REPORT_SCHEMA_VERSION,
      summary,
      results: vec![result],
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["schema_version"], crate::REPORT_SCHEMA_VERSION);
    assert_eq!(parsed["results"][0]["id"], "language/example.js");
  }

  #[test]
  fn expand_generates_strict_and_non_strict_variants() {
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("harness")).unwrap();
    fs::write(temp.path().join("harness/assert.js"), "").unwrap();
    fs::write(temp.path().join("harness/sta.js"), "").unwrap();
    let test_dir = temp.path().join("test");
    fs::create_dir_all(&test_dir).unwrap();
    fs::write(
      test_dir.join("a.js"),
      "/*---\nflags: []\n---*/\nlet x = 1;\n",
    )
    .unwrap();

    let discovered = vec![DiscoveredTest {
      id: "a.js".to_string(),
      path: test_dir.join("a.js"),
    }];

    let cases = expand_cases(&discovered, &Filter::All).unwrap();
    let variants: Vec<_> = cases.iter().map(|c| c.variant).collect();
    assert_eq!(variants, vec![Variant::NonStrict, Variant::Strict]);
  }

  #[test]
  fn variants_for_raw_generates_only_non_strict() {
    let metadata = Frontmatter {
      flags: vec!["raw".to_string()],
      ..Frontmatter::default()
    };
    assert_eq!(variants_for(&metadata), vec![Variant::NonStrict]);
  }

  #[derive(Debug, Clone)]
  struct DummyExecutor {
    result: crate::executor::ExecResult,
  }

  impl Executor for DummyExecutor {
    fn execute(
      &self,
      _case: &TestCase,
      _source: &str,
      cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> crate::executor::ExecResult {
      if cancel.load(Ordering::Relaxed) {
        return Err(ExecError::Cancelled);
      }
      self.result.clone()
    }
  }

  fn test262_fixture() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("harness")).unwrap();
    fs::write(temp.path().join("harness/assert.js"), "").unwrap();
    fs::write(temp.path().join("harness/sta.js"), "").unwrap();
    temp
  }

  #[test]
  fn auto_skip_reason_skips_unsupported_features() {
    let case = TestCase {
      id: "built-ins/Atomics/Symbol.toStringTag.js".to_string(),
      path: PathBuf::from("test/built-ins/Atomics/Symbol.toStringTag.js"),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter {
        features: vec!["Atomics".to_string(), "Symbol".to_string()],
        ..Frontmatter::default()
      },
      body: String::new(),
    };
    assert_eq!(
      auto_skip_reason(&case),
      Some("unsupported test262 feature(s): Atomics".to_string())
    );
  }

  #[test]
  fn auto_skip_reason_skips_temporal_feature() {
    let case = TestCase {
      id: "built-ins/Temporal/toStringTag/string.js".to_string(),
      path: PathBuf::from("test/built-ins/Temporal/toStringTag/string.js"),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter {
        features: vec!["Symbol.toStringTag".to_string(), "Temporal".to_string()],
        ..Frontmatter::default()
      },
      body: String::new(),
    };
    assert_eq!(
      auto_skip_reason(&case),
      Some("unsupported test262 feature(s): Temporal".to_string())
    );
  }

  #[test]
  fn auto_skip_reason_skips_intl402_suite() {
    let case = TestCase {
      id: "intl402/Collator/prototype/toStringTag/toString.js".to_string(),
      path: PathBuf::from("test/intl402/Collator/prototype/toStringTag/toString.js"),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter {
        // Many intl402 tests do not declare `Intl.*` in the `features:` frontmatter, so this suite
        // must be skipped based on path until Intl is implemented.
        features: vec!["Symbol.toStringTag".to_string()],
        ..Frontmatter::default()
      },
      body: String::new(),
    };
    assert_eq!(
      auto_skip_reason(&case),
      Some("unsupported test262 suite: intl402 (Intl APIs not implemented)".to_string())
    );
  }

  fn run_negative_case(js_error: JsError, expected_phase: &str, expected_type: &str) -> TestResult {
    let temp = test262_fixture();
    let case = TestCase {
      id: "language/negative.js".to_string(),
      path: temp.path().join("test/language/negative.js"),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Negative {
        phase: expected_phase.to_string(),
        typ: expected_type.to_string(),
      },
      metadata: Frontmatter::default(),
      body: "/* body unused in dummy executor */\n".to_string(),
    };

    let executor = DummyExecutor {
      result: Err(ExecError::Js(js_error)),
    };
    let expectations = Expectations::empty();
    let expectation = expectations.lookup(&case.id);

    let timeout_manager = TimeoutManager::new();
    run_single_case(
      temp.path(),
      HarnessMode::Test262,
      &case,
      expectation,
      &executor,
      false,
      Duration::from_secs(1),
      &timeout_manager,
    )
  }

  #[test]
  fn negative_parse_expectation_matches_parse_error() {
    let result = run_negative_case(
      JsError::new(
        ExecPhase::Parse,
        Some("SyntaxError".to_string()),
        "unexpected token",
      ),
      "parse",
      "SyntaxError",
    );
    assert_eq!(result.outcome, TestOutcome::Passed);
    assert!(
      !result.mismatched,
      "expected matched negative, got {result:#?}"
    );
    assert!(
      result.error.is_none(),
      "matched negative tests should not record an error, got {result:#?}"
    );
  }

  #[test]
  fn negative_parse_expectation_mismatches_runtime_error() {
    let result = run_negative_case(
      JsError::new(ExecPhase::Runtime, Some("TypeError".to_string()), "boom"),
      "parse",
      "SyntaxError",
    );
    assert_eq!(result.outcome, TestOutcome::Failed);
    assert!(result.mismatched);
    assert!(
      result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("expected parse SyntaxError, got runtime TypeError"),
      "error message should explain mismatch, got: {:#?}",
      result.error
    );
  }

  #[test]
  fn negative_runtime_typeerror_expectation_mismatches_rangeerror() {
    let result = run_negative_case(
      JsError::new(
        ExecPhase::Runtime,
        Some("RangeError".to_string()),
        "out of range",
      ),
      "runtime",
      "TypeError",
    );
    assert_eq!(result.outcome, TestOutcome::Failed);
    assert!(result.mismatched);
    assert!(
      result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("expected runtime TypeError, got runtime RangeError"),
      "error message should explain mismatch, got: {:#?}",
      result.error
    );
  }

  #[test]
  fn negative_runtime_typeerror_expectation_mismatches_unknown_error_type() {
    let result = run_negative_case(
      JsError::new(ExecPhase::Runtime, None, "unknown error"),
      "runtime",
      "TypeError",
    );
    assert_eq!(result.outcome, TestOutcome::Failed);
    assert!(result.mismatched);
    assert!(
      result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("unknown error type"),
      "error message should mention unknown type, got: {:#?}",
      result.error
    );
  }
}
