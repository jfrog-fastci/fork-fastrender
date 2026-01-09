// Re-export the style tokenization test into the `font_tests` harness.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/font_style_oblique_tokenization_test.rs"]
mod font_style_oblique_tokenization_test;
