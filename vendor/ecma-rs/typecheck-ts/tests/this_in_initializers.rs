mod common;

use typecheck_ts::lib_support::{CompilerOptions, ScriptTarget};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn this_in_class_static_block_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"class C { static x: number = 1; static { const y = this.x; } }"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("this.x").expect("this.x offset") as u32 + "this.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn this_in_instance_field_initializer_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"class C { x: number = 1; y = this.x; }"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("this.x").expect("this.x offset") as u32 + "this.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at this.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn type_of_def_inferred_from_this_in_initializer_body_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"
class C {
  x: number = 1;
  m() { const y = this.x; return y; }
}
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
    .expect("definition for y");

  assert_eq!(
    program.display_type(program.type_of_def(y_def)).to_string(),
    "number"
  );
}

