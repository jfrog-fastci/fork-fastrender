//! Interaction-related integration tests.
//!
//! `tests/integration.rs` includes `mod interaction;` to keep historical module structure, even
//! when there are currently no tests that live under this namespace.

// Keep the RTL range-pointer regression test in its historical location under
// `tests/browser_integration/` while allowing it to run in non-UI test builds.
#[path = "../browser_integration/range_input_rtl_pointer_drag.rs"]
mod range_input_rtl_pointer_drag;
