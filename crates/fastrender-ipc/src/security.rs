use crate::{FrameBuffer, FrameId, RendererId, RendererToBrowser};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererToBrowserKind {
  FrameReady,
  NavigationCommitted,
  NavigationFailed,
  InputAck,
  Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameOwnershipViolation {
  UnknownFrame { frame_id: FrameId },
  NotOwner {
    frame_id: FrameId,
    expected: RendererId,
    actual: RendererId,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcSecurityEvent {
  ProtocolViolation {
    process_id: RendererId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
    violation: FrameOwnershipViolation,
  },
  ProcessTerminated {
    process_id: RendererId,
  },
}

/// Minimal browser-side state to enforce the `FrameId -> RendererProcessId` capability boundary.
///
/// In the real browser, `frame_owner` is part of the FrameTree/registry and `terminate_process`
/// should kill/disconnect the renderer. This type exists to make the security boundary explicit and
/// unit-testable.
#[derive(Debug, Default)]
pub struct BrowserIpcSecurityState {
  frame_owner: HashMap<FrameId, RendererId>,
  process_frames: HashMap<RendererId, HashSet<FrameId>>,
  terminated_processes: HashSet<RendererId>,
  crashed_frames: HashSet<FrameId>,
  events: Vec<IpcSecurityEvent>,

  /// Simulated UI state: latest frame buffer per frame.
  latest_frame: HashMap<FrameId, FrameBuffer>,
}

impl BrowserIpcSecurityState {
  pub fn is_process_terminated(&self, process_id: RendererId) -> bool {
    self.terminated_processes.contains(&process_id)
  }

  pub fn is_frame_crashed(&self, frame_id: FrameId) -> bool {
    self.crashed_frames.contains(&frame_id)
  }

  pub fn latest_frame(&self, frame_id: FrameId) -> Option<&FrameBuffer> {
    self.latest_frame.get(&frame_id)
  }

  pub fn take_events(&mut self) -> Vec<IpcSecurityEvent> {
    std::mem::take(&mut self.events)
  }

  pub fn assign_frame(&mut self, frame_id: FrameId, process_id: RendererId) {
    if let Some(prev) = self.frame_owner.insert(frame_id, process_id) {
      if prev != process_id {
        if let Some(frames) = self.process_frames.get_mut(&prev) {
          frames.remove(&frame_id);
          if frames.is_empty() {
            self.process_frames.remove(&prev);
          }
        }
      }
    }

    self
      .process_frames
      .entry(process_id)
      .or_default()
      .insert(frame_id);
    self.crashed_frames.remove(&frame_id);
  }

  pub fn handle_renderer_message(&mut self, sender: RendererId, msg: RendererToBrowser) {
    if self.terminated_processes.contains(&sender) {
      return;
    }

    match msg {
      RendererToBrowser::FrameReady { frame_id, buffer, .. } => {
        if self.check_frame(sender, frame_id, RendererToBrowserKind::FrameReady) {
          self.latest_frame.insert(frame_id, buffer);
        }
      }
      RendererToBrowser::NavigationCommitted { frame_id, .. } => {
        let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::NavigationCommitted);
      }
      RendererToBrowser::NavigationFailed { frame_id, .. } => {
        let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::NavigationFailed);
      }
      RendererToBrowser::InputAck { frame_id, .. } => {
        let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::InputAck);
      }
      RendererToBrowser::Error { frame_id, .. } => {
        if let Some(frame_id) = frame_id {
          let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::Error);
        }
      }
    }
  }

  fn check_frame(
    &mut self,
    sender: RendererId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
  ) -> bool {
    match self.frame_owner.get(&frame_id).copied() {
      Some(owner) if owner == sender => true,
      Some(owner) => {
        self.protocol_violation(
          sender,
          frame_id,
          message,
          FrameOwnershipViolation::NotOwner {
            frame_id,
            expected: owner,
            actual: sender,
          },
        );
        false
      }
      None => {
        self.protocol_violation(
          sender,
          frame_id,
          message,
          FrameOwnershipViolation::UnknownFrame { frame_id },
        );
        false
      }
    }
  }

  fn protocol_violation(
    &mut self,
    sender: RendererId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
    violation: FrameOwnershipViolation,
  ) {
    self.events.push(IpcSecurityEvent::ProtocolViolation {
      process_id: sender,
      frame_id,
      message,
      violation,
    });
    self.terminate_process(sender);
  }

  fn terminate_process(&mut self, process_id: RendererId) {
    if !self.terminated_processes.insert(process_id) {
      return;
    }

    if let Some(frames) = self.process_frames.remove(&process_id) {
      for frame_id in frames {
        self.frame_owner.remove(&frame_id);
        self.latest_frame.remove(&frame_id);
        self.crashed_frames.insert(frame_id);
      }
    }

    self.events.push(IpcSecurityEvent::ProcessTerminated { process_id });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn spoofed_frame_ready_kills_sender_and_does_not_mutate_ui_state() {
    let mut browser = BrowserIpcSecurityState::default();
    let honest = RendererId(1);
    let attacker = RendererId(2);
    let frame = FrameId(99);
    browser.assign_frame(frame, honest);

    let buffer = FrameBuffer {
      width: 1,
      height: 1,
      rgba8: vec![0, 0, 0, 255],
    };
    browser.handle_renderer_message(
      attacker,
      RendererToBrowser::FrameReady {
        frame_id: frame,
        buffer,
        subframes: vec![],
      },
    );

    assert!(browser.is_process_terminated(attacker));
    assert!(
      !browser.is_process_terminated(honest),
      "protocol violation should not affect other processes"
    );
    assert!(
      !browser.is_frame_crashed(frame),
      "protocol violation should not mark unrelated frames as crashed"
    );
    assert!(
      browser.latest_frame(frame).is_none(),
      "protocol violation should not update presentation state"
    );

    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation { process_id, frame_id, message: RendererToBrowserKind::FrameReady, .. }
          if *process_id == attacker && *frame_id == frame
      )),
      "expected protocol violation to be recorded (events={events:?})"
    );
  }
}
