use crate::error::{Error, ResourceError, Result};
use crate::js::ScriptLoader;
use crate::resource::ResourceFetcher;
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;

const DEFAULT_MAX_WORKERS: usize = 8;

#[derive(Debug)]
struct WorkerState {
  queue: VecDeque<(u64, String)>,
  shutdown: bool,
}

/// A [`ScriptLoader`] implementation backed by FastRender's [`ResourceFetcher`].
///
/// This is a small async adapter: external scripts can be fetched in parallel on a bounded pool of
/// worker threads while the HTML parser continues (for `async` / `defer`).
pub struct ResourceScriptLoader<F: ResourceFetcher + 'static> {
  fetcher: Arc<F>,

  next_handle: u64,

  state: Arc<(Mutex<WorkerState>, Condvar)>,

  completion_rx: mpsc::Receiver<(u64, Result<String>)>,

  workers: Vec<JoinHandle<()>>,
}

impl<F: ResourceFetcher + 'static> ResourceScriptLoader<F> {
  pub fn new(fetcher: F) -> Self {
    let max_workers = std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(1)
      .min(DEFAULT_MAX_WORKERS)
      .max(1);
    Self::with_max_workers(fetcher, max_workers)
  }

  pub fn with_max_workers(fetcher: F, max_workers: usize) -> Self {
    let max_workers = max_workers.max(1).min(DEFAULT_MAX_WORKERS);

    let fetcher = Arc::new(fetcher);

    let state = Arc::new((
      Mutex::new(WorkerState {
        queue: VecDeque::new(),
        shutdown: false,
      }),
      Condvar::new(),
    ));

    let (completion_tx, completion_rx) = mpsc::channel::<(u64, Result<String>)>();

    let mut workers = Vec::with_capacity(max_workers);
    for _ in 0..max_workers {
      let fetcher = Arc::clone(&fetcher);
      let state = Arc::clone(&state);
      let completion_tx = completion_tx.clone();
      workers.push(std::thread::spawn(move || worker_loop(fetcher, state, completion_tx)));
    }

    Self {
      fetcher,
      next_handle: 1,
      state,
      completion_rx,
      workers,
    }
  }

  fn fetch_and_decode(fetcher: &F, url: &str) -> Result<String> {
    let res = fetcher.fetch(url)?;
    String::from_utf8(res.bytes).map_err(|source| {
      Error::Resource(
        ResourceError::new(url, "script response was not valid UTF-8").with_source(source),
      )
    })
  }
}

fn worker_loop<F: ResourceFetcher + 'static>(
  fetcher: Arc<F>,
  state: Arc<(Mutex<WorkerState>, Condvar)>,
  completion_tx: mpsc::Sender<(u64, Result<String>)>,
) {
  loop {
    let (handle, url) = {
      let (lock, cv) = &*state;
      let mut st = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      while st.queue.is_empty() && !st.shutdown {
        st = cv.wait(st).unwrap_or_else(|poisoned| poisoned.into_inner());
      }
      if st.shutdown && st.queue.is_empty() {
        return;
      }
      let Some(entry) = st.queue.pop_front() else {
        continue;
      };
      entry
    };

    let result = ResourceScriptLoader::<F>::fetch_and_decode(&fetcher, &url);
    let _ = completion_tx.send((handle, result));
  }
}

impl<F: ResourceFetcher + 'static> ScriptLoader for ResourceScriptLoader<F> {
  type Handle = u64;

  fn load_blocking(&mut self, url: &str) -> Result<String> {
    Self::fetch_and_decode(&self.fetcher, url)
  }

  fn start_load(&mut self, url: &str) -> Result<Self::Handle> {
    let handle = self.next_handle;
    self.next_handle += 1;

    let (lock, cv) = &*self.state;
    let mut st = lock.lock().map_err(|_| Error::Other("worker state mutex poisoned".to_string()))?;
    if st.shutdown {
      return Err(Error::Other(
        "start_load called after ResourceScriptLoader shutdown".to_string(),
      ));
    }
    st.queue.push_back((handle, url.to_string()));
    cv.notify_one();

    Ok(handle)
  }

  fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
    match self.completion_rx.try_recv() {
      Ok((handle, res)) => match res {
        Ok(source) => Ok(Some((handle, source))),
        Err(err) => Err(err),
      },
      Err(mpsc::TryRecvError::Empty) => Ok(None),
      Err(mpsc::TryRecvError::Disconnected) => Err(Error::Other(
        "script loader completion channel disconnected".to_string(),
      )),
    }
  }
}

impl<F: ResourceFetcher + 'static> Drop for ResourceScriptLoader<F> {
  fn drop(&mut self) {
    let (lock, cv) = &*self.state;
    if let Ok(mut st) = lock.lock() {
      st.shutdown = true;
      cv.notify_all();
    }

    for worker in self.workers.drain(..) {
      let _ = worker.join();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::FetchedResource;
  use std::collections::HashMap;
  use std::sync::mpsc;
  use std::sync::{Arc, Mutex};

  #[derive(Default)]
  struct MapFetcher {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    waits: Mutex<HashMap<String, mpsc::Receiver<()>>>,
    started: Mutex<HashMap<String, mpsc::Sender<()>>>,
  }

  impl MapFetcher {
    fn insert(&self, url: &str, bytes: Vec<u8>) {
      self
        .entries
        .lock()
        .unwrap()
        .insert(url.to_string(), bytes);
    }

    fn gate(&self, url: &str) -> mpsc::Sender<()> {
      let (tx, rx) = mpsc::channel();
      self
        .waits
        .lock()
        .unwrap()
        .insert(url.to_string(), rx);
      tx
    }

    fn notify_on_start(&self, url: &str) -> mpsc::Receiver<()> {
      let (tx, rx) = mpsc::channel();
      self
        .started
        .lock()
        .unwrap()
        .insert(url.to_string(), tx);
      rx
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      let (bytes, wait, started) = {
        let bytes = self
          .entries
          .lock()
          .unwrap()
          .get(url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("no entry for url={url}")))?;

        let wait = self.waits.lock().unwrap().remove(url);
        let started = self.started.lock().unwrap().get(url).cloned();
        (bytes, wait, started)
      };

      if let Some(started) = started {
        let _ = started.send(());
      }

      if let Some(wait) = wait {
        // Wait for the test to release this request.
        let _ = wait.recv();
      }

      Ok(FetchedResource {
        bytes,
        content_type: None,
        nosniff: false,
        content_encoding: None,
        status: None,
        etag: None,
        last_modified: None,
        access_control_allow_origin: None,
        timing_allow_origin: None,
        vary: None,
        response_referrer_policy: None,
        access_control_allow_credentials: false,
        final_url: None,
        cache_policy: None,
        response_headers: None,
      })
    }
  }

  #[test]
  fn async_loads_complete_out_of_order() -> Result<()> {
    let fetcher = MapFetcher::default();
    fetcher.insert("a.js", b"a".to_vec());
    fetcher.insert("b.js", b"b".to_vec());

    // Gate a.js so b.js can complete first.
    let release_a = fetcher.gate("a.js");

    let mut loader = ResourceScriptLoader::with_max_workers(fetcher, 2);

    let h1 = loader.start_load("a.js")?;
    let h2 = loader.start_load("b.js")?;

    // b.js should complete first.
    let (got_handle, got_source) = loop {
      if let Some((h, s)) = loader.poll_complete()? {
        break (h, s);
      }
      std::thread::yield_now();
    };
    assert_eq!(got_handle, h2);
    assert_eq!(got_source, "b");

    // Now allow a.js to complete.
    release_a.send(()).unwrap();

    let (got_handle, got_source) = loop {
      if let Some((h, s)) = loader.poll_complete()? {
        break (h, s);
      }
      std::thread::yield_now();
    };
    assert_eq!(got_handle, h1);
    assert_eq!(got_source, "a");

    Ok(())
  }

  #[test]
  fn invalid_utf8_results_in_error() -> Result<()> {
    let fetcher = MapFetcher::default();
    fetcher.insert("bad.js", vec![0xFF, 0xFE, 0xFD]);
    let mut loader = ResourceScriptLoader::with_max_workers(fetcher, 1);

    let _handle = loader.start_load("bad.js")?;

    let err = loop {
      match loader.poll_complete() {
        Ok(Some(_)) => panic!("expected error completion"),
        Ok(None) => {
          std::thread::yield_now();
          continue;
        }
        Err(e) => break e,
      }
    };

    let msg = err.to_string();
    assert!(msg.contains("not valid UTF-8"), "msg={msg}");
    Ok(())
  }

  #[test]
  fn drop_joins_worker_threads() -> Result<()> {
    let fetcher = Arc::new(MapFetcher::default());
    fetcher.insert("blocked.js", b"ok".to_vec());

    let started_rx = fetcher.notify_on_start("blocked.js");
    let release = fetcher.gate("blocked.js");

    let mut loader = ResourceScriptLoader::with_max_workers(ArcFetcher(fetcher.clone()), 1);
    let _handle = loader.start_load("blocked.js")?;

    // Ensure the worker thread has started fetching and is now blocked.
    started_rx.recv().unwrap();

    let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
    let drop_thread = std::thread::spawn(move || {
      drop(loader);
      let _ = dropped_tx.send(());
    });

    // The drop thread should be blocked until we release the fetch.
    assert!(matches!(dropped_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

    release.send(()).unwrap();

    dropped_rx.recv().unwrap();
    drop_thread.join().unwrap();
    Ok(())
  }

  /// Wrapper so we can keep an `Arc<MapFetcher>` outside the loader (for synchronization).
  struct ArcFetcher(Arc<MapFetcher>);

  impl ResourceFetcher for ArcFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self.0.fetch(url)
    }
  }
}
