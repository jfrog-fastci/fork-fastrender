// Separate integration test crate so we can run this regression in isolation without compiling
// the full `paint_tests` harness.
#[path = "paint/rayon_test_util.rs"]
mod rayon_test_util;

#[path = "paint/backdrop_root_clip_path_test.rs"]
mod backdrop_root_clip_path_test;
