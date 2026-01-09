use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

#[test]
fn dry_run_prints_expected_plan() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--fixture", "example.com", "--dry-run"])
    .output()
    .expect("run cargo xtask page-loop --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("page-loop plan:"),
    "expected plan header; got:\n{stdout}"
  );
  assert!(
    stdout.contains("fixture: example.com"),
    "expected fixture name in plan; got:\n{stdout}"
  );
  assert!(
    stdout.contains("scripts/cargo_agent.sh run --release --bin render_fixtures"),
    "expected render_fixtures command to be present; got:\n{stdout}"
  );
  assert!(
    stdout.contains("target/page_loop") && stdout.contains("example.com") && stdout.contains("fastrender"),
    "expected output path to mention target/page_loop/<fixture>/fastrender; got:\n{stdout}"
  );
}

