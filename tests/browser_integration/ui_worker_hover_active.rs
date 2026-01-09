#![cfg(feature = "browser_ui")]

use fastrender::geometry::Point;
use fastrender::interaction::InteractionEngine;
use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::{BrowserDocument, FastRender, RenderOptions};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

struct TabState {
  viewport_css: (u32, u32),
  dpr: f32,
  base_url: Option<String>,
  doc: Option<BrowserDocument>,
  interaction: InteractionEngine,
  scroll_state: ScrollState,
}

impl TabState {
  fn new() -> Self {
    Self {
      viewport_css: (0, 0),
      dpr: 1.0,
      base_url: None,
      doc: None,
      interaction: InteractionEngine::new(),
      scroll_state: ScrollState::default(),
    }
  }
}

/// Minimal headless worker loop for browser-ui integration tests.
///
/// This is intentionally tiny: it only supports the subset of the UI protocol needed by the hover
/// + active tests (navigate, pointer move/down/up).
fn worker_loop(rx: Receiver<UiToWorker>, tx: Sender<WorkerToUi>) {
  let mut tabs: HashMap<TabId, TabState> = HashMap::new();
  while let Ok(msg) = rx.recv() {
    match msg {
      UiToWorker::CreateTab { tab_id, initial_url } => {
        tabs.insert(tab_id, TabState::new());
        if let Some(url) = initial_url {
          let _ = handle_navigation(&mut tabs, &tx, tab_id, url);
        }
      }
      UiToWorker::CloseTab { tab_id } => {
        tabs.remove(&tab_id);
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        if let Some(tab) = tabs.get_mut(&tab_id) {
          tab.viewport_css = viewport_css;
          tab.dpr = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
          if let Some(doc) = tab.doc.as_mut() {
            doc.set_viewport(viewport_css.0, viewport_css.1);
          }
        }
      }
      UiToWorker::Navigate { tab_id, url, .. } => {
        let _ = handle_navigation(&mut tabs, &tx, tab_id, url);
      }
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        let _ = handle_pointer_move(&mut tabs, &tx, tab_id, pos_css);
      }
      UiToWorker::PointerDown { tab_id, pos_css, .. } => {
        let _ = handle_pointer_down(&mut tabs, &tx, tab_id, pos_css);
      }
      UiToWorker::PointerUp { tab_id, pos_css, .. } => {
        let _ = handle_pointer_up(&mut tabs, &tx, tab_id, pos_css);
      }
      _ => {}
    }
  }
}

fn handle_navigation(
  tabs: &mut HashMap<TabId, TabState>,
  tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  url: String,
) -> Result<(), String> {
  let tab = tabs
    .get_mut(&tab_id)
    .ok_or_else(|| format!("navigate for unknown tab: {tab_id:?}"))?;
  let (w, h) = tab.viewport_css;
  let options = RenderOptions::new()
    .with_viewport(w.max(1), h.max(1))
    .with_device_pixel_ratio(tab.dpr);

  // Use a fresh renderer per navigation (matching the "one renderer per tab" UI intent).
  let mut renderer = FastRender::builder()
    .base_url(url.clone())
    .build()
    .map_err(|err| err.to_string())?;
  let report = renderer
    .prepare_url(&url, options.clone())
    .map_err(|err| err.to_string())?;

  tab.base_url = report.base_url.clone().or_else(|| Some(url.clone()));
  tab.interaction = InteractionEngine::new();

  let mut doc = BrowserDocument::from_prepared(renderer, report.document, options)
    .map_err(|err| err.to_string())?;
  let painted = doc
    .render_frame_with_scroll_state()
    .map_err(|err| err.to_string())?;

  tab.scroll_state = painted.scroll_state.clone();
  tab.doc = Some(doc);

  tx.send(WorkerToUi::FrameReady {
    tab_id,
    frame: RenderedFrame {
      pixmap: painted.pixmap,
      viewport_css: tab.viewport_css,
      dpr: tab.dpr,
      scroll_state: painted.scroll_state,
    },
  })
  .map_err(|err| err.to_string())?;

  Ok(())
}

fn handle_pointer_move(
  tabs: &mut HashMap<TabId, TabState>,
  tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  pos_css: (f32, f32),
) -> Result<(), String> {
  let tab = tabs
    .get_mut(&tab_id)
    .ok_or_else(|| format!("pointer move for unknown tab: {tab_id:?}"))?;
  let Some(doc) = tab.doc.as_mut() else {
    return Ok(());
  };

  let (box_tree, fragment_tree) = {
    let prepared = doc
      .prepared()
      .ok_or_else(|| "pointer move before initial render".to_string())?;
    (prepared.box_tree().clone(), prepared.fragment_tree().clone())
  };
  let page_point = Point::new(
    pos_css.0 + tab.scroll_state.viewport.x,
    pos_css.1 + tab.scroll_state.viewport.y,
  );

  let changed = doc.mutate_dom(|dom| {
    tab
      .interaction
      .pointer_move(dom, &box_tree, &fragment_tree, page_point)
  });

  if !changed {
    return Ok(());
  }

  if let Some(painted) = doc
    .render_if_needed_with_scroll_state()
    .map_err(|err| err.to_string())?
  {
    tab.scroll_state = painted.scroll_state.clone();
    tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: tab.dpr,
        scroll_state: painted.scroll_state,
      },
    })
    .map_err(|err| err.to_string())?;
  }

  Ok(())
}

fn handle_pointer_down(
  tabs: &mut HashMap<TabId, TabState>,
  tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  pos_css: (f32, f32),
) -> Result<(), String> {
  let tab = tabs
    .get_mut(&tab_id)
    .ok_or_else(|| format!("pointer down for unknown tab: {tab_id:?}"))?;
  let Some(doc) = tab.doc.as_mut() else {
    return Ok(());
  };

  let (box_tree, fragment_tree) = {
    let prepared = doc
      .prepared()
      .ok_or_else(|| "pointer down before initial render".to_string())?;
    (prepared.box_tree().clone(), prepared.fragment_tree().clone())
  };
  let page_point = Point::new(
    pos_css.0 + tab.scroll_state.viewport.x,
    pos_css.1 + tab.scroll_state.viewport.y,
  );

  let changed =
    doc.mutate_dom(|dom| tab.interaction.pointer_down(dom, &box_tree, &fragment_tree, page_point));

  if !changed {
    return Ok(());
  }

  if let Some(painted) = doc
    .render_if_needed_with_scroll_state()
    .map_err(|err| err.to_string())?
  {
    tab.scroll_state = painted.scroll_state.clone();
    tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: tab.dpr,
        scroll_state: painted.scroll_state,
      },
    })
    .map_err(|err| err.to_string())?;
  }

  Ok(())
}

fn handle_pointer_up(
  tabs: &mut HashMap<TabId, TabState>,
  tx: &Sender<WorkerToUi>,
  tab_id: TabId,
  pos_css: (f32, f32),
) -> Result<(), String> {
  let tab = tabs
    .get_mut(&tab_id)
    .ok_or_else(|| format!("pointer up for unknown tab: {tab_id:?}"))?;
  let Some(doc) = tab.doc.as_mut() else {
    return Ok(());
  };

  let (box_tree, fragment_tree) = {
    let prepared = doc
      .prepared()
      .ok_or_else(|| "pointer up before initial render".to_string())?;
    (prepared.box_tree().clone(), prepared.fragment_tree().clone())
  };
  let page_point = Point::new(
    pos_css.0 + tab.scroll_state.viewport.x,
    pos_css.1 + tab.scroll_state.viewport.y,
  );
  let base_url = tab
    .base_url
    .as_deref()
    .ok_or_else(|| "pointer up before navigation base_url set".to_string())?;

  let changed = doc.mutate_dom(|dom| {
    let (dom_changed, _action) =
      tab
        .interaction
        .pointer_up(dom, &box_tree, &fragment_tree, page_point, base_url);
    dom_changed
  });

  if !changed {
    return Ok(());
  }

  if let Some(painted) = doc
    .render_if_needed_with_scroll_state()
    .map_err(|err| err.to_string())?
  {
    tab.scroll_state = painted.scroll_state.clone();
    tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css: tab.viewport_css,
        dpr: tab.dpr,
        scroll_state: painted.scroll_state,
      },
    })
    .map_err(|err| err.to_string())?;
  }

  Ok(())
}

struct WorkerHarness {
  tx: Option<Sender<UiToWorker>>,
  rx: Receiver<WorkerToUi>,
  join: Option<std::thread::JoinHandle<()>>,
}

impl WorkerHarness {
  fn new() -> Self {
    let (ui_tx, worker_rx) = std::sync::mpsc::channel::<UiToWorker>();
    let (worker_tx, ui_rx) = std::sync::mpsc::channel::<WorkerToUi>();

    let join = std::thread::spawn(move || worker_loop(worker_rx, worker_tx));
    Self {
      tx: Some(ui_tx),
      rx: ui_rx,
      join: Some(join),
    }
  }

  fn send(&self, msg: UiToWorker) {
    self
      .tx
      .as_ref()
      .expect("worker sender")
      .send(msg)
      .expect("send UiToWorker");
  }

  fn recv_frame(&self, tab_id: TabId) -> RenderedFrame {
    let start = Instant::now();
    loop {
      let remaining = FRAME_TIMEOUT.saturating_sub(start.elapsed());
      if remaining.is_zero() {
        panic!("timed out waiting for FrameReady");
      }

      match self.rx.recv_timeout(remaining) {
        Ok(msg) => match msg {
          WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => return frame,
          WorkerToUi::NavigationFailed { tab_id: got, url, error } if got == tab_id => {
            panic!("navigation failed for {url}: {error}");
          }
          _ => continue,
        },
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
          panic!("timed out waiting for FrameReady");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
          panic!("worker channel disconnected before FrameReady");
        }
      }
    }
  }
}

impl Drop for WorkerHarness {
  fn drop(&mut self) {
    drop(self.tx.take());
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

fn write_fixture() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("tempdir");
  let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #box { width: 64px; height: 64px; background: rgb(255,0,0); }
      #box[data-fastr-hover="true"] { background: rgb(0,255,0); }
      #box[data-fastr-active="true"] { background: rgb(0,0,255); }
    </style>
  </head>
  <body>
    <div id="box"></div>
  </body>
</html>
"#;
  let path = dir.path().join("index.html");
  std::fs::write(&path, html).expect("write index.html");
  let url = Url::from_file_path(&path)
    .expect("file url")
    .to_string();
  (dir, url)
}

fn assert_pixel_rgb(frame: &RenderedFrame, x: u32, y: u32, expected: (u8, u8, u8)) {
  let pixel = frame.pixmap.pixel(x, y).unwrap_or_else(|| {
    panic!(
      "missing pixel at ({x},{y}) in {}x{}",
      frame.pixmap.width(),
      frame.pixmap.height()
    )
  });
  let got = (pixel.red(), pixel.green(), pixel.blue());
  assert_eq!(got, expected, "unexpected pixel at ({x},{y})");
  assert_eq!(pixel.alpha(), 0xFF, "expected opaque pixel at ({x},{y})");
}

#[test]
fn pointer_move_sets_hover_and_repaints() {
  let (_dir, url) = write_fixture();

  let tab_id = TabId(1);
  let worker = WorkerHarness::new();
  worker.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
  });
  worker.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (256, 256),
    dpr: 1.0,
  });
  worker.send(UiToWorker::Navigate {
    tab_id,
    url,
    reason: NavigationReason::TypedUrl,
  });

  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (255, 0, 0));

  worker.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::None,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (0, 255, 0));

  worker.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: (200.0, 200.0),
    button: PointerButton::None,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (255, 0, 0));
}

#[test]
fn pointer_down_sets_active_until_pointer_up() {
  let (_dir, url) = write_fixture();

  let tab_id = TabId(1);
  let worker = WorkerHarness::new();
  worker.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
  });
  worker.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (256, 256),
    dpr: 1.0,
  });
  worker.send(UiToWorker::Navigate {
    tab_id,
    url,
    reason: NavigationReason::TypedUrl,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (255, 0, 0));

  // Ensure hover is on before asserting active -> back to hover deterministically.
  worker.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::None,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (0, 255, 0));

  worker.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (0, 0, 255));

  worker.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  });
  let frame = worker.recv_frame(tab_id);
  assert_pixel_rgb(&frame, 10, 10, (0, 255, 0));
}
