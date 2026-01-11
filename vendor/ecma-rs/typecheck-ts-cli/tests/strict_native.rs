use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use typecheck_ts::codes;

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
    .stdout(contains("TC4000"));
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
    "let x = 1 as string;\nlet y: string | null = null;\nlet z = y!;\n",
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
    stdout.contains("TC4005"),
    "expected TC4005 for unsafe type assertion, got {stdout}"
  );
  assert!(
    stdout.contains("TC4006"),
    "expected TC4006 for non-null assertion, got {stdout}"
  );
}

#[test]
fn strict_native_reports_forbidden_eval() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
declare function eval(code: string): unknown;
eval("1+1");
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(codes::NATIVE_STRICT_EVAL.as_str()));
}

#[test]
fn strict_native_reports_non_constant_computed_key() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(
    &entry,
    r#"
const dict: { [k: string]: number } = { x: 1 };
let key: string = "x";
dict[key];
"#,
  )
  .expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains(
      codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.as_str(),
    ));
}
