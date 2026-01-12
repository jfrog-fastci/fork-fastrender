//! WHATWG HTML import maps.
//!
//! This module contains spec-mapped import map plumbing used by HTML `<script type="importmap">`
//! and module specifier resolution:
//!
//! - Parsing/normalization:
//!   - "parse an import map string"
//!   - "sort and normalize a module specifier map"
//!   - "normalize a specifier key"
//!   - "sort and normalize scopes"
//!   - "normalize a module integrity map"
//! - Script-element parse result:
//!   - "create an import map parse result"
//! - Resolution:
//!   - "resolve a module specifier"
//!   - "resolve an imports match"
//!   - "add module to resolved module set"
//! - Fetch option helpers:
//!   - "resolve a module integrity metadata"
//! - Merging:
//!   - "merge module specifier maps"
//!   - "merge existing and new import maps"
//!   - "register an import map"
//!
//! See `docs/import_maps.md` for a spec-mapped developer guide and integration notes.

mod integrity;
mod limits;
mod merge;
mod parse;
mod resolve;
mod strings;
mod types;

pub use integrity::resolve_module_integrity_metadata;
pub use limits::ImportMapLimits;
pub use merge::{
  merge_existing_and_new_import_maps, merge_existing_and_new_import_maps_with_limits,
  merge_module_specifier_maps, register_import_map, register_import_map_with_limits,
};
pub use parse::{
  create_import_map_parse_result, create_import_map_parse_result_with_limits,
  parse_import_map_string, parse_import_map_string_with_limits,
};
pub use resolve::{
  add_module_to_resolved_module_set, resolve_imports_match, resolve_module_specifier,
};
pub use types::{
  ImportMap, ImportMapError, ImportMapParseResult, ImportMapState, ImportMapWarning,
  ImportMapWarningKind, ModuleIntegrityMap, ModuleResolutionError, ModuleSpecifierMap,
  ResolvedModuleSet, ResolvedModuleSetIndex, ScopeMap, ScopesMap, SpecifierAsUrlKind,
  SpecifierResolutionRecord,
};

#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
mod resolve_tests;
#[cfg(test)]
mod strings_tests;

#[cfg(test)]
mod fixture_tests;

#[cfg(test)]
mod tests;
