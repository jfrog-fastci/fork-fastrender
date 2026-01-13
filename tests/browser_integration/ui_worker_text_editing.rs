#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;

use super::support::{
  create_tab_msg, key_action, navigate_msg, pointer_down, pointer_up, text_input,
  viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};

fn recv_until_frame(
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

fn pixel_rgba_unpremultiplied(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  let a = data[idx + 3];
  if a == 0 {
    return (0, 0, 0, 0);
  }
  let r = ((data[idx] as u16 * 255) / a as u16) as u8;
  let g = ((data[idx + 1] as u16 * 255) / a as u16) as u8;
  let b = ((data[idx + 2] as u16 * 255) / a as u16) as u8;
  (r, g, b, a)
}

fn rgb_near(a: (u8, u8, u8), b: (u8, u8, u8), tol: u8) -> bool {
  let dr = a.0.abs_diff(b.0);
  let dg = a.1.abs_diff(b.1);
  let db = a.2.abs_diff(b.2);
  dr <= tol && dg <= tol && db <= tol
}

fn caret_column_in_rect(
  pixmap: &tiny_skia::Pixmap,
  rect: (u32, u32, u32, u32),
  caret_rgb: (u8, u8, u8),
) -> Option<u32> {
  let (x0, y0, w, h) = rect;
  let x1 = (x0 + w).min(pixmap.width());
  let y1 = (y0 + h).min(pixmap.height());
  if x0 >= x1 || y0 >= y1 {
    return None;
  }

  let mut counts = vec![0u32; (x1 - x0) as usize];
  for y in y0..y1 {
    for x in x0..x1 {
      let (r, g, b, a) = pixel_rgba_unpremultiplied(pixmap, x, y);
      if a >= 80 && rgb_near((r, g, b), caret_rgb, 12) {
        counts[(x - x0) as usize] = counts[(x - x0) as usize].saturating_add(1);
      }
    }
  }

  let (best_dx, best) = counts.iter().enumerate().max_by_key(|(_, count)| *count)?;
  if *best < 6 {
    return None;
  }
  Some(x0 + best_dx as u32)
}

fn assert_pixel_rgb(pixmap: &tiny_skia::Pixmap, x: u32, y: u32, expected: (u8, u8, u8)) {
  let (r, g, b, a) = pixel_rgba_unpremultiplied(pixmap, x, y);
  assert_eq!(
    (r, g, b, a),
    (expected.0, expected.1, expected.2, 255),
    "unexpected pixel at ({x}, {y})"
  );
}

#[test]
fn ui_worker_text_input_caret_moves_and_inserts_in_middle() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
            #txt {
              position: absolute;
              left: 10px;
              top: 10px;
              width: 180px;
              height: 50px;
              padding: 0;
              border: 0;
              font-family: "Noto Sans Mono", monospace;
              font-size: 32px;
              line-height: 40px;
              color: rgb(255,255,255);
              background: rgb(20,20,20);
              caret-color: rgb(255,0,255);
            }
            #box {
              position: absolute;
              left: 10px;
              top: 70px;
              width: 50px;
              height: 50px;
              background: rgb(255,0,0);
            }
            input[value="aXbc"] + #box { background: rgb(0,255,0); }
          </style>
        </head>
        <body>
          <input id="txt" value="abc" />
          <div id="box"></div>
        </body>
      </html>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-text-editing-input").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 140), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Initial frame.
  let _ = recv_until_frame(&ui_rx, tab_id);

  // Click to focus the input.
  ui_tx
    .send(pointer_down(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerUp");
  // Consume the PointerDown/Up repaints.
  let _ = recv_until_frame(&ui_rx, tab_id);
  let _ = recv_until_frame(&ui_rx, tab_id);

  let caret_rgb = (255, 0, 255);
  let input_rect = (10, 10, 180, 50);

  // Put caret at end and record x.
  ui_tx.send(key_action(tab_id, KeyAction::End)).expect("End");
  let frame_end = recv_until_frame(&ui_rx, tab_id);
  let x_end = caret_column_in_rect(&frame_end.pixmap, input_rect, caret_rgb).expect("caret at end");

  // Move caret to start and record x.
  ui_tx
    .send(key_action(tab_id, KeyAction::Home))
    .expect("Home");
  let frame_home = recv_until_frame(&ui_rx, tab_id);
  let x_home =
    caret_column_in_rect(&frame_home.pixmap, input_rect, caret_rgb).expect("caret at home");
  assert!(
    x_home + 30 < x_end,
    "expected Home caret x to be well left of End (home={x_home}, end={x_end})"
  );

  // Move caret one character to the right.
  ui_tx
    .send(key_action(tab_id, KeyAction::ArrowRight))
    .expect("ArrowRight");
  let frame_after_1 = recv_until_frame(&ui_rx, tab_id);
  let x_after_1 =
    caret_column_in_rect(&frame_after_1.pixmap, input_rect, caret_rgb).expect("caret after 1");
  assert!(
    x_after_1 > x_home + 5,
    "expected ArrowRight to move caret right (home={x_home}, after_1={x_after_1})"
  );
  let char_w = x_after_1 - x_home;

  // Insert in the middle: "abc" -> "aXbc".
  ui_tx.send(text_input(tab_id, "X")).expect("TextInput");
  let frame_insert = recv_until_frame(&ui_rx, tab_id);

  // CSS adjacent sibling selector should turn the box green when the value matches.
  assert_pixel_rgb(&frame_insert.pixmap, 35, 95, (0, 255, 0));

  // Caret should now be after two characters ("aX").
  let x_after_insert =
    caret_column_in_rect(&frame_insert.pixmap, input_rect, caret_rgb).expect("caret after insert");
  let expected = x_home.saturating_add(char_w.saturating_mul(2));
  let delta = x_after_insert.abs_diff(expected);
  assert!(
    delta <= 3,
    "expected caret x after insert to be at column 2 (expected~{expected}, got {x_after_insert}, delta={delta}, char_w={char_w})"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn ui_worker_text_input_ime_preedit_renders_at_caret_and_honors_ime_cursor() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
            #txt {
              position: absolute;
              left: 10px;
              top: 10px;
              width: 180px;
              height: 50px;
              padding: 0;
              border: 0;
              font-family: "Noto Sans Mono", monospace;
              font-size: 32px;
              line-height: 40px;
              color: rgb(255,255,255);
              background: rgb(20,20,20);
              caret-color: rgb(255,0,255);
            }
            #box {
              position: absolute;
              left: 10px;
              top: 70px;
              width: 50px;
              height: 50px;
              background: rgb(255,0,0);
            }
            input[value="aXbc"] + #box { background: rgb(0,255,0); }
          </style>
        </head>
        <body>
          <input id="txt" value="abc" />
          <div id="box"></div>
        </body>
      </html>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-ime-preedit").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 140), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = recv_until_frame(&ui_rx, tab_id);

  // Click to focus the input.
  ui_tx
    .send(pointer_down(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerUp");
  let _ = recv_until_frame(&ui_rx, tab_id);
  let _ = recv_until_frame(&ui_rx, tab_id);

  let caret_rgb = (255, 0, 255);
  let input_rect = (10, 10, 180, 50);

  // Put caret at end and record x.
  ui_tx.send(key_action(tab_id, KeyAction::End)).expect("End");
  let frame_end = recv_until_frame(&ui_rx, tab_id);
  let x_end = caret_column_in_rect(&frame_end.pixmap, input_rect, caret_rgb).expect("caret at end");

  // Move caret to start and then right by one char.
  ui_tx
    .send(key_action(tab_id, KeyAction::Home))
    .expect("Home");
  let frame_home = recv_until_frame(&ui_rx, tab_id);
  let x_home =
    caret_column_in_rect(&frame_home.pixmap, input_rect, caret_rgb).expect("caret at home");
  ui_tx
    .send(key_action(tab_id, KeyAction::ArrowRight))
    .expect("ArrowRight");
  let frame_after_1 = recv_until_frame(&ui_rx, tab_id);
  let x_after_1 =
    caret_column_in_rect(&frame_after_1.pixmap, input_rect, caret_rgb).expect("caret after 1");
  let char_w = x_after_1 - x_home;

  // Start an IME composition with a cursor inside the preedit string ("XY" with cursor after "X"),
  // so the painted caret must honor the IME cursor rather than always jumping to the end.
  ui_tx
    .send(UiToWorker::ImePreedit {
      tab_id,
      text: "XY".to_string(),
      cursor: Some((1, 1)),
    })
    .expect("ImePreedit");
  let frame_preedit = recv_until_frame(&ui_rx, tab_id);

  // Preedit is paint-only; DOM value should remain "abc", so the sibling box stays red.
  assert_pixel_rgb(&frame_preedit.pixmap, 35, 95, (255, 0, 0));

  let x_preedit =
    caret_column_in_rect(&frame_preedit.pixmap, input_rect, caret_rgb).expect("caret during IME");
  let expected = x_home.saturating_add(char_w.saturating_mul(2));
  let delta = x_preedit.abs_diff(expected);
  assert!(
    delta <= 3,
    "expected caret during IME to be after the preedit at column 2 (expected~{expected}, got {x_preedit}, delta={delta}, char_w={char_w})"
  );
  assert!(
    x_preedit + 30 < x_end,
    "expected IME caret x to not jump to end (ime={x_preedit}, end={x_end})"
  );

  // Cancelling composition should restore the caret location (still after the first char).
  ui_tx
    .send(UiToWorker::ImeCancel { tab_id })
    .expect("ImeCancel");
  let frame_cancel = recv_until_frame(&ui_rx, tab_id);
  let x_cancel =
    caret_column_in_rect(&frame_cancel.pixmap, input_rect, caret_rgb).expect("caret after cancel");
  let delta = x_cancel.abs_diff(x_after_1);
  assert!(
    delta <= 3,
    "expected caret x after IME cancel to return to original caret (expected~{x_after_1}, got {x_cancel}, delta={delta})"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn ui_worker_textarea_click_places_caret_on_clicked_line() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
            #ta {
              position: absolute;
              left: 10px;
              top: 10px;
              width: 180px;
              height: 110px;
              padding: 0;
              border: 0;
              font-family: "Noto Sans Mono", monospace;
              font-size: 32px;
              line-height: 40px;
              color: rgb(255,255,255);
              background: rgb(20,20,20);
              caret-color: rgb(255,0,255);
            }
          </style>
        </head>
        <body>
          <textarea id="ta">ab
cd</textarea>
        </body>
      </html>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-text-editing-textarea").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 140), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = recv_until_frame(&ui_rx, tab_id);

  // Click on the *second* line inside the textarea (y ~ 10px + 40px).
  let click = (20.0, 60.0);
  ui_tx
    .send(pointer_down(tab_id, click, PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, click, PointerButton::Primary))
    .expect("PointerUp");
  let _ = recv_until_frame(&ui_rx, tab_id);
  let frame = recv_until_frame(&ui_rx, tab_id);

  let caret_rgb = (255, 0, 255);
  let textarea_rect = (10, 10, 180, 110);
  let caret_x =
    caret_column_in_rect(&frame.pixmap, textarea_rect, caret_rgb).expect("caret column");

  // Find the minimum y where the caret color appears in the chosen column.
  let (_x0, y0, _w, h) = textarea_rect;
  let y1 = (y0 + h).min(frame.pixmap.height());
  let mut min_y: Option<u32> = None;
  for y in y0..y1 {
    let (r, g, b, a) = pixel_rgba_unpremultiplied(&frame.pixmap, caret_x, y);
    if a >= 80 && rgb_near((r, g, b), caret_rgb, 12) {
      min_y = Some(min_y.map_or(y, |prev| prev.min(y)));
    }
  }
  let min_y = min_y.expect("caret pixels in chosen column");

  // The click was on the second line (line-height = 40px). The caret's top should be noticeably
  // below the first line.
  assert!(
    min_y >= y0 + 30,
    "expected caret y to be on the second line (min_y={min_y})"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
