use native_js::{compile, CompilerOptions as NativeCompilerOptions, EmitKind, NativeJsError};
use std::process::{Command, Stdio};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn cmd_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn clang_available() -> bool {
  cmd_works("clang-18") || cmd_works("clang")
}

fn lld_available() -> bool {
  cmd_works("ld.lld-18") || cmd_works("ld.lld")
}

#[test]
fn compile_to_llvm_ir_contains_expected_symbols() {
  // Avoid loading TypeScript's default lib set (which includes `dom` and is large). We only need
  // `es5` for core built-in types like `Array`/`String`.
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    "export function main(): number { return 1 + 2 * 3; }\n",
  );

  let program = Program::new(host, vec![entry.clone()]);

  let file = program.file_id(&entry).expect("file id");
  let def = program
    .exports_of(file)
    .get("main")
    .and_then(|e| e.def)
    .expect("exported def for `main`");
  let expected_symbol = native_js::llvm_symbol_for_def(&program, def);

  let tmp = tempfile::tempdir().expect("tempdir");
  let out_path = tmp.path().join("out.ll");

  let mut opts = NativeCompilerOptions::default();
  opts.emit = EmitKind::LlvmIr;
  opts.output = Some(out_path.clone());

  let out = compile(&program, &opts).expect("compile");
  assert_eq!(out.artifact, out_path);
  let ir = out.llvm_ir.expect("expected llvm_ir");

  assert!(
    ir.contains(&expected_symbol),
    "IR did not contain `{expected_symbol}`:\n{ir}"
  );
  assert!(
    ir.contains("__nativejs_file_init_"),
    "IR did not contain any __nativejs_file_init_ symbols:\n{ir}"
  );
  assert!(
    ir.contains("define i32 @main()"),
    "IR did not contain a C ABI main() shim:\n{ir}"
  );
}

#[test]
#[cfg(target_os = "linux")]
fn compile_to_executable_and_run_returns_exit_code() {
  if !clang_available() {
    eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
    return;
  }
  if !lld_available() {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return;
  }

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), "export function main(): number { return 42; }\n");

  let program = Program::new(host, vec![entry.clone()]);
  let file = program.file_id(&entry).expect("file id");
  let def = program
    .exports_of(file)
    .get("main")
    .and_then(|e| e.def)
    .expect("exported def for `main`");
  let expected_symbol = native_js::llvm_symbol_for_def(&program, def);

  let tmp = tempfile::tempdir().expect("tempdir");
  let exe_path = tmp.path().join("out");
  let ir_path = tmp.path().join("out.ll");

  let mut opts = NativeCompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe_path.clone());
  opts.emit_ir = Some(ir_path);

  let out = compile(&program, &opts).expect("compile");
  let ir = out.llvm_ir.expect("expected llvm_ir when emit_ir is set");
  assert!(
    ir.contains(&expected_symbol),
    "IR did not contain `{expected_symbol}`:\n{ir}"
  );
  let status = Command::new(&out.artifact)
    .status()
    .expect("run compiled executable");

  assert_eq!(status.code(), Some(42));
}

#[test]
fn compile_reports_typecheck_failed_for_type_errors() {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    "export function main(): number { return \"nope\"; }\n",
  );

  let program = Program::new(host, vec![entry.clone()]);

  let mut opts = NativeCompilerOptions::default();
  opts.emit = EmitKind::LlvmIr;

  let err = compile(&program, &opts).expect_err("expected typecheck error");
  match err {
    NativeJsError::TypecheckFailed { diagnostics } => {
      assert!(
        diagnostics.iter().any(|d| d.code == "TS2322"),
        "expected a TS2322 diagnostic, got: {diagnostics:?}"
      );
    }
    other => panic!("expected NativeJsError::TypecheckFailed, got {other:?}"),
  }
}

#[test]
fn compile_rejects_multi_root_programs() {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let a = FileKey::new("a.ts");
  let b = FileKey::new("b.ts");
  host.insert(a.clone(), "export function main(): number { return 0; }\n");
  host.insert(b.clone(), "export function main(): number { return 0; }\n");

  let program = Program::new(host, vec![a, b]);

  let mut opts = NativeCompilerOptions::default();
  opts.emit = EmitKind::LlvmIr;

  let err = compile(&program, &opts).expect_err("expected unsupported feature error");
  match err {
    NativeJsError::UnsupportedFeature(msg) => {
      assert!(
        msg.contains("exactly one root file"),
        "unexpected message: {msg}"
      );
    }
    other => panic!("expected NativeJsError::UnsupportedFeature, got {other:?}"),
  }
}
