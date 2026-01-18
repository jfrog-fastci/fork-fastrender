//! Guardrail against stale documentation pointing at the legacy QuickJS backend.
//!
//! FastRender keeps some QuickJS code around for debugging/comparison, but the public-facing docs and
//! the offline WPT DOM corpus should not mention it (to avoid confusing contributors about the
//! supported/runtime-backed JS stacks).
//!
//! This test enforces the same constraint as the cleanup task:
//! `rg -n "QuickJS|quickjs" docs tests/wpt_dom` should return no hits.

use std::path::Path;

use walkdir::WalkDir;

#[test]
fn no_quickjs_references_in_docs_and_wpt_dom() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let roots = [repo_root.join("docs"), repo_root.join("tests").join("wpt_dom")];

  let needles = ["QuickJS", "quickjs"];
  let mut matches = Vec::new();

  for root in roots {
    if !root.exists() {
      continue;
    }

    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
      let path = entry.path();
      if !path.is_file() {
        continue;
      }

      let Ok(bytes) = std::fs::read(path) else {
        continue;
      };
      let content = String::from_utf8_lossy(&bytes);
      if !needles.iter().any(|needle| content.contains(needle)) {
        continue;
      }

      let rel = path
        .strip_prefix(repo_root)
        .unwrap_or(path)
        .display()
        .to_string();

      for (idx, line) in content.lines().enumerate() {
        if needles.iter().any(|needle| line.contains(needle)) {
          matches.push(format!("{rel}:{}:{line}", idx + 1));
        }
      }
    }
  }

  assert!(
    matches.is_empty(),
    "Found QuickJS references in docs/ or tests/wpt_dom (these should stay QuickJS-free):\n{}",
    matches.join("\n")
  );
}

