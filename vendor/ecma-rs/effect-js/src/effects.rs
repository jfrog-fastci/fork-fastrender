use std::collections::HashMap;

use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};
use hir_js::{
  ArrayElement, ArrayLiteral, Body, BodyId, CallExpr, Expr, ExprId, ExprKind, LowerResult, MemberExpr,
  ObjectKey, ObjectLiteral, ObjectProperty,
};
use knowledge_base::{Api, KnowledgeBase};

use crate::callback::analyze_inline_callback;
use crate::eval::{eval_api_call, CallSiteInfo as EvalCallSiteInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectsTables {
  pub effects_by_expr: Vec<EffectSet>,
  pub purity_by_expr: Vec<Purity>,
}

pub fn analyze_effects_untyped(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
) -> HashMap<BodyId, EffectsTables> {
  analyze_effects_inner(kb, lowered, None)
}

#[cfg(feature = "typed")]
pub fn analyze_effects_typed(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  types: &dyn crate::types::TypeProvider,
) -> HashMap<BodyId, EffectsTables> {
  analyze_effects_inner(kb, lowered, Some(types))
}

fn analyze_effects_inner(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  #[cfg(feature = "typed")] types: Option<&dyn crate::types::TypeProvider>,
  #[cfg(not(feature = "typed"))] _types: Option<&()>,
) -> HashMap<BodyId, EffectsTables> {
  let mut out = HashMap::new();
  for (&body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(body_id) else {
      continue;
    };
    let mut analyzer = EffectsAnalyzer::new(kb, lowered, body_id, body);
    #[cfg(feature = "typed")]
    {
      analyzer.types = types;
    }
    analyzer.compute_all();
    out.insert(
      body_id,
      EffectsTables {
        effects_by_expr: analyzer.effects_by_expr,
        purity_by_expr: analyzer.purity_by_expr,
      },
    );
  }
  out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
  NotVisited,
  Visiting,
  Done,
}

struct EffectsAnalyzer<'a> {
  kb: &'a KnowledgeBase,
  lowered: &'a LowerResult,
  body_id: BodyId,
  body: &'a Body,
  #[cfg(feature = "typed")]
  types: Option<&'a dyn crate::types::TypeProvider>,
  effects_by_expr: Vec<EffectSet>,
  purity_by_expr: Vec<Purity>,
  state: Vec<VisitState>,
}

impl<'a> EffectsAnalyzer<'a> {
  fn new(kb: &'a KnowledgeBase, lowered: &'a LowerResult, body_id: BodyId, body: &'a Body) -> Self {
    let exprs_len = body.exprs.len();
    Self {
      kb,
      lowered,
      body_id,
      body,
      #[cfg(feature = "typed")]
      types: None,
      effects_by_expr: vec![EffectSet::UNKNOWN_CALL; exprs_len],
      purity_by_expr: vec![Purity::Impure; exprs_len],
      state: vec![VisitState::NotVisited; exprs_len],
    }
  }

  fn compute_all(&mut self) {
    for idx in 0..self.body.exprs.len() {
      let _ = self.analyze_expr(ExprId(idx as u32));
    }
  }

  fn analyze_expr(&mut self, expr_id: ExprId) -> (EffectSet, Purity) {
    let idx = expr_id.0 as usize;
    if idx >= self.state.len() {
      return (EffectSet::UNKNOWN_CALL, Purity::Impure);
    }
    match self.state[idx] {
      VisitState::Done => return (self.effects_by_expr[idx], self.purity_by_expr[idx]),
      VisitState::Visiting => return (EffectSet::UNKNOWN_CALL, Purity::Impure),
      VisitState::NotVisited => {}
    }
    self.state[idx] = VisitState::Visiting;

    let Some(expr) = self.body.exprs.get(idx) else {
      self.effects_by_expr[idx] = EffectSet::UNKNOWN_CALL;
      self.purity_by_expr[idx] = Purity::Impure;
      self.state[idx] = VisitState::Done;
      return (self.effects_by_expr[idx], self.purity_by_expr[idx]);
    };

    let (effects, mut purity) = self.analyze_expr_kind(expr_id, expr);
    purity = Purity::join(purity, effects.inferred_purity());

    self.effects_by_expr[idx] = effects;
    self.purity_by_expr[idx] = purity;
    self.state[idx] = VisitState::Done;
    (effects, purity)
  }

  fn analyze_expr_kind(&mut self, expr_id: ExprId, expr: &Expr) -> (EffectSet, Purity) {
    match &expr.kind {
      ExprKind::Missing => (EffectSet::UNKNOWN_CALL, Purity::Impure),

      ExprKind::Ident(_) | ExprKind::This | ExprKind::Literal(_) => (EffectSet::empty(), Purity::Pure),

      // These are runtime values but do not perform observable effects on their own.
      ExprKind::Super | ExprKind::ImportMeta | ExprKind::NewTarget => (EffectSet::empty(), Purity::Pure),

      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Satisfies { expr, .. } => self.analyze_expr(*expr),

      ExprKind::Unary { op, expr } => {
        let (child_effects, child_purity) = self.analyze_expr(*expr);
        match op {
          hir_js::UnaryOp::Delete => (child_effects | EffectSet::UNKNOWN_CALL, Purity::Impure),
          _ => (child_effects, child_purity),
        }
      }

      ExprKind::Await { expr } => {
        let (child_effects, child_purity) = self.analyze_expr(*expr);
        (
          child_effects | EffectSet::NONDETERMINISTIC | EffectSet::MAY_THROW,
          child_purity,
        )
      }

      ExprKind::Update { expr, .. } => {
        let (child_effects, _child_purity) = self.analyze_expr(*expr);
        (child_effects | EffectSet::UNKNOWN_CALL, Purity::Impure)
      }

      ExprKind::Binary { left, right, .. } => {
        let (left_effects, left_purity) = self.analyze_expr(*left);
        let (right_effects, right_purity) = self.analyze_expr(*right);
        (
          left_effects | right_effects,
          Purity::join(left_purity, right_purity),
        )
      }

      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        // Short-circuiting: any branch may run; union conservatively.
        let (test_e, test_p) = self.analyze_expr(*test);
        let (cons_e, cons_p) = self.analyze_expr(*consequent);
        let (alt_e, alt_p) = self.analyze_expr(*alternate);
        (
          test_e | cons_e | alt_e,
          Purity::join(test_p, Purity::join(cons_p, alt_p)),
        )
      }

      ExprKind::Assignment { target, value, .. } => {
        // Treat assignments as impure (may throw + may have arbitrary effects).
        // Still include child effects for completeness.
        let (value_e, _value_p) = self.analyze_expr(*value);
        let (target_e, _target_p) = self.analyze_pat_eval(*target);
        (value_e | target_e | EffectSet::UNKNOWN_CALL, Purity::Impure)
      }

      ExprKind::Call(call) => self.analyze_call_expr(expr_id, call),

      ExprKind::Member(member) => self.analyze_member_expr(expr_id, member),

      ExprKind::Array(arr) => self.analyze_array_literal(arr),

      ExprKind::Object(obj) => self.analyze_object_literal(obj),

      ExprKind::FunctionExpr { .. } => (EffectSet::ALLOCATES, Purity::Allocating),

      ExprKind::ClassExpr { .. } => (EffectSet::UNKNOWN_CALL | EffectSet::ALLOCATES, Purity::Impure),

      ExprKind::Template(tmpl) => {
        let mut effects = EffectSet::ALLOCATES;
        let mut purity = Purity::Allocating;
        for span in &tmpl.spans {
          let (e, p) = self.analyze_expr(span.expr);
          effects |= e;
          purity = Purity::join(purity, p);
        }
        (effects, purity)
      }

      ExprKind::TaggedTemplate { tag, template } => {
        let (tag_e, tag_p) = self.analyze_expr(*tag);
        let (tmpl_e, tmpl_p) = self.analyze_template_literal(template);
        (
          tag_e | tmpl_e | EffectSet::UNKNOWN_CALL,
          Purity::join(tag_p, Purity::join(tmpl_p, Purity::Impure)),
        )
      }

      ExprKind::Yield { expr, .. } => {
        let (child_e, child_p) = expr.map(|e| self.analyze_expr(e)).unwrap_or((EffectSet::empty(), Purity::Pure));
        (child_e | EffectSet::NONDETERMINISTIC, child_p)
      }

      ExprKind::ImportCall { argument, attributes } => {
        let (arg_e, arg_p) = self.analyze_expr(*argument);
        let (attr_e, attr_p) = attributes
          .map(|e| self.analyze_expr(e))
          .unwrap_or((EffectSet::empty(), Purity::Pure));
        (
          arg_e | attr_e | EffectSet::UNKNOWN_CALL,
          Purity::join(arg_p, Purity::join(attr_p, Purity::Impure)),
        )
      }

      ExprKind::Jsx(jsx) => self.analyze_jsx(jsx),

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayMap { .. }
      | ExprKind::ArrayFilter { .. }
      | ExprKind::ArrayReduce { .. }
      | ExprKind::ArrayFind { .. }
      | ExprKind::ArrayEvery { .. }
      | ExprKind::ArraySome { .. }
      | ExprKind::PromiseAll { .. }
      | ExprKind::PromiseRace { .. }
      | ExprKind::KnownApiCall { .. } => self.analyze_semantic_call(expr_id),

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::ArrayChain { array, ops } => self.analyze_array_chain(*array, ops),

      #[cfg(feature = "hir-semantic-ops")]
      ExprKind::AwaitExpr { value, .. } => {
        let (child_effects, child_purity) = self.analyze_expr(*value);
        (
          child_effects | EffectSet::NONDETERMINISTIC | EffectSet::MAY_THROW,
          child_purity,
        )
      }
    }
  }

  fn analyze_call_expr(&mut self, expr_id: ExprId, call: &CallExpr) -> (EffectSet, Purity) {
    let mut effects = EffectSet::empty();
    let mut purity = Purity::Pure;

    // Evaluate callee and args.
    let (callee_e, callee_p) = self.analyze_expr(call.callee);
    effects |= callee_e;
    purity = Purity::join(purity, callee_p);

    for arg in &call.args {
      let (arg_e, arg_p) = self.analyze_expr(arg.expr);
      effects |= arg_e;
      purity = Purity::join(purity, arg_p);
      if arg.spread {
        effects |= EffectSet::UNKNOWN_CALL;
        purity = Purity::Impure;
      }
    }

    if call.optional || self.callee_is_optional_member(call.callee) {
      effects |= EffectSet::UNKNOWN_CALL;
      purity = Purity::Impure;
      return (effects, purity);
    }

    // Model the call itself.
    let (call_effects, call_purity) = if call.is_new {
      self.eval_new_call(expr_id, call)
    } else {
      self.eval_resolved_call(expr_id, call)
    };
    effects |= call_effects;
    purity = Purity::join(purity, call_purity);

    (effects, purity)
  }

  fn eval_resolved_call(&mut self, expr_id: ExprId, call: &CallExpr) -> (EffectSet, Purity) {
    let resolved = crate::resolve::resolve_call(
      self.lowered,
      self.body_id,
      self.body,
      expr_id,
      self.kb,
      self.types_opt(),
    );
    let Some(resolved) = resolved else {
      return (EffectSet::UNKNOWN_CALL, Purity::Impure);
    };
    let Some(api) = self.kb.get_by_id(resolved.api_id) else {
      return (EffectSet::UNKNOWN_CALL, Purity::Impure);
    };

    let site = self.build_callsite_info_from_call_args(api, &call.args);
    let sem = eval_api_call(api, &site);
    (sem.effects, sem.purity)
  }

  fn eval_new_call(&mut self, expr_id: ExprId, call: &CallExpr) -> (EffectSet, Purity) {
    // `resolve_call` is conservative and skips `new`, but `api_use` can still
    // resolve obvious constructor paths like `new URL(...)`.
    let mut effects = EffectSet::ALLOCATES;
    let mut purity = Purity::Allocating;

    let resolved = crate::api_use::resolve_api_use(
      self.lowered.hir.as_ref(),
      self.body,
      expr_id,
      self.lowered.names.as_ref(),
      self.kb,
    );

    if let Some(resolved) = resolved {
      if matches!(resolved.kind, crate::ApiUseKind::Construct) {
        if let Some(api) = self.kb.get_by_id(resolved.api) {
          let site = self.build_callsite_info_from_call_args(api, &call.args);
          let sem = eval_api_call(api, &site);
          effects |= sem.effects;
          purity = Purity::join(purity, sem.purity);
        } else {
          effects |= EffectSet::UNKNOWN_CALL;
          purity = Purity::Impure;
        }
      } else {
        effects |= EffectSet::UNKNOWN_CALL;
        purity = Purity::Impure;
      }
    } else {
      effects |= EffectSet::UNKNOWN_CALL;
      purity = Purity::Impure;
    }

    (effects, purity)
  }

  fn callee_is_optional_member(&self, callee: ExprId) -> bool {
    let callee = self.strip_value_wrappers(callee);
    let Some(node) = self.body.exprs.get(callee.0 as usize) else {
      return false;
    };
    matches!(&node.kind, ExprKind::Member(MemberExpr { optional: true, .. }))
  }

  fn analyze_member_expr(&mut self, expr_id: ExprId, member: &MemberExpr) -> (EffectSet, Purity) {
    let mut effects = EffectSet::empty();
    let mut purity = Purity::Pure;

    // Evaluate receiver + computed property key.
    let (obj_e, obj_p) = self.analyze_expr(member.object);
    effects |= obj_e;
    purity = Purity::join(purity, obj_p);
    if let ObjectKey::Computed(expr) = &member.property {
      let (key_e, key_p) = self.analyze_expr(*expr);
      effects |= key_e;
      purity = Purity::join(purity, key_p);
    }

    // Optional chaining: conservatively unknown.
    if member.optional {
      effects |= EffectSet::UNKNOWN_CALL;
      purity = Purity::Impure;
      return (effects, purity);
    }

    // Typed-only: resolve known property getters (e.g. `arr.length`).
    #[cfg(feature = "typed")]
    if let Some(types) = self.types {
      if let Some(resolved) = crate::resolve::resolve_member(self.kb, self.lowered, self.body_id, expr_id, types) {
        if let Some(api) = self.kb.get_by_id(resolved.api_id) {
          let sem = eval_api_call(api, &EvalCallSiteInfo::default());
          effects |= sem.effects;
          purity = Purity::join(purity, sem.purity);
          return (effects, purity);
        }
      }

      // Best-effort: treat proven prototype method values as pure.
      if let Some(prop) = self.static_object_key_name(&member.property) {
        if types.expr_is_array(self.body_id, member.object) {
          if self.kb.get(&format!("Array.prototype.{prop}")).is_some() {
            return (effects, purity);
          }
        }
        if types.expr_is_string(self.body_id, member.object) {
          if self.kb.get(&format!("String.prototype.{prop}")).is_some() {
            return (effects, purity);
          }
        }
        for ty in ["Map", "Set", "Promise", "URL"] {
          if types.expr_is_named_ref(self.body_id, member.object, ty) {
            if self.kb.get(&format!("{ty}.prototype.{prop}")).is_some() {
              return (effects, purity);
            }
          }
        }
      }
    }

    // Untyped (and typed fallback): resolve safe global static member paths.
    if let Some(api) = self.resolve_global_member_api(expr_id) {
      // `getter` entries model property access directly; for other kinds, treat
      // the member read as a pure reference.
      match api.kind {
        knowledge_base::ApiKind::Getter | knowledge_base::ApiKind::Value => {
          let sem = eval_api_call(api, &EvalCallSiteInfo::default());
          effects |= sem.effects;
          purity = Purity::join(purity, sem.purity);
        }
        _ => {}
      }
      return (effects, purity);
    }

    effects |= EffectSet::UNKNOWN_CALL;
    purity = Purity::Impure;
    (effects, purity)
  }

  fn analyze_array_literal(&mut self, arr: &ArrayLiteral) -> (EffectSet, Purity) {
    let mut effects = EffectSet::ALLOCATES;
    let mut purity = Purity::Allocating;
    for element in &arr.elements {
      match element {
        ArrayElement::Expr(expr) => {
          let (e, p) = self.analyze_expr(*expr);
          effects |= e;
          purity = Purity::join(purity, p);
        }
        ArrayElement::Spread(expr) => {
          let (e, p) = self.analyze_expr(*expr);
          effects |= e | EffectSet::UNKNOWN_CALL;
          purity = Purity::Impure;
          purity = Purity::join(purity, p);
        }
        ArrayElement::Empty => {}
      }
    }
    (effects, purity)
  }

  fn analyze_object_literal(&mut self, obj: &ObjectLiteral) -> (EffectSet, Purity) {
    let mut effects = EffectSet::ALLOCATES;
    let mut purity = Purity::Allocating;
    for prop in &obj.properties {
      match prop {
        ObjectProperty::KeyValue { key, value, .. } => {
          let (key_e, key_p) = self.analyze_object_key_eval(key);
          effects |= key_e;
          purity = Purity::join(purity, key_p);

          let (val_e, val_p) = self.analyze_expr(*value);
          effects |= val_e;
          purity = Purity::join(purity, val_p);
        }
        ObjectProperty::Getter { key, .. } | ObjectProperty::Setter { key, .. } => {
          let (key_e, key_p) = self.analyze_object_key_eval(key);
          effects |= key_e;
          purity = Purity::join(purity, key_p);
          // Creating accessor functions allocates.
          effects |= EffectSet::ALLOCATES;
        }
        ObjectProperty::Spread(expr) => {
          let (e, p) = self.analyze_expr(*expr);
          effects |= e | EffectSet::UNKNOWN_CALL;
          purity = Purity::Impure;
          purity = Purity::join(purity, p);
        }
      }
    }
    (effects, purity)
  }

  fn analyze_template_literal(&mut self, tmpl: &hir_js::TemplateLiteral) -> (EffectSet, Purity) {
    let mut effects = EffectSet::ALLOCATES;
    let mut purity = Purity::Allocating;
    for span in &tmpl.spans {
      let (e, p) = self.analyze_expr(span.expr);
      effects |= e;
      purity = Purity::join(purity, p);
    }
    (effects, purity)
  }

  fn analyze_object_key_eval(&mut self, key: &ObjectKey) -> (EffectSet, Purity) {
    match key {
      ObjectKey::Computed(expr) => self.analyze_expr(*expr),
      _ => (EffectSet::empty(), Purity::Pure),
    }
  }

  fn analyze_pat_eval(&mut self, pat_id: hir_js::PatId) -> (EffectSet, Purity) {
    let Some(pat) = self.body.pats.get(pat_id.0 as usize) else {
      return (EffectSet::empty(), Purity::Pure);
    };
    match &pat.kind {
      hir_js::PatKind::Ident(_) => (EffectSet::empty(), Purity::Pure),
      hir_js::PatKind::Assign { target, default_value } => {
        let (t_e, t_p) = self.analyze_pat_eval(*target);
        let (d_e, d_p) = self.analyze_expr(*default_value);
        (t_e | d_e, Purity::join(t_p, d_p))
      }
      hir_js::PatKind::AssignTarget(expr) => self.analyze_expr(*expr),
      hir_js::PatKind::Rest(inner) => self.analyze_pat_eval(**inner),
      hir_js::PatKind::Array(arr) => {
        let mut effects = EffectSet::empty();
        let mut purity = Purity::Pure;
        for el in arr.elements.iter().flatten() {
          let (p_e, p_p) = self.analyze_pat_eval(el.pat);
          effects |= p_e;
          purity = Purity::join(purity, p_p);
          if let Some(default) = el.default_value {
            let (d_e, d_p) = self.analyze_expr(default);
            effects |= d_e;
            purity = Purity::join(purity, d_p);
          }
        }
        if let Some(rest) = arr.rest {
          let (r_e, r_p) = self.analyze_pat_eval(rest);
          effects |= r_e;
          purity = Purity::join(purity, r_p);
        }
        (effects, purity)
      }
      hir_js::PatKind::Object(obj) => {
        let mut effects = EffectSet::empty();
        let mut purity = Purity::Pure;
        for prop in &obj.props {
          let (k_e, k_p) = self.analyze_object_key_eval(&prop.key);
          effects |= k_e;
          purity = Purity::join(purity, k_p);
          let (v_e, v_p) = self.analyze_pat_eval(prop.value);
          effects |= v_e;
          purity = Purity::join(purity, v_p);
          if let Some(default) = prop.default_value {
            let (d_e, d_p) = self.analyze_expr(default);
            effects |= d_e;
            purity = Purity::join(purity, d_p);
          }
        }
        if let Some(rest) = obj.rest {
          let (r_e, r_p) = self.analyze_pat_eval(rest);
          effects |= r_e;
          purity = Purity::join(purity, r_p);
        }
        (effects, purity)
      }
    }
  }

  fn resolve_global_member_api(&self, expr_id: ExprId) -> Option<&Api> {
    let path = self.member_path_string(expr_id)?;
    let canonical = canonical_name_with_global_prefix_stripping(self.kb, &path)?;
    // Only accept global/static paths (including `Foo.prototype.bar`).
    let api = self.kb.get(canonical)?;
    Some(api)
  }

  fn member_path_string(&self, expr_id: ExprId) -> Option<String> {
    let segs = self.member_path_segments(expr_id)?;
    Some(segs.join("."))
  }

  fn member_path_segments(&self, expr_id: ExprId) -> Option<Vec<String>> {
    let expr_id = self.strip_value_wrappers(expr_id);
    let expr = self.body.exprs.get(expr_id.0 as usize)?;
    match &expr.kind {
      ExprKind::Ident(name) => Some(vec![self.lowered.names.resolve(*name)?.to_string()]),
      ExprKind::Member(member) => {
        if member.optional {
          return None;
        }
        let mut segs = self.member_path_segments(member.object)?;
        let prop = self.static_object_key_name(&member.property)?;
        segs.push(prop);
        Some(segs)
      }
      _ => None,
    }
  }

  fn static_object_key_name(&self, key: &ObjectKey) -> Option<String> {
    match key {
      ObjectKey::Ident(name) => Some(self.lowered.names.resolve(*name)?.to_string()),
      ObjectKey::String(s) => Some(s.clone()),
      ObjectKey::Number(n) => Some(crate::js_string::number_literal_to_js_string(n)),
      ObjectKey::Computed(expr) => {
        let expr = self.strip_value_wrappers(*expr);
        let expr = self.body.exprs.get(expr.0 as usize)?;
        match &expr.kind {
          ExprKind::Literal(hir_js::Literal::String(s)) => Some(s.lossy.clone()),
          ExprKind::Literal(hir_js::Literal::Number(n)) => Some(crate::js_string::number_literal_to_js_string(n)),
          ExprKind::Literal(hir_js::Literal::BigInt(n)) => Some(n.clone()),
          ExprKind::Template(tmpl) if tmpl.spans.is_empty() => Some(tmpl.head.clone()),
          _ => None,
        }
      }
    }
  }

  fn strip_value_wrappers(&self, mut expr: ExprId) -> ExprId {
    loop {
      let Some(node) = self.body.exprs.get(expr.0 as usize) else {
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

  fn build_callsite_info_from_call_args(&self, api: &Api, args: &[hir_js::CallArg]) -> EvalCallSiteInfo {
    let mut referenced = referenced_arg_indices(api);

    let mut len = args.len();
    if let Some(max) = referenced.iter().max().copied() {
      len = len.max(max + 1);
    }
    if len == 0 {
      return EvalCallSiteInfo::default();
    }

    let mut arg_effects = vec![EffectSet::UNKNOWN_CALL; len];
    let mut arg_purity = vec![Purity::Impure; len];
    let mut callback_uses_index = false;
    let mut callback_uses_array = false;

    referenced.sort_unstable();
    referenced.dedup();

    for idx in referenced {
      let Some(arg) = args.get(idx) else {
        continue;
      };
      if arg.spread {
        continue;
      }
      if let Some(cb) = analyze_inline_callback(self.lowered, self.body_id, arg.expr, self.kb) {
        arg_effects[idx] = cb.effects;
        arg_purity[idx] = cb.purity;
        if idx == 0 {
          callback_uses_index = cb.uses_index;
          callback_uses_array = cb.uses_array;
        }
      }
    }

    EvalCallSiteInfo {
      arg_effects,
      arg_purity,
      callback_uses_index,
      callback_uses_array,
    }
  }

  #[cfg(feature = "hir-semantic-ops")]
  fn build_callsite_info_from_expr_args(&self, api: &Api, args: &[ExprId]) -> EvalCallSiteInfo {
    let mut referenced = referenced_arg_indices(api);

    let mut len = args.len();
    if let Some(max) = referenced.iter().max().copied() {
      len = len.max(max + 1);
    }
    if len == 0 {
      return EvalCallSiteInfo::default();
    }

    let mut arg_effects = vec![EffectSet::UNKNOWN_CALL; len];
    let mut arg_purity = vec![Purity::Impure; len];
    let mut callback_uses_index = false;
    let mut callback_uses_array = false;

    referenced.sort_unstable();
    referenced.dedup();

    for idx in referenced {
      let Some(expr) = args.get(idx).copied() else {
        continue;
      };
      if let Some(cb) = analyze_inline_callback(self.lowered, self.body_id, expr, self.kb) {
        arg_effects[idx] = cb.effects;
        arg_purity[idx] = cb.purity;
        if idx == 0 {
          callback_uses_index = cb.uses_index;
          callback_uses_array = cb.uses_array;
        }
      }
    }

    EvalCallSiteInfo {
      arg_effects,
      arg_purity,
      callback_uses_index,
      callback_uses_array,
    }
  }

  fn types_opt(&self) -> Option<&dyn crate::types::TypeProvider> {
    #[cfg(feature = "typed")]
    {
      return self.types;
    }
    #[cfg(not(feature = "typed"))]
    {
      None
    }
  }

  #[cfg(feature = "hir-semantic-ops")]
  fn analyze_semantic_call(&mut self, expr_id: ExprId) -> (EffectSet, Purity) {
    // Evaluate semantic-op children explicitly (resolve_call may recover wrapper
    // array expressions, but these fields are the authoritative runtime inputs).
    let mut effects = EffectSet::empty();
    let mut purity = Purity::Pure;

    let Some(expr) = self.body.exprs.get(expr_id.0 as usize) else {
      return (EffectSet::UNKNOWN_CALL, Purity::Impure);
    };

    match &expr.kind {
      ExprKind::ArrayMap { array, callback }
      | ExprKind::ArrayFilter { array, callback }
      | ExprKind::ArrayFind { array, callback }
      | ExprKind::ArrayEvery { array, callback }
      | ExprKind::ArraySome { array, callback } => {
        let (a_e, a_p) = self.analyze_expr(*array);
        let (c_e, c_p) = self.analyze_expr(*callback);
        effects |= a_e | c_e;
        purity = Purity::join(purity, Purity::join(a_p, c_p));
      }
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        let (a_e, a_p) = self.analyze_expr(*array);
        let (c_e, c_p) = self.analyze_expr(*callback);
        effects |= a_e | c_e;
        purity = Purity::join(purity, Purity::join(a_p, c_p));
        if let Some(init) = init {
          let (i_e, i_p) = self.analyze_expr(*init);
          effects |= i_e;
          purity = Purity::join(purity, i_p);
        }
      }
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        for p in promises {
          let (e, p) = self.analyze_expr(*p);
          effects |= e;
          purity = Purity::join(purity, p);
        }
      }
      ExprKind::KnownApiCall { args, .. } => {
        for arg in args {
          let (e, p) = self.analyze_expr(*arg);
          effects |= e;
          purity = Purity::join(purity, p);
        }
      }
      _ => {}
    }

    // Model the call itself via KB semantics.
    let resolved = crate::resolve::resolve_call(
      self.lowered,
      self.body_id,
      self.body,
      expr_id,
      self.kb,
      self.types_opt(),
    );
    let Some(resolved) = resolved else {
      effects |= EffectSet::UNKNOWN_CALL;
      purity = Purity::Impure;
      return (effects, purity);
    };
    let Some(api) = self.kb.get_by_id(resolved.api_id) else {
      effects |= EffectSet::UNKNOWN_CALL;
      purity = Purity::Impure;
      return (effects, purity);
    };

    let site = self.build_callsite_info_from_expr_args(api, &resolved.args);
    let sem = eval_api_call(api, &site);
    effects |= sem.effects;
    purity = Purity::join(purity, sem.purity);

    (effects, purity)
  }

  #[cfg(feature = "hir-semantic-ops")]
  fn analyze_array_chain(&mut self, array: ExprId, ops: &[hir_js::ArrayChainOp]) -> (EffectSet, Purity) {
    let mut effects = EffectSet::empty();
    let mut purity = Purity::Pure;

    let (a_e, a_p) = self.analyze_expr(array);
    effects |= a_e;
    purity = Purity::join(purity, a_p);

    for op in ops {
      let (api_name, callback, init) = match op {
        hir_js::ArrayChainOp::Map(cb) => ("Array.prototype.map", *cb, None),
        hir_js::ArrayChainOp::Filter(cb) => ("Array.prototype.filter", *cb, None),
        hir_js::ArrayChainOp::Reduce(cb, init) => ("Array.prototype.reduce", *cb, *init),
        hir_js::ArrayChainOp::Find(cb) => ("Array.prototype.find", *cb, None),
        hir_js::ArrayChainOp::Every(cb) => ("Array.prototype.every", *cb, None),
        hir_js::ArrayChainOp::Some(cb) => ("Array.prototype.some", *cb, None),
      };
      let (cb_e, cb_p) = self.analyze_expr(callback);
      effects |= cb_e;
      purity = Purity::join(purity, cb_p);
      let mut call_args = vec![callback];
      if let Some(init) = init {
        let (i_e, i_p) = self.analyze_expr(init);
        effects |= i_e;
        purity = Purity::join(purity, i_p);
        call_args.push(init);
      }

      let Some(api) = self.kb.get(api_name) else {
        effects |= EffectSet::UNKNOWN_CALL;
        purity = Purity::Impure;
        continue;
      };
      let site = self.build_callsite_info_from_expr_args(api, &call_args);
      let sem = eval_api_call(api, &site);
      effects |= sem.effects;
      purity = Purity::join(purity, sem.purity);
    }

    (effects, purity)
  }

  fn analyze_jsx(&mut self, jsx: &hir_js::JsxElement) -> (EffectSet, Purity) {
    // JSX semantics are runtime-dependent (React, Preact, custom factories). Be
    // conservative.
    let mut effects = EffectSet::UNKNOWN_CALL;
    let mut purity = Purity::Impure;

    for attr in &jsx.attributes {
      match attr {
        hir_js::JsxAttr::Named { value, .. } => match value {
          Some(hir_js::JsxAttrValue::Expression(container)) => {
            if let Some(expr) = container.expr {
              let (e, p) = self.analyze_expr(expr);
              effects |= e;
              purity = Purity::join(purity, p);
            }
            if container.spread {
              effects |= EffectSet::UNKNOWN_CALL;
              purity = Purity::Impure;
            }
          }
          Some(hir_js::JsxAttrValue::Element(expr)) => {
            let (e, p) = self.analyze_expr(*expr);
            effects |= e;
            purity = Purity::join(purity, p);
          }
          _ => {}
        },
        hir_js::JsxAttr::Spread { expr } => {
          let (e, p) = self.analyze_expr(*expr);
          effects |= e | EffectSet::UNKNOWN_CALL;
          purity = Purity::Impure;
          purity = Purity::join(purity, p);
        }
      }
    }

    for child in &jsx.children {
      match child {
        hir_js::JsxChild::Element(expr) => {
          let (e, p) = self.analyze_expr(*expr);
          effects |= e;
          purity = Purity::join(purity, p);
        }
        hir_js::JsxChild::Expr(container) => {
          if let Some(expr) = container.expr {
            let (e, p) = self.analyze_expr(expr);
            effects |= e;
            purity = Purity::join(purity, p);
          }
          if container.spread {
            effects |= EffectSet::UNKNOWN_CALL;
            purity = Purity::Impure;
          }
        }
        hir_js::JsxChild::Text(_) => {}
      }
    }

    (effects, purity)
  }
}

fn referenced_arg_indices(api: &Api) -> Vec<usize> {
  let mut referenced: Vec<usize> = Vec::new();
  if let EffectTemplate::DependsOnArgs { args, .. } = &api.effects {
    referenced.extend(args.iter().copied());
  }
  if let PurityTemplate::DependsOnArgs { args, .. } = &api.purity {
    referenced.extend(args.iter().copied());
  }
  referenced
}

fn canonical_name_with_global_prefix_stripping<'a>(kb: &'a KnowledgeBase, name_or_alias: &str) -> Option<&'a str> {
  if let Some(canonical) = kb.canonical_name(name_or_alias) {
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
    if let Some(canonical) = kb.canonical_name(cur) {
      return Some(canonical);
    }
  }
}
