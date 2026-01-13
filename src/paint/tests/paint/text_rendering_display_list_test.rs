use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::style::types::TextRendering;
use crate::{FastRender, FontConfig};

fn build_display_list(html: &str) -> crate::paint::display_list::DisplayList {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let dom = renderer.parse_html(html).expect("parsed HTML");
  let tree = renderer.layout_document(&dom, 128, 128).expect("layout");

  DisplayListBuilder::new()
    .with_parallelism(&PaintParallelism::disabled())
    .build_tree_with_stacking(&tree)
}

#[test]
fn text_rendering_property_sets_display_list_text_rendering() {
  let html = r#"<!doctype html><style>
    html, body { margin: 0; padding: 0; }
    div { text-rendering: geometricPrecision; font-size: 16px; }
  </style><div>Hello</div>"#;

  let list = build_display_list(html);
  assert!(!list.is_empty(), "expected non-empty display list");

  let mut saw_text = false;
  for item in list.items() {
    if let DisplayItem::Text(text) = item {
      if text.glyphs.is_empty() {
        continue;
      }
      saw_text = true;
      assert_eq!(
        text.text_rendering,
        TextRendering::GeometricPrecision,
        "expected display list text item to preserve text-rendering: geometricPrecision"
      );
    }
  }

  assert!(saw_text, "expected at least one non-empty text item");
}

