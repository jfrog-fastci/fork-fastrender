use effect_model::{EffectFlags, EffectSummary, EffectTemplate, Purity, PurityTemplate, ThrowBehavior};
use hir_js::{Body, BodyId, ExprId, ExprKind, LowerResult, ObjectKey};
use knowledge_base::{ApiSemantics, KnowledgeBase};

use crate::callback::analyze_inline_callback;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallEval {
  pub effects: EffectSummary,
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
      purity: Purity::Unknown,
    };
  };
  let Some(expr) = body_ref.exprs.get(call_expr.0 as usize) else {
    return CallEval {
      effects: unknown_effects(),
      purity: Purity::Unknown,
    };
  };
  let ExprKind::Call(call) = &expr.kind else {
    return CallEval {
      effects: EffectSummary::PURE,
      purity: Purity::Pure,
    };
  };

  if call.optional {
    return CallEval {
      effects: unknown_effects(),
      purity: Purity::Unknown,
    };
  }

  let api = resolve_api_semantics(kb, lowered, body, call_expr);

  let callback = match api {
    Some(api)
      if matches!(api.effects, EffectTemplate::DependsOnCallback)
        || matches!(api.purity, PurityTemplate::DependsOnCallback) =>
    {
      call
        .args
        .first()
        .filter(|arg| !arg.spread)
        .and_then(|arg| analyze_inline_callback(lowered, body, arg.expr, kb))
    }
    _ => None,
  };

  let mut effects = match api {
    Some(api) => match api.effects {
      EffectTemplate::DependsOnCallback => {
        let base = crate::effect_template_to_summary(&api.effects);
        let cb_effects = callback.map(|cb| cb.effects).unwrap_or_else(unknown_effects);
        EffectSummary::join(base, cb_effects)
      }
      _ => crate::effect_template_to_summary(&api.effects),
    },
    None => EffectSummary::PURE,
  };

  if call.is_new {
    effects.flags |= EffectFlags::ALLOCATES;
  }

  let mut purity = match api {
    Some(api) => match api.purity {
      PurityTemplate::DependsOnCallback => {
        callback.map(|cb| cb.purity).unwrap_or(Purity::Unknown)
      }
      _ => crate::purity_template_to_purity(&api.purity),
    },
    None => {
      if call.is_new {
        Purity::Allocating
      } else {
        Purity::Unknown
      }
    }
  };

  purity = Purity::join(purity, effects.inferred_purity());

  if api.is_none() && !call.is_new {
    effects = EffectSummary::join(effects, unknown_effects());
  }

  CallEval { effects, purity }
}

fn unknown_effects() -> EffectSummary {
  EffectSummary {
    flags: EffectFlags::all(),
    throws: ThrowBehavior::Maybe,
  }
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
  let effects = crate::effect_template_to_summary(&api.effects);
  let purity = crate::purity_template_to_purity(&api.purity);
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
