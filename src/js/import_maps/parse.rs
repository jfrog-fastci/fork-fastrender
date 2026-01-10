use std::collections::HashMap;
use std::fmt;

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use url::Url;

use super::limits::ImportMapLimits;
use super::types::{
  code_unit_cmp, ImportMap, ImportMapError, ImportMapParseResult, ImportMapWarning, ImportMapWarningKind,
  ModuleIntegrityMap, ModuleSpecifierMap, ScopesMap,
};

const LIMIT_EXCEEDED_ERROR_PREFIX: &str = "__fastrender_import_map_limit_exceeded__:";

fn serde_json_error_to_import_map_error(err: serde_json::Error) -> ImportMapError {
  let s = err.to_string();
  if let Some(rest) = s.strip_prefix(LIMIT_EXCEEDED_ERROR_PREFIX) {
    let detail = rest.split(" at line ").next().unwrap_or(rest).to_string();
    return ImportMapError::LimitExceeded(detail);
  }
  ImportMapError::Json(err)
}

fn de_limit_exceeded<E: serde::de::Error>(detail: String) -> E {
  E::custom(format!("{LIMIT_EXCEEDED_ERROR_PREFIX}{detail}"))
}

#[derive(Debug, Clone, PartialEq)]
enum JsonStringOrOther {
  String(String),
  Other,
}

#[derive(Debug, Clone, PartialEq)]
enum JsonObjectOrOther<V> {
  Object(Vec<(String, V)>),
  Other,
}

#[derive(Debug, Default)]
struct ParsedImportMapJson {
  imports: Option<JsonObjectOrOther<JsonStringOrOther>>,
  scopes: Option<JsonObjectOrOther<JsonObjectOrOther<JsonStringOrOther>>>,
  integrity: Option<JsonObjectOrOther<JsonStringOrOther>>,
  unknown_top_level_keys: Vec<String>,
}

#[derive(Debug)]
enum ParsedTopLevel {
  Object(ParsedImportMapJson),
  Other,
}

#[derive(Debug, Default)]
struct ImportMapEntryCounter {
  total_entries: usize,
}

impl ImportMapEntryCounter {
  fn bump_total_entries<E: serde::de::Error>(&mut self, limits: &ImportMapLimits) -> Result<(), E> {
    self.total_entries = self.total_entries.saturating_add(1);
    if self.total_entries > limits.max_total_entries {
      return Err(de_limit_exceeded(format!(
        "max_total_entries exceeded ({} > max {})",
        self.total_entries, limits.max_total_entries
      )));
    }
    Ok(())
  }
}

struct ImportMapTopLevelSeed<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
}

impl<'de> DeserializeSeed<'de> for ImportMapTopLevelSeed<'_> {
  type Value = ParsedTopLevel;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(ImportMapTopLevelVisitor {
      limits: self.limits,
      counter: self.counter,
    })
  }
}

struct ImportMapTopLevelVisitor<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
}

impl<'de> Visitor<'de> for ImportMapTopLevelVisitor<'_> {
  type Value = ParsedTopLevel;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("an import map JSON value")
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut out = ParsedImportMapJson::default();
    while let Some(key) = map.next_key::<String>()? {
      if key.len() > self.limits.max_key_bytes {
        return Err(de_limit_exceeded(format!(
          "top-level key exceeded max_key_bytes ({} > max {})",
          key.len(),
          self.limits.max_key_bytes
        )));
      }

      match key.as_str() {
        "imports" => {
          let value = map.next_value_seed(StringMapOrOtherSeed {
            limits: self.limits,
            counter: self.counter,
            max_entries: self.limits.max_imports_entries,
            kind: "\"imports\"",
          })?;
          out.imports = Some(value);
        }
        "scopes" => {
          let value = map.next_value_seed(ScopesMapOrOtherSeed {
            limits: self.limits,
            counter: self.counter,
          })?;
          out.scopes = Some(value);
        }
        "integrity" => {
          let value = map.next_value_seed(StringMapOrOtherSeed {
            limits: self.limits,
            counter: self.counter,
            max_entries: self.limits.max_integrity_entries,
            kind: "\"integrity\"",
          })?;
          out.integrity = Some(value);
        }
        _ => {
          // Unknown top-level keys are warnings; their values are ignored.
          let _ignored: IgnoredAny = map.next_value()?;
          out.unknown_top_level_keys.push(key);
        }
      }
    }
    Ok(ParsedTopLevel::Object(out))
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    while let Some(_ignored) = seq.next_element::<IgnoredAny>()? {}
    Ok(ParsedTopLevel::Other)
  }

  fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_string<E>(self, _v: String) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(ParsedTopLevel::Other)
  }
}

struct JsonStringOrOtherSeed<'a> {
  limits: &'a ImportMapLimits,
}

impl<'de> DeserializeSeed<'de> for JsonStringOrOtherSeed<'_> {
  type Value = JsonStringOrOther;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(JsonStringOrOtherVisitor { limits: self.limits })
  }
}

struct JsonStringOrOtherVisitor<'a> {
  limits: &'a ImportMapLimits,
}

impl<'de> Visitor<'de> for JsonStringOrOtherVisitor<'_> {
  type Value = JsonStringOrOther;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a JSON string or other JSON value")
  }

  fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    if v.len() > self.limits.max_value_bytes {
      return Err(de_limit_exceeded(format!(
        "string value exceeded max_value_bytes ({} > max {})",
        v.len(),
        self.limits.max_value_bytes
      )));
    }
    Ok(JsonStringOrOther::String(v.to_string()))
  }

  fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    if v.len() > self.limits.max_value_bytes {
      return Err(de_limit_exceeded(format!(
        "string value exceeded max_value_bytes ({} > max {})",
        v.len(),
        self.limits.max_value_bytes
      )));
    }
    Ok(JsonStringOrOther::String(v))
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    while let Some(_ignored) = seq.next_element::<IgnoredAny>()? {}
    Ok(JsonStringOrOther::Other)
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    while let Some((_k, _v)) = map.next_entry::<IgnoredAny, IgnoredAny>()? {}
    Ok(JsonStringOrOther::Other)
  }

  fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
    Ok(JsonStringOrOther::Other)
  }

  fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
    Ok(JsonStringOrOther::Other)
  }

  fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
    Ok(JsonStringOrOther::Other)
  }

  fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    Ok(JsonStringOrOther::Other)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(JsonStringOrOther::Other)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(JsonStringOrOther::Other)
  }
}

struct StringMapOrOtherSeed<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
  max_entries: usize,
  kind: &'static str,
}

impl<'de> DeserializeSeed<'de> for StringMapOrOtherSeed<'_> {
  type Value = JsonObjectOrOther<JsonStringOrOther>;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(StringMapOrOtherVisitor {
      limits: self.limits,
      counter: self.counter,
      max_entries: self.max_entries,
      kind: self.kind,
    })
  }
}

struct StringMapOrOtherVisitor<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
  max_entries: usize,
  kind: &'static str,
}

impl<'de> Visitor<'de> for StringMapOrOtherVisitor<'_> {
  type Value = JsonObjectOrOther<JsonStringOrOther>;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a JSON object or other JSON value")
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut entries: Vec<(String, JsonStringOrOther)> = Vec::new();
    let mut count_in_map = 0usize;
    while let Some(key) = map.next_key::<String>()? {
      if key.len() > self.limits.max_key_bytes {
        return Err(de_limit_exceeded(format!(
          "{} key exceeded max_key_bytes ({} > max {})",
          self.kind,
          key.len(),
          self.limits.max_key_bytes
        )));
      }

      count_in_map = count_in_map.saturating_add(1);
      if count_in_map > self.max_entries {
        return Err(de_limit_exceeded(format!(
          "{} exceeded max entries ({} > max {})",
          self.kind, count_in_map, self.max_entries
        )));
      }

      self.counter.bump_total_entries(self.limits)?;

      let value = map.next_value_seed(JsonStringOrOtherSeed { limits: self.limits })?;
      entries.push((key, value));
    }
    Ok(JsonObjectOrOther::Object(entries))
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    while let Some(_ignored) = seq.next_element::<IgnoredAny>()? {}
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_string<E>(self, _v: String) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }
}

struct ScopesMapOrOtherSeed<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
}

impl<'de> DeserializeSeed<'de> for ScopesMapOrOtherSeed<'_> {
  type Value = JsonObjectOrOther<JsonObjectOrOther<JsonStringOrOther>>;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(ScopesMapOrOtherVisitor {
      limits: self.limits,
      counter: self.counter,
    })
  }
}

struct ScopesMapOrOtherVisitor<'a> {
  limits: &'a ImportMapLimits,
  counter: &'a mut ImportMapEntryCounter,
}

impl<'de> Visitor<'de> for ScopesMapOrOtherVisitor<'_> {
  type Value = JsonObjectOrOther<JsonObjectOrOther<JsonStringOrOther>>;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a JSON object or other JSON value")
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut entries: Vec<(String, JsonObjectOrOther<JsonStringOrOther>)> = Vec::new();
    let mut scope_count = 0usize;
    while let Some(scope_prefix) = map.next_key::<String>()? {
      if scope_prefix.len() > self.limits.max_key_bytes {
        return Err(de_limit_exceeded(format!(
          "\"scopes\" prefix exceeded max_key_bytes ({} > max {})",
          scope_prefix.len(),
          self.limits.max_key_bytes
        )));
      }

      scope_count = scope_count.saturating_add(1);
      if scope_count > self.limits.max_scopes {
        return Err(de_limit_exceeded(format!(
          "\"scopes\" exceeded max_scopes ({} > max {})",
          scope_count, self.limits.max_scopes
        )));
      }

      // Count scope prefixes toward the global total.
      self.counter.bump_total_entries(self.limits)?;

      let scope_value = map.next_value_seed(StringMapOrOtherSeed {
        limits: self.limits,
        counter: self.counter,
        max_entries: self.limits.max_scope_entries,
        kind: "scope",
      })?;

      entries.push((scope_prefix, scope_value));
    }
    Ok(JsonObjectOrOther::Object(entries))
  }

  fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    while let Some(_ignored) = seq.next_element::<IgnoredAny>()? {}
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_string<E>(self, _v: String) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(JsonObjectOrOther::Other)
  }
}

/// Parse and normalize an import map string per the WHATWG HTML Standard.
pub fn parse_import_map_string(
  input: &str,
  base_url: &Url,
) -> Result<(ImportMap, Vec<ImportMapWarning>), ImportMapError> {
  parse_import_map_string_with_limits(input, base_url, &ImportMapLimits::default())
}

/// WHATWG HTML: "create an import map parse result".
pub fn create_import_map_parse_result(input: &str, base_url: &Url) -> ImportMapParseResult {
  create_import_map_parse_result_with_limits(input, base_url, &ImportMapLimits::default())
}

/// WHATWG HTML: "resolve a URL-like module specifier".
///
/// Exposed within the `import_maps` module so resolution + merge code can share the canonicalizer.
pub(super) fn resolve_url_like_module_specifier(specifier: &str, base_url: &Url) -> Option<Url> {
  if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
    base_url.join(specifier).ok()
  } else {
    Url::parse(specifier).ok()
  }
}

fn resolve_import_map_address(address: &str, base_url: &Url) -> Option<Url> {
  resolve_url_like_module_specifier(address, base_url)
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
  original_map: &[(String, JsonStringOrOther)],
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
) -> ModuleSpecifierMap {
  let mut normalized: HashMap<String, Option<Url>> = HashMap::new();

  for (specifier_key, value) in original_map {
    let Some(normalized_specifier_key) = normalize_specifier_key(specifier_key, base_url, warnings) else {
      continue;
    };

    let JsonStringOrOther::String(address) = value else {
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
  original_map: &[(String, JsonObjectOrOther<JsonStringOrOther>)],
  base_url: &Url,
  warnings: &mut Vec<ImportMapWarning>,
  limits: &ImportMapLimits,
) -> Result<ScopesMap, ImportMapError> {
  let mut normalized: HashMap<String, ModuleSpecifierMap> = HashMap::new();

  for (scope_prefix, potential_specifier_map) in original_map {
    let JsonObjectOrOther::Object(map) = potential_specifier_map else {
      return Err(ImportMapError::TypeError(format!(
        "the value of the scope with prefix {scope_prefix} needs to be a JSON object."
      )));
    };

    if map.len() > limits.max_scope_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "scope {scope_prefix:?} exceeded max_scope_entries ({} > max {})",
        map.len(),
        limits.max_scope_entries
      )));
    }

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
  original_map: &[(String, JsonStringOrOther)],
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

    let JsonStringOrOther::String(metadata) = value else {
      warnings.push(ImportMapWarning::new(
        ImportMapWarningKind::IntegrityValueNotString { key: key.clone() },
      ));
      continue;
    };

    // Preserve insertion order but dedupe by resolved key (last wins).
    let resolved_key = resolved_url.to_string();
    if let Some((_, existing_value)) = entries.iter_mut().find(|(k, _)| k == &resolved_key) {
      *existing_value = metadata.clone();
    } else {
      entries.push((resolved_key, metadata.clone()));
    }
  }

  ModuleIntegrityMap { entries }
}

/// Parse and normalize an import map string per the WHATWG HTML Standard with deterministic limits.
pub fn parse_import_map_string_with_limits(
  input: &str,
  base_url: &Url,
  limits: &ImportMapLimits,
) -> Result<(ImportMap, Vec<ImportMapWarning>), ImportMapError> {
  if input.len() > limits.max_bytes {
    return Err(ImportMapError::LimitExceeded(format!(
      "input exceeded max_bytes ({} > max {})",
      input.len(),
      limits.max_bytes
    )));
  }

  let mut de = serde_json::Deserializer::from_str(input);
  let mut counter = ImportMapEntryCounter::default();
  let top_level = ImportMapTopLevelSeed { limits, counter: &mut counter }
    .deserialize(&mut de)
    .map_err(serde_json_error_to_import_map_error)?;
  de.end().map_err(serde_json_error_to_import_map_error)?;

  let ParsedTopLevel::Object(parsed) = top_level else {
    return Err(ImportMapError::TypeError(
      "top-level value needs to be a JSON object.".to_string(),
    ));
  };

  let mut warnings = Vec::new();
  for key in parsed.unknown_top_level_keys {
    warnings.push(ImportMapWarning::new(
      ImportMapWarningKind::UnknownTopLevelKey { key },
    ));
  }

  let mut imports = ModuleSpecifierMap::default();
  if let Some(imports_value) = parsed.imports {
    let JsonObjectOrOther::Object(map) = imports_value else {
      return Err(ImportMapError::TypeError(
        "the value for the \"imports\" top-level key needs to be a JSON object.".to_string(),
      ));
    };

    // Defense in depth: enforce limits post-parse too.
    if map.len() > limits.max_imports_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"imports\" exceeded max_imports_entries ({} > max {})",
        map.len(),
        limits.max_imports_entries
      )));
    }

    imports = sort_and_normalize_module_specifier_map(&map, base_url, &mut warnings);
  }

  let mut scopes = ScopesMap::default();
  if let Some(scopes_value) = parsed.scopes {
    let JsonObjectOrOther::Object(map) = scopes_value else {
      return Err(ImportMapError::TypeError(
        "the value for the \"scopes\" top-level key needs to be a JSON object.".to_string(),
      ));
    };

    if map.len() > limits.max_scopes {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"scopes\" exceeded max_scopes ({} > max {})",
        map.len(),
        limits.max_scopes
      )));
    }

    scopes = sort_and_normalize_scopes(&map, base_url, &mut warnings, limits)?;
  }

  let mut integrity = ModuleIntegrityMap::default();
  if let Some(integrity_value) = parsed.integrity {
    let JsonObjectOrOther::Object(map) = integrity_value else {
      return Err(ImportMapError::TypeError(
        "the value for the \"integrity\" top-level key needs to be a JSON object.".to_string(),
      ));
    };

    if map.len() > limits.max_integrity_entries {
      return Err(ImportMapError::LimitExceeded(format!(
        "\"integrity\" exceeded max_integrity_entries ({} > max {})",
        map.len(),
        limits.max_integrity_entries
      )));
    }

    integrity = normalize_module_integrity_map(&map, base_url, &mut warnings);
  }

  let import_map = ImportMap {
    imports,
    scopes,
    integrity,
  };
  limits.validate_import_map(&import_map)?;

  Ok((import_map, warnings))
}

/// WHATWG HTML: "create an import map parse result" with deterministic limits.
pub fn create_import_map_parse_result_with_limits(
  input: &str,
  base_url: &Url,
  limits: &ImportMapLimits,
) -> ImportMapParseResult {
  match parse_import_map_string_with_limits(input, base_url, limits) {
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
