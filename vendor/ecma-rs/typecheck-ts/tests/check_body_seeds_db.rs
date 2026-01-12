use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn check_body_seeds_results_into_type_at_cached_query() {
  let mut host = MemoryHost::default();
  let source = "let x: number = 1;\nexport const y = x + 1;\n";
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");

  // `Program::type_at_cached` relies on `BodyCheckResult`s seeded into the internal salsa DB.
  // Ensure `Program::check_body` makes the result available to DB-backed queries.
  //
  // Note: offsets can land inside nested bodies (e.g. synthesized initializer bodies),
  // so seed the specific body covering the offset rather than assuming the file's
  // top-level body owns the expression.
  let offset = source
    .rfind("x + 1")
    .expect("offset of x usage")
    .try_into()
    .expect("offset fits");

  let (body, _) = program
    .expr_at(file_id, offset)
    .expect("offset should be inside an expression");
  program.check_body(body);

  let ty = program
    .type_at_cached(file_id, offset)
    .expect("cached type at offset");
  assert_eq!(program.display_type(ty).to_string(), "number");
}
