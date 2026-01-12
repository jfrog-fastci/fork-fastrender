//! Focused UI test binary.
//!
//! The main `tests/integration.rs` binary pulls in a large swath of integration tests so the suite
//! links once. For UI-only changes, it's useful to have a smaller target so CI and agents can run
//! a tight subset quickly (e.g. `cargo test --test ui_tests appearance_settings`).

mod ui;

