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

fn has_use_before_assignment(diags: &[diagnostics::Diagnostic]) -> bool {
  diags
    .iter()
    .any(|d| d.code.as_str() == codes::USE_BEFORE_ASSIGNMENT.as_str())
}

#[test]
fn computed_key_expression_is_evaluated() {
  let src = "function f() { let k: string; ({ [k]: 1 }); }";
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
  assert!(has_use_before_assignment(res.diagnostics()));
}

#[test]
fn computed_key_value_expression_is_evaluated() {
  let src = "function f() { let v: number; ({ [\"a\"]: v }); }";
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
  assert!(has_use_before_assignment(res.diagnostics()));
}

#[test]
fn computed_getter_name_expression_is_evaluated() {
  let src = "function f() { let k: string; ({ get [k]() { return 1; } }); }";
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
  assert!(has_use_before_assignment(res.diagnostics()));
}
