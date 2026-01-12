#![cfg(feature = "browser_ui")]

use fastrender::render_control::StageHeartbeat;
use fastrender::scroll::ScrollState;
use fastrender::tree::box_tree::SelectControl;
use fastrender::ui::messages::{CursorKind, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI), and the worker
// runtime performs real parsing/layout/paint work. Use a generous timeout to avoid flakes under CPU
// contention.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerToUiEvent {
  Stage {
    tab_id: TabId,
    stage: StageHeartbeat,
  },
  Favicon {
    tab_id: TabId,
    width: u32,
    height: u32,
  },
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
  RequestOpenInNewTab {
    tab_id: TabId,
    url: String,
  },
  NavigationStarted {
    tab_id: TabId,
    url: String,
  },
  NavigationCommitted {
    tab_id: TabId,
    url: String,
    can_go_back: bool,
    can_go_forward: bool,
  },
  NavigationFailed {
    tab_id: TabId,
    url: String,
    error: String,
  },
  ScrollStateUpdated {
    tab_id: TabId,
    scroll: ScrollState,
  },
  LoadingState {
    tab_id: TabId,
    loading: bool,
  },
  Warning {
    tab_id: TabId,
    text: String,
  },
  SetClipboardText {
    tab_id: TabId,
    text: String,
  },
  DebugLog {
    tab_id: TabId,
    line: String,
  },
  SelectDropdownClosed {
    tab_id: TabId,
  },
  ContextMenu {
    tab_id: TabId,
    pos_css: (f32, f32),
    link_url: Option<String>,
  },
  HoverChanged {
    tab_id: TabId,
    hovered_url: Option<String>,
    cursor: CursorKind,
  },
  FindResult {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
    match_count: usize,
    active_match_index: Option<usize>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEventKind {
  Stage,
  Favicon,
  FrameReady,
  OpenSelectDropdown,
  RequestOpenInNewTab,
  NavigationStarted,
  NavigationCommitted,
  NavigationFailed,
  ScrollStateUpdated,
  LoadingState(bool),
  Warning,
  SetClipboardText,
  DebugLog,
  SelectDropdownClosed,
  ContextMenu,
  HoverChanged,
  FindResult,
}

impl WorkerToUiEvent {
  pub fn kind(&self) -> WorkerEventKind {
    match self {
      WorkerToUiEvent::Stage { .. } => WorkerEventKind::Stage,
      WorkerToUiEvent::Favicon { .. } => WorkerEventKind::Favicon,
      WorkerToUiEvent::FrameReady { .. } => WorkerEventKind::FrameReady,
      WorkerToUiEvent::OpenSelectDropdown { .. } => WorkerEventKind::OpenSelectDropdown,
      WorkerToUiEvent::RequestOpenInNewTab { .. } => WorkerEventKind::RequestOpenInNewTab,
      WorkerToUiEvent::NavigationStarted { .. } => WorkerEventKind::NavigationStarted,
      WorkerToUiEvent::NavigationCommitted { .. } => WorkerEventKind::NavigationCommitted,
      WorkerToUiEvent::NavigationFailed { .. } => WorkerEventKind::NavigationFailed,
      WorkerToUiEvent::ScrollStateUpdated { .. } => WorkerEventKind::ScrollStateUpdated,
      WorkerToUiEvent::LoadingState { loading, .. } => WorkerEventKind::LoadingState(*loading),
      WorkerToUiEvent::Warning { .. } => WorkerEventKind::Warning,
      WorkerToUiEvent::SetClipboardText { .. } => WorkerEventKind::SetClipboardText,
      WorkerToUiEvent::DebugLog { .. } => WorkerEventKind::DebugLog,
      WorkerToUiEvent::SelectDropdownClosed { .. } => WorkerEventKind::SelectDropdownClosed,
      WorkerToUiEvent::ContextMenu { .. } => WorkerEventKind::ContextMenu,
      WorkerToUiEvent::HoverChanged { .. } => WorkerEventKind::HoverChanged,
      WorkerToUiEvent::FindResult { .. } => WorkerEventKind::FindResult,
    }
  }
}

fn split_message(msg: WorkerToUi) -> (WorkerToUiEvent, Option<RenderedFrame>) {
  match msg {
    WorkerToUi::Stage { tab_id, stage } => (WorkerToUiEvent::Stage { tab_id, stage }, None),
    WorkerToUi::Favicon {
      tab_id,
      rgba: _,
      width,
      height,
    } => (
      WorkerToUiEvent::Favicon {
        tab_id,
        width,
        height,
      },
      None,
    ),
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
    WorkerToUi::RequestOpenInNewTab { tab_id, url } => {
      (WorkerToUiEvent::RequestOpenInNewTab { tab_id, url }, None)
    }
    WorkerToUi::NavigationStarted { tab_id, url } => {
      (WorkerToUiEvent::NavigationStarted { tab_id, url }, None)
    }
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
    WorkerToUi::NavigationFailed {
      tab_id, url, error, ..
    } => (
      WorkerToUiEvent::NavigationFailed { tab_id, url, error },
      None,
    ),
    WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
      (WorkerToUiEvent::ScrollStateUpdated { tab_id, scroll }, None)
    }
    WorkerToUi::LoadingState { tab_id, loading } => {
      (WorkerToUiEvent::LoadingState { tab_id, loading }, None)
    }
    WorkerToUi::Warning { tab_id, text } => (WorkerToUiEvent::Warning { tab_id, text }, None),
    WorkerToUi::DebugLog { tab_id, line } => (WorkerToUiEvent::DebugLog { tab_id, line }, None),
    WorkerToUi::SelectDropdownClosed { tab_id } => {
      (WorkerToUiEvent::SelectDropdownClosed { tab_id }, None)
    }
    WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      link_url,
    } => (
      WorkerToUiEvent::ContextMenu {
        tab_id,
        pos_css,
        link_url,
      },
      None,
    ),
    WorkerToUi::FindResult {
      tab_id,
      query,
      case_sensitive,
      match_count,
      active_match_index,
    } => (
      WorkerToUiEvent::FindResult {
        tab_id,
        query,
        case_sensitive,
        match_count,
        active_match_index,
      },
      None,
    ),
    WorkerToUi::SetClipboardText { tab_id, text } => {
      (WorkerToUiEvent::SetClipboardText { tab_id, text }, None)
    }
    WorkerToUi::HoverChanged {
      tab_id,
      hovered_url,
      cursor,
    } => (
      WorkerToUiEvent::HoverChanged {
        tab_id,
        hovered_url,
        cursor,
      },
      None,
    ),
    WorkerToUi::FindResult {
      tab_id,
      query,
      case_sensitive,
      match_count,
      active_match_index,
    } => (
      WorkerToUiEvent::FindResult {
        tab_id,
        query,
        case_sensitive,
        match_count,
        active_match_index,
      },
      None,
    ),
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
  // The browser integration suite mutates a handful of process-global knobs (e.g.
  // `render_control::set_test_render_delay_ms`). Serialize the worker runtime harness with the rest
  // of the suite so those overrides don't leak across tests and cause flakiness/timeouts.
  _stage_lock: parking_lot::ReentrantMutexGuard<'static, ()>,
  ui_tx: Option<Sender<UiToWorker>>,
  ui_rx: Receiver<WorkerToUi>,
  handle: Option<JoinHandle<()>>,
}

impl WorkerHarness {
  pub fn spawn() -> Self {
    let stage_lock = super::stage_listener_test_lock();

    let worker = spawn_ui_worker("fastr-browser-worker-runtime-test")
      .expect("spawn ui worker for runtime harness");

    Self {
      _stage_lock: stage_lock,
      ui_tx: Some(worker.ui_tx),
      ui_rx: worker.ui_rx,
      handle: Some(worker.join),
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
        panic!("timed out waiting for worker event; got {events:?}");
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
        panic!("timed out waiting for FrameReady; got {events:?}");
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
      if let (
        WorkerToUiEvent::FrameReady {
          tab_id: msg_tab, ..
        },
        Some(frame),
      ) = (event, frame)
      {
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
