use crate::{FrameBuffer, FrameId, RendererId, RendererToBrowser, SubframeInfo};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererToBrowserKind {
  FrameReady,
  FramePaintPlan,
  SubframesDiscovered,
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
  InvalidSubframe {
    parent_frame_id: FrameId,
    child_frame_id: FrameId,
    expected_parent_frame_id: Option<FrameId>,
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

/// Minimal browser-side state to enforce the `FrameId -> renderer process` capability boundary.
///
/// In the real browser, `frame_owner` is part of the FrameTree/registry and `terminate_process`
/// should kill/disconnect the renderer. This type exists to make the security boundary explicit and
/// unit-testable.
#[derive(Debug, Default)]
pub struct BrowserIpcSecurityState {
  frame_owner: HashMap<FrameId, RendererId>,
  frame_parent: HashMap<FrameId, FrameId>,
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

  /// Set the browser-owned parent relationship for `child`.
  ///
  /// This is part of the browser's frame tree state, not renderer authority. Renderer messages that
  /// reference subframes should be validated against this mapping.
  pub fn set_frame_parent(&mut self, child: FrameId, parent: FrameId) {
    self.frame_parent.insert(child, parent);
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
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        subframes,
      } => {
        if !self.check_frame(sender, frame_id, RendererToBrowserKind::FrameReady) {
          return;
        }
        if !self.check_subframes(sender, frame_id, &subframes, RendererToBrowserKind::FrameReady) {
          return;
        }
        self.latest_frame.insert(frame_id, buffer);
      }
      RendererToBrowser::FramePaintPlan(plan) => {
        let frame_id = plan.frame_id;
        if !self.check_frame(sender, frame_id, RendererToBrowserKind::FramePaintPlan) {
          return;
        }
        if !self.check_subframes(
          sender,
          frame_id,
          &plan.slots,
          RendererToBrowserKind::FramePaintPlan,
        ) {
          return;
        }
        // Simulated presentation state: flatten the embedder layers, treating any missing child
        // surfaces as transparent.
        if let Ok(buffer) = crate::composite_paint_plan(
          plan,
          std::iter::empty::<(&crate::SubframeInfo, &crate::FrameBuffer)>(),
        ) {
          self.latest_frame.insert(frame_id, buffer);
        }
      }
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id,
        subframes,
      } => {
        if !self.check_frame(
          sender,
          parent_frame_id,
          RendererToBrowserKind::SubframesDiscovered,
        ) {
          return;
        }
        let _ = self.check_subframes(
          sender,
          parent_frame_id,
          &subframes,
          RendererToBrowserKind::SubframesDiscovered,
        );
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

  fn check_subframes(
    &mut self,
    sender: RendererId,
    parent_frame_id: FrameId,
    subframes: &[SubframeInfo],
    message: RendererToBrowserKind,
  ) -> bool {
    for subframe in subframes {
      let child = subframe.child;
      let expected_parent = self.frame_parent.get(&child).copied();
      if expected_parent != Some(parent_frame_id) {
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::InvalidSubframe {
            parent_frame_id,
            child_frame_id: child,
            expected_parent_frame_id: expected_parent,
          },
        );
        return false;
      }
    }
    true
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

  #[test]
  fn spoofed_subframes_discovered_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let honest = RendererId(1);
    let attacker = RendererId(2);
    let frame = FrameId(123);
    browser.assign_frame(frame, honest);

    browser.handle_renderer_message(
      attacker,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: frame,
        subframes: vec![],
      },
    );

    assert!(browser.is_process_terminated(attacker));
    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation { process_id, frame_id, message: RendererToBrowserKind::SubframesDiscovered, .. }
          if *process_id == attacker && *frame_id == frame
      )),
      "expected protocol violation to be recorded (events={events:?})"
    );
  }

  #[test]
  fn unknown_frame_id_kills_sender_and_marks_its_frames_crashed() {
    let mut browser = BrowserIpcSecurityState::default();
    let attacker = RendererId(42);

    // Attacker owns one legitimate frame.
    let owned = FrameId(10);
    browser.assign_frame(owned, attacker);

    // But attempts to send a message for an unknown frame id.
    let unknown = FrameId(999);
    browser.handle_renderer_message(
      attacker,
      RendererToBrowser::NavigationCommitted {
        frame_id: unknown,
        url: "https://example.test/".to_string(),
        csp: Vec::new(),
      },
    );

    assert!(browser.is_process_terminated(attacker));
    assert!(
      browser.is_frame_crashed(owned),
      "all frames owned by the terminated process should be marked crashed"
    );

    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::NavigationCommitted,
          ..
        } if *process_id == attacker && *frame_id == unknown
      )),
      "expected protocol violation to be recorded (events={events:?})"
    );
  }

  #[test]
  fn messages_from_terminated_process_are_ignored() {
    let mut browser = BrowserIpcSecurityState::default();
    let attacker = RendererId(7);
    let owned = FrameId(1);
    browser.assign_frame(owned, attacker);

    // Trigger termination by spoofing another frame id.
    browser.handle_renderer_message(
      attacker,
      RendererToBrowser::InputAck {
        frame_id: FrameId(999),
        seq: 1,
      },
    );
    assert!(browser.is_process_terminated(attacker));
    browser.take_events();

    // A subsequent message from the terminated process should be ignored (no new events).
    browser.handle_renderer_message(
      attacker,
      RendererToBrowser::FrameReady {
        frame_id: owned,
        buffer: FrameBuffer {
          width: 1,
          height: 1,
          rgba8: vec![1, 2, 3, 4],
        },
        subframes: vec![],
      },
    );

    assert!(
      browser.take_events().is_empty(),
      "expected terminated process messages to be ignored"
    );
  }

  #[test]
  fn frame_ready_cannot_reference_non_child_subframes() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let other = RendererId(2);

    let parent = FrameId(10);
    let unrelated_child = FrameId(11);

    browser.assign_frame(parent, renderer);
    browser.assign_frame(unrelated_child, other);
    // Record that the child's expected parent is some other frame (not `parent`).
    browser.set_frame_parent(unrelated_child, FrameId(999));

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: FrameBuffer {
          width: 1,
          height: 1,
          rgba8: vec![0, 0, 0, 255],
        },
        subframes: vec![crate::SubframeInfo {
          child: unrelated_child,
          src: None,
          transform: crate::AffineTransform::IDENTITY,
          clip_stack: Vec::new(),
          z_index: 0,
          hit_testable: true,
          referrer_policy: None,
          sandbox_flags: crate::SandboxFlags::NONE,
          opaque_origin: false,
        }],
      },
    );

    assert!(browser.is_process_terminated(renderer));
    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::FrameReady,
          violation: FrameOwnershipViolation::InvalidSubframe { child_frame_id, .. },
        } if *process_id == renderer && *frame_id == parent && *child_frame_id == unrelated_child
      )),
      "expected invalid subframe reference violation (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_valid_subframes_updates_presentation_state() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);

    let parent = FrameId(1);
    let child = FrameId(2);
    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: FrameBuffer {
          width: 1,
          height: 1,
          rgba8: vec![9, 8, 7, 6],
        },
        subframes: vec![crate::SubframeInfo {
          child,
          src: None,
          transform: crate::AffineTransform::IDENTITY,
          clip_stack: Vec::new(),
          z_index: 0,
          hit_testable: true,
          referrer_policy: None,
          sandbox_flags: crate::SandboxFlags::NONE,
          opaque_origin: false,
        }],
      },
    );

    assert!(!browser.is_process_terminated(renderer));
    assert!(
      browser.latest_frame(parent).is_some(),
      "expected valid FrameReady to update presentation state"
    );
    assert!(
      browser.take_events().is_empty(),
      "expected no protocol events for valid FrameReady"
    );
  }

  #[test]
  fn subframes_discovered_rejects_invalid_child_parent() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);

    let parent = FrameId(10);
    let child = FrameId(11);
    browser.assign_frame(parent, renderer);
    // Browser frame tree says `child` belongs elsewhere.
    browser.set_frame_parent(child, FrameId(999));

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::SubframeInfo {
          child,
          src: None,
          transform: crate::AffineTransform::IDENTITY,
          clip_stack: Vec::new(),
          z_index: 0,
          hit_testable: true,
          referrer_policy: None,
          sandbox_flags: crate::SandboxFlags::NONE,
          opaque_origin: false,
        }],
      },
    );

    assert!(browser.is_process_terminated(renderer));
    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::SubframesDiscovered,
          violation: FrameOwnershipViolation::InvalidSubframe { child_frame_id, .. },
        } if *process_id == renderer && *frame_id == parent && *child_frame_id == child
      )),
      "expected invalid subframe reference violation (events={events:?})"
    );
  }

  #[test]
  fn subframes_discovered_accepts_valid_children() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);

    let parent = FrameId(10);
    let child = FrameId(11);
    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::SubframeInfo {
          child,
          src: None,
          transform: crate::AffineTransform::IDENTITY,
          clip_stack: Vec::new(),
          z_index: 0,
          hit_testable: true,
          referrer_policy: None,
          sandbox_flags: crate::SandboxFlags::NONE,
          opaque_origin: false,
        }],
      },
    );

    assert!(
      !browser.is_process_terminated(renderer),
      "valid SubframesDiscovered should not terminate the renderer"
    );
    assert!(
      browser.take_events().is_empty(),
      "expected no protocol events for valid SubframesDiscovered"
    );
  }

  fn solid_buffer(rgba: [u8; 4]) -> FrameBuffer {
    FrameBuffer {
      width: 1,
      height: 1,
      rgba8: rgba.to_vec(),
    }
  }

  fn simple_slot(child: FrameId) -> crate::SubframeInfo {
    crate::SubframeInfo {
      child,
      src: None,
      transform: crate::AffineTransform::IDENTITY,
      clip_stack: Vec::new(),
      z_index: 0,
      hit_testable: true,
      referrer_policy: None,
      sandbox_flags: crate::SandboxFlags::NONE,
      opaque_origin: false,
    }
  }

  #[test]
  fn frame_paint_plan_spoofed_frame_id_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let honest = RendererId(1);
    let attacker = RendererId(2);
    let frame = FrameId(10);
    browser.assign_frame(frame, honest);

    let plan = crate::FramePaintPlan {
      frame_id: frame,
      layers: vec![solid_buffer([0, 0, 0, 0])],
      slots: vec![],
    };
    browser.handle_renderer_message(attacker, RendererToBrowser::FramePaintPlan(plan));

    assert!(browser.is_process_terminated(attacker));
    assert!(
      !browser.is_process_terminated(honest),
      "protocol violation should not affect other processes"
    );
    assert!(
      browser.latest_frame(frame).is_none(),
      "spoofed FramePaintPlan must not update presentation state"
    );
  }

  #[test]
  fn frame_paint_plan_invalid_subframe_reference_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    let child = FrameId(11);

    browser.assign_frame(parent, renderer);
    // Browser frame tree says child belongs elsewhere (not `parent`).
    browser.set_frame_parent(child, FrameId(999));

    let plan = crate::FramePaintPlan {
      frame_id: parent,
      layers: vec![solid_buffer([0, 0, 0, 0]), solid_buffer([1, 2, 3, 255])],
      slots: vec![simple_slot(child)],
    };
    browser.handle_renderer_message(renderer, RendererToBrowser::FramePaintPlan(plan));

    assert!(browser.is_process_terminated(renderer));
    assert!(
      browser.is_frame_crashed(parent),
      "terminating the renderer should mark its frames as crashed"
    );
  }

  #[test]
  fn frame_paint_plan_with_valid_children_updates_presentation_state() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    let child = FrameId(2);

    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    let plan = crate::FramePaintPlan {
      frame_id: parent,
      layers: vec![solid_buffer([0, 0, 0, 0]), solid_buffer([9, 8, 7, 255])],
      slots: vec![simple_slot(child)],
    };
    browser.handle_renderer_message(renderer, RendererToBrowser::FramePaintPlan(plan));

    assert!(
      !browser.is_process_terminated(renderer),
      "valid FramePaintPlan should not terminate the renderer"
    );
    let Some(buffer) = browser.latest_frame(parent) else {
      panic!("expected FramePaintPlan to update presentation state");
    };
    assert_eq!(buffer.width, 1);
    assert_eq!(buffer.height, 1);
    assert_eq!(buffer.rgba8, vec![9, 8, 7, 255]);
  }
}
