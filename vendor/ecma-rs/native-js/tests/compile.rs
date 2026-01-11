use native_js::{compile_program, CompilerOptions, EmitKind, NativeJsError};
use typecheck_ts::{FileKey, MemoryHost, Program, Severity};

#[test]
fn compile_emits_llvm_ir_file() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main() { return 1 + 2; }");

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
