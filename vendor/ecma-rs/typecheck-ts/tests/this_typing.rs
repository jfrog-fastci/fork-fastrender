use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn explicit_this_param_types_this_exprs() {
  let mut host = MemoryHost::default();
  let file = FileKey::new("main.ts");
  let source: Arc<str> = Arc::from("function f(this: { x: number }) { return this.x; }");
  host.insert(file.clone(), Arc::clone(&source));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  // Use the `.` in `this.x` so we query the full member expression span rather
  // than the nested `this` expression.
  let offset = source
    .rfind("this.x")
    .expect("this.x in return statement") as u32
    + "this".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn contextual_this_param_types_this_exprs() {
  let mut host = MemoryHost::default();
  let file = FileKey::new("main.ts");
  let source: Arc<str> = Arc::from(
    "const f: (this: { x: number }) => number = function () { return this.x; };",
  );
  host.insert(file.clone(), Arc::clone(&source));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  // Use the `.` in `this.x` so we query the full member expression span rather
  // than the nested `this` expression.
  let offset = source
    .rfind("this.x")
    .expect("this.x in return statement") as u32
    + "this".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}
