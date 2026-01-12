// Keep the test implementation alongside other layout regression tests, but expose it as a
// standalone integration test target so it can be run in isolation via:
// `cargo test --test calc_percent_height_is_not_collapsible_through`.
#[path = "layout/calc_percent_height_is_not_collapsible_through.rs"]
mod calc_percent_height_is_not_collapsible_through;
