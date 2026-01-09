//! Compatibility re-export for the dom2-backed html5ever `TreeSink`.
//!
//! The primary implementation lives in `crate::dom2::Dom2TreeSink` (see
//! `src/dom2/html5ever_tree_sink.rs`). This module exists so html-side code can
//! refer to the sink via `crate::html::dom2_tree_sink`.

pub use crate::dom2::Dom2TreeSink;

