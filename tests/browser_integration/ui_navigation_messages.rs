#![cfg(feature = "browser_ui")]

use fastrender::api::FastRenderFactory;
use fastrender::ui::browser_worker::BrowserWorker;
use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::RenderOptions;
use tempfile::tempdir;

#[test]
fn navigation_invalid_url_emits_navigation_failed() {
  let _lock = super::stage_listener_test_lock();
  let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
  let factory = FastRenderFactory::new().expect("factory");
  let mut worker = BrowserWorker::new(factory, tx);

  let url = "foo://example.com";
  worker
    .navigate(TabId(1), url, RenderOptions::new().with_viewport(32, 32))
    .expect("navigate should render about:error frame");

  let messages: Vec<WorkerToUi> = rx.try_iter().collect();
  let failed = messages.iter().find_map(|msg| match msg {
    WorkerToUi::NavigationFailed { url: msg_url, error, .. } if msg_url == url => Some(error),
    _ => None,
  });

  let Some(error) = failed else {
    panic!("expected NavigationFailed message for {url:?}, got {messages:?}");
  };
  assert!(
    !error.as_str().trim().is_empty(),
    "expected non-empty NavigationFailed error string"
  );
}

#[test]
fn navigation_file_url_emits_started_committed_and_loading_toggle() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("index.html");
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><title>Hello</title></head><body>Hi</body></html>",
  )
  .expect("write html");

  let url = format!("file://{}/index.html", dir.path().display());
  let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
  let factory = FastRenderFactory::new().expect("factory");
  let mut worker = BrowserWorker::new(factory, tx);

  worker
    .navigate(TabId(1), &url, RenderOptions::new().with_viewport(32, 32))
    .expect("navigate");

  let messages: Vec<WorkerToUi> = rx.try_iter().collect();

  let mut started_idx = None;
  let mut committed_idx = None;
  let mut loading_true_idx = None;
  let mut loading_false_idx = None;

  for (idx, msg) in messages.iter().enumerate() {
    match msg {
      WorkerToUi::NavigationStarted { url: msg_url, .. } if msg_url == &url => {
        started_idx.get_or_insert(idx);
      }
      WorkerToUi::NavigationCommitted {
        url: msg_url,
        title,
        can_go_back,
        can_go_forward,
        ..
      } if msg_url == &url => {
        committed_idx.get_or_insert(idx);
        assert_eq!(title.as_deref(), Some("Hello"));
        assert!(!can_go_back);
        assert!(!can_go_forward);
      }
      WorkerToUi::LoadingState { loading: true, .. } => {
        loading_true_idx.get_or_insert(idx);
      }
      WorkerToUi::LoadingState { loading: false, .. } => {
        loading_false_idx.get_or_insert(idx);
      }
      _ => {}
    }
  }

  let started_idx = started_idx.unwrap_or_else(|| {
    panic!("expected NavigationStarted for {url:?}, got {messages:?}");
  });
  let committed_idx = committed_idx.unwrap_or_else(|| {
    panic!("expected NavigationCommitted for {url:?}, got {messages:?}");
  });
  assert!(
    started_idx < committed_idx,
    "expected NavigationStarted before NavigationCommitted"
  );

  let loading_true_idx = loading_true_idx.unwrap_or_else(|| {
    panic!("expected LoadingState {{ loading: true }} message, got {messages:?}");
  });
  let loading_false_idx = loading_false_idx.unwrap_or_else(|| {
    panic!("expected LoadingState {{ loading: false }} message, got {messages:?}");
  });
  assert!(
    loading_true_idx < loading_false_idx,
    "expected LoadingState true before false"
  );
}
