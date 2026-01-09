// Separate integration test crate so we can run the non-trigger stacking-context regressions in
// isolation without compiling the full `paint_tests` harness.
#[path = "paint/rayon_test_util.rs"]
use crate::rayon_test_util;

#[path = "paint/backdrop_root_non_trigger_stacking_contexts_test.rs"]
mod backdrop_root_non_trigger_stacking_contexts_test;
