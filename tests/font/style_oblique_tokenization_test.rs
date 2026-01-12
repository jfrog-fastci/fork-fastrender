// Re-export the style tokenization test.
//
// This file used to be a standalone `tests/*.rs` integration-test binary. Keeping it as a module
// avoids spawning yet another test executable (see `AGENTS.md` / `docs/testing.md`).
#[path = "../style/font_style_oblique_tokenization_test.rs"]
mod font_style_oblique_tokenization_test;
