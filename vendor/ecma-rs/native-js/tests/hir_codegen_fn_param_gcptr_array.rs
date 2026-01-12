use inkwell::context::Context;
use native_js::{codegen, strict};
use std::process::Command;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn hir_codegen_fn_param_gcptr_array() {
  let Some((clang, runtime_native_a)) = common::clang_and_runtime_native() else {
    return;
  };

  let key = FileKey::new("main.ts");
  let src = r#"
    function sum(xs: number[]): number {
      return xs[0] + xs[1];
    }

    export function main(): number {
      const xs = [1,2];
      return sum(xs);
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
  let sum_sig = ir
    .lines()
    .find(|line| line.contains("define") && line.contains("_sum("))
    .expect("expected `sum` definition in IR");
  assert!(
    sum_sig.contains("double") && sum_sig.contains("ptr addrspace(1)"),
    "expected `sum` signature to take `ptr addrspace(1)`:\n{sum_sig}\n\nIR:\n{ir}"
  );

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

