use anyhow::{anyhow, Result};

use knowledge_base::{Api, KnowledgeBase};

/// Per-callsite metadata used by heuristics in `effect-js`.
///
/// This is derived from inline callback analysis in [`crate::callback`]. It is
/// re-exported here to keep older call sites that used `effect_js::db::CallSiteInfo`
/// compiling while the API stabilizes.
pub type CallSiteInfo = crate::callback::CallSiteInfo;

#[derive(Debug, Clone)]
pub struct EffectDb {
  kb: KnowledgeBase,
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
