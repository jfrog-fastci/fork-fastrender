use crate::api::{FastRender, PreparedDocument, PreparedPaintOptions, RenderOptions};
use crate::geometry::Point;
use crate::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use crate::scroll::ScrollState;
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::about_pages;
use crate::ui::messages::{RenderedFrame, TabId, UiToWorker, WorkerToUi};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

pub struct UiWorkerHandle {
  pub ui_tx: Sender<UiToWorker>,
  pub ui_rx: Receiver<WorkerToUi>,
  pub join: std::thread::JoinHandle<()>,
}

struct TabState {
  renderer: FastRender,
  viewport_css: (u32, u32),
  dpr: f32,
  url: Option<String>,
  document: Option<PreparedDocument>,
  scroll: ScrollState,
}

fn sanitize_delta(v: f32) -> f32 {
  if v.is_finite() { v } else { 0.0 }
}

fn sanitize_pointer(v: (f32, f32)) -> Option<(f32, f32)> {
  (v.0.is_finite() && v.1.is_finite()).then_some(v)
}

fn paint_document(
  tab_id: TabId,
  tab: &TabState,
  doc: &PreparedDocument,
  ui_tx: &Sender<WorkerToUi>,
  scroll_state: ScrollState,
) -> Option<ScrollState> {
  let painted = doc
    .paint_with_options_frame(
      PreparedPaintOptions::new()
        .with_scroll_state(scroll_state)
        .with_viewport(tab.viewport_css.0, tab.viewport_css.1),
    )
    .ok()?;

  let scroll_state = painted.scroll_state;
  let _ = ui_tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state: scroll_state.clone(),
    },
  });
  Some(scroll_state)
}

fn navigate_tab(tab_id: TabId, tab: &mut TabState, ui_tx: &Sender<WorkerToUi>, url: String) {
  let _ = ui_tx.send(WorkerToUi::NavigationStarted {
    tab_id,
    url: url.clone(),
  });

  tab.url = Some(url.clone());
  tab.scroll = ScrollState::default();

  let options = RenderOptions::new()
    .with_viewport(tab.viewport_css.0, tab.viewport_css.1)
    .with_device_pixel_ratio(tab.dpr)
    .with_scroll(tab.scroll.viewport.x, tab.scroll.viewport.y)
    .with_element_scroll_offsets(tab.scroll.elements.clone());

  let report = if about_pages::is_about_url(&url) {
    let html = about_pages::html_for_about_url(&url).unwrap_or_else(|| {
      about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
    });
    tab.renderer.set_base_url(about_pages::ABOUT_BASE_URL);
    let dom = match tab.renderer.parse_html(&html) {
      Ok(dom) => dom,
      Err(err) => {
        let _ = ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.to_string(),
        });
        return;
      }
    };
    match tab.renderer.prepare_dom_with_options(dom, Some(&url), options) {
      Ok(report) => report,
      Err(err) => {
        let _ = ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.to_string(),
        });
        return;
      }
    }
  } else {
    match tab.renderer.prepare_url(&url, options) {
      Ok(report) => report,
      Err(err) => {
        let _ = ui_tx.send(WorkerToUi::NavigationFailed {
          tab_id,
          url,
          error: err.to_string(),
        });
        return;
      }
    }
  };

  let committed_url = report.final_url.clone().unwrap_or_else(|| url.clone());
  let doc = report.document;
  let scroll = paint_document(tab_id, tab, &doc, ui_tx, tab.scroll.clone());
  if let Some(scroll) = scroll {
    tab.scroll = scroll;
    let _ = ui_tx.send(WorkerToUi::NavigationCommitted {
      tab_id,
      url: committed_url,
      title: None,
      can_go_back: false,
      can_go_forward: false,
    });
  }
  tab.document = Some(doc);
}

/// Spawns a headless UI worker loop used by the browser UI integration tests.
///
/// This worker owns per-tab `FastRender` instances and processes [`UiToWorker`] messages, emitting
/// [`WorkerToUi`] events over standard mpsc channels.
pub fn spawn_ui_worker(name: impl Into<String>) -> std::io::Result<UiWorkerHandle> {
  let (to_worker_tx, to_worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
  let (to_ui_tx, to_ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let join = std::thread::Builder::new()
    .name(name.into())
    .stack_size(DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || run_worker_loop(to_worker_rx, to_ui_tx))?;

  Ok(UiWorkerHandle {
    ui_tx: to_worker_tx,
    ui_rx: to_ui_rx,
    join,
  })
}

fn run_worker_loop(rx: Receiver<UiToWorker>, ui_tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();

  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab { tab_id, initial_url } => {
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

        let tab = TabState {
          renderer,
          viewport_css: (800, 600),
          dpr: 1.0,
          url: None,
          document: None,
          scroll: ScrollState::default(),
        };
        tabs.insert(tab_id, tab);

        if let Some(url) = initial_url {
          if let Some(tab) = tabs.get_mut(&tab_id) {
            navigate_tab(tab_id, tab, &ui_tx, url);
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
        }
      }
      UiToWorker::Navigate {
        tab_id,
        url,
        reason: _,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        navigate_tab(tab_id, tab, &ui_tx, url);
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_ref() else {
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
            let page_point = Point::new(x + current.viewport.x, y + current.viewport.y);
            apply_wheel_scroll_at_point(
              doc.fragment_tree(),
              &current,
              page_point,
              ScrollWheelInput {
                delta_x,
                delta_y,
              },
            )
          }
        };

        let painted = doc.paint_with_options_frame(
          PreparedPaintOptions::new()
            .with_scroll_state(next)
            .with_viewport(tab.viewport_css.0, tab.viewport_css.1),
        );
        let painted = match painted {
          Ok(painted) => painted,
          Err(err) => {
            let _ = ui_tx.send(WorkerToUi::DebugLog {
              tab_id,
              line: format!("paint failed after scroll: {err}"),
            });
            continue;
          }
        };

        if painted.scroll_state != tab.scroll {
          tab.scroll = painted.scroll_state.clone();
          let _ = ui_tx.send(WorkerToUi::ScrollStateUpdated {
            tab_id,
            scroll: painted.scroll_state.clone(),
          });
          let _ = ui_tx.send(WorkerToUi::FrameReady {
            tab_id,
            frame: RenderedFrame {
              pixmap: painted.pixmap,
              viewport_css: tab.viewport_css,
              dpr: tab.dpr,
              scroll_state: painted.scroll_state,
            },
          });
        }
      }
      UiToWorker::PointerMove { .. }
      | UiToWorker::PointerDown { .. }
      | UiToWorker::PointerUp { .. }
      | UiToWorker::TextInput { .. }
      | UiToWorker::KeyAction { .. } => {}
      UiToWorker::RequestRepaint { tab_id, reason: _ } => {
        let Some(tab) = tabs.get_mut(&tab_id) else {
          continue;
        };
        let Some(doc) = tab.document.as_ref() else {
          continue;
        };
        if let Some(scroll) = paint_document(tab_id, tab, doc, &ui_tx, tab.scroll.clone()) {
          tab.scroll = scroll;
        }
      }
    }
  }
}

