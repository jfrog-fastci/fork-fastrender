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

mod discover;
mod backend;
mod backend_quickjs;
mod backend_vmjs;
mod meta;
mod timer_event_loop;
mod runner;
mod suite;
pub mod wpt_fs;
mod wpt_report;

pub use backend::{BackendKind, BackendSelection};
pub use discover::{discover_tests, TestCase, TestKind};
pub use meta::{MetaDirective, MetaParseResult};
pub use runner::{RunError, RunOutcome, RunResult, Runner, RunnerConfig};
pub use suite::{
  run_suite, should_fail, ExpectationOutcome, MismatchSummary, Report, SuiteConfig, Summary,
  TestOutcome, TestResult, REPORT_SCHEMA_VERSION,
};
pub use wpt_fs::WptFs;
pub use wpt_report::{WptReport, WptSubtest};
