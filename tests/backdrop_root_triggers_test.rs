// Keep these Backdrop Root trigger regressions in `tests/paint/` alongside other paint tests, but
// expose them as a standalone integration test target so they can be run quickly:
//
//   cargo test --test backdrop_root_triggers_test
//
#[path = "paint/rayon_test_util.rs"]
mod rayon_test_util;

#[path = "paint/backdrop_root_triggers_test.rs"]
mod backdrop_root_triggers_test;
