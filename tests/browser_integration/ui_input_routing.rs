#![cfg(feature = "browser_ui")]

use fastrender::interaction::dom_index::DomIndex;
use fastrender::ui::messages::{PointerButton, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::{BoxNode, BoxTree, Result};

fn extract_frame(messages: Vec<WorkerToUi>) -> Option<RenderedFrame> {
  messages.into_iter().rev().find_map(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
}

fn find_element_by_id<'a>(dom: &'a fastrender::dom::DomNode, element_id: &str) -> Option<&'a fastrender::dom::DomNode> {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

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
    if node.styled_node_id == Some(styled_node_id) {
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
fn browser_tab_controller_routes_basic_inputs() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (200, 200);
  let url = "https://example.com/index.html";

  let html = r##"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #cb { position: absolute; top: 10px; left: 10px; width: 20px; height: 20px; }
          #cb_label { position: absolute; top: 10px; left: 40px; width: 80px; height: 20px; background: rgb(255, 0, 0); }
          #cb[checked] + #cb_label { background: rgb(0, 255, 0); }

          #text { position: absolute; top: 40px; left: 10px; width: 120px; height: 22px; border: 1px solid #000; }

          /* Give the link a predictable hit target so pointer events reliably land on the <a>. */
          #link { position: absolute; top: 70px; left: 10px; display: block; width: 80px; height: 24px; background: rgb(220, 220, 0); }

          #scroller { position: absolute; top: 100px; left: 10px; width: 120px; height: 60px; overflow: scroll; border: 1px solid #000; }
          #scroller .inner { height: 240px; background: rgb(240, 240, 240); }

          #spacer { height: 500px; }
          #target { height: 20px; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <input id="cb" type="checkbox"><label id="cb_label" for="cb">check</label>
        <input id="text" type="text" value="">
        <a id="link" href="#target">go</a>
        <div id="scroller"><div class="inner">scroll me<br>more<br>more<br>more</div></div>
        <div id="spacer"></div>
        <div id="target">target</div>
      </body>
    </html>
  "##;

  let mut controller = BrowserTabController::from_html(tab_id, html, url, viewport_css, 1.0)?;

  // Initial paint.
  let frame0 = extract_frame(controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?)
  .expect("expected initial FrameReady");
  let baseline_bytes = frame0.pixmap.data().to_vec();

  // Click checkbox (down+up).
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (15.0, 15.0),
    button: PointerButton::Primary,
  })?;
  let frame_after_checkbox = extract_frame(controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (15.0, 15.0),
    button: PointerButton::Primary,
  })?)
  .expect("expected FrameReady after checkbox click");
  assert_ne!(
    frame_after_checkbox.pixmap.data(),
    baseline_bytes.as_slice(),
    "expected checkbox click to change rendered pixels"
  );

  let checkbox = find_element_by_id(controller.document().dom(), "cb").expect("checkbox element");
  assert!(
    checkbox.get_attribute_ref("checked").is_some(),
    "expected checkbox to be checked after click"
  );

  // Focus input and type into it.
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (15.0, 50.0),
    button: PointerButton::Primary,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (15.0, 50.0),
    button: PointerButton::Primary,
  })?;
  let _ = controller.handle_message(UiToWorker::TextInput {
    tab_id,
    text: "hi".to_string(),
  })?;
  let input = find_element_by_id(controller.document().dom(), "text").expect("text input element");
  assert_eq!(input.get_attribute_ref("value"), Some("hi"));

  // Scroll inside the element scroller: element scroll offset should change, viewport should not.
  let scroller_box_id = {
    let prepared = controller
      .document()
      .prepared()
      .expect("expected prepared doc after interaction");
    let scroller_dom_id = dom_preorder_id(controller.document().dom(), "scroller");
    box_id_for_styled_node(prepared.box_tree(), scroller_dom_id)
  };

  let scroll_msgs = controller.handle_message(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 40.0),
    pointer_css: Some((15.0, 110.0)),
  })?;
  assert!(
    scroll_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::ScrollStateUpdated { .. })),
    "expected ScrollStateUpdated message after scroll"
  );
  assert!(
    controller.scroll_state().viewport.y.abs() < 1e-3,
    "expected viewport scroll to remain unchanged when scrolling over element scroller, got {:?}",
    controller.scroll_state().viewport
  );
  assert!(
    controller.scroll_state().element_offset(scroller_box_id).y > 0.0,
    "expected element scroll offset to change for scroller box_id={scroller_box_id}"
  );

  // Same-document anchor navigation should scroll viewport without reloading/mutating existing state.
  let expected_anchor_scroll = {
    let prepared = controller
      .document()
      .prepared()
      .expect("expected prepared doc for anchor scroll computation");
    fastrender::interaction::scroll_offset_for_fragment_target(
      controller.document().dom(),
      prepared.box_tree(),
      prepared.fragment_tree(),
      "target",
      prepared.fragment_tree().viewport_size(),
    )
    .expect("expected anchor scroll target to resolve")
  };

  let nav_msgs = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (15.0, 75.0),
    button: PointerButton::Primary,
  })?;
  assert!(
    nav_msgs.iter().any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected pointer down to repaint active state"
  );
  let nav_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (15.0, 75.0),
    button: PointerButton::Primary,
  })?;
  assert!(
    nav_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. })),
    "expected NavigationStarted message for link click"
  );
  assert!(
    nav_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::NavigationCommitted { .. })),
    "expected NavigationCommitted message for link click"
  );
  let viewport_scroll = controller.scroll_state().viewport;
  assert!(
    (viewport_scroll.y - expected_anchor_scroll.y).abs() < 1.0,
    "expected anchor navigation to update viewport scroll to ~{:?}, got {:?}",
    expected_anchor_scroll,
    viewport_scroll
  );
  assert!(
    controller.current_url().ends_with("#target"),
    "expected current URL to include fragment, got {:?}",
    controller.current_url()
  );
  assert!(
    find_element_by_id(controller.document().dom(), "cb")
      .expect("checkbox element")
      .get_attribute_ref("checked")
      .is_some(),
    "expected same-document fragment navigation to preserve DOM state (checkbox still checked)"
  );

  // Scroll outside element scroller should affect viewport scroll.
  // At this point the anchor navigation likely clamps us near the max scroll offset, so scroll up.
  let before_viewport_scroll = controller.scroll_state().viewport.y;
  let _ = controller.handle_message(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, -25.0),
    pointer_css: Some((190.0, 190.0)),
  })?;
  assert!(
    controller.scroll_state().viewport.y < before_viewport_scroll,
    "expected viewport scroll to decrease when scrolling outside element scroller"
  );

  Ok(())
}
