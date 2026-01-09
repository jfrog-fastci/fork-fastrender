// Keep this as a tiny harness so `cargo test --test backdrop_root_non_triggers_test` compiles a
// minimal subset of the paint suite.

#[path = "paint/rayon_test_util.rs"]
mod rayon_test_util;

#[path = "paint/backdrop_root_non_triggers_test.rs"]
mod backdrop_root_non_triggers_test;
