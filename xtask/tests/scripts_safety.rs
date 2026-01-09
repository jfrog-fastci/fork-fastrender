use std::fs;
use std::path::PathBuf;

use regex::Regex;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn heredoc_delimiter(line: &str) -> Option<String> {
  // Match common heredoc forms:
  //   cat <<EOF
  //   cat <<'EOF'
  //   python3 - <<'PY'
  //   cat <<-EOF
  //
  // We treat heredoc bodies as documentation/embedded scripts and do not scan them for `cargo ...`
  // invocations.
  static HEREDOC_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
  let re = HEREDOC_RE.get_or_init(|| {
    // Note: `regex` crate does not support backreferences, so we match quoted and unquoted variants
    // explicitly.
    Regex::new(r#"<<-?\s*(?:'([A-Za-z0-9_]+)'|"([A-Za-z0-9_]+)"|([A-Za-z0-9_]+))"#).unwrap()
  });
  re.captures(line)
    .and_then(|caps| {
      caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().to_string())
    })
}

#[test]
fn scripts_do_not_invoke_raw_cargo() {
  let scripts_dir = repo_root().join("scripts");

  let raw_cargo =
    Regex::new(r"\bcargo\s+(build|run|test|check|clippy|xtask)\b").expect("compile regex");

  let mut violations = Vec::new();

  for entry in fs::read_dir(&scripts_dir).expect("list scripts/") {
    let entry = entry.expect("read dir entry");
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("sh") {
      continue;
    }

    let contents =
      fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut heredoc_end: Option<String> = None;

    for (idx, line) in contents.lines().enumerate() {
      let line_no = idx + 1;

      if let Some(end) = heredoc_end.as_ref() {
        if line.trim() == end {
          heredoc_end = None;
        }
        continue;
      }

      let trimmed = line.trim_start();
      if trimmed.starts_with('#') {
        continue;
      }

      if let Some(end) = heredoc_delimiter(line) {
        heredoc_end = Some(end);
        continue;
      }

      if raw_cargo.is_match(line) {
        let file = path.strip_prefix(&scripts_dir).unwrap_or(&path);
        violations.push(format!(
          "{}:{}: {line}",
          file.display(),
          line_no,
          line = line.trim_end()
        ));
      }
    }
  }

  if !violations.is_empty() {
    panic!(
      "found raw `cargo <subcommand>` invocations in scripts/*.sh; use scripts/cargo_agent.sh instead:\n{}",
      violations.join("\n")
    );
  }
}
