use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::process::Command as StdCommand;
use std::time::Duration;
use tempfile::TempDir;
use wait_timeout::ChildExt;

fn native_js() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js")
}

fn native_js_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js-cli")
}

fn run_with_timeout(
  cmd: &mut StdCommand,
  timeout: Duration,
) -> std::io::Result<std::process::ExitStatus> {
  let mut child = cmd.spawn()?;
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(30))
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
    .timeout(Duration::from_secs(60))
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(42));
}

#[test]
fn emit_llvm_ir_contains_symbols() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let ll = tmp.path().join("out.ll");
  native_js()
    .timeout(Duration::from_secs(60))
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
    .timeout(Duration::from_secs(60))
    .arg("run")
    .arg(&entry)
    .arg("--")
    .arg("--dummy-arg")
    .assert()
    .failure()
    .code(42);
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
    .timeout(Duration::from_secs(60))
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(42));
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
    .timeout(Duration::from_secs(60))
    .arg("--project")
    .arg(&tsconfig)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(42));
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
    .timeout(Duration::from_secs(60))
    .arg("check")
    .arg(&entry)
    .assert()
    .success();

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(Duration::from_secs(60))
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
fn check_and_build_reject_eval() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(
    &entry,
    "export function main(): number { eval(\"1\"); return 0; }\n",
  )
  .unwrap();

  native_js()
    .timeout(Duration::from_secs(60))
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .stderr(predicates::str::contains("NJS0009"))
    .stderr(predicates::str::contains("`eval()` is not supported"));

  let out = tmp.path().join("out-bin");
  native_js()
    .timeout(Duration::from_secs(60))
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
    .timeout(Duration::from_secs(60))
    .arg("--project")
    .arg(&tsconfig)
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .assert()
    .success();

  let status = run_with_timeout(&mut StdCommand::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(0));
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
    .timeout(Duration::from_secs(60))
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
    .timeout(Duration::from_secs(60))
    .arg("--pipeline")
    .arg("checked")
    .arg("run")
    .arg(&entry)
    .assert()
    .success()
    .stdout(predicate::eq("3\n"));
}

#[test]
fn checked_pipeline_check_succeeds_on_simple_program() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  native_js_cli()
    .timeout(Duration::from_secs(60))
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
    .timeout(Duration::from_secs(60))
    .arg("--pipeline")
    .arg("checked")
    .arg("check")
    .arg(&entry)
    .assert()
    .failure()
    .code(1)
    .stderr(predicates::str::contains("TS2322"));
}
