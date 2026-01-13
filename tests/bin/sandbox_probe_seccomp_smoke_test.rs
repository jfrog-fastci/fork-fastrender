use std::process::Command;

#[cfg(target_os = "linux")]
#[test]
fn sandbox_probe_seccomp_all_smoke() {
  let output = Command::new(env!("CARGO_BIN_EXE_sandbox_probe"))
    .args(["--mode", "seccomp", "--probe", "all"])
    // Enforce the sandbox for this security smoke test even if the developer has globally set
    // debug escape hatches in their shell environment.
    //
    // This keeps CI deterministic and ensures local runs exercise the real sandbox path unless the
    // test is explicitly modified.
    .env_remove("FASTR_DISABLE_RENDERER_SANDBOX")
    .env_remove("FASTR_RENDERER_SECCOMP")
    .env_remove("FASTR_RENDERER_LANDLOCK")
    .env_remove("FASTR_RENDERER_CLOSE_FDS")
    .output()
    .expect("run sandbox_probe");

  assert!(
    output.status.success(),
    "sandbox_probe should exit 0 (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
