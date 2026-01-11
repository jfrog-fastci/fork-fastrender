use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult};
use knowledge_base::ApiDatabase;

use crate::api::ApiId;
use crate::resolve::resolve_call;
use crate::types::{TypeId, TypeProvider};

#[cfg(feature = "typed")]
use hir_js::{FunctionBody, StmtId, StmtKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayOp {
  Map { callback: ExprId },
  Filter { callback: ExprId },
  Reduce { callback: ExprId, init: ExprId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizedPattern {
  /// `arr.map(f).filter(g).reduce(h, init)` (typed only).
  MapFilterReduce { array: ExprId, ops: Vec<ArrayOp> },
  /// `Promise.all(urls.map(url => fetch(url)))` (typed only).
  PromiseAllFetch { urls: ExprId },
  /// `const x: T = JSON.parse(input)` (typed only; uses inferred `TypeId`).
  TypedJsonParse { input: ExprId, target: TypeId },
}

/// Stable identifier for a recognized pattern within a single [`PatternTables`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecognizedPatternId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternTables {
  /// Per-expression resolved canonical API ID for call expressions, indexed by `ExprId`.
  pub resolved_call: Vec<Option<ApiId>>,

  /// Per-expression list of recognized patterns rooted at the expression, indexed by `ExprId`.
  pub patterns: Vec<Vec<RecognizedPatternId>>,

  /// Flat list of recognized patterns for this body.
  pub recognized: Vec<RecognizedPattern>,
}

pub fn recognize_patterns(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &Body,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> Vec<RecognizedPattern> {
  recognize_pattern_tables(lowered, body_id, body, db, types).recognized
}

pub fn recognize_pattern_tables(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &Body,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> PatternTables {
  // Build an ExprId-aligned table of resolved calls.
  let mut resolved_call: Vec<Option<ApiId>> = vec![None; body.exprs.len()];
  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    resolved_call[idx] = match &expr.kind {
      ExprKind::Call(_) => resolve_call(lowered, body_id, body, expr_id, db, types).map(|c| c.api_id),
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { .. }
      | ExprKind::ArrayMap { .. }
      | ExprKind::ArrayFilter { .. }
      | ExprKind::ArrayReduce { .. } => resolve_call(lowered, body_id, body, expr_id, db, types).map(|c| c.api_id),
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { ops, .. } => match ops.last() {
        Some(hir_js::ArrayChainOp::Map(_)) => Some(ApiId::from_name("Array.prototype.map")),
        Some(hir_js::ArrayChainOp::Filter(_)) => Some(ApiId::from_name("Array.prototype.filter")),
        Some(hir_js::ArrayChainOp::Reduce(..)) => Some(ApiId::from_name("Array.prototype.reduce")),
        _ => None,
      },
      _ => None,
    };
  }

  let typed_json_parse = collect_typed_json_parse_targets(body_id, body, &resolved_call, types);

  let mut patterns: Vec<Vec<RecognizedPatternId>> = vec![Vec::new(); body.exprs.len()];
  let mut recognized: Vec<RecognizedPattern> = Vec::new();
  let array_reduce = ApiId::from_name("Array.prototype.reduce");
  let promise_all = ApiId::from_name("Promise.all");

  for expr_idx in 0..body.exprs.len() {
    let expr_id = ExprId(expr_idx as u32);

    // TypedJsonParse.
    if let Some((input, target)) = typed_json_parse.get(expr_idx).copied().flatten() {
      let pat_id = RecognizedPatternId(recognized.len() as u32);
      recognized.push(RecognizedPattern::TypedJsonParse { input, target });
      patterns[expr_idx].push(pat_id);
    }

    // MapFilterReduce.
    if resolved_call[expr_idx] == Some(array_reduce) {
      if let Some(pattern_rec) =
        recognize_map_filter_reduce(body_id, body, expr_id, &resolved_call, types)
      {
        let pat_id = RecognizedPatternId(recognized.len() as u32);
        recognized.push(pattern_rec);
        patterns[expr_idx].push(pat_id);
      }
    }

    // PromiseAllFetch.
    if resolved_call[expr_idx] == Some(promise_all) {
      if let Some(pattern_rec) = recognize_promise_all_fetch(
        lowered,
        body_id,
        body,
        expr_id,
        db,
        &resolved_call,
        types,
      ) {
        let pat_id = RecognizedPatternId(recognized.len() as u32);
        recognized.push(pattern_rec);
        patterns[expr_idx].push(pat_id);
      }
    }
  }

  PatternTables {
    resolved_call,
    patterns,
    recognized,
  }
}

#[cfg(feature = "typed")]
#[cfg(feature = "typed")]
fn walk_stmt(body: &Body, stmt_id: StmtId, mut f: impl FnMut(&StmtKind)) {
  fn walk(body: &Body, stmt_id: StmtId, f: &mut impl FnMut(&StmtKind)) {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      return;
    };
    f(&stmt.kind);
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

#[cfg(feature = "typed")]
fn collect_typed_json_parse_targets(
  body_id: BodyId,
  body: &Body,
  resolved_call: &[Option<ApiId>],
  types: Option<&dyn TypeProvider>,
) -> Vec<Option<(ExprId, TypeId)>> {
  let Some(types) = types else {
    return vec![None; body.exprs.len()];
  };
  let json_parse = ApiId::from_name("JSON.parse");

  let mut results: Vec<Option<(ExprId, TypeId)>> = vec![None; body.exprs.len()];

  for stmt_id in body.root_stmts.iter().copied() {
    walk_stmt(body, stmt_id, |stmt| {
      let StmtKind::Var(var) = stmt else {
        return;
      };
      for decl in var.declarators.iter() {
        if decl.type_annotation.is_none() {
          continue;
        }
        let Some(init) = decl.init else {
          continue;
        };

        if resolved_call.get(init.0 as usize).copied().flatten() != Some(json_parse) {
          continue;
        }

        let ExprKind::Call(call) = &body.exprs[init.0 as usize].kind else {
          continue;
        };
        if call.optional || call.is_new || call.args.len() != 1 {
          continue;
        }
        let arg0 = &call.args[0];
        if arg0.spread {
          continue;
        }
        let input = arg0.expr;

        let Some(target) = types
          .pat_type(body_id, decl.pat)
          .or_else(|| types.expr_type(body_id, init))
        else {
          continue;
        };

        use crate::types::TypeKindSummary;
        match types.type_kind(target) {
          Some(TypeKindSummary::Unknown | TypeKindSummary::Any) | None => continue,
          _ => {}
        }

        results[init.0 as usize] = Some((input, target));
      }
    });
  }

  results
}

#[cfg(not(feature = "typed"))]
fn collect_typed_json_parse_targets(
  _body_id: BodyId,
  body: &Body,
  _resolved_call: &[Option<ApiId>],
  _types: Option<&dyn TypeProvider>,
) -> Vec<Option<(ExprId, TypeId)>> {
  vec![None; body.exprs.len()]
}

#[cfg(feature = "typed")]
fn recognize_map_filter_reduce(
  body_id: BodyId,
  body: &Body,
  reduce_call: ExprId,
  resolved_call: &[Option<ApiId>],
  types: Option<&dyn TypeProvider>,
) -> Option<RecognizedPattern> {
  let Some(types) = types else {
    return None;
  };

  let reduce_expr = body.exprs.get(reduce_call.0 as usize)?;

  // `hir-js` semantic-ops lowering can collapse array pipelines into a single
  // `ArrayChain` node.
  #[cfg(feature = "hir-semantic-ops")]
  if let ExprKind::ArrayChain { array, ops } = &reduce_expr.kind {
    let mut out = Vec::new();
    for op in ops {
      match op {
        hir_js::ArrayChainOp::Map(callback) => out.push(ArrayOp::Map {
          callback: *callback,
        }),
        hir_js::ArrayChainOp::Filter(callback) => out.push(ArrayOp::Filter {
          callback: *callback,
        }),
        hir_js::ArrayChainOp::Reduce(callback, Some(init)) => out.push(ArrayOp::Reduce {
          callback: *callback,
          init: *init,
        }),
        // This pattern requires an explicit init argument so we can expose it in
        // `ArrayOp::Reduce`.
        hir_js::ArrayChainOp::Reduce(_, None) => return None,
        _ => return None,
      }
    }

    // Require at least one map/filter op in addition to reduce.
    if out.len() < 2 || !matches!(out.last(), Some(ArrayOp::Reduce { .. })) {
      return None;
    }

    if !types.expr_is_array(body_id, *array) {
      return None;
    }

    return Some(RecognizedPattern::MapFilterReduce {
      array: *array,
      ops: out,
    });
  }

  let ExprKind::Call(reduce) = &reduce_expr.kind else {
    return None;
  };
  if reduce.optional || reduce.is_new || reduce.args.len() != 2 {
    return None;
  }
  if reduce.args[0].spread || reduce.args[1].spread {
    return None;
  }
  let reduce_callback = reduce.args[0].expr;
  let reduce_init = reduce.args[1].expr;

  let ExprKind::Member(reduce_callee) = &body.exprs.get(reduce.callee.0 as usize)?.kind else {
    return None;
  };
  let mut cur = reduce_callee.object;

  let mut ops_rev = vec![ArrayOp::Reduce {
    callback: reduce_callback,
    init: reduce_init,
  }];
  let array_map = ApiId::from_name("Array.prototype.map");
  let array_filter = ApiId::from_name("Array.prototype.filter");

  loop {
    let ExprKind::Call(call) = &body.exprs.get(cur.0 as usize)?.kind else {
      break;
    };

    if call.optional || call.is_new || call.args.len() != 1 {
      return None;
    }
    if call.args[0].spread {
      return None;
    }
    let callback = call.args[0].expr;

    let api = resolved_call.get(cur.0 as usize).copied().flatten();
    let op = match api {
      Some(id) if id == array_map => ArrayOp::Map { callback },
      Some(id) if id == array_filter => ArrayOp::Filter { callback },
      _ => break,
    };

    let ExprKind::Member(callee) = &body.exprs.get(call.callee.0 as usize)?.kind else {
      return None;
    };

    ops_rev.push(op);
    cur = callee.object;
  }

  // Require at least one map/filter op in addition to reduce.
  if ops_rev.len() < 2 {
    return None;
  }

  // Type-gate the base array expression.
  if !types.expr_is_array(body_id, cur) {
    return None;
  }

  ops_rev.reverse();
  Some(RecognizedPattern::MapFilterReduce {
    array: cur,
    ops: ops_rev,
  })
}

#[cfg(not(feature = "typed"))]
fn recognize_map_filter_reduce(
  _body_id: BodyId,
  _body: &Body,
  _reduce_call: ExprId,
  _resolved_call: &[Option<ApiId>],
  _types: Option<&dyn TypeProvider>,
) -> Option<RecognizedPattern> {
  None
}

#[cfg(feature = "typed")]
fn recognize_promise_all_fetch(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &Body,
  promise_all_call: ExprId,
  db: &ApiDatabase,
  resolved_call: &[Option<ApiId>],
  types: Option<&dyn TypeProvider>,
) -> Option<RecognizedPattern> {
  let Some(types) = types else {
    return None;
  };
  let array_map = ApiId::from_name("Array.prototype.map");
  let fetch = ApiId::from_name("fetch");

  let ExprKind::Call(promise_all) = &body.exprs.get(promise_all_call.0 as usize)?.kind else {
    return None;
  };
  if promise_all.optional || promise_all.is_new || promise_all.args.len() != 1 {
    return None;
  }
  if promise_all.args[0].spread {
    return None;
  }
  let promises_expr = promise_all.args[0].expr;

  // Promise.all(arg0) where arg0 is `urls.map(...)`.
  if resolved_call.get(promises_expr.0 as usize).copied().flatten() != Some(array_map) {
    return None;
  }
  let (urls, callback_expr) = match &body.exprs.get(promises_expr.0 as usize)?.kind {
    ExprKind::Call(map) => {
      if map.optional || map.is_new || map.args.len() != 1 {
        return None;
      }
      if map.args[0].spread {
        return None;
      }
      let callback_expr = map.args[0].expr;
      let ExprKind::Member(map_callee) = &body.exprs.get(map.callee.0 as usize)?.kind else {
        return None;
      };
      (map_callee.object, callback_expr)
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { array, callback } => (*array, *callback),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayChain { array, ops } => match ops.last() {
      Some(hir_js::ArrayChainOp::Map(callback)) => (*array, *callback),
      _ => return None,
    },
    _ => return None,
  };
  if !types.expr_is_array(body_id, urls) {
    return None;
  }

  let ExprKind::FunctionExpr {
    body: callback_body,
    is_arrow: true,
    ..
  } = &body.exprs.get(callback_expr.0 as usize)?.kind
  else {
    return None;
  };

  let cb_body = lowered.body(*callback_body)?;
  let cb_fn = cb_body.function.as_ref()?;
  let FunctionBody::Expr(fetch_call_expr) = cb_fn.body else {
    return None;
  };

  // The arrow function expression body must be a strict `fetch(url)` call.
  if resolve_call(lowered, *callback_body, cb_body, fetch_call_expr, db, Some(types))
    .map(|c| c.api_id)
    != Some(fetch)
  {
    return None;
  }
  let ExprKind::Call(fetch_call) = &cb_body.exprs.get(fetch_call_expr.0 as usize)?.kind else {
    return None;
  };
  if fetch_call.optional || fetch_call.is_new || fetch_call.args.len() != 1 {
    return None;
  }
  if fetch_call.args[0].spread {
    return None;
  }

  Some(RecognizedPattern::PromiseAllFetch { urls })
}

#[cfg(not(feature = "typed"))]
fn recognize_promise_all_fetch(
  _lowered: &LowerResult,
  _body_id: BodyId,
  _body: &Body,
  _promise_all_call: ExprId,
  _db: &ApiDatabase,
  _resolved_call: &[Option<ApiId>],
  _types: Option<&dyn TypeProvider>,
) -> Option<RecognizedPattern> {
  None
}
