use effect_model::{EffectSet, Purity};
use hir_js::{
  ArrayElement, BinaryOp, Body, BodyId, ExprId, ExprKind, ForHead, ForInit, FunctionBody, NameId,
  ObjectKey, ObjectProperty, PatId, PatKind, StmtId, StmtKind, TypeArenas, TypeExprId, TypeExprKind,
  VarDecl, VarDeclKind,
};
use hir_js::hir::SwitchCase;
#[cfg(feature = "hir-semantic-ops")]
use hir_js::ArrayChainOp;
use knowledge_base::{ApiKind, KnowledgeBase};

use crate::api_use::{resolve_api_use, ApiUseKind};
use crate::eval::eval_api_call;
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
  let Some(body_ref) = lowered.body(body) else {
    return crate::db::CallSiteInfo::default();
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return crate::db::CallSiteInfo::default();
  };
  let callback_expr = match &expr.kind {
    ExprKind::Call(call) => call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { callback, .. }
    | ExprKind::ArrayFilter { callback, .. }
    | ExprKind::ArrayReduce { callback, .. }
    | ExprKind::ArrayFind { callback, .. }
    | ExprKind::ArrayEvery { callback, .. }
    | ExprKind::ArraySome { callback, .. } => Some(*callback),
    _ => None,
  };

  let Some(callback_expr) = callback_expr else {
    return crate::db::CallSiteInfo::default();
  };
  let callback = analyze_inline_callback(lowered, body, callback_expr, kb);
  let associative = callback.and_then(|_| infer_associative_inline_callback(lowered, body, callback_expr));
  crate::db::CallSiteInfo {
    callback_is_pure: callback.map(|cb| matches!(cb.purity, Purity::Pure | Purity::Allocating)),
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
  let Some(body_ref) = lowered.body(body) else {
    return crate::eval::CallSiteInfo::default();
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return crate::eval::CallSiteInfo::default();
  };
  let callback_expr = match &expr.kind {
    ExprKind::Call(call) => call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr),
    #[cfg(feature = "hir-semantic-ops")]
    ExprKind::ArrayMap { callback, .. }
    | ExprKind::ArrayFilter { callback, .. }
    | ExprKind::ArrayReduce { callback, .. }
    | ExprKind::ArrayFind { callback, .. }
    | ExprKind::ArrayEvery { callback, .. }
    | ExprKind::ArraySome { callback, .. } => Some(*callback),
    _ => None,
  };

  let Some(callback_expr) = callback_expr else {
    return crate::eval::CallSiteInfo::default();
  };
  let callback = analyze_inline_callback(lowered, body, callback_expr, kb);

  crate::eval::CallSiteInfo {
    callback_purity: callback.map(|cb| cb.purity),
    callback_effects: callback.map(|cb| cb.effects),
    callback_uses_index: callback.map(|cb| cb.uses_index).unwrap_or(false),
    callback_uses_array: callback.map(|cb| cb.uses_array).unwrap_or(false),
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
  let callsite_body = lowered.body(body)?;
  let cb_expr = callsite_body.exprs.get(callback_expr.0 as usize)?;

  let ExprKind::FunctionExpr { body: cb_body, .. } = cb_expr.kind else {
    return analyze_known_callback_reference(lowered, callsite_body, callback_expr, kb);
  };

  let cb_body_data = lowered.body(cb_body)?;
  let func = cb_body_data.function.as_ref()?;

  let index_param = func.params.get(1).and_then(|param| {
    let pat = cb_body_data.pats.get(param.pat.0 as usize)?;
    match pat.kind {
      PatKind::Ident(name) => Some(name),
      _ => None,
    }
  });

  let array_param = func.params.get(2).and_then(|param| {
    let pat = cb_body_data.pats.get(param.pat.0 as usize)?;
    match pat.kind {
      PatKind::Ident(name) => Some(name),
      _ => None,
    }
  });

  let mut analyzer = CallbackAnalyzer {
    lowered,
    kb,
    body: cb_body,
    index_param,
    array_param,
    uses_index: false,
    uses_array: false,
    shadow_stack: vec![ShadowScope::default()],
    effects: EffectSet::empty(),
    purity: Purity::Pure,
  };

  match &func.body {
    FunctionBody::Block(stmts) => {
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
  kb: &KnowledgeBase,
) -> Option<CallbackInfo> {
  let resolved = resolve_api_use(
    lowered.hir.as_ref(),
    body,
    callback_expr,
    lowered.names.as_ref(),
    kb,
  )?;
  if resolved.kind != ApiUseKind::Value {
    return None;
  }

  let api = kb.get_by_id(resolved.api)?;
  if !matches!(api.kind, ApiKind::Function | ApiKind::Constructor) {
    return None;
  }

  let sem = eval_api_call(api, &crate::eval::CallSiteInfo::default());
  Some(CallbackInfo {
    effects: sem.effects,
    purity: sem.purity,
    uses_index: true,
    uses_array: true,
  })
}

struct CallbackAnalyzer<'a> {
  lowered: &'a hir_js::LowerResult,
  kb: &'a KnowledgeBase,
  body: BodyId,
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
}

impl CallbackAnalyzer<'_> {
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

  fn scan_shadow_in_stmts(&self, body: &Body, stmts: &[StmtId]) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if self.index_param.is_none() && self.array_param.is_none() {
      return scope;
    }
    let index = self.index_param;
    let array = self.array_param;
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
            if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, index)) {
              scope.shadow_index = true;
            }
          }
          if let Some(array) = array {
            if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, array)) {
              scope.shadow_array = true;
            }
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
        }
        _ => continue,
      };
    }
    scope
  }

  fn scan_shadow_in_switch(&self, body: &Body, cases: &[SwitchCase]) -> ShadowScope {
    let mut scope = ShadowScope::default();
    if self.index_param.is_none() && self.array_param.is_none() {
      return scope;
    }
    let index = self.index_param;
    let array = self.array_param;
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
              if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, index)) {
                scope.shadow_index = true;
              }
            }
            if let Some(array) = array {
              if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, array)) {
                scope.shadow_array = true;
              }
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
          }
          _ => continue,
        }
      }
    }
    scope
  }

  fn scan_shadow_in_var_decl(&self, body: &Body, var: &VarDecl) -> ShadowScope {
    let mut scope = ShadowScope::default();
    let Some(index) = self.index_param else {
      if let Some(array) = self.array_param {
        if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, array)) {
          scope.shadow_array = true;
        }
      }
      return scope;
    };

    if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, index)) {
      scope.shadow_index = true;
    }
    if let Some(array) = self.array_param {
      if var.declarators.iter().any(|d| pat_binds_name(body, d.pat, array)) {
        scope.shadow_array = true;
      }
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
    scope
  }

  fn name_is_shadowed(&self, name: NameId) -> bool {
    if Some(name) == self.index_param {
      return self.shadow_stack.iter().any(|s| s.shadow_index);
    }
    if Some(name) == self.array_param {
      return self.shadow_stack.iter().any(|s| s.shadow_array);
    }
    false
  }

  fn visit_stmt(&mut self, body: &Body, stmt_id: StmtId) {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      return;
    };

    match &stmt.kind {
      StmtKind::Expr(expr) => self.visit_expr(body, *expr),
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
            self.shadow_stack.push(self.scan_shadow_in_var_decl(body, var));
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
      StmtKind::ForIn { left, right, body: inner, .. } => {
        let mut pushed = false;
        if let ForHead::Var(var) = left {
          if is_lexical_var_decl(var) {
            self.shadow_stack.push(self.scan_shadow_in_var_decl(body, var));
            pushed = true;
          }
        }
        match left {
          ForHead::Pat(pat) => self.visit_assign_pat(body, *pat),
          ForHead::Var(var) => self.visit_var_decl(body, var),
        }
        self.visit_expr(body, *right);
        self.visit_stmt(body, *inner);
        if pushed {
          self.shadow_stack.pop();
        }
      }
      StmtKind::Switch { discriminant, cases } => {
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
      StmtKind::Break(_)
      | StmtKind::Continue(_)
      | StmtKind::Debugger
      | StmtKind::Empty => {}
      StmtKind::Var(var) => self.visit_var_decl(body, var),
      StmtKind::Labeled { body: inner, .. } => self.visit_stmt(body, *inner),
      StmtKind::With { object, body: inner } => {
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
          self.visit_binding_pat(body, rest);
        }
      }
      PatKind::Rest(rest) => self.visit_binding_pat(body, **rest),
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
        self.visit_expr(body, mem.object);
        if let ObjectKey::Computed(expr) = &mem.property {
          self.visit_expr(body, *expr);
        }

        if let Some(resolved) = resolve_api_use(
          self.lowered.hir.as_ref(),
          body,
          expr_id,
          self.lowered.names.as_ref(),
          self.kb,
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

      // Semantic ops are higher-level "call-like" expressions introduced by
      // `hir-js/semantic-ops`. Until we have full modeling for them in `effect-js`,
      // treat them conservatively as unknown effects while still visiting their
      // child expressions for identifier tracking.
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { array, callback }
      | ExprKind::ArrayFilter { array, callback }
      | ExprKind::ArrayFind { array, callback }
      | ExprKind::ArrayEvery { array, callback }
      | ExprKind::ArraySome { array, callback } => {
        self.mark_unknown();
        self.visit_expr(body, *array);
        self.visit_expr(body, *callback);
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        self.mark_unknown();
        self.visit_expr(body, *array);
        self.visit_expr(body, *callback);
        if let Some(init) = init {
          self.visit_expr(body, *init);
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        self.mark_unknown();
        self.visit_expr(body, *array);
        for op in ops {
          match op {
            ArrayChainOp::Map(callback)
            | ArrayChainOp::Filter(callback)
            | ArrayChainOp::Find(callback)
            | ArrayChainOp::Every(callback)
            | ArrayChainOp::Some(callback) => {
              self.visit_expr(body, *callback);
            }
            ArrayChainOp::Reduce(callback, init) => {
              self.visit_expr(body, *callback);
              if let Some(init) = init {
                self.visit_expr(body, *init);
              }
            }
          }
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        self.mark_unknown();
        for promise in promises {
          self.visit_expr(body, *promise);
        }
      }
      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::KnownApiCall { args, .. } => {
        self.mark_unknown();
        for arg in args {
          self.visit_expr(body, *arg);
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

      ExprKind::TypeAssertion { expr, .. } | ExprKind::Satisfies { expr, .. } => {
        self.visit_expr(body, *expr);
      }

      ExprKind::ImportCall { argument, attributes } => {
        self.mark_unknown();
        self.visit_expr(body, *argument);
        if let Some(attributes) = attributes {
          self.visit_expr(body, *attributes);
        }
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

  fn record_ident(&mut self, name: NameId) {
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
  }

  #[test]
  fn callback_models_kb_getter_effects() {
    use effect_model::{EffectSummary, EffectTemplate, PurityTemplate, ThrowBehavior};
    use knowledge_base::{ApiDatabase, ApiId, ApiKind, ApiSemantics};
    use std::collections::BTreeMap;

    let kb = ApiDatabase::from_entries([
      ApiSemantics {
        id: ApiId::from_name("Foo"),
        name: "Foo".to_string(),
        aliases: Vec::new(),
        effects: EffectTemplate::Custom(EffectSet::ALLOCATES),
        effect_summary: EffectSummary {
          flags: EffectSet::ALLOCATES,
          throws: ThrowBehavior::Never,
        },
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
        effect_summary: EffectSummary {
          flags: EffectSet::IO,
          throws: ThrowBehavior::Never,
        },
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

    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(() => new Foo().bar);",
    )
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
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(url => fetch(url));",
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
  fn callsite_info_includes_index_and_array_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i, a) => a);",
    )
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
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i, a) => a);",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let info = eval_callsite_info_for_args(&lowered, body, call_expr, &kb);
    assert_eq!(info.callback_purity, Some(Purity::Pure));
    assert_eq!(info.callback_effects, Some(EffectSet::empty()));
    assert!(!info.callback_uses_index);
    assert!(info.callback_uses_array);
  }

  #[test]
  fn nested_function_does_not_count_for_param_usage() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map((x, i, a) => () => a);",
    )
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

  #[test]
  fn depends_on_args_respects_api_effect_summary_base_flags() {
    use effect_model::{EffectSummary, EffectTemplate, PurityTemplate, ThrowBehavior};
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
      effect_summary: EffectSummary {
        flags: EffectSet::NONDETERMINISTIC,
        throws: ThrowBehavior::Never,
      },
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

    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(() => cbApi(() => 1));",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let body_ref = lowered.body(body).unwrap();
    let cb_expr = first_callback_arg(body_ref, call_expr);
    let cb = analyze_inline_callback(&lowered, body, cb_expr, &kb).expect("callback");

    assert!(cb.effects.contains(EffectSet::NONDETERMINISTIC));
  }
}
