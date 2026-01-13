#![cfg(feature = "browser_ui")]

use super::support::{
  self, create_tab_msg, navigate_msg, scroll_at_pointer, scroll_viewport, viewport_changed_msg,
};
use super::worker_harness::{
  assert_event_subsequence, format_events, WorkerEventKind, WorkerHarness, WorkerToUiEvent,
};
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

fn file_url(path: &Path) -> String {
  Url::from_file_path(path)
    .ok()
    .expect("convert path to file:// URL")
    .to_string()
}

fn create_tab(h: &WorkerHarness, viewport: (u32, u32)) -> TabId {
  let tab_id = TabId::new();
  // The canonical UI worker does not auto-navigate when `initial_url` is `None`. These tests
  // exercise interactions against a live document, so start tabs at `about:newtab` explicitly.
  h.send(create_tab_msg(tab_id, Some("about:newtab".to_string())));
  h.send(viewport_changed_msg(tab_id, viewport, 1.0));
  // When `ViewportChanged` arrives while the initial navigation is preparing, the worker may emit an
  // initial frame at the default viewport and then repaint at the requested viewport. Wait until we
  // observe a frame at the desired dimensions so subsequent scroll/clamp assertions are deterministic.
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      panic!("timed out waiting for initial frame at viewport {viewport:?}");
    }
    let (frame, events) = h.wait_for_frame(tab_id, remaining);
    if frame.viewport_css == viewport {
      let _ = drain_after_frame(h, events);
      break;
    }
  }
  tab_id
}

fn drain_after_frame(h: &WorkerHarness, mut events: Vec<WorkerToUiEvent>) -> Vec<WorkerToUiEvent> {
  events.extend(h.drain_events(std::time::Duration::from_millis(200)));
  events
}

#[test]
fn pointer_move_sets_hover_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("hover.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head>
          <style>
             html, body { margin: 0; padding: 0; }
             #box { width:64px; height:64px; background: rgb(255, 0, 0); }
            #box:hover { background: rgb(0, 255, 0); }
           </style>
         </head>
         <body>
           <div id="box"></div>
        </body>
      </html>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (256, 256));

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);
  assert_eq!(support::rgba_at(&frame.pixmap, 10, 10), [255, 0, 0, 255]);

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    support::pointer_move(tab_id, (10.0, 10.0), PointerButton::None),
  );
  let _ = drain_after_frame(&h, events);
  assert_eq!(support::rgba_at(&frame.pixmap, 10, 10), [0, 255, 0, 255]);

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    support::pointer_move(tab_id, (200.0, 200.0), PointerButton::None),
  );
  let _ = drain_after_frame(&h, events);
  assert_eq!(support::rgba_at(&frame.pixmap, 10, 10), [255, 0, 0, 255]);
}

#[test]
fn post_navigation_js_pump_syncs_script_mutated_input_value() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("input_value_pump.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            input { display: none; }
            #marker { width:64px; height:64px; background: rgb(255, 0, 0); }
            input[value="pumped"] + #marker { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <input id="i">
          <div id="marker"></div>
          <script>
            // Mutate `input.value` (internal form-control state) without touching the `value=`
            // attribute. The UI worker's post-navigation JS pump should project this state into the
            // renderer DOM so attribute selectors reflect the change on first render.
            document.getElementById('i').value = 'pumped';
          </script>
        </body>
      </html>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (128, 128));

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);

  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected marker to reflect JS-updated input value"
  );
}

#[test]
fn listbox_select_scroll_then_click_respects_element_scroll_offset() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("select.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            select { display: block; width: 180px; height: 60px; margin: 0; padding: 0; border: 0; font-size: 20px; line-height: 20px; }
            #marker { width: 180px; height: 60px; background: rgb(255, 0, 0); }
            select:has(option#opt3[selected]) + #marker { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <select size="3">
            <option id="opt1">Option 1</option>
            <option id="opt2">Option 2</option>
            <option id="opt3">Option 3</option>
            <option id="opt4">Option 4</option>
            <option id="opt5">Option 5</option>
            <option id="opt6">Option 6</option>
            <option id="opt7">Option 7</option>
            <option id="opt8">Option 8</option>
            <option id="opt9">Option 9</option>
            <option id="opt10">Option 10</option>
          </select>
          <div id="marker"></div>
        </body>
      </html>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (200, 160));

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 80),
    [255, 0, 0, 255],
    "expected marker to start red"
  );

  // Scroll the listbox down by ~2 rows.
  let (frame, events) =
    h.send_and_wait_for_frame(tab_id, scroll_at_pointer(tab_id, (0.0, 40.0), (10.0, 10.0)));
  let _ = drain_after_frame(&h, events);
  assert!(
    frame
      .scroll_state
      .elements
      .values()
      .any(|offset| offset.y > 0.0),
    "expected listbox select to scroll an element, got scroll state {:?}",
    frame.scroll_state
  );

  // Click within the first visible row; the selection should account for the element scroll offset.
  let click_pos = (10.0_f32, 10.0_f32);
  h.send(support::pointer_down(
    tab_id,
    click_pos,
    PointerButton::Primary,
  ));
  // PointerDown can repaint (e.g. active/focus state). Drain that frame so the subsequent
  // PointerUp wait observes the post-click selection update.
  let (_frame, events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(3));
  let _ = drain_after_frame(&h, events);

  // PointerDown may repaint (active/focus styling), so `send_and_wait_for_frame` after PointerUp
  // could observe the earlier PointerDown frame. Keep waiting until the click-driven selection
  // change shows up.
  let (mut frame, events) = h.send_and_wait_for_frame(
    tab_id,
    support::pointer_up(tab_id, click_pos, PointerButton::Primary),
  );
  let _ = drain_after_frame(&h, events);

  if support::rgba_at(&frame.pixmap, 10, 80) != [0, 0, 255, 255] {
    let deadline = std::time::Instant::now() + support::DEFAULT_TIMEOUT;
    loop {
      let now = std::time::Instant::now();
      if now >= deadline {
        panic!("timed out waiting for listbox click to update marker");
      }
      let remaining = deadline.saturating_duration_since(now);
      let (next_frame, events) = h.wait_for_frame(tab_id, remaining);
      let _ = drain_after_frame(&h, events);
      frame = next_frame;
      if support::rgba_at(&frame.pixmap, 10, 80) == [0, 0, 255, 255] {
        break;
      }
    }
  }

  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 80),
    [0, 0, 255, 255],
    "expected click after listbox scroll to select option 3 and turn marker blue"
  );
}

#[test]
fn navigation_about_newtab_renders_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (200, 140));

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ),
  );

  assert_eq!(frame.viewport_css, (200, 140));
  assert_eq!(frame.pixmap.width(), 200);
  assert_eq!(frame.pixmap.height(), 140);

  let events = drain_after_frame(&h, events);
  assert_event_subsequence(
    &events,
    &[
      WorkerEventKind::NavigationStarted,
      WorkerEventKind::LoadingState(true),
      WorkerEventKind::FrameReady,
      WorkerEventKind::LoadingState(false),
    ],
  );
}

#[test]
fn navigation_file_url_emits_committed_and_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("index.html");
  std::fs::write(
    &path,
    "<!doctype html><title>file</title><body>hello</body>",
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (160, 120));

  let (_frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl),
  );
  let events = drain_after_frame(&h, events);

  let committed = events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::NavigationCommitted { url, .. } => Some(url.as_str()),
    _ => None,
  });
  assert_eq!(committed, Some(url.as_str()));

  assert_event_subsequence(
    &events,
    &[
      WorkerEventKind::NavigationStarted,
      WorkerEventKind::LoadingState(true),
      WorkerEventKind::NavigationCommitted,
      WorkerEventKind::FrameReady,
      WorkerEventKind::LoadingState(false),
    ],
  );
}

#[test]
fn navigation_unsupported_scheme_rejects_with_failed() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (120, 80));

  h.send(navigate_msg(
    tab_id,
    "ftp://example.com/".to_string(),
    NavigationReason::TypedUrl,
  ));

  let deadline = Instant::now() + Duration::from_secs(5);
  let mut events = Vec::new();
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!(
        "timed out waiting for NavigationFailed; received:\n{}",
        format_events(&events)
      );
    }
    let remaining = deadline.saturating_duration_since(now);
    let ev = h
      .recv_event(remaining)
      .unwrap_or_else(|err| panic!("waiting for NavigationFailed: {err}"));
    let done = matches!(ev, WorkerToUiEvent::NavigationFailed { .. });
    events.push(ev);
    if done {
      break;
    }
  }
  assert!(
    events.iter().any(|ev| matches!(
      ev,
      WorkerToUiEvent::NavigationFailed { url, error, .. }
        if url == "ftp://example.com/" && !error.is_empty()
    )),
    "expected NavigationFailed with error, got {events:?}"
  );

  // Unsupported URL schemes should fail fast without rendering a new `about:error` frame; the tab
  // should keep showing the previous page.
  let drained = h.drain_events(std::time::Duration::from_millis(200));
  assert!(
    !drained
      .iter()
      .any(|ev| matches!(ev, WorkerToUiEvent::FrameReady { .. })),
    "expected no FrameReady after unsupported-scheme navigation; got {drained:?}"
  );
}

#[test]
fn history_back_forward_emits_committed_urls() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let a_path = dir.path().join("a.html");
  let b_path = dir.path().join("b.html");
  std::fs::write(&a_path, "<!doctype html><title>A</title><body>A</body>").unwrap();
  std::fs::write(&b_path, "<!doctype html><title>B</title><body>B</body>").unwrap();
  let a_url = file_url(&a_path);
  let b_url = file_url(&b_path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (140, 90));

  let (_, events_a) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, a_url.clone(), NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events_a);

  let (_, events_b) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, b_url.clone(), NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events_b);

  let (_, back_events) = h.send_and_wait_for_frame(tab_id, UiToWorker::GoBack { tab_id });
  let back_events = drain_after_frame(&h, back_events);
  let back_commit = back_events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::NavigationCommitted {
      url,
      can_go_back,
      can_go_forward,
      ..
    } => Some((url.as_str(), *can_go_back, *can_go_forward)),
    _ => None,
  });
  assert_eq!(back_commit, Some((a_url.as_str(), true, true)));

  let (_, forward_events) = h.send_and_wait_for_frame(tab_id, UiToWorker::GoForward { tab_id });
  let forward_events = drain_after_frame(&h, forward_events);
  let forward_commit = forward_events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::NavigationCommitted {
      url,
      can_go_back,
      can_go_forward,
      ..
    } => Some((url.as_str(), *can_go_back, *can_go_forward)),
    _ => None,
  });
  assert_eq!(forward_commit, Some((b_url.as_str(), true, false)));
}

#[test]
fn reload_preserves_scroll_offset() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("scroll.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        #spacer { height: 2000px; }
      </style>
      <div id="spacer"></div>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (100, 100));

  let (_, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);

  let (frame, scroll_events) = h.send_and_wait_for_frame(tab_id, scroll_viewport(tab_id, (0.0, 120.0)));
  let _ = drain_after_frame(&h, scroll_events);
  let scrolled_y = frame.scroll_state.viewport.y;
  assert!(
    (scrolled_y - 120.0).abs() < 1.0,
    "expected scroll to move to ~120, got {scrolled_y}"
  );

  let (frame, reload_events) = h.send_and_wait_for_frame(tab_id, UiToWorker::Reload { tab_id });
  let _ = drain_after_frame(&h, reload_events);
  let reloaded_y = frame.scroll_state.viewport.y;
  assert!(
    (reloaded_y - scrolled_y).abs() < 1.0,
    "expected reload to preserve scroll (before={scrolled_y}, after={reloaded_y})"
  );
}

#[test]
fn scroll_updates_scroll_state_and_frame_snap_and_clamp() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("scroll.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html { scroll-snap-type: y mandatory; }
            .snap { height: 100px; scroll-snap-align: start; }
          </style>
        </head>
        <body>
          <div class="snap" style="background: rgb(255,0,0)"></div>
          <div class="snap" style="background: rgb(0,0,255)"></div>
          <div class="snap" style="background: rgb(0,255,0)"></div>
        </body>
      </html>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (100, 100));
  let (_nav_frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);

  // Scroll down; mandatory scroll snap should snap to the second section.
  let (frame, scroll_events) = h.send_and_wait_for_frame(tab_id, scroll_viewport(tab_id, (0.0, 60.0)));
  let _ = drain_after_frame(&h, scroll_events);
  let scroll_y = frame.scroll_state.viewport.y;
  assert!(
    (scroll_y - 100.0).abs() < 1.0,
    "expected scroll snap to ~100, got {scroll_y}"
  );

  // Scroll beyond the end; should clamp to max scroll (200px for 3x100px content in 100px viewport).
  let (frame, clamp_events) = h.send_and_wait_for_frame(tab_id, scroll_viewport(tab_id, (0.0, 10_000.0)));
  let _ = drain_after_frame(&h, clamp_events);
  let clamp_y = frame.scroll_state.viewport.y;
  assert!(
    (clamp_y - 200.0).abs() < 1.0,
    "expected clamp to ~200, got {clamp_y}"
  );
}

#[test]
fn interaction_click_link_navigates() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let a_path = dir.path().join("a.html");
  let b_path = dir.path().join("b.html");
  std::fs::write(
    &a_path,
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        a { display: block; width: 200px; height: 200px; background: rgb(0, 255, 0); }
      </style>
      <a href="b.html">Go</a>
    "#,
  )
  .unwrap();
  std::fs::write(&b_path, "<!doctype html><title>B</title><body>B</body>").unwrap();
  let a_url = file_url(&a_path);
  let b_url = file_url(&b_path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (200, 200));
  let (_, events_a) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, a_url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events_a);

  h.send(support::pointer_down(
    tab_id,
    (10.0, 10.0),
    PointerButton::Primary,
  ));
  h.send(support::pointer_up(
    tab_id,
    (10.0, 10.0),
    PointerButton::Primary,
  ));

  let (_frame, events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(10));
  let events = drain_after_frame(&h, events);
  let committed = events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::NavigationCommitted { url, .. } => Some(url.as_str()),
    _ => None,
  });
  assert_eq!(committed, Some(b_url.as_str()));
}

#[test]
fn interaction_text_input_triggers_repaint_and_frame_changes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("input.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        input { font-size: 24px; width: 180px; height: 40px; }
      </style>
      <input type="text" value="">
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (220, 80));
  let (initial_frame, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);

  // Focus the input; the runtime should repaint due to focus state change.
  h.send(support::pointer_down(
    tab_id,
    (10.0, 10.0),
    PointerButton::Primary,
  ));
  h.send(support::pointer_up(
    tab_id,
    (10.0, 10.0),
    PointerButton::Primary,
  ));
  let (focused_frame, _events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(10));

  // Typing should mutate the DOM (value attribute) and trigger another repaint.
  let (typed_frame, _events) = h.send_and_wait_for_frame(tab_id, support::text_input(tab_id, "x"));

  assert_ne!(
    focused_frame.pixmap.data(),
    typed_frame.pixmap.data(),
    "expected typing to change rendered output"
  );
  // Also ensure typing is not a no-op relative to the initial navigation frame.
  assert_ne!(
    initial_frame.pixmap.data(),
    typed_frame.pixmap.data(),
    "expected typing to change rendered output relative to initial frame"
  );
}

#[test]
fn cancellation_rapid_scroll_coalesces_to_last_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("long.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        .row { height: 100px; }
      </style>
      <div class="row" style="background: rgb(255,0,0)"></div>
      <div class="row" style="background: rgb(0,255,0)"></div>
      <div class="row" style="background: rgb(0,0,255)"></div>
      <div class="row" style="background: rgb(255,255,0)"></div>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (100, 100));
  let (_, events) = h.send_and_wait_for_frame(
    tab_id,
    navigate_msg(tab_id, url, NavigationReason::TypedUrl),
  );
  let _ = drain_after_frame(&h, events);

  // Fire multiple scroll messages back-to-back.
  h.send(scroll_viewport(tab_id, (0.0, 10.0)));
  h.send(scroll_viewport(tab_id, (0.0, 20.0)));
  h.send(scroll_viewport(tab_id, (0.0, 30.0)));

  let (frame, events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(10));
  assert!(
    (frame.scroll_state.viewport.y - 60.0).abs() < 1.0,
    "expected coalesced scroll to apply all deltas (10+20+30), got {:?}",
    frame.scroll_state.viewport
  );

  // Ensure no additional frames were produced for the intermediate scroll deltas.
  let drained = drain_after_frame(&h, events);
  let extra_frames = drained
    .iter()
    .filter(|ev| matches!(ev, WorkerToUiEvent::FrameReady { .. }))
    .count();
  assert_eq!(
    extra_frames, 1,
    "expected only one FrameReady for coalesced scroll, got {extra_frames} ({drained:?})"
  );
}
