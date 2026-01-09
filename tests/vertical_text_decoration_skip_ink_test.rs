// Separate integration test wrapper so we can run just these vertical decoration
// regressions via:
//   scripts/cargo_agent.sh test -p fastrender --test vertical_text_decoration_skip_ink_test
//
// The actual tests live under `tests/paint/` alongside other paint-level coverage.
#[path = "paint/vertical_text_decoration_skip_ink_test.rs"]
mod vertical_text_decoration_skip_ink_test;

