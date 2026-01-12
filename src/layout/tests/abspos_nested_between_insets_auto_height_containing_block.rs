use fastrender::geometry::{Point, Rect, Size};
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::{
  BoxNode, CrossOriginAttribute, ImageDecodingAttribute, ReplacedType, SrcsetCandidate,
};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
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
    | FragmentContent::Inline {
      box_id: Some(id), ..
    }
    | FragmentContent::Text {
      box_id: Some(id), ..
    }
    | FragmentContent::Replaced {
      box_id: Some(id), ..
    } => *id == box_id,
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
fn abspos_top_bottom_auto_height_depends_on_used_containing_block_height() {
  // Regression test: `top:0; bottom:0; height:auto` stretches to the containing block height
  // (CSS 2.1 §10.6.4). When the containing block itself is `height:auto`, its used height is only
  // known after in-flow layout, so we must trigger the abspos relayout pass even though no
  // percentages are involved.
  //
  // This matches patterns like image link overlays:
  //   <div class="cb"><img ... /><a style="position:absolute; top:0; bottom:0"></a></div>
  // where `<a>` is inline-level by default.

  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(200.0));
  let root_style = Arc::new(root_style);

  let mut cb_style = ComputedStyle::default();
  cb_style.position = Position::Relative;
  cb_style.width = Some(Length::px(100.0));
  let cb_style = Arc::new(cb_style);

  let mut img_style = ComputedStyle::default();
  img_style.width = Some(Length::px(100.0));
  img_style.height = Some(Length::px(50.0));
  let img_style = Arc::new(img_style);

  let mut img = BoxNode::new_replaced(
    img_style,
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
    Some(Size::new(100.0, 50.0)),
    None,
  );
  img.id = 2;

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  let mut abs = BoxNode::new_inline(Arc::new(abs_style), vec![]);
  abs.id = 3;

  let mut cb = BoxNode::new_block(cb_style, FormattingContextType::Block, vec![img, abs]);
  cb.id = 1;

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![cb]);

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let (_cb_fragment, cb_abs_bounds) =
    find_fragment_by_box_id_abs(&fragment, 1, Point::ZERO).expect("cb fragment");
  let (_abs_fragment, abs_abs_bounds) =
    find_fragment_by_box_id_abs(&fragment, 3, Point::ZERO).expect("abs fragment");

  assert!(
    (abs_abs_bounds.size.height - cb_abs_bounds.size.height).abs() < 0.5,
    "expected abspos `top:0; bottom:0; height:auto` to stretch to the used CB height (got {}, expected {})",
    abs_abs_bounds.size.height,
    cb_abs_bounds.size.height
  );
}
