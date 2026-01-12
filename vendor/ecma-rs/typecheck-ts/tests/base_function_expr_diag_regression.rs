use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn base_checker_uses_arrow_function_def_types_for_calls() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let source = r#"
export function outer() {
  const f = (x: number) => x + 1;
  const y: number = f(1);
  return y;
}
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let outer_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("outer"))
    .expect("outer definition");
  let outer_ty = program.type_of_def(outer_def);
  let sigs = program.call_signatures(outer_ty);
  assert_eq!(
    sigs.len(),
    1,
    "expected one call signature for outer, got {sigs:?}"
  );
  let ret_ty = sigs[0].signature.ret;
  assert_eq!(program.display_type(ret_ty).to_string(), "number");
}

#[test]
fn base_checker_uses_class_expression_def_types_for_static_props() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("a.ts");
  let source = r#"
export function make() {
  const C = class { static x: number = 1; };
  const y: number = C.x;
  return y;
}
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

