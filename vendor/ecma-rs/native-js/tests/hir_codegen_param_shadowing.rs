use inkwell::context::Context;
use native_js::{codegen, strict};
use std::process::Command;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(cand);
    }
  }
  None
}

#[test]
fn hir_codegen_resolves_param_shadowing_outer_binding() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found");
    return;
  };
  let Some(runtime_native_a) = native_js::link::find_runtime_native_staticlib() else {
    eprintln!("skipping: runtime-native staticlib not found");
    return;
  };

  let source = r#"
let x = 1;

function f(x: number): number {
  return x + 1;
}

export function main(): number {
  return f(2) + x;
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
  let mut cmd = Command::new(clang);
  cmd
    .arg("-Wno-override-module")
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    // Reset language so archive inputs are not treated as IR.
    .arg("-x")
    .arg("none")
    .arg("-O0")
    .arg(&runtime_native_a);
  // `runtime-native` is a Rust `staticlib`, so we need to explicitly provide the system libraries
  // rustc would normally inject when it is the final linker driver.
  if cfg!(target_os = "linux") {
    cmd.args(["-lpthread", "-ldl", "-lm", "-lrt"]);
  }
  let status = cmd.arg("-o").arg(&exe_path).status().expect("clang");
  assert!(status.success(), "clang failed with {status}");

  let out = Command::new(&exe_path).output().expect("run exe");
  assert_eq!(out.status.code(), Some(4));
  assert!(out.stdout.is_empty());
}
