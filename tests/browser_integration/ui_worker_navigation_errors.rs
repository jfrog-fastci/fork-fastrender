#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

fn recv_until_deadline(
  rx: &fastrender::ui::WorkerToUiInbox,
  deadline: Instant,
) -> Option<WorkerToUi> {
  loop {
    let now = Instant::now();
    if now >= deadline {
      return None;
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => return Some(msg),
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => return None,
    }
  }
}

fn context_menu_link_at(
  tx: &Sender<UiToWorker>,
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  pos_css: (f32, f32),
  deadline: Instant,
) -> Option<Option<String>> {
  tx.send(UiToWorker::ContextMenuRequest {
    tab_id,
    pos_css,
    modifiers: PointerModifiers::NONE,
  })
    .expect("send context menu request");
  loop {
    let msg = recv_until_deadline(rx, deadline)?;
    match msg {
      WorkerToUi::ContextMenu {
        tab_id: msg_tab,
        pos_css: msg_pos,
        link_url,
        ..
      } if msg_tab == tab_id && msg_pos == pos_css => return Some(link_url),
      _ => continue,
    }
  }
}

#[test]
fn missing_file_navigation_emits_navigation_failed_renders_error_frame_and_stops_loading() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-missing-file-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  let viewport_css = (320, 240);
  ui_tx
    .send(viewport_changed_msg(tab_id, viewport_css, 1.0))
    .expect("viewport");

  ui_tx
    .send(navigate_msg(
      tab_id,
      missing_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("send navigate");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;

  let mut saw_started = false;
  let mut saw_loading_true = false;
  let mut saw_failed = false;
  let mut saw_error_frame = false;
  let mut saw_loading_false = false;

  while !(saw_failed && saw_error_frame && saw_loading_false) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for navigation failure flow (started={saw_started}, loading_true={saw_loading_true}, failed={saw_failed}, frame={saw_error_frame}, loading_false={saw_loading_false})"
      );
    };

    match msg {
      WorkerToUi::NavigationStarted {
        tab_id: msg_tab,
        url,
      } if msg_tab == tab_id => {
        assert_eq!(url, missing_url);
        saw_started = true;
      }
      WorkerToUi::LoadingState {
        tab_id: msg_tab,
        loading,
      } if msg_tab == tab_id => {
        if loading {
          saw_loading_true = true;
        } else {
          saw_loading_false = true;
        }
      }
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        ..
      } if msg_tab == tab_id => {
        assert!(
          saw_started,
          "expected NavigationStarted before NavigationFailed"
        );
        assert!(
          saw_loading_true,
          "expected LoadingState(true) before NavigationFailed"
        );
        assert_eq!(url, missing_url);
        assert!(!error.is_empty(), "expected non-empty error string");
        saw_failed = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab, ..
      } if msg_tab == tab_id => {
        // The worker should render an `about:error` fallback for missing files.
        assert!(saw_failed, "expected NavigationFailed before FrameReady");
        saw_error_frame = true;
      }
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab, ..
      } if msg_tab == tab_id => {
        panic!("missing-file navigation should not commit");
      }
      _ => {}
    }
  }

  // The `about:error` fallback should include a Retry link pointing at the original URL. The
  // integration test probes this via `ContextMenuRequest` hit-testing to avoid relying on pixel
  // comparisons or fragile pointer coordinates.
  let scan_deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut found_retry_link = false;
  let mut seen_links = std::collections::BTreeSet::new();
  let max_y = viewport_css.1 as usize;
  let max_x = viewport_css.0 as usize;
  // The error page places the Retry button below the header nav, which can put it very close to
  // the bottom edge of small viewports. Include an extra scan row/column at the max extents so we
  // don't accidentally miss edge-aligned links when stepping.
  let mut y_positions: Vec<usize> = (0..max_y).step_by(16).collect();
  let mut x_positions: Vec<usize> = (0..max_x).step_by(16).collect();
  if max_y > 0 {
    y_positions.push(max_y - 1);
  }
  if max_x > 0 {
    x_positions.push(max_x - 1);
  }
  y_positions.sort_unstable();
  y_positions.dedup();
  x_positions.sort_unstable();
  x_positions.dedup();

  // The error page header nav can be tall enough on small viewports that the Retry button is
  // initially below the fold. Scan the visible viewport first, then scroll down in a few steps
  // and re-scan to find the link deterministically.
  for _ in 0..8 {
    for y in y_positions.iter().copied() {
      for x in x_positions.iter().copied() {
        let pos = (x as f32 + 0.5, y as f32 + 0.5);
        let link_url =
          context_menu_link_at(&ui_tx, &ui_rx, tab_id, pos, scan_deadline).unwrap_or(None);
        if let Some(url) = link_url.as_deref() {
          seen_links.insert(url.to_string());
        }
        if link_url.as_deref() == Some(missing_url.as_str()) {
          found_retry_link = true;
          break;
        }
      }
      if found_retry_link {
        break;
      }
    }
    if found_retry_link {
      break;
    }

    ui_tx
      .send(UiToWorker::Scroll {
        tab_id,
        delta_css: (0.0, 160.0),
        pointer_css: None,
      })
      .expect("scroll error page");
    loop {
      let Some(msg) = recv_until_deadline(&ui_rx, scan_deadline) else {
        break;
      };
      if matches!(msg, WorkerToUi::FrameReady { tab_id: msg_tab, .. } if msg_tab == tab_id)
        || matches!(msg, WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, .. } if msg_tab == tab_id)
      {
        break;
      }
    }
  }
  assert!(
    found_retry_link,
    "expected about:error to contain a Retry link to the original URL {missing_url} (seen link URLs: {seen_links:?})"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn unknown_about_page_still_commits_and_renders_error_page() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-unknown-about-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  let url = "about:does-not-exist".to_string();
  ui_tx
    .send(navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("send navigate");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_commit = false;
  let mut saw_frame = false;

  while !(saw_commit && saw_frame) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for about navigation messages (commit={saw_commit}, frame={saw_frame})"
      );
    };

    match msg {
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url: committed,
        ..
      } if msg_tab == tab_id => {
        assert_eq!(committed, url);
        saw_commit = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab, ..
      } if msg_tab == tab_id => {
        saw_frame = true;
      }
      _ => {}
    }
  }

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn missing_file_navigation_renders_about_error_frame_and_updates_nav_flags() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let worker =
    spawn_ui_worker("fastr-ui-worker-missing-file-history-test").expect("spawn ui worker");

  let tab_id = TabId::new();
  worker
    .ui_tx
    .send(create_tab_msg(tab_id, Some("about:newtab".to_string())))
    .expect("create tab");
  worker
    .ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  // Wait for the initial about:newtab navigation to paint so history exists.
  super::support::recv_for_tab(&worker.ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");

  // Navigate to a file:// URL that does not exist.
  worker
    .ui_tx
    .send(navigate_msg(
      tab_id,
      missing_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate missing file");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_frame = false;
  let mut saw_failed = false;

  while Instant::now() < deadline {
    let Some(msg) = recv_until_deadline(&worker.ui_rx, deadline) else {
      break;
    };

    match msg {
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        can_go_back,
        can_go_forward,
        ..
      } if msg_tab == tab_id => {
        if url != missing_url {
          continue;
        }
        assert!(!error.is_empty(), "expected non-empty error string");
        assert!(
          can_go_back,
          "expected can_go_back=true after failed navigation from about:newtab"
        );
        assert!(
          !can_go_forward,
          "expected can_go_forward=false after a new failing navigation"
        );
        saw_failed = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      } if msg_tab == tab_id => {
        if !saw_failed {
          // The fallback error page should only be painted after we report the failure.
          continue;
        }
        assert!(
          frame.pixmap.width() > 0 && frame.pixmap.height() > 0,
          "expected non-empty pixmap for about:error fallback"
        );
        saw_frame = true;
      }
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url,
        ..
      } if msg_tab == tab_id => {
        if url == missing_url {
          panic!("missing-file navigation should not commit");
        }
      }
      _ => {}
    }

    if saw_failed && saw_frame {
      break;
    }
  }

  assert!(saw_failed, "expected NavigationFailed for missing file");
  assert!(
    saw_frame,
    "expected about:error fallback FrameReady after failure"
  );

  worker.join().expect("join ui worker");
}

#[test]
fn model_worker_missing_file_navigation_emits_navigation_failed_and_stops_loading() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let (ui_tx, ui_rx, join) = (worker.tx, worker.rx, worker.join);

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, Some(missing_url.clone())))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  let mut saw_started = false;
  let mut saw_loading_true = false;
  let mut saw_failed = false;
  let mut saw_frame = false;
  let mut saw_loading_false = false;
  let deadline = Instant::now() + DEFAULT_TIMEOUT;

  while !(saw_failed && saw_frame && saw_loading_false) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for missing-file navigation messages (started={saw_started}, loading_true={saw_loading_true}, failed={saw_failed}, frame={saw_frame}, loading_false={saw_loading_false})"
      );
    };

    match msg {
      WorkerToUi::NavigationStarted {
        tab_id: msg_tab,
        url,
      } if msg_tab == tab_id => {
        if url == missing_url {
          saw_started = true;
        }
      }
      WorkerToUi::LoadingState {
        tab_id: msg_tab,
        loading,
      } if msg_tab == tab_id => {
        if loading {
          saw_loading_true = true;
        } else {
          saw_loading_false = true;
        }
      }
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        ..
      } if msg_tab == tab_id => {
        assert!(
          saw_started,
          "expected NavigationStarted before NavigationFailed"
        );
        assert!(
          saw_loading_true,
          "expected LoadingState(true) before NavigationFailed"
        );
        assert_eq!(url, missing_url);
        assert!(!error.is_empty(), "expected non-empty error string");
        saw_failed = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      } if msg_tab == tab_id => {
        assert!(saw_failed, "expected NavigationFailed before FrameReady");
        assert!(
          frame.pixmap.width() > 0 && frame.pixmap.height() > 0,
          "expected a non-empty pixmap for about:error fallback"
        );
        saw_frame = true;
      }
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab, ..
      } if msg_tab == tab_id => {
        panic!("missing-file navigation should not commit");
      }
      _ => {}
    }
  }

  drop(ui_tx);
  join.join().expect("join browser worker");
}

#[test]
fn model_worker_unknown_about_page_still_commits_and_renders_error_page() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let (ui_tx, ui_rx, join) = (worker.tx, worker.rx, worker.join);

  let tab_id = TabId::new();
  let url = "about:does-not-exist".to_string();
  ui_tx
    .send(create_tab_msg(tab_id, Some(url.clone())))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_commit = false;
  let mut saw_frame = false;
  let mut saw_loading_false = false;

  while !(saw_commit && saw_frame && saw_loading_false) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for about navigation messages (commit={saw_commit}, frame={saw_frame}, loading_false={saw_loading_false})"
      );
    };

    match msg {
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url: committed,
        ..
      } if msg_tab == tab_id => {
        assert_eq!(committed, url);
        saw_commit = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab, ..
      } if msg_tab == tab_id => {
        saw_frame = true;
      }
      WorkerToUi::LoadingState {
        tab_id: msg_tab,
        loading,
      } if msg_tab == tab_id && !loading => {
        saw_loading_false = true;
      }
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url: failed,
        error,
        ..
      } if msg_tab == tab_id => {
        panic!("about navigation unexpectedly failed for {failed}: {error}");
      }
      _ => {}
    }
  }

  drop(ui_tx);
  join.join().expect("join browser worker");
}
