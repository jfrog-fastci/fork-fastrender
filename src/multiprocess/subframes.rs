use crate::geometry::{Point, Rect};
use crate::site_isolation::site_key_for_navigation;
use crate::site_isolation::SiteKey;
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
pub struct SubframeId(u64);

impl SubframeId {
  pub const fn new(raw: u64) -> Self {
    Self(raw)
  }

  pub const fn raw(self) -> u64 {
    self.0
  }
}

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
  },
  DestroyFrame {
    frame_id: FrameId,
  },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredSubframe {
  pub id: SubframeId,
  pub url: String,
  /// True when the iframe's origin is forced to be opaque regardless of URL.
  ///
  /// This is currently used for `<iframe sandbox>` when the token list does **not** include
  /// `allow-same-origin`.
  pub force_opaque_origin: bool,
  pub rect: Rect,
  pub clip: Rect,
  /// Whether the embedding `<iframe>` participates in hit testing / pointer events.
  ///
  /// When `false`, the browser must treat the embedded subframe as non-interactive and allow input
  /// to pass through to underlying content (e.g. `pointer-events: none`, `visibility: hidden`, or
  /// `inert` on the `<iframe>` element).
  pub hit_testable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RendererToBrowserFrame {
  SubframesDiscovered {
    parent_frame_id: FrameId,
    subframes: Vec<DiscoveredSubframe>,
  },
}

// -----------------------------------------------------------------------------
// Frame tree
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FrameEmbedding {
  pub rect: Rect,
  pub clip: Rect,
  pub hit_testable: bool,
}

#[derive(Debug)]
pub struct FrameNode {
  pub id: FrameId,
  pub parent: Option<FrameId>,
  pub site: SiteKey,
  pub url: String,
  pub process_id: RendererProcessId,
  pub embedding: Option<FrameEmbedding>,
  children_by_subframe: HashMap<SubframeId, FrameId>,
}

impl FrameNode {
  fn new_root(id: FrameId, site: SiteKey, url: String, process_id: RendererProcessId) -> Self {
    Self {
      id,
      parent: None,
      site,
      url,
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
    embedding: FrameEmbedding,
  ) -> Self {
    Self {
      id,
      parent: Some(parent),
      site,
      url,
      process_id,
      embedding: Some(embedding),
      children_by_subframe: HashMap::new(),
    }
  }

  pub fn child_frame_id(&self, subframe_id: SubframeId) -> Option<FrameId> {
    self.children_by_subframe.get(&subframe_id).copied()
  }

  pub fn child_count(&self) -> usize {
    self.children_by_subframe.len()
  }

  fn set_child_mapping(&mut self, subframe_id: SubframeId, child_frame_id: FrameId) {
    self.children_by_subframe.insert(subframe_id, child_frame_id);
  }

  fn remove_child_mapping(&mut self, subframe_id: SubframeId) -> Option<FrameId> {
    self.children_by_subframe.remove(&subframe_id)
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

  pub fn insert_child(&mut self, parent: FrameId, subframe_id: SubframeId, node: FrameNode) {
    if let Some(parent_node) = self.frames.get_mut(&parent) {
      parent_node.set_child_mapping(subframe_id, node.id);
    }
    self.frames.insert(node.id, node);
  }

  pub fn detach_child(&mut self, parent: FrameId, subframe_id: SubframeId) -> Option<FrameId> {
    self
      .frames
      .get_mut(&parent)
      .and_then(|node| node.remove_child_mapping(subframe_id))
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

    // `children_by_subframe` is a HashMap; sort by SubframeId so hit testing is deterministic.
    let mut children: Vec<(SubframeId, FrameId)> = node
      .children_by_subframe
      .iter()
      .map(|(&subframe_id, &child_frame_id)| (subframe_id, child_frame_id))
      .collect();
    children.sort_by_key(|(subframe_id, child_frame_id)| (subframe_id.raw(), child_frame_id.raw()));

    // Assume later DOM ids are painted above earlier ones; hit-test in reverse order (topmost first).
    for (_subframe_id, child_frame_id) in children.into_iter().rev() {
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
        subframes,
      } => self.handle_subframes_discovered(parent_frame_id, subframes),
    }
  }

  pub fn handle_subframes_discovered(&mut self, parent_frame_id: FrameId, subframes: Vec<DiscoveredSubframe>) {
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

    let existing_children: HashMap<SubframeId, FrameId> = self
      .frame_tree
      .frame(parent_frame_id)
      .map(|node| node.children_by_subframe.clone())
      .unwrap_or_default();

    let mut reported_ids: HashSet<SubframeId> = HashSet::new();
    for subframe in &subframes {
      reported_ids.insert(subframe.id);
    }

    // Destroy any existing frames that disappeared.
    for (subframe_id, child_frame_id) in &existing_children {
      if !reported_ids.contains(subframe_id) {
        let _ = self.frame_tree.detach_child(parent_frame_id, *subframe_id);
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
        .and_then(|node| node.child_frame_id(subframe.id))
        .is_some();

      if !already_exists && current_child_count >= self.max_subframes_per_parent {
        continue;
      }

      let embedding = FrameEmbedding {
        rect: subframe.rect,
        clip: subframe.clip,
        hit_testable: subframe.hit_testable,
      };

      let existing_child_frame_id = self
        .frame_tree
        .frame(parent_frame_id)
        .and_then(|node| node.child_frame_id(subframe.id));

      if let Some(child_frame_id) = existing_child_frame_id {
        // Existing child frame: update geometry and handle potential process changes.
        let (current_process, needs_nav, old_rect, existing_site) = self
          .frame_tree
          .frame(child_frame_id)
          .map(|node| {
            (
              node.process_id,
              node.url.as_str() != subframe.url,
              node.embedding.as_ref().map(|e| e.rect),
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

        let needs_resize = old_rect.is_some_and(|r| {
          r.width() != subframe.rect.width() || r.height() != subframe.rect.height()
        });

        let desired_existing_process = if isolate {
          self.processes.process_for_site(&desired_site_for_process)
        } else {
          Some(parent_process_id)
        };
        let needs_process_change = desired_existing_process != Some(current_process);

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
            },
          );
          continue;
        }

        if needs_nav {
          if let Some(child_node) = self.frame_tree.frame_mut(child_frame_id) {
            child_node.url = subframe.url.clone();
            child_node.site = child_site.clone();
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
          subframe.id,
          FrameNode::new_child(
            frame_id,
            parent_frame_id,
            child_site.clone(),
            subframe.url.clone(),
            process_id,
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

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex};

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
      id: SubframeId::new(id),
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
      SubframeId::new(1),
      FrameNode::new_child(
        child,
        root,
        SiteKey::Opaque(2),
        "https://child.test/".to_string(),
        RendererProcessId::new(2),
        FrameEmbedding {
          rect: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          clip: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          hit_testable: false,
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
      SubframeId::new(1),
      FrameNode::new_child(
        child,
        root,
        SiteKey::Opaque(2),
        "https://child.test/".to_string(),
        RendererProcessId::new(2),
        FrameEmbedding {
          rect: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          clip: Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
          hit_testable: true,
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
      .and_then(|n| n.child_frame_id(SubframeId::new(1)))
      .expect("child1 frame exists");
    let child2_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeId::new(2)))
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
        BrowserToRendererFrame::Resize { frame_id, width, height }
          if *frame_id == child1_frame && *width == 100 && *height == 50
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
      .and_then(|n| n.child_frame_id(SubframeId::new(1)))
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
      subframes: vec![test_subframe(1, "https://child.test/", 50.0, 50.0)],
    });

    let child_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeId::new(1)))
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
      .and_then(|n| n.child_frame_id(SubframeId::new(0)))
      .expect("child0 frame exists");
    let child1_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeId::new(1)))
      .expect("child1 frame exists");
    let child2_frame = browser
      .frame_tree
      .frame(root)
      .and_then(|n| n.child_frame_id(SubframeId::new(2)))
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
}
