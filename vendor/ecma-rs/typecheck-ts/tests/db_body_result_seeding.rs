mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{db, FileKey, MemoryHost, Program};

#[test]
fn check_body_seeds_body_result_for_db_type_at() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let src = "const x: number = 1;\nx;";
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let body = program.file_body(file_id).expect("file body");
  program.check_body(body);

  let typecheck_db = program.typecheck_db();
  let offset = src.rfind('x').expect("x offset") as u32;
  assert!(
    db::type_at(&typecheck_db, file_id, offset).is_some(),
    "expected db::type_at to find a cached BodyCheckResult after Program::check_body"
  );
}
