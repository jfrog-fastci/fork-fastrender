mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn class_expr_type_in_class_body_is_not_unknown() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"export class Holder {
  field = class { static x = 1 };
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src
    .find("field = class")
    .expect("offset for class expression") as u32
    + "field = ".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at class expression");
  let display = program.display_type(ty).to_string();
  assert_ne!(display, "unknown", "type at class expression: {display}");
}

