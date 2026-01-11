use crate::types::TypeProvider;
use hir_js::{Body, BodyId, ExprId, ExprKind, ObjectKey};
use knowledge_base::ApiDatabase;
use std::sync::Arc;

/// Rewrite `hir-js` call expressions into semantic-ops nodes when safe.
///
/// This pass is intentionally conservative:
/// - It only rewrites non-`new`, non-optional calls.
/// - It never rewrites computed-member calls (e.g. `obj[prop](...)` or `obj["x"](...)`).
/// - It never rewrites optional chaining calls (`obj?.method(...)` / `obj.method?.(...)`).
/// - It requires non-spread arguments (since `ExprKind::KnownApiCall` can't encode spreads).
/// - It preserves `BodyId`/`ExprId` numbering by rewriting in-place within each body's arenas.
pub fn annotate_known_api_calls(
  lower: &hir_js::LowerResult,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> hir_js::LowerResult {
  let mut out = lower.clone();

  for (&body_id, &idx) in lower.body_index.iter() {
    let body = &lower.bodies[idx];
    let Some(rewritten) = rewrite_body_known_api_calls(lower, body_id, body, db, types) else {
      continue;
    };
    out.bodies[idx] = rewritten;
  }

  out
}

fn rewrite_body_known_api_calls(
  lower: &hir_js::LowerResult,
  body_id: BodyId,
  body: &Arc<Body>,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Option<Arc<Body>> {
  let body_ref = body.as_ref();
  let mut rewritten: Option<Body> = None;

  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let ExprKind::Call(call) = &expr.kind else {
      continue;
    };
    if call.optional || call.is_new || call.args.iter().any(|arg| arg.spread) {
      continue;
    }

    if !callee_is_supported(body_ref, call.callee) {
      continue;
    }

    let call_expr_id = ExprId(idx as u32);
    let Some(api) = resolve_call_api_id(lower, body_id, body_ref, call_expr_id, db, types) else {
      continue;
    };

    let args = call.args.iter().map(|arg| arg.expr).collect();

    let new_kind = ExprKind::KnownApiCall { api, args };

    let rewritten_body = rewritten.get_or_insert_with(|| body_ref.clone());
    rewritten_body.exprs[idx].kind = new_kind;
  }

  rewritten.map(Arc::new)
}

fn callee_is_supported(body: &Body, expr_id: ExprId) -> bool {
  let expr_id = strip_transparent_wrappers(body, expr_id);
  let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
    return false;
  };
  match &expr.kind {
    ExprKind::Member(mem) => {
      if mem.optional {
        return false;
      }
      if !matches!(mem.property, ObjectKey::Ident(_)) {
        return false;
      }
      callee_is_supported(body, mem.object)
    }
    ExprKind::TypeAssertion { .. } | ExprKind::NonNull { .. } | ExprKind::Satisfies { .. } => {
      // `strip_transparent_wrappers` should have removed these.
      false
    }
    _ => true,
  }
}

fn strip_transparent_wrappers(body: &Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

fn resolve_call_api_id(
  lower: &hir_js::LowerResult,
  body_id: BodyId,
  body: &Body,
  call_expr: ExprId,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Option<hir_js::ApiId> {
  // 1) Import/require-aware resolver (node/web modules).
  if let Some(name) = crate::resolver::resolve_api_call(db, lower, body_id, call_expr) {
    return db.id_of(name).map(hir_api_id_from_kb);
  }

  let ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
    return None;
  };

  // 2) Statically-known global/member path (e.g. `JSON.parse`, `Math.sqrt`, `fetch`).
  if let Some(path) = static_callee_path(lower, body, call.callee) {
    if db.canonical_name(&path) == Some(path.as_str()) {
      if let Some(id) = db.id_of(&path) {
        return Some(hir_api_id_from_kb(id));
      }
    }
  }

  // 3) Typed receiver → prototype API mapping (e.g. `arr.map` → `Array.prototype.map`).
  resolve_typed_receiver_api(lower, body_id, body, call.callee, db, types)
}

fn static_callee_path(lower: &hir_js::LowerResult, body: &Body, expr_id: ExprId) -> Option<String> {
  let expr_id = strip_transparent_wrappers(body, expr_id);
  let expr = body.exprs.get(expr_id.0 as usize)?;

  match &expr.kind {
    ExprKind::Ident(name) => Some(lower.names.resolve(*name)?.to_string()),
    ExprKind::Member(mem) => {
      if mem.optional {
        return None;
      }
      let ObjectKey::Ident(prop) = mem.property else {
        return None;
      };
      let base = static_callee_path(lower, body, mem.object)?;
      let prop = lower.names.resolve(prop)?;
      Some(format!("{base}.{prop}"))
    }
    ExprKind::TypeAssertion { expr: inner, .. }
    | ExprKind::NonNull { expr: inner }
    | ExprKind::Satisfies { expr: inner, .. } => static_callee_path(lower, body, *inner),
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn resolve_typed_receiver_api(
  lower: &hir_js::LowerResult,
  body_id: BodyId,
  body: &Body,
  callee: ExprId,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Option<hir_js::ApiId> {
  let ExprKind::Member(mem) = &body.exprs[callee.0 as usize].kind else {
    return None;
  };
  if mem.optional {
    return None;
  }
  let ObjectKey::Ident(prop) = mem.property else {
    return None;
  };
  let prop = lower.names.resolve(prop)?;

  let types = types?;
  let api_name = match prop {
    "map" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.map"),
    "filter" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.filter"),
    "reduce" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.reduce"),
    "find" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.find"),
    "every" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.every"),
    "some" if types.expr_is_array(body_id, mem.object) => Some("Array.prototype.some"),
    "toLowerCase" if types.expr_is_string(body_id, mem.object) => Some("String.prototype.toLowerCase"),
    _ => None,
  }?;

  db.id_of(api_name).map(hir_api_id_from_kb)
}

#[cfg(not(feature = "typed"))]
fn resolve_typed_receiver_api(
  _lower: &hir_js::LowerResult,
  _body_id: BodyId,
  _body: &Body,
  _callee: ExprId,
  _db: &ApiDatabase,
  _types: Option<&dyn TypeProvider>,
) -> Option<hir_js::ApiId> {
  None
}

fn hir_api_id_from_kb(id: knowledge_base::ApiId) -> hir_js::ApiId {
  // `hir-js` `ApiId` uses the same stable 64-bit hash as `knowledge-base`.
  hir_js::ApiId(id.raw())
}
