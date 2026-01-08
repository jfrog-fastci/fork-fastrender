use crate::api::{FastRender, PreparedDocument, PreparedPaintOptions};
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::system::DEFAULT_RENDER_STACK_SIZE;
use crate::ui::messages::{TabId, WorkerToUi};
use crate::{Pixmap, RenderOptions, Result};
use std::sync::mpsc::Sender;
use std::sync::Arc;

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
