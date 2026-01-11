use native_js::{compile, CompilerOptions, EmitKind};
use std::process::Command;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn compile_to_llvm_ir_contains_expected_symbols() {
  let mut host = MemoryHost::new();
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

  let mut opts = CompilerOptions::default();
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
  let mut host = MemoryHost::new();
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

  let mut opts = CompilerOptions::default();
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
