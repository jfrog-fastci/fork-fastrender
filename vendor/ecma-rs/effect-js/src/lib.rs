#![deny(missing_debug_implementations)]

mod api;
mod api_use;
pub mod callback;
pub mod encoding;
pub mod eval;
mod recognize;
mod resolve;
mod template_eval;
pub mod types;

pub mod kb;
pub mod resolver;
pub mod db;
pub mod meta;
pub mod signals;
pub mod validate;

#[cfg(feature = "typed")]
pub mod typed;

pub use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};

pub use api::ApiId;
pub use callback::{analyze_inline_callback, callsite_info_for_args, CallbackInfo};
pub use api_use::{resolve_api_use, ApiUseKind, ResolvedApiUse};
pub use encoding::{analyze_string_encodings, EncodingResult, StringEncoding};
#[cfg(feature = "typed")]
pub use encoding::analyze_string_encodings_typed;
pub use eval::{eval_api_call, CallSemantics, CallSiteInfo as EvalCallSiteInfo};
pub use kb::load_default_api_database;
pub use db::{CallSiteInfo, EffectDb};
pub use recognize::{
  recognize_patterns_best_effort_untyped, recognize_patterns_untyped, GuardKind, RecognizedPattern,
};
pub use resolver::{collect_require_bindings, resolve_api_call, RequireBindings};
pub use signals::{collect_signals, SemanticSignal, SignalTables};

#[cfg(feature = "typed")]
pub use recognize::recognize_patterns_typed;

pub use resolve::{
  resolve_api_call_best_effort_untyped, resolve_api_call_untyped, resolve_call, ResolvedCall,
};
pub use types::TypeProvider;

pub use knowledge_base::{Api, KnowledgeBase};
pub use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase, ApiSemantics};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bundled_kb_contains_core_semantics() {
    let db = load_default_api_database();

    let required = [
      "Array.prototype.map",
      "Array.prototype.filter",
      "Array.prototype.reduce",
      "Array.prototype.forEach",
      "Promise.all",
      "Promise.race",
      "JSON.parse",
      "JSON.stringify",
      "String.prototype.toLowerCase",
      "String.prototype.split",
      "Math.sqrt",
      "Math.floor",
      "fetch",
    ];

    for name in required {
      assert!(
        db.get(name).is_some(),
        "required API not found in bundled knowledge-base: {name}"
      );
    }

    let json_parse = db.get("JSON.parse").expect("JSON.parse present");
    assert!(json_parse.effect_summary.contains(EffectSet::MAY_THROW));

    let array_map = db
      .get("Array.prototype.map")
      .expect("Array.prototype.map present");
    assert!(array_map.effect_summary.contains(EffectSet::ALLOCATES));
    match &array_map.effects {
      EffectTemplate::DependsOnArgs { base, args } => {
        assert!(base.contains(EffectSet::ALLOCATES));
        assert_eq!(args.as_slice(), &[0]);
      }
      other => panic!("expected DependsOnArgs for Array.prototype.map, got {other:?}"),
    }
    match &array_map.purity {
      PurityTemplate::DependsOnArgs { base, args } => {
        assert_eq!(*base, Purity::Allocating);
        assert_eq!(args.as_slice(), &[0]);
      }
      other => panic!(
        "expected DependsOnArgs purity for Array.prototype.map, got {other:?}"
      ),
    }

    let fetch = db.get("fetch").expect("fetch present");
    assert!(fetch.effect_summary.contains(EffectSet::IO));
  }
}

