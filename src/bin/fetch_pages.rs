//! Fetch and cache HTML pages for testing
//!
//! Fetches all target pages in parallel and caches to fetches/html/

#[cfg(feature = "renderer_tools")]
fn main() {
  eprintln!("fetch_pages is disabled when built with the `renderer_tools` feature.");
  eprintln!("Rebuild without `renderer_tools` to use this tool.");
  std::process::exit(2);
}

#[cfg(not(feature = "renderer_tools"))]
include!("_real/fetch_pages.rs");
