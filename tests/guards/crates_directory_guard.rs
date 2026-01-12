//! Guard against `crates/` accumulating new parallel infrastructure crates.
//!
//! See `instructions/webidl_consolidation.md`.
//!
//! WebIDL stack consolidation is complete: shared JS/WebIDL infrastructure should live in the
//! vendored `vendor/ecma-rs/` workspace and `crates/` should be reserved for FastRender-specific
//! tooling.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Crate directories that are allowed to exist under `crates/`.
///
/// Adding any new crate under `crates/` must be an explicit decision: update this allowlist and
/// justify why it doesn't belong under `vendor/ecma-rs/`.
const ALLOWED_CRATE_DIRS: [&str; 2] = ["js-wpt-dom-runner", "webidl-js-runtime"];

fn list_crate_dirs(crates_dir: &Path) -> BTreeSet<String> {
  let mut dirs = BTreeSet::new();

  for entry in fs::read_dir(crates_dir)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", crates_dir.display()))
  {
    let entry =
      entry.unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", crates_dir.display()));

    let file_type = entry
      .file_type()
      .unwrap_or_else(|err| panic!("failed to stat {}: {err}", entry.path().display()));
    if !file_type.is_dir() {
      continue;
    }

    let file_name = entry.file_name();
    let Some(name) = file_name.to_str() else {
      continue;
    };
    if name.starts_with('.') {
      continue;
    }

    dirs.insert(name.to_owned());
  }

  dirs
}

#[test]
fn crates_directory_is_explicitly_allowlisted() {
  // Intentionally scan only `crates/` one level deep. Do not walk into `vendor/` or `specs/`.
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let crates_dir = repo_root.join("crates");
  assert!(
    crates_dir.is_dir(),
    "expected a crates/ directory at {}",
    crates_dir.display()
  );

  let actual = list_crate_dirs(&crates_dir);
  let allowlist: BTreeSet<String> =
    ALLOWED_CRATE_DIRS.iter().map(|dir| (*dir).to_owned()).collect();

  let unexpected: Vec<_> = actual.difference(&allowlist).cloned().collect();

  assert!(
    unexpected.is_empty(),
    "unexpected crate directories found under crates/.\n\
unexpected crate dirs found: {unexpected:?}\n\
allowlisted crate dirs: {allowlist:?}\n\
actual crate dirs: {actual:?}",
  );
}
