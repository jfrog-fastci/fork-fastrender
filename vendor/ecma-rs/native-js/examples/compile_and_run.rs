use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use std::process::{Command, Stdio};

fn main() {
  let source = r#"
console.log(1 + 2 * 3);
console.log(true);
console.log("hello from native-js");
"#;

  let mut opts = CompileOptions::default();
  opts.builtins = true;

  let ir = compile_typescript_to_llvm_ir(source, opts).expect("compile TS to LLVM IR");

  let dir = tempfile::tempdir().expect("tempdir");
  let ll_path = dir.path().join("out.ll");
  std::fs::write(&ll_path, ir).expect("write IR");

  let exe = dir.path().join("program");
  let clang = find_clang().expect("find clang (clang-18 or clang)");
  let status = Command::new(clang)
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-o")
    .arg(&exe)
    .status()
    .expect("invoke clang");
  assert!(status.success(), "clang failed with status {status}");

  let output = Command::new(&exe)
    .stdin(Stdio::null())
    .output()
    .expect("run compiled program");
  print!("{}", String::from_utf8_lossy(&output.stdout));
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
}
