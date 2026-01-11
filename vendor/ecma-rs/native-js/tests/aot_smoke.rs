use native_js::compiler::compile_typescript_to_artifact;
use native_js::{CompileOptions, EmitKind};
use std::process::Command;

#[test]
#[cfg(target_os = "linux")]
fn aot_smoke() {
  let dir = tempfile::tempdir().unwrap();
  let exe_path = dir.path().join("aot_smoke");

  let source = r#"
    console.log("native-js aot ok");
  "#;

  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::Executable;
  opts.debug = false;

  compile_typescript_to_artifact(source, opts, Some(exe_path.clone())).unwrap();

  let output = Command::new("timeout")
    .args(["5", exe_path.to_str().unwrap()])
    .output()
    .unwrap();

  assert!(output.status.success(), "status={:?} stderr={}", output.status, String::from_utf8_lossy(&output.stderr));
  assert_eq!(String::from_utf8_lossy(&output.stdout), "native-js aot ok\n");
}
