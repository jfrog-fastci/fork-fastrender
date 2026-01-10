use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn fragment_box_id(fragment: &fastrender::FragmentNode) -> Option<usize> {
  match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

#[test]
fn flex_root_rerun_avoids_child_drift_after_root_width_correction() {
  // Regression: `taffy_to_fragment` used to correct an unusable root width (e.g. ~0px after an
  // intrinsic probe) without rerunning Taffy. Child coordinates were still computed against the
  // pre-correction container size, so justification could drift.
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.justify_content = JustifyContent::FlexEnd;
  // Use a percentage width so the root depends on the provided constraint / percentage base.
  container_style.width = Some(Length::percent(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(20.0));
  child_style.height = Some(Length::px(10.0));
  // Prevent flexing so justify-content is the only source of the main-axis offset.
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child],
  );

  // Simulate the original bug: Taffy can compute a near-zero root width for percentage-sized
  // roots during certain probes. The flex layout pipeline corrects the root to a definite width
  // (percentage base), but must rerun Taffy so children use the corrected line size.
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(0.0), AvailableSpace::Indefinite)
    .with_inline_percentage_base(Some(500.0));

  let fragment = fc
    .layout(&container, &constraints)
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.width() - 500.0).abs() < 0.5,
    "expected corrected root width of 500px, got {:.2}",
    fragment.bounds.width()
  );

  let child_fragment = fragment
    .children
    .iter()
    .find(|f| fragment_box_id(f) == Some(1))
    .unwrap_or_else(|| panic!("expected flex child with id=1, got {:?}", fragment.children));

  let bounds = child_fragment.bounds;
  assert!(
    bounds.x().is_finite()
      && bounds.y().is_finite()
      && bounds.width().is_finite()
      && bounds.height().is_finite(),
    "expected finite child bounds, got {bounds:?}"
  );

  let expected_x = 480.0;
  assert!(
    (bounds.x() - expected_x).abs() < 0.5,
    "expected justify-content:flex-end to place child near x={expected_x:.1}, got x={:.2} (root_w={:.2})",
    bounds.x(),
    fragment.bounds.width()
  );
  assert!(
    bounds.x() >= -0.5 && bounds.max_x() <= fragment.bounds.width() + 0.5,
    "expected child to sit within the corrected root width, got child_bounds={bounds:?} root_bounds={:?}",
    fragment.bounds
  );
}
