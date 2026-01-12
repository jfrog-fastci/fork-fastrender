//! Regression test: `flex-wrap` containers with CSS `order` reordering must not trip the flex
//! main-axis manual placement fallback (which can create gaps/drift when iterating in DOM order).

#[path = "layout/flex_wrap_order_does_not_trigger_manual_placement.rs"]
mod flex_wrap_order_does_not_trigger_manual_placement;
