use fastrender::dom::DomNode;
use fastrender::interaction::InteractionEngine;
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BrowserDocument, FastRender, FontConfig, Point, RenderOptions, Result};

fn deterministic_renderer() -> FastRender {
  fastrender::FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build deterministic renderer")
}

fn find_element_by_id<'a>(dom: &'a DomNode, element_id: &str) -> &'a DomNode {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return node;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={element_id:?}");
}

#[test]
fn range_input_rtl_pointer_drag_respects_visual_direction() -> Result<()> {
  let _lock = crate::common::global_state::global_test_lock();
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #r { position: absolute; left: 0; top: 0; width: 200px; height: 30px; padding: 0; margin: 0; border: 0; direction: rtl; }
        </style>
      </head>
      <body>
        <input id="r" type="range" dir="rtl" min="0" max="2" step="1" value="0">
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(220, 60);
  let mut doc = BrowserDocument::new(deterministic_renderer(), html, options)?;
  // Populate layout artifacts required for hit-testing based interaction.
  let _ = doc.render_frame()?;
  let mut engine = InteractionEngine::new();
  let scroll = doc.scroll_state();

  // In RTL, the painted range thumb is mirrored: min on the right, max on the left. Pointer
  // mapping should match.
  {
    let point = Point::new(5.0, 15.0);
    doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let mut changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll, point);
      let (changed_up, _) = engine.pointer_up_with_scroll(
        dom,
        box_tree,
        fragment_tree,
        &scroll,
        point,
        PointerButton::Primary,
        PointerModifiers::NONE,
        true,
        url,
        url,
      );
      changed |= changed_up;

      // Range value updates do not affect layout for our deterministic test page; keep cached layout
      // artifacts so subsequent clicks don't need to re-render.
      let _ = changed;
      (false, ())
    })?;
  }
  assert_eq!(
    find_element_by_id(doc.dom(), "r").get_attribute_ref("value"),
    Some("2")
  );

  {
    let point = Point::new(195.0, 15.0);
    doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let mut changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll, point);
      let (changed_up, _) = engine.pointer_up_with_scroll(
        dom,
        box_tree,
        fragment_tree,
        &scroll,
        point,
        PointerButton::Primary,
        PointerModifiers::NONE,
        true,
        url,
        url,
      );
      changed |= changed_up;
      let _ = changed;
      (false, ())
    })?;
  }
  assert_eq!(
    find_element_by_id(doc.dom(), "r").get_attribute_ref("value"),
    Some("0")
  );

  Ok(())
}
