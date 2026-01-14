use std::path::Path;

use test262_semantic::discover::discover_tests;
use test262_semantic::suite::{load_builtin_suite, select_tests};

#[test]
fn modules_smoke_suite_selects_module_tests_from_vendored_corpus() {
  let suite = load_builtin_suite("modules_smoke").unwrap();

  // `data/` is a vendored `tc39/test262` checkout.
  let test262_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
  let discovered = discover_tests(&test262_dir).unwrap();

  let selected = select_tests(&suite, &discovered).unwrap();
  assert!(!selected.is_empty());

  // Stable module tests that should exist in any reasonably recent test262 checkout.
  assert!(selected
    .iter()
    .any(|id| id == "language/module-code/early-export-unresolvable.js"));

  // Ensure we keep covering the standalone `language/import` + `language/export` directories (not
  // just the `language/module-code` corpus).
  assert!(selected
    .iter()
    .any(|id| id == "language/import/dup-bound-names.js"));
  assert!(selected
    .iter()
    .any(|id| id == "language/export/escaped-default.js"));
}
