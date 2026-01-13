use std::process::Command;

#[test]
fn help_mentions_rayon_threads_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_ui_perf_smoke"))
    .arg("--help")
    .output()
    .expect("run ui_perf_smoke --help");

  assert!(output.status.success(), "--help should exit successfully");

  // clap writes help to stdout; keep stderr for compatibility with older parsers.
  let help = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout)
  } else {
    String::from_utf8_lossy(&output.stderr)
  };

  assert!(
    help.contains("--rayon-threads"),
    "help should mention --rayon-threads; got:\n{}",
    help
  );
}

