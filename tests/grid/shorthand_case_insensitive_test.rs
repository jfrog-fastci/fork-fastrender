// Re-export the style shorthand parsing test into the `grid_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/grid_shorthand_case_insensitive_test.rs"]
mod grid_shorthand_case_insensitive_test;
