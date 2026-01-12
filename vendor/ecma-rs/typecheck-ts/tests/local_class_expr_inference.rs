mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn local_class_expr_inference() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
export function make() {
  const C = class {
    static x: number = 1;
    y: string = "a";
  };
  const n = C.x;
  const inst = new C();
  const s = inst.y;
  return [n, s];
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let return_elems = src.find("[n, s]").expect("return tuple literal");
  let n_offset = (return_elems + 1) as u32;
  let s_offset = (return_elems + 4) as u32;

  let n_ty = program.type_at(file_id, n_offset).expect("type at n");
  assert_eq!(program.display_type(n_ty).to_string(), "number");

  let s_ty = program.type_at(file_id, s_offset).expect("type at s");
  assert_eq!(program.display_type(s_ty).to_string(), "string");
}

