use crate::interaction::selection_serialize::{DocumentSelectionPoint, DocumentSelectionRange};
use crate::interaction::state::DocumentSelectionState;
use crate::style::computed::Visibility;
use crate::style::types::UserSelect;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree, TextSourceRange};
use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[cfg(feature = "vmjs")]
use crate::dom2::RendererDomMapping;
#[cfg(feature = "vmjs")]
use crate::interaction::state::DocumentSelectionStateDom2;

fn box_is_selectable(node: &BoxNode) -> bool {
  // Keep this consistent with `interaction::selection_serialize::box_is_selectable`.
  if node.style.display.is_none() {
    return false;
  }
  if node.style.visibility != Visibility::Visible {
    return false;
  }
  if node.style.user_select == UserSelect::None {
    return false;
  }
  if node.style.inert {
    return false;
  }
  true
}

fn cmp_point(a: DocumentSelectionPoint, b: DocumentSelectionPoint) -> Ordering {
  a.node_id
    .cmp(&b.node_id)
    .then_with(|| a.char_offset.cmp(&b.char_offset))
}

fn char_boundary_bytes(text: &str) -> Vec<usize> {
  let mut out = Vec::with_capacity(text.chars().count().saturating_add(1));
  for (idx, _) in text.char_indices() {
    out.push(idx);
  }
  out.push(text.len());
  out
}

fn char_idx_for_byte(boundaries: &[usize], byte_idx: usize) -> usize {
  match boundaries.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

#[cfg(test)]
static SELECTION_INDEX_PATH_VISITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_selection_index_counters() {
  SELECTION_INDEX_PATH_VISITS.store(0, AtomicOrdering::Relaxed);
}

#[cfg(test)]
pub(crate) fn selection_index_path_visits() -> usize {
  SELECTION_INDEX_PATH_VISITS.load(AtomicOrdering::Relaxed)
}

#[cfg(test)]
fn record_selection_index_path_visit() {
  SELECTION_INDEX_PATH_VISITS.fetch_add(1, AtomicOrdering::Relaxed);
}

#[derive(Clone, Copy, Debug)]
struct TextFragmentSpan {
  node_id: usize,
  node_len: usize,
  frag_start: usize,
  frag_end: usize,
}

fn text_fragment_span(
  box_tree: &BoxTree,
  boundaries_cache: &mut FxHashMap<usize, Vec<usize>>,
  box_id: usize,
  source_range: TextSourceRange,
) -> Option<TextFragmentSpan> {
  let box_node = box_node_by_id(box_tree, box_id)?;
  if !box_is_selectable(box_node) {
    return None;
  }
  let node_id = box_node.styled_node_id?;
  let BoxType::Text(text_box) = &box_node.box_type else {
    return None;
  };

  let boundaries = boundaries_cache
    .entry(box_id)
    .or_insert_with(|| char_boundary_bytes(&text_box.text));

  let node_len = boundaries.len().saturating_sub(1);
  let start = char_idx_for_byte(boundaries, source_range.start());
  let end = char_idx_for_byte(boundaries, source_range.end());
  let (frag_start, frag_end) = if start <= end {
    (start, end)
  } else {
    (end, start)
  };

  Some(TextFragmentSpan {
    node_id,
    node_len,
    frag_start: frag_start.min(node_len),
    frag_end: frag_end.min(node_len),
  })
}

fn box_node_by_id<'a>(box_tree: &'a BoxTree, target_box_id: usize) -> Option<&'a BoxNode> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.id == target_box_id {
      return Some(node);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[derive(Debug, Clone)]
struct SelectableTextBoxData {
  node_id: usize,
  boundaries: Vec<usize>,
}

fn selectable_text_boxes(box_tree: &BoxTree) -> FxHashMap<usize, SelectableTextBoxData> {
  let mut map: FxHashMap<usize, SelectableTextBoxData> = FxHashMap::default();
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let (Some(node_id), BoxType::Text(text_box)) = (node.styled_node_id, &node.box_type) {
      if box_is_selectable(node) {
        map.insert(
          node.id,
          SelectableTextBoxData {
            node_id,
            boundaries: char_boundary_bytes(&text_box.text),
          },
        );
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  map
}

#[derive(Debug, Clone)]
struct DocumentSelectionIndexEntry {
  /// Child-index path to the text fragment's location within the root fragment.
  path: Box<[u32]>,
  box_id: usize,
  source_range: TextSourceRange,
  node_id: usize,
  node_len: usize,
  frag_start: usize,
  frag_end: usize,
  local_len: usize,
}

#[derive(Debug, Clone)]
struct DocumentSelectionIndexRoot {
  /// 0 = `FragmentTree::root`, otherwise `FragmentTree::additional_fragments[(id-1)]`.
  root_id: u32,
  entries: Vec<DocumentSelectionIndexEntry>,
}

/// Index of selectable text fragments for fast document selection updates.
///
/// This is built once after layout and reused for paint-only selection updates so we can avoid
/// repeatedly traversing the full fragment tree structure.
#[derive(Debug, Clone)]
pub(crate) struct DocumentSelectionIndex {
  roots: Vec<DocumentSelectionIndexRoot>,
}

impl DocumentSelectionIndex {
  pub(crate) fn build(box_tree: &BoxTree, fragment_tree: &FragmentTree) -> Self {
    let text_boxes = selectable_text_boxes(box_tree);
    let mut roots = Vec::with_capacity(fragment_tree.additional_fragments.len().saturating_add(1));
    roots.push(Self::build_root_index(
      0,
      &fragment_tree.root,
      &text_boxes,
    ));
    for (idx, root) in fragment_tree.additional_fragments.iter().enumerate() {
      roots.push(Self::build_root_index(
        idx.saturating_add(1) as u32,
        root,
        &text_boxes,
      ));
    }
    Self { roots }
  }

  fn build_root_index(
    root_id: u32,
    root: &FragmentNode,
    text_boxes: &FxHashMap<usize, SelectableTextBoxData>,
  ) -> DocumentSelectionIndexRoot {
    let mut entries: Vec<DocumentSelectionIndexEntry> = Vec::new();
    let mut stack: Vec<(Vec<u32>, &FragmentNode)> = vec![(Vec::new(), root)];

    while let Some((path, node)) = stack.pop() {
      if let FragmentContent::Text {
        text,
        box_id,
        source_range,
        ..
      } = &node.content
      {
        if let (Some(box_id), Some(source_range)) = (*box_id, *source_range) {
          if let Some(box_data) = text_boxes.get(&box_id) {
            let node_len = box_data.boundaries.len().saturating_sub(1);
            let start = char_idx_for_byte(&box_data.boundaries, source_range.start());
            let end = char_idx_for_byte(&box_data.boundaries, source_range.end());
            let (frag_start, frag_end) = if start <= end { (start, end) } else { (end, start) };
            let local_len = text.chars().count();

            entries.push(DocumentSelectionIndexEntry {
              path: path.clone().into_boxed_slice(),
              box_id,
              source_range,
              node_id: box_data.node_id,
              node_len,
              frag_start: frag_start.min(node_len),
              frag_end: frag_end.min(node_len),
              local_len,
            });
          }
        }
      }

      if matches!(
        node.content,
        FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
      ) {
        continue;
      }

      for (idx, child) in node.children.iter().enumerate().rev() {
        let mut child_path = path.clone();
        child_path.push(idx as u32);
        stack.push((child_path, child));
      }
    }

    DocumentSelectionIndexRoot { root_id, entries }
  }

  pub(crate) fn text_fragment_count(&self) -> usize {
    self.roots.iter().map(|root| root.entries.len()).sum()
  }
}

fn resolve_fragment_node_mut<'a>(
  fragment_tree: &'a mut FragmentTree,
  root_id: u32,
  path: &[u32],
) -> Option<&'a mut FragmentNode> {
  let mut node: &mut FragmentNode = if root_id == 0 {
    &mut fragment_tree.root
  } else {
    fragment_tree
      .additional_fragments
      .get_mut(root_id.saturating_sub(1) as usize)?
  };
  #[cfg(test)]
  record_selection_index_path_visit();

  for &idx in path {
    node = node.children_mut().get_mut(idx as usize)?;
    #[cfg(test)]
    record_selection_index_path_visit();
  }
  Some(node)
}

fn set_fragment_document_selection(
  fragment_tree: &mut FragmentTree,
  root_id: u32,
  path: &[u32],
  selection: Option<Arc<Vec<Range<usize>>>>,
) {
  if let Some(node) = resolve_fragment_node_mut(fragment_tree, root_id, path) {
    if let FragmentContent::Text {
      document_selection, ..
    } = &mut node.content
    {
      *document_selection = selection;
    }
  }
}

/// Apply the current document selection onto a fragment tree for paint-time highlighting, using a
/// precomputed [`DocumentSelectionIndex`] to avoid full fragment tree traversal.
pub(crate) fn apply_document_selection_to_fragment_tree_with_index(
  fragment_tree: &mut FragmentTree,
  index: &DocumentSelectionIndex,
  selection: Option<&DocumentSelectionState>,
) {
  match selection {
    None => {
      for root in &index.roots {
        for entry in &root.entries {
          set_fragment_document_selection(fragment_tree, root.root_id, &entry.path, None);
        }
      }
    }
    Some(DocumentSelectionState::All) => {
      for root in &index.roots {
        for entry in &root.entries {
          let selection = if entry.local_len > 0 {
            Some(Arc::new(vec![0..entry.local_len]))
          } else {
            None
          };
          set_fragment_document_selection(fragment_tree, root.root_id, &entry.path, selection);
        }
      }
    }
    Some(DocumentSelectionState::Ranges(ranges)) => {
      if ranges.ranges.is_empty() {
        for root in &index.roots {
          for entry in &root.entries {
            set_fragment_document_selection(fragment_tree, root.root_id, &entry.path, None);
          }
        }
        return;
      }

      let selection_ranges = ranges.ranges.as_slice();

      for root in &index.roots {
        let mut cursor = 0usize;
        for entry in &root.entries {
          let frag_start_point = DocumentSelectionPoint {
            node_id: entry.node_id,
            char_offset: entry.frag_start,
          };
          let frag_end_point = DocumentSelectionPoint {
            node_id: entry.node_id,
            char_offset: entry.frag_end,
          };

          while cursor < selection_ranges.len()
            && cmp_point(selection_ranges[cursor].end, frag_start_point) != Ordering::Greater
          {
            cursor += 1;
          }

          let mut local: Vec<Range<usize>> = Vec::new();
          let mut idx = cursor;
          while idx < selection_ranges.len()
            && cmp_point(selection_ranges[idx].start, frag_end_point) == Ordering::Less
          {
            let range = selection_ranges[idx].normalized();
            if entry.node_id < range.start.node_id || entry.node_id > range.end.node_id {
              idx += 1;
              continue;
            }

            let mut sel_start = 0usize;
            let mut sel_end = entry.node_len;
            if entry.node_id == range.start.node_id {
              sel_start = range.start.char_offset.min(entry.node_len);
            }
            if entry.node_id == range.end.node_id {
              sel_end = range.end.char_offset.min(entry.node_len);
            }

            if sel_start < sel_end {
              let start = sel_start.max(entry.frag_start);
              let end = sel_end.min(entry.frag_end);
              if start < end {
                let local_start = start.saturating_sub(entry.frag_start).min(entry.local_len);
                let local_end = end.saturating_sub(entry.frag_start).min(entry.local_len);
                if local_start < local_end {
                  local.push(local_start..local_end);
                }
              }
            }

            if cmp_point(range.end, frag_end_point) != Ordering::Greater {
              idx += 1;
            } else {
              break;
            }
          }

          let selection = if local.is_empty() {
            None
          } else {
            Some(Arc::new(local))
          };
          set_fragment_document_selection(fragment_tree, root.root_id, &entry.path, selection);
        }
      }
    }
  }
}

fn clear_fragment_selection(node: &mut FragmentNode) {
  if let FragmentContent::Text {
    document_selection, ..
  } = &mut node.content
  {
    *document_selection = None;
  }

  if matches!(
    node.content,
    FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
  ) {
    return;
  }

  for child in node.children_mut().iter_mut() {
    clear_fragment_selection(child);
  }
}

fn apply_all_selection(node: &mut FragmentNode, box_tree: &BoxTree) {
  if let FragmentContent::Text {
    text,
    box_id,
    document_selection,
    ..
  } = &mut node.content
  {
    let selectable = box_id
      .and_then(|id| box_node_by_id(box_tree, id))
      .is_some_and(box_is_selectable);
    if selectable {
      let len = text.chars().count();
      if len > 0 {
        *document_selection = Some(Arc::new(vec![0..len]));
      }
    }
  }

  if matches!(
    node.content,
    FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
  ) {
    return;
  }

  for child in node.children_mut().iter_mut() {
    apply_all_selection(child, box_tree);
  }
}

fn apply_ranges_selection(
  node: &mut FragmentNode,
  box_tree: &BoxTree,
  boundaries_cache: &mut FxHashMap<usize, Vec<usize>>,
  selection_ranges: &[DocumentSelectionRange],
  cursor: &mut usize,
) {
  if let FragmentContent::Text {
    text,
    box_id,
    source_range,
    document_selection,
    ..
  } = &mut node.content
  {
    if let (Some(box_id), Some(source_range)) = (*box_id, *source_range) {
      if let Some(span) = text_fragment_span(box_tree, boundaries_cache, box_id, source_range) {
        let frag_start_point = DocumentSelectionPoint {
          node_id: span.node_id,
          char_offset: span.frag_start,
        };
        let frag_end_point = DocumentSelectionPoint {
          node_id: span.node_id,
          char_offset: span.frag_end,
        };

        while *cursor < selection_ranges.len()
          && cmp_point(selection_ranges[*cursor].end, frag_start_point) != Ordering::Greater
        {
          *cursor += 1;
        }

        let mut local: Vec<Range<usize>> = Vec::new();
        let mut idx = *cursor;
        while idx < selection_ranges.len()
          && cmp_point(selection_ranges[idx].start, frag_end_point) == Ordering::Less
        {
          let range = selection_ranges[idx].normalized();
          if span.node_id < range.start.node_id || span.node_id > range.end.node_id {
            idx += 1;
            continue;
          }

          let mut sel_start = 0usize;
          let mut sel_end = span.node_len;
          if span.node_id == range.start.node_id {
            sel_start = range.start.char_offset.min(span.node_len);
          }
          if span.node_id == range.end.node_id {
            sel_end = range.end.char_offset.min(span.node_len);
          }
          if sel_start < sel_end {
            let start = sel_start.max(span.frag_start);
            let end = sel_end.min(span.frag_end);
            if start < end {
              let frag_len = text.chars().count();
              let local_start = start.saturating_sub(span.frag_start).min(frag_len);
              let local_end = end.saturating_sub(span.frag_start).min(frag_len);
              if local_start < local_end {
                local.push(local_start..local_end);
              }
            }
          }

          if cmp_point(range.end, frag_end_point) != Ordering::Greater {
            idx += 1;
          } else {
            break;
          }
        }

        if !local.is_empty() {
          *document_selection = Some(Arc::new(local));
        }
      }
    }
  }

  if matches!(
    node.content,
    FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
  ) {
    return;
  }

  for child in node.children_mut().iter_mut() {
    apply_ranges_selection(child, box_tree, boundaries_cache, selection_ranges, cursor);
  }
}

/// Apply the current document selection onto a fragment tree for paint-time highlighting.
pub(crate) fn apply_document_selection_to_fragment_tree(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  selection: Option<&DocumentSelectionState>,
) {
  clear_fragment_selection(&mut fragment_tree.root);
  for root in fragment_tree.additional_fragments.iter_mut() {
    clear_fragment_selection(root);
  }

  let Some(selection) = selection else {
    return;
  };

  match selection {
    DocumentSelectionState::All => {
      apply_all_selection(&mut fragment_tree.root, box_tree);
      for root in fragment_tree.additional_fragments.iter_mut() {
        apply_all_selection(root, box_tree);
      }
    }
    DocumentSelectionState::Ranges(ranges) => {
      if ranges.ranges.is_empty() {
        return;
      }

      let mut boundaries_cache: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
      let selection_ranges = ranges.ranges.as_slice();
      let mut cursor = 0usize;
      apply_ranges_selection(
        &mut fragment_tree.root,
        box_tree,
        &mut boundaries_cache,
        selection_ranges,
        &mut cursor,
      );

      for root in fragment_tree.additional_fragments.iter_mut() {
        cursor = 0;
        apply_ranges_selection(
          root,
          box_tree,
          &mut boundaries_cache,
          selection_ranges,
          &mut cursor,
        );
      }
    }
  }
}

/// Apply a `dom2`-stable document selection onto a fragment tree for paint-time highlighting.
///
/// This projects the selection into renderer preorder space using `mapping`, then reuses the legacy
/// selection application logic.
#[cfg(feature = "vmjs")]
pub(crate) fn apply_document_selection_to_fragment_tree_dom2(
  box_tree: &BoxTree,
  fragment_tree: &mut FragmentTree,
  mapping: &RendererDomMapping,
  selection: Option<&DocumentSelectionStateDom2>,
) {
  let projected = selection.map(|sel| sel.project_to_preorder(mapping));
  apply_document_selection_to_fragment_tree(box_tree, fragment_tree, projected.as_ref());
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::interaction::state::DocumentSelectionRanges;
  use crate::style::display::FormattingContextType;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::BoxNode;
  use crate::tree::fragment_tree::FragmentNode;

  fn build_deep_non_text_chain(depth: usize) -> FragmentNode {
    let mut node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
    for _ in 0..depth {
      node = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![node]);
    }
    node
  }

  #[test]
  fn selection_index_update_does_not_walk_unrelated_non_text_subtrees() {
    let style = Arc::new(ComputedStyle::default());

    let full_text = "abcdefghijklmnopqrstuvwxyz".repeat(50);
    let mut text_box = BoxNode::new_text(Arc::clone(&style), full_text.clone());
    text_box.styled_node_id = Some(1);
    let root_box = BoxNode::new_block(style, FormattingContextType::Block, vec![text_box]);
    let box_tree = BoxTree::new(root_box);

    let text_box_id = 2usize; // Pre-order id after `BoxTree::new` for the single text child.
    let chunk = 10usize;
    let text_fragments = 100usize;
    let mut children: Vec<FragmentNode> = Vec::new();

    // Large, deeply nested non-text subtree that contains no text fragments.
    children.push(build_deep_non_text_chain(5000));

    // Many text fragments as direct children of the root.
    for idx in 0..text_fragments {
      let start = idx * chunk;
      let end = start + chunk;
      let slice = &full_text[start..end];
      let mut node = FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
        slice.to_string(),
        0.0,
      );
      if let FragmentContent::Text {
        box_id,
        source_range,
        ..
      } = &mut node.content
      {
        *box_id = Some(text_box_id);
        *source_range = TextSourceRange::new(start..end);
      }
      children.push(node);
    }

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), children);
    let mut fragment_tree = FragmentTree::new(root);

    let index = DocumentSelectionIndex::build(&box_tree, &fragment_tree);
    assert_eq!(index.text_fragment_count(), text_fragments);

    let selection = DocumentSelectionState::Ranges(DocumentSelectionRanges {
      ranges: vec![DocumentSelectionRange {
        start: DocumentSelectionPoint {
          node_id: 1,
          char_offset: 0,
        },
        end: DocumentSelectionPoint {
          node_id: 1,
          char_offset: 10,
        },
      }],
      primary: 0,
      anchor: DocumentSelectionPoint {
        node_id: 1,
        char_offset: 0,
      },
      focus: DocumentSelectionPoint {
        node_id: 1,
        char_offset: 10,
      },
    });

    reset_selection_index_counters();
    apply_document_selection_to_fragment_tree_with_index(&mut fragment_tree, &index, Some(&selection));

    let visits = selection_index_path_visits();
    // Paths are `root -> child`, so visits scale with the number of text fragments and should be
    // independent of the deep non-text subtree size.
    assert!(
      visits <= index.text_fragment_count() * 6,
      "expected selection update to visit O(text_fragments) nodes, got {visits}"
    );
    assert!(
      visits < 5000,
      "expected selection update to avoid walking the deep non-text subtree (visits={visits})"
    );

    // Basic correctness sanity: ensure at least one fragment got a selection range.
    let mut found_selected = false;
    let mut stack = vec![&fragment_tree.root];
    while let Some(node) = stack.pop() {
      if let FragmentContent::Text {
        document_selection, ..
      } = &node.content
      {
        if document_selection.is_some() {
          found_selected = true;
          break;
        }
      }
      for child in node.children.iter() {
        stack.push(child);
      }
    }
    assert!(found_selected, "expected some text fragment to be selected");

    // Ensure we can clear selection via the index too.
    reset_selection_index_counters();
    apply_document_selection_to_fragment_tree_with_index(&mut fragment_tree, &index, None);
    assert!(selection_index_path_visits() < 5000);
    let mut stack = vec![&fragment_tree.root];
    while let Some(node) = stack.pop() {
      if let FragmentContent::Text {
        document_selection, ..
      } = &node.content
      {
        assert!(document_selection.is_none());
      }
      for child in node.children.iter() {
        stack.push(child);
      }
    }
  }

  #[test]
  fn selection_index_build_handles_empty_tree() {
    let style = Arc::new(ComputedStyle::default());
    let root_box = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
    let box_tree = BoxTree::new(root_box);
    let fragment_tree = FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
      vec![],
    ));
    let index = DocumentSelectionIndex::build(&box_tree, &fragment_tree);
    assert_eq!(index.text_fragment_count(), 0);
  }
}
