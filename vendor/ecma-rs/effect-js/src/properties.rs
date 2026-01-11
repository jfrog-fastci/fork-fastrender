use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

use crate::{Api, CallSiteInfo};

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

pub fn is_parallelizable(api: &Api, callsite: &CallSiteInfo) -> bool {
  let requires_callback_pure = get_path(&api.properties, &["parallel", "requires_callback_pure"])
    .and_then(get_bool)
    .unwrap_or(false);
  if !requires_callback_pure {
    return false;
  }

  if callsite.callback_is_pure != Some(true) {
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
      callback_is_pure: Some(true),
      callback_uses_index: Some(true),
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
  fn output_length_relations_are_parsed() {
    let db = array_db();
    let map = db.get("Array.prototype.map").unwrap();
    let filter = db.get("Array.prototype.filter").unwrap();

    assert_eq!(output_length_relation(map), OutputLengthRelation::SameAsInput);
    assert_eq!(output_length_relation(filter), OutputLengthRelation::LeInput);
  }
}
