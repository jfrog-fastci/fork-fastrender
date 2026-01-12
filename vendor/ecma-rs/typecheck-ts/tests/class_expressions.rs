mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn class_expression_initializer_has_concrete_type_with_no_implicit_any() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    no_implicit_any: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export const C = class {};\n");

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::IMPLICIT_ANY.as_str()),
    "unexpected implicit-any diagnostics: {diagnostics:?}"
  );
  assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
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

#[test]
fn class_expression_new_call_argument_checked() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let source = r#"
new (class { constructor(x: number) {} })("oops");
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::ARGUMENT_TYPE_MISMATCH.as_str()
        || diag.code.as_str() == codes::NO_OVERLOAD.as_str()
    }),
    "expected argument type mismatch diagnostic, got {diagnostics:?}"
  );
}
