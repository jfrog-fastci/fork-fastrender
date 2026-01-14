use super::messages::{UiToWorker, WorkerToUi, WorkerToUiInbox};
use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Abstraction over how the browser UI talks to its renderer/worker implementation.
///
/// Today the browser UI runs a renderer worker in-process on a dedicated thread (see
/// [`ThreadRendererBackend`]). Multiprocess security work will require swapping this out for a
/// per-tab/per-site renderer *process* later; keeping the UI side behind this trait means the chrome
/// and input handling code does not need to be rewritten.
pub trait RendererBackend: Send + Sync {
  /// Send a UI→worker protocol message.
  ///
  /// Returns a send error when the worker is no longer receiving (disconnected/crashed/shutdown).
  fn send(&self, msg: UiToWorker) -> Result<(), SendError<UiToWorker>>;

  /// Blocking receive for worker→UI protocol messages.
  fn recv(&self) -> Result<WorkerToUi, RecvError>;

  /// Non-blocking receive for worker→UI protocol messages.
  fn try_recv(&self) -> Result<WorkerToUi, TryRecvError>;

  /// Blocking receive with a timeout.
  fn recv_timeout(&self, timeout: Duration) -> Result<WorkerToUi, RecvTimeoutError>;

  /// Best-effort liveness check.
  ///
  /// Backends should treat this as a hint only (e.g. a process may still be alive even if we failed
  /// to query its status).
  fn is_alive(&self) -> bool;

  /// Signal shutdown (best-effort).
  ///
  /// For channel-backed implementations this typically drops the UI→worker sender so the worker can
  /// observe disconnection and exit.
  fn shutdown(&self);

  /// Join/wait for the backend to exit.
  ///
  /// Should be idempotent; joining twice should succeed.
  fn join(&self) -> std::thread::Result<()>;

  /// Attempt to extract a raw `JoinHandle` for this backend, if available.
  ///
  /// This is used by the windowed browser UI to join workers without blocking the UI thread, while
  /// allowing the backend (and its worker→UI receiver) to be dropped promptly on window close.
  ///
  /// Default implementation returns `None`.
  fn take_join_handle(&self) -> Option<JoinHandle<()>> {
    None
  }
}

/// Shared renderer backend handle type used by the windowed browser and headless smoke mode.
pub type RendererBackendHandle = Arc<dyn RendererBackend>;

/// Renderer backend backed by an in-process worker thread and `std::sync::mpsc` channels.
pub struct ThreadRendererBackend {
  tx: Mutex<Option<Sender<UiToWorker>>>,
  rx: Mutex<WorkerToUiInbox>,
  join: Mutex<Option<JoinHandle<()>>>,
}

impl ThreadRendererBackend {
  fn new(tx: Sender<UiToWorker>, rx: WorkerToUiInbox, join: JoinHandle<()>) -> Self {
    Self {
      tx: Mutex::new(Some(tx)),
      rx: Mutex::new(rx),
      join: Mutex::new(Some(join)),
    }
  }

  /// Create a [`ThreadRendererBackend`] from a spawned [`super::render_worker::BrowserWorkerHandle`].
  pub fn from_browser_worker_handle(handle: super::render_worker::BrowserWorkerHandle) -> Self {
    Self::new(handle.tx, handle.rx, handle.join)
  }

  /// Spawn the production browser UI worker and wrap it in a [`ThreadRendererBackend`].
  pub fn spawn_browser_ui_worker(
    name: impl Into<String>,
  ) -> std::io::Result<RendererBackendHandle> {
    let (tx, rx, join) = super::render_worker::spawn_browser_ui_worker(name)?;
    Ok(Arc::new(Self::new(tx, WorkerToUiInbox::new(rx), join)))
  }

  /// Spawn the browser worker thread with an explicit name and wrap it in a [`ThreadRendererBackend`].
  pub fn spawn_browser_worker_with_name(
    name: impl Into<String>,
  ) -> crate::Result<RendererBackendHandle> {
    let handle = super::render_worker::spawn_browser_worker_with_name(name)?;
    Ok(Arc::new(Self::from_browser_worker_handle(handle)))
  }
}

impl RendererBackend for ThreadRendererBackend {
  fn send(&self, msg: UiToWorker) -> Result<(), SendError<UiToWorker>> {
    let tx = self
      .tx
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .as_ref()
      .cloned();
    match tx {
      Some(tx) => tx.send(msg),
      None => Err(SendError(msg)),
    }
  }

  fn recv(&self) -> Result<WorkerToUi, RecvError> {
    self.rx.lock().unwrap_or_else(|err| err.into_inner()).recv()
  }

  fn try_recv(&self) -> Result<WorkerToUi, TryRecvError> {
    self
      .rx
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .try_recv()
  }

  fn recv_timeout(&self, timeout: Duration) -> Result<WorkerToUi, RecvTimeoutError> {
    self
      .rx
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .recv_timeout(timeout)
  }

  fn is_alive(&self) -> bool {
    self
      .join
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .as_ref()
      .is_some_and(|join| !join.is_finished())
  }

  fn shutdown(&self) {
    let _ = self.tx.lock().unwrap_or_else(|err| err.into_inner()).take();
  }

  fn take_join_handle(&self) -> Option<JoinHandle<()>> {
    // Ensure the worker loop can observe channel closure before we detach/join.
    self.shutdown();
    self
      .join
      .lock()
      .unwrap_or_else(|err| err.into_inner())
      .take()
  }

  fn join(&self) -> std::thread::Result<()> {
    // Ensure the worker loop can observe channel closure before we block on joining.
    let join = self.take_join_handle();
    match join {
      Some(join) => join.join(),
      None => Ok(()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{RendererBackend, RendererBackendHandle, ThreadRendererBackend};
  use crate::ui::messages::WorkerToUiMsg;
  use std::sync::mpsc;
  use std::sync::Arc;

  #[test]
  fn take_join_handle_is_idempotent_and_works_through_trait_object() {
    let (ui_tx, _ui_rx) = mpsc::channel();
    let (_worker_tx, worker_rx) = mpsc::channel::<WorkerToUiMsg>();
    let inbox = super::WorkerToUiInbox::new(worker_rx);
    let join = std::thread::spawn(|| {});

    let backend: RendererBackendHandle = Arc::new(ThreadRendererBackend::new(ui_tx, inbox, join));

    let join = backend.take_join_handle();
    assert!(join.is_some(), "expected a JoinHandle to be extracted");

    // Taking twice should be harmless.
    assert!(backend.take_join_handle().is_none());

    // Join the extracted handle to avoid leaving threads behind in the test harness.
    join
      .unwrap()
      .join()
      .expect("expected worker thread to join");

    // Joining after the handle is taken should be a no-op.
    backend.join().expect("expected join to be idempotent");
  }
}
