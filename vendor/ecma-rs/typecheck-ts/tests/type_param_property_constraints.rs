use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn generic_type_param_constraint_allows_property_access() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "function f<T extends { a: string }>(x: T) {\n  return x.a;\n}\n";
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let offset = source.find("x.a").expect("x.a present in source") as u32 + 2;
  let ty = program.type_at(file_id, offset).expect("type at x.a");
  assert_eq!(program.display_type(ty).to_string(), "string");
}

#[test]
fn contextual_generic_signature_allows_property_access() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = "type Fn = <T extends { a: string }>(x: T) => string;\nconst f: Fn = x => x.a;\n";
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

