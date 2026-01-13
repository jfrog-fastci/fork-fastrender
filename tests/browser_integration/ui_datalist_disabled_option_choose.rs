#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::{dom::DomNode, Result};

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
fn browser_tab_controller_datalist_choose_rejects_disabled_option_and_closes() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (220, 80);
  let url = "https://example.com/index.html";
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #q { position: absolute; left: 0; top: 0; width: 160px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="q" list="dl" value="">
        <datalist id="dl">
          <option value="ok"></option>
          <option value="no" disabled></option>
        </datalist>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html(tab_id, html, url, viewport_css, 1.0)?;

  // Ensure we have a prepared tree for hit-testing.
  let _ = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;

  // Click the input to focus it.
  let click_pos = (10.0, 10.0);
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  // Type "n" so the disabled option ("no") is the best match.
  let out = controller.handle_message(UiToWorker::TextInput {
    tab_id,
    text: "n".to_string(),
  })?;

  let (input_node_id, option_node_id) = out
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::DatalistOpened {
        tab_id: msg_tab,
        input_node_id,
        options,
        ..
      } if *msg_tab == tab_id => {
        let opt = options.iter().find(|opt| opt.value == "no")?;
        assert!(
          opt.disabled,
          "expected the 'no' <option> to be marked disabled in DatalistOpened"
        );
        Some((*input_node_id, opt.option_node_id))
      }
      _ => None,
    })
    .expect("expected WorkerToUi::DatalistOpened containing disabled option 'no'");

  let dom = controller.document().dom();
  assert_eq!(
    find_element_by_id(dom, "q")
      .get_attribute_ref("value")
      .unwrap_or(""),
    "n",
    "expected TextInput to set the input value to the typed prefix"
  );

  // Attempt to choose the disabled option by id spoofing. The worker must reject this and keep the
  // input value unchanged, but still close the popup deterministically.
  let choose_out =
    controller.handle_message(UiToWorker::datalist_choose(tab_id, input_node_id, option_node_id))?;
  assert!(
    choose_out.iter().any(|msg| matches!(
      msg,
      WorkerToUi::DatalistClosed { tab_id: msg_tab } if *msg_tab == tab_id
    )),
    "expected DatalistChoose to emit DatalistClosed even when selection is rejected"
  );

  let dom = controller.document().dom();
  assert_eq!(
    find_element_by_id(dom, "q")
      .get_attribute_ref("value")
      .unwrap_or(""),
    "n",
    "disabled datalist option must not be selectable via UiToWorker::DatalistChoose"
  );

  Ok(())
}

