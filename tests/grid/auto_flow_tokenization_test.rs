// Re-export the style tokenization test into the `grid_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/grid_auto_flow_tokenization_test.rs"]
mod grid_auto_flow_tokenization_test;
