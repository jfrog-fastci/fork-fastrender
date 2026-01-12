//! Helpers for selecting and loading TypeScript lib declaration files.
//!
//! This module exposes lib selection and loading utilities used by the main
//! checker. Bundled libs are chosen based on [`CompilerOptions`] and can be
//! cached via [`LibManager`].

mod compiler_options;
pub mod lib_env;

pub use compiler_options::{
  effective_module_kind, parse_jsx_mode, parse_module_detection_kind, parse_module_kind,
  parse_module_resolution_kind, parse_script_target, CacheMode, CacheOptions, CompilerOptions,
  JsxMode, LibName, LibSet, ModuleDetectionKind, ModuleKind, ModuleResolutionKind, ScriptTarget,
};
#[cfg(feature = "resolve")]
pub use compiler_options::{effective_module_resolution_mode, resolve_options_for_node_resolver};
pub use lib_env::{bundled_typescript_version, LibManager, LoadedLibs};
pub use types_ts_interned::TypeOptions;

use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::FileKey;

/// Kinds of supported files.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
  Js,
  Ts,
  Tsx,
  Jsx,
  Dts,
}

/// A library file that can be loaded before user source files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibFile {
  pub key: FileKey,
  pub name: Arc<str>,
  pub kind: FileKind,
  pub text: Arc<str>,
}
