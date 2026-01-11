use native_js::compiler::compile_typescript_to_artifact;
use native_js::{CompileOptions, EmitKind};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

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

  let mut child = Command::new(&exe_path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let Some(status) = child.wait_timeout(Duration::from_secs(5)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("compiled executable timed out");
  };

  let mut stdout = String::new();
  child
    .stdout
    .take()
    .unwrap()
    .read_to_string(&mut stdout)
    .unwrap();
  let mut stderr = String::new();
  child
    .stderr
    .take()
    .unwrap()
    .read_to_string(&mut stderr)
    .unwrap();

  assert!(status.success(), "status={status:?} stderr={stderr}");
  assert_eq!(stdout, "native-js aot ok\n");
}

#[test]
#[cfg(target_os = "linux")]
fn aot_smoke_debug_keeps_intermediates() {
  let dir = tempfile::tempdir().unwrap();
  let exe_path = dir.path().join("aot_smoke_debug");

  let source = r#"
    console.log("native-js aot ok");
  "#;

  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::Executable;
  opts.debug = true;

  compile_typescript_to_artifact(source, opts, Some(exe_path.clone())).unwrap();

  assert!(
    exe_path.with_extension("o").is_file(),
    "expected object file next to executable"
  );
  assert!(
    exe_path.with_extension("ll").is_file(),
    "expected .ll file next to executable"
  );

  let mut child = Command::new(&exe_path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let Some(status) = child.wait_timeout(Duration::from_secs(5)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("compiled executable timed out");
  };

  let mut stdout = String::new();
  child
    .stdout
    .take()
    .unwrap()
    .read_to_string(&mut stdout)
    .unwrap();

  assert!(status.success());
  assert_eq!(stdout, "native-js aot ok\n");
}
