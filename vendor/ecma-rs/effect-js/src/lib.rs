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

#[cfg(feature = "typed")]
pub use recognize::recognize_patterns_typed;

pub use resolve::{
  resolve_api_call_best_effort_untyped, resolve_api_call_untyped, resolve_call, ResolvedCall,
};
pub use types::TypeProvider;

pub use knowledge_base::{Api, KnowledgeBase};
pub use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase, ApiSemantics};
