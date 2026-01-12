#![cfg(feature = "native-js-runner")]

use native_oracle_harness::{NativeJsRunner, NativeRunner, NativeRunner2, RunOutcome};
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
}

#[test]
#[cfg(target_os = "linux")]
fn native_js_runner_smoke() {
  if !llvm_toolchain_available() {
    eprintln!("skipping native-js runner smoke test: LLVM toolchain not found in PATH");
    return;
  }

  let runner = NativeJsRunner::new();

  let out = NativeRunner::compile_and_run(&runner, "console.log(1 + 2 * 3);")
    .expect("compile_and_run arithmetic");
  assert_eq!(out, "7");

  let out = NativeRunner2::compile_and_run(&runner, "console.log(true);");
  match out {
    RunOutcome::Ok { value, stdout, .. } => {
      assert_eq!(value, "undefined");
      assert_eq!(stdout, "true");
    }
    other => panic!("expected Ok outcome, got {other:?}"),
  }
}
