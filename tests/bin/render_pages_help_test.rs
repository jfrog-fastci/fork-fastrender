use std::process::Command;

#[test]
fn render_pages_help_mentions_memory_guardrails() {
  let output = Command::new(env!("CARGO_BIN_EXE_render_pages"))
    .arg("--help")
    .output()
    .expect("run render_pages --help");

  assert!(output.status.success(), "--help should exit successfully");

  let help = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout)
  } else {
    String::from_utf8_lossy(&output.stderr)
  };

  assert!(
    help.contains("--mem-limit-mb"),
    "help should mention --mem-limit-mb; got:\n{}",
    help
  );
  assert!(
    help.contains("--stage-mem-budget-mb"),
    "help should mention --stage-mem-budget-mb; got:\n{}",
    help
  );
}
