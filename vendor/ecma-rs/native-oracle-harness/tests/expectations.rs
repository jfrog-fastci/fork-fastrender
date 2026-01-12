use native_oracle_harness::expectations::{load_expectations, ExpectMode, FixtureExpectation};
use std::fs;
use std::path::Path;

#[test]
fn parses_default_and_fixture_overrides() {
  let dir = tempfile::tempdir().expect("tempdir");
  let path = dir.path().join("expectations.toml");

  fs::write(
    &path,
    r#"
[default]
mode = "pass"

[fixture.alpha]
mode = "xfail-compile"
reason = "not implemented yet"

[fixture.bravo]
mode = "xfail-run"

[fixture.charlie]
mode = "skip"
reason = "flaky on CI"

[fixture.delta]
mode = "pass"
"#,
  )
  .expect("write manifest");

  let manifest = load_expectations(&path);
  let default = manifest.get("default").expect("default entry");
  assert_eq!(default.mode, ExpectMode::Pass);
  assert_eq!(default.reason, None);

  let alpha = manifest.get("alpha").expect("alpha entry");
  assert_eq!(alpha.mode, ExpectMode::XfailCompile);
  assert_eq!(alpha.reason.as_deref(), Some("not implemented yet"));

  let bravo = manifest.get("bravo").expect("bravo entry");
  assert_eq!(bravo.mode, ExpectMode::XfailRun);
  assert_eq!(bravo.reason, None);

  let charlie = manifest.get("charlie").expect("charlie entry");
  assert_eq!(charlie.mode, ExpectMode::Skip);
  assert_eq!(charlie.reason.as_deref(), Some("flaky on CI"));

  let delta = manifest.get("delta").expect("delta entry");
  assert_eq!(delta.mode, ExpectMode::Pass);
}

#[test]
fn defaults_to_pass_when_default_section_missing() {
  let dir = tempfile::tempdir().expect("tempdir");
  let path = dir.path().join("expectations.toml");

  fs::write(
    &path,
    r#"
[fixture.alpha]
mode = "skip"
"#,
  )
  .expect("write manifest");

  let manifest = load_expectations(&path);
  assert!(manifest.get("default").is_none(), "no [default] section");
  assert_eq!(FixtureExpectation::pass().mode, ExpectMode::Pass);
}

#[test]
#[should_panic(expected = "invalid mode")]
fn rejects_unknown_modes() {
  let dir = tempfile::tempdir().expect("tempdir");
  let path = dir.path().join("expectations.toml");

  fs::write(
    &path,
    r#"
[default]
mode = "pass"

[fixture.alpha]
mode = "nope"
"#,
  )
  .expect("write manifest");

  let _ = load_expectations(&path);
}

#[test]
fn loads_repo_native_compare_manifest() {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  let path = manifest_dir
    .parent()
    .expect("native-oracle-harness should live under vendor/ecma-rs/")
    .join("fixtures/native_compare/expectations.toml");

  let manifest = load_expectations(&path);
  let default = manifest.get("default").expect("default entry");
  assert_eq!(default.mode, ExpectMode::Pass);

  let promise_all = manifest.get("promise_all").expect("promise_all entry");
  assert_eq!(promise_all.mode, ExpectMode::XfailCompile);
  assert!(promise_all.reason.is_some());
}
