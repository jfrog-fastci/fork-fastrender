#![cfg(feature = "browser_ui")]

use fastrender::scroll::ScrollState;
use fastrender::tree::box_tree::SelectControl;
use fastrender::ui::messages::{RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_runtime::spawn_browser_worker_runtime_thread;
use fastrender::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI), and the worker
// runtime performs real parsing/layout/paint work. Use a generous timeout to avoid flakes under CPU
// contention.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerToUiEvent {
  Stage { tab_id: TabId },
  FrameReady {
    tab_id: TabId,
    viewport_css: (u32, u32),
    dpr: f32,
    scroll_state: ScrollState,
  },
  OpenSelectDropdown {
    tab_id: TabId,
    select_node_id: usize,
    control: SelectControl,
  },
  NavigationStarted { tab_id: TabId, url: String },
  NavigationCommitted {
    tab_id: TabId,
    url: String,
    can_go_back: bool,
    can_go_forward: bool,
  },
  NavigationFailed { tab_id: TabId, url: String, error: String },
  ScrollStateUpdated { tab_id: TabId, scroll: ScrollState },
  LoadingState { tab_id: TabId, loading: bool },
  DebugLog { tab_id: TabId, line: String },
  SelectDropdownClosed { tab_id: TabId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEventKind {
  Stage,
  FrameReady,
  OpenSelectDropdown,
  NavigationStarted,
  NavigationCommitted,
  NavigationFailed,
  ScrollStateUpdated,
  LoadingState(bool),
  DebugLog,
  SelectDropdownClosed,
}

impl WorkerToUiEvent {
  pub fn kind(&self) -> WorkerEventKind {
    match self {
      WorkerToUiEvent::Stage { .. } => WorkerEventKind::Stage,
      WorkerToUiEvent::FrameReady { .. } => WorkerEventKind::FrameReady,
      WorkerToUiEvent::OpenSelectDropdown { .. } => WorkerEventKind::OpenSelectDropdown,
      WorkerToUiEvent::NavigationStarted { .. } => WorkerEventKind::NavigationStarted,
      WorkerToUiEvent::NavigationCommitted { .. } => WorkerEventKind::NavigationCommitted,
      WorkerToUiEvent::NavigationFailed { .. } => WorkerEventKind::NavigationFailed,
      WorkerToUiEvent::ScrollStateUpdated { .. } => WorkerEventKind::ScrollStateUpdated,
      WorkerToUiEvent::LoadingState { loading, .. } => WorkerEventKind::LoadingState(*loading),
      WorkerToUiEvent::DebugLog { .. } => WorkerEventKind::DebugLog,
      WorkerToUiEvent::SelectDropdownClosed { .. } => WorkerEventKind::SelectDropdownClosed,
    }
  }
}

fn split_message(msg: WorkerToUi) -> (WorkerToUiEvent, Option<RenderedFrame>) {
  match msg {
    WorkerToUi::Stage { tab_id, stage: _ } => (WorkerToUiEvent::Stage { tab_id }, None),
    WorkerToUi::FrameReady { tab_id, frame } => {
      let event = WorkerToUiEvent::FrameReady {
        tab_id,
        viewport_css: frame.viewport_css,
        dpr: frame.dpr,
        scroll_state: frame.scroll_state.clone(),
      };
      (event, Some(frame))
    }
    WorkerToUi::OpenSelectDropdown {
      tab_id,
      select_node_id,
      control,
    } => (
      WorkerToUiEvent::OpenSelectDropdown {
        tab_id,
        select_node_id,
        control,
      },
      None,
    ),
    WorkerToUi::SelectDropdownOpened {
      tab_id,
      select_node_id,
      control,
      anchor_css: _,
    } => (
      WorkerToUiEvent::OpenSelectDropdown {
        tab_id,
        select_node_id,
        control,
      },
      None,
    ),
    WorkerToUi::NavigationStarted { tab_id, url } => (WorkerToUiEvent::NavigationStarted { tab_id, url }, None),
    WorkerToUi::NavigationCommitted {
      tab_id,
      url,
      title: _,
      can_go_back,
      can_go_forward,
    } => (
      WorkerToUiEvent::NavigationCommitted {
        tab_id,
        url,
        can_go_back,
        can_go_forward,
      },
      None,
    ),
    WorkerToUi::NavigationFailed { tab_id, url, error, .. } => (
      WorkerToUiEvent::NavigationFailed { tab_id, url, error },
      None,
    ),
    WorkerToUi::ScrollStateUpdated { tab_id, scroll } => (
      WorkerToUiEvent::ScrollStateUpdated { tab_id, scroll },
      None,
    ),
    WorkerToUi::LoadingState { tab_id, loading } => (
      WorkerToUiEvent::LoadingState { tab_id, loading },
      None,
    ),
    WorkerToUi::DebugLog { tab_id, line } => (WorkerToUiEvent::DebugLog { tab_id, line }, None),
    WorkerToUi::SelectDropdownClosed { tab_id } => {
      (WorkerToUiEvent::SelectDropdownClosed { tab_id }, None)
    }
  }
}

pub fn assert_event_subsequence(events: &[WorkerToUiEvent], expected: &[WorkerEventKind]) {
  let mut next = 0usize;
  for ev in events {
    if next < expected.len() && ev.kind() == expected[next] {
      next += 1;
      if next == expected.len() {
        break;
      }
    }
  }
  assert_eq!(
    next,
    expected.len(),
    "expected event subsequence {:?} in {:?}",
    expected,
    events.iter().map(WorkerToUiEvent::kind).collect::<Vec<_>>()
  );
}

pub struct WorkerHarness {
  ui_tx: Option<Sender<UiToWorker>>,
  ui_rx: Receiver<WorkerToUi>,
  handle: Option<JoinHandle<()>>,
  stages: Arc<Mutex<Vec<StageHeartbeat>>>,
  _stage_guard: Option<GlobalStageListenerGuard>,
}

impl WorkerHarness {
  pub fn spawn() -> Self {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    let stage_guard = GlobalStageListenerGuard::new(Arc::new(move |stage| {
      // Only used for debugging failing/hanging integration tests; ignore poison.
      if let Ok(mut guard) = stages_for_listener.lock() {
        guard.push(stage);
      }
    }));
    let (ui_tx, worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
    let (worker_tx, ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

    let handle = spawn_browser_worker_runtime_thread(
      "fastr-browser-worker-runtime-test",
      worker_rx,
      worker_tx,
    )
    .expect("spawn browser worker runtime thread");

    Self {
      ui_tx: Some(ui_tx),
      ui_rx,
      handle: Some(handle),
      stages,
      _stage_guard: Some(stage_guard),
    }
  }

  pub fn send(&self, msg: UiToWorker) {
    self
      .ui_tx
      .as_ref()
      .expect("worker harness tx available")
      .send(msg)
      .expect("send UiToWorker");
  }

  pub fn drain_events(&self, timeout: Duration) -> Vec<WorkerToUiEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
      let now = Instant::now();
      if now >= deadline {
        break;
      }
      let remaining = deadline.saturating_duration_since(now);
      match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => {
          let (event, _frame) = split_message(msg);
          events.push(event);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }
    events
  }

  pub fn wait_for_event(
    &self,
    timeout: Duration,
    mut pred: impl FnMut(&WorkerToUiEvent) -> bool,
  ) -> Vec<WorkerToUiEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
      let now = Instant::now();
      if now >= deadline {
        let stages = self.stages.lock().ok();
        panic!("timed out waiting for worker event; stages={stages:?}; got {events:?}");
      }
      let remaining = deadline.saturating_duration_since(now);
      let msg = match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => msg,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
          panic!("worker channel disconnected while waiting for event; got {events:?}");
        }
      };
      let (event, _frame) = split_message(msg);
      let done = pred(&event);
      events.push(event);
      if done {
        return events;
      }
    }
  }

  pub fn wait_for_frame(
    &self,
    tab_id: TabId,
    timeout: Duration,
  ) -> (RenderedFrame, Vec<WorkerToUiEvent>) {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
      let now = Instant::now();
      if now >= deadline {
        let stages = self.stages.lock().ok();
        panic!("timed out waiting for FrameReady; stages={stages:?}; got {events:?}");
      }
      let remaining = deadline.saturating_duration_since(now);
      let msg = match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => msg,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
          panic!("worker channel disconnected while waiting for FrameReady; got {events:?}");
        }
      };
      let (event, frame) = split_message(msg);
      events.push(event.clone());
      if let (WorkerToUiEvent::FrameReady { tab_id: msg_tab, .. }, Some(frame)) = (event, frame) {
        if msg_tab == tab_id {
          return (frame, events);
        }
      }
    }
  }

  pub fn send_and_wait_for_frame(
    &self,
    tab_id: TabId,
    msg: UiToWorker,
  ) -> (RenderedFrame, Vec<WorkerToUiEvent>) {
    self.send(msg);
    self.wait_for_frame(tab_id, DEFAULT_TIMEOUT)
  }

  pub fn drain_default(&self) -> Vec<WorkerToUiEvent> {
    self.drain_events(Duration::from_millis(50))
  }
}

impl Drop for WorkerHarness {
  fn drop(&mut self) {
    // Close the channel first so the runtime exits its recv loop.
    self.ui_tx.take();
    if let Some(handle) = self.handle.take() {
      // Avoid hanging the entire test suite if the worker is stuck (e.g. during a long render or a
      // panic while a message is in-flight). Prefer clean teardown, but detach after a short grace
      // period so we still get useful failure output.
      const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
      let deadline = Instant::now() + JOIN_TIMEOUT;
      let handle = handle;
      while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
      }
      if handle.is_finished() {
        let _ = handle.join();
      }
    }
  }
}
