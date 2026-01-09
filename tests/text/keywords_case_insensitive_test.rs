// Re-export the style keyword-case-insensitivity test into the `text_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/text_keywords_case_insensitive_test.rs"]
mod text_keywords_case_insensitive_test;
