use std::fs;

use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::suite::{load_builtin_suite, select_tests};

#[test]
fn modules_smoke_suite_selects_module_tests() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: only the `test/` directory is required for discovery.
  let module_dir = temp.path().join("test/language/module-code");
  let import_dir = temp.path().join("test/language/import");
  let export_dir = temp.path().join("test/language/export");
  let import_meta_dir = temp.path().join("test/language/expressions/import.meta");
  fs::create_dir_all(&module_dir).unwrap();
  fs::create_dir_all(&import_dir).unwrap();
  fs::create_dir_all(&export_dir).unwrap();
  fs::create_dir_all(&import_meta_dir).unwrap();

  // Test IDs are paths relative to `test/` in the tc39/test262 checkout.
  fs::write(
    module_dir.join("early-export-unresolvable.js"),
    "/*---\nflags: [module]\n---*/\nexport { x } from './does-not-exist.js';\n",
  )
  .unwrap();
  fs::write(
    import_dir.join("dup-bound-names.js"),
    "/*---\nflags: [module]\n---*/\nimport { a as a } from './x.js';\n",
  )
  .unwrap();
  fs::write(
    export_dir.join("escaped-default.js"),
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
    selected.iter().any(|id| id == "language/import/dup-bound-names.js"),
    "expected suite to include language/import/dup-bound-names.js, got: {selected:#?}"
  );
  assert!(
    selected.iter().any(|id| id == "language/export/escaped-default.js"),
    "expected suite to include language/export/escaped-default.js, got: {selected:#?}"
  );
  assert!(
    selected
      .iter()
      .any(|id| id == "language/expressions/import.meta/same-object-returned.js"),
    "expected suite to include language/expressions/import.meta/same-object-returned.js, got: {selected:#?}"
  );
}
