use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::FileId;
use hir_js::{lower_from_source, Body, BodyId, DefKind, LowerResult, NameInterner};
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
  initial: &HashMap<hir_js::NameId, types_ts_interned::TypeId>,
) -> typecheck_ts::BodyCheckResult {
  let relate = RelateCtx::new(Arc::clone(store), store.options());
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
  )
}

#[test]
fn substitution_is_flow_checked() {
  let src = "function f() { let x: number; return `${x}`; }";
  let lowered = lower_from_source(src).expect("lower");
  let (body_id, body) = body_of(&lowered, &lowered.names, "f");
  let store = TypeStore::new();
  let initial = HashMap::new();
  let res = run_flow(
    body_id,
    body,
    &lowered.names,
    FileId(0),
    src,
    &store,
    &initial,
  );
  assert!(
    res
      .diagnostics()
      .iter()
      .any(|d| d.code.as_str() == codes::USE_BEFORE_ASSIGNMENT.as_str()),
    "expected diagnostics to include {} but got {:#?}",
    codes::USE_BEFORE_ASSIGNMENT.as_str(),
    res.diagnostics()
  );
}

