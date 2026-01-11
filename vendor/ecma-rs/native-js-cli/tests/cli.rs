use assert_cmd::prelude::*;
use std::fs;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use wait_timeout::ChildExt;

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<std::process::ExitStatus> {
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
fn build_and_run_returns_exit_code() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 42; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("native-js"));
  cmd.args([
    "build",
    entry.to_str().unwrap(),
    "-o",
    out.to_str().unwrap(),
  ]);
  cmd.assert().success();

  let status = run_with_timeout(&mut Command::new(&out), Duration::from_secs(5)).unwrap();
  assert_eq!(status.code(), Some(42));
}

#[test]
fn emit_llvm_ir_contains_symbols() {
  let tmp = TempDir::new().unwrap();
  let entry = tmp.path().join("entry.ts");
  fs::write(&entry, "export function main(): number { return 0; }\n").unwrap();

  let out = tmp.path().join("out-bin");
  let ll = tmp.path().join("out.ll");
  let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("native-js"));
  cmd.args([
    "build",
    entry.to_str().unwrap(),
    "-o",
    out.to_str().unwrap(),
    "--emit=llvm-ir",
    "--emit-path",
    ll.to_str().unwrap(),
  ]);
  cmd.assert().success();

  let text = fs::read_to_string(&ll).unwrap();
  assert!(text.contains("@ts_main"), "expected IR to mention ts_main");
  assert!(text.contains("define"), "expected IR to contain function definitions");
}
