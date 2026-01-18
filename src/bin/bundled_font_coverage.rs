//! Bundled font coverage audit tool (pageset-driven, offline).
//!
//! This binary answers: "Which Unicode codepoints used by cached pageset HTML are not covered by
//! the bundled font set?".
//!
//! - Inputs are loaded strictly from disk (`fetches/html/*.html` + optional `.html.meta` sidecars).
//! - Text is extracted from DOM text nodes (skipping script/style/template/hidden/inert subtrees).
//! - Coverage is checked against `FontDatabase::shared_bundled()` (plus bundled emoji when enabled).
//! - Output supports a human summary and a stable JSON report for diffing across commits.

#[cfg(feature = "renderer_tools")]
fn main() {
  eprintln!(
    "bundled_font_coverage is disabled when built with the `renderer_tools` feature."
  );
  eprintln!("Rebuild without `renderer_tools` to use this tool.");
  std::process::exit(2);
}

#[cfg(not(feature = "renderer_tools"))]
include!("_real/bundled_font_coverage.rs");
