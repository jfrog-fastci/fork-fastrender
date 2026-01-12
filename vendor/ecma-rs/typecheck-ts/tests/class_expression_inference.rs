mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program, TypeKindSummary};

#[test]
fn class_expression_initializer_infers_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    no_implicit_any: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
export const C = class {};
export const c = new C();
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let c_def = exports.get("C").and_then(|e| e.def).expect("export def C");
  let init = program.var_initializer(c_def).expect("initializer for C");
  let init_ty = program.type_of_expr(init.body, init.expr);
  assert!(
    !matches!(program.type_kind(init_ty), TypeKindSummary::Unknown),
    "class expression initializer should not have an unknown type"
  );
}
