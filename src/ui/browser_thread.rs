use crate::api::{BrowserDocument, FastRender, FastRenderFactory, PreparedDocumentReport, RenderOptions};
use crate::geometry::Point;
use crate::html::find_document_title;
use crate::interaction::anchor_scroll::scroll_offset_for_fragment_target;
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::ui::about_pages;
use crate::ui::cancel::{CancelGens, CancelSnapshot};
use crate::ui::history::TabHistory;
use crate::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use crate::ui::worker::spawn_render_worker_thread;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// Handle to the browser worker thread.
///
/// The UI thread sends [`UiToWorker`] messages over `tx`, and receives [`WorkerToUi`] updates on
/// `rx`.
pub struct BrowserWorkerHandle {
  pub tx: Sender<UiToWorker>,
  pub rx: Receiver<WorkerToUi>,
  pub join: std::thread::JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct NavigationRequest {
  url: String,
  apply_fragment_scroll: bool,
}

struct TabState {
  history: TabHistory,
  loading: bool,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  document: Option<BrowserDocument>,
  interaction: InteractionEngine,
  cancel: CancelGens,
  last_committed_url: Option<String>,
  last_base_url: Option<String>,

  pending_navigation: Option<NavigationRequest>,
  needs_repaint: bool,
  wants_scroll_update: bool,
}

impl TabState {
  fn new(cancel: CancelGens) -> Self {
    Self {
      history: TabHistory::new(),
      loading: false,
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      document: None,
      interaction: InteractionEngine::new(),
      cancel,
      last_committed_url: None,
      last_base_url: None,
      pending_navigation: None,
      needs_repaint: false,
      wants_scroll_update: false,
    }
  }
}

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

fn clamp_viewport((w, h): (u32, u32)) -> (u32, u32) {
  (w.max(1), h.max(1))
}

fn base_url_for_links(tab: &TabState) -> &str {
  tab
    .last_base_url
    .as_deref()
    .or(tab.last_committed_url.as_deref())
    .unwrap_or(about_pages::ABOUT_BASE_URL)
}

fn normalize_url_without_fragment(mut url: url::Url) -> url::Url {
  url.set_fragment(None);
  url
}

fn resolve_href_against(base: &url::Url, href: &str) -> Option<url::Url> {
  url::Url::parse(href).ok().or_else(|| base.join(href).ok())
}

/// Returns the fully-resolved target URL when `href` is a same-document navigation that only
/// changes the fragment (e.g. `#target`).
fn same_document_fragment_target(current_url: &str, href: &str) -> Option<url::Url> {
  let current_parsed = url::Url::parse(current_url).ok()?;
  let target_parsed = resolve_href_against(&current_parsed, href)?;

  let current_base = normalize_url_without_fragment(current_parsed.clone());
  let target_base = normalize_url_without_fragment(target_parsed.clone());
  if current_base != target_base {
    return None;
  }

  // Only treat this as a fragment navigation when either side actually has a fragment component.
  // (Pure same-URL navigations still trigger a reload.)
  if current_parsed.fragment().is_none() && target_parsed.fragment().is_none() {
    return None;
  }

  // Ignore no-op navigations to the exact same URL string.
  (current_url != target_parsed.as_str()).then_some(target_parsed)
}

fn url_fragment(url: &str) -> Option<&str> {
  url.split_once('#').map(|(_, fragment)| fragment)
}

fn is_allowed_navigation_url(url: &str) -> Result<(), String> {
  if about_pages::is_about_url(url) {
    return Ok(());
  }
  let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
  let scheme = parsed.scheme().to_ascii_lowercase();
  match scheme.as_str() {
    "http" | "https" | "file" => Ok(()),
    _ => Err(format!("unsupported URL scheme: {scheme}")),
  }
}

enum Job {
  Navigate {
    tab_id: TabId,
    request: NavigationRequest,
  },
  Paint {
    tab_id: TabId,
  },
}

struct JobOutput {
  tab_id: TabId,
  snapshot: CancelSnapshot,
  snapshot_kind: SnapshotKind,
  msgs: Vec<WorkerToUi>,
}

#[derive(Clone, Copy)]
enum SnapshotKind {
  Prepare,
  Paint,
}

struct BrowserRuntime {
  ui_rx: Receiver<UiToWorker>,
  ui_tx: Sender<WorkerToUi>,
  factory: FastRenderFactory,
  tabs: HashMap<TabId, TabState>,
  active_tab: Option<TabId>,
}

impl BrowserRuntime {
  fn new(ui_rx: Receiver<UiToWorker>, ui_tx: Sender<WorkerToUi>, factory: FastRenderFactory) -> Self {
    Self {
      ui_rx,
      ui_tx,
      factory,
      tabs: HashMap::new(),
      active_tab: None,
    }
  }

  fn run(&mut self) {
    loop {
      // If there is no pending work, block for the next message.
      if !self.has_pending_jobs() {
        let Ok(msg) = self.ui_rx.recv() else {
          break;
        };
        self.handle_message(msg);
      }

      self.drain_messages();

      let Some(job) = self.next_job() else {
        continue;
      };

      let output = self.run_job(job);

      // Messages might have arrived while we were preparing/painting. Drain and handle them before
      // deciding whether to emit the (potentially stale) output.
      self.drain_messages();

      let Some(output) = output else {
        continue;
      };

      if !self.is_output_still_current(&output) {
        continue;
      }

      for msg in output.msgs {
        let _ = self.ui_tx.send(msg);
      }
    }
  }

  fn has_pending_jobs(&self) -> bool {
    self
      .tabs
      .values()
      .any(|tab| tab.pending_navigation.is_some() || tab.needs_repaint)
  }

  fn drain_messages(&mut self) {
    while let Ok(msg) = self.ui_rx.try_recv() {
      self.handle_message(msg);
    }
  }

  fn handle_message(&mut self, msg: UiToWorker) {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        cancel,
      } => {
        self.tabs.insert(tab_id, TabState::new(cancel));
        self.active_tab.get_or_insert(tab_id);

        let url = initial_url.unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
        self.schedule_navigation(tab_id, url, NavigationReason::TypedUrl);
      }
      UiToWorker::NewTab { tab_id, initial_url } => {
        self.tabs.insert(tab_id, TabState::new(CancelGens::new()));
        self.active_tab.get_or_insert(tab_id);

        let url = initial_url.unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());
        self.schedule_navigation(tab_id, url, NavigationReason::TypedUrl);
      }
      UiToWorker::CloseTab { tab_id } => {
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
          self.active_tab = None;
        }
      }
      UiToWorker::SetActiveTab { tab_id } => {
        if self.tabs.contains_key(&tab_id) {
          self.active_tab = Some(tab_id);
        }
      }
      UiToWorker::Navigate { tab_id, url, reason } => {
        self.schedule_navigation(tab_id, url, reason);
      }
      UiToWorker::GoBack { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          tab.history.go_back().map(|entry| entry.url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(tab_id, url, NavigationReason::BackForward, false);
        }
      }
      UiToWorker::GoForward { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          tab.history.go_forward().map(|entry| entry.url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(tab_id, url, NavigationReason::BackForward, false);
        }
      }
      UiToWorker::Reload { tab_id } => {
        let url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          tab
            .history
            .reload_target()
            .map(|entry| entry.url.clone())
            .or_else(|| tab.last_committed_url.clone())
        };
        if let Some(url) = url {
          self.begin_navigation(tab_id, url, NavigationReason::Reload, false);
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
        tab.viewport_css = clamp_viewport(viewport_css);
        tab.dpr = if dpr.is_finite() { dpr.max(f32::EPSILON) } else { 1.0 };
        tab.cancel.bump_paint();
        tab.needs_repaint = true;

        if let Some(doc) = tab.document.as_mut() {
          doc.set_viewport(tab.viewport_css.0, tab.viewport_css.1);
          doc.set_device_pixel_ratio(tab.dpr);
        }
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };

        // Ignore invalid/no-op scroll deltas.
        let delta_x = delta_css.0;
        let delta_y = delta_css.1;
        if (!delta_x.is_finite() && !delta_y.is_finite()) || (delta_x == 0.0 && delta_y == 0.0) {
          return;
        }
        let delta_x = if delta_x.is_finite() { delta_x } else { 0.0 };
        let delta_y = if delta_y.is_finite() { delta_y } else { 0.0 };

        let Some(doc) = tab.document.as_mut() else {
          // No document yet (e.g. scrolling during initial load). Still record the viewport scroll
          // so it can be applied when the first frame is rendered.
          let mut next = tab.scroll_state.clone();
          next.viewport.x = (next.viewport.x + delta_x).max(0.0);
          next.viewport.y = (next.viewport.y + delta_y).max(0.0);
          if next != tab.scroll_state {
            tab.scroll_state = next;
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.wants_scroll_update = true;
          }
          return;
        };

        let current_scroll = doc.scroll_state();
        let mut changed = false;
        let mut wheel_handled = false;

        if let Some(pointer_css) = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite()) {
          // Apply scroll wheel deltas to the scroll container under the pointer (including element
          // scroll offsets like `<select size>` listboxes).
          match doc.wheel_scroll_at_viewport_point(Point::new(pointer_css.0, pointer_css.1), (delta_x, delta_y)) {
            Ok(scrolled) => {
              wheel_handled = true;
              if scrolled {
                tab.scroll_state = doc.scroll_state();
                changed = true;
              }
            }
            Err(_) => {
              // No cached layout yet; fall back to basic viewport scrolling below.
            }
          }
        }

        // If no pointer position was provided (or we couldn't apply wheel scrolling at all), treat
        // this as a basic viewport scroll and clamp to the content bounds when possible.
        if !wheel_handled {
          let mut next = current_scroll.clone();

          if let Some(prepared) = doc.prepared() {
            let viewport = prepared.fragment_tree().viewport_size();
            let content = prepared.fragment_tree().content_size();
            let max_scroll_x = (content.width() - viewport.width).max(0.0);
            let max_scroll_y = (content.height() - viewport.height).max(0.0);

            let apply_axis = |current: f32, delta: f32, max: f32| {
              if delta == 0.0 || !delta.is_finite() {
                return current;
              }
              let value = current + delta;
              if value.is_finite() {
                value.clamp(0.0, max)
              } else {
                current
              }
            };

            next.viewport.x = apply_axis(next.viewport.x, delta_x, max_scroll_x);
            next.viewport.y = apply_axis(next.viewport.y, delta_y, max_scroll_y);
          } else {
            next.viewport.x = (next.viewport.x + delta_x).max(0.0);
            next.viewport.y = (next.viewport.y + delta_y).max(0.0);
          }

          if next != current_scroll {
            doc.set_scroll_state(next.clone());
            tab.scroll_state = next;
            changed = true;
          }
        }

        if changed {
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
          tab.wants_scroll_update = true;
        }
      }
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button: _,
      } => {
        self.handle_pointer_move(tab_id, pos_css);
      }
      UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button,
      } => {
        self.handle_pointer_down(tab_id, pos_css, button);
      }
      UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button,
      } => {
        self.handle_pointer_up(tab_id, pos_css, button);
      }
      UiToWorker::TextInput { tab_id, text } => {
        self.handle_text_input(tab_id, &text);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        self.handle_key_action(tab_id, key);
      }
      UiToWorker::RequestRepaint { tab_id, reason: _ } => {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
          return;
        };
        tab.cancel.bump_paint();
        tab.needs_repaint = true;
      }
    }
  }

  fn schedule_navigation(&mut self, tab_id: TabId, url: String, reason: NavigationReason) {
    let requested_url = url.trim().to_string();
    if requested_url.is_empty() {
      return;
    }

    match reason {
      NavigationReason::TypedUrl => {
        // Only normalize user-typed URLs. Back/forward/reload should preserve the exact URL
        // stored in history (the UI sends those URLs verbatim).
        let url = crate::ui::normalize_user_url(&requested_url).unwrap_or(requested_url);
        self.begin_navigation(tab_id, url, NavigationReason::TypedUrl, true);
      }
      NavigationReason::LinkClick => {
        // Link clicks are resolved by the interaction engine against the current document base
        // URL, so we treat them as already-canonical.
        self.begin_navigation(tab_id, requested_url, NavigationReason::LinkClick, true);
      }
      NavigationReason::Reload => {
        let (nav_url, push_history) = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };
          let push_history = tab.history.current().is_none();
          let nav_url = tab
            .history
            .reload_target()
            .map(|entry| entry.url.clone())
            .unwrap_or_else(|| requested_url.clone());
          (nav_url, push_history)
        };
        self.begin_navigation(tab_id, nav_url, NavigationReason::Reload, push_history);
      }
      NavigationReason::BackForward => {
        let nav_url = {
          let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return;
          };

          if tab
            .history
            .current()
            .is_some_and(|entry| entry.url == requested_url)
          {
            Some(requested_url.clone())
          } else {
            tab
              .history
              .go_back_forward_to(&requested_url)
              .map(|entry| entry.url.clone())
          }
        };

        let Some(nav_url) = nav_url else {
          let _ = self.ui_tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("ignoring BackForward navigation to unknown URL: {requested_url}"),
          });
          return;
        };

        self.begin_navigation(tab_id, nav_url, NavigationReason::BackForward, false);
      }
    }
  }

  fn begin_navigation(
    &mut self,
    tab_id: TabId,
    url: String,
    reason: NavigationReason,
    push_history: bool,
  ) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };

    // Fragment-only navigation within the same document: update URL + scroll state in-place.
    //
    // Avoid a full reload/reprepare; we reuse the cached layout artifacts for hit-testing and
    // compute a new viewport offset for the fragment target.
    //
    // `Reload` must not take this path because callers expect a full reload.
    if reason != NavigationReason::Reload {
      if tab.pending_navigation.is_none() {
        if let (Some(current), Some(doc)) = (tab.last_committed_url.as_deref(), tab.document.as_mut()) {
          if let Some(target_url) = same_document_fragment_target(current, &url) {
            let url_string = target_url.to_string();

            if push_history {
              // Persist current scroll position for the previous history entry before pushing a
              // new entry for the fragment navigation.
              //
              // Note: for back/forward navigations, the history index has already been moved by
              // the caller, so updating scroll here would corrupt the target entry.
              tab
                .history
                .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
              tab.history.push(url_string.clone());
            }

            tab.last_committed_url = Some(url_string.clone());
            doc.set_document_url_without_invalidation(Some(url_string.clone()));

            let fragment = target_url.fragment().unwrap_or("");
            let offset = if matches!(reason, NavigationReason::BackForward) {
              tab
                .history
                .current()
                .map(|entry| Point::new(entry.scroll_x, entry.scroll_y))
                .unwrap_or(Point::ZERO)
            } else if fragment.is_empty() {
              Point::ZERO
            } else {
              match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
                let viewport = fragment_tree.viewport_size();
                let offset =
                  scroll_offset_for_fragment_target(dom, box_tree, fragment_tree, fragment, viewport);
                (false, offset)
              }) {
                Ok(Some(offset)) => offset,
                Ok(None) => Point::ZERO,
                Err(err) => {
                  let _ = self.ui_tx.send(WorkerToUi::DebugLog {
                    tab_id,
                    line: format!("fragment navigation scroll failed: {err}"),
                  });
                  tab.scroll_state.viewport
                }
              }
            };

            tab.scroll_state.viewport = offset;
            doc.set_scroll_state(tab.scroll_state.clone());

            let _ = self.ui_tx.send(WorkerToUi::NavigationStarted {
              tab_id,
              url: url_string.clone(),
            });
            let title = find_document_title(doc.dom());
            let _ = self.ui_tx.send(WorkerToUi::NavigationCommitted {
              tab_id,
              url: url_string,
              title,
              can_go_back: tab.history.can_go_back(),
              can_go_forward: tab.history.can_go_forward(),
            });

            tab.cancel.bump_paint();
            tab.needs_repaint = true;
            tab.wants_scroll_update = true;
            return;
          }
        }
      }
    }

    tab.cancel.bump_nav();
    tab.loading = true;
    tab.needs_repaint = false;
    tab.pending_navigation = Some(NavigationRequest {
      url: url.clone(),
      apply_fragment_scroll: matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick),
    });
    if push_history {
      tab.history.push(url.clone());
    }

    let _ = self.ui_tx.send(WorkerToUi::LoadingState {
      tab_id,
      loading: true,
    });
    let _ = self.ui_tx.send(WorkerToUi::NavigationStarted { tab_id, url });
  }

  fn handle_pointer_move(&mut self, tab_id: TabId, pos_css: (f32, f32)) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let scroll = &tab.scroll_state;
    let engine = &mut tab.interaction;

    let changed = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let changed = engine.pointer_move(dom, box_tree, fragment_tree, scroll, viewport_point);
      (changed, changed)
    }) {
      Ok(changed) => changed,
      Err(_) => return,
    };
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_pointer_down(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let scroll = &tab.scroll_state;
    let engine = &mut tab.interaction;

    let changed = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let changed = engine.pointer_down(dom, box_tree, fragment_tree, scroll, viewport_point);
      (changed, changed)
    }) {
      Ok(changed) => changed,
      Err(_) => return,
    };
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_pointer_up(&mut self, tab_id: TabId, pos_css: (f32, f32), button: PointerButton) {
    if !matches!(button, PointerButton::Primary) {
      return;
    }
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let base_url = base_url_for_links(tab).to_string();
    let viewport_point = Point::new(pos_css.0, pos_css.1);
    let scroll = &tab.scroll_state;
    let engine = &mut tab.interaction;
    let Some(doc) = tab.document.as_mut() else {
      return;
    };
    let (dom_changed, action) = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let (dom_changed, action) =
        engine.pointer_up(dom, box_tree, fragment_tree, scroll, viewport_point, &base_url);
      (dom_changed, (dom_changed, action))
    }) {
      Ok(result) => result,
      Err(_) => return,
    };

    match action {
      InteractionAction::Navigate { href } => {
        self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
      }
      InteractionAction::OpenSelectDropdown {
        select_node_id,
        control,
      } => {
        let _ = self.ui_tx.send(WorkerToUi::OpenSelectDropdown {
          tab_id,
          select_node_id,
          control,
        });
        if dom_changed {
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
        }
      }
      _ => {
        if dom_changed {
          tab.cancel.bump_paint();
          tab.needs_repaint = true;
        }
      }
    }
  }

  fn handle_text_input(&mut self, tab_id: TabId, text: &str) {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return;
    };
    let Some(doc) = tab.document.as_mut() else {
      return;
    };

    let changed = doc.mutate_dom(|dom| tab.interaction.text_input(dom, text));
    if changed {
      tab.cancel.bump_paint();
      tab.needs_repaint = true;
    }
  }

  fn handle_key_action(&mut self, tab_id: TabId, key: crate::interaction::KeyAction) {
    let mut navigate_to: Option<String> = None;

    {
      let Some(tab) = self.tabs.get_mut(&tab_id) else {
        return;
      };
      let base_url = base_url_for_links(tab).to_string();
      let document_url = tab.last_committed_url.clone().unwrap_or_default();

      let Some(doc) = tab.document.as_mut() else {
        return;
      };

      let mut action = InteractionAction::None;
      let changed = doc.mutate_dom(|dom| {
        let (dom_changed, next_action) =
          tab
            .interaction
            .key_activate(dom, key, &document_url, &base_url);
        action = next_action;
        dom_changed
      });

      match action {
        InteractionAction::Navigate { href } => {
          navigate_to = Some(href);
        }
        InteractionAction::OpenSelectDropdown {
          select_node_id,
          control,
        } => {
          let _ = self.ui_tx.send(WorkerToUi::OpenSelectDropdown {
            tab_id,
            select_node_id,
            control,
          });
          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
        _ => {
          if changed {
            tab.cancel.bump_paint();
            tab.needs_repaint = true;
          }
        }
      }
    }

    if let Some(href) = navigate_to {
      self.schedule_navigation(tab_id, href, NavigationReason::LinkClick);
    }
  }

  fn next_job(&mut self) -> Option<Job> {
    // Navigation takes priority over repaint.
    if let Some(active) = self.active_tab {
      if let Some(tab) = self.tabs.get_mut(&active) {
        if let Some(req) = tab.pending_navigation.take() {
          return Some(Job::Navigate {
            tab_id: active,
            request: req,
          });
        }
      }
    }
    // Any pending navigation.
    if let Some((tab_id, req)) = self
      .tabs
      .iter_mut()
      .find_map(|(id, tab)| tab.pending_navigation.take().map(|req| (*id, req)))
    {
      return Some(Job::Navigate { tab_id, request: req });
    }

    // Paint active tab first.
    if let Some(active) = self.active_tab {
      if self.tabs.get(&active).is_some_and(|t| t.needs_repaint) {
        if let Some(tab) = self.tabs.get_mut(&active) {
          tab.needs_repaint = false;
          return Some(Job::Paint { tab_id: active });
        }
      }
    }

    // Paint any tab.
    if let Some(tab_id) = self
      .tabs
      .iter()
      .find_map(|(id, tab)| tab.needs_repaint.then_some(*id))
    {
      if let Some(tab) = self.tabs.get_mut(&tab_id) {
        tab.needs_repaint = false;
      }
      return Some(Job::Paint { tab_id });
    }

    None
  }

  fn is_output_still_current(&self, output: &JobOutput) -> bool {
    let Some(tab) = self.tabs.get(&output.tab_id) else {
      return false;
    };
    match output.snapshot_kind {
      SnapshotKind::Prepare => output.snapshot == tab.cancel.snapshot_prepare(),
      SnapshotKind::Paint => output.snapshot == tab.cancel.snapshot_paint(),
    }
  }

  fn run_job(&mut self, job: Job) -> Option<JobOutput> {
    match job {
      Job::Navigate { tab_id, request } => self.run_navigation(tab_id, request),
      Job::Paint { tab_id } => self.run_paint(tab_id),
    }
  }

  fn run_navigation(&mut self, tab_id: TabId, request: NavigationRequest) -> Option<JobOutput> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    let snapshot = tab.cancel.snapshot_prepare();
    let cancel_callback = snapshot.cancel_callback_for_prepare(&tab.cancel);

    let viewport_css = tab.viewport_css;
    let dpr = tab.dpr;
    let initial_scroll = tab.history.current().map(|e| (e.scroll_x, e.scroll_y));
    let apply_fragment_scroll = request.apply_fragment_scroll;

    // Drop the mutable borrow for the potentially expensive prepare+paint.
    let (prepared, original_url) = {
      let original_url = request.url.clone();
      let options = RenderOptions::default()
        .with_viewport(viewport_css.0, viewport_css.1)
        .with_device_pixel_ratio(dpr);
      let mut options = options;
      options.cancel_callback = Some(cancel_callback);

      match is_allowed_navigation_url(&original_url) {
        Ok(()) => (
          self
            .prepare_document(tab_id, &original_url, options)
            .map_err(|e| e.to_string()),
          original_url,
        ),
        Err(err) => (Err(err), original_url),
      }
    };

    // If the tab was closed while we were preparing, drop the result.
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    // From this point, we will consider this navigation "current" unless cancelled by subsequent
    // message processing (handled by `is_output_still_current`).
    let paint_snapshot = tab.cancel.snapshot_paint();
    let paint_cancel_callback = paint_snapshot.cancel_callback_for_paint(&tab.cancel);

    let mut msgs = Vec::new();

    match prepared {
      Ok((renderer, report)) => {
        let PreparedDocumentReport {
          document,
          final_url: reported_final_url,
          base_url,
          diagnostics: _,
        } = report;

        let committed_url = reported_final_url
          .clone()
          .unwrap_or_else(|| original_url.clone());
        tab.last_committed_url = Some(committed_url.clone());
        tab.last_base_url = base_url.clone();

        // Create and paint the document.
        let mut scroll_state = ScrollState::with_viewport(Point::new(
          initial_scroll.map(|(x, _)| x).unwrap_or(0.0),
          initial_scroll.map(|(_, y)| y).unwrap_or(0.0),
        ));
        if apply_fragment_scroll {
          if let Some(fragment) = url_fragment(&committed_url) {
            let offset = if fragment.is_empty() {
              Some(Point::ZERO)
            } else {
              scroll_offset_for_fragment_target(
                document.dom(),
                document.box_tree(),
                document.fragment_tree(),
                fragment,
                document.layout_viewport(),
              )
            };
            if let Some(offset) = offset {
              scroll_state.viewport = offset;
            }
          }
        }

        let mut doc = match BrowserDocument::from_prepared(
          renderer,
          document,
          RenderOptions::default()
            .with_viewport(viewport_css.0, viewport_css.1)
            .with_device_pixel_ratio(dpr),
        ) {
          Ok(doc) => doc,
          Err(err) => {
            return self.run_navigation_error(
              tab_id,
              &original_url,
              &format!("failed to create BrowserDocument: {err}"),
              snapshot,
            );
          }
        };

        doc.set_navigation_urls(reported_final_url.clone(), base_url.clone());
        doc.set_scroll_state(scroll_state.clone());
        doc.set_cancel_callback(Some(paint_cancel_callback));

        let painted = {
          let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
          doc.render_if_needed_with_scroll_state()
        };

        let painted = match painted {
          Ok(Some(frame)) => frame,
          Ok(None) => {
            // Unexpected (we just changed scroll/cancel callback), but keep going.
            return None;
          }
          Err(err) => {
            return self.run_navigation_error(
              tab_id,
              &original_url,
              &format!("paint failed: {err}"),
              snapshot,
            );
          }
        };

        tab.scroll_state = painted.scroll_state.clone();
        tab.history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        tab.document = Some(doc);
        tab.interaction = InteractionEngine::new();

        // Update history and emit navigation state.
        let _ = tab
          .history
          .commit_navigation(&original_url, reported_final_url.as_deref());
        let title = tab
          .document
          .as_ref()
          .and_then(|doc| find_document_title(doc.dom()));
        if let Some(title) = title.as_deref() {
          tab.history.set_title(title.to_string());
        }

        msgs.push(WorkerToUi::NavigationCommitted {
          tab_id,
          url: committed_url.clone(),
          title: title.clone(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });

        msgs.push(WorkerToUi::FrameReady {
          tab_id,
          frame: RenderedFrame {
            pixmap: painted.pixmap,
            viewport_css,
            dpr: tab
              .document
              .as_ref()
              .and_then(|d| d.prepared())
              .map(|p| p.device_pixel_ratio())
              .unwrap_or(dpr),
            scroll_state: tab.scroll_state.clone(),
          },
        });

        tab.loading = false;
        msgs.push(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
      }
      Err(err) => {
        return self.run_navigation_error(tab_id, &original_url, &err, snapshot);
      }
    }

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Prepare,
      msgs,
    })
  }

  fn run_navigation_error(
    &mut self,
    tab_id: TabId,
    original_url: &str,
    error: &str,
    snapshot: CancelSnapshot,
  ) -> Option<JobOutput> {
    let (viewport_css, dpr) = match self.tabs.get(&tab_id) {
      Some(tab) => (tab.viewport_css, tab.dpr),
      None => return None,
    };

    let html = about_pages::error_page_html("Navigation failed", error);
    let prepared = {
      let options = RenderOptions::default()
        .with_viewport(viewport_css.0, viewport_css.1)
        .with_device_pixel_ratio(dpr);
      self.prepare_about_html(tab_id, about_pages::ABOUT_ERROR, &html, options)
    };

    let (renderer, report) = match prepared {
      Ok(r) => r,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: original_url.to_string(),
          error: format!("{error} (and failed to render error page: {err})"),
        });
        if let Some(tab) = self.tabs.get_mut(&tab_id) {
          tab.loading = false;
        }
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return None;
      }
    };

    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };

    let PreparedDocumentReport {
      document,
      final_url,
      base_url,
      diagnostics: _,
    } = report;

    let paint_snapshot = tab.cancel.snapshot_paint();
    let paint_cancel = paint_snapshot.cancel_callback_for_paint(&tab.cancel);
    let scroll_state = ScrollState::with_viewport(Point::ZERO);

    let mut doc = match BrowserDocument::from_prepared(
      renderer,
      document,
      RenderOptions::default()
        .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
        .with_device_pixel_ratio(tab.dpr),
    ) {
      Ok(doc) => doc,
      Err(_) => {
        // If even the error page can't be installed, just emit NavigationFailed.
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: original_url.to_string(),
          error: error.to_string(),
        });
        tab.loading = false;
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return None;
      }
    };

    doc.set_navigation_urls(final_url.clone(), base_url.clone());
    doc.set_scroll_state(scroll_state.clone());
    doc.set_cancel_callback(Some(paint_cancel));

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      doc.render_if_needed_with_scroll_state()
    };
    let painted = match painted {
      Ok(Some(frame)) => frame,
      _ => {
        let _ = self.ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: original_url.to_string(),
          error: error.to_string(),
        });
        tab.loading = false;
        let _ = self.ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return None;
      }
    };

    tab.document = Some(doc);
    tab.interaction = InteractionEngine::new();
    tab.scroll_state = painted.scroll_state.clone();
    tab.last_committed_url = Some(
      final_url
        .clone()
        .unwrap_or_else(|| about_pages::ABOUT_ERROR.to_string()),
    );
    tab.last_base_url = base_url.or_else(|| Some(about_pages::ABOUT_BASE_URL.to_string()));

    tab.loading = false;

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Prepare,
      msgs: vec![
        WorkerToUi::NavigationFailed {
          tab_id,
          url: original_url.to_string(),
          error: error.to_string(),
        },
        WorkerToUi::FrameReady {
          tab_id,
          frame: RenderedFrame {
            pixmap: painted.pixmap,
            viewport_css: tab.viewport_css,
            dpr: tab
              .document
              .as_ref()
              .and_then(|d| d.prepared())
              .map(|p| p.device_pixel_ratio())
              .unwrap_or(tab.dpr),
            scroll_state: tab.scroll_state.clone(),
          },
        },
        WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        },
      ],
    })
  }

  fn run_paint(&mut self, tab_id: TabId) -> Option<JobOutput> {
    let Some(tab) = self.tabs.get_mut(&tab_id) else {
      return None;
    };
    let Some(doc) = tab.document.as_mut() else {
      return None;
    };

    let snapshot = tab.cancel.snapshot_paint();
    let cancel_callback = snapshot.cancel_callback_for_paint(&tab.cancel);
    doc.set_cancel_callback(Some(cancel_callback));

    let wants_scroll = std::mem::take(&mut tab.wants_scroll_update);

    let painted = {
      let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
      doc.render_if_needed_with_scroll_state()
    };

    let painted = match painted {
      Ok(Some(frame)) => Some(frame),
      Ok(None) => None,
      Err(err) => {
        let _ = self.ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("paint error: {err}"),
        });
        None
      }
    };

    let mut msgs = Vec::new();

    if let Some(frame) = painted {
      tab.scroll_state = frame.scroll_state.clone();
      tab.history.update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

      msgs.push(WorkerToUi::FrameReady {
        tab_id,
        frame: RenderedFrame {
          pixmap: frame.pixmap,
          viewport_css: tab.viewport_css,
          dpr: tab
            .document
            .as_ref()
            .and_then(|d| d.prepared())
            .map(|p| p.device_pixel_ratio())
            .unwrap_or(tab.dpr),
          scroll_state: tab.scroll_state.clone(),
        },
      });
    }

    if wants_scroll {
      msgs.push(WorkerToUi::ScrollStateUpdated {
        tab_id,
        scroll: tab.scroll_state.clone(),
      });
    }

    Some(JobOutput {
      tab_id,
      snapshot,
      snapshot_kind: SnapshotKind::Paint,
      msgs,
    })
  }

  fn prepare_about_html(
    &self,
    tab_id: TabId,
    document_url: &str,
    html: &str,
    options: RenderOptions,
  ) -> crate::Result<(FastRender, PreparedDocumentReport)> {
    let mut renderer = self.factory.build_renderer()?;
    renderer.set_base_url(about_pages::ABOUT_BASE_URL);
    let dom = renderer.parse_html(html)?;
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    let report = renderer.prepare_dom_with_options(dom, Some(document_url), options)?;
    Ok((renderer, report))
  }

  fn prepare_document(
    &self,
    tab_id: TabId,
    url: &str,
    options: RenderOptions,
  ) -> crate::Result<(FastRender, PreparedDocumentReport)> {
    if about_pages::is_about_url(url) {
      let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
        about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
      });
      return self.prepare_about_html(tab_id, url, &html, options);
    }

    let mut renderer = self.factory.build_renderer()?;
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());
    let report = renderer.prepare_url(url, options)?;
    Ok((renderer, report))
  }
}

/// Spawn the browser worker thread.
///
/// The returned handle can be used from a headless caller (no winit/egui required).
pub fn spawn_browser_worker() -> crate::Result<BrowserWorkerHandle> {
  let factory = FastRenderFactory::new()?;
  // `spawn_render_worker_thread` requires a renderer instance even though this runtime builds its
  // own per-navigation renderers from the factory. Build one from the same factory to ensure we do
  // not duplicate global caches.
  let renderer = factory.build_renderer()?;

  let (ui_to_worker_tx, ui_to_worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (worker_to_ui_tx, worker_to_ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let factory_for_thread = factory.clone();
  let worker_to_ui_tx_for_thread = worker_to_ui_tx.clone();

  let join = spawn_render_worker_thread(
    "browser_worker",
    renderer,
    worker_to_ui_tx,
    move |_render_worker| {
      let mut runtime = BrowserRuntime::new(ui_to_worker_rx, worker_to_ui_tx_for_thread, factory_for_thread);
      runtime.run();
    },
  )?;

  Ok(BrowserWorkerHandle {
    tx: ui_to_worker_tx,
    rx: worker_to_ui_rx,
    join,
  })
}
