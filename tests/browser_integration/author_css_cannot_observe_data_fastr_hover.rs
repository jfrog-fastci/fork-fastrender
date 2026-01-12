use fastrender::dom::DomNode;
use fastrender::interaction::InteractionEngine;
use fastrender::scroll::ScrollState;
use fastrender::{BrowserDocument, Point, RenderOptions, Result};

use super::support;

fn dom_has_attr(root: &DomNode, name: &str) -> bool {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref(name).is_some() {
      return true;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

#[test]
fn author_css_cannot_observe_internal_hover_state_via_data_fastr_hover_attr() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  // Regression test: The renderer previously injected `data-fastr-hover` into the DOM to implement
  // `:hover`, which allowed author CSS to observe internal interaction state via `[data-fastr-hover]`
  // selectors. The DOM attribute must never be injected.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #target {
            position: absolute;
            left: 0;
            top: 0;
            width: 60px;
            height: 60px;
            background: rgb(0, 0, 0);
          }
          #target:hover { background: rgb(0, 0, 255); }
          /* This must NOT match: `data-fastr-hover` is internal state and must never be injected. */
          #target[data-fastr-hover], [data-fastr-hover] #target { background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <div id="target"></div>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(80, 80);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  let initial =
    doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  assert_eq!(
    support::rgba_at(&initial.pixmap, 10, 10),
    [0, 0, 0, 255],
    "expected the target's initial background to be black"
  );

  let scroll: ScrollState = doc.scroll_state();
  let hover_changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.pointer_move(
      dom,
      box_tree,
      fragment_tree,
      &scroll,
      Point::new(10.0, 10.0),
    );
    (changed, changed)
  })?;
  assert!(hover_changed, "expected pointer move to update hover state");

  assert!(
    !dom_has_attr(doc.dom(), "data-fastr-hover"),
    "renderer must not inject data-fastr-hover onto the DOM"
  );

  let hovered =
    doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  assert_eq!(
    support::rgba_at(&hovered.pixmap, 10, 10),
    [0, 0, 255, 255],
    "expected :hover rule to apply, and [data-fastr-hover] rule to NOT apply"
  );

  Ok(())
}
