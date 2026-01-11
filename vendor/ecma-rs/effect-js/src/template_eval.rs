use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};
use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
use knowledge_base::{ApiSemantics, KnowledgeBase};

use crate::callback::analyze_inline_callback;

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

  let mut arg_effects = Vec::with_capacity(call.args.len());
  let mut arg_purity = Vec::with_capacity(call.args.len());
  for (idx, arg) in call.args.iter().enumerate() {
    if arg.spread {
      arg_effects.push(unknown_effects());
      arg_purity.push(Purity::Impure);
      continue;
    }
    if idx == 0 {
      if let Some(cb) = callback {
        arg_effects.push(cb.effects);
        arg_purity.push(cb.purity);
        continue;
      }
    }
    arg_effects.push(unknown_effects());
    arg_purity.push(Purity::Impure);
  }

  let mut effects = match api {
    Some(api) => api.effects_for_call(&arg_effects),
    None => unknown_effects(),
  };
  if call.is_new {
    effects |= EffectSet::ALLOCATES;
  }

  let mut purity = match api {
    Some(api) => api.purity_for_call(&arg_purity),
    None => {
      if call.is_new {
        Purity::Allocating
      } else {
        Purity::Impure
      }
    }
  };

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
  let effects = api.effects_for_call(&[]);
  let purity = api.purity_for_call(&[]);
  if Purity::join(purity, effects.inferred_purity()) == Purity::Pure {
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
