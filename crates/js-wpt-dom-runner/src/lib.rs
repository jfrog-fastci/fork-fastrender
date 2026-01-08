//! Minimal WPT DOM (`testharness.js`) runner.
//!
//! This crate is intentionally scoped to the curated "DOM WPT" subset used by FastRender's JS
//! workstream. It supports:
//! - Discovering testharness tests from a WPT-like directory tree.
//! - Running `.window.js` and (for now) `.any.js` tests in a window-like JS realm.
//! - Parsing a limited set of `// META:` directives for `.js` test files.
//!
//! The intent is to grow this runner alongside the JS + DOM implementation without pulling in a
//! heavyweight external harness.

mod meta;
mod discover;
mod runner;
pub mod wpt_fs;

pub use discover::{discover_tests, JsTestKind, TestCase};
pub use meta::{MetaDirective, MetaParseResult};
pub use runner::{RunError, RunOutcome, RunResult, Runner, RunnerConfig};
pub use wpt_fs::WptFs;
