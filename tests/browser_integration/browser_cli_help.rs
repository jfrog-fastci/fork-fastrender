#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_help_exits_successfully_without_startup_logs() {
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--help")
    .output()
    .expect("spawn browser --help");

  assert!(
    output.status.success(),
    "browser --help exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  // clap writes help to stdout; keep stderr for compatibility with older parsers
  let help = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout)
  } else {
    String::from_utf8_lossy(&output.stderr)
  };
  assert!(
    help.contains("Usage:"),
    "expected help usage in output, got:\n{help}"
  );
  assert!(
    help.contains("Supported schemes:"),
    "expected help to mention supported schemes, got:\n{help}"
  );
  for flag in [
    "--restore",
    "--no-restore",
    "--mem-limit-mb",
    "--power-preference",
    "--force-fallback-adapter",
    "--wgpu-backends",
    "--headless-smoke",
    "--exit-immediately",
  ] {
    assert!(
      help.contains(flag),
      "expected help to mention {flag}, got:\n{help}"
    );
  }

  assert!(
    !help.contains("FASTR_BROWSER_MEM_LIMIT_MB:"),
    "expected --help to exit before startup/mem-limit logging, got:\n{help}"
  );
}
