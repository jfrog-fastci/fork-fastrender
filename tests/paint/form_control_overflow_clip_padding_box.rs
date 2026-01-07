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
  let has_arrow_glyph = list.items().iter().any(|item| match item {
    DisplayItem::Text(text) => text.glyphs.len() == 1,
    _ => false,
  });
  assert!(has_arrow_glyph, "expected dropdown select to paint an arrow glyph");
}

