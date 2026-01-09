// Re-export the style keyword-case-insensitivity test into the `font_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/font_table_keywords_case_insensitive_test.rs"]
mod font_table_keywords_case_insensitive_test;
