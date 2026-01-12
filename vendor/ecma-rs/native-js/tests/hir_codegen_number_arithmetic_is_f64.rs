use inkwell::context::Context;
use native_js::{codegen, strict};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn hir_codegen_numbers_use_f64_ops() {
  let key = FileKey::new("main.ts");
  let src = r#"
    export function main(): number {
      let x: number = 1;
      return x + x * 3;
    }
  "#;

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(key.clone(), src);

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
  assert!(ir.contains("fadd"), "expected `fadd` in IR:\n{ir}");
  assert!(ir.contains("fmul"), "expected `fmul` in IR:\n{ir}");
  assert!(
    !ir.contains("add i32"),
    "expected number arithmetic not to lower to `add i32`:\n{ir}"
  );
  assert!(
    !ir.contains("mul i32"),
    "expected number arithmetic not to lower to `mul i32`:\n{ir}"
  );
}
