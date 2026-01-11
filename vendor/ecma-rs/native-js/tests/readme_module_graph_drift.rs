use std::path::PathBuf;

fn readme_text() -> String {
  let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
  std::fs::read_to_string(&readme_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", readme_path.display()))
}

#[test]
fn readme_documents_reexports_type_only_edges_and_cycle_code() {
  let readme = readme_text();

  // Regression guard: the checked/HIR backend supports runtime re-exports as module dependencies, so
  // the README must not claim they are unsupported.
  assert!(
    !readme.contains("re-export syntax")
      && !readme.contains("not yet supported by the checked/HIR backend"),
    "native-js README appears to contain stale re-export support wording"
  );

  // The README should mention the specific re-export forms that are supported today.
  assert!(
    readme.contains("Re-export statements (`export { foo } from`,") && readme.contains("`export * from`)"),
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
}
