mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn class_expression_initializer_has_concrete_type_with_no_implicit_any() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    no_implicit_any: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "export const C = class {};";
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::IMPLICIT_ANY.as_str()),
    "unexpected implicit-any diagnostics: {diagnostics:?}"
  );
}

#[test]
fn class_expression_static_member_access_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "export const n: number = (class { static x = 1 }).x;";
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn new_class_expression_member_access_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "export const n: number = new (class { x = 1 })().x;";
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn class_expression_missing_static_member_reports_diagnostic() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "export const n = (class { static x = 1 }).y;";
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str()),
    "expected PROPERTY_DOES_NOT_EXIST diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn new_class_expression_missing_member_reports_diagnostic() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "export const n = new (class { x = 1 })().y;";
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str()),
    "expected PROPERTY_DOES_NOT_EXIST diagnostic, got {diagnostics:?}"
  );
}
