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

  let effects = api.effects_for_call(&arg_effects);
  let purity_from_template = api.purity_for_call(&arg_purity);
  let purity_from_effects = effects.inferred_purity();

  CallSemantics {
    effects,
    purity: Purity::join(purity_from_template, purity_from_effects),
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

  let mut arg_effects = vec![unknown_effects(); len];
  let mut arg_purity = vec![Purity::Impure; len];

  // We only model argument 0 (the callback) via `CallSiteInfo`. All other args are
  // treated as unknown/impure.
  let cb_effects = site.callback_effects.unwrap_or_else(unknown_effects);
  let cb_purity = site
    .callback_purity
    .unwrap_or_else(|| site.callback_effects.map(|e| e.inferred_purity()).unwrap_or(Purity::Impure));

  arg_effects[0] = cb_effects;
  arg_purity[0] = cb_purity;

  (arg_effects, arg_purity)
}

fn unknown_effects() -> EffectSet {
  EffectSet::UNKNOWN | EffectSet::MAY_THROW
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
}
