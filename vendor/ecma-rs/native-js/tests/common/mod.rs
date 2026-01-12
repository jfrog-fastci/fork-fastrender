use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

pub fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(cand);
    }
  }
  None
}

pub fn clang_and_runtime_native() -> Option<(&'static str, PathBuf)> {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found");
    return None;
  };
  let Some(runtime_native_a) = native_js::link::find_runtime_native_staticlib() else {
    eprintln!("skipping: runtime-native staticlib not found");
    return None;
  };
  Some((clang, runtime_native_a))
}

pub fn clang_link_ir_to_exe(
  clang: &str,
  ll_path: &Path,
  exe_path: &Path,
  runtime_native_a: &Path,
) -> ExitStatus {
  let mut cmd = Command::new(clang);
  cmd
    .arg("-Wno-override-module")
    .arg("-x")
    .arg("ir")
    .arg(ll_path)
    // Reset language so archive inputs are not treated as IR.
    .arg("-x")
    .arg("none")
    .arg("-O0")
    .arg(runtime_native_a);
  // `runtime-native` is a Rust `staticlib`, so we need to explicitly provide the system libraries
  // rustc would normally inject when it is the final linker driver.
  if cfg!(target_os = "linux") {
    cmd.args(["-lpthread", "-ldl", "-lm", "-lrt"]);
  }
  cmd.arg("-o").arg(exe_path).status().expect("clang")
}

