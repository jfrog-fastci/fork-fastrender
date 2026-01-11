use std::collections::BTreeMap;
use std::fmt;

use effect_model::{EffectFlags, EffectSummary, EffectTemplate, PurityTemplate};
use knowledge_base::{ApiDatabase, ApiSemantics};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
  DuplicateApiName {
    name: String,
    first: String,
    second: String,
  },
  UnknownEnumString {
    api: String,
    field: String,
    value: String,
  },
  InvalidDependsOnArgsIndex {
    api: String,
    index: i64,
  },
  EmptyDependsOnArgs {
    api: String,
  },
  InconsistentPurityEffects {
    api: String,
    purity: PurityTemplate,
    effects: EffectSummary,
  },
  PropertyGetHasArgs {
    api: String,
  },
}

impl fmt::Display for ValidationError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ValidationError::DuplicateApiName { name, first, second } => {
        write!(f, "duplicate API name `{name}` for `{first}` and `{second}`")
      }
      ValidationError::UnknownEnumString { api, field, value } => {
        write!(f, "API `{api}` has unknown value `{value}` for `{field}`")
      }
      ValidationError::InvalidDependsOnArgsIndex { api, index } => {
        write!(f, "API `{api}` has invalid depends_on_args index {index}")
      }
      ValidationError::EmptyDependsOnArgs { api } => {
        write!(f, "API `{api}` has empty depends_on_args list")
      }
      ValidationError::InconsistentPurityEffects { api, purity, effects } => write!(
        f,
        "API `{api}` has inconsistent purity/effects: purity={purity:?} effects={effects:?}"
      ),
      ValidationError::PropertyGetHasArgs { api } => {
        write!(f, "API `{api}` looks like a property_get but contains arguments")
      }
    }
  }
}

impl std::error::Error for ValidationError {}

fn semantics_match(a: &ApiSemantics, b: &ApiSemantics) -> bool {
  a.effects == b.effects
    && a.effect_summary == b.effect_summary
    && a.purity == b.purity
    && a.async_ == b.async_
    && a.idempotent == b.idempotent
    && a.deterministic == b.deterministic
    && a.parallelizable == b.parallelizable
    && a.semantics == b.semantics
    && a.signature == b.signature
    && a.since == b.since
    && a.until == b.until
    && a.kind == b.kind
    && a.properties == b.properties
}

fn parse_int_list(raw: &str) -> Result<Vec<i64>, ()> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(Vec::new());
  }

  let raw = raw.trim_matches(|ch| matches!(ch, '[' | ']' | '(' | ')' | '{' | '}'));
  let mut out = Vec::new();

  for token in raw.split(|ch: char| ch == ',' || ch.is_whitespace()) {
    let token = token.trim();
    if token.is_empty() {
      continue;
    }
    out.push(token.parse::<i64>().map_err(|_| ())?);
  }

  Ok(out)
}

fn validate_encoding_enum(
  api: &ApiSemantics,
  field: &str,
  value: &str,
  allowed: &[&str],
  errors: &mut Vec<ValidationError>,
) {
  if allowed.iter().any(|v| v == &value) {
    return;
  }

  errors.push(ValidationError::UnknownEnumString {
    api: api.name.clone(),
    field: field.to_string(),
    value: value.to_string(),
  });
}

pub fn validate(db: &ApiDatabase) -> Result<(), Vec<ValidationError>> {
  const MAX_DEPENDS_ON_ARG_INDEX: i64 = 10_000;

  let mut errors = Vec::new();

  // Detect ambiguous/duplicate name spellings from aliases.
  let mut alias_map = BTreeMap::<String, String>::new();
  for (_, api) in db.iter() {
    let node_alias = api.name.strip_prefix("node:");
    for alias in api.aliases.iter().map(|s| s.as_str()).chain(node_alias) {
      if alias.is_empty() || alias == api.name.as_str() {
        continue;
      }

      if let Some(prev) = db.get(alias) {
        if semantics_match(prev, api) {
          continue;
        }
        errors.push(ValidationError::DuplicateApiName {
          name: alias.to_string(),
          first: prev.name.clone(),
          second: api.name.clone(),
        });
        continue;
      }

      if let Some(prev) = alias_map.insert(alias.to_string(), api.name.clone()) {
        errors.push(ValidationError::DuplicateApiName {
          name: alias.to_string(),
          first: prev,
          second: api.name.clone(),
        });
      }
    }
  }

  for (_, api) in db.iter() {
    // Ensure canonical API names are "path-like" and do not include call syntax.
    if api.name.contains('(') || api.name.contains(')') {
      errors.push(ValidationError::PropertyGetHasArgs {
        api: api.name.clone(),
      });
    }

    // Validate enum-like string metadata that `effect-js` interprets.
    if let Some(value) = api.properties.get("encoding.output") {
      if let Some(value) = value.as_str() {
        validate_encoding_enum(
          api,
          "encoding.output",
          value,
          &["ascii", "latin1", "utf8", "unknown", "same_as_input"],
          &mut errors,
        );
      } else {
        errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "encoding.output".to_string(),
          value: value.to_string(),
        });
      }
    }
    if let Some(value) = api.properties.get("encoding.preserves_input_if") {
      if let Some(value) = value.as_str() {
        validate_encoding_enum(
          api,
          "encoding.preserves_input_if",
          value,
          &["ascii", "latin1", "utf8", "unknown"],
          &mut errors,
        );
      } else {
        errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "encoding.preserves_input_if".to_string(),
          value: value.to_string(),
        });
      }
    }
    if let Some(value) = api.properties.get("encoding.length_preserving_if") {
      if let Some(value) = value.as_str() {
        validate_encoding_enum(
          api,
          "encoding.length_preserving_if",
          value,
          &["ascii", "latin1", "utf8", "unknown"],
          &mut errors,
        );
      } else {
        errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "encoding.length_preserving_if".to_string(),
          value: value.to_string(),
        });
      }
    }

    // Validate callback-dependence metadata when present.
    if matches!(api.effects, EffectTemplate::DependsOnCallback)
      || matches!(api.purity, PurityTemplate::DependsOnCallback)
    {
      if let Some(raw) = api.properties.get("depends_on_args") {
        let depends = {
          let from_array = raw.as_array().map(|arr| {
            let mut out = Vec::new();
            for item in arr {
              if let Some(n) = item.as_i64() {
                out.push(n);
              } else if let Some(s) = item.as_str() {
                out.extend(parse_int_list(s)?);
              } else {
                return Err(());
              }
            }
            Ok(out)
          });
          if let Some(from_array) = from_array {
            from_array
          } else if let Some(n) = raw.as_i64() {
            Ok(vec![n])
          } else if let Some(s) = raw.as_str() {
            parse_int_list(s)
          } else {
            Err(())
          }
        };

        let depends = match depends {
          Ok(list) => list,
          Err(_) => {
            errors.push(ValidationError::UnknownEnumString {
              api: api.name.clone(),
              field: "depends_on_args".to_string(),
              value: raw.to_string(),
            });
            Vec::new()
          }
        };

        if depends.is_empty() {
          errors.push(ValidationError::EmptyDependsOnArgs {
            api: api.name.clone(),
          });
        }

        for index in depends {
          if index < 0 || index > MAX_DEPENDS_ON_ARG_INDEX {
            errors.push(ValidationError::InvalidDependsOnArgsIndex {
              api: api.name.clone(),
              index,
            });
          }
        }
      }
    }

    // Catch obvious semantic contradictions.
    if matches!(api.purity, PurityTemplate::Pure) {
      if api
        .effect_summary
        .flags
        .intersects(EffectFlags::IO | EffectFlags::NETWORK)
      {
        errors.push(ValidationError::InconsistentPurityEffects {
          api: api.name.clone(),
          purity: api.purity.clone(),
          effects: api.effect_summary,
        });
      }
    }
  }

  if errors.is_empty() {
    Ok(())
  } else {
    Err(errors)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::{EffectFlags, EffectSummary, ThrowBehavior};
  use serde_json::Value as JsonValue;
  use std::collections::BTreeMap;

  fn api(
    name: &str,
    effects: EffectTemplate,
    purity: PurityTemplate,
    properties: &[(&str, JsonValue)],
  ) -> ApiSemantics {
    let effect_summary = crate::effect_template_to_summary(&effects);
    ApiSemantics {
      id: knowledge_base::ApiId::from_name(name),
      name: name.to_string(),
      aliases: Vec::new(),
      effects,
      effect_summary,
      purity,
      async_: None,
      idempotent: None,
      deterministic: None,
      parallelizable: None,
      semantics: None,
      signature: None,
      since: None,
      until: None,
      kind: None,
      properties: properties
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect::<BTreeMap<_, _>>(),
    }
  }

  #[test]
  fn flags_pure_with_io_as_inconsistent() {
    let db = ApiDatabase::from_entries([api(
      "fs.readFileSync",
      EffectTemplate::Io,
      PurityTemplate::Pure,
      &[],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::InconsistentPurityEffects { .. }
    )));
  }

  #[test]
  fn validates_encoding_output_enum() {
    let db = ApiDatabase::from_entries([api(
      "String.prototype.slice",
      EffectTemplate::Custom(EffectSummary {
        flags: EffectFlags::ALLOCATES,
        throws: ThrowBehavior::Never,
      }),
      PurityTemplate::Pure,
      &[("encoding.output", JsonValue::String("bogus".to_string()))],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::UnknownEnumString { field, .. } if field == "encoding.output"
    )));
  }

  #[test]
  fn validates_depends_on_args_indices() {
    let db = ApiDatabase::from_entries([api(
      "Array.prototype.map",
      EffectTemplate::DependsOnCallback,
      PurityTemplate::DependsOnCallback,
      &[("depends_on_args", JsonValue::Array(vec![JsonValue::from(-1), JsonValue::from(10001)]))],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::InvalidDependsOnArgsIndex { index: -1, .. }
    )));
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::InvalidDependsOnArgsIndex { index: 10001, .. }
    )));
  }

  #[test]
  fn validates_depends_on_args_empty_list() {
    let db = ApiDatabase::from_entries([api(
      "Array.prototype.map",
      EffectTemplate::DependsOnCallback,
      PurityTemplate::DependsOnCallback,
      &[("depends_on_args", JsonValue::Array(Vec::new()))],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::EmptyDependsOnArgs { .. }
    )));
  }
}
