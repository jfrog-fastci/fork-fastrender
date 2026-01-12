//! Fixture discovery and expectation-based execution utilities.
//!
//! This module is shared by:
//! - the `native-oracle-harness` CLI (`src/main.rs`), and
//! - the `native_oracle_fixtures_pass` test (`tests/fixtures.rs`).
//!
//! It is intended to be reusable for a future **native-vs-oracle** comparison harness: fixtures are
//! discovered once ([`FixtureCase`]), then executed via an injected runner closure.
//!
//! ## Fixture kinds / protocols
//!
//! The `vendor/ecma-rs/fixtures/native_oracle/` directory contains two fixture styles:
//!
//! - **Observe protocol** (`*.ts` / `*.tsx`, or module directories with `entry.ts` / `entry.tsx` /
//!   `entry.js`):
//!   - The fixture should assign its deterministic observation string to
//!     `globalThis.__native_result`.
//!   - The harness evaluates `String(globalThis.__native_result)` after a microtask checkpoint.
//!   - These are executed via `run_fixture_ts*` APIs.
//!
//! - **Promise-return protocol** (`*.js`):
//!   - The script completion value must be either a string, or a `Promise<string>`.
//!   - These are executed via `run_fixture*` APIs.
//!
//! Only the observe-protocol fixtures currently participate in the expectation suite, but
//! [`FixtureKind`] allows this module to be reused for both protocols.
//!
//! Directory-based module fixtures are executed via [`crate::run_fixture_ts_module_dir`].
//!
//! ## Expected output rules
//!
//! For expectation-based suites, expected output is determined as follows:
//! 1) The first line with a `// EXPECT:` comment (leading whitespace allowed) wins. The remainder of
//!    the line is trimmed and used as the expected output string.
//! 2) Otherwise, if a sibling `*.out` file exists (same basename), its contents are used after
//!    trimming trailing newlines (`trim_end`).
//!
//! For directory-based module fixtures, the `// EXPECT:` comment is parsed from `entry.*`, but the
//! `.out` fallback is looked up as `<dir>.out` (i.e. `dir.with_extension("out")`).
//!
//! If neither is present, the fixture is considered a failure (missing expectation).

use std::fs;
use std::path::{Path, PathBuf};

use diagnostics::files::SimpleFiles;
use diagnostics::render::render_diagnostic;
use diagnostics::Diagnostic;

/// The fixture execution protocol implied by the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureKind {
  /// Observe-protocol fixtures that use `globalThis.__native_result`.
  Observe,
  /// `*.js` fixtures that return a string or `Promise<string>`.
  PromiseReturn,
}

impl FixtureKind {
  fn from_extension(ext: &str) -> Option<Self> {
    match ext {
      "ts" | "tsx" => Some(Self::Observe),
      "js" => Some(Self::PromiseReturn),
      _ => None,
    }
  }
}

/// A discovered fixture file and its parsed expectation (if any).
#[derive(Debug, Clone)]
pub struct FixtureCase {
  /// For single-file fixtures, the file path.
  ///
  /// For directory-based module fixtures, the entry module path (`<dir>/entry.*`).
  pub path: PathBuf,
  /// For directory-based module fixtures, the directory containing `entry.*`.
  pub module_dir: Option<PathBuf>,
  pub source: String,
  pub expected: Option<String>,
  pub kind: FixtureKind,
}

/// Discover fixtures under `dir` and return them in deterministic order.
///
/// The directory is scanned non-recursively. Fixtures are discovered as either:
/// - files recognized by extension:
///   - `*.ts` / `*.tsx` → [`FixtureKind::Observe`]
///   - `*.js` → [`FixtureKind::PromiseReturn`]
/// - directories recognized as module fixtures if they contain `entry.ts` / `entry.tsx` / `entry.js`
///   (in that priority order). These are always [`FixtureKind::Observe`].
///
/// Expected output is parsed via [`parse_expected_output`].
pub fn discover_native_oracle_fixtures(dir: &Path) -> Vec<FixtureCase> {
  fn stable_rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel
      .components()
      .map(|c| c.as_os_str().to_string_lossy())
      .collect::<Vec<_>>()
      .join("/")
  }

  let mut cases: Vec<FixtureCase> = Vec::new();
  for entry in fs::read_dir(dir).unwrap_or_else(|err| panic!("failed to read fixture dir {dir:?}: {err}"))
  {
    let path = entry
      .unwrap_or_else(|err| panic!("failed to read fixture dir entry under {dir:?}: {err}"))
      .path();

    if path.is_file() {
      let Some(kind) = path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(FixtureKind::from_extension)
      else {
        continue;
      };
      let source =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read fixture {path:?}: {err}"));
      let expected = parse_expected_output(&source, &path);
      cases.push(FixtureCase {
        path,
        module_dir: None,
        source,
        expected,
        kind,
      });
      continue;
    }

    if path.is_dir() {
      let entry_path = ["entry.ts", "entry.tsx", "entry.js"]
        .into_iter()
        .map(|name| path.join(name))
        .find(|p| p.is_file());
      let Some(entry_path) = entry_path else {
        continue;
      };

      let source = fs::read_to_string(&entry_path)
        .unwrap_or_else(|err| panic!("failed to read module fixture entry {entry_path:?}: {err}"));
      // For directory fixtures, `.out` expectations live next to the directory (`<dir>.out`), so
      // pass the directory path to `parse_expected_output`.
      let expected = parse_expected_output(&source, &path);
      cases.push(FixtureCase {
        path: entry_path,
        module_dir: Some(path),
        source,
        expected,
        kind: FixtureKind::Observe,
      });
    }
  }

  cases.sort_by(|a, b| stable_rel_path(dir, &a.path).cmp(&stable_rel_path(dir, &b.path)));
  cases
}

/// Parse the expected output for a fixture file.
///
/// See the [module-level documentation](self) for the exact rules.
pub fn parse_expected_output(source: &str, path: &Path) -> Option<String> {
  for line in source.lines() {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix("// EXPECT:") {
      return Some(rest.trim().to_string());
    }
  }

  let out_path = path.with_extension("out");
  fs::read_to_string(out_path).ok().map(|s| s.trim_end().to_string())
}

#[derive(Debug)]
pub enum FixtureFailureKind {
  MissingExpected,
  ExecutionFailed { diagnostic: Diagnostic, rendered: String },
  Mismatch {
    expected: String,
    actual: String,
    diff: Option<String>,
  },
}

#[derive(Debug)]
pub struct FixtureFailure {
  pub path: PathBuf,
  pub kind: FixtureKind,
  pub failure: FixtureFailureKind,
}

impl FixtureFailure {
  pub fn render(&self) -> String {
    match &self.failure {
      FixtureFailureKind::MissingExpected => format!(
        "FAIL {} (missing // EXPECT: comment and no .out file)",
        self.path.display()
      ),
      FixtureFailureKind::ExecutionFailed { rendered, .. } => format!(
        "FAIL {} (diagnostic)\n{}",
        self.path.display(),
        rendered.trim_end()
      ),
      FixtureFailureKind::Mismatch {
        expected,
        actual,
        diff,
      } => {
        let mut msg = format!(
          "FAIL {} expected={expected:?} actual={actual:?}",
          self.path.display()
        );
        if let Some(diff) = diff {
          msg.push('\n');
          msg.push_str(diff.trim_end());
        }
        msg
      }
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct ExpectationSuiteOptions {
  /// Whether to compute a short diff string for mismatches.
  pub include_diff: bool,
}

impl Default for ExpectationSuiteOptions {
  fn default() -> Self {
    Self { include_diff: true }
  }
}

/// A summary of an expectation-based fixture suite run.
#[derive(Debug, Default)]
pub struct SuiteReport {
  pub total: usize,
  pub passed: usize,
  pub failed: usize,
  pub failures: Vec<FixtureFailure>,
}

impl SuiteReport {
  pub fn is_success(&self) -> bool {
    self.failed == 0
  }

  pub fn failure_for_path(&self, path: &Path) -> Option<&FixtureFailure> {
    self.failures.iter().find(|f| f.path == path)
  }

  pub fn render(&self) -> String {
    let mut out = String::new();
    for failure in &self.failures {
      out.push_str(&failure.render());
      out.push('\n');
    }
    out.push_str(&format!(
      "summary: {} passed; {} failed; {} total",
      self.passed, self.failed, self.total
    ));
    out
  }
}

/// Run an expectation-based fixture suite.
///
/// Each fixture must have an expected output (see [`parse_expected_output`]); missing expectations
/// are reported as failures.
///
/// The `runner` closure is injected to keep this module independent of a particular runtime.
/// Today it is typically `run_fixture_ts_with_name` (observe protocol); a future native runner can
/// be wired in with the same API.
pub fn run_expectation_suite(
  cases: &[FixtureCase],
  runner: impl Fn(&FixtureCase) -> Result<String, Diagnostic>,
  options: ExpectationSuiteOptions,
) -> SuiteReport {
  let mut report = SuiteReport {
    total: cases.len(),
    passed: 0,
    failed: 0,
    failures: Vec::new(),
  };

  for case in cases {
    let Some(expected) = case.expected.as_ref() else {
      report.failed += 1;
      report.failures.push(FixtureFailure {
        path: case.path.clone(),
        kind: case.kind,
        failure: FixtureFailureKind::MissingExpected,
      });
      continue;
    };

    let actual = match runner(case) {
      Ok(v) => v,
      Err(diagnostic) => {
        let rendered = render_case_diagnostic(case, &diagnostic);
        report.failed += 1;
        report.failures.push(FixtureFailure {
          path: case.path.clone(),
          kind: case.kind,
          failure: FixtureFailureKind::ExecutionFailed { diagnostic, rendered },
        });
        continue;
      }
    };

    if actual == expected.as_str() {
      report.passed += 1;
    } else {
      report.failed += 1;
      let diff = options
        .include_diff
        .then(|| short_diff(expected, &actual))
        .flatten();
      report.failures.push(FixtureFailure {
        path: case.path.clone(),
        kind: case.kind,
        failure: FixtureFailureKind::Mismatch {
          expected: expected.clone(),
          actual,
          diff,
        },
      });
    }
  }

  report
}

fn render_case_diagnostic(case: &FixtureCase, diagnostic: &Diagnostic) -> String {
  let mut files = SimpleFiles::new();
  // Most harness diagnostics use `FileId(0)`, so ensure the fixture source occupies that slot.
  files.add(case.path.to_string_lossy().into_owned(), case.source.as_str());
  render_diagnostic(&files, diagnostic)
}

fn short_diff(expected: &str, actual: &str) -> Option<String> {
  if expected == actual {
    return None;
  }

  let mut i = 0usize;
  for (e, a) in expected.chars().zip(actual.chars()) {
    if e != a {
      break;
    }
    i += 1;
  }

  let expected_ch = expected.chars().nth(i);
  let actual_ch = actual.chars().nth(i);

  Some(format!(
    "diff: first differing char at index {i}: expected {expected_ch:?}, actual {actual_ch:?}"
  ))
}
