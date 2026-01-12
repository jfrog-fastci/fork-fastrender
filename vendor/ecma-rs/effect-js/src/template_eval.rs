use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};
use hir_js::{
  Body, BodyId, ExprId, ExprKind, ForHead, ForInit, LowerResult, NameId, ObjectKey, PatId, PatKind,
  StmtId, StmtKind,
};
use knowledge_base::{ApiSemantics, KnowledgeBase};
use std::collections::HashSet;

use crate::callback::analyze_inline_callback;
use crate::eval::{eval_api_call, CallSiteInfo as EvalCallSiteInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallEval {
  pub effects: EffectSet,
  pub purity: Purity,
}

pub(crate) fn eval_call_expr(
  kb: &KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> CallEval {
  let Some(body_ref) = lowered.body(body) else {
    return CallEval {
      effects: EffectSet::UNKNOWN_CALL,
      purity: Purity::Impure,
    };
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return CallEval {
      effects: EffectSet::UNKNOWN_CALL,
      purity: Purity::Impure,
    };
  };
  let ExprKind::Call(call) = &expr.kind else {
    return CallEval {
      effects: EffectSet::empty(),
      purity: Purity::Pure,
    };
  };

  if call.optional {
    return CallEval {
      effects: EffectSet::UNKNOWN_CALL,
      purity: Purity::Impure,
    };
  }

  let api = resolve_api_semantics(kb, lowered, body, call_expr);

  let sem = api.map(|api| {
    let (arg_effects, arg_purity) = build_arg_models(api, call, lowered, body, kb);

    // `effect_summary` preserves author-provided base flags even when `effects`
    // is a runtime-dependent template.
    let effects = api.effects_for_call(&arg_effects) | api.effect_summary.to_effect_set();
    let purity_from_template = api.purity_for_call(&arg_purity);
    let purity_from_effects = effects.inferred_purity();

    CallEval {
      effects,
      purity: Purity::join(purity_from_template, purity_from_effects),
    }
  });

  let mut effects = sem.map(|s| s.effects).unwrap_or(EffectSet::UNKNOWN_CALL);
  if call.is_new {
    effects |= EffectSet::ALLOCATES;
  }

  let mut purity = sem.map(|s| s.purity).unwrap_or_else(|| {
    if call.is_new {
      Purity::Allocating
    } else {
      Purity::Impure
    }
  });

  purity = Purity::join(purity, effects.inferred_purity());

  CallEval { effects, purity }
}

fn build_arg_models(
  api: &ApiSemantics,
  call: &hir_js::CallExpr,
  lowered: &LowerResult,
  body: BodyId,
  kb: &KnowledgeBase,
) -> (Vec<EffectSet>, Vec<Purity>) {
  let mut referenced: Vec<usize> = Vec::new();
  if let EffectTemplate::DependsOnArgs { args, .. } = &api.effects {
    referenced.extend(args.iter().copied());
  }
  if let PurityTemplate::DependsOnArgs { args, .. } = &api.purity {
    referenced.extend(args.iter().copied());
  }
  referenced.sort_unstable();
  referenced.dedup();

  let mut len = call.args.len();
  if let Some(max) = referenced.iter().max().copied() {
    len = len.max(max + 1);
  }
  if len == 0 {
    return (Vec::new(), Vec::new());
  }

  let mut arg_effects = vec![EffectSet::UNKNOWN_CALL; len];
  let mut arg_purity = vec![Purity::Impure; len];

  for &idx in &referenced {
    let Some(arg) = call.args.get(idx) else {
      continue;
    };
    if arg.spread {
      continue;
    }
    if let Some(cb) = analyze_inline_callback(lowered, body, arg.expr, kb) {
      arg_effects[idx] = cb.effects;
      arg_purity[idx] = cb.purity;
    }
  }

  (arg_effects, arg_purity)
}

fn resolve_api_semantics<'a>(
  kb: &'a KnowledgeBase,
  lowered: &LowerResult,
  body: BodyId,
  call_expr: ExprId,
) -> Option<&'a ApiSemantics> {
  if let Some(name) = crate::resolver::resolve_api_call(kb, lowered, body, call_expr) {
    return kb.get(name);
  }

  let body_ref = lowered.body(body)?;
  let ExprKind::Call(call) = &body_ref.exprs[call_expr.0 as usize].kind else {
    return None;
  };
  let path = static_callee_path(lowered, body_ref, call.callee)?;
  let api = kb
    .get(&path)
    .or_else(|| strip_global_prefixes(&path).and_then(|p| kb.get(p)))
    .or_else(|| resolve_api_alias(kb, &path))
    .or_else(|| strip_global_prefixes(&path).and_then(|p| resolve_api_alias(kb, p)))?;

  // Avoid treating unknown shadowed bindings as trusted *pure* builtins.
  //
  // If we resolve to an effectful API (IO/nondeterministic/throws/etc) and the
  // user actually shadowed the name with something pure, this is conservative.
  // Resolving the other way (assuming a pure built-in when the user shadowed it
  // with something impure) would be unsound.
  //
  // We allow pure builtins when the callee root identifier is not declared
  // anywhere in the surrounding body chain. This is still conservative (we do
  // not attempt precise block scoping), but captures the common case of pure
  // callbacks like `x => Math.sqrt(x)` without breaking shadowing safety.
  let sem = eval_api_call(api, &EvalCallSiteInfo::default());
  if sem.purity == Purity::Pure {
    let root = callee_root_ident(body_ref, call.callee);
    if root.is_some_and(|name| name_shadowed_in_body_chain(lowered, body, name)) {
      return None;
    }
  }

  Some(api)
}

fn resolve_api_alias<'a>(kb: &'a KnowledgeBase, alias: &str) -> Option<&'a ApiSemantics> {
  kb.iter().find_map(|(_, api)| {
    api
      .aliases
      .iter()
      .any(|a| a == alias)
      .then_some(api)
  })
}

fn strip_global_prefixes<'a>(mut path: &'a str) -> Option<&'a str> {
  let mut changed = false;
  loop {
    let mut did_strip = false;
    for prefix in ["globalThis.", "window.", "self.", "global."] {
      if let Some(rest) = path.strip_prefix(prefix) {
        path = rest;
        did_strip = true;
        changed = true;
        break;
      }
    }
    if !did_strip {
      break;
    }
  }
  changed.then_some(path)
}

fn strip_transparent_wrappers(body: &Body, mut expr: ExprId) -> ExprId {
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

fn callee_root_ident(body: &Body, mut expr: ExprId) -> Option<NameId> {
  expr = strip_transparent_wrappers(body, expr);
  loop {
    let node = body.exprs.get(expr.0 as usize)?;
    match &node.kind {
      ExprKind::Ident(name) => return Some(*name),
      ExprKind::Member(mem) => {
        if mem.optional {
          return None;
        }
        expr = mem.object;
      }
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return None,
    }
  }
}

fn name_shadowed_in_body_chain(lowered: &LowerResult, mut body_id: BodyId, name: NameId) -> bool {
  let mut seen = HashSet::new();
  loop {
    if !seen.insert(body_id) {
      return true;
    }
    let Some(body) = lowered.body(body_id) else {
      return true;
    };
    if body_declares_name_anywhere(lowered, body, name) {
      return true;
    }
    let Some(parent) = parent_body_id(lowered, body) else {
      return false;
    };
    body_id = parent;
  }
}

fn parent_body_id(lowered: &LowerResult, body: &Body) -> Option<BodyId> {
  let mut def = body.owner;
  loop {
    let parent_def = lowered.def(def)?.parent?;
    let parent_data = lowered.def(parent_def)?;
    if let Some(parent_body) = parent_data.body {
      return Some(parent_body);
    }
    def = parent_def;
  }
}

fn body_declares_name_anywhere(lowered: &LowerResult, body: &Body, name: NameId) -> bool {
  if let Some(func) = &body.function {
    for param in &func.params {
      if pat_binds_name(body, param.pat, name) {
        return true;
      }
    }
  }

  for stmt_id in body.root_stmts.iter().copied() {
    if stmt_declares_name_anywhere(lowered, body, stmt_id, name) {
      return true;
    }
  }

  false
}

fn stmt_declares_name_anywhere(
  lowered: &LowerResult,
  body: &Body,
  stmt_id: StmtId,
  name: NameId,
) -> bool {
  let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
    return false;
  };
  match &stmt.kind {
    StmtKind::Decl(def) => lowered.def(*def).is_some_and(|def| def.name == name),
    StmtKind::Var(var) => var
      .declarators
      .iter()
      .any(|decl| pat_binds_name(body, decl.pat, name)),
    StmtKind::Block(stmts) => stmts
      .iter()
      .any(|id| stmt_declares_name_anywhere(lowered, body, *id, name)),
    StmtKind::If {
      consequent,
      alternate,
      ..
    } => {
      stmt_declares_name_anywhere(lowered, body, *consequent, name)
        || alternate
          .as_ref()
          .is_some_and(|id| stmt_declares_name_anywhere(lowered, body, *id, name))
    }
    StmtKind::While { body: inner, .. } | StmtKind::DoWhile { body: inner, .. } => {
      stmt_declares_name_anywhere(lowered, body, *inner, name)
    }
    StmtKind::For { init, body: inner, .. } => {
      let init_declares = matches!(init, Some(ForInit::Var(var)) if var
        .declarators
        .iter()
        .any(|decl| pat_binds_name(body, decl.pat, name)));
      init_declares || stmt_declares_name_anywhere(lowered, body, *inner, name)
    }
    StmtKind::ForIn { left, body: inner, .. } => {
      let head_declares = match left {
        ForHead::Pat(pat) => pat_binds_name(body, *pat, name),
        ForHead::Var(var) => var
          .declarators
          .iter()
          .any(|decl| pat_binds_name(body, decl.pat, name)),
      };
      head_declares || stmt_declares_name_anywhere(lowered, body, *inner, name)
    }
    StmtKind::Switch { cases, .. } => cases.iter().any(|case| {
      case
        .consequent
        .iter()
        .any(|id| stmt_declares_name_anywhere(lowered, body, *id, name))
    }),
    StmtKind::Try {
      block,
      catch,
      finally_block,
    } => {
      stmt_declares_name_anywhere(lowered, body, *block, name)
        || catch.as_ref().is_some_and(|clause| {
          clause
            .param
            .is_some_and(|param| pat_binds_name(body, param, name))
            || stmt_declares_name_anywhere(lowered, body, clause.body, name)
        })
        || finally_block
          .as_ref()
          .is_some_and(|id| stmt_declares_name_anywhere(lowered, body, *id, name))
    }
    StmtKind::Labeled { body: inner, .. } => stmt_declares_name_anywhere(lowered, body, *inner, name),
    // `with (obj) { ... }` makes identifier resolution dynamic. Be conservative
    // and treat it as shadowing all names.
    StmtKind::With { .. } => true,
    _ => false,
  }
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

fn static_callee_path(lowered: &LowerResult, body: &Body, expr_id: ExprId) -> Option<String> {
  let expr = body.exprs.get(expr_id.0 as usize)?;
  match &expr.kind {
    ExprKind::Ident(name) => Some(lowered.names.resolve(*name)?.to_string()),
    ExprKind::Member(mem) => {
      if mem.optional {
        return None;
      }
      let base = static_callee_path(lowered, body, mem.object)?;
      let prop = match &mem.property {
        ObjectKey::Ident(name) => lowered.names.resolve(*name)?.to_string(),
        ObjectKey::String(s) => s.clone(),
        ObjectKey::Number(n) => crate::js_string::number_literal_to_js_string(n),
        ObjectKey::Computed(expr) => {
          let expr = strip_transparent_wrappers(body, *expr);
          let expr = body.exprs.get(expr.0 as usize)?;
          match &expr.kind {
            ExprKind::Literal(hir_js::Literal::String(lit)) => lit.lossy.clone(),
            ExprKind::Literal(hir_js::Literal::Number(n)) => crate::js_string::number_literal_to_js_string(n),
            ExprKind::Literal(hir_js::Literal::BigInt(n)) => n.clone(),
            ExprKind::Template(tmpl) if tmpl.spans.is_empty() => tmpl.head.clone(),
            _ => return None,
          }
        }
      };
      Some(format!("{base}.{prop}"))
    }
    ExprKind::TypeAssertion { expr: inner, .. }
    | ExprKind::NonNull { expr: inner }
    | ExprKind::Satisfies { expr: inner, .. } => static_callee_path(lowered, body, *inner),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::{EffectSet, EffectSummary};
  use hir_js::{FileKind, StmtKind};
  use knowledge_base::{ApiDatabase, ApiId, ApiKind};
  use std::collections::BTreeMap;

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
  fn does_not_panic_when_depends_on_args_argument_is_missing() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, "Array.prototype.map();").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let eval = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(eval.effects.contains(EffectSet::UNKNOWN));
  }

  #[test]
  fn analyzes_callback_for_nonzero_depends_on_args_indices() {
    let base = crate::load_default_api_database();
    let mut entries: Vec<ApiSemantics> = base.iter().map(|(_, api)| api.clone()).collect();
    entries.push(ApiSemantics {
      id: ApiId::from_name("cbApi"),
      name: "cbApi".to_string(),
      aliases: Vec::new(),
      effects: EffectTemplate::DependsOnArgs {
        base: EffectSet::empty(),
        args: vec![1],
      },
      effect_summary: EffectSummary::PURE,
      purity: PurityTemplate::DependsOnArgs {
        base: Purity::Pure,
        args: vec![1],
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
    });
    let kb = ApiDatabase::from_entries(entries);

    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, "cbApi(0, () => Date.now());").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let eval = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(eval.effects.contains(EffectSet::NONDETERMINISTIC));
  }

  #[test]
  fn uses_kb_effect_summary_for_depends_on_callback_templates() {
    let kb = crate::load_default_api_database();
    let lowered =
      hir_js::lower_from_source_with_kind(FileKind::Js, "Promise.prototype.then(x => x);").unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let eval = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(
      eval.effects.contains(EffectSet::ALLOCATES),
      "expected Promise.prototype.then to allocate (base effects)"
    );
  }

  #[test]
  fn global_this_constructor_resolves_via_prefix_stripping() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      FileKind::Js,
      r#"new globalThis.URL("https://example.com");"#,
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let sem = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
    assert!(sem.effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn global_this_fetch_resolves_via_computed_key() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      FileKind::Js,
      r#"globalThis["fetch"]("https://example.com");"#,
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let sem = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(sem.effects.contains(EffectSet::IO));
    assert!(sem.effects.contains(EffectSet::NETWORK));
  }

  #[test]
  fn global_this_fetch_resolves_via_computed_template_key() {
    let kb = crate::load_default_api_database();
    let lowered = hir_js::lower_from_source_with_kind(
      FileKind::Js,
      r#"globalThis[`fetch`]("https://example.com");"#,
    )
    .unwrap();
    let (body, call_expr) = first_stmt_expr(&lowered);

    let sem = eval_call_expr(&kb, &lowered, body, call_expr);
    assert!(sem.effects.contains(EffectSet::IO));
    assert!(sem.effects.contains(EffectSet::NETWORK));
  }
}
