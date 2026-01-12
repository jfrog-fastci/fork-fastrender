use typecheck_ts::{FileKey, MemoryHost, Program};

// Regression test for the base+flow merge logic inside the DB-backed body
// checker.
//
// The flow checker tracks a per-call-expression `CallSignatureState` and
// encodes both “unresolved” and “conflict” states as `None` in its final
// `FlowBodyCheckTables`. When those tables are merged into the base checker’s
// `BodyCheckResult`, a naive “always overwrite” policy can erase a correct base
// signature selection.
//
// This source triggers a flow conflict by forcing the flow checker to resolve
// the same call expression with two different instantiated signatures:
//   - Initially `x` is inferred as `string` from its initializer.
//   - After the loop back-edge merges, `x` becomes `string | number`.
//
// The base checker respects the explicit `string | number` annotation at the
// declaration and records the union-instantiated signature for `id(x)`.
const SOURCE: &str = r#"
declare function id<T>(value: T): T;

declare const cond: boolean;
declare const s: string;
declare const n: number;

export function f() {
  let x: string | number = s;
  while (cond) {
    x = n;
  }
  return id(x);
}
"#;

fn call_offset() -> u32 {
  let call_start = SOURCE
    .rfind("id(x)")
    .expect("call site exists") as u32;
  call_start + "id".len() as u32 // points at `(` in `id(x)`
}

#[test]
fn flow_merge_preserves_base_call_signature_in_state_checker() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("entry.ts");
  host.insert(key.clone(), SOURCE);
  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file = program.file_id(&key).expect("entry file id");
  let sig_id = program
    .call_signature_at(file, call_offset())
    .expect("call signature recorded");
  let sig = program.signature(sig_id).expect("signature in store");
  assert_eq!(program.display_type(sig.params[0].ty).to_string(), "string | number");
}

#[test]
fn flow_merge_preserves_base_call_signature_in_db_checker() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("entry.ts");
  host.insert(key.clone(), SOURCE);
  let program = Program::new(host, vec![key.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file = program.file_id(&key).expect("entry file id");
  let offset = call_offset();
  let (body, _) = program.expr_at(file, offset).expect("call expr body");

  let body_result = program.check_body(body);
  let (expr, _) = body_result.expr_at(offset).expect("call expr in body");
  let sig_id = body_result.call_signature(expr).expect("call signature recorded");

  let sig = program.signature(sig_id).expect("signature in store");
  assert_eq!(program.display_type(sig.params[0].ty).to_string(), "string | number");
}

