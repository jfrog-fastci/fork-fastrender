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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
  pub call: ExprId,
  /// Canonical knowledge-base API name (e.g. `JSON.parse`, `node:fs.readFile`).
  pub api: String,
  /// Stable identifier for a small curated subset of high-value APIs.
  pub api_id: Option<ApiId>,
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

  let callee = strip_transparent_wrappers(body, call.callee);

  match body.exprs.get(callee.0 as usize).map(|e| &e.kind) {
    Some(ExprKind::Ident(name)) => {
      let name = lower.names.resolve(*name)?;

      // Rule A: static global function identifier.
      if let Some(api) = db.get(name) {
        return Some(ResolvedCall {
          call: call_expr,
          api: api.name.clone(),
          api_id: ApiId::from_kb_name(&api.name),
          receiver: None,
          args: call.args.iter().map(|arg| arg.expr).collect(),
        });
      }

      // Rule C: imported bindings (typed only).
      #[cfg(feature = "typed")]
      {
        let Some(typed) = types.and_then(|types| types.as_typed_program()) else {
          return None;
        };
        let api = resolve_imported_ident_call(db, typed, body_id, callee)?;
        return Some(ResolvedCall {
          call: call_expr,
          api_id: ApiId::from_kb_name(&api),
          api,
          receiver: None,
          args: call.args.iter().map(|arg| arg.expr).collect(),
        });
      }

      #[cfg(not(feature = "typed"))]
      {
        let _ = (types, body_id);
        None
      }
    }
    Some(ExprKind::Member(member)) => {
      if member.optional {
        return None;
      }
      if matches!(member.property, ObjectKey::Computed(_)) {
        return None;
      }

      // Rule A: fully-static member path like `JSON.parse` or `Promise.all`.
      if let Some((path, receiver)) = static_member_path(lower, body, callee) {
        if let Some(api) = db.get(&path) {
          return Some(ResolvedCall {
            call: call_expr,
            api: api.name.clone(),
            api_id: ApiId::from_kb_name(&api.name),
            receiver: Some(receiver),
            args: call.args.iter().map(|arg| arg.expr).collect(),
          });
        }
      }

      #[cfg(feature = "typed")]
      {
        // Rule C: imported namespace/default/named-object bindings.
        if let Some(typed) = types.and_then(|types| types.as_typed_program()) {
          if let Some(api) = resolve_imported_member_call(db, typed, lower, body, body_id, callee) {
            return Some(ResolvedCall {
              call: call_expr,
              api_id: ApiId::from_kb_name(&api),
              api,
              receiver: Some(member.object),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
          }
        }

        // Rule B: receiver-type-based prototype method calls.
        let Some(types) = types else {
          return None;
        };

        let prop = static_object_key_name(lower, &member.property)?;

        if types.expr_is_array(body_id, member.object) {
          if let Some(api_id) = match prop {
            "map" => Some(ApiId::ArrayPrototypeMap),
            "filter" => Some(ApiId::ArrayPrototypeFilter),
            "reduce" => Some(ApiId::ArrayPrototypeReduce),
            "forEach" => Some(ApiId::ArrayPrototypeForEach),
            _ => None,
          } {
            if let Some(api) = db.get(api_id.as_str()) {
              return Some(ResolvedCall {
                call: call_expr,
                api: api.name.clone(),
                api_id: Some(api_id),
                receiver: Some(member.object),
                args: call.args.iter().map(|arg| arg.expr).collect(),
              });
            }
          }
        }

        if types.expr_is_string(body_id, member.object) {
          if let Some(api_id) = match prop {
            "toLowerCase" => Some(ApiId::StringPrototypeToLowerCase),
            "split" => Some(ApiId::StringPrototypeSplit),
            _ => None,
          } {
            if let Some(api) = db.get(api_id.as_str()) {
              return Some(ResolvedCall {
                call: call_expr,
                api: api.name.clone(),
                api_id: Some(api_id),
                receiver: Some(member.object),
                args: call.args.iter().map(|arg| arg.expr).collect(),
              });
            }
          }
        }

        None
      }

      #[cfg(not(feature = "typed"))]
      {
        let _ = (types, body_id);
        None
      }
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn join_api(module: &str, path: &[String]) -> String {
  if path.is_empty() {
    module.to_string()
  } else {
    format!("{module}.{}", path.join("."))
  }
}

#[cfg(feature = "typed")]
fn lookup_api(db: &ApiDatabase, module: &str, path: &[String]) -> Option<String> {
  if module.starts_with("node:") {
    let canonical = join_api(module, path);
    return db.get(&canonical).map(|api| api.name.clone());
  }

  let canonical_node = join_api(&format!("node:{module}"), path);
  if let Some(api) = db.get(&canonical_node) {
    return Some(api.name.clone());
  }

  let canonical = join_api(module, path);
  db.get(&canonical).map(|api| api.name.clone())
}

#[cfg(feature = "typed")]
fn resolve_imported_ident_call(
  db: &ApiDatabase,
  types: &crate::typed::TypedProgram,
  body_id: BodyId,
  callee: ExprId,
) -> Option<String> {
  let symbol = types.symbol_at_expr(body_id, callee)?;
  let info = types.program().symbol_info(symbol)?;
  let def = info.def?;

  if let Some(typecheck_ts::DefKind::Import(import)) = types.def_kind(def) {
    let typecheck_ts::ImportTarget::File(file_id) = import.target else {
      return None;
    };
    let module_key = types.file_key(file_id)?;
    return lookup_api(db, module_key.as_str(), &[import.original]);
  }

  // `symbol_at` may resolve directly to the imported definition (rather than the
  // local import binding). When that happens, fall back to using the definition's
  // file key as the module prefix.
  let body_file = types.file_for_body(body_id)?;
  let def_file = info
    .file
    .or_else(|| types.program().span_of_def(def).map(|span| span.file))?;
  if def_file == body_file {
    return None;
  }
  let module_key = types.file_key(def_file)?;
  let export_name = info.name.or_else(|| types.program().def_name(def))?;
  lookup_api(db, module_key.as_str(), &[export_name])
}

#[cfg(feature = "typed")]
fn flatten_member_chain(
  lower: &LowerResult,
  body: &Body,
  expr: ExprId,
) -> Option<(ExprId, Vec<String>)> {
  let mut cur = strip_transparent_wrappers(body, expr);
  let mut props = Vec::<String>::new();

  loop {
    cur = strip_transparent_wrappers(body, cur);
    match body.exprs.get(cur.0 as usize).map(|e| &e.kind) {
      Some(ExprKind::Member(member)) => {
        if member.optional {
          return None;
        }
        if matches!(member.property, ObjectKey::Computed(_)) {
          return None;
        }
        let prop = static_object_key_name(lower, &member.property)?;
        props.push(prop.to_string());
        cur = member.object;
      }
      _ => break,
    }
  }

  props.reverse();
  Some((strip_transparent_wrappers(body, cur), props))
}

#[cfg(feature = "typed")]
fn resolve_imported_member_call(
  db: &ApiDatabase,
  types: &crate::typed::TypedProgram,
  lower: &LowerResult,
  body: &Body,
  body_id: BodyId,
  callee: ExprId,
) -> Option<String> {
  let (base, mut member_path) = flatten_member_chain(lower, body, callee)?;
  let ExprKind::Ident(_) = body.exprs.get(base.0 as usize)?.kind else {
    return None;
  };

  let symbol = types.symbol_at_expr(body_id, base)?;
  let info = types.program().symbol_info(symbol)?;
  let def = info.def?;
  let Some(typecheck_ts::DefKind::Import(import)) = types.def_kind(def) else {
    return None;
  };
  let typecheck_ts::ImportTarget::File(file_id) = import.target else {
    return None;
  };

  // Namespace/default imports refer to the module value directly.
  if import.original != "*" && import.original != "default" {
    member_path.insert(0, import.original);
  }

  let module_key = types.file_key(file_id)?;
  lookup_api(db, module_key.as_str(), &member_path)
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
  let mut cur = strip_transparent_wrappers(body, expr);
  let mut receiver: Option<ExprId> = None;

  loop {
    cur = strip_transparent_wrappers(body, cur);
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
