use fastrender::scroll::{ScrollBounds, ScrollState};
use fastrender::ui::browser_limits::DEFAULT_MAX_DIM_PX;
use fastrender::ui::messages::{CursorKind, RenderedFrame, ScrollMetrics, WorkerToUi};
use fastrender::ui::protocol_limits::{MAX_ERROR_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES};
use fastrender::ui::url::sanitize_worker_url_for_ui;
use fastrender::ui::BrowserAppState;

fn make_overlong_url(max_bytes: usize) -> String {
  let target_len = max_bytes + 1;
  let prefix = "https://example.com/";
  assert!(
    prefix.len() < target_len,
    "prefix must be shorter than the target length"
  );
  let filler_len = target_len - prefix.len();
  format!("{prefix}{}", "a".repeat(filler_len))
}

#[test]
fn overlong_navigation_strings_are_clamped() {
  let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let tab_id = app.active_tab_id().unwrap();

  // Overlong committed URL is clamped/dropped (but must never be stored at attacker-controlled size).
  let url = make_overlong_url(MAX_URL_BYTES);

  app.apply_worker_msg(WorkerToUi::NavigationCommitted {
    tab_id,
    url,
    title: Some("OK".to_string()),
    can_go_back: false,
    can_go_forward: false,
  });

  let tab = app.active_tab().unwrap();
  assert!(
    tab.current_url.as_ref().unwrap().len() <= MAX_URL_BYTES,
    "expected committed URL to be clamped to MAX_URL_BYTES"
  );

  // Overlong title is clamped when URL is otherwise valid.
  let title = "t".repeat(MAX_TITLE_BYTES + 1);
  app.apply_worker_msg(WorkerToUi::NavigationCommitted {
    tab_id,
    url: "https://example.com/".to_string(),
    title: Some(title),
    can_go_back: false,
    can_go_forward: false,
  });

  let tab = app.active_tab().unwrap();
  assert!(
    tab.title.as_ref().is_some_and(|t| t.len() <= MAX_TITLE_BYTES),
    "expected committed title to be clamped to MAX_TITLE_BYTES"
  );

  // Overlong error strings are clamped when URL is otherwise valid.
  let error = "e".repeat(MAX_ERROR_BYTES + 1);
  app.apply_worker_msg(WorkerToUi::NavigationFailed {
    tab_id,
    url: "https://example.com/".to_string(),
    error,
    can_go_back: false,
    can_go_forward: false,
  });

  let tab = app.active_tab().unwrap();
  assert!(
    tab.error.as_ref().is_some_and(|e| e.len() <= MAX_ERROR_BYTES),
    "expected navigation error to be clamped to MAX_ERROR_BYTES"
  );
}

#[test]
fn hover_changed_rejects_disallowed_schemes() {
  let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let tab_id = app.active_tab_id().unwrap();

  app.apply_worker_msg(WorkerToUi::HoverChanged {
    tab_id,
    hovered_url: Some("javascript:alert(1)".to_string()),
    tooltip: None,
    cursor: CursorKind::Pointer,
    tooltip: None,
  });
  assert!(
    app.active_tab().unwrap().hovered_url.is_none(),
    "expected javascript: hovered URLs to be ignored"
  );

  app.apply_worker_msg(WorkerToUi::HoverChanged {
    tab_id,
    hovered_url: Some("data:text/plain,hello".to_string()),
    tooltip: None,
    cursor: CursorKind::Pointer,
    tooltip: None,
  });
  assert!(
    app.active_tab().unwrap().hovered_url.is_none(),
    "expected data: hovered URLs to be ignored"
  );
}

#[test]
fn context_menu_urls_reject_disallowed_schemes() {
  assert_eq!(
    sanitize_worker_url_for_ui("javascript:alert(1)"),
    None,
    "expected javascript: context menu URLs to be ignored"
  );
  assert_eq!(
    sanitize_worker_url_for_ui("data:text/plain,hello"),
    None,
    "expected data: context menu URLs to be ignored"
  );
  assert_eq!(
    sanitize_worker_url_for_ui("https://example.com/").as_deref(),
    Some("https://example.com/"),
    "expected https URLs to be accepted"
  );
}

#[test]
fn context_menu_urls_drop_overlong_inputs() {
  let url = make_overlong_url(MAX_URL_BYTES);
  assert_eq!(
    sanitize_worker_url_for_ui(&url),
    None,
    "expected overlong context menu URLs to be dropped"
  );
}

#[test]
fn favicon_with_mismatched_rgba_len_is_rejected() {
  let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let tab_id = app.active_tab_id().unwrap();

  let update = app.apply_worker_msg(WorkerToUi::Favicon {
    tab_id,
    width: 2,
    height: 2,
    rgba: vec![0u8; 15], // expected 16
  });

  assert!(
    update.favicon_ready.is_none(),
    "expected invalid favicon payload to be rejected"
  );
  assert!(
    app.active_tab().unwrap().favicon_meta.is_none(),
    "expected favicon metadata to not be updated for invalid payloads"
  );
}

#[test]
fn frame_ready_with_absurd_pixmap_dimensions_is_rejected() {
  let mut app = BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let tab_id = app.active_tab_id().unwrap();

  let pixmap = tiny_skia::Pixmap::new(DEFAULT_MAX_DIM_PX + 1, 1).expect("pixmap alloc");
  // Keep viewport/dpr consistent with the pixmap dimensions so the rejection is due to the browser
  // max-dimension limit, not a viewport↔pixmap mismatch.
  let viewport_css = (DEFAULT_MAX_DIM_PX + 1, 1);
  let scroll_metrics = ScrollMetrics {
    viewport_css,
    scroll_css: (0.0, 0.0),
    bounds_css: ScrollBounds {
      min_x: 0.0,
      min_y: 0.0,
      max_x: 0.0,
      max_y: 0.0,
    },
    content_css: (viewport_css.0 as f32, viewport_css.1 as f32),
  };

  let update = app.apply_worker_msg(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap,
      viewport_css,
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      scroll_metrics,
      next_tick: None,
    },
  });

  assert!(
    update.frame_ready.is_none(),
    "expected oversized pixmap frames to be rejected"
  );
  assert!(
    app.active_tab().unwrap().latest_frame_meta.is_none(),
    "expected tab frame metadata to not be updated for rejected frames"
  );
}
