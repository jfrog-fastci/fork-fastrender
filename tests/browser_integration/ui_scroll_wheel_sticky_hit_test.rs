#![cfg(feature = "browser_ui")]

use fastrender::interaction::dom_index::DomIndex;
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker};
use fastrender::ui::BrowserTabController;
use fastrender::{BoxNode, BoxTree, Result};

use super::support;

fn dom_preorder_id(dom: &fastrender::dom::DomNode, element_id: &str) -> usize {
  let mut clone = dom.clone();
  let index = DomIndex::build(&mut clone);
  *index
    .id_by_element_id
    .get(element_id)
    .unwrap_or_else(|| panic!("expected element with id={element_id:?}"))
}

fn box_id_for_styled_node(box_tree: &BoxTree, styled_node_id: usize) -> usize {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
      return node.id;
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected box for styled_node_id={styled_node_id}");
}

#[test]
fn wheel_over_sticky_header_scrolls_viewport_not_underlying_scroller() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 200);
  let url = "https://example.com/index.html";

  // The sticky header becomes pinned to the top of the viewport after scrolling past `#spacer`.
  // Once stuck, it overlaps `#scroller` as the viewport continues to scroll; wheel events over the
  // header should still target the viewport scroll, not the underlying element scroller.
  let html = r##"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #spacer { height: 200px; }
          #sticky {
            position: sticky;
            top: 0;
            height: 40px;
            background: rgb(240, 240, 240);
          }
          #scroller {
            height: 100px;
            overflow-y: scroll;
            border: 1px solid black;
          }
          #scroller_content { height: 600px; }
          #tail { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="spacer"></div>
        <div id="sticky">sticky</div>
        <div id="scroller"><div id="scroller_content"></div></div>
        <div id="tail"></div>
      </body>
    </html>
  "##;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Initial paint (populate prepared layout artifacts).
  let _ = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;

  let scroller_box_id = {
    let prepared = controller
      .document()
      .prepared()
      .expect("expected prepared doc after initial paint");
    let scroller_dom_id = dom_preorder_id(controller.document().dom(), "scroller");
    box_id_for_styled_node(prepared.box_tree(), scroller_dom_id)
  };

  // Scroll the viewport far enough that the sticky header is pinned and no longer occupies its
  // original flow rect (so hit-testing must consult sticky offsets).
  let _ = controller.handle_message(UiToWorker::ScrollTo {
    tab_id,
    pos_css: (0.0, 250.0),
  })?;

  let before_viewport = controller.scroll_state().viewport.y;
  let before_scroller = controller.scroll_state().element_offset(scroller_box_id).y;
  assert!(
    before_viewport > 230.0 && before_viewport < 330.0,
    "expected viewport scroll to land in the overlap range, got {}",
    before_viewport
  );
  assert!(
    before_scroller.abs() < 1e-3,
    "expected underlying element scroller to start at 0, got {}",
    before_scroller
  );

  // Wheel over the sticky header (top of viewport).
  let _ = controller.handle_message(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 40.0),
    pointer_css: Some((10.0, 10.0)),
  })?;

  let after_viewport = controller.scroll_state().viewport.y;
  let after_scroller = controller.scroll_state().element_offset(scroller_box_id).y;

  assert!(
    after_viewport > before_viewport,
    "expected wheel over sticky header to scroll the viewport (before={}, after={})",
    before_viewport,
    after_viewport
  );
  assert!(
    after_scroller.abs() < 1e-3,
    "expected wheel over sticky header to not scroll the underlying element scroller (before={}, after={})",
    before_scroller,
    after_scroller
  );

  Ok(())
}
