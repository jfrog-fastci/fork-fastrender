use effect_model::{EffectFlags, EffectSummary, Purity, ThrowBehavior};
use hir_js::{
  ArrayElement, Body, BodyId, ExprId, ExprKind, ForHead, ForInit, FunctionBody, NameId, ObjectKey,
  ObjectProperty, PatId, PatKind, StmtId, StmtKind, VarDecl,
};
use knowledge_base::KnowledgeBase;

use crate::template_eval::eval_call_expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackInfo {
  pub effects: EffectSummary,
  pub purity: Purity,
  pub uses_index: bool,
  pub uses_array: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallSiteInfo {
  pub callback: Option<CallbackInfo>,
}

pub fn callsite_info_for_args(
  lowered: &hir_js::LowerResult,
  body: BodyId,
  call_expr: ExprId,
  kb: &KnowledgeBase,
) -> CallSiteInfo {
  let Some(body_ref) = lowered.body(body) else {
    return CallSiteInfo::default();
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return CallSiteInfo::default();
  };
  let ExprKind::Call(call) = &expr.kind else {
    return CallSiteInfo::default();
  };

  let callback = call
    .args
    .first()
    .filter(|arg| !arg.spread)
    .and_then(|arg| analyze_inline_callback(lowered, body, arg.expr, kb));

  CallSiteInfo { callback }
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
    return None;
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
    effects: EffectSummary::PURE,
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

struct CallbackAnalyzer<'a> {
  lowered: &'a hir_js::LowerResult,
  kb: &'a KnowledgeBase,
  body: BodyId,
  index_param: Option<NameId>,
  array_param: Option<NameId>,
  uses_index: bool,
  uses_array: bool,
  effects: EffectSummary,
  purity: Purity,
}

impl CallbackAnalyzer<'_> {
  fn merge_effects(&mut self, other: EffectSummary) {
    self.effects = EffectSummary::join(self.effects, other);
  }

  fn merge_purity(&mut self, other: Purity) {
    self.purity = Purity::join(self.purity, other);
  }

  fn mark_unknown(&mut self) {
    self.merge_effects(unknown_effects());
    self.merge_purity(Purity::Unknown);
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
        self.effects.flags |= EffectFlags::ALLOCATES;
      }
      StmtKind::Return(expr) => {
        if let Some(expr) = expr {
          self.visit_expr(body, *expr);
        }
      }
      StmtKind::Block(stmts) => {
        for stmt in stmts {
          self.visit_stmt(body, *stmt);
        }
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
      }
      StmtKind::ForIn { left, right, body: inner, .. } => {
        match left {
          ForHead::Pat(pat) => self.visit_assign_pat(body, *pat),
          ForHead::Var(var) => self.visit_var_decl(body, var),
        }
        self.visit_expr(body, *right);
        self.visit_stmt(body, *inner);
      }
      StmtKind::Switch { discriminant, cases } => {
        self.visit_expr(body, *discriminant);
        for case in cases {
          if let Some(test) = case.test {
            self.visit_expr(body, test);
          }
          for stmt in &case.consequent {
            self.visit_stmt(body, *stmt);
          }
        }
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        self.visit_stmt(body, *block);
        if let Some(catch) = catch {
          if let Some(param) = catch.param {
            self.visit_binding_pat(body, param);
          }
          self.visit_stmt(body, catch.body);
        }
        if let Some(finally_block) = finally_block {
          self.visit_stmt(body, *finally_block);
        }
      }
      StmtKind::Throw(expr) => {
        self.effects.throws = ThrowBehavior::join(self.effects.throws, ThrowBehavior::Always);
        self.merge_purity(Purity::Impure);
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
      PatKind::AssignTarget(expr) => self.visit_expr(body, *expr),
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
        self.effects.flags |= EffectFlags::ALLOCATES;
        for elem in &array.elements {
          match elem {
            ArrayElement::Expr(expr) | ArrayElement::Spread(expr) => self.visit_expr(body, *expr),
            ArrayElement::Empty => {}
          }
        }
      }

      ExprKind::Object(obj) => {
        self.effects.flags |= EffectFlags::ALLOCATES;
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
        self.effects.flags |= EffectFlags::ALLOCATES;
      }

      ExprKind::Template(template) => {
        self.effects.flags |= EffectFlags::ALLOCATES;
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
        self.effects.flags |= EffectFlags::ALLOCATES;
      }
    }
  }

  fn record_ident(&mut self, name: NameId) {
    if Some(name) == self.index_param {
      self.uses_index = true;
    }
    if Some(name) == self.array_param {
      self.uses_array = true;
    }
  }
}

fn unknown_effects() -> EffectSummary {
  EffectSummary {
    flags: EffectFlags::all(),
    throws: ThrowBehavior::Maybe,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::{Purity, ThrowBehavior};

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
  fn callback_map_is_pure_and_does_not_use_index() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map(x => x + 1);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = callsite_info_for_args(&lowered, body, call_expr, &kb);
    let cb = callsite.callback.expect("callback");

    assert_eq!(cb.purity, Purity::Pure);
    assert!(!cb.uses_index);
  }

  #[test]
  fn callback_map_uses_index() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(hir_js::FileKind::Js, "arr.map((x, i) => i);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = callsite_info_for_args(&lowered, body, call_expr, &kb);
    let cb = callsite.callback.expect("callback");

    assert!(cb.uses_index);
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
    let callsite = callsite_info_for_args(&lowered, body, call_expr, &kb);
    let cb = callsite.callback.expect("callback");

    assert_eq!(cb.purity, Purity::Impure);
    assert!(cb.effects.flags.contains(EffectFlags::IO));
    assert!(cb.effects.flags.contains(EffectFlags::NETWORK));
  }

  #[test]
  fn callback_throwing_always_throws() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      hir_js::FileKind::Js,
      "arr.map(x => { throw new Error(); });",
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);
    let callsite = callsite_info_for_args(&lowered, body, call_expr, &kb);
    let cb = callsite.callback.expect("callback");

    assert_eq!(cb.effects.throws, ThrowBehavior::Always);
  }
}

