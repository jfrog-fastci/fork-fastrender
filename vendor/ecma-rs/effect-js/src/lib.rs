#![deny(missing_debug_implementations)]

mod api;
pub mod callback;
pub mod encoding;
mod recognize;
mod resolve;
mod template_eval;

pub mod kb;
pub mod resolver;
pub mod db;
pub mod meta;
pub mod types;
pub mod signals;

#[cfg(feature = "typed")]
pub mod typed;

use effect_model::{EffectFlags, EffectSummary, EffectTemplate, Purity, PurityTemplate, ThrowBehavior};

pub use api::ApiId;
pub use callback::{analyze_inline_callback, callsite_info_for_args, CallbackInfo};
pub use encoding::{analyze_string_encodings, EncodingResult, StringEncoding};
#[cfg(feature = "typed")]
pub use encoding::analyze_string_encodings_typed;
pub use kb::load_default_api_database;
pub use db::{CallSiteInfo, EffectDb};
pub use recognize::{
  recognize_patterns_best_effort_untyped, recognize_patterns_untyped, GuardKind, RecognizedPattern,
};
pub use resolver::{collect_require_bindings, resolve_api_call, RequireBindings};

#[cfg(feature = "typed")]
pub use recognize::recognize_patterns_typed;

pub use resolve::{resolve_api_call_best_effort_untyped, resolve_api_call_untyped};

pub use knowledge_base::{Api, KnowledgeBase};
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
    PurityTemplate::Allocating => Purity::Allocating,
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
      purity_template_to_purity(&PurityTemplate::Allocating),
      Purity::Allocating
    );
    assert_eq!(
      purity_template_to_purity(&PurityTemplate::DependsOnCallback),
      Purity::Unknown
    );
  }
}
