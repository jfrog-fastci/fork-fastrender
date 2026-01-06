use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use regex::Regex;
use walkdir::WalkDir;

/// `DebugInfo` is diagnostics-only (often `None` in release builds), so layout/parsing semantics
/// must never depend on it.
///
/// This test is intentionally lightweight and pattern-based: it exists to catch obvious
/// regressions like semantic fallbacks to `debug_info.tag_name` or span metadata.
#[test]
fn debug_info_semantics_guard() {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let src_dir = manifest_dir.join("src");

  let forbidden = [
    (
      "semantic fallback to debug_info.tag_name",
      Regex::new(r"(?s)\.debug_info\b.{0,500}?\.tag_name(?:[^\(]|$)").expect("valid regex"),
    ),
    (
      "semantic fallback to debug_info spans",
      Regex::new(r"(?s)\.debug_info\b.{0,500}?\.(colspan|rowspan|column_span)(?:[^\(]|$)")
        .expect("valid regex"),
    ),
    (
      "binding debug_info for non-diagnostic control flow",
      Regex::new(
        r"(?s)\bif\s+let\s+Some\s*\(\s*ref\s+debug_info\s*\)\s*=\s*[^;]{0,500}\.debug_info\b",
      )
      .expect("valid regex"),
    ),
    (
      "hash_components() usage (DebugInfo must not influence cache keys)",
      Regex::new(r"\.hash_components\s*\(").expect("valid regex"),
    ),
  ];

  let mut violations = Vec::new();

  for entry in WalkDir::new(&src_dir).into_iter().filter_map(Result::ok) {
    if !entry.file_type().is_file() {
      continue;
    }
    if entry.path().extension() != Some(OsStr::new("rs")) {
      continue;
    }

    let rel_path = entry
      .path()
      .strip_prefix(&manifest_dir)
      .unwrap_or(entry.path());
    if rel_path.starts_with("src/debug") || rel_path.starts_with("src/bin") {
      continue;
    }

    let Ok(contents) = fs::read_to_string(entry.path()) else {
      continue;
    };

    for (label, pattern) in &forbidden {
      for mat in pattern.find_iter(&contents) {
        let line_idx = contents[..mat.start()].lines().count();
        let line = contents
          .lines()
          .nth(line_idx)
          .unwrap_or_default()
          .trim();
        violations.push(format!(
          "{}:{}: {}: {}",
          rel_path.display(),
          line_idx + 1,
          label,
          line
        ));
      }
    }
  }

  assert!(
    violations.is_empty(),
    "DebugInfo semantic usage is forbidden outside debug tooling:\n{}",
    violations.join("\n")
  );
}
