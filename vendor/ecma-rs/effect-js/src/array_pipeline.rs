use effect_model::{EffectSet, Purity};
use hir_js::{BodyId, ExprId, ExprKind, FunctionBody, PatKind};
use hir_js::{BinaryOp, StmtId, StmtKind, TypeArenas, TypeExprId, TypeExprKind};
use knowledge_base::{ApiId, KnowledgeBase};

use crate::properties::OutputLengthRelation;
use crate::{properties, CallSiteInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayStageKind {
  Map,
  Filter,
  FlatMap,
  Reduce,
  Find,
  Every,
  Some,
  ForEach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrayStageMeta {
  /// Whether this stage can be fused with the immediately following stage.
  pub fusable_with_next: bool,
  pub output_len: OutputLengthRelation,
  pub parallelizable: bool,
  pub short_circuit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayStage {
  pub kind: ArrayStageKind,
  pub api: ApiId,
  /// Expression id for the callback argument.
  pub callback: ExprId,
  pub callsite: CallSiteInfo,
  pub meta: ArrayStageMeta,
}

impl ArrayStage {
  pub fn callback_span_range(&self, lowered: &hir_js::LowerResult, body: BodyId) -> Option<(u32, u32)> {
    lowered
      .body(body)
      .and_then(|b| b.exprs.get(self.callback.0 as usize))
      .map(|e| (e.span.start, e.span.end))
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayPipelinePlan {
  pub base: ExprId,
  pub stages: Vec<ArrayStage>,
}

impl ArrayPipelinePlan {
  pub fn callback_span_ranges(
    &self,
    lowered: &hir_js::LowerResult,
    body: BodyId,
  ) -> Vec<Option<(u32, u32)>> {
    self
      .stages
      .iter()
      .map(|stage| stage.callback_span_range(lowered, body))
      .collect()
  }
}

fn api_id_for_kind(kind: ArrayStageKind) -> ApiId {
  match kind {
    ArrayStageKind::Map => ApiId::from_name("Array.prototype.map"),
    ArrayStageKind::Filter => ApiId::from_name("Array.prototype.filter"),
    ArrayStageKind::FlatMap => ApiId::from_name("Array.prototype.flatMap"),
    ArrayStageKind::Reduce => ApiId::from_name("Array.prototype.reduce"),
    ArrayStageKind::Find => ApiId::from_name("Array.prototype.find"),
    ArrayStageKind::Every => ApiId::from_name("Array.prototype.every"),
    ArrayStageKind::Some => ApiId::from_name("Array.prototype.some"),
    ArrayStageKind::ForEach => ApiId::from_name("Array.prototype.forEach"),
  }
}

fn is_short_circuit(kind: ArrayStageKind) -> bool {
  matches!(
    kind,
    ArrayStageKind::Find | ArrayStageKind::Every | ArrayStageKind::Some
  )
}

fn callsite_info_for_callback(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  kind: ArrayStageKind,
  callback_expr: ExprId,
  kb: &KnowledgeBase,
) -> CallSiteInfo {
  let analysis_expr = resolve_callback_for_analysis(lowered, body, callback_expr);

  let callback = crate::analyze_inline_callback(lowered, body, analysis_expr, kb);
  let callback_purity = callback.map(|cb| cb.purity);
  let callback_effects = callback.map(|cb| cb.effects);
  let callback_may_throw = callback_effects
    .map(|e| e.contains(EffectSet::MAY_THROW) || e.contains(EffectSet::UNKNOWN_CALL));
  let associative = callback.and_then(|_| infer_associative_inline_callback(lowered, body, analysis_expr));

  let mut callback_uses_index = callback.map(|cb| cb.uses_index);
  let mut callback_uses_array = callback.map(|cb| cb.uses_array);
  if matches!(kind, ArrayStageKind::Reduce) {
    if let Some(cb) = callback {
      if let Some((idx, arr)) = remap_reduce_index_array_usage(lowered, body, analysis_expr, cb) {
        callback_uses_index = Some(idx);
        callback_uses_array = Some(arr);
      }
    }
  }

  CallSiteInfo {
    callback_purity,
    callback_effects,
    callback_may_throw,
    callback_is_pure: callback_purity.map(|p| matches!(p, Purity::Pure | Purity::Allocating)),
    callback_uses_index,
    callback_uses_array,
    callback_is_associative: associative,
    ..Default::default()
  }
}

fn remap_reduce_index_array_usage(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
  callback: crate::CallbackInfo,
) -> Option<(bool, bool)> {
  let callsite_body = lowered.body(body)?;
  let cb_expr = callsite_body.exprs.get(callback_expr.0 as usize)?;
  let ExprKind::FunctionExpr {
    body: cb_body,
    is_arrow,
    ..
  } = &cb_expr.kind
  else {
    return None;
  };
  if !*is_arrow {
    return None;
  }

  let cb_body_data = lowered.body(*cb_body)?;
  let func = cb_body_data.function.as_ref()?;
  if func.params.iter().any(|p| p.rest) {
    return None;
  }

  if func.params.len() <= 2 {
    return Some((false, false));
  }

  let uses_index = callback.uses_array;
  let uses_array = func.params.len() > 3;
  Some((uses_index, uses_array))
}

fn resolve_callback_for_analysis(lowered: &hir_js::LowerResult, body: BodyId, callback_expr: ExprId) -> ExprId {
  let Some(body_ref) = lowered.body(body) else {
    return callback_expr;
  };

  let callback_expr = strip_value_wrappers(body_ref, callback_expr);
  let Some(expr) = body_ref.exprs.get(callback_expr.0 as usize) else {
    return callback_expr;
  };

  // If the callback is an identifier, try to find a single in-body variable
  // declarator that binds it to a function expression (e.g. `const f = (...) => ...`).
  let ExprKind::Ident(name) = expr.kind else {
    return callback_expr;
  };

  let mut resolved: Option<ExprId> = None;
  for stmt in &body_ref.stmts {
    let StmtKind::Var(var) = &stmt.kind else {
      continue;
    };
    for decl in &var.declarators {
      let Some(pat) = body_ref.pats.get(decl.pat.0 as usize) else {
        continue;
      };
      let PatKind::Ident(pat_name) = pat.kind else {
        continue;
      };
      if pat_name != name {
        continue;
      }
      let Some(init) = decl.init else {
        continue;
      };
      let init = strip_value_wrappers(body_ref, init);
      let Some(init_expr) = body_ref.exprs.get(init.0 as usize) else {
        continue;
      };
      if !matches!(init_expr.kind, ExprKind::FunctionExpr { .. }) {
        continue;
      }

      if resolved.is_some() {
        // Multiple candidates; give up rather than guessing.
        return callback_expr;
      }
      resolved = Some(init);
    }
  }

  resolved.unwrap_or(callback_expr)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssocType {
  Boolean,
  BigInt,
  Number,
  String,
}

fn infer_associative_inline_callback(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
) -> Option<bool> {
  let callsite_body = lowered.body(body)?;
  let cb_expr = callsite_body.exprs.get(callback_expr.0 as usize)?;
  let ExprKind::FunctionExpr { def, body: cb_body, .. } = cb_expr.kind else {
    return None;
  };

  let cb_body_data = lowered.body(cb_body)?;
  let func = cb_body_data.function.as_ref()?;

  let param_a = func.params.get(0)?;
  let param_b = func.params.get(1)?;
  let name_a = match cb_body_data.pats.get(param_a.pat.0 as usize)?.kind {
    PatKind::Ident(name) => name,
    _ => return None,
  };
  let name_b = match cb_body_data.pats.get(param_b.pat.0 as usize)?.kind {
    PatKind::Ident(name) => name,
    _ => return None,
  };

  let arenas = lowered.type_arenas(def)?;
  let ty_a = assoc_type_for_param(arenas, param_a.type_annotation?)?;
  let ty_b = assoc_type_for_param(arenas, param_b.type_annotation?)?;
  if ty_a != ty_b {
    return None;
  }

  let expr = match &func.body {
    FunctionBody::Expr(expr) => *expr,
    FunctionBody::Block(stmts) => return_expr_only(cb_body_data, stmts)?,
  };
  let expr = strip_value_wrappers(cb_body_data, expr);

  let ExprKind::Binary { op, left, right } = &cb_body_data.exprs.get(expr.0 as usize)?.kind else {
    return None;
  };

  let left_name = match cb_body_data.exprs.get(left.0 as usize)?.kind {
    ExprKind::Ident(n) => n,
    _ => return None,
  };
  let right_name = match cb_body_data.exprs.get(right.0 as usize)?.kind {
    ExprKind::Ident(n) => n,
    _ => return None,
  };

  if !((left_name == name_a && right_name == name_b)
    || (is_commutative_op(ty_a, *op) && left_name == name_b && right_name == name_a))
  {
    return None;
  }

  Some(is_associative_op(ty_a, *op))
}

fn assoc_type_for_param(arenas: &TypeArenas, ty: TypeExprId) -> Option<AssocType> {
  let node = arenas.type_exprs.get(ty.0 as usize)?;
  match &node.kind {
    TypeExprKind::Parenthesized(inner) => assoc_type_for_param(arenas, *inner),
    TypeExprKind::Boolean => Some(AssocType::Boolean),
    TypeExprKind::BigInt => Some(AssocType::BigInt),
    TypeExprKind::Number => Some(AssocType::Number),
    TypeExprKind::String => Some(AssocType::String),
    _ => None,
  }
}

fn is_associative_op(ty: AssocType, op: BinaryOp) -> bool {
  match (ty, op) {
    (AssocType::Boolean, BinaryOp::LogicalAnd | BinaryOp::LogicalOr) => true,
    (AssocType::BigInt, BinaryOp::Add | BinaryOp::Multiply) => true,
    (AssocType::BigInt, BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor) => true,
    (AssocType::Number, BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor) => true,
    (AssocType::String, BinaryOp::Add) => true,
    _ => false,
  }
}

fn is_commutative_op(ty: AssocType, op: BinaryOp) -> bool {
  match (ty, op) {
    (AssocType::Boolean, BinaryOp::LogicalAnd | BinaryOp::LogicalOr) => true,
    (AssocType::BigInt, BinaryOp::Add | BinaryOp::Multiply) => true,
    (AssocType::BigInt, BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor) => true,
    (AssocType::Number, BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor) => true,
    _ => false,
  }
}

fn return_expr_only(body: &hir_js::Body, stmts: &[StmtId]) -> Option<ExprId> {
  if stmts.len() != 1 {
    return None;
  }
  let stmt = body.stmts.get(stmts[0].0 as usize)?;
  match &stmt.kind {
    StmtKind::Return(Some(expr)) => Some(*expr),
    _ => None,
  }
}

fn strip_value_wrappers(body: &hir_js::Body, mut expr: ExprId) -> ExprId {
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

#[cfg(feature = "typed")]
pub fn plan_array_chains_typed(
  kb: &KnowledgeBase,
  lowered: &hir_js::LowerResult,
  body_id: BodyId,
  types: &impl crate::types::TypeProvider,
) -> Vec<ArrayPipelinePlan> {
  let patterns = crate::recognize_patterns_typed(kb, lowered, body_id, types);

  let mut plans = Vec::new();
  for pat in patterns {
    let crate::RecognizedPattern::ArrayChain { base, ops, terminal } = pat else {
      continue;
    };

    let mut stages: Vec<ArrayStage> = Vec::with_capacity(ops.len() + terminal.as_ref().map(|_| 1).unwrap_or(0));

    for op in ops {
      let (kind, callback) = match op {
        crate::ArrayChainOp::Map { callback } => (ArrayStageKind::Map, callback),
        crate::ArrayChainOp::Filter { callback } => (ArrayStageKind::Filter, callback),
        crate::ArrayChainOp::FlatMap { callback } => (ArrayStageKind::FlatMap, callback),
      };

      let api_id = api_id_for_kind(kind);
      let api = kb.get_by_id(api_id);
      let callsite = callsite_info_for_callback(lowered, body_id, kind, callback, kb);
      let output_len = api.map(properties::output_length_relation).unwrap_or(OutputLengthRelation::Unknown);

      let parallelizable = if is_short_circuit(kind) {
        api.is_some_and(|api| api.parallelizable == Some(true)) && callsite.callback_may_throw == Some(false)
      } else {
        api.is_some_and(|api| crate::meta::parallelizable_at_callsite(api, &callsite))
      };

      stages.push(ArrayStage {
        kind,
        api: api_id,
        callback,
        callsite,
        meta: ArrayStageMeta {
          fusable_with_next: false,
          output_len,
          parallelizable,
          short_circuit: is_short_circuit(kind),
        },
      });
    }

    if let Some(terminal) = terminal {
      let (kind, callback) = match terminal {
        crate::ArrayTerminal::Reduce { callback, .. } => (ArrayStageKind::Reduce, callback),
        crate::ArrayTerminal::Find { callback } => (ArrayStageKind::Find, callback),
        crate::ArrayTerminal::Every { callback } => (ArrayStageKind::Every, callback),
        crate::ArrayTerminal::Some { callback } => (ArrayStageKind::Some, callback),
        crate::ArrayTerminal::ForEach { callback } => (ArrayStageKind::ForEach, callback),
      };

      let api_id = api_id_for_kind(kind);
      let api = kb.get_by_id(api_id);
      let callsite = callsite_info_for_callback(lowered, body_id, kind, callback, kb);
      let output_len = api.map(properties::output_length_relation).unwrap_or(OutputLengthRelation::Unknown);

      let parallelizable = if is_short_circuit(kind) {
        api.is_some_and(|api| api.parallelizable == Some(true)) && callsite.callback_may_throw == Some(false)
      } else {
        api.is_some_and(|api| crate::meta::parallelizable_at_callsite(api, &callsite))
      };

      stages.push(ArrayStage {
        kind,
        api: api_id,
        callback,
        callsite,
        meta: ArrayStageMeta {
          fusable_with_next: false,
          output_len,
          parallelizable,
          short_circuit: is_short_circuit(kind),
        },
      });
    }

    // Annotate fusion boundaries.
    for idx in 0..stages.len() {
      let Some(next) = stages.get(idx + 1) else {
        break;
      };

      let Some(cur_api) = kb.get_by_id(stages[idx].api) else {
        continue;
      };
      let Some(next_api) = kb.get_by_id(next.api) else {
        continue;
      };

      if properties::fusable_with(cur_api, next_api) || properties::fusable_with(next_api, cur_api) {
        stages[idx].meta.fusable_with_next = true;
      }
    }

    plans.push(ArrayPipelinePlan { base, stages });
  }

  plans
}
