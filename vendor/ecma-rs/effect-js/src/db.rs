use anyhow::{anyhow, Result};

use knowledge_base::{Api, KnowledgeBase};

/// Lightweight per-callsite metadata used by heuristics in `effect-js`.
///
/// This is intentionally small and conservative: when a field is `None`, the
/// analysis could not confidently infer the property.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallSiteInfo {
  pub callback_is_pure: Option<bool>,
  pub callback_uses_index: Option<bool>,
  pub callback_uses_array: Option<bool>,
}

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

