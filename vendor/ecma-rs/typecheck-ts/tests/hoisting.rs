mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn var_hoisting_across_blocks() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"export function f() {
  if (true) { var x = 1; }
  return x;
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src.find("return x").expect("offset for return x") as u32 + "return ".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type at x");
  let display = program.display_type(ty).to_string();
  assert!(
    display == "number" || display == "1",
    "expected type of x to be number (or literal 1), got {display}",
  );
}

#[test]
fn function_decl_hoisting() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let src = r#"export function f() {
  g();
  function g() { return 1; }
  return 0;
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

