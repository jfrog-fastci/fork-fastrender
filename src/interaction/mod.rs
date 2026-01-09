pub mod anchor_scroll;
pub mod dom_index;
pub mod dom_mutation;
pub mod engine;
pub mod form_submit;
pub mod fragment_geometry;
pub mod hit_test;
pub mod hit_testing;
mod image_maps;
pub mod scroll_wheel;

pub use anchor_scroll::scroll_offset_for_fragment_target;
pub use engine::{InputModality, InteractionAction, InteractionEngine, KeyAction};
pub use form_submit::form_submission_get_url;
pub use fragment_geometry::absolute_bounds_for_box_id;
pub use hit_test::{hit_test_dom, resolve_label_associated_control, HitTestKind, HitTestResult};
pub use hit_testing::{
  fragment_tree_with_scroll, hit_test_dom_viewport_point, hit_test_dom_with_scroll, hit_test_with_scroll,
};
