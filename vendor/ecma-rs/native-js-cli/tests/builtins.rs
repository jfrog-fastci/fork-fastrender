use assert_cmd::Command;
use predicates::prelude::*;
use std::time::Duration;
use tempfile::tempdir;

fn native_js_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js-cli")
}

#[test]
fn console_log_prints_number_expression() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(1 + 2);\n").unwrap();

  let assert = native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("3\n"));
}

#[test]
fn console_log_supports_multiple_args() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(1, true, \"x\");\n").unwrap();

  let assert = native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("1 true x\n"));
}

#[test]
fn console_log_prints_null_undefined_nan_and_infinity() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(null, undefined, NaN, Infinity);\n").unwrap();

  let assert = native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("null undefined NaN Infinity\n"));
}

#[test]
fn print_alias_prints_booleans() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "print(true);\nprint(false);\n").unwrap();

  let assert = native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("true\nfalse\n"));
}

#[test]
fn assert_passes() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(1 + 1 === 2);\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn assert_failure_prints_message_and_exits_non_zero() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(false, \"fail\");\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("fail"));
}

#[test]
fn numeric_literal_precision_is_preserved_for_strict_equality() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");

  // If numeric literals are rounded when lowered into LLVM IR, these two distinct values can end
  // up equal and make this assert incorrectly pass.
  std::fs::write(
    &path,
    "assert(1.23456789 === 1.2345678, \"precision lost\");\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("precision lost"));
}

#[test]
fn panic_builtin_exits_non_zero() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "panic(\"boom\");\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("boom"));
}

#[test]
fn trap_builtin_exits_non_zero() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "trap();\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg(&path)
    .assert()
    .failure();
}

#[test]
fn no_builtins_flag_disables_builtin_recognition() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(1);\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(30))
    .arg("--no-builtins")
    .arg(&path)
    .assert()
    .failure()
    .stderr(predicate::str::contains("builtins disabled"));
}
