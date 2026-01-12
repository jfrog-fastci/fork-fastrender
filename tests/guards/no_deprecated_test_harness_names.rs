use std::path::Path;

use walkdir::WalkDir;

/// Guardrail against stale documentation after the integration-test consolidation migration.
///
/// We previously had many per-category integration test binaries which were consolidated into a
/// unified `tests/integration.rs` harness (plus a small number of special-case binaries).
///
/// This test ensures the remaining documentation/comments under `tests/` don't keep pointing
/// contributors at those deprecated test targets.
#[test]
fn no_deprecated_test_harness_names_in_tests_docs() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let tests_dir = repo_root.join("tests");

  // Keep this list in sync with the post-migration verification command in
  // instructions/test_cleanup.md.
  //
  // Important: avoid embedding the full deprecated names as raw string literals so the docs sweep
  // (`rg` over tests/ excluding tests/*.rs) doesn't match this guard itself.
  let deprecated: &[(&str, &str)] = &[
    ("layout", "_tests"),
    ("style", "_tests"),
    ("paint", "_tests"),
    ("misc", "_tests"),
    ("regression", "_tests"),
    ("ref", "_tests"),
    ("fixtures", "_test"),
    ("determinism", "_tests"),
    ("allocation_failure", "_tests"),
    ("wpt", "_test"),
    ("wpt", "_tests"),
  ];
  let alternation = deprecated
    .iter()
    .map(|(prefix, suffix)| format!("{prefix}{suffix}"))
    .collect::<Vec<_>>()
    .join("|");
  let re = regex::Regex::new(&format!(r"\b({alternation})\b"))
    .expect("deprecated harness regex should compile");
  let harness_path_re = regex::Regex::new(r"\btests/[A-Za-z0-9_]+_(?:tests|test)\.rs\b")
    .expect("deprecated harness path regex should compile");

  let mut matches = Vec::new();
  for entry in WalkDir::new(&tests_dir)
    .into_iter()
    .filter_entry(|entry| !super::should_skip_tests_entry(entry, &tests_dir))
    .filter_map(Result::ok)
  {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }

    let ext = path.extension().and_then(|ext| ext.to_str());
    if !matches!(ext, Some("rs") | Some("md")) {
      continue;
    }

    // Skip the top-level `tests/*.rs` crates: during the migration these files are deleted/renamed,
    // and we only care about stale references in module trees + READMEs that survive post-cleanup.
    if ext == Some("rs") && path.parent() == Some(tests_dir.as_path()) {
      continue;
    }

    let Ok(content) = std::fs::read_to_string(path) else {
      continue;
    };

    for (idx, line) in content.lines().enumerate() {
      if re.is_match(line) || harness_path_re.is_match(line) {
        let rel = path
          .strip_prefix(repo_root)
          .unwrap_or(path)
          .display()
          .to_string();
        matches.push(format!("{rel}:{}:{line}", idx + 1));
      }
    }
  }

  assert!(
    matches.is_empty(),
    "Found deprecated integration-test harness names under tests/ (excluding tests/*.rs):\n{}\n\n\
Update these references to point at the unified integration test harness (tests/integration.rs) \
or remove the stale per-binary guidance.",
    matches.join("\n")
  );
}
