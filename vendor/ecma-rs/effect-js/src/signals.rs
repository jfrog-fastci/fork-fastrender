use hir_js::hir::{ExprKind, ObjectKey, StmtKind, TypeExprKind, TypeMemberKind, VarDeclKind};
use hir_js::{Body, BodyId, ExprId, HirFile, NameId, NameInterner, StmtId, TypeExprId};

/// Semantic cues surfaced from the user's source code.
///
/// These are intentionally not "optimization patterns": they represent explicit
/// intent expressed by the developer and can later drive warnings, analysis, and
/// optimizations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SemanticSignal {
  /// `Promise.all(...)` indicates that the promises are intended to be
  /// independent and can be awaited in parallel.
  PromiseAll { expr: ExprId },

  /// An `async` function body that contains no `await`.
  AsyncFunctionWithoutAwait { def: hir_js::DefId, body: BodyId },

  /// A `const` variable declaration.
  ConstBinding { stmt: StmtId, declarator_index: usize },

  /// A `readonly` type position (currently: readonly arrays and readonly type
  /// properties).
  ReadonlyTypePosition { type_expr: TypeExprId },

  /// `expr as const`.
  AsConstAssertion { expr: ExprId },

  /// Access to a private field (e.g. `this.#x`).
  PrivateFieldAccess { expr: ExprId },

  /// Any TypeScript type assertion (`expr as T`, `<T>expr`, and `as const`).
  TypeAssertion { expr: ExprId },
}

pub fn detect_signals(file: &HirFile, body: &Body, names: &NameInterner) -> Vec<SemanticSignal> {
  let mut signals = Vec::new();

  // Expression / statement signals.
  for &stmt in body.root_stmts.iter() {
    walk_stmt(stmt, body, names, &mut signals);
  }

  // Type syntax signals. `hir-js` attaches type syntax encountered inside an
  // executable body to the body's owner `DefId`.
  if let Some(arenas) = file.types.get(&body.owner) {
    for (idx, ty) in arenas.type_exprs.iter().enumerate() {
      let id = TypeExprId(idx as u32);
      if matches!(
        &ty.kind,
        TypeExprKind::Array(arr) if arr.readonly
      ) {
        signals.push(SemanticSignal::ReadonlyTypePosition { type_expr: id });
      }
    }

    for member in arenas.type_members.iter() {
      if let TypeMemberKind::Property(sig) = &member.kind {
        if sig.readonly {
          if let Some(type_expr) = sig.type_annotation {
            signals.push(SemanticSignal::ReadonlyTypePosition { type_expr });
          }
        }
      }
    }
  }

  // Async functions without await: use the final per-body arena to avoid
  // accidentally counting awaits from nested bodies.
  if let Some(func) = body.function.as_ref() {
    if func.async_ {
      let has_await = body.exprs.iter().any(|expr| matches!(expr.kind, ExprKind::Await { .. }));
      if !has_await {
        signals.push(SemanticSignal::AsyncFunctionWithoutAwait {
          def: body.owner,
          body: body_id_for_span(file, body),
        });
      }
    }
  }

  signals.sort_by_key(|signal| (signal_span_start(signal, file, body), signal_kind_rank(signal), signal_tiebreak(signal)));
  signals
}

fn walk_stmt(stmt: StmtId, body: &Body, names: &NameInterner, signals: &mut Vec<SemanticSignal>) {
  let stmt_data = &body.stmts[stmt.0 as usize];
  match &stmt_data.kind {
    StmtKind::Expr(expr) => walk_expr(*expr, body, names, signals),
    StmtKind::Return(Some(expr)) => walk_expr(*expr, body, names, signals),
    StmtKind::Return(None) => {}
    StmtKind::Block(stmts) => {
      for &inner in stmts {
        walk_stmt(inner, body, names, signals);
      }
    }
    StmtKind::If {
      test,
      consequent,
      alternate,
    } => {
      walk_expr(*test, body, names, signals);
      walk_stmt(*consequent, body, names, signals);
      if let Some(alt) = alternate {
        walk_stmt(*alt, body, names, signals);
      }
    }
    StmtKind::While { test, body: inner } | StmtKind::DoWhile { test, body: inner } => {
      walk_expr(*test, body, names, signals);
      walk_stmt(*inner, body, names, signals);
    }
    StmtKind::For {
      init,
      test,
      update,
      body: inner,
    } => {
      if let Some(init) = init {
        match init {
          hir_js::ForInit::Expr(expr) => walk_expr(*expr, body, names, signals),
          hir_js::ForInit::Var(var) => walk_var_decl(var, stmt, body, names, signals),
        }
      }
      if let Some(test) = test {
        walk_expr(*test, body, names, signals);
      }
      if let Some(update) = update {
        walk_expr(*update, body, names, signals);
      }
      walk_stmt(*inner, body, names, signals);
    }
    StmtKind::ForIn {
      left,
      right,
      body: inner,
      ..
    } => {
      match left {
        hir_js::ForHead::Pat(pat) => walk_pat(*pat, body, names, signals),
        hir_js::ForHead::Var(var) => walk_var_decl(var, stmt, body, names, signals),
      }
      walk_expr(*right, body, names, signals);
      walk_stmt(*inner, body, names, signals);
    }
    StmtKind::Switch { discriminant, cases } => {
      walk_expr(*discriminant, body, names, signals);
      for case in cases {
        if let Some(test) = case.test {
          walk_expr(test, body, names, signals);
        }
        for &cons in case.consequent.iter() {
          walk_stmt(cons, body, names, signals);
        }
      }
    }
    StmtKind::Try {
      block,
      catch,
      finally_block,
    } => {
      walk_stmt(*block, body, names, signals);
      if let Some(catch) = catch {
        if let Some(param) = catch.param {
          walk_pat(param, body, names, signals);
        }
        walk_stmt(catch.body, body, names, signals);
      }
      if let Some(finally) = finally_block {
        walk_stmt(*finally, body, names, signals);
      }
    }
    StmtKind::Throw(expr) => walk_expr(*expr, body, names, signals),
    StmtKind::Var(var) => walk_var_decl(var, stmt, body, names, signals),
    StmtKind::Labeled { body: inner, .. } => walk_stmt(*inner, body, names, signals),
    StmtKind::With { object, body: inner } => {
      walk_expr(*object, body, names, signals);
      walk_stmt(*inner, body, names, signals);
    }
    // Non-executable or leaf statements.
    StmtKind::Decl(_) | StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Debugger | StmtKind::Empty => {}
  }
}

fn walk_var_decl(
  var: &hir_js::VarDecl,
  stmt: StmtId,
  body: &Body,
  names: &NameInterner,
  signals: &mut Vec<SemanticSignal>,
) {
  if var.kind == VarDeclKind::Const {
    for idx in 0..var.declarators.len() {
      signals.push(SemanticSignal::ConstBinding {
        stmt,
        declarator_index: idx,
      });
    }
  }
  for declarator in var.declarators.iter() {
    walk_pat(declarator.pat, body, names, signals);
    if let Some(init) = declarator.init {
      walk_expr(init, body, names, signals);
    }
  }
}

fn walk_pat(pat: hir_js::PatId, body: &Body, names: &NameInterner, signals: &mut Vec<SemanticSignal>) {
  let pat_data = &body.pats[pat.0 as usize];
  match &pat_data.kind {
    hir_js::PatKind::Ident(_) => {}
    hir_js::PatKind::Array(arr) => {
      for el in arr.elements.iter().flatten() {
        walk_pat(el.pat, body, names, signals);
        if let Some(default) = el.default_value {
          walk_expr(default, body, names, signals);
        }
      }
      if let Some(rest) = arr.rest {
        walk_pat(rest, body, names, signals);
      }
    }
    hir_js::PatKind::Object(obj) => {
      for prop in obj.props.iter() {
        if let ObjectKey::Computed(expr) = &prop.key {
          walk_expr(*expr, body, names, signals);
        }
        walk_pat(prop.value, body, names, signals);
        if let Some(default) = prop.default_value {
          walk_expr(default, body, names, signals);
        }
      }
      if let Some(rest) = obj.rest {
        walk_pat(rest, body, names, signals);
      }
    }
    hir_js::PatKind::Rest(inner) => walk_pat(*inner.as_ref(), body, names, signals),
    hir_js::PatKind::Assign { target, default_value } => {
      walk_pat(*target, body, names, signals);
      walk_expr(*default_value, body, names, signals);
    }
    hir_js::PatKind::AssignTarget(expr) => walk_expr(*expr, body, names, signals),
  }
}

fn walk_expr(expr: ExprId, body: &Body, names: &NameInterner, signals: &mut Vec<SemanticSignal>) {
  let expr_data = &body.exprs[expr.0 as usize];
  match &expr_data.kind {
    ExprKind::Call(call) => {
      if is_promise_all_call(call, body, names) {
        signals.push(SemanticSignal::PromiseAll { expr });
      }
      walk_expr(call.callee, body, names, signals);
      for arg in call.args.iter() {
        walk_expr(arg.expr, body, names, signals);
      }
    }
    ExprKind::Member(member) => {
      if is_private_name(&member.property, names) {
        signals.push(SemanticSignal::PrivateFieldAccess { expr });
      }
      walk_expr(member.object, body, names, signals);
      if let ObjectKey::Computed(prop_expr) = &member.property {
        walk_expr(*prop_expr, body, names, signals);
      }
    }
    ExprKind::TypeAssertion { expr: inner, const_assertion, .. } => {
      signals.push(SemanticSignal::TypeAssertion { expr });
      if *const_assertion {
        signals.push(SemanticSignal::AsConstAssertion { expr });
      }
      walk_expr(*inner, body, names, signals);
    }
    ExprKind::Unary { expr: inner, .. }
    | ExprKind::Update { expr: inner, .. }
    | ExprKind::NonNull { expr: inner }
    | ExprKind::Await { expr: inner } => {
      walk_expr(*inner, body, names, signals);
    }
    ExprKind::Binary { left, right, .. } => {
      walk_expr(*left, body, names, signals);
      walk_expr(*right, body, names, signals);
    }
    ExprKind::Assignment { target, value, .. } => {
      walk_pat(*target, body, names, signals);
      walk_expr(*value, body, names, signals);
    }
    ExprKind::Conditional { test, consequent, alternate } => {
      walk_expr(*test, body, names, signals);
      walk_expr(*consequent, body, names, signals);
      walk_expr(*alternate, body, names, signals);
    }
    ExprKind::Array(arr) => {
      for el in arr.elements.iter() {
        match el {
          hir_js::ArrayElement::Expr(expr) | hir_js::ArrayElement::Spread(expr) => {
            walk_expr(*expr, body, names, signals);
          }
          hir_js::ArrayElement::Empty => {}
        }
      }
    }
    ExprKind::Object(obj) => {
      for prop in obj.properties.iter() {
        match prop {
          hir_js::ObjectProperty::KeyValue { key, value, .. } => {
            if let ObjectKey::Computed(expr) = key {
              walk_expr(*expr, body, names, signals);
            }
            walk_expr(*value, body, names, signals);
          }
          hir_js::ObjectProperty::Getter { key, .. } | hir_js::ObjectProperty::Setter { key, .. } => {
            if let ObjectKey::Computed(expr) = key {
              walk_expr(*expr, body, names, signals);
            }
          }
          hir_js::ObjectProperty::Spread(expr) => walk_expr(*expr, body, names, signals),
        }
      }
    }
    ExprKind::Template(tmpl) => {
      for span in tmpl.spans.iter() {
        walk_expr(span.expr, body, names, signals);
      }
    }
    ExprKind::TaggedTemplate { tag, template } => {
      walk_expr(*tag, body, names, signals);
      for span in template.spans.iter() {
        walk_expr(span.expr, body, names, signals);
      }
    }
    ExprKind::Yield { expr: Some(inner), .. } => walk_expr(*inner, body, names, signals),
    ExprKind::Yield { expr: None, .. } => {}
    ExprKind::Satisfies { expr: inner, .. } => walk_expr(*inner, body, names, signals),
    ExprKind::ImportCall { argument, attributes } => {
      walk_expr(*argument, body, names, signals);
      if let Some(attrs) = attributes {
        walk_expr(*attrs, body, names, signals);
      }
    }
    ExprKind::Jsx(el) => {
      for attr in el.attributes.iter() {
        match attr {
          hir_js::JsxAttr::Named { value: Some(hir_js::JsxAttrValue::Expression(container)), .. } => {
            if let Some(expr) = container.expr {
              walk_expr(expr, body, names, signals);
            }
          }
          hir_js::JsxAttr::Spread { expr } => walk_expr(*expr, body, names, signals),
          _ => {}
        }
      }
      for child in el.children.iter() {
        match child {
          hir_js::JsxChild::Element(expr) => walk_expr(*expr, body, names, signals),
          hir_js::JsxChild::Expr(container) => {
            if let Some(expr) = container.expr {
              walk_expr(expr, body, names, signals);
            }
          }
          hir_js::JsxChild::Text(_) => {}
        }
      }
    }
    // Leaves or nodes that reference other bodies (handled by separate calls).
    ExprKind::Missing
    | ExprKind::Ident(_)
    | ExprKind::This
    | ExprKind::Super
    | ExprKind::Literal(_)
    | ExprKind::FunctionExpr { .. }
    | ExprKind::ClassExpr { .. }
    | ExprKind::ImportMeta
    | ExprKind::NewTarget => {}
  }
}

fn is_private_name(key: &ObjectKey, names: &NameInterner) -> bool {
  match key {
    ObjectKey::Ident(id) => names.resolve(*id).is_some_and(|name| name.starts_with('#')),
    _ => false,
  }
}

fn is_promise_all_call(call: &hir_js::CallExpr, body: &Body, names: &NameInterner) -> bool {
  let ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
    return false;
  };
  if member.optional {
    // Optional chaining does not change the signal; still treat it as `Promise.all`.
  }
  let ExprKind::Ident(obj) = &body.exprs[member.object.0 as usize].kind else {
    return false;
  };
  let ObjectKey::Ident(prop) = &member.property else {
    return false;
  };
  name_eq(names, *obj, "Promise") && name_eq(names, *prop, "all")
}

fn name_eq(names: &NameInterner, id: NameId, expected: &str) -> bool {
  names.resolve(id) == Some(expected)
}

fn body_id_for_span(file: &HirFile, body: &Body) -> BodyId {
  // `Body::span` may include surrounding syntax (e.g. `function foo(...)`), while
  // the span map indexes bodies using a tighter range derived from the body's
  // contained expressions/statements/patterns. Pick an offset from the first root
  // statement when available to land inside the indexed span.
  let offset = body
    .root_stmts
    .first()
    .and_then(|stmt| body.stmts.get(stmt.0 as usize))
    .map(|stmt| stmt.span.start)
    .or_else(|| body.pats.first().map(|pat| pat.span.start))
    .or_else(|| body.exprs.first().map(|expr| expr.span.start))
    .unwrap_or(body.span.start);

  file
    .span_map
    .body_at_offset(offset)
    .unwrap_or(hir_js::ids::MISSING_BODY)
}

fn signal_span_start(signal: &SemanticSignal, file: &HirFile, body: &Body) -> u32 {
  match *signal {
    SemanticSignal::PromiseAll { expr }
    | SemanticSignal::AsConstAssertion { expr }
    | SemanticSignal::PrivateFieldAccess { expr }
    | SemanticSignal::TypeAssertion { expr } => body.exprs[expr.0 as usize].span.start,
    SemanticSignal::ConstBinding { stmt, .. } => body.stmts[stmt.0 as usize].span.start,
    SemanticSignal::AsyncFunctionWithoutAwait { body: body_id, .. } => file
      .span_map
      .body_span(body_id)
      .map(|range| range.start)
      .unwrap_or(body.span.start),
    SemanticSignal::ReadonlyTypePosition { type_expr } => file
      .span_map
      .type_expr_span(body.owner, type_expr)
      .map(|range| range.start)
      .unwrap_or(body.span.start),
  }
}

fn signal_kind_rank(signal: &SemanticSignal) -> u8 {
  match signal {
    SemanticSignal::PromiseAll { .. } => 0,
    SemanticSignal::AsyncFunctionWithoutAwait { .. } => 1,
    SemanticSignal::ConstBinding { .. } => 2,
    SemanticSignal::ReadonlyTypePosition { .. } => 3,
    SemanticSignal::AsConstAssertion { .. } => 4,
    SemanticSignal::PrivateFieldAccess { .. } => 5,
    SemanticSignal::TypeAssertion { .. } => 6,
  }
}

fn signal_tiebreak(signal: &SemanticSignal) -> u64 {
  match *signal {
    SemanticSignal::PromiseAll { expr }
    | SemanticSignal::AsConstAssertion { expr }
    | SemanticSignal::PrivateFieldAccess { expr }
    | SemanticSignal::TypeAssertion { expr } => expr.0 as u64,
    SemanticSignal::AsyncFunctionWithoutAwait { def, body } => def.0 ^ body.0,
    SemanticSignal::ConstBinding { stmt, declarator_index } => {
      ((stmt.0 as u64) << 32) | (declarator_index as u64)
    }
    SemanticSignal::ReadonlyTypePosition { type_expr } => type_expr.0 as u64,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use hir_js::{lower_from_source_with_kind, DefKind, FileKind};

  fn signals_for_source(source: &str) -> (hir_js::LowerResult, Vec<SemanticSignal>) {
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
    let root_body = lowered.root_body();
    let body = lowered.body(root_body).expect("root body");
    let signals = detect_signals(&lowered.hir, body, &lowered.names);
    (lowered, signals)
  }

  #[test]
  fn detects_promise_all() {
    let (_lowered, signals) = signals_for_source("Promise.all([a(), b()]);");
    assert!(signals.iter().any(|sig| matches!(sig, SemanticSignal::PromiseAll { .. })));
  }

  #[test]
  fn detects_async_function_without_await() {
    let source = "async function f(){ return 1; }";
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
    let f = lowered
      .defs
      .iter()
      .find(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("f"))
      .expect("function f");
    let body_id = f.body.expect("f body");
    let body = lowered.body(body_id).expect("function body");
    let signals = detect_signals(&lowered.hir, body, &lowered.names);
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionWithoutAwait { .. })),
      "expected AsyncFunctionWithoutAwait"
    );
  }

  #[test]
  fn does_not_flag_async_function_with_await() {
    let source = "async function f(){ await g(); }";
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
    let f = lowered
      .defs
      .iter()
      .find(|def| def.path.kind == DefKind::Function && lowered.names.resolve(def.name) == Some("f"))
      .expect("function f");
    let body_id = f.body.expect("f body");
    let body = lowered.body(body_id).expect("function body");
    let signals = detect_signals(&lowered.hir, body, &lowered.names);
    assert!(
      !signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::AsyncFunctionWithoutAwait { .. })),
      "did not expect AsyncFunctionWithoutAwait"
    );
  }

  #[test]
  fn detects_const_binding_but_not_let() {
    let (_lowered, signals) = signals_for_source("const a = 1; let b = 2;");
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::ConstBinding { .. })),
      "expected ConstBinding"
    );
    // Ensure only one const binding is surfaced for this snippet.
    let const_count = signals
      .iter()
      .filter(|sig| matches!(sig, SemanticSignal::ConstBinding { .. }))
      .count();
    assert_eq!(const_count, 1);
  }

  #[test]
  fn detects_as_const_assertion() {
    let (_lowered, signals) = signals_for_source("let x = [1] as const;");
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::AsConstAssertion { .. })),
      "expected AsConstAssertion"
    );
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::TypeAssertion { .. })),
      "expected TypeAssertion"
    );
  }

  #[test]
  fn detects_readonly_array_type() {
    let (_lowered, signals) = signals_for_source("let x: readonly number[] = [];");
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::ReadonlyTypePosition { .. })),
      "expected ReadonlyTypePosition"
    );
  }

  #[test]
  fn detects_private_field_access() {
    let source = "class C { #x = 1; f(){ return this.#x } }";
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).expect("lower source");
    let f = lowered
      .defs
      .iter()
      .find(|def| def.path.kind == DefKind::Method && lowered.names.resolve(def.name) == Some("f"))
      .expect("method f");
    let body_id = f.body.expect("method body");
    let body = lowered.body(body_id).expect("method body exists");
    let signals = detect_signals(&lowered.hir, body, &lowered.names);
    assert!(
      signals
        .iter()
        .any(|sig| matches!(sig, SemanticSignal::PrivateFieldAccess { .. })),
      "expected PrivateFieldAccess"
    );
  }
}
