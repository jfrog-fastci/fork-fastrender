use native_js::{compile_program, CompilerOptions, EmitKind, NativeJsError};
use typecheck_ts::{FileKey, MemoryHost, Program, Severity};

#[test]
fn compile_emits_llvm_ir_file() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main() { let x = 1; return x + 2; }");

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::LlvmIr;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::LlvmIr);
  assert!(artifact.path.exists(), "missing artifact {}", artifact.path.display());

  let ir = std::fs::read_to_string(&artifact.path).unwrap();
  assert!(ir.contains("define"), "expected LLVM IR to contain `define`:\n{ir}");
  assert!(ir.contains("@main"), "expected LLVM IR to contain `@main`:\n{ir}");
  assert!(
    ir.contains("add i32"),
    "expected LLVM IR to include codegen for `x + 2` (missing `add i32`):\n{ir}"
  );

  let _ = std::fs::remove_file(&artifact.path);
}

#[test]
#[cfg(target_os = "linux")]
fn compile_emits_executable_and_runs() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }

  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main(): number { return 3; }");

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert!(
    artifact.path.exists(),
    "missing artifact {}",
    artifact.path.display()
  );

  use std::process::{Command, Stdio};
  use std::time::Duration;
  use wait_timeout::ChildExt;

  let mut child = Command::new(&artifact.path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let Some(status) = child.wait_timeout(Duration::from_secs(5)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("compiled executable timed out");
  };

  assert_eq!(
    status.code(),
    Some(3),
    "expected exit code 3, got {status:?}"
  );

  let _ = std::fs::remove_file(&artifact.path);
}

#[test]
fn compile_rejects_type_errors() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    "export function main(): number { return \"not a number\"; }",
  );

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let opts = CompilerOptions::default();
  let err = compile_program(&program, entry, &opts).unwrap_err();

  match err {
    NativeJsError::TypecheckFailed { diagnostics } => {
      assert!(diagnostics.iter().any(|d| d.severity == Severity::Error));
    }
    other => panic!("expected NativeJsError::TypecheckFailed, got {other:?}"),
  }
}

#[cfg(target_os = "linux")]
fn command_works(cmd: &str) -> bool {
  std::process::Command::new(cmd)
    .arg("--version")
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}
