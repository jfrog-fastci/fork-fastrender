mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn derived_class_inherits_static_members() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  let source = r#"
class Base { static y: number = 1; }
class Derived extends Base {}
const z = Derived.y;
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = u32::try_from(source.find("Derived.y").expect("offset for Derived.y"))
    .expect("offset fits in u32")
    + "Derived.".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at Derived.y");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

