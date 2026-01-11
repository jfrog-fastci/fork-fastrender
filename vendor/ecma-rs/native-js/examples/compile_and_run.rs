use native_js::compiler::compile_typescript_to_artifact;
use native_js::{CompileOptions, EmitKind};
use std::process::{Command, Stdio};

fn main() {
  let source = r#"
 console.log(1 + 2 * 3);
 console.log(true);
 console.log("hello from native-js");
 "#;

  let mut opts = CompileOptions::default();
  opts.builtins = true;
  opts.emit = EmitKind::Executable;

  let out = compile_typescript_to_artifact(source, opts, None).expect("compile TS to executable");

  let output = Command::new(&out.path)
    .stdin(Stdio::null())
    .output()
    .expect("run compiled program");
  if !output.status.success() {
    eprintln!("compiled program failed: {}", output.status);
    eprintln!("stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    std::process::exit(1);
  }
  print!("{}", String::from_utf8_lossy(&output.stdout));
}
