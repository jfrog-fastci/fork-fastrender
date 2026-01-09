use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::style::color::Rgba;
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
fn display_contents_background_border_not_painted() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #wrapper {
        display: contents;
        padding: 10px;
        background: rgb(255 0 0);
        border: 5px solid rgb(0 0 255);
      }
      #child { width: 20px; height: 20px; background: rgb(0 255 0); }
    </style>
    <div id="wrapper"><div id="child"></div></div>
  "#;

  let list = build_display_list(html);
  assert!(!list.is_empty(), "expected non-empty display list");

  let mut saw_green = false;
  let mut saw_red = false;
  let mut saw_blue = false;

  for item in list.items() {
    match item {
      DisplayItem::FillRect(fill) => {
        if fill.color == Rgba::GREEN {
          saw_green = true;
        }
        if fill.color == Rgba::RED {
          saw_red = true;
        }
      }
      DisplayItem::FillRoundedRect(fill) => {
        if fill.color == Rgba::GREEN {
          saw_green = true;
        }
        if fill.color == Rgba::RED {
          saw_red = true;
        }
      }
      DisplayItem::StrokeRect(stroke) => {
        if stroke.color == Rgba::BLUE {
          saw_blue = true;
        }
      }
      DisplayItem::StrokeRoundedRect(stroke) => {
        if stroke.color == Rgba::BLUE {
          saw_blue = true;
        }
      }
      DisplayItem::Border(border) => {
        if border.top.color == Rgba::BLUE
          || border.right.color == Rgba::BLUE
          || border.bottom.color == Rgba::BLUE
          || border.left.color == Rgba::BLUE
        {
          saw_blue = true;
        }
      }
      DisplayItem::Outline(outline) => {
        if outline.color == Rgba::BLUE {
          saw_blue = true;
        }
      }
      _ => {}
    }
  }

  assert!(saw_green, "expected child background to paint");
  assert!(
    !saw_red,
    "display:contents element background must not paint (saw red fill)"
  );
  assert!(
    !saw_blue,
    "display:contents element border must not paint (saw blue stroke/border)"
  );
}

