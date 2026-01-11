use fastrender::geometry::{Point, Rect, Size};
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::LineHeight;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::CrossOriginAttribute;
use fastrender::tree::box_tree::ImageDecodingAttribute;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::SrcsetCandidate;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn find_fragment_by_box_id_abs<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
  origin: Point,
) -> Option<(&'a FragmentNode, Rect)> {
  let abs_origin = origin.translate(fragment.bounds.origin);
  let abs_bounds = Rect::new(abs_origin, fragment.bounds.size);
  let matches_id = match &fragment.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline { box_id: Some(id), .. }
    | FragmentContent::Text { box_id: Some(id), .. }
    | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  };
  if matches_id {
    return Some((fragment, abs_bounds));
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_by_box_id_abs(child, box_id, abs_origin) {
      return Some(found);
    }
  }
  None
}

#[test]
fn abspos_static_position_in_inline_context_anchors_to_line_box_top() {
  // Regression test for abspos static positioning inside an inline formatting context.
  //
  // Pattern (matches real-world placeholder images):
  //   <div style="position:relative; line-height:0">
  //     <img style="position:absolute; width:100%">
  //     <img style="width:100%">
  //   </div>
  //
  // When the abspos element has `top/bottom/left/right: auto`, CSS 2.1 §10.6.4 requires
  // `top` to resolve to the element's "static position" (top margin edge of the hypothetical
  // in-flow box). That static position is the top of the line box, not the baseline.
  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(200.0));
  let root_style = Arc::new(root_style);

  let mut container_style = ComputedStyle::default();
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.line_height = LineHeight::Length(Length::px(0.0));
  let container_style = Arc::new(container_style);

  let mut placeholder_style = ComputedStyle::default();
  placeholder_style.position = Position::Absolute;
  placeholder_style.width = Some(Length::percent(100.0));
  let placeholder_style = Arc::new(placeholder_style);

  let mut real_style = ComputedStyle::default();
  real_style.width = Some(Length::percent(100.0));
  let real_style = Arc::new(real_style);

  let mut placeholder = BoxNode::new_replaced(
    placeholder_style,
    ReplacedType::Image {
      src: String::new(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::<SrcsetCandidate>::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(160.0, 90.0)),
    None,
  );
  placeholder.id = 2;

  let mut real = BoxNode::new_replaced(
    real_style,
    ReplacedType::Image {
      src: String::new(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::<SrcsetCandidate>::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(320.0, 180.0)),
    None,
  );
  real.id = 3;

  let mut container = BoxNode::new_block(
    container_style,
    FormattingContextType::Block,
    vec![placeholder, real],
  );
  container.id = 1;

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![container]);

  let constraints = LayoutConstraints::definite(200.0, 300.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let (_, placeholder_bounds) =
    find_fragment_by_box_id_abs(&fragment, 2, Point::ZERO).expect("placeholder fragment");
  let (_, real_bounds) =
    find_fragment_by_box_id_abs(&fragment, 3, Point::ZERO).expect("real image fragment");

  assert!(
    (placeholder_bounds.origin.x - real_bounds.origin.x).abs() < 0.1,
    "expected abspos static position x to align with in-flow image (got {}, expected {})",
    placeholder_bounds.origin.x,
    real_bounds.origin.x
  );
  assert!(
    (placeholder_bounds.origin.y - real_bounds.origin.y).abs() < 0.1,
    "expected abspos static position y to align with in-flow image (got {}, expected {})",
    placeholder_bounds.origin.y,
    real_bounds.origin.y
  );
}

