#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  NavigationReason, PointerButton, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;

use super::support::{
  create_tab_msg, navigate_msg, pointer_down, pointer_up, request_repaint, rgba_at, text_input,
  viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};

fn recv_frame(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::RenderedFrame {
  super::support::recv_for_tab(rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .and_then(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
  .expect("timed out waiting for FrameReady")
}

fn drain_worker(rx: &fastrender::ui::WorkerToUiInbox) {
  while rx.try_recv().is_ok() {}
}

#[test]
fn ui_worker_preserves_form_state_across_tab_switching() {
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; }
        #txt {
          position: absolute;
          left: 0px;
          top: 0px;
          width: 40px;
          height: 20px;
          margin: 0;
          padding: 0;
          border: 0;
        }
        #box {
          position: absolute;
          left: 0px;
          top: 24px;
          width: 32px;
          height: 32px;
          background: rgb(255, 0, 0);
        }

        input[value="a"] + #box { background: rgb(0, 255, 0); }
        input[value="b"] + #box { background: rgb(0, 0, 255); }
      </style>
      <input id="txt" value="" />
      <div id="box"></div>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-form-state-tab-switch").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_a = TabId::new();
  let tab_b = TabId::new();

  ui_tx
    .send(create_tab_msg(tab_a, None))
    .expect("CreateTab A");
  ui_tx
    .send(create_tab_msg(tab_b, None))
    .expect("CreateTab B");

  ui_tx
    .send(viewport_changed_msg(tab_a, (64, 64), 1.0))
    .expect("ViewportChanged A");
  ui_tx
    .send(viewport_changed_msg(tab_b, (64, 64), 1.0))
    .expect("ViewportChanged B");

  // Navigate both tabs to the same page.
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .expect("SetActiveTab A");
  ui_tx
    .send(navigate_msg(tab_a, url.clone(), NavigationReason::TypedUrl))
    .expect("Navigate A");
  let _ = recv_frame(&ui_rx, tab_a);
  drain_worker(&ui_rx);

  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .expect("SetActiveTab B");
  ui_tx
    .send(navigate_msg(tab_b, url.clone(), NavigationReason::TypedUrl))
    .expect("Navigate B");
  let _ = recv_frame(&ui_rx, tab_b);
  drain_worker(&ui_rx);

  // Tab A: type "a" and assert the box turns green.
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .expect("SetActiveTab A (edit)");
  ui_tx
    .send(pointer_down(tab_a, (5.0, 5.0), PointerButton::Primary))
    .expect("PointerDown A");
  ui_tx
    .send(pointer_up(tab_a, (5.0, 5.0), PointerButton::Primary))
    .expect("PointerUp A");
  // Consume PointerDown/Up repaints so the next frame corresponds to the text input mutation.
  let _ = recv_frame(&ui_rx, tab_a);
  let _ = recv_frame(&ui_rx, tab_a);
  drain_worker(&ui_rx);

  ui_tx.send(text_input(tab_a, "a")).expect("TextInput A");
  let frame_a = recv_frame(&ui_rx, tab_a);
  assert_eq!(rgba_at(&frame_a.pixmap, 16, 40), [0, 255, 0, 255]);
  drain_worker(&ui_rx);

  // Tab B: type "b" and assert the box turns blue.
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .expect("SetActiveTab B (edit)");
  ui_tx
    .send(pointer_down(tab_b, (5.0, 5.0), PointerButton::Primary))
    .expect("PointerDown B");
  ui_tx
    .send(pointer_up(tab_b, (5.0, 5.0), PointerButton::Primary))
    .expect("PointerUp B");
  let _ = recv_frame(&ui_rx, tab_b);
  let _ = recv_frame(&ui_rx, tab_b);
  drain_worker(&ui_rx);

  ui_tx.send(text_input(tab_b, "b")).expect("TextInput B");
  let frame_b = recv_frame(&ui_rx, tab_b);
  assert_eq!(rgba_at(&frame_b.pixmap, 16, 40), [0, 0, 255, 255]);
  drain_worker(&ui_rx);

  // Switching back to tab A must preserve the typed value (and should not be overwritten by tab B).
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .expect("SetActiveTab A (return)");
  ui_tx
    .send(request_repaint(tab_a, RepaintReason::Explicit))
    .expect("RequestRepaint A");
  let frame_a_return = recv_frame(&ui_rx, tab_a);
  assert_eq!(rgba_at(&frame_a_return.pixmap, 16, 40), [0, 255, 0, 255]);
  drain_worker(&ui_rx);

  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .expect("SetActiveTab B (return)");
  ui_tx
    .send(request_repaint(tab_b, RepaintReason::Explicit))
    .expect("RequestRepaint B");
  let frame_b_return = recv_frame(&ui_rx, tab_b);
  assert_eq!(rgba_at(&frame_b_return.pixmap, 16, 40), [0, 0, 255, 255]);

  drop(ui_tx);
  join.join().expect("join ui worker");
}
