use std::sync::Arc;

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

