use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
use knowledge_base::ApiDatabase;
use smallvec::SmallVec;

use crate::api::ApiId;
use crate::types::TypeProvider;

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
fn receiver_is_array(types: &impl crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_array(body, recv)
}

#[cfg(feature = "typed")]
fn receiver_is_string(types: &impl crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_string(body, recv)
}

#[cfg(feature = "typed")]
pub fn resolve_api_call_typed(
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &impl crate::types::TypeProvider,
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
    _ => None,
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
  pub call: ExprId,
  pub api: ApiId,
  pub receiver: Option<ExprId>,
  pub args: Vec<ExprId>,
}

pub fn resolve_call(
  lower: &LowerResult,
  body_id: BodyId,
  body: &Body,
  call_expr: ExprId,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Option<ResolvedCall> {
  let call = match body.exprs.get(call_expr.0 as usize).map(|e| &e.kind) {
    Some(ExprKind::Call(call)) => call,
    _ => return None,
  };

  // Be conservative around optional chaining and `new` calls.
  if call.optional || call.is_new {
    return None;
  }

  match body.exprs.get(call.callee.0 as usize).map(|e| &e.kind) {
    Some(ExprKind::Ident(name)) => {
      let name = lower.names.resolve(*name)?;
      // Rule A: static global function identifier.
      if db.get(name).is_none() {
        return None;
      }
      let api = ApiId::from_kb_name(name)?;
      Some(ResolvedCall {
        call: call_expr,
        api,
        receiver: None,
        args: call.args.iter().map(|arg| arg.expr).collect(),
      })
    }
    Some(ExprKind::Member(member)) => {
      if member.optional {
        return None;
      }
      if matches!(member.property, ObjectKey::Computed(_)) {
        return None;
      }

      // Rule A: fully-static member path like `JSON.parse` or `Promise.all`.
      if let Some((path, receiver)) = static_member_path(lower, body, call.callee) {
        if db.get(&path).is_some() {
          if let Some(api) = ApiId::from_kb_name(&path) {
            return Some(ResolvedCall {
              call: call_expr,
              api,
              receiver: Some(receiver),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
          }
        }
      }

      // Rule B: receiver-type-based prototype method calls (typed only).
      #[cfg(feature = "typed")]
      {
        let Some(types) = types else {
          return None;
        };

        let prop = static_object_key_name(lower, &member.property)?;

        if types.expr_is_array(body_id, member.object) {
          if let Some(api) = match prop {
            "map" => Some(ApiId::ArrayPrototypeMap),
            "filter" => Some(ApiId::ArrayPrototypeFilter),
            "reduce" => Some(ApiId::ArrayPrototypeReduce),
            "forEach" => Some(ApiId::ArrayPrototypeForEach),
            _ => None,
          } {
            if db.get(api.as_str()).is_some() {
              return Some(ResolvedCall {
                call: call_expr,
                api,
                receiver: Some(member.object),
                args: call.args.iter().map(|arg| arg.expr).collect(),
              });
            }
          }
        }

        if types.expr_is_string(body_id, member.object) {
          if let Some(api) = match prop {
            "toLowerCase" => Some(ApiId::StringPrototypeToLowerCase),
            "split" => Some(ApiId::StringPrototypeSplit),
            _ => None,
          } {
            if db.get(api.as_str()).is_some() {
              return Some(ResolvedCall {
                call: call_expr,
                api,
                receiver: Some(member.object),
                args: call.args.iter().map(|arg| arg.expr).collect(),
              });
            }
          }
        }
      }

      #[cfg(not(feature = "typed"))]
      {
        let _ = (types, body_id);
      }

      None
    }
    _ => None,
  }
}

fn static_object_key_name<'a>(lower: &'a LowerResult, key: &'a ObjectKey) -> Option<&'a str> {
  match key {
    ObjectKey::Ident(name) => lower.names.resolve(*name),
    ObjectKey::String(s) => Some(s.as_str()),
    ObjectKey::Number(_) | ObjectKey::Computed(_) => None,
  }
}

/// If `expr` is a member expression chain with a root identifier and only static
/// identifier/string properties (and no optional chaining), return the canonical
/// dotted path plus the call receiver expression.
fn static_member_path(lower: &LowerResult, body: &Body, expr: ExprId) -> Option<(String, ExprId)> {
  let mut props: SmallVec<[&str; 4]> = SmallVec::new();
  let mut cur = expr;
  let mut receiver: Option<ExprId> = None;

  loop {
    match body.exprs.get(cur.0 as usize).map(|e| &e.kind) {
      Some(ExprKind::Member(member)) => {
        if member.optional {
          return None;
        }
        if receiver.is_none() {
          receiver = Some(member.object);
        }
        let prop = static_object_key_name(lower, &member.property)?;
        props.push(prop);
        cur = member.object;
      }
      Some(ExprKind::Ident(root)) => {
        let root = lower.names.resolve(*root)?;
        let mut len = root.len();
        for prop in props.iter().rev() {
          len += 1 + prop.len();
        }

        let mut path = String::with_capacity(len);
        path.push_str(root);
        for prop in props.iter().rev() {
          path.push('.');
          path.push_str(prop);
        }

        return Some((path, receiver?));
      }
      _ => return None,
    }
  }
}
