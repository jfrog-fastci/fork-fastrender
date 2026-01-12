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
  let (frag_start, frag_end) = if start <= end { (start, end) } else { (end, start) };

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

