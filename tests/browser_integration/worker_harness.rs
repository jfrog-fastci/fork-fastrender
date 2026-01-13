#![cfg(feature = "browser_ui")]

use fastrender::render_control::StageHeartbeat;
use fastrender::scroll::ScrollState;
use fastrender::tree::box_tree::SelectControl;
use fastrender::api::FastRenderFactory;
use fastrender::accessibility::AccessibilityNode;
use fastrender::ui::messages::{
  CursorKind, DateTimeInputKind, DownloadId, DownloadOutcome, FormSubmission, RenderedFrame, TabId,
  UiToWorker, WorkerToUi,
};
use fastrender::ui::{spawn_ui_worker, spawn_ui_worker_for_test, spawn_ui_worker_with_factory};
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI), and the worker
// runtime performs real parsing/layout/paint work. Use a generous timeout to avoid flakes under CPU
// contention.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BUFFERED_EVENTS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessErrorKind {
  Timeout,
  Disconnected,
}

pub struct HarnessError {
  pub kind: HarnessErrorKind,
  pub timeout: Duration,
  pub buffered: Vec<WorkerToUiEvent>,
}

impl HarnessError {
  fn timeout(timeout: Duration, buffered: Vec<WorkerToUiEvent>) -> Self {
    Self {
      kind: HarnessErrorKind::Timeout,
      timeout,
      buffered,
    }
  }

  fn disconnected(timeout: Duration, buffered: Vec<WorkerToUiEvent>) -> Self {
    Self {
      kind: HarnessErrorKind::Disconnected,
      timeout,
      buffered,
    }
  }

  pub fn formatted_buffer(&self) -> String {
    format_events(&self.buffered)
  }
}

impl fmt::Debug for HarnessError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("HarnessError")
      .field("kind", &self.kind)
      .field("timeout", &self.timeout)
      .field("buffered", &self.formatted_buffer())
      .finish()
  }
}

impl fmt::Display for HarnessError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.kind {
      HarnessErrorKind::Timeout => {
        write!(
          f,
          "timed out waiting for worker event after {:?}; buffered events:\n{}",
          self.timeout,
          self.formatted_buffer()
        )
      }
      HarnessErrorKind::Disconnected => write!(
        f,
        "worker channel disconnected while waiting for event; buffered events:\n{}",
        self.formatted_buffer()
      ),
    }
  }
}

impl std::error::Error for HarnessError {}

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
    pixmap_px: (u32, u32),
    viewport_css: (u32, u32),
    dpr: f32,
    scroll_state: ScrollState,
  },
  PageAccessibility {
    tab_id: TabId,
    node_count: usize,
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
  RequestOpenInNewTabRequest {
    tab_id: TabId,
    request: FormSubmission,
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
  DateTimePickerOpened {
    tab_id: TabId,
    input_node_id: usize,
    kind: DateTimeInputKind,
    value: String,
  },
  DateTimePickerClosed {
    tab_id: TabId,
  },
  FilePickerOpened {
    tab_id: TabId,
    input_node_id: usize,
    multiple: bool,
    accept: Option<String>,
  },
  FilePickerClosed {
    tab_id: TabId,
  },
  ContextMenu {
    tab_id: TabId,
    pos_css: (f32, f32),
    link_url: Option<String>,
    image_url: Option<String>,
  },
  HoverChanged {
    tab_id: TabId,
    hovered_url: Option<String>,
    cursor: CursorKind,
    tooltip: Option<String>,
  },
  FindResult {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
    match_count: usize,
    active_match_index: Option<usize>,
  },
  DownloadStarted {
    tab_id: TabId,
    download_id: DownloadId,
    url: String,
    file_name: String,
    path: PathBuf,
    total_bytes: Option<u64>,
  },
  DownloadProgress {
    tab_id: TabId,
    download_id: DownloadId,
    received_bytes: u64,
    total_bytes: Option<u64>,
  },
  DownloadFinished {
    tab_id: TabId,
    download_id: DownloadId,
    outcome: DownloadOutcome,
  },
  /// Catch-all event for forward compatibility when `WorkerToUi` grows new variants.
  Other { msg: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEventKind {
  Stage,
  Favicon,
  FrameReady,
  PageAccessibility,
  OpenSelectDropdown,
  RequestOpenInNewTab,
  RequestOpenInNewTabRequest,
  NavigationStarted,
  NavigationCommitted,
  NavigationFailed,
  ScrollStateUpdated,
  LoadingState(bool),
  Warning,
  SetClipboardText,
  DebugLog,
  SelectDropdownClosed,
  DateTimePickerOpened,
  DateTimePickerClosed,
  FilePickerOpened,
  FilePickerClosed,
  ContextMenu,
  HoverChanged,
  FindResult,
  DownloadStarted,
  DownloadProgress,
  DownloadFinished,
  Other,
}

impl WorkerToUiEvent {
  pub fn kind(&self) -> WorkerEventKind {
    match self {
      WorkerToUiEvent::Stage { .. } => WorkerEventKind::Stage,
      WorkerToUiEvent::Favicon { .. } => WorkerEventKind::Favicon,
      WorkerToUiEvent::FrameReady { .. } => WorkerEventKind::FrameReady,
      WorkerToUiEvent::PageAccessibility { .. } => WorkerEventKind::PageAccessibility,
      WorkerToUiEvent::OpenSelectDropdown { .. } => WorkerEventKind::OpenSelectDropdown,
      WorkerToUiEvent::RequestOpenInNewTab { .. } => WorkerEventKind::RequestOpenInNewTab,
      WorkerToUiEvent::RequestOpenInNewTabRequest { .. } => WorkerEventKind::RequestOpenInNewTabRequest,
      WorkerToUiEvent::NavigationStarted { .. } => WorkerEventKind::NavigationStarted,
      WorkerToUiEvent::NavigationCommitted { .. } => WorkerEventKind::NavigationCommitted,
      WorkerToUiEvent::NavigationFailed { .. } => WorkerEventKind::NavigationFailed,
      WorkerToUiEvent::ScrollStateUpdated { .. } => WorkerEventKind::ScrollStateUpdated,
      WorkerToUiEvent::LoadingState { loading, .. } => WorkerEventKind::LoadingState(*loading),
      WorkerToUiEvent::Warning { .. } => WorkerEventKind::Warning,
      WorkerToUiEvent::SetClipboardText { .. } => WorkerEventKind::SetClipboardText,
      WorkerToUiEvent::DebugLog { .. } => WorkerEventKind::DebugLog,
      WorkerToUiEvent::SelectDropdownClosed { .. } => WorkerEventKind::SelectDropdownClosed,
      WorkerToUiEvent::DateTimePickerOpened { .. } => WorkerEventKind::DateTimePickerOpened,
      WorkerToUiEvent::DateTimePickerClosed { .. } => WorkerEventKind::DateTimePickerClosed,
      WorkerToUiEvent::FilePickerOpened { .. } => WorkerEventKind::FilePickerOpened,
      WorkerToUiEvent::FilePickerClosed { .. } => WorkerEventKind::FilePickerClosed,
      WorkerToUiEvent::ContextMenu { .. } => WorkerEventKind::ContextMenu,
      WorkerToUiEvent::HoverChanged { .. } => WorkerEventKind::HoverChanged,
      WorkerToUiEvent::FindResult { .. } => WorkerEventKind::FindResult,
      WorkerToUiEvent::DownloadStarted { .. } => WorkerEventKind::DownloadStarted,
      WorkerToUiEvent::DownloadProgress { .. } => WorkerEventKind::DownloadProgress,
      WorkerToUiEvent::DownloadFinished { .. } => WorkerEventKind::DownloadFinished,
      WorkerToUiEvent::Other { .. } => WorkerEventKind::Other,
    }
  }
}

fn accessibility_node_count(root: &AccessibilityNode) -> usize {
  let mut count = 0usize;
  let mut stack: Vec<&AccessibilityNode> = vec![root];
  while let Some(node) = stack.pop() {
    count += 1;
    for child in &node.children {
      stack.push(child);
    }
  }
  count
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
        pixmap_px: (frame.pixmap.width(), frame.pixmap.height()),
        viewport_css: frame.viewport_css,
        dpr: frame.dpr,
        scroll_state: frame.scroll_state.clone(),
      };
      (event, Some(frame))
    }
    WorkerToUi::PageAccessibility {
      tab_id,
      tree,
      bounds_css: _,
    } => {
      let node_count = accessibility_node_count(&tree);
      (
        WorkerToUiEvent::PageAccessibility { tab_id, node_count },
        None,
      )
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
    WorkerToUi::RequestOpenInNewTabRequest { tab_id, request } => (
      WorkerToUiEvent::RequestOpenInNewTabRequest { tab_id, request },
      None,
    ),
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
    WorkerToUi::DateTimePickerOpened {
      tab_id,
      input_node_id,
      kind,
      value,
      anchor_css: _,
    } => (
      WorkerToUiEvent::DateTimePickerOpened {
        tab_id,
        input_node_id,
        kind,
        value,
      },
      None,
    ),
    WorkerToUi::DateTimePickerClosed { tab_id } => (WorkerToUiEvent::DateTimePickerClosed { tab_id }, None),
    WorkerToUi::FilePickerOpened {
      tab_id,
      input_node_id,
      multiple,
      accept,
      anchor_css: _,
    } => (
      WorkerToUiEvent::FilePickerOpened {
        tab_id,
        input_node_id,
        multiple,
        accept,
      },
      None,
    ),
    WorkerToUi::FilePickerClosed { tab_id } => (WorkerToUiEvent::FilePickerClosed { tab_id }, None),
    WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      link_url,
      image_url,
      ..
    } => (
      WorkerToUiEvent::ContextMenu {
        tab_id,
        pos_css,
        link_url,
        image_url,
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
      tooltip,
    } => (
      WorkerToUiEvent::HoverChanged {
        tab_id,
        hovered_url,
        cursor,
        tooltip,
      },
      None,
    ),
    WorkerToUi::DownloadStarted {
      tab_id,
      download_id,
      url,
      file_name,
      path,
      total_bytes,
    } => (
      WorkerToUiEvent::DownloadStarted {
        tab_id,
        download_id,
        url,
        file_name,
        path,
        total_bytes,
      },
      None,
    ),
    WorkerToUi::DownloadProgress {
      tab_id,
      download_id,
      received_bytes,
      total_bytes,
    } => (
      WorkerToUiEvent::DownloadProgress {
        tab_id,
        download_id,
        received_bytes,
        total_bytes,
      },
      None,
    ),
    WorkerToUi::DownloadFinished {
      tab_id,
      download_id,
      outcome,
    } => (
      WorkerToUiEvent::DownloadFinished {
        tab_id,
        download_id,
        outcome,
      },
      None,
    ),
    other => (WorkerToUiEvent::Other { msg: format!("{other:?}") }, None),
  }
}

pub fn format_events(events: &[WorkerToUiEvent]) -> String {
  use std::fmt::Write;

  if events.is_empty() {
    return "<no events>".to_string();
  }

  let mut out = String::new();
  for (idx, ev) in events.iter().enumerate() {
    let _ = write!(&mut out, "{idx}: ");
    match ev {
      WorkerToUiEvent::Stage { tab_id, stage } => {
        let _ = writeln!(&mut out, "Stage(tab={}, stage={stage:?})", tab_id.0);
      }
      WorkerToUiEvent::Favicon {
        tab_id,
        width,
        height,
      } => {
        let _ = writeln!(
          &mut out,
          "Favicon(tab={}, size={}x{})",
          tab_id.0, width, height
        );
      }
      WorkerToUiEvent::FrameReady {
        tab_id,
        pixmap_px,
        viewport_css,
        dpr,
        ..
      } => {
        let _ = writeln!(
          &mut out,
          "FrameReady(tab={}, pixmap={}x{}, viewport_css={viewport_css:?}, dpr={dpr})",
          tab_id.0, pixmap_px.0, pixmap_px.1
        );
      }
      WorkerToUiEvent::OpenSelectDropdown {
        tab_id,
        select_node_id,
        ..
      } => {
        let _ = writeln!(
          &mut out,
          "OpenSelectDropdown(tab={}, select_node_id={select_node_id})",
          tab_id.0
        );
      }
      WorkerToUiEvent::RequestOpenInNewTab { tab_id, url } => {
        let _ = writeln!(&mut out, "RequestOpenInNewTab(tab={}, url={url})", tab_id.0);
      }
      WorkerToUiEvent::RequestOpenInNewTabRequest { tab_id, request } => {
        let _ = writeln!(
          &mut out,
          "RequestOpenInNewTabRequest(tab={}, request={request:?})",
          tab_id.0
        );
      }
      WorkerToUiEvent::NavigationStarted { tab_id, url } => {
        let _ = writeln!(&mut out, "NavigationStarted(tab={}, url={url})", tab_id.0);
      }
      WorkerToUiEvent::NavigationCommitted {
        tab_id,
        url,
        can_go_back,
        can_go_forward,
      } => {
        let _ = writeln!(
          &mut out,
          "NavigationCommitted(tab={}, url={url}, back={can_go_back}, forward={can_go_forward})",
          tab_id.0
        );
      }
      WorkerToUiEvent::NavigationFailed { tab_id, url, error } => {
        let _ = writeln!(
          &mut out,
          "NavigationFailed(tab={}, url={url}, error={error})",
          tab_id.0
        );
      }
      WorkerToUiEvent::ScrollStateUpdated { tab_id, scroll } => {
        let _ = writeln!(
          &mut out,
          "ScrollStateUpdated(tab={}, viewport={:?})",
          tab_id.0,
          scroll.viewport
        );
      }
      WorkerToUiEvent::LoadingState { tab_id, loading } => {
        let _ = writeln!(
          &mut out,
          "LoadingState(tab={}, loading={loading})",
          tab_id.0
        );
      }
      WorkerToUiEvent::Warning { tab_id, text } => {
        let _ = writeln!(&mut out, "Warning(tab={}, text={text})", tab_id.0);
      }
      WorkerToUiEvent::SetClipboardText { tab_id, text } => {
        let _ = writeln!(
          &mut out,
          "SetClipboardText(tab={}, text={text:?})",
          tab_id.0
        );
      }
      WorkerToUiEvent::DebugLog { tab_id, line } => {
        let line = line.trim_end();
        let _ = writeln!(&mut out, "DebugLog(tab={}, line={line})", tab_id.0);
      }
      WorkerToUiEvent::SelectDropdownClosed { tab_id } => {
        let _ = writeln!(&mut out, "SelectDropdownClosed(tab={})", tab_id.0);
      }
      WorkerToUiEvent::DateTimePickerOpened {
        tab_id,
        input_node_id,
        kind,
        value,
      } => {
        let _ = writeln!(
          &mut out,
          "DateTimePickerOpened(tab={}, input_node_id={}, kind={kind:?}, value={value:?})",
          tab_id.0, input_node_id
        );
      }
      WorkerToUiEvent::DateTimePickerClosed { tab_id } => {
        let _ = writeln!(&mut out, "DateTimePickerClosed(tab={})", tab_id.0);
      }
      WorkerToUiEvent::FilePickerOpened {
        tab_id,
        input_node_id,
        multiple,
        accept,
      } => {
        let _ = writeln!(
          &mut out,
          "FilePickerOpened(tab={}, input_node_id={}, multiple={}, accept={accept:?})",
          tab_id.0, input_node_id, multiple
        );
      }
      WorkerToUiEvent::FilePickerClosed { tab_id } => {
        let _ = writeln!(&mut out, "FilePickerClosed(tab={})", tab_id.0);
      }
      WorkerToUiEvent::ContextMenu {
        tab_id,
        pos_css,
        link_url,
        image_url,
      } => {
        let _ = writeln!(
          &mut out,
          "ContextMenu(tab={}, pos_css={pos_css:?}, link_url={link_url:?}, image_url={image_url:?})",
          tab_id.0
        );
      }
      WorkerToUiEvent::HoverChanged {
        tab_id,
        hovered_url,
        cursor,
      } => {
        let _ = writeln!(
          &mut out,
          "HoverChanged(tab={}, cursor={cursor:?}, hovered_url={:?})",
          tab_id.0,
          hovered_url.as_deref()
        );
      }
      WorkerToUiEvent::FindResult {
        tab_id,
        query,
        case_sensitive,
        match_count,
        active_match_index,
      } => {
        let _ = writeln!(
          &mut out,
          "FindResult(tab={}, query={query:?}, case_sensitive={case_sensitive}, match_count={match_count}, active={active_match_index:?})",
          tab_id.0
        );
      }
      WorkerToUiEvent::DownloadStarted {
        tab_id,
        download_id,
        url,
        file_name,
        path,
        total_bytes,
      } => {
        let _ = writeln!(
          &mut out,
          "DownloadStarted(tab={}, id={}, url={url}, file_name={file_name:?}, path={}, total={total_bytes:?})",
          tab_id.0,
          download_id.0,
          path.display()
        );
      }
      WorkerToUiEvent::DownloadProgress {
        tab_id,
        download_id,
        received_bytes,
        total_bytes,
      } => {
        let _ = writeln!(
          &mut out,
          "DownloadProgress(tab={}, id={}, received={}, total={total_bytes:?})",
          tab_id.0,
          download_id.0,
          received_bytes
        );
      }
      WorkerToUiEvent::DownloadFinished {
        tab_id,
        download_id,
        outcome,
      } => {
        let _ = writeln!(
          &mut out,
          "DownloadFinished(tab={}, id={}, outcome={outcome:?})",
          tab_id.0,
          download_id.0
        );
      }
      WorkerToUiEvent::Other { msg } => {
        let _ = writeln!(&mut out, "Other(tab=?, msg={msg})");
      }
    }
  }
  out
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
    "expected event subsequence {:?}.\nEvents:\n{}",
    expected,
    format_events(events)
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
  buffered_events: parking_lot::Mutex<VecDeque<WorkerToUiEvent>>,
}

impl WorkerHarness {
  fn worker_thread_finished(&self) -> Option<bool> {
    self.handle.as_ref().map(JoinHandle::is_finished)
  }

  fn worker_thread_name(&self) -> Option<&str> {
    self.handle.as_ref().and_then(|handle| handle.thread().name())
  }

  pub fn spawn() -> Self {
    let stage_lock = super::stage_listener_test_lock();

    let worker = spawn_ui_worker("fastr-browser-worker-runtime-test")
      .expect("spawn ui worker for runtime harness");

    Self {
      _stage_lock: stage_lock,
      ui_tx: Some(worker.ui_tx),
      ui_rx: worker.ui_rx,
      handle: Some(worker.join),
      buffered_events: parking_lot::Mutex::new(VecDeque::new()),
    }
  }

  pub fn spawn_with_factory(factory: FastRenderFactory) -> Self {
    let stage_lock = super::stage_listener_test_lock();

    let worker = spawn_ui_worker_with_factory("fastr-browser-worker-runtime-test", factory)
      .expect("spawn ui worker for runtime harness");

    Self {
      _stage_lock: stage_lock,
      ui_tx: Some(worker.ui_tx),
      ui_rx: worker.ui_rx,
      handle: Some(worker.join),
      buffered_events: parking_lot::Mutex::new(VecDeque::new()),
    }
  }

  pub fn spawn_with_test_render_delay(test_render_delay_ms: Option<u64>) -> Self {
    let stage_lock = super::stage_listener_test_lock();

    let worker = spawn_ui_worker_for_test(
      "fastr-browser-worker-runtime-test",
      test_render_delay_ms,
    )
    .expect("spawn ui worker for runtime harness with test render delay");

    Self {
      _stage_lock: stage_lock,
      ui_tx: Some(worker.ui_tx),
      ui_rx: worker.ui_rx,
      handle: Some(worker.join),
      buffered_events: parking_lot::Mutex::new(VecDeque::new()),
    }
  }

  pub fn send(&self, msg: UiToWorker) {
    let Some(tx) = self.ui_tx.as_ref() else {
      panic!(
        "worker harness tx not available; recent events:\n{}",
        format_events(&self.buffered_snapshot())
      );
    };
    if let Err(err) = tx.send(msg) {
      let worker_finished = self.worker_thread_finished();
      let worker_name = self.worker_thread_name().unwrap_or("<unnamed>");
      let msg = err.0;
      panic!(
        "failed to send UiToWorker to worker thread {worker_name} (finished={worker_finished:?}): {msg:?}\nrecent events:\n{}",
        format_events(&self.buffered_snapshot())
      );
    }
  }

  fn push_buffered_event(&self, event: &WorkerToUiEvent) {
    let mut buf = self.buffered_events.lock();
    if buf.len() >= MAX_BUFFERED_EVENTS {
      buf.pop_front();
    }
    buf.push_back(event.clone());
  }

  fn buffered_snapshot(&self) -> Vec<WorkerToUiEvent> {
    self
      .buffered_events
      .lock()
      .iter()
      .cloned()
      .collect::<Vec<_>>()
  }

  fn recv_message(
    &self,
    timeout: Duration,
  ) -> Result<(WorkerToUiEvent, Option<RenderedFrame>), HarnessError> {
    match self.ui_rx.recv_timeout(timeout) {
      Ok(msg) => {
        let (event, frame) = split_message(msg);
        self.push_buffered_event(&event);
        Ok((event, frame))
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        Err(HarnessError::timeout(timeout, self.buffered_snapshot()))
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(HarnessError::disconnected(
        timeout,
        self.buffered_snapshot(),
      )),
    }
  }

  pub fn recv_event(&self, timeout: Duration) -> Result<WorkerToUiEvent, HarnessError> {
    self.recv_message(timeout).map(|(event, _frame)| event)
  }

  /// Drain worker events for `duration`, returning a pretty-printed log.
  pub fn drain_for(&self, duration: Duration) -> String {
    let events = self.drain_events(duration);
    format_events(&events)
  }

  /// Assert that the worker disconnects within `timeout`, returning buffered events for debugging.
  pub fn assert_disconnect_within(&self, timeout: Duration) -> Vec<WorkerToUiEvent> {
    let deadline = Instant::now() + timeout;
    loop {
      let now = Instant::now();
      if now >= deadline {
        panic!(
          "timed out waiting for worker disconnect; buffered events:\n{}",
          format_events(&self.buffered_snapshot())
        );
      }
      let remaining = deadline.saturating_duration_since(now);
      match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => {
          let (event, _frame) = split_message(msg);
          self.push_buffered_event(&event);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
          return self.buffered_snapshot();
        }
      }
    }
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
          self.push_buffered_event(&event);
          events.push(event);
        }
        Err(RecvTimeoutError::Timeout) => break,
        Err(RecvTimeoutError::Disconnected) => break,
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
        panic!(
          "timed out waiting for worker event; recent events:\n{}",
          format_events(&self.buffered_snapshot())
        );
      }
      let remaining = deadline.saturating_duration_since(now);
      let msg = match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => msg,
        Err(RecvTimeoutError::Timeout) => continue,
        Err(RecvTimeoutError::Disconnected) => {
          let kinds = events.iter().map(WorkerToUiEvent::kind).collect::<Vec<_>>();
          let worker_finished = self.worker_thread_finished();
          let worker_name = self.worker_thread_name().unwrap_or("<unnamed>");
          panic!(
            "worker channel disconnected while waiting for event (timeout={timeout:?}, thread={worker_name}, finished={worker_finished:?}); collected event kinds: {kinds:?}; recent events:\n{}",
            format_events(&self.buffered_snapshot())
          );
        }
      };
      let (event, _frame) = split_message(msg);
      self.push_buffered_event(&event);
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
        panic!(
          "timed out waiting for FrameReady(tab_id={tab_id:?}); recent events:\n{}",
          format_events(&self.buffered_snapshot())
        );
      }
      let remaining = deadline.saturating_duration_since(now);
      let msg = match self.ui_rx.recv_timeout(remaining) {
        Ok(msg) => msg,
        Err(RecvTimeoutError::Timeout) => continue,
        Err(RecvTimeoutError::Disconnected) => {
          let kinds = events.iter().map(WorkerToUiEvent::kind).collect::<Vec<_>>();
          let worker_finished = self.worker_thread_finished();
          let worker_name = self.worker_thread_name().unwrap_or("<unnamed>");
          panic!(
            "worker channel disconnected while waiting for FrameReady(tab_id={tab_id:?}, timeout={timeout:?}, thread={worker_name}, finished={worker_finished:?}); collected event kinds: {kinds:?}; recent events:\n{}",
            format_events(&self.buffered_snapshot())
          );
        }
      };
      let (event, frame) = split_message(msg);
      self.push_buffered_event(&event);
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

  /// Wait for a `FrameReady` + matching `ScrollStateUpdated` pair for `tab_id`.
  ///
  /// The worker may emit `ScrollStateUpdated` either before or after the corresponding frame (for
  /// example if scroll updates are forwarded immediately on input). This helper matches the scroll
  /// update to the returned frame by comparing `scroll.viewport` with
  /// `frame.scroll_state.viewport`.
  pub fn wait_for_frame_and_scroll_state(
    &self,
    tab_id: TabId,
    timeout: Duration,
  ) -> (RenderedFrame, ScrollState, Vec<WorkerToUiEvent>) {
    let start = Instant::now();
    let (frame, mut events) = self.wait_for_frame(tab_id, timeout);
    let expected_viewport = frame.scroll_state.viewport;

    let find_matching_scroll = |events: &[WorkerToUiEvent]| {
      events.iter().rev().find_map(|ev| match ev {
        WorkerToUiEvent::ScrollStateUpdated {
          tab_id: got,
          scroll,
        } if *got == tab_id && scroll.viewport == expected_viewport => Some(scroll.clone()),
        _ => None,
      })
    };

    if let Some(scroll) = find_matching_scroll(&events) {
      return (frame, scroll, events);
    }

    let remaining = timeout.saturating_sub(start.elapsed());
    let more = self.wait_for_event(remaining, |ev| match ev {
      WorkerToUiEvent::ScrollStateUpdated {
        tab_id: got,
        scroll,
      } if *got == tab_id && scroll.viewport == expected_viewport => true,
      _ => false,
    });
    events.extend(more);

    let scroll = find_matching_scroll(&events)
      .expect("wait_for_event should yield a matching ScrollStateUpdated");
    (frame, scroll, events)
  }

  /// Wait until the worker channel disconnects (e.g. worker crash/panic) and return any events that
  /// were received before shutdown.
  pub fn wait_for_disconnect(&self, timeout: Duration) -> Vec<WorkerToUiEvent> {
    self.assert_disconnect_within(timeout)
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
      // Avoid hanging the entire test suite if the worker is stuck (e.g. during a long render, or
      // a crash mid-send that prevents the worker thread from noticing channel shutdown). Prefer
      // clean teardown, but detach after a short grace period so we still get useful failure
      // output.
      const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
      let (done_tx, done_rx) = std::sync::mpsc::channel();

      // Join the worker on a helper thread so `Drop` is always bounded even if `JoinHandle::join`
      // blocks (it has no built-in timeout).
      let _ = std::thread::Builder::new()
        .name("fastr-worker-harness-join".to_string())
        .spawn(move || {
          let _ = done_tx.send(handle.join());
        });

      let _ = done_rx.recv_timeout(JOIN_TIMEOUT);
    }
  }
}
