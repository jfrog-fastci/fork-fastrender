#![deny(missing_debug_implementations)]

//! Deterministic, interned TypeScript type representation.
//!
//! [`TypeStore`] interns canonicalized [`TypeKind`] values into stable IDs
//! (`TypeId`, `ShapeId`, `ObjectId`, `SignatureId`, `NameId`, ...). IDs are
//! derived from stable hashes of canonical data, making them deterministic and
//! thread-scheduling independent **assuming no hash collisions**.
//!
//! ## Hash collisions and determinism
//!
//! Hash collisions are astronomically unlikely with the default 128-bit
//! fingerprints.
//!
//! By default, this crate enables the `strict-determinism` feature: if an ID
//! collision occurs (two distinct values hashing to the same ID), the store
//! will panic. This makes collision handling schedule-independent: you either
//! get deterministic output, or a deterministic fail-fast error.
//!
//! To opt out of fail-fast collision handling, disable default features:
//!
//! ```toml
//! # In this workspace:
//! types-ts-interned = { workspace = true, default-features = false }
//!
//! # Or from crates.io:
//! # types-ts-interned = { version = "0.1.0", default-features = false }
//! ```
//!
//! In that mode, the store falls back to salt-based rehashing on collision. This
//! is deterministic for a *fixed insertion sequence*, but under parallelism the
//! insertion order can vary, making collision resolution schedule-dependent.
//!
//! # Example
//! ```
//! use types_ts_interned::{TypeDisplay, TypeStore};
//!
//! let store = TypeStore::new();
//! let primitives = store.primitive_ids();
//! assert_eq!(
//!   TypeDisplay::new(store.as_ref(), primitives.number).to_string(),
//!   "number"
//! );
//! ```
//!
//! # Runnable example
//!
//! ```bash
//! bash scripts/cargo_agent.sh run -p types-ts-interned --example basic
//! ```

mod cache;
mod display;
mod eval;
mod infer;
#[cfg(all(feature = "fuzzing", feature = "serde-json"))]
mod fuzz;
mod gc_trace;
mod ids;
mod kind;
mod layout;
mod number_format;
mod options;
mod relate;
mod shape;
mod signature;
mod store;

pub use cache::{CacheConfig, CacheStats, ShardedCache};
pub use display::TypeDisplay;
pub use eval::EvaluatorCacheStats;
pub use eval::EvaluatorCaches;
pub use eval::EvaluatorLimits;
pub use eval::ExpandedType;
pub use eval::TypeEvaluator;
pub use eval::TypeExpander;
#[cfg(all(feature = "fuzzing", feature = "serde-json"))]
pub use fuzz::fuzz_type_graph;
pub use gc_trace::FieldTrace;
pub use gc_trace::GcTraceLayout;
pub use gc_trace::VariantTrace;
pub use ids::DefId;
pub use ids::NameId;
pub use ids::ObjectId;
pub use ids::ShapeId;
pub use ids::SignatureId;
pub use ids::TypeId;
pub use ids::TypeParamId;
pub use kind::IntrinsicKind;
pub use kind::MappedModifier;
pub use kind::MappedType;
pub use kind::PredicateParam;
pub use kind::TemplateChunk;
pub use kind::TemplateLiteralType;
pub use kind::TupleElem;
pub use kind::TypeKind;
pub use layout::AbiScalar;
pub use layout::ArrayElemRepr;
pub use layout::FieldKey;
pub use layout::FieldLayout;
pub use layout::GcTraceKind;
pub use layout::GcTraceStep;
pub use layout::GcTraceVariant;
pub use layout::Layout;
pub use layout::LayoutComputer;
pub use layout::LayoutId;
pub use layout::PtrKind;
pub use layout::TagLayout;
pub use layout::VariantLayout;
pub use options::TypeOptions;
pub use relate::ReasonNode;
pub use relate::RelateCtx;
pub use relate::RelateHooks;
pub use relate::RelateTypeExpander;
pub use relate::RelationCache;
pub use relate::RelationKind;
pub use relate::RelationLimits;
pub use relate::RelationMode;
pub use relate::RelationResult;
pub use shape::Accessibility;
pub use shape::Indexer;
pub use shape::ObjectType;
pub use shape::PropData;
pub use shape::PropKey;
pub use shape::Property;
pub use shape::Shape;
pub use signature::Param;
pub use signature::Signature;
pub use signature::TypeParamDecl;
pub use signature::TypeParamVariance;
pub use store::PrimitiveIds;
pub use store::TypeStore as Store;
pub use store::TypeStore;
pub use store::TypeStoreSnapshot;
