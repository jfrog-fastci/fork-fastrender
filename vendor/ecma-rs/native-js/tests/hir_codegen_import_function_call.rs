use inkwell::context::Context;
use native_js::{codegen, strict};
use std::process::Command;
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
fn hir_codegen_supports_calls_to_imported_functions() {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found");
    return;
  };

  let a_key = FileKey::new("a.ts");
  let b_key = FileKey::new("b.ts");

  let a_src = r#"
export function addOne(x: number): number {
  return x + 1;
}
"#;

  let b_src = r#"
import { addOne } from "./a.ts";

export function main(): number {
  return addOne(2);
}
"#;

  let mut host = MemoryHost::new();
  host.insert(a_key.clone(), a_src);
  host.insert(b_key.clone(), b_src);
  host.link(b_key.clone(), "./a.ts", a_key.clone());

  let program = Program::new(host, vec![b_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");

  let file_a = program.file_id(&a_key).expect("file a id");
  let file_b = program.file_id(&b_key).expect("file b id");
  let strict_diags = strict::validate(&program, &[file_a, file_b]);
  assert!(strict_diags.is_empty(), "{strict_diags:#?}");
  let entrypoint = strict::entrypoint(&program, file_b).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file_b,
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
  assert!(out.status.success());
  assert_eq!(String::from_utf8_lossy(&out.stdout), "3\n");
}
