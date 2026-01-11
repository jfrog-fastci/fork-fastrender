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

  let callback = api.and_then(|api| {
    let needs_first_arg = match (&api.effects, &api.purity) {
      (EffectTemplate::DependsOnArgs { args, .. }, _) if args.contains(&0) => true,
      (_, PurityTemplate::DependsOnArgs { args, .. }) if args.contains(&0) => true,
      _ => false,
    };
    if !needs_first_arg {
      return None;
    }
    call
      .args
      .first()
      .filter(|arg| !arg.spread)
      .and_then(|arg| analyze_inline_callback(lowered, body, arg.expr, kb))
  });

  let sem = api.map(|api| {
    // NOTE: `eval_api_call` models callback behavior in argument 0. This matches
    // the current KB encoding for callback-dependent templates
    // (`depends_on_callback` => `DependsOnArgs { args: [0] }`).
    let site = EvalCallSiteInfo {
      callback_purity: callback.map(|cb| cb.purity),
      callback_effects: callback.map(|cb| cb.effects),
      callback_uses_index: callback.map(|cb| cb.uses_index).unwrap_or(false),
      callback_uses_array: callback.map(|cb| cb.uses_array).unwrap_or(false),
    };
    eval_api_call(api, &site)
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
