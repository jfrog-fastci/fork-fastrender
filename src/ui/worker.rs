use crate::api::{BrowserDocument, FastRender, PreparedDocument, PreparedPaintOptions};
use crate::error::{Error, RenderError};
use crate::geometry::{Point, Rect, Size};
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::interaction::scroll_offset_for_fragment_target;
use crate::render_control::{GlobalStageListenerGuard, RenderDeadline, StageHeartbeat};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::text::font_db::FontConfig;
use crate::ui::about_pages;
use crate::ui::cancel::CancelGens;
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

struct LoadingStateGuard {
  tab_id: TabId,
  tx: Sender<WorkerToUi>,
}

impl LoadingStateGuard {
  fn new(tab_id: TabId, tx: Sender<WorkerToUi>) -> Self {
    Self { tab_id, tx }
  }
}

impl Drop for LoadingStateGuard {
  fn drop(&mut self) {
    let _ = self.tx.send(WorkerToUi::LoadingState {
      tab_id: self.tab_id,
      loading: false,
    });
  }
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
  /// Shut down the worker loop and join its thread.
  ///
  /// This is equivalent to [`Self::join`], but named explicitly to match the "send shutdown then
  /// join" semantics expected by browser integration tests.
  pub fn shutdown(self) -> std::thread::Result<()> {
    self.join()
  }

  pub fn join(self) -> std::thread::Result<()> {
    // Ensure the worker loop can observe channel closure before we block on joining.
    drop(self.ui_tx);
    self.handle.join()
  }

  pub fn into_parts(self) -> (
    Sender<UiToWorker>,
    Receiver<WorkerToUi>,
    std::thread::JoinHandle<()>,
  ) {
    (self.ui_tx, self.ui_rx, self.handle)
  }

  pub fn split(self) -> (Sender<UiToWorker>, Receiver<WorkerToUi>, std::thread::JoinHandle<()>) {
    self.into_parts()
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
  cancel: CancelGens,
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_state: ScrollState,
  history: TabHistory,
  document: Option<BrowserDocument>,
  current_url: Option<String>,
  interaction: InteractionEngine,
}

impl TabState {
  fn new(cancel: CancelGens) -> Self {
    Self {
      cancel,
      viewport_css: (800, 600),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      history: TabHistory::new(),
      document: None,
      current_url: None,
      interaction: InteractionEngine::new(),
    }
  }

  fn effective_base_url(&self) -> Option<&str> {
    self
      .document
      .as_ref()
      .and_then(|doc| doc.base_url())
      .or_else(|| self.current_url.as_deref())
  }
}

struct SameDocumentFragmentNavigation {
  target_url: Url,
  fragment: Option<String>,
  url_changed: bool,
}

fn same_document_fragment_navigation(
  current_url: Option<&str>,
  target_url: &str,
) -> Option<SameDocumentFragmentNavigation> {
  let current_url = current_url?;
  let current = Url::parse(current_url).ok()?;

  let target = if target_url.starts_with('#') {
    let mut resolved = current.clone();
    resolved.set_fragment(Some(&target_url[1..]));
    resolved
  } else {
    Url::parse(target_url).ok()?
  };

  let mut current_base = current.clone();
  current_base.set_fragment(None);
  let mut target_base = target.clone();
  target_base.set_fragment(None);

  if current_base != target_base {
    return None;
  }

  // No fragment on either URL means this isn't a fragment navigation (it is an exact URL match).
  // Treat this as a normal navigation so callers can reload the page.
  if current.fragment().is_none() && target.fragment().is_none() {
    return None;
  }

  let fragment = target.fragment().map(str::to_string);
  Some(SameDocumentFragmentNavigation {
    url_changed: current.fragment() != target.fragment(),
    target_url: target,
    fragment,
  })
}

fn select_anchor_css(
  prepared: &crate::PreparedDocument,
  scroll_state: &ScrollState,
  select_node_id: usize,
) -> Option<Rect> {
  let select_box_id = {
    let mut stack: Vec<&crate::BoxNode> = vec![&prepared.box_tree().root];
    let mut found = None;
    while let Some(node) = stack.pop() {
      if node.styled_node_id == Some(select_node_id) {
        found = Some(node.id);
        break;
      }
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    found?
  };

  let mut fragment_tree_scrolled = prepared.fragment_tree().clone();
  crate::scroll::apply_scroll_offsets(&mut fragment_tree_scrolled, scroll_state);
  let page_rect =
    crate::interaction::absolute_bounds_for_box_id(&fragment_tree_scrolled, select_box_id)?;
  Some(page_rect.translate(Point::new(
    -scroll_state.viewport.x,
    -scroll_state.viewport.y,
  )))
}

#[derive(Debug, Clone, Copy)]
enum SelectRow {
  OptGroupLabel { disabled: bool },
  Option { node_id: usize, disabled: bool },
}

fn is_ancestor_or_self(
  index: &crate::interaction::dom_index::DomIndex,
  ancestor: usize,
  mut node: usize,
) -> bool {
  while node != 0 {
    if node == ancestor {
      return true;
    }
    node = *index.parent.get(node).unwrap_or(&0);
  }
  false
}

fn has_disabled_optgroup_ancestor(
  index: &crate::interaction::dom_index::DomIndex,
  mut node_id: usize,
  root_id: usize,
) -> bool {
  while node_id != 0 && node_id != root_id {
    let parent = *index.parent.get(node_id).unwrap_or(&0);
    if parent == 0 || parent == root_id {
      break;
    }
    if index.node(parent).is_some_and(|node| {
      node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"))
        && node.get_attribute_ref("disabled").is_some()
    }) {
      return true;
    }
    node_id = parent;
  }
  false
}

fn collect_select_rows(
  index: &crate::interaction::dom_index::DomIndex,
  select_id: usize,
) -> Vec<SelectRow> {
  // Mirror `build_select_control`: `<optgroup>` contributes a label row followed by its descendants.
  let mut end = select_id;
  for id in (select_id + 1)..=index.len() {
    if is_ancestor_or_self(index, select_id, id) {
      end = id;
    } else {
      break;
    }
  }

  let mut rows = Vec::new();
  for id in (select_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    let Some(tag) = node.tag_name() else {
      continue;
    };

    if tag.eq_ignore_ascii_case("optgroup") {
      let disabled = node.get_attribute_ref("disabled").is_some()
        || has_disabled_optgroup_ancestor(index, id, select_id);
      rows.push(SelectRow::OptGroupLabel { disabled });
      continue;
    }

    if tag.eq_ignore_ascii_case("option") {
      let disabled = node.get_attribute_ref("disabled").is_some()
        || has_disabled_optgroup_ancestor(index, id, select_id);
      rows.push(SelectRow::Option {
        node_id: id,
        disabled,
      });
    }
  }

  rows
}

fn box_id_for_styled_node(
  box_tree: &crate::tree::box_tree::BoxTree,
  styled_node_id: usize,
) -> Option<usize> {
  let mut stack: Vec<&crate::tree::box_tree::BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      return Some(node.id);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn select_anchor_rect_viewport_css(
  dom: &crate::dom::DomNode,
  box_tree: &crate::tree::box_tree::BoxTree,
  fragment_tree: &crate::tree::fragment_tree::FragmentTree,
  page_point_css: Point,
  select_node_id: usize,
  scroll_viewport_css: Point,
) -> Rect {
  let box_id = crate::interaction::hit_test_dom(dom, box_tree, fragment_tree, page_point_css)
    .filter(|hit| hit.dom_node_id == select_node_id)
    .map(|hit| hit.box_id)
    .or_else(|| box_id_for_styled_node(box_tree, select_node_id));

  let Some(page_rect) = box_id
    .and_then(|box_id| crate::interaction::absolute_bounds_for_box_id(fragment_tree, box_id))
  else {
    return Rect::ZERO;
  };

  Rect::from_xywh(
    page_rect.x() - scroll_viewport_css.x,
    page_rect.y() - scroll_viewport_css.y,
    page_rect.width(),
    page_rect.height(),
  )
}

fn ui_worker_main(rx: Receiver<UiToWorker>, tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();

  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        cancel,
      } => {
        let entry = tabs
          .entry(tab_id)
          .or_insert_with(|| TabState::new(cancel.clone()));
        entry.cancel = cancel;
        entry.history = TabHistory::new();
        entry.document = None;
        entry.current_url = None;
        entry.interaction = InteractionEngine::new();

        if let Some(url) = initial_url {
          navigate_tab(tab_id, entry, url, NavigationReason::TypedUrl, None, &tx);
        }
      }
      UiToWorker::NewTab { tab_id, initial_url } => {
        let entry = tabs
          .entry(tab_id)
          .or_insert_with(|| TabState::new(CancelGens::new()));
        entry.history = TabHistory::new();
        entry.document = None;
        entry.current_url = None;
        entry.interaction = InteractionEngine::new();

        if let Some(url) = initial_url {
          navigate_tab(tab_id, entry, url, NavigationReason::TypedUrl, None, &tx);
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
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        tab
          .history
          .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
        navigate_tab(tab_id, tab, url, reason, None, &tx);
      }
      UiToWorker::GoBack { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        // Persist the current scroll position before moving in history.
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
        navigate_tab(
          tab_id,
          tab,
          url,
          NavigationReason::BackForward,
          Some((scroll_x, scroll_y)),
          &tx,
        );
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
        navigate_tab(
          tab_id,
          tab,
          url,
          NavigationReason::BackForward,
          Some((scroll_x, scroll_y)),
          &tx,
        );
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
        navigate_tab(tab_id, tab, url, NavigationReason::Reload, None, &tx);
      }
      UiToWorker::Tick { .. } => {
        // Headless worker loop does not run a JS event loop; ticks are a no-op.
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
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
          let scrolled = doc
            .wheel_scroll_at_viewport_point(Point::new(x, y), delta)
            .unwrap_or(false);
          if scrolled {
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
        let viewport_point = Point::new(pos_css.0, pos_css.1);
        let scroll = &tab.scroll_state;
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let _ = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_move(dom, box_tree, fragment_tree, scroll, viewport_point);
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
        let viewport_point = Point::new(pos_css.0, pos_css.1);
        let scroll = &tab.scroll_state;
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let _ = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_down(dom, box_tree, fragment_tree, scroll, viewport_point);
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
        let viewport_point = Point::new(pos_css.0, pos_css.1);
        let scroll = &tab.scroll_state;
        // Avoid borrowing from `tab` across the DOM mutation call below (we need mutable borrows for
        // `tab.document` and `tab.interaction`).
        let document_url = tab.current_url.clone().unwrap_or_default();
        let base_url = tab.effective_base_url().unwrap_or("").to_string();
        let engine = &mut tab.interaction;
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let action = match doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          engine.pointer_up_with_scroll(
            dom,
            box_tree,
            fragment_tree,
            scroll,
            viewport_point,
            &document_url,
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
            navigate_tab(tab_id, tab, href, NavigationReason::LinkClick, None, &tx);
          }
          InteractionAction::OpenSelectDropdown {
            select_node_id,
            control,
          } => {
            // Back-compat: keep the older cursor-anchored dropdown message.
            let _ = tx.send(WorkerToUi::OpenSelectDropdown {
              tab_id,
              select_node_id,
              control: control.clone(),
            });

            let anchor_css = doc
              .prepared()
              .and_then(|prepared| select_anchor_css(prepared, scroll, select_node_id))
              .unwrap_or_else(|| Rect::from_xywh(viewport_point.x, viewport_point.y, 0.0, 0.0));

            let _ = tx.send(WorkerToUi::SelectDropdownOpened {
              tab_id,
              select_node_id,
              control,
              anchor_css,
            });
            repaint_if_needed(tab_id, tab, &tx);
          }
          _ => {
            repaint_if_needed(tab_id, tab, &tx);
          }
        }
      }
      UiToWorker::SelectDropdownChoose {
        tab_id,
        select_node_id,
        option_node_id,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let changed = doc.mutate_dom(|dom| {
          crate::interaction::dom_mutation::activate_select_option(
            dom,
            select_node_id,
            option_node_id,
            /*toggle_for_multiple=*/ false,
          )
        });
        if changed {
          repaint_if_needed(tab_id, tab, &tx);
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
        let result = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
          let (changed, a) =
            engine.key_activate_with_box_tree(dom, Some(box_tree), key, &document_url, &base_url);
          (changed, (changed, a))
        });
        let changed = match result {
          Ok((dom_changed, next_action)) => {
            action = next_action;
            dom_changed
          }
          Err(_) => doc.mutate_dom(|dom| {
            let (dom_changed, next_action) = engine.key_activate(dom, key, &document_url, &base_url);
            action = next_action;
            dom_changed
          }),
        };
        match action {
          InteractionAction::Navigate { href } => {
            tab
              .history
              .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);
            navigate_tab(tab_id, tab, href, NavigationReason::LinkClick, None, &tx);
          }
          InteractionAction::OpenSelectDropdown {
            select_node_id,
            control,
          } => {
            // Back-compat: keep the older cursor-anchored dropdown message.
            let _ = tx.send(WorkerToUi::OpenSelectDropdown {
              tab_id,
              select_node_id,
              control: control.clone(),
            });

            let anchor_css = doc
              .prepared()
              .and_then(|prepared| select_anchor_css(prepared, &tab.scroll_state, select_node_id))
              .unwrap_or(Rect::ZERO);

            let _ = tx.send(WorkerToUi::SelectDropdownOpened {
              tab_id,
              select_node_id,
              control,
              anchor_css,
            });
            if changed {
              repaint_if_needed(tab_id, tab, &tx);
            }
          }
          _ => {
            if changed {
              repaint_if_needed(tab_id, tab, &tx);
            }
          }
        }
      }
      UiToWorker::SelectDropdownPick {
        tab_id,
        select_node_id,
        item_index,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_mut() else {
          continue;
        };

        let mut should_close = false;
        let _ = doc.mutate_dom(|dom| {
          let index = crate::interaction::dom_index::DomIndex::build(dom);
          let rows = collect_select_rows(&index, select_node_id);
          let row = rows.get(item_index).copied();
          match row {
            Some(SelectRow::OptGroupLabel { .. }) | None => false,
            Some(SelectRow::Option { node_id, disabled }) => {
              if disabled {
                return false;
              }
              should_close = true;
              crate::interaction::dom_mutation::activate_select_option(dom, select_node_id, node_id, false)
            }
          }
        });

        if should_close {
          let _ = tx.send(WorkerToUi::SelectDropdownClosed { tab_id });
        }
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

fn url_fragment(url: &str) -> Option<&str> {
  url.split_once('#').map(|(_, fragment)| fragment)
}

fn render_navigation_error_page(
  tab_id: TabId,
  tab: &mut TabState,
  tx: &Sender<WorkerToUi>,
  message: &str,
) {
  // Best-effort: if we can't render the error page, the caller still emits NavigationFailed so the
  // UI can surface the error string.
  let html = about_pages::error_page_html("Navigation failed", message);
  let url = about_pages::ABOUT_ERROR.to_string();

  // Ensure the error page renders at the top of the viewport.
  tab.scroll_state = ScrollState::default();

  let options = RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
    .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y)
    .with_element_scroll_offsets(tab.scroll_state.elements.clone());

  if let Some(doc) = tab.document.as_mut() {
    doc.set_navigation_urls(Some(url.clone()), Some(about_pages::ABOUT_BASE_URL.to_string()));
    doc.set_document_url(Some(url.clone()));
    if doc.reset_with_html(&html, options).is_err() {
      return;
    }
  } else {
    let mut renderer = match FastRender::new() {
      Ok(renderer) => renderer,
      Err(_) => return,
    };
    renderer.set_base_url(about_pages::ABOUT_BASE_URL);
    let dom = match renderer.parse_html(&html) {
      Ok(dom) => dom,
      Err(_) => return,
    };
    let report = match renderer.prepare_dom_with_options(dom, Some(&url), options.clone()) {
      Ok(report) => report,
      Err(_) => return,
    };
    let doc = match BrowserDocument::from_prepared(renderer, report.document, options) {
      Ok(doc) => doc,
      Err(_) => return,
    };
    tab.document = Some(doc);
  }

  tab.current_url = Some(url);
  if let Some(doc) = tab.document.as_mut() {
    doc.set_scroll_state(tab.scroll_state.clone());
  }
  repaint_force(tab_id, tab, tx);
}

fn navigate_tab(
  tab_id: TabId,
  tab: &mut TabState,
  mut url: String,
  reason: NavigationReason,
  mut restore_scroll: Option<(f32, f32)>,
  tx: &Sender<WorkerToUi>,
) {
  url = url.trim().to_string();

  // Apply history semantics first so that Back/Forward restores the correct scroll position and
  // Reload uses the current entry URL (without creating a new history entry).
  match reason {
    // `TypedUrl` / `LinkClick` history push is handled after we check for same-document fragment
    // navigations. Fragment-only navigations should not reset scroll container offsets.
    NavigationReason::TypedUrl | NavigationReason::LinkClick => {}
    NavigationReason::Reload => {
      if let Some(entry) = tab.history.reload_target() {
        url = entry.url.clone();
      }
    }
    NavigationReason::BackForward => {
      if !tab
        .history
        .current()
        .is_some_and(|entry| entry.url == url)
      {
        if tab.history.go_back_forward_to(&url).is_none() {
          let _ = tx.send(WorkerToUi::DebugLog {
            tab_id,
            line: format!("BackForward navigation to unknown URL: {url}"),
          });
          return;
        }
      }

      let Some(entry) = tab.history.current() else {
        return;
      };
      url = entry.url.clone();
      restore_scroll.get_or_insert((entry.scroll_x, entry.scroll_y));

      // Apply restored viewport scroll before fragment-only navigation handling. Keep element scroll
      // offsets intact for same-document fragment history navigations.
      if let Some((scroll_x, scroll_y)) = restore_scroll {
        tab.scroll_state.viewport.x = if scroll_x.is_finite() { scroll_x.max(0.0) } else { 0.0 };
        tab.scroll_state.viewport.y = if scroll_y.is_finite() { scroll_y.max(0.0) } else { 0.0 };
      }
    }
  }

  // Any navigation should cancel in-flight work for this tab (scroll, hover highlights, etc).
  //
  // The UI also bumps this generation before sending explicit `Navigate`/`GoBack`/`Reload`
  // messages. We still bump here so worker-initiated navigations (link clicks) cancel correctly.
  tab.cancel.bump_nav();

  let snapshot_prepare = tab.cancel.snapshot_prepare();
  let cancel_prepare = snapshot_prepare.cancel_callback_for_prepare(&tab.cancel);

  // Allow Reload to fully reload the document even if the URL only differs by fragment.
  if reason != NavigationReason::Reload {
    if let Some(nav) = same_document_fragment_navigation(tab.current_url.as_deref(), &url) {
      if handle_fragment_navigation(tab_id, tab, nav, reason, snapshot_prepare, tx) {
        return;
      }
    }
  }

  let _ = tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url.clone(),
  });
  let _ = tx.send(WorkerToUi::LoadingState {
    tab_id,
    loading: true,
  });
  let _loading_guard = LoadingStateGuard::new(tab_id, tx.clone());

  // Forward `StageHeartbeat` updates while we perform the render pipeline work for this navigation.
  // This is intentionally scoped to the synchronous "navigation" job so we don't leak a
  // process-global stage listener across unrelated renders (including those from other tabs).
  let _stage_guard = forward_stage_heartbeats(tab_id, tx.clone());

  let push_history = matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick);
  if push_history {
    tab.history.push(url.clone());
  }

  // New navigation resets interaction state. This avoids leaking focus/hover chain ids across DOM
  // trees.
  tab.interaction = InteractionEngine::new();
  match reason {
    NavigationReason::TypedUrl | NavigationReason::LinkClick => {
      tab.scroll_state = ScrollState::default();
    }
    NavigationReason::BackForward => {
      tab.scroll_state = ScrollState::with_viewport(tab.scroll_state.viewport);
      if let Some((scroll_x, scroll_y)) = restore_scroll {
        tab.scroll_state.viewport.x = if scroll_x.is_finite() { scroll_x.max(0.0) } else { 0.0 };
        tab.scroll_state.viewport.y = if scroll_y.is_finite() { scroll_y.max(0.0) } else { 0.0 };
      }
    }
    NavigationReason::Reload => {
      // Preserve scroll offsets on reload (matching typical browser behavior).
    }
  }

  let options = RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
    .with_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y)
    .with_element_scroll_offsets(tab.scroll_state.elements.clone())
    .with_cancel_callback(Some(cancel_prepare.clone()));

  let final_url = if about_pages::is_about_url(&url) {
    let html = about_pages::html_for_about_url(&url).unwrap_or_else(|| {
      about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
    });

    if let Some(doc) = tab.document.as_mut() {
      // Ensure `about:*` pages never resolve relative URLs against the previous navigation origin.
      doc.set_navigation_urls(Some(url.clone()), Some(about_pages::ABOUT_BASE_URL.to_string()));
      doc.set_document_url(Some(url.clone()));
      if let Err(err) = doc.reset_with_html(&html, options) {
        doc.set_cancel_callback(None);
        if is_cancel_timeout(&err) || !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
          return;
        }
        let err = err.to_string();
        let _ = tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.clone(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        render_navigation_error_page(tab_id, tab, tx, &err);
        return;
      }
      doc.set_cancel_callback(None);
      url.clone()
    } else {
      let mut renderer = match FastRender::new() {
        Ok(renderer) => renderer,
        Err(err) => {
          let _ = tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url,
            error: err.to_string(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          render_navigation_error_page(tab_id, tab, tx, &err.to_string());
          return;
        }
      };
      // Ensure `about:*` pages never resolve relative URLs against the previous navigation origin.
      renderer.set_base_url(about_pages::ABOUT_BASE_URL);

      let dom = match renderer.parse_html(&html) {
        Ok(dom) => dom,
        Err(err) => {
          let _ = tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url,
            error: err.to_string(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          render_navigation_error_page(tab_id, tab, tx, &err.to_string());
          return;
        }
      };

      let report = match renderer.prepare_dom_with_options(dom, Some(&url), options.clone()) {
        Ok(report) => report,
        Err(err) => {
          if is_cancel_timeout(&err) || !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
            return;
          }
          let _ = tx.send(WorkerToUi::NavigationFailed {
            tab_id,
            url,
            error: err.to_string(),
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          render_navigation_error_page(tab_id, tab, tx, &err.to_string());
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
            can_go_back: tab.history.can_go_back(),
            can_go_forward: tab.history.can_go_forward(),
          });
          render_navigation_error_page(tab_id, tab, tx, &err.to_string());
          return;
        }
      };
      tab.document = Some(doc);
      final_url
    }
  } else if let Some(doc) = tab.document.as_mut() {
    match doc.navigate_url(&url, options) {
      Ok(report) => report.final_url.clone().unwrap_or_else(|| url.clone()),
      Err(err) => {
        doc.set_cancel_callback(None);
        if is_cancel_timeout(&err) || !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
          return;
        }
        let err = err.to_string();
        let _ = tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.clone(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        render_navigation_error_page(tab_id, tab, tx, &err);
        return;
      }
    }
  } else {
    let mut renderer = match FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
    {
      Ok(renderer) => renderer,
      Err(err) => {
        let _ = tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.to_string(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        render_navigation_error_page(tab_id, tab, tx, &err.to_string());
        return;
      }
    };

    let report = match renderer.prepare_url(&url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        if is_cancel_timeout(&err) || !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
          return;
        }
        let err = err.to_string();
        let _ = tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.clone(),
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        render_navigation_error_page(tab_id, tab, tx, &err);
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
          can_go_back: tab.history.can_go_back(),
          can_go_forward: tab.history.can_go_forward(),
        });
        render_navigation_error_page(tab_id, tab, tx, &err.to_string());
        return;
      }
    };
    tab.document = Some(doc);
    final_url
  };

  let Some(doc) = tab.document.as_mut() else {
    return;
  };
  if !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
    doc.set_cancel_callback(None);
    return;
  }
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

  // Update current history entry URL after redirects. For typed/link-click navigations, the entry
  // was pushed above using the original URL.
  tab.history.commit_navigation(&url, Some(&final_url));
  let title = tab
    .document
    .as_ref()
    .and_then(|doc| crate::html::title::find_document_title(doc.dom()));
  if let Some(title) = title.as_ref() {
    tab.history.set_title(title.clone());
  }

  if let Some(title) = title.clone() {
    tab.history.set_title(title);
  }
  let committed_url = final_url;

  let painted = render_frame_force_with_cancel(tab_id, tab, tx);

  if !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
    return;
  }
  let _ = tx.send(WorkerToUi::NavigationCommitted {
    tab_id,
    url: committed_url,
    title,
    can_go_back: tab.history.can_go_back(),
    can_go_forward: tab.history.can_go_forward(),
  });

  if let Some(painted) = painted {
    emit_painted_frame(tab_id, tab, tx, painted);
  }
}

fn is_cancel_timeout(err: &Error) -> bool {
  matches!(err, Error::Render(RenderError::Timeout { .. }))
}

fn handle_fragment_navigation(
  tab_id: TabId,
  tab: &mut TabState,
  nav: SameDocumentFragmentNavigation,
  reason: NavigationReason,
  snapshot_prepare: crate::ui::cancel::CancelSnapshot,
  tx: &Sender<WorkerToUi>,
) -> bool {
  let mut committed_url_for_event: Option<String> = None;

  // Preserve scroll offset for the current history entry before we potentially push a new one.
  tab
    .history
    .update_scroll(tab.scroll_state.viewport.x, tab.scroll_state.viewport.y);

  {
    let Some(doc) = tab.document.as_mut() else {
      return false;
    };
    if doc.prepared().is_none() {
      return false;
    }

    if nav.url_changed {
      let committed_url = nav.target_url.to_string();
      if matches!(reason, NavigationReason::TypedUrl | NavigationReason::LinkClick) {
        tab.history.push(committed_url.clone());
      }
      tab.current_url = Some(committed_url.clone());
      committed_url_for_event = Some(committed_url.clone());
      // Update document URL so `:target` / `:target-within` can respond to the new fragment.
      //
      // Note: this marks the document dirty (style/layout) so the next repaint reflects the new
      // selector state. We still avoid a full navigation fetch by reusing the existing DOM/layout
      // artifacts for scroll target resolution.
      doc.set_document_url(Some(committed_url.clone()));
      // We'll emit NavigationCommitted after painting to avoid sending stale commits when this
      // navigation is cancelled by a subsequent `bump_nav`.
    }

    // Typed URL / link clicks should scroll to the fragment target (or top for an empty fragment).
    // Back/forward navigations should restore the stored scroll offset for the history entry, so do
    // not override `tab.scroll_state.viewport` here.
    if !matches!(reason, NavigationReason::BackForward) {
      let viewport_size = Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32);
      let desired_scroll = match nav.fragment.as_deref() {
        None | Some("") => Some(Point::ZERO),
        Some(fragment) => {
          let Some(prepared) = doc.prepared() else {
            return false;
          };
          scroll_offset_for_fragment_target(
            doc.dom(),
            prepared.box_tree(),
            prepared.fragment_tree(),
            fragment,
            viewport_size,
          )
        }
      };

      if let Some(point) = desired_scroll {
        tab.scroll_state.viewport = point;
      }
    }
    doc.set_scroll_state(tab.scroll_state.clone());
  }
  let painted = render_frame_force_with_cancel(tab_id, tab, tx);

  if let Some(committed_url) = committed_url_for_event {
    if !snapshot_prepare.is_still_current_for_prepare(&tab.cancel) {
      return true;
    }
    let _ = tx.send(WorkerToUi::NavigationCommitted {
      tab_id,
      url: committed_url,
      title: tab
        .document
        .as_ref()
        .and_then(|doc| crate::html::title::find_document_title(doc.dom())),
      can_go_back: tab.history.can_go_back(),
      can_go_forward: tab.history.can_go_forward(),
    });
  }

  if let Some(painted) = painted {
    emit_painted_frame(tab_id, tab, tx, painted);
  }

  true
}

fn repaint_if_needed(tab_id: TabId, tab: &mut TabState, tx: &Sender<WorkerToUi>) {
  let Some(doc) = tab.document.as_mut() else {
    return;
  };

  // Prepare/layout should only be cancelable by new navigations. Painting should cancel on any
  // bump (including scroll and hover/input highlights).
  let snapshot_prepare = tab.cancel.snapshot_prepare();
  let prepare_cancel_cb = snapshot_prepare.cancel_callback_for_prepare(&tab.cancel);
  doc.set_cancel_callback(Some(prepare_cancel_cb));
  let snapshot_paint = tab.cancel.snapshot_paint();
  let paint_cancel_cb = snapshot_paint.cancel_callback_for_paint(&tab.cancel);
  let paint_deadline = RenderDeadline::new(None, Some(paint_cancel_cb));

  let painted = match doc.render_if_needed_with_deadlines(Some(&paint_deadline)) {
    Ok(Some(painted)) => painted,
    Ok(None) => {
      doc.set_cancel_callback(None);
      return;
    }
    Err(err) => {
      doc.set_cancel_callback(None);
      if is_cancel_timeout(&err) {
        return;
      }
      let _ = tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("render_if_needed failed: {err}"),
      });
      return;
    }
  };
  doc.set_cancel_callback(None);

  if !snapshot_paint.is_still_current_for_paint(&tab.cancel) {
    return;
  }
  emit_painted_frame(tab_id, tab, tx, painted);
}

fn repaint_force(tab_id: TabId, tab: &mut TabState, tx: &Sender<WorkerToUi>) {
  let Some(painted) = render_frame_force_with_cancel(tab_id, tab, tx) else {
    return;
  };
  emit_painted_frame(tab_id, tab, tx, painted);
}

fn emit_painted_frame(
  tab_id: TabId,
  tab: &mut TabState,
  tx: &Sender<WorkerToUi>,
  painted: crate::PaintedFrame,
) {
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

fn render_frame_force_with_cancel(
  tab_id: TabId,
  tab: &mut TabState,
  tx: &Sender<WorkerToUi>,
) -> Option<crate::PaintedFrame> {
  let Some(doc) = tab.document.as_mut() else {
    return None;
  };

  let snapshot_prepare = tab.cancel.snapshot_prepare();
  let prepare_cancel_cb = snapshot_prepare.cancel_callback_for_prepare(&tab.cancel);
  doc.set_cancel_callback(Some(prepare_cancel_cb));
  let snapshot_paint = tab.cancel.snapshot_paint();
  let paint_cancel_cb = snapshot_paint.cancel_callback_for_paint(&tab.cancel);
  let paint_deadline = RenderDeadline::new(None, Some(paint_cancel_cb));

  let painted = match doc.render_frame_with_deadlines(Some(&paint_deadline)) {
    Ok(frame) => frame,
    Err(err) => {
      doc.set_cancel_callback(None);
      if is_cancel_timeout(&err) {
        return None;
      }
      let _ = tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("render_frame failed: {err}"),
      });
      return None;
    }
  };
  doc.set_cancel_callback(None);

  if !snapshot_paint.is_still_current_for_paint(&tab.cancel) {
    return None;
  }

  Some(painted)
}
