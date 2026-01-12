// Aggregator for the WPT-style render-and-compare harness under `tests/wpt/`.
//
// Keeping this as a dedicated test binary allows running WPT-style tests without pulling
// unrelated integration suites into the same link unit (and without adding `[[test]]`
// entries in Cargo.toml).

mod common;
mod wpt;
