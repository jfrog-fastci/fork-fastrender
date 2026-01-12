use hir_js::{
  ArrayElement, BinaryOp, BodyId, ExprId, ExprKind, LowerResult, ObjectProperty, PatId, PatKind,
  StmtId, StmtKind, UnaryOp, VarDeclKind,
};

use knowledge_base::{ApiId, KnowledgeBase};
use crate::resolve::ApiCallResolver;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExprFingerprint {
  Ident(hir_js::NameId),
  This,
  Member(Box<ExprFingerprint>, MemberKey),
  LiteralNull,
  LiteralUndefined,
  LiteralBoolean(bool),
  LiteralNumber(String),
  LiteralString(String),
  LiteralBigInt(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberKey {
  String(String),
  Number(String),
}

fn expr_fingerprint(lowered: &LowerResult, body: BodyId, expr: ExprId) -> Option<ExprFingerprint> {
  let body_ref = lowered.body(body)?;
  let expr = body_ref.exprs.get(expr.0 as usize)?;
  match &expr.kind {
    ExprKind::TypeAssertion { expr, .. }
    | ExprKind::NonNull { expr }
    | ExprKind::Instantiation { expr, .. }
    | ExprKind::Satisfies { expr, .. } => expr_fingerprint(lowered, body, *expr),
    ExprKind::Ident(name) => Some(ExprFingerprint::Ident(*name)),
    ExprKind::This => Some(ExprFingerprint::This),
    ExprKind::Member(member) => {
      if member.optional {
        return None;
      }
      let obj = expr_fingerprint(lowered, body, member.object)?;
      let key = match &member.property {
        hir_js::ObjectKey::Ident(id) => MemberKey::String(lowered.names.resolve(*id)?.to_string()),
        hir_js::ObjectKey::String(s) => MemberKey::String(s.clone()),
        hir_js::ObjectKey::Number(n) => MemberKey::Number(crate::js_string::number_literal_to_js_string(n)),
        hir_js::ObjectKey::Computed(expr) => {
          let expr = strip_transparent_wrappers(body_ref, *expr);
          let expr = body_ref.exprs.get(expr.0 as usize)?;
          match &expr.kind {
            ExprKind::Literal(hir_js::Literal::String(lit)) => MemberKey::String(lit.lossy.clone()),
            ExprKind::Literal(hir_js::Literal::Number(n)) => MemberKey::Number(crate::js_string::number_literal_to_js_string(n)),
            ExprKind::Literal(hir_js::Literal::BigInt(n)) => MemberKey::Number(n.clone()),
            _ => return None,
          }
        }
      };
      Some(ExprFingerprint::Member(Box::new(obj), key))
    }
    ExprKind::Literal(lit) => Some(match lit {
      hir_js::Literal::Null => ExprFingerprint::LiteralNull,
      hir_js::Literal::Undefined => ExprFingerprint::LiteralUndefined,
      hir_js::Literal::Boolean(b) => ExprFingerprint::LiteralBoolean(*b),
      hir_js::Literal::Number(n) => ExprFingerprint::LiteralNumber(n.clone()),
      hir_js::Literal::String(s) => ExprFingerprint::LiteralString(s.lossy.clone()),
      hir_js::Literal::BigInt(n) => ExprFingerprint::LiteralBigInt(n.clone()),
      hir_js::Literal::Regex(_) => return None,
    }),
    _ => None,
  }
}

fn parse_simple_method_call_untyped(
  lowered: &LowerResult,
  body: BodyId,
  expr: ExprId,
) -> Option<(ExprId, String, ExprId)> {
  let body_ref = lowered.body(body)?;
  let expr = strip_transparent_wrappers(body_ref, expr);
  let expr = body_ref.exprs.get(expr.0 as usize)?;
  let ExprKind::Call(call) = &expr.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }
  if call.args.len() != 1 {
    return None;
  }
  let arg0 = call.args.first()?;
  if arg0.spread {
    return None;
  }

  let callee = strip_transparent_wrappers(body_ref, call.callee);
  let callee = body_ref.exprs.get(callee.0 as usize)?;
  let ExprKind::Member(member) = &callee.kind else {
    return None;
  };
  if member.optional {
    return None;
  }
  let prop = static_object_key_name(lowered, body_ref, &member.property)?;

  Some((member.object, prop, arg0.expr))
}

fn static_object_key_name(
  lowered: &LowerResult,
  body: &hir_js::Body,
  key: &hir_js::ObjectKey,
) -> Option<String> {
  match key {
    hir_js::ObjectKey::Ident(id) => lowered.names.resolve(*id).map(|s| s.to_string()),
    hir_js::ObjectKey::String(s) => Some(s.clone()),
    hir_js::ObjectKey::Number(n) => Some(crate::js_string::number_literal_to_js_string(n)),
    hir_js::ObjectKey::Computed(expr) => {
      let expr = strip_transparent_wrappers(body, *expr);
      let expr = body.exprs.get(expr.0 as usize)?;
      match &expr.kind {
        ExprKind::Literal(hir_js::Literal::String(lit)) => Some(lit.lossy.clone()),
        ExprKind::Literal(hir_js::Literal::Number(n)) => Some(crate::js_string::number_literal_to_js_string(n)),
        ExprKind::Literal(hir_js::Literal::BigInt(n)) => Some(n.clone()),
        _ => None,
      }
    }
  }
}

fn is_null_or_undefined_expr(lowered: &LowerResult, body: &hir_js::Body, expr: ExprId) -> bool {
  let expr = strip_transparent_wrappers(body, expr);
  let Some(expr) = body.exprs.get(expr.0 as usize) else {
    return false;
  };
  match &expr.kind {
    ExprKind::Literal(hir_js::Literal::Null) => true,
    ExprKind::Literal(hir_js::Literal::Undefined) => true,
    ExprKind::Ident(name) => lowered.names.resolve(*name) == Some("undefined"),
    _ => false,
  }
}

fn guard_clause_subject(lowered: &LowerResult, body: &hir_js::Body, test: ExprId) -> Option<ExprId> {
  let test = strip_transparent_wrappers(body, test);
  let test_expr = body.exprs.get(test.0 as usize)?;
  match &test_expr.kind {
    ExprKind::Unary {
      op: UnaryOp::Not,
      expr,
    } => Some(strip_transparent_wrappers(body, *expr)),
    ExprKind::Binary { op, left, right } => {
      if !matches!(op, BinaryOp::Equality | BinaryOp::StrictEquality) {
        return None;
      }
      let left = strip_transparent_wrappers(body, *left);
      let right = strip_transparent_wrappers(body, *right);
      if is_null_or_undefined_expr(lowered, body, right) {
        Some(left)
      } else if is_null_or_undefined_expr(lowered, body, left) {
        Some(right)
      } else {
        None
      }
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use hir_js::{lower_from_source_with_kind, FileKind};

  #[test]
  fn map_get_or_default_nullish_sets_conditional_to_binary_expr() {
    let lowered = lower_from_source_with_kind(
      FileKind::Ts,
      r#"
const m = new Map();
const k = "x";
const v = m.get(k) ?? 123;
"#,
    )
    .expect("lower source");

    let root_body = lowered.root_body();
    let body = lowered.body(root_body).expect("root body exists");

    let binary_expr_id = body
      .exprs
      .iter()
      .enumerate()
      .find_map(|(idx, expr)| match &expr.kind {
        ExprKind::Binary {
          op: BinaryOp::NullishCoalescing,
          ..
        } => Some(ExprId(idx as u32)),
        _ => None,
      })
      .expect("expected a nullish coalescing expression");

    let ExprKind::Binary { left, right, .. } = &body.exprs[binary_expr_id.0 as usize].kind else {
      unreachable!("binary expr id points at binary node")
    };
    let left = *left;
    let right = *right;

    let kb = crate::load_default_api_database();
    let patterns = recognize_patterns_best_effort_untyped(&kb, &lowered, root_body);

    let matches: Vec<_> = patterns
      .iter()
      .filter_map(|pat| match pat {
        RecognizedPattern::MapGetOrDefault {
          conditional,
          map,
          key,
          default,
        } if *conditional == binary_expr_id => Some((*map, *key, *default)),
        _ => None,
      })
      .collect();

    assert_eq!(
      matches.len(),
      1,
      "expected one MapGetOrDefault pattern, got {patterns:#?}"
    );
    let (map_expr, key_expr, default_expr) = matches[0];
    assert_eq!(default_expr, right);

    let ExprKind::Call(call) = &body.exprs[left.0 as usize].kind else {
      panic!("expected binary left to be a call expression");
    };
    let ExprKind::Member(member) = &body.exprs[call.callee.0 as usize].kind else {
      panic!("expected call callee to be a member expression");
    };
    assert_eq!(map_expr, member.object);
    assert_eq!(key_expr, call.args[0].expr);
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
  Return,
  Throw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayChainOp {
  Map { callback: ExprId },
  Filter { callback: ExprId },
  FlatMap { callback: ExprId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayTerminal {
  Reduce { callback: ExprId, init: Option<ExprId> },
  Find { callback: ExprId },
  Every { callback: ExprId },
  Some { callback: ExprId },
  ForEach { callback: ExprId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizedPattern {
  /// A call site that could be resolved to a canonical API identifier.
  CanonicalCall { call: ExprId, api: ApiId },

  /// `arr.map(f).filter(g).reduce(h, init)` (typed or best-effort).
  MapFilterReduce {
    base: ExprId,
    map_call: ExprId,
    filter_call: ExprId,
    reduce_call: ExprId,
  },

  /// Array prototype call chain such as `arr.map(f).filter(g).reduce(h, init)`
  /// (typed only).
  ArrayChain {
    base: ExprId,
    ops: Vec<ArrayChainOp>,
    terminal: Option<ArrayTerminal>,
  },

  /// `Promise.all(urls.map(url => fetch(url)))` (best-effort; untyped).
  PromiseAllFetch {
    promise_all_call: ExprId,
    urls_expr: ExprId,
    map_call: Option<ExprId>,
    fetch_call_count: usize,
  },

  /// `for await (const x of asyncIterable) { ... }` (untyped).
  AsyncIterator {
    stmt: StmtId,
    iterable: ExprId,
    binding_pat: PatId,
    binding_kind: Option<VarDeclKind>,
    body: StmtId,
  },

  /// `` `${a} ${b} ${c}` `` (untyped).
  StringTemplate { expr: ExprId, span_count: usize },

  /// `{ ...a, ...b, x: 1 }` (untyped).
  ObjectSpread { expr: ExprId, spread_count: usize },

  /// `const [a, b, c] = arr` (untyped).
  ArrayDestructure {
    stmt: StmtId,
    pat: hir_js::PatId,
    arity: usize,
    source: ExprId,
  },

  /// `if (!x) return;` / `if (!x) throw ...;` (untyped).
  GuardClause {
    stmt: StmtId,
    test: ExprId,
    kind: GuardKind,
  },

  /// `map.has(key) ? map.get(key) : default` (typed only).
  MapGetOrDefault {
    conditional: ExprId,
    map: ExprId,
    key: ExprId,
    default: ExprId,
  },

  /// `const x: T = JSON.parse(input)` (untyped; uses declared annotation).
  JsonParseTyped { call: ExprId, target: hir_js::TypeExprId },
}
fn walk_stmt(body: &hir_js::Body, stmt_id: StmtId, mut f: impl FnMut(StmtId, &StmtKind)) {
  fn walk(body: &hir_js::Body, stmt_id: StmtId, f: &mut impl FnMut(StmtId, &StmtKind)) {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      return;
    };
    f(stmt_id, &stmt.kind);
    match &stmt.kind {
      StmtKind::Block(stmts) => {
        for stmt in stmts {
          walk(body, *stmt, f);
        }
      }
      StmtKind::If {
        consequent,
        alternate,
        ..
      } => {
        walk(body, *consequent, f);
        if let Some(alt) = alternate {
          walk(body, *alt, f);
        }
      }
      StmtKind::While { body: inner, .. }
      | StmtKind::DoWhile { body: inner, .. }
      | StmtKind::With { body: inner, .. } => {
        walk(body, *inner, f);
      }
      StmtKind::For { body: inner, .. } => {
        walk(body, *inner, f);
      }
      StmtKind::ForIn { body: inner, .. } => {
        walk(body, *inner, f);
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        walk(body, *block, f);
        if let Some(catch) = catch {
          walk(body, catch.body, f);
        }
        if let Some(finally) = finally_block {
          walk(body, *finally, f);
        }
      }
      StmtKind::Switch { cases, .. } => {
        for case in cases {
          for stmt in &case.consequent {
            walk(body, *stmt, f);
          }
        }
      }
      StmtKind::Labeled { body: inner, .. } => walk(body, *inner, f),
      _ => {}
    }
  }

  walk(body, stmt_id, &mut f)
}

fn strip_transparent_wrappers(body: &hir_js::Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

fn unwrap_await_value(body: &hir_js::Body, expr: ExprId) -> Option<ExprId> {
  match &body.exprs.get(expr.0 as usize)?.kind {
    ExprKind::Await { expr } => Some(*expr),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::AwaitExpr { value, .. } => Some(*value),
    _ => None,
  }
}

fn sort_patterns_by_span(body: &hir_js::Body, patterns: &mut Vec<RecognizedPattern>) {
  fn expr_span(body: &hir_js::Body, expr: ExprId) -> Option<(u32, u32)> {
    let expr = body.exprs.get(expr.0 as usize)?;
    Some((expr.span.start, expr.span.end))
  }

  fn stmt_span(body: &hir_js::Body, stmt: StmtId) -> Option<(u32, u32)> {
    let stmt = body.stmts.get(stmt.0 as usize)?;
    Some((stmt.span.start, stmt.span.end))
  }

  fn pat_span(body: &hir_js::Body, pat: PatId) -> Option<(u32, u32)> {
    let pat = body.pats.get(pat.0 as usize)?;
    Some((pat.span.start, pat.span.end))
  }

  fn key(body: &hir_js::Body, pattern: &RecognizedPattern) -> (u32, u32, u8, u32) {
    match pattern {
      RecognizedPattern::CanonicalCall { call, .. } => expr_span(body, *call)
        .map(|(s, e)| (s, e, 0, call.0))
        .unwrap_or((u32::MAX, u32::MAX, 0, call.0)),
      RecognizedPattern::JsonParseTyped { call, .. } => expr_span(body, *call)
        .map(|(s, e)| (s, e, 1, call.0))
        .unwrap_or((u32::MAX, u32::MAX, 1, call.0)),
      RecognizedPattern::MapFilterReduce {
        base,
        reduce_call,
        ..
      } => {
        let start = expr_span(body, *base).map(|(s, _)| s).unwrap_or(u32::MAX);
        let end = expr_span(body, *reduce_call)
          .map(|(_, e)| e)
          .unwrap_or(u32::MAX);
        (start, end, 2, reduce_call.0)
      }
      RecognizedPattern::ArrayChain {
        base,
        ops,
        terminal,
      } => {
        let start = expr_span(body, *base).map(|(s, _)| s).unwrap_or(u32::MAX);
        let end_expr = match terminal {
          Some(ArrayTerminal::Reduce { init: Some(init), .. }) => Some(*init),
          Some(ArrayTerminal::Reduce { callback, .. }) => Some(*callback),
          Some(ArrayTerminal::Find { callback })
          | Some(ArrayTerminal::Every { callback })
          | Some(ArrayTerminal::Some { callback })
          | Some(ArrayTerminal::ForEach { callback }) => Some(*callback),
          None => ops.last().map(|op| match op {
            ArrayChainOp::Map { callback }
            | ArrayChainOp::Filter { callback }
            | ArrayChainOp::FlatMap { callback } => *callback,
          }),
        };
        let end = end_expr
          .and_then(|expr| expr_span(body, expr))
          .map(|(_, e)| e)
          .unwrap_or(u32::MAX);
        (start, end, 3, base.0)
      }
      RecognizedPattern::PromiseAllFetch {
        promise_all_call, ..
      } => expr_span(body, *promise_all_call)
        .map(|(s, e)| (s, e, 4, promise_all_call.0))
        .unwrap_or((u32::MAX, u32::MAX, 4, promise_all_call.0)),
      RecognizedPattern::AsyncIterator { stmt, .. } => stmt_span(body, *stmt)
        .map(|(s, e)| (s, e, 5, stmt.0))
        .unwrap_or((u32::MAX, u32::MAX, 5, stmt.0)),
      RecognizedPattern::StringTemplate { expr, .. } => expr_span(body, *expr)
        .map(|(s, e)| (s, e, 6, expr.0))
        .unwrap_or((u32::MAX, u32::MAX, 6, expr.0)),
      RecognizedPattern::ObjectSpread { expr, .. } => expr_span(body, *expr)
        .map(|(s, e)| (s, e, 7, expr.0))
        .unwrap_or((u32::MAX, u32::MAX, 7, expr.0)),
      RecognizedPattern::ArrayDestructure { pat, .. } => pat_span(body, *pat)
        .map(|(s, e)| (s, e, 8, pat.0))
        .unwrap_or((u32::MAX, u32::MAX, 8, pat.0)),
      RecognizedPattern::GuardClause { stmt, .. } => stmt_span(body, *stmt)
        .map(|(s, e)| (s, e, 9, stmt.0))
        .unwrap_or((u32::MAX, u32::MAX, 9, stmt.0)),
      RecognizedPattern::MapGetOrDefault { map, .. } => expr_span(body, *map)
        .map(|(s, e)| (s, e, 10, map.0))
        .unwrap_or((u32::MAX, u32::MAX, 10, map.0)),
    }
  }

  patterns.sort_by(|a, b| key(body, a).cmp(&key(body, b)));
}

#[cfg(feature = "typed")]
fn collect_reachable_exprs(body: &hir_js::Body) -> std::collections::HashSet<ExprId> {
  fn visit_pat(
    body: &hir_js::Body,
    pat_id: PatId,
    reachable: &mut std::collections::HashSet<ExprId>,
  ) {
    let Some(pat) = body.pats.get(pat_id.0 as usize) else {
      return;
    };
    match &pat.kind {
      PatKind::Ident(_) => {}
      PatKind::Array(arr) => {
        for element in &arr.elements {
          if let Some(element) = element {
            visit_pat(body, element.pat, reachable);
            if let Some(default_value) = element.default_value {
              visit_expr(body, default_value, reachable);
            }
          }
        }
        if let Some(rest) = arr.rest {
          visit_pat(body, rest, reachable);
        }
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          visit_pat(body, prop.value, reachable);
          if let Some(default_value) = prop.default_value {
            visit_expr(body, default_value, reachable);
          }
          if let hir_js::ObjectKey::Computed(expr) = prop.key {
            visit_expr(body, expr, reachable);
          }
        }
        if let Some(rest) = obj.rest {
          visit_pat(body, rest, reachable);
        }
      }
      PatKind::Rest(inner) => visit_pat(body, **inner, reachable),
      PatKind::Assign {
        target,
        default_value,
      } => {
        visit_pat(body, *target, reachable);
        visit_expr(body, *default_value, reachable);
      }
      PatKind::AssignTarget(expr) => visit_expr(body, *expr, reachable),
    }
  }

  fn visit_var_decl(
    body: &hir_js::Body,
    decl: &hir_js::VarDecl,
    reachable: &mut std::collections::HashSet<ExprId>,
  ) {
    for d in &decl.declarators {
      visit_pat(body, d.pat, reachable);
      if let Some(init) = d.init {
        visit_expr(body, init, reachable);
      }
    }
  }

  fn visit_stmt(
    body: &hir_js::Body,
    stmt_id: StmtId,
    reachable: &mut std::collections::HashSet<ExprId>,
  ) {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      return;
    };
    match &stmt.kind {
      StmtKind::Expr(expr) => visit_expr(body, *expr, reachable),
      StmtKind::Decl(_) => {}
      StmtKind::Return(expr) => {
        if let Some(expr) = expr {
          visit_expr(body, *expr, reachable);
        }
      }
      StmtKind::Block(stmts) => {
        for stmt in stmts {
          visit_stmt(body, *stmt, reachable);
        }
      }
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => {
        visit_expr(body, *test, reachable);
        visit_stmt(body, *consequent, reachable);
        if let Some(alternate) = alternate {
          visit_stmt(body, *alternate, reachable);
        }
      }
      StmtKind::While { test, body: inner } | StmtKind::DoWhile { test, body: inner } => {
        visit_expr(body, *test, reachable);
        visit_stmt(body, *inner, reachable);
      }
      StmtKind::For {
        init,
        test,
        update,
        body: inner,
      } => {
        if let Some(init) = init {
          match init {
            hir_js::ForInit::Expr(expr) => visit_expr(body, *expr, reachable),
            hir_js::ForInit::Var(var) => visit_var_decl(body, var, reachable),
          }
        }
        if let Some(test) = test {
          visit_expr(body, *test, reachable);
        }
        if let Some(update) = update {
          visit_expr(body, *update, reachable);
        }
        visit_stmt(body, *inner, reachable);
      }
      StmtKind::ForIn {
        left,
        right,
        body: inner,
        ..
      } => {
        match left {
          hir_js::ForHead::Pat(pat) => visit_pat(body, *pat, reachable),
          hir_js::ForHead::Var(var) => visit_var_decl(body, var, reachable),
        }
        visit_expr(body, *right, reachable);
        visit_stmt(body, *inner, reachable);
      }
      StmtKind::Switch { discriminant, cases } => {
        visit_expr(body, *discriminant, reachable);
        for case in cases {
          if let Some(test) = case.test {
            visit_expr(body, test, reachable);
          }
          for stmt in &case.consequent {
            visit_stmt(body, *stmt, reachable);
          }
        }
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        visit_stmt(body, *block, reachable);
        if let Some(catch) = catch {
          if let Some(param) = catch.param {
            visit_pat(body, param, reachable);
          }
          visit_stmt(body, catch.body, reachable);
        }
        if let Some(finally) = finally_block {
          visit_stmt(body, *finally, reachable);
        }
      }
      StmtKind::Throw(expr) => visit_expr(body, *expr, reachable),
      StmtKind::Break(_) | StmtKind::Continue(_) => {}
      StmtKind::Var(var) => visit_var_decl(body, var, reachable),
      StmtKind::Labeled { body: inner, .. } => visit_stmt(body, *inner, reachable),
      StmtKind::With { object, body: inner } => {
        visit_expr(body, *object, reachable);
        visit_stmt(body, *inner, reachable);
      }
      StmtKind::Debugger | StmtKind::Empty => {}
    }
  }

  fn visit_expr(body: &hir_js::Body, expr_id: ExprId, reachable: &mut std::collections::HashSet<ExprId>) {
    if !reachable.insert(expr_id) {
      return;
    }
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return;
    };
    match &expr.kind {
      ExprKind::Missing
      | ExprKind::Ident(_)
      | ExprKind::This
      | ExprKind::Super
      | ExprKind::Literal(_)
      | ExprKind::ImportMeta
      | ExprKind::NewTarget => {}
      ExprKind::Unary { expr, .. }
      | ExprKind::Update { expr, .. }
      | ExprKind::Await { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::NonNull { expr } => {
        visit_expr(body, *expr, reachable);
      }
      ExprKind::Binary { left, right, .. } => {
        visit_expr(body, *left, reachable);
        visit_expr(body, *right, reachable);
      }
      ExprKind::Assignment { target, value, .. } => {
        visit_pat(body, *target, reachable);
        visit_expr(body, *value, reachable);
      }
      ExprKind::Call(call) => {
        visit_expr(body, call.callee, reachable);
        for arg in &call.args {
          visit_expr(body, arg.expr, reachable);
        }
      }
      ExprKind::Member(member) => {
        visit_expr(body, member.object, reachable);
        if let hir_js::ObjectKey::Computed(expr) = member.property {
          visit_expr(body, expr, reachable);
        }
      }
      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        visit_expr(body, *test, reachable);
        visit_expr(body, *consequent, reachable);
        visit_expr(body, *alternate, reachable);
      }
      ExprKind::Array(array) => {
        for element in &array.elements {
          match element {
            ArrayElement::Expr(expr) | ArrayElement::Spread(expr) => visit_expr(body, *expr, reachable),
            ArrayElement::Empty => {}
          }
        }
      }
      ExprKind::Object(object) => {
        for prop in &object.properties {
          match prop {
            ObjectProperty::KeyValue { key, value, .. } => {
              if let hir_js::ObjectKey::Computed(expr) = key {
                visit_expr(body, *expr, reachable);
              }
              visit_expr(body, *value, reachable);
            }
            ObjectProperty::Getter { key, .. } | ObjectProperty::Setter { key, .. } => {
              if let hir_js::ObjectKey::Computed(expr) = key {
                visit_expr(body, *expr, reachable);
              }
            }
            ObjectProperty::Spread(expr) => visit_expr(body, *expr, reachable),
          }
        }
      }
      ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => {}
      ExprKind::Template(template) => {
        for span in &template.spans {
          visit_expr(body, span.expr, reachable);
        }
      }
      ExprKind::TaggedTemplate { tag, template } => {
        visit_expr(body, *tag, reachable);
        for span in &template.spans {
          visit_expr(body, span.expr, reachable);
        }
      }
      ExprKind::Yield { expr: Some(expr), .. } => visit_expr(body, *expr, reachable),
      ExprKind::Yield { expr: None, .. } => {}
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => visit_expr(body, *expr, reachable),
      ExprKind::ImportCall { argument, attributes } => {
        visit_expr(body, *argument, reachable);
        if let Some(attributes) = attributes {
          visit_expr(body, *attributes, reachable);
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, callback }
      | ExprKind::ArrayFilter { array, callback }
      | ExprKind::ArrayFind { array, callback }
      | ExprKind::ArrayEvery { array, callback }
      | ExprKind::ArraySome { array, callback } => {
        visit_expr(body, *array, reachable);
        visit_expr(body, *callback, reachable);
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        visit_expr(body, *array, reachable);
        visit_expr(body, *callback, reachable);
        if let Some(init) = init {
          visit_expr(body, *init, reachable);
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        visit_expr(body, *array, reachable);
        for op in ops {
          match op {
            hir_js::ArrayChainOp::Map(cb)
            | hir_js::ArrayChainOp::Filter(cb)
            | hir_js::ArrayChainOp::Find(cb)
            | hir_js::ArrayChainOp::Every(cb)
            | hir_js::ArrayChainOp::Some(cb) => visit_expr(body, *cb, reachable),
            hir_js::ArrayChainOp::Reduce(cb, init) => {
              visit_expr(body, *cb, reachable);
              if let Some(init) = init {
                visit_expr(body, *init, reachable);
              }
            }
          }
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        for promise in promises {
          visit_expr(body, *promise, reachable);
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::AwaitExpr { value, .. } => visit_expr(body, *value, reachable),
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::KnownApiCall { args, .. } => {
        for arg in args {
          visit_expr(body, *arg, reachable);
        }
      }
      ExprKind::Jsx(jsx) => {
        for attr in &jsx.attributes {
          match attr {
            hir_js::JsxAttr::Named { value, .. } => {
              if let Some(value) = value {
                match value {
                  hir_js::JsxAttrValue::Expression(container) => {
                    if let Some(expr) = container.expr {
                      visit_expr(body, expr, reachable);
                    }
                  }
                  hir_js::JsxAttrValue::Element(expr) => visit_expr(body, *expr, reachable),
                  hir_js::JsxAttrValue::Text(_) => {}
                }
              }
            }
            hir_js::JsxAttr::Spread { expr } => visit_expr(body, *expr, reachable),
          }
        }
        for child in &jsx.children {
          match child {
            hir_js::JsxChild::Element(expr) => visit_expr(body, *expr, reachable),
            hir_js::JsxChild::Expr(container) => {
              if let Some(expr) = container.expr {
                visit_expr(body, expr, reachable);
              }
            }
            hir_js::JsxChild::Text(_) => {}
          }
        }
      }
    }
  }

  let mut reachable = std::collections::HashSet::new();
  for stmt in &body.root_stmts {
    visit_stmt(body, *stmt, &mut reachable);
  }
  reachable
}

pub fn recognize_patterns_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
) -> Vec<RecognizedPattern> {
  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let resolver = ApiCallResolver::new(kb, lowered);
  let json_parse = kb.id_of("JSON.parse");
  let mut patterns = Vec::new();

  // 1) Canonical call sites that are safe to resolve from HIR alone (e.g. JSON.parse).
  for (idx, _expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if let Some(api) = resolver.resolve_call_untyped(body, expr_id) {
      patterns.push(RecognizedPattern::CanonicalCall {
        call: expr_id,
        api,
      });
    }
  }

  // 2) Patterns that can be inferred from declared annotations without full typing.
  for stmt_id in &body_ref.root_stmts {
    walk_stmt(body_ref, *stmt_id, |_stmt_id, stmt| {
      let StmtKind::Var(var) = stmt else {
        return;
      };
      for decl in &var.declarators {
        let Some(target) = decl.type_annotation else {
          continue;
        };
        let Some(init) = decl.init else {
          continue;
        };
        if json_parse.is_some_and(|id| resolver.resolve_call_untyped(body, init) == Some(id)) {
          patterns.push(RecognizedPattern::JsonParseTyped { call: init, target });
        }
      }
    });
  }

  sort_patterns_by_span(body_ref, &mut patterns);
  patterns
}

#[cfg(feature = "typed")]
fn promise_all_fetch_match_typed(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &impl crate::types::TypeProvider,
) -> Option<PromiseAllFetchMatch> {
  let body_ref = lowered.body(body)?;
  let promise_all = kb.id_of("Promise.all")?;
  let fetch = kb.id_of("fetch")?;
  let array_map = kb.id_of("Array.prototype.map")?;

  let resolver = ApiCallResolver::new(kb, lowered);

  let call_node = body_ref.exprs.get(call_expr.0 as usize)?;

  #[cfg(feature = "hir-semantic-ops")]
  if let ExprKind::PromiseAll { promises } = &call_node.kind {
    let fetch_call_count = promises
      .iter()
      .filter(|expr_id| resolver.resolve_call_untyped(body, **expr_id) == Some(fetch))
      .count();
    return (fetch_call_count > 0).then_some(PromiseAllFetchMatch {
      // `hir-js` lowers `Promise.all([..])` into a `PromiseAll { promises }` node
      // and drops the array-literal wrapper. Use the `PromiseAll` expr itself as
      // the "urls expression" marker.
      urls_expr: call_expr,
      map_call: None,
      fetch_call_count,
    });
  }

  if resolver.resolve_call_typed(body, call_expr, types) != Some(promise_all) {
    return None;
  }

  let ExprKind::Call(call) = &call_node.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let arg0 = call.args.first()?;
  if arg0.spread {
    return None;
  }

  let arg_expr_id = strip_transparent_wrappers(body_ref, arg0.expr);
  let arg_expr = body_ref.exprs.get(arg_expr_id.0 as usize)?;
  match &arg_expr.kind {
    ExprKind::Array(array) => {
      let mut fetch_call_count = 0usize;
      for element in &array.elements {
        match element {
          ArrayElement::Expr(expr_id) => {
            let expr_id = strip_transparent_wrappers(body_ref, *expr_id);
            if resolver.resolve_call_untyped(body, expr_id) == Some(fetch) {
              fetch_call_count += 1;
            }
          }
          ArrayElement::Empty => {}
          ArrayElement::Spread(_) => return None,
        }
      }
      (fetch_call_count > 0).then_some(PromiseAllFetchMatch {
        urls_expr: arg_expr_id,
        map_call: None,
        fetch_call_count,
      })
    }
    ExprKind::Call(map_call) => {
      if map_call.optional || map_call.is_new {
        return None;
      }
      if resolver.resolve_call_typed(body, arg_expr_id, types) != Some(array_map) {
        return None;
      }

      let callee = body_ref.exprs.get(map_call.callee.0 as usize)?;
      let ExprKind::Member(member) = &callee.kind else {
        return None;
      };
      if member.optional {
        return None;
      }

      let cb_arg = map_call.args.first()?;
      if cb_arg.spread {
        return None;
      }
      let cb_expr_id = strip_transparent_wrappers(body_ref, cb_arg.expr);
      let cb_expr = body_ref.exprs.get(cb_expr_id.0 as usize)?;

      match &cb_expr.kind {
        ExprKind::Ident(name) if lowered.names.resolve(*name) == Some("fetch") => Some(PromiseAllFetchMatch {
          urls_expr: strip_transparent_wrappers(body_ref, member.object),
          map_call: Some(arg_expr_id),
          fetch_call_count: 1,
        }),
        ExprKind::FunctionExpr { body: cb_body, .. } => {
          let cb_body_id = *cb_body;
          let cb_body = lowered.body(cb_body_id)?;
          let func = cb_body.function.as_ref()?;
          let ret_expr = match &func.body {
            hir_js::FunctionBody::Expr(expr) => Some(*expr),
            hir_js::FunctionBody::Block(stmts) if stmts.len() == 1 => {
              let stmt = cb_body.stmts.get(stmts[0].0 as usize)?;
              let StmtKind::Return(Some(expr)) = &stmt.kind else {
                return None;
              };
              Some(*expr)
            }
            _ => None,
          }?;

          let ret_expr = strip_transparent_wrappers(cb_body, ret_expr);
          let fetch_call_count =
            if resolver.resolve_call_untyped(cb_body_id, ret_expr) == Some(fetch) {
              1
            } else if let Some(expr) = unwrap_await_value(cb_body, ret_expr) {
              let expr = strip_transparent_wrappers(cb_body, expr);
              if resolver.resolve_call_untyped(cb_body_id, expr) == Some(fetch) {
                1
              } else {
                return None;
              }
            } else {
              return None;
            };

          Some(PromiseAllFetchMatch {
            urls_expr: strip_transparent_wrappers(body_ref, member.object),
            map_call: Some(arg_expr_id),
            fetch_call_count,
          })
        }
        _ => None,
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { array, callback } => {
      let urls_expr = strip_transparent_wrappers(body_ref, *array);
      if !types.expr_is_array(body, urls_expr) {
        return None;
      }

      let cb_expr_id = strip_transparent_wrappers(body_ref, *callback);
      let cb_expr = body_ref.exprs.get(cb_expr_id.0 as usize)?;

      match &cb_expr.kind {
        ExprKind::Ident(name) if lowered.names.resolve(*name) == Some("fetch") => Some(PromiseAllFetchMatch {
          urls_expr,
          map_call: Some(arg_expr_id),
          fetch_call_count: 1,
        }),
        ExprKind::FunctionExpr { body: cb_body, .. } => {
          let cb_body_id = *cb_body;
          let cb_body = lowered.body(cb_body_id)?;
          let func = cb_body.function.as_ref()?;
          let ret_expr = match &func.body {
            hir_js::FunctionBody::Expr(expr) => Some(*expr),
            hir_js::FunctionBody::Block(stmts) if stmts.len() == 1 => {
              let stmt = cb_body.stmts.get(stmts[0].0 as usize)?;
              let StmtKind::Return(Some(expr)) = &stmt.kind else {
                return None;
              };
              Some(*expr)
            }
            _ => None,
          }?;

          let ret_expr = strip_transparent_wrappers(cb_body, ret_expr);
          let fetch_call_count =
            if resolver.resolve_call_untyped(cb_body_id, ret_expr) == Some(fetch) {
              1
            } else if let Some(expr) = unwrap_await_value(cb_body, ret_expr) {
              let expr = strip_transparent_wrappers(cb_body, expr);
              if resolver.resolve_call_untyped(cb_body_id, expr) == Some(fetch) {
                1
              } else {
                return None;
              }
            } else {
              return None;
            };

          Some(PromiseAllFetchMatch {
            urls_expr,
            map_call: Some(arg_expr_id),
            fetch_call_count,
          })
        }
        _ => None,
      }
    }
    _ => None,
  }
}

fn call_chain(lowered: &LowerResult, body: BodyId, call_expr: ExprId) -> Option<(ExprId, Vec<(ExprId, String)>)> {
  let body_ref = lowered.body(body)?;
  let mut methods = Vec::new();
  let mut cur = call_expr;

  loop {
    let call = body_ref.exprs.get(cur.0 as usize)?;
    let ExprKind::Call(call) = &call.kind else {
      return None;
    };
    if call.optional || call.is_new {
      return None;
    }

    let callee_expr = body_ref.exprs.get(call.callee.0 as usize)?;
    let ExprKind::Member(member) = &callee_expr.kind else {
      return None;
    };
    if member.optional {
      return None;
    }
    let prop = static_object_key_name(lowered, body_ref, &member.property)?;
    methods.push((cur, prop));

    let recv = member.object;
    match body_ref.exprs.get(recv.0 as usize).map(|e| &e.kind) {
      Some(ExprKind::Call(_)) => cur = recv,
      Some(_) => {
        methods.reverse();
        return Some((recv, methods));
      }
      None => return None,
    }
  }
}

/// Like [`recognize_patterns_untyped`], but includes additional best-effort
/// patterns that can be inferred without type information.
pub fn recognize_patterns_best_effort_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
) -> Vec<RecognizedPattern> {
  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let mut patterns = recognize_patterns_untyped(kb, lowered, body);

  for stmt_id in &body_ref.root_stmts {
    walk_stmt(body_ref, *stmt_id, |stmt_id, stmt| match stmt {
      StmtKind::ForIn {
        is_for_of: true,
        await_: true,
        left,
        right,
        body,
        ..
      } => {
        let (binding_pat, binding_kind) = match left {
          hir_js::ForHead::Pat(pat) => (*pat, None),
          hir_js::ForHead::Var(var) => {
            let Some(decl) = var.declarators.first() else {
              return;
            };
            (decl.pat, Some(var.kind))
          }
        };

        patterns.push(RecognizedPattern::AsyncIterator {
          stmt: stmt_id,
          iterable: *right,
          binding_pat,
          binding_kind,
          body: *body,
        });
      }
      StmtKind::Var(var) => {
        for decl in &var.declarators {
          let Some(source) = decl.init else {
            continue;
          };
          let pat_id = decl.pat;
          let Some(pat) = body_ref.pats.get(pat_id.0 as usize) else {
            continue;
          };
          let PatKind::Array(array) = &pat.kind else {
            continue;
          };
          if array.rest.is_some() {
            continue;
          }
          let binding_count = array.elements.iter().flatten().count();
          if binding_count == 0 {
            continue;
          }
          if array
            .elements
            .iter()
            .flatten()
            .any(|e| e.default_value.is_some())
          {
            continue;
          }
          patterns.push(RecognizedPattern::ArrayDestructure {
            stmt: stmt_id,
            pat: pat_id,
            arity: binding_count,
            source,
          });
        }
      }
      StmtKind::If {
        test,
        consequent,
        alternate: None,
      } => {
        let if_stmt_id = stmt_id;
        let Some(subject) = guard_clause_subject(lowered, body_ref, *test) else {
          return;
        };

        let mut arm = Some(*consequent);
        while let Some(consequent_id) = arm.take() {
          let Some(consequent_stmt) = body_ref.stmts.get(consequent_id.0 as usize) else {
            break;
          };
          match &consequent_stmt.kind {
            StmtKind::Return(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: subject,
                kind: GuardKind::Return,
              });
            }
            StmtKind::Throw(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: subject,
                kind: GuardKind::Throw,
              });
            }
            StmtKind::Block(stmts) if stmts.len() == 1 => {
              arm = stmts.first().copied();
              continue;
            }
            _ => {}
          }
          break;
        }
      }
      _ => {}
    });
  }

  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);

    // MapFilterReduce: recognize only at the terminal `reduce(...)` call.
    if let Some((base, chain)) = call_chain(lowered, body, expr_id) {
      if chain.len() == 3
        && chain[2].1 == "reduce"
        && chain[0].1 == "map"
        && chain[1].1 == "filter"
      {
        patterns.push(RecognizedPattern::MapFilterReduce {
          base,
          map_call: chain[0].0,
          filter_call: chain[1].0,
          reduce_call: chain[2].0,
        });
      }
    }

    if let Some(m) = promise_all_fetch_match_untyped(kb, lowered, body, expr_id) {
      patterns.push(RecognizedPattern::PromiseAllFetch {
        promise_all_call: expr_id,
        urls_expr: m.urls_expr,
        map_call: m.map_call,
        fetch_call_count: m.fetch_call_count,
      });
    }

    if let ExprKind::Template(template) = &expr.kind {
      if template.spans.len() >= 2 {
        patterns.push(RecognizedPattern::StringTemplate {
          expr: expr_id,
          span_count: template.spans.len(),
        });
      }
    }

    if let ExprKind::Object(obj) = &expr.kind {
      let mut spread_count = 0usize;
      let mut valid = true;
      for prop in &obj.properties {
        match prop {
          ObjectProperty::Spread(_) => spread_count += 1,
          ObjectProperty::KeyValue { key, .. } => {
            if matches!(key, hir_js::ObjectKey::Computed(_)) {
              valid = false;
              break;
            }
          }
          ObjectProperty::Getter { .. } | ObjectProperty::Setter { .. } => {
            valid = false;
            break;
          }
        }
      }
      if valid && spread_count > 0 {
        patterns.push(RecognizedPattern::ObjectSpread {
          expr: expr_id,
          spread_count,
        });
      }
    }

    // Best-effort MapGetOrDefault: `m.has(k) ? m.get(k) : default`.
    if let ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } = &expr.kind
    {
      let Some((has_map, has_prop, has_key)) = parse_simple_method_call_untyped(lowered, body, *test)
      else {
        continue;
      };
      if has_prop != "has" {
        continue;
      }

      let Some((get_map, get_prop, get_key)) =
        parse_simple_method_call_untyped(lowered, body, *consequent)
      else {
        continue;
      };
      if get_prop != "get" {
        continue;
      }

      let Some(has_map_fp) = expr_fingerprint(lowered, body, has_map) else {
        continue;
      };
      let Some(get_map_fp) = expr_fingerprint(lowered, body, get_map) else {
        continue;
      };
      if has_map_fp != get_map_fp {
        continue;
      }

      let Some(has_key_fp) = expr_fingerprint(lowered, body, has_key) else {
        continue;
      };
      let Some(get_key_fp) = expr_fingerprint(lowered, body, get_key) else {
        continue;
      };
      if has_key_fp != get_key_fp {
        continue;
      }

      patterns.push(RecognizedPattern::MapGetOrDefault {
        conditional: expr_id,
        map: has_map,
        key: has_key,
        default: *alternate,
      });
    }

    // Best-effort MapGetOrDefault: `m.get(k) ?? default` / `m.get(k) || default`.
    if let ExprKind::Binary { op, left, right } = &expr.kind {
      if !matches!(op, BinaryOp::NullishCoalescing | BinaryOp::LogicalOr) {
        continue;
      }
      let Some((map, get_prop, key)) = parse_simple_method_call_untyped(lowered, body, *left) else {
        continue;
      };
      if get_prop != "get" {
        continue;
      }
      patterns.push(RecognizedPattern::MapGetOrDefault {
        conditional: expr_id,
        map,
        key,
        default: *right,
      });
    }
  }

  sort_patterns_by_span(body_ref, &mut patterns);
  patterns
}

struct PromiseAllFetchMatch {
  urls_expr: ExprId,
  map_call: Option<ExprId>,
  fetch_call_count: usize,
}

fn promise_all_fetch_match_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<PromiseAllFetchMatch> {
  let body_ref = lowered.body(body)?;
  let promise_all = kb.id_of("Promise.all")?;
  let fetch = kb.id_of("fetch")?;
  let array_map = kb.id_of("Array.prototype.map")?;

  let resolver = ApiCallResolver::new(kb, lowered);

  let call_node = body_ref.exprs.get(call_expr.0 as usize)?;

  #[cfg(feature = "hir-semantic-ops")]
  if let ExprKind::PromiseAll { promises } = &call_node.kind {
    let fetch_call_count = promises
      .iter()
      .filter(|expr_id| resolver.resolve_call_untyped(body, **expr_id) == Some(fetch))
      .count();
    return (fetch_call_count > 0).then_some(PromiseAllFetchMatch {
      // `hir-js` lowers `Promise.all([..])` into a `PromiseAll { promises }` node
      // and drops the array-literal wrapper. Use the `PromiseAll` expr itself as
      // the "urls expression" marker.
      urls_expr: call_expr,
      map_call: None,
      fetch_call_count,
    });
  }

  if resolver.resolve_call_untyped(body, call_expr) != Some(promise_all) {
    return None;
  }

  let ExprKind::Call(call) = &call_node.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let arg0 = call.args.first()?;
  if arg0.spread {
    return None;
  }

  let arg_expr_id = strip_transparent_wrappers(body_ref, arg0.expr);
  let arg_expr = body_ref.exprs.get(arg_expr_id.0 as usize)?;
  match &arg_expr.kind {
    ExprKind::Array(array) => {
      let mut fetch_call_count = 0usize;
      for element in &array.elements {
        match element {
          ArrayElement::Expr(expr_id) => {
            let expr_id = strip_transparent_wrappers(body_ref, *expr_id);
            if resolver.resolve_call_untyped(body, expr_id) == Some(fetch) {
              fetch_call_count += 1;
            }
          }
          ArrayElement::Empty => {}
          ArrayElement::Spread(_) => return None,
        }
      }
      (fetch_call_count > 0).then_some(PromiseAllFetchMatch {
        urls_expr: arg_expr_id,
        map_call: None,
        fetch_call_count,
      })
    }
    ExprKind::Call(map_call) => {
      if map_call.optional || map_call.is_new {
        return None;
      }
      if resolver.resolve_call_best_effort_untyped(body, arg_expr_id) != Some(array_map) {
        return None;
      }

      let callee = body_ref.exprs.get(map_call.callee.0 as usize)?;
      let ExprKind::Member(member) = &callee.kind else {
        return None;
      };
      if member.optional {
        return None;
      }

      let cb_arg = map_call.args.first()?;
      if cb_arg.spread {
        return None;
      }
      let cb_expr_id = strip_transparent_wrappers(body_ref, cb_arg.expr);
      let cb_expr = body_ref.exprs.get(cb_expr_id.0 as usize)?;

      match &cb_expr.kind {
        ExprKind::Ident(name) if lowered.names.resolve(*name) == Some("fetch") => Some(PromiseAllFetchMatch {
          urls_expr: strip_transparent_wrappers(body_ref, member.object),
          map_call: Some(arg_expr_id),
          fetch_call_count: 1,
        }),
        ExprKind::FunctionExpr { body: cb_body, .. } => {
          let cb_body_id = *cb_body;
          let cb_body = lowered.body(cb_body_id)?;
          let func = cb_body.function.as_ref()?;
          let ret_expr = match &func.body {
            hir_js::FunctionBody::Expr(expr) => Some(*expr),
            hir_js::FunctionBody::Block(stmts) if stmts.len() == 1 => {
              let stmt = cb_body.stmts.get(stmts[0].0 as usize)?;
              let StmtKind::Return(Some(expr)) = &stmt.kind else {
                return None;
              };
              Some(*expr)
            }
            _ => None,
          }?;

          let ret_expr = strip_transparent_wrappers(cb_body, ret_expr);
          let fetch_call_count =
            if resolver.resolve_call_untyped(cb_body_id, ret_expr) == Some(fetch) {
              1
            } else if let Some(expr) = unwrap_await_value(cb_body, ret_expr) {
              let expr = strip_transparent_wrappers(cb_body, expr);
              if resolver.resolve_call_untyped(cb_body_id, expr) == Some(fetch) {
                1
              } else {
                return None;
              }
            } else {
              return None;
            };

          Some(PromiseAllFetchMatch {
            urls_expr: strip_transparent_wrappers(body_ref, member.object),
            map_call: Some(arg_expr_id),
            fetch_call_count,
          })
        }
        _ => None,
      }
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { array, callback } => {
      let urls_expr = strip_transparent_wrappers(body_ref, *array);

      let cb_expr_id = strip_transparent_wrappers(body_ref, *callback);
      let cb_expr = body_ref.exprs.get(cb_expr_id.0 as usize)?;

      match &cb_expr.kind {
        ExprKind::Ident(name) if lowered.names.resolve(*name) == Some("fetch") => Some(PromiseAllFetchMatch {
          urls_expr,
          map_call: Some(arg_expr_id),
          fetch_call_count: 1,
        }),
        ExprKind::FunctionExpr { body: cb_body, .. } => {
          let cb_body_id = *cb_body;
          let cb_body = lowered.body(cb_body_id)?;
          let func = cb_body.function.as_ref()?;
          let ret_expr = match &func.body {
            hir_js::FunctionBody::Expr(expr) => Some(*expr),
            hir_js::FunctionBody::Block(stmts) if stmts.len() == 1 => {
              let stmt = cb_body.stmts.get(stmts[0].0 as usize)?;
              let StmtKind::Return(Some(expr)) = &stmt.kind else {
                return None;
              };
              Some(*expr)
            }
            _ => None,
          }?;

          let ret_expr = strip_transparent_wrappers(cb_body, ret_expr);
          let fetch_call_count =
            if resolver.resolve_call_untyped(cb_body_id, ret_expr) == Some(fetch) {
              1
            } else if let Some(expr) = unwrap_await_value(cb_body, ret_expr) {
              let expr = strip_transparent_wrappers(cb_body, expr);
              if resolver.resolve_call_untyped(cb_body_id, expr) == Some(fetch) {
                1
              } else {
                return None;
              }
            } else {
              return None;
            };

          Some(PromiseAllFetchMatch {
            urls_expr,
            map_call: Some(arg_expr_id),
            fetch_call_count,
          })
        }
        _ => None,
      }
    }
    _ => None,
  }
}

#[cfg(feature = "typed")]
enum ArrayCallNode {
  Op(ArrayChainOp),
  Terminal(ArrayTerminal),
}

#[cfg(feature = "typed")]
fn classify_array_call(
  lowered: &LowerResult,
  body: &hir_js::Body,
  call_expr: ExprId,
) -> Option<(ExprId, ArrayCallNode)> {
  let call = body.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }
  if call.args.iter().any(|arg| arg.spread) {
    return None;
  }

  let callee_expr = body.exprs.get(call.callee.0 as usize)?;
  let ExprKind::Member(member) = &callee_expr.kind else {
    return None;
  };
  if member.optional {
    return None;
  }
  let prop = static_object_key_name(lowered, body, &member.property)?;

  let callback = call.args.first()?.expr;
  match prop.as_str() {
    "map" => Some((
      member.object,
      ArrayCallNode::Op(ArrayChainOp::Map { callback }),
    )),
    "filter" => Some((
      member.object,
      ArrayCallNode::Op(ArrayChainOp::Filter { callback }),
    )),
    "flatMap" => Some((
      member.object,
      ArrayCallNode::Op(ArrayChainOp::FlatMap { callback }),
    )),
    "reduce" => Some((
      member.object,
      ArrayCallNode::Terminal(ArrayTerminal::Reduce {
        callback,
        init: call.args.get(1).map(|arg| arg.expr),
      }),
    )),
    "find" => Some((
      member.object,
      ArrayCallNode::Terminal(ArrayTerminal::Find { callback }),
    )),
    "every" => Some((
      member.object,
      ArrayCallNode::Terminal(ArrayTerminal::Every { callback }),
    )),
    "some" => Some((
      member.object,
      ArrayCallNode::Terminal(ArrayTerminal::Some { callback }),
    )),
    "forEach" => Some((
      member.object,
      ArrayCallNode::Terminal(ArrayTerminal::ForEach { callback }),
    )),
    _ => None,
  }
}

#[cfg(all(feature = "typed", feature = "hir-semantic-ops"))]
fn convert_hir_array_chain_ops(
  ops: &[hir_js::ArrayChainOp],
) -> Option<(Vec<ArrayChainOp>, Option<ArrayTerminal>)> {
  let mut out_ops = Vec::new();
  let mut terminal = None;
  for (idx, op) in ops.iter().enumerate() {
    match op {
      hir_js::ArrayChainOp::Map(callback) => {
        if terminal.is_some() {
          return None;
        }
        out_ops.push(ArrayChainOp::Map { callback: *callback });
      }
      hir_js::ArrayChainOp::Filter(callback) => {
        if terminal.is_some() {
          return None;
        }
        out_ops.push(ArrayChainOp::Filter { callback: *callback });
      }
      hir_js::ArrayChainOp::Reduce(callback, init) => {
        if idx != ops.len().saturating_sub(1) {
          return None;
        }
        terminal = Some(ArrayTerminal::Reduce {
          callback: *callback,
          init: *init,
        });
      }
      hir_js::ArrayChainOp::Find(callback) => {
        if idx != ops.len().saturating_sub(1) {
          return None;
        }
        terminal = Some(ArrayTerminal::Find { callback: *callback });
      }
      hir_js::ArrayChainOp::Every(callback) => {
        if idx != ops.len().saturating_sub(1) {
          return None;
        }
        terminal = Some(ArrayTerminal::Every { callback: *callback });
      }
      hir_js::ArrayChainOp::Some(callback) => {
        if idx != ops.len().saturating_sub(1) {
          return None;
        }
        terminal = Some(ArrayTerminal::Some { callback: *callback });
      }
    }
  }
  Some((out_ops, terminal))
}

#[cfg(feature = "typed")]
fn parse_array_chain(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &hir_js::Body,
  types: &impl crate::types::TypeProvider,
  expr_id: ExprId,
) -> Option<RecognizedPattern> {
  #[cfg(feature = "hir-semantic-ops")]
  {
    let expr = body.exprs.get(expr_id.0 as usize)?;
    if let ExprKind::ArrayChain { array, ops } = &expr.kind {
      let base = strip_transparent_wrappers(body, *array);
      if !types.expr_is_array(body_id, base) {
        return None;
      }
      let (ops, terminal) = convert_hir_array_chain_ops(ops)?;
      let ok_len = if terminal.is_some() { ops.len() >= 1 } else { ops.len() >= 2 };
      return ok_len.then_some(RecognizedPattern::ArrayChain { base, ops, terminal });
    }
  }

  let mut ops_rev = Vec::new();
  let mut terminal = None;

  let (mut recv, node) = classify_array_call(lowered, body, expr_id)?;
  match node {
    ArrayCallNode::Op(op) => ops_rev.push(op),
    ArrayCallNode::Terminal(term) => terminal = Some(term),
  }

  loop {
    recv = strip_transparent_wrappers(body, recv);
    let recv_expr = body.exprs.get(recv.0 as usize)?;
    match &recv_expr.kind {
      ExprKind::Call(_) => {
        let (next_recv, node) = classify_array_call(lowered, body, recv)?;
        match node {
          ArrayCallNode::Op(op) => ops_rev.push(op),
          // Terminal methods must be the final call in the chain.
          ArrayCallNode::Terminal(_) => return None,
        }
        recv = next_recv;
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, callback } => {
        ops_rev.push(ArrayChainOp::Map { callback: *callback });
        recv = *array;
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayFilter { array, callback } => {
        ops_rev.push(ArrayChainOp::Filter { callback: *callback });
        recv = *array;
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        let (prefix_ops, prefix_terminal) = convert_hir_array_chain_ops(ops)?;
        // Terminal methods must be the final call in the chain.
        if prefix_terminal.is_some() {
          return None;
        }
        for op in prefix_ops.into_iter().rev() {
          ops_rev.push(op);
        }
        recv = *array;
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayReduce { .. }
      | ExprKind::ArrayFind { .. }
      | ExprKind::ArrayEvery { .. }
      | ExprKind::ArraySome { .. } => {
        // Terminal methods must be the final call in the chain.
        return None;
      }
      _ => {
        let base = recv;
        if !types.expr_is_array(body_id, base) {
          return None;
        }

        ops_rev.reverse();

        let ok_len = if terminal.is_some() {
          ops_rev.len() >= 1
        } else {
          ops_rev.len() >= 2
        };
        if !ok_len {
          return None;
        }

        return Some(RecognizedPattern::ArrayChain { base, ops: ops_rev, terminal });
      }
    }
  }
}

#[cfg(feature = "typed")]
pub fn recognize_patterns_typed(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  types: &impl crate::types::TypeProvider,
) -> Vec<RecognizedPattern> {
  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let resolver = ApiCallResolver::new(kb, lowered);
  let array_map = kb.id_of("Array.prototype.map");
  let map_has = kb.id_of("Map.prototype.has");
  let map_get = kb.id_of("Map.prototype.get");
  let mut patterns = Vec::new();
  // `hir-js/semantic-ops` may lower a source expression into a semantic-op node
  // (e.g. `ExprKind::ArrayChain`) and leave the original `Call`/`Member` nodes in
  // the arena but unreachable from the root statements. Filter to the reachable
  // subgraph so we don't emit patterns for dead nodes.
  let reachable_exprs = collect_reachable_exprs(body_ref);

  for stmt_id in &body_ref.root_stmts {
    walk_stmt(body_ref, *stmt_id, |stmt_id, stmt| match stmt {
      StmtKind::ForIn {
        is_for_of: true,
        await_: true,
        left,
        right,
        body,
        ..
      } => {
        let (binding_pat, binding_kind) = match left {
          hir_js::ForHead::Pat(pat) => (*pat, None),
          hir_js::ForHead::Var(var) => {
            let Some(decl) = var.declarators.first() else {
              return;
            };
            (decl.pat, Some(var.kind))
          }
        };

        patterns.push(RecognizedPattern::AsyncIterator {
          stmt: stmt_id,
          iterable: *right,
          binding_pat,
          binding_kind,
          body: *body,
        });
      }
      StmtKind::Var(var) => {
        for decl in &var.declarators {
          let Some(source) = decl.init else {
            continue;
          };
          let pat_id = decl.pat;
          let Some(pat) = body_ref.pats.get(pat_id.0 as usize) else {
            continue;
          };
          let PatKind::Array(array) = &pat.kind else {
            continue;
          };
          if array.rest.is_some() {
            continue;
          }
          let binding_count = array.elements.iter().flatten().count();
          if binding_count == 0 {
            continue;
          }
          if array
            .elements
            .iter()
            .flatten()
            .any(|e| e.default_value.is_some())
          {
            continue;
          }
          patterns.push(RecognizedPattern::ArrayDestructure {
            stmt: stmt_id,
            pat: pat_id,
            arity: binding_count,
            source,
          });
        }
      }
      StmtKind::If {
        test,
        consequent,
        alternate: None,
      } => {
        let if_stmt_id = stmt_id;
        let Some(subject) = guard_clause_subject(lowered, body_ref, *test) else {
          return;
        };

        let mut arm = Some(*consequent);
        while let Some(consequent_id) = arm.take() {
          let Some(consequent_stmt) = body_ref.stmts.get(consequent_id.0 as usize) else {
            break;
          };
          match &consequent_stmt.kind {
            StmtKind::Return(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: subject,
                kind: GuardKind::Return,
              });
            }
            StmtKind::Throw(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: subject,
                kind: GuardKind::Throw,
              });
            }
            StmtKind::Block(stmts) if stmts.len() == 1 => {
              arm = stmts.first().copied();
              continue;
            }
            _ => {}
          }
          break;
        }
      }
      _ => {}
    });
  }

  // 1) Canonical call sites, using types to gate prototype/instance methods.
  for (idx, _expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if !reachable_exprs.contains(&expr_id) {
      continue;
    }
    if let Some(api) = resolver.resolve_call_typed(body, expr_id, types) {
      patterns.push(RecognizedPattern::CanonicalCall {
        call: expr_id,
        api,
      });
    }
  }

  // 2) Typed-only higher-level patterns.
  //
  // These are intentionally conservative: if typing is missing or the chain
  // includes unknown/any/union receivers, we do not emit the pattern.
  let mut non_outermost_array_exprs = std::collections::HashSet::<ExprId>::new();
  for (idx, _expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if !reachable_exprs.contains(&expr_id) {
      continue;
    }
    let Some((recv, _node)) = classify_array_call(lowered, body_ref, expr_id) else {
      continue;
    };
    let recv_kind = body_ref.exprs.get(recv.0 as usize).map(|e| &e.kind);
    let mark = matches!(recv_kind, Some(ExprKind::Call(_)))
      || {
        #[cfg(feature = "hir-semantic-ops")]
        {
          matches!(recv_kind, Some(ExprKind::ArrayChain { .. }))
        }
        #[cfg(not(feature = "hir-semantic-ops"))]
        {
          false
        }
      };
    if mark {
      non_outermost_array_exprs.insert(recv);
    }
  }

  #[cfg(feature = "hir-semantic-ops")]
  {
    // `hir-js` may lower chained array operations into nested `ExprKind::ArrayChain` nodes. The
    // outer chain node does not retain the intermediate receiver expression, so use spans to
    // suppress inner chains and keep `ArrayChain` recognition outermost-only.
    let mut max_end_by_start = std::collections::BTreeMap::<u32, u32>::new();
    for expr in &body_ref.exprs {
      if matches!(expr.kind, ExprKind::ArrayChain { .. }) {
        max_end_by_start
          .entry(expr.span.start)
          .and_modify(|end| *end = (*end).max(expr.span.end))
          .or_insert(expr.span.end);
      }
    }
    for (idx, expr) in body_ref.exprs.iter().enumerate() {
      if matches!(expr.kind, ExprKind::ArrayChain { .. }) {
        if let Some(max_end) = max_end_by_start.get(&expr.span.start) {
          if expr.span.end < *max_end {
            non_outermost_array_exprs.insert(ExprId(idx as u32));
          }
        }
      }
    }
  }

  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if !reachable_exprs.contains(&expr_id) {
      continue;
    }

    // ArrayChain: recognize only at the outermost chain call.
    if !non_outermost_array_exprs.contains(&expr_id) {
      match &expr.kind {
        ExprKind::Call(_) => {
          if let Some(pat) = parse_array_chain(lowered, body, body_ref, types, expr_id) {
            patterns.push(pat);
          }
        }
        #[cfg(feature = "hir-semantic-ops")]
        ExprKind::ArrayChain { .. } => {
          if let Some(pat) = parse_array_chain(lowered, body, body_ref, types, expr_id) {
            patterns.push(pat);
          }
        }
        _ => {}
      }
    }

    // MapFilterReduce: recognize only at the terminal `reduce(...)` call.
    if let Some((base, chain)) = call_chain(lowered, body, expr_id) {
      if chain.len() == 3
        && chain[2].1 == "reduce"
        && chain[0].1 == "map"
        && chain[1].1 == "filter"
        // Only require the *base* receiver to be a proven array. The checker may
        // leave intermediate call result types as `unknown`, but the chain is
        // still safe if it starts from a known array/readonly-array.
        && array_map.is_some_and(|id| resolver.resolve_call_typed(body, chain[0].0, types) == Some(id))
      {
        patterns.push(RecognizedPattern::MapFilterReduce {
          base,
          map_call: chain[0].0,
          filter_call: chain[1].0,
          reduce_call: chain[2].0,
        });
      }
    }

    #[cfg(feature = "hir-semantic-ops")]
    if let ExprKind::ArrayChain { array, ops } = &expr.kind {
      let base = strip_transparent_wrappers(body_ref, *array);
      if types.expr_is_array(body, base) {
        if let [hir_js::ArrayChainOp::Map(map_cb), hir_js::ArrayChainOp::Filter(filter_cb), hir_js::ArrayChainOp::Reduce(_reduce_cb, _reduce_init)] =
          ops.as_slice()
        {
          // Note: `hir-js` collapses chained calls into a single `ArrayChain` node,
          // but the intermediate call expressions still exist (typically unreachable)
          // in the expression arena. Best-effort recover them so downstream can
          // still point at the per-call nodes.
          let map_call = body_ref
            .exprs
            .iter()
            .enumerate()
            .find_map(|(idx, candidate)| match &candidate.kind {
              ExprKind::ArrayMap { array, callback }
                if strip_transparent_wrappers(body_ref, *array) == base && callback == map_cb =>
              {
                Some(ExprId(idx as u32))
              }
              _ => None,
            })
            .unwrap_or(expr_id);

          let filter_call = body_ref
            .exprs
            .iter()
            .enumerate()
            .find_map(|(idx, candidate)| match &candidate.kind {
              ExprKind::ArrayChain { array, ops }
                if strip_transparent_wrappers(body_ref, *array) == base
                  && matches!(ops.as_slice(), [hir_js::ArrayChainOp::Map(a), hir_js::ArrayChainOp::Filter(b)] if a == map_cb && b == filter_cb) =>
              {
                Some(ExprId(idx as u32))
              }
              _ => None,
            })
            .unwrap_or(expr_id);

          patterns.push(RecognizedPattern::MapFilterReduce {
            base,
            map_call,
            filter_call,
            reduce_call: expr_id,
          });
        }
      }
    }

    if let Some(m) = promise_all_fetch_match_typed(kb, lowered, body, expr_id, types) {
      patterns.push(RecognizedPattern::PromiseAllFetch {
        promise_all_call: expr_id,
        urls_expr: m.urls_expr,
        map_call: m.map_call,
        fetch_call_count: m.fetch_call_count,
      });
    }

    if let ExprKind::Template(template) = &expr.kind {
      if template.spans.len() >= 2 {
        patterns.push(RecognizedPattern::StringTemplate {
          expr: expr_id,
          span_count: template.spans.len(),
        });
      }
    }

    if let ExprKind::Object(obj) = &expr.kind {
      let mut spread_count = 0usize;
      let mut valid = true;
      for prop in &obj.properties {
        match prop {
          ObjectProperty::Spread(_) => spread_count += 1,
          ObjectProperty::KeyValue { key, .. } => {
            if matches!(key, hir_js::ObjectKey::Computed(_)) {
              valid = false;
              break;
            }
          }
          ObjectProperty::Getter { .. } | ObjectProperty::Setter { .. } => {
            valid = false;
            break;
          }
        }
      }
      if valid && spread_count > 0 {
        patterns.push(RecognizedPattern::ObjectSpread {
          expr: expr_id,
          spread_count,
        });
      }
    }

    // MapGetOrDefault: `map.has(k) ? map.get(k) : default`.
    if let ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } = &expr.kind
    {
      let test = strip_transparent_wrappers(body_ref, *test);
      let consequent = strip_transparent_wrappers(body_ref, *consequent);

      if map_has.is_some_and(|id| resolver.resolve_call_typed(body, test, types) == Some(id))
        && map_get.is_some_and(|id| resolver.resolve_call_typed(body, consequent, types) == Some(id))
      {
        if let Some((map, key)) = map_get_or_default_conditional(
          lowered,
          body_ref,
          body,
          test,
          consequent,
          types,
        ) {
          patterns.push(RecognizedPattern::MapGetOrDefault {
            conditional: expr_id,
            map,
            key,
            default: *alternate,
          });
        }
      }
    }

    // MapGetOrDefault: `map.get(k)! ?? default`.
    if let ExprKind::Binary { op, left, right } = &expr.kind {
      if !matches!(op, hir_js::BinaryOp::NullishCoalescing) {
        continue;
      }
      let Some(map_get) = map_get else {
        continue;
      };
      let left = strip_transparent_wrappers(body_ref, *left);
      if resolver.resolve_call_typed(body, left, types) != Some(map_get) {
        continue;
      }

      let Some(left_expr) = body_ref.exprs.get(left.0 as usize) else {
        continue;
      };
      let ExprKind::Call(call) = &left_expr.kind else {
        continue;
      };
      if call.optional || call.is_new || call.args.len() != 1 || call.args[0].spread {
        continue;
      }

      let Some(callee) = body_ref.exprs.get(call.callee.0 as usize) else {
        continue;
      };
      let ExprKind::Member(member) = &callee.kind else {
        continue;
      };
      if member.optional {
        continue;
      }

      patterns.push(RecognizedPattern::MapGetOrDefault {
        conditional: expr_id,
        map: member.object,
        key: call.args[0].expr,
        default: *right,
      });
    }
  }

  // 3) Annotation-driven patterns (same as untyped).
  patterns.extend(
    recognize_patterns_untyped(kb, lowered, body)
      .into_iter()
      .filter(|pat| matches!(pat, RecognizedPattern::JsonParseTyped { .. })),
  );

  sort_patterns_by_span(body_ref, &mut patterns);
  patterns
}

#[cfg(feature = "typed")]
fn expr_equivalent(lowered: &LowerResult, body: &hir_js::Body, a: ExprId, b: ExprId) -> bool {
  let a = strip_transparent_wrappers(body, a);
  let b = strip_transparent_wrappers(body, b);
  let Some(a_expr) = body.exprs.get(a.0 as usize) else {
    return false;
  };
  let Some(b_expr) = body.exprs.get(b.0 as usize) else {
    return false;
  };

  match (&a_expr.kind, &b_expr.kind) {
    (ExprKind::Ident(a), ExprKind::Ident(b)) => a == b,
    (ExprKind::Literal(a), ExprKind::Literal(b)) => a == b,
    (ExprKind::Member(a), ExprKind::Member(b)) => {
      if a.optional || b.optional {
        return false;
      }
      let Some(a_key) = static_object_key_name(lowered, body, &a.property) else {
        return false;
      };
      let Some(b_key) = static_object_key_name(lowered, body, &b.property) else {
        return false;
      };
      if a_key != b_key {
        return false;
      }
      expr_equivalent(lowered, body, a.object, b.object)
    }
    _ => false,
  }
}

#[cfg(feature = "typed")]
fn match_single_arg_member_call<'a>(
  lowered: &'a LowerResult,
  body: &hir_js::Body,
  call_expr: ExprId,
) -> Option<(ExprId, String, ExprId)> {
  let call_expr = strip_transparent_wrappers(body, call_expr);
  let call = body.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
    return None;
  };
  if call.optional || call.is_new || call.args.len() != 1 {
    return None;
  }
  let arg0 = call.args.first()?;
  if arg0.spread {
    return None;
  }

  let callee = strip_transparent_wrappers(body, call.callee);
  let callee = body.exprs.get(callee.0 as usize)?;
  let ExprKind::Member(member) = &callee.kind else {
    return None;
  };
  if member.optional {
    return None;
  }
  let prop = static_object_key_name(lowered, body, &member.property)?;

  Some((member.object, prop, arg0.expr))
}

#[cfg(feature = "typed")]
fn map_get_or_default_conditional(
  lowered: &LowerResult,
  body_ref: &hir_js::Body,
  body_id: BodyId,
  test: ExprId,
  consequent: ExprId,
  types: &impl crate::types::TypeProvider,
) -> Option<(ExprId, ExprId)> {
  let (has_recv, has_prop, has_key) = match_single_arg_member_call(lowered, body_ref, test)?;
  if has_prop != "has" {
    return None;
  }
  if !types.expr_is_named_ref(body_id, strip_transparent_wrappers(body_ref, has_recv), "Map") {
    return None;
  }

  let (get_recv, get_prop, get_key) = match_single_arg_member_call(lowered, body_ref, consequent)?;
  if get_prop != "get" {
    return None;
  }
  if !types.expr_is_named_ref(body_id, strip_transparent_wrappers(body_ref, get_recv), "Map") {
    return None;
  }

  if !expr_equivalent(lowered, body_ref, has_recv, get_recv)
    || !expr_equivalent(lowered, body_ref, has_key, get_key)
  {
    return None;
  }

  Some((
    strip_transparent_wrappers(body_ref, has_recv),
    strip_transparent_wrappers(body_ref, has_key),
  ))
}
