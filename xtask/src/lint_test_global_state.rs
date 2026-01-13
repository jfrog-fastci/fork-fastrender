use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const BASELINE_PATH: &str = "tools/lint_test_global_state_baseline.json";
const ALLOWLIST_ENV_GUARD_PATH: &str = "tests/common/global_state.rs";

/// Fail CI if new process-global state mutations are introduced inside `tests/`.
///
/// Integration tests are consolidated into a shared binary, so anything that mutates process-wide
/// state (environment variables, current working directory, global Rayon pool, stage listeners)
/// makes the suite flaky under parallel execution.
#[derive(Args, Debug, Clone, Copy)]
pub struct LintTestGlobalStateArgs {
  /// Rewrite the committed baseline file with the current set of violations.
  ///
  /// This should only be used when intentionally adjusting what is allowed (for example, after
  /// removing existing violations). CI should always run without this flag.
  #[arg(long)]
  pub update_baseline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
  EnvSetVar,
  EnvRemoveVar,
  EnvSetCurrentDir,
  RayonBuildGlobal,
  SetStageListener,
}

impl ViolationKind {
  fn label(self) -> &'static str {
    match self {
      ViolationKind::EnvSetVar => "std::env::set_var",
      ViolationKind::EnvRemoveVar => "std::env::remove_var",
      ViolationKind::EnvSetCurrentDir => "std::env::set_current_dir",
      ViolationKind::RayonBuildGlobal => "rayon build_global",
      ViolationKind::SetStageListener => "set_stage_listener",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
  pub path: PathBuf,
  pub line: usize,
  pub kind: ViolationKind,
  pub line_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BaselineKey {
  path: String,
  kind: ViolationKind,
  line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaselineEntry {
  path: String,
  kind: ViolationKind,
  line: String,
  count: usize,
}

pub fn run_lint_test_global_state(repo_root: &Path, args: LintTestGlobalStateArgs) -> Result<()> {
  let violations = lint_repo(repo_root)?;

  if args.update_baseline {
    write_baseline(repo_root, &violations)?;
    println!(
      "✓ lint-test-global-state: baseline updated at {BASELINE_PATH} ({} recorded violation(s))",
      violations.len()
    );
    return Ok(());
  }

  let mut baseline = load_baseline(repo_root)?;
  let mut new_violations = Vec::new();

  for violation in violations {
    let key = BaselineKey {
      path: normalize_repo_rel_path(&violation.path),
      kind: violation.kind,
      line: violation.line_text.trim().to_string(),
    };
    match baseline.get_mut(&key) {
      Some(remaining) if *remaining > 0 => {
        *remaining -= 1;
      }
      _ => new_violations.push(violation),
    }
  }

  if new_violations.is_empty() {
    println!("✓ lint-test-global-state: no new violations found");
    return Ok(());
  }

  eprintln!(
    "lint-test-global-state: found {} new process-global state mutation(s) in tests/\n",
    new_violations.len()
  );
  for v in &new_violations {
    eprintln!(
      "{}:{}: {}\n  {}\n",
      v.path.display(),
      v.line,
      v.kind.label(),
      v.line_text.trim_end()
    );
  }

  bail!(
    "lint-test-global-state failed: avoid introducing new process-global mutations in tests.\n\
     \n\
     Why this matters:\n\
       Integration tests now share a single process, so changing env vars / cwd / Rayon global pool\n\
       / stage listeners can race with other tests.\n\
     \n\
     Suggested fixes:\n\
       - Prefer runtime knobs over env vars (e.g. `FontConfig::bundled_only()` instead of `FASTR_USE_BUNDLED_FONTS`).\n\
       - If an env var is unavoidable, centralize it via an RAII guard + global lock (see `tests/common/global_state.rs`).\n\
       - Avoid `std::env::set_current_dir`; pass explicit paths instead.\n\
       - Avoid `.build_global()`; use per-test pools or shared initialization.\n\
       - Avoid installing global stage listeners; pass listeners explicitly where possible."
  );
}

pub fn lint_repo(repo_root: &Path) -> Result<Vec<Violation>> {
  let tests_root = repo_root.join("tests");
  lint_dir(repo_root, &tests_root)
}

pub fn lint_dir(repo_root: &Path, dir: &Path) -> Result<Vec<Violation>> {
  let mut violations = Vec::new();

  for entry in WalkDir::new(dir)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
  {
    if entry.path().extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }

    let source = fs::read_to_string(entry.path())
      .with_context(|| format!("read {}", entry.path().display()))?;
    violations.extend(lint_source(entry.path(), &source));
  }

  for violation in &mut violations {
    if let Ok(rel) = violation.path.strip_prefix(repo_root) {
      violation.path = rel.to_path_buf();
    }
  }

  violations.retain(|violation| !is_allowlisted(violation));
  violations.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
  Ok(violations)
}

fn is_allowlisted(violation: &Violation) -> bool {
  let allow_env_guard = matches!(
    violation.kind,
    ViolationKind::EnvSetVar | ViolationKind::EnvRemoveVar
  );
  if !allow_env_guard {
    return false;
  }

  normalize_repo_rel_path(&violation.path) == ALLOWLIST_ENV_GUARD_PATH
}

pub fn lint_source(path: &Path, source: &str) -> Vec<Violation> {
  let bytes = source.as_bytes();
  let lines: Vec<&str> = source.lines().collect();
  let mut violations = Vec::new();

  let mut i = 0usize;
  let mut line = 1usize;

  let mut in_line_comment = false;
  let mut block_comment_depth = 0usize;

  while i < bytes.len() {
    let b = bytes[i];

    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
        line += 1;
      }
      i += 1;
      continue;
    }

    if block_comment_depth > 0 {
      if bytes.get(i..i + 2) == Some(b"/*") {
        block_comment_depth += 1;
        i += 2;
        continue;
      }
      if bytes.get(i..i + 2) == Some(b"*/") {
        block_comment_depth = block_comment_depth.saturating_sub(1);
        i += 2;
        continue;
      }
      if b == b'\n' {
        line += 1;
      }
      i += 1;
      continue;
    }

    if b == b'\n' {
      line += 1;
      i += 1;
      continue;
    }

    if bytes.get(i..i + 2) == Some(b"//") {
      in_line_comment = true;
      i += 2;
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"/*") {
      block_comment_depth += 1;
      i += 2;
      continue;
    }

    if let Some((end_after, _prefix_len)) = skip_raw_string(bytes, i) {
      line += count_newlines(&bytes[i..end_after]);
      i = end_after;
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"b\"") {
      let end = skip_string(bytes, i, 2);
      line += count_newlines(&bytes[i..end]);
      i = end;
      continue;
    }
    if b == b'"' {
      let end = skip_string(bytes, i, 1);
      line += count_newlines(&bytes[i..end]);
      i = end;
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"b'") {
      if let Some(end_after) = skip_char_literal(bytes, i + 1) {
        line += count_newlines(&bytes[i..end_after]);
        i = end_after;
        continue;
      }
    }
    if b == b'\'' {
      if let Some(end_after) = skip_char_literal(bytes, i) {
        line += count_newlines(&bytes[i..end_after]);
        i = end_after;
        continue;
      }
    }

    let mut record = |kind: ViolationKind| {
      if line == 0 || line > lines.len() {
        return;
      }
      let line_text = lines.get(line - 1).copied().unwrap_or_default();
      violations.push(Violation {
        path: path.to_path_buf(),
        line,
        kind,
        line_text: line_text.to_string(),
      });
    };

    for (needle, kind) in [
      (b"std::env::set_var" as &[u8], ViolationKind::EnvSetVar),
      (b"std::env::remove_var", ViolationKind::EnvRemoveVar),
      (
        b"std::env::set_current_dir",
        ViolationKind::EnvSetCurrentDir,
      ),
      (b".build_global", ViolationKind::RayonBuildGlobal),
    ] {
      if starts_with_token(bytes, i, needle) {
        let after = i + needle.len();
        let j = skip_ws_and_comments(bytes, after);
        if bytes.get(j) == Some(&b'(') {
          record(kind);
        }
      }
    }

    {
      let needle = b"set_stage_listener";
      if starts_with_token(bytes, i, needle) {
        if i > 0 && is_ident_continue(bytes[i - 1]) {
          i += 1;
          continue;
        }
        let after = i + needle.len();
        if bytes
          .get(after)
          .is_some_and(|b| is_ident_continue(*b))
        {
          i += 1;
          continue;
        }

        let j = skip_ws_and_comments(bytes, after);
        if bytes.get(j) == Some(&b'(') {
          record(ViolationKind::SetStageListener);
        }
      }
    }

    i += 1;
  }

  violations
}

fn load_baseline(repo_root: &Path) -> Result<HashMap<BaselineKey, usize>> {
  let path = repo_root.join(BASELINE_PATH);
  if !path.exists() {
    bail!(
      "missing {BASELINE_PATH}. Run `bash scripts/cargo_agent.sh xtask lint-test-global-state --update-baseline` to generate it."
    );
  }
  let raw =
    fs::read_to_string(&path).with_context(|| format!("read baseline {}", path.display()))?;
  let entries: Vec<BaselineEntry> =
    serde_json::from_str(&raw).context("parse lint-test-global-state baseline JSON")?;
  let mut out = HashMap::new();
  for entry in entries {
    let key = BaselineKey {
      path: normalize_baseline_path(&entry.path),
      kind: entry.kind,
      line: entry.line,
    };
    *out.entry(key).or_insert(0) += entry.count;
  }
  Ok(out)
}

fn write_baseline(repo_root: &Path, violations: &[Violation]) -> Result<()> {
  let mut counts: HashMap<BaselineKey, usize> = HashMap::new();
  for violation in violations {
    let key = BaselineKey {
      path: normalize_repo_rel_path(&violation.path),
      kind: violation.kind,
      line: violation.line_text.trim().to_string(),
    };
    *counts.entry(key).or_insert(0) += 1;
  }

  let mut entries: Vec<BaselineEntry> = counts
    .into_iter()
    .map(|(key, count)| BaselineEntry {
      path: key.path,
      kind: key.kind,
      line: key.line,
      count,
    })
    .collect();
  entries.sort_by(|a, b| {
    a.path
      .cmp(&b.path)
      .then_with(|| a.kind.label().cmp(b.kind.label()))
      .then_with(|| a.line.cmp(&b.line))
  });

  let path = repo_root.join(BASELINE_PATH);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create baseline dir {}", parent.display()))?;
  }
  let json = serde_json::to_string_pretty(&entries).context("serialize baseline JSON")?;
  fs::write(&path, format!("{json}\n"))
    .with_context(|| format!("write baseline {}", path.display()))?;
  Ok(())
}

fn normalize_baseline_path(path: &str) -> String {
  path
    .trim_start_matches("./")
    .trim_start_matches(".\\")
    .replace('\\', "/")
}

fn normalize_repo_rel_path(path: &Path) -> String {
  let mut out = String::new();
  for part in path.iter() {
    if !out.is_empty() {
      out.push('/');
    }
    out.push_str(&part.to_string_lossy());
  }
  out
}

fn count_newlines(bytes: &[u8]) -> usize {
  bytes.iter().filter(|&&b| b == b'\n').count()
}

fn is_ident_continue(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_'
}

fn starts_with_token(bytes: &[u8], idx: usize, token: &[u8]) -> bool {
  bytes.get(idx..idx + token.len()) == Some(token)
}

fn skip_ws_and_comments(bytes: &[u8], mut idx: usize) -> usize {
  loop {
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
      idx += 1;
    }

    if bytes.get(idx..idx + 2) == Some(b"//") {
      idx += 2;
      while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
      }
      continue;
    }

    if bytes.get(idx..idx + 2) == Some(b"/*") {
      idx += 2;
      let mut depth = 1usize;
      while idx < bytes.len() && depth > 0 {
        if bytes.get(idx..idx + 2) == Some(b"/*") {
          depth += 1;
          idx += 2;
          continue;
        }
        if bytes.get(idx..idx + 2) == Some(b"*/") {
          depth = depth.saturating_sub(1);
          idx += 2;
          continue;
        }
        idx += 1;
      }
      continue;
    }

    break;
  }

  idx
}

fn skip_raw_string(bytes: &[u8], idx: usize) -> Option<(usize, usize)> {
  let (prefix_len, mut i) = if bytes.get(idx..idx + 2) == Some(b"br") {
    (2usize, idx + 2)
  } else if bytes.get(idx) == Some(&b'r') {
    (1usize, idx + 1)
  } else {
    return None;
  };

  let mut hashes = 0usize;
  while bytes.get(i) == Some(&b'#') {
    hashes += 1;
    i += 1;
  }
  if bytes.get(i) != Some(&b'"') {
    return None;
  }
  i += 1;

  while i < bytes.len() {
    if bytes[i] == b'"' {
      if hashes == 0 {
        return Some((i + 1, prefix_len));
      }
      if bytes
        .get(i + 1..i + 1 + hashes)
        .is_some_and(|tail| tail.iter().all(|c| *c == b'#'))
      {
        return Some((i + 1 + hashes, prefix_len));
      }
    }
    i += 1;
  }
  None
}

fn skip_string(bytes: &[u8], idx: usize, opening_len: usize) -> usize {
  let mut i = idx + opening_len;
  let mut escape = false;
  while i < bytes.len() {
    let b = bytes[i];
    if escape {
      escape = false;
      i += 1;
      continue;
    }
    if b == b'\\' {
      escape = true;
      i += 1;
      continue;
    }
    if b == b'"' {
      return i + 1;
    }
    i += 1;
  }
  bytes.len()
}

fn skip_char_literal(bytes: &[u8], idx: usize) -> Option<usize> {
  let bytes = bytes.get(idx..)?;
  if bytes.get(0) != Some(&b'\'') {
    return None;
  }

  let mut i = 1usize;
  let mut escape = false;
  while i < bytes.len() {
    let b = bytes[i];
    if escape {
      escape = false;
      i += 1;
      continue;
    }
    if b == b'\\' {
      escape = true;
      i += 1;
      continue;
    }
    if b == b'\n' {
      return None;
    }
    if b == b'\'' {
      return Some(idx + i + 1);
    }
    i += 1;
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::Path;

  #[test]
  fn flags_mutations_but_ignores_comments_and_strings() {
    let src = r#"
use std::env::set_var;

fn demo() {
  // std::env::set_var("A", "B");
  /* std::env::remove_var("A"); */
  let s = "std::env::set_current_dir(";
  std::env::set_var("A", "B");
  std::env::remove_var("A");
  std::env::set_current_dir("/tmp");
  rayon::ThreadPoolBuilder::new().build_global().unwrap();
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert_eq!(
      violations
        .iter()
        .map(|v| (v.kind, v.line))
        .collect::<Vec<_>>(),
      vec![
        (ViolationKind::EnvSetVar, 8),
        (ViolationKind::EnvRemoveVar, 9),
        (ViolationKind::EnvSetCurrentDir, 10),
        (ViolationKind::RayonBuildGlobal, 11),
      ],
      "unexpected violations: {violations:#?}"
    );
  }

  #[test]
  fn flags_set_stage_listener_but_not_ident_substrings() {
    let src = r#"
fn demo() {
  reset_stage_listener();
  set_stage_listener(|| {});
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert_eq!(violations.len(), 1, "expected one violation: {violations:#?}");
    assert_eq!(violations[0].kind, ViolationKind::SetStageListener);
    assert_eq!(violations[0].line, 4);
  }

  #[test]
  fn skip_ws_and_comments_treats_comments_like_whitespace() {
    let src = r#"
fn demo() {
  std::env::set_var/*comment*/("A", "B");
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].kind, ViolationKind::EnvSetVar);
  }
}
