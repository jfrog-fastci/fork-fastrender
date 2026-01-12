use diagnostics::{Diagnostic, TextRange};
use typecheck_ts::codes;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn substring_range(source: &str, substring: &str) -> TextRange {
  let start = source
    .find(substring)
    .unwrap_or_else(|| panic!("missing substring {substring:?} in source"));
  let start = start as u32;
  let end = start + substring.len() as u32;
  TextRange::new(start, end)
}

fn assert_primary_span_equals(diag: &Diagnostic, source: &str, substring: &str) {
  assert_eq!(diag.primary.range, substring_range(source, substring));
}

#[test]
fn ts1194_export_list_without_module_specifier_points_at_statement() {
  let source = "export namespace M { export { x }; }\nconst x = 1;\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(diag.code.as_str(), codes::EXPORT_DECLARATION_IN_NAMESPACE.as_str());
  assert_primary_span_equals(diag, source, "export { x };");
}

#[test]
fn ts1194_export_list_without_module_specifier_includes_semicolon_after_comment() {
  let source = "export namespace M { export { x } /*c*/; }\nconst x = 1;\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(diag.code.as_str(), codes::EXPORT_DECLARATION_IN_NAMESPACE.as_str());
  assert_primary_span_equals(diag, source, "export { x } /*c*/;");
}

#[test]
fn ts1194_export_list_with_module_specifier_points_at_specifier() {
  let source = "declare namespace N { export { x } from \"mod\"; }\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(diag.code.as_str(), codes::EXPORT_DECLARATION_IN_NAMESPACE.as_str());
  assert_primary_span_equals(diag, source, "\"mod\"");
}

#[test]
fn ts1194_export_all_with_module_specifier_points_at_specifier() {
  let source = "declare namespace N { export * from \"mod\"; }\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(diag.code.as_str(), codes::EXPORT_DECLARATION_IN_NAMESPACE.as_str());
  assert_primary_span_equals(diag, source, "\"mod\"");
}

#[test]
fn ts1147_import_equals_require_in_namespace_points_at_specifier() {
  let source = "export namespace M { import foo = require(\"pkg\"); }\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(
    diag.code.as_str(),
    codes::IMPORT_IN_NAMESPACE_CANNOT_REFERENCE_MODULE.as_str()
  );
  assert_primary_span_equals(diag, source, "\"pkg\"");
}

#[test]
fn ts1147_es_import_in_namespace_points_at_specifier() {
  let source = "export namespace M { import * as M2 from \"M2\"; }\n";
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {diagnostics:?}");

  let diag = &diagnostics[0];
  assert_eq!(
    diag.code.as_str(),
    codes::IMPORT_IN_NAMESPACE_CANNOT_REFERENCE_MODULE.as_str()
  );
  assert_primary_span_equals(diag, source, "\"M2\"");
}

#[test]
fn no_ts1194_or_ts1147_inside_external_module_declaration() {
  let source = r#"
 declare module "foo" {
   import foo = require("pkg");
  export { x } from "mod";
}
"#;
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .all(|diag| diag.code.as_str() != "TS1194" && diag.code.as_str() != "TS1147"),
    "unexpected namespace-context diagnostics: {diagnostics:?}"
  );
}
