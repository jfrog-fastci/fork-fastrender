use crate::geometry::Rect;
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
  pub rect: Rect,
  pub clip: Rect,
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
}

// -----------------------------------------------------------------------------
// Process integration
// -----------------------------------------------------------------------------

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
    let site = site_key_for_navigation(url, None);
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
      };

      let child_site = site_key_for_navigation(&subframe.url, Some(&parent_site));
      let isolate = should_isolate_child_frame(&parent_site, &child_site);
      let desired_site_for_process = if isolate {
        child_site.clone()
      } else {
        parent_site.clone()
      };

      if let Some(child_frame_id) = self
        .frame_tree
        .frame(parent_frame_id)
        .and_then(|node| node.child_frame_id(subframe.id))
      {
        // Existing child frame: update geometry and handle potential process changes.
        let current_process = self
          .frame_tree
          .frame(child_frame_id)
          .map(|node| node.process_id)
          .unwrap_or(parent_process_id);
        let needs_nav = self
          .frame_tree
          .frame(child_frame_id)
          .map(|node| node.url.as_str() != subframe.url)
          .unwrap_or(true);

        let old_rect = self
          .frame_tree
          .frame(child_frame_id)
          .and_then(|node| node.embedding.as_ref().map(|e| e.rect));
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
      rect: Rect::from_xywh(0.0, 0.0, w, h),
      clip: Rect::from_xywh(0.0, 0.0, w, h),
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
}

