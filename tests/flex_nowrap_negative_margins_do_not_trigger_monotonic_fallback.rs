// Keep the test implementation alongside other layout regression tests, but expose it as a
// standalone integration test target so it can be run in isolation via:
// `cargo test --test flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback`.
#[path = "layout/flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback.rs"]
mod flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback;
