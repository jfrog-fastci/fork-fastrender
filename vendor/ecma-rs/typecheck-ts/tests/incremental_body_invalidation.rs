use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn inferred_function_return_updates_after_set_file_text() {
  let mut host = MemoryHost::default();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    Arc::from("export function f() { return 1; }\n".to_string()),
  );

  let mut program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let f_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("f"))
    .expect("f def");

  let before = program.display_type(program.type_of_def(f_def)).to_string();
  assert!(
    before.contains("=> number"),
    "expected inferred return type for f() to be number, got {before}"
  );

  program.set_file_text(
    file_id,
    Arc::from("export function f() { return \"x\"; }\n".to_string()),
  );

  let f_def_after = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("f"))
    .expect("f def after edit");
  assert_eq!(
    f_def, f_def_after,
    "DefId should remain stable across body-only edits"
  );

  let after = program.display_type(program.type_of_def(f_def)).to_string();
  assert!(
    after.contains("=> string"),
    "expected inferred return type for f() to be string, got {after}"
  );
}

#[test]
fn inferred_initializer_updates_after_set_file_text() {
  let mut host = MemoryHost::default();
  let file = FileKey::new("init.ts");
  host.insert(
    file.clone(),
    Arc::from("export const x = 1 + 2;\n".to_string()),
  );

  let mut program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let x_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("x"))
    .expect("x def");

  let before = program.display_type(program.type_of_def(x_def)).to_string();
  assert_eq!(before, "number", "expected x to be number, got {before}");

  program.set_file_text(
    file_id,
    Arc::from("export const x = \"a\" + \"b\";\n".to_string()),
  );

  let x_def_after = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("x"))
    .expect("x def after edit");
  assert_eq!(
    x_def, x_def_after,
    "DefId should remain stable across initializer-only edits"
  );

  let after = program.display_type(program.type_of_def(x_def)).to_string();
  assert_eq!(after, "string", "expected x to be string, got {after}");
}

#[test]
fn expr_at_does_not_use_stale_spans_after_set_file_text() {
  let mut host = MemoryHost::default();
  let file = FileKey::new("spans.ts");
  let source = "export function f() { return 1; }\n";
  host.insert(file.clone(), Arc::from(source.to_string()));

  let mut program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let offset = source.find('1').expect("literal offset") as u32;

  let (body, _expr) = program.expr_at(file_id, offset).expect("expr at literal");
  // Seed a DB-backed body result so `expr_at` prefers typed spans. These spans
  // must be invalidated when the file text changes, otherwise the old offsets
  // could still resolve to expressions in the previous version.
  let _ = program.check_body(body);

  // Insert a comment immediately before the `1` literal.
  let edited = source.replace("return 1;", "return /*pad*/ 1;");
  program.set_file_text(file_id, Arc::from(edited));

  assert!(
    program.expr_at(file_id, offset).is_none(),
    "expected old offset to be inside inserted comment, not an expression"
  );
}

#[test]
fn return_type_inference_updates_across_edits() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  let key = FileKey::new("main.ts");
  let source = r#"
export function f() { return 1; }
export const x = f();
"#;
  host.insert(key.clone(), Arc::from(source));

  let mut program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");

  // `no_default_lib=true` intentionally skips loading bundled libs, which means
  // the checker will emit missing-global-type diagnostics (TS2318). We still
  // expect body-based inference to work and update across edits.
  let _ = program.check();

  let exports = program.exports_of(file_id);
  let x_ty = exports
    .get("x")
    .and_then(|entry| entry.type_id)
    .expect("x export type");
  assert_eq!(program.display_type(x_ty).to_string(), "number");

  let updated = r#"
export function f() { return "a"; }
export const x = f();
"#;
  program.set_file_text(file_id, Arc::from(updated));

  let _ = program.check();

  let exports = program.exports_of(file_id);
  let x_ty = exports
    .get("x")
    .and_then(|entry| entry.type_id)
    .expect("x export type after edit");
  assert_eq!(program.display_type(x_ty).to_string(), "string");
}

#[test]
fn inferred_types_update_in_dependents_after_dependency_body_edit() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  let entry = FileKey::new("main.ts");
  let dep = FileKey::new("dep.ts");
  host.insert(
    entry.clone(),
    Arc::from("import { foo } from \"./dep\";\nexport const x = foo();\n"),
  );
  host.insert(dep.clone(), Arc::from("export function foo() { return 1; }\n"));
  host.link(entry.clone(), "./dep", dep.clone());

  let mut program = Program::new(host, vec![entry.clone()]);
  let entry_id = program.file_id(&entry).expect("entry file id");
  let dep_id = program.file_id(&dep).expect("dep file id");

  // As in `return_type_inference_updates_across_edits`, we allow the missing
  // global lib diagnostics but still expect inferred types to update correctly.
  let _ = program.check();

  let exports = program.exports_of(entry_id);
  let x_ty = exports
    .get("x")
    .and_then(|entry| entry.type_id)
    .expect("x export type");
  assert_eq!(program.display_type(x_ty).to_string(), "number");

  program.set_file_text(dep_id, Arc::from("export function foo() { return \"a\"; }\n"));
  let _ = program.check();

  let exports = program.exports_of(entry_id);
  let x_ty = exports
    .get("x")
    .and_then(|entry| entry.type_id)
    .expect("x export type after dep edit");
  assert_eq!(program.display_type(x_ty).to_string(), "string");
}

#[test]
fn body_check_result_spans_update_across_edits() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  let key = FileKey::new("main.ts");
  let source = "export const x = 1 + 2;\n";
  host.insert(key.clone(), Arc::from(source));

  let mut program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let body = program.file_body(file_id).expect("file body");

  let initial = program.check_body(body);
  let expr_offset = source.rfind('2').expect("`2` offset") as u32;
  let (expr, _) = initial.expr_at(expr_offset).expect("expr at `2`");
  let initial_span = initial.expr_span(expr).expect("expr span");

  let prefix = "/* inserted */\n";
  let updated_source = format!("{prefix}{source}");
  program.set_file_text(file_id, Arc::from(updated_source.as_str()));

  let updated_body = program.file_body(file_id).expect("file body after edit");
  assert_eq!(
    updated_body, body,
    "expected file body id to remain stable across incremental edit"
  );

  let updated = program.check_body(body);
  let updated_offset = expr_offset + prefix.len() as u32;
  let (updated_expr, _) = updated.expr_at(updated_offset).expect("expr at `2` after edit");
  let updated_span = updated.expr_span(updated_expr).expect("expr span after edit");

  assert_eq!(
    updated_span.start,
    initial_span.start + prefix.len() as u32,
    "expected expr span start to shift after edit"
  );
  assert_eq!(
    updated_span.end,
    initial_span.end + prefix.len() as u32,
    "expected expr span end to shift after edit"
  );
}
