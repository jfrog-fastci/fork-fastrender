use crate::api::FastRender;
use crate::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use crate::ui::about_pages;
use crate::ui::messages::{RenderedFrame, TabId, WorkerToUi};
use crate::{PreparedPaintOptions, RenderOptions, Result};
use std::sync::mpsc::Sender;
use std::sync::Arc;

fn forward_stage_heartbeats(tab_id: TabId, sender: Sender<WorkerToUi>) -> GlobalStageListenerGuard {
  let listener = Arc::new(move |stage: StageHeartbeat| {
    // Best-effort: UI might have dropped its receiver.
    let _ = sender.send(WorkerToUi::Stage { tab_id, stage });
  });
  GlobalStageListenerGuard::new(listener)
}

pub struct BrowserWorker {
  renderer: FastRender,
  ui_tx: Sender<WorkerToUi>,
}

impl BrowserWorker {
  pub fn new(renderer: FastRender, ui_tx: Sender<WorkerToUi>) -> Self {
    Self { renderer, ui_tx }
  }

  /// Navigate and synchronously render one frame.
  ///
  /// On navigation errors, the worker tries to render `about:error` with the error message.
  pub fn navigate(&mut self, tab_id: TabId, url: &str, options: RenderOptions) -> Result<()> {
    let url = url.trim();
    let _guard = forward_stage_heartbeats(tab_id, self.ui_tx.clone());

    let report = if about_pages::is_about_url(url) {
      self.prepare_about_url(url, options.clone())?
    } else {
      match self.renderer.prepare_url(url, options.clone()) {
        Ok(report) => report,
        Err(err) => {
          let html = about_pages::error_page_html("Navigation failed", &err.to_string());
          self.prepare_about_html(about_pages::ABOUT_ERROR, &html, options.clone())?
        }
      }
    };

    // Best-effort: surface JS errors/console output in the UI debug log so pages can be debugged
    // without attaching a debugger.
    for exception in &report.diagnostics.js_exceptions {
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
    for message in &report.diagnostics.console_messages {
      let _ = self.ui_tx.send(WorkerToUi::DebugLog {
        tab_id,
        line: format!(
          "Console[{}]: {}",
          message.level.as_str(),
          message.message
        ),
      });
    }

    let painted = report.document.paint_with_options_frame(PreparedPaintOptions {
      scroll: None,
      viewport: None,
      background: None,
      animation_time: options.animation_time,
    })?;
    let viewport_css = options.viewport.unwrap_or_else(|| {
      let size = report.document.layout_viewport();
      (size.width.round() as u32, size.height.round() as u32)
    });

    let _ = self.ui_tx.send(WorkerToUi::FrameReady {
      tab_id,
      frame: RenderedFrame {
        pixmap: painted.pixmap,
        viewport_css,
        dpr: report.document.device_pixel_ratio(),
        scroll_state: painted.scroll_state,
      },
    });

    Ok(())
  }

  fn prepare_about_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<crate::PreparedDocumentReport> {
    let html = about_pages::html_for_about_url(url).unwrap_or_else(|| {
      about_pages::error_page_html("Unknown about page", &format!("Unknown URL: {url}"))
    });
    self.prepare_about_html(url, &html, options)
  }

  fn prepare_about_html(
    &mut self,
    document_url: &str,
    html: &str,
    options: RenderOptions,
  ) -> Result<crate::PreparedDocumentReport> {
    self.renderer.set_base_url(about_pages::ABOUT_BASE_URL);
    let dom = self.renderer.parse_html(html)?;
    self
      .renderer
      .prepare_dom_with_options(dom, Some(document_url), options)
  }
}

#[cfg(test)]
mod tests {
  use super::BrowserWorker;
  use crate::render_control::StageHeartbeat;
  use crate::ui::messages::{TabId, WorkerToUi};
  use crate::{FastRender, RenderOptions};
  use std::time::Duration;

  #[test]
  fn about_blank_navigation_does_not_fetch_document() {
    let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
    let renderer = FastRender::new().unwrap();
    let mut worker = BrowserWorker::new(renderer, tx);

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
