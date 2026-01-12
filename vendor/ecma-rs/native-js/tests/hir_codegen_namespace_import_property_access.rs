use inkwell::context::Context;
use native_js::{codegen, strict};
use std::process::Command;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn hir_codegen_supports_namespace_import_property_access() {
  let Some((clang, runtime_native_a)) = common::clang_and_runtime_native() else {
    return;
  };

  let a_key = FileKey::new("a.ts");
  let main_key = FileKey::new("main.ts");

  let a_src = r#"
export function addOne(x: number): number {
  return x + 1;
}

export const y: number = 3;
"#;

  let main_src = r#"
import * as ns from "./a.ts";

export function main(): number {
  return ns.addOne(ns.y);
}
"#;

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(a_key.clone(), a_src);
  host.insert(main_key.clone(), main_src);
  host.link(main_key.clone(), "./a.ts", a_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file_a = program.file_id(&a_key).expect("file a id");
  let file_main = program.file_id(&main_key).expect("file main id");
  let strict_diags = strict::validate(&program, &[file_a, file_main]);
  assert!(strict_diags.is_empty(), "{strict_diags:#?}");
  let entrypoint = strict::entrypoint(&program, file_main).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file_main,
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
  assert_eq!(out.status.code(), Some(4));
  assert!(out.stdout.is_empty());
}
