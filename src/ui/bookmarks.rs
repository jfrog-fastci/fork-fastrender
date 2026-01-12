//! Bookmarks data model and core operations.
//!
//! The UI needs more than a set of URLs:
//! - Stable IDs so editing/reordering doesn't rely on URL uniqueness
//! - User-defined ordering (bookmarks bar + folders)
//! - Titles and timestamps
//!
//! This module is intentionally UI-framework agnostic (no egui/winit types).
//!
//! URL uniqueness policy
//! ---------------------
//! This store **allows duplicate URLs** (e.g. via import). The `toggle` API implements the
//! user-facing "star button" semantics: if *any* bookmark exists for the URL, toggling removes all
//! of them; otherwise it adds a new bookmark to the root.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::ui::url::validate_user_navigation_url_scheme;

pub const BOOKMARK_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BookmarkId(pub u64);

impl BookmarkId {
  fn checked_next(self) -> Option<Self> {
    self.0.checked_add(1).map(Self)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BookmarkNode {
  Bookmark(BookmarkEntry),
  Folder(BookmarkFolder),
}

impl BookmarkNode {
  pub fn id(&self) -> BookmarkId {
    match self {
      Self::Bookmark(entry) => entry.id,
      Self::Folder(folder) => folder.id,
    }
  }

  pub fn parent(&self) -> Option<BookmarkId> {
    match self {
      Self::Bookmark(entry) => entry.parent,
      Self::Folder(folder) => folder.parent,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookmarkEntry {
  pub id: BookmarkId,
  pub url: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Unix epoch milliseconds. `0` means unknown (used by legacy migrations).
  pub added_at_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent: Option<BookmarkId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookmarkFolder {
  pub id: BookmarkId,
  pub title: String,
  /// Unix epoch milliseconds. `0` means unknown (used by legacy migrations).
  pub added_at_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent: Option<BookmarkId>,
  /// Ordered list of child node IDs.
  #[serde(default)]
  pub children: Vec<BookmarkId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookmarkStore {
  pub version: u32,
  #[serde(default = "default_next_id")]
  pub next_id: BookmarkId,
  /// Ordered list of root node IDs (bookmarks bar ordering).
  #[serde(default)]
  pub roots: Vec<BookmarkId>,
  /// All nodes keyed by stable ID.
  #[serde(default)]
  pub nodes: BTreeMap<BookmarkId, BookmarkNode>,
}

fn default_next_id() -> BookmarkId {
  BookmarkId(1)
}

impl Default for BookmarkStore {
  fn default() -> Self {
    Self {
      version: BOOKMARK_STORE_VERSION,
      next_id: default_next_id(),
      roots: Vec::new(),
      nodes: BTreeMap::new(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkError {
  NotFound(BookmarkId),
  ParentNotFound(BookmarkId),
  ParentNotFolder(BookmarkId),
  InvalidFolderTitle,
  InvalidReorder,
  IdExhausted,
  WouldCreateCycle,
  UnsupportedVersion(u32),
  InvalidStore(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkStoreMigration {
  None,
  FromLegacyUrls,
  FromLegacyHeadlessArray,
}

impl BookmarkStore {
  pub fn contains_url(&self, url: &str) -> bool {
    self
      .nodes
      .values()
      .any(|node| matches!(node, BookmarkNode::Bookmark(b) if b.url == url))
  }

  pub fn toggle(&mut self, url: &str, title: Option<&str>) -> bool {
    let url = url.trim();
    if url.is_empty() {
      return false;
    }
    if self.contains_url(url) {
      let _removed = self.remove_by_url(url);
      false
    } else {
      // Root-level bookmark by default.
      self
        .add(url.to_string(), title.map(|s| s.to_string()), None)
        .is_ok()
    }
  }

  pub fn add(
    &mut self,
    url: String,
    title: Option<String>,
    parent: Option<BookmarkId>,
  ) -> Result<BookmarkId, BookmarkError> {
    let url = url.trim().to_string();
    if url.is_empty() {
      return Err(BookmarkError::InvalidStore("bookmark URL is empty".to_string()));
    }
    validate_user_navigation_url_scheme(&url).map_err(BookmarkError::InvalidStore)?;
    let title = normalize_optional_string(title);
    let added_at_ms = now_unix_ms();
    self.add_with_timestamp(url, title, parent, added_at_ms)
  }

  pub fn create_folder(
    &mut self,
    title: String,
    parent: Option<BookmarkId>,
  ) -> Result<BookmarkId, BookmarkError> {
    let title = title.trim();
    if title.is_empty() {
      return Err(BookmarkError::InvalidFolderTitle);
    }

    let id = self.alloc_id()?;
    let folder = BookmarkFolder {
      id,
      title: title.to_string(),
      added_at_ms: now_unix_ms(),
      parent,
      children: Vec::new(),
    };
    let node = BookmarkNode::Folder(folder);
    self.insert_new_node(node)?;
    Ok(id)
  }

  pub fn remove_by_id(&mut self, id: BookmarkId) -> bool {
    if !self.nodes.contains_key(&id) {
      return false;
    }

    // Detach the root entry from ordering lists first.
    let parent = self.nodes.get(&id).and_then(BookmarkNode::parent);
    self.detach_from_parent_list(id, parent);

    let mut subtree = Vec::new();
    self.collect_subtree_ids(id, &mut subtree);
    for node_id in subtree {
      self.nodes.remove(&node_id);
    }
    self.repair_next_id();
    true
  }

  /// Remove all bookmarks whose URL matches `url`.
  ///
  /// Returns the number of removed bookmarks.
  pub fn remove_by_url(&mut self, url: &str) -> usize {
    let ids: Vec<BookmarkId> = self
      .nodes
      .iter()
      .filter_map(|(&id, node)| match node {
        BookmarkNode::Bookmark(bookmark) if bookmark.url == url => Some(id),
        _ => None,
      })
      .collect();
    let mut removed = 0;
    for id in ids {
      if self.remove_by_id(id) {
        removed += 1;
      }
    }
    removed
  }

  pub fn update(
    &mut self,
    id: BookmarkId,
    new_title: Option<String>,
    new_url: String,
    new_parent: Option<BookmarkId>,
  ) -> Result<(), BookmarkError> {
    let old_parent = self
      .nodes
      .get(&id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(id))?;

    if old_parent != new_parent {
      self.move_node(id, new_parent)?;
    }

    let node = self.nodes.get_mut(&id).ok_or(BookmarkError::NotFound(id))?;
    match node {
      BookmarkNode::Bookmark(entry) => {
        entry.url = new_url;
        entry.title = normalize_optional_string(new_title);
      }
      BookmarkNode::Folder(_) => {
        return Err(BookmarkError::InvalidStore(
          "update: id is a folder".to_string(),
        ))
      }
    }
    Ok(())
  }

  pub fn move_node(
    &mut self,
    id: BookmarkId,
    new_parent: Option<BookmarkId>,
  ) -> Result<(), BookmarkError> {
    let old_parent = self
      .nodes
      .get(&id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(id))?;

    if old_parent == new_parent {
      return Ok(());
    }

    if let Some(parent_id) = new_parent {
      match self.nodes.get(&parent_id) {
        Some(BookmarkNode::Folder(_)) => {}
        Some(_) => return Err(BookmarkError::ParentNotFolder(parent_id)),
        None => return Err(BookmarkError::ParentNotFound(parent_id)),
      }
    }

    // Prevent folder cycles (moving a folder into itself or its descendants).
    if matches!(self.nodes.get(&id), Some(BookmarkNode::Folder(_))) {
      if let Some(parent_id) = new_parent {
        if parent_id == id || self.is_ancestor(id, parent_id) {
          return Err(BookmarkError::WouldCreateCycle);
        }
      }
    }

    self.detach_from_parent_list(id, old_parent);
    self.attach_to_parent_list(id, new_parent)?;

    if let Some(node) = self.nodes.get_mut(&id) {
      match node {
        BookmarkNode::Bookmark(entry) => entry.parent = new_parent,
        BookmarkNode::Folder(folder) => folder.parent = new_parent,
      }
    }

    Ok(())
  }

  /// Move a bookmark or folder to a new parent folder (or `None` for the root).
  pub fn move_to_parent(
    &mut self,
    id: BookmarkId,
    new_parent: Option<BookmarkId>,
  ) -> Result<(), BookmarkError> {
    self.move_node(id, new_parent)
  }

  pub fn reorder_root(&mut self, ids_in_new_order: &[BookmarkId]) -> Result<(), BookmarkError> {
    if ids_in_new_order.len() != self.roots.len() {
      return Err(BookmarkError::InvalidReorder);
    }
    let expected: HashSet<BookmarkId> = self.roots.iter().copied().collect();
    let got: HashSet<BookmarkId> = ids_in_new_order.iter().copied().collect();
    if expected != got || got.len() != ids_in_new_order.len() {
      return Err(BookmarkError::InvalidReorder);
    }
    self.roots = ids_in_new_order.to_vec();
    Ok(())
  }

  /// Reorder the root list (bookmarks bar).
  pub fn reorder(&mut self, ids_in_new_order: &[BookmarkId]) -> Result<(), BookmarkError> {
    self.reorder_root(ids_in_new_order)
  }

  pub fn from_json_str_migrating(
    data: &str,
  ) -> Result<(Self, BookmarkStoreMigration), BookmarkError> {
    let value: serde_json::Value =
      serde_json::from_str(data).map_err(|err| BookmarkError::InvalidStore(err.to_string()))?;
    Self::from_json_value_migrating(value)
  }

  pub fn from_json_value_migrating(
    value: serde_json::Value,
  ) -> Result<(Self, BookmarkStoreMigration), BookmarkError> {
    match value {
      serde_json::Value::Object(map) => {
        if let Some(version_value) = map.get("version") {
          let version = version_value.as_u64().ok_or_else(|| {
            BookmarkError::InvalidStore("bookmarks.version must be a number".to_string())
          })?;
          let version = u32::try_from(version).map_err(|_| {
            BookmarkError::InvalidStore("bookmarks.version overflowed u32".to_string())
          })?;
          if version != BOOKMARK_STORE_VERSION {
            return Err(BookmarkError::UnsupportedVersion(version));
          }
          let mut store: BookmarkStore = serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|err| BookmarkError::InvalidStore(err.to_string()))?;
          store.repair_next_id();
          store
            .validate()
            .map_err(|err| BookmarkError::InvalidStore(err))?;
          Ok((store, BookmarkStoreMigration::None))
        } else if map.contains_key("urls") {
          #[derive(Debug, Deserialize)]
          struct LegacyUrls {
            #[serde(default)]
            urls: BTreeSet<String>,
          }
          let legacy: LegacyUrls = serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|err| BookmarkError::InvalidStore(err.to_string()))?;
          Ok((
            Self::from_legacy_urls(legacy.urls),
            BookmarkStoreMigration::FromLegacyUrls,
          ))
        } else {
          Err(BookmarkError::InvalidStore(
            "unrecognized bookmarks JSON object (missing `version` and `urls`)".to_string(),
          ))
        }
      }
      serde_json::Value::Array(entries) => Ok((
        Self::from_legacy_headless_array(entries)?,
        BookmarkStoreMigration::FromLegacyHeadlessArray,
      )),
      other => Err(BookmarkError::InvalidStore(format!(
        "expected bookmarks JSON object/array, got {other}"
      ))),
    }
  }

  fn from_legacy_urls(urls: BTreeSet<String>) -> Self {
    let mut store = Self::default();
    for url in urls {
      // Deterministic migration: preserve the BTreeSet iteration order and use `0` for unknown
      // timestamps.
      store
        .add_with_timestamp(url.clone(), Some(url), None, 0)
        .expect("alloc id should not fail during migration");
    }
    store
  }

  fn from_legacy_headless_array(entries: Vec<serde_json::Value>) -> Result<Self, BookmarkError> {
    #[derive(Debug, Deserialize)]
    struct LegacyEntry {
      #[serde(default)]
      title: Option<String>,
      url: String,
    }

    let mut store = Self::default();
    for raw in entries {
      match raw {
        serde_json::Value::String(url) => {
          store.add_with_timestamp(url.clone(), Some(url), None, 0)?;
        }
        serde_json::Value::Object(_) => {
          let parsed: LegacyEntry = serde_json::from_value(raw)
            .map_err(|err| BookmarkError::InvalidStore(err.to_string()))?;
          let title = normalize_optional_string(parsed.title).or_else(|| Some(parsed.url.clone()));
          store.add_with_timestamp(parsed.url, title, None, 0)?;
        }
        other => {
          return Err(BookmarkError::InvalidStore(format!(
            "legacy bookmarks entries must be objects/strings, got {other}"
          )));
        }
      }
    }
    Ok(store)
  }

  fn add_with_timestamp(
    &mut self,
    url: String,
    title: Option<String>,
    parent: Option<BookmarkId>,
    added_at_ms: u64,
  ) -> Result<BookmarkId, BookmarkError> {
    let id = self.alloc_id()?;
    let entry = BookmarkEntry {
      id,
      url,
      title,
      added_at_ms,
      parent,
    };
    let node = BookmarkNode::Bookmark(entry);
    self.insert_new_node(node)?;
    Ok(id)
  }

  fn alloc_id(&mut self) -> Result<BookmarkId, BookmarkError> {
    let id = self.next_id;
    let next = id.checked_next().ok_or(BookmarkError::IdExhausted)?;
    self.next_id = next;
    Ok(id)
  }

  fn insert_new_node(&mut self, node: BookmarkNode) -> Result<(), BookmarkError> {
    if self.version != BOOKMARK_STORE_VERSION {
      return Err(BookmarkError::UnsupportedVersion(self.version));
    }
    let id = node.id();
    if self.nodes.contains_key(&id) {
      return Err(BookmarkError::InvalidStore(format!(
        "duplicate bookmark id {id:?}"
      )));
    }
    let parent = node.parent();
    if let Some(parent_id) = parent {
      match self.nodes.get(&parent_id) {
        Some(BookmarkNode::Folder(_)) => {}
        Some(_) => return Err(BookmarkError::ParentNotFolder(parent_id)),
        None => return Err(BookmarkError::ParentNotFound(parent_id)),
      }
    }
    self.nodes.insert(id, node);
    match self.attach_to_parent_list(id, parent) {
      Ok(()) => Ok(()),
      Err(err) => {
        // Roll back: keep the store invariant "every node is reachable from roots".
        self.nodes.remove(&id);
        Err(err)
      }
    }
  }

  fn attach_to_parent_list(
    &mut self,
    id: BookmarkId,
    parent: Option<BookmarkId>,
  ) -> Result<(), BookmarkError> {
    match parent {
      Some(parent_id) => match self.nodes.get_mut(&parent_id) {
        Some(BookmarkNode::Folder(folder)) => {
          folder.children.push(id);
          Ok(())
        }
        Some(_) => Err(BookmarkError::ParentNotFolder(parent_id)),
        None => Err(BookmarkError::ParentNotFound(parent_id)),
      },
      None => {
        self.roots.push(id);
        Ok(())
      }
    }
  }

  fn detach_from_parent_list(&mut self, id: BookmarkId, parent: Option<BookmarkId>) {
    match parent {
      Some(parent_id) => {
        if let Some(BookmarkNode::Folder(folder)) = self.nodes.get_mut(&parent_id) {
          remove_first(&mut folder.children, id);
        }
      }
      None => {
        remove_first(&mut self.roots, id);
      }
    }
  }

  fn collect_subtree_ids(&self, id: BookmarkId, out: &mut Vec<BookmarkId>) {
    out.push(id);
    let Some(node) = self.nodes.get(&id) else {
      return;
    };
    if let BookmarkNode::Folder(folder) = node {
      for child in &folder.children {
        self.collect_subtree_ids(*child, out);
      }
    }
  }

  fn is_ancestor(&self, ancestor: BookmarkId, descendant: BookmarkId) -> bool {
    let mut cur = Some(descendant);
    for _ in 0..=self.nodes.len() {
      let Some(id) = cur else {
        return false;
      };
      if id == ancestor {
        return true;
      }
      cur = self.nodes.get(&id).and_then(BookmarkNode::parent);
    }
    // Cycle detected in a corrupted store; be conservative.
    true
  }

  fn repair_next_id(&mut self) {
    let max = self.nodes.keys().map(|id| id.0).max().unwrap_or(0);
    let mut next = BookmarkId(max.saturating_add(1));
    if next.0 == 0 {
      next = BookmarkId(1);
    }
    if self.next_id.0 < next.0 {
      self.next_id = next;
    }
  }

  fn validate(&self) -> Result<(), String> {
    if self.version != BOOKMARK_STORE_VERSION {
      return Err(format!(
        "unsupported bookmarks schema version {}; expected {}",
        self.version, BOOKMARK_STORE_VERSION
      ));
    }

    // Key/id consistency.
    for (key, node) in &self.nodes {
      if *key != node.id() {
        return Err(format!(
          "bookmark node key {key:?} does not match node id {:?}",
          node.id()
        ));
      }
    }

    // Roots must exist and have parent=None.
    let root_set: HashSet<BookmarkId> = self.roots.iter().copied().collect();
    if root_set.len() != self.roots.len() {
      return Err("duplicate bookmark ids in roots list".to_string());
    }
    for id in &self.roots {
      let node = self
        .nodes
        .get(id)
        .ok_or_else(|| format!("root id {id:?} missing from nodes map"))?;
      if node.parent().is_some() {
        return Err(format!(
          "root id {id:?} has non-root parent {:?}",
          node.parent()
        ));
      }
    }

    // Parent pointers must refer to folders and match children lists.
    for node in self.nodes.values() {
      if let Some(parent_id) = node.parent() {
        match self.nodes.get(&parent_id) {
          Some(BookmarkNode::Folder(parent)) => {
            if !parent.children.contains(&node.id()) {
              return Err(format!(
                "node {:?} parent {:?} does not list it as a child",
                node.id(),
                parent_id
              ));
            }
          }
          Some(_) => {
            return Err(format!(
              "node {:?} parent {:?} is not a folder",
              node.id(),
              parent_id
            ))
          }
          None => {
            return Err(format!(
              "node {:?} parent {:?} missing",
              node.id(),
              parent_id
            ))
          }
        }
      }
    }

    // Folder children must exist and have parent pointers.
    for node in self.nodes.values() {
      if let BookmarkNode::Folder(folder) = node {
        let child_set: HashSet<BookmarkId> = folder.children.iter().copied().collect();
        if child_set.len() != folder.children.len() {
          return Err(format!(
            "folder {:?} contains duplicate child ids",
            folder.id
          ));
        }
        for child_id in &folder.children {
          let child = self.nodes.get(child_id).ok_or_else(|| {
            format!(
              "folder {:?} references missing child {:?}",
              folder.id, child_id
            )
          })?;
          if child.parent() != Some(folder.id) {
            return Err(format!(
              "folder {:?} child {:?} has mismatched parent {:?}",
              folder.id,
              child_id,
              child.parent()
            ));
          }
        }
      }
    }

    // Every node must be reachable from roots (either directly or via folder children).
    let mut reachable = HashSet::new();
    let mut stack: Vec<BookmarkId> = self.roots.clone();
    while let Some(id) = stack.pop() {
      if !reachable.insert(id) {
        continue;
      }
      if let Some(BookmarkNode::Folder(folder)) = self.nodes.get(&id) {
        stack.extend(folder.children.iter().copied());
      }
    }
    let all: HashSet<BookmarkId> = self.nodes.keys().copied().collect();
    if reachable != all {
      let mut missing: Vec<_> = all.difference(&reachable).copied().collect();
      missing.sort();
      return Err(format!("unreachable bookmark nodes: {missing:?}"));
    }

    Ok(())
  }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
  value.and_then(|raw| {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      None
    } else {
      Some(trimmed.to_string())
    }
  })
}

fn now_unix_ms() -> u64 {
  use std::time::{SystemTime, UNIX_EPOCH};
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

fn remove_first(items: &mut Vec<BookmarkId>, needle: BookmarkId) -> bool {
  if let Some(idx) = items.iter().position(|id| *id == needle) {
    items.remove(idx);
    true
  } else {
    false
  }
}

// -----------------------------------------------------------------------------
// Bookmarks bar (browser UI)
// -----------------------------------------------------------------------------
//
// Keep the core `BookmarkStore` UI-framework agnostic. The bookmarks bar is only compiled for the
// windowed egui UI.
#[cfg(feature = "browser_ui")]
mod bookmarks_bar_ui {
  use super::{BookmarkId, BookmarkNode, BookmarkStore};
  use egui::{Color32, Rect, Stroke};

  #[derive(Debug, Default)]
  pub struct BookmarksBarOutput {
    pub navigate_to: Option<String>,
    /// If `true`, the navigation should open in a new tab (like middle-click / Ctrl/Cmd+Click).
    pub navigate_new_tab: bool,
    /// If set, reorders the root list (bookmarks bar) to exactly this order.
    pub reorder_roots: Option<Vec<BookmarkId>>,
  }

  #[derive(Debug, Default, Clone, Copy)]
  struct DragState {
    dragging: Option<BookmarkId>,
    drop_index: Option<usize>,
  }

  fn move_before_id(
    current: &[BookmarkId],
    id: BookmarkId,
    before_id: BookmarkId,
  ) -> Option<Vec<BookmarkId>> {
    if id == before_id {
      return None;
    }
    let old = current.iter().position(|x| *x == id)?;
    let mut out = current.to_vec();
    out.remove(old);
    let pos = out.iter().position(|x| *x == before_id)?;
    out.insert(pos, id);
    Some(out)
  }

  fn move_after_id(
    current: &[BookmarkId],
    id: BookmarkId,
    after_id: BookmarkId,
  ) -> Option<Vec<BookmarkId>> {
    if id == after_id {
      return None;
    }
    let old = current.iter().position(|x| *x == id)?;
    let mut out = current.to_vec();
    out.remove(old);
    let pos = out.iter().position(|x| *x == after_id)?;
    out.insert(pos + 1, id);
    Some(out)
  }

  fn move_within_visible(
    roots: &[BookmarkId],
    visible: &[BookmarkId],
    dragged: BookmarkId,
    drop_index: usize,
  ) -> Option<Vec<BookmarkId>> {
    let visible_without: Vec<BookmarkId> = visible
      .iter()
      .copied()
      .filter(|id| *id != dragged)
      .collect();
    if visible_without.is_empty() {
      return None;
    }
    if drop_index == 0 {
      move_before_id(roots, dragged, visible_without[0])
    } else if drop_index >= visible_without.len() {
      move_after_id(roots, dragged, visible_without[visible_without.len() - 1])
    } else {
      move_before_id(roots, dragged, visible_without[drop_index])
    }
  }

  pub fn bookmarks_bar_ui(
    ui: &mut egui::Ui,
    bookmarks: &BookmarkStore,
    max_items: usize,
  ) -> BookmarksBarOutput {
    let ctx = ui.ctx().clone();
    let bar_id = ui.make_persistent_id("bookmarks_bar");
    let drag_id = bar_id.with("drag_state");
    let mut drag: DragState = ctx.data_mut(|d| d.get_persisted(drag_id).unwrap_or_default());

    let mut out = BookmarksBarOutput::default();

    let mut item_rects: Vec<(BookmarkId, Rect)> = Vec::new();
    let mut drag_released: Option<BookmarkId> = None;

    let mut visible_ids: Vec<BookmarkId> = Vec::new();
    for &id in &bookmarks.roots {
      if max_items > 0 && visible_ids.len() >= max_items {
        break;
      }
      let Some(BookmarkNode::Bookmark(entry)) = bookmarks.nodes.get(&id) else {
        continue;
      };
      if entry.url.trim().is_empty() {
        continue;
      }
      visible_ids.push(id);
    }

    let bar = ui.allocate_ui_with_layout(
      egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
      egui::Layout::left_to_right(egui::Align::Center),
      |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.set_min_height(ui.spacing().interact_size.y);

        for &id in &visible_ids {
          let Some(BookmarkNode::Bookmark(entry)) = bookmarks.nodes.get(&id) else {
            continue;
          };

          let url = entry.url.trim();
          if url.is_empty() {
            continue;
          }

          let title = entry
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
          let label = title
            .map(str::to_string)
            .unwrap_or_else(|| crate::ui::url_display::truncate_url_middle(url, 36));

          let button = egui::Button::new(label)
            .small()
            .sense(egui::Sense::click_and_drag());
          let response = ui.add(button).on_hover_text(url);
          item_rects.push((id, response.rect));

          let open_new_tab =
            response.middle_clicked() || (response.clicked() && ui.input(|i| i.modifiers.command));
          if response.clicked() || response.middle_clicked() {
            out.navigate_to = Some(url.to_string());
            out.navigate_new_tab = open_new_tab;
          }
          if response.drag_started() {
            drag.dragging = Some(id);
            drag.drop_index = None;
          }
          if response.drag_released() {
            drag_released = Some(id);
          }

          // Keyboard-accessible reorder.
          response.context_menu(|ui| {
            ui.set_min_width(140.0);
            if let Some(idx) = visible_ids.iter().position(|x| *x == id) {
              ui.add_enabled_ui(idx > 0, |ui| {
                if ui.button("Move left").clicked() {
                  if let Some(new_order) =
                    move_before_id(&bookmarks.roots, id, visible_ids[idx - 1])
                  {
                    out.reorder_roots = Some(new_order);
                  }
                  ui.close_menu();
                }
              });
              ui.add_enabled_ui(idx + 1 < visible_ids.len(), |ui| {
                if ui.button("Move right").clicked() {
                  if let Some(new_order) =
                    move_after_id(&bookmarks.roots, id, visible_ids[idx + 1])
                  {
                    out.reorder_roots = Some(new_order);
                  }
                  ui.close_menu();
                }
              });
            }
          });
        }
      },
    );

    let bar_rect = bar.response.rect;

    let pointer_x = ctx.input(|i| i.pointer.hover_pos().map(|p| p.x));
    if let (Some(dragging_id), Some(pointer_x)) = (drag.dragging, pointer_x) {
      ctx.request_repaint();

      let others: Vec<Rect> = item_rects
        .iter()
        .filter_map(|(id, rect)| (*id != dragging_id).then_some(*rect))
        .collect();

      let drop_index = others
        .iter()
        .filter(|rect| pointer_x > rect.center().x)
        .count();
      drag.drop_index = Some(drop_index);

      // Draw drop indicator.
      let indicator_x = if others.is_empty() {
        bar_rect.left() + 8.0
      } else if drop_index == 0 {
        others[0].left()
      } else if drop_index >= others.len() {
        others[others.len() - 1].right()
      } else {
        (others[drop_index - 1].right() + others[drop_index].left()) * 0.5
      };

      let y0 = bar_rect.top() + 4.0;
      let y1 = bar_rect.bottom() - 4.0;
      ui.painter().line_segment(
        [egui::pos2(indicator_x, y0), egui::pos2(indicator_x, y1)],
        Stroke::new(2.0, ui.visuals().selection.stroke.color),
      );

      // Highlight the dragged item (if visible).
      if let Some((_, rect)) = item_rects.iter().find(|(id, _)| *id == dragging_id) {
        ui.painter().rect_stroke(
          rect.expand(1.0),
          egui::Rounding::same(6.0),
          Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 120)),
        );
      }
    }

    if let Some(released_id) = drag_released {
      if let Some(drop_index) = drag.drop_index {
        if let Some(new_order) =
          move_within_visible(&bookmarks.roots, &visible_ids, released_id, drop_index)
        {
          if new_order != bookmarks.roots {
            out.reorder_roots = Some(new_order);
          }
        }
      }
      drag.dragging = None;
      drag.drop_index = None;
    }

    ctx.data_mut(|d| {
      d.insert_persisted(drag_id, drag);
    });

    out
  }
}

#[cfg(feature = "browser_ui")]
pub use bookmarks_bar_ui::{bookmarks_bar_ui, BookmarksBarOutput};

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn toggle_adds_and_removes() {
    let mut store = BookmarkStore::default();
    assert!(!store.contains_url("https://example.com/"));
    assert_eq!(store.toggle("https://example.com/", Some("Example")), true);
    assert!(store.contains_url("https://example.com/"));
    assert_eq!(store.toggle("https://example.com/", Some("Ignored")), false);
    assert!(!store.contains_url("https://example.com/"));
  }

  #[test]
  fn add_preserves_root_order_and_reorder_changes_it() {
    let mut store = BookmarkStore::default();
    let a = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
      )
      .unwrap();
    let b = store
      .add(
        "https://b.example/".to_string(),
        Some("B".to_string()),
        None,
      )
      .unwrap();
    assert_eq!(store.roots, vec![a, b]);
    store.reorder_root(&[b, a]).unwrap();
    assert_eq!(store.roots, vec![b, a]);
  }

  #[test]
  fn remove_by_url_removes_all_duplicates() {
    let mut store = BookmarkStore::default();
    let a1 = store
      .add(
        "https://a.example/".to_string(),
        Some("A1".to_string()),
        None,
      )
      .unwrap();
    let a2 = store
      .add(
        "https://a.example/".to_string(),
        Some("A2".to_string()),
        None,
      )
      .unwrap();
    assert_ne!(a1, a2);
    assert!(store.contains_url("https://a.example/"));
    assert_eq!(store.remove_by_url("https://a.example/"), 2);
    assert!(!store.contains_url("https://a.example/"));
    assert!(store.nodes.is_empty());
    assert!(store.roots.is_empty());
  }

  #[test]
  fn move_into_folder_updates_parent_and_children() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let a = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
      )
      .unwrap();
    store.move_node(a, Some(folder)).unwrap();

    assert_eq!(
      store.nodes.get(&a).and_then(BookmarkNode::parent),
      Some(folder)
    );
    let BookmarkNode::Folder(folder_node) = store.nodes.get(&folder).unwrap() else {
      panic!("expected folder node");
    };
    assert_eq!(folder_node.children, vec![a]);
    assert!(!store.roots.contains(&a));
  }

  #[test]
  fn migrate_from_legacy_urls_json() {
    let legacy = r#"{"urls":["https://a.example/","https://b.example/"]}"#;
    let (store, migration) = BookmarkStore::from_json_str_migrating(legacy).unwrap();
    assert_eq!(migration, BookmarkStoreMigration::FromLegacyUrls);
    assert_eq!(store.roots.len(), 2);
    let a_id = store.roots[0];
    let b_id = store.roots[1];
    let BookmarkNode::Bookmark(a) = store.nodes.get(&a_id).unwrap() else {
      panic!("expected bookmark");
    };
    assert_eq!(a.url, "https://a.example/");
    assert_eq!(a.title.as_deref(), Some("https://a.example/"));
    assert_eq!(a.added_at_ms, 0);
    let BookmarkNode::Bookmark(b) = store.nodes.get(&b_id).unwrap() else {
      panic!("expected bookmark");
    };
    assert_eq!(b.url, "https://b.example/");
    assert_eq!(b.title.as_deref(), Some("https://b.example/"));
    assert_eq!(b.added_at_ms, 0);
  }

  #[test]
  fn serde_roundtrip() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let a = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        Some(folder),
      )
      .unwrap();
    let b = store
      .add("https://b.example/".to_string(), None, None)
      .unwrap();
    store.reorder_root(&[folder, b]).unwrap();
    // Ensure the folder still contains the child.
    let BookmarkNode::Folder(folder_node) = store.nodes.get(&folder).unwrap() else {
      panic!("expected folder");
    };
    assert_eq!(folder_node.children, vec![a]);

    let json = serde_json::to_string(&store).unwrap();
    let decoded: BookmarkStore = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, store);
  }

  #[test]
  fn add_rejects_invalid_url_scheme() {
    let mut store = BookmarkStore::default();
    assert!(store.add("javascript:alert(1)".to_string(), None, None).is_err());
    assert!(!store.contains_url("javascript:alert(1)"));
  }
}
