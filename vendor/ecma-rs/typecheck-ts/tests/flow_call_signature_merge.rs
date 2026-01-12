use typecheck_ts::{FileKey, MemoryHost, Program};

const SOURCE: &str = r#"
export function f() {
  const g = (x: string): string => x;
  return g("hi");
}
"#;

fn call_offset() -> u32 {
  let call_start = SOURCE
    .find(r#"g("hi")"#)
    .expect("call site exists") as u32;
  call_start + "g".len() as u32
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
  assert_eq!(program.display_type(sig.params[0].ty).to_string(), "string");
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
  assert_eq!(program.display_type(sig.params[0].ty).to_string(), "string");
}
