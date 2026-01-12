use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::FileId;
use hir_js::{lower_from_source, Body, BodyId, DefKind, LowerResult, NameId, NameInterner};
use typecheck_ts::check::hir_body::check_body_with_env;
use typecheck_ts::codes;
use types_ts_interned::{RelateCtx, TypeStore};

fn body_of<'a>(lowered: &'a LowerResult, names: &NameInterner, func: &str) -> (BodyId, &'a Body) {
  let def = lowered
    .defs
    .iter()
    .find(|d| names.resolve(d.name) == Some(func) && d.path.kind == DefKind::Function)
    .unwrap_or_else(|| panic!("missing function {func}"));
  let body_id = def.body.expect("function body");
  (body_id, lowered.body(body_id).unwrap())
}

fn run_flow(
  body_id: BodyId,
  body: &Body,
  names: &NameInterner,
  file: FileId,
  src: &str,
  store: &Arc<TypeStore>,
  initial: &HashMap<NameId, types_ts_interned::TypeId>,
) -> typecheck_ts::BodyCheckResult {
  let relate = RelateCtx::new(Arc::clone(store), store.options());
  let prim = store.primitive_ids();
  check_body_with_env(
    body_id,
    body,
    names,
    file,
    src,
    Arc::clone(store),
    initial,
    relate,
    None,
    prim.unknown,
    prim.unknown,
  )
}

#[test]
fn use_before_assignment_in_import_call_argument() {
  let src = r#"function f() {
  let spec: string;
  import(spec);
}"#;
  let lowered = lower_from_source(src).expect("lower");
  let (body_id, body) = body_of(&lowered, &lowered.names, "f");
  let store = TypeStore::new();
  let res = run_flow(
    body_id,
    body,
    &lowered.names,
    FileId(0),
    src,
    &store,
    &HashMap::new(),
  );

  let count = res
    .diagnostics()
    .iter()
    .filter(|diag| diag.code.as_str() == codes::USE_BEFORE_ASSIGNMENT.as_str())
    .count();
  assert_eq!(
    count,
    1,
    "expected one TS2454 inside import() argument, got {:?}",
    res.diagnostics()
  );
}

#[test]
fn use_before_assignment_in_tagged_template_substitution() {
  let src = r#"function tag(strings: any, ...values: any[]) {}
function f() {
  let x: number;
  tag`${x}`;
}"#;
  let lowered = lower_from_source(src).expect("lower");
  let (body_id, body) = body_of(&lowered, &lowered.names, "f");
  let store = TypeStore::new();
  let res = run_flow(
    body_id,
    body,
    &lowered.names,
    FileId(0),
    src,
    &store,
    &HashMap::new(),
  );

  let count = res
    .diagnostics()
    .iter()
    .filter(|diag| diag.code.as_str() == codes::USE_BEFORE_ASSIGNMENT.as_str())
    .count();
  assert_eq!(
    count,
    1,
    "expected one TS2454 inside tagged template substitution, got {:?}",
    res.diagnostics()
  );
}
