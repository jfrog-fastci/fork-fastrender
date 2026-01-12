use crate::read_utf8_file;
use crate::HarnessError;
use clap::ValueEnum;
use globset::Glob;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExpectationKind {
  Pass,
  Skip,
  Xfail,
  Flaky,
}

impl Default for ExpectationKind {
  fn default() -> Self {
    ExpectationKind::Pass
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Expectation {
  #[serde(default)]
  pub kind: ExpectationKind,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tracking_issue: Option<String>,
}

impl Default for Expectation {
  fn default() -> Self {
    Self {
      kind: ExpectationKind::Pass,
      reason: None,
      tracking_issue: None,
    }
  }
}

#[derive(Debug, Clone, Default)]
pub struct AppliedExpectation {
  pub expectation: Expectation,
  pub from_manifest: bool,
}

impl AppliedExpectation {
  pub fn matches(&self, mismatched: bool) -> bool {
    match self.expectation.kind {
      ExpectationKind::Pass => !mismatched,
      ExpectationKind::Skip => true,
      ExpectationKind::Xfail | ExpectationKind::Flaky => mismatched,
    }
  }

  pub fn covers_mismatch(&self) -> bool {
    matches!(
      self.expectation.kind,
      ExpectationKind::Skip | ExpectationKind::Xfail | ExpectationKind::Flaky
    )
  }

  pub fn is_flaky(&self) -> bool {
    self.expectation.kind == ExpectationKind::Flaky
  }
}

#[derive(Debug, Clone, Default)]
pub struct Expectations {
  exact: Vec<Entry>,
  globs: Vec<Entry>,
  regexes: Vec<Entry>,
}

impl Expectations {
  pub fn empty() -> Self {
    Self::default()
  }

  pub fn from_path(path: &Path) -> Result<Self, HarnessError> {
    let raw = read_utf8_file(path)?;
    Self::from_str(&raw).map_err(|err| match err {
      HarnessError::Manifest(msg) => HarnessError::Manifest(format!("{}: {msg}", path.display())),
      other => other,
    })
  }

  pub fn from_str(raw: &str) -> Result<Self, HarnessError> {
    let manifest = match toml::from_str::<RawManifest>(raw) {
      Ok(manifest) => manifest,
      Err(toml_err) => serde_json::from_str::<RawManifest>(raw).map_err(|json_err| {
        HarnessError::Manifest(format!(
          "failed to parse manifest as TOML ({toml_err}) or JSON ({json_err})"
        ))
      })?,
    };

    Self::from_manifest(manifest)
  }

  pub fn lookup(&self, id: &str) -> AppliedExpectation {
    if let Some(found) = self.lookup_in(&self.exact, id) {
      return found;
    }

    if let Some(found) = self.lookup_in(&self.globs, id) {
      return found;
    }

    if let Some(found) = self.lookup_in(&self.regexes, id) {
      return found;
    }

    AppliedExpectation::default()
  }

  fn lookup_in(&self, entries: &[Entry], id: &str) -> Option<AppliedExpectation> {
    for entry in entries {
      if entry.matches(id) {
        return Some(AppliedExpectation {
          expectation: entry.expectation.clone(),
          from_manifest: true,
        });
      }
    }

    None
  }

  fn from_manifest(manifest: RawManifest) -> Result<Self, HarnessError> {
    let mut expectations = Expectations::default();
    for entry in manifest.expectations {
      let matcher = entry.matcher()?;
      let expectation = Expectation {
        kind: entry
          .status
          .ok_or_else(|| HarnessError::Manifest("manifest entry missing `status`".to_string()))?,
        reason: entry.reason,
        tracking_issue: entry.tracking_issue,
      };

      match matcher {
        Matcher::Exact(pattern) => expectations.exact.push(Entry {
          matcher: Matcher::Exact(pattern),
          expectation,
        }),
        Matcher::Glob(pattern) => expectations.globs.push(Entry {
          matcher: Matcher::Glob(pattern),
          expectation,
        }),
        Matcher::Regex(pattern) => expectations.regexes.push(Entry {
          matcher: Matcher::Regex(pattern),
          expectation,
        }),
      }
    }

    Ok(expectations)
  }
}

#[derive(Debug, Clone)]
struct Entry {
  matcher: Matcher,
  expectation: Expectation,
}

impl Entry {
  fn matches(&self, id: &str) -> bool {
    self.matcher.matches(id)
  }
}

#[derive(Debug, Clone)]
enum Matcher {
  Exact(String),
  Glob(globset::GlobMatcher),
  Regex(Regex),
}

impl Matcher {
  fn matches(&self, id: &str) -> bool {
    match self {
      Matcher::Exact(pattern) => pattern == id,
      Matcher::Glob(glob) => glob.is_match(id),
      Matcher::Regex(re) => re.is_match(id),
    }
  }
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
}

impl RawEntry {
  fn matcher(&self) -> Result<Matcher, HarnessError> {
    let mut seen = 0;
    if self.id.is_some() {
      seen += 1;
    }
    if self.glob.is_some() {
      seen += 1;
    }
    if self.regex.is_some() {
      seen += 1;
    }

    if seen == 0 {
      return Err(HarnessError::Manifest(
        "manifest entry missing `id`/`glob`/`regex`".to_string(),
      ));
    }

    if seen > 1 {
      return Err(HarnessError::Manifest(
        "manifest entry must specify exactly one of `id`/`glob`/`regex`".to_string(),
      ));
    }

    if let Some(id) = &self.id {
      if id.starts_with('/') {
        return Err(HarnessError::Manifest(format!(
          "invalid id '{id}': ids must be relative to the suite root (no leading slash)"
        )));
      }
      if id.is_empty() {
        return Err(HarnessError::Manifest(
          "invalid id: ids must be non-empty".to_string(),
        ));
      }
      if id.contains('\\') {
        return Err(HarnessError::Manifest(format!(
          "invalid id '{id}': ids must use forward slashes"
        )));
      }
      if id.split('/').next().is_some_and(|seg| seg.contains(':')) {
        return Err(HarnessError::Manifest(format!(
          "invalid id '{id}': ids must be relative to the suite root (no drive letter)"
        )));
      }
      if id.split('/').any(|seg| seg.is_empty()) {
        return Err(HarnessError::Manifest(format!(
          "invalid id '{id}': ids must be normalized (no empty path segments)"
        )));
      }
      if id.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(HarnessError::Manifest(format!(
          "invalid id '{id}': ids must be normalized (no '.' or '..' segments)"
        )));
      }
      return Ok(Matcher::Exact(id.clone()));
    }

    if let Some(glob) = &self.glob {
      if glob.starts_with('/') {
        return Err(HarnessError::Manifest(format!(
          "invalid glob '{glob}': globs must be relative to the suite root (no leading slash)"
        )));
      }
      if glob.is_empty() {
        return Err(HarnessError::Manifest(
          "invalid glob: globs must be non-empty".to_string(),
        ));
      }
      if glob.contains('\\') {
        return Err(HarnessError::Manifest(format!(
          "invalid glob '{glob}': globs must use forward slashes"
        )));
      }
      if glob.split('/').next().is_some_and(|seg| seg.contains(':')) {
        return Err(HarnessError::Manifest(format!(
          "invalid glob '{glob}': globs must be relative to the suite root (no drive letter)"
        )));
      }
      if glob.split('/').any(|seg| seg.is_empty()) {
        return Err(HarnessError::Manifest(format!(
          "invalid glob '{glob}': globs must be normalized (no empty path segments)"
        )));
      }
      if glob.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(HarnessError::Manifest(format!(
          "invalid glob '{glob}': globs must be normalized (no '.' or '..' segments)"
        )));
      }
      let compiled = Glob::new(glob)
        .map_err(|err| HarnessError::Manifest(format!("invalid glob '{glob}': {err}")))?
        .compile_matcher();
      return Ok(Matcher::Glob(compiled));
    }

    let regex = self.regex.as_ref().expect("validated regex presence");
    let compiled = Regex::new(regex)
      .map_err(|err| HarnessError::Manifest(format!("invalid regex '{regex}': {err}")))?;

    Ok(Matcher::Regex(compiled))
  }
}

#[derive(Debug, Clone, Copy, Default, ValueEnum, PartialEq, Eq)]
pub enum FailOn {
  /// Non-zero on any mismatch
  All,
  /// Non-zero only for mismatches not covered by manifest (default)
  #[default]
  New,
  /// Always zero
  None,
}

impl FailOn {
  pub fn should_fail(&self, unexpected_mismatches: usize, total_mismatches: usize) -> bool {
    match self {
      FailOn::All => total_mismatches > 0,
      FailOn::New => unexpected_mismatches > 0,
      FailOn::None => false,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::Path;

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
id = "a/b/c.ts"
status = "skip"
    "#;

    let expectations = Expectations::from_str(manifest).expect("manifest parsed");
    let applied = expectations.lookup("a/b/c.ts");
    assert_eq!(applied.expectation.kind, ExpectationKind::Skip);
  }

  #[test]
  fn manifest_uses_first_match_within_priority() {
    let manifest = r#"
[[expectations]]
glob = "cases/**"
status = "xfail"
reason = "first"

[[expectations]]
glob = "cases/**"
status = "flaky"
reason = "second"
    "#;

    let expectations = Expectations::from_str(manifest).expect("manifest parsed");
    let applied = expectations.lookup("cases/sample.ts");
    assert_eq!(applied.expectation.kind, ExpectationKind::Xfail);
    assert_eq!(applied.expectation.reason.as_deref(), Some("first"));
  }

  #[test]
  fn manifest_loads_from_fixture_path() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance_manifest.toml");
    let expectations = Expectations::from_path(&path).expect("manifest parsed from file");

    let xfail = expectations.lookup("err/parse_error.ts");
    assert_eq!(xfail.expectation.kind, ExpectationKind::Xfail);

    let flaky = expectations.lookup("multi/sample.ts");
    assert_eq!(flaky.expectation.kind, ExpectationKind::Flaky);
  }

  #[test]
  fn manifest_loads_upstream_conformance_manifest() {
    let path =
      Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance-upstream/manifest.toml");
    Expectations::from_path(&path).expect("upstream conformance manifest parsed from file");
  }

  #[test]
  fn manifest_rejects_backslashes_in_id_and_glob() {
    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "a\\b.ts"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("forward slashes"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "a\\**"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("forward slashes"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn manifest_rejects_non_relative_id_and_glob() {
    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "/a/b.ts"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("relative"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "/**"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("relative"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "C:/a/b.ts"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("drive letter"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "C:/**"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("drive letter"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn manifest_rejects_non_normalized_id_and_glob() {
    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "a//b.ts"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("empty path segments"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "a//**"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("empty path segments"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "a/"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("empty path segments"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "a/"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("empty path segments"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
id = ""
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("non-empty"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = ""
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("non-empty"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
id = "../a.ts"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("normalized"),
      "unexpected error: {err}"
    );

    let err = Expectations::from_str(
      r#"
[[expectations]]
glob = "./**"
status = "xfail"
"#,
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("normalized"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn upstream_manifest_includes_metadata_for_skips_and_glob_xfails() {
    let path =
      Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/conformance-upstream/manifest.toml");
    let raw = std::fs::read_to_string(&path).expect("read upstream manifest");
    let value: toml::Value = toml::from_str(&raw).expect("manifest parses as TOML");
    let entries = value
      .get("expectations")
      .and_then(|v| v.as_array())
      .expect("manifest has expectations array");

    for entry in entries {
      let table = entry.as_table().expect("expectation is a table");
      let status = table
        .get("status")
        .and_then(|v| v.as_str())
        .expect("expectation has status");
      let is_skip = status == "skip";
      let is_glob_xfail = status == "xfail" && table.contains_key("glob");
      if !(is_skip || is_glob_xfail) {
        continue;
      }
      assert!(
        table
          .get("reason")
          .and_then(|v| v.as_str())
          .is_some_and(|s| !s.trim().is_empty()),
        "upstream manifest entries with status={status} must include reason: {table:?}"
      );
      assert!(
        table
          .get("tracking_issue")
          .and_then(|v| v.as_str())
          .is_some_and(|s| !s.trim().is_empty()),
        "upstream manifest entries with status={status} must include tracking_issue: {table:?}"
      );
    }
  }
}
