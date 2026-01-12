mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn function_expression_initializer_infers_return_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("input.ts");
  let src = r#"const f = (x: number) => x + 1;
const y = f(1);
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let y_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("y"))
    .expect("y def");
  let y_ty = program.type_of_def(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "number");

  let call_offset = src.find("f(1)").expect("call site") as u32 + 1;
  let call_ty = program.type_at(file_id, call_offset).expect("type at f(1)");
  assert_eq!(program.display_type(call_ty).to_string(), "number");
}
