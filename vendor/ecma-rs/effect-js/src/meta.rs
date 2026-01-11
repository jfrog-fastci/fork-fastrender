use knowledge_base::Api;

use crate::db::CallSiteInfo;

fn has_semantics(api: &Api, expected: &str) -> bool {
  api
    .semantics
    .as_deref()
    .is_some_and(|s| s.eq_ignore_ascii_case(expected))
}

pub fn is_async(api: &Api) -> bool {
  api.async_.unwrap_or(false)
}

pub fn is_idempotent(api: &Api) -> Option<bool> {
  api.idempotent
}

pub fn is_deterministic(api: &Api) -> Option<bool> {
  api.deterministic
}

pub fn is_parallelizable(api: &Api) -> Option<bool> {
  api.parallelizable
}

pub fn parallelizable_at_callsite(api: &Api, callsite: &CallSiteInfo) -> bool {
  if let Some(p) = api.parallelizable {
    return p;
  }

  // Fallback heuristic for callback-driven collection APIs when the KB entry
  // doesn't specify `parallelizable` directly.
  if has_semantics(api, "Map")
    || has_semantics(api, "Filter")
    || api.name.ends_with(".map")
    || api.name.ends_with(".filter")
  {
    return callsite.callback_is_pure.unwrap_or(false)
      && !callsite.callback_uses_index.unwrap_or(false);
  }
  if has_semantics(api, "Reduce") || api.name.ends_with(".reduce") {
    return callsite.callback_is_pure.unwrap_or(false)
      && callsite.callback_is_associative.unwrap_or(false);
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::EffectDb;
  use hir_js::{BodyId, ExprId, StmtKind};

  fn first_stmt_expr(lowered: &hir_js::LowerResult) -> (BodyId, ExprId) {
    let root = lowered.root_body();
    let root_body = lowered.body(root).expect("root body");
    let first_stmt = *root_body.root_stmts.first().expect("root stmt");
    let stmt = &root_body.stmts[first_stmt.0 as usize];
    match stmt.kind {
      StmtKind::Expr(expr) => (root, expr),
      _ => panic!("expected expression statement"),
    }
  }

  #[test]
  fn meta_queries() {
    let db = EffectDb::load_default().unwrap();

    let fetch = db.api("fetch").unwrap();
    assert!(is_async(fetch));
    assert_eq!(is_parallelizable(fetch), Some(true));

    let sqrt = db.api("Math.sqrt").unwrap();
    assert_eq!(is_deterministic(sqrt), Some(true));
    assert_eq!(is_idempotent(sqrt), Some(true));
    assert!(!is_async(sqrt));
  }

  #[test]
  fn parallelizable_heuristic_array_map() {
    let db = EffectDb::load_default().unwrap();
    let api = db.api("Array.prototype.map").unwrap();

    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => x + 1);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = crate::callsite_info_for_args(&lowered, body, call_expr, db.kb());

    assert!(parallelizable_at_callsite(api, &callsite));
  }

  #[test]
  fn parallelizable_heuristic_array_map_index_callback() {
    let db = EffectDb::load_default().unwrap();
    let api = db.api("Array.prototype.map").unwrap();

    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => i);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = crate::callsite_info_for_args(&lowered, body, call_expr, db.kb());

    assert!(!parallelizable_at_callsite(api, &callsite));
  }

  #[test]
  fn parallelizable_heuristic_array_reduce_is_conservative_without_associativity() {
    let db = EffectDb::load_default().unwrap();
    let api = db.api("Array.prototype.reduce").unwrap();

    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.reduce((a, b) => a + b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = crate::callsite_info_for_args(&lowered, body, call_expr, db.kb());

    assert!(!parallelizable_at_callsite(api, &callsite));
  }

  #[test]
  fn parallelizable_heuristic_array_reduce_associative_bigint_add() {
    let db = EffectDb::load_default().unwrap();
    let api = db.api("Array.prototype.reduce").unwrap();

    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: bigint, b: bigint) => a + b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = crate::callsite_info_for_args(&lowered, body, call_expr, db.kb());

    assert!(parallelizable_at_callsite(api, &callsite));
  }
}
