//! Minimal multiprocess browser model (process-per-site) used by the multiprocess security
//! workstream.
//!
//! The production browser UI currently runs rendering in-process. The multiprocess security
//! workstream incrementally introduces a browser↔renderer split where multiple frames (and even
//! multiple tabs) can share a single renderer process when they share a `SiteKey`.
//!
//! This module provides a small, deterministic state machine that models:
//! - process-per-site reuse,
//! - tracking which frames are attached to which renderer process, and
//! - crash handling when a shared renderer process exits.
//!
//! It is intentionally independent of winit/egui so it can be exercised from integration tests and
//! reused by future browser-process plumbing.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ui::TabId;
use url::Url;

// -----------------------------------------------------------------------------
// Core identifiers
// -----------------------------------------------------------------------------

static NEXT_FRAME_ID: AtomicU64 = AtomicU64::new(1);

// Re-export the canonical browser-session-local renderer process id type. This keeps the
// multiprocess state machine aligned with the rest of the UI without introducing a second
// `RendererProcessId` definition.
pub use super::RendererProcessId;

/// Identifier for a renderer-owned frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameId(pub u64);

impl FrameId {
  /// Generate a new process-unique `FrameId`.
  pub fn new() -> Self {
    loop {
      let id = NEXT_FRAME_ID.fetch_add(1, Ordering::Relaxed);
      if id != 0 {
        return Self(id);
      }
    }
  }
}

// -----------------------------------------------------------------------------
// SiteKey
// -----------------------------------------------------------------------------

/// Key used for process-per-origin (process-per-site) renderer assignment.
///
/// For now this is a pragmatic "origin-like" key: scheme + host + port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteKey {
  scheme: String,
  host: Option<String>,
  port: Option<u16>,
}

impl SiteKey {
  pub fn from_url(url: &Url) -> Self {
    Self {
      scheme: url.scheme().to_string(),
      host: url.host_str().map(|h| h.to_ascii_lowercase()),
      port: url.port_or_known_default(),
    }
  }

  pub fn parse(url: &str) -> Result<Self, url::ParseError> {
    let parsed = Url::parse(url)?;
    Ok(Self::from_url(&parsed))
  }
}

impl std::fmt::Display for SiteKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.scheme)?;
    write!(f, "://")?;
    if let Some(host) = &self.host {
      write!(f, "{host}")?;
    }
    if let Some(port) = self.port {
      write!(f, ":{port}")?;
    }
    Ok(())
  }
}

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum MultiprocessError {
  #[error("invalid URL {url:?}: {source}")]
  InvalidUrl {
    url: String,
    #[source]
    source: url::ParseError,
  },
  #[error("unknown tab {tab_id:?}")]
  UnknownTab { tab_id: TabId },
  #[error("unknown frame {frame_id:?}")]
  UnknownFrame { frame_id: FrameId },
  #[error("unknown renderer process {process_id:?}")]
  UnknownProcess { process_id: RendererProcessId },
  #[error("renderer process {process_id:?} is dead")]
  ProcessDead { process_id: RendererProcessId },
}

// -----------------------------------------------------------------------------
// RendererProcessRegistry
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RendererProcessEntry {
  site: SiteKey,
  alive: bool,
  attached_frames: BTreeSet<FrameId>,
}

/// Tracks live renderer processes and the set of frames attached to each process.
///
/// This is the core piece needed to deterministically update all affected frames when a shared
/// renderer crashes/disconnects.
#[derive(Debug, Default)]
pub struct RendererProcessRegistry {
  processes: HashMap<RendererProcessId, RendererProcessEntry>,
  live_process_by_site: HashMap<SiteKey, RendererProcessId>,
}

impl RendererProcessRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Returns an existing live renderer process for `site`, or spawns a new one.
  pub fn get_or_spawn(&mut self, site: SiteKey) -> RendererProcessId {
    if let Some(pid) = self.live_process_by_site.get(&site).copied() {
      if self.is_alive(pid) {
        return pid;
      }
      // Stale mapping; drop it and create a fresh process.
      self.live_process_by_site.remove(&site);
    }

    let pid = RendererProcessId::new();
    self.processes.insert(
      pid,
      RendererProcessEntry {
        site: site.clone(),
        alive: true,
        attached_frames: BTreeSet::new(),
      },
    );
    self.live_process_by_site.insert(site, pid);
    pid
  }

  pub fn is_alive(&self, process_id: RendererProcessId) -> bool {
    self
      .processes
      .get(&process_id)
      .is_some_and(|entry| entry.alive)
  }

  pub fn attach_frame(
    &mut self,
    process_id: RendererProcessId,
    frame_id: FrameId,
  ) -> Result<(), MultiprocessError> {
    let entry = self
      .processes
      .get_mut(&process_id)
      .ok_or(MultiprocessError::UnknownProcess { process_id })?;
    if !entry.alive {
      return Err(MultiprocessError::ProcessDead { process_id });
    }
    entry.attached_frames.insert(frame_id);
    Ok(())
  }

  pub fn detach_frame(&mut self, process_id: RendererProcessId, frame_id: FrameId) {
    let Some(entry) = self.processes.get_mut(&process_id) else {
      return;
    };
    entry.attached_frames.remove(&frame_id);
  }

  /// Returns the (sorted) set of frames currently attached to the process.
  pub fn attached_frames(&self, process_id: RendererProcessId) -> Vec<FrameId> {
    self
      .processes
      .get(&process_id)
      .map(|entry| entry.attached_frames.iter().copied().collect())
      .unwrap_or_default()
  }

  /// Marks the renderer process as dead and returns the frames that were attached at the time of
  /// exit.
  ///
  /// The returned list is sorted to keep crash propagation deterministic.
  pub fn mark_dead(&mut self, process_id: RendererProcessId) -> Vec<FrameId> {
    let Some(entry) = self.processes.get_mut(&process_id) else {
      return Vec::new();
    };

    entry.alive = false;
    if self.live_process_by_site.get(&entry.site) == Some(&process_id) {
      self.live_process_by_site.remove(&entry.site);
    }

    let frames: Vec<FrameId> = entry.attached_frames.iter().copied().collect();
    entry.attached_frames.clear();
    frames
  }
}

// -----------------------------------------------------------------------------
// Browser-side state machine
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLifecycle {
  Live,
  Crashed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameKind {
  Root { tab_id: TabId },
  Subframe { parent: FrameId },
}

#[derive(Debug, Clone)]
pub struct FrameState {
  pub id: FrameId,
  pub site: SiteKey,
  pub kind: FrameKind,
  pub lifecycle: FrameLifecycle,
  pub process: Option<RendererProcessId>,
}

#[derive(Debug, Clone)]
pub struct TabState {
  pub tab_id: TabId,
  pub root_frame: FrameId,
  pub crashed: bool,
}

/// Minimal subset of messages from renderer → browser used to verify crash gating.
#[derive(Debug, Clone, Copy)]
pub enum RendererToBrowser {
  /// A renderer event scoped to a specific frame.
  FrameReady { frame_id: FrameId },
  NavigationCommitted { frame_id: FrameId },
  NavigationFailed { frame_id: FrameId },
  SubframesDiscovered { parent_frame_id: FrameId },
  InputAck { frame_id: FrameId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererToBrowserKind {
  FrameReady,
  NavigationCommitted,
  NavigationFailed,
  SubframesDiscovered,
  InputAck,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameOwnershipViolation {
  UnknownFrame { frame_id: FrameId },
  NotOwner {
    frame_id: FrameId,
    expected: Option<RendererProcessId>,
    actual: RendererProcessId,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiprocessSecurityEvent {
  ProtocolViolation {
    process_id: RendererProcessId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
    violation: FrameOwnershipViolation,
  },
  ProcessTerminated {
    process_id: RendererProcessId,
  },
}

/// Deterministic browser-side model of tabs/frames hosted in shared renderer processes.
#[derive(Debug, Default)]
pub struct MultiprocessBrowser {
  process_registry: RendererProcessRegistry,
  tabs: HashMap<TabId, TabState>,
  frames: HashMap<FrameId, FrameState>,
  security_events: Vec<MultiprocessSecurityEvent>,
}

impl MultiprocessBrowser {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn process_registry(&self) -> &RendererProcessRegistry {
    &self.process_registry
  }

  pub fn tab_state(&self, tab_id: TabId) -> Option<&TabState> {
    self.tabs.get(&tab_id)
  }

  pub fn frame_state(&self, frame_id: FrameId) -> Option<&FrameState> {
    self.frames.get(&frame_id)
  }

  pub fn root_frame(&self, tab_id: TabId) -> Option<FrameId> {
    self.tabs.get(&tab_id).map(|tab| tab.root_frame)
  }

  pub fn tab_is_crashed(&self, tab_id: TabId) -> bool {
    self.tabs.get(&tab_id).is_some_and(|tab| tab.crashed)
  }

  pub fn frame_is_crashed(&self, frame_id: FrameId) -> bool {
    self
      .frames
      .get(&frame_id)
      .is_some_and(|frame| frame.lifecycle == FrameLifecycle::Crashed)
  }

  pub fn process_for_tab(&self, tab_id: TabId) -> Option<RendererProcessId> {
    let root = self.root_frame(tab_id)?;
    self.frames.get(&root).and_then(|frame| frame.process)
  }

  pub fn process_is_alive(&self, process_id: RendererProcessId) -> bool {
    self.process_registry.is_alive(process_id)
  }

  pub fn process_attached_frames(&self, process_id: RendererProcessId) -> Vec<FrameId> {
    self.process_registry.attached_frames(process_id)
  }

  /// Returns and clears any recorded multiprocess security events.
  ///
  /// These are primarily intended for tests that want to assert protocol-violation handling.
  pub fn take_security_events(&mut self) -> Vec<MultiprocessSecurityEvent> {
    std::mem::take(&mut self.security_events)
  }

  /// Create a new tab and root frame for the provided URL.
  pub fn open_tab(&mut self, url: &str) -> Result<TabId, MultiprocessError> {
    let parsed = Url::parse(url).map_err(|source| MultiprocessError::InvalidUrl {
      url: url.to_string(),
      source,
    })?;
    let site = SiteKey::from_url(&parsed);
    let process_id = self.process_registry.get_or_spawn(site.clone());

    let tab_id = TabId::new();
    let frame_id = FrameId::new();
    self.process_registry.attach_frame(process_id, frame_id)?;

    self.frames.insert(
      frame_id,
      FrameState {
        id: frame_id,
        site,
        kind: FrameKind::Root { tab_id },
        lifecycle: FrameLifecycle::Live,
        process: Some(process_id),
      },
    );
    self.tabs.insert(
      tab_id,
      TabState {
        tab_id,
        root_frame: frame_id,
        crashed: false,
      },
    );

    Ok(tab_id)
  }

  /// Mark the given renderer process as crashed/exited and update all attached frames/tabs.
  pub fn crash_process(&mut self, process_id: RendererProcessId) {
    let attached_frames = self.process_registry.mark_dead(process_id);
    for frame_id in attached_frames {
      self.mark_frame_crashed(frame_id);
    }
  }

  /// Handle a message emitted from a renderer process.
  ///
  /// Returns `true` when the message was accepted/processed; returns `false` when the browser
  /// ignored it (e.g. because the process is dead or the frame is no longer attached).
  pub fn handle_renderer_message(
    &mut self,
    process_id: RendererProcessId,
    msg: RendererToBrowser,
  ) -> bool {
    if !self.process_registry.is_alive(process_id) {
      return false;
    }

    match msg {
      RendererToBrowser::FrameReady { frame_id } => {
        self.validate_frame_message(process_id, frame_id, RendererToBrowserKind::FrameReady)
      }
      RendererToBrowser::NavigationCommitted { frame_id } => self.validate_frame_message(
        process_id,
        frame_id,
        RendererToBrowserKind::NavigationCommitted,
      ),
      RendererToBrowser::NavigationFailed { frame_id } => self.validate_frame_message(
        process_id,
        frame_id,
        RendererToBrowserKind::NavigationFailed,
      ),
      RendererToBrowser::SubframesDiscovered { parent_frame_id } => self.validate_frame_message(
        process_id,
        parent_frame_id,
        RendererToBrowserKind::SubframesDiscovered,
      ),
      RendererToBrowser::InputAck { frame_id } => {
        self.validate_frame_message(process_id, frame_id, RendererToBrowserKind::InputAck)
      }
    }
  }

  /// Reload a crashed (or live) tab by reattaching its root frame to a fresh renderer process for
  /// the same `SiteKey`.
  pub fn reload_tab(&mut self, tab_id: TabId) -> Result<(), MultiprocessError> {
    let root = self
      .tabs
      .get(&tab_id)
      .map(|tab| tab.root_frame)
      .ok_or(MultiprocessError::UnknownTab { tab_id })?;
    self.reload_frame(root)?;
    Ok(())
  }

  fn reload_frame(&mut self, frame_id: FrameId) -> Result<(), MultiprocessError> {
    let (site, kind, prev_process) = {
      let frame = self
        .frames
        .get(&frame_id)
        .ok_or(MultiprocessError::UnknownFrame { frame_id })?;
      (frame.site.clone(), frame.kind.clone(), frame.process)
    };

    let new_process = self.process_registry.get_or_spawn(site.clone());
    if let Some(prev) = prev_process {
      if prev != new_process {
        self.process_registry.detach_frame(prev, frame_id);
      }
    }
    self.process_registry.attach_frame(new_process, frame_id)?;

    if let Some(frame) = self.frames.get_mut(&frame_id) {
      frame.lifecycle = FrameLifecycle::Live;
      frame.process = Some(new_process);
    }

    if let FrameKind::Root { tab_id } = kind {
      if let Some(tab) = self.tabs.get_mut(&tab_id) {
        tab.crashed = false;
      }
    }

    Ok(())
  }

  fn mark_frame_crashed(&mut self, frame_id: FrameId) {
    let Some(frame) = self.frames.get_mut(&frame_id) else {
      return;
    };

    frame.lifecycle = FrameLifecycle::Crashed;
    frame.process = None;

    if let FrameKind::Root { tab_id } = frame.kind {
      if let Some(tab) = self.tabs.get_mut(&tab_id) {
        tab.crashed = true;
      }
    }
  }

  fn validate_frame_message(
    &mut self,
    process_id: RendererProcessId,
    frame_id: FrameId,
    kind: RendererToBrowserKind,
  ) -> bool {
    let Some(frame) = self.frames.get(&frame_id) else {
      self.protocol_violation(
        process_id,
        frame_id,
        kind,
        FrameOwnershipViolation::UnknownFrame { frame_id },
      );
      return false;
    };

    if frame.process == Some(process_id) {
      return true;
    }

    self.protocol_violation(
      process_id,
      frame_id,
      kind,
      FrameOwnershipViolation::NotOwner {
        frame_id,
        expected: frame.process,
        actual: process_id,
      },
    );
    false
  }

  fn protocol_violation(
    &mut self,
    process_id: RendererProcessId,
    frame_id: FrameId,
    message: RendererToBrowserKind,
    violation: FrameOwnershipViolation,
  ) {
    self.security_events.push(MultiprocessSecurityEvent::ProtocolViolation {
      process_id,
      frame_id,
      message,
      violation,
    });
    self.crash_process(process_id);
    self
      .security_events
      .push(MultiprocessSecurityEvent::ProcessTerminated { process_id });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn protocol_violation_kills_renderer_process() {
    let mut browser = MultiprocessBrowser::new();

    let tab_a = browser.open_tab("https://a.example/").expect("open tab a");
    let tab_b = browser.open_tab("https://b.example/").expect("open tab b");

    let proc_a = browser.process_for_tab(tab_a).expect("proc a");
    let proc_b = browser.process_for_tab(tab_b).expect("proc b");
    assert_ne!(proc_a, proc_b, "tabs from different SiteKeys should not share a process");

    let frame_a = browser.root_frame(tab_a).expect("root frame a");
    let frame_b = browser.root_frame(tab_b).expect("root frame b");

    // Attacker: process A attempts to send FrameReady for frame B.
    assert!(
      !browser.handle_renderer_message(proc_a, RendererToBrowser::FrameReady { frame_id: frame_b }),
      "expected spoofed FrameReady to be rejected"
    );
    assert!(
      !browser.process_is_alive(proc_a),
      "expected protocol violation to terminate offending process"
    );
    assert!(
      browser.tab_is_crashed(tab_a),
      "expected tab A to be marked crashed when its renderer is terminated"
    );
    assert!(
      !browser.tab_is_crashed(tab_b),
      "expected unrelated tab B to remain live"
    );
    assert!(
      browser.process_is_alive(proc_b),
      "expected unrelated renderer process to remain alive"
    );
    assert!(
      browser.handle_renderer_message(proc_b, RendererToBrowser::FrameReady { frame_id: frame_b }),
      "expected honest process to still be able to send messages"
    );

    let events = browser.take_security_events();
    assert!(
      events.iter().any(|evt| matches!(
        evt,
        MultiprocessSecurityEvent::ProtocolViolation {
          process_id,
          frame_id,
          message: RendererToBrowserKind::FrameReady,
          ..
        } if *process_id == proc_a && *frame_id == frame_b
      )),
      "expected protocol violation event to be recorded (events={events:?})"
    );

    // Root frame A should now be crashed.
    assert!(browser.frame_is_crashed(frame_a));
  }
}
