//! Browser-side renderer IPC session state machine.
//!
//! This is a small, deterministic helper that tracks:
//! - the browser↔renderer handshake (`Hello`/`HelloAck`)
//! - whether a frame-buffer set has been installed (so `FrameReady` can be validated)
//! - explicit shutdown (`Shutdown`/`ShutdownAck`)

use super::protocol::{
  BrowserToRenderer, FrameBufferSet, RendererToBrowser, RendererToBrowserValidationContext,
  IPC_PROTOCOL_VERSION,
};
use super::IpcError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererSessionState {
  /// No messages have been exchanged yet.
  Init,
  /// Browser sent `Hello`, awaiting `HelloAck`.
  AwaitHelloAck,
  /// Handshake complete, awaiting the first frame buffer set.
  AwaitFrameBuffers,
  /// Frame buffers have been installed; `FrameReady` is accepted.
  Running,
  /// Browser requested shutdown; ignore all messages except `ShutdownAck`.
  Closing,
  /// Renderer acknowledged shutdown.
  Closed,
  /// Renderer reported a crash.
  Crashed,
}

/// Browser-side state machine for a single renderer process session.
#[derive(Debug, Clone)]
pub struct RendererSession {
  state: RendererSessionState,
  expected_protocol_version: u32,
  frame_buffers: Option<FrameBufferSet>,
}

impl Default for RendererSession {
  fn default() -> Self {
    Self::new()
  }
}

impl RendererSession {
  pub fn new() -> Self {
    Self {
      state: RendererSessionState::Init,
      expected_protocol_version: IPC_PROTOCOL_VERSION,
      frame_buffers: None,
    }
  }

  pub fn state(&self) -> RendererSessionState {
    self.state
  }

  pub fn frame_buffers(&self) -> Option<&FrameBufferSet> {
    self.frame_buffers.as_ref()
  }

  fn validation_context(&self) -> RendererToBrowserValidationContext<'_> {
    RendererToBrowserValidationContext {
      expected_protocol_version: self.expected_protocol_version,
      frame_buffers: self.frame_buffers.as_ref(),
    }
  }

  /// Handle a browser→renderer message emitted by upper layers.
  ///
  /// Returns `Ok(Some(msg))` when the message should be sent to the renderer, or `Ok(None)` when the
  /// message is ignored due to the session state (e.g. already closing).
  pub fn handle_browser_to_renderer(
    &mut self,
    msg: BrowserToRenderer,
  ) -> Result<Option<BrowserToRenderer>, IpcError> {
    // Shutdown is always accepted (tab close / browser shutdown) so callers do not need to care
    // about the precise session state.
    if matches!(msg, BrowserToRenderer::Shutdown { .. }) {
      msg.validate()?;
      match self.state {
        RendererSessionState::Closing
        | RendererSessionState::Closed
        | RendererSessionState::Crashed => return Ok(None),
        _ => {}
      }
      self.state = RendererSessionState::Closing;
      return Ok(Some(msg));
    }

    match self.state {
      RendererSessionState::Closing
      | RendererSessionState::Closed
      | RendererSessionState::Crashed => {
        return Ok(None);
      }
      _ => {}
    }

    msg.validate()?;

    match msg {
      BrowserToRenderer::Hello { .. } => {
        if self.state != RendererSessionState::Init {
          return Err(IpcError::InvalidParameters {
            msg: format!("Hello sent in state {:?}", self.state),
          });
        }
        self.state = RendererSessionState::AwaitHelloAck;
        Ok(Some(msg))
      }

      BrowserToRenderer::SetFrameBuffers {
        generation,
        buffers,
      } => {
        match self.state {
          RendererSessionState::AwaitFrameBuffers | RendererSessionState::Running => {}
          other => {
            return Err(IpcError::InvalidParameters {
              msg: format!("SetFrameBuffers sent in state {other:?}"),
            });
          }
        }

        self.frame_buffers = Some(FrameBufferSet {
          generation,
          buffers: buffers.clone(),
        });
        self.state = RendererSessionState::Running;
        Ok(Some(BrowserToRenderer::SetFrameBuffers {
          generation,
          buffers,
        }))
      }

      BrowserToRenderer::FrameAck { frame_seq } => {
        if self.state != RendererSessionState::Running {
          return Err(IpcError::InvalidParameters {
            msg: format!("FrameAck sent in state {:?}", self.state),
          });
        }
        Ok(Some(BrowserToRenderer::FrameAck { frame_seq }))
      }

      BrowserToRenderer::ReleaseFrameBuffer {
        generation,
        buffer_index,
      } => {
        if self.state != RendererSessionState::Running {
          return Err(IpcError::InvalidParameters {
            msg: format!("ReleaseFrameBuffer sent in state {:?}", self.state),
          });
        }
        Ok(Some(BrowserToRenderer::ReleaseFrameBuffer {
          generation,
          buffer_index,
        }))
      }

      BrowserToRenderer::FrameAck { frame_seq } => {
        if self.state != RendererSessionState::Running {
          return Err(IpcError::InvalidParameters {
            msg: format!("FrameAck sent in state {:?}", self.state),
          });
        }
        Ok(Some(BrowserToRenderer::FrameAck { frame_seq }))
      }

      BrowserToRenderer::Shutdown { .. } => unreachable!("handled above"), // fastrender-allow-panic
    }
  }

  /// Handle a renderer→browser message.
  ///
  /// Returns `Ok(Some(msg))` when upper layers should observe the message.
  pub fn handle_renderer_to_browser(
    &mut self,
    msg: RendererToBrowser,
  ) -> Result<Option<RendererToBrowser>, IpcError> {
    match self.state {
      RendererSessionState::Closing => {
        if matches!(msg, RendererToBrowser::ShutdownAck) {
          self.state = RendererSessionState::Closed;
          return Ok(Some(msg));
        }
        return Ok(None);
      }
      RendererSessionState::Closed | RendererSessionState::Crashed => return Ok(None),
      _ => {}
    }

    let ctx = self.validation_context();
    msg.validate(&ctx)?;

    match msg {
      RendererToBrowser::HelloAck { .. } => {
        if self.state != RendererSessionState::AwaitHelloAck {
          return Err(IpcError::ProtocolViolation {
            msg: format!("unexpected HelloAck in state {:?}", self.state),
          });
        }
        self.state = RendererSessionState::AwaitFrameBuffers;
        Ok(Some(msg))
      }

      RendererToBrowser::FrameReady { .. } => {
        if self.state != RendererSessionState::Running {
          return Err(IpcError::ProtocolViolation {
            msg: format!("unexpected FrameReady in state {:?}", self.state),
          });
        }
        Ok(Some(msg))
      }

      RendererToBrowser::Crashed { .. } => {
        self.state = RendererSessionState::Crashed;
        Ok(Some(msg))
      }

      RendererToBrowser::ShutdownAck => Err(IpcError::ProtocolViolation {
        msg: "unexpected ShutdownAck before browser shutdown".to_string(),
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ipc::protocol::{FrameBufferDesc, ScrollMetrics};

  fn mk_desc() -> FrameBufferDesc {
    let desc = FrameBufferDesc {
      buffer_index: 0,
      shmem_id: "buf0".to_string(),
      byte_len: 400 * 100,
      max_width_px: 100,
      max_height_px: 100,
      stride_bytes: 400,
    };
    desc.validate().expect("desc should be valid");
    desc
  }

  fn mk_running_session() -> RendererSession {
    let mut session = RendererSession::new();

    session
      .handle_browser_to_renderer(BrowserToRenderer::Hello {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello should be sent");
    assert_eq!(session.state(), RendererSessionState::AwaitHelloAck);

    session
      .handle_renderer_to_browser(RendererToBrowser::HelloAck {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello ack should be accepted");
    assert_eq!(session.state(), RendererSessionState::AwaitFrameBuffers);

    let desc = mk_desc();
    session
      .handle_browser_to_renderer(BrowserToRenderer::SetFrameBuffers {
        generation: 7,
        buffers: vec![desc],
      })
      .unwrap()
      .expect("set frame buffers should be sent");
    assert_eq!(session.state(), RendererSessionState::Running);

    session
  }

  #[test]
  fn frame_ack_rejected_before_running() {
    let mut session = RendererSession::new();

    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 1 })
      .expect_err("frame ack in Init should be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));

    session
      .handle_browser_to_renderer(BrowserToRenderer::Hello {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello should be sent");
    assert_eq!(session.state(), RendererSessionState::AwaitHelloAck);

    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 2 })
      .expect_err("frame ack during handshake should be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));

    session
      .handle_renderer_to_browser(RendererToBrowser::HelloAck {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello ack should be accepted");
    assert_eq!(session.state(), RendererSessionState::AwaitFrameBuffers);

    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 3 })
      .expect_err("frame ack before buffers installed should be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));
  }

  #[test]
  fn frame_ack_allowed_in_running_and_ignored_when_closing_or_closed() {
    let mut session = mk_running_session();

    let msg = BrowserToRenderer::FrameAck { frame_seq: 123 };
    let sent = session
      .handle_browser_to_renderer(msg.clone())
      .unwrap()
      .expect("frame ack should be sent");
    assert_eq!(sent, msg);
    assert_eq!(session.state(), RendererSessionState::Running);

    session
      .handle_browser_to_renderer(BrowserToRenderer::Shutdown { reason: None })
      .unwrap()
      .expect("shutdown should be sent");
    assert_eq!(session.state(), RendererSessionState::Closing);

    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 124 })
      .unwrap();
    assert!(ignored.is_none());

    let ack = session
      .handle_renderer_to_browser(RendererToBrowser::ShutdownAck)
      .unwrap();
    assert!(matches!(ack, Some(RendererToBrowser::ShutdownAck)));
    assert_eq!(session.state(), RendererSessionState::Closed);

    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 125 })
      .unwrap();
    assert!(ignored.is_none());
  }

  #[test]
  fn shutdown_before_buffers_installed() {
    let mut session = RendererSession::new();

    session
      .handle_browser_to_renderer(BrowserToRenderer::Hello {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello should be sent");
    assert_eq!(session.state(), RendererSessionState::AwaitHelloAck);

    session
      .handle_renderer_to_browser(RendererToBrowser::HelloAck {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello ack should be accepted");
    assert_eq!(session.state(), RendererSessionState::AwaitFrameBuffers);

    session
      .handle_browser_to_renderer(BrowserToRenderer::Shutdown {
        reason: Some("tab closed".to_string()),
      })
      .unwrap()
      .expect("shutdown should be sent");
    assert_eq!(session.state(), RendererSessionState::Closing);

    // Further browser messages are ignored once closing.
    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::SetFrameBuffers {
        generation: 1,
        buffers: vec![mk_desc()],
      })
      .unwrap();
    assert!(ignored.is_none());

    // Renderer messages are ignored except for ShutdownAck.
    let ignored = session
      .handle_renderer_to_browser(RendererToBrowser::Crashed {
        reason: "ignored".to_string(),
      })
      .unwrap();
    assert!(ignored.is_none());

    let ack = session
      .handle_renderer_to_browser(RendererToBrowser::ShutdownAck)
      .unwrap();
    assert!(matches!(ack, Some(RendererToBrowser::ShutdownAck)));
    assert_eq!(session.state(), RendererSessionState::Closed);
  }

  #[test]
  fn shutdown_during_running() {
    let mut session = mk_running_session();
    let buffers = session.frame_buffers().expect("buffers installed").clone();

    session
      .handle_browser_to_renderer(BrowserToRenderer::Shutdown { reason: None })
      .unwrap()
      .expect("shutdown should be sent");
    assert_eq!(session.state(), RendererSessionState::Closing);

    // A concurrent FrameReady should be ignored once closing.
    let ignored = session
      .handle_renderer_to_browser(RendererToBrowser::FrameReady {
        generation: buffers.generation,
        buffer_index: 0,
        width_px: 10,
        height_px: 10,
        viewport_css: (10, 10),
        dpr: 1.0,
        scroll_metrics: ScrollMetrics::default(),
        wants_ticks: false,
      })
      .unwrap();
    assert!(ignored.is_none());

    let ack = session
      .handle_renderer_to_browser(RendererToBrowser::ShutdownAck)
      .unwrap();
    assert!(matches!(ack, Some(RendererToBrowser::ShutdownAck)));
    assert_eq!(session.state(), RendererSessionState::Closed);
  }

  #[test]
  fn frame_ack_is_only_sent_while_running() {
    let mut session = RendererSession::new();

    // Before running, FrameAck is a caller bug.
    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 1 })
      .expect_err("expected FrameAck before running to be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));

    session
      .handle_browser_to_renderer(BrowserToRenderer::Hello {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello should be sent");
    assert_eq!(session.state(), RendererSessionState::AwaitHelloAck);

    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 2 })
      .expect_err("expected FrameAck before buffers installed to be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));

    session
      .handle_renderer_to_browser(RendererToBrowser::HelloAck {
        protocol_version: IPC_PROTOCOL_VERSION,
      })
      .unwrap()
      .expect("hello ack should be accepted");
    assert_eq!(session.state(), RendererSessionState::AwaitFrameBuffers);

    let err = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 3 })
      .expect_err("expected FrameAck before running to be rejected");
    assert!(matches!(err, IpcError::InvalidParameters { .. }));

    let mut session = mk_running_session();
    let sent = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 9 })
      .unwrap();
    assert_eq!(sent, Some(BrowserToRenderer::FrameAck { frame_seq: 9 }));
    assert_eq!(session.state(), RendererSessionState::Running);
  }

  #[test]
  fn frame_ack_is_ignored_when_closing_closed_or_crashed() {
    let mut session = mk_running_session();

    session
      .handle_browser_to_renderer(BrowserToRenderer::Shutdown { reason: None })
      .unwrap()
      .expect("shutdown should be sent");
    assert_eq!(session.state(), RendererSessionState::Closing);

    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 1 })
      .unwrap();
    assert!(ignored.is_none());

    // Once the renderer acks shutdown, we're fully closed; FrameAck is still ignored.
    session
      .handle_renderer_to_browser(RendererToBrowser::ShutdownAck)
      .unwrap()
      .expect("shutdown ack should transition to closed");
    assert_eq!(session.state(), RendererSessionState::Closed);

    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 2 })
      .unwrap();
    assert!(ignored.is_none());

    // Crashed sessions also ignore further browser messages.
    let mut session = mk_running_session();
    session
      .handle_renderer_to_browser(RendererToBrowser::Crashed {
        reason: "boom".to_string(),
      })
      .unwrap()
      .expect("crash should be observed");
    assert_eq!(session.state(), RendererSessionState::Crashed);

    let ignored = session
      .handle_browser_to_renderer(BrowserToRenderer::FrameAck { frame_seq: 3 })
      .unwrap();
    assert!(ignored.is_none());
  }
}
