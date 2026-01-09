use crate::api::{BrowserDocument, FastRender, PreparedDocument, PreparedPaintOptions};
use crate::geometry::Point;
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use crate::{Pixmap, RenderOptions, Result};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// Install a stage listener that forwards heartbeats to the UI for the lifetime of the returned
/// guard.
///
/// # Concurrency
///
/// Stage listeners are global (shared by the entire process). The browser UI currently assumes
/// that the render worker executes at most one render job at a time.
///
/// If we introduce concurrent rendering (multiple render worker threads or overlapping prepare +
/// paint jobs), this implementation must be replaced with per-job routing (e.g. making stage
/// listeners scoped per-thread/job, or attaching a job identifier to the heartbeat).
fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

/// Minimal render worker wrapper used by the browser UI.
///
/// This struct owns a `FastRender` instance and forwards stage heartbeats to the UI via
/// [`WorkerToUi::Stage`] messages.
pub struct RenderWorker {
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
}

impl RenderWorker {
  pub fn new(renderer: FastRender, ui_tx: Sender<WorkerToUi>) -> Self {
    Self { renderer, ui_tx }
  }

  pub fn prepare_html(
    &mut self,
    tab_id: TabId,
    html: &str,
    options: RenderOptions,
  ) -> Result<PreparedDocument> {
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    self.renderer.prepare_html(html, options)
  }

  pub fn paint_prepared(
    &self,
    tab_id: TabId,
    doc: &PreparedDocument,
    options: PreparedPaintOptions,
  ) -> Result<Pixmap> {
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    doc.paint_with_options(options)
  }
}

/// Spawn a dedicated render worker thread for the browser UI.
///
/// The full render pipeline can recurse deeply on complex pages (DOM/style/layout), so the browser
/// UI should run it on a large-stack thread (matching the CLI render worker stack size).
pub fn spawn_render_worker_thread<T: Send + 'static>(
  name: impl Into<String>,
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
  f: impl FnOnce(RenderWorker) -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
  std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || {
      let worker = RenderWorker::new(renderer, ui_tx);
      f(worker)
    })
}

pub struct UiWorkerHandle {
  pub ui_tx: Sender<UiToWorker>,
  pub ui_rx: Receiver<WorkerToUi>,
  handle: std::thread::JoinHandle<()>,
}

impl UiWorkerHandle {
  pub fn join(self) -> std::thread::Result<()> {
    // Ensure the worker loop can observe channel closure before we block on joining.
    drop(self.ui_tx);
    self.handle.join()
  }
}

/// Spawn the headless browser UI worker loop.
///
/// This worker consumes [`UiToWorker`] messages and emits [`WorkerToUi`] updates (frames,
/// navigation events, etc). It is intended to be driven by a UI thread/event loop, but it is also
/// usable from tests to exercise end-to-end interaction wiring.
pub fn spawn_ui_worker(name: impl Into<String>) -> std::io::Result<UiWorkerHandle> {
  let (ui_tx, worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (worker_tx, ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let handle = std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || ui_worker_main(worker_rx, worker_tx))?;

  Ok(UiWorkerHandle { ui_tx, ui_rx, handle })
}

struct TabState {
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  history: TabHistory,
  document: Option<BrowserDocument>,
  current_url: Option<String>,
  base_url: Option<String>,
  interaction: InteractionEngine,
}

impl TabState {
  fn new() -> Self {
    Self {
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      history: TabHistory::new(),
      document: None,
      current_url: None,
      base_url: None,
      interaction: InteractionEngine::new(),
    }
  }

  fn page_point_for_viewport_pos(&self, pos_css: (f32, f32)) -> Point {
    Point::new(
      pos_css.0 + self.scroll_state.viewport.x,
      pos_css.1 + self.scroll_state.viewport.y,
    )
  }

  fn effective_base_url(&self) -> Option<&str> {
    self
      .base_url
      .as_deref()
      .or_else(|| self.current_url.as_deref())
  }
}

fn ui_worker_main(rx: Receiver<UiToWorker>, tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();

  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab { tab_id, initial_url } => {
        let entry = tabs.entry(tab_id).or_insert_with(TabState::new);
        entry.history = TabHistory::new();
        entry.document = None;
        entry.current_url = None;
        entry.base_url = None;
        entry.interaction = InteractionEngine::new();

        if let Some(url) = initial_url {
          navigate_tab(tab_id, entry, url, NavigationReason::TypedUrl, &tx);
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        tabs.remove(&tab_id);
      }
      UiToWorker::SetActiveTab { .. } => {
        // Headless worker tracks state per tab id; active-tab routing is handled by the UI.
      }
      UiToWorker::Navigate { tab_id, url, reason } => {
        let tab = tabs.entry(tab_id).or_insert_with(TabState::new);
        navigate_tab(tab_id, tab, url, reason, &tx);
      }
      UiToWorker::GoBack { .. } | UiToWorker::GoForward { .. } | UiToWorker::Reload { .. } => {
        // `TabEngine` owns the navigation history state machine for the real browser UI.
        // This headless worker loop is primarily used for interaction wiring tests and expects the
        // UI to issue explicit `Navigate` commands.
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let tab = tabs.entry(tab_id).or_insert_with(TabState::new);
        tab.viewport_css = viewport_css;
        tab.dpr = dpr;
        if let Some(doc) = tab.document.as_mut() {
          doc.set_viewport(viewport_css.0, viewport_css.1);
          repaint_if_needed(tab_id, tab, &tx);
        }
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        ..
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        if tab.document.is_none() {
          continue;
        }

        let next = Point::new(
          tab.scroll_state.viewport.x + delta_css.0,
          tab.scroll_state.viewport.y + delta_css.1,
        );
        tab.scroll_state.viewport.x = if next.x.is_finite() { next.x.max(0.0) } else { 0.0 };
        tab.scroll_state.viewport.y = if next.y.is_finite() { next.y.max(0.0) } else { 0.0 };

        if let Some(doc) = tab.document.as_mut() {
          doc.set_scroll_state(tab.scroll_state.clone());
        }
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let page_point = tab.page_point_for_viewport_pos(pos_css);
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let _ = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_move(dom, box_tree, fragment_tree, page_point);
          (changed, ())
        });
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } => {
        if button != PointerButton::Primary {
          continue;
        }
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let page_point = tab.page_point_for_viewport_pos(pos_css);
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let _ = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_down(dom, box_tree, fragment_tree, page_point);
          (changed, ())
        });
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } => {
        if button != PointerButton::Primary {
          continue;
        }
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let page_point = tab.page_point_for_viewport_pos(pos_css);
        let base_url = tab.effective_base_url().unwrap_or("").to_string();
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let action = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          engine.pointer_up(dom, box_tree, fragment_tree, page_point, &base_url)
        }) {
          Ok(action) => action,
          Err(_) => continue,
        };

        match action {
          InteractionAction::Navigate { href } => {
            navigate_tab(tab_id, tab, href, NavigationReason::LinkClick, &tx);
          }
          _ => {
            repaint_if_needed(tab_id, tab, &tx);
          }
        }
      }
      UiToWorker::TextInput { tab_id, text } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let engine = &mut tab.interaction;
        let _ = doc.mutate_dom(|dom| engine.text_input(dom, &text));
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };
        let engine = &mut tab.interaction;
        let _ = doc.mutate_dom(|dom| engine.key_action(dom, key));
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::RequestRepaint { tab_id, .. } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        repaint_force(tab_id, tab, &tx);
      }
    }
  }
}

fn navigate_tab(
  tab_id: TabId,
  tab: &mut TabState,
  url: String,
  reason: NavigationReason,
  tx: &Sender<WorkerToUi>,
) {
  let _ = tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url.clone(),
  });

  // New navigation resets interaction state. This avoids leaking focus/hover chain ids across DOM
  // trees.
  tab.interaction = InteractionEngine::new();
  tab.scroll_state = ScrollState::default();

  let options = RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr);

  let mut renderer = match FastRender::new() {
    Ok(renderer) => renderer,
    Err(err) => {
      let _ = tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url,
        error: err.to_string(),
      });
      return;
    }
  };

  let report = match renderer.prepare_url(&url, options.clone()) {
    Ok(report) => report,
    Err(err) => {
      let _ = tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url,
        error: err.to_string(),
      });
      return;
    }
  };

  let final_url = report.final_url.clone().unwrap_or_else(|| url.clone());
  let base_url = report
    .base_url
    .clone()
    .unwrap_or_else(|| final_url.clone());

  let doc = match BrowserDocument::from_prepared(renderer, report.document, options) {
    Ok(doc) => doc,
    Err(err) => {
      let _ = tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url,
        error: err.to_string(),
      });
      return;
    }
  };

  tab.current_url = Some(final_url.clone());
  tab.base_url = Some(base_url);
  tab.document = Some(doc);

  // History bookkeeping (best-effort for MVP headless worker).
  match reason {
    NavigationReason::BackForward | NavigationReason::Reload => {}
    NavigationReason::TypedUrl | NavigationReason::LinkClick => tab.history.push(final_url.clone()),
  }

  let title = tab
    .document
    .as_ref()
    .and_then(|doc| crate::html::title::find_document_title(doc.dom()));

  let _ = tx.send(WorkerToUi::NavigationCommitted {
    tab_id,
    url: final_url,
    title,
    can_go_back: tab.history.can_go_back(),
    can_go_forward: tab.history.can_go_forward(),
  });

  repaint_force(tab_id, tab, tx);
}

fn repaint_if_needed(tab_id: TabId, tab: &mut TabState, tx: &Sender<WorkerToUi>) {
  let Some(doc) = tab.document.as_mut() else {
    return;
  };

  let Ok(Some(painted)) = doc.render_if_needed_with_scroll_state() else {
    return;
  };

  tab.scroll_state = painted.scroll_state.clone();
  let _ = tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state: painted.scroll_state.clone(),
    },
  });
  let _ = tx.send(WorkerToUi::ScrollStateUpdated {
    tab_id,
    scroll: painted.scroll_state,
  });
}

fn repaint_force(tab_id: TabId, tab: &mut TabState, tx: &Sender<WorkerToUi>) {
  let Some(doc) = tab.document.as_mut() else {
    return;
  };

  let painted = match doc.render_frame_with_scroll_state() {
    Ok(frame) => frame,
    Err(_) => return,
  };

  tab.scroll_state = painted.scroll_state.clone();
  let _ = tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state: painted.scroll_state.clone(),
    },
  });
  let _ = tx.send(WorkerToUi::ScrollStateUpdated {
    tab_id,
    scroll: painted.scroll_state,
  });
}
