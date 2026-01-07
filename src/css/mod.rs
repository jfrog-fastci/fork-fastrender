//! CSS parsing and types
//!
//! This module handles parsing CSS stylesheets and provides types
//! for representing CSS rules, selectors, and values.

pub mod encoding;
pub(crate) mod ident;
pub mod loader;
pub mod parser;
pub mod properties;
pub mod selectors;
pub mod supports;
pub mod types;
pub(crate) mod value_cache;
