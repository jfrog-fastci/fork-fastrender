use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

fn check_no_diagnostics(source: &str) {
  let mut host = MemoryHost::default();
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn type_param_constraint_allows_member_access() {
  check_no_diagnostics(
    r#"
export function f<T extends { a: number }>(x: T) {
  const n: number = x.a;
  return n;
}
"#,
  );
}

#[test]
fn contextual_generic_signature_pushes_type_params_into_scope() {
  check_no_diagnostics(
    r#"
export const g: <T extends { a: number }>(x: T) => number = x => x.a;
"#,
  );
}

#[test]
fn type_param_constraint_allows_index_access() {
  check_no_diagnostics(
    r#"
type Rec = { [k: string]: number };

export function h<T extends Rec>(x: T, k: string) {
  const n: number = x[k];
  return n;
}
"#,
  );
}

