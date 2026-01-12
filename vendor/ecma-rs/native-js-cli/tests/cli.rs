use assert_cmd::Command;
use diagnostics::paths::normalize_fs_path;
use object::{Object, ObjectSymbol};
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::process::{Command as StdCommand, Output, Stdio};
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

fn run_with_timeout(cmd: &mut StdCommand, timeout: Duration) -> std::io::Result<Output> {
  let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

  let status = match child.wait_timeout(timeout)? {
    Some(status) => status,
    None => {
      let _ = child.kill();
      child.wait()?
    }
  };

  // These tiny test programs produce minimal output, so reading after `wait_timeout` is fine.
  let mut stdout = Vec::new();
  let mut stderr = Vec::new();
  if let Some(mut out) = child.stdout.take() {
    out.read_to_end(&mut stdout)?;
  }
  if let Some(mut err) = child.stderr.take() {
    err.read_to_end(&mut stderr)?;
  }

  Ok(Output {
    status,
    stdout,
    stderr,
  })
}

fn json_files_map(value: &Value) -> HashMap<u32, String> {
  let files = value
    .get("files")
    .and_then(|value| value.as_array())
    .expect("expected files array");
  files
    .iter()
    .map(|entry| {
      let id = entry
        .get("id")
        .and_then(|value| value.as_u64())
        .expect("expected file id") as u32;
      let path = entry
        .get("path")
        .and_then(|value| value.as_str())
        .expect("expected file path")
        .to_string();
      (id, path)
    })
    .collect()
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

  let files = json_files_map(&value);
  let expected_entry = normalize_fs_path(&entry);
  assert!(
    files.values().any(|path| path == &expected_entry),
    "expected JSON files to include entry path {expected_entry:?}, got: {files:?}"
  );
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

  let files = json_files_map(&value);
  let first_file_id = diagnostics[0]["primary"]["file"]
    .as_u64()
    .expect("expected diagnostic.primary.file") as u32;
  assert!(
    files.contains_key(&first_file_id),
    "expected files mapping to include diagnostic file id {first_file_id}, got: {files:?}"
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

  let files = json_files_map(&value);
  let file_id = diagnostics[0]["primary"]["file"]
    .as_u64()
    .expect("expected diagnostic.primary.file") as u32;
  assert!(
    files.contains_key(&file_id),
    "expected files mapping to include diagnostic file id {file_id}, got: {files:?}"
  );
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
fn json_bench_success_contains_schema_version_command_and_timings() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("bench")
    .arg(&entry)
    .arg("--warmup")
    .arg("0")
    .arg("--iters")
    .arg("2")
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
  assert_eq!(value["command"], "bench");
  assert!(value["compile_time_ms"].as_f64().is_some());
  assert!(value["run_times_ms"].as_array().is_some());
  assert_eq!(value["run_times_ms"].as_array().unwrap().len(), 2);
  assert!(value["mean_ms"].as_f64().is_some());
  assert!(value["median_ms"].as_f64().is_some());
  assert!(value["min_ms"].as_f64().is_some());
  assert!(value["max_ms"].as_f64().is_some());
}

#[test]
fn json_bench_invalid_target_still_uses_bench_schema() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("--target")
    .arg("not-a-triple")
    .arg("bench")
    .arg(&entry)
    .arg("--warmup")
    .arg("0")
    .arg("--iters")
    .arg("1")
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
  assert_eq!(value["command"], "bench");
  assert!(value["error"].as_str().is_some());
  assert!(
    value["diagnostics"].as_array().is_some_and(|arr| !arr.is_empty()),
    "expected non-empty diagnostics, got: {}",
    value["diagnostics"]
  );
}

#[test]
fn bench_text_output_contains_summary_lines() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("bench")
    .arg(&entry)
    .arg("--warmup")
    .arg("0")
    .arg("--iters")
    .arg("2")
    .assert()
    .success()
    .code(0)
    .stdout(predicates::str::contains("compile_time_ms:"))
    .stdout(predicates::str::contains("run_times_ms:"))
    .stdout(predicates::str::contains("mean_ms:"))
    .stdout(predicates::str::contains("median_ms:"))
    .stdout(predicates::str::contains("min_ms:"))
    .stdout(predicates::str::contains("max_ms:"));
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn build_with_target_triple_succeeds() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--target")
    .arg("x86_64-unknown-linux-gnu")
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
fn invalid_target_triple_is_rejected_by_cli() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--target")
    .arg("definitely-not-a-triple")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(2)
    .stderr(predicate::str::contains("invalid target triple"));
}

#[test]
fn release_build_succeeds() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--release")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let output = StdCommand::new(&out).output().unwrap();
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected status {:?}",
    output.status
  );
}

#[test]
fn debug_build_succeeds_and_keeps_intermediates() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--debug")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  assert!(out.is_file(), "expected executable at {}", out.display());
  assert!(
    out.with_extension("o").is_file(),
    "expected object file at {}",
    out.with_extension("o").display()
  );
  assert!(
    out.with_extension("ll").is_file(),
    "expected LLVM IR at {}",
    out.with_extension("ll").display()
  );
}

#[test]
fn addr2line_resolves_main_symbol_to_typescript_location() {
  fn tool_available(name: &str) -> bool {
    StdCommand::new(name)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|status| status.success())
  }

  let has_clang = tool_available("clang-18") || tool_available("clang");
  let has_lld = tool_available("ld.lld-18")
    || tool_available("ld.lld")
    || tool_available("ld64.lld")
    || tool_available("lld");
  if !has_clang || !has_lld {
    eprintln!("skipping addr2line test: clang/lld not available");
    return;
  }

  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--debug")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let data = fs::read(&out).unwrap();
  let obj = object::File::parse(&*data).unwrap();
  let main_addr = obj
    .symbols()
    .chain(obj.dynamic_symbols())
    .find_map(|sym| {
      let name = sym.name().ok()?;
      if name != "main" && name != "_main" {
        return None;
      }
      let addr = sym.address();
      (addr != 0).then_some(addr)
    })
    .expect("expected output to contain a `main` symbol");

  // Use a hex address without 0x to verify parsing.
  let addr_arg = format!("{main_addr:x}");

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("addr2line")
    .arg(&out)
    .arg(addr_arg)
    .assert()
    .success()
    .stdout(predicate::str::contains("entry.ts:1"))
    .stdout(predicate::str::contains("main"));

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("addr2line")
    .arg("--stdin")
    .arg(&out)
    .write_stdin(format!("#0  0x{main_addr:x} in main\n"))
    .assert()
    .success()
    .stdout(predicate::str::contains("entry.ts:1"))
    .stdout(predicate::str::contains("main"));

  // `--base` is intended for PIE/ASLR runtime addresses. We can validate the arithmetic by adding a
  // synthetic base offset and ensuring it resolves to the same location.
  let base = 0x1000u64;
  let runtime_addr = main_addr + base;
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("addr2line")
    .arg(&out)
    .arg("--base")
    .arg(format!("0x{base:x}"))
    .arg(format!("0x{runtime_addr:x}"))
    .assert()
    .success()
    .stdout(predicate::str::contains("entry.ts:1"))
    .stdout(predicate::str::contains("main"));

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("addr2line")
    .arg("--strict")
    .arg(&out)
    .arg("0")
    .assert()
    .failure()
    .code(1)
    .stdout(predicate::str::contains("??:0"));

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("addr2line")
    .arg("--stdin")
    .arg(&out)
    .write_stdin(format!("0x{main_addr:x}\n"))
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
  assert_eq!(value["command"], "addr2line");
  assert_eq!(value["exit_code"], 0);
  assert_eq!(value["stdin"], true);

  let results = value["results"].as_array().expect("expected results array");
  assert_eq!(results.len(), 1);
  let file = results[0]["file"].as_str().unwrap_or("");
  assert!(
    file.ends_with("entry.ts"),
    "expected file to end with entry.ts, got {file:?}"
  );
  assert_eq!(results[0]["line"], 1);
  let function = results[0]["function"].as_str().unwrap_or("");
  let symbol = results[0]["symbol"].as_str().unwrap_or("");
  assert!(
    function.contains("main") || symbol.contains("main"),
    "expected function/symbol to contain main, got function={function:?} symbol={symbol:?}"
  );

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("addr2line")
    .arg(&out)
    .arg("--base")
    .arg(format!("0x{base:x}"))
    .arg(format!("0x{runtime_addr:x}"))
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
  assert_eq!(value["command"], "addr2line");
  assert_eq!(value["base"], format!("0x{base:x}"));
  let results = value["results"].as_array().expect("expected results array");
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["addr"], format!("0x{runtime_addr:x}"));
  assert_eq!(results[0]["probe"], format!("0x{main_addr:x}"));

  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--json")
    .arg("addr2line")
    .arg("--strict")
    .arg(&out)
    .arg("0")
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
  assert_eq!(value["command"], "addr2line");
  assert_eq!(value["exit_code"], 1);
  let results = value["results"].as_array().expect("expected results array");
  assert_eq!(results.len(), 1);
}

#[test]
fn build_verbose_prints_clang_invocation() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--verbose")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success()
    .stderr(predicates::str::contains("clang"));
}

#[test]
fn build_keep_temp_preserves_tempdirs() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  let assert = native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--keep-temp")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
  let kept_line = stderr
    .lines()
    .find(|line| line.contains("kept tempdir:"))
    .expect("expected --keep-temp to print at least one `kept tempdir:` line");
  let kept_path = kept_line
    .splitn(2, "kept tempdir:")
    .nth(1)
    .expect("kept tempdir line missing separator")
    .trim();
  assert!(
    !kept_path.is_empty(),
    "expected non-empty kept tempdir path in stderr, got: {stderr}"
  );
  assert!(
    std::path::Path::new(kept_path).is_dir(),
    "expected kept tempdir to exist after exit: {kept_path}\nstderr:\n{stderr}"
  );
}

#[test]
fn build_with_invalid_clang_path_fails_with_exit_code_2() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("--clang")
    .arg("/nonexistent/native-js-clang")
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .code(2)
    .stderr(predicates::str::contains("clang"))
    .stderr(predicates::str::contains("/nonexistent/native-js-clang"));
}

#[test]
fn build_and_run_returns_boolean_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): boolean { return true; }\n").unwrap();

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
  assert_eq!(
    output.status.code(),
    Some(1),
    "unexpected status {:?}",
    output.status
  );
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
  assert_eq!(
    output.status.code(),
    Some(7),
    "unexpected status {:?}",
    output.status
  );
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
fn emit_hir_writes_dump_file_containing_main() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out_dir = tmp.path().join("emit-out");
  fs::create_dir_all(&out_dir).unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("emit")
    .arg(&entry)
    .arg("--emit")
    .arg("hir")
    .arg("--out-dir")
    .arg(&out_dir)
    .assert()
    .success();

  let hir = out_dir.join("out.hir.txt");
  assert!(hir.is_file(), "expected HIR dump at {}", hir.display());
  let text = fs::read_to_string(&hir).unwrap();
  assert!(
    text.contains("main"),
    "expected HIR dump to contain `main`, got: {text}"
  );
}

#[test]
fn build_with_emit_llvm_and_asm_writes_both_files() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out_dir = tmp.path().join("emit-out");
  fs::create_dir_all(&out_dir).unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&entry)
    .arg("--emit")
    .arg("llvm")
    .arg("--emit")
    .arg("asm")
    .arg("--out-dir")
    .arg(&out_dir)
    .assert()
    .success();

  let ll = out_dir.join("out.ll");
  let asm = out_dir.join("out.s");
  assert!(ll.is_file(), "expected LLVM IR at {}", ll.display());
  assert!(asm.is_file(), "expected assembly at {}", asm.display());

  let ll_text = fs::read_to_string(&ll).unwrap();
  assert!(
    ll_text.contains("define i32 @main"),
    "expected emitted IR to define a `main` function"
  );

  let asm_meta = fs::metadata(&asm).unwrap();
  assert!(asm_meta.len() > 0, "expected assembly output to be non-empty");
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
fn run_exits_with_boolean_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): boolean { return true; }\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stdout(predicate::eq(""));
}

#[test]
fn run_void_entrypoint_exits_zero_without_stdout() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): void {}\n").unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .code(0)
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
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
  assert_eq!(
    output.status.code(),
    Some(1),
    "unexpected status {:?}",
    output.status
  );
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
    .stderr(predicates::str::contains(
      "string literals are not supported",
    ));

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
    .stderr(predicates::str::contains(
      "string literals are not supported",
    ))
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
  assert_eq!(
    output.status.code(),
    Some(0),
    "unexpected status {:?}",
    output.status
  );
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
fn checked_pipeline_supports_import_and_call_across_modules() {
  let tmp = TempDir::new().unwrap();
  let math = tmp.path().join("math.ts");
  fs::write(
    &math,
    "export function add(a:number,b:number): number { return a+b; }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import {add} from \"./math\";\nexport function main(): number { print(add(20, 22)); return 0; }\n",
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
    .stdout(predicate::eq("42\n"));
}

#[test]
fn checked_pipeline_resolves_tsconfig_paths_with_project_flag() {
  let tmp = TempDir::new().unwrap();

  let lib_dir = tmp.path().join("src").join("lib");
  fs::create_dir_all(&lib_dir).unwrap();
  let dep = lib_dir.join("math.ts");
  fs::write(
    &dep,
    "export function add(a:number,b:number): number { return a+b; }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import {add} from \"@lib/math\";\nexport function main(): number { print(add(20, 22)); return 0; }\n",
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

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("--project")
    .arg(&tsconfig)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn checked_pipeline_loads_type_roots_and_types_with_project_flag() {
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

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("--project")
    .arg(&tsconfig)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq(""));
}

#[test]
fn project_pipeline_resolves_tsconfig_paths_with_project_flag() {
  let tmp = TempDir::new().unwrap();

  let lib_dir = tmp.path().join("src").join("lib");
  fs::create_dir_all(&lib_dir).unwrap();
  let dep = lib_dir.join("math.ts");
  fs::write(
    &dep,
    "export function add(a:number,b:number): number { return a+b; }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import {add} from \"@lib/math\";\nprint(add(20, 22));\n",
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

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("project")
    .arg("--project")
    .arg(&tsconfig)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("42\n"));
}

#[test]
fn checked_pipeline_runs_module_initializers_in_import_order_with_transitive_deps() {
  let tmp = TempDir::new().unwrap();
  let a = tmp.path().join("a.ts");
  let b = tmp.path().join("b.ts");
  let c = tmp.path().join("c.ts");
  let entry = tmp.path().join("entry.ts");

  fs::write(&c, "print(1);\n").unwrap();
  fs::write(&b, "import \"./c\";\nprint(2);\n").unwrap();
  fs::write(&a, "print(3);\n").unwrap();
  fs::write(
    &entry,
    "import \"./b\";\nimport \"./a\";\nprint(99);\nexport function main(): number { print(4); return 0; }\n",
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
    .stdout(predicate::eq("1\n2\n3\n99\n4\n"));
}

#[test]
fn checked_pipeline_supports_importing_from_reexport_modules() {
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

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(42)
    .stdout(predicate::eq(""));
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

  let output = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(output.status.code(), Some(7));
  assert_eq!(String::from_utf8_lossy(&output.stdout), "3\n");
  assert!(
    output.stderr.is_empty(),
    "expected stderr to be empty, got: {}",
    String::from_utf8_lossy(&output.stderr)
  );
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
fn checked_pipeline_json_check_error_contains_diagnostics_array() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { return \"nope\"; }\n",
  )
  .unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
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
fn project_pipeline_json_parse_error_contains_diagnostics_array() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");

  // Syntax error: missing closing braces.
  fs::write(&entry, "export function main(): number { return 0;\n").unwrap();

  let assert = native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("project")
    .arg("--json")
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
fn native_js_cli_color_flag_emits_ansi_escapes() {
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
    .arg("--color")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stderr(predicates::str::contains("\u{1b}["));
}

#[test]
fn native_js_cli_no_color_flag_disables_ansi_escapes() {
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
    .arg("--no-color")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stderr(predicates::str::contains("\u{1b}[").not())
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
fn checked_pipeline_boolean_entrypoint_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): boolean { return true; }\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stdout(predicate::eq(""));
}

#[test]
fn checked_pipeline_run_supports_reexported_main() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(&impl_file, "export function main(): number { return 7; }\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export { main } from \"./impl\";\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(7)
    .stdout(predicate::eq(""));
}

#[test]
fn checked_pipeline_run_supports_local_reexported_main() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(
    &impl_file,
    "print(1);\nexport function main(): number { print(3); return 7; }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import { main } from \"./impl\";\nprint(2);\nexport { main };\n",
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
    .code(7)
    .stdout(predicate::eq("1\n2\n3\n"));
}

#[test]
fn checked_pipeline_run_supports_renamed_reexported_main() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(&impl_file, "export function run(): number { return 7; }\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export { run as main } from \"./impl\";\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(7)
    .stdout(predicate::eq(""));
}

#[test]
fn checked_pipeline_run_supports_renamed_local_reexported_main() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(
    &impl_file,
    "print(1);\nexport function run(): number { print(3); return 7; }\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "import { run } from \"./impl\";\nprint(2);\nexport { run as main };\n",
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
    .code(7)
    .stdout(predicate::eq("1\n2\n3\n"));
}

#[test]
fn checked_pipeline_run_supports_export_all_reexported_main() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(&impl_file, "export function main(): number { return 7; }\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export * from \"./impl\";\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(7)
    .stdout(predicate::eq(""));
}

#[test]
fn checked_pipeline_run_supports_reexported_main_through_reexport_chain() {
  let tmp = TempDir::new().unwrap();

  let impl_file = tmp.path().join("impl.ts");
  fs::write(
    &impl_file,
    "print(1);\nexport function main(): number { print(4); return 7; }\n",
  )
  .unwrap();

  let mid = tmp.path().join("mid.ts");
  fs::write(&mid, "print(2);\nexport { main } from \"./impl\";\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "print(3);\nexport * from \"./mid\";\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .failure()
    .code(7)
    .stdout(predicate::eq("1\n2\n3\n4\n"));
}

#[test]
fn checked_pipeline_void_entrypoint_exits_zero_without_stdout() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): void {}\n").unwrap();

  native_js_cli()
    .timeout(CLI_TIMEOUT)
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .code(0)
    .stdout(predicate::eq(""));
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
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
  assert_eq!(
    output.status.code(),
    Some(2),
    "unexpected status {:?}",
    output.status
  );
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn renamed_reexported_module_init_runs() {
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
  fs::write(&reexport, r#"export { x as y } from "./dep";"#).unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { y } from "./reexport";
export function main(): number { return y; }
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
  assert_eq!(
    output.status.code(),
    Some(42),
    "unexpected status {:?}",
    output.status
  );
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn local_reexported_import_resolves_and_initializers_run_in_order() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"print(1);
export function value(a: number, b: number): number { return a + b; }
"#,
  )
  .unwrap();

  let reexport = tmp.path().join("reexport.ts");
  fs::write(
    &reexport,
    r#"import { value } from "./dep";
print(2);
export { value };
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { value } from "./reexport";
print(3);
export function main(): number {
  print(value(20, 22));
  return 0;
}
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n3\n42\n"));
}

#[test]
fn renamed_local_reexported_import_resolves_and_initializers_run_in_order() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"print(1);
export function value(a: number, b: number): number { return a + b; }
"#,
  )
  .unwrap();

  let reexport = tmp.path().join("reexport.ts");
  fs::write(
    &reexport,
    r#"import { value } from "./dep";
print(2);
export { value as other };
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { other } from "./reexport";
print(3);
export function main(): number {
  print(other(20, 22));
  return 0;
}
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n3\n42\n"));
}

#[test]
fn local_reexported_import_with_import_alias_resolves_and_initializers_run_in_order() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(
    &dep,
    r#"print(1);
export function value(a: number, b: number): number { return a + b; }
"#,
  )
  .unwrap();

  let reexport = tmp.path().join("reexport.ts");
  fs::write(
    &reexport,
    r#"import { value as other } from "./dep";
print(2);
export { other };
"#,
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"import { other } from "./reexport";
print(3);
export function main(): number {
  print(other(20, 22));
  return 0;
}
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n3\n42\n"));
}

#[test]
fn side_effect_only_reexport_runs() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(42);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export {} from "./dep";
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
fn reexports_and_imports_run_in_declaration_order() {
  let tmp = TempDir::new().unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "print(1);\n").unwrap();

  let c = tmp.path().join("c.ts");
  fs::write(&c, "print(2);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export {} from "./b";
import "./c";
print(99);
export function main(): number { print(4); return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n99\n4\n"));
}

#[test]
fn named_reexports_and_imports_run_in_declaration_order() {
  let tmp = TempDir::new().unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "print(1);\nexport let x: number = 0;\n").unwrap();

  let c = tmp.path().join("c.ts");
  fs::write(&c, "print(2);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export { x } from "./b";
import "./c";
print(99);
export function main(): number { print(4); return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n99\n4\n"));
}

#[test]
fn export_all_reexports_and_imports_run_in_declaration_order() {
  let tmp = TempDir::new().unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "print(1);\nexport let x: number = 0;\n").unwrap();

  let c = tmp.path().join("c.ts");
  fs::write(&c, "print(2);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export * from "./b";
import "./c";
print(99);
export function main(): number { print(4); return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n99\n4\n"));
}

#[test]
fn export_all_namespace_reexports_and_imports_run_in_declaration_order() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(1);\nexport let x: number = 0;\n").unwrap();

  let other = tmp.path().join("other.ts");
  fs::write(&other, "print(2);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export * as ns from "./dep";
import "./other";
print(99);
export function main(): number { print(4); return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n99\n4\n"));
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
fn export_all_namespace_reexport_initializes_dependency() {
  let tmp = TempDir::new().unwrap();

  let dep = tmp.path().join("dep.ts");
  fs::write(&dep, "print(1);\n").unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    r#"export * as ns from "./dep";
print(2);
export function main(): number { print(3); return 0; }
"#,
  )
  .unwrap();

  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("1\n2\n3\n"));
}

#[test]
fn type_only_export_all_reexport_does_not_execute_module() {
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
    r#"export type * from "./dep";
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
fn type_only_export_all_namespace_reexport_does_not_execute_module() {
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
    r#"export type * as ns from "./dep";
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
fn type_only_reexport_cycles_do_not_trigger_cycle_detection() {
  let tmp = TempDir::new().unwrap();

  let a = tmp.path().join("a.ts");
  fs::write(&a, "print(1);\nexport type * from \"./b\";\n").unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(
    &b,
    "print(2);\nexport type T = number;\nexport type * from \"./a\";\n",
  )
  .unwrap();

  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export type * from \"./a\";\nexport function main(): number { print(3); return 0; }\n",
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
fn cyclic_module_dependency_is_rejected() {
  let tmp = TempDir::new().unwrap();

  let a = tmp.path().join("a.ts");
  fs::write(
    &a,
    "import \"./b\";\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "import \"./a\";\nprint(1);\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&a)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0146"))
    .stderr(predicates::str::contains("cyclic module dependency"));
}

#[test]
fn cyclic_module_dependency_through_reexports_is_rejected() {
  let tmp = TempDir::new().unwrap();

  let a = tmp.path().join("a.ts");
  fs::write(
    &a,
    "export {} from \"./b\";\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "export {} from \"./a\";\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&a)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0146"))
    .stderr(predicates::str::contains("cyclic module dependency"));
}

#[test]
fn cyclic_module_dependency_through_export_all_is_rejected() {
  let tmp = TempDir::new().unwrap();

  let a = tmp.path().join("a.ts");
  fs::write(
    &a,
    "export * from \"./b\";\nexport function main(): number { return 0; }\n",
  )
  .unwrap();

  let b = tmp.path().join("b.ts");
  fs::write(&b, "export * from \"./a\";\n").unwrap();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(CLI_TIMEOUT)
    .arg("build")
    .arg(&a)
    .arg("-o")
    .arg(&out)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0146"))
    .stderr(predicates::str::contains("cyclic module dependency"));
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

  let output = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(output.status.code(), Some(45));
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
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

  let output = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(output.status.code(), Some(1));
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
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

  let output = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(output.status.code(), Some(3));
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
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

  let output = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(output.status.code(), Some(55));
  assert!(
    output.stdout.is_empty(),
    "expected stdout to be empty, got: {}",
    String::from_utf8_lossy(&output.stdout)
  );
}
