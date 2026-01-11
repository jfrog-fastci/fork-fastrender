use anyhow::{anyhow, Result};

use knowledge_base::{Api, KnowledgeBase};

use crate::ApiId;

#[derive(Debug, Clone)]
pub struct EffectDb {
  kb: KnowledgeBase,
}

/// Facts inferred about a specific callsite (e.g. callback purity/index usage).
///
/// This is intentionally a small, stable surface that downstream analyses can
/// consume without needing to understand the full callback body.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallSiteInfo {
  pub callback_is_pure: Option<bool>,
  pub callback_uses_index: Option<bool>,
  pub callback_uses_array: Option<bool>,
  pub callback_is_associative: Option<bool>,
}

/// Per-body side tables produced by `effect-js` analyses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BodyTables {
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

/// Compute per-body side tables using type information.
#[cfg(feature = "typed")]
pub fn analyze_body_tables_typed(
  lowered: &hir_js::LowerResult,
  types: &impl crate::types::TypeProvider,
) -> std::collections::HashMap<hir_js::BodyId, BodyTables> {
  use hir_js::{ExprId, ExprKind};
  use std::collections::HashMap;

  let mut out = HashMap::new();
  for (&body_id, idx) in lowered.body_index.iter() {
    let body = &lowered.bodies[*idx];
    let mut tables = BodyTables {
      resolved_member: vec![None; body.exprs.len()],
    };

    for (expr_idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Member(_)) {
        continue;
      }
      if let Some(res) = crate::resolve::resolve_member(lowered, body_id, ExprId(expr_idx as u32), types) {
        tables.resolved_member[expr_idx] = Some(res.api);
      }
    }

    out.insert(body_id, tables);
  }
  out
}
