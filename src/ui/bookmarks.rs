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
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::ui::url::validate_user_navigation_url_scheme;
use super::string_match::contains_ascii_case_insensitive;

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

#[derive(Debug, Clone, Serialize)]
pub struct BookmarkStore {
  pub version: u32,
  #[serde(default = "default_next_id")]
  pub next_id: BookmarkId,
  /// Monotonically increasing revision counter for UI caching.
  ///
  /// This is *not* part of the persisted bookmark schema (it exists so immediate-mode UIs can
  /// cheaply detect when a store mutation has occurred).
  #[serde(skip)]
  revision: u64,
  /// Monotonically increasing revision counter for bookmark tree structure changes.
  ///
  /// This only changes when the *shape* or ordering of the bookmark tree changes (add/remove/move
  /// nodes, reorder roots/siblings, replace the whole store). It does **not** change for bookmark
  /// content-only updates (e.g. editing a bookmark title/URL without moving it).
  ///
  /// This is *not* part of the persisted bookmark schema.
  #[serde(skip)]
  structure_revision: u64,
  /// Monotonically increasing revision counter for folder structure changes.
  ///
  /// Used for caching folder dropdown options: many bookmark mutations (editing a bookmark title,
  /// toggling a URL, etc) do not change the set of folders or their display paths, so we avoid
  /// invalidating folder caches on every `revision` bump.
  ///
  /// This is *not* part of the persisted bookmark schema.
  #[serde(skip)]
  folder_revision: u64,
  /// URL membership index used by UI surfaces (bookmark star, omnibox suggestions, etc).
  ///
  /// The store intentionally allows duplicate URLs; the map value is the number of bookmarks
  /// currently present for that URL.
  ///
  /// This field is derived from `nodes` and thus not serialized.
  #[serde(skip)]
  url_index: FxHashMap<String, usize>,
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
      revision: 0,
      structure_revision: 0,
      folder_revision: 0,
      url_index: FxHashMap::default(),
      roots: Vec::new(),
      nodes: BTreeMap::new(),
    }
  }
}

impl PartialEq for BookmarkStore {
  fn eq(&self, other: &Self) -> bool {
    // `revision`/`structure_revision`/`folder_revision`/`url_index` are intentionally excluded: they
    // are derived in-memory metadata, not persisted data.
    self.version == other.version
      && self.next_id == other.next_id
      && self.roots == other.roots
      && self.nodes == other.nodes
  }
}

impl Eq for BookmarkStore {}

impl<'de> Deserialize<'de> for BookmarkStore {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    #[derive(Deserialize)]
    struct BookmarkStorePersisted {
      version: u32,
      #[serde(default = "default_next_id")]
      next_id: BookmarkId,
      #[serde(default)]
      roots: Vec<BookmarkId>,
      #[serde(default)]
      nodes: BTreeMap<BookmarkId, BookmarkNode>,
    }

    let persisted = BookmarkStorePersisted::deserialize(deserializer)?;
    let mut store = Self {
      version: persisted.version,
      next_id: persisted.next_id,
      roots: persisted.roots,
      nodes: persisted.nodes,
      revision: 0,
      structure_revision: 0,
      folder_revision: 0,
      url_index: FxHashMap::default(),
    };
    store.rebuild_url_index();
    Ok(store)
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

/// Incremental bookmark change operations for synchronizing stores across windows.
///
/// These deltas are intended to be:
/// - **O(delta)** to apply (no full-store cloning required).
/// - **Deterministic**: they include generated ids and timestamps so applying them later yields the
///   exact same persisted store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkDelta {
  /// Insert a new node (bookmark or folder).
  ///
  /// The node's `parent` determines which ordering list (`roots` vs folder `children`) it is
  /// appended to.
  AddNode(BookmarkNode),
  /// Remove a node and its full subtree.
  RemoveSubtree(BookmarkId),
  /// Update bookmark fields (and optionally move it to a new parent).
  UpdateBookmark {
    id: BookmarkId,
    title: Option<String>,
    url: String,
    parent: Option<BookmarkId>,
  },
  /// Move an existing node to a new parent (appended to the destination ordering list).
  MoveNode {
    id: BookmarkId,
    parent: Option<BookmarkId>,
  },
  /// Reorder a node within its current parent list by moving it before another sibling.
  ReorderBefore {
    id: BookmarkId,
    parent: Option<BookmarkId>,
    before_id: BookmarkId,
  },
  /// Reorder a node within its current parent list by moving it after another sibling.
  ReorderAfter {
    id: BookmarkId,
    parent: Option<BookmarkId>,
    after_id: BookmarkId,
  },
  /// Replace the root ordering list (bookmarks bar ordering).
  ReorderRoot(Vec<BookmarkId>),
  /// Replace the entire store (used for imports).
  ReplaceAll(BookmarkStore),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkStoreMigration {
  None,
  FromLegacyUrls,
  FromLegacyHeadlessArray,
}

impl BookmarkStore {
  /// Returns the current store revision (incremented on successful mutation).
  pub fn revision(&self) -> u64 {
    self.revision
  }

  /// Returns the current structure revision (incremented when the bookmark tree shape/order
  /// changes).
  pub fn structure_revision(&self) -> u64 {
    self.structure_revision
  }

  /// Returns the current folder revision (incremented when folder structure changes).
  pub fn folder_revision(&self) -> u64 {
    self.folder_revision
  }

  /// Manually bump the store revision.
  ///
  /// Most callers should not need this: all mutation APIs (`add`, `remove_by_id`, `update`, etc)
  /// update the revision automatically. This exists for situations where the store is replaced via
  /// assignment and the caller wants to invalidate UI caches that key on [`Self::revision`]. If
  /// folders may have changed, prefer [`Self::touch_folders`].
  pub fn touch(&mut self) {
    // Saturating keeps the revision monotonic even under extreme mutation counts.
    self.revision = self.revision.saturating_add(1);
  }

  /// Manually bump the structure revision *and* the global revision.
  ///
  /// This should be used when nodes are added/removed/moved/reordered.
  pub fn touch_structure(&mut self) {
    self.revision = self.revision.saturating_add(1);
    self.structure_revision = self.structure_revision.saturating_add(1);
  }

  /// Manually bump the folder revision (and also structure/global revisions).
  ///
  /// This should be used when folder paths/orderings may have changed (creating/removing/moving a
  /// folder, or replacing the entire store).
  pub fn touch_folders(&mut self) {
    self.revision = self.revision.saturating_add(1);
    self.structure_revision = self.structure_revision.saturating_add(1);
    self.folder_revision = self.folder_revision.saturating_add(1);
  }

  /// Apply an incremental update to the store.
  ///
  /// This is used by the windowed browser to synchronize bookmark updates across windows without
  /// cloning the entire [`BookmarkStore`].
  pub fn apply_delta(&mut self, delta: &BookmarkDelta) -> Result<(), BookmarkError> {
    match delta {
      BookmarkDelta::AddNode(node) => {
        let id = node.id();
        if let Some(existing) = self.nodes.get(&id) {
          if existing == node {
            return Ok(());
          }
          return Err(BookmarkError::InvalidStore(format!(
            "apply delta: duplicate bookmark id {id:?}"
          )));
        }

        self.insert_new_node(node.clone())?;

        // Keep `next_id` monotonic without scanning the whole store.
        let next = id.checked_next().ok_or(BookmarkError::IdExhausted)?;
        if self.next_id.0 < next.0 {
          self.next_id = next;
        }

        Ok(())
      }
      BookmarkDelta::RemoveSubtree(id) => {
        if self.remove_by_id(*id) {
          Ok(())
        } else {
          Err(BookmarkError::NotFound(*id))
        }
      }
      BookmarkDelta::UpdateBookmark {
        id,
        title,
        url,
        parent,
      } => self.update(*id, title.clone(), url.clone(), *parent),
      BookmarkDelta::MoveNode { id, parent } => self.move_node(*id, *parent),
      BookmarkDelta::ReorderBefore {
        id,
        parent,
        before_id,
      } => self.reorder_before_in_parent(*id, *parent, *before_id),
      BookmarkDelta::ReorderAfter {
        id,
        parent,
        after_id,
      } => self.reorder_after_in_parent(*id, *parent, *after_id),
      BookmarkDelta::ReorderRoot(order) => {
        if self.roots == order.as_slice() {
          Ok(())
        } else {
          self.reorder_root(order)
        }
      }
      BookmarkDelta::ReplaceAll(store) => {
        let next_revision = self.revision.saturating_add(1);
        let next_structure_revision = self.structure_revision.saturating_add(1);
        let next_folder_revision = self.folder_revision.saturating_add(1);
        *self = store.clone();
        self.revision = next_revision;
        self.structure_revision = next_structure_revision;
        self.folder_revision = next_folder_revision;
        Ok(())
      }
    }
  }

  pub fn apply_deltas(&mut self, deltas: &[BookmarkDelta]) -> Result<(), BookmarkError> {
    for delta in deltas {
      self.apply_delta(delta)?;
    }
    Ok(())
  }

  /// [`Self::toggle`] but also records the mutation as deltas.
  pub fn toggle_with_deltas(
    &mut self,
    url: &str,
    title: Option<&str>,
    deltas: &mut Vec<BookmarkDelta>,
  ) -> bool {
    let url = url.trim();
    if url.is_empty() {
      return false;
    }

    if self.contains_url(url) {
      let _removed = self.remove_by_url_with_deltas(url, deltas);
      false
    } else {
      // Root-level bookmark by default.
      self
        .add_with_deltas(
          url.to_string(),
          title.map(|s| s.to_string()),
          None,
          deltas,
        )
        .is_ok()
    }
  }

  /// [`Self::add`] but also records the mutation as deltas.
  pub fn add_with_deltas(
    &mut self,
    url: String,
    title: Option<String>,
    parent: Option<BookmarkId>,
    deltas: &mut Vec<BookmarkDelta>,
  ) -> Result<BookmarkId, BookmarkError> {
    let url = url.trim().to_string();
    if url.is_empty() {
      return Err(BookmarkError::InvalidStore("bookmark URL is empty".to_string()));
    }
    validate_user_navigation_url_scheme(&url).map_err(BookmarkError::InvalidStore)?;
    let title = normalize_optional_string(title);
    let added_at_ms = now_unix_ms();
    self.add_with_timestamp_and_deltas(url, title, parent, added_at_ms, deltas)
  }

  /// [`Self::create_folder`] but also records the mutation as deltas.
  pub fn create_folder_with_deltas(
    &mut self,
    title: String,
    parent: Option<BookmarkId>,
    deltas: &mut Vec<BookmarkDelta>,
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
    self.insert_new_node(node.clone())?;
    deltas.push(BookmarkDelta::AddNode(node));
    Ok(id)
  }

  /// [`Self::remove_by_id`] but also records the mutation as deltas.
  pub fn remove_by_id_with_deltas(
    &mut self,
    id: BookmarkId,
    deltas: &mut Vec<BookmarkDelta>,
  ) -> bool {
    if self.remove_by_id(id) {
      deltas.push(BookmarkDelta::RemoveSubtree(id));
      true
    } else {
      false
    }
  }

  /// [`Self::remove_by_url`] but also records the mutation as deltas.
  pub fn remove_by_url_with_deltas(&mut self, url: &str, deltas: &mut Vec<BookmarkDelta>) -> usize {
    let expected = self.url_index.get(url).copied().unwrap_or(0);
    if expected == 0 {
      return 0;
    }

    // The store maintains an O(1) URL membership index (`url_index`). Use it as an upper bound so we
    // can stop scanning once we've found all matching IDs, avoiding an O(n) traversal in the common
    // case where the matching bookmarks are clustered early in the tree.
    let mut ids: Vec<BookmarkId> = Vec::with_capacity(expected);
    for (&id, node) in self.nodes.iter() {
      if let BookmarkNode::Bookmark(bookmark) = node {
        if bookmark.url == url {
          ids.push(id);
          if ids.len() == expected {
            break;
          }
        }
      }
    }
    let mut removed = 0;
    for id in ids {
      if self.remove_by_id(id) {
        removed += 1;
        deltas.push(BookmarkDelta::RemoveSubtree(id));
      }
    }
    removed
  }

  /// [`Self::update`] but also records the mutation as deltas.
  pub fn update_with_deltas(
    &mut self,
    id: BookmarkId,
    new_title: Option<String>,
    new_url: String,
    new_parent: Option<BookmarkId>,
    deltas: &mut Vec<BookmarkDelta>,
  ) -> Result<(), BookmarkError> {
    // Mirror `update`'s normalization so applying deltas is deterministic.
    let new_url = new_url.trim().to_string();
    let new_title = normalize_optional_string(new_title);

    self.update(id, new_title.clone(), new_url.clone(), new_parent)?;
    deltas.push(BookmarkDelta::UpdateBookmark {
      id,
      title: new_title,
      url: new_url,
      parent: new_parent,
    });
    Ok(())
  }

  /// [`Self::move_node`] but also records the mutation as deltas.
  pub fn move_node_with_deltas(
    &mut self,
    id: BookmarkId,
    new_parent: Option<BookmarkId>,
    deltas: &mut Vec<BookmarkDelta>,
  ) -> Result<(), BookmarkError> {
    let old_parent = self
      .nodes
      .get(&id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(id))?;
    self.move_node(id, new_parent)?;
    if old_parent != new_parent {
      deltas.push(BookmarkDelta::MoveNode {
        id,
        parent: new_parent,
      });
    }
    Ok(())
  }

  /// [`Self::reorder_root`] but also records the mutation as deltas.
  pub fn reorder_root_with_deltas(
    &mut self,
    ids_in_new_order: &[BookmarkId],
    deltas: &mut Vec<BookmarkDelta>,
  ) -> Result<(), BookmarkError> {
    if ids_in_new_order.len() != self.roots.len() {
      return Err(BookmarkError::InvalidReorder);
    }

    let expected: FxHashSet<BookmarkId> = self.roots.iter().copied().collect();
    let got: FxHashSet<BookmarkId> = ids_in_new_order.iter().copied().collect();
    if expected != got || got.len() != ids_in_new_order.len() {
      return Err(BookmarkError::InvalidReorder);
    }

    if self.roots == ids_in_new_order {
      return Ok(());
    }

    // Common case: the UI performs a single-item move (drag reorder). Detect that and record a
    // compact delta instead of copying the full roots vector into the delta payload.
    let len = self.roots.len();
    if len >= 2 {
      let mut start = 0usize;
      while start < len && self.roots[start] == ids_in_new_order[start] {
        start += 1;
      }

      let mut end = len;
      while end > start && self.roots[end - 1] == ids_in_new_order[end - 1] {
        end -= 1;
      }

      if start < end {
        let old_start = self.roots[start];
        let old_end = self.roots[end - 1];
        let new_start = ids_in_new_order[start];
        let new_end = ids_in_new_order[end - 1];

        // Move a trailing element earlier: `[... a b c] -> [... c a b]`
        if new_start == old_end {
          let mut ok = true;
          for idx in start..(end - 1) {
            if ids_in_new_order[idx + 1] != self.roots[idx] {
              ok = false;
              break;
            }
          }

          if ok {
            self.reorder_before_in_parent(old_end, None, old_start)?;
            deltas.push(BookmarkDelta::ReorderBefore {
              id: old_end,
              parent: None,
              before_id: old_start,
            });
            return Ok(());
          }
        }

        // Move a leading element later: `[... a b c] -> [... b c a]`
        if new_end == old_start {
          let mut ok = true;
          for idx in (start + 1)..end {
            if ids_in_new_order[idx - 1] != self.roots[idx] {
              ok = false;
              break;
            }
          }

          if ok {
            self.reorder_after_in_parent(old_start, None, old_end)?;
            deltas.push(BookmarkDelta::ReorderAfter {
              id: old_start,
              parent: None,
              after_id: old_end,
            });
            return Ok(());
          }
        }
      }
    }

    // Fallback: record a full root ordering vector.
    let folder_order_changed = !self.folder_subsequence_equal(&self.roots, ids_in_new_order);

    let new_vec = ids_in_new_order.to_vec();
    self.roots = new_vec.clone();
    if folder_order_changed {
      self.touch_folders();
    } else {
      self.touch_structure();
    }
    deltas.push(BookmarkDelta::ReorderRoot(new_vec));
    Ok(())
  }

  pub fn contains_url(&self, url: &str) -> bool {
    self.url_index.get(url).is_some_and(|count| *count > 0)
  }

  /// Search bookmarks by title and URL (tokenized, case-insensitive).
  ///
  /// - `query` is split by whitespace into tokens; every token must match either the bookmark title
  ///   or URL.
  /// - Matching is ASCII case-insensitive (non-ASCII bytes must match exactly).
  /// - Results are returned in the user-defined ordering (roots + folder children).
  /// - `scan_limit` caps the number of bookmark entries examined (folders do not count toward this
  ///   limit). This is useful for UI surfaces that need to remain cheap (e.g. the omnibox).
  pub fn search(&self, query: &str, scan_limit: usize) -> Vec<BookmarkId> {
    if scan_limit == 0 {
      return Vec::new();
    }

    // The shared ASCII-only matcher expects the needle to already be ASCII-lowercased so it can be
    // reused across repeated `(haystack, needle)` comparisons. Avoid allocating unless the user
    // actually typed uppercase ASCII.
    let query_lower: Cow<'_, str> = if query.as_bytes().iter().any(|b| b.is_ascii_uppercase()) {
      Cow::Owned(query.to_ascii_lowercase())
    } else {
      Cow::Borrowed(query)
    };
    let mut tokens_iter = query_lower.split_whitespace().filter(|t| !t.is_empty());
    let Some(first_token) = tokens_iter.next() else {
      return Vec::new();
    };
    let tokens: Option<SmallVec<[&str; 4]>> = tokens_iter.next().map(|second_token| {
      let mut tokens: SmallVec<[&str; 4]> = SmallVec::new();
      tokens.push(first_token);
      tokens.push(second_token);
      tokens.extend(tokens_iter);
      tokens
    });

    let mut out = Vec::new();
    let mut scanned = 0usize;

    // Traverse nodes in the user-defined store ordering (roots + folder children). This keeps
    // results deterministic even when we early-exit at `scan_limit`.
    let mut stack: Vec<BookmarkId> = self.roots.iter().rev().copied().collect();
    'nodes: while let Some(id) = stack.pop() {
      let Some(node) = self.nodes.get(&id) else {
        // Shouldn't happen in a validated store, but skip gracefully.
        continue;
      };

      match node {
        BookmarkNode::Bookmark(entry) => {
          if scanned >= scan_limit {
            break 'nodes;
          }
          scanned += 1;

          let url = entry.url.trim();
          if url.is_empty() {
            continue 'nodes;
          }

          let title = entry
            .title
            .as_deref()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty());

          if let Some(tokens) = &tokens {
            // Multi-token query: every token must match either title or URL.
            for token_lower in tokens {
              if !contains_ascii_case_insensitive(url, token_lower)
                && !title.is_some_and(|t| contains_ascii_case_insensitive(t, token_lower))
              {
                continue 'nodes;
              }
            }
          } else if !contains_ascii_case_insensitive(url, first_token)
            && !title.is_some_and(|t| contains_ascii_case_insensitive(t, first_token))
          {
            // Single-token query fast path: avoid allocating a token vector for the common case.
            continue 'nodes;
          }

          out.push(id);
        }
        BookmarkNode::Folder(folder) => {
          // Depth-first traversal: push children in reverse so pop() visits them in order.
          stack.extend(folder.children.iter().rev().copied());
        }
      }
    }

    out
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
      return Err(BookmarkError::InvalidStore(
        "bookmark URL is empty".to_string(),
      ));
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

    let removed_is_folder = matches!(self.nodes.get(&id), Some(BookmarkNode::Folder(_)));

    // Detach the root entry from ordering lists first.
    let parent = self.nodes.get(&id).and_then(BookmarkNode::parent);
    self.detach_from_parent_list(id, parent);

    // Remove the full subtree without allocating an intermediate `Vec` of IDs (which can be large
    // when deleting folders).
    let mut stack: Vec<BookmarkId> = vec![id];
    while let Some(node_id) = stack.pop() {
      let Some(node) = self.nodes.remove(&node_id) else {
        continue;
      };
      match node {
        BookmarkNode::Bookmark(entry) => {
          self.url_index_dec(&entry.url);
        }
        BookmarkNode::Folder(folder) => {
          // Maintain the same traversal order as the old recursive implementation by pushing
          // children in reverse onto the LIFO stack.
          for child in folder.children.into_iter().rev() {
            stack.push(child);
          }
        }
      }
    }
    if removed_is_folder {
      self.touch_folders();
    } else {
      self.touch_structure();
    }
    true
  }

  /// Remove all bookmarks whose URL matches `url`.
  ///
  /// Returns the number of removed bookmarks.
  pub fn remove_by_url(&mut self, url: &str) -> usize {
    let expected = self.url_index.get(url).copied().unwrap_or(0);
    if expected == 0 {
      return 0;
    }

    let mut ids: Vec<BookmarkId> = Vec::with_capacity(expected);
    for (&id, node) in self.nodes.iter() {
      if let BookmarkNode::Bookmark(bookmark) = node {
        if bookmark.url == url {
          ids.push(id);
          if ids.len() == expected {
            break;
          }
        }
      }
    }
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
    let old_parent = match self.nodes.get(&id) {
      Some(BookmarkNode::Bookmark(entry)) => entry.parent,
      Some(BookmarkNode::Folder(_)) => {
        return Err(BookmarkError::InvalidStore(
          "update: id is a folder".to_string(),
        ))
      }
      None => return Err(BookmarkError::NotFound(id)),
    };

    let new_url = new_url.trim().to_string();
    if new_url.is_empty() {
      return Err(BookmarkError::InvalidStore(
        "bookmark URL is empty".to_string(),
      ));
    }
    validate_user_navigation_url_scheme(&new_url).map_err(BookmarkError::InvalidStore)?;

    let new_title = normalize_optional_string(new_title);

    // Perform the parent move after URL validation so `update` is "all or nothing" (we don't want a
    // move to succeed and then URL validation to fail).
    if old_parent != new_parent {
      self.move_node(id, new_parent)?;
    }

    // Keep the URL index in sync with the new URL (including when multiple bookmarks share the
    // same URL).
    let node = self.nodes.get_mut(&id).ok_or(BookmarkError::NotFound(id))?;
    let old_url = match node {
      BookmarkNode::Bookmark(entry) => {
        let old_url = std::mem::replace(&mut entry.url, new_url);
        entry.title = new_title;
        old_url
      }
      BookmarkNode::Folder(_) => unreachable!("validated above"),
    };
    let new_url_for_index = self
      .nodes
      .get(&id)
      .and_then(|node| match node {
        BookmarkNode::Bookmark(entry) => Some(entry.url.as_str()),
        BookmarkNode::Folder(_) => None,
      })
      .ok_or_else(|| {
        BookmarkError::InvalidStore("update: bookmark disappeared after mutation".to_string())
      })?;

    if old_url != new_url_for_index {
      self.url_index_dec(&old_url);
      self.url_index_inc(new_url_for_index);
    }
    self.touch();
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
    let moving_folder = matches!(self.nodes.get(&id), Some(BookmarkNode::Folder(_)));
    if moving_folder {
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

    if moving_folder {
      self.touch_folders();
    } else {
      self.touch_structure();
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

  fn folder_subsequence_equal(&self, a: &[BookmarkId], b: &[BookmarkId]) -> bool {
    let mut iter_a = a
      .iter()
      .copied()
      .filter(|id| matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))));
    let mut iter_b = b
      .iter()
      .copied()
      .filter(|id| matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))));

    loop {
      match (iter_a.next(), iter_b.next()) {
        (None, None) => return true,
        (Some(a_id), Some(b_id)) if a_id == b_id => {}
        _ => return false,
      }
    }
  }

  pub fn reorder_root(&mut self, ids_in_new_order: &[BookmarkId]) -> Result<(), BookmarkError> {
    if ids_in_new_order.len() != self.roots.len() {
      return Err(BookmarkError::InvalidReorder);
    }
    let expected: FxHashSet<BookmarkId> = self.roots.iter().copied().collect();
    let got: FxHashSet<BookmarkId> = ids_in_new_order.iter().copied().collect();
    if expected != got || got.len() != ids_in_new_order.len() {
      return Err(BookmarkError::InvalidReorder);
    }
    if self.roots.as_slice() == ids_in_new_order {
      return Ok(());
    }

    let folder_order_changed = !self.folder_subsequence_equal(&self.roots, ids_in_new_order);

    self.roots = ids_in_new_order.to_vec();
    if folder_order_changed {
      self.touch_folders();
    } else {
      self.touch_structure();
    }
    Ok(())
  }

  /// Reorder the root list (bookmarks bar).
  pub fn reorder(&mut self, ids_in_new_order: &[BookmarkId]) -> Result<(), BookmarkError> {
    self.reorder_root(ids_in_new_order)
  }

  /// Return the human-facing folder path (a list of folder titles) for `folder_id`.
  ///
  /// The returned vector is ordered from root → leaf, and includes the folder itself.
  pub fn folder_path_titles(&self, folder_id: BookmarkId) -> Result<Vec<String>, BookmarkError> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = Some(folder_id);
    for _ in 0..=self.nodes.len() {
      let Some(id) = cur else {
        break;
      };

      let Some(node) = self.nodes.get(&id) else {
        return Err(BookmarkError::NotFound(id));
      };
      let BookmarkNode::Folder(folder) = node else {
        return Err(BookmarkError::InvalidStore(format!(
          "expected folder id {id:?}, got bookmark node"
        )));
      };
      out.push(folder.title.clone());
      cur = folder.parent;
    }

    if cur.is_some() {
      return Err(BookmarkError::InvalidStore(
        "cycle detected in bookmark folder ancestry".to_string(),
      ));
    }

    out.reverse();
    Ok(out)
  }

  /// Compute the folder path for a node's parent folder (`[]` for the root).
  pub fn folder_path_titles_for_parent(
    &self,
    parent: Option<BookmarkId>,
  ) -> Result<Vec<String>, BookmarkError> {
    match parent {
      Some(id) => self.folder_path_titles(id),
      None => Ok(Vec::new()),
    }
  }

  /// Enumerate all folders in a deterministic depth-first traversal order.
  ///
  /// The resulting list is suitable for UI dropdowns:
  /// - ordering follows `roots` / `children` ordering
  /// - each entry includes its full display path (root → leaf)
  pub fn folders_in_display_order(&self) -> Vec<(BookmarkId, Vec<String>)> {
    fn walk(
      store: &BookmarkStore,
      folder_id: BookmarkId,
      path: &mut Vec<String>,
      out: &mut Vec<(BookmarkId, Vec<String>)>,
    ) {
      let Some(node) = store.nodes.get(&folder_id) else {
        return;
      };
      let BookmarkNode::Folder(folder) = node else {
        return;
      };

      path.push(folder.title.clone());
      out.push((folder_id, path.clone()));

      for child in &folder.children {
        if matches!(store.nodes.get(child), Some(BookmarkNode::Folder(_))) {
          walk(store, *child, path, out);
        }
      }

      path.pop();
    }

    let mut out = Vec::new();
    for id in &self.roots {
      if matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))) {
        let mut path = Vec::new();
        walk(self, *id, &mut path, &mut out);
      }
    }
    out
  }

  /// Enumerate all folders in a deterministic depth-first traversal order, but return the display
  /// path as a single `/`-joined string.
  ///
  /// This is optimized for UI dropdowns: it avoids allocating a full `Vec<String>` per folder (as
  /// [`Self::folders_in_display_order`] does).
  pub fn folders_in_display_order_joined(&self) -> Vec<(BookmarkId, String)> {
    fn walk(
      store: &BookmarkStore,
      folder_id: BookmarkId,
      path: &mut String,
      out: &mut Vec<(BookmarkId, String)>,
    ) {
      let Some(node) = store.nodes.get(&folder_id) else {
        return;
      };
      let BookmarkNode::Folder(folder) = node else {
        return;
      };

      // Maintain an in-place `root/…/leaf` path buffer to avoid rebuilding strings for every folder.
      let prev_len = path.len();
      if !path.is_empty() {
        path.push('/');
      }
      path.push_str(folder.title.as_str());
      out.push((folder_id, path.clone()));

      for child in &folder.children {
        if matches!(store.nodes.get(child), Some(BookmarkNode::Folder(_))) {
          walk(store, *child, path, out);
        }
      }

      path.truncate(prev_len);
    }

    let mut out = Vec::new();
    let mut path = String::new();
    for id in &self.roots {
      if matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))) {
        walk(self, *id, &mut path, &mut out);
      }
    }
    out
  }

  /// Alias for [`Self::folders_in_display_order`].
  pub fn folders(&self) -> Vec<(BookmarkId, Vec<String>)> {
    self.folders_in_display_order()
  }

  /// Resolve a folder by its display path (root → leaf folder titles).
  ///
  /// If multiple folders share the same path (possible in a corrupted store), the first match in
  /// display order is returned.
  pub fn folder_id_by_path_titles(&self, folder_path_titles: &[String]) -> Option<BookmarkId> {
    if folder_path_titles.is_empty() {
      return None;
    }
    for (id, path) in self.folders_in_display_order() {
      if path == folder_path_titles {
        return Some(id);
      }
    }
    None
  }

  /// Move all bookmarks matching `url` into the folder at `folder_path_titles`.
  ///
  /// This is primarily useful for importers that describe folder targets by name/path rather than
  /// stable IDs.
  ///
  /// Returns the number of bookmarks moved.
  pub fn move_to_folder(
    &mut self,
    url: &str,
    folder_path_titles: &[String],
  ) -> Result<usize, BookmarkError> {
    let url = url.trim();
    if url.is_empty() {
      return Ok(0);
    }

    let new_parent = if folder_path_titles.is_empty() {
      None
    } else {
      Some(
        self
          .folder_id_by_path_titles(folder_path_titles)
          .ok_or_else(|| {
            BookmarkError::InvalidStore(format!(
              "folder path not found: {}",
              folder_path_titles.join("/")
            ))
          })?,
      )
    };

    let ids: Vec<BookmarkId> = self
      .nodes
      .iter()
      .filter_map(|(&id, node)| match node {
        BookmarkNode::Bookmark(entry) if entry.url == url => Some(id),
        _ => None,
      })
      .collect();

    let mut moved = 0usize;
    for id in ids {
      self.move_node(id, new_parent)?;
      moved += 1;
    }

    Ok(moved)
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

  fn add_with_timestamp_and_deltas(
    &mut self,
    url: String,
    title: Option<String>,
    parent: Option<BookmarkId>,
    added_at_ms: u64,
    deltas: &mut Vec<BookmarkDelta>,
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
    self.insert_new_node(node.clone())?;
    deltas.push(BookmarkDelta::AddNode(node));
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

    // Capture cheap metadata about the node before moving `node` into `self.nodes`.
    let is_folder = matches!(&node, BookmarkNode::Folder(_));
    let is_bookmark = !is_folder;

    self.nodes.insert(id, node);
    match self.attach_to_parent_list(id, parent) {
      Ok(()) => {
        if is_bookmark {
          let BookmarkNode::Bookmark(entry) = self
            .nodes
            .get(&id)
            .expect("node inserted above") // fastrender-allow-unwrap
          else {
            unreachable!("inserted bookmark node should remain a bookmark");
          };
          self.url_index_inc(entry.url.as_str());
        }
        if is_folder {
          self.touch_folders();
        } else {
          self.touch_structure();
        }
        Ok(())
      }
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

  fn parent_list_mut(
    &mut self,
    parent: Option<BookmarkId>,
  ) -> Result<&mut Vec<BookmarkId>, BookmarkError> {
    match parent {
      None => Ok(&mut self.roots),
      Some(parent_id) => match self.nodes.get_mut(&parent_id) {
        Some(BookmarkNode::Folder(folder)) => Ok(&mut folder.children),
        Some(_) => Err(BookmarkError::ParentNotFolder(parent_id)),
        None => Err(BookmarkError::ParentNotFound(parent_id)),
      },
    }
  }

  fn reorder_before_in_parent(
    &mut self,
    id: BookmarkId,
    parent: Option<BookmarkId>,
    before_id: BookmarkId,
  ) -> Result<(), BookmarkError> {
    if id == before_id {
      return Ok(());
    }

    let actual_parent = self
      .nodes
      .get(&id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(id))?;
    if actual_parent != parent {
      return Err(BookmarkError::InvalidReorder);
    }
    let before_parent = self
      .nodes
      .get(&before_id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(before_id))?;
    if before_parent != parent {
      return Err(BookmarkError::InvalidReorder);
    }

    let moved_is_folder = matches!(self.nodes.get(&id), Some(BookmarkNode::Folder(_)));
    let mut crossed: Vec<BookmarkId> = Vec::new();

    {
      let list = self.parent_list_mut(parent)?;
      let old_idx = list
        .iter()
        .position(|x| *x == id)
        .ok_or(BookmarkError::InvalidReorder)?;
      let before_idx = list
        .iter()
        .position(|x| *x == before_id)
        .ok_or(BookmarkError::InvalidReorder)?;

      // Already immediately before the target.
      if old_idx + 1 == before_idx {
        return Ok(());
      }

      if moved_is_folder {
        // Bump `folder_revision` only if we cross at least one other folder sibling.
        if old_idx < before_idx {
          // Moving later: crossed IDs are the items between `id` and `before_id` (excluding
          // `before_id`, since `id` stays before it).
          let start = old_idx.saturating_add(1);
          let end = before_idx;
          crossed.extend(list[start..end].iter().copied());
        } else {
          // Moving earlier: crossed IDs include `before_id` (we end up before it).
          crossed.extend(list[before_idx..old_idx].iter().copied());
        }
      }

      list.remove(old_idx);
      let mut insert_idx = before_idx;
      if old_idx < before_idx {
        insert_idx = insert_idx.saturating_sub(1);
      }
      list.insert(insert_idx, id);
    }

    let folder_order_changed = moved_is_folder
      && crossed
        .iter()
        .any(|id| matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))));

    if folder_order_changed {
      self.touch_folders();
    } else {
      self.touch_structure();
    }
    Ok(())
  }

  fn reorder_after_in_parent(
    &mut self,
    id: BookmarkId,
    parent: Option<BookmarkId>,
    after_id: BookmarkId,
  ) -> Result<(), BookmarkError> {
    if id == after_id {
      return Ok(());
    }

    let actual_parent = self
      .nodes
      .get(&id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(id))?;
    if actual_parent != parent {
      return Err(BookmarkError::InvalidReorder);
    }
    let after_parent = self
      .nodes
      .get(&after_id)
      .map(BookmarkNode::parent)
      .ok_or(BookmarkError::NotFound(after_id))?;
    if after_parent != parent {
      return Err(BookmarkError::InvalidReorder);
    }

    let moved_is_folder = matches!(self.nodes.get(&id), Some(BookmarkNode::Folder(_)));
    let mut crossed: Vec<BookmarkId> = Vec::new();

    {
      let list = self.parent_list_mut(parent)?;
      let old_idx = list
        .iter()
        .position(|x| *x == id)
        .ok_or(BookmarkError::InvalidReorder)?;
      let after_idx = list
        .iter()
        .position(|x| *x == after_id)
        .ok_or(BookmarkError::InvalidReorder)?;

      // Already immediately after the target.
      if after_idx + 1 == old_idx {
        return Ok(());
      }

      if moved_is_folder {
        // Bump `folder_revision` only if we cross at least one other folder sibling.
        if old_idx < after_idx {
          // Moving later: crossed IDs include `after_id` (we end up after it).
          let start = old_idx.saturating_add(1);
          let end = after_idx.saturating_add(1);
          crossed.extend(list[start..end].iter().copied());
        } else {
          // Moving earlier: crossed IDs are between `after_id` and `id` (excluding `after_id`, since
          // `id` stays after it).
          let start = after_idx.saturating_add(1);
          let end = old_idx;
          crossed.extend(list[start..end].iter().copied());
        }
      }

      list.remove(old_idx);
      let mut insert_idx = after_idx;
      if old_idx < after_idx {
        insert_idx = insert_idx.saturating_sub(1);
      }
      list.insert(insert_idx + 1, id);
    }

    let folder_order_changed = moved_is_folder
      && crossed
        .iter()
        .any(|id| matches!(self.nodes.get(id), Some(BookmarkNode::Folder(_))));

    if folder_order_changed {
      self.touch_folders();
    } else {
      self.touch_structure();
    }
    Ok(())
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
    let root_set: FxHashSet<BookmarkId> = self.roots.iter().copied().collect();
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
        let child_set: FxHashSet<BookmarkId> = folder.children.iter().copied().collect();
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
    let mut reachable = FxHashSet::default();
    let mut stack: Vec<BookmarkId> = self.roots.clone();
    while let Some(id) = stack.pop() {
      if !reachable.insert(id) {
        continue;
      }
      if let Some(BookmarkNode::Folder(folder)) = self.nodes.get(&id) {
        stack.extend(folder.children.iter().copied());
      }
    }
    let all: FxHashSet<BookmarkId> = self.nodes.keys().copied().collect();
    if reachable != all {
      let mut missing: Vec<_> = all.difference(&reachable).copied().collect();
      missing.sort();
      return Err(format!("unreachable bookmark nodes: {missing:?}"));
    }

    // URL index must exactly reflect the current bookmark URLs in `nodes`.
    let mut expected: FxHashMap<&str, usize> = FxHashMap::default();
    for node in self.nodes.values() {
      if let BookmarkNode::Bookmark(entry) = node {
        *expected.entry(entry.url.as_str()).or_insert(0) += 1;
      }
    }
    if expected.len() != self.url_index.len() {
      return Err(format!(
        "url_index size mismatch: expected {}, got {}",
        expected.len(),
        self.url_index.len()
      ));
    }
    for (url, expected_count) in expected {
      match self.url_index.get(url) {
        Some(got) if *got == expected_count => {}
        Some(got) => {
          return Err(format!(
            "url_index count mismatch for {url:?}: expected {expected_count}, got {got}"
          ))
        }
        None => return Err(format!("url_index missing entry for {url:?}")),
      }
    }

    Ok(())
  }

  fn rebuild_url_index(&mut self) {
    self.url_index.clear();
    for node in self.nodes.values() {
      if let BookmarkNode::Bookmark(entry) = node {
        if let Some(count) = self.url_index.get_mut(entry.url.as_str()) {
          *count += 1;
        } else {
          self.url_index.insert(entry.url.clone(), 1);
        }
      }
    }
  }

  fn url_index_inc(&mut self, url: &str) {
    if let Some(count) = self.url_index.get_mut(url) {
      *count += 1;
    } else {
      self.url_index.insert(url.to_string(), 1);
    }
  }

  fn url_index_dec(&mut self, url: &str) {
    let should_remove = match self.url_index.get_mut(url) {
      Some(count) => {
        if *count > 1 {
          *count -= 1;
          false
        } else {
          true
        }
      }
      None => {
        debug_assert!(
          false,
          "url_index underflow for {url:?} (index out of sync)"
        );
        false
      }
    };

    if should_remove {
      self.url_index.remove(url);
    }
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

fn format_bookmark_widget_info_label(title: Option<&str>, url: &str) -> String {
  let url = url.trim();
  let title = title.map(str::trim).filter(|t| !t.is_empty());
  match title {
    Some(title) => format!("Bookmark: {title} — {url}"),
    None => format!("Bookmark: {url}"),
  }
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
  use egui::{Pos2, Rect, Stroke};

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

  pub(super) fn bookmarks_bar_item_widget_id(bookmark_id: BookmarkId) -> egui::Id {
    egui::Id::new(("bookmarks_bar_item", bookmark_id))
  }

  #[derive(Debug, Clone, Copy)]
  struct BookmarkBarItemContextMenuState {
    open: bool,
    /// Screen-space anchor position in egui points.
    anchor_pos: Pos2,
  }

  impl Default for BookmarkBarItemContextMenuState {
    fn default() -> Self {
      Self {
        open: false,
        anchor_pos: Pos2::ZERO,
      }
    }
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
          // Stable widget IDs are critical for AccessKit. Without a deterministic id scope,
          // reordering or changing the visible subset can cause egui's auto IDs to shift, which in
          // turn churns AccessKit NodeIds (breaking screen reader cursor / focus persistence).
          ui.push_id(("bookmark_bar_item", id.0), |ui| {
            let Some(BookmarkNode::Bookmark(entry)) = bookmarks.nodes.get(&id) else {
              return;
            };

            let url = entry.url.trim();
            if url.is_empty() {
              return;
            }

            let title = entry
              .title
              .as_deref()
              .map(str::trim)
              .filter(|t| !t.is_empty());
            let label = title
              .map(str::to_string)
              .unwrap_or_else(|| crate::ui::url_display::truncate_url_middle(url, 36));

            let tooltip = if let Some(title) = title {
              format!("{title}\n{url}")
            } else {
              url.to_string()
            };
            let a11y_label = super::format_bookmark_widget_info_label(title, url);

            let button = egui::Button::new(label)
              .small()
              .sense(egui::Sense::click_and_drag());
            let response = ui
              .add(button)
              .on_hover_text(tooltip.clone())
              .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.has_focus() && !response.hovered() {
              // Egui tooltips only show on pointer hover. Mirror the hover tooltip while
              // keyboard-focused so bookmark buttons remain discoverable for keyboard-only users.
              egui::show_tooltip_text(ui.ctx(), response.id.with("focus_tooltip"), tooltip);
            }
            response.widget_info({
              let a11y_label = a11y_label.clone();
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label.clone())
            });
            item_rects.push((id, response.rect));

            let open_new_tab = response.middle_clicked()
              || (response.clicked() && ui.input(|i| i.modifiers.command));
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

            // ---------------------------------------------------------------------------
            // Bookmark reorder context menu
            // ---------------------------------------------------------------------------
            // Open the context menu with either:
            // - Pointer: right click on the bookmark (existing behaviour)
            // - Keyboard: Shift+F10 while the bookmark has focus (Windows-style context menu gesture)
            //
            // Egui's built-in `Response::context_menu` does not currently provide a keyboard
            // activation path, so we manage open-state explicitly (similar to the tab-group chip
            // context menu).
            let button_id = bookmarks_bar_item_widget_id(id);
            let menu_state_id = button_id.with("context_menu_state");
            let mut menu_state = ui
              .ctx()
              .data(|d| d.get_temp::<BookmarkBarItemContextMenuState>(menu_state_id))
              .unwrap_or_default();
            let menu_open_prev = menu_state.open;

            let open_by_pointer = response.clicked_by(egui::PointerButton::Secondary);
            let open_by_keyboard = response.has_focus()
              && ui.input_mut(|i| {
                i.consume_key(
                  egui::Modifiers {
                    shift: true,
                    ..Default::default()
                  },
                  egui::Key::F10,
                )
              });

            let mut opened_now_via_keyboard = false;
            if open_by_pointer {
              // Anchor to the click/hover position so the menu appears where the user clicked.
              menu_state.anchor_pos = response
                .interact_pointer_pos()
                .or_else(|| ui.input(|i| i.pointer.hover_pos()))
                .unwrap_or(Pos2::new(response.rect.left(), response.rect.bottom()));
              menu_state.open = true;
            } else if open_by_keyboard {
              if menu_state.open {
                // Pressing Shift+F10 again closes the menu (standard toggle behaviour).
                menu_state.open = false;
              } else {
                // Anchor below the button when opened via keyboard (no cursor position).
                menu_state.anchor_pos = Pos2::new(response.rect.left(), response.rect.bottom());
                menu_state.open = true;
                opened_now_via_keyboard = true;
              }
            }

            // Clicking the button while its context menu is open should dismiss the menu (standard
            // popup behaviour). We still allow the bookmark activation itself to proceed.
            if menu_state.open
              && (response.clicked()
                || response.middle_clicked()
                || (response.clicked() && ui.input(|i| i.modifiers.command)))
            {
              menu_state.open = false;
            }

            if menu_state.open {
              let mut close_menu = false;
              if ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)) {
                close_menu = true;
              }

              let menu_id = button_id.with("context_menu_popup");
              let area = egui::Area::new(menu_id)
                .order(egui::Order::Foreground)
                .fixed_pos(menu_state.anchor_pos)
                .constrain_to(ui.ctx().screen_rect())
                .interactable(true);

              let inner = area.show(ui.ctx(), |ui| {
                // Scope the menu under a stable id derived from the bookmark ID so menu item ids are
                // stable (important for focus + AccessKit).
                ui.push_id(("bookmark_context_menu", id.0), |ui| {
                  let frame = egui::Frame::popup(ui.style());
                  frame
                    .show(ui, |ui| {
                      ui.set_min_width(140.0);

                      let Some(idx) = visible_ids.iter().position(|x| *x == id) else {
                        return;
                      };

                      let can_move_left = idx > 0;
                      let can_move_right = idx + 1 < visible_ids.len();

                      let move_left =
                        ui.add_enabled(can_move_left, egui::Button::new("Move left"));
                      move_left.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, "Move bookmark left")
                      });
                      if move_left.clicked() {
                        if let Some(new_order) =
                          move_before_id(&bookmarks.roots, id, visible_ids[idx - 1])
                        {
                          out.reorder_roots = Some(new_order);
                        }
                        close_menu = true;
                      }

                      let move_right =
                        ui.add_enabled(can_move_right, egui::Button::new("Move right"));
                      move_right.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, "Move bookmark right")
                      });
                      if move_right.clicked() {
                        if let Some(new_order) =
                          move_after_id(&bookmarks.roots, id, visible_ids[idx + 1])
                        {
                          out.reorder_roots = Some(new_order);
                        }
                        close_menu = true;
                      }

                      if opened_now_via_keyboard {
                        // Focus the first enabled menu item so keyboard users can act immediately.
                        if can_move_left {
                          move_left.request_focus();
                        } else if can_move_right {
                          move_right.request_focus();
                        }
                      }

                      #[cfg(test)]
                      ui.ctx().data_mut(|d| {
                        d.insert_temp(
                          egui::Id::new(("test_bookmarks_bar_move_left_id", id.0)),
                          move_left.id,
                        );
                        d.insert_temp(
                          egui::Id::new(("test_bookmarks_bar_move_right_id", id.0)),
                          move_right.id,
                        );
                      });

                      if close_menu {
                        ui.close_menu();
                      }
                    })
                    .inner
                });
              });

              let menu_rect = inner.response.rect;

              // Best-effort: close when clicking outside the button and the popup.
              let clicked_outside = ui.ctx().input(|i| {
                i.pointer.any_pressed()
                  && i
                    .pointer
                    .interact_pos()
                    .or_else(|| i.pointer.latest_pos())
                    .is_some_and(|pos| !response.rect.contains(pos) && !menu_rect.contains(pos))
              });
              if clicked_outside {
                close_menu = true;
              }

              if close_menu {
                menu_state.open = false;
                // Return focus to the opener so the keyboard user isn't left in limbo.
                response.request_focus();
              }
            }

            if menu_open_prev != menu_state.open {
              ui.ctx().request_repaint();
            }

            ui.ctx().data_mut(|d| {
              d.insert_temp(menu_state_id, menu_state);
            });

            #[cfg(test)]
            ui.ctx().data_mut(|d| {
              d.insert_temp(
                egui::Id::new(("test_bookmarks_bar_context_menu_open", id.0)),
                menu_state.open,
              );
              d.insert_temp(
                egui::Id::new(("test_bookmarks_bar_button_id", id.0)),
                response.id,
              );
            });
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
        let highlight = ui.visuals().selection.stroke;
        ui.painter().rect_stroke(
          rect.expand(1.0),
          egui::Rounding::same(6.0),
          Stroke::new(highlight.width, highlight.color),
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
  fn revision_bumps_on_structural_changes() {
    let mut store = BookmarkStore::default();
    assert_eq!(store.revision, 0);

    // Adding a bookmark should bump.
    let a = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
      )
      .unwrap();
    let rev_after_add = store.revision;
    assert!(rev_after_add > 0, "expected revision to bump after add");

    // Creating a folder should bump.
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let rev_after_folder = store.revision;
    assert!(
      rev_after_folder > rev_after_add,
      "expected revision to bump after create_folder"
    );

    // Moving a node should bump.
    store.move_node(a, Some(folder)).unwrap();
    let rev_after_move = store.revision;
    assert!(
      rev_after_move > rev_after_folder,
      "expected revision to bump after move_node"
    );

    // No-op move should not bump.
    store.move_node(a, Some(folder)).unwrap();
    assert_eq!(
      store.revision, rev_after_move,
      "expected no-op move to not bump revision"
    );

    // Removing a node should bump.
    assert!(store.remove_by_id(a));
    let rev_after_remove = store.revision;
    assert!(
      rev_after_remove > rev_after_move,
      "expected revision to bump after remove_by_id"
    );

    // Removing a missing node should not bump.
    assert!(!store.remove_by_id(BookmarkId(999_999)));
    assert_eq!(
      store.revision, rev_after_remove,
      "expected remove_by_id on missing id to not bump revision"
    );

    // Toggle add should bump.
    let rev_before_toggle_add = store.revision;
    assert!(store.toggle("https://toggle.example/", Some("Toggle")));
    assert!(
      store.revision > rev_before_toggle_add,
      "expected revision to bump after toggle add"
    );

    // Toggle remove should bump (removes all duplicates).
    let rev_before_toggle_remove = store.revision;
    assert!(!store.toggle("https://toggle.example/", Some("Ignored")));
    assert!(
      store.revision > rev_before_toggle_remove,
      "expected revision to bump after toggle remove"
    );

    // Reorder bumps only when it actually changes the order.
    let x = store
      .add("https://x.example/".to_string(), Some("X".to_string()), None)
      .unwrap();
    let y = store
      .add("https://y.example/".to_string(), Some("Y".to_string()), None)
      .unwrap();
    let rev_before_reorder = store.revision;
    store.reorder_root(&[y, x, folder]).unwrap();
    assert!(
      store.revision > rev_before_reorder,
      "expected revision to bump after reorder"
    );
    let rev_after_reorder = store.revision;
    store.reorder_root(&[y, x, folder]).unwrap();
    assert_eq!(
      store.revision, rev_after_reorder,
      "expected reorder_root to not bump revision when order is unchanged"
    );
  }

  #[test]
  fn folder_revision_tracks_folder_structure_changes() {
    let mut store = BookmarkStore::default();
    assert_eq!(store.folder_revision(), 0);

    // Adding/moving/removing bookmarks should not bump folder revision.
    let bookmark = store
      .add(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
      )
      .unwrap();
    let folder_rev_after_bookmark = store.folder_revision();

    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    assert!(
      store.folder_revision() > folder_rev_after_bookmark,
      "expected folder_revision to bump after creating a folder"
    );

    let folder_rev_before_move_bookmark = store.folder_revision();
    store.move_node(bookmark, Some(folder)).unwrap();
    assert_eq!(
      store.folder_revision(),
      folder_rev_before_move_bookmark,
      "expected moving a bookmark to not bump folder_revision"
    );

    let folder_rev_before_remove_bookmark = store.folder_revision();
    assert!(store.remove_by_id(bookmark));
    assert_eq!(
      store.folder_revision(),
      folder_rev_before_remove_bookmark,
      "expected removing a bookmark to not bump folder_revision"
    );

    // Reordering roots without changing relative folder order should not bump folder revision.
    let folder2 = store.create_folder("Folder2".to_string(), None).unwrap();
    let x = store
      .add("https://x.example/".to_string(), Some("X".to_string()), None)
      .unwrap();
    let y = store
      .add("https://y.example/".to_string(), Some("Y".to_string()), None)
      .unwrap();

    let folder_rev_before_reorder_bookmarks = store.folder_revision();
    // Keep folder ordering `[folder, folder2]` the same.
    store.reorder_root(&[y, folder, folder2, x]).unwrap();
    assert_eq!(
      store.folder_revision(),
      folder_rev_before_reorder_bookmarks,
      "expected bookmark-only root reorder to not bump folder_revision"
    );

    // Changing folder ordering should bump folder revision.
    let folder_rev_before_reorder_folders = store.folder_revision();
    store.reorder_root(&[y, folder2, folder, x]).unwrap();
    assert!(
      store.folder_revision() > folder_rev_before_reorder_folders,
      "expected folder root reorder to bump folder_revision"
    );

    // Removing a folder should bump folder revision.
    let folder_rev_before_remove_folder = store.folder_revision();
    assert!(store.remove_by_id(folder));
    assert!(
      store.folder_revision() > folder_rev_before_remove_folder,
      "expected removing a folder to bump folder_revision"
    );
  }

  #[test]
  fn structure_revision_tracks_tree_shape_changes() {
    let mut store = BookmarkStore::default();
    assert_eq!(store.structure_revision(), 0);

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

    let structure_after_add = store.structure_revision();
    assert!(
      structure_after_add > 0,
      "expected structure_revision to bump after adding bookmarks"
    );

    // Content-only updates should not bump structure revision.
    store
      .update(
        a,
        Some("A (edited)".to_string()),
        "https://a.example/".to_string(),
        None,
      )
      .unwrap();
    assert_eq!(
      store.structure_revision(),
      structure_after_add,
      "expected updating bookmark fields to not bump structure_revision"
    );

    // Creating a folder bumps structure revision.
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    assert!(
      store.structure_revision() > structure_after_add,
      "expected creating a folder to bump structure_revision"
    );

    // Moving a bookmark bumps structure revision.
    let structure_before_move = store.structure_revision();
    store.move_node(a, Some(folder)).unwrap();
    assert!(
      store.structure_revision() > structure_before_move,
      "expected moving a bookmark to bump structure_revision"
    );

    // Reordering roots (even just bookmarks vs folders) bumps structure revision.
    let structure_before_reorder = store.structure_revision();
    store.reorder_root(&[folder, b]).unwrap();
    assert!(
      store.structure_revision() > structure_before_reorder,
      "expected reorder_root to bump structure_revision when the order changes"
    );

    // Removing a bookmark bumps structure revision.
    let structure_before_remove = store.structure_revision();
    assert!(store.remove_by_id(b));
    assert!(
      store.structure_revision() > structure_before_remove,
      "expected removing a bookmark to bump structure_revision"
    );
  }

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
  fn url_index_counts_duplicates_and_decrements_on_remove() {
    let mut store = BookmarkStore::default();
    let a1 = store
      .add("https://a.example/".to_string(), Some("A1".to_string()), None)
      .unwrap();
    let a2 = store
      .add("https://a.example/".to_string(), Some("A2".to_string()), None)
      .unwrap();

    assert_eq!(store.url_index.get("https://a.example/"), Some(&2));

    assert!(store.remove_by_id(a1));
    assert_eq!(store.url_index.get("https://a.example/"), Some(&1));
    assert!(store.contains_url("https://a.example/"));

    assert!(store.remove_by_id(a2));
    assert!(store.url_index.get("https://a.example/").is_none());
    assert!(!store.contains_url("https://a.example/"));
  }

  #[test]
  fn toggle_removes_all_duplicates_and_clears_url_index_entry() {
    let mut store = BookmarkStore::default();
    let _ = store
      .add("https://a.example/".to_string(), Some("A1".to_string()), None)
      .unwrap();
    let _ = store
      .add("https://a.example/".to_string(), Some("A2".to_string()), None)
      .unwrap();
    assert_eq!(store.url_index.get("https://a.example/"), Some(&2));

    // Toggle semantics remove *all* bookmarks for the URL.
    assert_eq!(store.toggle("https://a.example/", None), false);
    assert!(store.url_index.get("https://a.example/").is_none());
    assert!(!store.contains_url("https://a.example/"));
    assert!(store.nodes.is_empty());
  }

  #[test]
  fn url_index_is_rebuilt_on_load_and_survives_normalization() {
    let mut store = BookmarkStore::default();
    let _ = store
      .add("https://a.example/".to_string(), Some("A1".to_string()), None)
      .unwrap();
    let _ = store
      .add("https://a.example/".to_string(), Some("A2".to_string()), None)
      .unwrap();

    // Simulate a corrupted persisted file where `next_id` is lower than the max node id.
    store.next_id = BookmarkId(1);

    let json = serde_json::to_string_pretty(&store).unwrap();
    let (loaded, migration) = BookmarkStore::from_json_str_migrating(&json).unwrap();
    assert_eq!(migration, BookmarkStoreMigration::None);
    assert_eq!(loaded.next_id, BookmarkId(3));
    assert!(loaded.contains_url("https://a.example/"));
    assert_eq!(loaded.url_index.get("https://a.example/"), Some(&2));
  }

  #[test]
  fn bookmark_widget_info_label_formats_title_and_url() {
    assert_eq!(
      format_bookmark_widget_info_label(Some("Example"), "https://example.com"),
      "Bookmark: Example — https://example.com"
    );
    assert_eq!(
      format_bookmark_widget_info_label(None, "https://example.com"),
      "Bookmark: https://example.com"
    );
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
  fn reorder_root_rejects_invalid_permutations_and_duplicates() {
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
    let c = store
      .add(
        "https://c.example/".to_string(),
        Some("C".to_string()),
        None,
      )
      .unwrap();

    assert_eq!(store.roots, vec![a, b, c]);

    // Wrong length (missing items).
    assert_eq!(
      store.reorder_root(&[a, b]).unwrap_err(),
      BookmarkError::InvalidReorder
    );

    // Duplicate IDs (also implies one missing).
    assert_eq!(
      store.reorder_root(&[a, a, b]).unwrap_err(),
      BookmarkError::InvalidReorder
    );

    // Unknown ID not present in the current roots list.
    assert_eq!(
      store
        .reorder_root(&[a, b, BookmarkId(999)])
        .unwrap_err(),
      BookmarkError::InvalidReorder
    );
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
  fn update_can_change_title_url_and_parent() {
    let mut store = BookmarkStore::default();
    let folder_a = store.create_folder("A".to_string(), None).unwrap();
    let folder_b = store.create_folder("B".to_string(), None).unwrap();
    let bookmark = store
      .add(
        "https://example.com/".to_string(),
        Some("Old".to_string()),
        Some(folder_a),
      )
      .unwrap();
    assert_eq!(store.url_index.get("https://example.com/"), Some(&1));

    store
      .update(
        bookmark,
        Some("New title".to_string()),
        "https://example.com/new".to_string(),
        Some(folder_b),
      )
      .unwrap();
    assert!(!store.contains_url("https://example.com/"));
    assert!(store.contains_url("https://example.com/new"));
    assert!(store.url_index.get("https://example.com/").is_none());
    assert_eq!(store.url_index.get("https://example.com/new"), Some(&1));

    let BookmarkNode::Bookmark(entry) = store.nodes.get(&bookmark).unwrap() else {
      panic!("expected bookmark");
    };
    assert_eq!(entry.url, "https://example.com/new");
    assert_eq!(entry.title.as_deref(), Some("New title"));
    assert_eq!(entry.parent, Some(folder_b));

    let BookmarkNode::Folder(a) = store.nodes.get(&folder_a).unwrap() else {
      panic!("expected folder");
    };
    assert!(
      !a.children.contains(&bookmark),
      "expected bookmark to be detached from old parent"
    );
    let BookmarkNode::Folder(b) = store.nodes.get(&folder_b).unwrap() else {
      panic!("expected folder");
    };
    assert!(
      b.children.contains(&bookmark),
      "expected bookmark to be attached to new parent"
    );
  }

  #[test]
  fn folder_paths_are_deterministic() {
    let mut store = BookmarkStore::default();
    let work = store.create_folder("Work".to_string(), None).unwrap();
    let project = store
      .create_folder("Project".to_string(), Some(work))
      .unwrap();
    let _bookmark = store
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        Some(project),
      )
      .unwrap();

    assert_eq!(
      store.folder_path_titles(project).unwrap(),
      vec!["Work".to_string(), "Project".to_string()]
    );
    let folders = store.folders();
    assert_eq!(
      folders,
      vec![
        (work, vec!["Work".to_string()]),
        (project, vec!["Work".to_string(), "Project".to_string()])
      ]
    );
    assert_eq!(
      store.folders_in_display_order_joined(),
      vec![(work, "Work".to_string()), (project, "Work/Project".to_string())]
    );

    assert_eq!(
      store.folder_id_by_path_titles(&vec!["Work".to_string(), "Project".to_string()]),
      Some(project)
    );

    // Convenience wrapper for importing/bookmarks-by-url workflows.
    let moved = store
      .add(
        "https://move.example/".to_string(),
        Some("Move".to_string()),
        None,
      )
      .unwrap();
    assert_eq!(
      store.move_to_folder("https://move.example/", &vec!["Work".to_string()]),
      Ok(1)
    );
    assert_eq!(
      store.nodes.get(&moved).and_then(BookmarkNode::parent),
      Some(work)
    );
  }

  #[test]
  fn json_export_is_stable_after_roundtrip() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    store
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        Some(folder),
      )
      .unwrap();

    let json_a = serde_json::to_string_pretty(&store).unwrap();
    let decoded: BookmarkStore = serde_json::from_str(&json_a).unwrap();
    let json_b = serde_json::to_string_pretty(&decoded).unwrap();
    assert_eq!(json_a, json_b);
  }

  #[test]
  fn migrate_from_legacy_urls_json() {
    let legacy = r#"{"urls":["https://a.example/","https://b.example/"]}"#;
    let (store, migration) = BookmarkStore::from_json_str_migrating(legacy).unwrap();
    assert_eq!(migration, BookmarkStoreMigration::FromLegacyUrls);
    assert_eq!(store.roots.len(), 2);
    assert!(store.contains_url("https://a.example/"));
    assert!(store.contains_url("https://b.example/"));
    assert_eq!(store.url_index.get("https://a.example/"), Some(&1));
    assert_eq!(store.url_index.get("https://b.example/"), Some(&1));
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
  fn search_tokenized_case_insensitive_matches_title_and_url() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let a = store
      .add(
        "https://example.com/rust".to_string(),
        Some("Rust Lang".to_string()),
        None,
      )
      .unwrap();
    let b = store
      .add(
        "https://mozilla.org/".to_string(),
        Some("Mozilla".to_string()),
        Some(folder),
      )
      .unwrap();

    assert_eq!(store.search("rust", usize::MAX), vec![a]);
    assert_eq!(store.search("RUST lang", usize::MAX), vec![a]);
    assert_eq!(store.search("example RUST", usize::MAX), vec![a]);
    assert_eq!(store.search("moz", usize::MAX), vec![b]);

    // Scan limit: only the first bookmark in store order is examined (the folder's child `b`).
    assert_eq!(store.search("rust", 1), Vec::<BookmarkId>::new());
  }

  #[test]
  fn search_is_ascii_case_insensitive_only() {
    let mut store = BookmarkStore::default();
    let id = store
      .add(
        "https://example.com/über".to_string(),
        Some("über".to_string()),
        None,
      )
      .unwrap();

    // ASCII letters match case-insensitively.
    assert_eq!(store.search("üBER", usize::MAX), vec![id]);

    // Non-ASCII bytes compare exactly: Ü != ü.
    assert_eq!(store.search("ÜBER", usize::MAX), Vec::<BookmarkId>::new());
  }

  #[test]
  fn export_import_roundtrip_json_migrating() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let _ = store
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        Some(folder),
      )
      .unwrap();

    let json = serde_json::to_string_pretty(&store).unwrap();
    let (decoded, migration) = BookmarkStore::from_json_str_migrating(&json).unwrap();
    assert_eq!(migration, BookmarkStoreMigration::None);
    assert_eq!(decoded, store);
  }

  #[test]
  fn add_rejects_invalid_url_scheme() {
    let mut store = BookmarkStore::default();
    assert!(store
      .add("javascript:alert(1)".to_string(), None, None)
      .is_err());
    assert!(!store.contains_url("javascript:alert(1)"));
  }

  #[test]
  fn update_rejects_invalid_url_scheme_and_is_atomic() {
    let mut store = BookmarkStore::default();
    let folder = store.create_folder("Folder".to_string(), None).unwrap();
    let bookmark = store
      .add(
        "https://example.com/".to_string(),
        Some("Example".to_string()),
        None,
      )
      .unwrap();

    assert!(store
      .update(
        bookmark,
        Some("Should not apply".to_string()),
        "javascript:alert(1)".to_string(),
        Some(folder),
      )
      .is_err());

    // URL + title should be unchanged.
    let BookmarkNode::Bookmark(entry) = store.nodes.get(&bookmark).unwrap() else {
      panic!("expected bookmark");
    };
    assert_eq!(entry.url, "https://example.com/");
    assert_eq!(entry.title.as_deref(), Some("Example"));

    // Parent move should not have applied either.
    assert_eq!(entry.parent, None);
    assert!(store.roots.contains(&bookmark));
    let BookmarkNode::Folder(folder_node) = store.nodes.get(&folder).unwrap() else {
      panic!("expected folder");
    };
    assert!(
      !folder_node.children.contains(&bookmark),
      "bookmark should not have been moved when update failed"
    );
  }

  #[test]
  fn deltas_replay_to_same_store_state() {
    let mut mutated = BookmarkStore::default();
    let mut deltas = Vec::<BookmarkDelta>::new();

    let folder = mutated
      .create_folder_with_deltas("Folder".to_string(), None, &mut deltas)
      .unwrap();
    let a = mutated
      .add_with_deltas(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
        &mut deltas,
      )
      .unwrap();
    let b = mutated
      .add_with_deltas(
        "https://b.example/".to_string(),
        Some("B".to_string()),
        Some(folder),
        &mut deltas,
      )
      .unwrap();

    mutated
      .update_with_deltas(
        a,
        Some("A+".to_string()),
        "https://a.example/new".to_string(),
        Some(folder),
        &mut deltas,
      )
      .unwrap();
    mutated
      .move_node_with_deltas(b, None, &mut deltas)
      .unwrap();
    mutated
      .reorder_root_with_deltas(&[b, folder], &mut deltas)
      .unwrap();
    // Toggle an existing bookmark (removes it).
    assert!(!mutated.toggle_with_deltas("https://b.example/", None, &mut deltas));
    // Toggle a new URL (adds it).
    assert!(mutated.toggle_with_deltas(
      "https://c.example/",
      Some("C"),
      &mut deltas
    ));
    // Remove the folder subtree.
    assert!(mutated.remove_by_id_with_deltas(folder, &mut deltas));

    let mut applied = BookmarkStore::default();
    applied.apply_deltas(&deltas).unwrap();

    assert_eq!(applied, mutated);
  }

  #[test]
  fn deltas_keep_multiple_stores_in_sync() {
    let mut global = BookmarkStore::default();
    let mut win_a = BookmarkStore::default();
    let mut win_b = BookmarkStore::default();

    let mut deltas = Vec::<BookmarkDelta>::new();

    let _folder = win_a
      .create_folder_with_deltas("Folder".to_string(), None, &mut deltas)
      .unwrap();
    assert!(win_a.toggle_with_deltas(
      "https://example.com/",
      Some("Example"),
      &mut deltas
    ));

    global.apply_deltas(&deltas).unwrap();
    win_b.apply_deltas(&deltas).unwrap();

    assert_eq!(global, win_a);
    assert_eq!(win_b, win_a);

    deltas.clear();
    // Window B toggles the same URL (removes it).
    assert!(!win_b.toggle_with_deltas("https://example.com/", None, &mut deltas));

    global.apply_deltas(&deltas).unwrap();
    win_a.apply_deltas(&deltas).unwrap();

    assert_eq!(global, win_a);
    assert_eq!(win_b, win_a);
  }

  #[test]
  fn reorder_root_with_deltas_uses_compact_delta_for_single_move() {
    let mut store = BookmarkStore::default();
    let a = store
      .add("https://a.example/".to_string(), Some("A".to_string()), None)
      .unwrap();
    let b = store
      .add("https://b.example/".to_string(), Some("B".to_string()), None)
      .unwrap();
    let c = store
      .add("https://c.example/".to_string(), Some("C".to_string()), None)
      .unwrap();
    let d = store
      .add("https://d.example/".to_string(), Some("D".to_string()), None)
      .unwrap();

    let mut deltas = Vec::new();
    store
      .reorder_root_with_deltas(&[a, c, d, b], &mut deltas)
      .unwrap();

    assert_eq!(
      deltas,
      vec![BookmarkDelta::ReorderAfter {
        id: b,
        parent: None,
        after_id: d
      }],
      "expected compact reorder delta"
    );

    let mut other = BookmarkStore::default();
    // Mirror the pre-reorder state by replaying the corresponding creation deltas.
    let mut bootstrap = Vec::new();
    other
      .add_with_deltas(
        "https://a.example/".to_string(),
        Some("A".to_string()),
        None,
        &mut bootstrap,
      )
      .unwrap();
    other
      .add_with_deltas(
        "https://b.example/".to_string(),
        Some("B".to_string()),
        None,
        &mut bootstrap,
      )
      .unwrap();
    other
      .add_with_deltas(
        "https://c.example/".to_string(),
        Some("C".to_string()),
        None,
        &mut bootstrap,
      )
      .unwrap();
    other
      .add_with_deltas(
        "https://d.example/".to_string(),
        Some("D".to_string()),
        None,
        &mut bootstrap,
      )
      .unwrap();

    other.apply_deltas(&deltas).unwrap();
    assert_eq!(other.roots, vec![a, c, d, b]);
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod a11y_id_tests {
  use super::*;
  use crate::ui::a11y_test_util;

  fn begin_frame(ctx: &egui::Context) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 200.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    ctx.begin_frame(raw);
  }

  fn render_bookmarks_bar(ctx: &egui::Context, store: &BookmarkStore) -> egui::FullOutput {
    begin_frame(ctx);
    egui::CentralPanel::default().show(ctx, |ui| {
      let _ = super::bookmarks_bar_ui(ui, store, 12);
    });
    ctx.end_frame()
  }

  fn accesskit_button_id(output: &egui::FullOutput, name: &str) -> String {
    let snapshot = a11y_test_util::accesskit_snapshot_from_full_output(output);
    snapshot
      .nodes
      .iter()
      .find(|n| n.role == "Button" && n.name == name)
      .map(|n| n.id.clone())
      .unwrap_or_else(|| {
        let pretty = a11y_test_util::accesskit_pretty_json_from_full_output(output);
        panic!("failed to find AccessKit Button with name {name:?}.\n\n{pretty}");
      })
  }

  #[test]
  fn bookmarks_bar_bookmark_buttons_have_stable_accesskit_ids_across_reorder() {
    let ctx = egui::Context::default();
    // AccessKit output is typically enabled/disabled by the platform adapter (egui-winit).
    // In headless unit tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

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

    let name_a = format_bookmark_widget_info_label(Some("A"), "https://a.example/");
    let name_b = format_bookmark_widget_info_label(Some("B"), "https://b.example/");

    let first = render_bookmarks_bar(&ctx, &store);
    let first_id_a = accesskit_button_id(&first, &name_a);
    let first_id_b = accesskit_button_id(&first, &name_b);

    store.reorder_root(&[b, a]).unwrap();

    let second = render_bookmarks_bar(&ctx, &store);
    let second_id_a = accesskit_button_id(&second, &name_a);
    let second_id_b = accesskit_button_id(&second, &name_b);

    assert_eq!(
      first_id_a, second_id_a,
      "expected bookmark A to keep the same AccessKit node id across reorder"
    );
    assert_eq!(
      first_id_b, second_id_b,
      "expected bookmark B to keep the same AccessKit node id across reorder"
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod bookmarks_bar_ui_tests {
  use super::*;
  use crate::ui::a11y_test_util;

  fn begin_frame(ctx: &egui::Context, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.events = events;
    ctx.begin_frame(raw);
  }

  fn render_bar(ctx: &egui::Context, store: &BookmarkStore) -> egui::FullOutput {
    begin_frame(ctx, Vec::new());
    egui::CentralPanel::default().show(ctx, |ui| {
      let _out = bookmarks_bar_ui(ui, store, usize::MAX);
    });
    ctx.end_frame()
  }

  #[test]
  fn bookmark_context_menu_opens_via_shift_f10_focuses_first_item_and_closes_on_escape() {
    let mut store = BookmarkStore::default();
    let _a = store
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

    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    // Frame 1: render once so button ids are registered.
    let _ = render_bar(&ctx, &store);

    let button_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new(("test_bookmarks_bar_button_id", b.0))))
      .expect("expected bookmark button id to be stored for tests");

    // Frame 2: focus the bookmark button.
    ctx.memory_mut(|mem| mem.request_focus(button_id));
    begin_frame(&ctx, Vec::new());
    egui::CentralPanel::default().show(&ctx, |ui| {
      let _out = bookmarks_bar_ui(ui, &store, usize::MAX);
    });
    let _ = ctx.end_frame();
    assert!(
      ctx.memory(|mem| mem.has_focus(button_id)),
      "expected bookmark button to have focus"
    );

    // Frame 3: inject Shift+F10.
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    raw.time = Some(0.0);
    raw.focused = true;
    raw.modifiers.shift = true;
    raw.events = vec![egui::Event::Key {
      key: egui::Key::F10,
      pressed: true,
      repeat: false,
      modifiers: egui::Modifiers {
        shift: true,
        ..Default::default()
      },
    }];
    ctx.begin_frame(raw);
    egui::CentralPanel::default().show(&ctx, |ui| {
      let _out = bookmarks_bar_ui(ui, &store, usize::MAX);
    });
    let output = ctx.end_frame();

    let menu_open = ctx
      .data(|d| d.get_temp::<bool>(egui::Id::new(("test_bookmarks_bar_context_menu_open", b.0))))
      .unwrap_or(false);
    assert!(
      menu_open,
      "expected bookmark context menu to be open after Shift+F10"
    );

    let move_left_id = ctx
      .data(|d| d.get_temp::<egui::Id>(egui::Id::new(("test_bookmarks_bar_move_left_id", b.0))))
      .expect("expected move-left id to be stored");
    assert!(
      ctx.memory(|mem| mem.has_focus(move_left_id)),
      "expected first enabled menu item to have focus when opened via keyboard"
    );

    // AccessKit: ensure menu items are present when the menu is open.
    let names = a11y_test_util::accesskit_names_from_full_output(&output);
    let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
    assert!(
      names
        .iter()
        .any(|n| n == "Move bookmark left" || n == "Move left"),
      "expected Move left menu item in AccessKit output.\n\nnames: {names:#?}\n\nsnapshot:\n{snapshot}"
    );

    // Frame 4: Escape closes the menu and returns focus to the opener.
    begin_frame(
      &ctx,
      vec![egui::Event::Key {
        key: egui::Key::Escape,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::default(),
      }],
    );
    egui::CentralPanel::default().show(&ctx, |ui| {
      let _out = bookmarks_bar_ui(ui, &store, usize::MAX);
    });
    let _ = ctx.end_frame();

    let menu_open = ctx
      .data(|d| d.get_temp::<bool>(egui::Id::new(("test_bookmarks_bar_context_menu_open", b.0))))
      .unwrap_or(false);
    assert!(
      !menu_open,
      "expected bookmark context menu to close on Escape"
    );
    assert!(
      ctx.memory(|mem| mem.has_focus(button_id)),
      "expected focus to return to bookmark button after closing menu"
    );
  }
}
