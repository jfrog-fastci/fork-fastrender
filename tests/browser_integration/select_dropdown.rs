#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use std::time::{Duration, Instant};

// Worker startup + first render can take a while in debug builds (font init, cache warming, etc).
// Keep this generous so the test remains reliable when run in isolation.
const TIMEOUT: Duration = Duration::from_secs(120);

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn next_frame_ready(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_select_dropdown_opened(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> (usize, usize) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::SelectDropdownOpened { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for SelectDropdownOpened for tab {tab_id:?}"));

  match msg {
    WorkerToUi::SelectDropdownOpened {
      select_node_id,
      control,
      ..
    } => {
      let option_node_id = control
        .items
        .iter()
        .find_map(|item| match item {
          SelectItem::Option { value, node_id, .. } if value == "b" => Some(*node_id),
          _ => None,
        })
        .expect("expected <option value=\"b\"> in SelectControl");
      (select_node_id, option_node_id)
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn select_dropdown_choose_updates_dom_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #sel { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
      #box { position: absolute; left: 0; top: 40px; width: 64px; height: 64px; background: rgb(255, 0, 0); }
      #sel:has(option[value="b"][selected]) + #box { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <select id="sel">
      <option value="a" selected>Red</option>
      <option value="b">Green</option>
    </select>
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle {
    tx: ui_tx,
    rx: ui_rx,
    join,
  } = worker;
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some(url),
      CancelGens::new(),
    ))
    .unwrap();
  ui_tx.send(UiToWorker::SetActiveTab { tab_id }).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  let frame = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(rgba_at_css(&frame, 10, 50), [255, 0, 0, 255]);

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  let (select_node_id, option_node_id) = next_select_dropdown_opened(&ui_rx, tab_id);

  ui_tx
    .send(UiToWorker::SelectDropdownChoose {
      tab_id,
      select_node_id,
      option_node_id,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_closed = false;
  let mut saw_green_frame = false;
  loop {
    if Instant::now() >= deadline {
      panic!(
        "timed out waiting for select dropdown repaint (saw_closed={saw_closed}, saw_green_frame={saw_green_frame})"
      );
    }
    let msg = support::recv_for_tab(&ui_rx, tab_id, Duration::from_millis(250), |msg| {
      matches!(
        msg,
        WorkerToUi::SelectDropdownClosed { .. }
          | WorkerToUi::FrameReady { .. }
          | WorkerToUi::NavigationFailed { .. }
      )
    });
    let Some(msg) = msg else {
      continue;
    };
    match msg {
      WorkerToUi::SelectDropdownClosed { .. } => {
        saw_closed = true;
      }
      WorkerToUi::FrameReady { frame, .. } => {
        if rgba_at_css(&frame, 10, 50) == [0, 255, 0, 255] {
          saw_green_frame = true;
        }
      }
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed for {url}: {error}");
      }
      _ => {}
    }
    if saw_closed && saw_green_frame {
      break;
    }
  }

  drop(ui_tx);
  drop(ui_rx);
  join.join().unwrap();
}
