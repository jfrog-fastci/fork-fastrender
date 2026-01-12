use std::path::PathBuf;

fn readme_text() -> String {
  let readme_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
  std::fs::read_to_string(&readme_path)
    .unwrap_or_else(|err| panic!("failed to read {}: {err}", readme_path.display()))
}

#[test]
fn readme_does_not_claim_checked_backend_lacks_reexport_support() {
  let readme = readme_text();
  let readme_lower = readme.to_lowercase();

  // Historical docs drift: this README used to claim re-export syntax was not supported by the
  // checked/HIR backend, despite the implementation and tests supporting it.
  let stale_reexport_claim = readme_lower.contains("re-export syntax")
    && readme_lower.contains("checked")
    && (readme_lower.contains("not yet supported") || readme_lower.contains("not supported"));
  assert!(
    !stale_reexport_claim,
    "native-js-cli README contains stale checked/HIR re-export support wording"
  );
}

#[test]
fn readme_documents_type_only_reexports_and_cycles() {
  let readme = readme_text();
  let readme_lower = readme.to_lowercase();

  // Runtime re-exports participate in module initialization ordering.
  assert!(
    readme.contains("re-export-only modules participate in module initialization ordering"),
    "native-js-cli README should explain that runtime re-exports participate in module initialization ordering"
  );

  // Type-only re-exports are runtime-inert (do not trigger module evaluation).
  assert!(
    readme.contains("type_only_reexport_does_not_execute_module"),
    "native-js-cli README should reference the `type_only_reexport_does_not_execute_module` test"
  );
  assert!(
    readme.contains("export { type"),
    "native-js-cli README should include a `export {{ type ... }} from \"...\"` example for type-only re-export semantics"
  );

  // Cyclic runtime module dependencies are rejected.
  assert!(
    readme.contains("NJS0146"),
    "native-js-cli README should mention the cycle diagnostic code (NJS0146)"
  );
  let normalized = readme_lower.replace('*', "").replace('\n', " ");
  assert!(
    normalized.contains("cyclic runtime") && normalized.contains("njs0146"),
    "native-js-cli README should clarify that NJS0146 is for cyclic *runtime* module dependencies"
  );
}

#[test]
fn readme_documents_project_flag_tsconfig_support() {
  let readme = readme_text();
  let readme_lower = readme.to_lowercase();

  // `native-js-cli` historically did not support loading a tsconfig at all; it now supports
  // `--project/-p` for both pipelines.
  assert!(
    readme_lower.contains("--project"),
    "native-js-cli README should mention the --project/-p flag"
  );
  assert!(
    readme_lower.contains("native-js-cli --project ./tsconfig.json"),
    "native-js-cli README should include a `native-js-cli --project ./tsconfig.json ...` example"
  );

  // Docs drift guard: the README used to claim unconditionally that `native-js-cli` doesn't load
  // tsconfig.json. Now tsconfig support is gated on `--project`.
  let normalized = readme_lower.replace('`', "");
  assert!(
    !normalized.contains("does not load tsconfig.json"),
    "native-js-cli README contains stale wording claiming --project/tsconfig is unsupported"
  );
  assert!(
    !normalized.contains("tsconfig.json is not loaded"),
    "native-js-cli README contains stale wording claiming --project/tsconfig is unsupported"
  );
}
