#![cfg(feature = "browser_ui")]

use super::worker_harness::{assert_event_subsequence, WorkerEventKind, WorkerHarness, WorkerToUiEvent};
use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker};
use std::path::Path;
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
  h.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: CancelGens::new(),
  });
  h.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: viewport,
    dpr: 1.0,
  });
  tab_id
}

fn drain_after_frame(h: &WorkerHarness, mut events: Vec<WorkerToUiEvent>) -> Vec<WorkerToUiEvent> {
  events.extend(h.drain_events(std::time::Duration::from_millis(200)));
  events
}

#[test]
fn listbox_select_scroll_then_click_respects_element_scroll_offset() {
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
    UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 80),
    [255, 0, 0, 255],
    "expected marker to start red"
  );

  // Scroll the listbox down by ~2 rows.
  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 40.0),
      pointer_css: Some((10.0, 10.0)),
    },
  );
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
  h.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
  });

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::PointerUp {
      tab_id,
      pos_css: click_pos,
      button: PointerButton::Primary,
    },
  );
  let _ = drain_after_frame(&h, events);

  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 80),
    [0, 0, 255, 255],
    "expected click after listbox scroll to select option 3 and turn marker blue"
  );
}

#[test]
fn navigation_about_newtab_renders_frame() {
  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (200, 140));

  let (frame, events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Navigate {
      tab_id,
      url: "about:newtab".to_string(),
      reason: NavigationReason::TypedUrl,
    },
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
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("index.html");
  std::fs::write(&path, "<!doctype html><title>file</title><body>hello</body>").unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (160, 120));

  let (_frame, events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Navigate {
      tab_id,
      url: url.clone(),
      reason: NavigationReason::TypedUrl,
    },
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
  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (120, 80));

  h.send(UiToWorker::Navigate {
    tab_id,
    url: "ftp://example.com/".to_string(),
    reason: NavigationReason::TypedUrl,
  });

  let events = h.wait_for_event(std::time::Duration::from_secs(2), |ev| {
    matches!(ev, WorkerToUiEvent::NavigationFailed { .. })
  });
  assert!(
    events.iter().any(|ev| matches!(
      ev,
      WorkerToUiEvent::NavigationFailed { url, error, .. }
        if url == "ftp://example.com/" && !error.is_empty()
    )),
    "expected NavigationFailed with error, got {events:?}"
  );

  let drained = h.drain_default();
  assert!(
    drained
      .iter()
      .all(|ev| !matches!(ev, WorkerToUiEvent::FrameReady { .. })),
    "expected no FrameReady after unsupported navigation, got {drained:?}"
  );
}

#[test]
fn history_back_forward_emits_committed_urls() {
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
    UiToWorker::Navigate {
      tab_id,
      url: a_url.clone(),
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events_a);

  let (_, events_b) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Navigate {
      tab_id,
      url: b_url.clone(),
      reason: NavigationReason::TypedUrl,
    },
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
  assert_eq!(back_commit, Some((a_url.as_str(), false, true)));

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
fn scroll_emits_scroll_state_updated_and_frame_snap_and_clamp() {
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("scroll.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        html { scroll-snap-type: y mandatory; }
        .snap { height: 100px; scroll-snap-align: start; }
      </style>
      <div class="snap" style="background: rgb(255,0,0)"></div>
      <div class="snap" style="background: rgb(0,0,255)"></div>
    "#,
  )
  .unwrap();
  let url = file_url(&path);

  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (100, 100));
  let (_, events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events);

  // Scroll down; mandatory scroll snap should snap to the second section.
  let (_frame, scroll_events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 60.0),
      pointer_css: None,
    },
  );
  let scroll_events = drain_after_frame(&h, scroll_events);
  let scroll_y = scroll_events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::ScrollStateUpdated { scroll, .. } => Some(scroll.viewport.y),
    _ => None,
  });
  let scroll_y = scroll_y.expect("ScrollStateUpdated");
  assert!(
    (scroll_y - 100.0).abs() < 1.0,
    "expected scroll snap to ~100, got {scroll_y}"
  );

  // Scroll beyond the end; should clamp to max scroll (100px for 2x100px content in 100px viewport).
  let (_frame, clamp_events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 10_000.0),
      pointer_css: None,
    },
  );
  let clamp_events = drain_after_frame(&h, clamp_events);
  let clamp_y = clamp_events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::ScrollStateUpdated { scroll, .. } => Some(scroll.viewport.y),
    _ => None,
  });
  let clamp_y = clamp_y.expect("ScrollStateUpdated after clamp");
  assert!(
    (clamp_y - 100.0).abs() < 1.0,
    "expected clamp to ~100, got {clamp_y}"
  );
}

#[test]
fn interaction_click_link_navigates() {
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
    UiToWorker::Navigate {
      tab_id,
      url: a_url,
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events_a);

  h.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });
  h.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });

  let (_frame, events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(3));
  let events = drain_after_frame(&h, events);
  let committed = events.iter().find_map(|ev| match ev {
    WorkerToUiEvent::NavigationCommitted { url, .. } => Some(url.as_str()),
    _ => None,
  });
  assert_eq!(committed, Some(b_url.as_str()));
}

#[test]
fn interaction_text_input_triggers_repaint_and_frame_changes() {
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
    UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events);

  // Focus the input; the runtime should repaint due to focus state change.
  h.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });
  h.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });
  let (focused_frame, _events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(3));

  // Typing should mutate the DOM (value attribute) and trigger another repaint.
  let (typed_frame, _events) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::TextInput {
      tab_id,
      text: "x".to_string(),
    },
  );

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
    UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    },
  );
  let _ = drain_after_frame(&h, events);

  // Fire multiple scroll messages back-to-back.
  h.send(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 10.0),
    pointer_css: None,
  });
  h.send(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 20.0),
    pointer_css: None,
  });
  h.send(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 30.0),
    pointer_css: None,
  });

  let (frame, events) = h.wait_for_frame(tab_id, std::time::Duration::from_secs(3));
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
    extra_frames,
    1,
    "expected only one FrameReady for coalesced scroll, got {extra_frames} ({drained:?})"
  );
}
