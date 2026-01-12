//! Guardrail to ensure conformance targets are documented and enforced.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

fn normalize_markdown_link_target(raw_target: &str) -> Option<String> {
  // Support the common Markdown forms:
  //   [text](path)
  //   [text](path#fragment)
  //   [text](path "title")
  // We intentionally keep this lightweight (not a full Markdown parser).
  let raw_target = raw_target.trim();
  let raw_target = raw_target
    .split_whitespace()
    .next()
    .unwrap_or_default()
    .trim_matches('<')
    .trim_matches('>');
  let raw_target = raw_target
    .split_once('#')
    .map(|(path, _frag)| path)
    .unwrap_or(raw_target);
  let raw_target = raw_target
    .split_once('?')
    .map(|(path, _query)| path)
    .unwrap_or(raw_target);

  if raw_target.is_empty()
    || raw_target.starts_with('#')
    || raw_target.starts_with("http://")
    || raw_target.starts_with("https://")
    || raw_target.starts_with("mailto:")
  {
    return None;
  }

  Some(raw_target.to_string())
}

fn is_test_link_target(
  target: &str,
  docs_dir: &Path,
  src_test_cache: &mut HashMap<PathBuf, bool>,
) -> bool {
  if target.starts_with("../tests/") {
    return true;
  }

  if !target.starts_with("../src/") {
    return false;
  }

  let resolved = docs_dir.join(target);
  if !resolved.is_file() || resolved.extension().and_then(|ext| ext.to_str()) != Some("rs") {
    return false;
  }

  if let Some(cached) = src_test_cache.get(&resolved) {
    return *cached;
  }

  let Ok(file) = std::fs::read_to_string(&resolved) else {
    src_test_cache.insert(resolved, false);
    return false;
  };

  let contains_tests =
    file.contains("#[test]") || file.contains("#[tokio::test]") || file.contains("cfg(test)");
  src_test_cache.insert(resolved, contains_tests);
  contains_tests
}

fn column_has_code_link(column: &str, link_re: &regex::Regex) -> bool {
  link_re.captures_iter(column).any(|cap| {
    let raw_target = cap.get(1).expect("link target capture").as_str();
    let Some(target) = normalize_markdown_link_target(raw_target) else {
      return false;
    };

    target.starts_with("../src/")
      || target.starts_with("../crates/")
      || target.starts_with("../vendor/")
  })
}

fn column_has_test_link(
  column: &str,
  link_re: &regex::Regex,
  docs_dir: &Path,
  src_test_cache: &mut HashMap<PathBuf, bool>,
) -> bool {
  link_re.captures_iter(column).any(|cap| {
    let raw_target = cap.get(1).expect("link target capture").as_str();
    let Some(target) = normalize_markdown_link_target(raw_target) else {
      return false;
    };

    is_test_link_target(&target, docs_dir, src_test_cache)
  })
}

#[test]
fn conformance_doc_is_present_and_non_empty() {
  let conformance = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/conformance.md");
  assert!(
    conformance.exists(),
    "docs/conformance.md should exist as the conformance source of truth"
  );

  let content = std::fs::read_to_string(&conformance).expect("read docs/conformance.md");
  assert!(
    !content.trim().is_empty(),
    "docs/conformance.md should not be empty"
  );
}

#[test]
fn conformance_doc_links_to_real_code_and_tests() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let conformance_path = root.join("docs/conformance.md");
  let content = std::fs::read_to_string(&conformance_path).expect("read docs/conformance.md");
  let docs_dir = conformance_path
    .parent()
    .expect("docs/conformance.md should have a parent directory");

  let link_re =
    regex::Regex::new(r"\[[^\]]*]\(([^)]+)\)").expect("regex for markdown links should compile");

  // Validate that the support matrix table is structurally parseable:
  // - header exists
  // - every row has 6 columns
  // - status column uses the legend markers
  let mut src_test_cache: HashMap<PathBuf, bool> = HashMap::new();
  let mut in_table = false;
  let mut saw_data_row = false;
  for (idx, line) in content.lines().enumerate() {
    let trimmed = line.trim();
    if !in_table {
      if trimmed.starts_with("| Stage") {
        in_table = true;
      } else {
        continue;
      }
    }

    if !trimmed.starts_with('|') {
      break;
    }

    let parts: Vec<&str> = trimmed.split('|').collect();
    // A well-formed markdown row looks like: | a | b | ... |.
    // That yields an empty first/last element.
    assert!(
      parts.len() >= 3,
      "docs/conformance.md support matrix row is malformed at line {}: {trimmed:?}",
      idx + 1
    );
    let cols = parts.len() - 2;
    assert_eq!(
      cols, 6,
      "docs/conformance.md support matrix row must have 6 columns (found {cols}) at line {}: {trimmed:?}",
      idx + 1
    );

    // Skip header + delimiter rows.
    if trimmed.starts_with("| Stage") || trimmed.starts_with("| ---") {
      continue;
    }

    saw_data_row = true;
    let status = parts[3].trim(); // Stage | Feature | Status | ...
    assert!(
      matches!(status, "✅" | "⚠️" | "🚫"),
      "docs/conformance.md support matrix status must be ✅/⚠️/🚫 (got {status:?}) at line {}",
      idx + 1
    );

    // Keep the matrix grounded in real code/tests without hardcoding specific paths.
    // For supported/partial features, require at least one link in both the Implementation and
    // Tests columns so the documentation stays tied to repo reality while allowing files to move.
    if status != "🚫" {
      let implementation = parts[4].trim();
      let tests = parts[5].trim();
      assert!(
        link_re.is_match(implementation),
        "docs/conformance.md support matrix row must include a markdown link in the Implementation column at line {} (got {implementation:?})",
        idx + 1
      );
      assert!(
        link_re.is_match(tests),
        "docs/conformance.md support matrix row must include a markdown link in the Tests column at line {} (got {tests:?})",
        idx + 1
      );

      assert!(
        column_has_code_link(implementation, &link_re),
        "docs/conformance.md support matrix row must link to implementation code (under ../src/, ../crates/, or ../vendor/) at line {} (got {implementation:?})",
        idx + 1
      );
      assert!(
        column_has_test_link(tests, &link_re, docs_dir, &mut src_test_cache),
        "docs/conformance.md support matrix row must link to at least one test in the Tests column at line {} (got {tests:?})",
        idx + 1
      );
    }
  }
  assert!(
    in_table,
    "docs/conformance.md should contain a support matrix table starting with a `| Stage` header row"
  );
  assert!(
    saw_data_row,
    "docs/conformance.md support matrix table should have at least one data row"
  );

  // Validate that every markdown link target resolves to an existing path (relative to docs/).
  // This guards against doc drift when files are renamed/moved.
  let mut linked: HashSet<String> = HashSet::new();
  for cap in link_re.captures_iter(&content) {
    let raw_target = cap.get(1).expect("link target capture").as_str().trim();
    if let Some(target) = normalize_markdown_link_target(raw_target) {
      linked.insert(target);
    }
  }

  let mut missing = Vec::<(String, PathBuf)>::new();
  for path in &linked {
    let resolved = docs_dir.join(path);
    if !resolved.exists() {
      missing.push((path.clone(), resolved));
    }
  }

  if !missing.is_empty() {
    missing.sort_by(|a, b| a.0.cmp(&b.0));
    let formatted = missing
      .into_iter()
      .map(|(rel, abs)| format!("{rel} (resolved to {})", abs.display()))
      .collect::<Vec<_>>()
      .join("\n");
    panic!("docs/conformance.md contains links to paths that do not exist:\n{formatted}");
  }

  assert!(
    linked.iter().any(|p| p.starts_with("../src/")),
    "docs/conformance.md should link to at least one source file under ../src/"
  );

  // Post-cleanup, conformance.md may link to unit tests in `src/` instead of `tests/`.
  //
  // Keep this guardrail strict enough to fail if the document loses any concrete linkage to tests,
  // while allowing either:
  // - `../tests/...` integration modules (included by `tests/integration.rs`), or
  // - `../src/...` files that actually contain Rust tests.
  let has_test_link = linked
    .iter()
    .any(|target| is_test_link_target(target, docs_dir, &mut src_test_cache));

  assert!(
    has_test_link,
    "docs/conformance.md should link to at least one test (either a `../tests/...` integration module or a `../src/...` Rust file containing tests)"
  );
}
