//! Helpers for resolving module specifiers against a host environment.
//!
//! This module is gated behind the `resolve` feature to keep the core checker
//! lightweight while allowing downstream hosts (including the CLI) to opt into
//! a deterministic, Node/TS-style resolver.

#[cfg(feature = "resolve")]
pub mod node;
#[cfg(feature = "resolve")]
pub mod path;
#[cfg(feature = "resolve")]
pub mod ts_node;

#[cfg(feature = "resolve")]
pub use node::{
  ModuleResolutionMode, NodeResolver, RealFs, ResolveFs, ResolutionKind, ResolveOptions, Resolver,
  TypeScriptVersion, DEFAULT_EXTENSIONS,
};
#[cfg(feature = "resolve")]
pub use path::{canonicalize_path, normalize_path, normalize_path_str};
