use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use effect_model::{EffectSet, EffectSummary, EffectTemplate, PurityTemplate};
use knowledge_base::{ApiDatabase, ApiSemantics, JsonValue};

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
    index: usize,
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
    && properties_match(&a.properties, &b.properties)
}

fn properties_match(a: &BTreeMap<String, JsonValue>, b: &BTreeMap<String, JsonValue>) -> bool {
  fn is_ignored_key(key: &str) -> bool {
    matches!(key, "effects.base" | "effects.depends_on_args" | "purity.kind")
  }

  for (key, value) in a {
    if is_ignored_key(key) {
      continue;
    }
    if b.get(key) != Some(value) {
      return false;
    }
  }
  for (key, value) in b {
    if is_ignored_key(key) {
      continue;
    }
    if a.get(key) != Some(value) {
      return false;
    }
  }
  true
}

fn normalize_ident(raw: &str) -> String {
  raw
    .trim()
    .to_ascii_lowercase()
    .replace(['-', ' '], "_")
}

fn parse_usize_list(raw: &JsonValue) -> Result<Vec<usize>, ()> {
  if let Some(arr) = raw.as_array() {
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
      if let Some(n) = item.as_u64() {
        out.push(usize::try_from(n).map_err(|_| ())?);
      } else if let Some(n) = item.as_i64() {
        out.push(usize::try_from(n).map_err(|_| ())?);
      } else {
        return Err(());
      }
    }
    return Ok(out);
  }

  if let Some(n) = raw.as_u64() {
    return Ok(vec![usize::try_from(n).map_err(|_| ())?]);
  }
  if let Some(n) = raw.as_i64() {
    return Ok(vec![usize::try_from(n).map_err(|_| ())?]);
  }

  Err(())
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
  const MAX_DEPENDS_ON_ARG_INDEX: usize = 10_000;

  let mut errors = Vec::new();

  // Detect ambiguous/duplicate name spellings from aliases.
  let mut alias_map = BTreeMap::<String, String>::new();
  for (_, api) in db.iter() {
    let node_alias = api.name.strip_prefix("node:");
    let mut seen = BTreeSet::<&str>::new();
    for alias in api.aliases.iter().map(|s| s.as_str()).chain(node_alias) {
      if !seen.insert(alias) {
        continue;
      }
      if alias.is_empty() || alias == api.name.as_str() {
        continue;
      }

      // Only treat the alias spelling as a collision with a canonical entry when that spelling
      // exists as an actual API name in the DB.
      if db.canonical_name(alias).is_some_and(|canonical| canonical == alias) {
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
      }

      if let Some(prev) = alias_map.get(alias) {
        // Allow duplicate alias spellings when they resolve to the same canonical API name.
        //
        // This happens in practice for Node APIs where the knowledge-base explicitly lists
        // aliases like `fs.readFile` alongside `node:fs.readFile`, while `effect-js` also
        // synthesizes the `node:`-stripped spelling as an implicit alias.
        if prev == &api.name {
          continue;
        }
        errors.push(ValidationError::DuplicateApiName {
          name: alias.to_string(),
          first: prev.clone(),
          second: api.name.clone(),
        });
        continue;
      }

      alias_map.insert(alias.to_string(), api.name.clone());
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

    // Validate argument-dependence templates (e.g. callback-dependent APIs).
    let mut depends_on_args = BTreeSet::<usize>::new();
    let mut saw_depends = false;
    if let EffectTemplate::DependsOnArgs { args, .. } = &api.effects {
      saw_depends = true;
      depends_on_args.extend(args.iter().copied());
    }
    if let PurityTemplate::DependsOnArgs { args, .. } = &api.purity {
      saw_depends = true;
      depends_on_args.extend(args.iter().copied());
    }

    if let Some(raw) = api.properties.get("effects.depends_on_args") {
      match parse_usize_list(raw) {
        Ok(list) => {
          saw_depends = true;
          depends_on_args.extend(list);
        }
        Err(_) => errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "effects.depends_on_args".to_string(),
          value: raw.to_string(),
        }),
      }
    }

    if saw_depends {
      if depends_on_args.is_empty() {
        errors.push(ValidationError::EmptyDependsOnArgs {
          api: api.name.clone(),
        });
      }

      for index in depends_on_args {
        if index > MAX_DEPENDS_ON_ARG_INDEX {
          errors.push(ValidationError::InvalidDependsOnArgsIndex {
            api: api.name.clone(),
            index,
          });
        }
      }
    }

      let mut base_effects = EffectSet::empty();
      if let Some(raw) = api.properties.get("effects.base") {
        if let Some(arr) = raw.as_array() {
          for token in arr {
          let Some(token) = token.as_str() else {
            errors.push(ValidationError::UnknownEnumString {
              api: api.name.clone(),
              field: "effects.base".to_string(),
              value: token.to_string(),
            });
            continue;
          };

            match normalize_ident(token).as_str() {
              "alloc" | "allocates" => base_effects |= EffectSet::ALLOCATES,
              "io" => base_effects |= EffectSet::IO,
              "network" => base_effects |= EffectSet::NETWORK,
              "nondeterministic" | "non_deterministic" => base_effects |= EffectSet::NONDETERMINISTIC,
              "reads_global" | "read_global" => base_effects |= EffectSet::READS_GLOBAL,
              "writes_global" | "write_global" => base_effects |= EffectSet::WRITES_GLOBAL,
              "may_throw" | "throws" => base_effects |= EffectSet::MAY_THROW,
              "unknown" => base_effects |= EffectSet::UNKNOWN,
              // Informational-only tags (no effect flags).
              "async" | "depends_on_callback" | "depends_on_args" => {}
              other => errors.push(ValidationError::UnknownEnumString {
              api: api.name.clone(),
              field: "effects.base".to_string(),
              value: other.to_string(),
            }),
          }
        }
      } else {
        errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "effects.base".to_string(),
          value: raw.to_string(),
        });
      }
    }

    if let Some(raw_kind) = api.properties.get("purity.kind") {
      if let Some(kind) = raw_kind.as_str() {
        // `purity.kind` is legacy/informational metadata. Be conservative and only
        // catch obvious contradictions.
        let forbidden =
          EffectSet::IO | EffectSet::NETWORK | EffectSet::READS_GLOBAL | EffectSet::WRITES_GLOBAL;
        if normalize_ident(kind) == "pure" && base_effects.intersects(forbidden) {
          errors.push(ValidationError::InconsistentPurityEffects {
            api: api.name.clone(),
            purity: PurityTemplate::Pure,
            effects: base_effects.to_effect_summary(),
          });
        }
      } else {
        errors.push(ValidationError::UnknownEnumString {
          api: api.name.clone(),
          field: "purity.kind".to_string(),
          value: raw_kind.to_string(),
        });
      }
    }

    // Catch obvious semantic contradictions.
    if matches!(api.purity, PurityTemplate::Pure) {
      // `effect_summary` is meant to reflect the base effects of `effects` (even
      // for callback-dependent templates), but some external callers may
      // construct entries with only one of them. Be conservative and check both.
      let combined = api.effects.base_effects() | api.effect_summary.to_effect_set();
      // Catch obvious semantic contradictions.
      //
      // Pure APIs may still:
      // - allocate (not externally observable)
      // - be non-deterministic (determinism is tracked separately in KB metadata)
      // - be partially unknown (conservative placeholders)
      //
      // But they should not:
      // - perform I/O
      // - read or write observable global state
      let forbidden =
        EffectSet::IO | EffectSet::NETWORK | EffectSet::READS_GLOBAL | EffectSet::WRITES_GLOBAL;
      if combined.intersects(forbidden) {
        errors.push(ValidationError::InconsistentPurityEffects {
          api: api.name.clone(),
          purity: api.purity.clone(),
          effects: combined.to_effect_summary(),
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
  use effect_model::Purity;
  use knowledge_base::ApiKind;
  use knowledge_base::JsonValue;

  fn api(
    name: &str,
    effects: EffectTemplate,
    purity: PurityTemplate,
    properties: &[(&str, JsonValue)],
  ) -> ApiSemantics {
    let effect_summary = effects.base_effects().to_effect_summary();
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
      kind: ApiKind::Function,
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
  fn flags_pure_with_global_write_as_inconsistent() {
    let db = ApiDatabase::from_entries([api(
      "document.createElement",
      EffectTemplate::Custom(EffectSet::WRITES_GLOBAL),
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
  fn flags_pure_with_global_read_as_inconsistent() {
    let db = ApiDatabase::from_entries([api(
      "document.querySelector",
      EffectTemplate::Custom(EffectSet::READS_GLOBAL),
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
  fn effects_base_accepts_global_tokens() {
    let db = ApiDatabase::from_entries([api(
      "queue.drain",
      EffectTemplate::Custom(EffectSet::READS_GLOBAL | EffectSet::WRITES_GLOBAL),
      PurityTemplate::Impure,
      &[(
        "effects.base",
        JsonValue::Array(vec![
          JsonValue::String("reads_global".to_string()),
          JsonValue::String("writes_global".to_string()),
        ]),
      )],
    )]);
    validate(&db).expect("global effects.base tokens are accepted");
  }

  #[test]
  fn purity_kind_pure_with_global_write_as_inconsistent() {
    let mut api = api(
      "document.createElement",
      EffectTemplate::Custom(EffectSet::WRITES_GLOBAL),
      PurityTemplate::Impure,
      &[(
        "effects.base",
        JsonValue::Array(vec![JsonValue::String("writes_global".to_string())]),
      )],
    );
    api
      .properties
      .insert("purity.kind".to_string(), JsonValue::String("pure".to_string()));
    let db = ApiDatabase::from_entries([api]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::InconsistentPurityEffects { .. }
    )));
  }

  #[test]
  fn purity_kind_pure_with_global_read_as_inconsistent() {
    let mut api = api(
      "document.querySelector",
      EffectTemplate::Custom(EffectSet::READS_GLOBAL),
      PurityTemplate::Impure,
      &[(
        "effects.base",
        JsonValue::Array(vec![JsonValue::String("reads_global".to_string())]),
      )],
    );
    api
      .properties
      .insert("purity.kind".to_string(), JsonValue::String("pure".to_string()));
    let db = ApiDatabase::from_entries([api]);

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
      EffectTemplate::Custom(EffectSet::ALLOCATES),
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
      EffectTemplate::DependsOnArgs {
        base: EffectSet::empty(),
        args: vec![0, 10_001],
      },
      PurityTemplate::Pure,
      &[],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::InvalidDependsOnArgsIndex { index: 10001, .. }
    )));
  }

  #[test]
  fn validates_depends_on_args_empty_list() {
    let db = ApiDatabase::from_entries([api(
      "Array.prototype.map",
      EffectTemplate::DependsOnArgs {
        base: EffectSet::ALLOCATES,
        args: vec![],
      },
      PurityTemplate::DependsOnArgs {
        base: Purity::Allocating,
        args: vec![],
      },
      &[],
    )]);

    let errs = validate(&db).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
      e,
      ValidationError::EmptyDependsOnArgs { .. }
    )));
  }
}
