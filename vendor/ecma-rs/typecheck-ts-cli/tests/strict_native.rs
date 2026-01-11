use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn typecheck_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-cli")
}

#[test]
fn strict_native_reports_explicit_any() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TN0001"));
}

#[test]
fn strict_native_json_includes_compiler_option() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg("--json")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();

  let json: Value = serde_json::from_slice(&output).expect("valid JSON output");
  assert_eq!(
    json
      .get("compiler_options")
      .and_then(|o| o.get("strict_native"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "expected compiler_options.strict_native=true, got {json:?}"
  );
}

#[test]
fn strict_native_reports_type_and_non_null_assertions() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    "let x = 1 as number;\nlet y: string | null = null;\nlet z = y!;\n",
  )
  .expect("write main.ts");

  let output = typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();

  let stdout = String::from_utf8_lossy(&output);
  assert!(
    stdout.contains("TN0002"),
    "expected TN0002 for type assertion, got {stdout}"
  );
  assert!(
    stdout.contains("TN0003"),
    "expected TN0003 for non-null assertion, got {stdout}"
  );
}
