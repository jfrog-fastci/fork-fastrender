use crate::dom::{DomNode, DomNodeType};
use crate::paint::display_list::{BlendMode, ClipShape, DisplayItem, DisplayList, TextItem};
use crate::style::cascade::StyledNode;
use crate::style::color::Rgba;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType, MarkerContent, ReplacedType};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use selectors::context::QuirksMode;
use serde::{Deserialize, Serialize};

/// Schema version for all snapshot types.
///
/// Bump this when the JSON structure changes in an incompatible way. Callers can
/// use this to gate downstream tooling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaVersion {
  V1,
}

/// Document quirks mode snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuirksModeSnapshot {
  NoQuirks,
  LimitedQuirks,
  Quirks,
}

impl Default for QuirksModeSnapshot {
  fn default() -> Self {
    QuirksModeSnapshot::NoQuirks
  }
}

impl From<QuirksMode> for QuirksModeSnapshot {
  fn from(mode: QuirksMode) -> Self {
    match mode {
      QuirksMode::Quirks => QuirksModeSnapshot::Quirks,
      QuirksMode::LimitedQuirks => QuirksModeSnapshot::LimitedQuirks,
      QuirksMode::NoQuirks => QuirksModeSnapshot::NoQuirks,
    }
  }
}

impl QuirksModeSnapshot {
  pub fn as_str(&self) -> &'static str {
    match self {
      QuirksModeSnapshot::NoQuirks => "no_quirks",
      QuirksModeSnapshot::LimitedQuirks => "limited_quirks",
      QuirksModeSnapshot::Quirks => "quirks",
    }
  }
}

impl SchemaVersion {
  fn current() -> Self {
    SchemaVersion::V1
  }

  /// Numeric major version for compatibility checks.
  pub fn major(&self) -> u32 {
    match self {
      SchemaVersion::V1 => 1,
    }
  }

  /// Human-readable label (matches the serialized value).
  pub fn label(&self) -> &'static str {
    match self {
      SchemaVersion::V1 => "v1",
    }
  }
}

/// Snapshot of the parsed DOM tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomSnapshot {
  pub schema_version: SchemaVersion,
  #[serde(default)]
  pub quirks_mode: QuirksModeSnapshot,
  pub root: DomNodeSnapshot,
}

/// Snapshot of a DOM node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomNodeSnapshot {
  pub node_id: usize,
  pub kind: DomNodeKindSnapshot,
  pub children: Vec<DomNodeSnapshot>,
}

/// Node kind specific data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomNodeKindSnapshot {
  Document,
  ShadowRoot {
    mode: String,
  },
  Slot {
    namespace: String,
    attributes: Vec<AttributeSnapshot>,
  },
  Element {
    tag_name: String,
    namespace: String,
    attributes: Vec<AttributeSnapshot>,
  },
  Text {
    content: String,
  },
}

/// Attribute key/value pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttributeSnapshot {
  pub name: String,
  pub value: String,
}

/// Snapshot of a live `dom2::Document` tree.
///
/// Unlike [`DomSnapshot`], this snapshot captures `dom2` node ids (stable indices into the document's
/// node arena) and explicit parent/child relationships so ordering/connectedness issues can be
/// debugged without re-rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dom2Snapshot {
  pub schema_version: SchemaVersion,
  /// Node id of the document root (typically `0`).
  pub root: usize,
  /// All nodes currently allocated in the `dom2::Document`, in `NodeId` index order.
  pub nodes: Vec<Dom2NodeSnapshot>,
}

/// Snapshot of an individual `dom2` node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dom2NodeSnapshot {
  /// `dom2::NodeId::index()`.
  pub node_id: usize,
  /// Parent node id, if known.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub parent: Option<usize>,
  /// Child node ids, in tree order.
  pub children: Vec<usize>,
  /// Whether this node's subtree is considered inert (currently used for `<template>` contents).
  pub inert_subtree: bool,
  /// Node kind specific data.
  pub kind: Dom2NodeKindSnapshot,
}

/// `dom2` node kind specific data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Dom2NodeKindSnapshot {
  Document {
    #[serde(default)]
    quirks_mode: QuirksModeSnapshot,
  },
  DocumentFragment,
  Comment {
    content: String,
  },
  ProcessingInstruction {
    target: String,
    data: String,
  },
  Doctype {
    name: String,
    public_id: String,
    system_id: String,
  },
  ShadowRoot {
    mode: String,
    delegates_focus: bool,
  },
  Slot {
    namespace: String,
    attributes: Vec<AttributeSnapshot>,
    assigned: bool,
  },
  Element {
    tag_name: String,
    namespace: String,
    attributes: Vec<AttributeSnapshot>,
  },
  Text {
    content: String,
  },
}

/// Snapshot of a styled DOM tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyledSnapshot {
  pub schema_version: SchemaVersion,
  pub root: StyledNodeSnapshot,
}

/// Snapshot of a single styled node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyledNodeSnapshot {
  pub node_id: usize,
  pub node: DomNodeSummary,
  pub style: StyleSnapshot,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub before: Option<StyleSnapshot>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub after: Option<StyleSnapshot>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub marker: Option<StyleSnapshot>,
  pub children: Vec<StyledNodeSnapshot>,
}

/// Minimal DOM summary for styled/debug snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomNodeSummary {
  Document,
  ShadowRoot {
    mode: String,
  },
  Element {
    tag_name: String,
    id: Option<String>,
    classes: Vec<String>,
  },
  Slot {
    name: Option<String>,
  },
  Text {
    content: String,
  },
}

/// Snapshot of a computed style with only the most debug-relevant properties.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleSnapshot {
  pub display: String,
  pub position: String,
  pub z_index: Option<i32>,
  pub visibility: String,
  pub opacity: f32,
  pub overflow_x: String,
  pub overflow_y: String,
  pub top: String,
  pub right: String,
  pub bottom: String,
  pub left: String,
  pub margin: EdgeSnapshot,
  pub margin_auto: EdgeAutoSnapshot,
  pub padding: EdgeSnapshot,
  pub border: EdgeSnapshot,
  pub background_color: ColorSnapshot,
  pub color: ColorSnapshot,
}

/// Snapshot of four-sided measurements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeSnapshot {
  pub top: String,
  pub right: String,
  pub bottom: String,
  pub left: String,
}

/// Auto flags for margins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeAutoSnapshot {
  pub top: bool,
  pub right: bool,
  pub bottom: bool,
  pub left: bool,
}

/// Snapshot of a box tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoxTreeSnapshot {
  pub schema_version: SchemaVersion,
  pub root: BoxNodeSnapshot,
}

/// Snapshot of a box node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoxNodeSnapshot {
  pub box_id: usize,
  pub kind: BoxKindSnapshot,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub styled_node_id: Option<usize>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub debug: Option<DebugInfoSnapshot>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub table_spans: Option<TableSpansSnapshot>,
  pub style: StyleSnapshot,
  pub children: Vec<BoxNodeSnapshot>,
}

/// Snapshot of debug info attached to a box.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DebugInfoSnapshot {
  pub selector: String,
}

/// Snapshot of table span metadata attached to a box.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableSpansSnapshot {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub colspan: Option<usize>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub rowspan: Option<usize>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub column_span: Option<usize>,
}

/// Box kind snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BoxKindSnapshot {
  Block { formatting_context: String },
  Inline { formatting_context: Option<String> },
  Text { text: String },
  Marker { text: Option<String> },
  Replaced { replaced: ReplacedSnapshot },
  Anonymous { kind: String },
}

/// Replaced element snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplacedSnapshot {
  Image { src: String, alt: Option<String> },
  Video { src: String },
  Audio { src: String },
  Canvas,
  Svg,
  Iframe { src: String, srcdoc: Option<String> },
  Embed { src: String },
  Object { data: String },
  Math,
  FormControl { control: String },
  Unknown,
}

/// Snapshot of a fragment tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FragmentTreeSnapshot {
  pub schema_version: SchemaVersion,
  pub viewport: RectSnapshot,
  pub roots: Vec<FragmentNodeSnapshot>,
}

/// Snapshot of a fragment node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FragmentNodeSnapshot {
  pub fragment_id: usize,
  pub bounds: RectSnapshot,
  pub scroll_overflow: RectSnapshot,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub baseline: Option<f32>,
  pub fragment_index: usize,
  pub fragment_count: usize,
  pub fragmentainer_index: usize,
  pub content: FragmentContentSnapshot,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub style: Option<StyleSnapshot>,
  pub children: Vec<FragmentNodeSnapshot>,
}

/// Fragment content details.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FragmentContentSnapshot {
  Block {
    box_id: Option<usize>,
  },
  Inline {
    box_id: Option<usize>,
    fragment_index: usize,
  },
  Text {
    text: String,
    box_id: Option<usize>,
    baseline_offset: f32,
    is_marker: bool,
  },
  Line {
    baseline: f32,
  },
  Replaced {
    box_id: Option<usize>,
    replaced: ReplacedSnapshot,
  },
}

/// Snapshot of a display list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayListSnapshot {
  pub schema_version: SchemaVersion,
  pub items: Vec<DisplayItemSnapshot>,
}

/// Snapshot of an individual display item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayItemSnapshot {
  pub item_id: usize,
  pub kind: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub bounds: Option<RectSnapshot>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<serde_json::Value>,
}

/// Snapshot of a rectangle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RectSnapshot {
  pub x: f32,
  pub y: f32,
  pub width: f32,
  pub height: f32,
}

/// Snapshot of a color.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColorSnapshot {
  pub r: u8,
  pub g: u8,
  pub b: u8,
  pub a: f32,
}

/// Combined pipeline snapshot for convenience.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PipelineSnapshot {
  pub schema_version: SchemaVersion,
  pub dom: DomSnapshot,
  pub styled: StyledSnapshot,
  pub box_tree: BoxTreeSnapshot,
  pub fragment_tree: FragmentTreeSnapshot,
  pub display_list: DisplayListSnapshot,
}

/// Capture a DOM snapshot with stable node IDs.
pub fn snapshot_dom(dom: &DomNode) -> DomSnapshot {
  let mut next = 1usize;
  DomSnapshot {
    schema_version: SchemaVersion::current(),
    quirks_mode: QuirksModeSnapshot::from(dom.document_quirks_mode()),
    root: snapshot_dom_node(dom, &mut next),
  }
}

/// Capture a composed DOM snapshot (shadow DOM + slot assignment applied) with stable node IDs.
pub fn snapshot_composed_dom(dom: &DomNode) -> crate::Result<DomSnapshot> {
  let composed = crate::dom::composed_dom_snapshot(dom)?;
  Ok(snapshot_dom(&composed))
}

#[cfg(feature = "vmjs")]
const DOM2_SNAPSHOT_TEXT_MAX_CHARS: usize = 200;

/// Capture a `dom2::Document` snapshot (including parent pointers and inert subtree flags).
#[cfg(feature = "vmjs")]
pub fn snapshot_dom2(doc: &crate::dom2::Document) -> Dom2Snapshot {
  let root = doc.root().index();
  let nodes = doc
    .nodes()
    .iter()
    .enumerate()
    .map(|(idx, node)| Dom2NodeSnapshot {
      node_id: idx,
      parent: node.parent.map(|id| id.index()),
      children: node.children.iter().map(|id| id.index()).collect(),
      inert_subtree: node.inert_subtree,
      kind: snapshot_dom2_kind(&node.kind),
    })
    .collect();

  Dom2Snapshot {
    schema_version: SchemaVersion::current(),
    root,
    nodes,
  }
}

/// Convenience helper: snapshot the renderer's immutable [`DomNode`] produced by a `dom2` document.
#[cfg(feature = "vmjs")]
pub fn snapshot_dom_from_dom2(doc: &crate::dom2::Document) -> DomSnapshot {
  let renderer_dom = doc.to_renderer_dom();
  snapshot_dom(&renderer_dom)
}

#[cfg(feature = "vmjs")]
fn snapshot_dom2_kind(kind: &crate::dom2::NodeKind) -> Dom2NodeKindSnapshot {
  match kind {
    crate::dom2::NodeKind::Document { quirks_mode } => Dom2NodeKindSnapshot::Document {
      quirks_mode: QuirksModeSnapshot::from(*quirks_mode),
    },
    crate::dom2::NodeKind::DocumentFragment => Dom2NodeKindSnapshot::DocumentFragment,
    crate::dom2::NodeKind::Comment { content } => Dom2NodeKindSnapshot::Comment {
      content: truncate_dom2_snapshot_text(content),
    },
    crate::dom2::NodeKind::ProcessingInstruction { target, data } => {
      Dom2NodeKindSnapshot::ProcessingInstruction {
        target: truncate_dom2_snapshot_text(target),
        data: truncate_dom2_snapshot_text(data),
      }
    }
    crate::dom2::NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => Dom2NodeKindSnapshot::Doctype {
      name: truncate_dom2_snapshot_text(name),
      public_id: truncate_dom2_snapshot_text(public_id),
      system_id: truncate_dom2_snapshot_text(system_id),
    },
    crate::dom2::NodeKind::ShadowRoot {
      mode,
      delegates_focus,
      ..
    } => Dom2NodeKindSnapshot::ShadowRoot {
      mode: format!("{mode:?}"),
      delegates_focus: *delegates_focus,
    },
    crate::dom2::NodeKind::Slot {
      namespace,
      attributes,
      assigned,
    } => Dom2NodeKindSnapshot::Slot {
      namespace: namespace.clone(),
      attributes: snapshot_dom2_attributes(attributes),
      assigned: *assigned,
    },
    crate::dom2::NodeKind::Element {
      tag_name,
      namespace,
      attributes,
      ..
    } => Dom2NodeKindSnapshot::Element {
      tag_name: tag_name.clone(),
      namespace: namespace.clone(),
      attributes: snapshot_dom2_attributes(attributes),
    },
    crate::dom2::NodeKind::Text { content } => Dom2NodeKindSnapshot::Text {
      content: truncate_dom2_snapshot_text(content),
    },
  }
}

#[cfg(feature = "vmjs")]
fn truncate_dom2_snapshot_text(text: &str) -> String {
  let mut chars = text.chars();
  let mut out = String::new();
  for _ in 0..DOM2_SNAPSHOT_TEXT_MAX_CHARS {
    let Some(ch) = chars.next() else {
      return out;
    };
    out.push(ch);
  }
  if chars.next().is_some() {
    out.push_str("…");
  }
  out
}

#[track_caller]
#[cfg(feature = "vmjs")]
pub fn assert_dom2_snapshot_invariants(snapshot: &Dom2Snapshot) {
  debug_assert!(
    snapshot.root < snapshot.nodes.len(),
    "dom2 snapshot root out of range: root={} nodes={}",
    snapshot.root,
    snapshot.nodes.len()
  );

  for (idx, node) in snapshot.nodes.iter().enumerate() {
    debug_assert_eq!(
      node.node_id, idx,
      "dom2 snapshot node_id mismatch: expected {idx}, got {}",
      node.node_id
    );

    if idx == snapshot.root {
      debug_assert!(
        node.parent.is_none(),
        "dom2 snapshot root must have no parent"
      );
    }

    if let Some(parent) = node.parent {
      debug_assert!(
        parent < snapshot.nodes.len(),
        "dom2 snapshot parent out of range: node_id={idx} parent={parent}"
      );
      debug_assert!(
        snapshot.nodes[parent].children.contains(&idx),
        "dom2 snapshot parent->children missing: node_id={idx} parent={parent}"
      );
    }

    for &child in &node.children {
      debug_assert!(
        child < snapshot.nodes.len(),
        "dom2 snapshot child out of range: node_id={idx} child={child}"
      );
      debug_assert_eq!(
        snapshot.nodes[child].parent,
        Some(idx),
        "dom2 snapshot child->parent mismatch: parent={idx} child={child}"
      );
    }
  }
}

#[track_caller]
#[cfg(feature = "vmjs")]
pub fn assert_dom2_snapshot_eq(actual: &Dom2Snapshot, expected: &Dom2Snapshot) {
  let actual_json = serde_json::to_string_pretty(actual)
    .unwrap_or_else(|err| format!("<failed to serialize actual Dom2Snapshot: {err}>"));
  let expected_json = serde_json::to_string_pretty(expected)
    .unwrap_or_else(|err| format!("<failed to serialize expected Dom2Snapshot: {err}>"));
  debug_assert_eq!(actual_json, expected_json);
}

fn snapshot_dom_node(node: &DomNode, next: &mut usize) -> DomNodeSnapshot {
  let node_id = *next;
  *next += 1;
  let kind = match &node.node_type {
    DomNodeType::Document { .. } => DomNodeKindSnapshot::Document,
    DomNodeType::ShadowRoot { mode, .. } => DomNodeKindSnapshot::ShadowRoot {
      mode: format!("{mode:?}"),
    },
    DomNodeType::Slot {
      namespace,
      attributes,
      ..
    } => DomNodeKindSnapshot::Slot {
      namespace: namespace.clone(),
      attributes: snapshot_attributes(attributes),
    },
    DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
    } => DomNodeKindSnapshot::Element {
      tag_name: tag_name.clone(),
      namespace: namespace.clone(),
      attributes: snapshot_attributes(attributes),
    },
    DomNodeType::Text { content } => DomNodeKindSnapshot::Text {
      content: content.clone(),
    },
  };
  DomNodeSnapshot {
    node_id,
    kind,
    children: node
      .children
      .iter()
      .map(|child| snapshot_dom_node(child, next))
      .collect(),
  }
}

fn snapshot_attributes(attributes: &[(String, String)]) -> Vec<AttributeSnapshot> {
  let mut attrs: Vec<_> = attributes
    .iter()
    .map(|(k, v)| AttributeSnapshot {
      name: k.clone(),
      value: v.clone(),
    })
    .collect();
  attrs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.value.cmp(&b.value)));
  attrs
}

#[cfg(feature = "vmjs")]
fn snapshot_dom2_attributes(attributes: &[crate::dom2::Attribute]) -> Vec<AttributeSnapshot> {
  let mut attrs: Vec<_> = attributes
    .iter()
    .map(|attr| AttributeSnapshot {
      name: attr.qualified_name().into_owned(),
      value: attr.value.clone(),
    })
    .collect();
  attrs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.value.cmp(&b.value)));
  attrs
}

/// Capture a styled tree snapshot.
pub fn snapshot_styled(styled: &StyledNode) -> StyledSnapshot {
  StyledSnapshot {
    schema_version: SchemaVersion::current(),
    root: snapshot_styled_node(styled),
  }
}

fn snapshot_styled_node(node: &StyledNode) -> StyledNodeSnapshot {
  StyledNodeSnapshot {
    node_id: node.node_id,
    node: summarize_dom_node(&node.node),
    style: snapshot_style(&node.styles),
    before: node.before_styles.as_deref().map(snapshot_style),
    after: node.after_styles.as_deref().map(snapshot_style),
    marker: node.marker_styles.as_deref().map(snapshot_style),
    children: node.children.iter().map(snapshot_styled_node).collect(),
  }
}

fn summarize_dom_node(node: &DomNode) -> DomNodeSummary {
  match &node.node_type {
    DomNodeType::Document { .. } => DomNodeSummary::Document,
    DomNodeType::ShadowRoot { mode, .. } => DomNodeSummary::ShadowRoot {
      mode: format!("{mode:?}"),
    },
    DomNodeType::Slot { .. } => DomNodeSummary::Slot {
      name: node.get_attribute("name"),
    },
    DomNodeType::Element { tag_name, .. } => {
      let classes = node
        .get_attribute_ref("class")
        .map(|c| {
          c.split_ascii_whitespace()
            .map(ToString::to_string)
            .collect()
        })
        .unwrap_or_default();
      DomNodeSummary::Element {
        tag_name: tag_name.clone(),
        id: node.get_attribute("id"),
        classes,
      }
    }
    DomNodeType::Text { content } => DomNodeSummary::Text {
      content: content.clone(),
    },
  }
}

fn snapshot_style(style: &ComputedStyle) -> StyleSnapshot {
  StyleSnapshot {
    display: style.display.to_string(),
    position: style.position.to_string(),
    z_index: style.z_index,
    visibility: format!("{:?}", style.visibility),
    opacity: style.opacity,
    overflow_x: format_overflow(style.overflow_x),
    overflow_y: format_overflow(style.overflow_y),
    top: format_inset_value(&style.top),
    right: format_inset_value(&style.right),
    bottom: format_inset_value(&style.bottom),
    left: format_inset_value(&style.left),
    margin: EdgeSnapshot {
      top: format_length_opt(style.margin_top.as_ref()),
      right: format_length_opt(style.margin_right.as_ref()),
      bottom: format_length_opt(style.margin_bottom.as_ref()),
      left: format_length_opt(style.margin_left.as_ref()),
    },
    margin_auto: EdgeAutoSnapshot {
      top: style.margin_top.is_none(),
      right: style.margin_right.is_none(),
      bottom: style.margin_bottom.is_none(),
      left: style.margin_left.is_none(),
    },
    padding: EdgeSnapshot {
      top: format_length(&style.padding_top),
      right: format_length(&style.padding_right),
      bottom: format_length(&style.padding_bottom),
      left: format_length(&style.padding_left),
    },
    border: EdgeSnapshot {
      top: format_length(&style.border_top_width),
      right: format_length(&style.border_right_width),
      bottom: format_length(&style.border_bottom_width),
      left: format_length(&style.border_left_width),
    },
    background_color: snapshot_color(style.background_color),
    color: snapshot_color(style.color),
  }
}

fn format_overflow(value: Overflow) -> String {
  format!("{:?}", value).to_ascii_lowercase()
}

fn format_length_opt(value: Option<&Length>) -> String {
  match value {
    Some(len) => format_length(len),
    None => "auto".to_string(),
  }
}

fn format_inset_value(value: &crate::style::types::InsetValue) -> String {
  match value {
    crate::style::types::InsetValue::Auto => "auto".to_string(),
    crate::style::types::InsetValue::Length(len) => format_length(len),
    crate::style::types::InsetValue::Anchor(anchor) => {
      let side = match anchor.side {
        crate::style::types::AnchorSide::Inside => "inside".to_string(),
        crate::style::types::AnchorSide::Outside => "outside".to_string(),
        crate::style::types::AnchorSide::Top => "top".to_string(),
        crate::style::types::AnchorSide::Right => "right".to_string(),
        crate::style::types::AnchorSide::Bottom => "bottom".to_string(),
        crate::style::types::AnchorSide::Left => "left".to_string(),
        crate::style::types::AnchorSide::Start => "start".to_string(),
        crate::style::types::AnchorSide::End => "end".to_string(),
        crate::style::types::AnchorSide::SelfStart => "self-start".to_string(),
        crate::style::types::AnchorSide::SelfEnd => "self-end".to_string(),
        crate::style::types::AnchorSide::InlineStart => "inline-start".to_string(),
        crate::style::types::AnchorSide::InlineEnd => "inline-end".to_string(),
        crate::style::types::AnchorSide::BlockStart => "block-start".to_string(),
        crate::style::types::AnchorSide::BlockEnd => "block-end".to_string(),
        crate::style::types::AnchorSide::Center => "center".to_string(),
        crate::style::types::AnchorSide::Percent(pct) => format!("{pct}%"),
      };
      let mut out = String::from("anchor(");
      if let Some(name) = &anchor.name {
        out.push_str(name);
        out.push(' ');
      }
      out.push_str(&side);
      if let Some(fallback) = anchor.fallback {
        out.push_str(", ");
        out.push_str(&format_length(&fallback));
      }
      out.push(')');
      out
    }
  }
}

fn format_length(length: &Length) -> String {
  format!("{:?}", length)
}

fn snapshot_color(color: Rgba) -> ColorSnapshot {
  ColorSnapshot {
    r: color.r,
    g: color.g,
    b: color.b,
    a: (color.a * 1_000_000.0).round() / 1_000_000.0,
  }
}

/// Capture a snapshot of a box tree.
pub fn snapshot_box_tree(tree: &BoxTree) -> BoxTreeSnapshot {
  BoxTreeSnapshot {
    schema_version: SchemaVersion::current(),
    root: snapshot_box_node(&tree.root),
  }
}

fn snapshot_box_node(node: &BoxNode) -> BoxNodeSnapshot {
  let colspan = node.table_colspan();
  let rowspan = node.table_rowspan();
  let column_span = node.table_column_span();
  let table_spans = (colspan > 1 || rowspan > 1 || column_span > 1).then_some(TableSpansSnapshot {
    colspan: (colspan > 1).then_some(colspan),
    rowspan: (rowspan > 1).then_some(rowspan),
    column_span: (column_span > 1).then_some(column_span),
  });

  BoxNodeSnapshot {
    box_id: node.id,
    kind: snapshot_box_kind(&node.box_type),
    styled_node_id: node.styled_node_id,
    debug: node.debug_info.as_ref().map(|d| DebugInfoSnapshot {
      selector: d.to_selector(),
    }),
    table_spans,
    style: snapshot_style(&node.style),
    children: {
      let mut children: Vec<BoxNodeSnapshot> =
        node.children.iter().map(snapshot_box_node).collect();
      if let Some(body) = node.footnote_body.as_deref() {
        children.push(snapshot_box_node(body));
      }
      children
    },
  }
}

fn snapshot_box_kind(kind: &BoxType) -> BoxKindSnapshot {
  match kind {
    BoxType::Block(block) => BoxKindSnapshot::Block {
      formatting_context: format!("{:?}", block.formatting_context),
    },
    BoxType::Inline(inline) => BoxKindSnapshot::Inline {
      formatting_context: inline.formatting_context.map(|fc| format!("{fc:?}")),
    },
    BoxType::LineBreak(_) => BoxKindSnapshot::Inline {
      formatting_context: None,
    },
    BoxType::Text(text) => BoxKindSnapshot::Text {
      text: text.text.clone(),
    },
    BoxType::Marker(marker) => BoxKindSnapshot::Marker {
      text: match &marker.content {
        MarkerContent::Text(t) => Some(t.clone()),
        MarkerContent::Image(_) => None,
      },
    },
    BoxType::Replaced(replaced) => BoxKindSnapshot::Replaced {
      replaced: snapshot_replaced(&replaced.replaced_type),
    },
    BoxType::Anonymous(anon) => BoxKindSnapshot::Anonymous {
      kind: format!("{:?}", anon.anonymous_type),
    },
  }
}

fn snapshot_replaced(replaced: &ReplacedType) -> ReplacedSnapshot {
  match replaced {
    ReplacedType::Image { src, alt, .. } => ReplacedSnapshot::Image {
      src: src.clone(),
      alt: alt.clone(),
    },
    ReplacedType::Video { src, .. } => ReplacedSnapshot::Video { src: src.clone() },
    ReplacedType::Audio { src, .. } => ReplacedSnapshot::Audio { src: src.clone() },
    ReplacedType::Canvas => ReplacedSnapshot::Canvas,
    ReplacedType::Svg { .. } => ReplacedSnapshot::Svg,
    ReplacedType::Iframe { src, srcdoc, .. } => ReplacedSnapshot::Iframe {
      src: src.clone(),
      srcdoc: srcdoc.clone(),
    },
    ReplacedType::Embed { src } => ReplacedSnapshot::Embed { src: src.clone() },
    ReplacedType::Object { data } => ReplacedSnapshot::Object { data: data.clone() },
    ReplacedType::Math(_) => ReplacedSnapshot::Math,
    ReplacedType::FormControl(control) => ReplacedSnapshot::FormControl {
      control: control.control.snapshot_label(),
    },
  }
}

/// Capture a fragment tree snapshot with stable fragment IDs.
pub fn snapshot_fragment_tree(tree: &FragmentTree) -> FragmentTreeSnapshot {
  let mut next = 1usize;
  let mut roots = Vec::with_capacity(1 + tree.additional_fragments.len());
  roots.push(snapshot_fragment_node(&tree.root, &mut next));
  for extra in &tree.additional_fragments {
    roots.push(snapshot_fragment_node(extra, &mut next));
  }
  FragmentTreeSnapshot {
    schema_version: SchemaVersion::current(),
    viewport: RectSnapshot {
      x: 0.0,
      y: 0.0,
      width: tree.viewport_size().width,
      height: tree.viewport_size().height,
    },
    roots,
  }
}

fn snapshot_fragment_node(node: &FragmentNode, next: &mut usize) -> FragmentNodeSnapshot {
  let fragment_id = *next;
  *next += 1;
  FragmentNodeSnapshot {
    fragment_id,
    bounds: snapshot_rect(node.bounds),
    scroll_overflow: snapshot_rect(node.scroll_overflow),
    baseline: node.baseline,
    fragment_index: node.fragment_index,
    fragment_count: node.fragment_count,
    fragmentainer_index: node.fragmentainer_index,
    content: snapshot_fragment_content(&node.content),
    style: node
      .style
      .as_ref()
      .map(|style| snapshot_style(style.as_ref())),
    children: node
      .children
      .iter()
      .map(|child| snapshot_fragment_node(child, next))
      .collect(),
  }
}

fn snapshot_fragment_content(content: &FragmentContent) -> FragmentContentSnapshot {
  match content {
    FragmentContent::Block { box_id } => FragmentContentSnapshot::Block { box_id: *box_id },
    FragmentContent::Inline {
      box_id,
      fragment_index,
    } => FragmentContentSnapshot::Inline {
      box_id: *box_id,
      fragment_index: *fragment_index,
    },
    FragmentContent::Text {
      text,
      box_id,
      baseline_offset,
      is_marker,
      ..
    } => FragmentContentSnapshot::Text {
      text: text.to_string(),
      box_id: *box_id,
      baseline_offset: *baseline_offset,
      is_marker: *is_marker,
    },
    FragmentContent::Line { baseline } => FragmentContentSnapshot::Line {
      baseline: *baseline,
    },
    FragmentContent::Replaced {
      box_id,
      replaced_type,
    } => FragmentContentSnapshot::Replaced {
      box_id: *box_id,
      replaced: snapshot_replaced(replaced_type),
    },
    FragmentContent::RunningAnchor { .. } => FragmentContentSnapshot::Block { box_id: None },
    FragmentContent::FootnoteAnchor { .. } => FragmentContentSnapshot::Block { box_id: None },
  }
}

fn snapshot_rect(rect: crate::geometry::Rect) -> RectSnapshot {
  RectSnapshot {
    x: rect.x(),
    y: rect.y(),
    width: rect.width(),
    height: rect.height(),
  }
}

/// Capture a display list snapshot.
pub fn snapshot_display_list(list: &DisplayList) -> DisplayListSnapshot {
  DisplayListSnapshot {
    schema_version: SchemaVersion::current(),
    items: list
      .items()
      .iter()
      .enumerate()
      .map(|(idx, item)| snapshot_display_item(idx + 1, item))
      .collect(),
  }
}

fn snapshot_display_item(item_id: usize, item: &DisplayItem) -> DisplayItemSnapshot {
  let bounds = item.bounds().map(snapshot_rect);
  let (kind, details) = match item {
    DisplayItem::FillRect(rect) => (
      "fill_rect".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(rect.rect),
        "color": snapshot_color(rect.color),
      })),
    ),
    DisplayItem::StrokeRect(rect) => (
      "stroke_rect".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(rect.rect),
        "color": snapshot_color(rect.color),
        "width": rect.width,
        "blend_mode": format!("{:?}", rect.blend_mode),
      })),
    ),
    DisplayItem::Outline(outline) => (
      "outline".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(outline.rect),
        "color": snapshot_color(outline.color),
        "width": outline.width,
        "style": format!("{:?}", outline.style),
        "offset": outline.offset,
        "invert": outline.invert,
      })),
    ),
    DisplayItem::FillRoundedRect(item) => (
      "fill_rounded_rect".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(item.rect),
        "color": snapshot_color(item.color),
        "radii": snapshot_radii(item.radii),
      })),
    ),
    DisplayItem::StrokeRoundedRect(item) => (
      "stroke_rounded_rect".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(item.rect),
        "color": snapshot_color(item.color),
        "width": item.width,
        "radii": snapshot_radii(item.radii),
      })),
    ),
    DisplayItem::Text(text) => ("text".to_string(), Some(snapshot_text_item(text))),
    DisplayItem::Image(image) => (
      "image".to_string(),
      Some(serde_json::json!({
        "dest_rect": snapshot_rect(image.dest_rect),
        "src_rect": image.src_rect.map(snapshot_rect),
        "filter_quality": format!("{:?}", image.filter_quality),
        "image": {
          "width": image.image.width,
          "height": image.image.height,
          "css_width": image.image.css_width,
          "css_height": image.image.css_height,
        }
      })),
    ),
    DisplayItem::RemoteFrameSlot(slot) => (
      "remote_frame_slot".to_string(),
      Some(serde_json::json!({
        "slot_index": slot.slot_index,
        "src": slot.src,
        "rect": snapshot_rect(slot.rect),
        "clip": slot.clip.as_ref().map(|clip| serde_json::json!({
          "rect": snapshot_rect(clip.rect),
          "radii": clip.radii.map(snapshot_radii),
        })),
      })),
    ),
    DisplayItem::ImagePattern(pattern) => (
      "image_pattern".to_string(),
      Some(serde_json::json!({
        "dest_rect": snapshot_rect(pattern.dest_rect),
        "tile_size": { "width": pattern.tile_size.width, "height": pattern.tile_size.height },
        "origin": { "x": pattern.origin.x, "y": pattern.origin.y },
        "repeat": format!("{:?}", pattern.repeat),
        "filter_quality": format!("{:?}", pattern.filter_quality),
        "image": {
          "width": pattern.image.width,
          "height": pattern.image.height,
          "css_width": pattern.image.css_width,
          "css_height": pattern.image.css_height,
        }
      })),
    ),
    DisplayItem::BoxShadow(shadow) => (
      "box_shadow".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(shadow.rect),
        "color": snapshot_color(shadow.color),
        "offset": { "x": shadow.offset.x, "y": shadow.offset.y },
        "radii": snapshot_radii(shadow.radii),
        "blur_radius": shadow.blur_radius,
        "spread_radius": shadow.spread_radius,
        "inset": shadow.inset,
      })),
    ),
    DisplayItem::ListMarker(marker) => (
      "list_marker".to_string(),
      Some(serde_json::json!({
        "origin": { "x": marker.origin.x, "y": marker.origin.y },
        "glyphs": marker.glyphs.len(),
        "font_size": marker.font_size,
        "advance_width": marker.advance_width,
        "color": snapshot_color(marker.color),
      })),
    ),
    DisplayItem::LinearGradient(grad) => (
      "linear_gradient".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(grad.rect),
        "start": { "x": grad.start.x, "y": grad.start.y },
        "end": { "x": grad.end.x, "y": grad.end.y },
        "spread": format!("{:?}", grad.spread),
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::LinearGradientPattern(grad) => (
      "linear_gradient_pattern".to_string(),
      Some(serde_json::json!({
        "dest_rect": snapshot_rect(grad.dest_rect),
        "tile_size": { "width": grad.tile_size.width, "height": grad.tile_size.height },
        "origin": { "x": grad.origin.x, "y": grad.origin.y },
        "start": { "x": grad.start.x, "y": grad.start.y },
        "end": { "x": grad.end.x, "y": grad.end.y },
        "spread": format!("{:?}", grad.spread),
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::RadialGradient(grad) => (
      "radial_gradient".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(grad.rect),
        "center": { "x": grad.center.x, "y": grad.center.y },
        "radii": { "x": grad.radii.x, "y": grad.radii.y },
        "spread": format!("{:?}", grad.spread),
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::RadialGradientPattern(grad) => (
      "radial_gradient_pattern".to_string(),
      Some(serde_json::json!({
        "dest_rect": snapshot_rect(grad.dest_rect),
        "tile_size": { "width": grad.tile_size.width, "height": grad.tile_size.height },
        "origin": { "x": grad.origin.x, "y": grad.origin.y },
        "center": { "x": grad.center.x, "y": grad.center.y },
        "radii": { "x": grad.radii.x, "y": grad.radii.y },
        "spread": format!("{:?}", grad.spread),
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::ConicGradient(grad) => (
      "conic_gradient".to_string(),
      Some(serde_json::json!({
        "rect": snapshot_rect(grad.rect),
        "center": { "x": grad.center.x, "y": grad.center.y },
        "from_angle": grad.from_angle,
        "repeating": grad.repeating,
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::ConicGradientPattern(grad) => (
      "conic_gradient_pattern".to_string(),
      Some(serde_json::json!({
        "dest_rect": snapshot_rect(grad.dest_rect),
        "tile_size": { "width": grad.tile_size.width, "height": grad.tile_size.height },
        "origin": { "x": grad.origin.x, "y": grad.origin.y },
        "center": { "x": grad.center.x, "y": grad.center.y },
        "from_angle": grad.from_angle,
        "repeating": grad.repeating,
        "stops": grad.stops.iter().map(|s| serde_json::json!({
          "position": s.position,
          "color": snapshot_color(s.color),
        })).collect::<Vec<_>>(),
      })),
    ),
    DisplayItem::Border(border) => ("border".to_string(), {
      let mut obj = serde_json::Map::new();
      obj.insert(
        "rect".to_string(),
        serde_json::json!(snapshot_rect(border.rect)),
      );
      obj.insert("top".to_string(), snapshot_border_side(&border.top));
      obj.insert("right".to_string(), snapshot_border_side(&border.right));
      obj.insert("bottom".to_string(), snapshot_border_side(&border.bottom));
      obj.insert("left".to_string(), snapshot_border_side(&border.left));
      obj.insert(
        "has_image".to_string(),
        serde_json::Value::Bool(border.image.is_some()),
      );
      obj.insert("radii".to_string(), snapshot_radii(border.radii));
      if let Some(gap) = border.gap {
        obj.insert(
          "gap".to_string(),
          serde_json::json!({
            "edge": format!("{:?}", gap.edge),
            "start": gap.start,
            "end": gap.end,
          }),
        );
      }
      Some(serde_json::Value::Object(obj))
    }),
    DisplayItem::TableCollapsedBorders(borders) => (
      "table_collapsed_borders".to_string(),
      Some(serde_json::json!({
        "rows": borders.borders.row_count,
        "columns": borders.borders.column_count,
      })),
    ),
    DisplayItem::TextDecoration(dec) => (
      "text_decoration".to_string(),
      Some(snapshot_text_decoration(dec)),
    ),
    DisplayItem::PushClip(clip) => ("push_clip".to_string(), Some(snapshot_clip(clip))),
    DisplayItem::PopClip => ("pop_clip".to_string(), None),
    DisplayItem::PushOpacity(opacity) => (
      "push_opacity".to_string(),
      Some(serde_json::json!({ "opacity": opacity.opacity })),
    ),
    DisplayItem::PopOpacity => ("pop_opacity".to_string(), None),
    DisplayItem::PushTransform(transform) => (
      "push_transform".to_string(),
      Some(snapshot_transform(transform)),
    ),
    DisplayItem::PopTransform => ("pop_transform".to_string(), None),
    DisplayItem::PushBlendMode(mode) => (
      "push_blend_mode".to_string(),
      Some(serde_json::json!({ "mode": snapshot_blend_mode(mode.mode) })),
    ),
    DisplayItem::PopBlendMode => ("pop_blend_mode".to_string(), None),
    DisplayItem::PushStackingContext(ctx) => (
      "push_stacking_context".to_string(),
      Some(snapshot_stacking_context(ctx)),
    ),
    DisplayItem::PopStackingContext => ("pop_stacking_context".to_string(), None),
    DisplayItem::PushBackfaceVisibility(visibility) => (
      "push_backface_visibility".to_string(),
      Some(serde_json::json!({
        "backface_visibility": format!("{:?}", visibility),
      })),
    ),
    DisplayItem::PopBackfaceVisibility => ("pop_backface_visibility".to_string(), None),
  };

  DisplayItemSnapshot {
    item_id,
    kind,
    bounds,
    details,
  }
}

fn snapshot_radii(radii: crate::paint::display_list::BorderRadii) -> serde_json::Value {
  fn snapshot_radius(radius: crate::paint::display_list::BorderRadius) -> serde_json::Value {
    serde_json::json!({ "x": radius.x, "y": radius.y })
  }

  serde_json::json!({
    "top_left": snapshot_radius(radii.top_left),
    "top_right": snapshot_radius(radii.top_right),
    "bottom_right": snapshot_radius(radii.bottom_right),
    "bottom_left": snapshot_radius(radii.bottom_left),
  })
}

fn snapshot_border_side(side: &crate::paint::display_list::BorderSide) -> serde_json::Value {
  serde_json::json!({
    "width": side.width,
    "style": format!("{:?}", side.style),
    "color": snapshot_color(side.color),
  })
}

fn snapshot_text_item(item: &TextItem) -> serde_json::Value {
  serde_json::json!({
    "origin": { "x": item.origin.x, "y": item.origin.y },
    "glyphs": item.glyphs.len(),
    "font_size": item.font_size,
    "advance_width": item.advance_width,
    "color": snapshot_color(item.color),
    "shadows": item.shadows.iter().map(|s| serde_json::json!({
      "offset": { "x": s.offset.x, "y": s.offset.y },
      "blur_radius": s.blur_radius,
      "color": snapshot_color(s.color),
    })).collect::<Vec<_>>(),
  })
}

fn snapshot_text_decoration(
  dec: &crate::paint::display_list::TextDecorationItem,
) -> serde_json::Value {
  serde_json::json!({
    "bounds": snapshot_rect(dec.bounds),
    "line_start": dec.line_start,
    "line_width": dec.line_width,
    "inline_vertical": dec.inline_vertical,
    "decorations": dec.decorations.iter().map(|d| serde_json::json!({
      "style": format!("{:?}", d.style),
      "color": snapshot_color(d.color),
      "underline": d.underline.as_ref().map(snapshot_decoration_stroke),
      "overline": d.overline.as_ref().map(snapshot_decoration_stroke),
      "line_through": d.line_through.as_ref().map(snapshot_decoration_stroke),
    })).collect::<Vec<_>>()
  })
}

fn snapshot_decoration_stroke(
  stroke: &crate::paint::display_list::DecorationStroke,
) -> serde_json::Value {
  serde_json::json!({
    "center": stroke.center,
    "thickness": stroke.thickness,
    "segments": stroke.segments,
  })
}

fn snapshot_clip(clip: &crate::paint::display_list::ClipItem) -> serde_json::Value {
  match &clip.shape {
    ClipShape::Rect { rect, radii } => serde_json::json!({
      "shape": "rect",
      "rect": snapshot_rect(*rect),
      "radii": radii.as_ref().map(|r| snapshot_radii(*r)),
    }),
    ClipShape::Path { path } => serde_json::json!({
      "shape": "path",
      "bounds": snapshot_rect(path.bounds()),
    }),
    ClipShape::Text { runs } => serde_json::json!({
      "shape": "text",
      "bounds": snapshot_rect(crate::paint::display_list::text_runs_bounds(runs.as_ref())),
      "runs": runs.len(),
    }),
    ClipShape::AlphaMask { rect, image } => serde_json::json!({
      "shape": "alpha_mask",
      "rect": snapshot_rect(*rect),
      "image": {
        "width": image.width,
        "height": image.height,
      }
    }),
  }
}

fn snapshot_transform(transform: &crate::paint::display_list::TransformItem) -> serde_json::Value {
  let approx_2d = transform.transform.to_2d();
  serde_json::json!({
    "matrix": transform.transform.m,
    "approx_2d": approx_2d.map(|t| serde_json::json!({
      "a": t.a,
      "b": t.b,
      "c": t.c,
      "d": t.d,
      "e": t.e,
      "f": t.f,
    }))
  })
}

fn snapshot_blend_mode(mode: BlendMode) -> serde_json::Value {
  serde_json::json!(format!("{:?}", mode))
}

fn snapshot_stacking_context(
  ctx: &crate::paint::display_list::StackingContextItem,
) -> serde_json::Value {
  serde_json::json!({
    "z_index": ctx.z_index,
    "creates_stacking_context": ctx.creates_stacking_context,
    "establishes_backdrop_root": ctx.establishes_backdrop_root,
    "bounds": snapshot_rect(ctx.bounds),
    "mix_blend_mode": snapshot_blend_mode(ctx.mix_blend_mode),
    "is_isolated": ctx.is_isolated,
    "transform": ctx.transform.as_ref().map(|t| snapshot_transform(&crate::paint::display_list::TransformItem { transform: *t })),
    "transform_style": format!("{:?}", ctx.transform_style),
    "backface_visibility": format!("{:?}", ctx.backface_visibility),
    "filters": ctx.filters.len(),
    "backdrop_filters": ctx.backdrop_filters.len(),
    "has_clip_path": ctx.has_clip_path,
    "has_mask": ctx.mask.is_some(),
    "radii": snapshot_radii(ctx.radii),
  })
}

/// Convenience helper that snapshots all core pipeline structures.
pub fn snapshot_pipeline(
  dom: &DomNode,
  styled: &StyledNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  display_list: &DisplayList,
) -> PipelineSnapshot {
  PipelineSnapshot {
    schema_version: SchemaVersion::current(),
    dom: snapshot_dom(dom),
    styled: snapshot_styled(styled),
    box_tree: snapshot_box_tree(box_tree),
    fragment_tree: snapshot_fragment_tree(fragment_tree),
    display_list: snapshot_display_list(display_list),
  }
}

#[cfg(test)]
mod dom2_snapshot_tests {
  use super::*;
  use crate::dom2::Document as Dom2Document;

  #[test]
  fn dom2_snapshot_captures_inert_template_and_parent_child_invariants() {
    let html = concat!(
      "<!doctype html>",
      "<html><body>",
      "<div id=host>",
      "<template shadowroot=open>",
      "<slot name=s></slot><span>shadow</span>",
      "</template>",
      "<p>light</p>",
      "</div>",
      "<template><span>inert</span></template>",
      "</body></html>"
    );
    let renderer_dom = crate::dom::parse_html(html).unwrap();
    let dom2 = Dom2Document::from_renderer_dom(&renderer_dom);

    let snapshot = snapshot_dom2(&dom2);
    assert_dom2_snapshot_invariants(&snapshot);

    let template = snapshot
      .nodes
      .iter()
      .find(|node| match &node.kind {
        Dom2NodeKindSnapshot::Element { tag_name, .. } => tag_name.eq_ignore_ascii_case("template"),
        _ => false,
      })
      .expect("expected inert <template> element in dom2 snapshot");
    assert!(
      template.inert_subtree,
      "expected <template> to set inert_subtree=true in snapshot"
    );
    assert!(
      !template.children.is_empty(),
      "expected <template> contents to remain in the tree"
    );

    assert!(
      snapshot
        .nodes
        .iter()
        .any(|node| matches!(node.kind, Dom2NodeKindSnapshot::ShadowRoot { .. })),
      "expected a ShadowRoot node in the dom2 snapshot"
    );
    assert!(
      snapshot
        .nodes
        .iter()
        .any(|node| matches!(node.kind, Dom2NodeKindSnapshot::Slot { .. })),
      "expected a Slot node in the dom2 snapshot"
    );
  }
}

#[cfg(test)]
mod snapshot_pipeline_tests {
  use super::*;
  use crate::css::types::StyleSheet;
  use crate::dom;
  use crate::geometry::Size;
  use crate::layout::engine::{LayoutConfig, LayoutEngine};
  use crate::paint::display_list_builder::DisplayListBuilder;
  use crate::style::cascade::apply_styles;
  use crate::style::display::{Display, FormattingContextType};
  use crate::text::font_db::FontConfig;
  use crate::text::font_loader::FontContext;
  use crate::tree::box_generation::generate_box_tree_with_anonymous_fixup;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use std::sync::Arc;

  #[test]
  fn pipeline_snapshot_matches_fixture() {
    const STACK_SIZE: usize = 64 * 1024 * 1024;
    let handle = std::thread::Builder::new()
      .name("debug-snapshot-test".to_string())
      .stack_size(STACK_SIZE)
      .spawn(|| {
        let html = r#"
    <!doctype html>
    <html>
      <body style="margin: 0">
        <div id="root" style="position: relative; z-index: 2; overflow: hidden; width: 120px; height: 60px; padding: 4px; margin: 8px; border: 2px solid rgb(10, 20, 30); background: rgb(200, 210, 220);">
          <span class="child" style="display: inline-block; position: absolute; left: 6px; top: 10px; padding: 2px; border: 1px dashed rgb(50, 60, 70); color: rgb(5, 6, 7);">Hi</span>
          <p style="margin: 2px 0 0 0;">Bye</p>
        </div>
      </body>
    </html>
  "#;

        let dom = dom::parse_html(html).expect("parse html");
        let stylesheet = StyleSheet::new();
        let styled = apply_styles(&dom, &stylesheet);
        let box_tree = generate_box_tree_with_anonymous_fixup(&styled).unwrap();

        let font_context = FontContext::with_config(
          FontConfig::new()
            .with_system_fonts(false)
            .with_bundled_fonts(true),
        );
        let engine = LayoutEngine::with_font_context(
          LayoutConfig::for_viewport(Size::new(200.0, 200.0)),
          font_context,
        );
        let fragment_tree = engine.layout_tree(&box_tree).expect("layout");

        let mut display_list =
          DisplayListBuilder::new().build_with_stacking_tree(&fragment_tree.root);
        for extra in &fragment_tree.additional_fragments {
          let extra_list = DisplayListBuilder::new().build_with_stacking_tree(extra);
          display_list.append(extra_list);
        }

        let snapshot = snapshot_pipeline(&dom, &styled, &box_tree, &fragment_tree, &display_list);
        let actual = serde_json::to_string_pretty(&snapshot).unwrap();
        let expected = include_str!(concat!(
          env!("CARGO_MANIFEST_DIR"),
          "/tests/fixtures/snapshots/basic.json"
        ));

        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
          let path = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/snapshots/basic.json"
          ));
          std::fs::create_dir_all(path.parent().unwrap()).unwrap();
          std::fs::write(&path, &actual).unwrap();
        }

        assert_eq!(actual, expected);
      })
      .expect("spawn snapshot test");

    match handle.join() {
      Ok(()) => {}
      Err(panic) => std::panic::resume_unwind(panic),
    }
  }

  #[test]
  fn dom_snapshot_records_no_quirks_for_doctype() {
    let dom = dom::parse_html("<!doctype html><html><body></body></html>").expect("parse html");
    let snapshot = snapshot_dom(&dom);
    assert_eq!(snapshot.quirks_mode, QuirksModeSnapshot::NoQuirks);
    let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
    assert_eq!(json["quirks_mode"], "no_quirks");
  }

  #[test]
  fn dom_snapshot_records_quirks_without_doctype() {
    let dom = dom::parse_html("<html><body></body></html>").expect("parse html");
    let snapshot = snapshot_dom(&dom);
    assert_eq!(snapshot.quirks_mode, QuirksModeSnapshot::Quirks);
    let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
    assert_eq!(json["quirks_mode"], "quirks");
  }

  #[test]
  fn box_tree_snapshot_includes_table_spans_from_metadata() {
    let mut cell_style = crate::ComputedStyle::default();
    cell_style.display = Display::TableCell;
    let cell = BoxNode::new_block(Arc::new(cell_style), FormattingContextType::Block, vec![])
      .with_table_cell_spans(2, 3);

    let mut col_style = crate::ComputedStyle::default();
    col_style.display = Display::TableColumn;
    let col = BoxNode::new_block(Arc::new(col_style), FormattingContextType::Block, vec![])
      .with_table_column_span(4);

    let root = BoxNode::new_block(
      Arc::new(crate::ComputedStyle::default()),
      FormattingContextType::Block,
      vec![cell, col],
    );
    let tree = BoxTree::new(root);

    let snapshot = snapshot_box_tree(&tree);
    let json = serde_json::to_value(&snapshot).expect("serialize snapshot");

    let children = json["root"]["children"].as_array().expect("children array");
    let cell_json = &children[0];
    assert_eq!(cell_json["table_spans"]["colspan"], 2);
    assert_eq!(cell_json["table_spans"]["rowspan"], 3);
    assert!(
      cell_json.get("debug").is_none(),
      "debug field should be omitted"
    );

    let col_json = &children[1];
    assert_eq!(col_json["table_spans"]["column_span"], 4);
  }

  #[test]
  fn box_tree_snapshot_includes_footnote_body() {
    let call = BoxNode::new_inline(Arc::new(crate::ComputedStyle::default()), vec![]);
    let mut call = call;
    call.footnote_body = Some(Box::new(BoxNode::new_block(
      Arc::new(crate::ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    )));
    let tree = BoxTree::new(call);

    let snapshot = snapshot_box_tree(&tree);
    let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
    let children = json["root"]["children"].as_array().expect("children array");
    assert_eq!(
      children.len(),
      1,
      "expected footnote_body to be included as a child in box tree snapshots"
    );
    assert_eq!(
      children[0]["box_id"], 2,
      "expected footnote body to have its own box id"
    );
  }
}
