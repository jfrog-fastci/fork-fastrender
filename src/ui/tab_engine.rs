use crate::api::FastRender;
use crate::geometry::Point;
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::ui::about_pages;
use crate::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use crate::ui::TabHistory;
use crate::{PreparedPaintOptions, RenderOptions, Result};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

struct TabState {
  history: TabHistory,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  prepared: Option<crate::PreparedDocument>,
}

impl TabState {
  fn new() -> Self {
    Self {
      history: TabHistory::new(),
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      prepared: None,
    }
  }
}

/// Render-thread tab engine for the browser UI.
///
/// Owns tab history (back/forward/reload) and is responsible for emitting accurate navigation
/// state (`can_go_back` / `can_go_forward`) via [`WorkerToUi::NavigationCommitted`].
pub struct TabEngine {
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, TabState>,
}

impl TabEngine {
  pub fn new(renderer: FastRender, ui_tx: Sender<WorkerToUi>) -> Self {
    Self {
      renderer,
      ui_tx,
      tabs: HashMap::new(),
    }
  }

  pub fn handle(&mut self, msg: UiToWorker) {
    let Self {
      renderer,
      ui_tx,
      tabs,
    } = self;

    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
      } => {
        let mut tab = TabState::new();
        if let Some(url) = initial_url {
          tab.history.push(url.clone());
          // Follow the same semantics as a typed navigation: reset scroll.
          tab.scroll_state = ScrollState::default();
          let _ = navigate(renderer, ui_tx, tab_id, &mut tab, url);
        }
        tabs.insert(tab_id, tab);
      }
      UiToWorker::CloseTab { tab_id } => {
        tabs.remove(&tab_id);
      }
      UiToWorker::SetActiveTab { .. } => {}
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };

        match reason {
          NavigationReason::TypedUrl | NavigationReason::LinkClick => {
            tab.history.push(url.clone());
            tab.scroll_state = ScrollState::default();
            let _ = navigate(renderer, ui_tx, tab_id, tab, url);
          }
          NavigationReason::Reload => {
            // Legacy fallback: treat as an explicit reload request.
            if let Some(entry) = tab.history.reload_target() {
              let url = entry.url.clone();
              tab.scroll_state =
                ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y));
              let _ = navigate(renderer, ui_tx, tab_id, tab, url);
            }
          }
          NavigationReason::BackForward => {
            // Legacy fallback: without direction we can't reliably mutate history, so treat it as
            // a normal navigation.
            tab.history.push(url.clone());
            tab.scroll_state = ScrollState::default();
            let _ = navigate(renderer, ui_tx, tab_id, tab, url);
          }
        }
      }
      UiToWorker::GoBack { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.go_back() else {
          return;
        };
        let url = entry.url.clone();
        tab.scroll_state = ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y));
        let _ = navigate(renderer, ui_tx, tab_id, tab, url);
      }
      UiToWorker::GoForward { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.go_forward() else {
          return;
        };
        let url = entry.url.clone();
        tab.scroll_state = ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y));
        let _ = navigate(renderer, ui_tx, tab_id, tab, url);
      }
      UiToWorker::Reload { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.reload_target() else {
          return;
        };
        let url = entry.url.clone();
        tab.scroll_state = ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y));
        let _ = navigate(renderer, ui_tx, tab_id, tab, url);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };
        tab.viewport_css = viewport_css;
        tab.dpr = dpr;
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css: _,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          return;
        };

        let mut next = tab.scroll_state.clone();
        next.viewport.x = (next.viewport.x + delta_css.0).max(0.0);
        next.viewport.y = (next.viewport.y + delta_css.1).max(0.0);
        tab.scroll_state = next;

        // Best-effort: if we have a prepared document, repaint and synchronize the scroll state.
        let Some(doc) = tab.prepared.as_ref() else {
          tab
            .history
            .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
          let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
            tab_id,
            scroll: tab.scroll_state.clone(),
          });
          return;
        };

        let _guard = forward_stage_heartbeats(tab_id, ui_tx.clone());
        match doc.paint_with_options_frame(
          PreparedPaintOptions::new().with_scroll_state(tab.scroll_state.clone()),
        ) {
          Ok(painted) => {
            tab.scroll_state = painted.scroll_state.clone();
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

            let _ = ui_tx.send(WorkerToUi::FrameReady {
              tab_id,
              frame: RenderedFrame {
                pixmap: painted.pixmap,
                viewport_css: tab.viewport_css,
                dpr: doc.device_pixel_ratio(),
                scroll_state: tab.scroll_state.clone(),
              },
            });
            let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
              tab_id,
              scroll: tab.scroll_state.clone(),
            });
          }
          Err(err) => {
            let _ = ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("scroll repaint failed: {err}"),
            });
          }
        }
      }
      UiToWorker::PointerMove { .. }
      | UiToWorker::PointerDown { .. }
      | UiToWorker::PointerUp { .. }
      | UiToWorker::TextInput { .. }
      | UiToWorker::KeyAction { .. }
      | UiToWorker::RequestRepaint { .. } => {}
    }
  }
}

fn navigate(
  renderer: &mut FastRender,
  ui_tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  tab: &mut TabState,
  url: String,
) -> Result<()> {
  let url_trimmed = url.trim().to_string();
  if url_trimmed.is_empty() {
    return Ok(());
  }

  let _ = ui_tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url_trimmed.clone(),
  });
  let _ = ui_tx.send(WorkerToUi::LoadingState {
    tab_id,
    loading: true,
  });

  let _guard = forward_stage_heartbeats(tab_id, ui_tx.clone());

  let options = RenderOptions::default()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
    .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y)
    .with_element_scroll_offsets(tab.scroll_state.elements.clone());

  let (report, failure) = if about_pages::is_about_url(&url_trimmed) {
    (
      prepare_about_url(renderer, &url_trimmed, options.clone())?,
      None,
    )
  } else {
    match renderer.prepare_url(&url_trimmed, options.clone()) {
      Ok(report) => (report, None),
      Err(err) => {
        let html = about_pages::error_page_html("Navigation failed", &err.to_string());
        let report = prepare_about_html(renderer, about_pages::ABOUT_ERROR, &html, options)?;
        (report, Some(err.to_string()))
      }
    }
  };

  if let Some(error) = failure {
    let _ = ui_tx.send(WorkerToUi::NavigationFailed {
      tab_id,
      url: url_trimmed.clone(),
      error,
    });
  }

  let crate::PreparedDocumentReport {
    document,
    final_url,
    ..
  } = report;

  let dpr = document.device_pixel_ratio();
  let title = crate::html::find_document_title(document.dom());

  let painted = document.paint_with_options_frame(PreparedPaintOptions::new())?;
  tab.scroll_state = painted.scroll_state.clone();
  tab
    .history
    .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
  tab.prepared = Some(document);

  if let Some(title) = title.clone() {
    tab.history.set_title(title);
  }

  tab
    .history
    .commit_navigation(&url_trimmed, final_url.as_deref());
  let Some(current) = tab.history.current() else {
    return Ok(());
  };

  let _ = ui_tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr,
      scroll_state: tab.scroll_state.clone(),
    },
  });
  let _ = ui_tx.send(WorkerToUi::NavigationCommitted {
    tab_id,
    url: current.url.clone(),
    title: current.title.clone(),
    can_go_back: tab.history.can_go_back(),
    can_go_forward: tab.history.can_go_forward(),
  });
  let _ = ui_tx.send(WorkerToUi::LoadingState {
    tab_id,
    loading: false,
  });

  Ok(())
}

fn prepare_about_url(
  renderer: &mut FastRender,
  url: &str,
  options: RenderOptions,
) -> Result<crate::PreparedDocumentReport> {
  let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
    about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
  });
  prepare_about_html(renderer, url, &html, options)
}

fn prepare_about_html(
  renderer: &mut FastRender,
  document_url: &str,
  html: &str,
  options: RenderOptions,
) -> Result<crate::PreparedDocumentReport> {
  renderer.set_base_url(about_pages::ABOUT_BASE_URL);
  let dom = renderer.parse_html(html)?;
  renderer.prepare_dom_with_options(dom, Some(document_url), options)
}

#[cfg(test)]
mod tests {
  use super::TabEngine;
  use crate::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
  use crate::FastRender;

  fn drain_committed(
    rx: &std::sync::mpsc::Receiver<WorkerToUi>,
    tab_id: TabId,
  ) -> Vec<(String, bool, bool)> {
    rx.try_iter()
      .filter_map(|msg| match msg {
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          can_go_back,
          can_go_forward,
          ..
        } if msg_tab == tab_id => Some((url, can_go_back, can_go_forward)),
        _ => None,
      })
      .collect()
  }

  #[test]
  fn back_forward_and_reload_are_worker_history_driven() {
    let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
    let renderer = FastRender::new().unwrap();
    let mut engine = TabEngine::new(renderer, tx);

    let tab_id = TabId(1);
    engine.handle(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    });
    engine.handle(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (32, 32),
      dpr: 1.0,
    });

    engine.handle(UiToWorker::Navigate {
      tab_id,
      url: "about:blank".to_string(),
      reason: NavigationReason::TypedUrl,
    });
    assert_eq!(
      drain_committed(&rx, tab_id),
      vec![("about:blank".to_string(), false, false)]
    );

    engine.handle(UiToWorker::Navigate {
      tab_id,
      url: "about:newtab".to_string(),
      reason: NavigationReason::LinkClick,
    });
    assert_eq!(
      drain_committed(&rx, tab_id),
      vec![("about:newtab".to_string(), true, false)]
    );

    assert_eq!(engine.tabs.get(&tab_id).unwrap().history.len(), 2);

    engine.handle(UiToWorker::GoBack { tab_id });
    assert_eq!(
      drain_committed(&rx, tab_id),
      vec![("about:blank".to_string(), false, true)]
    );
    assert_eq!(engine.tabs.get(&tab_id).unwrap().history.len(), 2);

    // Reload should preserve forward history and keep history length stable.
    engine.handle(UiToWorker::Reload { tab_id });
    assert_eq!(
      drain_committed(&rx, tab_id),
      vec![("about:blank".to_string(), false, true)]
    );
    assert_eq!(engine.tabs.get(&tab_id).unwrap().history.len(), 2);

    engine.handle(UiToWorker::GoForward { tab_id });
    assert_eq!(
      drain_committed(&rx, tab_id),
      vec![("about:newtab".to_string(), true, false)]
    );
  }
}
