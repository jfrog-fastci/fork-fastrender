use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Marker for audited `panic!`/`todo!`/`unimplemented!` sites.
pub const ALLOW_PANIC_MARKER: &str = "fastrender-allow-panic";
/// Marker for audited `.unwrap()`/`.expect()` sites.
pub const ALLOW_UNWRAP_MARKER: &str = "fastrender-allow-unwrap";

const BASELINE_PATH: &str = "tools/no_panics_baseline.json";

#[derive(Args, Debug, Clone, Copy)]
pub struct LintNoPanicsArgs {
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
  Panic,
  Todo,
  Unimplemented,
  Unwrap,
  Expect,
}

impl ViolationKind {
  fn allow_marker(self) -> &'static str {
    match self {
      ViolationKind::Panic | ViolationKind::Todo | ViolationKind::Unimplemented => {
        ALLOW_PANIC_MARKER
      }
      ViolationKind::Unwrap | ViolationKind::Expect => ALLOW_UNWRAP_MARKER,
    }
  }

  fn description(self) -> &'static str {
    match self {
      ViolationKind::Panic => "panic! macro",
      ViolationKind::Todo => "todo! macro",
      ViolationKind::Unimplemented => "unimplemented! macro",
      ViolationKind::Unwrap => ".unwrap()",
      ViolationKind::Expect => ".expect()",
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
  /// Repository-relative path using `/` separators.
  ///
  /// We intentionally avoid `PathBuf` here because the baseline file is committed with `/`
  /// separators, but Windows `PathBuf` values (and `walkdir`) typically use `\`. Path equality is
  /// lexical, so normalizing to a stable string keeps the baseline portable across platforms.
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

pub fn run_lint_no_panics(repo_root: &Path, args: LintNoPanicsArgs) -> Result<()> {
  let violations = lint_repo(repo_root)?;

  if args.update_baseline {
    write_baseline(repo_root, &violations)?;
    println!(
      "✓ lint-no-panics: baseline updated at {BASELINE_PATH} ({} recorded violation(s))",
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
    println!("✓ lint-no-panics: no new violations found");
    return Ok(());
  }

  eprintln!(
    "lint-no-panics: found {} new violation(s) in src/ (excluding #[cfg(test)] code)\n",
    new_violations.len()
  );
  for v in &new_violations {
    eprintln!(
      "{}:{}: {} (allow with `// {}`)\n  {}\n",
      v.path.display(),
      v.line,
      v.kind.description(),
      v.kind.allow_marker(),
      v.line_text.trim_end()
    );
  }

  bail!(
    "lint-no-panics failed: avoid introducing new panics (or allow sparingly via {ALLOW_PANIC_MARKER}/{ALLOW_UNWRAP_MARKER})"
  );
}

pub fn lint_repo(repo_root: &Path) -> Result<Vec<Violation>> {
  let src_root = repo_root.join("src");
  lint_dir(repo_root, &src_root)
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

  violations.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
  Ok(violations)
}

fn load_baseline(repo_root: &Path) -> Result<HashMap<BaselineKey, usize>> {
  let path = repo_root.join(BASELINE_PATH);
  if !path.exists() {
    bail!(
      "missing {BASELINE_PATH}. Run `cargo xtask lint-no-panics --update-baseline` to generate it."
    );
  }
  let raw =
    fs::read_to_string(&path).with_context(|| format!("read baseline {}", path.display()))?;
  let entries: Vec<BaselineEntry> =
    serde_json::from_str(&raw).context("parse lint-no-panics baseline JSON")?;
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
      .then_with(|| a.kind.description().cmp(b.kind.description()))
      .then_with(|| a.line.cmp(&b.line))
  });

  let path = repo_root.join(BASELINE_PATH);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create baseline dir {}", parent.display()))?;
  }
  let json = serde_json::to_string_pretty(&entries).context("serialize baseline JSON")?;
  fs::write(&path, format!("{json}\n")).with_context(|| format!("write baseline {}", path.display()))?;
  Ok(())
}

fn normalize_baseline_path(path: &str) -> String {
  // Keep the committed baseline portable across platforms by ensuring forward slashes.
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

fn is_ident_continue(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_'
}

fn count_newlines(bytes: &[u8]) -> usize {
  bytes.iter().filter(|&&b| b == b'\n').count()
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

    return idx;
  }
}

fn attribute_range(bytes: &[u8], idx: usize) -> Option<(usize, usize, usize)> {
  let content_start = if bytes.get(idx..idx + 2) == Some(b"#[") {
    idx + 2
  } else if bytes.get(idx..idx + 3) == Some(b"#![") {
    idx + 3
  } else {
    return None;
  };

  let mut i = content_start;
  let mut block_comment_depth = 0usize;
  let mut in_line_comment = false;
  let mut in_string = false;
  let mut string_escape = false;
  let mut in_raw_string: Option<usize> = None;

  while i < bytes.len() {
    let b = bytes[i];

    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
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
      i += 1;
      continue;
    }

    if let Some(hashes) = in_raw_string {
      if b == b'"' {
        if hashes == 0 {
          in_raw_string = None;
          i += 1;
          continue;
        }
        if bytes
          .get(i + 1..i + 1 + hashes)
          .is_some_and(|tail| tail.iter().all(|c| *c == b'#'))
        {
          in_raw_string = None;
          i += 1 + hashes;
          continue;
        }
      }
      i += 1;
      continue;
    }

    if in_string {
      if string_escape {
        string_escape = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        string_escape = true;
        i += 1;
        continue;
      }
      if b == b'"' {
        in_string = false;
        i += 1;
        continue;
      }
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

    if b == b'r' {
      let mut j = i + 1;
      let mut hashes = 0usize;
      while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
      }
      if bytes.get(j) == Some(&b'"') {
        in_raw_string = Some(hashes);
        i = j + 1;
        continue;
      }
    }

    if b == b'"' {
      in_string = true;
      string_escape = false;
      i += 1;
      continue;
    }

    if b == b']' {
      let content_end = i;
      let end_after = i + 1;
      return Some((end_after, content_start, content_end));
    }

    i += 1;
  }

  None
}

fn parse_attribute<'a>(
  source: &'a str,
  bytes: &[u8],
  idx: usize,
) -> Option<(usize, &'a str)> {
  let (end_after, content_start, content_end) = attribute_range(bytes, idx)?;
  let content = source.get(content_start..content_end)?;
  Some((end_after, content))
}

fn skip_attribute(bytes: &[u8], idx: usize) -> Option<usize> {
  attribute_range(bytes, idx).map(|(end_after, _, _)| end_after)
}

#[derive(Debug, Clone)]
enum CfgExpr {
  Test,
  Var(String),
  All(Vec<CfgExpr>),
  Any(Vec<CfgExpr>),
  Not(Box<CfgExpr>),
}

impl CfgExpr {
  fn contains_test(&self) -> bool {
    match self {
      CfgExpr::Test => true,
      CfgExpr::Var(_) => false,
      CfgExpr::All(items) | CfgExpr::Any(items) => items.iter().any(|item| item.contains_test()),
      CfgExpr::Not(inner) => inner.contains_test(),
    }
  }
}

struct CfgParser<'a> {
  source: &'a str,
  bytes: &'a [u8],
  idx: usize,
}

impl<'a> CfgParser<'a> {
  fn new(source: &'a str) -> Self {
    Self {
      source,
      bytes: source.as_bytes(),
      idx: 0,
    }
  }

  fn is_eof(&self) -> bool {
    self.idx >= self.bytes.len()
  }

  fn skip_ws(&mut self) {
    while self.idx < self.bytes.len() && self.bytes[self.idx].is_ascii_whitespace() {
      self.idx += 1;
    }
  }

  fn consume_byte(&mut self, b: u8) -> bool {
    self.skip_ws();
    if self.bytes.get(self.idx) == Some(&b) {
      self.idx += 1;
      true
    } else {
      false
    }
  }

  fn parse_ident(&mut self) -> Option<String> {
    self.skip_ws();
    let start = self.idx;
    let first = *self.bytes.get(self.idx)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
      return None;
    }
    self.idx += 1;
    while self.idx < self.bytes.len() && is_ident_continue(self.bytes[self.idx]) {
      self.idx += 1;
    }
    Some(self.source[start..self.idx].to_string())
  }

  fn parse_value_token(&mut self) -> Option<String> {
    self.skip_ws();
    let start = self.idx;

    if let Some((end_after, _prefix_len)) = skip_raw_string(self.bytes, self.idx) {
      self.idx = end_after;
      return Some(self.source[start..end_after].to_string());
    }

    if self.bytes.get(self.idx..self.idx + 2) == Some(b"b\"") {
      let end = skip_string(self.bytes, self.idx, 2);
      self.idx = end;
      return Some(self.source[start..end].to_string());
    }

    if self.bytes.get(self.idx) == Some(&b'"') {
      let end = skip_string(self.bytes, self.idx, 1);
      self.idx = end;
      return Some(self.source[start..end].to_string());
    }

    // Numeric literal (rare in cfg, but accepted in meta items).
    if self.bytes.get(self.idx).is_some_and(|b| b.is_ascii_digit()) {
      self.idx += 1;
      while self.idx < self.bytes.len() && self.bytes[self.idx].is_ascii_digit() {
        self.idx += 1;
      }
      return Some(self.source[start..self.idx].to_string());
    }

    self.parse_ident()
  }

  fn parse_cfg_expr(&mut self) -> Option<CfgExpr> {
    let name = self.parse_ident()?;

    // Key/value meta item: `feature = "foo"`.
    if self.consume_byte(b'=') {
      let value = self.parse_value_token()?;
      return Some(CfgExpr::Var(format!("{name}={value}")));
    }

    // List meta item / operators: `all(...)`, `any(...)`, `not(...)`.
    if self.consume_byte(b'(') {
      let mut args = Vec::new();
      self.skip_ws();
      if self.consume_byte(b')') {
        return Some(match name.as_str() {
          "all" => CfgExpr::All(args),
          "any" => CfgExpr::Any(args),
          _ => CfgExpr::Var(format!("{name}()")),
        });
      }

      loop {
        let arg = self.parse_cfg_expr()?;
        args.push(arg);
        self.skip_ws();
        if self.consume_byte(b',') {
          if self.consume_byte(b')') {
            break;
          }
          continue;
        }
        if self.consume_byte(b')') {
          break;
        }
        return None;
      }

      return Some(match name.as_str() {
        "all" => CfgExpr::All(args),
        "any" => CfgExpr::Any(args),
        "not" => CfgExpr::Not(Box::new(args.into_iter().next()?)),
        _ => CfgExpr::Var(name),
      });
    }

    if name == "test" {
      return Some(CfgExpr::Test);
    }
    Some(CfgExpr::Var(name))
  }

  fn parse_cfg_attribute(&mut self) -> Option<CfgExpr> {
    let name = self.parse_ident()?;
    if name != "cfg" {
      return None;
    }
    if !self.consume_byte(b'(') {
      return None;
    }
    let expr = self.parse_cfg_expr()?;
    self.skip_ws();
    if !self.consume_byte(b')') {
      return None;
    }
    self.skip_ws();
    if !self.is_eof() {
      return None;
    }
    Some(expr)
  }
}

fn cfg_expr_is_satisfiable(expr: &CfgExpr, test_value: bool) -> bool {
  use std::collections::hash_map::Entry;

  fn collect(expr: &CfgExpr, vars: &mut HashMap<String, usize>) {
    match expr {
      CfgExpr::Test => {}
      CfgExpr::Var(name) => {
        let next_idx = vars.len();
        if let Entry::Vacant(entry) = vars.entry(name.clone()) {
          entry.insert(next_idx);
        }
      }
      CfgExpr::All(items) | CfgExpr::Any(items) => {
        for item in items {
          collect(item, vars);
        }
      }
      CfgExpr::Not(inner) => collect(inner, vars),
    }
  }

  fn eval(
    expr: &CfgExpr,
    test_value: bool,
    vars: &HashMap<String, usize>,
    assignment: &[bool],
  ) -> bool {
    match expr {
      CfgExpr::Test => test_value,
      CfgExpr::Var(name) => assignment[*vars.get(name).unwrap_or(&0)],
      CfgExpr::All(items) => items
        .iter()
        .all(|item| eval(item, test_value, vars, assignment)),
      CfgExpr::Any(items) => items
        .iter()
        .any(|item| eval(item, test_value, vars, assignment)),
      CfgExpr::Not(inner) => !eval(inner, test_value, vars, assignment),
    }
  }

  let mut vars = HashMap::new();
  collect(expr, &mut vars);
  let var_count = vars.len();
  // `cfg()` expressions should stay small; be conservative if something weird shows up.
  if var_count > 16 {
    return true;
  }

  let mut assignment = vec![false; var_count];
  for mask in 0..(1usize << var_count) {
    for (idx, slot) in assignment.iter_mut().enumerate() {
      *slot = (mask >> idx) & 1 == 1;
    }
    if eval(expr, test_value, &vars, &assignment) {
      return true;
    }
  }

  false
}

fn attribute_is_cfg_test(attr: &str) -> bool {
  let mut parser = CfgParser::new(attr);
  let Some(expr) = parser.parse_cfg_attribute() else {
    return false;
  };
  if !expr.contains_test() {
    return false;
  }

  // Treat `#[cfg(...)]` as test-only only when the cfg expression is unsatisfiable with
  // `test=false`. This handles `cfg(all(test, ...))` while avoiding `cfg(any(test, feature = ...))`.
  !cfg_expr_is_satisfiable(&expr, false)
}

fn starts_with_token(bytes: &[u8], idx: usize, token: &[u8]) -> bool {
  bytes.get(idx..idx + token.len()) == Some(token)
}

fn skip_char_literal(bytes: &[u8], idx: usize) -> Option<usize> {
  if bytes.get(idx) != Some(&b'\'') {
    return None;
  }
  let mut i = idx + 1;
  let b = *bytes.get(i)?;
  if b == b'\\' {
    i += 1;
    let esc = *bytes.get(i)?;
    i += 1;
    if esc == b'u' && bytes.get(i) == Some(&b'{') {
      i += 1;
      while let Some(&b) = bytes.get(i) {
        i += 1;
        if b == b'}' {
          break;
        }
      }
    }
  } else {
    // Consume a single UTF-8 codepoint (char literal payload). We only need to ensure we do not
    // treat delimiters inside char literals as syntax tokens.
    let ch = std::str::from_utf8(&bytes[i..]).ok()?.chars().next()?;
    i += ch.len_utf8();
  }

  if bytes.get(i) == Some(&b'\'') {
    Some(i + 1)
  } else {
    None
  }
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
  i += 1; // after opening quote

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
  let mut i = idx + opening_len; // after opening quote
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

fn skip_cfg_item(bytes: &[u8], start: usize) -> usize {
  let mut i = start;
  i = skip_ws_and_comments(bytes, i);

  // Skip additional attributes attached to the same item.
  loop {
    if bytes.get(i..i + 2) == Some(b"#[") || bytes.get(i..i + 3) == Some(b"#![") {
      // We can skip the attribute without parsing its content.
      if let Some(end_after) = skip_attribute(bytes, i) {
        i = end_after;
        i = skip_ws_and_comments(bytes, i);
        continue;
      }
    }
    break;
  }

  let mut brace_depth: i32 = 0;
  let mut paren_depth: i32 = 0;
  let mut bracket_depth: i32 = 0;
  let mut saw_top_level_brace = false;

  while i < bytes.len() {
    // Skip comments first.
    if bytes.get(i..i + 2) == Some(b"//") {
      i += 2;
      while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
      }
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"/*") {
      i += 2;
      let mut depth = 1usize;
      while i < bytes.len() && depth > 0 {
        if bytes.get(i..i + 2) == Some(b"/*") {
          depth += 1;
          i += 2;
          continue;
        }
        if bytes.get(i..i + 2) == Some(b"*/") {
          depth = depth.saturating_sub(1);
          i += 2;
          continue;
        }
        i += 1;
      }
      continue;
    }

    // Skip strings and chars so delimiter counting stays correct.
    if let Some((end_after, _prefix_len)) = skip_raw_string(bytes, i) {
      i = end_after;
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"b\"") {
      i = skip_string(bytes, i, 2);
      continue;
    }
    if bytes.get(i) == Some(&b'"') {
      i = skip_string(bytes, i, 1);
      continue;
    }
    if bytes.get(i..i + 2) == Some(b"b'") {
      if let Some(end_after) = skip_char_literal(bytes, i + 1) {
        i = end_after;
        continue;
      }
    }
    if bytes.get(i) == Some(&b'\'') {
      if let Some(end_after) = skip_char_literal(bytes, i) {
        i = end_after;
        continue;
      }
    }

    let b = bytes[i];
    match b {
      b'{' => {
        if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 {
          saw_top_level_brace = true;
        }
        brace_depth += 1;
        i += 1;
      }
      b'}' => {
        if brace_depth == 0 {
          return i;
        }
        brace_depth -= 1;
        i += 1;

        if saw_top_level_brace && brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 {
          let mut j = skip_ws_and_comments(bytes, i);
          if bytes.get(j) == Some(&b';') {
            j += 1;
            i = j;
          }

          // Handle `if ... { ... } else { ... }` blocks.
          j = skip_ws_and_comments(bytes, i);
          if bytes.get(j..j + 4) == Some(b"else")
            && !bytes
              .get(j + 4)
              .is_some_and(|b| is_ident_continue(*b))
          {
            i = j + 4;
            continue;
          }

          return i;
        }
      }
      b'(' => {
        paren_depth += 1;
        i += 1;
      }
      b')' => {
        if paren_depth == 0 {
          return i;
        }
        paren_depth -= 1;
        i += 1;
      }
      b'[' => {
        bracket_depth += 1;
        i += 1;
      }
      b']' => {
        if bracket_depth == 0 {
          return i;
        }
        bracket_depth -= 1;
        i += 1;
      }
      b';' => {
        if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 {
          return i + 1;
        }
        i += 1;
      }
      _ => i += 1,
    }
  }

  bytes.len()
}

pub fn lint_source(path: &Path, source: &str) -> Vec<Violation> {
  let bytes = source.as_bytes();
  let lines: Vec<&str> = source.lines().collect();

  let mut violations = Vec::new();
  let mut i = 0usize;
  let mut line = 1usize;

  let mut in_line_comment = false;
  let mut block_comment_depth = 0usize;
  let mut pending_cfg_test = false;

  while i < bytes.len() {
    if pending_cfg_test && !in_line_comment && block_comment_depth == 0 {
      let old = i;
      let end = skip_cfg_item(bytes, i);
      line += count_newlines(&bytes[old..end]);
      i = end;
      pending_cfg_test = false;
      continue;
    }

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

    // Skip attributes (and detect `#[cfg(test)]`).
    if bytes.get(i..i + 2) == Some(b"#[") || bytes.get(i..i + 3) == Some(b"#![") {
      if let Some((end_after, content)) = parse_attribute(source, bytes, i) {
        if attribute_is_cfg_test(content) {
          pending_cfg_test = true;
        }
        line += count_newlines(&bytes[i..end_after]);
        i = end_after;
        continue;
      }
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
        i = end_after;
        continue;
      }
    }
    if b == b'\'' {
      if let Some(end_after) = skip_char_literal(bytes, i) {
        i = end_after;
        continue;
      }
    }

    let mut record = |kind: ViolationKind| {
      if line == 0 || line > lines.len() {
        return;
      }
      let line_text = lines.get(line - 1).copied().unwrap_or_default();
      if line_text.contains(kind.allow_marker()) {
        return;
      }
      violations.push(Violation {
        path: path.to_path_buf(),
        line,
        kind,
        line_text: line_text.to_string(),
      });
    };

    // Macro invocations (panic/todo/unimplemented).
    for (token, kind) in [
      (b"panic" as &[u8], ViolationKind::Panic),
      (b"todo" as &[u8], ViolationKind::Todo),
      (b"unimplemented" as &[u8], ViolationKind::Unimplemented),
    ] {
      if starts_with_token(bytes, i, token) {
        if i > 0 && is_ident_continue(bytes[i - 1]) {
          continue;
        }
        let after = i + token.len();
        if bytes.get(after).is_some_and(|b| is_ident_continue(*b)) {
          continue;
        }
        let mut j = skip_ws_and_comments(bytes, after);
        if bytes.get(j) != Some(&b'!') {
          continue;
        }
        j += 1;
        j = skip_ws_and_comments(bytes, j);
        if bytes.get(j) == Some(&b'(') {
          record(kind);
        }
      }
    }

    // `.unwrap()` / `.expect()`.
    for (method, kind) in [(b"unwrap" as &[u8], ViolationKind::Unwrap), (b"expect", ViolationKind::Expect)] {
      if b == b'.' && starts_with_token(bytes, i + 1, method) {
        let after = i + 1 + method.len();
        if bytes.get(after).is_some_and(|b| is_ident_continue(*b)) {
          continue;
        }
        let j = skip_ws_and_comments(bytes, after);
        if bytes.get(j) == Some(&b'(') {
          record(kind);
        }
      }
    }

    i += 1;
  }

  violations
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn flags_production_panics_and_unwraps_but_ignores_cfg_test_and_comments() {
    let src = r#"
/// Doc comment should be ignored: `Some(1).unwrap()`.
pub fn demo() {
  let _ = Some(1).unwrap();
  // panic!("in comment");
  #[cfg(test)]
  {
    let _ = Some(1).unwrap();
    panic!("ok in cfg(test)");
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn allows_panics_in_tests() {
    panic!("expected");
  }
}

pub fn allowlisted() {
  let _ = Some(1).unwrap(); // fastrender-allow-unwrap
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert_eq!(violations.len(), 1, "expected exactly one violation: {violations:#?}");
    assert_eq!(violations[0].kind, ViolationKind::Unwrap);
    assert_eq!(violations[0].line, 4);
  }

  #[test]
  fn allows_inline_marker_for_panic_macros() {
    let src = r#"
pub fn demo() {
  panic!("boom"); // fastrender-allow-panic
  todo!("later"); // fastrender-allow-panic
  unimplemented!("later"); // fastrender-allow-panic
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert!(violations.is_empty(), "expected allow markers to suppress: {violations:#?}");
  }

  #[test]
  fn normalizes_baseline_paths_with_backslashes() {
    assert_eq!(normalize_baseline_path("src\\api.rs"), "src/api.rs");
    assert_eq!(normalize_baseline_path("./src/api.rs"), "src/api.rs");
    assert_eq!(normalize_baseline_path(".\\src\\api.rs"), "src/api.rs");
  }

  #[test]
  fn ignores_cfg_all_test_blocks() {
    let src = r#"
pub fn demo() {
  #[cfg(all(test, not(feature = "disk_cache")))]
  {
    let _ = Some(1).unwrap();
    panic!("boom");
  }
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert!(
      violations.is_empty(),
      "expected cfg(all(test, ...)) to be ignored: {violations:#?}"
    );
  }

  #[test]
  fn ignores_cfg_all_test_blocks_with_trailing_commas() {
    let src = r#"
pub fn demo() {
  #[cfg(all(
    test,
    not(feature = "disk_cache"),
  ))]
  {
    let _ = Some(1).unwrap();
    panic!("boom");
  }
}
"#;

    let violations = lint_source(Path::new("demo.rs"), src);
    assert!(
      violations.is_empty(),
      "expected trailing commas in cfg(all(test, ...)) to be ignored: {violations:#?}"
    );
  }
}
