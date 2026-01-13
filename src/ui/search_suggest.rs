//! Remote (network) search query suggestions for the omnibox.
//!
//! This module is intentionally **egui-agnostic** and can be polled from a UI thread without
//! blocking.
//!
//! # Default provider
//!
//! The default provider uses DuckDuckGo's autocomplete endpoint:
//!
//! ```text
//! GET https://duckduckgo.com/ac/?q=<query>&type=list
//! ```
//!
//! Response shape (JSON):
//!
//! ```json
//! [
//!   { "phrase": "rust", "score": 600, ... },
//!   { "phrase": "rust lang", "score": 550, ... }
//! ]
//! ```
//!
//! We only depend on the `"phrase"` field and ignore everything else.

use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime};

/// Environment override for [`SearchSuggestConfig::endpoint_base`].
///
/// This is primarily intended for deterministic tests (point at a local HTTP server).
pub const ENV_ENDPOINT_BASE: &str = "FASTR_SEARCH_SUGGEST_ENDPOINT_BASE";

pub const DEFAULT_ENDPOINT_BASE: &str = "https://duckduckgo.com/ac/";

const MAX_SUGGESTIONS: usize = 10;
const WORKER_DEBOUNCE: Duration = Duration::from_millis(150);
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_millis(300);

/// Conservative UA string (avoids leaking host details).
const USER_AGENT: &str = "fastrender/0.1 (search_suggest; +https://github.com/wilsonzlin/fastrender)";

#[derive(Debug, Clone)]
pub struct SearchSuggestConfig {
  /// Base URL of the suggestion endpoint (without the query string).
  ///
  /// For DuckDuckGo this is `https://duckduckgo.com/ac/`.
  pub endpoint_base: String,
  /// When false, the service is a no-op and never hits the network.
  pub enabled: bool,
  /// Timeout applied to connect + overall request.
  pub timeout_ms: u64,
  #[cfg(test)]
  worker_test_hook: Option<SearchSuggestWorkerTestHook>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct SearchSuggestWorkerTestHook {
  entered_request_tx: mpsc::Sender<()>,
  sleep: Duration,
}

impl Default for SearchSuggestConfig {
  fn default() -> Self {
    Self {
      endpoint_base: DEFAULT_ENDPOINT_BASE.to_string(),
      enabled: true,
      timeout_ms: 700,
      #[cfg(test)]
      worker_test_hook: None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSuggestUpdate {
  pub query: String,
  pub suggestions: Vec<String>,
  pub fetched_at: SystemTime,
}

#[derive(Debug)]
pub struct SearchSuggestService {
  latest_gen: Arc<AtomicU64>,
  next_gen: u64,
  request_tx: Option<mpsc::Sender<SearchSuggestRequest>>,
  update_rx: Option<mpsc::Receiver<SearchSuggestUpdate>>,
  worker_join: Option<std::thread::JoinHandle<()>>,
  // Used to suppress redundant requests when the UI is redrawn without input changes.
  last_requested_query: String,
}

#[derive(Debug, Clone)]
struct SearchSuggestRequest {
  gen: u64,
  query: String,
}

impl SearchSuggestService {
  pub fn new(mut config: SearchSuggestConfig) -> Self {
    if let Ok(raw) = std::env::var(ENV_ENDPOINT_BASE) {
      let trimmed = raw.trim();
      if !trimmed.is_empty() {
        config.endpoint_base = trimmed.to_string();
      }
    }

    let latest_gen = Arc::new(AtomicU64::new(0));

    if !config.enabled {
      return Self {
        latest_gen,
        next_gen: 0,
        request_tx: None,
        update_rx: None,
        worker_join: None,
        last_requested_query: String::new(),
      };
    }

    let (request_tx, request_rx) = mpsc::channel::<SearchSuggestRequest>();
    let (update_tx, update_rx) = mpsc::channel::<SearchSuggestUpdate>();

    let worker_latest_gen = Arc::clone(&latest_gen);
    let worker_join = std::thread::Builder::new()
      .name("search_suggest_worker".to_string())
      .spawn(move || worker_loop(config, worker_latest_gen, request_rx, update_tx))
      .ok();

    Self {
      latest_gen,
      next_gen: 0,
      request_tx: Some(request_tx),
      update_rx: Some(update_rx),
      worker_join,
      last_requested_query: String::new(),
    }
  }

  /// Request suggestions for `query`.
  ///
  /// This call is non-blocking: it only enqueues work for the background thread.
  pub fn request(&mut self, query: String) {
    let Some(tx) = self.request_tx.as_ref() else {
      return;
    };

    // Avoid spamming the worker when we get redraws without input changes.
    if self.last_requested_query == query {
      return;
    }
    // Keep the allocation for the cached query around. This runs on a hot path (omnibox typing).
    self.last_requested_query.clear();
    self.last_requested_query.push_str(&query);

    // `request` takes `&mut self`, so we can keep a non-atomic counter on the UI thread and only
    // publish the latest generation to the worker via an atomic store (cheaper than an RMW
    // `fetch_add` on every keystroke).
    self.next_gen = self.next_gen.wrapping_add(1);
    let gen = self.next_gen;
    self.latest_gen.store(gen, Ordering::Release);
    let _ = tx.send(SearchSuggestRequest { gen, query });
  }

  /// Non-blocking poll for updates from the worker.
  pub fn try_recv(&self) -> Option<SearchSuggestUpdate> {
    let rx = self.update_rx.as_ref()?;
    match rx.try_recv() {
      Ok(update) => Some(update),
      Err(mpsc::TryRecvError::Empty) => None,
      Err(mpsc::TryRecvError::Disconnected) => None,
    }
  }
}

impl Drop for SearchSuggestService {
  fn drop(&mut self) {
    // Closing the request channel tells the worker to exit.
    self.request_tx.take();
    if let Some(join) = self.worker_join.take() {
      // Best-effort join: don't risk hanging the UI thread forever if the worker is stuck in a
      // long/hung network request.
      let (done_tx, done_rx) = mpsc::channel::<std::thread::Result<()>>();
      // `JoinHandle` has no timeout API, so join on a helper thread and wait on a channel.
      //
      // If we fail to spawn the helper thread, dropping the closure drops `join`, which detaches
      // the worker thread. (The worker will still observe channel closure and exit eventually.)
      if std::thread::Builder::new()
        .name("search_suggest_join".to_string())
        .spawn(move || {
          let _ = done_tx.send(join.join());
        })
        .is_ok()
      {
        let _ = done_rx.recv_timeout(WORKER_JOIN_TIMEOUT);
      }
    }
  }
}

fn worker_loop(
  config: SearchSuggestConfig,
  latest_gen: Arc<AtomicU64>,
  request_rx: mpsc::Receiver<SearchSuggestRequest>,
  update_tx: mpsc::Sender<SearchSuggestUpdate>,
) {
  while let Ok(mut req) = request_rx.recv() {
    // Debounce: collapse rapid keystrokes into a single network request.
    loop {
      match request_rx.recv_timeout(WORKER_DEBOUNCE) {
        Ok(newer) => req = newer,
        Err(mpsc::RecvTimeoutError::Timeout) => break,
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    }

    #[cfg(test)]
    if let Some(hook) = config.worker_test_hook.as_ref() {
      // Best-effort; tests should tolerate disconnects.
      let _ = hook.entered_request_tx.send(());
      if hook.sleep > Duration::ZERO {
        std::thread::sleep(hook.sleep);
      }
    }

    let query_trimmed = req.query.trim();
    let suggestions = if query_trimmed.is_empty() {
      Vec::new()
    } else {
      fetch_duckduckgo_suggestions(&config.endpoint_base, query_trimmed, config.timeout_ms)
    };

    // Cancellation: drop late results if a newer request was issued while we were fetching.
    if req.gen != latest_gen.load(Ordering::Acquire) {
      continue;
    }

    let _ = update_tx.send(SearchSuggestUpdate {
      query: query_trimmed.to_string(),
      suggestions,
      fetched_at: SystemTime::now(),
    });
  }
}

#[cfg(not(feature = "direct_network"))]
fn fetch_duckduckgo_suggestions(_endpoint_base: &str, _query: &str, _timeout_ms: u64) -> Vec<String> {
  // Direct network access is disabled in this build. The omnibox can still function without remote
  // suggestions (local history/bookmark providers remain available).
  Vec::new()
}

#[cfg(feature = "direct_network")]
fn fetch_duckduckgo_suggestions(endpoint_base: &str, query: &str, timeout_ms: u64) -> Vec<String> {
  let timeout = Duration::from_millis(timeout_ms.max(1));

  let mut url = match url::Url::parse(endpoint_base) {
    Ok(url) => url,
    Err(_) => return Vec::new(),
  };
  {
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("q", query);
    pairs.append_pair("type", "list");
  }

  let client = match reqwest::blocking::Client::builder()
    .connect_timeout(timeout)
    .timeout(timeout)
    .user_agent(USER_AGENT)
    .build()
  {
    Ok(client) => client,
    Err(_) => return Vec::new(),
  };

  let res = match client.get(url).send() {
    Ok(res) => res,
    Err(_) => return Vec::new(),
  };

  let bytes = match res.bytes() {
    Ok(bytes) => bytes,
    Err(_) => return Vec::new(),
  };

  parse_duckduckgo_ac_json(&bytes)
}

#[derive(Debug, Deserialize)]
struct DuckDuckGoAcEntry {
  #[serde(default)]
  phrase: String,
}

fn parse_duckduckgo_ac_json(bytes: &[u8]) -> Vec<String> {
  let parsed: Vec<DuckDuckGoAcEntry> = match serde_json::from_slice(bytes) {
    Ok(parsed) => parsed,
    Err(_) => return Vec::new(),
  };

  let mut out = Vec::new();
  for entry in parsed {
    let phrase = entry.phrase.trim();
    if phrase.is_empty() {
      continue;
    }
    // Avoid unbounded growth if the endpoint misbehaves.
    if out.len() >= MAX_SUGGESTIONS {
      break;
    }
    // Best-effort de-dupe.
    if out.iter().any(|s| s == phrase) {
      continue;
    }
    out.push(phrase.to_string());
  }
  out
}

#[cfg(all(test, feature = "direct_network"))]
mod tests {
  use super::*;
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::time::Duration;

  use crate::testing as net;

  fn http_response(body: &str) -> Vec<u8> {
    let mut out = Vec::new();
    write!(
      &mut out,
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
      body.as_bytes().len(),
      body
    )
    .unwrap();
    out
  }

  fn spawn_server(
    listener: TcpListener,
    expected_requests: usize,
    handler: impl Fn(String) -> (Duration, String) + Send + 'static,
  ) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
      for stream in listener.incoming().take(expected_requests) {
        let mut stream = stream.expect("accept failed");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
          let n = stream.read(&mut tmp).expect("read failed");
          if n == 0 {
            break;
          }
          buf.extend_from_slice(&tmp[..n]);
          if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
          }
          // Avoid unbounded memory use on malformed clients.
          if buf.len() > 64 * 1024 {
            break;
          }
        }

        let req = String::from_utf8_lossy(&buf);
        let first_line = req.lines().next().unwrap_or_default();
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
        let q = url
          .query_pairs()
          .find(|(k, _)| k == "q")
          .map(|(_, v)| v.to_string())
          .unwrap_or_default();

        let (delay, body) = handler(q);
        if delay > Duration::ZERO {
          std::thread::sleep(delay);
        }
        let resp = http_response(&body);
        stream.write_all(&resp).expect("write failed");
      }
    })
  }

  fn poll_update(service: &SearchSuggestService, timeout: Duration) -> Option<SearchSuggestUpdate> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
      if let Some(update) = service.try_recv() {
        return Some(update);
      }
      std::thread::sleep(Duration::from_millis(5));
    }
    None
  }

  #[test]
  fn parses_duckduckgo_json_over_http() {
    let _lock = net::net_test_lock();
    let Some(listener) = net::try_bind_localhost("search suggest http test") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let join = spawn_server(listener, 1, |q| {
      assert_eq!(q, "rust");
      let body = r#"[{"phrase":"rust"},{"phrase":"rust lang"},{"phrase":"rust lang"}]"#;
      (Duration::ZERO, body.to_string())
    });

    let endpoint_base = format!("http://{}/ac/", addr);
    let mut service = SearchSuggestService::new(SearchSuggestConfig {
      endpoint_base,
      enabled: true,
      timeout_ms: 1000,
      worker_test_hook: None,
    });
    service.request("rust".to_string());

    let update = poll_update(&service, Duration::from_secs(2)).expect("expected update");
    assert_eq!(update.query, "rust");
    assert_eq!(update.suggestions, vec!["rust".to_string(), "rust lang".to_string()]);

    join.join().unwrap();
  }

  #[test]
  fn cancellation_drops_late_results() {
    let _lock = net::net_test_lock();
    let Some(listener) = net::try_bind_localhost("search suggest cancellation test") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let (saw_slow_tx, saw_slow_rx) = mpsc::channel::<()>();

    // First request ("slow") is delayed; second ("fast") should win.
    let join = spawn_server(listener, 2, move |q| {
      if q == "slow" {
        let _ = saw_slow_tx.send(());
        let body = r#"[{"phrase":"slow result"}]"#;
        (Duration::from_millis(400), body.to_string())
      } else if q == "fast" {
        let body = r#"[{"phrase":"fast result"}]"#;
        (Duration::ZERO, body.to_string())
      } else {
        panic!("unexpected query: {q:?}");
      }
    });

    let endpoint_base = format!("http://{}/ac/", addr);
    let mut service = SearchSuggestService::new(SearchSuggestConfig {
      endpoint_base,
      enabled: true,
      timeout_ms: 2000,
      worker_test_hook: None,
    });

    service.request("slow".to_string());
    // Wait until the server has observed the first request so we're testing the "late result"
    // cancellation path (rather than debounce collapsing the two keystrokes into one).
    saw_slow_rx
      .recv_timeout(Duration::from_secs(2))
      .expect("expected slow request to hit server");
    service.request("fast".to_string());

    // We should eventually see "fast", and never see "slow".
    let mut saw_slow = false;
    let mut saw_fast = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
      if let Some(update) = service.try_recv() {
        if update.query == "slow" {
          saw_slow = true;
        }
        if update.query == "fast" {
          saw_fast = true;
          assert_eq!(update.suggestions, vec!["fast result".to_string()]);
          break;
        }
      }
      std::thread::sleep(Duration::from_millis(5));
    }

    assert!(saw_fast, "expected to receive update for 'fast'");
    assert!(!saw_slow, "expected late 'slow' result to be dropped");

    join.join().unwrap();
  }

  #[test]
  fn redundant_requests_are_suppressed() {
    let _lock = net::net_test_lock();
    let Some(listener) = net::try_bind_localhost("search suggest redundant request test") else {
      return;
    };
    let addr = listener.local_addr().unwrap();

    let join = spawn_server(listener, 1, |q| {
      assert_eq!(q, "rust");
      let body = r#"[{"phrase":"rust"}]"#;
      (Duration::ZERO, body.to_string())
    });

    let endpoint_base = format!("http://{}/ac/", addr);
    let mut service = SearchSuggestService::new(SearchSuggestConfig {
      endpoint_base,
      enabled: true,
      timeout_ms: 200,
      worker_test_hook: None,
    });

    service.request("rust".to_string());
    let update = poll_update(&service, Duration::from_secs(2)).expect("expected initial update");
    assert_eq!(update.query, "rust");

    // The UI can redraw without input changes; the service should not re-enqueue identical queries.
    service.request("rust".to_string());
    let update = poll_update(&service, Duration::from_millis(700));
    assert!(update.is_none(), "expected redundant request to be suppressed");

    join.join().unwrap();
  }

  #[test]
  fn drop_does_not_block_on_slow_worker() {
    // Simulate a worker that is "stuck" doing something slow (e.g. a hung network request), and
    // ensure dropping the service does not block indefinitely.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();

    let mut service = SearchSuggestService::new(SearchSuggestConfig {
      endpoint_base: "not a url".to_string(),
      enabled: true,
      timeout_ms: 1000,
      worker_test_hook: Some(SearchSuggestWorkerTestHook {
        entered_request_tx: entered_tx,
        sleep: Duration::from_secs(1),
      }),
    });

    service.request("rust".to_string());
    entered_rx
      .recv_timeout(Duration::from_secs(2))
      .expect("expected worker to start handling request");

    let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
      drop(service);
      let _ = dropped_tx.send(());
    });

    dropped_rx
      .recv_timeout(Duration::from_millis(800))
      .expect("expected SearchSuggestService::drop to return promptly");
  }
}
