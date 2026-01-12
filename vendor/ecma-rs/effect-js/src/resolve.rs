#[cfg(feature = "hir-semantic-ops")]
use hir_js::{ArrayChainOp, ArrayElement};
use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
use knowledge_base::{ApiDatabase, ApiId, KnowledgeBase, TargetEnv};

use crate::target::TargetedKb;
use crate::types::TypeProvider;

fn expr<'a>(lowered: &'a LowerResult, body: BodyId, id: ExprId) -> Option<&'a hir_js::Expr> {
  lowered.body(body)?.exprs.get(id.0 as usize)
}

fn ident_name<'a>(lowered: &'a LowerResult, name: hir_js::NameId) -> Option<&'a str> {
  lowered.names.resolve(name)
}

fn static_object_key_name<'a>(
  lowered: &'a LowerResult,
  body: &'a Body,
  key: &'a ObjectKey,
) -> Option<String> {
  match key {
    ObjectKey::Ident(name) => ident_name(lowered, *name).map(|s| s.to_string()),
    ObjectKey::String(s) => Some(s.clone()),
    ObjectKey::Number(n) => Some(crate::js_string::number_literal_to_js_string(n)),
    ObjectKey::Computed(expr) => {
      let expr = strip_transparent_wrappers(body, *expr);
      let expr = body.exprs.get(expr.0 as usize)?;
      match &expr.kind {
        ExprKind::Literal(hir_js::Literal::String(s)) => Some(s.lossy.clone()),
        ExprKind::Literal(hir_js::Literal::Number(n)) => {
          Some(crate::js_string::number_literal_to_js_string(n))
        }
        ExprKind::Literal(hir_js::Literal::BigInt(n)) => Some(n.clone()),
        ExprKind::Template(tmpl) if tmpl.spans.is_empty() => Some(tmpl.head.clone()),
        _ => None,
      }
    }
  }
}

pub(crate) struct ApiCallResolver<'a> {
  kb: &'a ApiDatabase,
  #[cfg(feature = "typed")]
  target: TargetEnv,
  lowered: &'a LowerResult,
}

impl<'a> ApiCallResolver<'a> {
  pub(crate) fn new(kb: &'a ApiDatabase, lowered: &'a LowerResult) -> Self {
    Self {
      kb,
      lowered,
      #[cfg(feature = "typed")]
      target: TargetEnv::Unknown,
    }
  }

  #[cfg(feature = "typed")]
  pub(crate) fn new_for_target(
    kb: &'a ApiDatabase,
    lowered: &'a LowerResult,
    target: TargetEnv,
  ) -> Self {
    Self {
      kb,
      target,
      lowered,
    }
  }

  fn callee_segments(&self, body: BodyId, id: ExprId) -> Option<Vec<String>> {
    let expr = expr(self.lowered, body, id)?;
    match &expr.kind {
      ExprKind::Instantiation { expr, .. } => self.callee_segments(body, *expr),
      ExprKind::Ident(name) => Some(vec![ident_name(self.lowered, *name)?.to_string()]),
      ExprKind::Member(member) => {
        if member.optional {
          return None;
        }
        let mut segs = self.callee_segments(body, member.object)?;
        let body_ref = self.lowered.body(body)?;
        let prop = static_object_key_name(self.lowered, body_ref, &member.property)?;
        segs.push(prop);
        Some(segs)
      }
      _ => None,
    }
  }

  fn callee_string(&self, body: BodyId, id: ExprId) -> Option<String> {
    let segs = self.callee_segments(body, id)?;
    Some(segs.join("."))
  }

  /// Resolve a potentially-global-prefixed callee path into a canonical KB name.
  ///
  /// Many global APIs are legally accessed via the global object
  /// (`globalThis.Promise.all`, `window.JSON.parse`, etc.) but the knowledge base
  /// typically stores the unprefixed canonical name (`Promise.all`, `JSON.parse`).
  fn canonical_name_with_global_prefix_stripping(&self, name_or_alias: &str) -> Option<&str> {
    if let Some(canonical) = self.kb.canonical_name(name_or_alias) {
      return Some(canonical);
    }

    let mut cur = name_or_alias;
    loop {
      let mut did_strip = false;
      for prefix in ["globalThis.", "window.", "self.", "global."] {
        if let Some(rest) = cur.strip_prefix(prefix) {
          cur = rest;
          did_strip = true;
          break;
        }
      }
      if !did_strip {
        return None;
      }
      if let Some(canonical) = self.kb.canonical_name(cur) {
        return Some(canonical);
      }
    }
  }

  pub(crate) fn resolve_call_untyped(&self, body: BodyId, call_expr: ExprId) -> Option<ApiId> {
    let body_ref = self.lowered.body(body)?;
    let call = body_ref.exprs.get(call_expr.0 as usize)?;
    #[cfg(feature = "hir-semantic-ops")]
    match &call.kind {
      ExprKind::PromiseAll { .. } => return self.kb.id_of("Promise.all"),
      ExprKind::PromiseRace { .. } => return self.kb.id_of("Promise.race"),
      _ => {}
    }

    let ExprKind::Call(call) = &call.kind else {
      return None;
    };
    // Be conservative around optional chaining and `new` calls.
    if call.optional || call.is_new {
      return None;
    }

    // 1) Prefer binding-based resolution (ES imports / CommonJS require) which
    // maps module specifiers to the KB namespace (e.g. `fs` → `node:fs`).
    if let Some(api) = crate::resolver::resolve_api_call(self.kb, self.lowered, body, call_expr) {
      let canonical = self.kb.canonical_name(api)?;
      if canonical.contains(".prototype.") {
        return None;
      }
      return self.kb.id_of(canonical);
    }

    // 2) Fall back to global/static callee path resolution (e.g. `JSON.parse`,
    // `Promise.all`, `fetch`, `window.fetch`).
    let candidate = self.callee_string(body, call.callee)?;
    let canonical = self.canonical_name_with_global_prefix_stripping(&candidate)?;

    // Untyped mode is intentionally conservative: do not resolve prototype /
    // instance methods without type evidence.
    if canonical.contains(".prototype.") {
      return None;
    }

    self.kb.id_of(canonical)
  }

  /// Best-effort API resolution without type information.
  ///
  /// This is intentionally more permissive than [`Self::resolve_call_untyped`]:
  /// it may return prototype method identifiers without proving the receiver
  /// type (e.g. treating `x.map(...)` as `Array.prototype.map`).
  pub(crate) fn resolve_call_best_effort_untyped(
    &self,
    body: BodyId,
    call_expr: ExprId,
  ) -> Option<ApiId> {
    if let Some(api) = self.resolve_call_untyped(body, call_expr) {
      return Some(api);
    }

    let body_ref = self.lowered.body(body)?;
    let call = body_ref.exprs.get(call_expr.0 as usize)?;

    // `hir-js` semantic-op nodes for common array operations.
    #[cfg(feature = "hir-semantic-ops")]
    match &call.kind {
      ExprKind::ArrayMap { .. } => return self.kb.id_of("Array.prototype.map"),
      ExprKind::ArrayFilter { .. } => return self.kb.id_of("Array.prototype.filter"),
      ExprKind::ArrayReduce { .. } => return self.kb.id_of("Array.prototype.reduce"),
      _ => {}
    }

    let ExprKind::Call(call) = &call.kind else {
      return None;
    };
    if call.optional || call.is_new {
      return None;
    }

    let callee_id = strip_transparent_wrappers(body_ref, call.callee);
    let callee = expr(self.lowered, body, callee_id)?;
    let ExprKind::Member(member) = &callee.kind else {
      return None;
    };
    if member.optional {
      return None;
    }

    let prop = static_object_key_name(self.lowered, body_ref, &member.property)?;

    match prop.as_str() {
      "map" => self.kb.id_of("Array.prototype.map"),
      "filter" => self.kb.id_of("Array.prototype.filter"),
      "reduce" => self.kb.id_of("Array.prototype.reduce"),
      "forEach" => self.kb.id_of("Array.prototype.forEach"),
      _ => None,
    }
  }

  #[cfg(feature = "typed")]
  pub(crate) fn resolve_call_typed(
    &self,
    body: BodyId,
    call_expr: ExprId,
    types: &dyn crate::types::TypeProvider,
  ) -> Option<ApiId> {
    // Prefer the typed resolver for imported/require bindings when available.
    //
    // The typechecker can recover stable import specifiers even when the resolved
    // file keys are host-specific (paths/synthetic IDs), which makes the API key
    // mapping into the knowledge base more reliable.
    if let Some(typed) = types.as_typed_program() {
      if let Some(api) = crate::resolver::resolve_api_call_typed(
        self.kb,
        typed.program(),
        self.lowered,
        body,
        call_expr,
      ) {
        // Typed resolution here is only for module bindings; avoid resolving
        // prototype methods without explicit receiver gating below.
        if !api.contains(".prototype.") {
          return self.kb.id_of(api);
        }
      }
    }

    // Always allow resolution for HIR-only safe APIs first (globals + best-effort
    // module binding resolution via the HIR import table).
    if let Some(api) = self.resolve_call_untyped(body, call_expr) {
      return Some(api);
    }

    let body_ref = self.lowered.body(body)?;
    let call = body_ref.exprs.get(call_expr.0 as usize)?;

    // `hir-js` semantic-op nodes for common array operations.
    #[cfg(feature = "hir-semantic-ops")]
    match &call.kind {
      ExprKind::ArrayMap { array, .. } if receiver_is_array(types, body, *array) => {
        return self.kb.id_of("Array.prototype.map");
      }
      ExprKind::ArrayFilter { array, .. } if receiver_is_array(types, body, *array) => {
        return self.kb.id_of("Array.prototype.filter");
      }
      ExprKind::ArrayReduce { array, .. } if receiver_is_array(types, body, *array) => {
        return self.kb.id_of("Array.prototype.reduce");
      }
      _ => {}
    }

    let ExprKind::Call(call) = &call.kind else {
      return None;
    };
    if call.optional || call.is_new {
      return None;
    }

    let callee_id = strip_transparent_wrappers(body_ref, call.callee);
    let callee = expr(self.lowered, body, callee_id)?;
    let ExprKind::Member(member) = &callee.kind else {
      return None;
    };
    if member.optional {
      return None;
    }

    let prop = static_object_key_name(self.lowered, body_ref, &member.property)?;

    // Receiver-type-based prototype method calls. Once the receiver is proven,
    // resolve any known prototype method name present in the KB.
    //
    // Note: Filter out non-function entries (e.g. `Array.prototype.length`) since
    // we're resolving call expressions here.
    let resolve_prototype_call = |prefix: &str| -> Option<ApiId> {
      let candidate = format!("{prefix}.prototype.{prop}");
      let api = self.kb.api_for_target(&candidate, &self.target)?;
      if !matches!(api.kind, knowledge_base::ApiKind::Function) {
        return None;
      }
      Some(api.id)
    };

    if receiver_is_array_method_receiver(self.lowered, body, member.object, types) {
      if let Some(api) = resolve_prototype_call("Array") {
        return Some(api);
      }
    }

    if receiver_is_string(types, body, member.object) {
      if let Some(api) = resolve_prototype_call("String") {
        return Some(api);
      }
    }

    if types.expr_is_named_ref(body, member.object, "Map") {
      if let Some(api) = resolve_prototype_call("Map") {
        return Some(api);
      }
    }

    if types.expr_is_named_ref(body, member.object, "Set") {
      if let Some(api) = resolve_prototype_call("Set") {
        return Some(api);
      }
    }

    if types.expr_is_named_ref(body, member.object, "Promise") {
      if let Some(api) = resolve_prototype_call("Promise") {
        return Some(api);
      }
    }

    // Some platform types inherit behavior from base prototypes that are modeled
    // in the KB (but not duplicated under the derived type name).
    //
    // Keep this mapping small and explicit; we do not attempt general prototype
    // inheritance resolution.
    if types.expr_is_named_ref(body, member.object, "File") {
      if let Some(api) = resolve_prototype_call("File") {
        return Some(api);
      }
      // `File` extends `Blob` (DOM + Node fetch globals), so allow resolving
      // inherited `Blob.prototype.*` methods like `file.text()`.
      if let Some(api) = resolve_prototype_call("Blob") {
        return Some(api);
      }
    }

    if types.expr_is_named_ref(body, member.object, "AbortSignal") {
      if let Some(api) = resolve_prototype_call("AbortSignal") {
        return Some(api);
      }
      // `AbortSignal` extends `EventTarget`, but the KB models listener methods
      // on `EventTarget.prototype.*`.
      if let Some(api) = resolve_prototype_call("EventTarget") {
        return Some(api);
      }
    }

    for ty in [
      "URL",
      "URLSearchParams",
      "Headers",
      "Request",
      "Response",
      "TextDecoder",
      "TextEncoder",
      "AbortController",
      "Buffer",
      "Blob",
      "FormData",
      "EventTarget",
      "Date",
      "RegExp",
    ] {
      if types.expr_is_named_ref(body, member.object, ty) {
        if let Some(api) = resolve_prototype_call(ty) {
          return Some(api);
        }
      }
    }

    None
  }
}

pub fn resolve_api_call_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<ApiId> {
  ApiCallResolver::new(kb, lowered).resolve_call_untyped(body, call_expr)
}

pub fn resolve_api_call_best_effort_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<ApiId> {
  ApiCallResolver::new(kb, lowered).resolve_call_best_effort_untyped(body, call_expr)
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
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> Option<ApiId> {
  ApiCallResolver::new(kb, lowered).resolve_call_typed(body, call_expr, types)
}

#[cfg(feature = "typed")]
fn receiver_is_array(types: &dyn crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_array(body, recv)
}

#[cfg(feature = "typed")]
fn receiver_is_string(types: &dyn crate::types::TypeProvider, body: BodyId, recv: ExprId) -> bool {
  types.expr_is_string(body, recv)
}

#[cfg(feature = "typed")]
fn receiver_is_array_method_receiver(
  lowered: &LowerResult,
  body: BodyId,
  recv: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> bool {
  let Some(body_ref) = lowered.body(body) else {
    return false;
  };

  if receiver_is_array(types, body, recv) {
    return true;
  }

  // `typecheck-ts` can leave intermediate call result types as `unknown`, so allow
  // `arr.map(...).filter(...)` style chains to be treated as array receivers when
  // the receiver is itself an array-returning array method call on an array.
  let Some(recv_expr) = expr(lowered, body, recv) else {
    return false;
  };
  let ExprKind::Call(call) = &recv_expr.kind else {
    return false;
  };
  if call.optional || call.is_new {
    return false;
  }

  let callee_id = strip_transparent_wrappers(body_ref, call.callee);
  let Some(callee) = expr(lowered, body, callee_id) else {
    return false;
  };
  let ExprKind::Member(member) = &callee.kind else {
    return false;
  };
  if member.optional {
    return false;
  }
  let Some(prop) = static_object_key_name(lowered, body_ref, &member.property) else {
    return false;
  };

  match prop.as_str() {
    "map" | "filter" | "flatMap" => {
      receiver_is_array_method_receiver(lowered, body, member.object, types)
    }
    _ => false,
  }
}

fn strip_transparent_wrappers(body: &Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

#[cfg(feature = "hir-semantic-ops")]
fn semantic_op_receiver_is_array(
  body: &Body,
  body_id: BodyId,
  receiver: ExprId,
  types: Option<&dyn TypeProvider>,
) -> bool {
  let receiver = strip_transparent_wrappers(body, receiver);
  let Some(expr) = body.exprs.get(receiver.0 as usize) else {
    return false;
  };

  // Typed mode: trust the type provider when available.
  #[cfg(feature = "typed")]
  if let Some(types) = types {
    if types.expr_is_array(body_id, receiver) {
      return true;
    }
  }
  #[cfg(not(feature = "typed"))]
  {
    let _ = (body_id, types);
  }

  // Untyped fallback: only accept receivers that are *syntactically* known to be arrays,
  // or are derived from a known array via array-returning semantic ops.
  match &expr.kind {
    ExprKind::Array(_) => true,
    ExprKind::ArrayMap { array, .. } | ExprKind::ArrayFilter { array, .. } => {
      semantic_op_receiver_is_array(body, body_id, *array, types)
    }
    ExprKind::ArrayChain { array, ops } => match ops.last() {
      Some(hir_js::ArrayChainOp::Map(_) | hir_js::ArrayChainOp::Filter(_)) => {
        semantic_op_receiver_is_array(body, body_id, *array, types)
      }
      _ => false,
    },
    _ => false,
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
  pub call: ExprId,
  /// Canonical knowledge-base API name (e.g. `JSON.parse`, `node:fs.readFile`).
  pub api: String,
  /// Stable knowledge-base [`ApiId`] for the resolved entry.
  pub api_id: ApiId,
  pub receiver: Option<ExprId>,
  pub args: Vec<ExprId>,
}

pub fn resolve_call_for_target(
  lower: &LowerResult,
  body_id: BodyId,
  body: &Body,
  call_expr: ExprId,
  db: &ApiDatabase,
  target: &TargetEnv,
  types: Option<&dyn TypeProvider>,
) -> Option<ResolvedCall> {
  let kb = TargetedKb::new(db, target.clone());
  let expr = body.exprs.get(call_expr.0 as usize)?;

  #[cfg(feature = "hir-semantic-ops")]
  match &expr.kind {
    ExprKind::PromiseAll { promises } => {
      let api = kb.get("Promise.all")?;
      let api_id = api.id;

      // `hir-js` lowers `Promise.all([..])` into `PromiseAll { promises }`,
      // discarding the wrapper array-literal expression. Prefer to recover the
      // original array argument so `ResolvedCall.args` remains consistent with
      // the `CallExpr` representation (i.e. `Promise.all(<arg0>)`).
      let span = (expr.span.start, expr.span.end);
      let arg0 = body.exprs.iter().enumerate().find_map(|(idx, candidate)| {
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
        api_id,
        receiver: None,
        args: arg0.into_iter().collect(),
      });
    }
    ExprKind::PromiseRace { promises } => {
      let api = kb.get("Promise.race")?;
      let api_id = api.id;

      // `hir-js` lowers `Promise.race([..])` into `PromiseRace { promises }`,
      // discarding the wrapper array-literal expression. Prefer to recover the
      // original array argument so `ResolvedCall.args` remains consistent with
      // the `CallExpr` representation (i.e. `Promise.race(<arg0>)`).
      let span = (expr.span.start, expr.span.end);
      let arg0 = body.exprs.iter().enumerate().find_map(|(idx, candidate)| {
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
        api_id,
        receiver: None,
        args: arg0.into_iter().collect(),
      });
    }
    ExprKind::ArrayMap { array, callback } => {
      let receiver = strip_transparent_wrappers(body, *array);
      // Typed mode: ensure the receiver is truly an array (avoid resolving `anyVal.map(...)`).
      //
      // Untyped mode: `hir-js` has already opted into a semantic-op node, so treat it as
      // best-effort `Array.prototype.*` for effect inference and downstream heuristics.
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.map")?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayFilter { array, callback } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.filter")?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayReduce {
      array,
      callback,
      init,
    } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.reduce")?;
      let api_id = api.id;
      let mut args = vec![*callback];
      if let Some(init) = init {
        args.push(*init);
      }
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args,
      });
    }
    ExprKind::ArrayFind { array, callback } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.find")?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayEvery { array, callback } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.every")?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args: vec![*callback],
      });
    }
    ExprKind::ArraySome { array, callback } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }
      let api = kb.get("Array.prototype.some")?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args: vec![*callback],
      });
    }
    ExprKind::ArrayChain { array, ops } => {
      let receiver = strip_transparent_wrappers(body, *array);
      if types.is_some() && !semantic_op_receiver_is_array(body, body_id, receiver, types) {
        return None;
      }

      let last = ops.last()?;

      let (api_name, args) = match last {
        ArrayChainOp::Map(callback) => ("Array.prototype.map", vec![*callback]),
        ArrayChainOp::Filter(callback) => ("Array.prototype.filter", vec![*callback]),
        ArrayChainOp::Reduce(callback, init) => {
          let mut args = vec![*callback];
          if let Some(init) = init {
            args.push(*init);
          }
          ("Array.prototype.reduce", args)
        }
        ArrayChainOp::Find(callback) => ("Array.prototype.find", vec![*callback]),
        ArrayChainOp::Every(callback) => ("Array.prototype.every", vec![*callback]),
        ArrayChainOp::Some(callback) => ("Array.prototype.some", vec![*callback]),
      };

      let api = kb.get(api_name)?;
      let api_id = api.id;
      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
        receiver: Some(receiver),
        args,
      });
    }
    ExprKind::KnownApiCall { api: hir_api, args } => {
      let api_id = ApiId::from_raw(hir_api.raw());
      let api = kb.get_by_id(api_id)?;

      return Some(ResolvedCall {
        call: call_expr,
        api: api.name.clone(),
        api_id,
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

  #[cfg(feature = "typed")]
  let resolver = ApiCallResolver::new_for_target(db, lower, target.clone());
  #[cfg(not(feature = "typed"))]
  let resolver = ApiCallResolver::new(db, lower);

  #[cfg(feature = "typed")]
  let api_id = match types {
    Some(types) => resolver.resolve_call_typed(body_id, call_expr, types),
    None => resolver.resolve_call_untyped(body_id, call_expr),
  };

  #[cfg(not(feature = "typed"))]
  let api_id = {
    let _ = (types, body_id);
    resolver.resolve_call_untyped(body_id, call_expr)
  };

  let api_id = api_id?;
  let api = kb.get_by_id(api_id)?.name.clone();

  let callee = strip_transparent_wrappers(body, call.callee);
  let receiver = match body.exprs.get(callee.0 as usize).map(|e| &e.kind) {
    Some(ExprKind::Member(member)) if !member.optional => Some(member.object),
    _ => None,
  };

  Some(ResolvedCall {
    call: call_expr,
    api,
    api_id,
    receiver,
    args: call.args.iter().map(|arg| arg.expr).collect(),
  })
}

pub fn resolve_call(
  lower: &LowerResult,
  body_id: BodyId,
  body: &Body,
  call_expr: ExprId,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Option<ResolvedCall> {
  resolve_call_for_target(
    lower,
    body_id,
    body,
    call_expr,
    db,
    &TargetEnv::Unknown,
    types,
  )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMember {
  pub member: ExprId,
  /// Canonical knowledge-base API name (e.g. `URL.prototype.pathname`).
  pub api: String,
  /// Stable knowledge-base [`ApiId`] for the resolved entry.
  pub api_id: ApiId,
  pub receiver: ExprId,
}

/// Resolve a known property read (`obj.prop`) to a canonical [`ApiId`].
///
/// Typed-only and intentionally conservative:
/// - skips optional chaining (`obj?.prop`)
/// - skips computed keys unless the key expression is a literal (`obj["prop"]`)
#[cfg(feature = "typed")]
pub fn resolve_member_for_target(
  kb: &ApiDatabase,
  target: &TargetEnv,
  lowered: &LowerResult,
  body: BodyId,
  member_expr_id: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> Option<ResolvedMember> {
  let kb = TargetedKb::new(kb, target.clone());
  let body_ref = lowered.body(body)?;
  let member_expr = body_ref.exprs.get(member_expr_id.0 as usize)?;
  let ExprKind::Member(member) = &member_expr.kind else {
    return None;
  };
  if member.optional {
    return None;
  }

  let prop = static_object_key_name(lowered, body_ref, &member.property)?;

  let resolve_prototype_get = |prefix: &str| -> Option<&knowledge_base::ApiSemantics> {
    let candidate = format!("{prefix}.prototype.{prop}");
    let api = kb.get(&candidate)?;
    matches!(api.kind, knowledge_base::ApiKind::Getter).then_some(api)
  };

  let entry = if receiver_is_array_method_receiver(lowered, body, member.object, types) {
    resolve_prototype_get("Array")
  } else if receiver_is_string(types, body, member.object) {
    resolve_prototype_get("String")
  } else if receiver_is_named_ref(types, body, member.object, "Map") {
    resolve_prototype_get("Map")
  } else if receiver_is_named_ref(types, body, member.object, "Set") {
    resolve_prototype_get("Set")
  } else if receiver_is_named_ref(types, body, member.object, "File") {
    resolve_prototype_get("File").or_else(|| resolve_prototype_get("Blob"))
  } else if receiver_is_named_ref(types, body, member.object, "Blob") {
    resolve_prototype_get("Blob")
  } else if receiver_is_named_ref(types, body, member.object, "URL") {
    resolve_prototype_get("URL")
  } else if receiver_is_named_ref(types, body, member.object, "URLSearchParams") {
    resolve_prototype_get("URLSearchParams")
  } else if receiver_is_named_ref(types, body, member.object, "Headers") {
    resolve_prototype_get("Headers")
  } else if receiver_is_named_ref(types, body, member.object, "Request") {
    resolve_prototype_get("Request")
  } else if receiver_is_named_ref(types, body, member.object, "Response") {
    resolve_prototype_get("Response")
  } else if receiver_is_named_ref(types, body, member.object, "AbortController") {
    resolve_prototype_get("AbortController")
  } else if receiver_is_named_ref(types, body, member.object, "AbortSignal") {
    resolve_prototype_get("AbortSignal").or_else(|| resolve_prototype_get("EventTarget"))
  } else if receiver_is_named_ref(types, body, member.object, "FormData") {
    resolve_prototype_get("FormData")
  } else if receiver_is_named_ref(types, body, member.object, "EventTarget") {
    resolve_prototype_get("EventTarget")
  } else {
    None
  }?;

  Some(ResolvedMember {
    member: member_expr_id,
    api: entry.name.clone(),
    api_id: entry.id,
    receiver: member.object,
  })
}

#[cfg(feature = "typed")]
pub fn resolve_member(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  member_expr_id: ExprId,
  types: &dyn crate::types::TypeProvider,
) -> Option<ResolvedMember> {
  resolve_member_for_target(
    kb,
    &TargetEnv::Unknown,
    lowered,
    body,
    member_expr_id,
    types,
  )
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
    let fetch_id = db.id_of("fetch").expect("fetch in KB");
    let lowered = hir_js::lower_from_source_with_kind(
      FileKind::Js,
      r#"globalThis.fetch("https://example.com");"#,
    )
    .unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    let body = lowered.body(body_id).expect("body");

    let resolved = resolve_call(&lowered, body_id, body, call_expr, &db, None).expect("resolved");
    assert_eq!(resolved.api, "fetch");
    assert_eq!(resolved.api_id, fetch_id);
  }

  #[test]
  fn resolves_global_this_fetch_call_untyped() {
    let db = crate::load_default_api_database();
    let fetch_id = db.id_of("fetch").expect("fetch in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.fetch("x");"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(fetch_id)
    );
  }

  #[test]
  fn resolves_global_this_fetch_call_untyped_via_computed_template_key() {
    let db = crate::load_default_api_database();
    let fetch_id = db.id_of("fetch").expect("fetch in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis[`fetch`]("x");"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(fetch_id)
    );
  }

  #[test]
  fn resolves_global_this_promise_all_call_untyped() {
    let db = crate::load_default_api_database();
    let promise_all_id = db.id_of("Promise.all").expect("Promise.all in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.Promise.all([]);"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(promise_all_id)
    );
  }

  #[test]
  fn resolves_global_this_promise_race_call_untyped() {
    let db = crate::load_default_api_database();
    let promise_race_id = db.id_of("Promise.race").expect("Promise.race in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"globalThis.Promise.race([]);"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(promise_race_id)
    );
  }

  #[cfg(feature = "hir-semantic-ops")]
  #[test]
  fn resolves_semantic_promise_race_call_untyped() {
    let db = crate::load_default_api_database();
    let promise_race_id = db.id_of("Promise.race").expect("Promise.race in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"Promise.race([]);"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(promise_race_id)
    );
  }

  #[test]
  fn resolves_window_json_parse_call_untyped() {
    let db = crate::load_default_api_database();
    let json_parse_id = db.id_of("JSON.parse").expect("JSON.parse in KB");
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, r#"window.JSON.parse("x");"#).unwrap();
    let (body_id, call_expr) = first_stmt_expr(&lowered);
    assert_eq!(
      resolve_api_call_untyped(&db, &lowered, body_id, call_expr),
      Some(json_parse_id)
    );
  }

  #[test]
  fn computed_number_property_keys_use_js_number_to_string() {
    let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "obj[1e21];").unwrap();
    let (body_id, member_expr) = first_stmt_expr(&lowered);
    let body = lowered.body(body_id).expect("body");
    let expr = body.exprs.get(member_expr.0 as usize).expect("expr");
    let ExprKind::Member(member) = &expr.kind else {
      panic!("expected member expr");
    };
    let key = static_object_key_name(&lowered, body, &member.property).expect("key");
    assert_eq!(key, "1e+21");

    let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "obj[1e-7];").unwrap();
    let (body_id, member_expr) = first_stmt_expr(&lowered);
    let body = lowered.body(body_id).expect("body");
    let expr = body.exprs.get(member_expr.0 as usize).expect("expr");
    let ExprKind::Member(member) = &expr.kind else {
      panic!("expected member expr");
    };
    let key = static_object_key_name(&lowered, body, &member.property).expect("key");
    assert_eq!(key, "1e-7");

    let lowered = hir_js::lower_from_source_with_kind(FileKind::Js, "obj[1e400];").unwrap();
    let (body_id, member_expr) = first_stmt_expr(&lowered);
    let body = lowered.body(body_id).expect("body");
    let expr = body.exprs.get(member_expr.0 as usize).expect("expr");
    let ExprKind::Member(member) = &expr.kind else {
      panic!("expected member expr");
    };
    let key = static_object_key_name(&lowered, body, &member.property).expect("key");
    assert_eq!(key, "Infinity");
  }
}
