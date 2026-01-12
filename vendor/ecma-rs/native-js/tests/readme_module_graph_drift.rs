use std::path::PathBuf;

fn readme_text() -> String {
  let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
  std::fs::read_to_string(&readme_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", readme_path.display()))
}

#[test]
fn readme_documents_reexports_type_only_edges_and_cycle_code() {
  let readme = readme_text();
  let readme_lower = readme.to_lowercase();

  // Regression guard: the checked/HIR backend supports runtime re-exports as module dependencies, so
  // the README must not claim they are unsupported.
  let stale_reexport_claim = readme_lower.contains("re-export syntax")
    && readme_lower.contains("checked")
    && (readme_lower.contains("not yet supported") || readme_lower.contains("not supported"));
  assert!(
    !stale_reexport_claim,
    "native-js README appears to contain stale checked/HIR re-export support wording"
  );

  // The README should mention the specific re-export forms that are supported today.
  assert!(
    readme.contains("Re-export statements (`export {") && readme.contains("`export * from`)"),
    "native-js README should mention supported re-export forms (`export {{ foo }} from`, `export * from`)"
  );

  // Type-only exports/re-exports are erased from JS output and must not trigger module evaluation.
  assert!(
    readme.contains("type_only_reexport_does_not_execute_module"),
    "native-js README should point to the `type_only_reexport_does_not_execute_module` test for type-only re-export semantics"
  );

  // Cyclic runtime module dependencies are rejected deterministically.
  assert!(
    readme.contains("NJS0146"),
    "native-js README should mention the current cycle diagnostic code (NJS0146)"
  );
  let normalized = readme_lower.replace('*', "").replace('\n', " ");
  assert!(
    normalized.contains("cyclic runtime") && normalized.contains("njs0146"),
    "native-js README should clarify that NJS0146 is for cyclic *runtime* module dependencies"
  );
}
