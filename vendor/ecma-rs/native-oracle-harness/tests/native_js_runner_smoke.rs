#![cfg(feature = "native-js-runner")]

use native_oracle_harness::{
  NativeJsRunner, NativeRunner, NativeRunner2, RunOutcome, VmJsOracleRunner,
};
use std::process::{Command, Stdio};

fn cmd_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn llvm_toolchain_available() -> bool {
  // `native-js` executable emission uses:
  // - clang as the linker driver
  // - ld.lld via `clang -fuse-ld=lld{,-18}`
  // - llvm-objcopy to relocate stackmaps into a RELRO-friendly section when using lld
  (cmd_works("clang-18") || cmd_works("clang"))
    && (cmd_works("ld.lld-18") || cmd_works("ld.lld"))
    && (cmd_works("llvm-objcopy-18") || cmd_works("llvm-objcopy"))
    && native_js::link::find_runtime_native_staticlib()
      .is_some_and(|p| p.is_file())
}

#[test]
#[cfg(target_os = "linux")]
fn native_js_runner_smoke() {
  if !llvm_toolchain_available() {
    eprintln!("skipping native-js runner smoke test: LLVM toolchain not found in PATH");
    return;
  }

  let runner = NativeJsRunner::new();
  let oracle = VmJsOracleRunner::new();

  let out = NativeRunner::compile_and_run(&runner, "console.log(1 + 2 * 3);")
    .expect("compile_and_run arithmetic");
  assert_eq!(out, "7");
  let oracle_out = NativeRunner::compile_and_run(&oracle, "console.log(1 + 2 * 3);")
    .expect("oracle arithmetic");
  assert_eq!(oracle_out, "7");

  let out = NativeRunner2::compile_and_run(&runner, "console.log(true);");
  match out {
    RunOutcome::Ok { value, stdout, .. } => {
      assert_eq!(value, "undefined");
      assert_eq!(stdout, "true");
    }
    other => panic!("expected Ok outcome, got {other:?}"),
  }

  let oracle_out = NativeRunner::compile_and_run(&oracle, "console.log(true);").expect("oracle boolean");
  assert_eq!(oracle_out, "true");

  // Regression: ensure we preserve trailing spaces (only the trailing newline is stripped).
  let out = NativeRunner::compile_and_run(&runner, r#"console.log("x ");"#).expect("compile_and_run trailing space");
  assert_eq!(out, "x ");
  let oracle_out =
    NativeRunner::compile_and_run(&oracle, r#"console.log("x ");"#).expect("oracle trailing space");
  assert_eq!(oracle_out, "x ");
}
