use regex::Regex;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

#[test]
fn xtask_source_does_not_contain_raw_cargo_run_or_xtask_hints() {
  // `cargo run` / `cargo xtask` are easy to copy/paste from help/error output and will bypass the
  // repo's safety wrappers. Keep the check simple and disallow them anywhere in xtask sources.
  let raw_run_or_xtask = Regex::new(r"\bcargo\s+(run|xtask)\b").expect("valid regex");

  let xtask_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let src_root = xtask_root.join("src");

  let mut offenders: Vec<String> = Vec::new();
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
    for (idx, line) in contents.lines().enumerate() {
      if raw_run_or_xtask.is_match(line) {
        offenders.push(format!(
          "{}:{}: raw cargo run/xtask mention: {}",
          entry.path().display(),
          idx + 1,
          line.trim()
        ));
      }
    }
  }

  assert!(
    offenders.is_empty(),
    "Found raw `cargo run` / `cargo xtask` mentions in xtask sources.\n\
     Use wrapper-safe invocations instead, e.g. `bash scripts/cargo_agent.sh run ...` or\n\
     `bash scripts/cargo_agent.sh xtask ...`.\n\n{}",
    offenders.join("\n")
  );
}
