use crate::style::computed::Visibility;
use crate::style::display::Display;
use crate::style::types::UserSelect;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType, MarkerContent};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use rustc_hash::FxHashSet;

/// A document text selection, used for serializing clipboard text.
///
/// This is intentionally a small, layout-oriented representation rather than a full DOM Range:
/// today we only need selection-for-copy, and our MVP selection engine tracks endpoints within
/// text nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentSelection {
  /// The entire rendered document (excluding non-selectable/hidden content).
  All,
  /// A selection spanning text nodes in DOM order.
  Range(DocumentSelectionRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentSelectionRange {
  pub start: DocumentSelectionPoint,
  pub end: DocumentSelectionPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
  None,
  Space,
  Tab,
  Newline,
  Text,
}

struct TextBuilder {
  out: String,
  last: LastToken,
}

impl TextBuilder {
  fn new() -> Self {
    Self {
      out: String::new(),
      last: LastToken::None,
    }
  }

  fn trim_trailing_spaces_and_tabs(&mut self) {
    while matches!(self.last, LastToken::Space | LastToken::Tab) {
      self.out.pop();
      self.last = self
        .out
        .chars()
        .last()
        .map(|c| match c {
          '\n' => LastToken::Newline,
          '\t' => LastToken::Tab,
          ' ' => LastToken::Space,
          _ => LastToken::Text,
        })
        .unwrap_or(LastToken::None);
    }
  }

  fn push_space(&mut self) {
    // Avoid leading whitespace and collapse consecutive spaces.
    if self.out.is_empty() || matches!(self.last, LastToken::Space | LastToken::Newline | LastToken::Tab) {
      return;
    }
    self.out.push(' ');
    self.last = LastToken::Space;
  }

  fn push_tab(&mut self) {
    if matches!(self.last, LastToken::Tab) {
      return;
    }
    self.trim_trailing_spaces_and_tabs();
    self.out.push('\t');
    self.last = LastToken::Tab;
  }

  fn push_newline(&mut self) {
    if matches!(self.last, LastToken::Newline) {
      return;
    }
    self.trim_trailing_spaces_and_tabs();
    self.out.push('\n');
    self.last = LastToken::Newline;
  }

  fn push_text(&mut self, text: &str) {
    for ch in text.chars() {
      if matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ') {
        self.push_space();
      } else {
        self.out.push(ch);
        self.last = LastToken::Text;
      }
    }
  }

  fn finish(mut self) -> String {
    // Browsers generally avoid trailing whitespace in clipboard text.
    self.trim_trailing_spaces_and_tabs();
    while matches!(self.last, LastToken::Newline) {
      self.out.pop();
      self.last = self
        .out
        .chars()
        .last()
        .map(|c| match c {
          '\n' => LastToken::Newline,
          '\t' => LastToken::Tab,
          ' ' => LastToken::Space,
          _ => LastToken::Text,
        })
        .unwrap_or(LastToken::None);
    }
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
    FragmentContent::Line { .. } | FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. } => {}
  }

  for child in node.children.iter() {
    collect_fragment_box_ids(child, ids);
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

fn slice_text_by_selection(text: &str, node_id: Option<usize>, selection: DocumentSelection) -> Option<&str> {
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
    Self { row_stack: Vec::new() }
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
  if display.is_block_level() && !builder.out.is_empty() && !matches!(builder.last, LastToken::Newline | LastToken::Tab) {
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
        if let Some(text) = slice_text_by_selection(&text_box.text, node.styled_node_id, selection) {
          builder.push_text(text);
        }
      }
    }
    BoxType::Marker(marker) => {
      // List markers are rendered as text (e.g. bullets, numbers). Include them when selecting the
      // document, but treat them as not sliceable since the selection endpoints track DOM text
      // nodes, not generated markers.
      if let MarkerContent::Text(text) = &marker.content {
        builder.push_text(text);
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
/// - Collapses runs of ASCII whitespace to single spaces.
pub fn serialize_document_selection(box_tree: &BoxTree, fragment_tree: &FragmentTree, selection: DocumentSelection) -> String {
  let mut visible_box_ids: FxHashSet<usize> = FxHashSet::default();
  collect_fragment_box_ids(&fragment_tree.root, &mut visible_box_ids);
  for root in &fragment_tree.additional_fragments {
    collect_fragment_box_ids(root, &mut visible_box_ids);
  }

  let mut builder = TextBuilder::new();
  let mut ctx = WalkCtx::new();
  walk_box_tree(&box_tree.root, selection, &visible_box_ids, &mut ctx, &mut builder);
  builder.finish()
}
