use std::fs;
use std::path::Path;

use walkdir::WalkDir;

#[test]
fn xtask_does_not_spawn_raw_cargo() {
  let xtask_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let src_root = xtask_root.join("src");

  let mut offenders = Vec::new();
  for entry in WalkDir::new(&src_root) {
    let entry = entry.expect("walkdir entry");
    if !entry.file_type().is_file() {
      continue;
    }
    if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
      continue;
    }

    let contents = fs::read_to_string(entry.path())
      .unwrap_or_else(|_| panic!("read {}", entry.path().display()));
    if contents.contains(r#"Command::new("cargo")"#) {
      offenders.push(entry.path().display().to_string());
    }
  }

  assert!(
    offenders.is_empty(),
    "xtask should not spawn raw cargo (use scripts/cargo_agent.sh wrappers instead). Found in:\n{}",
    offenders.join("\n")
  );
}

