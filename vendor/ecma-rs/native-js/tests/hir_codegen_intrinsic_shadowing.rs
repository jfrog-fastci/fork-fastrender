use inkwell::context::Context;
use native_js::{codegen, strict, validate};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

#[test]
fn hir_codegen_lowers_global_print_statement_to_intrinsic() {
  let mut host = es5_host();
  host.add_lib(native_js::builtins::checked_builtins_lib());

  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function main(): number {
  print(1);
  return 0;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  validate::validate_strict_subset(&program).expect("strict-subset validation");

  let file = program.file_id(&key).expect("file id");
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
    ir.contains("rt_print_i32"),
    "expected global `print(1);` to lower to the `rt_print_i32` intrinsic call:\n{ir}"
  );
}

#[test]
fn hir_codegen_does_not_treat_user_defined_print_as_intrinsic() {
  let mut host = es5_host();
  host.add_lib(native_js::builtins::checked_builtins_lib());

  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function print(x: number): number {
  return x + 1;
}

export function main(): number {
  // Statement position call should invoke the user-defined function, not the intrinsic.
  print(1);
  return print(41);
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  validate::validate_strict_subset(&program).expect("strict-subset validation");

  let file = program.file_id(&key).expect("file id");
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
    !ir.contains("rt_print_i32"),
    "expected user-defined print not to lower to rt_print_i32 intrinsic call:\n{ir}"
  );
}
