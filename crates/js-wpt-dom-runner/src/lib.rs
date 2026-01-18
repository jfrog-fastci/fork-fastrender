//! Minimal WPT DOM (`testharness.js`) runner.
//!
//! This crate is intentionally scoped to the curated "DOM WPT" subset used by FastRender's JS
//! workstream. It supports:
//! - Discovering testharness tests from a WPT-like directory tree.
//! - Running `.window.js`, `.any.js`, and testharness HTML files (`.html` / `.htm`) in a
//!   window-like JS realm.
//! - Parsing a limited set of `// META:` directives for `.js` test files (plus a small subset of
//!   HTML metadata for `.html` tests).
//!
//! The intent is to grow this runner alongside the JS + DOM implementation without pulling in a
//! heavyweight external harness.

mod backend;
#[cfg(feature = "vmjs")]
mod backend_vmjs;
#[cfg(feature = "vmjs")]
mod backend_vmjs_rendered;
#[cfg_attr(
  not(any(feature = "vmjs", feature = "quickjs")),
  allow(dead_code, unused_imports)
)]
mod cookie_jar;
mod discover;
#[cfg(feature = "quickjs")]
mod dom_shims;
#[cfg_attr(
  not(any(feature = "vmjs", feature = "quickjs")),
  allow(dead_code, unused_imports)
)]
mod engine;
#[cfg(feature = "quickjs")]
mod fetch;
mod meta;
#[cfg_attr(
  not(any(feature = "vmjs", feature = "quickjs")),
  allow(dead_code, unused_imports)
)]
mod runner;
mod suite;
#[cfg(feature = "quickjs")]
mod url_shims;
#[cfg(feature = "quickjs")]
mod window_or_worker_global_scope;
pub mod wpt_fs;
#[cfg(feature = "vmjs")]
mod wpt_resource_fetcher;
mod wpt_report;

pub use backend::{BackendKind, BackendSelection};
pub use conformance_harness::{FailOn, Shard};
pub use discover::{discover_tests, TestCase, TestKind};
pub use meta::{MetaDirective, MetaParseResult};
pub use runner::{RunError, RunOutcome, RunResult, Runner, RunnerConfig};
pub use suite::{
  run_suite, should_fail, ExpectationOutcome, MismatchSummary, Report, SuiteConfig, Summary,
  TestOutcome, TestResult, REPORT_SCHEMA_VERSION,
};
pub use wpt_fs::WptFs;
#[cfg(feature = "vmjs")]
pub use wpt_resource_fetcher::WptResourceFetcher;
pub use wpt_report::{WptReport, WptSubtest};
