// Re-export the style invalid-value handling test into the `text_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/text_overflow_invalid_values_ignored_test.rs"]
mod text_overflow_invalid_values_ignored_test;
