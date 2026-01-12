use std::path::Path;

use tempfile::TempDir;
use xtask::webidl::load::{load_combined_webidl, WebIdlSource};
use xtask::webidl::parse_webidl;
use xtask::webidl::resolve::resolve_webidl_world;

fn write_file(path: &Path, contents: &str) {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).expect("create parent dirs");
  }
  std::fs::write(path, contents).expect("write file");
}

#[test]
fn webidl_overrides_are_loaded_and_partial_constructors_merge_as_overloads() {
  let tmp = TempDir::new().expect("create temp repo root");
  let repo_root = tmp.path();

  const PRELUDE_SENTINEL: &str = "PRELUDE_SENTINEL_WEBIDL_OVERRIDES_TEST";
  const OVERRIDE_SENTINEL: &str = "OVERRIDE_SENTINEL_WEBIDL_OVERRIDES_TEST";
  const SPEC_SENTINEL: &str = "SPEC_SENTINEL_WEBIDL_OVERRIDES_TEST";

  // Create a minimal repo-like structure:
  // - tools/webidl/prelude.idl
  // - tools/webidl/overrides/*.idl
  // - specs/test.bs with a <pre class=idl> block
  write_file(
    &repo_root.join("tools/webidl/prelude.idl"),
    &format!("// {PRELUDE_SENTINEL}\n"),
  );
  write_file(
    &repo_root.join("tools/webidl/overrides/00_override.idl"),
    &format!(
      "// {OVERRIDE_SENTINEL}\npartial interface EventTarget {{ constructor(any parent); }};\n"
    ),
  );
  write_file(
    &repo_root.join("specs/test.bs"),
    &format!(
      "<pre class=idl>\n// {SPEC_SENTINEL}\ninterface EventTarget {{\n  constructor();\n}};\n</pre>\n"
    ),
  );

  let loaded = load_combined_webidl(
    repo_root,
    &[WebIdlSource {
      rel_path: "specs/test.bs",
      label: "Test",
    }],
  )
  .expect("load combined WebIDL");
  assert!(
    loaded.missing_sources.is_empty(),
    "expected no missing sources; got {:?}",
    loaded.missing_sources
  );

  // Ensure load ordering: prelude + overrides are concatenated before extracted spec blocks.
  let combined = loaded.combined_idl;
  let prelude_idx = combined
    .find(PRELUDE_SENTINEL)
    .expect("combined IDL should include prelude content");
  let override_idx = combined
    .find(OVERRIDE_SENTINEL)
    .expect("combined IDL should include override content");
  let spec_idx = combined
    .find(SPEC_SENTINEL)
    .expect("combined IDL should include extracted spec IDL content");
  assert!(
    prelude_idx < override_idx,
    "expected prelude to appear before overrides; idx prelude={prelude_idx} override={override_idx}\ncombined:\n{combined}"
  );
  assert!(
    prelude_idx < spec_idx,
    "expected prelude to appear before spec-extracted IDL; idx prelude={prelude_idx} spec={spec_idx}\ncombined:\n{combined}"
  );
  assert!(
    override_idx < spec_idx,
    "expected overrides to appear before spec-extracted IDL; idx override={override_idx} spec={spec_idx}\ncombined:\n{combined}"
  );

  // Parse + resolve.
  let parsed = parse_webidl(&combined).expect("parse combined IDL");
  let resolved = resolve_webidl_world(&parsed);

  let iface = resolved
    .interface("EventTarget")
    .expect("resolved world should contain interface EventTarget");
  let constructors: Vec<_> = iface
    .members
    .iter()
    .filter(|m| m.name.as_deref() == Some("constructor"))
    .map(|m| m.raw.as_str())
    .collect();
  assert_eq!(
    constructors.len(),
    2,
    "expected base + override constructors to be preserved as overloads; got {constructors:?}"
  );
  assert!(
    constructors.contains(&"constructor()"),
    "expected base constructor() in resolved interface; got {constructors:?}"
  );
  assert!(
    constructors.contains(&"constructor(any parent)"),
    "expected override constructor(any parent) in resolved interface; got {constructors:?}"
  );
}

