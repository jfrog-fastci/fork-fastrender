use crate::geometry::{Point, Rect};
use crate::site_isolation::{iframe_navigation_from_src, site_key_for_navigation, IframeNavigation, SiteKey};
use std::collections::{HashMap, HashSet};

use super::registry::{FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry};

// -----------------------------------------------------------------------------
// IDs / keys
// -----------------------------------------------------------------------------

/// Stable identifier for an `<iframe>` element reported by a renderer.
///
/// This is not a `FrameId`: it is tied to the DOM node identity (as observed by the renderer) so
/// the browser can keep `FrameId` stable across geometry/navigation updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubframeToken(u64);

impl SubframeToken {
  pub const fn new(raw: u64) -> Self {
    Self(raw)
  }

  /// MVP token generation: reuse the DOM pre-order id (`StyledNode.node_id`) for the `<iframe>`
  /// element.
  ///
  /// This is stable across paints as long as the DOM structure is unchanged.
  pub const fn from_styled_node_id(node_id: usize) -> Self {
    Self(node_id as u64)
  }

  pub const fn raw(self) -> u64 {
    self.0
  }
}

/// Backwards-compatible alias for the per-iframe stable identifier.
///
/// Historically this was named `SubframeId`; the codebase now uses [`SubframeToken`] to emphasize
/// that this identifier is derived from renderer-reported DOM identity rather than the browser's
/// internal [`FrameId`].
pub type SubframeId = SubframeToken;

// -----------------------------------------------------------------------------
// IPC-ish message surface (browser → renderer / renderer → browser).
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserToRendererFrame {
  CreateFrame {
    frame_id: FrameId,
    parent_frame_id: Option<FrameId>,
  },
  Navigate {
    frame_id: FrameId,
    url: String,
  },
  Resize {
    frame_id: FrameId,
    width: u32,
    height: u32,
    device_pixel_ratio: f32,
  },
  DestroyFrame {
    frame_id: FrameId,
  },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredSubframe {
  pub subframe_token: SubframeToken,
  pub url: String,
  /// True when the iframe's origin is forced to be opaque regardless of URL.
  ///
  /// This is currently used for `<iframe sandbox>` when the token list does **not** include
  /// `allow-same-origin`.
  pub force_opaque_origin: bool,
  /// Iframe content box rect in CSS pixels, relative to the parent frame viewport origin.
  pub rect: Rect,
  /// Clip rect in CSS pixels (same coordinate space as `rect`).
  pub clip: Rect,
  /// Whether the embedding `<iframe>` participates in hit testing / pointer events.
  ///
  /// When `false`, the browser must treat the embedded subframe as non-interactive and allow input
  /// to pass through to underlying content (e.g. `pointer-events: none`, `visibility: hidden`, or
  /// `inert` on the `<iframe>` element).
  pub hit_testable: bool,
}

impl DiscoveredSubframe {
  /// Construct a discovered subframe from a raw `<iframe src>` attribute and base URL context.
  ///
  /// Returns `None` for "no navigation" cases (empty/whitespace-only/fragment-only/javascript:...).
  pub fn from_raw_src(
    subframe_token: SubframeToken,
    raw_src: Option<&str>,
    base_url: &str,
    force_opaque_origin: bool,
    rect: Rect,
    clip: Rect,
    hit_testable: bool,
  ) -> Option<Self> {
    match iframe_navigation_from_src(raw_src, base_url) {
      IframeNavigation::None => None,
      IframeNavigation::AboutBlank => Some(Self {
        subframe_token,
        url: "about:blank".to_string(),
        force_opaque_origin,
        rect,
        clip,
        hit_testable,
      }),
      IframeNavigation::Url(url) => Some(Self {
        subframe_token,
        url,
        force_opaque_origin,
        rect,
        clip,
        hit_testable,
      }),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RendererToBrowserFrame {
  SubframesDiscovered {
    parent_frame_id: FrameId,
    /// Device pixel ratio of the *parent* frame at the time this geometry was computed.
    ///
    /// The browser uses this to:
    /// - map `DiscoveredSubframe::rect` to device pixels in the compositor, and
    /// - instruct child renderers to re-render at the correct resolution.
    parent_dpr: f32,
    subframes: Vec<DiscoveredSubframe>,
  },
}

// -----------------------------------------------------------------------------
// Frame tree
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FrameEmbedding {
  /// Iframe content box rect in CSS pixels, relative to the parent frame viewport origin.
  pub rect: Rect,
  /// Clip rect in CSS pixels (same coordinate space as `rect`).
  pub clip: Rect,
  pub hit_testable: bool,
  /// Device pixel ratio of the parent frame at the time this geometry was computed.
  pub parent_dpr: f32,
}

#[derive(Debug)]
pub struct FrameNode {
  pub id: FrameId,
  pub parent: Option<FrameId>,
  pub site: SiteKey,
  pub url: String,
  /// Whether this frame should be treated as having an opaque origin regardless of URL.
  ///
  /// This is currently used for sandboxed iframes where `allow-same-origin` is not present.
  pub force_opaque_origin: bool,
  pub process_id: RendererProcessId,
  pub embedding: Option<FrameEmbedding>,
  children_by_subframe: HashMap<SubframeToken, FrameId>,
}

impl FrameNode {
  fn new_root(id: FrameId, site: SiteKey, url: String, process_id: RendererProcessId) -> Self {
    Self {
      id,
      parent: None,
      site,
      url,
      force_opaque_origin: false,
      process_id,
      embedding: None,
      children_by_subframe: HashMap::new(),
    }
  }

  fn new_child(
    id: FrameId,
    parent: FrameId,
    site: SiteKey,
    url: String,
    process_id: RendererProcessId,
    force_opaque_origin: bool,
    embedding: FrameEmbedding,
  ) -> Self {
    Self {
      id,
      parent: Some(parent),
      site,
      url,
      force_opaque_origin,
      process_id,
      embedding: Some(embedding),
      children_by_subframe: HashMap::new(),
    }
  }

  pub fn child_frame_id(&self, subframe_token: SubframeToken) -> Option<FrameId> {
    self.children_by_subframe.get(&subframe_token).copied()
  }

  pub fn child_count(&self) -> usize {
    self.children_by_subframe.len()
  }

  fn set_child_mapping(&mut self, subframe_token: SubframeToken, child_frame_id: FrameId) {
    self
      .children_by_subframe
      .insert(subframe_token, child_frame_id);
  }

  fn remove_child_mapping(&mut self, subframe_token: SubframeToken) -> Option<FrameId> {
    self.children_by_subframe.remove(&subframe_token)
  }

  fn child_frame_ids(&self) -> Vec<FrameId> {
    self.children_by_subframe.values().copied().collect()
  }
}

#[derive(Debug, Default)]
pub struct FrameTree {
  frames: HashMap<FrameId, FrameNode>,
}

impl FrameTree {
  pub fn frame(&self, id: FrameId) -> Option<&FrameNode> {
    self.frames.get(&id)
  }

  pub fn frame_mut(&mut self, id: FrameId) -> Option<&mut FrameNode> {
    self.frames.get_mut(&id)
  }

  pub fn insert_root(&mut self, node: FrameNode) {
    self.frames.insert(node.id, node);
  }

  pub fn insert_child(&mut self, parent: FrameId, subframe_token: SubframeToken, node: FrameNode) {
    if let Some(parent_node) = self.frames.get_mut(&parent) {
      parent_node.set_child_mapping(subframe_token, node.id);
    }
    self.frames.insert(node.id, node);
  }

  pub fn detach_child(
    &mut self,
    parent: FrameId,
    subframe_token: SubframeToken,
  ) -> Option<FrameId> {
    self
      .frames
      .get_mut(&parent)
      .and_then(|node| node.remove_child_mapping(subframe_token))
  }

  /// Remove a subtree rooted at `frame_id`, returning removed nodes in postorder (children first).
  pub fn remove_subtree(&mut self, frame_id: FrameId) -> Vec<FrameNode> {
    let child_ids = self
      .frames
      .get(&frame_id)
      .map(|node| node.child_frame_ids())
      .unwrap_or_default();

    let mut removed = Vec::new();
    for child in child_ids {
      removed.extend(self.remove_subtree(child));
    }

    if let Some(node) = self.frames.remove(&frame_id) {
      removed.push(node);
    }
    removed
  }

  /// Hit-test a point in the coordinate space of `root_frame_id`, returning the deepest frame that
  /// should receive input.
  ///
  /// This is used by browser-side input routing for OOPIF: it must respect
  /// [`FrameEmbedding::hit_testable`] so iframes with `pointer-events: none` (or inert/hidden) do
  /// not capture clicks/scroll.
  pub fn hit_test(&self, root_frame_id: FrameId, point: Point) -> FrameId {
    self.hit_test_in_frame(root_frame_id, point, MAX_FRAME_HIT_TEST_DEPTH)
  }

  fn hit_test_in_frame(&self, frame_id: FrameId, point: Point, depth_left: usize) -> FrameId {
    if depth_left == 0 || !point.x.is_finite() || !point.y.is_finite() {
      return frame_id;
    }
    let Some(node) = self.frames.get(&frame_id) else {
      return frame_id;
    };

    // `children_by_subframe` is a HashMap; sort by `SubframeToken` so hit testing is deterministic.
    let mut children: Vec<(SubframeToken, FrameId)> = node
      .children_by_subframe
      .iter()
      .map(|(&subframe_token, &child_frame_id)| (subframe_token, child_frame_id))
      .collect();
    children.sort_by_key(|(subframe_token, child_frame_id)| {
      (subframe_token.raw(), child_frame_id.raw())
    });

    // Assume later DOM ids are painted above earlier ones; hit-test in reverse order (topmost first).
    for (_subframe_token, child_frame_id) in children.into_iter().rev() {
      let Some(child_node) = self.frames.get(&child_frame_id) else {
        continue;
      };
      let Some(embedding) = child_node.embedding.as_ref() else {
        continue;
      };
      if !embedding.hit_testable {
        continue;
      }

      if !embedding.rect.contains_point(point) || !embedding.clip.contains_point(point) {
        continue;
      }

      let child_point = Point::new(point.x - embedding.rect.x(), point.y - embedding.rect.y());
      return self.hit_test_in_frame(child_frame_id, child_point, depth_left - 1);
    }

    frame_id
  }
}

// -----------------------------------------------------------------------------
// Process integration
// -----------------------------------------------------------------------------

const MAX_FRAME_HIT_TEST_DEPTH: usize = 64;

/// Minimal message sender surface required by [`SubframesController`].
///
/// Production implementations will likely wrap an IPC channel; unit tests can use an in-memory log.
pub trait FrameCommandSender {
  fn send(&mut self, msg: BrowserToRendererFrame);
}

/// Return true when a child frame should be isolated into its own renderer process.
///
/// For now, isolation is purely "cross-site": different [`SiteKey`] values imply isolation.
pub fn should_isolate_child_frame(parent_site: &SiteKey, child_site: &SiteKey) -> bool {
  parent_site != child_site
}

// -----------------------------------------------------------------------------
// Browser-side orchestration
// -----------------------------------------------------------------------------

pub struct SubframesController<S>
where
  S: ProcessSpawner,
  S::Handle: ProcessHandle + FrameCommandSender,
{
  pub frame_tree: FrameTree,
  pub processes: RendererProcessRegistry<S>,
  next_frame_id: u64,
  max_subframes_per_parent: usize,
}

impl<S> SubframesController<S>
where
  S: ProcessSpawner,
  S::Handle: ProcessHandle + FrameCommandSender,
{
  pub fn new(processes: RendererProcessRegistry<S>) -> Self {
    Self {
      frame_tree: FrameTree::default(),
      processes,
      next_frame_id: 1,
      max_subframes_per_parent: 64,
    }
  }

  pub fn set_max_subframes_per_parent(&mut self, max: usize) {
    self.max_subframes_per_parent = max;
  }

  fn alloc_frame_id(&mut self) -> FrameId {
    let id = FrameId::new(self.next_frame_id);
    self.next_frame_id = self.next_frame_id.saturating_add(1);
    id
  }

  fn send_to_process(&mut self, process_id: RendererProcessId, msg: BrowserToRendererFrame) {
    let Some(handle) = self.processes.handle_mut(process_id) else {
      return;
    };
    handle.send(msg);
  }

  fn destroy_frame_subtree(&mut self, frame_id: FrameId) {
    let removed = self.frame_tree.remove_subtree(frame_id);
    for node in removed {
      self.send_to_process(
        node.process_id,
        BrowserToRendererFrame::DestroyFrame { frame_id: node.id },
      );
      self.processes.release_frame(node.process_id, node.id);
    }
  }

  /// Destroy and detach all current children of `frame_id`, leaving the frame itself intact.
  ///
  /// This is used when a frame navigates: per browser semantics, a navigation replaces the document
  /// and therefore tears down any existing descendant browsing contexts.
  fn destroy_frame_children(&mut self, frame_id: FrameId) {
    let children: Vec<(SubframeToken, FrameId)> = self
      .frame_tree
      .frame(frame_id)
      .map(|node| {
        node
          .children_by_subframe
          .iter()
          .map(|(&subframe_token, &child_frame_id)| (subframe_token, child_frame_id))
          .collect()
      })
      .unwrap_or_default();

    for (subframe_token, child_frame_id) in children {
      let _ = self.frame_tree.detach_child(frame_id, subframe_token);
      self.destroy_frame_subtree(child_frame_id);
    }
  }

  pub fn create_root_frame(&mut self, url: &str) -> FrameId {
    let site = site_key_for_navigation(url, None, false);
    let process_id = self.processes.get_or_spawn(site.clone());
    let frame_id = self.alloc_frame_id();

    self.frame_tree.insert_root(FrameNode::new_root(
      frame_id,
      site,
      url.to_string(),
      process_id,
    ));
    self.processes.retain_frame(process_id, frame_id);

    self.send_to_process(
      process_id,
      BrowserToRendererFrame::CreateFrame {
        frame_id,
        parent_frame_id: None,
      },
    );
    self.send_to_process(
      process_id,
      BrowserToRendererFrame::Navigate {
        frame_id,
        url: url.to_string(),
      },
    );

    frame_id
  }

  pub fn handle_renderer_message(&mut self, msg: RendererToBrowserFrame) {
    match msg {
      RendererToBrowserFrame::SubframesDiscovered {
        parent_frame_id,
        parent_dpr,
        subframes,
      } => self.handle_subframes_discovered(parent_frame_id, parent_dpr, subframes),
    }
  }

  pub fn handle_subframes_discovered(
    &mut self,
    parent_frame_id: FrameId,
    parent_dpr: f32,
    subframes: Vec<DiscoveredSubframe>,
  ) {
    let Some(parent_site) = self
      .frame_tree
      .frame(parent_frame_id)
      .map(|node| node.site.clone())
    else {
      return;
    };

    let Some(parent_process_id) = self
      .frame_tree
      .frame(parent_frame_id)
      .map(|node| node.process_id)
    else {
      return;
    };

    let parent_dpr = sanitize_dpr(parent_dpr);

    let existing_children: HashMap<SubframeToken, FrameId> = self
      .frame_tree
      .frame(parent_frame_id)
      .map(|node| node.children_by_subframe.clone())
      .unwrap_or_default();

    // Renderer messages are untrusted: keep the work per update bounded so a hostile page cannot
    // force the browser to build arbitrarily large `HashSet`s or spend unbounded time iterating a
    // huge `subframes` vector.
    //
    // `max_subframes_per_parent` is the authoritative cap for how many child frames we will track.
    // We allow a small multiple when scanning updates so existing children that happen to appear
    // later in the list still have a chance to be recognized.
    let mut subframes = subframes;
    let max_reports = self.max_subframes_per_parent.saturating_mul(4).max(self.max_subframes_per_parent);
    if subframes.len() > max_reports {
      subframes.truncate(max_reports);
    }

    let mut reported_ids: HashSet<SubframeToken> = HashSet::with_capacity(subframes.len());
    for subframe in &subframes {
      reported_ids.insert(subframe.subframe_token);
    }

    // Destroy any existing frames that disappeared.
    for (subframe_token, child_frame_id) in &existing_children {
      if !reported_ids.contains(subframe_token) {
        let _ = self.frame_tree.detach_child(parent_frame_id, *subframe_token);
        self.destroy_frame_subtree(*child_frame_id);
      }
    }

    let mut current_child_count = self
      .frame_tree
      .frame(parent_frame_id)
      .map(|node| node.child_count())
      .unwrap_or(0);

    for subframe in subframes {
      let already_exists = self
        .frame_tree
        .frame(parent_frame_id)
        .and_then(|node| node.child_frame_id(subframe.subframe_token))
        .is_some();

      if !already_exists && current_child_count >= self.max_subframes_per_parent {
        continue;
      }

      let embedding = FrameEmbedding {
        rect: subframe.rect,
        clip: subframe.clip,
        hit_testable: subframe.hit_testable,
        parent_dpr,
      };

      let existing_child_frame_id = self
        .frame_tree
        .frame(parent_frame_id)
        .and_then(|node| node.child_frame_id(subframe.subframe_token));

        if let Some(child_frame_id) = existing_child_frame_id {
          // Existing child frame: update geometry and handle potential process changes.
          let (current_process, needs_nav, old_rect, existing_site) = self
            .frame_tree
            .frame(child_frame_id)
            .map(|node| {
              (
                node.process_id,
                node.url.as_str() != subframe.url || node.force_opaque_origin != subframe.force_opaque_origin,
                node.embedding.as_ref().map(|e| (e.rect, e.parent_dpr)),
                node.site.clone(),
              )
            })
            .unwrap_or((parent_process_id, true, None, parent_site.clone()));

        let child_site = if needs_nav {
          site_key_for_navigation(&subframe.url, Some(&parent_site), subframe.force_opaque_origin)
        } else {
          existing_site
        };
        let isolate = should_isolate_child_frame(&parent_site, &child_site);
        let desired_site_for_process = if isolate {
          child_site.clone()
        } else {
          parent_site.clone()
        };

        let needs_resize = old_rect.is_some_and(|(r, old_dpr)| {
          r.width() != subframe.rect.width()
            || r.height() != subframe.rect.height()
            || old_dpr != parent_dpr
        });

        let desired_existing_process = if isolate {
          self.processes.process_for_site(&desired_site_for_process)
        } else {
          Some(parent_process_id)
        };
        let needs_process_change = desired_existing_process != Some(current_process);

        if needs_nav || needs_process_change {
          // Navigations (including sandbox/opaque-origin toggles that imply an origin change) replace
          // the document, so any existing descendant frame tree must be torn down.
          self.destroy_frame_children(child_frame_id);
        }

        // Update stored geometry.
        if let Some(child_node) = self.frame_tree.frame_mut(child_frame_id) {
          child_node.embedding = Some(embedding.clone());
        }

        if needs_process_change {
          // Move the frame to its new process.
          self.send_to_process(
            current_process,
            BrowserToRendererFrame::DestroyFrame {
              frame_id: child_frame_id,
            },
          );
          self.processes.release_frame(current_process, child_frame_id);

          let new_process = self.processes.get_or_spawn(desired_site_for_process.clone());
          self.processes.retain_frame(new_process, child_frame_id);

          if let Some(child_node) = self.frame_tree.frame_mut(child_frame_id) {
            child_node.site = child_site.clone();
            child_node.process_id = new_process;
            child_node.url = subframe.url.clone();
            child_node.force_opaque_origin = subframe.force_opaque_origin;
          }

          self.send_to_process(
            new_process,
            BrowserToRendererFrame::CreateFrame {
              frame_id: child_frame_id,
              parent_frame_id: Some(parent_frame_id),
            },
          );
          self.send_to_process(
            new_process,
            BrowserToRendererFrame::Navigate {
              frame_id: child_frame_id,
              url: subframe.url.clone(),
            },
          );
          let (w, h) = size_from_rect(subframe.rect);
          self.send_to_process(
            new_process,
            BrowserToRendererFrame::Resize {
              frame_id: child_frame_id,
              width: w,
              height: h,
              device_pixel_ratio: parent_dpr,
            },
          );
          continue;
        }

        if needs_nav {
          if let Some(child_node) = self.frame_tree.frame_mut(child_frame_id) {
            child_node.url = subframe.url.clone();
            child_node.site = child_site.clone();
            child_node.force_opaque_origin = subframe.force_opaque_origin;
          }
          self.send_to_process(
            current_process,
            BrowserToRendererFrame::Navigate {
              frame_id: child_frame_id,
              url: subframe.url.clone(),
            },
          );
        }

        if needs_resize {
          let (w, h) = size_from_rect(subframe.rect);
          self.send_to_process(
            current_process,
            BrowserToRendererFrame::Resize {
              frame_id: child_frame_id,
              width: w,
              height: h,
              device_pixel_ratio: parent_dpr,
            },
          );
        }
      } else {
        // New child frame.
        let child_site =
          site_key_for_navigation(&subframe.url, Some(&parent_site), subframe.force_opaque_origin);
        let isolate = should_isolate_child_frame(&parent_site, &child_site);
        let frame_id = self.alloc_frame_id();
        let process_id = if isolate {
          self.processes.get_or_spawn(child_site.clone())
        } else {
          parent_process_id
        };
        self.processes.retain_frame(process_id, frame_id);

        self.frame_tree.insert_child(
          parent_frame_id,
          subframe.subframe_token,
          FrameNode::new_child(
            frame_id,
            parent_frame_id,
            child_site.clone(),
            subframe.url.clone(),
            process_id,
            subframe.force_opaque_origin,
            embedding.clone(),
          ),
        );

        self.send_to_process(
          process_id,
          BrowserToRendererFrame::CreateFrame {
            frame_id,
            parent_frame_id: Some(parent_frame_id),
          },
        );
        self.send_to_process(
          process_id,
          BrowserToRendererFrame::Navigate {
            frame_id,
            url: subframe.url.clone(),
          },
        );
        let (w, h) = size_from_rect(subframe.rect);
        self.send_to_process(
          process_id,
          BrowserToRendererFrame::Resize {
            frame_id,
            width: w,
            height: h,
            device_pixel_ratio: parent_dpr,
          },
        );

        current_child_count = current_child_count.saturating_add(1);
      }
    }
  }
}

fn size_from_rect(rect: Rect) -> (u32, u32) {
  let sanitize = |v: f32| {
    if v.is_finite() && v > 0.0 {
      v.ceil().min(u32::MAX as f32) as u32
    } else {
      1
    }
  };
  (sanitize(rect.width()), sanitize(rect.height()))
}

fn sanitize_dpr(dpr: f32) -> f32 {
  if dpr.is_finite() && dpr > 0.0 {
    dpr
  } else {
    1.0
  }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};

  #[test]
  fn discovered_subframe_from_raw_src_whitespace_only_is_none() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    assert_eq!(
      DiscoveredSubframe::from_raw_src(
        SubframeToken::new(1),
        Some("   "),
        "https://example.com/",
        false,
        rect,
        rect,
        true,
      ),
      None
    );
  }

  #[test]
  fn discovered_subframe_from_raw_src_fragment_only_is_none() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    assert_eq!(
      DiscoveredSubframe::from_raw_src(
        SubframeToken::new(1),
        Some("#"),
        "https://example.com/",
        false,
        rect,
        rect,
        true
      ),
      None
    );
  }

  #[test]
  fn discovered_subframe_from_raw_src_javascript_is_none() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    assert_eq!(
      DiscoveredSubframe::from_raw_src(
        SubframeToken::new(1),
        Some("javascript:alert(1)"),
        "https://example.com/",
        false,
        rect,
        rect,
        true,
      ),
      None
    );
  }

  #[test]
  fn discovered_subframe_from_raw_src_missing_defaults_to_about_blank() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    let sub = DiscoveredSubframe::from_raw_src(
      SubframeToken::new(1),
      None,
      "https://example.com/",
      false,
      rect,
      rect,
      true,
    )
    .expect("missing src should yield about:blank");
    assert_eq!(sub.url, "about:blank");
  }

  #[test]
  fn discovered_subframe_from_raw_src_trims_ascii_whitespace() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    let sub = DiscoveredSubframe::from_raw_src(
      SubframeToken::new(1),
      Some(" \t  https://example.com"),
      "https://bad.example/",
      false,
      rect,
      rect,
      true,
    )
    .expect("expected URL navigation");
    assert_eq!(sub.url, "https://example.com/");
  }

  #[test]
  fn discovered_subframe_from_raw_src_does_not_trim_non_ascii_whitespace() {
    let rect = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
    let nbsp = "\u{00A0}";
    let src = format!("foo{nbsp}");
    let sub = DiscoveredSubframe::from_raw_src(
      SubframeToken::new(1),
      Some(&src),
      "https://example.com/",
      false,
      rect,
      rect,
      true,
    )
    .expect("expected URL navigation");
    assert_eq!(sub.url, "https://example.com/foo%C2%A0");
  }

  #[derive(Debug)]
  struct FakeHandle {
    id: RendererProcessId,
    log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>>,
    terminate_count: Arc<AtomicUsize>,
  }

  impl ProcessHandle for FakeHandle {
    fn id(&self) -> RendererProcessId {
      self.id
    }

    fn terminate(&mut self) {
      self.terminate_count.fetch_add(1, Ordering::Relaxed);
    }
  }

  impl FrameCommandSender for FakeHandle {
    fn send(&mut self, msg: BrowserToRendererFrame) {
      let mut guard = self.log.lock().unwrap_or_else(|e| e.into_inner());
      guard.entry(self.id).or_default().push(msg);
    }
  }

  #[derive(Debug)]
  struct FakeSpawner {
    next_id: u64,
    log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>>,
    terminate_count: Arc<AtomicUsize>,
  }

  impl FakeSpawner {
    fn new(
      log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>>,
      terminate_count: Arc<AtomicUsize>,
    ) -> Self {
      Self {
        next_id: 1,
        log,
        terminate_count,
      }
    }
  }

  impl ProcessSpawner for FakeSpawner {
    type Handle = FakeHandle;

    fn spawn(&mut self, _site: &SiteKey) -> Self::Handle {
      let id = RendererProcessId::new(self.next_id);
      self.next_id += 1;
      FakeHandle {
        id,
        log: Arc::clone(&self.log),
        terminate_count: Arc::clone(&self.terminate_count),
      }
    }
  }

  fn test_subframe(id: u64, url: &str, w: f32, h: f32) -> DiscoveredSubframe {
    DiscoveredSubframe {
      subframe_token: SubframeToken::new(id),
      url: url.to_string(),
      force_opaque_origin: false,
      rect: Rect::from_xywh(0.0, 0.0, w, h),
      clip: Rect::from_xywh(0.0, 0.0, w, h),
      hit_testable: true,
    }
  }

  fn logged_msgs(
    log: &Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>>,
    process_id: RendererProcessId,
  ) -> Vec<BrowserToRendererFrame> {
    log
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(&process_id)
      .cloned()
      .unwrap_or_default()
  }

  #[test]
  fn frame_hit_testing_ignores_non_hit_testable_iframe() {
    let root = FrameId::new(1);
    let child = FrameId::new(2);

    let mut tree = FrameTree::default();
    tree.insert_root(FrameNode::new_root(
      root,
      SiteKey::Opaque(1),
      "https://root.test/".to_string(),
      RendererProcessId::new(1),
    ));
    tree.insert_child(
      root,
      SubframeToken::new(1),
      FrameNode::new_child(
        child,
        root,
        SiteKey::Opaque(2),
        "https://child.test/".to_string(),
        RendererProcessId::new(2),
        false,
        FrameEmbedding {
          rect: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          clip: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          hit_testable: false,
          parent_dpr: 1.0,
        },
      ),
    );

    assert_eq!(
      tree.hit_test(root, Point::new(10.0, 10.0)),
      root,
      "expected non-hit-testable iframe to be ignored during frame hit testing"
    );
  }

  #[test]
  fn frame_hit_testing_returns_child_when_hit_testable() {
    let root = FrameId::new(1);
    let child = FrameId::new(2);

    let mut tree = FrameTree::default();
    tree.insert_root(FrameNode::new_root(
      root,
      SiteKey::Opaque(1),
      "https://root.test/".to_string(),
      RendererProcessId::new(1),
    ));
    tree.insert_child(
      root,
      SubframeToken::new(1),
      FrameNode::new_child(
        child,
        root,
        SiteKey::Opaque(2),
        "https://child.test/".to_string(),
        RendererProcessId::new(2),
        false,
        FrameEmbedding {
          rect: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          clip: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          hit_testable: true,
          parent_dpr: 1.0,
        },
      ),
    );

    assert_eq!(tree.hit_test(root, Point::new(10.0, 10.0)), child);
    assert_eq!(
      tree.hit_test(root, Point::new(80.0, 80.0)),
      root,
      "outside iframe bounds should hit-test to the root frame"
    );
  }

  #[test]
  fn cross_origin_subframes_create_distinct_processes() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![
        test_subframe(1, "https://a.test/", 100.0, 50.0),
        test_subframe(2, "https://b.test/", 80.0, 40.0),
      ],
    });

    assert_eq!(
      browser.processes.process_count(),
      3,
      "expected a distinct process per site (parent + 2 children)"
    );

    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    let child1_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child1 frame exists");
    let child2_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(2)))
      .expect("child2 frame exists");

    let child1_process = browser
      .frame_tree
      .frame(child1_frame)
      .expect("child1 node exists")
      .process_id;
    let child2_process = browser
      .frame_tree
      .frame(child2_frame)
      .expect("child2 node exists")
      .process_id;

    assert_ne!(child1_process, parent_process);
    assert_ne!(child2_process, parent_process);
    assert_ne!(child1_process, child2_process);

    let msgs1 = logged_msgs(&log, child1_process);
    assert!(
      msgs1.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::CreateFrame { frame_id, parent_frame_id: Some(pid) }
          if *frame_id == child1_frame && *pid == root
      )),
      "expected CreateFrame for child1, got {msgs1:?}"
    );
    assert!(
      msgs1.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::Navigate { frame_id, url }
          if *frame_id == child1_frame && url == "https://a.test/"
      )),
      "expected Navigate for child1, got {msgs1:?}"
    );
    assert!(
      msgs1.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::Resize { frame_id, width, height, device_pixel_ratio }
          if *frame_id == child1_frame && *width == 100 && *height == 50 && *device_pixel_ratio == 1.0
      )),
      "expected Resize for child1, got {msgs1:?}"
    );
  }

  #[test]
  fn same_origin_subframe_reuses_parent_process() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://example.test/");
    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(1, "https://example.test/inner", 10.0, 20.0)],
    });

    assert_eq!(
      browser.processes.process_count(),
      1,
      "expected same-site iframe to reuse the parent process"
    );

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child frame exists");
    let child_process = browser
      .frame_tree
      .frame(child_frame)
      .expect("child node exists")
      .process_id;
    assert_eq!(child_process, parent_process);

    let parent_msgs = logged_msgs(&log, parent_process);
    assert!(
      parent_msgs.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::CreateFrame { frame_id, parent_frame_id: Some(pid) }
          if *frame_id == child_frame && *pid == root
      )),
      "expected CreateFrame on the parent process, got {parent_msgs:?}"
    );
  }

  #[test]
  fn stable_subframe_tokens_reuse_frame_ids_and_only_update_geometry() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://example.test/");
    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    let token = SubframeToken::new(1);
    let url = "https://example.test/inner";

    let rect_a = Rect::from_xywh(0.0, 0.0, 100.0, 50.0);
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![DiscoveredSubframe {
        subframe_token: token,
        url: url.to_string(),
        force_opaque_origin: false,
        rect: rect_a,
        clip: rect_a,
        hit_testable: true,
      }],
    });

    let child_a = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(token))
      .expect("child frame exists");
    let embedding_a = browser
      .frame_tree
      .frame(child_a)
      .and_then(|n| n.embedding.as_ref())
      .expect("child embedding exists");
    assert_eq!(embedding_a.rect, rect_a);

    let msgs_after_first = logged_msgs(&log, parent_process);
    let create_count_first = msgs_after_first
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::CreateFrame { frame_id, .. } if *frame_id == child_a
      ))
      .count();
    let destroy_count_first = msgs_after_first
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::DestroyFrame { frame_id } if *frame_id == child_a
      ))
      .count();
    let resize_count_first = msgs_after_first
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::Resize { frame_id, .. } if *frame_id == child_a
      ))
      .count();
    assert_eq!(create_count_first, 1);
    assert_eq!(destroy_count_first, 0);
    assert_eq!(resize_count_first, 1);

    // Send the same token again with a different rect (e.g. reflow/scroll). The FrameId should be
    // reused and no Create/Destroy churn should occur.
    let rect_b = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![DiscoveredSubframe {
        subframe_token: token,
        url: url.to_string(),
        force_opaque_origin: false,
        rect: rect_b,
        clip: rect_b,
        hit_testable: true,
      }],
    });

    let child_b = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(token))
      .expect("child frame still exists");
    assert_eq!(child_b, child_a, "expected stable token to reuse FrameId");

    let embedding_b = browser
      .frame_tree
      .frame(child_b)
      .and_then(|n| n.embedding.as_ref())
      .expect("child embedding exists");
    assert_eq!(embedding_b.rect, rect_b);

    let msgs_after_second = logged_msgs(&log, parent_process);
    let create_count_second = msgs_after_second
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::CreateFrame { frame_id, .. } if *frame_id == child_a
      ))
      .count();
    let destroy_count_second = msgs_after_second
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::DestroyFrame { frame_id } if *frame_id == child_a
      ))
      .count();
    let resize_count_second = msgs_after_second
      .iter()
      .filter(|msg| matches!(
        msg,
        BrowserToRendererFrame::Resize { frame_id, .. } if *frame_id == child_a
      ))
      .count();
    assert_eq!(create_count_second, 1);
    assert_eq!(destroy_count_second, 0);
    assert_eq!(
      resize_count_second, 1,
      "expected only geometry updates, not additional resizes"
    );

    assert_eq!(
      browser.processes.process_count(),
      1,
      "expected child to reuse the parent process for same-site URLs"
    );
    assert_eq!(
      browser
        .frame_tree
        .frame(root)
        .map(|n| n.child_count())
        .unwrap_or(0),
      1,
      "expected only one child frame"
    );
    assert_eq!(
      terminate_count.load(Ordering::Relaxed),
      0,
      "expected no process churn for geometry-only updates"
    );
  }

  #[test]
  fn removing_iframe_sends_destroy_and_releases_process() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");
    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(1, "https://child.test/", 50.0, 50.0)],
    });

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child frame exists");
    let child_process = browser
      .frame_tree
      .frame(child_frame)
      .expect("child node exists")
      .process_id;
    assert_ne!(child_process, parent_process);
    assert_eq!(browser.processes.process_count(), 2);

    // Now report no subframes → child should be destroyed and its process ref released.
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: Vec::new(),
    });

    assert_eq!(
      browser.processes.process_count(),
      1,
      "expected child process to be released once the iframe disappears"
    );
    assert_eq!(
      terminate_count.load(Ordering::Relaxed),
      1,
      "expected the released process to be terminated"
    );

    let msgs = logged_msgs(&log, child_process);
    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::DestroyFrame { frame_id } if *frame_id == child_frame
      )),
      "expected DestroyFrame for removed child, got {msgs:?}"
    );

    assert!(
      browser
        .frame_tree
        .frame(root)
        .map(|n| n.child_count())
        .unwrap_or(0)
        == 0,
      "expected frame tree to drop the iframe child"
    );
  }

  #[test]
  fn sandboxed_srcdoc_subframes_force_opaque_site_keys_and_stable_processes() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");
    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    let mut opaque1 = test_subframe(1, "about:srcdoc", 10.0, 10.0);
    opaque1.force_opaque_origin = true;
    let mut opaque2 = test_subframe(2, "about:srcdoc", 10.0, 10.0);
    opaque2.force_opaque_origin = true;

    let msg = RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(0, "about:srcdoc", 10.0, 10.0), opaque1, opaque2],
    };

    browser.handle_renderer_message(msg.clone());

    assert_eq!(
      browser.processes.process_count(),
      3,
      "expected parent + 2 sandboxed opaque-origin srcdoc iframes to use distinct processes"
    );

    let child0_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(0)))
      .expect("child0 frame exists");
    let child1_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child1 frame exists");
    let child2_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(2)))
      .expect("child2 frame exists");

    let child0_process = browser
      .frame_tree
      .frame(child0_frame)
      .expect("child0 node exists")
      .process_id;
    let child1_process = browser
      .frame_tree
      .frame(child1_frame)
      .expect("child1 node exists")
      .process_id;
    let child2_process = browser
      .frame_tree
      .frame(child2_frame)
      .expect("child2 node exists")
      .process_id;

    assert_eq!(
      child0_process, parent_process,
      "expected non-sandboxed srcdoc iframe to share the parent process"
    );
    assert_ne!(child1_process, parent_process);
    assert_ne!(child2_process, parent_process);
    assert_ne!(child1_process, child2_process);

    // Replaying the same discovery message should not allocate new opaque site keys for existing
    // subframes (no navigation), avoiding process churn.
    browser.handle_renderer_message(msg);
    assert_eq!(browser.processes.process_count(), 3);
    assert_eq!(
      browser
        .frame_tree
        .frame(child1_frame)
        .expect("child1 still exists")
        .process_id,
      child1_process
    );
    assert_eq!(
      browser
        .frame_tree
        .frame(child2_frame)
        .expect("child2 still exists")
        .process_id,
      child2_process
    );
  }

  #[test]
  fn changing_force_opaque_origin_triggers_process_swap_without_url_change() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");
    let parent_process = browser
      .frame_tree
      .frame(root)
      .expect("root frame exists")
      .process_id;

    // Non-sandboxed `about:srcdoc` inherits the parent site key and stays in the same process.
    let subframe_normal = test_subframe(1, "about:srcdoc", 10.0, 10.0);
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![subframe_normal],
    });
    assert_eq!(browser.processes.process_count(), 1);

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child frame exists");
    assert_eq!(
      browser
        .frame_tree
        .frame(child_frame)
        .expect("child node exists")
        .process_id,
      parent_process,
      "expected non-sandboxed srcdoc iframe to share the parent process"
    );

    // Now toggle the iframe to an opaque origin (sandbox without allow-same-origin). Even though the
    // URL is unchanged, the browser must treat the origin change as a navigation boundary and move
    // the frame to an isolated process.
    let mut subframe_opaque = test_subframe(1, "about:srcdoc", 10.0, 10.0);
    subframe_opaque.force_opaque_origin = true;
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![subframe_opaque],
    });

    let child_frame_after = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child still exists");
    assert_eq!(child_frame_after, child_frame, "expected FrameId stability across updates");

    let child_process_isolated = browser
      .frame_tree
      .frame(child_frame_after)
      .expect("child node exists")
      .process_id;
    assert_ne!(
      child_process_isolated, parent_process,
      "expected opaque-origin iframe to be isolated into a separate process"
    );
    assert_eq!(
      browser.processes.process_count(),
      2,
      "expected a new renderer process to be spawned for the opaque-origin iframe"
    );

    // Toggle back to non-opaque: should move the frame back to the parent process and terminate the
    // now-unused isolated renderer.
    let subframe_back = test_subframe(1, "about:srcdoc", 10.0, 10.0);
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![subframe_back],
    });
    assert_eq!(browser.processes.process_count(), 1);
    assert_eq!(
      terminate_count.load(Ordering::Relaxed),
      1,
      "expected the isolated process to be terminated when the frame returns to the parent process"
    );
    assert_eq!(
      browser
        .frame_tree
        .frame(child_frame)
        .expect("child still exists")
        .process_id,
      parent_process
    );
  }

  #[test]
  fn navigating_child_frame_clears_existing_descendant_subframes() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");

    // Discover a cross-origin iframe so it is isolated into its own process.
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(1, "https://child.test/", 10.0, 10.0)],
    });

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child frame exists");

    // Now have the child frame report its own cross-origin iframe.
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: child_frame,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(2, "https://grandchild.test/", 5.0, 6.0)],
    });

    let grandchild_frame = browser
      .frame_tree
      .frame(child_frame)
      .and_then(|n| n.child_frame_id(SubframeToken::new(2)))
      .expect("grandchild frame exists");
    let grandchild_process = browser
      .frame_tree
      .frame(grandchild_frame)
      .expect("grandchild node exists")
      .process_id;

    assert_eq!(
      browser.processes.process_count(),
      3,
      "expected parent + child + grandchild processes before navigation"
    );

    // Navigate the child frame within the same site (path-only change). This should clear the
    // grandchild subtree immediately even before the new document reports its own subframes.
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(1, "https://child.test/other", 10.0, 10.0)],
    });

    assert!(
      browser.frame_tree.frame(grandchild_frame).is_none(),
      "expected grandchild frame to be removed from the browser frame tree on navigation"
    );
    assert_eq!(
      browser
        .frame_tree
        .frame(child_frame)
        .expect("child frame still exists")
        .child_count(),
      0,
      "expected child frame to have no remaining subframes after navigation"
    );

    assert_eq!(
      browser.processes.process_count(),
      2,
      "expected the grandchild process to be released after navigation"
    );
    assert_eq!(
      terminate_count.load(Ordering::Relaxed),
      1,
      "expected the grandchild process to be terminated"
    );

    let msgs = logged_msgs(&log, grandchild_process);
    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        BrowserToRendererFrame::DestroyFrame { frame_id } if *frame_id == grandchild_frame
      )),
      "expected DestroyFrame for grandchild on navigation, got {msgs:?}"
    );
  }

  #[test]
  fn parent_dpr_change_triggers_resize_to_child_frames() {
    let log: Arc<Mutex<HashMap<RendererProcessId, Vec<BrowserToRendererFrame>>>> =
      Arc::new(Mutex::new(HashMap::new()));
    let terminate_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(Arc::clone(&log), Arc::clone(&terminate_count));
    let processes = RendererProcessRegistry::new(spawner);
    let mut browser = SubframesController::new(processes);
    browser.set_max_subframes_per_parent(8);

    let root = browser.create_root_frame("https://parent.test/");
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 1.0,
      subframes: vec![test_subframe(1, "https://child.test/", 10.0, 20.0)],
    });

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeToken::new(1)))
      .expect("child frame exists");
    let child_process = browser
      .frame_tree
      .frame(child_frame)
      .expect("child node exists")
      .process_id;

    let msgs_before = logged_msgs(&log, child_process);
    let resize_before: Vec<_> = msgs_before
      .iter()
      .filter_map(|m| match m {
        BrowserToRendererFrame::Resize {
          frame_id,
          width,
          height,
          device_pixel_ratio,
        } => Some((*frame_id, *width, *height, *device_pixel_ratio)),
        _ => None,
      })
      .collect();
    assert_eq!(resize_before.len(), 1, "expected initial resize message");
    assert_eq!(
      resize_before[0],
      (child_frame, 10, 20, 1.0),
      "expected initial Resize to carry CSS size + DPR"
    );

    // Now simulate a HiDPI / device-pixel-ratio change in the parent frame without changing the
    // iframe element size. The browser should still send a Resize so the child can re-render at the
    // correct device resolution.
    browser.handle_renderer_message(RendererToBrowserFrame::SubframesDiscovered {
      parent_frame_id: root,
      parent_dpr: 2.0,
      subframes: vec![test_subframe(1, "https://child.test/", 10.0, 20.0)],
    });

    let msgs_after = logged_msgs(&log, child_process);
    let resize_after: Vec<_> = msgs_after
      .iter()
      .filter_map(|m| match m {
        BrowserToRendererFrame::Resize {
          frame_id,
          width,
          height,
          device_pixel_ratio,
        } => Some((*frame_id, *width, *height, *device_pixel_ratio)),
        _ => None,
      })
      .collect();
    assert_eq!(resize_after.len(), 2, "expected resize to be sent again on DPR change");
    assert!(
      resize_after.contains(&(child_frame, 10, 20, 2.0)),
      "expected a Resize with updated DPR, got {resize_after:?}"
    );
  }
}
