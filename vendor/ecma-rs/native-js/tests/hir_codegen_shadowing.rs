use inkwell::context::Context;
use native_js::{codegen, strict};
use std::process::Command;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn hir_codegen_resolves_shadowing_and_assignments() {
  let Some((clang, runtime_native_a)) = common::clang_and_runtime_native() else {
    return;
  };

  let source = r#"
export function main(): number {
  let x = 1;
  let y = 0;
  {
    let x = 2;
    y = x;
  }
  return y + x;
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
  let td = tempfile::tempdir().expect("tempdir");
  let ll_path = td.path().join("out.ll");
  std::fs::write(&ll_path, ir).expect("write ir");

  let exe_path = td.path().join("out");
  let out = common::clang_link_ir_to_exe(clang, &ll_path, &exe_path, &runtime_native_a);
  assert!(
    out.status.success(),
    "clang failed (status={status}):\nstdout:\n{stdout}\nstderr:\n{stderr}",
    status = out.status,
    stdout = String::from_utf8_lossy(&out.stdout),
    stderr = String::from_utf8_lossy(&out.stderr)
  );

  let out = Command::new(&exe_path).output().expect("run exe");
  assert_eq!(out.status.code(), Some(3));
  assert!(out.stdout.is_empty());
}
