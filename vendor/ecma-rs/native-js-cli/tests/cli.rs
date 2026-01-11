use assert_cmd::Command;
use std::fs;
use std::process::Command as StdCommand;
use std::time::Duration;
use tempfile::TempDir;
use wait_timeout::ChildExt;

fn native_js() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("native-js")
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

  let out = tmp.path().join("out-bin");
  let ll = tmp.path().join("out.ll");
  native_js()
    .timeout(Duration::from_secs(60))
    .arg("build")
    .arg(&entry)
    .arg("-o")
    .arg(&out)
    .arg("--emit=llvm-ir")
    .arg("--emit-path")
    .arg(&ll)
    .assert()
    .success();

  let text = fs::read_to_string(&ll).unwrap();
  assert!(text.contains("@ts_main"), "expected IR to mention ts_main");
  assert!(text.contains("define"), "expected IR to contain function definitions");
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
