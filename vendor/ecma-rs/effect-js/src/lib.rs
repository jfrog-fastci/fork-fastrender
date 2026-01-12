#![deny(missing_debug_implementations)]

mod api_use;
pub mod array_pipeline;
pub mod callback;
pub mod effects;
pub mod encoding;
pub mod eval;
mod js_string;
#[deprecated(note = "Legacy AST-level patterns; use `effect_js::pattern_engine` (canonical) or `effect_js::recognize_patterns_*` (legacy) instead.")]
pub mod pattern;
pub mod pattern_engine;
mod recognize;
mod resolve;
pub mod target;
mod template_eval;
pub mod types;

pub mod db;
#[cfg(feature = "hir-semantic-ops")]
pub mod hir_rewrite;
pub mod kb;
pub mod meta;
pub mod resolver;
pub mod signals;
pub mod validate;

#[cfg(feature = "typed")]
pub mod typed;

pub use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate};

pub use api_use::{resolve_api_use, ApiUseKind, ResolvedApiUse};
pub use callback::{
  analyze_inline_callback, analyze_inline_callback_for_target, callsite_info_for_args,
  callsite_info_for_args_for_target, eval_callsite_info_for_args,
  eval_callsite_info_for_args_for_target, CallbackInfo,
};
#[cfg(feature = "typed")]
pub use db::analyze_body_tables_typed;
pub use db::analyze_body_tables_untyped;
pub use db::{BodyTables, CallSiteInfo, EffectDb};
#[cfg(feature = "typed")]
pub use effects::{analyze_effects_typed, analyze_effects_typed_for_target};
pub use effects::{analyze_effects_untyped, analyze_effects_untyped_for_target, EffectsTables};
pub use encoding::{
  analyze_string_encodings, analyze_string_encodings_for_target, EncodingResult, StringEncoding,
};
#[cfg(feature = "typed")]
pub use encoding::{analyze_string_encodings_typed, analyze_string_encodings_typed_for_target};
pub use eval::{eval_api_call, CallSemantics, CallSiteInfo as EvalCallSiteInfo};
pub use kb::load_default_api_database;
pub use patterns::{
  recognize_patterns, ExprPatternTables, RecognizePatternsResult, RecognizedPatternId,
  StmtPatternTables,
};
pub use pattern_engine::{analyze_patterns, PatternEngineResult};
pub use recognize::{
  recognize_patterns_best_effort_untyped, recognize_patterns_untyped, ArrayChainOp, ArrayTerminal,
  GuardKind, RecognizedPattern,
};
pub use resolver::{collect_require_bindings, resolve_api_call, RequireBindings};
pub use semantic_patterns::{
  recognize_pattern_tables as recognize_semantic_pattern_tables,
  recognize_patterns as recognize_semantic_patterns, ArrayChainOp as SemanticArrayChainOp,
  ArrayOp as SemanticArrayOp, ArrayTerminal as SemanticArrayTerminal,
  PatternTables as SemanticPatternTables, PromiseCombinatorKind as SemanticPromiseCombinatorKind,
  PromiseInputPattern as SemanticPromiseInputPattern, RecognizedPattern as SemanticPattern,
  RecognizedPatternId as SemanticPatternId,
};
pub use signals::{collect_signals, detect_signals, SemanticSignal, SignalTables};

#[cfg(feature = "typed")]
pub use recognize::recognize_patterns_typed;

pub use resolve::{
  resolve_api_call_best_effort_untyped, resolve_api_call_untyped, resolve_call,
  resolve_call_for_target, ResolvedCall, ResolvedMember,
};
#[cfg(feature = "typed")]
pub use resolve::{resolve_member, resolve_member_for_target};
pub use types::TypeProvider;

pub use array_pipeline::{ArrayPipelinePlan, ArrayStage, ArrayStageKind, ArrayStageMeta};
#[cfg(feature = "typed")]
pub use array_pipeline::plan_array_chains_typed;

#[cfg(feature = "typed")]
pub use resolve::resolve_api_call_typed;

pub use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase, ApiSemantics};
pub use knowledge_base::{Api, ApiId, KnowledgeBase};
pub use knowledge_base::{TargetEnv, WebPlatform};
pub use target::TargetedKb;

#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::ThrowBehavior;

  #[test]
  fn api_id_is_kb_api_id() {
    fn assert_kb_id(_: knowledge_base::ApiId) {}
    assert_kb_id(ApiId::from_name("JSON.parse"));
  }

  #[test]
  fn bundled_kb_contains_core_semantics() {
    let db = load_default_api_database();

    let required = [
      "Array.prototype.map",
      "Array.prototype.filter",
      "Array.prototype.flatMap",
      "Array.prototype.flat",
      "Array.prototype.reduce",
      "Array.prototype.forEach",
      "Array.isArray",
      "Array.prototype.findIndex",
      "Array.prototype.includes",
      "Array.prototype.indexOf",
      "Array.prototype.lastIndexOf",
      "Array.prototype.slice",
      "Promise.all",
      "Promise.race",
      "Promise.allSettled",
      "Promise.any",
      "Promise.prototype.then",
      "Promise.prototype.catch",
      "Promise.prototype.finally",
      "JSON.parse",
      "JSON.stringify",
      "String.prototype.toLowerCase",
      "String.prototype.includes",
      "String.prototype.startsWith",
      "String.prototype.endsWith",
      "String.prototype.indexOf",
      "String.prototype.lastIndexOf",
      "String.prototype.repeat",
      "String.prototype.padStart",
      "String.prototype.padEnd",
      "String.prototype.trimStart",
      "String.prototype.trimEnd",
      "String.prototype.split",
      "Math.sqrt",
      "Math.floor",
      "Math.ceil",
      "Math.round",
      "Math.random",
      "Math.trunc",
      "Math.sign",
      "Math.imul",
      "Math.clz32",
      "Number.parseInt",
      "Number.parseFloat",
      "Number.isInteger",
      "Number.isSafeInteger",
      "Object.keys",
      "Object.values",
      "Object.entries",
      "Object.fromEntries",
      "Object.create",
      "Object.is",
      "Object.hasOwn",
      "Object.prototype.hasOwnProperty",
      "fetch",
      "MutationObserver",
      "MutationObserver.prototype.observe",
      "MutationObserver.prototype.takeRecords",
      "IntersectionObserver",
      "IntersectionObserver.prototype.takeRecords",
      "ResizeObserver",
      "ResizeObserver.prototype.takeRecords",
    ];

    for name in required {
      assert!(
        db.get(name).is_some(),
        "required API not found in bundled knowledge-base: {name}"
      );
    }

    let json_parse = db.get("JSON.parse").expect("JSON.parse present");
    assert_ne!(json_parse.effect_summary.throws, ThrowBehavior::Never);

    let array_map = db
      .get("Array.prototype.map")
      .expect("Array.prototype.map present");
    assert!(array_map
      .effect_summary
      .flags
      .contains(EffectSet::ALLOCATES));
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
      other => panic!("expected DependsOnArgs purity for Array.prototype.map, got {other:?}"),
    }

    let fetch = db.get("fetch").expect("fetch present");
    assert!(fetch.effect_summary.flags.contains(EffectSet::IO));
  }
}

pub mod patterns;
pub mod properties;
pub mod semantic_patterns;

pub fn effect_template_to_summary(template: &EffectTemplate) -> EffectSet {
  template.base_effects()
}
