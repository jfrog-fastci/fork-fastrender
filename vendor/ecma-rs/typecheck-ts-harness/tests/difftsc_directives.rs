use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

mod common;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn harness_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-harness")
}

#[test]
fn difftsc_honors_fixture_directives_for_rust_runs() {
  let (_dir, suite) = common::temp_difftsc_suite(&["win_paths", "es_module_interop.ts"]);
  let baseline = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("baselines")
    .join("difftsc")
    .join("win_paths.json");
  let es_module_baseline = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("baselines")
    .join("difftsc")
    .join("es_module_interop.json");

  let output = harness_cli()
    .timeout(CLI_TIMEOUT)
    .arg("difftsc")
    .arg("--suite")
    .arg(&suite)
    // Avoid oversubscribing CPU/memory when multiple harness integration tests
    // run in parallel under the default Rust test runner settings.
    .arg("--jobs")
    .arg("1")
    .arg("--use-baselines")
    .arg("--compare-rust")
    .arg("--allow-mismatches")
    .arg("--json")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("json output");
  let results = json
    .get("results")
    .and_then(|r| r.as_array())
    .expect("results array");
  let case = results
    .iter()
    .find(|case| case.get("name").and_then(|n| n.as_str()) == Some("win_paths"))
    .expect("win_paths case present");
  assert!(
    case.get("actual_types").is_none() && case.get("type_diff").is_none(),
    "expected difftsc to skip type-fact collection when the baseline has no type_facts; case: {case:?}"
  );

  let baseline_json: Value =
    serde_json::from_str(&std::fs::read_to_string(&baseline).expect("read baseline"))
      .expect("parse baseline json");
  let baseline_opts = baseline_json
    .get("metadata")
    .and_then(|m| m.get("options"))
    .and_then(|o| o.as_object())
    .expect("baseline metadata options");

  let harness = case
    .get("harness_options")
    .and_then(|o| o.as_object())
    .expect("harness_options present");
  assert_eq!(
    harness.get("strict").and_then(|v| v.as_bool()),
    Some(true),
    "expected strict=true in computed harness options; case: {case:?}"
  );

  let tsc = case
    .get("tsc_options")
    .and_then(|o| o.as_object())
    .expect("tsc_options present");
  assert_eq!(
    tsc.get("strict").and_then(|v| v.as_bool()),
    Some(true),
    "expected strict=true in computed tsc options; case: {case:?}"
  );
  assert_eq!(
    tsc.get("noImplicitAny").and_then(|v| v.as_bool()),
    Some(true),
    "expected noImplicitAny=true in computed tsc options; case: {case:?}"
  );

  assert_eq!(
    baseline_opts.get("strict").and_then(|v| v.as_bool()),
    Some(true),
    "expected strict=true in baseline metadata; baseline: {baseline_json:?}"
  );
  assert_eq!(
    baseline_opts.get("noImplicitAny").and_then(|v| v.as_bool()),
    Some(true),
    "expected noImplicitAny=true in baseline metadata; baseline: {baseline_json:?}"
  );

  let es_case = results
    .iter()
    .find(|case| case.get("name").and_then(|n| n.as_str()) == Some("es_module_interop"))
    .expect("es_module_interop case present");
  let es_baseline_json: Value = serde_json::from_str(
    &std::fs::read_to_string(&es_module_baseline).expect("read es_module_interop baseline"),
  )
  .expect("parse es_module_interop baseline json");
  let es_baseline_opts = es_baseline_json
    .get("metadata")
    .and_then(|m| m.get("options"))
    .and_then(|o| o.as_object())
    .expect("es_module_interop baseline metadata options");

  let es_harness = es_case
    .get("harness_options")
    .and_then(|o| o.as_object())
    .expect("es_module_interop harness_options present");
  assert_eq!(
    es_harness
      .get("es_module_interop")
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected es_module_interop=true in computed harness options; case: {es_case:?}"
  );

  let es_tsc = es_case
    .get("tsc_options")
    .and_then(|o| o.as_object())
    .expect("es_module_interop tsc_options present");
  assert_eq!(
    es_tsc.get("esModuleInterop").and_then(|v| v.as_bool()),
    Some(true),
    "expected esModuleInterop=true in computed tsc options; case: {es_case:?}"
  );
  assert_eq!(
    es_baseline_opts
      .get("esModuleInterop")
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected esModuleInterop=true in baseline metadata; baseline: {es_baseline_json:?}"
  );

  let notes = es_case
    .get("notes")
    .and_then(|n| n.as_array())
    .expect("es_module_interop notes present");
  assert!(
    notes.iter().any(|n| {
      n.as_str()
        .is_some_and(|s| s.contains("tsc option esModuleInterop"))
    }),
    "expected note about esModuleInterop being ignored by Rust checker; notes={notes:?}"
  );
}
