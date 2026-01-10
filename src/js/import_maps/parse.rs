use std::collections::HashMap;
use std::fmt;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use url::Url;

use super::types::{
  code_unit_cmp, ImportMap, ImportMapError, ImportMapParseResult, ImportMapWarning, ImportMapWarningKind,
  ModuleIntegrityMap, ModuleSpecifierMap, ScopesMap,
};

/// Parse and normalize an import map string per the WHATWG HTML Standard.
pub fn parse_import_map_string(
  input: &str,
  base_url: &Url,
) -> Result<(ImportMap, Vec<ImportMapWarning>), ImportMapError> {
  let parsed: OrderedJsonValue = serde_json::from_str(input)?;
  let OrderedJsonValue::Object(top_level) = parsed else {
    return Err(ImportMapError::TypeError(
      "top-level value needs to be a JSON object".to_string(),
    ));
  };

  let mut warnings = Vec::new();

  for (key, _) in &top_level {
    if key != "imports" && key != "scopes" && key != "integrity" {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::UnknownTopLevelKey { key: key.clone() },
      ));
    }
  }

  let mut imports = ModuleSpecifierMap::default();
  if let Some(imports_value) = get_last_property(&top_level, "imports") {
    let OrderedJsonValue::Object(map) = imports_value else {
      return Err(ImportMapError::TypeError(
        r#"value for the "imports" top-level key needs to be a JSON object."#.to_string(),
      ));
    };
    imports = sort_and_normalize_module_specifier_map(map, base_url, &mut warnings);
  }

  let mut scopes = ScopesMap::default();
  if let Some(scopes_value) = get_last_property(&top_level, "scopes") {
    let OrderedJsonValue::Object(map) = scopes_value else {
      return Err(ImportMapError::TypeError(
        r#"value for the "scopes" top-level key needs to be a JSON object."#.to_string(),
      ));
    };
    scopes = sort_and_normalize_scopes(map, base_url, &mut warnings)?;
  }

  let mut integrity = ModuleIntegrityMap::default();
  if let Some(integrity_value) = get_last_property(&top_level, "integrity") {
    let OrderedJsonValue::Object(map) = integrity_value else {
      return Err(ImportMapError::TypeError(
        r#"value for the "integrity" top-level key needs to be a JSON object."#.to_string(),
      ));
    };
    integrity = normalize_module_integrity_map(map, base_url, &mut warnings);
  }

  Ok((ImportMap { imports, scopes, integrity }, warnings))
}

/// WHATWG HTML: "create an import map parse result".
pub fn create_import_map_parse_result(input: &str, base_url: &Url) -> ImportMapParseResult {
  match parse_import_map_string(input, base_url) {
    Ok((import_map, warnings)) => ImportMapParseResult {
      import_map: Some(import_map),
      error_to_rethrow: None,
      warnings,
    },
    Err(err) => ImportMapParseResult {
      import_map: None,
      error_to_rethrow: Some(err),
      warnings: Vec::new(),
    },
  }
}

fn get_last_property<'a>(
  object: &'a [(String, OrderedJsonValue)],
  key: &str,
) -> Option<&'a OrderedJsonValue> {
  object
    .iter()
    .rev()
    .find(|(k, _)| k == key)
    .map(|(_, v)| v)
}

fn resolve_url_like_module_specifier(specifier: &str, base_url: &Url) -> Option<Url> {
  if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
    base_url.join(specifier).ok()
  } else {
    Url::parse(specifier).ok()
  }
}

fn resolve_import_map_address(address: &str, base_url: &Url) -> Option<Url> {
  base_url.join(address).ok()
}

fn normalize_specifier_key(
  specifier_key: &str,
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
) -> Option<String> {
  if specifier_key.is_empty() {
    warnings.push(ImportMapWarning::new(ImportMapWarningKind::EmptySpecifierKey));
    return None;
  }

  if let Some(url) = resolve_url_like_module_specifier(specifier_key, base_url) {
    return Some(url.to_string());
  }

  Some(specifier_key.to_string())
}

fn sort_and_normalize_module_specifier_map(
  original_map: &[(String, OrderedJsonValue)],
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
) -> ModuleSpecifierMap {
  let mut normalized: HashMap<String, Option<Url>> = HashMap::new();

  for (specifier_key, value) in original_map {
    let Some(normalized_specifier_key) =
      normalize_specifier_key(specifier_key, base_url, warnings)
    else {
      continue;
    };

    let OrderedJsonValue::String(address) = value else {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::AddressNotString {
          specifier_key: specifier_key.clone(),
        },
      ));
      normalized.insert(normalized_specifier_key, None);
      continue;
    };

    let Some(address_url) = resolve_import_map_address(address, base_url) else {
      warnings.push(ImportMapWarning::new(ImportMapWarningKind::AddressInvalid {
        specifier_key: specifier_key.clone(),
        address: address.clone(),
      }));
      normalized.insert(normalized_specifier_key, None);
      continue;
    };

    // NOTE: enforce the trailing-slash invariant using the *normalized* key.
    //
    // URL serialization can add an implicit trailing slash (e.g. "https://example.com" →
    // "https://example.com/"). Without checking `normalized_specifier_key`, a non-trailing-slash
    // input key could normalize into a prefix key and violate resolver invariants.
    if normalized_specifier_key.ends_with('/') && !address_url.as_str().ends_with('/') {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::TrailingSlashMismatch {
          specifier_key: specifier_key.clone(),
          address: address_url.to_string(),
        },
      ));
      normalized.insert(normalized_specifier_key, None);
      continue;
    }

    normalized.insert(normalized_specifier_key, Some(address_url));
  }

  let mut entries: Vec<(String, Option<Url>)> = normalized.into_iter().collect();
  entries.sort_by(|(a, _), (b, _)| code_unit_cmp(b, a));
  ModuleSpecifierMap { entries }
}

fn sort_and_normalize_scopes(
  original_map: &[(String, OrderedJsonValue)],
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
) -> Result<ScopesMap, ImportMapError> {
  let mut normalized: HashMap<String, ModuleSpecifierMap> = HashMap::new();

  for (scope_prefix, potential_specifier_map) in original_map {
    let OrderedJsonValue::Object(map) = potential_specifier_map else {
      return Err(ImportMapError::TypeError(format!(
        "value of the scope with prefix {scope_prefix} needs to be a JSON object."
      )));
    };

    let scope_prefix_url = match base_url.join(scope_prefix) {
      Ok(url) => url,
      Err(_) => {
        warnings.push(ImportMapWarning::new(
          ImportMapWarningKind::ScopePrefixNotParseable {
            prefix: scope_prefix.clone(),
          },
        ));
        continue;
      }
    };

    let normalized_scope_prefix = scope_prefix_url.to_string();
    normalized.insert(
      normalized_scope_prefix,
      sort_and_normalize_module_specifier_map(map, base_url, warnings),
    );
  }

  let mut entries: Vec<(String, ModuleSpecifierMap)> = normalized.into_iter().collect();
  entries.sort_by(|(a, _), (b, _)| code_unit_cmp(b, a));
  Ok(ScopesMap { entries })
}

fn normalize_module_integrity_map(
  original_map: &[(String, OrderedJsonValue)],
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
) -> ModuleIntegrityMap {
  let mut entries: Vec<(String, String)> = Vec::new();

  for (key, value) in original_map {
    let Some(resolved_url) = resolve_url_like_module_specifier(key, base_url) else {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::IntegrityKeyFailedToResolve { key: key.clone() },
      ));
      continue;
    };

    let OrderedJsonValue::String(metadata) = value else {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::IntegrityValueNotString { key: key.clone() },
      ));
      continue;
    };

    let resolved_key = resolved_url.to_string();
    if let Some((_, existing_value)) = entries.iter_mut().find(|(k, _)| k == &resolved_key) {
      *existing_value = metadata.clone();
    } else {
      entries.push((resolved_key, metadata.clone()));
    }
  }

  ModuleIntegrityMap { entries }
}

/// JSON value with object insertion order preserved.
#[derive(Debug, Clone, PartialEq)]
enum OrderedJsonValue {
  Null,
  Bool(bool),
  Number(serde_json::Number),
  String(String),
  Array(Vec<OrderedJsonValue>),
  Object(Vec<(String, OrderedJsonValue)>),
}

impl<'de> Deserialize<'de> for OrderedJsonValue {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(OrderedJsonVisitor)
  }
}

struct OrderedJsonVisitor;

impl<'de> Visitor<'de> for OrderedJsonVisitor {
  type Value = OrderedJsonValue;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a JSON value")
  }

  fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::Bool(v))
  }

  fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::Number(serde_json::Number::from(v)))
  }

  fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::Number(serde_json::Number::from(v)))
  }

  fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    let Some(n) = serde_json::Number::from_f64(v) else {
      return Err(E::custom("invalid number"));
    };
    Ok(OrderedJsonValue::Number(n))
  }

  fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::String(v.to_string()))
  }

  fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::String(v))
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::Null)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(OrderedJsonValue::Null)
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    let mut elements = Vec::new();
    while let Some(elem) = seq.next_element::<OrderedJsonValue>()? {
      elements.push(elem);
    }
    Ok(OrderedJsonValue::Array(elements))
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut entries = Vec::new();
    while let Some((k, v)) = map.next_entry::<String, OrderedJsonValue>()? {
      entries.push((k, v));
    }
    Ok(OrderedJsonValue::Object(entries))
  }
}
