//! Minimal WPT-only integration test binary.
//!
//! The main `tests/integration.rs` crate pulls *all* integration test modules into one binary so the
//! suite links once, but that can make tight local/WPT-focused runs expensive to compile/link under
//! multi-agent constraints.
//!
//! This crate exists so we can run the WPT harness in isolation:
//!
//! ```bash
//! WPT_FILTER=css/subgrid/subgrid-writing-mode-001 \
//!   bash scripts/cargo_agent.sh test -p fastrender --test wpt_smoke -- --exact wpt::wpt_local_suite_passes
//! ```
//!
//! (See `tests/wpt/mod.rs` for the harness entrypoint.)

mod common;
mod wpt;

