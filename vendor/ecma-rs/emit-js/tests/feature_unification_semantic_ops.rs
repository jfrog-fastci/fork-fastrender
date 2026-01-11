use std::path::{Path, PathBuf};
use std::process::Command;

fn find_cargo_agent_script(start: &Path) -> PathBuf {
  let mut dir = start.to_path_buf();
  loop {
    let candidate = dir.join("scripts").join("cargo_agent.sh");
    if candidate.is_file() {
      return candidate;
    }
    if !dir.pop() {
      panic!("unable to locate scripts/cargo_agent.sh from {start:?}");
    }
  }
}

#[test]
fn feature_unification_semantic_ops_compiles() {
  if cfg!(windows) {
    return;
  }

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let ecma_rs_root = manifest_dir
    .parent()
    .expect("emit-js manifest dir should have a parent directory")
    .to_path_buf();
  let cargo_agent = find_cargo_agent_script(&manifest_dir);

  let target_dir = ecma_rs_root
    .join("target")
    .join("emit_js_feature_unification_semantic_ops_check");

  let output = Command::new("bash")
    .arg(cargo_agent)
    .arg("check")
    .arg("-p")
    .arg("emit-js")
    .arg("-p")
    .arg("typecheck-ts")
    .arg("--features")
    .arg("semantic-ops")
    .current_dir(ecma_rs_root)
    .env("CARGO_TARGET_DIR", target_dir)
    .output()
    .expect("run cargo check for feature unification graph");

  assert!(
    output.status.success(),
    "expected semantic-ops feature unification graph to compile.\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
}

