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
use url::Url;

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

  Ok(UiWorkerHandle {
    ui_tx,
    ui_rx,
    handle,
  })
}

struct TabState {
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  history: TabHistory,
  document: Option<BrowserDocument>,
  current_url: Option<String>,
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
      .document
      .as_ref()
      .and_then(|doc| doc.base_url())
      .or_else(|| self.current_url.as_deref())
  }
}

fn ui_worker_main(rx: Receiver<UiToWorker>, tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();

  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        ..
      } => {
        let entry = tabs.entry(tab_id).or_insert_with(TabState::new);
        entry.history = TabHistory::new();
        entry.document = None;
        entry.current_url = None;
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
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        let tab = tabs.entry(tab_id).or_insert_with(TabState::new);
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        navigate_tab(tab_id, tab, url, reason, &tx);
      }
      UiToWorker::GoBack { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        let Some((url, scroll_x, scroll_y)) = tab
          .history
          .go_back()
          .map(|entry| (entry.url.clone(), entry.scroll_x, entry.scroll_y))
        else {
          let _ = tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: "cannot go back: no history entry".to_string(),
          });
          continue;
        };
        tab.scroll_state = ScrollState::with_viewport(Point::new(scroll_x, scroll_y));
        navigate_tab(tab_id, tab, url, NavigationReason::BackForward, &tx);
      }
      UiToWorker::GoForward { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        let Some((url, scroll_x, scroll_y)) = tab
          .history
          .go_forward()
          .map(|entry| (entry.url.clone(), entry.scroll_x, entry.scroll_y))
        else {
          let _ = tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: "cannot go forward: no history entry".to_string(),
          });
          continue;
        };
        tab.scroll_state = ScrollState::with_viewport(Point::new(scroll_x, scroll_y));
        navigate_tab(tab_id, tab, url, NavigationReason::BackForward, &tx);
      }
      UiToWorker::Reload { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        let Some(url) = tab
          .history
          .reload_target()
          .map(|entry| entry.url.clone())
          .or_else(|| tab.current_url.clone())
        else {
          let _ = tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: "cannot reload: no active URL".to_string(),
          });
          continue;
        };
        navigate_tab(tab_id, tab, url, NavigationReason::Reload, &tx);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let tab = tabs.entry(tab_id).or_insert_with(TabState::new);
        tab.viewport_css = (viewport_css.0.max(1), viewport_css.1.max(1));
        tab.dpr = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
        if let Some(doc) = tab.document.as_mut() {
          doc.set_viewport(tab.viewport_css.0, tab.viewport_css.1);
          doc.set_device_pixel_ratio(tab.dpr);
          repaint_if_needed(tab_id, tab, &tx);
        }
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let pointer = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite());
        let delta = (
          if delta_css.0.is_finite() { delta_css.0 } else { 0.0 },
          if delta_css.1.is_finite() { delta_css.1 } else { 0.0 },
        );

        if let Some((x, y)) = pointer {
          // Prefer pointer-based wheel scrolling when we have a cached layout; this enables nested
          // overflow container scrolling, scroll chaining, and viewport fallback.
          if doc
            .wheel_scroll_at_viewport_point(Point::new(x, y), delta)
            .is_ok()
          {
            tab.scroll_state = doc.scroll_state();
          } else {
            let next = Point::new(
              tab.scroll_state.viewport.x + delta.0,
              tab.scroll_state.viewport.y + delta.1,
            );
            tab.scroll_state.viewport.x =
              if next.x.is_finite() { next.x.max(0.0) } else { 0.0 };
            tab.scroll_state.viewport.y =
              if next.y.is_finite() { next.y.max(0.0) } else { 0.0 };
            doc.set_scroll_state(tab.scroll_state.clone());
          }
        } else {
          let next = Point::new(
            tab.scroll_state.viewport.x + delta.0,
            tab.scroll_state.viewport.y + delta.1,
          );
          tab.scroll_state.viewport.x = if next.x.is_finite() { next.x.max(0.0) } else { 0.0 };
          tab.scroll_state.viewport.y = if next.y.is_finite() { next.y.max(0.0) } else { 0.0 };
          doc.set_scroll_state(tab.scroll_state.clone());
        }

        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        repaint_if_needed(tab_id, tab, &tx);
      }
      UiToWorker::PointerMove {
        tab_id, pos_css, ..
      } => {
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
        let scroll_state = tab.scroll_state.clone();
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let action = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          engine.pointer_up_with_scroll(
            dom,
            box_tree,
            fragment_tree,
            &scroll_state,
            page_point,
            &base_url,
          )
        }) {
          Ok(action) => action,
          Err(_) => continue,
        };

        match action {
          InteractionAction::Navigate { href } => {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
            navigate_tab(tab_id, tab, href, NavigationReason::LinkClick, &tx);
          }
          InteractionAction::OpenSelectDropdown {
            select_node_id,
            control,
          } => {
            let _ = tx.send(WorkerToUi::OpenSelectDropdown {
              tab_id,
              select_node_id,
              control,
            });
            repaint_if_needed(tab_id, tab, &tx);
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
        let base_url = tab.effective_base_url().unwrap_or("").to_string();
        let document_url = tab.current_url.clone().unwrap_or_default();
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };
        let engine = &mut tab.interaction;
        let mut action = InteractionAction::None;
        let _ = doc.mutate_dom(|dom| {
          let (changed, a) = engine.key_activate(dom, key, &document_url, &base_url);
          action = a;
          changed
        });
        match action {
          InteractionAction::Navigate { href } => {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
            navigate_tab(tab_id, tab, href, NavigationReason::LinkClick, &tx);
          }
          InteractionAction::OpenSelectDropdown {
            select_node_id,
            control,
          } => {
            let _ = tx.send(WorkerToUi::OpenSelectDropdown {
              tab_id,
              select_node_id,
              control,
            });
            repaint_if_needed(tab_id, tab, &tx);
          }
          _ => repaint_if_needed(tab_id, tab, &tx),
        }
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

fn url_fragment(url: &str) -> Option<&str> {
  url.split_once('#').map(|(_, fragment)| fragment)
}

fn urls_match_except_fragment(a: &str, b: &str) -> bool {
  let Ok(a_url) = Url::parse(a) else {
    return false;
  };
  let Ok(b_url) = Url::parse(b) else {
    return false;
  };
  let mut a_no_frag = a_url.clone();
  a_no_frag.set_fragment(None);
  let mut b_no_frag = b_url.clone();
  b_no_frag.set_fragment(None);
  a_no_frag == b_no_frag
}

fn navigate_tab(
  tab_id: TabId,
  tab: &mut TabState,
  url: String,
  reason: NavigationReason,
  tx: &Sender<WorkerToUi>,
) {
  if let (Some(current), Some(doc)) = (tab.current_url.as_deref(), tab.document.as_mut()) {
    // Fragment-only navigation within the same document.
    //
    // We intentionally avoid a full reload (re-prepare/re-layout) and instead:
    // - update the tab/document URL,
    // - compute a new scroll position using existing layout artifacts,
    // - emit navigation messages so the UI updates its address bar/history,
    // - repaint at the new scroll offset.
    //
    // `Reload` must not take this path because the caller expects a full reload.
    if reason != NavigationReason::Reload && current != url && urls_match_except_fragment(current, &url) {
      let _ = tx.send(WorkerToUi::NavigationStarted {
        tab_id,
        url: url.clone(),
      });

      tab.current_url = Some(url.clone());
      match reason {
        NavigationReason::BackForward | NavigationReason::Reload => {}
        NavigationReason::TypedUrl | NavigationReason::LinkClick => tab.history.push(url.clone()),
      }
      doc.set_document_url_without_invalidation(Some(url.clone()));

      if matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick) {
        let computed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let viewport = fragment_tree.viewport_size();
          let fragment = url_fragment(&url).unwrap_or("");
          let offset = crate::interaction::scroll_offset_for_fragment_target(
            dom,
            box_tree,
            fragment_tree,
            fragment,
            viewport,
          );
          (false, offset)
        });

        // When the fragment is empty or missing, or the target cannot be found, scroll to the top
        // of the document (matching common browser `href=\"#\"` behavior).
        let offset = match computed {
          Ok(Some(offset)) => offset,
          Ok(None) => Point::ZERO,
          Err(err) => {
            let _ = tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("fragment navigation scroll failed: {err}"),
            });
            tab.scroll_state.viewport
          }
        };

        tab.scroll_state.viewport = offset;
      }

      doc.set_scroll_state(tab.scroll_state.clone());

      let title = crate::html::title::find_document_title(doc.dom());
      let _ = tx.send(WorkerToUi::NavigationCommitted {
        tab_id,
        url: url.clone(),
        title,
        can_go_back: tab.history.can_go_back(),
        can_go_forward: tab.history.can_go_forward(),
      });

      repaint_force(tab_id, tab, tx);
      return;
    }
  }

  let _ = tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url.clone(),
  });

  // New navigation resets interaction state. This avoids leaking focus/hover chain ids across DOM
  // trees.
  tab.interaction = InteractionEngine::new();
  match reason {
    NavigationReason::TypedUrl | NavigationReason::LinkClick => {
      tab.scroll_state = ScrollState::default();
    }
    NavigationReason::BackForward => {
      // Preserve viewport scroll across history navigations, but clear element offsets since they
      // belong to the old fragment tree.
      tab.scroll_state = ScrollState::with_viewport(tab.scroll_state.viewport);
    }
    NavigationReason::Reload => {}
  }

  let options = RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
    .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y)
    .with_element_scroll_offsets(tab.scroll_state.elements.clone());

  let final_url = if let Some(doc) = tab.document.as_mut() {
    match doc.navigate_url(&url, options) {
      Ok(report) => report.final_url.clone().unwrap_or_else(|| url.clone()),
      Err(err) => {
        let _ = tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.to_string(),
        });
        return;
      }
    }
  } else {
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
    tab.document = Some(doc);
    final_url
  };

  let Some(doc) = tab.document.as_mut() else {
    return;
  };
  doc.set_scroll_state(tab.scroll_state.clone());
  if matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick) {
    if let Some(fragment) = url_fragment(&final_url) {
      let offset = doc.prepared().and_then(|prepared| {
        crate::interaction::scroll_offset_for_fragment_target(
          doc.dom(),
          prepared.box_tree(),
          prepared.fragment_tree(),
          fragment,
          prepared.layout_viewport(),
        )
      });
      if let Some(offset) = offset {
        tab.scroll_state.viewport = offset;
        doc.set_scroll_state(tab.scroll_state.clone());
      }
    }
  }
  tab.current_url = Some(final_url.clone());

  let title = tab
    .document
    .as_ref()
    .and_then(|doc| crate::html::title::find_document_title(doc.dom()));

  // History bookkeeping (best-effort for MVP headless worker).
  match reason {
    NavigationReason::TypedUrl | NavigationReason::LinkClick => tab.history.push(final_url.clone()),
    NavigationReason::BackForward | NavigationReason::Reload => {}
  }
  tab.history.commit_navigation(&url, Some(&final_url));
  if let Some(title) = title.clone() {
    tab.history.set_title(title);
  }

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
  tab
    .history
    .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
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
  tab
    .history
    .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
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
