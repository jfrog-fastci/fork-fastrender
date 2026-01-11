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
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
}

#[test]
fn hir_codegen_resolves_shadowing_and_assignments() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found");
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
  let status = Command::new(clang)
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-O0")
    .arg("-o")
    .arg(&exe_path)
    .status()
    .expect("clang");
  assert!(status.success(), "clang failed with {status}");

  let out = Command::new(&exe_path).output().expect("run exe");
  assert_eq!(out.status.code(), Some(3));
  assert!(out.stdout.is_empty());
}
