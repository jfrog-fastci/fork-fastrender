//! WHATWG HTML import maps.
//!
//! This module contains spec-mapped import map plumbing used by HTML `<script type="importmap">`
//! and (eventually) module specifier resolution:
//!
//! - Parsing/normalization:
//!   - "parse an import map string"
//!   - "sort and normalize a module specifier map"
//!   - "normalize a specifier key"
//!   - "sort and normalize scopes"
//!   - "normalize a module integrity map"
//! - Script-element parse result:
//!   - "create an import map parse result"
//! - Resolution helper:
//!   - "resolve an imports match"
//!
//! What is not implemented yet:
//! - "register an import map"
//! - "merge existing and new import maps"
//! - full "resolve a module specifier" (scopes + fallbacks + error reporting + resolved-module-set)
//!
//! See `docs/import_maps.md` for a spec-mapped developer guide to the intended full pipeline.

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
