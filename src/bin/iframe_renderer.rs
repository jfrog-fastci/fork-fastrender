//! Minimal out-of-process iframe renderer used by the parent process for site isolation.
//!
//! This binary is intentionally small: it reads a single JSON request on stdin, performs the work
//! (or crashes intentionally for `crash://`), writes a JSON response on stdout, and exits.

#[cfg(feature = "renderer_tools")]
fn main() {
  eprintln!(
    "iframe_renderer is disabled when built with the `renderer_tools` feature (site isolation/sandboxing is gated off)."
  );
  eprintln!("Rebuild without `renderer_tools` to use this tool.");
  std::process::exit(2);
}

#[cfg(not(feature = "renderer_tools"))]
include!("_real/iframe_renderer.rs");
