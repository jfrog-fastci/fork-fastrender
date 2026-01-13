use crate::style::computed::Visibility;
use crate::style::display::Display;
use crate::style::types::UserSelect;
use crate::style::types::WhiteSpace;
use crate::style::types::WritingMode;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType, MarkerContent};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use rustc_hash::FxHashSet;
use std::sync::Arc;

/// A document text selection, used for serializing clipboard text.
///
/// This is intentionally a small, layout-oriented representation rather than a full DOM Range:
/// today we only need selection-for-copy, and our MVP selection engine tracks endpoints within
/// text nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentSelection {
  /// The entire rendered document (excluding non-selectable/hidden content).
  All,
  /// A selection spanning text nodes in DOM order.
  Range(DocumentSelectionRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentSelectionRange {
  pub start: DocumentSelectionPoint,
  pub end: DocumentSelectionPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentSelectionPoint {
  /// DOM pre-order id (matching `crate::dom::enumerate_dom_ids` / `BoxNode::styled_node_id`).
  pub node_id: usize,
  /// Character offset within the text node.
  pub char_offset: usize,
}

impl DocumentSelectionRange {
  pub fn normalized(mut self) -> Self {
    if self.start.node_id > self.end.node_id
      || (self.start.node_id == self.end.node_id && self.start.char_offset > self.end.char_offset)
    {
      std::mem::swap(&mut self.start, &mut self.end);
    }
    self
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastToken {
  CollapsibleSpace,
  PreservedSpace,
  StructuralTab,
  PreservedTab,
  StructuralNewline,
  PreservedNewline,
  Text,
}

struct TextBuilder {
  out: String,
  tokens: Vec<LastToken>,
}

impl TextBuilder {
  fn new() -> Self {
    Self {
      out: String::new(),
      tokens: Vec::new(),
    }
  }

  fn last(&self) -> Option<LastToken> {
    self.tokens.last().copied()
  }

  fn push_char(&mut self, ch: char, tok: LastToken) {
    self.out.push(ch);
    self.tokens.push(tok);
  }

  fn pop_char(&mut self) {
    let a = self.out.pop();
    let b = self.tokens.pop();
    debug_assert_eq!(a.is_some(), b.is_some());
  }

  fn trim_trailing_collapsible_spaces_and_tabs(&mut self) {
    while matches!(
      self.last(),
      Some(LastToken::CollapsibleSpace | LastToken::StructuralTab)
    ) {
      self.pop_char();
    }
  }

  fn push_space(&mut self) {
    // Avoid leading whitespace and collapse consecutive spaces.
    if self.out.is_empty()
      || matches!(
        self.last(),
        Some(
          LastToken::CollapsibleSpace
            | LastToken::PreservedSpace
            | LastToken::StructuralNewline
            | LastToken::PreservedNewline
            | LastToken::StructuralTab
            | LastToken::PreservedTab
        )
      )
    {
      return;
    }
    self.push_char(' ', LastToken::CollapsibleSpace);
  }

  fn push_tab(&mut self) {
    if matches!(self.last(), Some(LastToken::StructuralTab)) {
      return;
    }
    self.trim_trailing_collapsible_spaces_and_tabs();
    self.push_char('\t', LastToken::StructuralTab);
  }

  fn push_newline(&mut self) {
    if matches!(self.last(), Some(LastToken::StructuralNewline)) {
      return;
    }
    self.trim_trailing_collapsible_spaces_and_tabs();
    self.push_char('\n', LastToken::StructuralNewline);
  }

  fn push_text_collapsed(&mut self, text: &str) {
    for ch in text.chars() {
      if matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ') {
        self.push_space();
      } else {
        self.push_char(ch, LastToken::Text);
      }
    }
  }

  fn push_text_preserved(&mut self, text: &str) {
    for ch in text.chars() {
      match ch {
        '\u{0009}' => self.push_char('\t', LastToken::PreservedTab),
        '\u{000A}' | '\u{000C}' | '\u{000D}' => self.push_char('\n', LastToken::PreservedNewline),
        ' ' => self.push_char(' ', LastToken::PreservedSpace),
        _ => self.push_char(ch, LastToken::Text),
      }
    }
  }

  fn push_text(&mut self, text: &str, preserve_whitespace: bool) {
    if preserve_whitespace {
      self.push_text_preserved(text);
    } else {
      self.push_text_collapsed(text);
    }
  }

  fn finish(mut self) -> String {
    // Browsers generally avoid trailing newlines in clipboard text.
    //
    // Note: we intentionally avoid trimming preserved (preformatted) spaces/tabs, since browsers
    // keep them when selecting `white-space: pre|pre-wrap|break-spaces` content.
    self.trim_trailing_collapsible_spaces_and_tabs();
    while matches!(
      self.last(),
      Some(LastToken::StructuralNewline | LastToken::PreservedNewline)
    ) {
      self.pop_char();
    }
    self.trim_trailing_collapsible_spaces_and_tabs();
    self.out
  }
}

fn collect_fragment_box_ids(node: &FragmentNode, ids: &mut FxHashSet<usize>) {
  match &node.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. } => {
      if let Some(id) = *box_id {
        ids.insert(id);
      }
    }
    FragmentContent::Text { box_id, .. } => {
      if let Some(id) = *box_id {
        ids.insert(id);
      }
    }
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => {}
  }

  for child in node.children.iter() {
    collect_fragment_box_ids(child, ids);
  }
}

fn is_vertical_typographic_mode(mode: WritingMode) -> bool {
  matches!(mode, WritingMode::VerticalRl | WritingMode::VerticalLr)
}

/// Inline layout can coalesce adjacent text boxes (e.g. comment-split text nodes like `1<!-- -->2`)
/// into a single text run when `text-combine-upright` is active, so only one of the boxes appears
/// in the fragment tree.
///
/// When serializing selections, we use the fragment tree to determine which text nodes were
/// actually rendered (to avoid copying collapsed inter-element whitespace). Coalescing would cause
/// sibling text boxes to be treated as not rendered, dropping their text from the selection.
///
/// Expand `visible_box_ids` so that if any box in a coalesced group produced fragments, all boxes
/// in that group are considered visible for selection serialization.
fn expand_visible_box_ids_for_text_combine_upright_groups(
  node: &BoxNode,
  visible: &mut FxHashSet<usize>,
) {
  if node.children.is_empty() && node.footnote_body.is_none() {
    return;
  }

  let children = &node.children;
  let mut idx = 0usize;
  while idx < children.len() {
    let start = idx;
    let child = &children[idx];
    let eligible = matches!(child.box_type, BoxType::Text(_))
      && is_vertical_typographic_mode(child.style.writing_mode)
      && !matches!(
        child.style.text_combine_upright,
        crate::style::types::TextCombineUpright::None
      );
    if !eligible {
      idx += 1;
      continue;
    }

    let style_arc = child.style.clone();
    idx += 1;
    while idx < children.len() {
      let next = &children[idx];
      if !matches!(next.box_type, BoxType::Text(_)) {
        break;
      }
      if !Arc::ptr_eq(&style_arc, &next.style) && style_arc.as_ref() != next.style.as_ref() {
        break;
      }
      if !is_vertical_typographic_mode(next.style.writing_mode)
        || matches!(
          next.style.text_combine_upright,
          crate::style::types::TextCombineUpright::None
        )
      {
        break;
      }
      idx += 1;
    }

    let end = idx;
    if end - start > 1 {
      let any_visible = children[start..end].iter().any(|n| visible.contains(&n.id));
      if any_visible {
        for n in &children[start..end] {
          visible.insert(n.id);
        }
      }
    }
  }

  if let Some(body) = node.footnote_body.as_deref() {
    expand_visible_box_ids_for_text_combine_upright_groups(body, visible);
  }
  for child in node.children.iter() {
    expand_visible_box_ids_for_text_combine_upright_groups(child, visible);
  }
}

fn byte_offset_for_char_idx(text: &str, char_idx: usize) -> usize {
  if char_idx == 0 {
    return 0;
  }
  let mut count = 0usize;
  for (byte_idx, _) in text.char_indices() {
    if count == char_idx {
      return byte_idx;
    }
    count += 1;
  }
  text.len()
}

fn slice_text_by_selection(
  text: &str,
  node_id: Option<usize>,
  selection: DocumentSelection,
) -> Option<&str> {
  let DocumentSelection::Range(range) = selection else {
    return Some(text);
  };
  let range = range.normalized();
  let node_id = node_id?;

  if node_id < range.start.node_id || node_id > range.end.node_id {
    return None;
  }

  let len = text.chars().count();
  let start = if node_id == range.start.node_id {
    range.start.char_offset.min(len)
  } else {
    0
  };
  let end = if node_id == range.end.node_id {
    range.end.char_offset.min(len)
  } else {
    len
  };

  if start >= end {
    return None;
  }

  let start_byte = byte_offset_for_char_idx(text, start);
  let end_byte = byte_offset_for_char_idx(text, end);
  if start_byte >= end_byte {
    return None;
  }
  Some(&text[start_byte..end_byte])
}

#[derive(Default)]
struct TableRowCtx {
  cell_index: usize,
}

struct WalkCtx {
  row_stack: Vec<TableRowCtx>,
}

impl WalkCtx {
  fn new() -> Self {
    Self {
      row_stack: Vec::new(),
    }
  }

  fn current_row_mut(&mut self) -> Option<&mut TableRowCtx> {
    self.row_stack.last_mut()
  }
}

fn box_is_selectable(node: &BoxNode) -> bool {
  // `display:none` nodes should not exist in the box tree, but keep this robust.
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

fn before_enter_box(builder: &mut TextBuilder, ctx: &mut WalkCtx, node: &BoxNode) {
  let display = node.style.display;

  match display {
    Display::TableRow => {
      // Newline between rows.
      if !builder.out.is_empty() {
        builder.push_newline();
      }
      ctx.row_stack.push(TableRowCtx::default());
      return;
    }
    Display::TableCell => {
      if let Some(row) = ctx.current_row_mut() {
        if row.cell_index > 0 {
          builder.push_tab();
        }
        row.cell_index += 1;
      }
      return;
    }
    _ => {}
  }

  // Block-level boundaries: browsers typically separate blocks with line breaks when copying.
  //
  // Avoid inserting a newline directly after a table-cell tab separator.
  if display.is_block_level()
    && !builder.out.is_empty()
    && !matches!(
      builder.last(),
      Some(LastToken::StructuralNewline | LastToken::StructuralTab)
    )
  {
    builder.push_newline();
  }
}

fn after_exit_box(ctx: &mut WalkCtx, node: &BoxNode) {
  if matches!(node.style.display, Display::TableRow) {
    ctx.row_stack.pop();
  }
}

fn walk_box_tree(
  node: &BoxNode,
  selection: DocumentSelection,
  visible_box_ids: &FxHashSet<usize>,
  ctx: &mut WalkCtx,
  builder: &mut TextBuilder,
) {
  if !box_is_selectable(node) {
    return;
  }

  before_enter_box(builder, ctx, node);

  match &node.box_type {
    BoxType::Text(text_box) => {
      // Use the fragment tree to ensure we only serialize text that actually produced layout.
      if visible_box_ids.contains(&node.id) {
        if let Some(text) = slice_text_by_selection(&text_box.text, node.styled_node_id, selection)
        {
          let preserve_whitespace = matches!(
            node.style.white_space,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::BreakSpaces
          );
          builder.push_text(text, preserve_whitespace);
        }
      }
    }
    BoxType::Marker(marker) => {
      // List markers are rendered as text (e.g. bullets, numbers). Include them when selecting the
      // document, but treat them as not sliceable since the selection endpoints track DOM text
      // nodes, not generated markers.
      if let MarkerContent::Text(text) = &marker.content {
        builder.push_text(text, false);
      }
    }
    BoxType::LineBreak(_) => {
      builder.push_newline();
    }
    _ => {}
  }

  if let Some(body) = node.footnote_body.as_deref() {
    walk_box_tree(body, selection, visible_box_ids, ctx, builder);
  }
  for child in node.children.iter() {
    walk_box_tree(child, selection, visible_box_ids, ctx, builder);
  }

  after_exit_box(ctx, node);
}

/// Serialize a document selection into plain text suitable for the clipboard.
///
/// This is a best-effort approximation of browser selection serialization:
/// - Uses layout artifacts (fragment tree + computed display/visibility/user-select/inert).
/// - Inserts newlines between block-level elements and `<br>`.
/// - Inserts `\t` between table cells and `\n` between table rows.
/// - Collapses runs of ASCII whitespace to single spaces unless the text box has
///   `white-space: pre|pre-wrap|break-spaces`.
pub fn serialize_document_selection(
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  selection: DocumentSelection,
) -> String {
  let mut visible_box_ids: FxHashSet<usize> = FxHashSet::default();
  collect_fragment_box_ids(&fragment_tree.root, &mut visible_box_ids);
  for root in &fragment_tree.additional_fragments {
    collect_fragment_box_ids(root, &mut visible_box_ids);
  }
  expand_visible_box_ids_for_text_combine_upright_groups(&box_tree.root, &mut visible_box_ids);

  let mut builder = TextBuilder::new();
  let mut ctx = WalkCtx::new();
  walk_box_tree(
    &box_tree.root,
    selection,
    &visible_box_ids,
    &mut ctx,
    &mut builder,
  );
  builder.finish()
}

#[cfg(test)]
  mod tests {
  use super::*;
  use crate::style::display::FormattingContextType;
  use crate::style::types::TextCombineUpright;
  use crate::style::ComputedStyle;
  use crate::tree::fragment_tree::{TextEmphasisOffset, TextSourceRange};
  use crate::Rect;
  use std::sync::Arc;

  #[test]
  fn selection_serialization_treats_text_combine_merged_text_nodes_as_visible() {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalRl;
    style.text_combine_upright = TextCombineUpright::Digits(2);
    let style = Arc::new(style);

    let span = BoxNode::new_inline(
      style.clone(),
      vec![
        BoxNode::new_text(style.clone(), "1".into()),
        BoxNode::new_text(style.clone(), "2".into()),
      ],
    );
    let root = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![span]);
    let box_tree = BoxTree::new(root);

    let span = &box_tree.root.children[0];
    let text1_id = span.children[0].id;
    let _text2_id = span.children[1].id;

    // Model the fragment output produced by inline layout when comment-split text nodes are
    // coalesced for `text-combine-upright`: a single text fragment with the first node's box id.
    let text_fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      FragmentContent::Text {
        text: Arc::from("12"),
        box_id: Some(text1_id),
        source_range: TextSourceRange::new(0..2),
        baseline_offset: 0.0,
        shaped: None,
        is_marker: false,
        emphasis_offset: TextEmphasisOffset::default(),
        document_selection: None,
      },
      vec![],
      style.clone(),
    );
    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      0.0,
      vec![text_fragment],
    );
    let fragment_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![line]);
    let fragment_tree = FragmentTree::new(fragment_root);

    let serialized =
      serialize_document_selection(&box_tree, &fragment_tree, DocumentSelection::All);
    assert_eq!(serialized, "12");
  }
}
