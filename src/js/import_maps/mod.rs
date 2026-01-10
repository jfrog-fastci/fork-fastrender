//! WHATWG HTML import maps (parsing + normalization).
//!
//! This module currently implements only the parsing/normalization pipeline:
//! - "parse an import map string"
//! - "sort and normalize a module specifier map"
//! - "normalize a specifier key"
//! - "sort and normalize scopes"
//! - "normalize a module integrity map"
//!
//! Import maps merging and full module specifier resolution are handled in separate tasks.

mod parse;
mod resolve;
mod types;

pub use parse::create_import_map_parse_result;
pub use parse::parse_import_map_string;
pub use resolve::resolve_imports_match;
pub use types::{
  ImportMap, ImportMapError, ImportMapParseResult, ImportMapWarning, ImportMapWarningKind, ModuleIntegrityMap,
  ModuleSpecifierMap, ScopesMap,
};

#[cfg(test)]
mod parse_tests;
