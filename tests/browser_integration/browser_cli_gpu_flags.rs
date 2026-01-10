#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

fn run_browser(args: &[&str]) -> std::process::Output {
  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .args(args)
    .output()
    .expect("spawn browser")
}

fn assert_browser_ok(output: &std::process::Output) {
  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );
}

fn assert_browser_clap_error(output: &std::process::Output) {
  // clap uses exit code 2 for argument parsing errors.
  assert_eq!(
    output.status.code(),
    Some(2),
    "expected clap exit code 2, got {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );
}

#[test]
fn browser_accepts_gpu_flag_values() {
  let _lock = super::stage_listener_test_lock();

  for pref in ["high", "low", "none"] {
    let output = run_browser(&["--exit-immediately", "--power-preference", pref]);
    assert_browser_ok(&output);
  }

  let output = run_browser(&[
    "--exit-immediately",
    "--force-fallback-adapter",
    "--power-preference",
    "high",
  ]);
  assert_browser_ok(&output);

  for backends in ["vulkan", "gl", "vulkan,gl"] {
    let output = run_browser(&["--exit-immediately", "--wgpu-backends", backends]);
    assert_browser_ok(&output);
  }
}

#[test]
fn browser_rejects_invalid_gpu_flag_values() {
  let _lock = super::stage_listener_test_lock();

  let output = run_browser(&["--exit-immediately", "--power-preference", "potato"]);
  assert_browser_clap_error(&output);
  let combined = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );
  assert!(
    combined.contains("invalid value"),
    "expected clap invalid value error, got:\n{combined}"
  );

  let output = run_browser(&["--exit-immediately", "--wgpu-backends", "potato"]);
  assert_browser_clap_error(&output);
}
