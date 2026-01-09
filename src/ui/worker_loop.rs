use crate::api::{BrowserDocument, FastRender, RenderOptions};
use crate::geometry::{Point, Size};
use crate::interaction::anchor_scroll::scroll_offset_for_fragment_target;
use crate::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use crate::interaction::{InteractionAction, InteractionEngine};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::about_pages;
use crate::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use url::Url;

pub struct UiWorkerHandle {
  ui_tx: Option<Sender<UiToWorker>>,
  ui_rx: Option<Receiver<WorkerToUi>>,
  join: Option<JoinHandle<()>>,
}

impl UiWorkerHandle {
  pub fn shutdown(mut self) -> std::thread::Result<()> {
    let _ = self.ui_tx.take();
    let _ = self.ui_rx.take();
    match self.join.take() {
      Some(handle) => handle.join(),
      None => Ok(()),
    }
  }

  pub fn split(mut self) -> (Sender<UiToWorker>, Receiver<WorkerToUi>, JoinHandle<()>) {
    let ui_tx = self.ui_tx.take().unwrap_or_else(|| {
      let (tx, _rx) = std::sync::mpsc::channel();
      tx
    });
    let ui_rx = self.ui_rx.take().unwrap_or_else(|| {
      let (_tx, rx) = std::sync::mpsc::channel();
      rx
    });
    let join = self.join.take().unwrap_or_else(|| std::thread::spawn(|| {}));
    (ui_tx, ui_rx, join)
  }
}

impl Drop for UiWorkerHandle {
  fn drop(&mut self) {
    let _ = self.ui_tx.take();
    let _ = self.ui_rx.take();
    if let Some(handle) = self.join.take() {
      let _ = handle.join();
    }
  }
}

struct TabState {
  document: BrowserDocument,
  viewport_css: (u32, u32),
  dpr: f32,
  url: Option<String>,
  scroll: ScrollState,
  interaction: InteractionEngine,
}

fn sanitize_delta(v: f32) -> f32 {
  if v.is_finite() { v } else { 0.0 }
}

fn sanitize_pointer(v: (f32, f32)) -> Option<(f32, f32)> {
  (v.0.is_finite() && v.1.is_finite()).then_some(v)
}

fn effective_base_url(tab: &TabState) -> &str {
  tab
    .document
    .base_url()
    .or_else(|| tab.url.as_deref())
    .unwrap_or("")
}

fn render_options_for_navigation(tab: &TabState) -> RenderOptions {
  RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
}

fn normalize_url_without_fragment(mut url: Url) -> Url {
  url.set_fragment(None);
  url
}

fn resolve_href_against(base: &Url, href: &str) -> Option<Url> {
  Url::parse(href).ok().or_else(|| base.join(href).ok())
}

fn fragment_navigation_target(tab: &TabState, href: &str) -> Option<Url> {
  let current = tab.url.as_deref()?;
  let current_url = Url::parse(current).ok()?;

  let target_url = resolve_href_against(&current_url, href)?;

  let current_no_frag = normalize_url_without_fragment(current_url.clone());
  let target_no_frag = normalize_url_without_fragment(target_url.clone());
  let is_same_document = current_no_frag == target_no_frag;
  if !is_same_document {
    return None;
  }

  let is_fragment_only =
    current_url.fragment().is_some() || target_url.fragment().is_some();
  is_fragment_only.then_some(target_url)
}

fn navigate_fragment_in_place(
  tab_id: TabId,
  tab: &mut TabState,
  ui_tx: &Sender<WorkerToUi>,
  url: Url,
) {
  let url_string = url.to_string();

  let _ = ui_tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url_string.clone(),
  });

  // Same-document fragment navigations should not reload the page; we only update the tab's URL
  // and adjust scroll based on the existing layout cache.
  tab.url = Some(url_string.clone());

  let title = crate::html::title::find_document_title(tab.document.dom());
  let _ = ui_tx.send(WorkerToUi::NavigationCommitted {
    tab_id,
    url: url_string,
    title,
    can_go_back: false,
    can_go_forward: false,
  });

  let viewport = Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32);
  let fragment = url.fragment().unwrap_or("");

  let target_offset = match tab.document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let offset = scroll_offset_for_fragment_target(dom, box_tree, fragment_tree, fragment, viewport);
    (false, offset)
  }) {
    Ok(offset) => offset.unwrap_or(Point::ZERO),
    Err(err) => {
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("fragment navigation scroll failed: {err}"),
      });
      return;
    }
  };

  // If the fragment is empty or the target is not found, we fall back to scrolling to the top of
  // the document (matching the common `href="#"` behavior in browsers).
  let mut next_scroll = tab.scroll.clone();
  next_scroll.viewport = target_offset;
  tab.document.set_scroll_state(next_scroll);

  let painted = match tab.document.render_if_needed_with_scroll_state() {
    Ok(Some(frame)) => frame,
    Ok(None) => match tab.document.render_frame_with_scroll_state() {
      Ok(frame) => frame,
      Err(err) => {
        let _ = ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: format!("paint failed after fragment navigation scroll: {err}"),
        });
        return;
      }
    },
    Err(err) => {
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("paint failed after fragment navigation scroll: {err}"),
      });
      return;
    }
  };

  emit_frame(tab_id, tab, ui_tx, painted.pixmap, painted.scroll_state);
}

fn emit_frame(
  tab_id: TabId,
  tab: &mut TabState,
  ui_tx: &Sender<WorkerToUi>,
  pixmap: tiny_skia::Pixmap,
  scroll_state: ScrollState,
) {
  tab.scroll = scroll_state.clone();
  let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
    tab_id,
    scroll: scroll_state.clone(),
  });
  let _ = ui_tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state,
    },
  });
}

fn repaint_if_needed(tab_id: TabId, tab: &mut TabState, ui_tx: &Sender<WorkerToUi>) {
  let painted = match tab.document.render_if_needed_with_scroll_state() {
    Ok(Some(frame)) => frame,
    Ok(None) => return,
    Err(err) => {
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("render_if_needed failed: {err}"),
      });
      return;
    }
  };
  emit_frame(tab_id, tab, ui_tx, painted.pixmap, painted.scroll_state);
}

fn repaint_force(tab_id: TabId, tab: &mut TabState, ui_tx: &Sender<WorkerToUi>) {
  let painted = match tab.document.render_frame_with_scroll_state() {
    Ok(frame) => frame,
    Err(err) => {
      let _ = ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!("render_frame failed: {err}"),
      });
      return;
    }
  };
  emit_frame(tab_id, tab, ui_tx, painted.pixmap, painted.scroll_state);
}

fn render_navigation_error_page(tab_id: TabId, tab: &mut TabState, ui_tx: &Sender<WorkerToUi>, message: &str) {
  let html = about_pages::error_page_html("Navigation failed", message);

  tab.url = Some(about_pages::ABOUT_ERROR.to_string());
  tab.document.set_navigation_urls(
    Some(about_pages::ABOUT_ERROR.to_string()),
    Some(about_pages::ABOUT_BASE_URL.to_string()),
  );
  tab
    .document
    .set_document_url(Some(about_pages::ABOUT_ERROR.to_string()));

  let options = render_options_for_navigation(tab);
  if tab.document.reset_with_html(&html, options).is_err() {
    return;
  }
  tab.document.set_scroll_state(tab.scroll.clone());
  if let Ok(frame) = tab.document.render_frame_with_scroll_state() {
    emit_frame(tab_id, tab, ui_tx, frame.pixmap, frame.scroll_state);
  }
}

fn navigate_tab(
  tab_id: TabId,
  tab: &mut TabState,
  ui_tx: &Sender<WorkerToUi>,
  url: String,
  _reason: NavigationReason,
) {
  let _ = ui_tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url.clone(),
  });
  let _ = ui_tx.send(WorkerToUi::LoadingState {
    tab_id,
    loading: true,
  });

  tab.scroll = ScrollState::default();
  tab.interaction = InteractionEngine::new();

  let options = render_options_for_navigation(tab);

  let committed_url = if about_pages::is_about_url(&url) {
    let html = about_pages::html_for_about_url(&url).unwrap_or_else(|| {
      about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
    });
    tab
      .document
      .set_navigation_urls(Some(url.clone()), Some(about_pages::ABOUT_BASE_URL.to_string()));
    tab.document.set_document_url(Some(url.clone()));
    if let Err(err) = tab.document.reset_with_html(&html, options) {
      let err = err.to_string();
      let _ = ui_tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url: url.clone(),
        error: err.clone(),
      });
      render_navigation_error_page(tab_id, tab, ui_tx, &err);
      let _ = ui_tx.send(WorkerToUi::LoadingState {
        tab_id,
        loading: false,
      });
      return;
    }
    url.clone()
  } else {
    match tab.document.navigate_url_with_options(&url, options) {
      Ok((committed, _base)) => committed,
      Err(err) => {
        let err = err.to_string();
        let _ = ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url: url.clone(),
          error: err.clone(),
        });
        render_navigation_error_page(tab_id, tab, ui_tx, &err);
        let _ = ui_tx.send(WorkerToUi::LoadingState {
          tab_id,
          loading: false,
        });
        return;
      }
    }
  };

  tab.url = Some(committed_url.clone());
  tab.document.set_scroll_state(tab.scroll.clone());

  let painted = match tab.document.render_frame_with_scroll_state() {
    Ok(frame) => frame,
    Err(err) => {
      let err = err.to_string();
      let _ = ui_tx.send(WorkerToUi::NavigationFailed {
        tab_id,
        url: committed_url.clone(),
        error: err.clone(),
      });
      render_navigation_error_page(tab_id, tab, ui_tx, &err);
      let _ = ui_tx.send(WorkerToUi::LoadingState {
        tab_id,
        loading: false,
      });
      return;
    }
  };

  let title = crate::html::title::find_document_title(tab.document.dom());
  let _ = ui_tx.send(WorkerToUi::NavigationCommitted {
    tab_id,
    url: committed_url,
    title,
    can_go_back: false,
    can_go_forward: false,
  });

  emit_frame(tab_id, tab, ui_tx, painted.pixmap, painted.scroll_state);

  let _ = ui_tx.send(WorkerToUi::LoadingState {
    tab_id,
    loading: false,
  });
}

/// Spawns a headless UI worker loop used by the browser UI integration tests.
///
/// This worker owns per-tab [`BrowserDocument`] instances and processes [`UiToWorker`] messages,
/// emitting [`WorkerToUi`] events over standard mpsc channels.
pub fn spawn_ui_worker(name: impl Into<String>) -> std::io::Result<UiWorkerHandle> {
  let (to_worker_tx, to_worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (to_ui_tx, to_ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let join = std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || run_worker_loop(to_worker_rx, to_ui_tx))?;

  Ok(UiWorkerHandle {
    ui_tx: Some(to_worker_tx),
    ui_rx: Some(to_ui_rx),
    join: Some(join),
  })
}

fn run_worker_loop(rx: Receiver<UiToWorker>, ui_tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();

  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab {
        tab_id,
        initial_url,
        ..
      } => {
        let renderer = match FastRender::builder().build() {
          Ok(renderer) => renderer,
          Err(err) => {
            let _ = ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("failed to create renderer: {err}"),
            });
            continue;
          }
        };

        let options = RenderOptions::new()
          .with_viewport(800, 600)
          .with_device_pixel_ratio(1.0);
        let document = match BrowserDocument::new(renderer, "<!doctype html><html></html>", options) {
          Ok(doc) => doc,
          Err(err) => {
            let _ = ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("failed to create BrowserDocument: {err}"),
            });
            continue;
          }
        };

        let tab = TabState {
          document,
          viewport_css: (800, 600),
          dpr: 1.0,
          url: None,
          scroll: ScrollState::default(),
          interaction: InteractionEngine::new(),
        };
        tabs.insert(tab_id, tab);

        if let Some(url) = initial_url {
          if let Some(tab) = tabs.get_mut(&tab_id) {
            navigate_tab(tab_id, tab, &ui_tx, url, NavigationReason::TypedUrl);
          }
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        tabs.remove(&tab_id);
      }
      UiToWorker::SetActiveTab { .. } => {}
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        if let Some(tab) = tabs.get_mut(&tab_id) {
          tab.viewport_css = (viewport_css.0.max(1), viewport_css.1.max(1));
          tab.dpr = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
          tab.document.set_viewport(tab.viewport_css.0, tab.viewport_css.1);
          tab.document.set_device_pixel_ratio(tab.dpr);
          if tab.url.is_some() {
            repaint_if_needed(tab_id, tab, &ui_tx);
          }
        }
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        navigate_tab(tab_id, tab, &ui_tx, url, reason);
      }
      UiToWorker::GoBack { tab_id } | UiToWorker::GoForward { tab_id } => {
        // History navigation is implemented in the real UI worker (`ui_worker.rs`). This minimal
        // headless worker loop exists primarily for pixel-based integration tests, so ignore
        // back/forward requests for now.
        let _ = ui_tx.send(WorkerToUi::DebugLog {
          tab_id,
          line: "navigation history is not tracked by this worker loop; ignoring back/forward".to_string(),
        });
      }
      UiToWorker::Reload { tab_id } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(url) = tab.url.clone() else {
          continue;
        };
        navigate_tab(tab_id, tab, &ui_tx, url, NavigationReason::Reload);
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };

        let delta_x = sanitize_delta(delta_css.0);
        let delta_y = sanitize_delta(delta_css.1);
        let current = tab.scroll.clone();

        let next = match pointer_css.and_then(sanitize_pointer) {
          None => {
            let mut viewport = current.viewport;
            let x = viewport.x + delta_x;
            let y = viewport.y + delta_y;
            viewport.x = if x.is_finite() { x.max(0.0) } else { viewport.x };
            viewport.y = if y.is_finite() { y.max(0.0) } else { viewport.y };
            ScrollState::from_parts(viewport, current.elements)
          }
          Some((x, y)) => {
            let Some(prepared) = tab.document.prepared() else {
              continue;
            };
            let page_point = Point::new(x + current.viewport.x, y + current.viewport.y);
            apply_wheel_scroll_at_point(
              prepared.fragment_tree(),
              &current,
              Size::new(tab.viewport_css.0 as f32, tab.viewport_css.1 as f32),
              page_point,
              ScrollWheelInput {
                delta_x,
                delta_y,
              },
            )
          }
        };

        tab.document.set_scroll_state(next);
        let painted = match tab.document.render_if_needed_with_scroll_state() {
          Ok(Some(frame)) => frame,
          Ok(None) => continue,
          Err(err) => {
            let _ = ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("paint failed after scroll: {err}"),
            });
            continue;
          }
        };

        if painted.scroll_state != tab.scroll {
          emit_frame(tab_id, tab, &ui_tx, painted.pixmap, painted.scroll_state);
        }
      }
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let viewport_point = Point::new(pos_css.0, pos_css.1);
        let scroll = &tab.scroll;
        let engine = &mut tab.interaction;

        let _ = tab.document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_move(dom, box_tree, fragment_tree, scroll, viewport_point);
          (changed, ())
        });
        repaint_if_needed(tab_id, tab, &ui_tx);
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
        let scroll = &tab.scroll;
        let engine = &mut tab.interaction;

        let _ = tab.document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          let changed = engine.pointer_down(dom, box_tree, fragment_tree, scroll, viewport_point);
          (changed, ())
        });
        repaint_if_needed(tab_id, tab, &ui_tx);
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
        let base_url = effective_base_url(tab).to_string();
        let viewport_point = Point::new(pos_css.0, pos_css.1);
        let scroll = &tab.scroll;
        let engine = &mut tab.interaction;

        let action = match tab.document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
          engine.pointer_up(dom, box_tree, fragment_tree, scroll, viewport_point, &base_url)
        }) {
          Ok(action) => action,
          Err(_) => continue,
        };

        match action {
          InteractionAction::Navigate { href } => {
            if let Some(url) = fragment_navigation_target(tab, &href) {
              navigate_fragment_in_place(tab_id, tab, &ui_tx, url);
            } else {
              navigate_tab(tab_id, tab, &ui_tx, href, NavigationReason::LinkClick);
            }
          }
          InteractionAction::OpenSelectDropdown {
            select_node_id,
            control,
          } => {
            let _ = ui_tx.send(WorkerToUi::OpenSelectDropdown {
              tab_id,
              select_node_id,
              control,
            });
            repaint_if_needed(tab_id, tab, &ui_tx);
          }
          _ => {
            repaint_if_needed(tab_id, tab, &ui_tx);
          }
        }
      }
      UiToWorker::TextInput { tab_id, text } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let engine = &mut tab.interaction;
        let _ = tab.document.mutate_dom(|dom| engine.text_input(dom, &text));
        repaint_if_needed(tab_id, tab, &ui_tx);
      }
      UiToWorker::KeyAction { tab_id, key } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let base_url = effective_base_url(tab).to_string();
        let document_url = tab.url.clone().unwrap_or_default();
        let (doc, interaction) = (&mut tab.document, &mut tab.interaction);
        let mut action = InteractionAction::None;
        let _ = doc.mutate_dom(|dom| {
          let (changed, a) = interaction.key_activate(dom, key, &document_url, &base_url);
          action = a;
          changed
        });
        match action {
          InteractionAction::Navigate { href } => {
            navigate_tab(tab_id, tab, &ui_tx, href, NavigationReason::LinkClick);
          }
          _ => {
            repaint_if_needed(tab_id, tab, &ui_tx);
          }
        }
      }
      UiToWorker::RequestRepaint { tab_id, reason: _ } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        repaint_force(tab_id, tab, &ui_tx);
      }
    }
  }
}
