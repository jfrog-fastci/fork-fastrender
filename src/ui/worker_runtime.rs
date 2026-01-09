use crate::api::{FastRender, PreparedDocument, PreparedPaintOptions, RenderOptions};
use crate::dom::DomNode;
use crate::geometry::{Point, Size};
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::about_pages;
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  NavigationReason, PointerButton, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};

struct TabState {
  renderer: Option<FastRender>,
  viewport_css: (u32, u32),
  dpr: f32,

  // Navigation state.
  url: Option<String>,
  base_url: Option<String>,
  history: TabHistory,

  // Current document snapshot and cached layout.
  dom: Option<DomNode>,
  prepared: Option<PreparedDocument>,
  dirty: bool,

  // Viewport/interaction state.
  scroll: ScrollState,
  interaction: InteractionEngine,
}

impl TabState {
  fn new() -> Self {
    Self {
      renderer: None,
      viewport_css: (800, 600),
      dpr: 1.0,
      url: None,
      base_url: None,
      history: TabHistory::new(),
      dom: None,
      prepared: None,
      dirty: false,
      scroll: ScrollState::default(),
      interaction: InteractionEngine::new(),
    }
  }
}

/// Headless browser UI worker runtime.
///
/// This runtime is intentionally kept free of any GPU/windowing dependencies so it can be exercised
/// in unit/integration tests without a window system.
pub struct BrowserWorkerRuntime {
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
  pending: VecDeque<UiToWorker>,
}

struct CoalescedScroll {
  tab_id: TabId,
  delta_css: (f32, f32),
  pointer_css: Option<(f32, f32)>,
}

impl BrowserWorkerRuntime {
  pub fn new(ui_rx: Receiver<UiToWorker>, ui_tx: Sender<WorkerToUi>) -> Self {
    Self {
      ui_rx,
      ui_tx,
      tabs: HashMap::new(),
      active_tab: None,
      pending: VecDeque::new(),
    }
  }

  pub fn run(mut self) {
    while let Some(msg) = self.next_message() {
      self.handle_message(msg);
    }
  }

  fn next_message(&mut self) -> Option<UiToWorker> {
    if let Some(msg) = self.pending.pop_front() {
      return Some(msg);
    }
    self.ui_rx.recv().ok()
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        ..
      }
      | UiToWorker::NewTab { tab_id, initial_url } => {
        self.tabs.insert(tab_id, TabState::new());
        if self.active_tab.is_none() {
          self.active_tab = Some(tab_id);
        }
        if let Some(url) = initial_url {
          self.navigate(tab_id, url, NavigationReason::TypedUrl);
        }
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
      UiToWorker::GoBack { tab_id } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.go_back().cloned() else {
          return;
        };
        self.navigate(tab_id, entry.url, NavigationReason::BackForward);
      }
      UiToWorker::GoForward { tab_id } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.go_forward().cloned() else {
          return;
        };
        self.navigate(tab_id, entry.url, NavigationReason::BackForward);
      }
      UiToWorker::Reload { tab_id } => {
        let Some(tab) = self.tabs.get(&tab_id) else {
          return;
        };
        let Some(entry) = tab.history.reload_target() else {
          return;
        };
        self.navigate(tab_id, entry.url.clone(), NavigationReason::Reload);
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        self.navigate(tab_id, url, reason);
      }
      UiToWorker::Tick { .. } => {
        // Headless runtime has no JS event loop to drive; repaints are triggered immediately by
        // navigation/input messages.
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
        tab.dpr = if dpr.is_finite() && dpr > 0.0 {
          dpr
        } else {
          1.0
        };
        tab.dirty = true;
        self.render_current(tab_id, RepaintReason::ViewportChanged);
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let msg = self.coalesce_scroll(tab_id, delta_css, pointer_css);
        self.apply_scroll(msg.tab_id, msg.delta_css, msg.pointer_css);
        self.render_after_scroll_coalescing(msg.tab_id);
      }
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button: _,
      } => {
        self.pointer_move(tab_id, pos_css);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } => {
        if button == PointerButton::Back {
          let _ = self.handle_message(UiToWorker::GoBack { tab_id });
          return;
        }
        if button == PointerButton::Forward {
          let _ = self.handle_message(UiToWorker::GoForward { tab_id });
          return;
        }
        self.pointer_down(tab_id, pos_css);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } => {
        // Only treat primary button as a click for now.
        if button != PointerButton::Primary && button != PointerButton::None {
          return;
        }
        self.handle_pointer_up(tab_id, pos_css);
      }
      UiToWorker::SelectDropdownChoose {
        tab_id,
        select_node_id,
        option_node_id,
      } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        let Some(dom) = tab.dom.as_mut() else {
          return;
        };
        if crate::interaction::dom_mutation::activate_select_option(
          dom,
          select_node_id,
          option_node_id,
          false,
        ) {
          tab.dirty = true;
          self.render_current(tab_id, RepaintReason::Input);
        }
      }
      UiToWorker::TextInput { tab_id, text } => {
        self.text_input(tab_id, &text);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        self.key_action(tab_id, key);
      }
      UiToWorker::RequestRepaint { tab_id, reason } => {
        self.render_current(tab_id, reason);
      }
    }
  }

  fn supported_navigation(url: &str) -> bool {
    let trimmed = url.trim();
    if about_pages::is_about_url(trimmed) {
      return true;
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
      return false;
    };
    matches!(
      parsed.scheme().to_ascii_lowercase().as_str(),
      "http" | "https" | "file"
    )
  }

  fn navigation_options(tab: &TabState) -> RenderOptions {
    let mut opts = RenderOptions::new()
      .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
      .with_device_pixel_ratio(tab.dpr)
      .with_scroll(tab.scroll.viewport.x, tab.scroll.viewport.y)
      .with_element_scroll_offsets(tab.scroll.elements.clone());
    // The UI worker wants deterministic behavior; avoid time-based animation sampling by default.
    opts.animation_time = None;
    opts
  }

  fn navigate(&mut self, tab_id: TabId, url: String, reason: NavigationReason) {
    let url = url.trim().to_string();

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
      tab_id,
      url: url.clone(),
    });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });

    if !Self::supported_navigation(&url) {
      let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url: url.clone(),
        error: "unsupported URL scheme".to_string(),
      });
      let _ = self.ui_tx.send(WorkerToUi::LoadingState {
        tab_id,
        loading: false,
      });
      return;
    }

    // Reset per-document state.
    //
    // We intentionally preserve the *viewport* scroll offset across reload/back/forward navigations
    // (matching the behaviour of the full browser worker and headless `UiWorker`). We restore the
    // scroll state from `TabHistory` so we don't carry element scroll offsets keyed by box ids
    // across document reloads.
    tab.interaction = InteractionEngine::new();
    tab.scroll = match reason {
      NavigationReason::TypedUrl | NavigationReason::LinkClick => ScrollState::default(),
      NavigationReason::BackForward | NavigationReason::Reload => tab
        .history
        .current()
        .map(|entry| ScrollState::with_viewport(Point::new(entry.scroll_x, entry.scroll_y)))
        .unwrap_or_default(),
    };

    if tab.renderer.is_none() {
      match FastRender::new() {
        Ok(renderer) => tab.renderer = Some(renderer),
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url: url.clone(),
            error: format!("failed to create renderer: {err}"),
          });
          let _ = self.ui_tx.send(WorkerToUi::LoadingState {
            tab_id,
            loading: false,
          });
          return;
        }
      }
    }
    let opts = Self::navigation_options(tab);
    let Some(renderer) = tab.renderer.as_mut() else {
      let _ = self.ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: "renderer unavailable after initialization".to_string(),
      });
      let _ = self.ui_tx.send(WorkerToUi::LoadingState {
        tab_id,
        loading: false,
      });
      return;
    };

    let prepared = if about_pages::is_about_url(&url) {
      let html = about_pages::html_for_about_url(&url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
      });
      renderer.set_base_url(about_pages::ABOUT_BASE_URL);
      match renderer.parse_html(&html) {
        Ok(dom) => renderer.prepare_dom_with_options(dom, Some(&url), opts.clone()),
        Err(err) => Err(err),
      }
    } else {
      renderer.prepare_url(&url, opts.clone())
    };

    let report = match prepared {
      Ok(report) => report,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: url.clone(),
          error: err.to_string(),
        });
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return;
      }
    };

    let final_url = report.final_url.clone().unwrap_or_else(|| url.clone());

    match reason {
      NavigationReason::TypedUrl | NavigationReason::LinkClick => {
        tab.history.push(final_url.clone());
      }
      NavigationReason::BackForward | NavigationReason::Reload => {
        tab.history.commit_navigation(&url, Some(&final_url));
      }
    }

    tab.url = Some(final_url.clone());
    tab.base_url = report.base_url.clone().or_else(|| tab.url.clone());
    tab.dom = Some(report.document.dom().clone());
    tab.prepared = Some(report.document);
    tab.dirty = false;

    let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
      tab_id,
      url: final_url,
      title: None,
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });

    self.render_current(tab_id, RepaintReason::Navigation);

    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });
  }

  fn coalesce_scroll(
    &mut self,
    tab_id: TabId,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  ) -> CoalescedScroll {
    // Coalesce scroll bursts to avoid spamming intermediate paints/frames.
    //
    // Using a small timeout here makes coalescing more robust against scheduler races where the
    // worker thread wakes up and starts processing the first scroll message before the UI thread
    // manages to enqueue the rest of a rapid scroll sequence.
    const COALESCE_WAIT: std::time::Duration = std::time::Duration::from_millis(1);

    let mut total_dx = delta_css.0;
    let mut total_dy = delta_css.1;
    let mut last_pointer = pointer_css;

    loop {
      match self.ui_rx.recv_timeout(COALESCE_WAIT) {
        Ok(UiToWorker::Scroll {
          tab_id: next_id,
          delta_css: (dx, dy),
          pointer_css,
        }) if next_id == tab_id => {
          total_dx += dx;
          total_dy += dy;
          last_pointer = pointer_css;
        }
        Ok(other) => {
          self.pending.push_back(other);
          break;
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }

    CoalescedScroll {
      tab_id,
      delta_css: (total_dx, total_dy),
      pointer_css: last_pointer,
    }
  }

  fn apply_scroll(
    &mut self,
    tab_id: TabId,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    let dx = if delta_css.0.is_finite() {
      delta_css.0
    } else {
      0.0
    };
    let dy = if delta_css.1.is_finite() {
      delta_css.1
    } else {
      0.0
    };

    // Scroll wheel targeting is currently simplified: apply element scrolling first when a pointer
    // is provided, otherwise update viewport scroll.
    if let (Some(pointer_css), Some(prepared)) = (pointer_css, tab.prepared.as_ref()) {
      let page_point = Point::new(
        pointer_css.0 + tab.scroll.viewport.x,
        pointer_css.1 + tab.scroll.viewport.y,
      );
      tab.scroll = crate::interaction::scroll_wheel::apply_wheel_scroll_at_point(
        prepared.fragment_tree(),
        &tab.scroll,
        Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32),
        page_point,
        crate::interaction::scroll_wheel::ScrollWheelInput {
          delta_x: dx,
          delta_y: dy,
        },
      );
    } else {
      tab.scroll.viewport.x += dx;
      tab.scroll.viewport.y += dy;
    }
  }

  fn render_after_scroll_coalescing(&mut self, tab_id: TabId) {
    loop {
      let Some(frame) = self.paint_current(tab_id) else {
        return;
      };

      match self.ui_rx.try_recv() {
        Ok(UiToWorker::Scroll {
          tab_id: next_id,
          delta_css,
          pointer_css,
        }) if next_id == tab_id => {
          let msg = self.coalesce_scroll(next_id, delta_css, pointer_css);
          self.apply_scroll(msg.tab_id, msg.delta_css, msg.pointer_css);
          continue;
        }
        Ok(other) => {
          self.pending.push_back(other);
          self.emit_frame(tab_id, frame);
          break;
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
          self.emit_frame(tab_id, frame);
          break;
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
          self.emit_frame(tab_id, frame);
          break;
        }
      }
    }
  }

  fn pointer_down(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let (Some(dom), Some(prepared)) = (tab.dom.as_mut(), tab.prepared.as_ref()) else {
      return;
    };
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let changed = tab.interaction.pointer_down(
      dom,
      prepared.box_tree(),
      prepared.fragment_tree(),
      &tab.scroll,
      viewport_point,
    );
    if changed {
      tab.dirty = true;
    }
  }

  fn pointer_move(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let (Some(dom), Some(prepared)) = (tab.dom.as_mut(), tab.prepared.as_ref()) else {
      return;
    };
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let changed = tab.interaction.pointer_move(
      dom,
      prepared.box_tree(),
      prepared.fragment_tree(),
      &tab.scroll,
      viewport_point,
    );
    if changed {
      tab.dirty = true;
      self.render_current(tab_id, RepaintReason::Input);
    }
  }

  fn handle_pointer_up(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let document_url = tab.url.as_deref().unwrap_or(about_pages::ABOUT_BASE_URL);
    let base_url = tab
      .base_url
      .as_deref()
      .or(tab.url.as_deref())
      .unwrap_or(about_pages::ABOUT_BASE_URL);
    let document_url = tab.url.as_deref().unwrap_or("");
    let (Some(dom), Some(prepared)) = (tab.dom.as_mut(), tab.prepared.as_ref()) else {
      return;
    };
    let viewport_point = Point::new(pos_css.0, pos_css.1);

    let (dom_changed, action) = tab.interaction.pointer_up_with_scroll(
      dom,
      prepared.box_tree(),
      prepared.fragment_tree(),
      &tab.scroll,
      viewport_point,
      document_url,
      base_url,
    );
    if dom_changed {
      tab.dirty = true;
    }

    match action {
      InteractionAction::Navigate { href } => {
        // Navigation replaces the document, so any DOM changes (e.g. visited flag) are not painted
        // for the previous page.
        self.navigate(tab_id, href, NavigationReason::LinkClick);
      }
      InteractionAction::OpenSelectDropdown { .. } => {
        // The headless runtime does not currently surface select dropdown UI. Still trigger a paint
        // if DOM mutations occurred (e.g. focus changes).
        if tab.dirty {
          self.render_current(tab_id, RepaintReason::Input);
        }
      }
      InteractionAction::FocusChanged { .. } | InteractionAction::None => {
        if tab.dirty {
          self.render_current(tab_id, RepaintReason::Input);
        }
      }
    }
  }

  fn text_input(&mut self, tab_id: TabId, text: &str) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(dom) = tab.dom.as_mut() else {
      return;
    };
    if tab.interaction.text_input(dom, text) {
      tab.dirty = true;
      self.render_current(tab_id, RepaintReason::Input);
    }
  }

  fn key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(dom) = tab.dom.as_mut() else {
      return;
    };
    let document_url = tab.url.as_deref().unwrap_or(about_pages::ABOUT_BASE_URL);
    let base_url = tab
      .base_url
      .as_deref()
      .or(tab.url.as_deref())
      .unwrap_or(about_pages::ABOUT_BASE_URL);
    let box_tree = tab.prepared.as_ref().map(|prepared| prepared.box_tree());

    let (dom_changed, action) = tab.interaction.key_activate_with_box_tree(
      dom,
      box_tree,
      key,
      document_url,
      base_url,
    );
    if dom_changed {
      tab.dirty = true;
    }

    match action {
      InteractionAction::Navigate { href } => {
        // Navigation replaces the document, so any DOM changes (e.g. visited flag) are not painted
        // for the previous page.
        self.navigate(tab_id, href, NavigationReason::LinkClick);
      }
      InteractionAction::OpenSelectDropdown { .. } => {
        // The headless runtime does not currently surface select dropdown UI. Still trigger a paint
        // if DOM mutations occurred (e.g. focus changes).
        if tab.dirty {
          self.render_current(tab_id, RepaintReason::Input);
        }
      }
      InteractionAction::FocusChanged { .. } | InteractionAction::None => {
        if tab.dirty {
          self.render_current(tab_id, RepaintReason::Input);
        }
      }
    }
  }

  fn render_current(&mut self, tab_id: TabId, _reason: RepaintReason) {
    let Some(frame) = self.paint_current(tab_id) else {
      return;
    };
    self.emit_frame(tab_id, frame);
  }

  fn emit_frame(&mut self, tab_id: TabId, frame: crate::ui::messages::RenderedFrame) {
    let scroll = frame.scroll_state.clone();
    let _ = self.ui_tx.send(WorkerToUi::FrameReady { tab_id, frame });
    let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated { tab_id, scroll });
  }

  fn paint_current(&mut self, tab_id: TabId) -> Option<crate::ui::messages::RenderedFrame> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    // Ensure we have a prepared layout. Keep the previous `PreparedDocument` around for hit
    // testing even when dirty; only replace it after we successfully re-prepare.
    if tab.prepared.is_none() || tab.dirty {
      if tab.renderer.is_none() {
        match FastRender::new() {
          Ok(renderer) => tab.renderer = Some(renderer),
          Err(err) => {
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("failed to create renderer: {err}"),
            });
            return None;
          }
        }
      }
      let Some(dom) = tab.dom.as_ref() else {
        return None;
      };
      let opts = Self::navigation_options(tab);
      let Some(renderer) = tab.renderer.as_mut() else {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: "renderer unavailable after initialization".to_string(),
        });
        return None;
      };
      match renderer.prepare_dom_with_options(dom.clone(), tab.url.as_deref(), opts) {
        Ok(report) => {
          tab.prepared = Some(report.document);
          tab.base_url = report.base_url.or_else(|| tab.base_url.clone());
          tab.dirty = false;
        }
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("render prepare failed: {err}"),
          });
          return None;
        }
      }
    }

    let Some(prepared) = tab.prepared.as_ref() else {
      return None;
    };

    let painted = match prepared.paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(tab.scroll.clone()),
      viewport: Some(tab.viewport_css),
      background: None,
      animation_time: None,
    }) {
      Ok(frame) => frame,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("paint failed: {err}"),
        });
        return None;
      }
    };

    tab.scroll = painted.scroll_state.clone();
    tab
      .history
      .update_scroll(tab.scroll.viewport.x, tab.scroll.viewport.y);

    Some(crate::ui::messages::RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state: tab.scroll.clone(),
    })
  }
}

/// Spawn the headless browser worker runtime on a large-stack thread.
pub fn spawn_browser_worker_runtime_thread(
  name: impl Into<String>,
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || {
      let runtime = BrowserWorkerRuntime::new(ui_rx, ui_tx);
      runtime.run();
    })
}
