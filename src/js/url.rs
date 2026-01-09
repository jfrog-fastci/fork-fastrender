//! JS-facing URL core types.
//!
//! This module intentionally re-exports the bounded resource-layer URL implementation so JS
//! bindings do not accidentally use an unbounded parser.

pub use crate::resource::web_url::{
  WebUrl as Url, WebUrlError as UrlError, WebUrlLimits as UrlLimits,
  WebUrlSearchParams as UrlSearchParams,
};
