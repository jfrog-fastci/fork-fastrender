use effect_model::{EffectSet, Purity};
use hir_js::hir::SwitchCase;
#[cfg(feature = "hir-semantic-ops")]
use hir_js::ArrayChainOp;
use hir_js::{
  ArrayElement, BinaryOp, Body, BodyId, ExprId, ExprKind, ForHead, ForInit, FunctionBody, NameId,
  ObjectKey, ObjectProperty, PatId, PatKind, StmtId, StmtKind, TypeArenas, TypeExprId,
  TypeExprKind, VarDecl, VarDeclKind,
};
use knowledge_base::{ApiDatabase, ApiKind, KnowledgeBase, TargetEnv};

use crate::api_use::{resolve_api_use, ApiUseKind};
use crate::eval::eval_api_call;
use crate::target::TargetedKb;
use crate::template_eval::eval_call_expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackInfo {
  pub effects: EffectSet,
  pub purity: Purity,
  pub uses_index: bool,
  pub uses_array: bool,
}

pub fn callsite_info_for_args(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  kb: &KnowledgeBase,
) -> crate::db::CallSiteInfo {
  callsite_info_for_args_for_target(lowered, body, call_expr, kb, &TargetEnv::Unknown)
}

pub fn callsite_info_for_args_for_target(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  db: &ApiDatabase,
  target: &TargetEnv,
) -> crate::db::CallSiteInfo {
  let kb = TargetedKb::new(db, target.clone());
  callsite_info_for_args_with_kb(lowered, body, call_expr, &kb)
}

fn callsite_info_for_args_with_kb(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  kb: &TargetedKb<'_>,
) -> crate::db::CallSiteInfo {
  let Some(body_ref) = lowered.body(body) else {
    return crate::db::CallSiteInfo::default();
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return crate::db::CallSiteInfo::default();
  };
  let (callback_expr, index_arg_position, array_arg_position) = match &expr.kind {
    ExprKind::Call(call) => {
      let mut index_arg_position = 1;
      let mut array_arg_position = 2;
      let callee = strip_value_wrappers(body_ref, call.callee);
      if let Some(callee) = body_ref.exprs.get(callee.0 as usize) {
        if let ExprKind::Member(member) = &callee.kind {
          if !member.optional {
            let prop = match &member.property {
              ObjectKey::Ident(name) => lowered.names.resolve(*name),
              ObjectKey::String(s) => Some(s.as_str()),
              _ => None,
            };
            if matches!(prop, Some("reduce" | "reduceRight")) {
              // `Array.prototype.reduce` callback signature:
              //   (accumulator, current, index, array)
              index_arg_position = 2;
              array_arg_position = 3;
            }
          }
        }
      }

      (
        call
          .args
          .first()
          .filter(|arg| !arg.spread)
          .map(|arg| arg.expr),
        index_arg_position,
        array_arg_position,
      )
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { callback, .. }
    | ExprKind::ArrayFilter { callback, .. }
    | ExprKind::ArrayFind { callback, .. }
    | ExprKind::ArrayEvery { callback, .. }
    | ExprKind::ArraySome { callback, .. } => (Some(*callback), 1, 2),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayReduce { callback, .. } => (Some(*callback), 2, 3),
    _ => (None, 1, 2),
  };

  let Some(callback_expr) = callback_expr else {
    return crate::db::CallSiteInfo::default();
  };
  let callback = analyze_inline_callback_with_kb_with_arg_positions(
    lowered,
    body,
    callback_expr,
    kb,
    index_arg_position,
    array_arg_position,
  );
  let callback_purity = callback.map(|cb| cb.purity);
  let callback_effects = callback.map(|cb| cb.effects);
  let callback_may_throw = callback_effects
    .map(|e| e.contains(EffectSet::MAY_THROW) || e.contains(EffectSet::UNKNOWN_CALL));
  let associative = callback.and_then(|_| infer_associative_inline_callback(lowered, body, callback_expr));
  crate::db::CallSiteInfo {
    callback_purity,
    callback_effects,
    callback_may_throw,
    callback_is_pure: callback_purity.map(|p| matches!(p, Purity::Pure | Purity::Allocating)),
    callback_uses_index: callback.map(|cb| cb.uses_index),
    callback_uses_array: callback.map(|cb| cb.uses_array),
    callback_is_associative: associative,
    ..crate::db::CallSiteInfo::default()
  }
}

pub fn eval_callsite_info_for_args(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  kb: &KnowledgeBase,
) -> crate::eval::CallSiteInfo {
  eval_callsite_info_for_args_for_target(lowered, body, call_expr, kb, &TargetEnv::Unknown)
}

pub fn eval_callsite_info_for_args_for_target(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  db: &ApiDatabase,
  target: &TargetEnv,
) -> crate::eval::CallSiteInfo {
  let kb = TargetedKb::new(db, target.clone());
  eval_callsite_info_for_args_with_kb(lowered, body, call_expr, &kb)
}

fn eval_callsite_info_for_args_with_kb(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  kb: &TargetedKb<'_>,
) -> crate::eval::CallSiteInfo {
  let Some(body_ref) = lowered.body(body) else {
    return crate::eval::CallSiteInfo::default();
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return crate::eval::CallSiteInfo::default();
  };
  let (callback_expr, index_arg_position, array_arg_position) = match &expr.kind {
    ExprKind::Call(call) => {
      let mut index_arg_position = 1;
      let mut array_arg_position = 2;
      let callee = strip_value_wrappers(body_ref, call.callee);
      if let Some(callee) = body_ref.exprs.get(callee.0 as usize) {
        if let ExprKind::Member(member) = &callee.kind {
          if !member.optional {
            let prop = match &member.property {
              ObjectKey::Ident(name) => lowered.names.resolve(*name),
              ObjectKey::String(s) => Some(s.as_str()),
              _ => None,
            };
            if matches!(prop, Some("reduce" | "reduceRight")) {
              index_arg_position = 2;
              array_arg_position = 3;
            }
          }
        }
      }

      (
        call
          .args
          .first()
          .filter(|arg| !arg.spread)
          .map(|arg| arg.expr),
        index_arg_position,
        array_arg_position,
      )
    }
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { callback, .. }
    | ExprKind::ArrayFilter { callback, .. }
    | ExprKind::ArrayFind { callback, .. }
    | ExprKind::ArrayEvery { callback, .. }
    | ExprKind::ArraySome { callback, .. } => (Some(*callback), 1, 2),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayReduce { callback, .. } => (Some(*callback), 2, 3),
    _ => (None, 1, 2),
  };

  let Some(callback_expr) = callback_expr else {
    return crate::eval::CallSiteInfo::default();
  };
  let callback = analyze_inline_callback_with_kb_with_arg_positions(
    lowered,
    body,
    callback_expr,
    kb,
    index_arg_position,
    array_arg_position,
  );

  match callback {
    Some(cb) => crate::eval::CallSiteInfo {
      arg_purity: vec![cb.purity],
      arg_effects: vec![cb.effects],
      callback_uses_index: cb.uses_index,
      callback_uses_array: cb.uses_array,
    },
    None => crate::eval::CallSiteInfo::default(),
  }
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
  let ExprKind::FunctionExpr {
    def, body: cb_body, ..
  } = cb_expr.kind
  else {
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

  // For commutative operations, accept either operand order. This matters for
  // `reduce` callbacks that use `(acc, cur) => cur | acc` etc.
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

fn return_expr_only(body: &Body, stmts: &[StmtId]) -> Option<ExprId> {
  if stmts.len() != 1 {
    return None;
  }
  let stmt = body.stmts.get(stmts[0].0 as usize)?;
  match &stmt.kind {
    StmtKind::Return(Some(expr)) => Some(*expr),
    _ => None,
  }
}

fn strip_value_wrappers(body: &Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

pub fn analyze_inline_callback(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
  kb: &KnowledgeBase,
) -> Option<CallbackInfo> {
  analyze_inline_callback_with_kb(
    lowered,
    body,
    callback_expr,
    &TargetedKb::new(kb, TargetEnv::Unknown),
  )
}

pub fn analyze_inline_callback_for_target(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
  db: &ApiDatabase,
  target: &TargetEnv,
) -> Option<CallbackInfo> {
  let kb = TargetedKb::new(db, target.clone());
  analyze_inline_callback_with_kb(lowered, body, callback_expr, &kb)
}

pub(crate) fn analyze_inline_callback_with_kb(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
  kb: &TargetedKb<'_>,
) -> Option<CallbackInfo> {
  // Default to `Array.prototype.map`-style callback arguments:
  //   (value, index, array)
  analyze_inline_callback_with_kb_with_arg_positions(lowered, body, callback_expr, kb, 1, 2)
}

fn analyze_inline_callback_with_kb_with_arg_positions(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  callback_expr: ExprId,
  kb: &TargetedKb<'_>,
  index_arg_position: u32,
  array_arg_position: u32,
) -> Option<CallbackInfo> {
  let callsite_body = lowered.body(body)?;
  let cb_expr = callsite_body.exprs.get(callback_expr.0 as usize)?;

  let ExprKind::FunctionExpr {
    body: cb_body,
    is_arrow,
    ..
  } = &cb_expr.kind
  else {
    return analyze_known_callback_reference(
      lowered,
      callsite_body,
      callback_expr,
      kb,
      index_arg_position,
      array_arg_position,
    );
  };

  let cb_body_data = lowered.body(*cb_body)?;
  let func = cb_body_data.function.as_ref()?;

  let arguments_object_available = !*is_arrow
    && !func
      .params
      .iter()
      .any(|param| pat_binds_text(cb_body_data, lowered.names.as_ref(), param.pat, "arguments"));

  // Track whether the callback uses the index/array arguments. In addition to
  // explicit identifier references, destructuring/rest parameters also "use"
  // their argument values during binding.
  let mut uses_index = false;
  let mut uses_array = false;
  let mut index_param: Option<NameId> = None;
  let mut array_param: Option<NameId> = None;

  let index_param_pos = index_arg_position as usize;
  let array_param_pos = array_arg_position as usize;

  // Rest parameters can capture the `index`/`array` arguments even when they do
  // not appear as explicit named parameters (e.g. `(...rest)` or `(x, ...rest)`).
  if let Some(rest_pos) = func.params.iter().position(|param| param.rest) {
    let rest_param = func.params.get(rest_pos)?;
    let pat = cb_body_data.pats.get(rest_param.pat.0 as usize)?;
    match pat.kind {
      PatKind::Ident(name) => {
        if rest_pos <= index_param_pos {
          index_param = Some(name);
        }
        if rest_pos <= array_param_pos {
          array_param = Some(name);
        }
      }
      _ => {
        if rest_pos <= index_param_pos {
          uses_index = true;
        }
        if rest_pos <= array_param_pos {
          uses_array = true;
        }
      }
    }
  }

  if index_param.is_none() {
    if let Some(index_param_raw) = func.params.get(index_param_pos).filter(|p| !p.rest) {
      let pat = cb_body_data.pats.get(index_param_raw.pat.0 as usize)?;
      match pat.kind {
        PatKind::Ident(name) => index_param = Some(name),
        _ => uses_index = true,
      }
    }
  }

  if array_param.is_none() {
    if let Some(array_param_raw) = func.params.get(array_param_pos).filter(|p| !p.rest) {
      let pat = cb_body_data.pats.get(array_param_raw.pat.0 as usize)?;
      match pat.kind {
        PatKind::Ident(name) => array_param = Some(name),
        _ => uses_array = true,
      }
    }
  }

  let mut analyzer = CallbackAnalyzer {
    lowered,
    kb,
    body: *cb_body,
    arguments_object_available,
    index_arg_position,
    array_arg_position,
    index_param,
    array_param,
    uses_index,
    uses_array,
    shadow_stack: vec![ShadowScope::default()],
    effects: EffectSet::empty(),
    purity: Purity::Pure,
  };

  // Parameter binding (default initializers, destructuring defaults/computed keys,
  // rest params) happens before the function body executes, but is still part of
  // the callback's runtime behavior.
  //
  // Be conservative and assume parameter initializers may run (callers can pass
  // `undefined` or omit args).
  for param in &func.params {
    if let Some(default) = param.default {
      analyzer.visit_expr(cb_body_data, default);
    }
    analyzer.visit_binding_pat(cb_body_data, param.pat);
    if param.rest {
      analyzer.effects |= EffectSet::ALLOCATES;
    }
  }

  match &func.body {
    FunctionBody::Block(stmts) => {
      // The function body is a lexical scope; declarations inside it are hoisted
      // for the purposes of name resolution.
      analyzer.shadow_stack[0] = analyzer.scan_shadow_in_stmts(cb_body_data, stmts);
      for stmt in stmts {
        analyzer.visit_stmt(cb_body_data, *stmt);
      }
    }
    FunctionBody::Expr(expr) => analyzer.visit_expr(cb_body_data, *expr),
  }

  let effects = analyzer.effects;
  let purity = Purity::join(analyzer.purity, effects.inferred_purity());

  Some(CallbackInfo {
    effects,
    purity,
    uses_index: analyzer.uses_index,
    uses_array: analyzer.uses_array,
  })
}

fn analyze_known_callback_reference(
  lowered: &hir_js::LowerResult,
  body: &Body,
  callback_expr: ExprId,
  kb: &TargetedKb<'_>,
  index_arg_position: u32,
  array_arg_position: u32,
) -> Option<CallbackInfo> {
  let resolved = resolve_api_use(
    lowered.hir.as_ref(),
    body,
    callback_expr,
    lowered.names.as_ref(),
    kb.db(),
  )?;
  if resolved.kind != ApiUseKind::Value {
    return None;
  }

  let api = kb.get_by_id(resolved.api)?;
  if !matches!(api.kind, ApiKind::Function | ApiKind::Constructor) {
    return None;
  }

  let sem = eval_api_call(api, &crate::eval::CallSiteInfo::default());
  let (uses_index, uses_array) =
    infer_known_callback_arg_usage(api, index_arg_position, array_arg_position);
  Some(CallbackInfo {
    effects: sem.effects,
    purity: sem.purity,
    uses_index,
    uses_array,
  })
}

struct CallbackAnalyzer<'a, 'db> {
  lowered: &'a hir_js::LowerResult,
  kb: &'a TargetedKb<'db>,
  body: BodyId,
  arguments_object_available: bool,
  index_arg_position: u32,
  array_arg_position: u32,
  index_param: Option<NameId>,
  array_param: Option<NameId>,
  uses_index: bool,
  uses_array: bool,
  shadow_stack: Vec<ShadowScope>,
  effects: EffectSet,
  purity: Purity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ShadowScope {
  shadow_index: bool,
  shadow_array: bool,
  shadow_arguments: bool,
}

impl<'a, 'db> CallbackAnalyzer<'a, 'db> {
  fn merge_effects(&mut self, other: EffectSet) {
    self.effects |= other;
  }

  fn merge_purity(&mut self, other: Purity) {
    self.purity = Purity::join(self.purity, other);
  }

  fn mark_unknown(&mut self) {
    self.merge_effects(EffectSet::UNKNOWN_CALL);
    self.merge_purity(Purity::Impure);
  }

  fn record_arguments_slot(&mut self, slot: u32) {
    if slot == self.index_arg_position {
      self.uses_index = true;
    }
    if slot == self.array_arg_position {
      self.uses_array = true;
    }
  }

  fn record_arguments_slot_str(&mut self, key: &str) {
    if let Ok(slot) = key.parse::<u32>() {
      self.record_arguments_slot(slot);
    }
  }

  fn scan_shadow_in_stmts(&self, body: &Body, stmts: &[StmtId]) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if self.index_param.is_none() && self.array_param.is_none() && !self.arguments_object_available
    {
      return scope;
    }
    let index = self.index_param;
    let array = self.array_param;
    let track_arguments = self.arguments_object_available;
    for stmt_id in stmts {
      let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
        continue;
      };
      match &stmt.kind {
        StmtKind::Var(var) => {
          if !is_lexical_var_decl(var) {
            continue;
          }
          if let Some(index) = index {
            if var
              .declarators
              .iter()
              .any(|d| pat_binds_name(body, d.pat, index))
            {
              scope.shadow_index = true;
            }
          }
          if let Some(array) = array {
            if var
              .declarators
              .iter()
              .any(|d| pat_binds_name(body, d.pat, array))
            {
              scope.shadow_array = true;
            }
          }
          if track_arguments
            && var
              .declarators
              .iter()
              .any(|d| pat_binds_text(body, self.lowered.names.as_ref(), d.pat, "arguments"))
          {
            scope.shadow_arguments = true;
          }
        }
        StmtKind::Decl(def_id) => {
          let Some(def) = self.lowered.def(*def_id) else {
            continue;
          };
          if Some(def.name) == index {
            scope.shadow_index = true;
          }
          if Some(def.name) == array {
            scope.shadow_array = true;
          }
          if track_arguments && self.lowered.names.resolve(def.name) == Some("arguments") {
            scope.shadow_arguments = true;
          }
        }
        _ => continue,
      };
    }
    scope
  }

  fn scan_shadow_in_switch(&self, body: &Body, cases: &[SwitchCase]) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if self.index_param.is_none() && self.array_param.is_none() && !self.arguments_object_available
    {
      return scope;
    }
    let index = self.index_param;
    let array = self.array_param;
    let track_arguments = self.arguments_object_available;
    for case in cases {
      for stmt_id in &case.consequent {
        let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
          continue;
        };
        match &stmt.kind {
          StmtKind::Var(var) => {
            if !is_lexical_var_decl(var) {
              continue;
            }
            if let Some(index) = index {
              if var
                .declarators
                .iter()
                .any(|d| pat_binds_name(body, d.pat, index))
              {
                scope.shadow_index = true;
              }
            }
            if let Some(array) = array {
              if var
                .declarators
                .iter()
                .any(|d| pat_binds_name(body, d.pat, array))
              {
                scope.shadow_array = true;
              }
            }
            if track_arguments
              && var
                .declarators
                .iter()
                .any(|d| pat_binds_text(body, self.lowered.names.as_ref(), d.pat, "arguments"))
            {
              scope.shadow_arguments = true;
            }
          }
          StmtKind::Decl(def_id) => {
            let Some(def) = self.lowered.def(*def_id) else {
              continue;
            };
            if Some(def.name) == index {
              scope.shadow_index = true;
            }
            if Some(def.name) == array {
              scope.shadow_array = true;
            }
            if track_arguments && self.lowered.names.resolve(def.name) == Some("arguments") {
              scope.shadow_arguments = true;
            }
          }
          _ => continue,
        }
      }
    }
    scope
  }

  fn scan_shadow_in_var_decl(&self, body: &Body, var: &VarDecl) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if let Some(index) = self.index_param {
      if var
        .declarators
        .iter()
        .any(|d| pat_binds_name(body, d.pat, index))
      {
        scope.shadow_index = true;
      }
    }
    if let Some(array) = self.array_param {
      if var
        .declarators
        .iter()
        .any(|d| pat_binds_name(body, d.pat, array))
      {
        scope.shadow_array = true;
      }
    }
    if self.arguments_object_available
      && var
        .declarators
        .iter()
        .any(|d| pat_binds_text(body, self.lowered.names.as_ref(), d.pat, "arguments"))
    {
      scope.shadow_arguments = true;
    }
    scope
  }

  fn scan_shadow_in_pat(&self, body: &Body, pat: PatId) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if let Some(index) = self.index_param {
      if pat_binds_name(body, pat, index) {
        scope.shadow_index = true;
      }
    }
    if let Some(array) = self.array_param {
      if pat_binds_name(body, pat, array) {
        scope.shadow_array = true;
      }
    }
    if self.arguments_object_available
      && pat_binds_text(body, self.lowered.names.as_ref(), pat, "arguments")
    {
      scope.shadow_arguments = true;
    }
    scope
  }

  fn name_is_shadowed(&self, name: NameId) -> bool {
    if Some(name) == self.index_param {
      return self.shadow_stack.iter().any(|s| s.shadow_index);
    }
    if Some(name) == self.array_param {
      return self.shadow_stack.iter().any(|s| s.shadow_array);
    }
    if self.arguments_object_available && self.lowered.names.resolve(name) == Some("arguments") {
      return self.shadow_stack.iter().any(|s| s.shadow_arguments);
    }
    false
  }

  fn visit_stmt(&mut self, body: &Body, stmt_id: StmtId) {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      return;
    };

    match &stmt.kind {
      StmtKind::Expr(expr) => self.visit_expr(body, *expr),
      StmtKind::ExportDefaultExpr(expr) => self.visit_expr(body, *expr),
      StmtKind::Decl(_) => {
        // Declaring a function/class creates a runtime value, but the body is
        // not executed here.
        self.effects |= EffectSet::ALLOCATES;
      }
      StmtKind::Return(expr) => {
        if let Some(expr) = expr {
          self.visit_expr(body, *expr);
        }
      }
      StmtKind::Block(stmts) => {
        let scope = self.scan_shadow_in_stmts(body, stmts);
        self.shadow_stack.push(scope);
        for stmt in stmts {
          self.visit_stmt(body, *stmt);
        }
        self.shadow_stack.pop();
      }
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => {
        self.visit_expr(body, *test);
        self.visit_stmt(body, *consequent);
        if let Some(alternate) = alternate {
          self.visit_stmt(body, *alternate);
        }
      }
      StmtKind::While { test, body: inner } | StmtKind::DoWhile { test, body: inner } => {
        self.visit_expr(body, *test);
        self.visit_stmt(body, *inner);
      }
      StmtKind::For {
        init,
        test,
        update,
        body: inner,
      } => {
        let mut pushed = false;
        if let Some(ForInit::Var(var)) = init.as_ref() {
          if is_lexical_var_decl(var) {
            self
              .shadow_stack
              .push(self.scan_shadow_in_var_decl(body, var));
            pushed = true;
          }
        }
        if let Some(init) = init {
          match init {
            ForInit::Expr(expr) => self.visit_expr(body, *expr),
            ForInit::Var(var) => self.visit_var_decl(body, var),
          }
        }
        if let Some(test) = test {
          self.visit_expr(body, *test);
        }
        if let Some(update) = update {
          self.visit_expr(body, *update);
        }
        self.visit_stmt(body, *inner);
        if pushed {
          self.shadow_stack.pop();
        }
      }
      StmtKind::ForIn {
        left,
        right,
        body: inner,
        ..
      } => {
        // In `for..in`/`for..of`, the RHS is evaluated *before* introducing the
        // per-loop lexical environment for `let`/`const` bindings.
        //
        // Example:
        //   for (let i of [i]) {}
        // Here the `[i]` refers to the outer `i`, not the loop binding.
        self.visit_expr(body, *right);
        let mut pushed = false;
        if let ForHead::Var(var) = left {
          if is_lexical_var_decl(var) {
            self
              .shadow_stack
              .push(self.scan_shadow_in_var_decl(body, var));
            pushed = true;
          }
        }
        match left {
          ForHead::Pat(pat) => self.visit_assign_pat(body, *pat),
          ForHead::Var(var) => self.visit_var_decl(body, var),
        }
        self.visit_stmt(body, *inner);
        if pushed {
          self.shadow_stack.pop();
        }
      }
      StmtKind::Switch {
        discriminant,
        cases,
      } => {
        self.visit_expr(body, *discriminant);
        let scope = self.scan_shadow_in_switch(body, cases);
        self.shadow_stack.push(scope);
        for case in cases {
          if let Some(test) = case.test {
            self.visit_expr(body, test);
          }
          for stmt in &case.consequent {
            self.visit_stmt(body, *stmt);
          }
        }
        self.shadow_stack.pop();
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        self.visit_stmt(body, *block);
        if let Some(catch) = catch {
          let mut pushed = false;
          if let Some(param) = catch.param {
            self.shadow_stack.push(self.scan_shadow_in_pat(body, param));
            pushed = true;
            self.visit_binding_pat(body, param);
          }
          self.visit_stmt(body, catch.body);
          if pushed {
            self.shadow_stack.pop();
          }
        }
        if let Some(finally_block) = finally_block {
          self.visit_stmt(body, *finally_block);
        }
      }
      StmtKind::Throw(expr) => {
        self.effects |= EffectSet::MAY_THROW;
        self.visit_expr(body, *expr);
      }
      StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Debugger | StmtKind::Empty => {}
      StmtKind::Var(var) => self.visit_var_decl(body, var),
      StmtKind::Labeled { body: inner, .. } => self.visit_stmt(body, *inner),
      StmtKind::With {
        object,
        body: inner,
      } => {
        // `with` can change name resolution in ways we can't model locally.
        self.mark_unknown();
        self.visit_expr(body, *object);
        self.visit_stmt(body, *inner);
      }
    }
  }

  fn visit_var_decl(&mut self, body: &Body, var: &VarDecl) {
    for declarator in &var.declarators {
      self.visit_binding_pat(body, declarator.pat);
      if let Some(init) = declarator.init {
        self.visit_expr(body, init);
      }
    }
  }

  fn visit_binding_pat(&mut self, body: &Body, pat_id: PatId) {
    let Some(pat) = body.pats.get(pat_id.0 as usize) else {
      return;
    };

    match &pat.kind {
      PatKind::Ident(_) => {}
      PatKind::Array(array) => {
        for elem in &array.elements {
          if let Some(elem) = elem {
            self.visit_binding_pat(body, elem.pat);
            if let Some(default) = elem.default_value {
              self.visit_expr(body, default);
            }
          }
        }
        if let Some(rest) = array.rest {
          self.effects |= EffectSet::ALLOCATES;
          self.visit_binding_pat(body, rest);
        }
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          if let ObjectKey::Computed(expr) = &prop.key {
            self.visit_expr(body, *expr);
          }
          self.visit_binding_pat(body, prop.value);
          if let Some(default) = prop.default_value {
            self.visit_expr(body, default);
          }
        }
        if let Some(rest) = obj.rest {
          self.effects |= EffectSet::ALLOCATES;
          self.visit_binding_pat(body, rest);
        }
      }
      PatKind::Rest(rest) => {
        self.effects |= EffectSet::ALLOCATES;
        self.visit_binding_pat(body, **rest)
      }
      PatKind::Assign {
        target,
        default_value,
      } => {
        self.visit_binding_pat(body, *target);
        self.visit_expr(body, *default_value);
      }
      PatKind::AssignTarget(expr) => self.visit_expr(body, *expr),
    }
  }

  fn visit_assign_pat(&mut self, body: &Body, pat_id: PatId) {
    let Some(pat) = body.pats.get(pat_id.0 as usize) else {
      return;
    };

    match &pat.kind {
      PatKind::Ident(name) => self.record_ident(*name),
      PatKind::Array(array) => {
        for elem in &array.elements {
          if let Some(elem) = elem {
            self.visit_assign_pat(body, elem.pat);
            if let Some(default) = elem.default_value {
              self.visit_expr(body, default);
            }
          }
        }
        if let Some(rest) = array.rest {
          self.visit_assign_pat(body, rest);
        }
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          if let ObjectKey::Computed(expr) = &prop.key {
            self.visit_expr(body, *expr);
          }
          self.visit_assign_pat(body, prop.value);
          if let Some(default) = prop.default_value {
            self.visit_expr(body, default);
          }
        }
        if let Some(rest) = obj.rest {
          self.visit_assign_pat(body, rest);
        }
      }
      PatKind::Rest(rest) => self.visit_assign_pat(body, **rest),
      PatKind::Assign {
        target,
        default_value,
      } => {
        self.visit_assign_pat(body, *target);
        self.visit_expr(body, *default_value);
      }
      PatKind::AssignTarget(expr) => self.visit_assign_target_expr(body, *expr),
    }
  }

  fn visit_assign_target_expr(&mut self, body: &Body, expr_id: ExprId) {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return;
    };

    match &expr.kind {
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::Satisfies { expr, .. } => self.visit_assign_target_expr(body, *expr),

      ExprKind::Ident(name) => self.record_ident(*name),

      ExprKind::Member(mem) => {
        self.visit_expr(body, mem.object);
        if let ObjectKey::Computed(expr) = &mem.property {
          self.visit_expr(body, *expr);
        }
      }

      _ => self.visit_expr(body, expr_id),
    }
  }

  fn visit_expr(&mut self, body: &Body, expr_id: ExprId) {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return;
    };

    match &expr.kind {
      ExprKind::Missing
      | ExprKind::This
      | ExprKind::Super
      | ExprKind::Literal(_)
      | ExprKind::ImportMeta
      | ExprKind::NewTarget => {}

      ExprKind::Ident(name) => self.record_ident(*name),

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::AwaitExpr { value, .. } => {
        // Awaiting is currently treated as a transparent wrapper around the
        // awaited expression. Downstream phases may want to model this as
        // nondeterministic + may_throw.
        self.visit_expr(body, *value);
      }
      ExprKind::Unary {
        op: hir_js::UnaryOp::Delete,
        expr,
      } => {
        // `delete` can mutate objects and may throw (e.g. deleting non-configurable properties in
        // strict mode), so treat it as impure.
        self.mark_unknown();
        self.visit_expr(body, *expr);
      }
      ExprKind::Unary { expr, .. } | ExprKind::Await { expr } | ExprKind::NonNull { expr } => {
        self.visit_expr(body, *expr);
      }

      ExprKind::Update { expr, .. } => {
        self.mark_unknown();
        self.visit_expr(body, *expr);
      }

      ExprKind::Binary { left, right, .. } => {
        self.visit_expr(body, *left);
        self.visit_expr(body, *right);
      }

      ExprKind::Assignment { target, value, .. } => {
        self.mark_unknown();
        self.visit_assign_pat(body, *target);
        self.visit_expr(body, *value);
      }

      ExprKind::Call(call) => {
        self.visit_expr(body, call.callee);
        for arg in &call.args {
          self.visit_expr(body, arg.expr);
        }

        // Only model the call itself (excluding evaluation of callee/args).
        let call_eval = eval_call_expr(self.kb, self.lowered, self.body, expr_id);
        self.merge_effects(call_eval.effects);
        self.merge_purity(call_eval.purity);
      }

      ExprKind::Member(mem) => {
        if self.arguments_object_available {
          if let Some(name) = body
            .exprs
            .get(mem.object.0 as usize)
            .and_then(|expr| match expr.kind {
              ExprKind::Ident(name) => Some(name),
              _ => None,
            })
            .filter(|name| self.lowered.names.resolve(*name) == Some("arguments"))
            .filter(|name| !self.name_is_shadowed(*name))
          {
            // `arguments[n]` can be a (slightly) more precise proxy for whether
            // the callback uses the `index` / `array` callback arguments.
            match &mem.property {
              ObjectKey::Computed(expr) => {
                self.visit_expr(body, *expr);
                let expr_node = body.exprs.get(expr.0 as usize);
                match expr_node.map(|node| &node.kind) {
                  Some(ExprKind::Literal(hir_js::Literal::Number(n))) => {
                    self.record_arguments_slot_str(n)
                  }
                  Some(ExprKind::Literal(hir_js::Literal::BigInt(n))) => {
                    self.record_arguments_slot_str(n)
                  }
                  Some(ExprKind::Literal(hir_js::Literal::String(s))) => {
                    self.record_arguments_slot_str(&s.lossy)
                  }
                  Some(ExprKind::Template(tmpl)) if tmpl.spans.is_empty() => {
                    self.record_arguments_slot_str(&tmpl.head)
                  }
                  Some(ExprKind::Unary { op, expr: inner }) => match *op {
                    // `+<number>` preserves the numeric property key.
                    hir_js::UnaryOp::Plus => {
                      let inner_node = body.exprs.get(inner.0 as usize);
                      match inner_node.map(|node| &node.kind) {
                        Some(ExprKind::Literal(hir_js::Literal::Number(n))) => {
                          self.record_arguments_slot_str(n)
                        }
                        _ => {
                          // Unknown index; conservatively assume it may access either.
                          self.uses_index = true;
                          self.uses_array = true;
                        }
                      }
                    }
                    // `-<literal>` yields a negative numeric property key (or `-0`),
                    // so it cannot target the index/array callback arguments.
                    hir_js::UnaryOp::Minus => {
                      let inner_node = body.exprs.get(inner.0 as usize);
                      if !matches!(
                        inner_node.map(|node| &node.kind),
                        Some(ExprKind::Literal(
                          hir_js::Literal::Number(_) | hir_js::Literal::BigInt(_),
                        ))
                      ) {
                        // Unknown index; conservatively assume it may access either.
                        self.uses_index = true;
                        self.uses_array = true;
                      }
                    }
                    // `void`/`!`/`typeof` always produce non-numeric property keys.
                    hir_js::UnaryOp::Void | hir_js::UnaryOp::Not | hir_js::UnaryOp::Typeof => {}
                    _ => {
                      // Unknown index; conservatively assume it may access either.
                      self.uses_index = true;
                      self.uses_array = true;
                    }
                  },
                  // Non-numeric primitive property keys cannot target the `index` / `array` slots.
                  Some(ExprKind::Literal(
                    hir_js::Literal::Boolean(_)
                    | hir_js::Literal::Null
                    | hir_js::Literal::Undefined,
                  )) => {}
                  _ => {
                    // Unknown index; conservatively assume it may access either.
                    self.uses_index = true;
                    self.uses_array = true;
                  }
                };
              }
              // Non-computed `arguments.foo` can't observe the index/array args;
              // only numeric slots can.
              _ => {}
            }

            // Avoid double-counting via `record_ident(arguments)`.
            let _ = name;
            return;
          }
        }

        self.visit_expr(body, mem.object);
        if let ObjectKey::Computed(expr) = &mem.property {
          self.visit_expr(body, *expr);
        }

        if let Some(resolved) = resolve_api_use(
          self.lowered.hir.as_ref(),
          body,
          expr_id,
          self.lowered.names.as_ref(),
          self.kb.db(),
        ) {
          if resolved.kind == ApiUseKind::Get {
            if let Some(api) = self.kb.get_by_id(resolved.api) {
              let sem = eval_api_call(api, &crate::eval::CallSiteInfo::default());
              self.merge_effects(sem.effects);
              self.merge_purity(sem.purity);
            }
          }
        }
      }

      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        self.visit_expr(body, *test);
        self.visit_expr(body, *consequent);
        self.visit_expr(body, *alternate);
      }

      ExprKind::Array(array) => {
        self.effects |= EffectSet::ALLOCATES;
        for elem in &array.elements {
          match elem {
            ArrayElement::Expr(expr) | ArrayElement::Spread(expr) => self.visit_expr(body, *expr),
            ArrayElement::Empty => {}
          }
        }
      }

      ExprKind::Object(obj) => {
        self.effects |= EffectSet::ALLOCATES;
        for prop in &obj.properties {
          match prop {
            ObjectProperty::KeyValue { key, value, .. } => {
              if let ObjectKey::Computed(expr) = key {
                self.visit_expr(body, *expr);
              }
              self.visit_expr(body, *value);
            }
            ObjectProperty::Getter { key, .. } | ObjectProperty::Setter { key, .. } => {
              if let ObjectKey::Computed(expr) = key {
                self.visit_expr(body, *expr);
              }
              // Getter/setter bodies are not executed as part of object literal
              // evaluation.
            }
            ObjectProperty::Spread(expr) => self.visit_expr(body, *expr),
          }
        }
      }

      ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => {
        // Creating a function/class value allocates, but its body is not
        // executed here.
        self.effects |= EffectSet::ALLOCATES;
      }

      ExprKind::Template(template) => {
        self.effects |= EffectSet::ALLOCATES;
        for span in &template.spans {
          self.visit_expr(body, span.expr);
        }
      }

      ExprKind::TaggedTemplate { tag, template } => {
        self.mark_unknown();
        self.visit_expr(body, *tag);
        for span in &template.spans {
          self.visit_expr(body, span.expr);
        }
      }

      ExprKind::Yield { expr, .. } => {
        if let Some(expr) = expr {
          self.mark_unknown();
          self.visit_expr(body, *expr);
        }
      }

      ExprKind::Instantiation { expr, .. }
      | ExprKind::TypeAssertion { expr, .. }
      | ExprKind::Satisfies { expr, .. } => {
        self.visit_expr(body, *expr);
      }

      ExprKind::ImportCall {
        argument,
        attributes,
      } => {
        self.mark_unknown();
        self.visit_expr(body, *argument);
        if let Some(attributes) = attributes {
          self.visit_expr(body, *attributes);
        }
      }

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, callback }
      | ExprKind::ArrayFilter { array, callback }
      | ExprKind::ArrayFind { array, callback }
      | ExprKind::ArrayEvery { array, callback }
      | ExprKind::ArraySome { array, callback } => {
        let api = match &expr.kind {
          ExprKind::ArrayMap { .. } => "Array.prototype.map",
          ExprKind::ArrayFilter { .. } => "Array.prototype.filter",
          ExprKind::ArrayFind { .. } => "Array.prototype.find",
          ExprKind::ArrayEvery { .. } => "Array.prototype.every",
          ExprKind::ArraySome { .. } => "Array.prototype.some",
          _ => unreachable!("match arm only includes array semantic ops"),
        };

        self.visit_expr(body, *array);
        self.visit_expr(body, *callback);
        self.merge_semantic_op_call(api, Some(*callback));
      }

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        self.visit_expr(body, *array);
        self.visit_expr(body, *callback);
        if let Some(init) = init {
          self.visit_expr(body, *init);
        }

        self.merge_semantic_op_call("Array.prototype.reduce", Some(*callback));
      }

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        self.visit_expr(body, *array);
        for op in ops {
          match *op {
            ArrayChainOp::Map(callback) => {
              self.visit_expr(body, callback);
              self.merge_semantic_op_call("Array.prototype.map", Some(callback));
            }
            ArrayChainOp::Filter(callback) => {
              self.visit_expr(body, callback);
              self.merge_semantic_op_call("Array.prototype.filter", Some(callback));
            }
            ArrayChainOp::Find(callback) => {
              self.visit_expr(body, callback);
              self.merge_semantic_op_call("Array.prototype.find", Some(callback));
            }
            ArrayChainOp::Every(callback) => {
              self.visit_expr(body, callback);
              self.merge_semantic_op_call("Array.prototype.every", Some(callback));
            }
            ArrayChainOp::Some(callback) => {
              self.visit_expr(body, callback);
              self.merge_semantic_op_call("Array.prototype.some", Some(callback));
            }
            ArrayChainOp::Reduce(callback, init) => {
              self.visit_expr(body, callback);
              if let Some(init) = init {
                self.visit_expr(body, init);
              }
              self.merge_semantic_op_call("Array.prototype.reduce", Some(callback));
            }
          }
        }
      }

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        let api = match &expr.kind {
          ExprKind::PromiseAll { .. } => "Promise.all",
          ExprKind::PromiseRace { .. } => "Promise.race",
          _ => unreachable!("match arm only includes Promise.* semantic ops"),
        };

        for promise in promises {
          self.visit_expr(body, *promise);
        }

        self.merge_semantic_op_call(api, None);
      }

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::KnownApiCall { api, args } => {
        for arg in args {
          self.visit_expr(body, *arg);
        }

        let api_id = knowledge_base::ApiId::from_raw(api.raw());
        let Some(api) = self.kb.get_by_id(api_id) else {
          self.mark_unknown();
          return;
        };

        // Most known API calls do not require callsite-specific modeling; when
        // they do (e.g. `Array.prototype.map`), they conventionally use arg0 as
        // the callback.
        let callback = args.first().copied();
        let callback = callback
          .and_then(|expr| analyze_inline_callback_with_kb(self.lowered, self.body, expr, self.kb));
        let site = callback
          .map(|cb| crate::eval::CallSiteInfo {
            arg_purity: vec![cb.purity],
            arg_effects: vec![cb.effects],
            callback_uses_index: cb.uses_index,
            callback_uses_array: cb.uses_array,
          })
          .unwrap_or_default();

        let sem = eval_api_call(api, &site);
        self.merge_effects(sem.effects);
        self.merge_purity(sem.purity);
      }

      ExprKind::Jsx(_) => {
        self.effects |= EffectSet::ALLOCATES;
      }

      // Future-proofing: if `hir-js` grows new expression variants that `effect-js`
      // does not understand, keep analysis conservative rather than failing to
      // compile (or worse, under-approximating effects).
      #[allow(unreachable_patterns)]
      _ => {
        self.mark_unknown();
        self.uses_index = true;
        self.uses_array = true;
      }
    }
  }

  #[cfg(feature = "hir-semantic-ops")]
  fn merge_semantic_op_call(&mut self, api_name: &str, callback_expr: Option<ExprId>) {
    let Some(api) = self.kb.get(api_name) else {
      self.mark_unknown();
      return;
    };

    let cb = callback_expr
      .and_then(|expr| analyze_inline_callback_with_kb(self.lowered, self.body, expr, self.kb));
    let site = cb
      .map(|cb| crate::eval::CallSiteInfo {
        arg_purity: vec![cb.purity],
        arg_effects: vec![cb.effects],
        callback_uses_index: cb.uses_index,
        callback_uses_array: cb.uses_array,
      })
      .unwrap_or_default();

    let sem = eval_api_call(api, &site);
    self.merge_effects(sem.effects);
    self.merge_purity(sem.purity);
  }

  fn record_ident(&mut self, name: NameId) {
    if self.arguments_object_available
      && self.lowered.names.resolve(name) == Some("arguments")
      && !self.name_is_shadowed(name)
    {
      self.uses_index = true;
      self.uses_array = true;
    }
    if Some(name) == self.index_param && !self.name_is_shadowed(name) {
      self.uses_index = true;
    }
    if Some(name) == self.array_param && !self.name_is_shadowed(name) {
      self.uses_array = true;
    }
  }
}

fn is_lexical_var_decl(var: &VarDecl) -> bool {
  matches!(
    var.kind,
    VarDeclKind::Let | VarDeclKind::Const | VarDeclKind::Using | VarDeclKind::AwaitUsing
  )
}

fn pat_binds_name(body: &Body, pat: PatId, name: NameId) -> bool {
  let Some(pat) = body.pats.get(pat.0 as usize) else {
    return false;
  };
  match &pat.kind {
    PatKind::Ident(id) => *id == name,
    PatKind::Array(arr) => {
      for element in arr.elements.iter().flatten() {
        if pat_binds_name(body, element.pat, name) {
          return true;
        }
      }
      arr
        .rest
        .is_some_and(|rest| pat_binds_name(body, rest, name))
    }
    PatKind::Object(obj) => {
      for prop in &obj.props {
        if pat_binds_name(body, prop.value, name) {
          return true;
        }
      }
      obj
        .rest
        .is_some_and(|rest| pat_binds_name(body, rest, name))
    }
    PatKind::Rest(inner) => pat_binds_name(body, **inner, name),
    PatKind::Assign { target, .. } => pat_binds_name(body, *target, name),
    PatKind::AssignTarget(_) => false,
  }
}

fn pat_binds_text(body: &Body, names: &hir_js::NameInterner, pat: PatId, text: &str) -> bool {
  let Some(pat) = body.pats.get(pat.0 as usize) else {
    return false;
  };
  match &pat.kind {
    PatKind::Ident(id) => names.resolve(*id) == Some(text),
    PatKind::Array(arr) => {
      for element in arr.elements.iter().flatten() {
        if pat_binds_text(body, names, element.pat, text) {
          return true;
        }
      }
      arr
        .rest
        .is_some_and(|rest| pat_binds_text(body, names, rest, text))
    }
    PatKind::Object(obj) => {
      for prop in &obj.props {
        if pat_binds_text(body, names, prop.value, text) {
          return true;
        }
      }
      obj
        .rest
        .is_some_and(|rest| pat_binds_text(body, names, rest, text))
    }
    PatKind::Rest(inner) => pat_binds_text(body, names, **inner, text),
    PatKind::Assign { target, .. } => pat_binds_text(body, names, *target, text),
    PatKind::AssignTarget(_) => false,
  }
}

fn infer_known_callback_arg_usage(
  api: &knowledge_base::Api,
  index_arg_position: u32,
  array_arg_position: u32,
) -> (bool, bool) {
  let Some(signature) = api.signature.as_deref() else {
    return (true, true);
  };
  let Some((param_count, has_rest)) = parse_signature_params(signature) else {
    return (true, true);
  };
  if has_rest {
    return (true, true);
  }
  (
    param_count >= index_arg_position as usize + 1,
    param_count >= array_arg_position as usize + 1,
  )
}

fn parse_signature_params(signature: &str) -> Option<(usize, bool)> {
  let start = signature.find('(')?;
  let mut angle = 0u32;
  let mut paren = 0u32;
  let mut brace = 0u32;
  let mut bracket = 0u32;

  let mut count = 0usize;
  let mut saw_any = false;
  let mut has_rest = false;
  let mut param_start = None::<usize>;

  let bytes = signature.as_bytes();
  let mut i = start + 1;
  while i < bytes.len() {
    let c = bytes[i] as char;
    match c {
      '<' => angle += 1,
      '>' if angle > 0 => angle -= 1,
      '(' => paren += 1,
      ')' if paren > 0 => paren -= 1,
      '{' => brace += 1,
      '}' if brace > 0 => brace -= 1,
      '[' => bracket += 1,
      ']' if bracket > 0 => bracket -= 1,
      ')' if angle == 0 && paren == 0 && brace == 0 && bracket == 0 => break,
      ',' if angle == 0 && paren == 0 && brace == 0 && bracket == 0 => {
        if let Some(start) = param_start.take() {
          let end = i;
          let text = signature[start..end].trim();
          if !text.is_empty() {
            saw_any = true;
            count += 1;
            if text.starts_with("...") {
              has_rest = true;
            }
          }
        }
      }
      _ => {
        if param_start.is_none() && !c.is_whitespace() {
          param_start = Some(i);
        }
      }
    }
    i += 1;
  }

  if let Some(start) = param_start {
    let end = i;
    let text = signature[start..end].trim();
    if !text.is_empty() {
      saw_any = true;
      count += 1;
      if text.starts_with("...") {
        has_rest = true;
      }
    }
  }

  if !saw_any {
    count = 0;
  }
  Some((count, has_rest))
}
#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::Purity;

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

  fn first_callback_arg(body: &hir_js::Body, expr: ExprId) -> ExprId {
    let expr = body.exprs.get(expr.0 as usize).expect("expr");
    match &expr.kind {
      ExprKind::Call(call) => call.args.first().expect("callback arg").expr,
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { callback, .. }
      | ExprKind::ArrayFilter { callback, .. }
      | ExprKind::ArrayReduce { callback, .. }
      | ExprKind::ArrayFind { callback, .. }
      | ExprKind::ArrayEvery { callback, .. }
      | ExprKind::ArraySome { callback, .. } => *callback,
      other => panic!("expected call-like expr, got {other:?}"),
    }
  }

  #[test]
  fn callback_map_is_pure_and_does_not_use_index() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => x + 1);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Pure);
    assert!(!cb.uses_index);
  }

  #[test]
  fn callback_map_uses_index() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, i) => i);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.uses_index);
  }

  #[test]
  fn callsite_info_callback_fetch_effects_are_recorded() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(() => fetch('https://example.com'));",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert!(info
      .callback_effects
      .is_some_and(|e| e.contains(EffectSet::IO)));
    assert!(info
      .callback_effects
      .is_some_and(|e| e.contains(EffectSet::NETWORK)));
    assert_eq!(info.callback_may_throw, Some(true));
  }

  #[test]
  fn callback_destructuring_index_param_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, [i]) => x);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn callback_rest_index_param_counts_as_index_and_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, ...rest) => rest);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_object_rest_param_is_allocating() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(({ ...rest }) => rest);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Allocating);
    assert!(cb.effects.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn callback_array_rest_param_is_allocating() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(([x, ...rest]) => rest);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Allocating);
    assert!(cb.effects.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn callback_destructuring_array_param_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i, { length }) => length);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_counts_as_index_and_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[1]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_array_slot_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[2]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_numeric_literal_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[1e0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_hex_literal_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[0x2]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_bigint_literal_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[1n]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_bigint_literal_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[2n]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_null_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[null]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_boolean_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[true]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_unary_plus_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[+1]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_unary_plus_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[+2]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_unary_minus_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[-1]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_unary_minus_zero_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[-0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_unary_not_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[!0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_void_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[void 0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_typeof_key_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[typeof 0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_value_slot_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_length_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments.length; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_length_computed_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[\"length\"]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_length_template_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[`length`]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_numeric_template_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[`1`]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_array_template_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[`2`]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn callback_using_arguments_other_template_slot_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[`3`]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_other_slot_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[3]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_using_arguments_other_slot_computed_does_not_count_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { return arguments[\"3\"]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_shadowing_arguments_does_not_count_as_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x) { let arguments = []; return arguments[0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_arguments_param_does_not_count_as_index_or_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (arguments) { return arguments[0]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_named_arguments_function_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function arguments(x) { return arguments[1]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn callback_reference_to_known_api_is_modeled() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(Math.sqrt);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Pure);
    assert!(cb.effects.contains(EffectSet::MAY_THROW));
    assert!(!cb.uses_index);
    assert!(!cb.uses_array);
  }

  #[test]
  fn callback_inline_call_to_pure_builtin_is_modeled() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => Math.sqrt(x));")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Pure);
    assert!(cb.effects.contains(EffectSet::MAY_THROW));
    assert!(
      !cb.effects.contains(EffectSet::UNKNOWN),
      "expected Math.sqrt call to resolve via KB"
    );
  }

  #[test]
  fn callback_does_not_trust_shadowed_pure_builtin() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, Math) => Math.sqrt(x));",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::UNKNOWN));
    assert_eq!(cb.purity, Purity::Impure);
  }

  #[test]
  fn callback_reference_to_known_variadic_api_marks_index_and_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(Math.min);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.uses_index);
    assert!(cb.uses_array);
  }

  #[test]
  fn callback_models_kb_getter_effects() {
    use effect_model::{EffectTemplate, PurityTemplate};
    use knowledge_base::{ApiDatabase, ApiId, ApiKind, ApiSemantics};
    use std::collections::BTreeMap;

    let kb = ApiDatabase::from_entries([
      ApiSemantics {
        id: ApiId::from_name("Foo"),
        name: "Foo".to_string(),
        aliases: Vec::new(),
        effects: EffectTemplate::Custom(EffectSet::ALLOCATES),
        effect_summary: EffectSet::ALLOCATES.to_effect_summary(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        kind: ApiKind::Constructor,
        properties: BTreeMap::new(),
      },
      ApiSemantics {
        id: ApiId::from_name("Foo.prototype.bar"),
        name: "Foo.prototype.bar".to_string(),
        aliases: Vec::new(),
        effects: EffectTemplate::Custom(EffectSet::IO),
        effect_summary: EffectSet::IO.to_effect_summary(),
        purity: PurityTemplate::Impure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        kind: ApiKind::Getter,
        properties: BTreeMap::new(),
      },
    ]);

    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(() => new Foo().bar);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::IO));
  }

  #[test]
  fn callback_calling_fetch_is_impure() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(url => fetch(url));")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Impure);
    assert!(cb.effects.contains(EffectSet::IO));
    assert!(cb.effects.contains(EffectSet::NETWORK));
  }

  #[test]
  fn callback_default_param_calling_fetch_is_impure() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i, a, d = fetch(\"x\")) => x);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Impure);
    assert!(cb.effects.contains(EffectSet::IO));
    assert!(cb.effects.contains(EffectSet::NETWORK));
  }

  #[test]
  fn callback_throwing_sets_may_throw() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(x => { throw new Error(); });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn callback_delete_is_impure_and_unknown() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => delete x.foo);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert_eq!(cb.purity, Purity::Impure);
    assert!(cb.effects.contains(EffectSet::UNKNOWN));
    assert!(cb.effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn callsite_info_includes_index_and_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, i, a) => a);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn eval_callsite_info_includes_callback_effects_and_purity() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, i, a) => a);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = eval_callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.arg_purity.get(0).copied(), Some(Purity::Pure));
    assert_eq!(info.arg_effects.get(0).copied(), Some(EffectSet::empty()));
    assert!(!info.callback_uses_index);
    assert!(info.callback_uses_array);
  }

  #[test]
  fn nested_function_does_not_count_for_param_usage() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, i, a) => () => a);")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn block_shadow_does_not_count_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { { let i = 0; return i; } });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
  }

  #[test]
  fn function_decl_shadow_does_not_count_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { { function i() { return 0; } return i(); } });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
  }

  #[test]
  fn class_decl_shadow_does_not_count_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { { class i {} return i; } });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
  }

  #[test]
  fn var_decl_does_not_shadow_index_param_when_params_are_simple() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { var i; return i; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn var_decl_does_not_shadow_index_param_when_params_are_not_simple() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i = 0) => { var i; return i; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn var_decl_does_not_shadow_arguments_object_when_params_are_not_simple() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(function (x, i = 0) { var arguments; return arguments[1]; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn for_loop_shadow_does_not_count_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { for (let i = 0; i < 1; i++) {} return x; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
  }

  #[test]
  fn for_of_shadow_does_not_hide_right_expr_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { for (let i of [i]) {} return x; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn for_in_shadow_does_not_hide_right_expr_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { for (let i in { [i]: 1 }) {} return x; });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn catch_param_shadow_does_not_count_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { try { throw 1; } catch (i) { return i; } });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(false));
  }

  #[test]
  fn switch_case_shadow_does_not_hide_discriminant_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i) => { switch (i) { case 0: let i = 0; return i; } });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_uses_index, Some(true));
  }

  #[test]
  fn reduce_callback_second_param_does_not_count_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.reduce((a, b) => b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn reduce_callback_third_param_counts_as_index_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.reduce((a, b, i) => i);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_uses_index, Some(true));
    assert_eq!(info.callback_uses_array, Some(false));
  }

  #[test]
  fn reduce_callback_fourth_param_counts_as_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.reduce((a, b, i, arr) => arr.length);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_uses_index, Some(false));
    assert_eq!(info.callback_uses_array, Some(true));
  }

  #[test]
  fn infers_associative_reduce_callback_for_bigint_add() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: bigint, b: bigint) => a + b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_is_associative, Some(true));
  }

  #[test]
  fn infers_associative_reduce_callback_for_number_bitwise_or() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: number, b: number) => a | b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_is_associative, Some(true));
  }

  #[test]
  fn infers_associative_reduce_callback_for_number_bitwise_or_swapped_operands() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: number, b: number) => b | a);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_is_associative, Some(true));
  }

  #[test]
  fn infers_associative_reduce_callback_for_boolean_and() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: boolean, b: boolean) => a && b);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_is_associative, Some(true));
  }

  #[test]
  fn infers_associative_reduce_callback_for_boolean_and_swapped_operands() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Ts,
      "arr.reduce((a: boolean, b: boolean) => b && a);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let info = callsite_info_for_args(&lowered, body, call_expr, &kb);

    assert_eq!(info.callback_is_pure, Some(true));
    assert_eq!(info.callback_is_associative, Some(true));
  }

  #[test]
  fn callback_calling_date_now_is_nondeterministic() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(() => Date.now());")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::NONDETERMINISTIC));
  }

  #[test]
  fn callback_unknown_call_is_unknown_purity() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => foo(x));").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::UNKNOWN));
    assert_eq!(cb.purity, Purity::Impure);
  }

  #[cfg(feature = "hir-semantic-ops")]
  #[test]
  fn callback_models_nested_array_semantic_ops() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(() => arr2.map(x => x + 1));",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(
      !cb.effects.contains(EffectSet::UNKNOWN),
      "expected nested semantic ops to be modeled, got {:?}",
      cb.effects
    );
    assert_eq!(cb.purity, Purity::Allocating);
  }

  #[cfg(feature = "hir-semantic-ops")]
  #[test]
  fn callback_models_nested_array_chain_semantic_ops() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(() => arr2.map(x => x).filter(y => y));",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(
      !cb.effects.contains(EffectSet::UNKNOWN),
      "expected ArrayChain semantic op to be modeled, got {:?}",
      cb.effects
    );
    assert_eq!(cb.purity, Purity::Allocating);
  }

  #[test]
  fn depends_on_args_respects_api_effect_summary_base_flags() {
    use effect_model::{EffectTemplate, PurityTemplate};
    use knowledge_base::{ApiDatabase, ApiId, ApiKind, ApiSemantics};
    use std::collections::BTreeMap;

    let kb = ApiDatabase::from_entries([ApiSemantics {
      id: ApiId::from_name("cbApi"),
      name: "cbApi".to_string(),
      aliases: Vec::new(),
      effects: EffectTemplate::DependsOnArgs {
        base: EffectSet::empty(),
        args: vec![0],
      },
      effect_summary: EffectSet::NONDETERMINISTIC.to_effect_summary(),
      purity: PurityTemplate::DependsOnArgs {
        base: Purity::Pure,
        args: vec![0],
      },
      async_: None,
      idempotent: None,
      deterministic: None,
      parallelizable: None,
      semantics: None,
      signature: None,
      since: None,
      until: None,
      kind: ApiKind::Function,
      properties: BTreeMap::new(),
    }]);

    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(() => cbApi(() => 1));")
        .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::NONDETERMINISTIC));
  }
}
