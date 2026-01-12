//! Guard that ensures every Rust module file under `tests/` is reachable from a top-level test
//! harness (`tests/*.rs`).
//!
//! After consolidating the integration test suite into `tests/integration.rs` (plus a small number
//! of special harnesses), it is easy to leave behind a `tests/**/foo.rs` file without wiring it
//! into a `mod foo;` declaration. Such tests silently never run.
//!
//! This guard walks all top-level `tests/*.rs` crates, follows `mod name;` declarations (and the
//! small number of `include!(\"...\")` uses), and asserts that every `tests/**/*.rs` file is
//! reachable from at least one harness.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn module_search_dir(module_file: &Path, is_crate_root: bool) -> PathBuf {
  let parent = module_file
    .parent()
    .expect("module file must have a parent directory");

  if is_crate_root {
    return parent.to_path_buf();
  }

  if module_file.file_name().and_then(|name| name.to_str()) == Some("mod.rs") {
    return parent.to_path_buf();
  }

  let stem = module_file
    .file_stem()
    .and_then(|stem| stem.to_str())
    .expect("module file stem must be valid UTF-8");
  parent.join(stem)
}

fn resolve_module_file(module_dir: &Path, name: &str) -> PathBuf {
  let flat = module_dir.join(format!("{name}.rs"));
  let nested = module_dir.join(name).join("mod.rs");

  let flat_exists = flat.is_file();
  let nested_exists = nested.is_file();

  match (flat_exists, nested_exists) {
    (true, false) => flat,
    (false, true) => nested,
    (true, true) => panic!(
      "ambiguous module resolution for `mod {name};`: both {} and {} exist",
      flat.display(),
      nested.display()
    ),
    (false, false) => panic!(
      "could not resolve module `mod {name};`: neither {} nor {} exists",
      flat.display(),
      nested.display()
    ),
  }
}

fn list_top_level_test_crates(tests_dir: &Path) -> Vec<PathBuf> {
  let entries = fs::read_dir(tests_dir)
    .unwrap_or_else(|err| panic!("failed to read tests dir {}: {err}", tests_dir.display()));
  let mut roots = Vec::new();
  for entry in entries {
    let entry = entry.expect("read tests dir entry");
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    roots.push(path);
  }
  roots.sort();
  roots
}

fn collect_reachable_module_files(roots: &[PathBuf]) -> HashSet<PathBuf> {
  let mod_decl = Regex::new(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;",
  )
  .expect("mod declaration regex should compile");
  let include_decl =
    Regex::new(r#"^\s*include!\(\s*"([^"]+)"\s*\)\s*;"#).expect("include! regex should compile");

  let mut reachable = HashSet::new();
  let mut queue = VecDeque::new();

  for root in roots {
    queue.push_back((root.clone(), true));
  }

  while let Some((module_file, is_crate_root)) = queue.pop_front() {
    if !reachable.insert(module_file.clone()) {
      continue;
    }

    let content = fs::read_to_string(&module_file)
      .unwrap_or_else(|err| panic!("failed to read {}: {err}", module_file.display()));
    let module_dir = module_search_dir(&module_file, is_crate_root);

    for line in content.lines() {
      if let Some(caps) = mod_decl.captures(line) {
        let name = caps.get(1).expect("module name capture").as_str();
        let resolved = resolve_module_file(&module_dir, name);
        queue.push_back((resolved, false));
        continue;
      }

      if let Some(caps) = include_decl.captures(line) {
        let include_path = caps.get(1).expect("include path capture").as_str();
        let parent = module_file
          .parent()
          .expect("module file must have a parent directory");
        let resolved = parent.join(include_path);
        assert!(
          resolved.is_file(),
          "include! macro references missing file {} (from {})",
          resolved.display(),
          module_file.display()
        );
        queue.push_back((resolved, false));
      }
    }
  }

  reachable
}

#[test]
fn no_orphaned_test_modules() {
  let root = repo_root();
  let tests_dir = root.join("tests");
  let roots = list_top_level_test_crates(&tests_dir);
  assert!(
    !roots.is_empty(),
    "expected at least one top-level tests/*.rs crate under {}",
    tests_dir.display()
  );

  let reachable = collect_reachable_module_files(&roots);

  let mut all_rs = BTreeSet::new();
  for entry in WalkDir::new(&tests_dir).into_iter().filter_map(Result::ok) {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    all_rs.insert(path.to_path_buf());
  }

  let mut orphans = Vec::new();
  for path in all_rs {
    if reachable.contains(&path) {
      continue;
    }
    let rel = path.strip_prefix(&root).unwrap_or(&path);
    orphans.push(rel.display().to_string());
  }

  orphans.sort();
  assert!(
    orphans.is_empty(),
    "Found Rust files under tests/ that are not reachable from any top-level tests/*.rs harness:\n{}\n\n\
Wire them into the appropriate `mod ...;` tree (or delete the dead file).",
    orphans.join("\n")
  );
}

