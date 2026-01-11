use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSiteInfo {
  pub callback_purity: Option<Purity>,
  pub callback_effects: Option<EffectSet>,
  pub callback_uses_index: bool,
  pub callback_uses_array: bool,
}

impl Default for CallSiteInfo {
  fn default() -> Self {
    Self {
      callback_purity: None,
      callback_effects: None,
      callback_uses_index: false,
      callback_uses_array: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSemantics {
  pub effects: EffectSet,
  pub purity: Purity,
}

pub fn eval_api_call(api: &knowledge_base::Api, site: &CallSiteInfo) -> CallSemantics {
  let (arg_effects, arg_purity) = build_arg_models(api, site);

  // `effect_summary` preserves author-provided base flags even when `effects`
  // is a runtime-dependent template.
  let effects = api.effects_for_call(&arg_effects) | api.effect_summary.to_effect_set();
  let purity_from_template = api.purity_for_call(&arg_purity);
  let purity_from_effects = effects.inferred_purity();

  CallSemantics {
    effects,
    purity: Purity::join(purity_from_template, purity_from_effects),
  }
}

fn callback_effects_from_purity(purity: Purity) -> EffectSet {
  match purity {
    Purity::Pure => EffectSet::empty(),
    Purity::ReadOnly => EffectSet::READS_GLOBAL,
    Purity::Allocating => EffectSet::ALLOCATES,
    Purity::Impure => EffectSet::UNKNOWN_CALL,
  }
}

fn build_arg_models(api: &knowledge_base::Api, site: &CallSiteInfo) -> (Vec<EffectSet>, Vec<Purity>) {
  let mut len = 0usize;
  if let EffectTemplate::DependsOnArgs { args, .. } = &api.effects {
    if let Some(max) = args.iter().max().copied() {
      len = len.max(max + 1);
    }
  }
  if let PurityTemplate::DependsOnArgs { args, .. } = &api.purity {
    if let Some(max) = args.iter().max().copied() {
      len = len.max(max + 1);
    }
  }

  if len == 0 {
    return (Vec::new(), Vec::new());
  }

  let mut arg_effects = vec![EffectSet::UNKNOWN_CALL; len];
  let mut arg_purity = vec![Purity::Impure; len];

  // We only model argument 0 (the callback) via `CallSiteInfo`. All other args are
  // treated as unknown/impure.
  let cb_effects = site
    .callback_effects
    .or_else(|| site.callback_purity.map(callback_effects_from_purity))
    .unwrap_or(EffectSet::UNKNOWN_CALL);
  let cb_purity = match (site.callback_purity, site.callback_effects) {
    (Some(p), Some(e)) => Purity::join(p, e.inferred_purity()),
    (Some(p), None) => p,
    (None, Some(e)) => e.inferred_purity(),
    (None, None) => Purity::Impure,
  };

  arg_effects[0] = cb_effects;
  arg_purity[0] = cb_purity;

  (arg_effects, arg_purity)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn array_map_pure_callback_is_allocating() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.map").unwrap();

    let site = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_uses_index: false,
      callback_uses_array: false,
    };

    let sem = eval_api_call(api, &site);
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
    assert!(!sem.effects.contains(EffectSet::IO));
  }

  #[test]
  fn array_map_impure_callback_includes_io() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.map").unwrap();

    let site = CallSiteInfo {
      callback_purity: Some(Purity::Impure),
      callback_effects: Some(EffectSet::IO | EffectSet::NETWORK),
      callback_uses_index: false,
      callback_uses_array: false,
    };

    let sem = eval_api_call(api, &site);
    assert_eq!(sem.purity, Purity::Impure);
    assert!(sem.effects.contains(EffectSet::IO));
  }

  #[test]
  fn map_prototype_get_is_pure() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Map.prototype.get").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Pure);
    assert_eq!(sem.effects, EffectSet::empty());
  }

  #[test]
  fn map_prototype_has_is_pure() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Map.prototype.has").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Pure);
    assert_eq!(sem.effects, EffectSet::empty());
  }

  #[test]
  fn math_sqrt_may_throw_is_still_pure() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Math.sqrt").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Pure);
    assert!(sem.effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn fetch_is_network_io_and_impure() {
    let kb = crate::load_default_api_database();
    let api = kb.get("fetch").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Impure);
    assert!(sem.effects.contains(EffectSet::IO));
    assert!(sem.effects.contains(EffectSet::NETWORK));
  }

  #[test]
  fn json_parse_allocates_and_may_throw() {
    let kb = crate::load_default_api_database();
    let api = kb.get("JSON.parse").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
    assert!(sem.effects.contains(EffectSet::MAY_THROW));
    assert!(!sem.effects.contains(EffectSet::IO));
  }

  #[test]
  fn promise_all_is_not_pure_and_may_throw() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Promise.all").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_ne!(sem.purity, Purity::Pure);
    assert!(sem.effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn array_filter_pure_callback_is_allocating() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.filter").unwrap();
    let sem = eval_api_call(
      api,
      &CallSiteInfo {
        callback_purity: Some(Purity::Pure),
        callback_effects: Some(EffectSet::empty()),
        ..CallSiteInfo::default()
      },
    );
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn array_reduce_pure_callback_is_pure() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.reduce").unwrap();
    let sem = eval_api_call(
      api,
      &CallSiteInfo {
        callback_purity: Some(Purity::Pure),
        callback_effects: Some(EffectSet::empty()),
        ..CallSiteInfo::default()
      },
    );
    assert_eq!(sem.purity, Purity::Pure);
    assert!(sem.effects.contains(EffectSet::MAY_THROW));
    assert!(!sem.effects.contains(EffectSet::IO));
  }

  #[test]
  fn array_for_each_propagates_callback_io() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.forEach").unwrap();
    let sem = eval_api_call(
      api,
      &CallSiteInfo {
        callback_purity: Some(Purity::Impure),
        callback_effects: Some(EffectSet::IO),
        ..CallSiteInfo::default()
      },
    );
    assert_eq!(sem.purity, Purity::Impure);
    assert!(sem.effects.contains(EffectSet::IO));
  }

  #[test]
  fn callback_purity_without_effects_is_modeled() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Array.prototype.map").unwrap();
    let sem = eval_api_call(
      api,
      &CallSiteInfo {
        callback_purity: Some(Purity::Allocating),
        callback_effects: None,
        ..CallSiteInfo::default()
      },
    );

    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
    assert!(!sem.effects.contains(EffectSet::UNKNOWN));
  }

  #[test]
  fn mutation_observer_constructor_is_allocating() {
    let kb = crate::load_default_api_database();
    let api = kb.get("MutationObserver").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn mutation_observer_observe_writes_global() {
    let kb = crate::load_default_api_database();
    let api = kb.get("MutationObserver.prototype.observe").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Impure);
    assert!(sem.effects.contains(EffectSet::WRITES_GLOBAL));
  }

  #[test]
  fn resize_observer_take_records_allocates_and_drains_queue() {
    let kb = crate::load_default_api_database();
    let api = kb.get("ResizeObserver.prototype.takeRecords").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Impure);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
    assert!(sem.effects.contains(EffectSet::READS_GLOBAL));
    assert!(sem.effects.contains(EffectSet::WRITES_GLOBAL));
  }

  #[test]
  fn object_keys_allocates() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Object.keys").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn object_create_is_allocating() {
    let kb = crate::load_default_api_database();
    let api = kb.get("Object.create").unwrap();
    let sem = eval_api_call(api, &CallSiteInfo::default());
    assert_eq!(sem.purity, Purity::Allocating);
    assert!(sem.effects.contains(EffectSet::ALLOCATES));
  }
}
