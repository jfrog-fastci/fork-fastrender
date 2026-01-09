use crate::api::{FastRender, PreparedDocument, PreparedPaintOptions};
use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::ui::history::TabHistory;
use crate::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use crate::{RenderOptions, Result};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

struct TabState {
  history: TabHistory,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  document: Option<PreparedDocument>,
  title: Option<String>,
}

impl TabState {
  fn new(initial_url: Option<String>) -> Self {
    let history = match initial_url {
      Some(url) => TabHistory::with_initial(url),
      None => TabHistory::new(),
    };
    Self {
      history,
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      document: None,
      title: None,
    }
  }
}

/// Headless browser UI worker loop.
///
/// This is a minimal implementation intended to be driven by `UiToWorker` messages and to emit
/// `WorkerToUi` responses. It is used by integration tests to lock down navigation history and
/// scroll restoration semantics.
pub struct UiWorker {
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
}

impl UiWorker {
  pub fn new(renderer: FastRender, ui_tx: Sender<WorkerToUi>) -> Self {
    Self {
      renderer,
      ui_tx,
      tabs: HashMap::new(),
      active_tab: None,
    }
  }

  pub fn run(mut self, rx: Receiver<UiToWorker>) {
    while let Ok(msg) = rx.recv() {
      self.handle_message(msg);
    }
  }

  pub fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
      } => {
        self.tabs.insert(tab_id, TabState::new(initial_url));
        self.active_tab.get_or_insert(tab_id);
      }
      UiToWorker::CloseTab { tab_id } => {
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
          self.active_tab = self.tabs.keys().next().copied();
        }
      }
      UiToWorker::SetActiveTab { tab_id } => {
        if self.tabs.contains_key(&tab_id) {
          self.active_tab = Some(tab_id);
        }
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        tab.viewport_css = viewport_css;
        tab.dpr = dpr;
        if tab.document.is_some() {
          self.repaint(tab_id);
        }
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        let _ = self.navigate(tab_id, url, reason);
      }
      UiToWorker::GoBack { .. } | UiToWorker::GoForward { .. } | UiToWorker::Reload { .. } => {
        // Navigation history in this worker is driven through `UiToWorker::Navigate` with an
        // explicit `NavigationReason`. The newer dedicated history commands are handled by the
        // main browser tab engine.
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css: _,
      } => {
        let _ = self.scroll(tab_id, delta_css);
      }
      UiToWorker::PointerMove { .. }
      | UiToWorker::PointerDown { .. }
      | UiToWorker::PointerUp { .. }
      | UiToWorker::TextInput { .. }
      | UiToWorker::KeyAction { .. }
      | UiToWorker::RequestRepaint { .. } => {}
    }
  }

  fn navigate(&mut self, tab_id: TabId, url: String, reason: NavigationReason) -> Result<()> {
    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return Ok(());
      };

      let requested_url = url;
      let mut nav_url = requested_url.clone();

      match reason {
        NavigationReason::TypedUrl | NavigationReason::LinkClick => {
          tab.history.push(requested_url.clone());
          tab.scroll_state = ScrollState::default();
        }
        NavigationReason::Reload => {
          if let Some(entry) = tab.history.reload_target() {
            nav_url = entry.url.clone();
          } else {
            tab.history.push(requested_url.clone());
            tab.scroll_state = ScrollState::default();
          }
        }
        NavigationReason::BackForward => {
          if tab
            .history
            .current()
            .is_some_and(|entry| entry.url == requested_url)
          {
            // No index change.
          } else {
            let mut moved = false;

            if tab.history.can_go_back() {
              let entry = tab.history.go_back();
              if entry.is_some_and(|entry| entry.url == requested_url) {
                moved = true;
              } else {
                let _ = tab.history.go_forward();
              }
            }

            if !moved && tab.history.can_go_forward() {
              let entry = tab.history.go_forward();
              if entry.is_some_and(|entry| entry.url == requested_url) {
                moved = true;
              } else {
                let _ = tab.history.go_back();
              }
            }

            if !moved {
              return Ok(());
            }
          }

          let Some(entry) = tab.history.current() else {
            return Ok(());
          };
          nav_url = entry.url.clone();
          tab.scroll_state = ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y));
        }
      }

      let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
        tab_id,
        url: nav_url.clone(),
      });

      let options = RenderOptions::new()
        .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
        .with_device_pixel_ratio(tab.dpr)
        .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

      let report = match self.renderer.prepare_url(&nav_url, options) {
        Ok(report) => report,
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url: nav_url.clone(),
            error: err.to_string(),
          });
          return Ok(());
        }
      };

      let final_url = report.final_url.as_deref().unwrap_or(&nav_url).to_string();
      tab.history.commit_navigation(&nav_url, report.final_url.as_deref());
      nav_url = final_url;

      tab.document = Some(report.document);
      tab.title = None;

      let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
        tab_id,
        url: nav_url,
        title: tab.title.clone(),
        can_go_back: tab.history.can_go_back(),
        can_go_forward: tab.history.can_go_forward(),
      });
    }
    self.paint_current(tab_id, true)?;

    Ok(())
  }

  fn scroll(&mut self, tab_id: TabId, delta_css: (f32, f32)) -> Result<()> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return Ok(());
    };
    if tab.document.is_none() {
      return Ok(());
    }

    let mut desired = tab.scroll_state.clone();
    if delta_css.0.is_finite() {
      desired.viewport.x += delta_css.0;
    }
    if delta_css.1.is_finite() {
      desired.viewport.y += delta_css.1;
    }
    tab.scroll_state = desired;
    self.paint_current(tab_id, false)?;
    Ok(())
  }

  fn repaint(&mut self, tab_id: TabId) {
    let _ = self.paint_current(tab_id, false);
  }

  fn paint_current(&mut self, tab_id: TabId, is_navigation: bool) -> Result<()> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return Ok(());
    };
    let Some(doc) = tab.document.as_ref() else {
      return Ok(());
    };

    let painted = doc.paint_with_options_frame(
      PreparedPaintOptions::new()
        .with_scroll_state(tab.scroll_state.clone())
        .with_viewport(tab.viewport_css.0, tab.viewport_css.1),
    )?;

    tab.scroll_state = painted.scroll_state.clone();
    tab
      .history
      .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

    // Keep UI updated about the effective scroll state, since painting may clamp/snap.
    let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: tab.scroll_state.clone(),
    });

    let _ = self.ui_tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: doc.device_pixel_ratio(),
        scroll_state: tab.scroll_state.clone(),
      },
    });

    if is_navigation {
      let _ = self.ui_tx.send(WorkerToUi::LoadingState {
        tab_id,
        loading: false,
      });
    }

    Ok(())
  }
}
