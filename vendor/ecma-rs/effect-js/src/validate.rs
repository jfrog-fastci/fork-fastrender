use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use effect_model::{EffectSet, EffectSummary, EffectTemplate, PurityTemplate};
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
    && a.properties == b.properties
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
    for alias in api.aliases.iter().map(|s| s.as_str()).chain(node_alias) {
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

    // Validate argument-dependence templates (e.g. callback-dependent APIs).
    let mut depends_on_args = BTreeSet::<usize>::new();
    let mut saw_depends_template = false;
    if let EffectTemplate::DependsOnArgs { args, .. } = &api.effects {
      saw_depends_template = true;
      depends_on_args.extend(args.iter().copied());
    }
    if let PurityTemplate::DependsOnArgs { args, .. } = &api.purity {
      saw_depends_template = true;
      depends_on_args.extend(args.iter().copied());
    }

    if saw_depends_template {
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

    // Catch obvious semantic contradictions.
    if matches!(api.purity, PurityTemplate::Pure) {
      if api
        .effect_summary
        .flags
        .intersects(EffectSet::IO | EffectSet::NETWORK)
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
  use effect_model::{Purity, ThrowBehavior};
  use knowledge_base::ApiKind;
  use knowledge_base::JsonValue;

  fn effect_set_to_summary(effects: EffectSet) -> EffectSummary {
    EffectSummary {
      flags: effects & !EffectSet::MAY_THROW,
      throws: if effects.contains(EffectSet::MAY_THROW) {
        ThrowBehavior::Maybe
      } else {
        ThrowBehavior::Never
      },
    }
  }

  fn effect_template_to_summary(template: &EffectTemplate) -> EffectSummary {
    match template {
      EffectTemplate::Pure => EffectSummary::PURE,
      EffectTemplate::Io => effect_set_to_summary(EffectSet::IO | EffectSet::MAY_THROW),
      EffectTemplate::Custom(base) => effect_set_to_summary(*base),
      EffectTemplate::DependsOnArgs { base, .. } => effect_set_to_summary(*base),
      // Unknown means "unknown effects"; treat it as potentially-throwing for validation.
      EffectTemplate::Unknown => effect_set_to_summary(EffectSet::UNKNOWN | EffectSet::MAY_THROW),
    }
  }

  fn api(
    name: &str,
    effects: EffectTemplate,
    purity: PurityTemplate,
    properties: &[(&str, JsonValue)],
  ) -> ApiSemantics {
    let effect_summary = effect_template_to_summary(&effects);
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
