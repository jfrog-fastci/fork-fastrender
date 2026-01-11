mod common;

use std::sync::Arc;

use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn strict_host() -> MemoryHost {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    strict_native: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  host
}

#[test]
fn rejects_explicit_any_type_annotation() {
  let mut host = strict_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "let x: any = 1;");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::FORBIDDEN_ANY.as_str()),
    "expected TC4000, got {diagnostics:?}",
  );
}

#[test]
fn rejects_inferred_any_from_ambient_declaration() {
  let mut host = strict_host();
  host.add_lib(LibFile {
    key: FileKey::new("bad.d.ts"),
    name: Arc::from("bad.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from("declare function bad(): any;"),
  });
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "let x = bad();");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::FORBIDDEN_ANY.as_str()),
    "expected TC4000, got {diagnostics:?}",
  );
}

#[test]
fn rejects_unsafe_type_assertions() {
  let mut host = strict_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "let x = 1 as unknown as number;");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNSAFE_TYPE_ASSERTION.as_str()),
    "expected TC4005, got {diagnostics:?}",
  );
}

#[test]
fn rejects_non_null_assertions_that_discard_nullability() {
  let mut host = strict_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "let x: string | null = null; x!.length;");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::INVALID_NON_NULL_ASSERTION.as_str()),
    "expected TC4006, got {diagnostics:?}",
  );
}

#[test]
fn allows_non_null_assertions_when_already_narrowed() {
  let mut host = strict_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    "let x: string | null = null; if (x) { x!.length }",
  );

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
}
