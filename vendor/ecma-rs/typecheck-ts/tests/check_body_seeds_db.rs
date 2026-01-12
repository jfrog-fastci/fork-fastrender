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
  let body = program.file_body(file_id).expect("file body");

  // `Program::type_at_cached` relies on `BodyCheckResult`s seeded into the internal salsa DB.
  // Ensure `Program::check_body` makes the result available to DB-backed queries.
  program.check_body(body);

  let offset = source
    .rfind("x + 1")
    .expect("offset of x usage")
    .try_into()
    .expect("offset fits");

  let ty = program
    .type_at_cached(file_id, offset)
    .expect("cached type at offset");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

