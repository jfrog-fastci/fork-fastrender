use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FontConfig};

fn build_display_list(html: &str) -> fastrender::paint::display_list::DisplayList {
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
fn font_smoothing_property_sets_text_item_allow_subpixel_aa_flag() {
  let html = r#"<!doctype html><style>
    html, body { margin: 0; padding: 0; }
    div { -webkit-font-smoothing: antialiased; font-size: 16px; }
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
      assert!(
        !text.allow_subpixel_aa,
        "expected display list text item to disable subpixel AA when -webkit-font-smoothing: antialiased"
      );
    }
  }

  assert!(saw_text, "expected at least one non-empty text item");
}

