mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn delete_expr_is_usable_as_boolean_argument() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("entry.ts");
  host.insert(
    file.clone(),
    r#"
declare function takesBool(x: boolean): void;
export function f(obj: { x: number }) {
  takesBool(delete obj.x);
}
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn delete_expr_is_assignable_to_boolean() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("entry.ts");
  host.insert(
    file.clone(),
    r#"
const obj = { x: 1 };
export const ok: boolean = delete obj.x;
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

