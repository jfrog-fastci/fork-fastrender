//! WHATWG HTML import maps (parsing + normalization).
//!
//! This module currently implements only the parsing/normalization pipeline:
//! - "parse an import map string"
//! - "sort and normalize a module specifier map"
//! - "normalize a specifier key"
//! - "sort and normalize scopes"
//! - "normalize a module integrity map"
//!
//! Import maps merging and module specifier resolution are handled in separate tasks.

mod parse;
mod types;

pub use parse::parse_import_map_string;
pub use types::{
  ImportMap, ImportMapError, ImportMapWarning, ImportMapWarningKind, ModuleIntegrityMap, ModuleSpecifierMap,
  ScopesMap,
};

#[cfg(test)]
mod parse_tests;

