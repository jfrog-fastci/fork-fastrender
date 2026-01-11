use hir_js::{BodyId, ExprId, LowerResult, StmtId, StmtKind};

#[cfg(feature = "typed")]
use hir_js::{BinaryOp, ExprKind};

use crate::api::ApiId;
use crate::resolve::resolve_api_call_untyped;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizedPattern {
  /// A call site that could be resolved to a canonical API identifier.
  CanonicalCall { call: ExprId, api: ApiId },

  /// `arr.map(f).filter(g).reduce(h, init)` (typed only).
  MapFilterReduce {
    base: ExprId,
    map_call: ExprId,
    filter_call: ExprId,
    reduce_call: ExprId,
  },

  /// `map.get(key) ?? default` / `map.get(key) || default` (typed only).
  MapGetOrDefault {
    map: ExprId,
    key: ExprId,
    default: ExprId,
  },

  /// `const x: T = JSON.parse(input)` (untyped; uses declared annotation).
  JsonParseTyped { call: ExprId, target: hir_js::TypeExprId },
}

fn walk_stmt(body: &hir_js::Body, stmt_id: StmtId, mut f: impl FnMut(&StmtKind)) {
  fn walk(body: &hir_js::Body, stmt_id: StmtId, f: &mut impl FnMut(&StmtKind)) {
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
    walk_stmt(body_ref, *stmt_id, |stmt| {
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

  patterns
}

#[cfg(feature = "typed")]
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

#[cfg(feature = "typed")]
pub fn recognize_patterns_typed(
  lowered: &LowerResult,
  body: BodyId,
  types: &impl crate::typed::TypeProvider,
) -> Vec<RecognizedPattern> {
  use crate::resolve::resolve_api_call_typed;

  let Some(body_ref) = lowered.body(body) else {
    return Vec::new();
  };

  let mut patterns = Vec::new();

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
  for (idx, expr) in body_ref.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);

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
        continue;
      }
    }

    // MapGetOrDefault.
    if let ExprKind::Binary { op, left, right } = &expr.kind {
      if !matches!(op, BinaryOp::NullishCoalescing | BinaryOp::LogicalOr) {
        continue;
      }
      if resolve_api_call_typed(lowered, body, *left, types) != Some(ApiId::MapPrototypeGet) {
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

  patterns
}
