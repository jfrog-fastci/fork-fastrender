use crate::api::{FastRender, PreparedDocument, PreparedPaintOptions};
use crate::render_control::{push_stage_listener, StageHeartbeat, StageListenerGuard};
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::messages::{TabId, WorkerToUi};
use crate::{Pixmap, RenderOptions, Result};
use std::sync::mpsc::Sender;
use std::sync::Arc;

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> StageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  push_stage_listener(Some(listener))
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

