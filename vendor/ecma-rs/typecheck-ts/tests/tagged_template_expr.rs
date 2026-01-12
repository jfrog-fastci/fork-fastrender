use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

fn program_with_source(source: &str) -> Program {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from(source.to_string()));

  Program::new(host, vec![entry])
}

#[test]
fn tagged_template_substitutions_are_checked() {
  let program = program_with_source(
    r#"
export function tag(strings: any, ...values: any[]) { return 0; }
export const x = tag`${missing}`;
"#,
  );

  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected unknown identifier diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn tagged_template_expression_uses_tag_return_type() {
  let program = program_with_source(
    r#"
interface TemplateStringsArray {}
export function tag(strings: TemplateStringsArray, value: number): boolean { return true; }
export const b = tag`${1}`;
"#,
  );

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics in valid tagged template call: {diagnostics:?}"
  );

  let file_id = program.file_id(&FileKey::new("entry.ts")).expect("file id");
  let b_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("b"))
    .expect("export const b");

  let b_ty = program.type_of_def(b_def);
  assert_eq!(program.display_type(b_ty).to_string(), "boolean");
}

#[test]
fn tagged_template_argument_type_mismatch_is_reported() {
  let program = program_with_source(
    r#"
interface TemplateStringsArray {}
export function tag(strings: TemplateStringsArray, value: number): boolean { return true; }
export const bad = tag`${"x"}`;
"#,
  );

  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::ARGUMENT_TYPE_MISMATCH.as_str()),
    "expected argument type mismatch diagnostic, got {diagnostics:?}"
  );
}
