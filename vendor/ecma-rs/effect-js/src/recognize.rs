use hir_js::{
  ArrayElement, BodyId, ExprId, ExprKind, LowerResult, ObjectProperty, PatId, PatKind,
  StmtId, StmtKind, UnaryOp, VarDeclKind,
};

#[cfg(feature = "typed")]
use hir_js::BinaryOp;

use crate::api::ApiId;
use crate::resolve::{resolve_api_call_best_effort_untyped, resolve_api_call_untyped};

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
  Ident(hir_js::NameId),
  String(String),
  Number(String),
}

fn expr_fingerprint(lowered: &LowerResult, body: BodyId, expr: ExprId) -> Option<ExprFingerprint> {
  let body_ref = lowered.body(body)?;
  let expr = body_ref.exprs.get(expr.0 as usize)?;
  match &expr.kind {
    ExprKind::TypeAssertion { expr, .. }
    | ExprKind::NonNull { expr }
    | ExprKind::Satisfies { expr, .. } => expr_fingerprint(lowered, body, *expr),
    ExprKind::Ident(name) => Some(ExprFingerprint::Ident(*name)),
    ExprKind::This => Some(ExprFingerprint::This),
    ExprKind::Member(member) => {
      if member.optional {
        return None;
      }
      let obj = expr_fingerprint(lowered, body, member.object)?;
      let key = match &member.property {
        hir_js::ObjectKey::Ident(id) => MemberKey::Ident(*id),
        hir_js::ObjectKey::String(s) => MemberKey::String(s.clone()),
        hir_js::ObjectKey::Number(n) => MemberKey::Number(n.clone()),
        hir_js::ObjectKey::Computed(_) => return None,
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
) -> Option<(ExprId, hir_js::NameId, ExprId)> {
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
  let hir_js::ObjectKey::Ident(prop) = &member.property else {
    return None;
  };

  Some((member.object, *prop, arg0.expr))
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

  /// Map get-with-default idioms (typed only).
  ///
  /// Matches:
  /// - `map.get(key) ?? default`
  /// - `map.has(key) ? map.get(key) : default` (including `map.get(key)!`)
  MapGetOrDefault {
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
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
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

pub fn recognize_patterns_untyped(lowered: &LowerResult, body: BodyId) -> Vec<RecognizedPattern> {
  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let mut patterns = Vec::new();

  // 1) Canonical call sites that are safe to resolve from HIR alone (e.g. JSON.parse).
  for (idx, _expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if let Some(api) = resolve_api_call_untyped(lowered, body, expr_id) {
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
        if resolve_api_call_untyped(lowered, body, init) == Some(ApiId::JsonParse) {
          patterns.push(RecognizedPattern::JsonParseTyped { call: init, target });
        }
      }
    });
  }

  sort_patterns_by_span(body_ref, &mut patterns);
  patterns
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
    let hir_js::ObjectKey::Ident(prop) = member.property else {
      return None;
    };
    let prop = lowered.names.resolve(prop)?.to_string();
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
  lowered: &LowerResult,
  body: BodyId,
) -> Vec<RecognizedPattern> {
  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let mut patterns = recognize_patterns_untyped(lowered, body);

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
          if array.elements.iter().any(|e| e.is_none()) {
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
            arity: array.elements.len(),
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
        let Some(test_expr) = body_ref.exprs.get(test.0 as usize) else {
          return;
        };
        let ExprKind::Unary {
          op: UnaryOp::Not,
          expr,
        } = &test_expr.kind
        else {
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
                test: *expr,
                kind: GuardKind::Return,
              });
            }
            StmtKind::Throw(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: *expr,
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

    if let Some(m) = promise_all_fetch_match_untyped(lowered, body, expr_id) {
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
      let spread_count = obj
        .properties
        .iter()
        .filter(|p| matches!(p, ObjectProperty::Spread(_)))
        .count();
      if spread_count > 0 {
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
      if lowered.names.resolve(has_prop) != Some("has") {
        continue;
      }

      let Some((get_map, get_prop, get_key)) =
        parse_simple_method_call_untyped(lowered, body, *consequent)
      else {
        continue;
      };
      if lowered.names.resolve(get_prop) != Some("get") {
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
        map: has_map,
        key: has_key,
        default: *alternate,
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
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<PromiseAllFetchMatch> {
  let body_ref = lowered.body(body)?;
  let call = body_ref.exprs.get(call_expr.0 as usize)?;
  #[cfg(feature = "hir-semantic-ops")]
  if let ExprKind::PromiseAll { promises } = &call.kind {
    let fetch_call_count = promises
      .iter()
      .filter(|expr_id| resolve_api_call_untyped(lowered, body, **expr_id) == Some(ApiId::Fetch))
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

  if resolve_api_call_untyped(lowered, body, call_expr) != Some(ApiId::PromiseAll) {
    return None;
  }

  let ExprKind::Call(call) = &call.kind else {
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
            if resolve_api_call_untyped(lowered, body, expr_id) == Some(ApiId::Fetch) {
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
      if resolve_api_call_best_effort_untyped(lowered, body, arg_expr_id)
        != Some(ApiId::ArrayPrototypeMap)
      {
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
            if resolve_api_call_untyped(lowered, cb_body_id, ret_expr) == Some(ApiId::Fetch) {
              1
            } else if let ExprKind::Await { expr } = &cb_body.exprs.get(ret_expr.0 as usize)?.kind {
              let expr = strip_transparent_wrappers(cb_body, *expr);
              if resolve_api_call_untyped(lowered, cb_body_id, expr) == Some(ApiId::Fetch) {
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
            if resolve_api_call_untyped(lowered, cb_body_id, ret_expr) == Some(ApiId::Fetch) {
              1
            } else if let ExprKind::Await { expr } = &cb_body.exprs.get(ret_expr.0 as usize)?.kind {
              let expr = strip_transparent_wrappers(cb_body, *expr);
              if resolve_api_call_untyped(lowered, cb_body_id, expr) == Some(ApiId::Fetch) {
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
  let hir_js::ObjectKey::Ident(prop) = member.property else {
    return None;
  };
  let prop = lowered.names.resolve(prop)?;

  let callback = call.args.first()?.expr;
  match prop {
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

#[cfg(feature = "typed")]
fn parse_array_chain(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &hir_js::Body,
  types: &impl crate::types::TypeProvider,
  call_expr: ExprId,
) -> Option<RecognizedPattern> {
  let mut ops_rev = Vec::new();
  let mut terminal = None;

  let (mut recv, node) = classify_array_call(lowered, body, call_expr)?;
  match node {
    ArrayCallNode::Op(op) => ops_rev.push(op),
    ArrayCallNode::Terminal(term) => terminal = Some(term),
  }

  loop {
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
  lowered: &LowerResult,
  body: BodyId,
  types: &impl crate::types::TypeProvider,
) -> Vec<RecognizedPattern> {
  use crate::resolve::resolve_api_call_typed;

  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let mut patterns = Vec::new();

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
          if array.elements.iter().any(|e| e.is_none()) {
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
            arity: array.elements.len(),
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
        let Some(test_expr) = body_ref.exprs.get(test.0 as usize) else {
          return;
        };
        let ExprKind::Unary {
          op: UnaryOp::Not,
          expr,
        } = &test_expr.kind
        else {
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
                test: *expr,
                kind: GuardKind::Return,
              });
            }
            StmtKind::Throw(_) => {
              patterns.push(RecognizedPattern::GuardClause {
                stmt: if_stmt_id,
                test: *expr,
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
  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    if !matches!(expr.kind, ExprKind::Call(_)) {
      continue;
    }
    if let Some(api) = resolve_api_call_typed(lowered, body, expr_id, types) {
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
  let mut non_outermost_array_calls = std::collections::HashSet::<ExprId>::new();
  for (idx, _expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    let Some((recv, _node)) = classify_array_call(lowered, body_ref, expr_id) else {
      continue;
    };
    if matches!(
      body_ref.exprs.get(recv.0 as usize).map(|e| &e.kind),
      Some(ExprKind::Call(_))
    ) {
      non_outermost_array_calls.insert(recv);
    }
  }

  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);

    // ArrayChain: recognize only at the outermost chain call.
    if matches!(expr.kind, ExprKind::Call(_)) && !non_outermost_array_calls.contains(&expr_id) {
      if let Some(pat) = parse_array_chain(lowered, body, body_ref, types, expr_id) {
        patterns.push(pat);
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
        && resolve_api_call_typed(lowered, body, chain[0].0, types) == Some(ApiId::ArrayPrototypeMap)
      {
        patterns.push(RecognizedPattern::MapFilterReduce {
          base,
          map_call: chain[0].0,
          filter_call: chain[1].0,
          reduce_call: chain[2].0,
        });
      }
    }

    if let Some(m) = promise_all_fetch_match_typed(lowered, body, expr_id, types) {
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
      let spread_count = obj
        .properties
        .iter()
        .filter(|p| matches!(p, ObjectProperty::Spread(_)))
        .count();
      if spread_count > 0 {
        patterns.push(RecognizedPattern::ObjectSpread {
          expr: expr_id,
          spread_count,
        });
      }
    }

    // MapGetOrDefault.
    if let ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } = &expr.kind
    {
      if let Some((map, key)) = map_get_or_default_conditional(lowered, body_ref, body, *test, *consequent, types) {
        patterns.push(RecognizedPattern::MapGetOrDefault {
          map,
          key,
          default: *alternate,
        });
      }
    }

    if let ExprKind::Binary { op, left, right } = &expr.kind {
      if !matches!(op, BinaryOp::NullishCoalescing | BinaryOp::LogicalOr) {
        continue;
      }
      let left = strip_transparent_wrappers(body_ref, *left);
      if resolve_api_call_typed(lowered, body, left, types) != Some(ApiId::MapPrototypeGet) {
        continue;
      }
      let Some(call) = body_ref.exprs.get(left.0 as usize) else {
        continue;
      };
      let ExprKind::Call(call) = &call.kind else {
        continue;
      };
      let Some(callee) = body_ref.exprs.get(call.callee.0 as usize) else {
        continue;
      };
      let ExprKind::Member(member) = &callee.kind else {
        continue;
      };
      let Some(arg0) = call.args.first() else {
        continue;
      };
      if arg0.spread {
        continue;
      }
      patterns.push(RecognizedPattern::MapGetOrDefault {
        map: member.object,
        key: arg0.expr,
        default: *right,
      });
    }
  }

  // 3) Annotation-driven patterns (same as untyped).
  patterns.extend(recognize_patterns_untyped(lowered, body).into_iter().filter(|pat| {
    matches!(pat, RecognizedPattern::JsonParseTyped { .. })
  }));

  sort_patterns_by_span(body_ref, &mut patterns);
  patterns
}

#[cfg(feature = "typed")]
fn expr_equivalent(body: &hir_js::Body, a: ExprId, b: ExprId) -> bool {
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
      match (&a.property, &b.property) {
        (hir_js::ObjectKey::Ident(a), hir_js::ObjectKey::Ident(b)) if a == b => {}
        (hir_js::ObjectKey::String(a), hir_js::ObjectKey::String(b)) if a == b => {}
        (hir_js::ObjectKey::Number(a), hir_js::ObjectKey::Number(b)) if a == b => {}
        _ => return false,
      }
      expr_equivalent(body, a.object, b.object)
    }
    _ => false,
  }
}

#[cfg(feature = "typed")]
fn match_single_arg_member_call<'a>(
  lowered: &'a LowerResult,
  body: &hir_js::Body,
  call_expr: ExprId,
) -> Option<(ExprId, &'a str, ExprId)> {
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
  let hir_js::ObjectKey::Ident(prop) = member.property else {
    return None;
  };
  let prop = lowered.names.resolve(prop)?;

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

  if !expr_equivalent(body_ref, has_recv, get_recv) || !expr_equivalent(body_ref, has_key, get_key) {
    return None;
  }

  Some((get_recv, get_key))
}

#[cfg(feature = "typed")]
fn promise_all_fetch_match_typed(
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
  types: &impl crate::types::TypeProvider,
) -> Option<PromiseAllFetchMatch> {
  use crate::resolve::resolve_api_call_typed;

  let body_ref = lowered.body(body)?;
  if resolve_api_call_typed(lowered, body, call_expr, types) != Some(ApiId::PromiseAll) {
    return None;
  }

  let call = body_ref.exprs.get(call_expr.0 as usize)?;
  let ExprKind::Call(call) = &call.kind else {
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
            if resolve_api_call_untyped(lowered, body, expr_id) == Some(ApiId::Fetch) {
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
      if resolve_api_call_typed(lowered, body, arg_expr_id, types) != Some(ApiId::ArrayPrototypeMap) {
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
          if resolve_api_call_untyped(lowered, cb_body_id, ret_expr) == Some(ApiId::Fetch) {
            return Some(PromiseAllFetchMatch {
              urls_expr: strip_transparent_wrappers(body_ref, member.object),
              map_call: Some(arg_expr_id),
              fetch_call_count: 1,
            });
          }

          if let ExprKind::Await { expr } = &cb_body.exprs.get(ret_expr.0 as usize)?.kind {
            let expr = strip_transparent_wrappers(cb_body, *expr);
            if resolve_api_call_untyped(lowered, cb_body_id, expr) == Some(ApiId::Fetch) {
              return Some(PromiseAllFetchMatch {
                urls_expr: strip_transparent_wrappers(body_ref, member.object),
                map_call: Some(arg_expr_id),
                fetch_call_count: 1,
              });
            }
          }

          None
        }
        _ => None,
      }
    }
    _ => None,
  }
}
