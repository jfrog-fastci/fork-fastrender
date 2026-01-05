use std::process::Command;

#[test]
fn help_does_not_mention_cache_dir() {
  let output = Command::new(env!("CARGO_BIN_EXE_fetch_pages"))
    .arg("--help")
    .output()
    .expect("run fetch_pages --help");

  assert!(output.status.success(), "--help should exit successfully");

  // clap writes help to stdout; keep stderr for compatibility with older parsers
  let help = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout)
  } else {
    String::from_utf8_lossy(&output.stderr)
  };

  assert!(
    !help.contains("--cache-dir"),
    "fetch_pages should not expose the disk-backed subresource cache flag; got:\n{help}"
  );
}
