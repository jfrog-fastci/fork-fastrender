use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn generic_callable_assigns_to_non_generic_function_type() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  host.insert(
    file.clone(),
    Arc::from(
      r#"
export const id = <T>(x: T) => x;
export type NumFn = (x: number) => number;
export const f: (x: number) => number = id;
export const f_alias: NumFn = id;
export const g: (x: string) => string = id;
export let h: (x: number) => number;
h = id;
export const obj: { f: (x: number) => number } = { f: (x: number) => x };
obj.f = id;
"#,
    ),
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn generic_callable_assignment_rejects_incompatible_return_type() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  host.insert(
    file.clone(),
    Arc::from(
      r#"
export const id = <T>(x: T) => x;
export const bad: (x: number) => string = id;
"#,
    ),
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    !diagnostics.is_empty(),
    "expected type mismatch diagnostics, got none"
  );
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected TS2322 diagnostic, got {diagnostics:?}"
  );
}
