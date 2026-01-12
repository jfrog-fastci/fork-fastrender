use std::sync::Arc;

mod common;

use diagnostics::TextRange;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{Diagnostic, FileKey, MemoryHost, Program};

const TS2669_MESSAGE: &str =
  "Augmentations for the global scope can only be directly nested in external modules or ambient module declarations.";

fn host_with_libs() -> MemoryHost {
  if cfg!(feature = "bundled-libs") {
    MemoryHost::default()
  } else {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host
  }
}

fn find_ts2669<'a>(diagnostics: &'a [Diagnostic]) -> Vec<&'a Diagnostic> {
  diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == "TS2669")
    .collect()
}

#[test]
fn module_augmentation_global_in_script_errors() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  let source = "declare global { interface Array<T> { x: number; } }";
  host.insert(key.clone(), Arc::<str>::from(source));

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let diagnostics = program.check();

  let matches = find_ts2669(&diagnostics);
  assert_eq!(
    matches.len(),
    1,
    "expected exactly one TS2669 diagnostic, got: {diagnostics:?}"
  );
  let diag = matches[0];
  assert_eq!(diag.message, TS2669_MESSAGE);
  assert_eq!(diag.primary.file, file_id);

  let start = source
    .find("global")
    .expect("global keyword should be present") as u32;
  assert_eq!(diag.primary.range, TextRange::new(start, start + 6));
}

#[test]
fn module_augmentation_global_nested_in_namespace_errors() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  let source = "namespace A { declare global { interface Array<T> { x: number; } } }";
  host.insert(key.clone(), Arc::<str>::from(source));

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let diagnostics = program.check();

  let matches = find_ts2669(&diagnostics);
  assert_eq!(
    matches.len(),
    1,
    "expected exactly one TS2669 diagnostic, got: {diagnostics:?}"
  );
  let diag = matches[0];
  assert_eq!(diag.message, TS2669_MESSAGE);
  assert_eq!(diag.primary.file, file_id);

  let start = source
    .find("global")
    .expect("global keyword should be present") as u32;
  assert_eq!(diag.primary.range, TextRange::new(start, start + 6));
}

#[test]
fn module_augmentation_global_in_external_module_is_allowed() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    Arc::<str>::from("export {};\ndeclare global { interface Array<T> { x: number; } }"),
  );

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();

  assert!(
    find_ts2669(&diagnostics).is_empty(),
    "did not expect TS2669 diagnostics, got: {diagnostics:?}"
  );
}
