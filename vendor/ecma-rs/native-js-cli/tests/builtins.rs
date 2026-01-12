use assert_cmd::Command;
use predicates::prelude::*;
use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;
use tempfile::tempdir;

const MAX_CONCURRENT_CLI_PROCS: usize = 2;
static CLI_LIMITER: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

// These tests spawn `native-js-cli`, which performs LLVM object emission and system linking.
// Under heavy CI/agent contention this can take tens of seconds per invocation, so keep the
// timeout generous to avoid flaky `<interrupted>` failures.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);

struct CliPermit;

impl CliPermit {
  fn acquire() -> Self {
    let (lock, cv) = CLI_LIMITER.get_or_init(|| (Mutex::new(MAX_CONCURRENT_CLI_PROCS), Condvar::new()));
    let mut available = lock.lock().unwrap();
    while *available == 0 {
      available = cv.wait(available).unwrap();
    }
    *available -= 1;
    CliPermit
  }
}

impl Drop for CliPermit {
  fn drop(&mut self) {
    let (lock, cv) = CLI_LIMITER
      .get()
      .expect("CLI limiter must be initialized before drop");
    let mut available = lock.lock().unwrap();
    *available += 1;
    cv.notify_one();
  }
}

struct LimitedCommand {
  _permit: CliPermit,
  cmd: Command,
}

impl LimitedCommand {
  fn new() -> Self {
    let permit = CliPermit::acquire();
    let cmd = assert_cmd::cargo::cargo_bin_cmd!("native-js-cli");
    Self { _permit: permit, cmd }
  }
}

impl Deref for LimitedCommand {
  type Target = Command;

  fn deref(&self) -> &Self::Target {
    &self.cmd
  }
}

impl DerefMut for LimitedCommand {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.cmd
  }
}

fn native_js_cli() -> LimitedCommand {
  LimitedCommand::new()
}

#[test]
fn console_log_prints_number_expression() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(1 + 2);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("3\n"));
}

#[test]
fn const_binding_can_be_printed() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "const x = 1 + 2;\nconsole.log(x);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("3\n"));
}

#[test]
fn let_binding_without_initializer_defaults_to_undefined() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "let x;\nconsole.log(x);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("undefined\n"));
}

#[test]
fn assignment_updates_variable_value() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "let x = 1;\nx = 2;\nconsole.log(x);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("2\n"));
}

#[test]
fn assignment_addition_updates_number_variable() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "let x = 1;\nx += 2;\nconsole.log(x);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("null undefined NaN Infinity\n"));
}

#[test]
fn console_log_supports_negative_numbers_and_negative_infinity() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "console.log(-1, -Infinity);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success();

  assert.stdout(predicate::eq("-1 -Infinity\n"));
}

#[test]
fn assert_supports_logical_not() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(!false);\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn logical_not_uses_truthiness_for_supported_primitives() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(
    &path,
    "console.log(!0, !1, !\"\", !\"x\", !undefined, !null);\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("true false true false true true\n"));
}

#[test]
fn print_alias_prints_booleans() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "print(true);\nprint(false);\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn assert_accepts_truthy_numbers_and_strings() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(1);\nassert(\"x\");\nconsole.log(\"ok\");\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("ok\n"));
}

#[test]
fn assert_rejects_falsy_numbers_and_strings() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(0, \"zero\");\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("zero"));
}

#[test]
fn assert_supports_numeric_comparisons_and_logical_ops() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(1 < 2 && 2 > 1 && 2 >= 2 && 1 <= 1);\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn if_statement_uses_truthiness() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "if (0) { console.log(\"a\"); } else { console.log(\"b\"); }\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("b\n"));
}

#[test]
fn while_statement_uses_truthiness() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(
    &path,
    "let x = 3;\nwhile (x) { console.log(x); x = x - 1; }\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("3\n2\n1\n"));
}

#[test]
fn logical_and_short_circuits_rhs() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(
    &path,
    "function bump(): number { console.log(\"bump\"); return 1; }\nassert((false && (bump() === 1)) === false);\nconsole.log(\"ok\");\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("ok\n"));
}

#[test]
fn logical_or_short_circuits_rhs() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(
    &path,
    "function bump(): number { console.log(\"bump\"); return 1; }\nassert((true || (bump() === 1)) === true);\nconsole.log(\"ok\");\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq("ok\n"));
}

#[test]
fn assert_supports_strict_inequality_and_string_equality() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(NaN !== NaN);\nassert(\"a\" === \"a\");\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn strict_inequality_between_null_and_undefined_is_true() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(null !== undefined);\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn strict_equality_between_null_and_undefined_is_false() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(null === undefined, \"nope\");\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("nope"));
}

#[test]
fn assert_failure_prints_message_and_exits_non_zero() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(false, \"fail\");\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("fail"));
}

#[test]
fn assert_failure_without_message_prints_default_message() {
  let dir = tempdir().unwrap();
  let path = dir.path().join("main.ts");
  std::fs::write(&path, "assert(false);\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg(&path)
    .assert()
    .failure()
    .stdout(predicate::str::contains("assertion failed"));
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
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
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
    .timeout(CLI_TIMEOUT)
    .arg("--no-builtins")
    .arg(&path)
    .assert()
    .failure()
    .stderr(predicate::str::contains("NJS0012"))
    .stderr(predicate::str::contains("builtins"))
    .stderr(predicate::str::contains("disabled"));
}
