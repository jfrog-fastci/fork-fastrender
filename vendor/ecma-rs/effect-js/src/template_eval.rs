use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};
use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
use knowledge_base::{ApiSemantics, KnowledgeBase};

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
      effects: unknown_effects(),
      purity: Purity::Impure,
    };
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return CallEval {
      effects: unknown_effects(),
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
      effects: unknown_effects(),
      purity: Purity::Impure,
    };
  }

  let api = resolve_api_semantics(kb, lowered, body, call_expr);

  let sem = api.map(|api| {
    let (arg_effects, arg_purity) = build_arg_models(api, call, lowered, body, kb);

    // `effect_summary` preserves author-provided base flags even when `effects`
    // is a runtime-dependent template.
    let effects = api.effects_for_call(&arg_effects) | api.effect_summary;
    let purity_from_template = api.purity_for_call(&arg_purity);
    let purity_from_effects = effects.inferred_purity();

    CallEval {
      effects,
      purity: Purity::join(purity_from_template, purity_from_effects),
    }
  });

  let mut effects = sem.map(|s| s.effects).unwrap_or_else(unknown_effects);
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

fn unknown_effects() -> EffectSet {
  EffectSet::UNKNOWN | EffectSet::MAY_THROW
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

  let mut arg_effects = vec![unknown_effects(); len];
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
  let api = kb.get(&path).or_else(|| resolve_api_alias(kb, &path))?;

  // Avoid treating unknown local bindings as trusted *pure* builtins.
  //
  // If we resolve to an effectful API (IO/nondeterministic/throws/etc) and the
  // user actually shadowed the name with something pure, this is conservative.
  // Resolving the other way (assuming a pure built-in when the user shadowed it
  // with something impure) would be unsound.
  let sem = eval_api_call(api, &EvalCallSiteInfo::default());
  if sem.purity == Purity::Pure {
    return None;
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
        ObjectKey::Number(n) => n.clone(),
        ObjectKey::Computed(_) => return None,
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
  use effect_model::EffectSet;
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
      effect_summary: EffectSet::empty(),
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
}
