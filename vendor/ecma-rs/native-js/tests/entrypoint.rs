use diagnostics::Severity;
use native_js::{compile_program, CompilerOptions, EmitKind};
use std::process::Command;
use tempfile::tempdir;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn clang_available() -> bool {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand).arg("--version").output().is_ok() {
      return true;
    }
  }
  false
}

#[test]
fn entrypoint_number_returns_exit_code() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-js entrypoint test: executable emission is linux-only");
    return;
  }
  if !clang_available() {
    eprintln!("skipping native-js entrypoint test: clang not found");
    return;
  }

  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export function main(): number { return 42; }");

  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");

  let file_id = program.file_id(&file).unwrap();
  let dir = tempdir().unwrap();
  let exe = dir.path().join("out");
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe.clone());
  let artifact = compile_program(&program, file_id, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert_eq!(artifact.path, exe);

  let out = Command::new(&artifact.path).output().unwrap();
  assert_eq!(out.status.code(), Some(42));
  assert!(out.stdout.is_empty());
}

#[test]
fn entrypoint_boolean_returns_exit_code() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-js entrypoint test: executable emission is linux-only");
    return;
  }
  if !clang_available() {
    eprintln!("skipping native-js entrypoint test: clang not found");
    return;
  }

  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export function main(): boolean { return true; }");

  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");

  let file_id = program.file_id(&file).unwrap();
  let dir = tempdir().unwrap();
  let exe = dir.path().join("out");
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe.clone());
  let artifact = compile_program(&program, file_id, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert_eq!(artifact.path, exe);

  let out = Command::new(&artifact.path).output().unwrap();
  assert_eq!(out.status.code(), Some(1));
  assert!(out.stdout.is_empty());
}

#[test]
fn entrypoint_void_returns_exit_code() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-js entrypoint test: executable emission is linux-only");
    return;
  }
  if !clang_available() {
    eprintln!("skipping native-js entrypoint test: clang not found");
    return;
  }

  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export function main(): void {}");

  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");

  let file_id = program.file_id(&file).unwrap();
  let dir = tempdir().unwrap();
  let exe = dir.path().join("out");
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe.clone());
  let artifact = compile_program(&program, file_id, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert_eq!(artifact.path, exe);

  let out = Command::new(&artifact.path).output().unwrap();
  assert_eq!(out.status.code(), Some(0));
  assert!(out.stdout.is_empty());
}

#[test]
fn missing_entrypoint_reports_diagnostic() {
  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export const x = 1;");

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).unwrap();
  let opts = CompilerOptions::default();

  let err = compile_program(&program, file_id, &opts).unwrap_err();
  let diags = err.diagnostics().unwrap_or(&[]);
  assert!(
    diags
      .iter()
      .any(|d| d.severity == Severity::Error && d.code == "NJS0108"),
    "expected NJS0108 error diagnostic, got: {err:#?}"
  );
}
