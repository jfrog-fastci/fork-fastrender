use hir_js::{BodyId, ExprId, ExprKind, LowerResult, ObjectKey};

use crate::api::ApiId;

fn expr<'a>(lowered: &'a LowerResult, body: BodyId, id: ExprId) -> Option<&'a hir_js::Expr> {
  lowered.body(body)?.exprs.get(id.0 as usize)
}

fn ident_name<'a>(lowered: &'a LowerResult, name: hir_js::NameId) -> Option<&'a str> {
  lowered.names.resolve(name)
}

pub fn resolve_api_call_untyped(lowered: &LowerResult, body: BodyId, call_expr: ExprId) -> Option<ApiId> {
  let body_ref = lowered.body(body)?;
  let call = body_ref.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let callee = expr(lowered, body, call.callee)?;
  match &callee.kind {
    ExprKind::Ident(name) => {
      if ident_name(lowered, *name) == Some("fetch") {
        return Some(ApiId::Fetch);
      }
    }
    ExprKind::Member(member) => {
      if member.optional {
        return None;
      }

      let ObjectKey::Ident(prop) = member.property else {
        return None;
      };
      let prop = ident_name(lowered, prop)?;

      // Promise.all(...)
      if prop == "all" {
        let obj = expr(lowered, body, member.object)?;
        if let ExprKind::Ident(obj_name) = obj.kind {
          if ident_name(lowered, obj_name) == Some("Promise") {
            return Some(ApiId::PromiseAll);
          }
        }
      }

      // JSON.parse(...)
      if prop == "parse" {
        let obj = expr(lowered, body, member.object)?;
        if let ExprKind::Ident(obj_name) = obj.kind {
          if ident_name(lowered, obj_name) == Some("JSON") {
            return Some(ApiId::JsonParse);
          }
        }
      }
    }
    _ => {}
  }

  None
}

/// Best-effort API resolution without type information.
///
/// This is intentionally more permissive than [`resolve_api_call_untyped`]:
/// it may return `ApiId`s for prototype-like method names without proving the
/// receiver type (e.g. treating `x.map(...)` as `Array.prototype.map`).
pub fn resolve_api_call_best_effort_untyped(
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<ApiId> {
  if let Some(api) = resolve_api_call_untyped(lowered, body, call_expr) {
    return Some(api);
  }

  let body_ref = lowered.body(body)?;
  let call = body_ref.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let callee = expr(lowered, body, call.callee)?;
  let ExprKind::Member(member) = &callee.kind else {
    return None;
  };
  if member.optional {
    return None;
  }

  let ObjectKey::Ident(prop) = member.property else {
    return None;
  };
  let prop = ident_name(lowered, prop)?;

  match prop {
    "map" => Some(ApiId::ArrayPrototypeMap),
    "filter" => Some(ApiId::ArrayPrototypeFilter),
    "reduce" => Some(ApiId::ArrayPrototypeReduce),
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn receiver_is_array(types: &impl crate::typed::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  use types_ts_interned::TypeKind;
  let store = types.store();
  let ty = store.canon(types.type_of_expr(body, recv));
  match store.type_kind(ty) {
    TypeKind::Array { .. } => true,
    TypeKind::Ref { def, .. } => {
      let Some(name) = types.def_name(def) else {
        return false;
      };
      name == "Array" || name == "ReadonlyArray"
    }
    _ => false,
  }
}

#[cfg(feature = "typed")]
fn receiver_is_string(types: &impl crate::typed::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  use types_ts_interned::TypeKind;
  let store = types.store();
  let ty = store.canon(types.type_of_expr(body, recv));
  matches!(store.type_kind(ty), TypeKind::String | TypeKind::StringLiteral(_))
}

#[cfg(feature = "typed")]
fn receiver_is_named_ref(types: &impl crate::typed::TypeProvider, body: BodyId, recv: ExprId, expected: &str) -> bool {
  use types_ts_interned::TypeKind;
  let store = types.store();
  let ty = store.canon(types.type_of_expr(body, recv));
  let TypeKind::Ref { def, .. } = store.type_kind(ty) else {
    return false;
  };
  let Some(name) = types.def_name(def) else {
    return false;
  };
  name == expected
}

#[cfg(feature = "typed")]
pub fn resolve_api_call_typed(
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &impl crate::typed::TypeProvider,
) -> Option<ApiId> {
  // Always allow resolution for HIR-only safe APIs first.
  if let Some(api) = resolve_api_call_untyped(lowered, body, call_expr) {
    return Some(api);
  }

  let body_ref = lowered.body(body)?;
  let call = body_ref.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let callee = expr(lowered, body, call.callee)?;
  let ExprKind::Member(member) = &callee.kind else {
    return None;
  };
  if member.optional {
    return None;
  }

  let ObjectKey::Ident(prop) = member.property else {
    return None;
  };
  let prop = ident_name(lowered, prop)?;

  match prop {
    "map" if receiver_is_array(types, body, member.object) => Some(ApiId::ArrayPrototypeMap),
    "filter" if receiver_is_array(types, body, member.object) => {
      Some(ApiId::ArrayPrototypeFilter)
    }
    "reduce" if receiver_is_array(types, body, member.object) => {
      Some(ApiId::ArrayPrototypeReduce)
    }
    "toLowerCase" if receiver_is_string(types, body, member.object) => Some(ApiId::StringPrototypeToLowerCase),
    "then" if receiver_is_named_ref(types, body, member.object, "Promise") => Some(ApiId::PromisePrototypeThen),
    "get" if receiver_is_named_ref(types, body, member.object, "Map") => Some(ApiId::MapPrototypeGet),
    _ => None,
  }
}
