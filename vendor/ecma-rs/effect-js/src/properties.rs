use knowledge_base::JsonValue;
use std::collections::BTreeMap;

use crate::{Api, CallSiteInfo, EffectSet, Purity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLengthRelation {
  SameAsInput,
  LeInput,
  Unknown,
}

fn get_path<'a>(
  props: &'a BTreeMap<String, JsonValue>,
  path: &[&str],
) -> Option<&'a JsonValue> {
  let (first, rest) = path.split_first()?;
  if let Some(mut cur) = props.get(*first) {
    let mut ok = true;
    for field in rest {
      let Some(obj) = cur.as_object() else {
        ok = false;
        break;
      };
      let Some(next) = obj.get(*field) else {
        ok = false;
        break;
      };
      cur = next;
    }
    if ok {
      return Some(cur);
    }
  }

  // Some metadata uses dotted keys (e.g. `encoding.output`). For resilience,
  // accept `fusion.fusable_with`-style spellings too.
  if path.len() > 1 {
    let dotted = path.join(".");
    return props.get(&dotted);
  }

  None
}

fn get_bool(value: &JsonValue) -> Option<bool> {
  value.as_bool().or_else(|| match value.as_str()? {
    "true" | "True" | "TRUE" => Some(true),
    "false" | "False" | "FALSE" => Some(false),
    _ => None,
  })
}

pub fn fusable_with(api: &Api, other: &Api) -> bool {
  let Some(value) = get_path(&api.properties, &["fusion", "fusable_with"]) else {
    return false;
  };

  if let Some(items) = value.as_array() {
    return items
      .iter()
      .filter_map(|v| v.as_str())
      .any(|name| name == other.name);
  }

  value.as_str() == Some(other.name.as_str())
}

pub fn output_length_relation(api: &Api) -> OutputLengthRelation {
  match get_path(&api.properties, &["output", "length_relation"]).and_then(|v| v.as_str()) {
    Some(s) => match s {
      "same_as_input" => OutputLengthRelation::SameAsInput,
      "le_input" => OutputLengthRelation::LeInput,
      "unknown" => OutputLengthRelation::Unknown,
      _ => OutputLengthRelation::Unknown,
    },
    None => OutputLengthRelation::Unknown,
  }
}

fn callback_is_pure(callsite: &CallSiteInfo) -> Option<bool> {
  if let Some(p) = callsite.callback_purity {
    return Some(matches!(p, Purity::Pure | Purity::Allocating));
  }
  callsite.callback_is_pure
}

fn callback_may_throw(callsite: &CallSiteInfo) -> Option<bool> {
  if let Some(effects) = callsite.callback_effects {
    return Some(effects.contains(EffectSet::MAY_THROW) || effects.contains(EffectSet::UNKNOWN_CALL));
  }
  callsite.callback_may_throw
}

pub fn is_parallelizable(api: &Api, callsite: &CallSiteInfo) -> bool {
  let requires_callback_pure = get_path(&api.properties, &["parallel", "requires_callback_pure"])
    .and_then(get_bool)
    .unwrap_or(false);
  let associative_if_callback_associative = get_path(
    &api.properties,
    &["reduce", "associative_if_callback_associative"],
  )
  .and_then(get_bool)
  .unwrap_or(false);
  if !requires_callback_pure && !associative_if_callback_associative {
    return false;
  }

  if callback_is_pure(callsite) != Some(true) {
    return false;
  }

  if associative_if_callback_associative && callsite.callback_is_associative != Some(true) {
    return false;
  }

  // Associativity is required for safe parallel reduction/aggregation. Prefer the
  // canonical `parallel.requires_callback_associative` key but accept the legacy
  // `reduce.associative_if_callback_associative` too.
  let requires_callback_associative =
    get_path(&api.properties, &["parallel", "requires_callback_associative"])
      .and_then(get_bool)
      .unwrap_or(false)
      || get_path(
        &api.properties,
        &["reduce", "associative_if_callback_associative"],
      )
      .and_then(get_bool)
      .unwrap_or(false);
  if requires_callback_associative && callsite.callback_is_associative != Some(true) {
    return false;
  }

  let forbid_uses_index = get_path(&api.properties, &["parallel", "forbid_uses_index"])
    .and_then(get_bool)
    .unwrap_or(false);
  if forbid_uses_index && callsite.callback_uses_index != Some(false) {
    return false;
  }

  let forbid_uses_array = get_path(&api.properties, &["parallel", "forbid_uses_array"])
    .and_then(get_bool)
    .unwrap_or(false);
  if forbid_uses_array && callsite.callback_uses_array != Some(false) {
    return false;
  }

  let forbid_may_throw = get_path(&api.properties, &["parallel", "forbid_may_throw"])
    .and_then(get_bool)
    .unwrap_or(false);
  if forbid_may_throw && callback_may_throw(callsite) != Some(false) {
    return false;
  }

  true
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{parse_api_semantics_yaml_str, ApiDatabase, CallSiteInfo};

  fn array_db() -> ApiDatabase {
    let yaml = include_str!("../../knowledge-base/core/array.yaml");
    ApiDatabase::from_entries(parse_api_semantics_yaml_str(yaml).unwrap())
  }

  #[test]
  fn map_is_parallelizable_when_callback_is_pure_and_does_not_use_index() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();

    let callsite = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(is_parallelizable(map, &callsite));
  }

  #[test]
  fn map_is_not_parallelizable_when_callback_uses_index() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();

    let callsite = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_uses_index: Some(true),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(!is_parallelizable(map, &callsite));
  }

  #[test]
  fn reduce_is_parallelizable_when_callback_is_pure_and_associative() {
    let db = array_db();
    let reduce = db.get("Array.prototype.reduce").unwrap();

    let callsite = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_is_associative: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(is_parallelizable(reduce, &callsite));
  }

  #[test]
  fn map_is_not_parallelizable_when_callback_may_throw_and_kb_forbids_it() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();

    let callsite = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::MAY_THROW),
      callback_may_throw: Some(true),
      callback_is_pure: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(!is_parallelizable(map, &callsite));
  }

  #[test]
  fn map_is_fusable_with_filter() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();
    let filter = db.get("Array.prototype.filter").unwrap();
    assert!(fusable_with(map, filter));
  }

  #[test]
  fn flat_map_is_parallelizable_when_callback_is_pure_and_does_not_use_index_or_array() {
    let db = array_db();
    let flat_map = db.get("Array.prototype.flatMap").unwrap();

    let callsite = CallSiteInfo {
      callback_is_pure: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(is_parallelizable(flat_map, &callsite));
  }

  #[test]
  fn reduce_requires_callback_associative_to_be_parallelizable() {
    let db = array_db();
    let reduce = db.get("Array.prototype.reduce").unwrap();

    let non_associative = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_is_associative: Some(false),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(!is_parallelizable(reduce, &non_associative));

    let associative = CallSiteInfo {
      callback_purity: Some(Purity::Pure),
      callback_effects: Some(EffectSet::empty()),
      callback_may_throw: Some(false),
      callback_is_pure: Some(true),
      callback_is_associative: Some(true),
      callback_uses_index: Some(false),
      callback_uses_array: Some(false),
      ..Default::default()
    };
    assert!(is_parallelizable(reduce, &associative));
  }

  #[test]
  fn array_terminal_properties_exist_for_find_some_every_for_each() {
    fn prop_bool(api: &Api, path: &[&str]) -> Option<bool> {
      get_path(&api.properties, path).and_then(get_bool)
    }

    let db = array_db();
    let for_each = db.get("Array.prototype.forEach").unwrap();
    let every = db.get("Array.prototype.every").unwrap();
    let some = db.get("Array.prototype.some").unwrap();
    let find = db.get("Array.prototype.find").unwrap();

    for api in [for_each, every, some, find] {
      assert_eq!(prop_bool(api, &["array", "terminal"]), Some(true));
    }

    for api in [every, some, find] {
      assert_eq!(prop_bool(api, &["array", "short_circuit"]), Some(true));
    }
  }

  #[test]
  fn output_length_relations_are_parsed() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();
    let filter = db.get("Array.prototype.filter").unwrap();

    assert_eq!(output_length_relation(map), OutputLengthRelation::SameAsInput);
    assert_eq!(output_length_relation(filter), OutputLengthRelation::LeInput);
  }
}
