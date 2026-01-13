use std::process::Command;

#[cfg(target_os = "linux")]
#[test]
fn sandbox_probe_seccomp_all_smoke() {
  let output = Command::new(env!("CARGO_BIN_EXE_sandbox_probe"))
    .args(["--mode", "seccomp", "--probe", "all"])
    .output()
    .expect("run sandbox_probe");

  assert!(
    output.status.success(),
    "sandbox_probe should exit 0 (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
