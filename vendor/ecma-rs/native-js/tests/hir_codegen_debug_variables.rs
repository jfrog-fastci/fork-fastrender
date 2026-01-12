use inkwell::context::Context;
use native_js::{codegen, strict};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn hir_codegen_debug_variables_emits_dbg_declare_for_params_and_locals() {
  let source = r#"
function add(x: number): number {
  let y = x + 1;
  return y;
}

export function main(): number {
  return add(41);
}
  "#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file = program.file_id(&key).expect("file id");
  let strict_diags = strict::validate(&program, &[file]);
  assert!(strict_diags.is_empty(), "{strict_diags:#?}");
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file,
    entrypoint,
    codegen::CodegenOptions {
      debug: true,
      ..Default::default()
    },
  )
  .expect("codegen");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.dbg.declare"),
    "expected llvm.dbg.declare in IR:\n{ir}"
  );
  assert!(
    ir.contains("DILocalVariable(name: \"x\""),
    "expected parameter name `x` in debug info:\n{ir}"
  );
  assert!(
    ir.contains("DILocalVariable(name: \"y\""),
    "expected local name `y` in debug info:\n{ir}"
  );
}

#[test]
fn hir_codegen_debug_variables_disabled_has_no_dbg_declare() {
  let source = r#"
function add(x: number): number {
  let y = x + 1;
  return y;
}

export function main(): number {
  return add(41);
}
  "#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file = program.file_id(&key).expect("file id");
  let strict_diags = strict::validate(&program, &[file]);
  assert!(strict_diags.is_empty(), "{strict_diags:#?}");
  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .expect("codegen");

  let ir = module.print_to_string().to_string();
  assert!(
    !ir.contains("@llvm.dbg.declare"),
    "expected non-debug builds to omit llvm.dbg.declare:\n{ir}"
  );
}

