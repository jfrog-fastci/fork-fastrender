use crate::api::{BrowserDocument, FastRender, FastRenderFactory, RenderDiagnostics};
use crate::geometry::{Point, Size};
use crate::html::title::find_document_title;
use crate::interaction::scroll_offset_for_fragment_target;
use crate::js::{EventLoop, RunLimits, RunUntilIdleOutcome};
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::about_pages;
use crate::ui::history::TabHistory;
use crate::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use crate::{Error, PreparedDocument, PreparedPaintOptions, RenderOptions, Result};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

const NAVIGATION_RUN_LIMITS: RunLimits = RunLimits {
  max_tasks: 10_000,
  max_microtasks: 50_000,
  // Navigations may legitimately do some synchronous work (parser tasks, initial microtasks). Keep
  // this bounded so hostile pages can't hang the UI.
  max_wall_time: Some(Duration::from_millis(50)),
};

const TICK_RUN_LIMITS: RunLimits = RunLimits {
  max_tasks: 256,
  max_microtasks: 2048,
  max_wall_time: Some(Duration::from_millis(5)),
};

struct BrowserTabHost {
  document: BrowserDocument,
}

struct BrowserTab {
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
  options: RenderOptions,
}

impl BrowserTab {
  fn new(document: BrowserDocument, options: RenderOptions) -> Self {
    Self {
      host: BrowserTabHost { document },
      event_loop: EventLoop::new(),
      options,
    }
  }

  fn set_viewport(&mut self, viewport_css: (u32, u32), dpr: f32) {
    self.options.viewport = Some(viewport_css);
    self.options.device_pixel_ratio = Some(dpr);
    self.host
      .document
      .set_viewport(viewport_css.0, viewport_css.1);
    self.host.document.set_device_pixel_ratio(dpr);
  }

  fn sync_scroll_options(&mut self, scroll_state: &ScrollState) {
    self.options.scroll_x = scroll_state.viewport.x;
    self.options.scroll_y = scroll_state.viewport.y;
    self.options.element_scroll_offsets = scroll_state.elements.clone();
  }

  fn viewport_css(&self) -> (u32, u32) {
    if let Some(viewport) = self.options.viewport {
      return viewport;
    }

    let Some(prepared) = self.host.document.prepared() else {
      return (0, 0);
    };
    let size = prepared.layout_viewport();
    (size.width.round() as u32, size.height.round() as u32)
  }

  fn dpr(&self) -> f32 {
    self
      .host
      .document
      .prepared()
      .map(|doc| doc.device_pixel_ratio())
      .or(self.options.device_pixel_ratio)
      .unwrap_or(1.0)
  }

  fn render_frame(&mut self) -> Result<RenderedFrame> {
    let painted = self.host.document.render_frame_with_scroll_state()?;
    self.sync_scroll_options(&painted.scroll_state);
    Ok(RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: self.viewport_css(),
      dpr: self.dpr(),
      scroll_state: painted.scroll_state,
    })
  }

  fn render_if_needed(&mut self) -> Result<Option<RenderedFrame>> {
    let Some(painted) = self.host.document.render_if_needed_with_scroll_state()? else {
      return Ok(None);
    };
    self.sync_scroll_options(&painted.scroll_state);
    Ok(Some(RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: self.viewport_css(),
      dpr: self.dpr(),
      scroll_state: painted.scroll_state,
    }))
  }
}

struct LoadingStateGuard {
  tab_id: TabId,
  tx: Sender<WorkerToUi>,
  armed: bool,
}

impl LoadingStateGuard {
  fn new(tab_id: TabId, tx: Sender<WorkerToUi>) -> Self {
    Self {
      tab_id,
      tx,
      armed: true,
    }
  }

  fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for LoadingStateGuard {
  fn drop(&mut self) {
    if self.armed {
      let _ = self.tx.send(WorkerToUi::LoadingState {
        tab_id: self.tab_id,
        loading: false,
      });
    }
  }
}

pub struct BrowserWorker {
  factory: FastRenderFactory,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, BrowserTab>,
}

impl BrowserWorker {
  pub fn new(factory: FastRenderFactory, ui_tx: Sender<WorkerToUi>) -> Self {
    Self {
      factory,
      ui_tx,
      tabs: HashMap::new(),
    }
  }

  pub fn has_tab(&self, tab_id: TabId) -> bool {
    self.tabs.contains_key(&tab_id)
  }

  pub fn close_tab(&mut self, tab_id: TabId) {
    self.tabs.remove(&tab_id);
  }

  /// Navigate, execute the initial scripting/event-loop slice, and synchronously render a frame.
  ///
  /// On navigation errors, the worker tries to render `about:error` with the error message.
  pub fn navigate(&mut self, tab_id: TabId, url: &str, options: RenderOptions) -> Result<()> {
    let url = url.trim();
    let url_string = url.to_string();
    let fragment_target = Url::parse(url)
      .ok()
      .and_then(|parsed| {
        parsed
          .fragment()
          .filter(|frag| !frag.is_empty())
          .map(str::to_string)
      });
 
    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
      tab_id,
      url: url_string.clone(),
    });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });
    let mut loading_guard = LoadingStateGuard::new(tab_id, self.ui_tx.clone());

    // Forward render pipeline stage heartbeats to the UI for this navigation+paint.
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());

    let (mut tab, mut navigation_failure, navigation_diagnostics) =
      match self.create_tab_for_url(url, options.clone()) {
        Ok(parts) => parts,
        Err(err) => {
          let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url: url_string.clone(),
            error: err.to_string(),
            can_go_back: false,
            can_go_forward: false,
          });
          return Err(err);
        }
      };

    // Best-effort: surface JS errors/console output in the UI debug log so pages can be debugged
    // without attaching a debugger.
    if let Some(diagnostics) = navigation_diagnostics.as_ref() {
      for exception in &diagnostics.js_exceptions {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("JS exception: {}", exception.message),
        });
        if let Some(stack) = &exception.stack {
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("  stack: {stack}"),
          });
        }
      }
      for message in &diagnostics.console_messages {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!(
            "Console[{}]: {}",
            message.level.as_str(),
            message.message
          ),
        });
      }
    }

    // Drive the tab's event loop for a bounded slice so initial scripts/microtasks can run before
    // the first paint.
    match tab
      .event_loop
      .run_until_idle(&mut tab.host, NAVIGATION_RUN_LIMITS)?
    {
      RunUntilIdleOutcome::Idle => {}
      RunUntilIdleOutcome::Stopped(reason) => {
        let message = format!("JS event loop exceeded navigation budget: {reason:?}");
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: message.clone(),
        });
        navigation_failure = Some(message.clone());
        tab = self.create_error_tab(&message, options.clone())?;
      }
    }

    // Align to the fragment target before the first frame is rendered (mirrors the existing
    // browser_ui navigation semantics).
    if navigation_failure.is_none() {
      if let Some(fragment) = fragment_target.as_deref() {
        if let Some(prepared) = tab.host.document.prepared() {
          let viewport_css = tab.viewport_css();
          let viewport_size_css = Size::new(viewport_css.0 as f32, viewport_css.1 as f32);
          let scroll = scroll_offset_for_fragment_target(
            prepared.dom(),
            prepared.box_tree(),
            prepared.fragment_tree(),
            fragment,
            viewport_size_css,
          );
          if let Some(scroll) = scroll {
            let mut scroll_state = tab.host.document.scroll_state();
            scroll_state.viewport = scroll;
            tab.host.document.set_scroll_state(scroll_state);
          }
        }
      }
    }

    let committed_url = tab
      .host
      .document
      .document_url()
      .map(str::to_string)
      .unwrap_or_else(|| url_string.clone());
    let title = find_document_title(tab.host.document.dom());

    let frame = match tab.render_frame() {
      Ok(frame) => frame,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: url_string,
          error: err.to_string(),
          can_go_back: false,
          can_go_forward: false,
        });
        return Err(err);
      }
    };
    self.tabs.insert(tab_id, tab);

    let _ = self.ui_tx.send(WorkerToUi::FrameReady { tab_id, frame });

    match navigation_failure {
      Some(error) => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: url_string,
          error,
          can_go_back: false,
          can_go_forward: false,
        });
      }
      None => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
          tab_id,
          url: committed_url,
          title,
          can_go_back: false,
          can_go_forward: false,
        });
      }
    }

    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });
    loading_guard.disarm();

    Ok(())
  }

  /// Run one bounded event loop slice and, if the tab is dirty, repaint.
  pub fn tick(&mut self, tab_id: TabId) -> Result<()> {
    // Temporarily take ownership of the tab so we can replace it on failures without running into
    // borrow-checker issues (creating the error tab needs to borrow `self` for the factory).
    let Some(mut tab) = self.tabs.remove(&tab_id) else {
      return Ok(());
    };

    // Install stage forwarding only for the duration of this tick. The stage listener is global,
    // but the browser UI worker is single-threaded so we can safely swap it per tick.
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());

    match tab.event_loop.run_until_idle(&mut tab.host, TICK_RUN_LIMITS)? {
      RunUntilIdleOutcome::Idle => {}
      RunUntilIdleOutcome::Stopped(reason) => {
        // Replace the current tab contents with a deterministic error page so the UI doesn't hang.
        let message = format!("JS event loop exceeded tick budget: {reason:?}");
        let options = tab.options.clone();
        tab = self.create_error_tab(&message, options)?;

        // Render the error page immediately.
        let frame = tab.render_frame()?;
        let _ = self
          .ui_tx
          .send(WorkerToUi::FrameReady { tab_id, frame });
        self.tabs.insert(tab_id, tab);
        return Ok(());
      }
    }

    let Some(frame) = tab.render_if_needed()? else {
      self.tabs.insert(tab_id, tab);
      return Ok(());
    };
    let _ = self
      .ui_tx
      .send(WorkerToUi::FrameReady { tab_id, frame });
    self.tabs.insert(tab_id, tab);
    Ok(())
  }

  /// Update the tab viewport (CSS px + device pixel ratio), marking it dirty for relayout.
  pub fn viewport_changed(&mut self, tab_id: TabId, viewport_css: (u32, u32), dpr: f32) {
    if let Some(tab) = self.tabs.get_mut(&tab_id) {
      tab.set_viewport(viewport_css, dpr);
    }
  }

  /// Apply a scroll delta and repaint (best-effort).
  pub fn scroll(
    &mut self,
    tab_id: TabId,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  ) -> Result<()> {
    let Some(mut tab) = self.tabs.remove(&tab_id) else {
      return Ok(());
    };

    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());

    // Try element scrolling first when the UI provides a pointer location.
    let mut element_scrolled = false;
    if let Some((x, y)) = pointer_css {
      element_scrolled = tab
        .host
        .document
        .wheel_scroll_at_viewport_point(Point::new(x, y), delta_css)
        .unwrap_or(false);
    }

    // Fall back to viewport scrolling when no element consumed the scroll event.
    if !element_scrolled {
      let mut scroll_state = tab.host.document.scroll_state();
      scroll_state.viewport.x = (scroll_state.viewport.x + delta_css.0).max(0.0);
      scroll_state.viewport.y = (scroll_state.viewport.y + delta_css.1).max(0.0);
      tab.host.document.set_scroll_state(scroll_state);
    }

    if let Some(frame) = tab.render_if_needed()? {
      let _ = self
        .ui_tx
        .send(WorkerToUi::FrameReady { tab_id, frame });
    }
    self.tabs.insert(tab_id, tab);
    Ok(())
  }

  /// Force a repaint even when no dirty flags are set.
  pub fn request_repaint(&mut self, tab_id: TabId) -> Result<()> {
    let Some(mut tab) = self.tabs.remove(&tab_id) else {
      return Ok(());
    };

    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    let frame = tab.render_frame()?;
    let _ = self
      .ui_tx
      .send(WorkerToUi::FrameReady { tab_id, frame });
    self.tabs.insert(tab_id, tab);
    Ok(())
  }

  /// Test/embedding hook: schedule a timer that mutates the DOM and triggers a repaint.
  ///
  /// This models the "JS schedules work → DOM mutates → tab becomes dirty → next tick repaints"
  /// lifecycle without requiring full JS bindings in the UI worker tests.
  pub fn schedule_dom_mutation_timeout<F>(
    &mut self,
    tab_id: TabId,
    delay: Duration,
    f: F,
  ) -> Result<()>
  where
    F: FnOnce(&mut crate::dom::DomNode) -> bool + 'static,
  {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return Err(Error::Other(format!("unknown tab_id: {tab_id:?}")));
    };

    tab
      .event_loop
      .set_timeout(delay, move |host, _event_loop| {
        let _changed = host.document.mutate_dom(f);
        Ok(())
      })?;
    Ok(())
  }

  fn create_tab_for_url(
    &self,
    url: &str,
    options: RenderOptions,
  ) -> Result<(BrowserTab, Option<String>, Option<RenderDiagnostics>)> {
    let url = url.trim();

    if about_pages::is_about_url(url) {
      let tab = self.create_about_tab(url, options)?;
      return Ok((tab, None, None));
    }

    let mut renderer = self.factory.build_renderer()?;
    let report = match renderer.prepare_url(url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        let html = about_pages::error_page_html("Navigation failed", &err.to_string());
        let tab = self.create_about_html_tab(about_pages::ABOUT_ERROR, &html, options)?;
        return Ok((tab, Some(err.to_string()), None));
      }
    };

    let doc = BrowserDocument::from_prepared(renderer, report.document, options.clone())?;
    Ok((BrowserTab::new(doc, options), None, Some(report.diagnostics)))
  }

  fn create_about_tab(&self, url: &str, options: RenderOptions) -> Result<BrowserTab> {
    let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
      about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
    });
    self.create_about_html_tab(url, &html, options)
  }

  fn create_error_tab(&self, message: &str, options: RenderOptions) -> Result<BrowserTab> {
    let html = about_pages::error_page_html("JavaScript error", message);
    self.create_about_html_tab(about_pages::ABOUT_ERROR, &html, options)
  }

  fn create_about_html_tab(
    &self,
    document_url: &str,
    html: &str,
    options: RenderOptions,
  ) -> Result<BrowserTab> {
    let mut renderer = self.factory.build_renderer()?;
    renderer.set_base_url(about_pages::ABOUT_BASE_URL);
    let mut doc = BrowserDocument::new(renderer, html, options.clone())?;
    doc.set_document_url(Some(document_url.to_string()));
    Ok(BrowserTab::new(doc, options))
  }
}

struct WorkerTabState {
  history: TabHistory,
  prepared: Option<PreparedDocument>,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  loading: bool,
}

impl WorkerTabState {
  fn new() -> Self {
    Self {
      history: TabHistory::new(),
      prepared: None,
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      loading: false,
    }
  }
}

/// Message-driven worker used by the `browser` UI.
///
/// This worker owns navigation state (including per-tab history) so the UI doesn't need to guess
/// committed URLs (e.g. after redirects).
pub struct BrowserUiWorker {
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
  tabs: HashMap<TabId, WorkerTabState>,
  active_tab: Option<TabId>,
}

impl BrowserUiWorker {
  pub fn new(renderer: FastRender, ui_tx: Sender<WorkerToUi>) -> Self {
    Self {
      renderer,
      ui_tx,
      tabs: HashMap::new(),
      active_tab: None,
    }
  }

  pub fn run(&mut self, rx: Receiver<UiToWorker>) {
    while let Ok(msg) = rx.recv() {
      self.handle_message(msg);
    }
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        ..
      }
      | UiToWorker::NewTab {
        tab_id,
        initial_url,
      } => {
        self.tabs.insert(tab_id, WorkerTabState::new());
        if self.active_tab.is_none() {
          self.active_tab = Some(tab_id);
        }
        if let Some(url) = initial_url {
          self.navigate(tab_id, url, NavigationReason::TypedUrl, true, None);
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
          self.active_tab = None;
        }
      }
      UiToWorker::SetActiveTab { tab_id } => {
        self.active_tab = Some(tab_id);
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        let push = matches!(
          reason,
          NavigationReason::TypedUrl | NavigationReason::LinkClick
        );
        self.navigate(tab_id, url, reason, push, None);
      }
      UiToWorker::GoBack { tab_id } => {
        let Some((url, scroll)) = (|| {
          let tab = self.tabs.get_mut(&tab_id)?;
          let entry = tab.history.go_back()?;
          Some((entry.url.clone(), (entry.scroll_x, entry.scroll_y)))
        })() else {
          return;
        };
        self.navigate(
          tab_id,
          url,
          NavigationReason::BackForward,
          false,
          Some(scroll),
        );
      }
      UiToWorker::GoForward { tab_id } => {
        let Some((url, scroll)) = (|| {
          let tab = self.tabs.get_mut(&tab_id)?;
          let entry = tab.history.go_forward()?;
          Some((entry.url.clone(), (entry.scroll_x, entry.scroll_y)))
        })() else {
          return;
        };
        self.navigate(
          tab_id,
          url,
          NavigationReason::BackForward,
          false,
          Some(scroll),
        );
      }
      UiToWorker::Reload { tab_id } => {
        let Some((url, scroll)) = (|| {
          let tab = self.tabs.get_mut(&tab_id)?;
          let entry = tab.history.reload_target()?;
          Some((entry.url.clone(), (entry.scroll_x, entry.scroll_y)))
        })() else {
          return;
        };
        self.navigate(tab_id, url, NavigationReason::Reload, false, Some(scroll));
      }
      UiToWorker::Tick { .. } => {
        // This legacy worker implementation renders on-demand (navigation/scroll/explicit repaint)
        // and does not currently drive a JS event loop.
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        enum Action {
          Reload { url: String, scroll: (f32, f32) },
          Repaint { viewport: (u32, u32) },
          None,
        }

        let action = (|| {
          let tab = self.tabs.get_mut(&tab_id)?;
          let dpr_changed = (tab.dpr - dpr).abs() > f32::EPSILON;
          tab.viewport_css = viewport_css;
          tab.dpr = dpr;

          if dpr_changed {
            let entry = tab.history.current()?;
            Some(Action::Reload {
              url: entry.url.clone(),
              scroll: (entry.scroll_x, entry.scroll_y),
            })
          } else {
            Some(Action::Repaint {
              viewport: viewport_css,
            })
          }
        })()
        .unwrap_or(Action::None);

        match action {
          Action::Reload { url, scroll } => {
            self.navigate(tab_id, url, NavigationReason::Reload, false, Some(scroll));
          }
          Action::Repaint { viewport } => {
            self.repaint(tab_id, Some(viewport));
          }
          Action::None => {}
        }
      }
      UiToWorker::Scroll {
        tab_id, delta_css, ..
      } => {
        self.scroll(tab_id, delta_css);
      }
      UiToWorker::RequestRepaint { tab_id, .. } => {
        self.repaint(tab_id, None);
      }
      // Input events are currently ignored by this worker (handled by future interaction layers).
      UiToWorker::PointerMove { .. }
      | UiToWorker::PointerDown { .. }
      | UiToWorker::PointerUp { .. }
      | UiToWorker::SelectDropdownChoose { .. }
      | UiToWorker::SelectDropdownPick { .. }
      | UiToWorker::TextInput { .. }
      | UiToWorker::KeyAction { .. } => {}
    }
  }

  fn navigate(
    &mut self,
    tab_id: TabId,
    url: String,
    _reason: NavigationReason,
    push_history: bool,
    scroll_override: Option<(f32, f32)>,
  ) {
    let url = url.trim().to_string();
    if url.is_empty() {
      return;
    }

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    if push_history {
      tab.history.push(url.clone());
      tab.scroll_state = ScrollState::default();
    }

    if let Some((x, y)) = scroll_override {
      tab.scroll_state = ScrollState::with_viewport(Point::new(x, y));
    }

    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    tab.loading = true;
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });
    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
      tab_id,
      url: url.clone(),
    });

    let options = RenderOptions::new()
      .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
      .with_device_pixel_ratio(tab.dpr)
      .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

    let report_result = if about_pages::is_about_url(&url) {
      prepare_about_url(&mut self.renderer, &url, options.clone())
    } else {
      self.renderer.prepare_url(&url, options.clone())
    };

    let (prepared, committed_url, title, failed) = match report_result {
      Ok(report) => {
        let title = find_document_title(report.document.dom());
        (
          report.document,
          report.final_url.clone().unwrap_or_else(|| url.clone()),
          title,
          false,
        )
      }
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: url.clone(),
          error: err.to_string(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        let html = about_pages::error_page_html("Navigation failed", &err.to_string());
        let report = match prepare_about_html(
          &mut self.renderer,
          about_pages::ABOUT_ERROR,
          &html,
          options.clone(),
        ) {
          Ok(report) => report,
          Err(err) => {
            tab.loading = false;
            let _ = self.ui_tx.send(WorkerToUi::LoadingState {
              tab_id,
              loading: false,
            });
            let _ = self.ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("failed to render error page: {err}"),
            });
            return;
          }
        };
        let title = find_document_title(report.document.dom());
        (report.document, url.clone(), title, true)
      }
    };

    let paint_opts = PreparedPaintOptions {
      scroll: None,
      viewport: None,
      background: None,
      animation_time: options.animation_time,
    };

    let dpr = prepared.device_pixel_ratio();
    let painted = match prepared.paint_with_options_frame(paint_opts) {
      Ok(frame) => frame,
      Err(err) => {
        tab.loading = false;
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("paint failed: {err}"),
        });
        return;
      }
    };

    tab.prepared = Some(prepared);

    tab.scroll_state = painted.scroll_state.clone();
    tab.history.update_scroll(
      painted.scroll_state.viewport.x,
      painted.scroll_state.viewport.y,
    );

    if let Some(title) = title.clone() {
      tab.history.set_title(title.clone());
    }

    let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: painted.scroll_state.clone(),
    });

    let _ = self.ui_tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr,
        scroll_state: painted.scroll_state,
      },
    });

    // Only update the history entry URL for successful navigations (redirect handling).
    if !failed {
      let _ = tab
        .history
        .commit_navigation(&url, Some(committed_url.as_str()));
    }

    tab.loading = false;
    let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
      tab_id,
      url: committed_url,
      title,
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });
    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: false,
    });
  }

  fn repaint(&mut self, tab_id: TabId, viewport_override: Option<(u32, u32)>) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(prepared) = tab.prepared.as_ref() else {
      return;
    };

    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    let viewport = viewport_override.or(Some(tab.viewport_css));
    let opts = PreparedPaintOptions {
      scroll: Some(tab.scroll_state.clone()),
      viewport,
      background: None,
      animation_time: None,
    };
    let painted = match prepared.paint_with_options_frame(opts) {
      Ok(frame) => frame,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("repaint failed: {err}"),
        });
        return;
      }
    };

    tab.scroll_state = painted.scroll_state.clone();
    tab.history.update_scroll(
      painted.scroll_state.viewport.x,
      painted.scroll_state.viewport.y,
    );
    let _ = self.ui_tx.send(WorkerToUi::ScrollStateUpdated {
      tab_id,
      scroll: painted.scroll_state.clone(),
    });
    let _ = self.ui_tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: prepared.device_pixel_ratio(),
        scroll_state: painted.scroll_state,
      },
    });
  }

  fn scroll(&mut self, tab_id: TabId, delta_css: (f32, f32)) {
    let can_scroll = (|| {
      let tab = self.tabs.get_mut(&tab_id)?;
      if tab.prepared.is_none() {
        return Some(false);
      }
      let mut next = tab.scroll_state.clone();
      next.viewport.x = (next.viewport.x + delta_css.0).max(0.0);
      next.viewport.y = (next.viewport.y + delta_css.1).max(0.0);
      tab.scroll_state = next;
      Some(true)
    })()
    .unwrap_or(false);
    if !can_scroll {
      return;
    }
    self.repaint(tab_id, None);
  }
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

/// Spawn a `BrowserUiWorker` on a dedicated large-stack thread.
pub fn spawn_browser_ui_worker_thread(
  name: impl Into<String>,
  renderer: FastRender,
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || {
      let mut worker = BrowserUiWorker::new(renderer, ui_tx);
      worker.run(ui_rx);
    })
}

#[cfg(test)]
mod tests {
  use super::BrowserWorker;
  use crate::render_control::StageHeartbeat;
  use crate::ui::messages::{TabId, WorkerToUi};
  use crate::api::FastRenderFactory;
  use crate::RenderOptions;
  use std::time::Duration;

  #[test]
  fn about_blank_navigation_does_not_fetch_document() {
    let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
    let factory = FastRenderFactory::new().unwrap();
    let mut worker = BrowserWorker::new(factory, tx);

    worker
      .navigate(
        TabId(1),
        "about:blank",
        RenderOptions::default().with_viewport(32, 32),
      )
      .unwrap();

    let mut stages = Vec::new();
    let mut saw_frame = false;
    while let Ok(msg) = rx.recv_timeout(Duration::from_secs(1)) {
      match msg {
        WorkerToUi::Stage { stage, .. } => stages.push(stage),
        WorkerToUi::FrameReady { .. } => {
          saw_frame = true;
          break;
        }
        _ => {}
      }
    }

    assert!(saw_frame, "expected FrameReady message");
    assert!(
      !stages.iter().any(|stage| matches!(
        stage,
        StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects
      )),
      "about:blank should not perform document fetch stages (got {stages:?})"
    );
  }
}
