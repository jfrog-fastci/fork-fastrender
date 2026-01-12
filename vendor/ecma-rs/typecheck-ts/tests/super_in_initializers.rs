mod common;

use typecheck_ts::lib_support::{CompilerOptions, ScriptTarget};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn super_in_class_static_block_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"
class Base { static y: number = 1; }
class Derived extends Base { static { const z = super.y; } }
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("super.y").expect("super.y offset") as u32 + "super.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.y");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_in_instance_field_initializer_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"
class Base { x: number = 1; }
class Derived extends Base { y = super.x; }
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("super.x").expect("super.x offset") as u32 + "super.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_in_static_field_initializer_is_typed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let src = r#"
class Base { static y: string = "ok"; }
class Derived extends Base { static z = super.y; }
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("super.y").expect("super.y offset") as u32 + "super.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at super.y");
  assert_eq!(program.display_type(ty).to_string(), "string");
}

