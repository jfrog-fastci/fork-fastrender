use fastrender::geometry::{Point, Rect, Size};
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
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

fn count_line_fragments(fragment: &FragmentNode) -> usize {
  fragment
    .iter_fragments()
    .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    .count()
}

#[test]
fn abspos_percent_top_in_auto_height_positioned_block_does_not_create_empty_line_box() {
  // Regression test for two related inline-layout issues:
  // 1) Trailing collapsible whitespace before an out-of-flow positioned descendant should not
  //    create an extra (empty) line box once the whitespace is trimmed away.
  // 2) Percentage `top`/`bottom` on an absolutely positioned descendant should resolve against the
  //    used height of the containing block's padding box even when the containing block itself is
  //    `height:auto` (CSS 2.1 §10.5).
  //
  // This matches patterns like:
  //   <div class="cb"><img ... /> <a style="position:absolute; top:50%"></a></div>
  // where the `<img>` fills the available width and the whitespace before `<a>` would otherwise
  // wrap onto its own line.

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

  let whitespace = BoxNode::new_text(Arc::new(ComputedStyle::default()), " ".to_string());

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.top = InsetValue::Length(Length::percent(50.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  let mut abs = BoxNode::new_inline(Arc::new(abs_style), vec![]);
  abs.id = 3;

  let mut cb = BoxNode::new_block(cb_style, FormattingContextType::Block, vec![img, whitespace, abs]);
  cb.id = 1;

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![cb]);

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let (cb_fragment, cb_abs_bounds) =
    find_fragment_by_box_id_abs(&fragment, 1, Point::ZERO).expect("cb fragment");
  assert_eq!(
    count_line_fragments(cb_fragment),
    1,
    "expected trailing whitespace + abspos anchor not to create an extra empty line fragment"
  );

  let (_abs_fragment, abs_abs_bounds) =
    find_fragment_by_box_id_abs(&fragment, 3, Point::ZERO).expect("abs fragment");
  let expected_y = cb_abs_bounds.origin.y + cb_abs_bounds.size.height * 0.5;
  assert!(
    (abs_abs_bounds.origin.y - expected_y).abs() < 0.5,
    "expected abspos `top:50%` to resolve against used CB height (got y={}, expected {})",
    abs_abs_bounds.origin.y,
    expected_y
  );
}

