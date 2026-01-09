#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, DEFAULT_TIMEOUT};
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::ui::browser_worker::spawn_browser_ui_worker_thread;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::{Error, FastRender, FastRenderConfig, Result};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct StaticHtmlFetcher {
  responses: HashMap<String, (String, Option<String>)>,
}

impl StaticHtmlFetcher {
  fn with_html(mut self, url: &str, html: &str) -> Self {
    self
      .responses
      .insert(url.to_string(), (html.to_string(), None));
    self
  }

  fn with_redirect(mut self, url: &str, final_url: &str, html: &str) -> Self {
    self.responses.insert(
      url.to_string(),
      (html.to_string(), Some(final_url.to_string())),
    );
    self
  }
}

impl ResourceFetcher for StaticHtmlFetcher {
  fn fetch(&self, _url: &str) -> Result<FetchedResource> {
    panic!("tests expect fetch_with_request");
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    assert_eq!(
      req.destination,
      FetchDestination::Document,
      "expected document fetch"
    );
    let Some((html, final_url)) = self.responses.get(req.url) else {
      return Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing fixture: {}", req.url),
      )));
    };

    let mut res = FetchedResource::with_final_url(
      html.as_bytes().to_vec(),
      Some("text/html".to_string()),
      final_url.clone(),
    );
    res.status = Some(200);
    Ok(res)
  }
}

fn recv_nav_committed(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
) -> (String, bool, bool) {
  // Navigations can be CPU-heavy (font loading/layout/paint) and this integration test binary runs
  // many of them in parallel by default. Use a slightly more generous timeout to avoid flakes on
  // contended CI runners.
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  while Instant::now() < deadline {
    match rx.recv_timeout(Duration::from_millis(50)) {
      Ok(WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url,
        can_go_back,
        can_go_forward,
        ..
      }) if msg_tab == tab_id => return (url, can_go_back, can_go_forward),
      Ok(WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
      }) if msg_tab == tab_id => {
        panic!("navigation failed for {url}: {error}");
      }
      Ok(_) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker disconnected while waiting for NavigationCommitted for {tab_id:?}");
      }
    }
  }
  panic!("timed out waiting for NavigationCommitted for {tab_id:?}");
}

#[test]
fn per_tab_back_forward_state_machine() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let fetcher = Arc::new(
    StaticHtmlFetcher::default()
      .with_html(
        "https://example.test/a",
        "<!doctype html><title>A</title><body>A</body>",
      )
      .with_html(
        "https://example.test/b",
        "<!doctype html><title>B</title><body>B</body>",
      ),
  );
  let renderer = FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher))?;

  let (ui_tx, worker_rx) = std::sync::mpsc::channel::<WorkerToUi>();
  let (worker_tx, ui_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let handle =
    spawn_browser_ui_worker_thread("fastr-browser-history-test", renderer, ui_rx, ui_tx)?;

  let tab_id = TabId(1);
  worker_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  worker_tx
    .send(navigate_msg(
      tab_id,
      "https://example.test/a".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate a");
  let (committed_a, can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(committed_a, "https://example.test/a");
  assert!(!can_back);
  assert!(!can_forward);

  worker_tx
    .send(navigate_msg(
      tab_id,
      "https://example.test/b".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate b");
  let (committed_b, can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(committed_b, "https://example.test/b");
  assert!(can_back);
  assert!(!can_forward);

  worker_tx
    .send(UiToWorker::GoBack { tab_id })
    .expect("send GoBack");
  let (back_to_a, can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(back_to_a, "https://example.test/a");
  assert!(!can_back);
  assert!(can_forward);

  worker_tx
    .send(UiToWorker::GoForward { tab_id })
    .expect("send GoForward");
  let (forward_to_b, can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(forward_to_b, "https://example.test/b");
  assert!(can_back);
  assert!(!can_forward);

  drop(worker_tx);
  handle.join().expect("worker thread join");
  Ok(())
}

#[test]
fn redirects_commit_final_url_into_history_entry() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let fetcher = Arc::new(
    StaticHtmlFetcher::default()
      .with_html(
        "https://example.test/a",
        "<!doctype html><title>A</title><body>A</body>",
      )
      .with_redirect(
        "https://example.test/redirect",
        "https://example.test/final",
        "<!doctype html><title>Final</title><body>final</body>",
      )
      .with_html(
        "https://example.test/final",
        "<!doctype html><title>Final</title><body>final</body>",
      ),
  );
  let renderer = FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher))?;

  let (ui_tx, worker_rx) = std::sync::mpsc::channel::<WorkerToUi>();
  let (worker_tx, ui_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let handle = spawn_browser_ui_worker_thread(
    "fastr-browser-redirect-history-test",
    renderer,
    ui_rx,
    ui_tx,
  )?;

  let tab_id = TabId(1);
  worker_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  worker_tx
    .send(navigate_msg(
      tab_id,
      "https://example.test/a".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate a");
  let _ = recv_nav_committed(&worker_rx, tab_id);

  worker_tx
    .send(navigate_msg(
      tab_id,
      "https://example.test/redirect".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate redirect");
  let (committed, can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(committed, "https://example.test/final");
  assert!(can_back);
  assert!(!can_forward);

  worker_tx
    .send(UiToWorker::GoBack { tab_id })
    .expect("send GoBack");
  let (back_to_a, _can_back, can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(back_to_a, "https://example.test/a");
  assert!(can_forward);

  worker_tx
    .send(UiToWorker::GoForward { tab_id })
    .expect("send GoForward");
  let (forward_to_final, _can_back, _can_forward) = recv_nav_committed(&worker_rx, tab_id);
  assert_eq!(
    forward_to_final, "https://example.test/final",
    "forward navigation should use the committed final URL"
  );

  drop(worker_tx);
  handle.join().expect("worker thread join");
  Ok(())
}
