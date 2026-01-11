#![deny(missing_debug_implementations)]

mod api;
mod recognize;
mod resolve;

#[cfg(feature = "typed")]
pub mod typed;

use effect_model::{EffectFlags, EffectSummary, EffectTemplate, Purity, PurityTemplate, ThrowBehavior};

pub use api::ApiId;
pub use recognize::{recognize_patterns_untyped, RecognizedPattern};

#[cfg(feature = "typed")]
pub use recognize::recognize_patterns_typed;

pub use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase, ApiSemantics};

pub fn effect_template_to_summary(template: &EffectTemplate) -> EffectSummary {
  match template {
    EffectTemplate::Pure => EffectSummary::PURE,
    EffectTemplate::Io => EffectSummary {
      flags: EffectFlags::IO,
      throws: ThrowBehavior::Maybe,
    },
    EffectTemplate::DependsOnCallback => EffectSummary {
      flags: EffectFlags::empty(),
      throws: ThrowBehavior::Maybe,
    },
    EffectTemplate::Custom(summary) => *summary,
    EffectTemplate::Unknown => EffectSummary {
      flags: EffectFlags::all(),
      throws: ThrowBehavior::Maybe,
    },
  }
}

pub fn purity_template_to_purity(template: &PurityTemplate) -> Purity {
  match template {
    PurityTemplate::Pure => Purity::Pure,
    PurityTemplate::ReadOnly => Purity::ReadOnly,
    PurityTemplate::DependsOnCallback => Purity::Unknown,
    PurityTemplate::Impure => Purity::Impure,
    PurityTemplate::Unknown => Purity::Unknown,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn concretize_effect_template() {
    assert!(effect_template_to_summary(&EffectTemplate::Pure).is_pure());

    let io = effect_template_to_summary(&EffectTemplate::Io);
    assert!(io.flags.contains(EffectFlags::IO));
    assert_eq!(io.throws, ThrowBehavior::Maybe);

    let custom = EffectTemplate::Custom(EffectSummary {
      flags: EffectFlags::ALLOCATES,
      throws: ThrowBehavior::Never,
    });
    assert_eq!(
      effect_template_to_summary(&custom),
      EffectSummary {
        flags: EffectFlags::ALLOCATES,
        throws: ThrowBehavior::Never
      }
    );
  }

  #[test]
  fn concretize_purity_template() {
    assert_eq!(purity_template_to_purity(&PurityTemplate::Pure), Purity::Pure);
    assert_eq!(
      purity_template_to_purity(&PurityTemplate::ReadOnly),
      Purity::ReadOnly
    );
    assert_eq!(
      purity_template_to_purity(&PurityTemplate::DependsOnCallback),
      Purity::Unknown
    );
  }
}
