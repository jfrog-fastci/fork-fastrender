use std::path::PathBuf;

fn readme_text() -> String {
  let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
  std::fs::read_to_string(&readme_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", readme_path.display()))
}

#[test]
fn readme_does_not_claim_checked_backend_lacks_reexport_support() {
  let readme = readme_text();

  // Historical docs drift: this README used to claim re-export syntax was not supported by the
  // checked/HIR backend, despite the implementation and tests supporting it.
  assert!(
    !readme.contains("not yet supported by the checked/HIR backend"),
    "native-js-cli README contains stale checked/HIR re-export wording"
  );
}

#[test]
fn readme_documents_type_only_reexports_and_cycles() {
  let readme = readme_text();

  // Runtime re-exports participate in module initialization ordering.
  assert!(
    readme.contains("export { x } from \"./dep\""),
    "native-js-cli README should include an `export {{ x }} from \"./dep\"` example in the checked/HIR section"
  );
  assert!(
    readme.contains("export * from \"./dep\""),
    "native-js-cli README should include an `export * from \"./dep\"` example in the checked/HIR section"
  );

  // Type-only re-exports are runtime-inert (do not trigger module evaluation).
  assert!(
    readme.contains("type_only_reexport_does_not_execute_module"),
    "native-js-cli README should reference the `type_only_reexport_does_not_execute_module` test"
  );

  // Cyclic runtime module dependencies are rejected.
  assert!(
    readme.contains("NJS0146"),
    "native-js-cli README should mention the cycle diagnostic code (NJS0146)"
  );
}
