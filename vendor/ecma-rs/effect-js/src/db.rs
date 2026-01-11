use anyhow::{anyhow, Result};

use knowledge_base::{Api, KnowledgeBase};

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

/// Facts inferred about a specific callsite (e.g. callback purity).
#[derive(Debug, Default, Clone)]
pub struct CallSiteInfo {
  pub callback_is_pure: Option<bool>,
  pub callback_uses_index: Option<bool>,
  pub callback_is_associative: Option<bool>,
}
