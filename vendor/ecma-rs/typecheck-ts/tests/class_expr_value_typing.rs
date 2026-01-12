mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn class_expression_initializer_produces_value_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("input.ts");
  let src = r#"const C = class {
  static x: number = 1;
  y: string = "a";
};
const n = C.x;
const inst = new C();
const s = inst.y;
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");

  let defs = program.definitions_in_file(file_id);
  let n_def = defs
    .iter()
    .copied()
    .find(|def| program.def_name(*def).as_deref() == Some("n"))
    .expect("n def");
  let s_def = defs
    .iter()
    .copied()
    .find(|def| program.def_name(*def).as_deref() == Some("s"))
    .expect("s def");

  let n_ty = program.type_of_def(n_def);
  assert_eq!(program.display_type(n_ty).to_string(), "number");

  let s_ty = program.type_of_def(s_def);
  assert_eq!(program.display_type(s_ty).to_string(), "string");
}
