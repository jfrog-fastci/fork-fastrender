use fastrender::paint::display_list::ClipShape;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::scroll::ScrollState;
use fastrender::{ClipItem, DisplayItem, FastRender};

#[test]
fn form_control_overflow_clip_uses_padding_box() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; }
      select {
        box-sizing: border-box;
        width: 100px;
        height: 40px;
        border: 10px solid black;
        padding: 5px;
        overflow: clip;
      }
    </style>
    <select></select>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer.layout_document(&dom, 200, 100).expect("laid out");

  let list = DisplayListBuilder::new()
    .with_scroll_state(ScrollState::default())
    .build_tree_with_stacking(&fragment_tree);

  let clip_rect = list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Rect { rect, .. },
      }) if (rect.width() - 80.0).abs() < 0.01 && (rect.height() - 20.0).abs() < 0.01 => {
        Some(*rect)
      }
      _ => None,
    })
    .expect("expected select overflow clip to use padding box (80x20)");

  assert!(
    (clip_rect.width() - 80.0).abs() < 0.01,
    "expected padding box width 80, got {}",
    clip_rect.width()
  );
  assert!(
    (clip_rect.height() - 20.0).abs() < 0.01,
    "expected padding box height 20, got {}",
    clip_rect.height()
  );

  // Sanity check: dropdown selects paint a single-glyph arrow affordance inside the clip.
  let arrow_origin_x = list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::Text(text) if text.glyphs.len() == 1 => Some(text.origin.x),
      _ => None,
    })
    .expect("expected dropdown select to paint an arrow glyph");

  // The UA stylesheet reserves `padding-right: 20px` for dropdown selects. The arrow should be
  // painted into that padding region (i.e. to the right of the content box), not inside the
  // content box itself.
  //
  // Here, we set `border: 10px` and `padding: 5px` on a 100px-wide border box:
  // - padding box max-x: 90
  // - content box max-x: 85
  let content_max_x = clip_rect.max_x() - 5.0;
  assert!(
    arrow_origin_x + 0.01 >= content_max_x,
    "expected dropdown select arrow to be placed in the padding box (arrow origin x={arrow_origin_x}, content max x={content_max_x})"
  );
}
