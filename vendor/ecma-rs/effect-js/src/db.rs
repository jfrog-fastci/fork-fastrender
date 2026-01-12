use anyhow::{anyhow, Result};

use knowledge_base::{Api, KnowledgeBase};
use smallvec::SmallVec;

use crate::{ApiId, EffectSet, Purity};

#[derive(Debug, Clone)]
pub struct EffectDb {
  kb: KnowledgeBase,
}

/// Facts inferred about a specific callsite (e.g. callback purity/effects/index usage).
///
/// This is intentionally a small, stable surface that downstream analyses can
/// consume without needing to understand the full callback body.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallSiteInfo {
  /// Coarse purity classification for an inline callback argument, when available.
  pub callback_purity: Option<Purity>,
  /// Effect flags for an inline callback argument, when available.
  pub callback_effects: Option<EffectSet>,
  /// Whether the callback may throw.
  ///
  /// This is derived from `callback_effects` (i.e. `MAY_THROW` / `UNKNOWN_CALL`).
  pub callback_may_throw: Option<bool>,
  /// Legacy: whether the callback is "pure enough" for parallelization heuristics.
  ///
  /// This is equivalent to `callback_purity` being `Pure` or `Allocating`.
  pub callback_is_pure: Option<bool>,
  pub callback_uses_index: Option<bool>,
  pub callback_uses_array: Option<bool>,
  pub callback_is_associative: Option<bool>,
}

/// Per-body side tables produced by `effect-js` analyses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BodyTables {
  /// `CallExpr` + semantic-op call resolution table, indexed by `ExprId`.
  pub resolved_call: Vec<Option<ApiId>>,
  /// Per-call receiver expression, indexed by `ExprId`.
  pub resolved_call_receiver: Vec<Option<hir_js::ExprId>>,
  /// Per-call argument list, indexed by `ExprId`.
  pub resolved_call_args: Vec<SmallVec<[hir_js::ExprId; 4]>>,
  /// `ExprKind::Member` resolution table, indexed by `ExprId`.
  pub resolved_member: Vec<Option<ApiId>>,
}

impl EffectDb {
  pub fn load_default() -> Result<Self> {
    // `knowledge-base` errors are not `Send + Sync` (they may wrap dyn errors),
    // so we stringify them for `anyhow::Error`.
    let kb = KnowledgeBase::load_default().map_err(|err| anyhow!(err.to_string()))?;
    Ok(Self { kb })
  }

  pub fn api(&self, id: &str) -> Option<&Api> {
    self.kb.get(id)
  }

  pub fn kb(&self) -> &KnowledgeBase {
    &self.kb
  }
}

/// Compute per-body side tables without type information.
///
/// Member resolution is intentionally disabled in untyped mode: property reads like `obj.prop`
/// are frequently ambiguous without a proven receiver type (global vs prototype vs userland),
/// and `effect-js` prefers to be conservative rather than emitting incorrect KB identifiers.
pub fn analyze_body_tables_untyped(
  kb: &KnowledgeBase,
  lowered: &hir_js::LowerResult,
) -> std::collections::HashMap<hir_js::BodyId, BodyTables> {
  use hir_js::ExprId;
  use std::collections::HashMap;

  let mut out = HashMap::new();
  for (&body_id, idx) in lowered.body_index.iter() {
    let body = &lowered.bodies[*idx];
    let expr_len = body.exprs.len();
    let mut tables = BodyTables {
      resolved_call: vec![None; expr_len],
      resolved_call_receiver: vec![None; expr_len],
      resolved_call_args: std::iter::repeat_with(SmallVec::new).take(expr_len).collect(),
      resolved_member: vec![None; expr_len],
    };

    for expr_idx in 0..expr_len {
      let expr_id = ExprId(expr_idx as u32);
      if let Some(res) = crate::resolve::resolve_call(lowered, body_id, body, expr_id, kb, None) {
        tables.resolved_call[expr_idx] = Some(res.api_id);
        tables.resolved_call_receiver[expr_idx] = res.receiver;
        tables.resolved_call_args[expr_idx] = res.args.into_iter().collect();
      }
    }

    out.insert(body_id, tables);
  }
  out
}

/// Compute per-body side tables using type information.
#[cfg(feature = "typed")]
pub fn analyze_body_tables_typed(
  kb: &KnowledgeBase,
  lowered: &hir_js::LowerResult,
  types: &impl crate::types::TypeProvider,
) -> std::collections::HashMap<hir_js::BodyId, BodyTables> {
  use hir_js::{ExprId, ExprKind};
  use std::collections::HashMap;

  let mut out = HashMap::new();
  for (&body_id, idx) in lowered.body_index.iter() {
    let body = &lowered.bodies[*idx];
    let expr_len = body.exprs.len();
    let mut tables = BodyTables {
      resolved_call: vec![None; expr_len],
      resolved_call_receiver: vec![None; expr_len],
      resolved_call_args: std::iter::repeat_with(SmallVec::new).take(expr_len).collect(),
      resolved_member: vec![None; expr_len],
    };

    for (expr_idx, expr) in body.exprs.iter().enumerate() {
      if let Some(res) = crate::resolve::resolve_call(
        lowered,
        body_id,
        body,
        ExprId(expr_idx as u32),
        kb,
        Some(types),
      ) {
        tables.resolved_call[expr_idx] = Some(res.api_id);
        tables.resolved_call_receiver[expr_idx] = res.receiver;
        tables.resolved_call_args[expr_idx] = res.args.into_iter().collect();
      }

      if !matches!(expr.kind, ExprKind::Member(_)) {
        continue;
      }
      if let Some(res) = crate::resolve::resolve_member(kb, lowered, body_id, ExprId(expr_idx as u32), types) {
        tables.resolved_member[expr_idx] = Some(res.api_id);
      }
    }

    out.insert(body_id, tables);
  }
  out
}
