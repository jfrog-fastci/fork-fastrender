use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live one directory below the repo root")
    .to_path_buf()
}

fn iter_markdown_files(root: &Path) -> impl Iterator<Item = PathBuf> {
  WalkDir::new(root)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| entry.into_path())
    .filter(|path| {
      path
        .extension()
        .is_some_and(|ext| ext == std::ffi::OsStr::new("md"))
    })
}

#[test]
fn docs_do_not_use_raw_cargo_in_code_fences() {
  // Enforce that copy/pastable command examples use the multi-agent wrappers:
  // - `bash scripts/cargo_agent.sh ...` for *all* cargo invocations (limits concurrency + caps RAM)
  // - `scripts/run_limited.sh --as 64G -- ...` when executing renderer binaries (OS-enforced cap)
  //
  // This test focuses on fenced code blocks, where most command examples live.

  let cargo_invocation = Regex::new(
    r#"^(?:[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|\S+)\s+)*cargo\b"#,
  )
  .expect("valid regex");

  // Also catch patterns like `scripts/run_limited.sh --as 64G -- cargo run ...`.
  let cargo_after_double_dash =
    Regex::new(r#"(?:^|\s)--\s+cargo\b"#).expect("valid regex");

  // Allow examples that are explicitly labelled as incorrect (e.g. "WRONG — DO NOT RUN").
  let allowed_marker = Regex::new(r"(?i)\b(wrong|forbidden)\b").expect("valid regex");

  let repo_root = repo_root();
  let mut violations: Vec<String> = Vec::new();

  let mut scan_roots = vec![repo_root.join("docs"), repo_root.join("instructions")];
  scan_roots.push(repo_root.join("AGENTS.md"));

  for root in scan_roots {
    if root.is_file() {
      scan_file(
        &root,
        &cargo_invocation,
        &cargo_after_double_dash,
        &allowed_marker,
        &mut violations,
      );
      continue;
    }
    if !root.is_dir() {
      continue;
    }
    for path in iter_markdown_files(&root) {
      scan_file(
        &path,
        &cargo_invocation,
        &cargo_after_double_dash,
        &allowed_marker,
        &mut violations,
      );
    }
  }

  if !violations.is_empty() {
    panic!(
      "Found raw `cargo` invocations in documentation code fences.\n\
       Use `bash scripts/cargo_agent.sh ...` for cargo commands, and wrap renderer runs with\n\
       `scripts/run_limited.sh --as 64G -- ...`.\n\n{}",
      violations.join("\n")
    );
  }
}

fn scan_file(
  path: &Path,
  cargo_invocation: &Regex,
  cargo_after_double_dash: &Regex,
  allowed_marker: &Regex,
  violations: &mut Vec<String>,
) {
  let content = fs::read_to_string(path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

  let mut in_fence = false;
  let mut recent_nonempty: Vec<String> = Vec::new();

  for (idx, line) in content.lines().enumerate() {
    let line_no = idx + 1;

    // Markdown fenced code blocks toggle on any "```" line (language tags are ignored).
    if line.trim_start().starts_with("```") {
      in_fence = !in_fence;
      recent_nonempty.clear();
      continue;
    }

    if !in_fence {
      continue;
    }

    let trimmed = line.trim_start();

     if cargo_invocation.is_match(trimmed) || cargo_after_double_dash.is_match(trimmed) {
       let has_marker = recent_nonempty
         .iter()
         .rev()
         .take(3)
         .any(|prev| allowed_marker.is_match(prev));

      if !has_marker {
        violations.push(format!(
          "{}:{}: raw cargo invocation: {}",
          path.display(),
          line_no,
          trimmed
        ));
      }
    }

    if !trimmed.is_empty() {
      recent_nonempty.push(trimmed.to_string());
      if recent_nonempty.len() > 8 {
        recent_nonempty.remove(0);
      }
    }
  }

  if in_fence {
    violations.push(format!(
      "{}: unterminated fenced code block (missing closing ```)",
      path.display()
    ));
  }
}
