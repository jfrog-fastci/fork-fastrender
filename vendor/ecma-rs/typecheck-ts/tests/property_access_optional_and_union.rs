use std::sync::Arc;

use diagnostics::TextRange;
use typecheck_ts::codes;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn optional_property_reads_include_undefined() {
  let mut host = MemoryHost::new();
  let source = r#"
type Obj = { a?: number };
declare const o: Obj;
export const x = o.a;
"#;
  let file = FileKey::new("optional_prop.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let x_def = exports.get("x").and_then(|entry| entry.def).expect("x def");
  let ty = program.type_of_def(x_def);
  assert_eq!(program.display_type(ty).to_string(), "undefined | number");
}

#[test]
fn union_missing_property_emits_ts2339() {
  let mut host = MemoryHost::new();
  let source = r#"
type U = { a: number } | { b: string };
declare const u: U;
u.a;
"#;
  let file = FileKey::new("union_missing.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  let file_id = program.file_id(&file).expect("file id");
  let ts2339: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str())
    .collect();
  assert_eq!(
    ts2339.len(),
    1,
    "expected exactly one TS2339 diagnostic, got {diagnostics:?}"
  );

  let start = source.rfind("u.a").expect("member access") as u32 + 2;
  let expected = TextRange::new(start, start + 1);
  assert_eq!(ts2339[0].primary.file, file_id);
  assert_eq!(ts2339[0].primary.range, expected);
}

#[test]
fn union_common_property_returns_union_type() {
  let mut host = MemoryHost::new();
  let source = r#"
type U = { a: number } | { a: string };
declare const u: U;
export const x = u.a;
"#;
  let file = FileKey::new("union_common.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let x_def = exports.get("x").and_then(|entry| entry.def).expect("x def");
  let ty = program.type_of_def(x_def);
  assert_eq!(program.display_type(ty).to_string(), "string | number");
}

