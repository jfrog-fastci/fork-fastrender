use inkwell::context::Context;
use native_js::{codegen, strict};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn codegen_ir(source: &str) -> (String, String) {
  let mut host = es5_host();
  let file = FileKey::new("file0.ts");
  host.insert(file.clone(), source);
  let program = Program::new(host, vec![file.clone()]);

  let diags = program.check();
  assert!(diags.is_empty(), "typecheck diagnostics: {diags:#?}");

  let file_id = program.file_id(&file).expect("file id");
  let strict_diags = strict::validate(&program, &[file_id]);
  assert!(
    strict_diags.is_empty(),
    "strict-subset diagnostics: {strict_diags:#?}"
  );
  let entrypoint = strict::entrypoint(&program, file_id).expect("valid entrypoint");
  let ts_main_sym = native_js::llvm_symbol_for_def(&program, entrypoint.main_def);

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file_id,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .expect("codegen");
  if let Err(err) = module.verify() {
    panic!(
      "LLVM module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }
  (module.print_to_string().to_string(), ts_main_sym)
}

fn find_function_ir(ir: &str, symbol: &str) -> String {
  let needle = format!("@{symbol}(");
  let mut start = None;
  let mut end = None;

  for (idx, line) in ir.lines().enumerate() {
    let trimmed = line.trim_start();
    if start.is_none() && trimmed.starts_with("define") && trimmed.contains(&needle) {
      start = Some(idx);
      continue;
    }
    if start.is_some() && trimmed == "}" {
      end = Some(idx);
      break;
    }
  }

  let (Some(start), Some(end)) = (start, end) else {
    panic!("failed to locate function `{symbol}` in IR:\n{ir}");
  };

  ir.lines()
    .skip(start)
    .take(end - start + 1)
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn arithmetic_codegen_emits_mul_add() {
  let (ir, ts_main_sym) = codegen_ir(
    "export function main(): number { let a = 1; let b = 2; let c = 3; return a + b * c; }",
  );
  let fn_ir = find_function_ir(&ir, &ts_main_sym);
  assert!(
    fn_ir.contains("mul i32") || fn_ir.contains("fmul"),
    "IR did not contain mul/fmul:\n{fn_ir}"
  );
  assert!(
    fn_ir.contains("add i32") || fn_ir.contains("fadd"),
    "IR did not contain add/fadd:\n{fn_ir}"
  );
}

#[test]
fn comparison_codegen_emits_cmp() {
  let (ir, ts_main_sym) = codegen_ir("export function main(): boolean { let a = 1; let b = 2; return a < b; }");
  let fn_ir = find_function_ir(&ir, &ts_main_sym);
  assert!(
    fn_ir.contains("icmp slt") || fn_ir.contains("fcmp olt"),
    "IR did not contain comparison (`icmp slt` / `fcmp olt`):\n{fn_ir}"
  );
}

#[test]
fn typeof_is_rejected() {
  let mut host = es5_host();
  let file = FileKey::new("file0.ts");
  host.insert(
    file.clone(),
    "export function main(): number { typeof 1; return 0; }",
  );
  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "typecheck diagnostics: {diags:#?}");

  let file_id = program.file_id(&file).expect("file id");
  let entrypoint = strict::entrypoint(&program, file_id).expect("valid entrypoint");
  let context = Context::create();
  let result = codegen::codegen(
    &context,
    &program,
    file_id,
    entrypoint,
    codegen::CodegenOptions::default(),
  );
  let diags = result.expect_err("expected codegen to reject typeof");
  assert!(
    diags.iter().any(|d| d.code.as_str() == "NJS0105"),
    "expected NJS0105 unsupported unary operator diagnostic, got: {diags:#?}"
  );
}
