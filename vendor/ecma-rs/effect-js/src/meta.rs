use effect_model::Purity;
use knowledge_base::Api;

use crate::CallSiteInfo;

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
  let Some(cb) = callsite.callback else {
    return false;
  };

  // Allocating callbacks are still safe to run in parallel; the allocation is
  // thread-local from the caller's perspective.
  let callback_is_pure = matches!(cb.purity, Purity::Pure | Purity::Allocating);

  if api.name.ends_with(".map") || api.name.ends_with(".filter") {
    return callback_is_pure && !cb.uses_index;
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
