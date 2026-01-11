use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::process::Command as StdCommand;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use wait_timeout::ChildExt;

// These tests spawn `native-js` / `native-js-cli` binaries which perform LLVM codegen and system
// linking. Under heavy CI/agent contention this can take tens of seconds, so keep the timeout
// generous to avoid flaky `<interrupted>` failures.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);

const MAX_CONCURRENT_NATIVE_JS_TESTS: usize = 4;
static NATIVE_JS_TESTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct CodegenPermit;

impl CodegenPermit {
  fn acquire() -> Self {
    loop {
      let current = NATIVE_JS_TESTS_IN_FLIGHT.load(Ordering::Acquire);
      if current < MAX_CONCURRENT_NATIVE_JS_TESTS {
        if NATIVE_JS_TESTS_IN_FLIGHT
          .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          return Self;
        }
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }
}

impl Drop for CodegenPermit {
  fn drop(&mut self) {
    NATIVE_JS_TESTS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
  }
}

struct PermitCommand {
  _permit: CodegenPermit,
  inner: Command,
}

impl Deref for PermitCommand {
  type Target = Command;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl DerefMut for PermitCommand {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

fn native_js() -> PermitCommand {
  PermitCommand {
    _permit: CodegenPermit::acquire(),
    inner: assert_cmd::cargo::cargo_bin_cmd!("native-js"),
  }
}

fn native_js_cli() -> PermitCommand {
  PermitCommand {
    _permit: CodegenPermit::acquire(),
    inner: assert_cmd::cargo::cargo_bin_cmd!("native-js-cli"),
  }
}

fn run_with_timeout(cmd: &mut StdCommand, timeout: Duration) -> std::io::Result<ExitStatus> {
  let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
  match child.wait_timeout(timeout)? {
    Some(status) => Ok(status),
    None => {
      let _ = child.kill();
      child.wait()
    }
  }
}

#[test]
fn check_succeeds_on_simple_program() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("check")
    .arg(&entry)
    .assert()
    .success();
}

#[test]
fn check_fails_on_any() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "const x: any = 1;\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0010"))
    .stderr(predicates::str::contains("`any` is not supported"));
}

#[test]
fn json_check_success_contains_schema_version_and_diagnostics_array() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("check")
    .arg(&entry)
    .assert()
    .success()
    .code(0);

  assert!(
    assert.get_output().stderr.is_empty(),
    "expected stderr to be empty, got: {}",
    String::from_utf8_lossy(&assert.get_output().stderr)
  );

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  let value: Value = serde_json::from_str(&stdout).expect("stdout to be valid JSON");
  assert_eq!(value["schema_version"], 1);
  assert_eq!(value["diagnostics"].as_array().unwrap().len(), 0);
}

#[test]
fn json_check_error_contains_diagnostics_array() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "const x: any = 1;\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1);

  assert!(
    assert.get_output().stderr.is_empty(),
    "expected stderr to be empty, got: {}",
    String::from_utf8_lossy(&assert.get_output().stderr)
  );

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  let value: Value = serde_json::from_str(&stdout).expect("stdout to be valid JSON");
  assert_eq!(value["schema_version"], 1);

  let diagnostics = value
    .get("diagnostics")
    .and_then(|value| value.as_array())
    .expect("expected diagnostics array");
  assert!(
    !diagnostics.is_empty(),
    "expected diagnostics to be non-empty, got: {diagnostics:?}"
  );
}

#[test]
fn json_check_missing_entry_emits_host_error_diagnostic() {
  let tmp = TempDir::new().unwrap();
  let missing = tmp.path().join("missing.ts");

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("check")
    .arg(&missing)
    .assert()
    .failure()
    .code(2);

  assert!(
    assert.get_output().stderr.is_empty(),
    "expected stderr to be empty, got: {}",
    String::from_utf8_lossy(&assert.get_output().stderr)
  );

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  let value: Value = serde_json::from_str(&stdout).expect("stdout to be valid JSON");
  assert_eq!(value["schema_version"], 1);

  let diagnostics = value
    .get("diagnostics")
    .and_then(|value| value.as_array())
    .expect("expected diagnostics array");
  assert_eq!(diagnostics.len(), 1);
  assert_eq!(diagnostics[0]["code"], "HOST0001");
}

#[test]
fn run_rejects_json_flag() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(2)
    .stderr(predicates::str::contains("--json is not supported"));
}

#[test]
fn build_and_run_returns_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 42; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(42), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn build_with_emit_ir_writes_executable_and_ir_file() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 7; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  let ll = tmp.path().join("out.ll");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .arg("--emit-ir")
    .arg(&ll)
    .assert()
    .success();

  assert!(out.is_file(), "expected executable at {}", out.display());
  assert!(ll.is_file(), "expected LLVM IR at {}", ll.display());

  let text = fs::read_to_string(&ll).unwrap();
  assert!(
    text.contains("define i32 @main"),
    "expected IR to define a `main` function"
  );
  assert!(
    text.contains("@__nativejs_def_"),
    "expected IR to contain native-js definition symbols"
  );
  assert!(
    text.contains("__nativejs_file_init_"),
    "expected IR to contain module init symbols"
  );

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(7), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn emit_llvm_ir_contains_symbols() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let ll = tmp.path().join("out.ll");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("emit-ir")
    .arg(&entry)
    .arg("-o")
    .arg(&ll)
    .assert()
    .success();

  let text = fs::read_to_string(&ll).unwrap();
  assert!(
    text.contains("define i32 @main"),
    "expected IR to define a `main` function"
  );
  assert!(
    text.contains("@__nativejs_def_"),
    "expected IR to contain native-js definition symbols"
  );
  assert!(
    text.contains("define"),
    "expected IR to contain function definitions"
  );
}

#[test]
fn run_exits_with_program_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 42; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .arg("--")
    .arg("--dummy-arg")
    .assert()
    .failure()
    .code(42)
    .stdout(predicate::eq(""));
}

#[test]
fn relative_imports_are_resolved() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "export const unused: number = 0;\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import \"./dep\";\nexport function main(): number { return 42; }\n",
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(42), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn tsconfig_paths_are_resolved() {
  let tmp = TempDir::new().unwrap();

  let lib_dir = tmp.path().join("src").join("lib");
  fs::create_dir_all(&lib_dir).unwrap();
  let dep = lib_dir.join("dep.ts");
  fs::write(&dep, "export const unused: number = 0;\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import { unused } from \"@lib/dep\";\nexport function main(): number { return 42; }\n",
  )
  .unwrap();

  let tsconfig = tmp.path().join("tsconfig.json");
  fs::write(
    &tsconfig,
    r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@lib/*": ["src/lib/*"]
    }
  },
  "files": ["entry.ts"]
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--project")
    .arg(&tsconfig)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(42), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn ts_runtime_inert_wrappers_succeed_in_check_and_build() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { return ((1 satisfies number) as number)!; }\n",
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("check")
    .arg(&entry)
    .assert()
    .success();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(1), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn check_and_build_reject_eval() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { eval(\"1\"); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0009"))
    .stderr(predicates::str::contains("`eval()` is not supported"));

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0009"))
    .stderr(predicates::str::contains("`eval()` is not supported"));
}

#[test]
fn check_and_build_reject_string_literal() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { const s = \"hi\"; return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0009"))
    .stderr(predicates::str::contains("string literals are not supported"));

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0009"))
    .stderr(predicates::str::contains("string literals are not supported"))
    // Ensure we don't fall through to the opaque backend errors (`NJS01xx`) at build time.
    .stderr(predicates::str::contains("NJS010").not());
}

#[test]
fn tsconfig_types_are_loaded_from_type_roots() {
  let tmp = TempDir::new().unwrap();

  let types_dir = tmp.path().join("types").join("mypkg");
  fs::create_dir_all(&types_dir).unwrap();
  fs::write(
    types_dir.join("index.d.ts"),
    "declare interface FromTypes { x: number }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "type T = FromTypes;\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  let tsconfig = tmp.path().join("tsconfig.json");
  fs::write(
    &tsconfig,
    r#"{
  "compilerOptions": {
    "typeRoots": ["./types"],
    "types": ["mypkg"]
  },
  "files": ["entry.ts"]
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--project")
    .arg(&tsconfig)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(0), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn print_builtin_writes_stdout() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { print(1 + 2); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("3\n"));
}

#[test]
fn checked_pipeline_run_prints_stdout() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { print(1 + 2); return 0; }\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("3\n"));
}

#[test]
fn checked_pipeline_build_with_emit_llvm_writes_executable_and_ir_file() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { print(1 + 2); return 7; }\n",
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  let ll = tmp.path().join("out.ll");

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .arg("--emit-llvm")
    .arg(&ll)
    .assert()
    .success();

  assert!(out.is_file(), "expected executable at {}", out.display());
  assert!(ll.is_file(), "expected LLVM IR at {}", ll.display());

  let text = fs::read_to_string(&ll).unwrap();
  assert!(
    text.contains("define i32 @main"),
    "expected IR to define a `main` function"
  );
  assert!(
    text.contains("@__nativejs_def_"),
    "expected IR to contain native-js definition symbols"
  );
  assert!(
    text.contains("gc \"coreclr\""),
    "expected IR to use native-js GC strategy"
  );

  // This program prints to stdout; silence it so test output stays clean even
  // when the harness captures and replays child stdout/stderr.
  let mut cmd = StdCommand::new(&out);
  cmd.stdout(Stdio::null()).stderr(Stdio::null());
  let status = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(7));
}

#[test]
fn checked_pipeline_rejects_entry_fn_flag() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("--entry-fn")
    .arg("main")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(2)
    .stderr(predicates::str::contains(
      "--entry-fn is not supported with --pipeline checked",
    ));
}

#[test]
fn checked_pipeline_no_builtins_check_succeeds_without_print() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("--no-builtins")
    .arg("check")
    .arg(&entry)
    .assert()
    .success();
}

#[test]
fn checked_pipeline_no_builtins_rejects_print() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { print(1 + 2); return 0; }\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("--no-builtins")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stderr(predicates::str::contains("NJS0012"));
}

#[test]
fn checked_pipeline_check_succeeds_on_simple_program() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("check")
    .arg(&entry)
    .assert()
    .success();
}

#[test]
fn checked_pipeline_check_fails_on_type_error() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { return \"nope\"; }\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stderr(predicates::str::contains("TS2322"));
}

#[test]
fn checked_pipeline_run_exits_with_program_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { print(1 + 2); return 42; }\n",
  )
  .unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(42)
    .stdout(predicate::eq("3\n"));
}

#[test]
fn checked_pipeline_emit_ir_contains_symbols() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let ll = tmp.path().join("out.ll");
  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("emit-ir")
    .arg(&entry)
    .arg("-o")
    .arg(&ll)
    .assert()
    .success();

  let text = fs::read_to_string(&ll).unwrap();
  assert!(
    text.contains("define i32 @main"),
    "expected IR to define a `main` function"
  );
  assert!(
    text.contains("@__nativejs_def_"),
    "expected IR to contain native-js definition symbols"
  );
  assert!(
    text.contains("gc \"coreclr\""),
    "expected IR to use native-js GC strategy"
  );
}

#[test]
fn imported_module_init_runs() {
  let tmp = TempDir::new().unwrap();
  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"export let x: number = 40;
x += 2;
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { x } from "./dep";
export function main(): number { return x; }
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(42), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn side_effect_only_import_runs() {
  let tmp = TempDir::new().unwrap();
  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(42);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import "./dep";
import "./dep";
export function main(): number { return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn init_order_dependency_first() {
  let tmp = TempDir::new().unwrap();

  let c = tmp.path().join("c.ts");
  fs::write(
    &c,
    r#"export let x: number = 0;
x = 1;
"#,
  )
  .unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(
    &b,
    r#"import { x as cx } from "./c";
export let x: number = cx + 1;
"#,
  )
  .unwrap();

  let a = tmp.path().join("a.ts");
  fs::write(
    &a,
    r#"import { x } from "./b";
export function main(): number { return x; }
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&a)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(2), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn type_only_import_does_not_execute_module() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"export type T = number;
print(42);
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { type T } from "./dep";
export function main(): number { return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn reexported_module_init_runs() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"export let x: number = 40;
x += 2;
"#,
  )
  .unwrap();

  let reexport = tmp.path().join("reexport.ts");
  fs::write(&reexport, r#"export { x } from "./dep";"#).unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { x } from "./reexport";
 export function main(): number { return x; }
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(output.status.code(), Some(42), "unexpected status {:?}", output.status);
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn export_all_reexport_initializes_dependency() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"export let x: number = 40;
x += 2;
"#,
  )
  .unwrap();

  let reexport = tmp.path().join("reexport.ts");
  fs::write(&reexport, r#"export * from "./dep";"#).unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { x } from "./reexport";
export function main(): number { return x; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(42)
    .stdout(predicate::eq(""));
}

#[test]
fn type_only_reexport_does_not_execute_module() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"export type T = number;
print(42);
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export { type T } from "./dep";
 export function main(): number { return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn module_init_runs_once_and_in_import_order() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(0);\n").unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "import \"./dep\";\nprint(1);\n").unwrap();

  let c = tmp.path().join("c.ts");
  fs::write(&c, "import \"./dep\";\nprint(2);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import "./b";
import "./c";
export function main(): number { return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("0\n1\n2\n"));
}

#[test]
fn locals_and_while_loop_sum_0_to_9() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"
export function main(): number {
  let sum = 0;
  let i = 0;
  while (i < 10) {
    sum += i;
    i += 1;
  }
  return sum;
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(45));
}

#[test]
fn if_statement_controls_return_value() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"
export function main(): number {
  if (1 < 2) {
    return 1;
  } else {
    return 0;
  }
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(1));
}

#[test]
fn direct_function_call_add() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"
function add(a: number, b: number): number {
  return a + b;
}

export function main(): number {
  return add(1, 2);
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(3));
}

#[test]
fn recursion_fib_10() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"
function fib(n: number): number {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}

export function main(): number {
  return fib(10);
}
"#,
  )
  .unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(55));
}
