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
fn hir_codegen_supports_calls_to_reexported_functions() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found");
    return;
  };
  let Some(runtime_native_a) = native_js::link::find_runtime_native_staticlib() else {
    eprintln!("skipping: runtime-native staticlib not found");
    return;
  };

  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");
  let main_key = FileKey::new("main.ts");

  let a_src = r#"
export function foo(): number {
  return 7;
}
"#;

  let b_src = r#"
export { foo } from "./a.ts";
"#;

  let main_src = r#"
import { foo } from "./b.ts";

export function main(): number {
  return foo();
}
"#;

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.insert(main_key.clone(), main_src);
  host.link(b_key.clone(), "./a.ts", a_key.clone());
  host.link(main_key.clone(), "./b.ts", b_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file_a = program.file_id(&a_key).expect("file a id");
  let file_b = program.file_id(&b_key).expect("file b id");
  let file_main = program.file_id(&main_key).expect("file main id");
  let strict_diags = strict::validate(&program, &[file_a, file_b, file_main]);
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
  assert_eq!(out.status.code(), Some(7));
  assert!(out.stdout.is_empty());
}
