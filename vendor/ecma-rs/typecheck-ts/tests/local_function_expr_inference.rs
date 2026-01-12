mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn local_function_expr_inference() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
export function outer() {
  const f = (x: number) => x + 1;
  return f(1);
}
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let outer = exports.get("outer").expect("outer export");
  let outer_ty = outer.type_id.expect("type for outer export");
  let sigs = program.call_signatures(outer_ty);
  assert_eq!(sigs.len(), 1, "expected a single call signature");
  let ret = sigs[0].signature.ret;
  assert_eq!(program.display_type(ret).to_string(), "number");
}

