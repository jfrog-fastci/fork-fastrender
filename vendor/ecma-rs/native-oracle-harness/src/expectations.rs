//! Native-vs-oracle fixture expectation manifest.
//!
//! The native AOT compiler is still under heavy development; not every fixture will compile or
//! execute correctly yet. To allow incremental rollout (and keep CI signal high), the native-vs-oracle
//! fixture suite supports a small expectation manifest that classifies each fixture into one of four
//! modes:
//!
//! - `pass`: native output must match the oracle output
//! - `xfail-compile`: native compilation is expected to fail (known gap)
//! - `xfail-run`: native compilation is expected to succeed, but runtime mismatch/termination is
//!   expected (known gap)
//! - `skip`: do not run
//!
//! ## Manifest format
//!
//! Path: `vendor/ecma-rs/fixtures/native_compare/expectations.toml`
//!
//! ```toml
//! [default]
//! mode = "pass"
//!
//! [fixture.arithmetic]
//! mode = "pass"
//!
//! [fixture.promise_all]
//! mode = "xfail-compile"
//! reason = "native-js does not support Promises yet"
//! ```
//!
//! - `[default]` applies to any fixture without an explicit `[fixture.<name>]` entry.
//! - `[fixture.<name>]` sections are keyed by the fixture name (usually the filename stem).
//! - `reason` is optional and is printed for expected failures when tests are run with
//!   `--nocapture`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Expected outcome mode for a native-vs-oracle fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectMode {
  Pass,
  XfailCompile,
  XfailRun,
  Skip,
}

impl ExpectMode {
  pub fn as_str(self) -> &'static str {
    match self {
      ExpectMode::Pass => "pass",
      ExpectMode::XfailCompile => "xfail-compile",
      ExpectMode::XfailRun => "xfail-run",
      ExpectMode::Skip => "skip",
    }
  }

  pub fn parse(raw: &str) -> Option<Self> {
    let norm = raw.trim().to_ascii_lowercase().replace('_', "-");
    match norm.as_str() {
      "pass" => Some(Self::Pass),
      "xfail-compile" => Some(Self::XfailCompile),
      "xfail-run" => Some(Self::XfailRun),
      "skip" => Some(Self::Skip),
      _ => None,
    }
  }
}

impl std::fmt::Display for ExpectMode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Expectation for a single fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureExpectation {
  pub mode: ExpectMode,
  pub reason: Option<String>,
}

impl FixtureExpectation {
  pub fn pass() -> Self {
    Self {
      mode: ExpectMode::Pass,
      reason: None,
    }
  }
}

fn parse_expectation_table(path: &Path, section: &str, table: &toml::Table) -> FixtureExpectation {
  let mode_raw = table.get("mode").and_then(|v| v.as_str()).unwrap_or_else(|| {
    panic!(
      "expectations manifest {}: section [{section}] missing required `mode` string",
      path.display()
    )
  });
  let mode = ExpectMode::parse(mode_raw).unwrap_or_else(|| {
    panic!(
      "expectations manifest {}: section [{section}] has invalid mode {mode_raw:?} (expected pass|xfail-compile|xfail-run|skip)",
      path.display()
    )
  });
  let reason = table
    .get("reason")
    .and_then(|v| v.as_str())
    .map(|s| s.to_string());
  FixtureExpectation { mode, reason }
}

/// Load an expectations manifest from `path`.
///
/// The returned map uses these keys:
/// - `"default"`: the `[default]` section (if present)
/// - `"<fixture-name>"`: each `[fixture.<fixture-name>]` section
///
/// Any parse error will panic; this is intended to be used in tests/CI where a malformed manifest
/// should fail loudly.
pub fn load_expectations(path: &Path) -> HashMap<String, FixtureExpectation> {
  let raw = match fs::read_to_string(path) {
    Ok(s) => s,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
    Err(err) => panic!("failed to read expectations manifest {}: {err}", path.display()),
  };

  let root: toml::Value = raw.parse().unwrap_or_else(|err| {
    panic!(
      "failed to parse expectations manifest {} as TOML: {err}",
      path.display()
    )
  });
  let Some(root) = root.as_table() else {
    panic!(
      "expectations manifest {} must contain a TOML table at the top level",
      path.display()
    );
  };

  let mut out = HashMap::new();

  if let Some(default) = root.get("default").and_then(|v| v.as_table()) {
    out.insert(
      "default".to_string(),
      parse_expectation_table(path, "default", default),
    );
  }

  if let Some(fixtures) = root.get("fixture").and_then(|v| v.as_table()) {
    for (name, value) in fixtures {
      let Some(table) = value.as_table() else {
        continue;
      };
      out.insert(
        name.to_string(),
        parse_expectation_table(path, &format!("fixture.{name}"), table),
      );
    }
  }

  out
}

