use crate::{DiscoveredSubframe, FrameBuffer, FrameId, RendererId, RendererToBrowser, SubframeInfo, SubframeToken};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererToBrowserKind {
  FrameReady,
  FramePaintPlan,
  SubframesDiscovered,
  NavigationCommitted,
  NavigationFailed,
  HoverChanged,
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
  TooManySubframes {
    parent_frame_id: FrameId,
    count: usize,
    max: usize,
  },
  DuplicateSubframeToken {
    parent_frame_id: FrameId,
    token: SubframeToken,
  },
  DuplicateSubframeChild {
    parent_frame_id: FrameId,
    child_frame_id: FrameId,
  },
  InvalidDiscoveredSubframeRect {
    parent_frame_id: FrameId,
    token: SubframeToken,
    reason: InvalidSubframeRectReason,
  },
  InvalidSubframe {
    parent_frame_id: FrameId,
    child_frame_id: FrameId,
    expected_parent_frame_id: Option<FrameId>,
  },
  InvalidSubframeTransform {
    parent_frame_id: FrameId,
    child_frame_id: FrameId,
    reason: InvalidSubframeTransformReason,
  },
  ClipStackTooDeep {
    parent_frame_id: FrameId,
    child_frame_id: FrameId,
    depth: usize,
    max: usize,
  },
  InvalidFrameBuffer {
    frame_id: FrameId,
    width: u32,
    height: u32,
    rgba8_len: usize,
    expected_len: Option<usize>,
  },
  InvalidPaintPlan {
    frame_id: FrameId,
    error: crate::CompositeError,
  },
  OversizedString {
    frame_id: FrameId,
    field: &'static str,
    len: usize,
    max: usize,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSubframeTransformReason {
  NonAxisAligned,
  NonFinite,
  ZeroScale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSubframeRectReason {
  NonFinite,
  NegativeSize,
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
        if !self.check_frame_buffer(sender, frame_id, &buffer, RendererToBrowserKind::FrameReady) {
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
        match crate::composite_paint_plan(
          plan,
          std::iter::empty::<(&crate::SubframeInfo, &crate::FrameBuffer)>(),
        ) {
          Ok(buffer) => {
            self.latest_frame.insert(frame_id, buffer);
          }
          Err(err) => {
            self.protocol_violation(
              sender,
              frame_id,
              RendererToBrowserKind::FramePaintPlan,
              FrameOwnershipViolation::InvalidPaintPlan { frame_id, error: err },
            );
          }
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
        let _ = self.check_discovered_subframes(
          sender,
          parent_frame_id,
          &subframes,
          RendererToBrowserKind::SubframesDiscovered,
        );
      }
      RendererToBrowser::NavigationCommitted {
        frame_id,
        url,
        base_url,
        csp: _csp,
      } => {
        if !self.check_frame(sender, frame_id, RendererToBrowserKind::NavigationCommitted) {
          return;
        }
        if !self.check_string_len(
          sender,
          frame_id,
          RendererToBrowserKind::NavigationCommitted,
          "NavigationCommitted.url",
          &url,
          crate::MAX_UNTRUSTED_URL_BYTES,
        ) {
          return;
        }
        if let Some(base) = base_url.as_deref() {
          let _ = self.check_string_len(
            sender,
            frame_id,
            RendererToBrowserKind::NavigationCommitted,
            "NavigationCommitted.base_url",
            base,
            crate::MAX_UNTRUSTED_URL_BYTES,
          );
        }
      }
      RendererToBrowser::NavigationFailed { frame_id, url, error } => {
        if !self.check_frame(sender, frame_id, RendererToBrowserKind::NavigationFailed) {
          return;
        }
        if !self.check_string_len(
          sender,
          frame_id,
          RendererToBrowserKind::NavigationFailed,
          "NavigationFailed.url",
          &url,
          crate::MAX_UNTRUSTED_URL_BYTES,
        ) {
          return;
        }
        let _ = self.check_string_len(
          sender,
          frame_id,
          RendererToBrowserKind::NavigationFailed,
          "NavigationFailed.error",
          &error,
          crate::MAX_UNTRUSTED_URL_BYTES,
        );
      }
      RendererToBrowser::HoverChanged {
        frame_id,
        seq: _seq,
        hovered_url,
        cursor: _cursor,
      } => {
        if !self.check_frame(sender, frame_id, RendererToBrowserKind::HoverChanged) {
          return;
        }
        if let Some(url) = hovered_url.as_deref() {
          let _ = self.check_string_len(
            sender,
            frame_id,
            RendererToBrowserKind::HoverChanged,
            "HoverChanged.hovered_url",
            url,
            crate::MAX_UNTRUSTED_URL_BYTES,
          );
        }
      }
      RendererToBrowser::InputAck { frame_id, .. } => {
        let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::InputAck);
      }
      RendererToBrowser::Error { frame_id, message } => {
        let frame_for_event = frame_id.unwrap_or(FrameId(0));
        if !self.check_string_len(
          sender,
          frame_for_event,
          RendererToBrowserKind::Error,
          "Error.message",
          &message,
          crate::MAX_UNTRUSTED_URL_BYTES,
        ) {
          return;
        }
        if let Some(frame_id) = frame_id {
          let _ = self.check_frame(sender, frame_id, RendererToBrowserKind::Error);
        }
      }
    }
  }

  fn check_string_len(
    &mut self,
    sender: RendererId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
    field: &'static str,
    value: &str,
    max: usize,
  ) -> bool {
    if value.len() <= max {
      return true;
    }

    self.protocol_violation(
      sender,
      frame_id,
      message,
      FrameOwnershipViolation::OversizedString {
        frame_id,
        field,
        len: value.len(),
        max,
      },
    );
    false
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
    if subframes.len() > crate::MAX_SUBFRAMES_PER_FRAME {
      self.protocol_violation(
        sender,
        parent_frame_id,
        message,
        FrameOwnershipViolation::TooManySubframes {
          parent_frame_id,
          count: subframes.len(),
          max: crate::MAX_SUBFRAMES_PER_FRAME,
        },
      );
      return false;
    }
    let mut seen_children = HashSet::<FrameId>::new();
    for subframe in subframes {
      let child = subframe.child;
      if !seen_children.insert(child) {
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::DuplicateSubframeChild {
            parent_frame_id,
            child_frame_id: child,
          },
        );
        return false;
      }
      if let Some(src) = subframe.src.as_deref() {
        if src.len() > crate::MAX_UNTRUSTED_URL_BYTES {
          self.protocol_violation(
            sender,
            parent_frame_id,
            message,
            FrameOwnershipViolation::OversizedString {
              frame_id: parent_frame_id,
              field: "SubframeInfo.src",
              len: src.len(),
              max: crate::MAX_UNTRUSTED_URL_BYTES,
            },
          );
          return false;
        }
      }
      if subframe.clip_stack.len() > crate::MAX_SUBFRAME_CLIP_STACK_DEPTH {
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::ClipStackTooDeep {
            parent_frame_id,
            child_frame_id: child,
            depth: subframe.clip_stack.len(),
            max: crate::MAX_SUBFRAME_CLIP_STACK_DEPTH,
          },
        );
        return false;
      }
      let t = subframe.transform;
      let reason = if !t.is_axis_aligned() {
        Some(InvalidSubframeTransformReason::NonAxisAligned)
      } else if !t.a.is_finite() || !t.d.is_finite() || !t.e.is_finite() || !t.f.is_finite() {
        Some(InvalidSubframeTransformReason::NonFinite)
      } else if t.a == 0.0 || t.d == 0.0 {
        Some(InvalidSubframeTransformReason::ZeroScale)
      } else {
        None
      };
      if let Some(reason) = reason {
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::InvalidSubframeTransform {
            parent_frame_id,
            child_frame_id: child,
            reason,
          },
        );
        return false;
      }
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

  fn check_discovered_subframes(
    &mut self,
    sender: RendererId,
    parent_frame_id: FrameId,
    subframes: &[DiscoveredSubframe],
    message: RendererToBrowserKind,
  ) -> bool {
    if subframes.len() > crate::MAX_SUBFRAMES_PER_FRAME {
      self.protocol_violation(
        sender,
        parent_frame_id,
        message,
        FrameOwnershipViolation::TooManySubframes {
          parent_frame_id,
          count: subframes.len(),
          max: crate::MAX_SUBFRAMES_PER_FRAME,
        },
      );
      return false;
    }

    let mut seen = HashSet::<SubframeToken>::new();
    for subframe in subframes {
      if !seen.insert(subframe.token) {
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::DuplicateSubframeToken {
            parent_frame_id,
            token: subframe.token,
          },
        );
        return false;
      }

      if let crate::IframeNavigation::Url(url) = &subframe.navigation {
        if url.len() > crate::MAX_UNTRUSTED_URL_BYTES {
          self.protocol_violation(
            sender,
            parent_frame_id,
            message,
            FrameOwnershipViolation::OversizedString {
              frame_id: parent_frame_id,
              field: "DiscoveredSubframe.navigation",
              len: url.len(),
              max: crate::MAX_UNTRUSTED_URL_BYTES,
            },
          );
          return false;
        }
      }

      let rect = subframe.rect;
      let finite = rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite();
      let negative = rect.width < 0.0 || rect.height < 0.0;
      if !finite || negative {
        let reason = if !finite {
          InvalidSubframeRectReason::NonFinite
        } else {
          InvalidSubframeRectReason::NegativeSize
        };
        self.protocol_violation(
          sender,
          parent_frame_id,
          message,
          FrameOwnershipViolation::InvalidDiscoveredSubframeRect {
            parent_frame_id,
            token: subframe.token,
            reason,
          },
        );
        return false;
      }
    }

    true
  }

  fn check_frame_buffer(
    &mut self,
    sender: RendererId,
    frame_id: FrameId,
    buffer: &FrameBuffer,
    message: RendererToBrowserKind,
  ) -> bool {
    let expected_len = (buffer.width as usize)
      .checked_mul(buffer.height as usize)
      .and_then(|px| px.checked_mul(4));
    if expected_len == Some(buffer.rgba8.len()) {
      return true;
    }

    self.protocol_violation(
      sender,
      frame_id,
      message,
      FrameOwnershipViolation::InvalidFrameBuffer {
        frame_id,
        width: buffer.width,
        height: buffer.height,
        rgba8_len: buffer.rgba8.len(),
        expected_len,
      },
    );
    false
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
        base_url: None,
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
          effects: crate::SubframeEffects::default(),
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
          effects: crate::SubframeEffects::default(),
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
  fn subframes_discovered_accepts_owned_parent() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);

    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::DiscoveredSubframe {
          token: crate::SubframeToken(1),
          navigation: crate::IframeNavigation::Url("https://example.test/".to_string()),
          rect: crate::Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
          },
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

  #[test]
  fn subframes_discovered_with_too_many_entries_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    let template = crate::DiscoveredSubframe {
      token: crate::SubframeToken(1),
      navigation: crate::IframeNavigation::AboutBlank,
      rect: crate::Rect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
      },
      hit_testable: true,
      referrer_policy: None,
      sandbox_flags: crate::SandboxFlags::NONE,
      opaque_origin: false,
    };

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![template; crate::MAX_SUBFRAMES_PER_FRAME + 1],
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
          violation: FrameOwnershipViolation::TooManySubframes { .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected TooManySubframes protocol event (events={events:?})"
    );
  }

  #[test]
  fn subframes_discovered_with_duplicate_token_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    let template = crate::DiscoveredSubframe {
      token: crate::SubframeToken(1),
      navigation: crate::IframeNavigation::AboutBlank,
      rect: crate::Rect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
      },
      hit_testable: true,
      referrer_policy: None,
      sandbox_flags: crate::SandboxFlags::NONE,
      opaque_origin: false,
    };

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![template.clone(), template],
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
          violation: FrameOwnershipViolation::DuplicateSubframeToken { .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected DuplicateSubframeToken protocol event (events={events:?})"
    );
  }

  #[test]
  fn subframes_discovered_with_negative_rect_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::DiscoveredSubframe {
          token: crate::SubframeToken(1),
          navigation: crate::IframeNavigation::AboutBlank,
          rect: crate::Rect {
            x: 0.0,
            y: 0.0,
            width: -1.0,
            height: 1.0,
          },
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
          violation: FrameOwnershipViolation::InvalidDiscoveredSubframeRect { reason: InvalidSubframeRectReason::NegativeSize, .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected InvalidDiscoveredSubframeRect protocol event (events={events:?})"
    );
  }

  #[test]
  fn subframes_discovered_with_nan_rect_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::DiscoveredSubframe {
          token: crate::SubframeToken(1),
          navigation: crate::IframeNavigation::AboutBlank,
          rect: crate::Rect {
            x: 0.0,
            y: 0.0,
            width: f32::NAN,
            height: 1.0,
          },
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
          violation: FrameOwnershipViolation::InvalidDiscoveredSubframeRect { reason: InvalidSubframeRectReason::NonFinite, .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected InvalidDiscoveredSubframeRect protocol event (events={events:?})"
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
      effects: crate::SubframeEffects::default(),
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

  #[test]
  fn frame_ready_with_invalid_buffer_len_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let frame = FrameId(1);
    browser.assign_frame(frame, renderer);

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: frame,
        buffer: FrameBuffer {
          width: 1,
          height: 1,
          rgba8: vec![0, 0, 0], // should be 4 bytes
        },
        subframes: vec![],
      },
    );

    assert!(browser.is_process_terminated(renderer));
    assert!(
      browser.latest_frame(frame).is_none(),
      "invalid FrameReady must not update presentation state"
    );
    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::FrameReady,
          violation: FrameOwnershipViolation::InvalidFrameBuffer { .. },
        } if *process_id == renderer && *frame_id == frame
      )),
      "expected InvalidFrameBuffer protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_paint_plan_with_invalid_layer_buffer_len_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let frame = FrameId(1);
    browser.assign_frame(frame, renderer);

    let plan = crate::FramePaintPlan {
      frame_id: frame,
      layers: vec![FrameBuffer {
        width: 1,
        height: 1,
        rgba8: vec![0, 0, 0], // should be 4 bytes
      }],
      slots: vec![],
    };
    browser.handle_renderer_message(renderer, RendererToBrowser::FramePaintPlan(plan));

    assert!(browser.is_process_terminated(renderer));
    let events = browser.take_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        IpcSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::FramePaintPlan,
          violation: FrameOwnershipViolation::InvalidPaintPlan { .. },
        } if *process_id == renderer && *frame_id == frame
      )),
      "expected InvalidPaintPlan protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_too_many_subframes_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    browser.assign_frame(parent, renderer);

    let info = simple_slot(FrameId(2));
    let subframes = vec![info; crate::MAX_SUBFRAMES_PER_FRAME + 1];
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: solid_buffer([0, 0, 0, 255]),
        subframes,
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
          violation: FrameOwnershipViolation::TooManySubframes { .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected TooManySubframes protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_deep_clip_stack_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    browser.assign_frame(parent, renderer);

    let clip = crate::ClipItem {
      rect: crate::Rect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
      },
      radius: crate::BorderRadius::ZERO,
    };

    let mut info = simple_slot(FrameId(2));
    info.clip_stack = vec![clip; crate::MAX_SUBFRAME_CLIP_STACK_DEPTH + 1];
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: solid_buffer([0, 0, 0, 255]),
        subframes: vec![info],
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
          violation: FrameOwnershipViolation::ClipStackTooDeep { .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected ClipStackTooDeep protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_non_axis_aligned_transform_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    let child = FrameId(2);
    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    let mut info = simple_slot(child);
    info.transform.b = 1.0;

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: solid_buffer([0, 0, 0, 255]),
        subframes: vec![info],
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
          violation: FrameOwnershipViolation::InvalidSubframeTransform { reason: InvalidSubframeTransformReason::NonAxisAligned, .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected InvalidSubframeTransform protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_duplicate_child_subframes_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    let child = FrameId(2);
    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    let slot = simple_slot(child);
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: solid_buffer([0, 0, 0, 255]),
        subframes: vec![slot.clone(), slot],
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
          violation: FrameOwnershipViolation::DuplicateSubframeChild { .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected DuplicateSubframeChild protocol event (events={events:?})"
    );
  }

  #[test]
  fn frame_ready_with_oversized_src_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(1);
    let child = FrameId(2);
    browser.assign_frame(parent, renderer);
    browser.set_frame_parent(child, parent);

    let mut info = simple_slot(child);
    info.src = Some("a".repeat(crate::MAX_UNTRUSTED_URL_BYTES + 1));

    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::FrameReady {
        frame_id: parent,
        buffer: solid_buffer([0, 0, 0, 255]),
        subframes: vec![info],
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
          violation: FrameOwnershipViolation::OversizedString { field: "SubframeInfo.src", .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected OversizedString protocol event for SubframeInfo.src (events={events:?})"
    );
  }

  #[test]
  fn subframes_discovered_with_oversized_url_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let parent = FrameId(10);
    browser.assign_frame(parent, renderer);

    let long = "a".repeat(crate::MAX_UNTRUSTED_URL_BYTES + 1);
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::SubframesDiscovered {
        parent_frame_id: parent,
        subframes: vec![crate::DiscoveredSubframe {
          token: crate::SubframeToken(1),
          navigation: crate::IframeNavigation::Url(long),
          rect: crate::Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
          },
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
          violation: FrameOwnershipViolation::OversizedString { field: "DiscoveredSubframe.navigation", .. },
        } if *process_id == renderer && *frame_id == parent
      )),
      "expected OversizedString protocol event for DiscoveredSubframe.navigation (events={events:?})"
    );
  }

  #[test]
  fn navigation_committed_with_oversized_url_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let frame = FrameId(1);
    browser.assign_frame(frame, renderer);

    let long = "a".repeat(crate::MAX_UNTRUSTED_URL_BYTES + 1);
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::NavigationCommitted {
        frame_id: frame,
        url: long,
        base_url: None,
        csp: Vec::new(),
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
          message: RendererToBrowserKind::NavigationCommitted,
          violation: FrameOwnershipViolation::OversizedString { field: "NavigationCommitted.url", .. },
        } if *process_id == renderer && *frame_id == frame
      )),
      "expected OversizedString protocol event for NavigationCommitted.url (events={events:?})"
    );
  }

  #[test]
  fn hover_changed_with_oversized_url_kills_sender() {
    let mut browser = BrowserIpcSecurityState::default();
    let renderer = RendererId(1);
    let frame = FrameId(1);
    browser.assign_frame(frame, renderer);

    let long = "a".repeat(crate::MAX_UNTRUSTED_URL_BYTES + 1);
    browser.handle_renderer_message(
      renderer,
      RendererToBrowser::HoverChanged {
        frame_id: frame,
        seq: 1,
        hovered_url: Some(long),
        cursor: crate::CursorKind::Default,
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
          message: RendererToBrowserKind::HoverChanged,
          violation: FrameOwnershipViolation::OversizedString { field: "HoverChanged.hovered_url", .. },
        } if *process_id == renderer && *frame_id == frame
      )),
      "expected OversizedString protocol event for HoverChanged.hovered_url (events={events:?})"
    );
  }
}
