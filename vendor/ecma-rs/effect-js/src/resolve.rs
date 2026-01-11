use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
#[cfg(feature = "hir-semantic-ops")]
use hir_js::ArrayElement;
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

      // globalThis.fetch(...) / window.fetch(...)
      if prop == "fetch" {
        let obj = expr(lowered, body, member.object)?;
        if let ExprKind::Ident(obj_name) = &obj.kind {
          if matches!(
            ident_name(lowered, *obj_name),
            Some("globalThis" | "window" | "self" | "global")
          ) {
            return Some(ApiId::Fetch);
          }
        }
      }

      // Promise.all(...)
      if prop == "all" {
        let obj = expr(lowered, body, member.object)?;
        match &obj.kind {
          ExprKind::Ident(obj_name) => {
            if ident_name(lowered, *obj_name) == Some("Promise") {
              return Some(ApiId::PromiseAll);
            }
          }
          ExprKind::Member(inner) => {
            if inner.optional {
              return None;
            }
            let ObjectKey::Ident(obj_name) = &inner.property else {
              return None;
            };
            if ident_name(lowered, *obj_name) != Some("Promise") {
              return None;
            }
            let base = expr(lowered, body, inner.object)?;
            if let ExprKind::Ident(base_name) = &base.kind {
              if matches!(
                ident_name(lowered, *base_name),
                Some("globalThis" | "window" | "self" | "global")
              ) {
                return Some(ApiId::PromiseAll);
              }
            }
          }
          _ => {}
        }
      }

      // JSON.parse(...)
      if prop == "parse" {
        let obj = expr(lowered, body, member.object)?;
        match &obj.kind {
          ExprKind::Ident(obj_name) => {
            if ident_name(lowered, *obj_name) == Some("JSON") {
              return Some(ApiId::JsonParse);
            }
          }
          ExprKind::Member(inner) => {
            if inner.optional {
              return None;
            }
            let ObjectKey::Ident(obj_name) = &inner.property else {
              return None;
            };
            if ident_name(lowered, *obj_name) != Some("JSON") {
              return None;
            }
            let base = expr(lowered, body, inner.object)?;
            if let ExprKind::Ident(base_name) = &base.kind {
              if matches!(
                ident_name(lowered, *base_name),
                Some("globalThis" | "window" | "self" | "global")
              ) {
                return Some(ApiId::JsonParse);
              }
            }
          }
          _ => {}
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
fn receiver_is_array(types: &dyn crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_array(body, recv)
}

#[cfg(feature = "typed")]
fn receiver_is_array_method_receiver(
  lowered: &LowerResult,
  body: BodyId,
  recv: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> bool {
  if receiver_is_array(types, body, recv) {
    return true;
  }

  // `typecheck-ts` can leave intermediate chain results as `unknown`, so allow
  // `arr.map(...).filter(...)` to be treated as an array receiver when the
  // receiver is itself a proven array-returning array method call.
  let Some(expr) = expr(lowered, body, recv) else {
    return false;
  };
  if !matches!(expr.kind, ExprKind::Call(_)) {
    return false;
  }

  matches!(
    resolve_api_call_typed(lowered, body, recv, types),
    Some(ApiId::ArrayPrototypeMap | ApiId::ArrayPrototypeFilter)
  )
}

#[cfg(feature = "typed")]
fn receiver_is_string(types: &dyn crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_string(body, recv)
}

#[cfg(feature = "typed")]
fn receiver_is_named_ref(
  types: &dyn crate::types::TypeProvider,
  body: BodyId,
  recv: ExprId,
  expected: &str,
) -> bool {
  types.expr_is_named_ref(body, recv, expected)
}

#[cfg(feature = "typed")]
pub fn resolve_api_call_typed(
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &dyn crate::types::TypeProvider,
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
    "map" if receiver_is_array_method_receiver(lowered, body, member.object, types) => {
      Some(ApiId::ArrayPrototypeMap)
    }
    "filter" if receiver_is_array_method_receiver(lowered, body, member.object, types) => {
      Some(ApiId::ArrayPrototypeFilter)
    }
    "reduce" if receiver_is_array_method_receiver(lowered, body, member.object, types) => {
      Some(ApiId::ArrayPrototypeReduce)
    }
    "forEach" if receiver_is_array_method_receiver(lowered, body, member.object, types) => {
      Some(ApiId::ArrayPrototypeForEach)
    }
    "toLowerCase" if receiver_is_string(types, body, member.object) => Some(ApiId::StringPrototypeToLowerCase),
    "split" if receiver_is_string(types, body, member.object) => Some(ApiId::StringPrototypeSplit),
    "get" if receiver_is_named_ref(types, body, member.object, "Map") => Some(ApiId::MapPrototypeGet),
    "has" if receiver_is_named_ref(types, body, member.object, "Map") => Some(ApiId::MapPrototypeHas),
    "then" if receiver_is_named_ref(types, body, member.object, "Promise") => Some(ApiId::PromisePrototypeThen),
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
  let expr = body.exprs.get(call_expr.0 as usize)?;

  #[cfg(feature = "hir-semantic-ops")]
  match &expr.kind {
    ExprKind::PromiseAll { promises } => {
      let api = db.get(ApiId::PromiseAll.as_str())?;

      // `hir-js` lowers `Promise.all([..])` into `PromiseAll { promises }`,
      // discarding the wrapper array-literal expression. Prefer to recover the
      // original array argument so `ResolvedCall.args` remains consistent with
      // the `CallExpr` representation (i.e. `Promise.all(<arg0>)`).
      let span = (expr.span.start, expr.span.end);
      let arg0 = body
        .exprs
        .iter()
        .enumerate()
        .find_map(|(idx, candidate)| {
          if candidate.span.start < span.0 || candidate.span.end > span.1 {
            return None;
          }
          let ExprKind::Array(arr) = &candidate.kind else {
            return None;
          };
          let mut elements = Vec::with_capacity(arr.elements.len());
          for element in arr.elements.iter() {
            match element {
              ArrayElement::Expr(expr) => elements.push(*expr),
              ArrayElement::Empty | ArrayElement::Spread(_) => return None,
            }
          }
          (elements == promises.as_slice()).then_some(ExprId(idx as u32))
        });

      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: Some(ApiId::PromiseAll),
        receiver: None,
        args: arg0.into_iter().collect(),
      });
    }
    ExprKind::PromiseRace { promises } => {
      let api = db.get("Promise.race")?;

      // `hir-js` lowers `Promise.race([..])` into `PromiseRace { promises }`,
      // discarding the wrapper array-literal expression. Prefer to recover the
      // original array argument so `ResolvedCall.args` remains consistent with
      // the `CallExpr` representation (i.e. `Promise.race(<arg0>)`).
      let span = (expr.span.start, expr.span.end);
      let arg0 = body
        .exprs
        .iter()
        .enumerate()
        .find_map(|(idx, candidate)| {
          if candidate.span.start < span.0 || candidate.span.end > span.1 {
            return None;
          }
          let ExprKind::Array(arr) = &candidate.kind else {
            return None;
          };
          let mut elements = Vec::with_capacity(arr.elements.len());
          for element in arr.elements.iter() {
            match element {
              ArrayElement::Expr(expr) => elements.push(*expr),
              ArrayElement::Empty | ArrayElement::Spread(_) => return None,
            }
          }
          (elements == promises.as_slice()).then_some(ExprId(idx as u32))
        });

      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: ApiId::from_kb_name(&api.name),
        receiver: None,
        args: arg0.into_iter().collect(),
      });
    }
    ExprKind::ArrayMap { array, callback } => {
      let api = db.get(ApiId::ArrayPrototypeMap.as_str())?;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: Some(ApiId::ArrayPrototypeMap),
        receiver: Some(*array),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayFilter { array, callback } => {
      let api = db.get(ApiId::ArrayPrototypeFilter.as_str())?;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: Some(ApiId::ArrayPrototypeFilter),
        receiver: Some(*array),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayReduce {
      array,
      callback,
      init,
    } => {
      let api = db.get(ApiId::ArrayPrototypeReduce.as_str())?;
      let mut args = vec![*callback];
      if let Some(init) = init {
        args.push(*init);
      }
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: Some(ApiId::ArrayPrototypeReduce),
        receiver: Some(*array),
        args,
      });
    }
    ExprKind::ArrayFind { array, callback } => {
      let api = db.get("Array.prototype.find")?;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: ApiId::from_kb_name(&api.name),
        receiver: Some(*array),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayEvery { array, callback } => {
      let api = db.get("Array.prototype.every")?;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: ApiId::from_kb_name(&api.name),
        receiver: Some(*array),
        args: vec![*callback],
      });
    }
    ExprKind::ArraySome { array, callback } => {
      let api = db.get("Array.prototype.some")?;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: ApiId::from_kb_name(&api.name),
        receiver: Some(*array),
        args: vec![*callback],
      });
    }
    ExprKind::KnownApiCall { api, args } => {
      let kb_id = knowledge_base::ApiId::from_raw(api.raw());
      let api = db.get_by_id(kb_id)?;

      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id: ApiId::from_kb_name(&api.name),
        receiver: None,
        args: args.clone(),
      });
    }
    _ => {}
  }

  let ExprKind::Call(call) = &expr.kind else {
    return None;
  };

  // Be conservative around optional chaining and `new` calls.
  if call.optional || call.is_new {
    return None;
  }

  let callee = strip_transparent_wrappers(body, call.callee);

  match body.exprs.get(callee.0 as usize).map(|e| &e.kind) {
    Some(ExprKind::Ident(name)) => {
      let name_str = lower.names.resolve(*name)?;

      // Rule A: static global function identifier.
      if let Some(api) = db.get(name_str) {
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
        let api = resolve_imported_ident_call(db, typed, lower, *name, body_id, callee, name_str)?;
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
        let canonical = strip_global_prefixes(&path).unwrap_or(path.as_str());
        if let Some(api) = db.get(canonical) {
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

        // Rule B: receiver-type-based prototype method calls. Once the receiver
        // is proven, resolve any known prototype method name present in the KB.
        //
        // Note: Filter out non-function entries (e.g. `Array.prototype.length`)
        // since we're resolving call expressions here.
        let resolve_prototype_call = |prefix: &str| -> Option<(String, Option<ApiId>)> {
          let candidate = format!("{prefix}.prototype.{prop}");
          let api = db.get(&candidate)?;
          if !matches!(api.kind, knowledge_base::ApiKind::Function) {
            return None;
          }
          Some((api.name.clone(), ApiId::from_kb_name(&api.name)))
        };

        if receiver_is_array_method_receiver(lower, body_id, member.object, types) {
          if let Some((api, api_id)) = resolve_prototype_call("Array") {
            return Some(ResolvedCall {
              call: call_expr,
              api,
              api_id,
              receiver: Some(member.object),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
          }
        }

        if types.expr_is_string(body_id, member.object) {
          if let Some((api, api_id)) = resolve_prototype_call("String") {
            return Some(ResolvedCall {
              call: call_expr,
              api,
              api_id,
              receiver: Some(member.object),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
          }
        }

        if types.expr_is_named_ref(body_id, member.object, "Map") {
          if let Some((api, api_id)) = resolve_prototype_call("Map") {
            return Some(ResolvedCall {
              call: call_expr,
              api,
              api_id,
              receiver: Some(member.object),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
          }
        }

        if types.expr_is_named_ref(body_id, member.object, "Promise") {
          if let Some((api, api_id)) = resolve_prototype_call("Promise") {
            return Some(ResolvedCall {
              call: call_expr,
              api,
              api_id,
              receiver: Some(member.object),
              args: call.args.iter().map(|arg| arg.expr).collect(),
            });
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
fn import_specifier_for_def(
  types: &crate::typed::TypedProgram,
  def: typecheck_ts::DefId,
  import: &typecheck_ts::ImportData,
) -> Option<String> {
  // Task 234: prefer the original import specifier recorded on the import
  // definition. This is stable even when the import resolves to a host-specific
  // `FileKey` (paths, synthetic IDs, etc).
  if let Some(specifier) = types
    .program()
    .import_specifier(def)
    .filter(|s| !s.is_empty())
  {
    return Some(specifier);
  }

  // Older snapshots may miss `ImportData.specifier`; fall back to the specifier
  // stored on unresolved imports.
  match &import.target {
    typecheck_ts::ImportTarget::Unresolved { specifier } => {
      (!specifier.is_empty()).then_some(specifier.clone())
    }
    typecheck_ts::ImportTarget::File(file_id) => {
      // Last resort: avoid leaking filesystem paths into KB keys.
      //
      // Only accept file keys that already match a known KB naming convention.
      types
        .file_key(*file_id)
        .map(|key| key.as_str().to_string())
        .filter(|key| key.starts_with("node:"))
    }
  }
}

#[cfg(feature = "typed")]
fn resolve_imported_ident_call(
  db: &ApiDatabase,
  types: &crate::typed::TypedProgram,
  lower: &LowerResult,
  name: hir_js::NameId,
  body_id: BodyId,
  callee: ExprId,
  ident: &str,
) -> Option<String> {
  let body_file = types.file_for_body(body_id)?;
  let _ = (lower, name);

  let symbol_info = types
    .symbol_at_expr(body_id, callee)
    .and_then(|symbol| types.program().symbol_info(symbol));

  if let Some(info) = symbol_info {
    let def = info.def?;

    if let Some(typecheck_ts::DefKind::Import(import)) = types.def_kind(def) {
      let module = import_specifier_for_def(types, def, &import)?;
      return lookup_api(db, &module, std::slice::from_ref(&import.original));
    }

    // `symbol_at` may resolve directly to the imported definition (rather than
    // the local import binding). When that happens, map the resolved file back
    // to the original import specifier recorded on the local import binding.
    let def_file = info
      .file
      .or_else(|| types.program().span_of_def(def).map(|span| span.file))?;
    if def_file != body_file {
      let export_name = info.name.or_else(|| types.program().def_name(def))?;

      // Scan local import bindings to recover the original specifier string.
      if let Some((import_def, import)) = types
        .program()
        .definitions_in_file(body_file)
        .into_iter()
        .find_map(|candidate| match types.def_kind(candidate) {
          Some(typecheck_ts::DefKind::Import(import))
            if matches!(import.target, typecheck_ts::ImportTarget::File(file) if file == def_file)
              && import.original == export_name =>
          {
            Some((candidate, import))
          }
          _ => None,
        })
      {
        let module = import_specifier_for_def(types, import_def, &import)?;
        return lookup_api(db, &module, std::slice::from_ref(&export_name));
      }

      // Last resort: only accept file keys that already match known KB naming
      // conventions (avoid leaking host paths).
      let module_key = types
        .file_key(def_file)
        .map(|key| key.as_str().to_string())
        .filter(|key| key.starts_with("node:"))?;
      return lookup_api(db, &module_key, std::slice::from_ref(&export_name));
    }

    // If we have a resolved symbol in the *current file* and it's not an import,
    // do not fall back to matching imports by identifier name; that would allow
    // a shadowed import binding to incorrectly resolve to a KB API.
    return None;
  }

  // As a last resort (e.g. when module resolution failed and the typechecker did
  // not associate the identifier use with a symbol), fall back to matching the
  // local import binding by name in the current file.
  let mut matches = types
    .program()
    .definitions_in_file(body_file)
    .into_iter()
    .filter_map(|candidate| match types.def_kind(candidate) {
      Some(typecheck_ts::DefKind::Import(import)) => types
        .program()
        .def_name(candidate)
        .as_deref()
        .is_some_and(|name| name == ident)
        .then_some((candidate, import)),
      _ => None,
    });
  let (import_def, import) = matches.next()?;
  if matches.next().is_some() {
    return None;
  }

  let module = import_specifier_for_def(types, import_def, &import)?;
  lookup_api(db, &module, std::slice::from_ref(&import.original))
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
  let module = import_specifier_for_def(types, def, &import)?;

  // Namespace/default imports refer to the module value directly.
  if import.original != "*" && import.original != "default" {
    member_path.insert(0, import.original);
  }

  lookup_api(db, &module, &member_path)
}

fn static_object_key_name<'a>(lower: &'a LowerResult, key: &'a ObjectKey) -> Option<&'a str> {
  match key {
    ObjectKey::Ident(name) => lower.names.resolve(*name),
    ObjectKey::String(s) => Some(s.as_str()),
    ObjectKey::Number(_) | ObjectKey::Computed(_) => None,
  }
}

fn strip_global_prefixes<'a>(mut path: &'a str) -> Option<&'a str> {
  let mut changed = false;
  loop {
    let mut did_strip = false;
    for prefix in ["globalThis.", "window.", "self.", "global."] {
      if let Some(rest) = path.strip_prefix(prefix) {
        path = rest;
        did_strip = true;
        changed = true;
        break;
      }
    }
    if !did_strip {
      break;
    }
  }
  changed.then_some(path)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMember {
  pub member: ExprId,
  pub api: ApiId,
  pub receiver: ExprId,
}

/// Resolve a known property read (`obj.prop`) to a canonical [`ApiId`].
///
/// Typed-only and intentionally conservative:
/// - skips optional chaining (`obj?.prop`)
/// - skips computed keys unless the key expression is a string literal (`obj["prop"]`)
#[cfg(feature = "typed")]
pub fn resolve_member(
  lowered: &LowerResult,
  body: BodyId,
  member_expr_id: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> Option<ResolvedMember> {
  let body_ref = lowered.body(body)?;
  let member_expr = body_ref.exprs.get(member_expr_id.0 as usize)?;
  let ExprKind::Member(member) = &member_expr.kind else {
    return None;
  };
  if member.optional {
    return None;
  }

  let prop = match &member.property {
    ObjectKey::Ident(prop) => ident_name(lowered, *prop)?,
    // Allow bracket access with a literal string key: `obj["prop"]`.
    ObjectKey::Computed(prop_expr) => {
      let prop_expr = strip_transparent_wrappers(body_ref, *prop_expr);
      let prop_expr = body_ref.exprs.get(prop_expr.0 as usize)?;
      match &prop_expr.kind {
        ExprKind::Literal(hir_js::Literal::String(s)) => s.lossy.as_str(),
        _ => return None,
      }
    }
    // Be conservative around numeric/string member keys; `hir-js` currently lowers
    // bracket access as `Computed`, so these variants are generally for object literals.
    ObjectKey::String(_) | ObjectKey::Number(_) => return None,
  };

  let api = match prop {
    "length" if receiver_is_array_method_receiver(lowered, body, member.object, types) => {
      ApiId::ArrayPrototypeLength
    }
    "length" if receiver_is_string(types, body, member.object) => ApiId::StringPrototypeLength,
    "size" if receiver_is_named_ref(types, body, member.object, "Map") => ApiId::MapPrototypeSize,
    "size" if receiver_is_named_ref(types, body, member.object, "Set") => ApiId::SetPrototypeSize,
    "href" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeHref,
    "pathname" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypePathname,
    "origin" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeOrigin,
    "protocol" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeProtocol,
    "host" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeHost,
    "hostname" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeHostname,
    "port" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypePort,
    "search" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeSearch,
    "hash" if receiver_is_named_ref(types, body, member.object, "URL") => ApiId::UrlPrototypeHash,
    _ => return None,
  };

  Some(ResolvedMember {
    member: member_expr_id,
    api,
    receiver: member.object,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use hir_js::{FileKind, StmtKind};

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
  fn resolves_global_this_member_call_via_prefix_stripping() {
    let db = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      FileKind::Js,
      r#"globalThis.fetch("https://example.com");"#,
    )
    .unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    let body = lowered.body(body_id).expect("body");

    let resolved = resolve_call(&lowered, body_id, body, call_expr, &db, None).expect("resolved");
    assert_eq!(resolved.api, "fetch");
    assert_eq!(resolved.api_id, Some(ApiId::Fetch));
  }

  #[test]
  fn resolves_global_this_fetch_call_untyped() {
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.fetch("x");"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&lowered, body_id, call_expr),
      Some(ApiId::Fetch)
    );
  }

  #[test]
  fn resolves_global_this_promise_all_call_untyped() {
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.Promise.all([]);"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&lowered, body_id, call_expr),
      Some(ApiId::PromiseAll)
    );
  }

  #[test]
  fn resolves_window_json_parse_call_untyped() {
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"window.JSON.parse("x");"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&lowered, body_id, call_expr),
      Some(ApiId::JsonParse)
    );
  }
}
