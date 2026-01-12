use std::sync::Arc;

use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn computed_key_object_pattern_infers_property_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let source = r#"
const obj = { a: 1 };
const { ["a"]: x } = obj;
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let x_offset = source.find(": x").expect("x offset") as u32 + ": ".len() as u32;
  let x_ty = program.type_at(file_id, x_offset).expect("type of x binding");
  assert_eq!(program.display_type(x_ty).to_string(), "number");
}

#[test]
fn unknown_identifier_in_object_pattern_computed_key_is_reported() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let source = r#"
const obj = { a: 1 };
const { [missing]: x } = obj;
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected UNKNOWN_IDENTIFIER diagnostic, got: {diagnostics:?}"
  );
}

#[test]
fn use_before_assignment_in_object_pattern_computed_key() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let source = r#"
function f() {
  let k: string;
  const obj = { a: 1 };
  const { [k]: x } = obj;
  return x;
}
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::USE_BEFORE_ASSIGNMENT.as_str()),
    "expected USE_BEFORE_ASSIGNMENT diagnostic, got: {diagnostics:?}"
  );
}
