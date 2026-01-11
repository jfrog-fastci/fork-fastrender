use native_js::{compile_program, CompilerOptions, EmitKind, NativeJsError};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program, Severity};

static REL_OUT_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

#[test]
fn compile_emits_llvm_ir_file() {
  let mut host = es5_host();
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
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }

  let mut host = es5_host();
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

  let exe_bytes = std::fs::read(&artifact.path).unwrap();
  // `CompilerOptions::default()` should be non-PIE on Linux (ET_EXEC).
  let elf_type = u16::from_le_bytes([exe_bytes[16], exe_bytes[17]]);
  assert_eq!(elf_type, 2, "expected non-PIE ET_EXEC (e_type={elf_type})");

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

  let mut stdout = String::new();
  child
    .stdout
    .take()
    .unwrap()
    .read_to_string(&mut stdout)
    .unwrap();
  let mut stderr = String::new();
  child
    .stderr
    .take()
    .unwrap()
    .read_to_string(&mut stderr)
    .unwrap();

  assert_eq!(
    status.code(),
    Some(3),
    "unexpected exit status {status:?} stdout={stdout:?} stderr={stderr:?}"
  );
  assert_eq!(stdout, "");

  let _ = std::fs::remove_file(&artifact.path);
}

#[test]
#[cfg(target_os = "linux")]
fn compile_emits_pie_executable_and_runs() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }
  if !command_works("llvm-objcopy-18") && !command_works("llvm-objcopy") {
    eprintln!("skipping: llvm-objcopy not found in PATH");
    return;
  }

  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main(): number { return 3; }");

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.pie = true;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert!(
    artifact.path.exists(),
    "missing artifact {}",
    artifact.path.display()
  );

  let exe_bytes = std::fs::read(&artifact.path).unwrap();
  // PIE should be ET_DYN.
  let elf_type = u16::from_le_bytes([exe_bytes[16], exe_bytes[17]]);
  assert_eq!(elf_type, 3, "expected PIE ET_DYN (e_type={elf_type})");

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

  let mut stdout = String::new();
  child
    .stdout
    .take()
    .unwrap()
    .read_to_string(&mut stdout)
    .unwrap();
  let mut stderr = String::new();
  child
    .stderr
    .take()
    .unwrap()
    .read_to_string(&mut stderr)
    .unwrap();

  assert_eq!(
    status.code(),
    Some(3),
    "unexpected exit status {status:?} stdout={stdout:?} stderr={stderr:?}"
  );
  assert_eq!(stdout, "");

  let _ = std::fs::remove_file(&artifact.path);
}

#[test]
#[cfg(target_os = "linux")]
fn compile_allows_executable_output_path_without_parent_dir() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }

  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main(): number { return 7; }");

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let n = REL_OUT_COUNTER.fetch_add(1, Ordering::Relaxed);
  let exe_rel = PathBuf::from(format!("native-js-test-out-{}-{n}", std::process::id()));

  // Ensure we don't leak build artifacts into the repo root even if the test panics.
  struct Cleanup(PathBuf);
  impl Drop for Cleanup {
    fn drop(&mut self) {
      let _ = std::fs::remove_file(&self.0);
    }
  }
  let _cleanup = Cleanup(exe_rel.clone());

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  // Use a relative path with no parent component to exercise the `Path::parent == Some(\"\")`
  // behavior in the linker helpers.
  opts.output = Some(exe_rel.clone());

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert_eq!(artifact.path, exe_rel);
  assert!(
    artifact.path.exists(),
    "missing artifact {}",
    artifact.path.display()
  );

  // `Command` searches PATH when the program name contains no path separators, so run via an
  // absolute path.
  let abs_path = std::env::current_dir().unwrap().join(&artifact.path);

  use std::process::{Command, Stdio};
  use std::time::Duration;
  use wait_timeout::ChildExt;

  let mut child = Command::new(&abs_path)
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

  let mut stdout = String::new();
  child
    .stdout
    .take()
    .unwrap()
    .read_to_string(&mut stdout)
    .unwrap();
  let mut stderr = String::new();
  child
    .stderr
    .take()
    .unwrap()
    .read_to_string(&mut stderr)
    .unwrap();

  assert_eq!(
    status.code(),
    Some(7),
    "unexpected exit status {status:?} stdout={stdout:?} stderr={stderr:?}"
  );
  assert_eq!(stdout, "");
}

#[test]
fn compile_rejects_type_errors() {
  let mut host = es5_host();
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
