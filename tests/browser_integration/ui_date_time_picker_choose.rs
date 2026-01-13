#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  DateTimeInputKind, PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;
use std::collections::HashMap;

#[test]
fn browser_tab_controller_date_picker_choose_updates_form_submission() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "about:blank";
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #d { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #submit { position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <form method="get">
          <input id="d" type="date" name="d">
          <input id="submit" type="submit" value="Go">
        </form>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html(tab_id, html, url, viewport_css, 1.0)?;

  // Ensure the document is laid out before hit-testing the click.
  let initial_msgs = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;
  assert!(
    initial_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected initial FrameReady"
  );

  // Click within the date input control.
  let click_pos = (10.0, 10.0);
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let open_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let (input_node_id, kind) = open_msgs
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::DateTimePickerOpened {
        tab_id: msg_tab,
        input_node_id,
        kind,
        ..
      } if *msg_tab == tab_id => Some((*input_node_id, *kind)),
      _ => None,
    })
    .expect("expected WorkerToUi::DateTimePickerOpened after clicking date input");
  assert_eq!(kind, DateTimeInputKind::Date);

  let choose_msgs = controller.handle_message(UiToWorker::DateTimePickerChoose {
    tab_id,
    input_node_id,
    value: "2020-01-02".to_string(),
  })?;

  assert!(
    choose_msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::DateTimePickerClosed { tab_id: msg_tab } if *msg_tab == tab_id
    )),
    "expected DateTimePickerClosed after choosing a value"
  );

  // Click submit to trigger a GET navigation with the selected date.
  let submit_pos = (10.0, 50.0);
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: submit_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let submit_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: submit_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let nav_url = submit_msgs
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::NavigationStarted {
        tab_id: msg_tab,
        url,
      } if *msg_tab == tab_id => Some(url.clone()),
      _ => None,
    })
    .expect("expected NavigationStarted after submitting form");

  let parsed = url::Url::parse(&nav_url).expect("parse navigation URL");
  let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
  assert_eq!(params.get("d"), Some(&"2020-01-02".to_string()));

  Ok(())
}

