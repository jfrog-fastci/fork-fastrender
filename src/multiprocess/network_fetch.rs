//! Browser ↔ Network fetch IPC primitives.
//!
//! This module is an incremental step toward a multiprocess architecture where the browser process
//! owns network access and untrusted renderers must go through an IPC boundary.
//!
//! For now, the "IPC" is implemented with in-process channels and a dedicated network service
//! thread. The message protocol is intentionally serializable so it can later be transported over
//! a real process boundary.

use crate::resource::{HttpFetcher, ResourceFetcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub type RequestId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BrowserToNetwork {
  Fetch { id: RequestId, url: String },
  Cancel { id: RequestId },
  Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchResponse {
  pub status: u16,
  pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkToBrowser {
  FetchOk { id: RequestId, response: FetchResponse },
  FetchErr { id: RequestId, error: String },
  FetchCancelled { id: RequestId },
}

#[derive(Debug, Clone)]
struct RequestEntry {
  nonce: u64,
  cancelled: bool,
}

#[derive(Debug)]
enum ServiceMsg {
  Browser(BrowserToNetwork),
  Completed {
    id: RequestId,
    nonce: u64,
    result: Result<FetchResponse, String>,
  },
}

pub struct NetworkClient {
  tx: mpsc::Sender<ServiceMsg>,
  rx: mpsc::Receiver<NetworkToBrowser>,
  next_id: AtomicU64,
}

impl NetworkClient {
  pub fn fetch(&self, url: impl Into<String>) -> RequestId {
    let id = self.alloc_id();
    let _ = self.tx.send(ServiceMsg::Browser(BrowserToNetwork::Fetch {
      id,
      url: url.into(),
    }));
    id
  }

  pub fn cancel(&self, id: RequestId) {
    let _ = self
      .tx
      .send(ServiceMsg::Browser(BrowserToNetwork::Cancel { id }));
  }

  pub fn recv_timeout(&self, timeout: Duration) -> Option<NetworkToBrowser> {
    self.rx.recv_timeout(timeout).ok()
  }

  fn alloc_id(&self) -> RequestId {
    // Mirror `TabId::new`: reserve 0 as invalid.
    loop {
      let id = self.next_id.fetch_add(1, Ordering::Relaxed);
      if id != 0 {
        return id;
      }
    }
  }
}

pub struct NetworkService {
  tx: mpsc::Sender<ServiceMsg>,
  join: Option<thread::JoinHandle<()>>,
}

impl NetworkService {
  pub fn spawn() -> (NetworkClient, Self) {
    let (tx, rx) = mpsc::channel::<ServiceMsg>();
    let (browser_tx, browser_rx) = mpsc::channel::<NetworkToBrowser>();

    let join = thread::Builder::new()
      .name("fastr-network-service".to_string())
      .spawn({
        let tx = tx.clone();
        move || service_main(rx, browser_tx, tx)
      })
      .expect("spawn network service"); // fastrender-allow-unwrap

    let client = NetworkClient {
      tx: tx.clone(),
      rx: browser_rx,
      next_id: AtomicU64::new(1),
    };

    let svc = Self {
      tx,
      join: Some(join),
    };

    (client, svc)
  }

  pub fn shutdown(mut self) {
    self.shutdown_inner();
  }

  fn shutdown_inner(&mut self) {
    let _ = self.tx.send(ServiceMsg::Browser(BrowserToNetwork::Shutdown));
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

impl Drop for NetworkService {
  fn drop(&mut self) {
    self.shutdown_inner();
  }
}

fn service_main(
  rx: mpsc::Receiver<ServiceMsg>,
  browser_tx: mpsc::Sender<NetworkToBrowser>,
  service_tx: mpsc::Sender<ServiceMsg>,
) {
  let fetcher = HttpFetcher::new();

  let mut in_flight: HashMap<RequestId, RequestEntry> = HashMap::new();
  let mut next_nonce: u64 = 1;

  for msg in rx {
    match msg {
      ServiceMsg::Browser(BrowserToNetwork::Fetch { id, url }) => {
        let nonce = next_nonce;
        next_nonce = next_nonce.saturating_add(1);

        // Track the request before spawning work so cancellation can race safely.
        in_flight.insert(
          id,
          RequestEntry {
            nonce,
            cancelled: false,
          },
        );

        let tx = service_tx.clone();
        let fetcher = fetcher.clone();
        thread::spawn(move || {
          let result = fetch_url(&fetcher, &url);
          let _ = tx.send(ServiceMsg::Completed { id, nonce, result });
        });
      }
      ServiceMsg::Browser(BrowserToNetwork::Cancel { id }) => {
        if let Some(mut entry) = in_flight.remove(&id) {
          entry.cancelled = true;
          let _ = browser_tx.send(NetworkToBrowser::FetchCancelled { id });
          // Drop `entry` so future completions for this (id, nonce) are ignored.
        }
      }
      ServiceMsg::Browser(BrowserToNetwork::Shutdown) => {
        break;
      }
      ServiceMsg::Completed { id, nonce, result } => {
        let Some(entry) = in_flight.get(&id) else {
          // Request was cancelled (or never existed). Suppress stale response.
          continue;
        };

        // If the browser reused an id, ensure only the latest request can deliver a result.
        if entry.nonce != nonce {
          continue;
        }

        if entry.cancelled {
          // Cancellation was recorded; suppress completion.
          in_flight.remove(&id);
          continue;
        }

        // Happy path: deliver and clear bookkeeping.
        in_flight.remove(&id);
        let msg = match result {
          Ok(response) => NetworkToBrowser::FetchOk { id, response },
          Err(error) => NetworkToBrowser::FetchErr { id, error },
        };
        let _ = browser_tx.send(msg);
      }
    }
  }
}

fn fetch_url(fetcher: &HttpFetcher, url: &str) -> Result<FetchResponse, String> {
  // Prefer the existing `ResourceFetcher` plumbing so cookies / redirects / TLS config match the
  // rest of the codebase. Convert the response into a minimal IPC payload.
  let resource = fetcher
    .fetch(url)
    .map_err(|err| format!("fetch failed for {url}: {err}"))?;
  Ok(FetchResponse {
    status: resource.status.unwrap_or(200),
    body: resource.bytes,
  })
}
