use native_js::compiler::compile_entry_to_llvm_ir;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

#[test]
fn arithmetic_codegen_emits_fmul_fadd() {
  let mut host = es5_host();
  let file = FileKey::new("file0.ts");
  host.insert(
    file.clone(),
    "export function main(): number { let a = 1; let b = 2; let c = 3; return a + b * c; }",
  );
  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(
    diags.is_empty(),
    "typecheck diagnostics: {:#?}",
    diags
  );

  let file_id = program.file_id(&file).expect("file id");
  let output = compile_entry_to_llvm_ir(&program, file_id, "main");
  assert!(
    output.diagnostics.is_empty(),
    "native-js diagnostics: {:#?}",
    output.diagnostics
  );
  let ir = output.llvm_ir.expect("llvm ir");
  assert!(ir.contains("fmul"), "IR did not contain fmul:\n{ir}");
  assert!(ir.contains("fadd"), "IR did not contain fadd:\n{ir}");
}

#[test]
fn comparison_codegen_emits_fcmp() {
  let mut host = es5_host();
  let file = FileKey::new("file0.ts");
  host.insert(
    file.clone(),
    "export function main(): boolean { let a = 1; let b = 2; return a < b; }",
  );
  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(
    diags.is_empty(),
    "typecheck diagnostics: {:#?}",
    diags
  );

  let file_id = program.file_id(&file).expect("file id");
  let output = compile_entry_to_llvm_ir(&program, file_id, "main");
  assert!(
    output.diagnostics.is_empty(),
    "native-js diagnostics: {:#?}",
    output.diagnostics
  );
  let ir = output.llvm_ir.expect("llvm ir");
  assert!(ir.contains("fcmp olt"), "IR did not contain fcmp olt:\n{ir}");
}

#[test]
fn typeof_is_rejected() {
  let mut host = es5_host();
  let file = FileKey::new("file0.ts");
  host.insert(file.clone(), "export function main() { return typeof 1; }");
  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(
    diags.is_empty(),
    "typecheck diagnostics: {:#?}",
    diags
  );

  let file_id = program.file_id(&file).expect("file id");
  let output = compile_entry_to_llvm_ir(&program, file_id, "main");
  assert!(
    output
      .diagnostics
      .iter()
      .any(|d| d.code.as_str() == "NJS0200"),
    "expected NJS0200 unsupported expr diagnostic, got: {:#?}",
    output.diagnostics
  );
}
