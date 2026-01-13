#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, RepaintReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use url::Url;

// Navigation + rendering on CI can take a few seconds when tests run in parallel; keep this
// generous to avoid flakes.
const TIMEOUT: Duration = Duration::from_secs(20);

fn recv_frame(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady(tab={})", tab_id.0));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    _ => unreachable!(),
  }
}

fn rgba_unpremultiply(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let width = pixmap.width();
  let height = pixmap.height();
  assert!(
    x < width && y < height,
    "pixel out of bounds ({x}, {y}) in {width}x{height}"
  );
  let idx = (y as usize * width as usize + x as usize) * 4;
  let data = pixmap.data();
  let a = data[idx + 3];
  if a == 0 {
    return [0, 0, 0, 0];
  }
  // tiny-skia uses premultiplied alpha.
  let r = ((data[idx] as u16 * 255) / a as u16) as u8;
  let g = ((data[idx + 1] as u16 * 255) / a as u16) as u8;
  let b = ((data[idx + 2] as u16 * 255) / a as u16) as u8;
  [r, g, b, a]
}

fn assert_pixel_rgb(pixmap: &tiny_skia::Pixmap, x: u32, y: u32, expected: (u8, u8, u8)) {
  let got = rgba_unpremultiply(pixmap, x, y);
  assert_eq!(
    got,
    [expected.0, expected.1, expected.2, 255],
    "unexpected pixel at ({x}, {y})"
  );
}

#[test]
fn fieldset_disabled_semantics_apply_to_interaction_focus_and_submission() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
      fieldset, legend { margin: 0; padding: 0; border: 0; }

      #legend_input {
        position: absolute;
        left: 0;
        top: 30px;
        width: 120px;
        height: 24px;
      }

      #disabled_input {
        position: absolute;
        left: 0;
        top: 60px;
        width: 120px;
        height: 24px;
      }

      #disabled_cb {
        position: absolute;
        left: 0;
        top: 90px;
        width: 40px;
        height: 40px;
      }

      #outside_input {
        position: absolute;
        left: 0;
        top: 140px;
        width: 120px;
        height: 24px;
      }

      #submit {
        position: absolute;
        left: 0;
        top: 170px;
        width: 120px;
        height: 40px;
      }

      /* Marker squares for assertions. */
      #focus_status {
        position: absolute;
        left: 160px;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(0,0,0);
      }

      #check_status {
        position: absolute;
        left: 160px;
        top: 30px;
        width: 20px;
        height: 20px;
        background: rgb(0,0,0);
      }

      /* Tab traversal should focus the legend-contained control first. */
      fieldset:focus-within ~ #focus_status { background: rgb(255,0,255); }
      #outside_input:focus-visible ~ #focus_status { background: rgb(0,255,255); }

      /* Clicking the disabled checkbox must not toggle it. */
      #disabled_cb:checked ~ #check_status { background: rgb(255,0,0); }
    </style>
  </head>
  <body>
    <form action="result.html">
      <fieldset disabled>
        <legend><input id="legend_input" name="a" value="1"></legend>
        <input id="disabled_input" name="b" value="2">
        <input id="disabled_cb" type="checkbox" name="cb" value="1">
        <div id="check_status"></div>
      </fieldset>
      <input id="outside_input" name="c" value="3">
      <input id="submit" type="submit" value="Go">
      <div id="focus_status"></div>
    </form>
  </body>
</html>
"#,
  );
  let _result_url = site.write("result.html", "<!doctype html><html><body>ok</body></html>");

  let handle = spawn_ui_worker("fastr-ui-worker-fieldset-disabled").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let frame = recv_frame(&ui_rx, tab_id);
  // Focus + checkbox markers should start black.
  assert_pixel_rgb(&frame.pixmap, 170, 10, (0, 0, 0));
  assert_pixel_rgb(&frame.pixmap, 170, 40, (0, 0, 0));

  // Drain unrelated messages so assertions are scoped to each action.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Clicking the checkbox (disabled by the fieldset) must not toggle it.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 100.0),
      PointerButton::Primary,
    ))
    .expect("PointerDown");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 100.0),
      PointerButton::Primary,
    ))
    .expect("PointerUp");
  // Pointer events may trigger intermediate repaints (e.g. active state). Drain them so the frame we
  // assert against reflects the explicit repaint below.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(250));
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint");
  let frame = recv_frame(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 170, 40, (0, 0, 0));

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Tab from no focus should focus the legend-contained control (fieldset :focus-within marker).
  ui_tx
    .send(support::key_action(
      tab_id,
      fastrender::interaction::KeyAction::Tab,
    ))
    .expect("Tab");
  let _ = support::drain_for(&ui_rx, Duration::from_millis(250));
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint");
  let frame = recv_frame(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 170, 10, (255, 0, 255));

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Next Tab should skip the disabled fieldset control and move to the outside input.
  ui_tx
    .send(support::key_action(
      tab_id,
      fastrender::interaction::KeyAction::Tab,
    ))
    .expect("Tab");
  let _ = support::drain_for(&ui_rx, Duration::from_millis(250));
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint");
  let frame = recv_frame(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 170, 10, (0, 255, 255));

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Clicking submit should navigate with form data excluding the disabled control ("b") and
  // including the first-legend control ("a").
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 180.0),
      PointerButton::Primary,
    ))
    .expect("PointerDown");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 180.0),
      PointerButton::Primary,
    ))
    .expect("PointerUp");

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("a=1&c=3"));
  let expected_url = expected.to_string();

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("join ui worker");
}
