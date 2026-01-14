use std::fs;

use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::suite::{load_builtin_suite, select_tests};

#[test]
fn modules_smoke_suite_selects_module_tests() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: only the `test/` directory is required for discovery.
  let module_dir = temp.path().join("test/language/module-code");
  let ambiguous_export_dir = temp
    .path()
    .join("test/language/module-code/ambiguous-export-bindings");
  let tla_dir = temp.path().join("test/language/module-code/top-level-await");
  let import_dir = temp.path().join("test/language/import");
  let import_attr_dir = temp.path().join("test/language/import/import-attributes");
  let export_dir = temp.path().join("test/language/export");
  let import_meta_dir = temp.path().join("test/language/expressions/import.meta");
  fs::create_dir_all(&module_dir).unwrap();
  fs::create_dir_all(&ambiguous_export_dir).unwrap();
  fs::create_dir_all(&tla_dir).unwrap();
  fs::create_dir_all(&import_dir).unwrap();
  fs::create_dir_all(&import_attr_dir).unwrap();
  fs::create_dir_all(&export_dir).unwrap();
  fs::create_dir_all(&import_meta_dir).unwrap();

  // Test IDs are paths relative to `test/` in the tc39/test262 checkout.
  fs::write(
    module_dir.join("early-export-unresolvable.js"),
    "/*---\nflags: [module]\n---*/\nexport { x } from './does-not-exist.js';\n",
  )
  .unwrap();
  // Ensure `include = ["language/module-code/early-*.js"]` stays wired up (this id is *not* listed
  // explicitly under `tests = [...]` in the suite).
  fs::write(
    module_dir.join("early-other.js"),
    "/*---\nflags: [module]\n---*/\nexport {};\n",
  )
  .unwrap();
  fs::write(
    module_dir.join("export-default-basic.js"),
    "/*---\nflags: [module]\n---*/\nexport default 1;\n",
  )
  .unwrap();
  fs::write(
    module_dir.join("export-expname-basic.js"),
    "/*---\nflags: [module]\n---*/\nexport { x as y };\n",
  )
  .unwrap();
  fs::write(
    ambiguous_export_dir.join("star-export.js"),
    "/*---\nflags: [module]\n---*/\nexport * from './x.js';\n",
  )
  .unwrap();
  fs::write(
    module_dir.join("instn-local-bndng-basic.js"),
    "/*---\nflags: [module]\n---*/\nexport {};\n",
  )
  .unwrap();
  fs::write(
    module_dir.join("comment-basic.js"),
    "/*---\nflags: [module]\n---*/\n/* comment */\nexport {};\n",
  )
  .unwrap();
  fs::write(
    tla_dir.join("await-expr-resolution.js"),
    "/*---\nflags: [module, async]\n---*/\nawait 42;\n$DONE();\n",
  )
  .unwrap();
  fs::write(
    tla_dir.join("dynamic-import-resolution.js"),
    "/*---\nflags: [module, async]\n---*/\nimport('./mod.js').then($DONE, $DONE);\n",
  )
  .unwrap();
  fs::write(
    import_dir.join("dup-bound-names.js"),
    "/*---\nflags: [module]\n---*/\nimport { a as a } from './x.js';\n",
  )
  .unwrap();
  // Ensure `include = ["language/import/dup-*.js"]` stays wired up (this id is *not* listed
  // explicitly under `tests = [...]` in the suite).
  fs::write(
    import_dir.join("dup-custom.js"),
    "/*---\nflags: [module]\n---*/\nexport {};\n",
  )
  .unwrap();
  fs::write(
    import_attr_dir.join("json-value-string.js"),
    "/*---\nflags: [module]\n---*/\nimport value from './fixture.json' with { type: 'json' };\nvalue;\n",
  )
  .unwrap();
  fs::write(
    export_dir.join("escaped-default.js"),
    "/*---\nflags: [module]\n---*/\nexport default 1;\n",
  )
  .unwrap();
  // Ensure `include = ["language/export/escaped-*.js"]` stays wired up (this id is *not* listed
  // explicitly under `tests = [...]` in the suite).
  fs::write(
    export_dir.join("escaped-custom.js"),
    "/*---\nflags: [module]\n---*/\nexport default 1;\n",
  )
  .unwrap();
  fs::write(
    import_meta_dir.join("same-object-returned.js"),
    "/*---\nflags: [module]\n---*/\nimport.meta;\n",
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let suite = load_builtin_suite("modules_smoke").unwrap();
  let selected = select_tests(&suite, &discovered).unwrap();

  assert!(!selected.is_empty(), "expected suite to select >0 tests");
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/early-export-unresolvable.js"),
    "expected suite to include early-export-unresolvable.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/module-code/early-other.js"),
    "expected suite to include language/module-code/early-other.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/export-default-basic.js"),
    "expected suite to include language/module-code/export-default-basic.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/export-expname-basic.js"),
    "expected suite to include language/module-code/export-expname-basic.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/ambiguous-export-bindings/star-export.js"),
    "expected suite to include language/module-code/ambiguous-export-bindings/star-export.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/instn-local-bndng-basic.js"),
    "expected suite to include language/module-code/instn-local-bndng-basic.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/comment-basic.js"),
    "expected suite to include language/module-code/comment-basic.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/top-level-await/await-expr-resolution.js"),
    "expected suite to include language/module-code/top-level-await/await-expr-resolution.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/module-code/top-level-await/dynamic-import-resolution.js"),
    "expected suite to include language/module-code/top-level-await/dynamic-import-resolution.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/import/dup-bound-names.js"),
    "expected suite to include language/import/dup-bound-names.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/import/dup-custom.js"),
    "expected suite to include language/import/dup-custom.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/import/import-attributes/json-value-string.js"),
    "expected suite to include language/import/import-attributes/json-value-string.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/export/escaped-default.js"),
    "expected suite to include language/export/escaped-default.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/export/escaped-custom.js"),
    "expected suite to include language/export/escaped-custom.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/expressions/import.meta/same-object-returned.js"),
    "expected suite to include language/expressions/import.meta/same-object-returned.js, got: {selected:#?}"
  );
}
