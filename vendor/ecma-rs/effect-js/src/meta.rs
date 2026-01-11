use knowledge_base::Api;

use crate::db::CallSiteInfo;

pub fn is_async(api: &Api) -> bool {
  api.async_.unwrap_or(false)
}

pub fn is_idempotent(api: &Api) -> Option<bool> {
  api.idempotent
}

pub fn is_deterministic(api: &Api) -> Option<bool> {
  api.deterministic
}

pub fn is_parallelizable(api: &Api) -> Option<bool> {
  api.parallelizable
}

pub fn parallelizable_at_callsite(api: &Api, callsite: &CallSiteInfo) -> bool {
  if let Some(p) = api.parallelizable {
    return p;
  }

  // Fallback heuristic for callback-driven collection APIs when the KB entry
  // doesn't specify `parallelizable` directly.
  if api.name.ends_with(".map") || api.name.ends_with(".filter") {
    return callsite.callback_is_pure.unwrap_or(false)
      && !callsite.callback_uses_index.unwrap_or(false);
  }
  if api.name.ends_with(".reduce") {
    return callsite.callback_is_pure.unwrap_or(false)
      && callsite.callback_is_associative.unwrap_or(false);
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::EffectDb;

  #[test]
  fn meta_queries() {
    let db = EffectDb::load_default().unwrap();

    let fetch = db.api("fetch").unwrap();
    assert!(is_async(fetch));
    assert_eq!(is_parallelizable(fetch), Some(true));

    let sqrt = db.api("Math.sqrt").unwrap();
    assert_eq!(is_deterministic(sqrt), Some(true));
    assert_eq!(is_idempotent(sqrt), Some(true));
    assert!(!is_async(sqrt));
  }
}
