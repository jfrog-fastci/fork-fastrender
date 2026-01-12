mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn local_class_type_reference_resolves() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"function f() {
  class C { x: number = 1; }
  const c: C = new C();
  return c.x;
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    !diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNRESOLVED_TYPE_REFERENCE.as_str()),
    "expected no unresolved type reference diagnostics, got: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("c.x").expect("offset for c.x") as u32 + 2;
  let ty = program.type_at(file_id, offset).expect("type at c.x");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

