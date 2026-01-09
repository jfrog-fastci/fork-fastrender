//! Box generation - transforms styled DOM into BoxTree
//!
//! Implements the CSS box generation algorithm that determines what boxes
//! are created from DOM elements.
//!
//! CSS Specification: CSS 2.1 Section 9.2 - Box Generation
//! <https://www.w3.org/TR/CSS21/visuren.html#box-gen>

use crate::compat::CompatProfile;
use crate::debug::runtime;
use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::dom::ElementRef;
use crate::dom::HTML_NAMESPACE;
use crate::dom::SVG_NAMESPACE;
use crate::error::{RenderStage, Result};
use crate::geometry::Size;
use crate::html::image_attrs;
use crate::html::images::is_supported_image_mime;
use crate::render_control::check_active_periodic;
use crate::style::color::Rgba;
use crate::style::computed::Visibility;
use crate::style::content::ContentContext;
use crate::style::content::ContentItem;
use crate::style::content::ContentValue;
use crate::style::content::CounterStyle;
use crate::style::counter_styles::CounterStyleName;
use crate::style::counters::CounterManager;
use crate::style::counters::CounterSet;
use crate::style::defaults::parse_color_attribute;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::float::Float;
use crate::style::media::MediaQuery;
use crate::style::position::Position;
use crate::style::types::Direction;
use crate::style::types::FontStyle;
use crate::style::types::InsetValue;
use crate::style::types::ListStyleType;
use crate::style::types::SymbolsCounterStyle;
use crate::style::types::SymbolsType;
use crate::style::types::TextTransform;
use crate::style::types::WhiteSpace;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::css::types::TranslateValue;
use crate::svg::parse_svg_length;
use crate::svg::parse_svg_length_px;
use crate::svg::parse_svg_view_box;
use crate::svg::svg_intrinsic_dimensions_from_attributes;
use crate::svg::SvgLength;
use crate::tree::anonymous::AnonymousBoxCreator;
use crate::tree::anonymous::inherited_style;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::resource::ReferrerPolicy;
use crate::tree::box_tree::ForeignObjectInfo;
use crate::tree::box_tree::FormControl;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::GeneratedPseudoElement;
use crate::tree::box_tree::MarkerContent;
use crate::tree::box_tree::MathReplaced;
use crate::tree::box_tree::PictureSource;
use crate::tree::box_tree::ReplacedBox;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectControl;
use crate::tree::box_tree::SelectItem;
use crate::tree::box_tree::SizesList;
use crate::tree::box_tree::SrcsetCandidate;
use crate::tree::box_tree::SvgContent;
use crate::tree::box_tree::SvgDocumentCssInjection;
use crate::tree::box_tree::TableCellSpan;
use crate::tree::box_tree::TextControlKind;
use crate::tree::debug::DebugInfo;
use crate::tree::table_fixup::TableStructureFixer;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

#[cfg(any(test, feature = "box_generation_demo"))]
pub use crate::tree::box_generation_demo::{
  BoxGenerationConfig, BoxGenerationError, BoxGenerator, DOMNode,
};
pub(crate) fn parse_srcset(attr: &str) -> Vec<SrcsetCandidate> {
  image_attrs::parse_srcset(attr)
}

pub(crate) fn parse_sizes(attr: &str) -> Option<SizesList> {
  image_attrs::parse_sizes(attr)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn trim_ascii_whitespace_end(value: &str) -> &str {
  value.trim_end_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

// ============================================================================
// StyledNode-based Box Generation (for real DOM/style pipeline)
// ============================================================================

use crate::style::cascade::StyledNode;

/// Options that control how the box tree is generated from styled DOM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxGenerationOptions {
  /// Compatibility profile controlling whether site-specific heuristics are enabled.
  pub compat_profile: CompatProfile,
  /// Whether `float: footnote` should be interpreted as a paged-media footnote.
  ///
  /// Footnote floats require a pagination pass to build per-page footnote areas.
  /// When pagination is disabled, treating `float: footnote` as a real footnote
  /// would detach its contents from the normal flow and drop it from the output.
  pub enable_footnote_floats: bool,

  /// Whether the document was parsed with "scripting enabled" HTML semantics.
  ///
  /// When enabled, elements that represent nothing in JS-enabled environments (notably
  /// `<noscript>`) are suppressed from box generation so fallback markup does not appear in the
  /// rendered output.
  pub dom_scripting_enabled: bool,
}

impl Default for BoxGenerationOptions {
  fn default() -> Self {
    Self {
      compat_profile: CompatProfile::Standards,
      enable_footnote_floats: false,
      dom_scripting_enabled: false,
    }
  }
}

impl BoxGenerationOptions {
  /// Creates a new options struct with defaults.
  pub fn new() -> Self {
    Self::default()
  }

  /// Sets the compatibility profile for box generation.
  pub fn with_compat_profile(mut self, profile: CompatProfile) -> Self {
    self.compat_profile = profile;
    self
  }

  /// Enables or disables `float: footnote` handling.
  pub fn with_footnote_floats(mut self, enabled: bool) -> Self {
    self.enable_footnote_floats = enabled;
    self
  }

  /// Enables or disables JS-enabled HTML parsing semantics for box generation.
  pub fn with_dom_scripting_enabled(mut self, enabled: bool) -> Self {
    self.dom_scripting_enabled = enabled;
    self
  }

  fn site_compat_hacks_enabled(&self) -> bool {
    self.compat_profile.site_compat_hacks_enabled()
  }
}

const BOX_GEN_DEADLINE_STRIDE: usize = 256;
const MAX_EMBEDDED_SVG_CSS_BYTES: usize = 64 * 1024;

#[derive(Debug, Default, Clone, Copy)]
struct SvgSerializationProfile {
  calls: usize,
  bytes: usize,
  time_ms: f64,
}

thread_local! {
  static SVG_SERIALIZATION_PROFILE: RefCell<Option<SvgSerializationProfile>> = RefCell::new(None);
}

fn svg_serialization_profile_enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_SVG_PROFILE")
}

fn enable_svg_serialization_profile() {
  SVG_SERIALIZATION_PROFILE.with(|cell| {
    *cell.borrow_mut() = Some(SvgSerializationProfile::default());
  });
}

fn take_svg_serialization_profile() -> Option<SvgSerializationProfile> {
  SVG_SERIALIZATION_PROFILE.with(|cell| cell.borrow_mut().take())
}

fn record_svg_serialization(duration: std::time::Duration, bytes: usize) {
  SVG_SERIALIZATION_PROFILE.with(|cell| {
    if let Some(profile) = cell.borrow_mut().as_mut() {
      profile.calls += 1;
      profile.bytes += bytes;
      profile.time_ms += duration.as_secs_f64() * 1000.0;
    }
  });
}

#[derive(Debug, Clone)]
struct SvgDocumentCssPolicy {
  embedded_style_element: Option<Arc<str>>,
  max_embedded_svgs: Option<usize>,
  replaced_svg_count: usize,
  forced: Option<bool>,
}

fn svg_embed_document_css_override() -> Option<bool> {
  let toggles = runtime::runtime_toggles();
  let raw = toggles.get("FASTR_SVG_EMBED_DOCUMENT_CSS")?;
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let lower = trimmed.to_ascii_lowercase();
  Some(!matches!(lower.as_str(), "0" | "false" | "off"))
}

fn svg_embed_document_css_max_svgs() -> Option<usize> {
  runtime::runtime_toggles().usize("FASTR_SVG_EMBED_DOCUMENT_CSS_MAX_SVGS")
}

fn build_svg_cdata_style_element(css: &str) -> String {
  // Ensure embedded document CSS stays XML-safe by wrapping it in CDATA and splitting any
  // terminators that would otherwise close the section.
  let mut out = String::with_capacity(css.len() + 32);
  out.push_str("<style><![CDATA[");
  let mut last = 0;
  for (idx, _) in css.match_indices("]]>") {
    out.push_str(&css[last..idx]);
    out.push_str("]]]]><![CDATA[>");
    last = idx + 3;
  }
  out.push_str(&css[last..]);
  out.push_str("]]></style>");
  out
}

fn clone_starting_style(style: &Option<Arc<ComputedStyle>>) -> Option<Arc<ComputedStyle>> {
  style.as_ref().map(Arc::clone)
}

struct BoxGenerationPrepass<'a> {
  document_css: String,
  svg_document_css: SvgDocumentCssPolicy,
  picture_sources: PictureSourceLookup,
  styled_lookup: StyledLookup<'a>,
}

struct StyledLookup<'a> {
  nodes: Vec<Option<&'a StyledNode>>,
}

impl<'a> StyledLookup<'a> {
  fn new() -> Self {
    Self { nodes: vec![None] }
  }

  fn insert(&mut self, node_id: usize, node: &'a StyledNode) {
    if node_id == self.nodes.len() {
      self.nodes.push(Some(node));
      return;
    }

    if node_id >= self.nodes.len() {
      self.nodes.resize(node_id + 1, None);
    }
    self.nodes[node_id] = Some(node);
  }

  fn get(&self, node_id: usize) -> Option<&'a StyledNode> {
    self.nodes.get(node_id).copied().flatten()
  }
}

struct PictureSourceLookup {
  entries: Vec<Option<Vec<PictureSource>>>,
}

impl PictureSourceLookup {
  fn new() -> Self {
    Self {
      entries: vec![None],
    }
  }

  fn insert(&mut self, node_id: usize, sources: Vec<PictureSource>) {
    if node_id == self.entries.len() {
      self.entries.push(Some(sources));
      return;
    }

    if node_id >= self.entries.len() {
      self.entries.resize(node_id + 1, None);
    }
    self.entries[node_id] = Some(sources);
  }

  fn take(&mut self, node_id: usize) -> Vec<PictureSource> {
    self
      .entries
      .get_mut(node_id)
      .and_then(Option::take)
      .unwrap_or_default()
  }
}

fn collect_box_generation_prepass<'a>(
  styled: &'a StyledNode,
  deadline_counter: &mut usize,
) -> Result<BoxGenerationPrepass<'a>> {
  struct CssState {
    enabled: bool,
  }

  let max_css_bytes = foreign_object_css_limit_bytes().max(MAX_EMBEDDED_SVG_CSS_BYTES);
  let forced = svg_embed_document_css_override();
  let max_embedded_svgs = svg_embed_document_css_max_svgs();
  let mut out = BoxGenerationPrepass {
    document_css: String::new(),
    svg_document_css: SvgDocumentCssPolicy {
      embedded_style_element: None,
      max_embedded_svgs,
      replaced_svg_count: 0,
      forced,
    },
    picture_sources: PictureSourceLookup::new(),
    styled_lookup: StyledLookup::new(),
  };
  let mut css = CssState { enabled: true };
  // Avoid recursion for extremely deep trees by using an explicit stack.
  struct Frame<'a> {
    node: &'a StyledNode,
    css_allowed: bool,
    svg_count_allowed: bool,
  }

  let mut stack: Vec<Frame<'a>> = Vec::new();
  stack.push(Frame {
    node: styled,
    css_allowed: true,
    svg_count_allowed: true,
  });

  while let Some(Frame {
    node,
    css_allowed,
    svg_count_allowed,
  }) = stack.pop()
  {
    check_active_periodic(
      deadline_counter,
      BOX_GEN_DEADLINE_STRIDE,
      RenderStage::BoxTree,
    )?;

    out.styled_lookup.insert(node.node_id, node);
    if let Some((img_id, sources)) = picture_sources_for(node) {
      out.picture_sources.insert(img_id, sources);
    }

    let mut children_css_allowed = css_allowed;
    if css_allowed && css.enabled {
      if let Some(tag) = node.node.tag_name() {
        if node.node.is_template_element() {
          children_css_allowed = false;
        } else if tag.eq_ignore_ascii_case("style") {
          for child in node.children.iter() {
            if let Some(text) = child.node.text_content() {
              out.document_css.push_str(text);
              out.document_css.push('\n');
              if out.document_css.len() > max_css_bytes {
                out.document_css.clear();
                css.enabled = false;
                break;
              }
            }
          }
        }
      }
    }

    let mut children_svg_count_allowed = svg_count_allowed;
    if svg_count_allowed {
      if node.styles.display == Display::None {
        children_svg_count_allowed = false;
      } else if let Some(tag) = node.node.tag_name() {
        if is_replaced_element(tag) && node.styles.display != Display::Contents {
          let is_object_with_fallback =
            tag.eq_ignore_ascii_case("object") && !object_has_renderable_external_content(node);
          if !is_object_with_fallback {
            if tag.eq_ignore_ascii_case("svg") {
              out.svg_document_css.replaced_svg_count += 1;
            }
            children_svg_count_allowed = false;
          }
        }
      }
    }

    for child in node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        css_allowed: children_css_allowed,
        svg_count_allowed: children_svg_count_allowed,
      });
    }
  }

  let css_trimmed = trim_ascii_whitespace(&out.document_css);
  let css_size_ok = !css_trimmed.is_empty() && out.document_css.len() <= MAX_EMBEDDED_SVG_CSS_BYTES;
  let allow_embed = if !css_size_ok {
    false
  } else if let Some(forced) = out.svg_document_css.forced {
    forced
  } else if let Some(max) = out.svg_document_css.max_embedded_svgs {
    out.svg_document_css.replaced_svg_count <= max
  } else {
    true
  };
  if allow_embed {
    out.svg_document_css.embedded_style_element = Some(Arc::<str>::from(
      build_svg_cdata_style_element(&out.document_css),
    ));
  }

  Ok(out)
}

fn build_box_tree_root(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
  deadline_counter: &mut usize,
) -> Result<BoxNode> {
  // The styled tree's root is the document node, but the document element (<html>) establishes the
  // writing-mode and direction used for layout and fragmentation.
  let document_axes = if matches!(styled.node.node_type, DomNodeType::Document { .. }) {
    styled.children.iter().find_map(|child| match child.node.node_type {
      DomNodeType::Element { .. } | DomNodeType::Slot { .. } => Some((
        child.styles.writing_mode,
        child.styles.direction,
      )),
      _ => None,
    })
  } else {
    None
  };

  let BoxGenerationPrepass {
    document_css,
    svg_document_css,
    mut picture_sources,
    styled_lookup,
  } = collect_box_generation_prepass(styled, deadline_counter)?;
  let mut counters = CounterManager::new_with_styles(styled.styles.counter_styles.clone());
  counters.enter_scope();
  let mut roots = Vec::new();
  let svg_profile = svg_serialization_profile_enabled();
  if svg_profile {
    enable_svg_serialization_profile();
  }
  let result = generate_boxes_for_styled_into(
    styled,
    &styled_lookup,
    &mut counters,
    true,
    &document_css,
    svg_document_css.embedded_style_element.as_ref(),
    &mut picture_sources,
    options,
    deadline_counter,
    &mut roots,
  );
  counters.leave_scope();
  if svg_profile {
    if let Some(profile) = take_svg_serialization_profile() {
      eprintln!(
        "[svg-serialize] calls={} bytes={} time_ms={:.2} embed_doc_css={} doc_css_bytes={} svgs={} max_svgs={} forced={}",
        profile.calls,
        profile.bytes,
        profile.time_ms,
        svg_document_css.embedded_style_element.is_some(),
        document_css.len(),
        svg_document_css.replaced_svg_count,
        svg_document_css.max_embedded_svgs.unwrap_or(usize::MAX),
        svg_document_css
          .forced
          .map(|v| if v { "on" } else { "off" })
          .unwrap_or("auto")
      );
    }
  }
  result?;

  let mut root = match roots.len() {
    0 => BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      Vec::new(),
    ),
    1 => roots.remove(0),
    _ => BoxNode::new_anonymous_block(Arc::new(ComputedStyle::default()), roots),
  };

  if let Some((writing_mode, direction)) = document_axes {
    if root.style.writing_mode != writing_mode || root.style.direction != direction {
      let mut style = (*root.style).clone();
      style.writing_mode = writing_mode;
      style.direction = direction;
      root.style = Arc::new(style);
    }
  }

  Ok(root)
}

/// Generates a BoxTree from a StyledNode tree
///
/// This is the main entry point for box generation from styled DOM.
/// It recursively converts each StyledNode into the appropriate BoxNode type.
///
/// # Arguments
///
/// * `styled` - The root of the styled node tree
///
/// # Returns
///
/// A `BoxTree` containing the generated box structure
pub fn generate_box_tree(styled: &StyledNode) -> Result<BoxTree> {
  generate_box_tree_with_options(styled, &BoxGenerationOptions::default())
}

/// Generates a BoxTree from a StyledNode tree with custom options.
pub fn generate_box_tree_with_options(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
) -> Result<BoxTree> {
  let mut deadline_counter = 0usize;
  let mut root = build_box_tree_root(styled, options, &mut deadline_counter)?;
  propagate_root_axes_from_root_element(styled, &mut root);
  Ok(BoxTree::new(root))
}

fn root_element_axes(styled: &StyledNode) -> Option<(usize, WritingMode, Direction)> {
  let mut stack: Vec<&StyledNode> = styled.children.iter().rev().collect();
  while let Some(node) = stack.pop() {
    if matches!(node.node.node_type, DomNodeType::Element { .. }) {
      return Some((node.node_id, node.styles.writing_mode, node.styles.direction));
    }

    stack.extend(node.children.iter().rev());
  }
  None
}

fn propagate_root_axes_from_root_element(styled_root: &StyledNode, root: &mut BoxNode) {
  let Some((root_element_id, writing_mode, direction)) = root_element_axes(styled_root) else {
    return;
  };

  if root.styled_node_id == Some(root_element_id) {
    return;
  }

  let style = Arc::make_mut(&mut root.style);
  style.writing_mode = writing_mode;
  style.direction = direction;
}

/// Generates a BoxTree from a StyledNode tree and applies CSS-mandated anonymous box fixup.
///
/// This wraps inline-level runs in anonymous blocks and text in anonymous inline boxes so the
/// resulting tree satisfies the CSS 2.1 box-generation invariants (required for flex/grid/blocks
/// that contain raw text to be layed out correctly).
pub fn generate_box_tree_with_anonymous_fixup(styled: &StyledNode) -> Result<BoxTree> {
  generate_box_tree_with_anonymous_fixup_with_options(styled, &BoxGenerationOptions::default())
}

/// Generates a BoxTree from a StyledNode tree, applies anonymous box fixup, and
/// allows customizing generation behavior via options.
pub fn generate_box_tree_with_anonymous_fixup_with_options(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
) -> Result<BoxTree> {
  let timings_enabled = runtime::runtime_toggles().truthy("FASTR_RENDER_TIMINGS");
  let mut deadline_counter = 0usize;
  let build_start = timings_enabled.then(Instant::now);
  let root = build_box_tree_root(styled, options, &mut deadline_counter)?;
  if let Some(start) = build_start {
    eprintln!("timing:box_gen_build_root {:?}", start.elapsed());
  }
  let anon_start = timings_enabled.then(Instant::now);
  let fixed_root = AnonymousBoxCreator::fixup_tree_with_deadline(root, &mut deadline_counter)?;
  if let Some(start) = anon_start {
    eprintln!("timing:box_gen_anon_fixup {:?}", start.elapsed());
  }
  let table_start = timings_enabled.then(Instant::now);
  let mut fixed_root =
    TableStructureFixer::fixup_tree_internals_with_deadline(fixed_root, &mut deadline_counter)?;
  propagate_root_axes_from_root_element(styled, &mut fixed_root);
  if let Some(start) = table_start {
    eprintln!("timing:box_gen_table_fixup {:?}", start.elapsed());
  }
  Ok(BoxTree::new(fixed_root))
}

fn attach_styled_id(mut node: BoxNode, styled: &StyledNode) -> BoxNode {
  node.styled_node_id = Some(styled.node_id);
  node
}

fn push_escaped_text(out: &mut String, value: &str) {
  let bytes = value.as_bytes();
  let mut last = 0usize;
  while let Some(rel_idx) = memchr::memchr2(b'&', b'<', &bytes[last..]) {
    let idx = last + rel_idx;
    if last < idx {
      out.push_str(&value[last..idx]);
    }
    match bytes[idx] {
      b'&' => out.push_str("&amp;"),
      b'<' => out.push_str("&lt;"),
      _ => unreachable!("memchr2 returned non-matching byte"),
    }
    last = idx + 1;
  }
  if last < value.len() {
    out.push_str(&value[last..]);
  }
}

fn push_escaped_attr(out: &mut String, value: &str) {
  let bytes = value.as_bytes();
  let mut last = 0usize;
  while let Some(rel_idx) = memchr::memchr3(b'&', b'<', b'"', &bytes[last..]) {
    let idx = last + rel_idx;
    if last < idx {
      out.push_str(&value[last..idx]);
    }
    match bytes[idx] {
      b'&' => out.push_str("&amp;"),
      b'<' => out.push_str("&lt;"),
      b'"' => out.push_str("&quot;"),
      _ => unreachable!("memchr3 returned non-matching byte"),
    }
    last = idx + 1;
  }
  if last < value.len() {
    out.push_str(&value[last..]);
  }
}

fn dom_subtree_from_styled(node: &StyledNode) -> DomNode {
  // Avoid recursion for extremely deep styled trees.
  struct Frame<'a> {
    src: &'a StyledNode,
    dst: *mut DomNode,
    next_child: usize,
  }

  let mut root = DomNode {
    node_type: node.node.node_type.clone(),
    children: Vec::with_capacity(node.children.len()),
  };

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    src: node,
    dst: &mut root as *mut DomNode,
    next_child: 0,
  });

  while let Some(frame) = stack.last_mut() {
    if frame.next_child >= frame.src.children.len() {
      stack.pop();
      continue;
    }

    let child = &frame.src.children[frame.next_child];
    frame.next_child += 1;

    // SAFETY: `DomNode`s are never moved while their frames are on the stack. We only mutate a
    // `children` Vec after pushing a new child, and the child pointer we store always points to
    // the last element of that Vec.
    let dst = unsafe { &mut *frame.dst };
    dst.children.push(DomNode {
      node_type: child.node.node_type.clone(),
      children: Vec::with_capacity(child.children.len()),
    });
    let child_dst = dst.children.last_mut().expect("child was just pushed") as *mut DomNode;

    stack.push(Frame {
      src: child,
      dst: child_dst,
      next_child: 0,
    });
  }

  root
}

fn escape_attr(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  push_escaped_attr(&mut out, value);
  out
}

fn escape_text(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  push_escaped_text(&mut out, value);
  out
}

enum ComposedChildren<'a> {
  Slice(&'a [StyledNode]),
  Refs(Vec<&'a StyledNode>),
}

impl<'a> ComposedChildren<'a> {
  fn len(&self) -> usize {
    match self {
      Self::Slice(children) => children.len(),
      Self::Refs(children) => children.len(),
    }
  }

  fn get(&self, idx: usize) -> &'a StyledNode {
    match self {
      Self::Slice(children) => &children[idx],
      Self::Refs(children) => children[idx],
    }
  }
}

fn composed_children<'a>(
  styled: &'a StyledNode,
  lookup: &'a StyledLookup<'a>,
) -> ComposedChildren<'a> {
  // `<template>` contents are inert and must not participate in the composed/rendered tree.
  // Even if author CSS overrides `template { display: block }`, the template's child nodes are not
  // part of the document and should never generate boxes.
  if styled.node.template_contents_are_inert() {
    return ComposedChildren::Slice(&[]);
  }

  if let Some(shadow_root) = styled
    .children
    .iter()
    .find(|c| matches!(c.node.node_type, crate::dom::DomNodeType::ShadowRoot { .. }))
  {
    return ComposedChildren::Refs(vec![shadow_root]);
  }

  if matches!(styled.node.node_type, crate::dom::DomNodeType::Slot { .. })
    && !styled.slotted_node_ids.is_empty()
  {
    let mut resolved: Vec<&'a StyledNode> = Vec::with_capacity(styled.slotted_node_ids.len());
    for id in &styled.slotted_node_ids {
      if let Some(node) = lookup.get(*id) {
        resolved.push(node);
      }
    }
    return ComposedChildren::Refs(resolved);
  }

  ComposedChildren::Slice(&styled.children)
}

fn normalize_mime_type(value: &str) -> Option<String> {
  let base = trim_ascii_whitespace(value.split(';').next().unwrap_or(""));
  if base.is_empty() {
    None
  } else {
    Some(base.to_ascii_lowercase())
  }
}

fn picture_sources_for(styled: &StyledNode) -> Option<(usize, Vec<PictureSource>)> {
  let tag = styled.node.tag_name()?;
  if !tag.eq_ignore_ascii_case("picture") {
    return None;
  }

  let mut sources: Vec<PictureSource> = Vec::new();
  let mut fallback_img: Option<&StyledNode> = None;

  for child in &styled.children {
    let Some(child_tag) = child.node.tag_name() else {
      continue;
    };

    if child_tag.eq_ignore_ascii_case("source") {
      if fallback_img.is_some() {
        continue;
      }

      let Some(srcset_attr) = child.node.get_attribute_ref("srcset") else {
        continue;
      };
      let parsed_srcset = parse_srcset(srcset_attr);
      if parsed_srcset.is_empty() {
        continue;
      }

      let sizes = child.node.get_attribute_ref("sizes").and_then(parse_sizes);
      let media = child
        .node
        .get_attribute_ref("media")
        .and_then(|m| MediaQuery::parse_list(m).ok());
      let mime_type = child
        .node
        .get_attribute_ref("type")
        .and_then(normalize_mime_type);

      sources.push(PictureSource {
        srcset: parsed_srcset,
        sizes,
        media,
        mime_type,
      });
      continue;
    }

    if child_tag.eq_ignore_ascii_case("img") {
      fallback_img = Some(child);
      break;
    }
  }

  fallback_img.map(|img| (img.node_id, sources))
}

#[allow(dead_code)]
fn serialize_dom_subtree(node: &crate::dom::DomNode) -> String {
  match &node.node_type {
    crate::dom::DomNodeType::Text { content } => escape_text(content),
    crate::dom::DomNodeType::ShadowRoot { .. } => {
      node.children.iter().map(serialize_dom_subtree).collect()
    }
    crate::dom::DomNodeType::Slot { attributes, .. } => {
      let mut out = String::new();
      out.push_str("<slot");
      for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(&mut out, value);
        out.push('"');
      }
      out.push('>');
      for child in node.children.iter() {
        out.push_str(&serialize_dom_subtree(child));
      }
      out.push_str("</slot>");
      out
    }
    crate::dom::DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } => {
      let mut out = String::new();
      out.push('<');
      out.push_str(tag_name);
      for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(&mut out, value);
        out.push('"');
      }
      out.push('>');
      for child in node.children.iter() {
        out.push_str(&serialize_dom_subtree(child));
      }
      out.push_str("</");
      out.push_str(tag_name);
      out.push('>');
      out
    }
    crate::dom::DomNodeType::Document { .. } => {
      node.children.iter().map(serialize_dom_subtree).collect()
    }
  }
}

fn serialize_styled_dom_subtree_html(styled: &StyledNode, out: &mut String) {
  #[derive(Clone, Copy)]
  enum FrameState {
    Enter,
    Exit,
  }

  struct Frame<'a> {
    node: &'a StyledNode,
    state: FrameState,
  }

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    node: styled,
    state: FrameState::Enter,
  });

  while let Some(Frame { node, state }) = stack.pop() {
    match state {
      FrameState::Enter => match &node.node.node_type {
        DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. } => {
          for child in node.children.iter().rev() {
            stack.push(Frame {
              node: child,
              state: FrameState::Enter,
            });
          }
        }
        DomNodeType::Slot { attributes, .. } => {
          out.push_str("<slot");
          for (name, value) in attributes {
            out.push(' ');
            out.push_str(name);
            out.push('=');
            out.push('"');
            push_escaped_attr(out, value);
            out.push('"');
          }
          out.push('>');

          stack.push(Frame {
            node,
            state: FrameState::Exit,
          });
          for child in node.children.iter().rev() {
            stack.push(Frame {
              node: child,
              state: FrameState::Enter,
            });
          }
        }
        DomNodeType::Element {
          tag_name,
          attributes,
          ..
        } => {
          out.push('<');
          out.push_str(tag_name);
          for (name, value) in attributes {
            out.push(' ');
            out.push_str(name);
            out.push('=');
            out.push('"');
            push_escaped_attr(out, value);
            out.push('"');
          }
          out.push('>');

          stack.push(Frame {
            node,
            state: FrameState::Exit,
          });
          for child in node.children.iter().rev() {
            stack.push(Frame {
              node: child,
              state: FrameState::Enter,
            });
          }
        }
        DomNodeType::Text { content } => push_escaped_text(out, content),
      },
      FrameState::Exit => match &node.node.node_type {
        DomNodeType::Slot { .. } => out.push_str("</slot>"),
        DomNodeType::Element { tag_name, .. } => {
          out.push_str("</");
          out.push_str(tag_name);
          out.push('>');
        }
        DomNodeType::Document { .. }
        | DomNodeType::ShadowRoot { .. }
        | DomNodeType::Text { .. } => {}
      },
    }
  }
}

fn serialize_node_with_namespaces(
  styled: &StyledNode,
  inherited_xmlns: &[(String, String)],
  out: &mut String,
) {
  match &styled.node.node_type {
    crate::dom::DomNodeType::Document { .. } | crate::dom::DomNodeType::ShadowRoot { .. } => {
      for child in &styled.children {
        serialize_node_with_namespaces(child, inherited_xmlns, out);
      }
    }
    crate::dom::DomNodeType::Slot {
      namespace,
      attributes,
      ..
    } => {
      let mut attrs = attributes.clone();
      let mut namespaces: Vec<(String, String)> = inherited_xmlns.to_vec();
      for (name, value) in &attrs {
        if name.starts_with("xmlns")
          && !namespaces.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
        {
          namespaces.push((name.clone(), value.clone()));
        }
      }
      if !namespace.is_empty()
        && !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
        && !namespaces
          .iter()
          .any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
      {
        namespaces.push(("xmlns".to_string(), namespace.clone()));
      }
      for (name, value) in &namespaces {
        if !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
          attrs.push((name.clone(), value.clone()));
        }
      }

      out.push_str("<slot");
      for (name, value) in &attrs {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(out, value);
        out.push('"');
      }
      out.push('>');
      for child in &styled.children {
        serialize_node_with_namespaces(child, &namespaces, out);
      }
      out.push_str("</slot>");
    }
    crate::dom::DomNodeType::Text { content } => push_escaped_text(out, content),
    crate::dom::DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
    } => {
      let mut attrs = attributes.clone();
      let mut namespaces: Vec<(String, String)> = inherited_xmlns.to_vec();
      for (name, value) in &attrs {
        if name.starts_with("xmlns")
          && !namespaces.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
        {
          namespaces.push((name.clone(), value.clone()));
        }
      }
      if !namespace.is_empty()
        && !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
        && !namespaces
          .iter()
          .any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
      {
        namespaces.push(("xmlns".to_string(), namespace.clone()));
      }
      for (name, value) in &namespaces {
        if !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
          attrs.push((name.clone(), value.clone()));
        }
      }

      out.push('<');
      out.push_str(tag_name);
      for (name, value) in &attrs {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(out, value);
        out.push('"');
      }
      out.push('>');
      for child in &styled.children {
        serialize_node_with_namespaces(child, &namespaces, out);
      }
      out.push_str("</");
      out.push_str(tag_name);
      out.push('>');
    }
  }
}

/// Serializes a styled DOM subtree without injecting document CSS or foreignObject placeholders.
///
/// This is intended for defs-only serialization such as collecting SVG filter definitions.
pub fn serialize_styled_subtree_plain(styled: &StyledNode) -> String {
  let mut out = String::new();
  serialize_node_with_namespaces(styled, &[], &mut out);
  out
}

fn merge_style_attribute(attrs: &mut Vec<(String, String)>, extra: &str) {
  if trim_ascii_whitespace(extra).is_empty() {
    return;
  }
  if let Some((_, value)) = attrs
    .iter_mut()
    .find(|(name, _)| name.eq_ignore_ascii_case("style"))
  {
    if !trim_ascii_whitespace_end(value).ends_with(';') && !trim_ascii_whitespace(value).is_empty() {
      value.push(';');
    }
    value.push_str(extra);
  } else {
    attrs.push(("style".to_string(), extra.to_string()));
  }
}

fn svg_transform_attribute(style: &ComputedStyle) -> Option<String> {
  use crate::css::types::{RotateValue, ScaleValue, Transform as CssTransform, TranslateValue};
  use crate::style::values::Length;
  use std::fmt::Write as _;

  fn resolve_transform_length(len: Length, style: &ComputedStyle) -> Option<f32> {
    if len.is_zero() {
      return Some(0.0);
    }
    if let Some(calc) = len.calc {
      if calc.has_percentage() || calc.has_viewport_relative() {
        return None;
      }
      // A calc() with only absolute/font-relative terms can be resolved with a zero viewport base.
      return calc.resolve(None, 0.0, 0.0, style.font_size, style.root_font_size);
    }
    if len.unit.is_absolute() {
      return Some(len.to_px());
    }
    if len.unit.is_font_relative() {
      return len.resolve_with_context(None, 0.0, 0.0, style.font_size, style.root_font_size);
    }
    None
  }

  if !style.has_transform() {
    return None;
  }

  let mut out = String::new();
  let mut push_sep = |out: &mut String| {
    if !out.is_empty() {
      out.push(' ');
    }
  };

  // CSS Transforms Level 2: translate → rotate → scale → transform list.
  if let TranslateValue::Values { x, y, z } = style.translate {
    let z_is_zero = z.is_zero();
    let tx = resolve_transform_length(x, style)?;
    let ty = resolve_transform_length(y, style)?;
    if !z_is_zero {
      return None;
    }
    push_sep(&mut out);
    let _ = write!(&mut out, "translate({tx} {ty})");
  }

  match style.rotate {
    RotateValue::None => {}
    RotateValue::Angle(deg) => {
      if !deg.is_finite() {
        return None;
      }
      push_sep(&mut out);
      let _ = write!(&mut out, "rotate({deg})");
    }
    RotateValue::AxisAngle { x, y, z, angle } => {
      if !x.is_finite() || !y.is_finite() || !z.is_finite() || !angle.is_finite() {
        return None;
      }
      // SVG only supports 2D rotate (about the z axis). Treat (0, 0, -1) as rotate(-angle).
      if x.abs() > 1e-6 || y.abs() > 1e-6 || z.abs() <= 1e-6 {
        return None;
      }
      let signed = if z.is_sign_negative() { -angle } else { angle };
      push_sep(&mut out);
      let _ = write!(&mut out, "rotate({signed})");
    }
  }

  if let ScaleValue::Values { x, y, z } = style.scale {
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
      return None;
    }
    if (z - 1.0).abs() > 1e-6 {
      return None;
    }
    push_sep(&mut out);
    let _ = write!(&mut out, "scale({x} {y})");
  }

  for component in &style.transform {
    match *component {
      CssTransform::Translate(x, y) => {
        let tx = resolve_transform_length(x, style)?;
        let ty = resolve_transform_length(y, style)?;
        push_sep(&mut out);
        let _ = write!(&mut out, "translate({tx} {ty})");
      }
      CssTransform::TranslateX(x) => {
        let tx = resolve_transform_length(x, style)?;
        push_sep(&mut out);
        let _ = write!(&mut out, "translate({tx} 0)");
      }
      CssTransform::TranslateY(y) => {
        let ty = resolve_transform_length(y, style)?;
        push_sep(&mut out);
        let _ = write!(&mut out, "translate(0 {ty})");
      }
      CssTransform::TranslateZ(z) => {
        if !z.is_zero() {
          return None;
        }
      }
      CssTransform::Translate3d(x, y, z) => {
        if !z.is_zero() {
          return None;
        }
        let tx = resolve_transform_length(x, style)?;
        let ty = resolve_transform_length(y, style)?;
        push_sep(&mut out);
        let _ = write!(&mut out, "translate({tx} {ty})");
      }
      CssTransform::Scale(x, y) => {
        if !x.is_finite() || !y.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "scale({x} {y})");
      }
      CssTransform::ScaleX(x) => {
        if !x.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "scale({x} 1)");
      }
      CssTransform::ScaleY(y) => {
        if !y.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "scale(1 {y})");
      }
      CssTransform::ScaleZ(z) => {
        if !z.is_finite() {
          return None;
        }
        if (z - 1.0).abs() > 1e-6 {
          return None;
        }
      }
      CssTransform::Scale3d(x, y, z) => {
        if !x.is_finite() || !y.is_finite() || !z.is_finite() {
          return None;
        }
        if (z - 1.0).abs() > 1e-6 {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "scale({x} {y})");
      }
      CssTransform::Rotate(deg) | CssTransform::RotateZ(deg) => {
        if !deg.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "rotate({deg})");
      }
      CssTransform::RotateX(deg) | CssTransform::RotateY(deg) => {
        if !deg.is_finite() {
          return None;
        }
        if deg.abs() > 1e-6 {
          return None;
        }
      }
      CssTransform::Rotate3d(x, y, z, angle) => {
        if !x.is_finite() || !y.is_finite() || !z.is_finite() || !angle.is_finite() {
          return None;
        }
        if angle.abs() <= 1e-6 {
          continue;
        }
        if x.abs() > 1e-6 || y.abs() > 1e-6 || z.abs() <= 1e-6 {
          return None;
        }
        let signed = if z.is_sign_negative() { -angle } else { angle };
        push_sep(&mut out);
        let _ = write!(&mut out, "rotate({signed})");
      }
      CssTransform::SkewX(deg) => {
        if !deg.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "skewX({deg})");
      }
      CssTransform::SkewY(deg) => {
        if !deg.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "skewY({deg})");
      }
      CssTransform::Skew(ax, ay) => {
        if !ax.is_finite() || !ay.is_finite() {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "skewX({ax})");
        push_sep(&mut out);
        let _ = write!(&mut out, "skewY({ay})");
      }
      CssTransform::Matrix(a, b, c, d, e, f) => {
        if !a.is_finite()
          || !b.is_finite()
          || !c.is_finite()
          || !d.is_finite()
          || !e.is_finite()
          || !f.is_finite()
        {
          return None;
        }
        push_sep(&mut out);
        let _ = write!(&mut out, "matrix({a} {b} {c} {d} {e} {f})");
      }
      CssTransform::Perspective(_) | CssTransform::Matrix3d(_) => return None,
    }
  }

  (!out.is_empty()).then_some(out)
}

fn svg_presentation_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  use crate::style::types::ColorOrNone;
  use crate::style::types::FillRule;
  use crate::style::types::LengthOrNumber;
  use crate::style::types::StrokeDasharray;
  use crate::style::types::StrokeLinecap;
  use crate::style::types::StrokeLinejoin;
  use crate::style::types::SvgUrlOrNone;
  use std::fmt::Write as _;

  let mut out = String::new();
  let mut any = false;

  let mut start_decl = |out: &mut String, any: &mut bool| {
    if *any {
      out.push_str("; ");
    } else {
      *any = true;
    }
  };

  let mut push_color_or_none = |out: &mut String, value: ColorOrNone, current_color: Rgba| {
    match value {
      ColorOrNone::None => out.push_str("none"),
      ColorOrNone::Color(color) => {
        let _ = write!(
          out,
          "rgba({},{},{},{:.3})",
          color.r,
          color.g,
          color.b,
          color.a.clamp(0.0, 1.0)
        );
      }
      ColorOrNone::CurrentColor => {
        let _ = write!(
          out,
          "rgba({},{},{},{:.3})",
          current_color.r,
          current_color.g,
          current_color.b,
          current_color.a.clamp(0.0, 1.0)
        );
      }
    }
  };

  let effective_color_or_none = |value: ColorOrNone, current_color: Rgba| match value {
    ColorOrNone::CurrentColor => ColorOrNone::Color(current_color),
    other => other,
  };

  let mut push_length_or_number = |out: &mut String, value: LengthOrNumber| match value {
    LengthOrNumber::Number(v) => {
      let _ = write!(out, "{}", v);
    }
    LengthOrNumber::Length(len) => {
      let _ = write!(out, "{}", len);
    }
  };

  let mut push_svg_url_or_none = |out: &mut String, value: &SvgUrlOrNone| match value {
    SvgUrlOrNone::None => out.push_str("none"),
    SvgUrlOrNone::Url(url) => {
      out.push_str("url(");
      out.push_str(url);
      out.push(')');
    }
  };

  if let Some(fill) = style.svg_fill {
    let effective = effective_color_or_none(fill, style.color);
    let parent_effective =
      parent.and_then(|p| p.svg_fill.map(|value| effective_color_or_none(value, p.color)));
    if parent_effective != Some(effective) {
      start_decl(&mut out, &mut any);
      out.push_str("fill: ");
      push_color_or_none(&mut out, effective, style.color);
    }
  }

  if let Some(stroke) = style.svg_stroke {
    let effective = effective_color_or_none(stroke, style.color);
    let parent_effective =
      parent.and_then(|p| p.svg_stroke.map(|value| effective_color_or_none(value, p.color)));
    if parent_effective != Some(effective) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke: ");
      push_color_or_none(&mut out, effective, style.color);
    }
  }

  if let Some(width) = style.svg_stroke_width {
    if parent.and_then(|p| p.svg_stroke_width) != Some(width) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke-width: ");
      push_length_or_number(&mut out, width);
    }
  }

  if let Some(fill_rule) = style.svg_fill_rule {
    if parent.and_then(|p| p.svg_fill_rule) != Some(fill_rule) {
      start_decl(&mut out, &mut any);
      out.push_str("fill-rule: ");
      match fill_rule {
        FillRule::NonZero => out.push_str("nonzero"),
        FillRule::EvenOdd => out.push_str("evenodd"),
      }
    }
  }

  if let Some(clip_rule) = style.svg_clip_rule {
    if parent.and_then(|p| p.svg_clip_rule) != Some(clip_rule) {
      start_decl(&mut out, &mut any);
      out.push_str("clip-rule: ");
      match clip_rule {
        FillRule::NonZero => out.push_str("nonzero"),
        FillRule::EvenOdd => out.push_str("evenodd"),
      }
    }
  }

  if let Some(linecap) = style.svg_stroke_linecap {
    if parent.and_then(|p| p.svg_stroke_linecap) != Some(linecap) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke-linecap: ");
      match linecap {
        StrokeLinecap::Butt => out.push_str("butt"),
        StrokeLinecap::Round => out.push_str("round"),
        StrokeLinecap::Square => out.push_str("square"),
      }
    }
  }

  if let Some(linejoin) = style.svg_stroke_linejoin {
    if parent.and_then(|p| p.svg_stroke_linejoin) != Some(linejoin) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke-linejoin: ");
      match linejoin {
        StrokeLinejoin::Miter => out.push_str("miter"),
        StrokeLinejoin::Round => out.push_str("round"),
        StrokeLinejoin::Bevel => out.push_str("bevel"),
      }
    }
  }

  if let Some(limit) = style.svg_stroke_miterlimit {
    if parent.and_then(|p| p.svg_stroke_miterlimit) != Some(limit) {
      start_decl(&mut out, &mut any);
      let _ = write!(&mut out, "stroke-miterlimit: {}", limit);
    }
  }

  if let Some(dasharray) = style.svg_stroke_dasharray.as_ref() {
    if !parent
      .and_then(|p| p.svg_stroke_dasharray.as_ref())
      .is_some_and(|parent_dash| parent_dash == dasharray)
    {
      start_decl(&mut out, &mut any);
      out.push_str("stroke-dasharray: ");
      match dasharray {
        StrokeDasharray::None => out.push_str("none"),
        StrokeDasharray::Values(values) => {
          for (idx, value) in values.iter().enumerate() {
            if idx != 0 {
              out.push(' ');
            }
            push_length_or_number(&mut out, *value);
          }
        }
      }
    }
  }

  if let Some(dashoffset) = style.svg_stroke_dashoffset {
    if parent.and_then(|p| p.svg_stroke_dashoffset) != Some(dashoffset) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke-dashoffset: ");
      push_length_or_number(&mut out, dashoffset);
    }
  }

  if let Some(opacity) = style.svg_fill_opacity {
    if parent.and_then(|p| p.svg_fill_opacity) != Some(opacity) {
      start_decl(&mut out, &mut any);
      let _ = write!(&mut out, "fill-opacity: {:.3}", opacity);
    }
  }

  if let Some(opacity) = style.svg_stroke_opacity {
    if parent.and_then(|p| p.svg_stroke_opacity) != Some(opacity) {
      start_decl(&mut out, &mut any);
      let _ = write!(&mut out, "stroke-opacity: {:.3}", opacity);
    }
  }

  if let Some(marker) = style.svg_marker_start.as_ref() {
    if parent.and_then(|p| p.svg_marker_start.as_ref()) != Some(marker) {
      start_decl(&mut out, &mut any);
      out.push_str("marker-start: ");
      push_svg_url_or_none(&mut out, marker);
    }
  }

  if let Some(marker) = style.svg_marker_mid.as_ref() {
    if parent.and_then(|p| p.svg_marker_mid.as_ref()) != Some(marker) {
      start_decl(&mut out, &mut any);
      out.push_str("marker-mid: ");
      push_svg_url_or_none(&mut out, marker);
    }
  }

  if let Some(marker) = style.svg_marker_end.as_ref() {
    if parent.and_then(|p| p.svg_marker_end.as_ref()) != Some(marker) {
      start_decl(&mut out, &mut any);
      out.push_str("marker-end: ");
      push_svg_url_or_none(&mut out, marker);
    }
  }

  any.then_some(out)
}

fn svg_paint_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  use crate::style::display::Display;
  use std::fmt::Write as _;

  let mut out = String::new();
  let mut any = false;

  let mut start_decl = |out: &mut String, any: &mut bool| {
    if *any {
      out.push_str("; ");
    } else {
      *any = true;
    }
  };

  if style.display == Display::None {
    start_decl(&mut out, &mut any);
    out.push_str("display: none");
  }

  let normalize_visibility = |value: Visibility| match value {
    Visibility::Visible => Visibility::Visible,
    Visibility::Hidden | Visibility::Collapse => Visibility::Hidden,
  };
  let visibility = normalize_visibility(style.visibility);
  let parent_visibility =
    normalize_visibility(parent.map(|p| p.visibility).unwrap_or(Visibility::Visible));
  if visibility != parent_visibility {
    start_decl(&mut out, &mut any);
    out.push_str("visibility: ");
    match visibility {
      Visibility::Visible => out.push_str("visible"),
      Visibility::Hidden => out.push_str("hidden"),
      Visibility::Collapse => unreachable!("collapse is normalized to hidden"),
    }
  }

  if style.opacity.is_finite() && style.opacity != 1.0 {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "opacity: {:.3}", style.opacity.clamp(0.0, 1.0));
  }

  any.then_some(out)
}

fn serialize_svg_mask_subtree_with_namespaces(
  styled: &StyledNode,
  inherited_xmlns: &[(String, String)],
  parent_ns: Option<&str>,
  parent_svg_styles: Option<&ComputedStyle>,
  is_root: bool,
  out: &mut String,
) {
  match &styled.node.node_type {
    crate::dom::DomNodeType::Document { .. } | crate::dom::DomNodeType::ShadowRoot { .. } => {
      for child in &styled.children {
        serialize_svg_mask_subtree_with_namespaces(
          child,
          inherited_xmlns,
          parent_ns,
          parent_svg_styles,
          false,
          out,
        );
      }
    }
    crate::dom::DomNodeType::Slot {
      namespace,
      attributes,
      ..
    } => {
      let mut current_ns = namespace.as_str();
      if is_root && current_ns.is_empty() {
        current_ns = SVG_NAMESPACE;
      } else if current_ns.is_empty() {
        if let Some(parent_ns) = parent_ns {
          current_ns = parent_ns;
        }
      }

      let next_parent_svg_styles = if current_ns == SVG_NAMESPACE {
        Some(&*styled.styles)
      } else {
        parent_svg_styles
      };

      let mut attrs = attributes.clone();
      let mut namespaces: Vec<(String, String)> = inherited_xmlns.to_vec();
      for (name, value) in &attrs {
        if name.starts_with("xmlns")
          && !namespaces.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
        {
          namespaces.push((name.clone(), value.clone()));
        }
      }
      if !current_ns.is_empty()
        && !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
        && !namespaces
          .iter()
          .any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
      {
        namespaces.push(("xmlns".to_string(), current_ns.to_string()));
      }
      for (name, value) in &namespaces {
        if !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
          attrs.push((name.clone(), value.clone()));
        }
      }

      out.push_str("<slot");
      for (name, value) in &attrs {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(out, value);
        out.push('"');
      }
      out.push('>');
      for child in &styled.children {
        serialize_svg_mask_subtree_with_namespaces(
          child,
          &namespaces,
          Some(current_ns),
          next_parent_svg_styles,
          false,
          out,
        );
      }
      out.push_str("</slot>");
    }
    crate::dom::DomNodeType::Text { content } => push_escaped_text(out, content),
    crate::dom::DomNodeType::Element {
      tag_name,
      namespace,
      attributes,
    } => {
      let mut current_ns = namespace.as_str();
      if is_root && current_ns.is_empty() {
        current_ns = SVG_NAMESPACE;
      } else if current_ns.is_empty() {
        if let Some(parent_ns) = parent_ns {
          current_ns = parent_ns;
        }
      }

      let next_parent_svg_styles = if current_ns == SVG_NAMESPACE {
        Some(&*styled.styles)
      } else {
        parent_svg_styles
      };

      let mut attrs = attributes.clone();
      if current_ns == SVG_NAMESPACE
        && !attrs
          .iter()
          .any(|(name, _)| name.eq_ignore_ascii_case("transform"))
      {
        if let Some(transform) = svg_transform_attribute(&styled.styles) {
          attrs.push(("transform".to_string(), transform));
        }
      }
      let mut namespaces: Vec<(String, String)> = inherited_xmlns.to_vec();
      for (name, value) in &attrs {
        if name.starts_with("xmlns")
          && !namespaces.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
        {
          namespaces.push((name.clone(), value.clone()));
        }
      }
      if !current_ns.is_empty()
        && !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
        && !namespaces
          .iter()
          .any(|(n, _)| n.eq_ignore_ascii_case("xmlns"))
      {
        namespaces.push(("xmlns".to_string(), current_ns.to_string()));
      }
      for (name, value) in &namespaces {
        if !attrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
          attrs.push((name.clone(), value.clone()));
        }
      }

      if current_ns == SVG_NAMESPACE {
        if let Some(extra) = svg_presentation_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_paint_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
      }

      out.push('<');
      out.push_str(tag_name);
      for (name, value) in &attrs {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        push_escaped_attr(out, value);
        out.push('"');
      }
      out.push('>');
      for child in &styled.children {
        serialize_svg_mask_subtree_with_namespaces(
          child,
          &namespaces,
          Some(current_ns),
          next_parent_svg_styles,
          false,
          out,
        );
      }
      out.push_str("</");
      out.push_str(tag_name);
      out.push('>');
    }
  }
}

/// Collect all SVG `<mask>` definitions with an `id` attribute from a styled DOM tree.
///
/// The returned map contains serialized mask elements keyed by their id. Serialized masks inline
/// computed SVG presentation properties (fill/stroke/etc.) so downstream rasterizers (resvg) do
/// not need access to the full document CSS cascade.
///
/// Namespace declarations from ancestor elements are preserved to keep prefixed attributes valid.
pub fn collect_svg_mask_defs(styled: &StyledNode) -> HashMap<String, String> {
  fn walk(
    styled: &StyledNode,
    inherited_xmlns: &[(String, String)],
    masks: &mut HashMap<String, String>,
  ) {
    let mut owned_namespaces: Option<Vec<(String, String)>> = None;
    let mut namespaces = inherited_xmlns;
    if let crate::dom::DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } = &styled.node.node_type
    {
      if attributes.iter().any(|(name, _)| name.starts_with("xmlns")) {
        let mut updated = inherited_xmlns.to_vec();
        for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
          if !updated.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            updated.push((name.clone(), value.clone()));
          }
        }
        owned_namespaces = Some(updated);
        namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);
      }

      if tag_name.eq_ignore_ascii_case("mask") {
        if let Some(id) = styled.node.get_attribute_ref("id") {
          if !id.is_empty() && !masks.contains_key(id) {
            let mut serialized = String::new();
            serialize_svg_mask_subtree_with_namespaces(
              styled,
              namespaces,
              None,
              None,
              true,
              &mut serialized,
            );
            masks.insert(id.to_string(), serialized);
          }
        }
      }
    }

    for child in &styled.children {
      walk(child, namespaces, masks);
    }
  }

  let mut masks = HashMap::new();
  walk(styled, &[], &mut masks);
  masks
}

/// Collect all SVG `<filter>` definitions with an `id` attribute from a styled DOM tree.
///
/// The returned map contains serialized filter elements keyed by their id.
/// Namespace declarations from ancestor elements are preserved to keep prefixed attributes valid.
pub fn collect_svg_filter_defs(styled: &StyledNode) -> HashMap<String, String> {
  fn walk(
    styled: &StyledNode,
    inherited_xmlns: &[(String, String)],
    filters: &mut HashMap<String, String>,
  ) {
    let mut owned_namespaces: Option<Vec<(String, String)>> = None;
    let mut namespaces = inherited_xmlns;
    if let crate::dom::DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } = &styled.node.node_type
    {
      if attributes.iter().any(|(name, _)| name.starts_with("xmlns")) {
        let mut updated = inherited_xmlns.to_vec();
        for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
          if !updated.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            updated.push((name.clone(), value.clone()));
          }
        }
        owned_namespaces = Some(updated);
        namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);
      }

      if tag_name.eq_ignore_ascii_case("filter") {
        if let Some(id) = styled.node.get_attribute_ref("id") {
          if !id.is_empty() && !filters.contains_key(id) {
            let mut serialized = String::new();
            serialize_node_with_namespaces(styled, namespaces, &mut serialized);
            filters.insert(id.to_string(), serialized);
          }
        }
      }
    }

    for child in &styled.children {
      walk(child, namespaces, filters);
    }
  }

  let mut filters = HashMap::new();
  walk(styled, &[], &mut filters);
  filters
}

/// Collect serialized SVG id definitions required by fragment-only CSS masks.
///
/// This powers `mask-image: url(#id)` by serializing the referenced SVG `<mask>` element (and any
/// other defs it references via `href="#..."`, `url(#...)`, etc.).
///
/// We inline computed SVG presentation properties (fill/stroke/opacity/etc.) during serialization
/// so downstream rasterizers (resvg) do not need access to the full document CSS cascade.
///
/// Namespace declarations from ancestor elements are preserved to keep prefixed attributes valid.
pub fn collect_svg_id_defs(styled: &StyledNode) -> HashMap<String, String> {
  use crate::style::types::BackgroundImage;

  fn extract_url_fragment_ids(value: &str, out: &mut HashSet<String>) {
    let bytes = value.as_bytes();
    let mut idx = 0usize;
    while idx + 4 <= bytes.len() {
      let b = bytes[idx];
      if (b == b'u' || b == b'U')
        && (bytes[idx + 1] == b'r' || bytes[idx + 1] == b'R')
        && (bytes[idx + 2] == b'l' || bytes[idx + 2] == b'L')
        && bytes[idx + 3] == b'('
      {
        idx += 4;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
          idx += 1;
        }

        let mut quote: Option<u8> = None;
        if idx < bytes.len() && (bytes[idx] == b'\'' || bytes[idx] == b'"') {
          quote = Some(bytes[idx]);
          idx += 1;
          while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
          }
        }

        if idx < bytes.len() && bytes[idx] == b'#' {
          idx += 1;
          let start = idx;
          while idx < bytes.len() {
            let ch = bytes[idx];
            if ch == b')' || ch.is_ascii_whitespace() {
              break;
            }
            if quote.is_some_and(|q| q == ch) {
              break;
            }
            idx += 1;
          }
          if start < idx {
            out.insert(value[start..idx].to_string());
          }
        }

        while idx < bytes.len() && bytes[idx] != b')' {
          idx += 1;
        }
        if idx < bytes.len() {
          idx += 1;
        }
      } else {
        idx += 1;
      }
    }
  }

  fn is_href_attr(name: &str) -> bool {
    if name.eq_ignore_ascii_case("href") {
      return true;
    }
    name
      .rsplit_once(':')
      .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
  }

  fn collect_requested_mask_ids(styled: &StyledNode, out: &mut HashSet<String>) {
    for layer in styled.styles.mask_layers.iter() {
      let Some(image) = layer.image.as_ref() else {
        continue;
      };
      let BackgroundImage::Url(src) = image else {
        continue;
      };
      if let Some(id) = trim_ascii_whitespace(src)
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
      {
        out.insert(id.to_string());
      }
    }
    for child in &styled.children {
      collect_requested_mask_ids(child, out);
    }
  }

  struct IndexedNode<'a> {
    node: &'a StyledNode,
    namespaces: Vec<(String, String)>,
  }

  fn build_svg_id_index<'a>(
    styled: &'a StyledNode,
    inherited_xmlns: &[(String, String)],
    out: &mut HashMap<String, IndexedNode<'a>>,
  ) {
    let mut owned_namespaces: Option<Vec<(String, String)>> = None;
    let mut namespaces = inherited_xmlns;

    if let crate::dom::DomNodeType::Element {
      namespace,
      attributes,
      ..
    } = &styled.node.node_type
    {
      if attributes.iter().any(|(name, _)| name.starts_with("xmlns")) {
        let mut updated = inherited_xmlns.to_vec();
        for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
          if !updated.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            updated.push((name.clone(), value.clone()));
          }
        }
        owned_namespaces = Some(updated);
        namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);
      }

      if namespace == SVG_NAMESPACE {
        if let Some(id) = styled.node.get_attribute_ref("id").filter(|id| !id.is_empty()) {
          if !out.contains_key(id) {
            out.insert(
              id.to_string(),
              IndexedNode {
                node: styled,
                namespaces: namespaces.to_vec(),
              },
            );
          }
        }
      }
    }

    for child in &styled.children {
      build_svg_id_index(child, namespaces, out);
    }
  }

  fn collect_referenced_svg_ids(styled: &StyledNode, in_svg_style: bool, out: &mut HashSet<String>) {
    match &styled.node.node_type {
      crate::dom::DomNodeType::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        let is_svg = namespace == SVG_NAMESPACE;
        let is_style = is_svg && tag_name.eq_ignore_ascii_case("style");
        let next_in_svg_style = in_svg_style || is_style;

        if is_svg {
          for (name, value) in attributes {
            if is_href_attr(name) {
              let trimmed = trim_ascii_whitespace(value);
              if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
                out.insert(id.to_string());
              }
            }
            extract_url_fragment_ids(value, out);
          }
        }

        for child in &styled.children {
          collect_referenced_svg_ids(child, next_in_svg_style, out);
        }
      }
      crate::dom::DomNodeType::Text { content } => {
        if in_svg_style {
          extract_url_fragment_ids(content, out);
        }
      }
      _ => {
        for child in &styled.children {
          collect_referenced_svg_ids(child, in_svg_style, out);
        }
      }
    }
  }

  let mut requested = HashSet::new();
  collect_requested_mask_ids(styled, &mut requested);
  if requested.is_empty() {
    return HashMap::new();
  }

  let mut index: HashMap<String, IndexedNode<'_>> = HashMap::new();
  build_svg_id_index(styled, &[], &mut index);
  if index.is_empty() {
    return HashMap::new();
  }

  let mut required: HashSet<String> = HashSet::new();
  let mut queue: VecDeque<String> = VecDeque::new();
  for id in requested {
    if index.contains_key(&id) && required.insert(id.clone()) {
      queue.push_back(id);
    }
  }

  while let Some(id) = queue.pop_front() {
    let Some(entry) = index.get(&id) else {
      continue;
    };
    let mut refs = HashSet::new();
    collect_referenced_svg_ids(entry.node, false, &mut refs);
    for reference in refs {
      if !index.contains_key(&reference) {
        continue;
      }
      if required.insert(reference.clone()) {
        queue.push_back(reference);
      }
    }
  }

  let mut defs = HashMap::new();
  for id in required {
    let Some(entry) = index.get(&id) else {
      continue;
    };
    let mut serialized = String::new();
    serialize_svg_mask_subtree_with_namespaces(
      entry.node,
      &entry.namespaces,
      None,
      None,
      true,
      &mut serialized,
    );
    defs.insert(id, serialized);
  }

  defs
}

fn format_css_color(color: crate::style::color::Rgba) -> String {
  format!(
    "rgba({},{},{},{:.3})",
    color.r,
    color.g,
    color.b,
    color.a.clamp(0.0, 1.0)
  )
}

fn foreign_object_css_limit_bytes() -> usize {
  const DEFAULT_LIMIT: usize = 256 * 1024;
  static LIMIT: OnceLock<usize> = OnceLock::new();

  *LIMIT.get_or_init(|| {
    std::env::var("FASTR_MAX_FOREIGN_OBJECT_CSS_BYTES")
      .ok()
      .and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
          return None;
        }
        trimmed.parse::<usize>().ok()
      })
      .unwrap_or(DEFAULT_LIMIT)
  })
}

fn box_debug_info_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();

  *ENABLED.get_or_init(|| {
    if runtime::runtime_toggles().truthy("FASTR_BOX_DEBUG_INFO") {
      return true;
    }
    cfg!(debug_assertions) || cfg!(test)
  })
}

fn serialize_svg_subtree(
  styled: &StyledNode,
  document_css: &str,
  svg_document_css_style_element: Option<&Arc<str>>,
) -> SvgContent {
  let profile_start = SVG_SERIALIZATION_PROFILE
    .with(|cell| cell.borrow().is_some())
    .then(Instant::now);

  fn root_style_base(style: &ComputedStyle) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(64);
    out.push_str("color: rgba(");
    let color = style.color;
    let _ = write!(
      &mut out,
      "{},{},{},{:.3}",
      color.r,
      color.g,
      color.b,
      color.a.clamp(0.0, 1.0)
    );
    out.push(')');

    if !style.font_family.is_empty() {
      out.push_str("; font-family: ");
      for (idx, family) in style.font_family.iter().enumerate() {
        if idx != 0 {
          out.push_str(", ");
        }
        if family.contains(' ') && !(family.starts_with('"') && family.ends_with('"')) {
          out.push('"');
          out.push_str(family);
          out.push('"');
        } else {
          out.push_str(family);
        }
      }
    }

    let _ = write!(&mut out, "; font-size: {:.2}px", style.font_size);
    let _ = write!(&mut out, "; font-weight: {}", style.font_weight.to_u16());
    match style.font_style {
      FontStyle::Italic => out.push_str("; font-style: italic"),
      FontStyle::Oblique(Some(angle)) => {
        let _ = write!(&mut out, "; font-style: oblique {}deg", angle);
      }
      FontStyle::Oblique(None) => out.push_str("; font-style: oblique"),
      FontStyle::Normal => {}
    }

    if style.opacity.is_finite() && style.opacity != 1.0 {
      out.push_str("; opacity: 1 !important");
    }

    out
  }

  fn root_style_includes_fill_current_color(attrs: &[(String, String)]) -> bool {
    if attrs
      .iter()
      .any(|(name, _)| name.eq_ignore_ascii_case("fill"))
    {
      return false;
    }
    let Some((_, style)) = attrs
      .iter()
      .find(|(name, _)| name.eq_ignore_ascii_case("style"))
    else {
      return true;
    };
    let declarations = crate::css::parser::parse_declarations(style);
    !declarations.iter().any(|decl| decl.property.as_str() == "fill")
  }

  let embed_document_css = svg_document_css_style_element.is_some();

  #[derive(Clone, Copy, Debug)]
  struct SvgViewportSize {
    width: f32,
    height: f32,
  }

  fn svg_attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
      .iter()
      .find(|(attr, _)| attr.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
  }

  fn resolve_svg_length_axis(length: SvgLength, axis_size: Option<f32>) -> Option<f32> {
    let value = match length {
      SvgLength::Px(px) => px,
      SvgLength::Percentage(pct) => axis_size? * (pct / 100.0),
    };
    value.is_finite().then_some(value)
  }

  fn resolve_svg_attr_length(value: &str, axis_size: Option<f32>) -> Option<f32> {
    parse_svg_length(value).and_then(|len| resolve_svg_length_axis(len, axis_size))
  }

  fn resolve_svg_viewport_size(
    attrs: &[(String, String)],
    parent: Option<SvgViewportSize>,
    is_root: bool,
  ) -> Option<SvgViewportSize> {
    if let Some(view_box) = svg_attr(attrs, "viewBox").and_then(parse_svg_view_box) {
      return Some(SvgViewportSize {
        width: view_box.width,
        height: view_box.height,
      });
    }

    if is_root {
      let width = svg_attr(attrs, "width")
        .and_then(parse_svg_length_px)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(300.0);
      let height = svg_attr(attrs, "height")
        .and_then(parse_svg_length_px)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(150.0);
      return Some(SvgViewportSize { width, height });
    }

    let parent = parent?;

    let width = svg_attr(attrs, "width")
      .and_then(|v| resolve_svg_attr_length(v, Some(parent.width)))
      .filter(|v| v.is_finite() && *v >= 0.0)
      .unwrap_or(parent.width);
    let height = svg_attr(attrs, "height")
      .and_then(|v| resolve_svg_attr_length(v, Some(parent.height)))
      .filter(|v| v.is_finite() && *v >= 0.0)
      .unwrap_or(parent.height);

    Some(SvgViewportSize { width, height })
  }

  fn svg_uses_xlink_prefix(node: &StyledNode) -> bool {
    fn attr_name_uses_xlink_prefix(name: &str) -> bool {
      const NEEDLE: &[u8] = b"xlink:";
      let bytes = name.as_bytes();
      if bytes.len() < NEEDLE.len() {
        return false;
      }
      for idx in 0..=bytes.len() - NEEDLE.len() {
        if bytes[idx].to_ascii_lowercase() != b'x' {
          continue;
        }
        if bytes[idx..idx + NEEDLE.len()]
          .iter()
          .zip(NEEDLE)
          .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
          return true;
        }
      }
      false
    }

    if let crate::dom::DomNodeType::Element { attributes, .. } = &node.node.node_type {
      if attributes
        .iter()
        .any(|(name, _)| attr_name_uses_xlink_prefix(name))
      {
        return true;
      }
    }

    node.children.iter().any(svg_uses_xlink_prefix)
  }

  let needs_xmlns_xlink = svg_uses_xlink_prefix(styled)
    && matches!(
      &styled.node.node_type,
      crate::dom::DomNodeType::Element { attributes, .. }
        if !attributes
          .iter()
          .any(|(name, _)| name.eq_ignore_ascii_case("xmlns:xlink"))
    );

  fn serialize_foreign_object_placeholder(
    styled: &StyledNode,
    attrs: &[(String, String)],
    viewport: Option<SvgViewportSize>,
    out: &mut String,
  ) -> bool {
    let mut x = 0.0f32;
    let mut y = 0.0f32;
    let mut width: Option<f32> = None;
    let mut height: Option<f32> = None;
    for (name, value) in attrs {
      match name.as_str() {
        "x" => x = resolve_svg_attr_length(value, viewport.map(|v| v.width)).unwrap_or(0.0),
        "y" => y = resolve_svg_attr_length(value, viewport.map(|v| v.height)).unwrap_or(0.0),
        "width" => width = resolve_svg_attr_length(value, viewport.map(|v| v.width)),
        "height" => height = resolve_svg_attr_length(value, viewport.map(|v| v.height)),
        _ => {}
      }
    }

    let (width, height) = match (width, height) {
      (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
      _ => return false,
    };

    let mut fill = None;
    let mut text_color = None;
    let mut font_size = None;
    let mut text_content: Option<String> = None;

    for child in &styled.children {
      if child.styles.background_color.a > 0.0 {
        fill = Some(child.styles.background_color);
      }
      if text_color.is_none() {
        text_color = Some(child.styles.color);
        font_size = Some(child.styles.font_size);
      }
      if text_content.is_none() {
        if let Some(text) = child.node.text_content() {
          let trimmed = trim_ascii_whitespace(text);
          if !trimmed.is_empty() {
            text_content = Some(trimmed.to_string());
          }
        }
      }
      if fill.is_some() && text_content.is_some() {
        break;
      }
    }

    let has_fill = fill.is_some();
    let has_text = text_content.is_some();
    if !has_fill && !has_text {
      return false;
    }

    out.push_str("<g>");
    if let Some(color) = fill {
      out.push_str(&format!(
        "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" fill=\"{}\" />",
        x,
        y,
        width,
        height,
        format_css_color(color)
      ));
    }

    if let (Some(text), Some(color), Some(size)) = (text_content, text_color, font_size) {
      let baseline = y + size;
      out.push_str(&format!(
        "<text x=\"{:.3}\" y=\"{:.3}\" fill=\"{}\" font-size=\"{:.3}px\">{}</text>",
        x,
        baseline,
        format_css_color(color),
        size,
        escape_text(&text)
      ));
    }

    out.push_str("</g>");
    true
  }

  fn serialize_foreign_object(
    styled: &StyledNode,
    attrs: &[(String, String)],
    viewport: Option<SvgViewportSize>,
    _document_css: &str,
    out: &mut String,
    fallback_out: &mut Option<String>,
    foreign_objects: &mut Vec<ForeignObjectInfo>,
  ) -> bool {
    if styled.styles.display == Display::None
      || matches!(
        styled.styles.visibility,
        Visibility::Hidden | Visibility::Collapse
      )
      || (styled.styles.opacity.is_finite() && styled.styles.opacity <= 0.0)
    {
      return true;
    }

    // ForeignObject output can diverge between the primary SVG (placeholder comment for later
    // replacement) and the fallback SVG (best-effort placeholder rendering). Only allocate and
    // populate the fallback buffer once we know we need it.
    let fallback_out = fallback_out.get_or_insert_with(|| out.clone());

    let mut x = 0.0f32;
    let mut y = 0.0f32;
    let mut width: Option<f32> = None;
    let mut height: Option<f32> = None;
    for (name, value) in attrs {
      match name.as_str() {
        "x" => x = resolve_svg_attr_length(value, viewport.map(|v| v.width)).unwrap_or(0.0),
        "y" => y = resolve_svg_attr_length(value, viewport.map(|v| v.height)).unwrap_or(0.0),
        "width" => width = resolve_svg_attr_length(value, viewport.map(|v| v.width)),
        "height" => height = resolve_svg_attr_length(value, viewport.map(|v| v.height)),
        _ => {}
      }
    }

    let (width, height) = match (width, height) {
      (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
      _ => {
        let placeholder = serialize_foreign_object_placeholder(styled, attrs, viewport, out);
        let _ = serialize_foreign_object_placeholder(styled, attrs, viewport, fallback_out);
        if placeholder {
          return true;
        }
        out.push_str("<!--FASTRENDER_FOREIGN_OBJECT_UNRESOLVED-->");
        fallback_out.push_str("<!--FASTRENDER_FOREIGN_OBJECT_UNRESOLVED-->");
        return true;
      }
    };

    let placeholder = format!("<!--FASTRENDER_FOREIGN_OBJECT_{}-->", foreign_objects.len());
    out.push_str(&placeholder);
    if !serialize_foreign_object_placeholder(styled, attrs, viewport, fallback_out) {
      fallback_out.push_str(&placeholder);
    }

    let mut html = String::new();
    for child in &styled.children {
      serialize_styled_dom_subtree_html(child, &mut html);
    }

    let background = if styled.styles.background_color.a > 0.0 {
      Some(styled.styles.background_color)
    } else {
      None
    };

    foreign_objects.push(ForeignObjectInfo {
      placeholder,
      attributes: attrs.to_vec(),
      x,
      y,
      width,
      height,
      opacity: styled.styles.opacity,
      background,
      html,
      style: Arc::clone(&styled.styles),
      overflow_x: styled.styles.overflow_x,
      overflow_y: styled.styles.overflow_y,
    });

    true
  }

  fn serialize_node(
    styled: &StyledNode,
    document_css: &str,
    parent_ns: Option<&str>,
    parent_svg_styles: Option<&ComputedStyle>,
    svg_viewport: Option<SvgViewportSize>,
    needs_xmlns_xlink: bool,
    is_root: bool,
    out: &mut String,
    fallback_out: &mut Option<String>,
    foreign_objects: &mut Vec<ForeignObjectInfo>,
    record_document_css: bool,
    document_css_insert_pos: &mut Option<usize>,
  ) {
    match &styled.node.node_type {
      crate::dom::DomNodeType::Document { .. } | crate::dom::DomNodeType::ShadowRoot { .. } => {
        for child in &styled.children {
          serialize_node(
            child,
            document_css,
            parent_ns,
            parent_svg_styles,
            svg_viewport,
            needs_xmlns_xlink,
            false,
            out,
            fallback_out,
            foreign_objects,
            record_document_css,
            document_css_insert_pos,
          );
        }
      }
      crate::dom::DomNodeType::Slot { .. } => {
        for child in &styled.children {
          serialize_node(
            child,
            document_css,
            parent_ns,
            parent_svg_styles,
            svg_viewport,
            needs_xmlns_xlink,
            false,
            out,
            fallback_out,
            foreign_objects,
            record_document_css,
            document_css_insert_pos,
          );
        }
      }
      crate::dom::DomNodeType::Text { content } => {
        push_escaped_text(out, content);
        if let Some(fallback_out) = fallback_out.as_mut() {
          push_escaped_text(fallback_out, content);
        }
      }
      crate::dom::DomNodeType::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        let mut current_ns = namespace.as_str();
        if is_root && current_ns.is_empty() {
          current_ns = SVG_NAMESPACE;
        } else if current_ns.is_empty() {
          if let Some(parent_ns) = parent_ns {
            current_ns = parent_ns;
          }
        }

        let next_parent_svg_styles = if current_ns == SVG_NAMESPACE {
          Some(&*styled.styles)
        } else {
          parent_svg_styles
        };

        let mut owned_attrs: Option<Vec<(String, String)>> = None;
        if is_root {
          let include_fill_current_color = root_style_includes_fill_current_color(attributes);
          let mut attrs = attributes.clone();
          let has_xmlns = attrs
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("xmlns"));
          if !has_xmlns {
            attrs.push(("xmlns".to_string(), current_ns.to_string()));
          }
          if needs_xmlns_xlink
            && !attrs
              .iter()
              .any(|(name, _)| name.eq_ignore_ascii_case("xmlns:xlink"))
          {
            attrs.push((
              "xmlns:xlink".to_string(),
              "http://www.w3.org/1999/xlink".to_string(),
            ));
          }

          let style_attr = root_style_base(&styled.styles);
          merge_style_attribute(&mut attrs, &style_attr);
          if include_fill_current_color {
            // Make unstyled shapes pick up the computed text color (common for icon SVGs).
            merge_style_attribute(&mut attrs, "fill: currentColor");
          }
          owned_attrs = Some(attrs);
        } else if !current_ns.is_empty() && parent_ns != Some(current_ns) {
          let has_xmlns = attributes
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("xmlns"));
          if !has_xmlns {
            let mut attrs = attributes.clone();
            attrs.push(("xmlns".to_string(), current_ns.to_string()));
            owned_attrs = Some(attrs);
          }
        }

        if current_ns == SVG_NAMESPACE {
          if let Some(extra) = svg_presentation_style(&styled.styles, parent_svg_styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if !is_root {
            if let Some(extra) = svg_paint_style(&styled.styles, parent_svg_styles) {
              let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
              merge_style_attribute(attrs_mut, &extra);
            }
            let has_transform_attr = owned_attrs
              .as_deref()
              .unwrap_or(attributes)
              .iter()
              .any(|(name, _)| name.eq_ignore_ascii_case("transform"));
            if !has_transform_attr {
              if let Some(transform) = svg_transform_attribute(&styled.styles) {
                let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
                attrs_mut.push(("transform".to_string(), transform));
              }
            }
          }
        }

        let attrs: &[(String, String)] = owned_attrs.as_deref().unwrap_or(attributes);

        let next_svg_viewport =
          if current_ns == SVG_NAMESPACE && tag_name.eq_ignore_ascii_case("svg") {
            resolve_svg_viewport_size(attrs, svg_viewport, is_root)
          } else {
            svg_viewport
          };

        if tag_name.eq_ignore_ascii_case("foreignObject") {
          if serialize_foreign_object(
            styled,
            attrs,
            svg_viewport,
            document_css,
            out,
            fallback_out,
            foreign_objects,
          ) {
            return;
          }
        }

        out.push('<');
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push('<');
        }
        out.push_str(tag_name);
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push_str(tag_name);
        }
        for (name, value) in attrs {
          out.push(' ');
          if let Some(fallback_out) = fallback_out.as_mut() {
            fallback_out.push(' ');
          }
          out.push_str(name);
          if let Some(fallback_out) = fallback_out.as_mut() {
            fallback_out.push_str(name);
          }
          out.push('=');
          if let Some(fallback_out) = fallback_out.as_mut() {
            fallback_out.push('=');
          }
          out.push('"');
          if let Some(fallback_out) = fallback_out.as_mut() {
            fallback_out.push('"');
          }
          push_escaped_attr(out, value);
          if let Some(fallback_out) = fallback_out.as_mut() {
            push_escaped_attr(fallback_out, value);
          }
          out.push('"');
          if let Some(fallback_out) = fallback_out.as_mut() {
            fallback_out.push('"');
          }
        }
        out.push('>');
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push('>');
        }

        if is_root {
          if record_document_css && document_css_insert_pos.is_none() {
            *document_css_insert_pos = Some(out.len());
          }
        }

        for child in &styled.children {
          serialize_node(
            child,
            document_css,
            Some(current_ns),
            next_parent_svg_styles,
            next_svg_viewport,
            needs_xmlns_xlink,
            false,
            out,
            fallback_out,
            foreign_objects,
            record_document_css,
            document_css_insert_pos,
          );
        }

        out.push_str("</");
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push_str("</");
        }
        out.push_str(tag_name);
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push_str(tag_name);
        }
        out.push('>');
        if let Some(fallback_out) = fallback_out.as_mut() {
          fallback_out.push('>');
        }
      }
    }
  }

  let mut out = String::new();
  let mut fallback_out = None;
  let mut foreign_objects: Vec<ForeignObjectInfo> = Vec::new();
  let mut document_css_insert_pos = None;
  serialize_node(
    styled,
    document_css,
    None,
    None,
    None,
    needs_xmlns_xlink,
    true,
    &mut out,
    &mut fallback_out,
    &mut foreign_objects,
    embed_document_css,
    &mut document_css_insert_pos,
  );

  let fallback_svg = if foreign_objects.is_empty() {
    String::new()
  } else {
    fallback_out.unwrap_or_else(|| out.clone())
  };

  let shared_css = if !foreign_objects.is_empty()
    && document_css.as_bytes().len() <= foreign_object_css_limit_bytes()
  {
    document_css.to_string()
  } else {
    String::new()
  };

  let document_css_injection = if embed_document_css {
    match (svg_document_css_style_element, document_css_insert_pos) {
      (Some(style_element), Some(insert_pos)) => Some(SvgDocumentCssInjection {
        style_element: Arc::clone(style_element),
        insert_pos,
      }),
      _ => None,
    }
  } else {
    None
  };

  let content = SvgContent {
    svg: out,
    fallback_svg,
    foreign_objects,
    shared_css,
    document_css_injection,
  };

  if let Some(start) = profile_start {
    record_svg_serialization(
      start.elapsed(),
      content.svg.len() + content.fallback_svg.len(),
    );
  }

  content
}

/// Generates BoxNodes from a StyledNode, honoring display: contents by splicing grandchildren into
/// the parent’s child list rather than creating a box.
fn generate_boxes_for_styled_into(
  styled: &StyledNode,
  styled_lookup: &StyledLookup<'_>,
  counters: &mut CounterManager,
  _is_root: bool,
  document_css: &str,
  svg_document_css_style_element: Option<&Arc<str>>,
  picture_sources: &mut PictureSourceLookup,
  options: &BoxGenerationOptions,
  deadline_counter: &mut usize,
  out: &mut Vec<BoxNode>,
) -> Result<()> {
  // Avoid recursion for extremely deep trees.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum FrameState {
    Enter,
    Children,
    Finish,
  }

  struct Frame<'a> {
    styled: &'a StyledNode,
    state: FrameState,
    entered_counter_scope: bool,
    entered_style_containment_scope: bool,
    quote_containment_snapshot: Option<usize>,
    in_footnote: bool,
    force_position_relative: bool,
    composed_children: Option<ComposedChildren<'a>>,
    child_idx: usize,
    pending_children: Vec<&'a StyledNode>,
    children: Vec<BoxNode>,
  }

  impl<'a> Frame<'a> {
    fn new(styled: &'a StyledNode, in_footnote: bool) -> Self {
      Self {
        styled,
        state: FrameState::Enter,
        entered_counter_scope: false,
        entered_style_containment_scope: false,
        quote_containment_snapshot: None,
        in_footnote,
        force_position_relative: false,
        composed_children: None,
        child_idx: 0,
        pending_children: Vec::new(),
        children: Vec::new(),
      }
    }
  }

  let site_compat = options.site_compat_hacks_enabled();
  let mut quote_depth = 0usize;

  fn nearest_non_contents_container_display<'a>(stack: &[Frame<'a>]) -> Option<Display> {
    for frame in stack.iter().rev() {
      let display = frame.styled.styles.display;
      if display != Display::Contents {
        return Some(display);
      }
    }
    None
  }

  fn blockify_flex_or_grid_item_display(display: Display) -> Display {
    // CSS Display Level 3: When a box is a flex/grid item, its outer display type is blockified.
    //
    // https://www.w3.org/TR/css-display-3/#transformations
    match display {
      Display::Inline => Display::Block,
      Display::InlineBlock => Display::FlowRoot,
      Display::InlineFlex => Display::Flex,
      Display::InlineGrid => Display::Grid,
      Display::InlineTable => Display::Table,
      // FastRender models ruby as inline-level flow boxes; blockify to a plain block.
      Display::Ruby
      | Display::RubyBase
      | Display::RubyText
      | Display::RubyBaseContainer
      | Display::RubyTextContainer => Display::Block,
      _ => display,
    }
  }

  fn blockify_style_for_flex_or_grid_item_if_needed<'a>(
    style: &Arc<ComputedStyle>,
    stack: &[Frame<'a>],
  ) -> Arc<ComputedStyle> {
    // Absolutely positioned children of flex/grid containers are out-of-flow and do not become
    // flex/grid items, so blockification does not apply.
    if matches!(style.position, Position::Absolute | Position::Fixed) {
      return Arc::clone(style);
    }

    let container_display = nearest_non_contents_container_display(stack);
    if !matches!(
      container_display,
      Some(Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid)
    ) {
      return Arc::clone(style);
    }

    let blockified = blockify_flex_or_grid_item_display(style.display);
    if blockified == style.display {
      return Arc::clone(style);
    }

    let mut owned = (**style).clone();
    owned.display = blockified;
    Arc::new(owned)
  }
  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame::new(styled, false));

  while let Some(state) = stack.last().map(|frame| frame.state) {
    match state {
      FrameState::Enter => {
        let styled = stack.last().expect("frame exists").styled;

        check_active_periodic(
          deadline_counter,
          BOX_GEN_DEADLINE_STRIDE,
          RenderStage::BoxTree,
        )?;

        if let Some(text) = styled.node.text_content() {
          // Text nodes don't participate in the CSS cascade directly, but internal UA-style
          // behaviors (e.g. closed `<details>`) may still suppress them by setting their computed
          // `display` to `none`. Honor that here so hidden text never generates fragments.
          if styled.styles.display == Display::None {
            stack.pop();
            continue;
          }
          if !text.is_empty() {
            let is_whitespace_only = text.as_bytes().iter().all(|b| b.is_ascii_whitespace());
            if is_whitespace_only
              && matches!(
                styled.styles.white_space,
                WhiteSpace::Normal | WhiteSpace::Nowrap
              )
            {
              // Flex/grid containers treat direct text runs as anonymous items. Collapsible
              // inter-element whitespace should not generate those items, otherwise `gap` and
              // auto-placement treat them as real children.
              let mut container_display = None;
              for frame in stack.iter().rev().skip(1) {
                let display = frame.styled.styles.display;
                if display != Display::Contents {
                  container_display = Some(display);
                  break;
                }
              }
              if matches!(
                container_display,
                Some(Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid)
              ) {
                stack.pop();
                continue;
              }
            }
            let style = Arc::clone(&styled.styles);
            if let Some(needle) = runtime::runtime_toggles().get("FASTR_FIND_BOX_TEXT") {
              if text.contains(&needle) {
                eprintln!(
                  "[box-gen-text] styled_node_id={} tag={} display={:?} text={:?}",
                  styled.node_id,
                  styled.node.tag_name().unwrap_or("#text"),
                  styled.styles.display,
                  text
                );
              }
            }
            let mut box_node = BoxNode::new_text(style, text.to_string());
            box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
            let box_node = attach_styled_id(box_node, styled);

            stack.pop();
            if let Some(parent) = stack.last_mut() {
              parent.children.push(box_node);
            } else {
              out.push(box_node);
            }
            continue;
          }
        }

        // When HTML parsing is in "scripting enabled" mode, `<noscript>` represents nothing and
        // should not contribute boxes, even if author styles attempt to force it visible.
        if options.dom_scripting_enabled {
          if let DomNodeType::Element {
            tag_name,
            namespace,
            ..
          } = &styled.node.node_type
          {
            if tag_name.eq_ignore_ascii_case("noscript")
              && (namespace.is_empty() || namespace == HTML_NAMESPACE)
            {
              stack.pop();
              continue;
            }
          }
        }

        let in_footnote = stack
          .last()
          .map(|frame| frame.in_footnote)
          .unwrap_or(false);

        counters.enter_scope();
        apply_counter_properties_from_style(styled, counters, in_footnote, options.enable_footnote_floats);
        if let Some(frame) = stack.last_mut() {
          frame.entered_counter_scope = true;
        }

        // Common ad placeholders that hold space even when empty: drop when they have no children/content.
        if site_compat {
          if let Some(class_attr) = styled.node.get_attribute_ref("class") {
            if styled.children.is_empty()
              && (class_attr.contains("ad-height-hold")
                || class_attr.contains("ad__slot")
                || class_attr.contains("should-hold-space"))
            {
              stack.pop().expect("frame exists");
              counters.leave_scope();
              continue;
            }
          }
        }

        // display:none suppresses box generation entirely.
        if styled.styles.display == Display::None {
          stack.pop().expect("frame exists");
          counters.leave_scope();
          continue;
        }

        // HTML <br> elements represent forced line breaks. Model them explicitly so inline layout can
        // force a new line even under `white-space: normal/nowrap` (i.e., without relying on a
        // newline character that could be collapsed to a space).
        if let Some(tag) = styled.node.tag_name() {
          if tag.eq_ignore_ascii_case("br") {
            stack.pop().expect("frame exists");
            counters.leave_scope();
            let mut box_node = BoxNode::new_line_break(Arc::clone(&styled.styles));
            box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
            let box_node = attach_debug_info(box_node, styled);
            if let Some(parent) = stack.last_mut() {
              parent.children.push(box_node);
            } else {
              out.push(box_node);
            }
            continue;
          }
        }

        if let Some(tag) = styled.node.tag_name() {
          if tag.eq_ignore_ascii_case("math") {
            let dom_subtree = dom_subtree_from_styled(styled);
            let math_root = crate::math::parse_mathml(&dom_subtree)
              .unwrap_or_else(|| crate::math::MathNode::Row(Vec::new()));
            stack.pop().expect("frame exists");
            counters.leave_scope();
            let style = blockify_style_for_flex_or_grid_item_if_needed(&styled.styles, &stack);
            let box_node = BoxNode::new_replaced(
              style,
              ReplacedType::Math(MathReplaced {
                root: math_root,
                layout: None,
              }),
              None,
              None,
            );
            let mut box_node = box_node;
            box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
            let box_node = attach_debug_info(box_node, styled);
            if let Some(parent) = stack.last_mut() {
              parent.children.push(box_node);
            } else {
              out.push(box_node);
            }
            continue;
          }
        }

        // Native input/textarea/select controls render as replaced elements with intrinsic sizing
        // and native painting. (HTML <button> is intentionally *not* a replaced box so its
        // descendants can participate in layout, e.g. inline-flex icon+text buttons.)
        let mut appearance_none_form_control: Option<FormControl> = None;
        if let Some(form_control) = create_form_control_replaced(styled) {
          if !matches!(form_control.appearance, crate::style::types::Appearance::None) {
            stack.pop().expect("frame exists");
            counters.leave_scope();
            let style = blockify_style_for_flex_or_grid_item_if_needed(&styled.styles, &stack);
            let box_node = BoxNode::new_replaced(
              style,
              ReplacedType::FormControl(form_control),
              None,
              None,
            );
            let box_node = attach_debug_info(box_node, styled);
            if let Some(parent) = stack.last_mut() {
              parent.children.push(box_node);
            } else {
              out.push(box_node);
            }
            continue;
          }
          appearance_none_form_control = Some(form_control);
        }

        // Replaced elements short-circuit to a single replaced box unless they're display: contents.
        if let Some(tag) = styled.node.tag_name() {
          // Non-rendered elements: <source>, <track>, <option>, <optgroup> never create boxes.
          if tag.eq_ignore_ascii_case("source")
            || tag.eq_ignore_ascii_case("track")
            || tag.eq_ignore_ascii_case("option")
            || tag.eq_ignore_ascii_case("optgroup")
          {
            stack.pop().expect("frame exists");
            counters.leave_scope();
            continue;
          }

          let is_input_image = tag.eq_ignore_ascii_case("input")
            && styled
              .node
              .get_attribute_ref("type")
              .is_some_and(|input_type| input_type.eq_ignore_ascii_case("image"));

          if (is_replaced_element(tag) || is_input_image) && styled.styles.display != Display::Contents {
            let picture_sources_for_img = if tag.eq_ignore_ascii_case("img") {
              picture_sources.take(styled.node_id)
            } else {
              Vec::new()
            };
            let ancestor_len = stack.len().saturating_sub(1);
            let style = blockify_style_for_flex_or_grid_item_if_needed(
              &styled.styles,
              &stack[..ancestor_len],
            );
            if let Some(box_node) = create_replaced_box_from_styled(
              styled,
              style,
              document_css,
              svg_document_css_style_element,
              picture_sources_for_img,
              site_compat,
            ) {
              stack.pop().expect("frame exists");
              counters.leave_scope();
              let mut box_node = box_node;
              box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
              let box_node = attach_debug_info(box_node, styled);
              if let Some(parent) = stack.last_mut() {
                parent.children.push(box_node);
              } else {
                out.push(box_node);
              }
              continue;
            }
          }
        }

        // Generate leading pseudo-elements in tree order before descending into children so their
        // counter effects are visible to descendants.
        //
        // NOTE: `::marker` and `::before` are both before the element's DOM children. `::after` is
        // generated in `FrameState::Finish` after children have been processed.
        //
        // CSS Containment: `contain: style` (including implied style containment from
        // `content-visibility:auto|hidden`) scopes counter increments/sets and quote depth changes
        // to the element's subtree. For subtree scoping, the element itself is considered outside
        // the boundary; we enter the containment scope after applying the element's own counter
        // properties and keep it active for pseudo-elements and children.
        if styled.styles.containment.style && styled.styles.display != Display::Contents {
          let frame = stack.last_mut().expect("frame exists");
          counters.enter_style_containment();
          frame.entered_style_containment_scope = true;
          frame.quote_containment_snapshot = Some(quote_depth);
        }
        let marker_box = if styled.styles.display == Display::ListItem {
          create_marker_box(styled, counters, &mut quote_depth)
        } else {
          None
        };
        let before_box = if let Some(before_styles) = &styled.before_styles {
          let before_start = clone_starting_style(&styled.starting_styles.before);
          create_pseudo_element_box(
            styled,
            before_styles,
            before_start,
            "before",
            counters,
            &mut quote_depth,
          )
        } else {
          None
        };

        let (fallback_children, suppress_dom_children, force_position_relative) =
          if let Some(form_control) = appearance_none_form_control.as_ref() {
            build_appearance_none_form_control_fallback(styled, form_control)
          } else {
            (Vec::new(), false, false)
          };

        let composed_children = if suppress_dom_children {
          ComposedChildren::Slice(&[])
        } else {
          composed_children(styled, styled_lookup)
        };
        let composed_len = composed_children.len();
        let frame = stack.last_mut().expect("frame exists");
        frame.composed_children = Some(composed_children);
        frame.children = Vec::with_capacity(composed_len + 3 + fallback_children.len());
        frame.child_idx = 0;
        frame.pending_children.clear();
        frame.force_position_relative = force_position_relative;
        if let Some(marker_box) = marker_box {
          frame.children.push(marker_box);
        }
        if let Some(before_box) = before_box {
          frame.children.push(before_box);
        }
        frame.children.extend(fallback_children);
        frame.state = FrameState::Children;
      }
      FrameState::Children => {
        let mut next_child: Option<&StyledNode> = None;

        {
          let frame = stack.last_mut().expect("frame exists");
          if let Some(child) = frame.pending_children.pop() {
            next_child = Some(child);
          } else {
            let composed_children = frame
              .composed_children
              .as_ref()
              .expect("children state always has composed children");
            let composed_len = composed_children.len();
            if frame.child_idx >= composed_len {
              frame.state = FrameState::Finish;
            } else {
              let child = composed_children.get(frame.child_idx);
              if site_compat {
                if let Some(testid) = child.node.get_attribute_ref("data-testid") {
                  if testid == "one-nav-overlay" {
                    let overlay_hidden = matches!(child.styles.visibility, Visibility::Hidden)
                      || child.styles.opacity == 0.0;
                    if overlay_hidden {
                      // Skip the overlay and the subsequent focus-trap container when the overlay
                      // is hidden (menu closed).
                      let mut idx = frame.child_idx + 1;
                      while idx < composed_len {
                        let next = composed_children.get(idx);
                        // Skip over whitespace/text nodes between the overlay and drawer.
                        if let crate::dom::DomNodeType::Text { content } = &next.node.node_type {
                          if trim_ascii_whitespace(content).is_empty() {
                            idx += 1;
                            continue;
                          }
                        }

                        if let Some(class_attr) = next.node.get_attribute_ref("class") {
                          if class_attr.contains("FocusTrapContainer-") {
                            for grandchild in next.children.iter().rev() {
                              frame.pending_children.push(grandchild);
                            }
                            idx += 1;
                          }
                        }
                        break;
                      }
                      frame.child_idx = idx;
                      continue;
                    }
                  }
                }
              }

              frame.child_idx += 1;
              next_child = Some(child);
            }
          }
        }
 
        if let Some(child) = next_child {
          let child_in_footnote = stack
            .last()
            .map(|parent| {
              parent.in_footnote
                || (options.enable_footnote_floats && parent.styled.styles.float == Float::Footnote)
            })
            .unwrap_or(false);
          stack.push(Frame::new(child, child_in_footnote));
        }
      }
      FrameState::Finish => {
        let frame = stack.pop().expect("frame exists");
        debug_assert!(frame.entered_counter_scope);
        let in_footnote = frame.in_footnote;
        let force_position_relative = frame.force_position_relative;
        let styled = frame.styled;
        let mut children = frame.children;
        let entered_style_containment_scope = frame.entered_style_containment_scope;
        let quote_containment_snapshot = frame.quote_containment_snapshot;
        if let Some(after_styles) = &styled.after_styles {
          let after_start = clone_starting_style(&styled.starting_styles.after);
          if let Some(after_box) = create_pseudo_element_box(
            styled,
            after_styles,
            after_start,
            "after",
            counters,
            &mut quote_depth,
          ) {
            children.push(after_box);
          }
        }

        // display: contents contributes its children directly.
        if styled.styles.display == Display::Contents {
          if entered_style_containment_scope {
            counters.leave_style_containment();
            if let Some(snapshot) = quote_containment_snapshot {
              quote_depth = snapshot;
            }
          }
          counters.leave_scope();
          if let Some(backdrop_box) = create_backdrop_box(styled) {
            if let Some(parent) = stack.last_mut() {
              parent.children.push(backdrop_box);
            } else {
              out.push(backdrop_box);
            }
          }
          if let Some(top_layer) = styled.styles.top_layer {
            // Top-layer elements are modeled as stacking contexts at paint time. When the element is
            // `display: contents` it generates no box, so its descendants would otherwise remain in
            // the normal document stacking order and end up behind `::backdrop`. Promote the
            // generated child boxes into the top layer so they paint above the backdrop.
            for child in children.iter_mut() {
              if child.style.top_layer.is_none() {
                let mut style = child.style.as_ref().clone();
                style.top_layer = Some(top_layer);
                child.style = Arc::new(style);
              }
            }
          }
          if let Some(parent) = stack.last_mut() {
            parent.children.extend(children);
          } else {
            out.extend(children);
          }
          continue;
        }

        let base_style = if force_position_relative && styled.styles.position == Position::Static {
          let mut patched = styled.styles.as_ref().clone();
          patched.position = Position::Relative;
          Arc::new(patched)
        } else {
          Arc::clone(&styled.styles)
        };

        // HTML fieldset/legend rendering model:
        // - Separate the first `<legend>` element child (if any) so it can be positioned on the
        //   fieldset border.
        // - Wrap remaining children in an anonymous "fieldset content" box.
        if let DomNodeType::Element {
          tag_name,
          namespace,
          ..
        } = &styled.node.node_type
        {
          if namespace == HTML_NAMESPACE && tag_name.eq_ignore_ascii_case("fieldset") {
            let is_legend_child = |node: &BoxNode| -> bool {
              if node.generated_pseudo.is_some() {
                return false;
              }
              let Some(styled_id) = node.styled_node_id else {
                return false;
              };
              let Some(styled_node) = styled_lookup.get(styled_id) else {
                return false;
              };
              match &styled_node.node.node_type {
                DomNodeType::Element { tag_name, namespace, .. } => {
                  namespace == HTML_NAMESPACE && tag_name.eq_ignore_ascii_case("legend")
                }
                _ => false,
              }
            };

            let legend_index = children.iter().position(is_legend_child);
            let mut legend = legend_index.map(|idx| children.remove(idx));
            if let Some(legend) = legend.as_mut() {
              // Legends size to their contents even when they are blocks (`width: auto` behaves like
              // shrink-to-fit). Preserve authored CSS but apply the internal flag that enables the
              // sizing behavior in block layout.
              Arc::make_mut(&mut legend.style).shrink_to_fit_inline_size = true;
            }

            let mut wrapper_style = inherited_style(base_style.as_ref());
            wrapper_style.display = Display::Block;
            let wrapper = BoxNode::new_anonymous_fieldset_content(Arc::new(wrapper_style), children);

            children = if let Some(legend) = legend {
              vec![legend, wrapper]
            } else {
              vec![wrapper]
            };
          }
        }

        let style = blockify_style_for_flex_or_grid_item_if_needed(&base_style, &stack);
        let display = style.display;
        let fc_type = display
          .formatting_context_type()
          .unwrap_or(FormattingContextType::Block);

        let mut box_node = match display {
          Display::Block | Display::FlowRoot | Display::ListItem => {
            BoxNode::new_block(style, fc_type, children)
          }
          Display::Inline
          | Display::Ruby
          | Display::RubyBase
          | Display::RubyText
          | Display::RubyBaseContainer
          | Display::RubyTextContainer => BoxNode::new_inline(style, children),
          Display::InlineBlock => BoxNode::new_inline_block(style, fc_type, children),
          Display::Flex => BoxNode::new_block(style, FormattingContextType::Flex, children),
          Display::InlineFlex => {
            BoxNode::new_inline_block(style, FormattingContextType::Flex, children)
          }
          Display::Grid => BoxNode::new_block(style, FormattingContextType::Grid, children),
          Display::InlineGrid => {
            BoxNode::new_inline_block(style, FormattingContextType::Grid, children)
          }
          Display::Table => BoxNode::new_block(style, FormattingContextType::Table, children),
          Display::InlineTable => {
            BoxNode::new_inline_block(style, FormattingContextType::Table, children)
          }
          // Table-internal boxes (simplified for Wave 2)
          Display::TableRow
          | Display::TableCell
          | Display::TableRowGroup
          | Display::TableHeaderGroup
          | Display::TableFooterGroup
          | Display::TableColumn
          | Display::TableColumnGroup
          | Display::TableCaption => {
            BoxNode::new_block(style, FormattingContextType::Block, children)
          }
          Display::None | Display::Contents => unreachable!("handled above"),
        };

        box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
        box_node.first_line_style = styled.first_line_styles.as_ref().map(Arc::clone);
        box_node.first_letter_style = styled.first_letter_styles.as_ref().map(Arc::clone);

        if options.enable_footnote_floats && styled.styles.float == Float::Footnote && !in_footnote {
          let mut body_box = box_node;
          // The footnote body is laid out in the per-page footnote area; it should not itself be a
          // footnote float.
          if body_box.style.float == Float::Footnote {
            let mut body_style = body_box.style.as_ref().clone();
            body_style.float = Float::None;
            body_box.style = Arc::new(body_style);
          }

          if let Some(marker_styles) = &styled.footnote_marker_styles {
            let marker_start = clone_starting_style(&styled.starting_styles.footnote_marker);
            if let Some(marker_box) = create_pseudo_element_box(
              styled,
              marker_styles,
              marker_start,
              "footnote-marker",
              counters,
              &mut quote_depth,
            ) {
              let mut combined = Vec::with_capacity(body_box.children.len() + 1);
              combined.push(marker_box);
              combined.append(&mut body_box.children);
              body_box.children = combined;
            }
          }

          let body_box = attach_debug_info(body_box, styled);

          let call_start = clone_starting_style(&styled.starting_styles.footnote_call);
          let mut call_box = styled
            .footnote_call_styles
            .as_ref()
            .and_then(|styles| {
              create_pseudo_element_box(
                styled,
                styles,
                call_start.clone(),
                "footnote-call",
                counters,
                &mut quote_depth,
              )
            })
            .unwrap_or_else(|| {
              // If the call pseudo has `content: normal/none`, still insert a zero-width anchor so
              // the footnote can be placed deterministically.
              let mut anchor_style = styled.styles.as_ref().clone();
              anchor_style.float = Float::None;
              anchor_style.display = Display::Inline;
              let mut node = BoxNode::new_inline(Arc::new(anchor_style), Vec::new());
              node.starting_style = call_start.clone();
              node.styled_node_id = Some(styled.node_id);
              node.generated_pseudo = Some(GeneratedPseudoElement::FootnoteCall);
              node
            });
          call_box.footnote_body = Some(Box::new(body_box));

          if entered_style_containment_scope {
            counters.leave_style_containment();
            if let Some(snapshot) = quote_containment_snapshot {
              quote_depth = snapshot;
            }
          }
          counters.leave_scope();
          if let Some(parent) = stack.last_mut() {
            parent.children.push(call_box);
          } else {
            out.push(call_box);
          }
          continue;
        }

        if entered_style_containment_scope {
          counters.leave_style_containment();
          if let Some(snapshot) = quote_containment_snapshot {
            quote_depth = snapshot;
          }
        }
        counters.leave_scope();
        let box_node = attach_debug_info(box_node, styled);
        if let Some(parent) = stack.last_mut() {
          if let Some(backdrop_box) = create_backdrop_box(styled) {
            parent.children.push(backdrop_box);
          }
          parent.children.push(box_node);
        } else {
          if let Some(backdrop_box) = create_backdrop_box(styled) {
            out.push(backdrop_box);
          }
          out.push(box_node);
        }
      }
    }
  }

  Ok(())
}

fn attach_debug_info(mut box_node: BoxNode, styled: &StyledNode) -> BoxNode {
  box_node.styled_node_id = Some(styled.node_id);
  const HTML_TABLE_SPAN_MAX: u16 = 1000;

  fn parse_html_table_span_attr_min_1(raw: Option<&str>) -> u16 {
    let Some(raw) = raw else {
      return 1;
    };

    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() {
      return 1;
    }

    let mut value: u32 = 0;
    let mut saw_digit = false;
    while i < bytes.len() {
      let b = bytes[i];
      if !b.is_ascii_digit() {
        break;
      }
      saw_digit = true;
      if value < HTML_TABLE_SPAN_MAX as u32 {
        value = value.saturating_mul(10).saturating_add((b - b'0') as u32);
        if value > HTML_TABLE_SPAN_MAX as u32 {
          value = HTML_TABLE_SPAN_MAX as u32;
        }
      }
      i += 1;
    }

    if !saw_digit || value == 0 {
      1
    } else {
      value as u16
    }
  }

  fn parse_html_table_rowspan_attr(raw: Option<&str>) -> u16 {
    let Some(raw) = raw else {
      return 1;
    };

    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() {
      return 1;
    }

    let mut value: u32 = 0;
    let mut saw_digit = false;
    while i < bytes.len() {
      let b = bytes[i];
      if !b.is_ascii_digit() {
        break;
      }
      saw_digit = true;
      if value < HTML_TABLE_SPAN_MAX as u32 {
        value = value.saturating_mul(10).saturating_add((b - b'0') as u32);
        if value > HTML_TABLE_SPAN_MAX as u32 {
          value = HTML_TABLE_SPAN_MAX as u32;
        }
      }
      i += 1;
    }

    if !saw_digit {
      1
    } else {
      value as u16
    }
  }

  if let Some(tag) = styled.node.tag_name() {
    // Populate table span metadata (not debug-only). Table layout must not depend on `DebugInfo`,
    // since debug info is disabled by default in `--release`.
    if tag.eq_ignore_ascii_case("td") || tag.eq_ignore_ascii_case("th") {
      box_node.table_cell_span = Some(TableCellSpan {
        colspan: parse_html_table_span_attr_min_1(styled.node.get_attribute_ref("colspan")),
        rowspan: parse_html_table_rowspan_attr(styled.node.get_attribute_ref("rowspan")),
      });
    } else {
      box_node.table_cell_span = None;
    }
    if tag.eq_ignore_ascii_case("col") || tag.eq_ignore_ascii_case("colgroup") {
      box_node.table_column_span =
        Some(parse_html_table_span_attr_min_1(styled.node.get_attribute_ref("span")));
    } else {
      box_node.table_column_span = None;
    }

    if box_debug_info_enabled() {
      let id = styled.node.get_attribute("id");
      let classes = styled
        .node
        .get_attribute_ref("class")
        .map(|c| c.split_ascii_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();

      let dbg = DebugInfo::new(Some(tag.to_string()), id, classes);
      box_node = box_node.with_debug_info(dbg);
    }
  }
  box_node
}

fn create_backdrop_box(styled: &StyledNode) -> Option<BoxNode> {
  // `::backdrop` is represented as a sibling box inserted immediately before the originating
  // top-layer element. This ensures paint order matches the spec: backdrop behind the element but
  // above the rest of the document.
  let backdrop_style = styled.styles.backdrop.as_ref().map(Arc::clone)?;
  if backdrop_style.display == Display::None {
    return None;
  }

  let fc_type = backdrop_style
    .display
    .formatting_context_type()
    .unwrap_or(FormattingContextType::Block);

  let mut box_node = match backdrop_style.display {
    Display::Block | Display::FlowRoot | Display::ListItem => {
      BoxNode::new_block(backdrop_style, fc_type, Vec::new())
    }
    Display::Inline
    | Display::Ruby
    | Display::RubyBase
    | Display::RubyText
    | Display::RubyBaseContainer
    | Display::RubyTextContainer => BoxNode::new_inline(backdrop_style, Vec::new()),
    Display::InlineBlock => BoxNode::new_inline_block(backdrop_style, fc_type, Vec::new()),
    Display::Flex => BoxNode::new_block(backdrop_style, FormattingContextType::Flex, Vec::new()),
    Display::InlineFlex => {
      BoxNode::new_inline_block(backdrop_style, FormattingContextType::Flex, Vec::new())
    }
    Display::Grid => BoxNode::new_block(backdrop_style, FormattingContextType::Grid, Vec::new()),
    Display::InlineGrid => {
      BoxNode::new_inline_block(backdrop_style, FormattingContextType::Grid, Vec::new())
    }
    Display::Table => BoxNode::new_block(backdrop_style, FormattingContextType::Table, Vec::new()),
    Display::InlineTable => {
      BoxNode::new_inline_block(backdrop_style, FormattingContextType::Table, Vec::new())
    }
    // Table-internal boxes (simplified for Wave 2)
    Display::TableRow
    | Display::TableCell
    | Display::TableRowGroup
    | Display::TableHeaderGroup
    | Display::TableFooterGroup
    | Display::TableColumn
    | Display::TableColumnGroup
    | Display::TableCaption => {
      BoxNode::new_block(backdrop_style, FormattingContextType::Block, vec![])
    }
    Display::None | Display::Contents => return None,
  };

  box_node.debug_info = Some(DebugInfo::new(
    Some("backdrop".to_string()),
    None,
    vec!["pseudo-element".to_string()],
  ));
  box_node.styled_node_id = Some(styled.node_id);
  box_node.generated_pseudo = Some(GeneratedPseudoElement::Backdrop);
  Some(box_node)
}

/// Creates a box for a pseudo-element (e.g. `::before`, `::after`, `::footnote-call`).
fn create_pseudo_element_box(
  styled: &StyledNode,
  styles: &Arc<ComputedStyle>,
  starting_style: Option<Arc<ComputedStyle>>,
  pseudo_name: &str,
  counters: &mut CounterManager,
  quote_depth: &mut usize,
) -> Option<BoxNode> {
  let content_value = effective_content_value(styles);
  if matches!(content_value, ContentValue::None | ContentValue::Normal) {
    return None;
  }
  if styles.display == Display::None {
    return None;
  }

  counters.enter_scope();
  styles.counters.apply_to(counters);

  let generated_pseudo = match pseudo_name {
    "before" => Some(GeneratedPseudoElement::Before),
    "after" => Some(GeneratedPseudoElement::After),
    "footnote-call" => Some(GeneratedPseudoElement::FootnoteCall),
    "footnote-marker" => Some(GeneratedPseudoElement::FootnoteMarker),
    _ => None,
  };

  let pseudo_style = Arc::clone(styles);
  // Generated content items behave like anonymous child boxes of the pseudo-element. They inherit
  // inheritable properties (font, color, etc.) from the pseudo-element but should not copy
  // layout-affecting properties like `display` or `position`.
  let generated_content_style = Arc::new(crate::tree::anonymous::inherited_style(&pseudo_style));

  let mut context = ContentContext::new();
  for (name, value) in styled.node.attributes_iter() {
    context.set_attribute(name, value);
  }
  for (name, stack) in counters.snapshot() {
    context.set_counter_stack(&name, stack);
  }
  context.set_quotes(styles.quotes.clone());
  context.set_quote_depth(*quote_depth);

  // Build children based on content items, supporting both text and replaced content.
  let mut children: Vec<BoxNode> = Vec::new();
  let mut text_buf = String::new();

  let flush_text = |buf: &mut String,
                    text_style: &Arc<ComputedStyle>,
                    generated_pseudo: Option<GeneratedPseudoElement>,
                    out: &mut Vec<BoxNode>| {
    if buf.is_empty() {
      return;
    }
    let text = std::mem::take(buf);
    let mut text_box = BoxNode::new_text(text_style.clone(), text);
    text_box.styled_node_id = Some(styled.node_id);
    text_box.generated_pseudo = generated_pseudo;
    out.push(text_box);
  };

  let ContentValue::Items(items) = &content_value else {
    unreachable!("non-empty pseudo-element content values must be ContentValue::Items");
  };

  for item in items {
    match item {
      ContentItem::String(s) => text_buf.push_str(s),
      ContentItem::Attr { name, fallback, .. } => {
        if let Some(val) = context.get_attribute(name) {
          text_buf.push_str(&val);
        } else if let Some(fb) = fallback {
          text_buf.push_str(fb);
        }
      }
      ContentItem::Counter { name, style } => {
        let value = context.get_counter(name);
        let formatted = styles
          .counter_styles
          .format_value(value, style.clone().unwrap_or(CounterStyle::Decimal.into()));
        text_buf.push_str(&formatted);
      }
      ContentItem::Counters {
        name,
        separator,
        style,
      } => {
        let values = context.get_counters(name);
        let style_name = style.clone().unwrap_or(CounterStyle::Decimal.into());
        if values.is_empty() {
          text_buf.push_str(&styles.counter_styles.format_value(0, style_name));
        } else {
          for (idx, value) in values.iter().enumerate() {
            if idx != 0 {
              text_buf.push_str(separator);
            }
            text_buf.push_str(
              &styles
                .counter_styles
                .format_value(*value, style_name.clone()),
            );
          }
        }
      }
      ContentItem::StringReference { name, kind } => {
        if let Some(value) = context.get_running_string(name, *kind) {
          text_buf.push_str(value);
        }
      }
      ContentItem::OpenQuote => {
        text_buf.push_str(context.open_quote());
        context.push_quote();
      }
      ContentItem::CloseQuote => {
        text_buf.push_str(context.close_quote());
        context.pop_quote();
      }
      ContentItem::NoOpenQuote => context.push_quote(),
      ContentItem::NoCloseQuote => context.pop_quote(),
      ContentItem::Element { .. } => {
        // Running elements are not yet resolved into generated content.
      }
      ContentItem::Url(url) => {
        if trim_ascii_whitespace(url).is_empty() {
          continue;
        }
        flush_text(
          &mut text_buf,
          &generated_content_style,
          generated_pseudo,
          &mut children,
        );
        let mut replaced_node = BoxNode::new_replaced(
          generated_content_style.clone(),
          ReplacedType::Image {
            src: url.clone(),
            alt: None,
            crossorigin: CrossOriginAttribute::None,
            referrer_policy: None,
            sizes: None,
            srcset: Vec::new(),
            picture_sources: Vec::new(),
          },
          None,
          None,
        );
        replaced_node.styled_node_id = Some(styled.node_id);
        replaced_node.generated_pseudo = generated_pseudo;
        children.push(replaced_node);
      }
    }
  }

  flush_text(
    &mut text_buf,
    &generated_content_style,
    generated_pseudo,
    &mut children,
  );
  *quote_depth = context.quote_depth();

  // Determine the box type based on display property
  let fc_type = styles
    .display
    .formatting_context_type()
    .unwrap_or(FormattingContextType::Block);

  // Wrap in appropriate box type based on display
  let mut pseudo_box = match styles.display {
    Display::None => unreachable!("display:none pseudo-elements are filtered before counter scope"),
    Display::Block | Display::FlowRoot | Display::ListItem => {
      BoxNode::new_block(pseudo_style.clone(), fc_type, children)
    }
    Display::Inline
    | Display::Ruby
    | Display::RubyBase
    | Display::RubyText
    | Display::RubyBaseContainer
    | Display::RubyTextContainer => BoxNode::new_inline(pseudo_style.clone(), children),
    Display::InlineBlock => BoxNode::new_inline_block(pseudo_style.clone(), fc_type, children),
    Display::Flex => {
      BoxNode::new_block(pseudo_style.clone(), FormattingContextType::Flex, children)
    }
    Display::InlineFlex => {
      BoxNode::new_inline_block(pseudo_style.clone(), FormattingContextType::Flex, children)
    }
    Display::Grid => {
      BoxNode::new_block(pseudo_style.clone(), FormattingContextType::Grid, children)
    }
    Display::InlineGrid => {
      BoxNode::new_inline_block(pseudo_style.clone(), FormattingContextType::Grid, children)
    }
    Display::Table => {
      BoxNode::new_block(pseudo_style.clone(), FormattingContextType::Table, children)
    }
    Display::InlineTable => {
      BoxNode::new_inline_block(pseudo_style.clone(), FormattingContextType::Table, children)
    }
    Display::Contents => BoxNode::new_inline(pseudo_style.clone(), children),
    Display::TableRow
    | Display::TableCell
    | Display::TableRowGroup
    | Display::TableHeaderGroup
    | Display::TableFooterGroup
    | Display::TableColumn
    | Display::TableColumnGroup
    | Display::TableCaption => {
      BoxNode::new_block(pseudo_style.clone(), FormattingContextType::Block, children)
    }
  };

  // Add debug info to mark this as a pseudo-element
  pseudo_box.debug_info = Some(DebugInfo::new(
    Some(pseudo_name.to_string()),
    None,
    vec!["pseudo-element".to_string()],
  ));
  pseudo_box.styled_node_id = Some(styled.node_id);
  pseudo_box.generated_pseudo = generated_pseudo;
  pseudo_box.starting_style = starting_style;

  counters.leave_scope();
  Some(pseudo_box)
}

fn create_box_from_style(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> Option<BoxNode> {
  if matches!(style.display, Display::None) {
    return None;
  }

  let fc_type = style
    .display
    .formatting_context_type()
    .unwrap_or(FormattingContextType::Block);

  Some(match style.display {
    Display::Block | Display::FlowRoot | Display::ListItem => BoxNode::new_block(style, fc_type, children),
    Display::Inline
    | Display::Ruby
    | Display::RubyBase
    | Display::RubyText
    | Display::RubyBaseContainer
    | Display::RubyTextContainer => BoxNode::new_inline(style, children),
    Display::InlineBlock => BoxNode::new_inline_block(style, fc_type, children),
    Display::Flex => BoxNode::new_block(style, FormattingContextType::Flex, children),
    Display::InlineFlex => BoxNode::new_inline_block(style, FormattingContextType::Flex, children),
    Display::Grid => BoxNode::new_block(style, FormattingContextType::Grid, children),
    Display::InlineGrid => BoxNode::new_inline_block(style, FormattingContextType::Grid, children),
    Display::Table => BoxNode::new_block(style, FormattingContextType::Table, children),
    Display::InlineTable => BoxNode::new_inline_block(style, FormattingContextType::Table, children),
    Display::TableRow
    | Display::TableCell
    | Display::TableRowGroup
    | Display::TableHeaderGroup
    | Display::TableFooterGroup
    | Display::TableColumn
    | Display::TableColumnGroup
    | Display::TableCaption => BoxNode::new_block(style, FormattingContextType::Block, children),
    Display::Contents => BoxNode::new_inline(style, children),
    Display::None => unreachable!("handled above"),
  })
}

fn build_appearance_none_form_control_fallback(
  styled: &StyledNode,
  form_control: &FormControl,
) -> (Vec<BoxNode>, bool, bool) {
  let mut children: Vec<BoxNode> = Vec::new();
  let mut suppress_dom_children = false;
  let mut force_position_relative = false;

  let styled_id = styled.node_id;

  let push_text = |children: &mut Vec<BoxNode>,
                   style: Arc<ComputedStyle>,
                   text: String,
                   pseudo: Option<GeneratedPseudoElement>| {
    let mut node = BoxNode::new_text(style, text);
    node.styled_node_id = Some(styled_id);
    node.generated_pseudo = pseudo;
    children.push(node);
  };

  match &form_control.control {
    FormControlKind::Text {
      value,
      placeholder,
      placeholder_style,
      kind,
      ..
    } => {
      let mut text: Option<String> = None;
      let mut style = Arc::clone(&styled.styles);
      let mut pseudo = None;

      if !value.is_empty() {
        text = Some(value.clone());
      } else if let Some(ph) = placeholder.as_ref().filter(|p| !p.is_empty()) {
        text = Some(ph.clone());
        if let Some(ph_style) = placeholder_style.as_ref().or(form_control.placeholder_style.as_ref()) {
          style = Arc::clone(ph_style);
          pseudo = Some(GeneratedPseudoElement::Placeholder);
        }
      }

      if matches!(kind, TextControlKind::Password) {
        if let Some(raw) = text.as_ref().filter(|t| *t == value) {
          let mask_len = raw.chars().count().clamp(3, 50);
          text = Some("•".repeat(mask_len));
        }
      }

      if let Some(text) = text {
        push_text(&mut children, style, text, pseudo);
      }
    }
    FormControlKind::TextArea {
      value,
      placeholder,
      placeholder_style,
      ..
    } => {
      suppress_dom_children = true;
      let mut text: Option<String> = None;
      let mut style = Arc::clone(&styled.styles);
      let mut pseudo = None;
      if !value.is_empty() {
        text = Some(value.clone());
      } else if let Some(ph) = placeholder.as_ref().filter(|p| !p.is_empty()) {
        text = Some(ph.clone());
        if let Some(ph_style) = placeholder_style.as_ref().or(form_control.placeholder_style.as_ref()) {
          style = Arc::clone(ph_style);
          pseudo = Some(GeneratedPseudoElement::Placeholder);
        }
      }

      if let Some(text) = text {
        push_text(&mut children, style, text, pseudo);
      }
    }
    FormControlKind::Button { label } => {
      if styled
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
        && !label.is_empty()
      {
        push_text(&mut children, Arc::clone(&styled.styles), label.clone(), None);
      }
    }
    FormControlKind::Select(select) => {
      suppress_dom_children = true;
      let label = select
        .selected
        .first()
        .and_then(|&idx| match select.items.get(idx) {
          Some(SelectItem::Option { label, value, .. }) => {
            let trimmed = trim_ascii_whitespace(label);
            if trimmed.is_empty() { Some(value.as_str()) } else { Some(label.as_str()) }
          }
          _ => None,
        })
        .unwrap_or("Select");
      if !label.is_empty() {
        push_text(&mut children, Arc::clone(&styled.styles), label.to_string(), None);
      }
    }
    FormControlKind::Range { value, min, max } => {
      force_position_relative = true;
      suppress_dom_children = true;

      let min_val = *min;
      let max_val = *max;
      let span = (max_val - min_val).abs().max(0.0001);
      let clamped = ((*value - min_val) / span).clamp(0.0, 1.0);
      let clamped_pct = (clamped * 100.0).clamp(0.0, 100.0);

      if let Some(track_style) = form_control.slider_track_style.as_ref() {
        let mut style = (**track_style).clone();
        style.position = Position::Absolute;
        style.left = InsetValue::Length(Length::px(0.0));
        style.right = InsetValue::Length(Length::px(0.0));
        style.top = InsetValue::Length(Length::new(50.0, LengthUnit::Percent));
        style.bottom = InsetValue::Auto;
        style.translate = TranslateValue::Values {
          x: Length::px(0.0),
          y: Length::new(-50.0, LengthUnit::Percent),
          z: Length::px(0.0),
        };
        let style = Arc::new(style);
        if let Some(mut node) = create_box_from_style(Arc::clone(&style), Vec::new()) {
          node.styled_node_id = Some(styled_id);
          node.generated_pseudo = Some(GeneratedPseudoElement::SliderTrack);
          children.push(node);
        }
      }

      if let Some(thumb_style) = form_control.slider_thumb_style.as_ref() {
        let mut style = (**thumb_style).clone();
        style.position = Position::Absolute;
        style.left = InsetValue::Length(Length::new(clamped_pct, LengthUnit::Percent));
        style.right = InsetValue::Auto;
        style.top = InsetValue::Length(Length::new(50.0, LengthUnit::Percent));
        style.bottom = InsetValue::Auto;
        style.translate = TranslateValue::Values {
          x: Length::new(-clamped_pct, LengthUnit::Percent),
          y: Length::new(-50.0, LengthUnit::Percent),
          z: Length::px(0.0),
        };
        let style = Arc::new(style);
        if let Some(mut node) = create_box_from_style(Arc::clone(&style), Vec::new()) {
          node.styled_node_id = Some(styled_id);
          node.generated_pseudo = Some(GeneratedPseudoElement::SliderThumb);
          children.push(node);
        }
      }
    }
    FormControlKind::File { value } => {
      let file_label = value
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(v))
        .filter(|v| !v.is_empty())
        .unwrap_or("No file chosen");

      if let Some(button_style) = form_control.file_selector_button_style.as_ref() {
        let mut button_children: Vec<BoxNode> = Vec::new();

        let mut text_box = BoxNode::new_text(Arc::clone(button_style), "Choose File".to_string());
        text_box.styled_node_id = Some(styled_id);
        text_box.generated_pseudo = Some(GeneratedPseudoElement::FileSelectorButton);
        button_children.push(text_box);

        if let Some(mut button_node) = create_box_from_style(Arc::clone(button_style), button_children) {
          button_node.styled_node_id = Some(styled_id);
          button_node.generated_pseudo = Some(GeneratedPseudoElement::FileSelectorButton);
          children.push(button_node);
        }
      }

      push_text(&mut children, Arc::clone(&styled.styles), file_label.to_string(), None);
    }
    FormControlKind::Unknown { label } => {
      if let Some(text) = label.as_ref().filter(|t| !t.is_empty()) {
        push_text(&mut children, Arc::clone(&styled.styles), text.clone(), None);
      }
    }
    FormControlKind::Checkbox { .. }
    | FormControlKind::Color { .. }
    | FormControlKind::Progress { .. }
    | FormControlKind::Meter { .. } => {}
  }

  (children, suppress_dom_children, force_position_relative)
}

fn create_marker_box(
  styled: &StyledNode,
  counters: &mut CounterManager,
  quote_depth: &mut usize,
) -> Option<BoxNode> {
  // Prefer authored ::marker styles; fall back to the originating style when absent.
  let (mut marker_style, has_pseudo_styles) = if let Some(styles) = styled.marker_styles.as_deref()
  {
    (styles.clone(), true)
  } else {
    (styled.styles.as_ref().clone(), false)
  };
  // ::marker boxes are inline and should not carry layout-affecting edges from the list item.
  crate::style::cascade::reset_marker_box_properties(&mut marker_style);
  marker_style.display = Display::Inline;
  if !has_pseudo_styles {
    // We are synthesizing a ::marker style from the originating list-item style. Counter properties
    // are not inherited, so copying them here would double-apply the list item's counter effects.
    marker_style.counters = Default::default();
  }

  let content_value = effective_content_value(&marker_style);
  if matches!(content_value, ContentValue::None) || marker_style.content == "none" {
    return None;
  }

  let has_explicit_content = !matches!(content_value, ContentValue::Normal | ContentValue::None);
  if !has_explicit_content
    && marker_content_from_style(styled, &marker_style, counters, quote_depth).is_none()
  {
    return None;
  }

  counters.enter_scope();
  marker_style.counters.apply_to(counters);

  let content = marker_content_from_style(styled, &marker_style, counters, quote_depth)
    .unwrap_or_else(|| MarkerContent::Text(String::new()));
  marker_style.list_style_type = ListStyleType::None;
  marker_style.list_style_image = crate::style::types::ListStyleImage::None;
  if !has_pseudo_styles {
    // Ensure list-item text transforms do not alter markers when no ::marker styles are authored.
    marker_style.text_transform = TextTransform::none();
  }

  let mut node = BoxNode::new_marker(Arc::new(marker_style), content);
  node.styled_node_id = Some(styled.node_id);
  node.starting_style = clone_starting_style(&styled.starting_styles.marker);
  counters.leave_scope();
  Some(node)
}

pub(crate) fn marker_content_from_style(
  styled: &StyledNode,
  marker_style: &ComputedStyle,
  counters: &CounterManager,
  quote_depth: &mut usize,
) -> Option<MarkerContent> {
  let content_value = effective_content_value(marker_style);
  if matches!(content_value, ContentValue::None) || marker_style.content == "none" {
    return None;
  }

  if !matches!(content_value, ContentValue::Normal | ContentValue::None) {
    let mut context = ContentContext::new();
    for (name, value) in styled.node.attributes_iter() {
      context.set_attribute(name, value);
    }
    for (name, stack) in counters.snapshot() {
      context.set_counter_stack(&name, stack);
    }
    context.set_quotes(marker_style.quotes.clone());
    context.set_quote_depth(*quote_depth);

    let mut text = String::new();
    let mut image: Option<String> = None;

    if let ContentValue::Items(items) = &content_value {
      for item in items {
        match item {
          ContentItem::String(s) => text.push_str(s),
          ContentItem::Attr { name, fallback, .. } => {
            if let Some(val) = context.get_attribute(name) {
              text.push_str(&val);
            } else if let Some(fb) = fallback {
              text.push_str(fb);
            }
          }
          ContentItem::Counter { name, style } => {
            let value = context.get_counter(name);
            let formatted = style
              .clone()
              .unwrap_or(CounterStyleName::from(CounterStyle::Decimal));
            let formatted = marker_style.counter_styles.format_value(value, formatted);
            text.push_str(&formatted);
          }
          ContentItem::Counters {
            name,
            separator,
            style,
          } => {
            let values = context.get_counters(name);
            let style = style
              .clone()
              .unwrap_or(CounterStyleName::from(CounterStyle::Decimal));
            if values.is_empty() {
              text.push_str(&marker_style.counter_styles.format_value(0, style));
            } else {
              for (idx, value) in values.iter().enumerate() {
                if idx != 0 {
                  text.push_str(separator);
                }
                text.push_str(
                  &marker_style
                    .counter_styles
                    .format_value(*value, style.clone()),
                );
              }
            }
          }
          ContentItem::StringReference { name, kind } => {
            if let Some(value) = context.get_running_string(name, *kind) {
              text.push_str(value);
            }
          }
          ContentItem::OpenQuote => {
            text.push_str(context.open_quote());
            context.push_quote();
          }
          ContentItem::CloseQuote => {
            text.push_str(context.close_quote());
            context.pop_quote();
          }
          ContentItem::NoOpenQuote => context.push_quote(),
          ContentItem::NoCloseQuote => context.pop_quote(),
          ContentItem::Element { .. } => {
            // Running elements are not supported for list markers yet.
          }
          ContentItem::Url(url) => {
            if trim_ascii_whitespace(url).is_empty() {
              continue;
            }
            // If the author supplies multiple URLs we take the last; mixed text+image returns text.
            image = Some(url.clone());
          }
        }
      }
    }

    if !text.is_empty() {
      *quote_depth = context.quote_depth();
      return Some(MarkerContent::Text(text));
    }
    if let Some(src) = image {
      *quote_depth = context.quote_depth();
      let replaced = ReplacedBox {
        replaced_type: ReplacedType::Image {
          src,
          alt: None,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
        intrinsic_size: None,
        aspect_ratio: None,
        no_intrinsic_ratio: false,
      };
      return Some(MarkerContent::Image(replaced));
    }
    *quote_depth = context.quote_depth();
    return None;
  }

  match &marker_style.list_style_image {
    crate::style::types::ListStyleImage::Url(url) => {
      let replaced = ReplacedBox {
        replaced_type: ReplacedType::Image {
          src: url.clone(),
          alt: None,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset: Vec::new(),
          picture_sources: Vec::new(),
        },
        intrinsic_size: None,
        aspect_ratio: None,
        no_intrinsic_ratio: false,
      };
      return Some(MarkerContent::Image(replaced));
    }
    crate::style::types::ListStyleImage::None => {}
  }

  let text = list_marker_text(marker_style, counters);
  if text.is_empty() {
    None
  } else {
    Some(MarkerContent::Text(text))
  }
}

/// Derive an effective `content` value that falls back to the legacy `content`
/// string when structured parsing has not populated `content_value`.
fn effective_content_value(style: &ComputedStyle) -> ContentValue {
  match &style.content_value {
    ContentValue::Normal => {
      let raw = trim_ascii_whitespace(&style.content);
      if raw.is_empty() || raw.eq_ignore_ascii_case("normal") {
        ContentValue::Normal
      } else if raw.eq_ignore_ascii_case("none") {
        ContentValue::None
      } else {
        ContentValue::Items(vec![ContentItem::String(style.content.clone())])
      }
    }
    other => other.clone(),
  }
}

fn apply_counter_properties_from_style(
  styled: &StyledNode,
  counters: &mut CounterManager,
  in_footnote: bool,
  enable_footnote_floats: bool,
) {
  if styled.node.text_content().is_some() {
    return;
  }

  // Counters are evaluated as part of box generation. Elements that are fully removed from the
  // box tree (`display:none`) must not reset/set/increment counters.
  //
  // Note that `display: contents` only removes the element's principal box; it does *not* remove
  // the element from the element tree. Per the display spec, "semantics based on the document
  // tree ... are not affected", and CSS counters are defined over the element tree.
  if styled.styles.display == Display::None {
    return;
  }

  let tag_name = styled.node.tag_name();
  if tag_name.is_some_and(|tag| {
    // Non-rendered elements: <source>, <track>, <option>, <optgroup> never create boxes.
    tag.eq_ignore_ascii_case("source")
      || tag.eq_ignore_ascii_case("track")
      || tag.eq_ignore_ascii_case("option")
      || tag.eq_ignore_ascii_case("optgroup")
  }) {
    return;
  }

  let is_ol = tag_name.is_some_and(|t| t.eq_ignore_ascii_case("ol"));
  let reversed = is_ol && styled.node.get_attribute_ref("reversed").is_some();

  let is_list_container = tag_name.is_some_and(|tag| {
    tag.eq_ignore_ascii_case("ol")
      || tag.eq_ignore_ascii_case("ul")
      || tag.eq_ignore_ascii_case("menu")
      || tag.eq_ignore_ascii_case("dir")
  });

  if is_list_container {
    // Each list establishes its own default step; child lists shouldn't inherit reversed steps.
    counters.set_list_item_increment(1);
    if reversed {
      counters.set_list_item_increment(-1);
    }
  }

  let css_reset = styled.styles.counters.counter_reset.clone();
  let reset_is_ua_default = matches!(
      css_reset.as_ref(),
      Some(reset) if reset.items.len() == 1 && reset.items[0].name == "list-item" && reset.items[0].value == 0
  );
  let mut applied_reset = false;

  if let Some(reset) = css_reset {
    if is_list_container && reset_is_ua_default {
      // Defer to HTML list defaults below so start/reversed can override UA list-item reset.
    } else {
      counters.apply_reset(&reset);
      applied_reset = true;
    }
  }

  if is_list_container && !applied_reset {
    let start = styled
      .node
      .get_attribute_ref("start")
      .and_then(|s| s.parse::<i32>().ok());
    let step = counters.list_item_increment();
    let start_value = if is_ol {
      if reversed {
        // reversed lists count down; default start is the number of list items
        let item_count = list_item_count(styled) as i32;
        start.unwrap_or_else(|| item_count.max(0))
      } else {
        start.unwrap_or(1)
      }
    } else {
      1
    };
    let default_value = start_value.saturating_sub(step);
    let default_reset = CounterSet::single("list-item", default_value);
    counters.apply_reset(&default_reset);
  }

  if let Some(set) = &styled.styles.counters.counter_set {
    counters.apply_set(set);
  }

  // HTML LI value attribute sets the list-item counter for this item.
  if tag_name.as_deref() == Some("li") {
    if let Some(value_attr) = styled
      .node
      .get_attribute("value")
      .and_then(|v| v.parse::<i32>().ok())
    {
      let step = counters.list_item_increment();
      let target = value_attr.saturating_sub(step);
      counters.apply_set(&CounterSet::single("list-item", target));
    }
  }

  let is_list_item = styled.styles.display == Display::ListItem;
  let css_increment = styled.styles.counters.counter_increment.as_ref();
  let increment_is_ua_default = matches!(
      css_increment,
      Some(increment) if increment.items.len() == 1 && increment.items[0].name == "list-item" && increment.items[0].value == 1
  );
  let increment_mentions_list_item = css_increment
    .is_some_and(|increment| increment.items.iter().any(|item| item.name == "list-item"));
  let increment_mentions_footnote = css_increment
    .is_some_and(|increment| increment.items.iter().any(|item| item.name == "footnote"));
  let set_mentions_footnote = styled
    .styles
    .counters
    .counter_set
    .as_ref()
    .is_some_and(|set| set.items.iter().any(|item| item.name == "footnote"));

  let list_item_step = counters.list_item_increment();
  if let Some(increment) = css_increment {
    if is_list_item {
      if increment_is_ua_default {
        // Treat UA default `counter-increment: list-item 1` as the implicit list-item counter,
        // rewritten to respect `<ol reversed>` semantics.
        counters.apply_increment(&CounterSet::single("list-item", list_item_step));
      } else {
        // Apply authored counter-increment, but still increment the implicit list-item counter
        // for list-item boxes unless `counter-increment` explicitly mentions `list-item`.
        counters.apply_increment(increment);
        if !increment_mentions_list_item {
          counters.apply_increment(&CounterSet::single("list-item", list_item_step));
        }
      }
    } else {
      counters.apply_increment(increment);
    }
  } else if is_list_item {
    counters.apply_increment(&CounterSet::single("list-item", list_item_step));
  }

  if enable_footnote_floats
    && !in_footnote
    && styled.styles.display != Display::None
    && styled.styles.float == Float::Footnote
    && !increment_mentions_footnote
    && !set_mentions_footnote
  {
    counters.apply_increment(&CounterSet::single("footnote", 1));
  }
}

/// Count immediate list items belonging to this list, ignoring nested lists.
fn list_item_count(styled: &StyledNode) -> usize {
  let mut count = 0usize;
  let mut stack: Vec<(&StyledNode, bool)> = Vec::new();
  for child in styled.children.iter().rev() {
    stack.push((child, false));
  }

  while let Some((node, in_nested_list)) = stack.pop() {
    if node.styles.display == Display::None {
      continue;
    }
    let tag = node.node.tag_name();
    if tag.is_some_and(|tag| {
      tag.eq_ignore_ascii_case("source")
        || tag.eq_ignore_ascii_case("track")
        || tag.eq_ignore_ascii_case("option")
        || tag.eq_ignore_ascii_case("optgroup")
    }) {
      continue;
    }
    // Treat list containers as nested list boundaries, even when they are `display: contents`.
    //
    // Although `display: contents` removes the list container's principal box, it does not remove
    // the element from the element tree; list counter properties (notably the UA default
    // `counter-reset: list-item`) still apply to its descendants.
    let is_list = tag.is_some_and(|tag| {
      tag.eq_ignore_ascii_case("ol")
        || tag.eq_ignore_ascii_case("ul")
        || tag.eq_ignore_ascii_case("menu")
        || tag.eq_ignore_ascii_case("dir")
    });
    let now_nested = in_nested_list || is_list;
    if !now_nested && node.styles.display == Display::ListItem {
      count += 1;
    }

    for child in node.children.iter().rev() {
      stack.push((child, now_nested));
    }
  }

  count
}

fn collect_text_content(node: &StyledNode) -> String {
  let mut text = String::new();
  let mut stack: Vec<&StyledNode> = Vec::new();
  stack.push(node);

  while let Some(node) = stack.pop() {
    if let DomNodeType::Text { content } = &node.node.node_type {
      text.push_str(content);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  text
}

fn option_text_from_node(node: &StyledNode) -> String {
  let mut text = String::new();
  let mut stack: Vec<&StyledNode> = Vec::new();
  stack.push(node);

  while let Some(node) = stack.pop() {
    match &node.node.node_type {
      DomNodeType::Text { content } => text.push_str(content),
      DomNodeType::Element {
        tag_name,
        namespace,
        ..
      } => {
        if tag_name.eq_ignore_ascii_case("script")
          && (namespace.is_empty() || namespace == HTML_NAMESPACE || namespace == SVG_NAMESPACE)
        {
          continue;
        }
      }
      _ => {}
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  crate::dom::strip_and_collapse_ascii_whitespace(&text)
}

fn option_label_from_node(node: &StyledNode) -> String {
  if let Some(label) = node
    .node
    .get_attribute_ref("label")
    .filter(|label| !label.is_empty())
  {
    return label.to_string();
  }

  option_text_from_node(node)
}

fn optgroup_label_from_node(node: &StyledNode) -> String {
  node
    .node
    .get_attribute_ref("label")
    .map(|label| label.to_string())
    .unwrap_or_default()
}

fn option_value_from_node(node: &StyledNode) -> String {
  if let Some(value) = node.node.get_attribute_ref("value") {
    return value.to_string();
  }

  option_text_from_node(node)
}

fn build_select_control(node: &StyledNode) -> SelectControl {
  let multiple = node.node.get_attribute_ref("multiple").is_some();
  let size = crate::dom::select_effective_size(&node.node);

  let mut items = Vec::new();
  let mut option_item_indices = Vec::new();
  let mut stack: Vec<(&StyledNode, bool, bool)> = Vec::new();
  for child in node.children.iter().rev() {
    stack.push((child, false, false));
  }
  while let Some((node, optgroup_disabled, in_optgroup)) = stack.pop() {
    if node.styles.display == Display::None {
      continue;
    }

    if let Some(tag) = node.node.tag_name() {
      if tag.eq_ignore_ascii_case("option") {
        let disabled = optgroup_disabled || node.node.get_attribute_ref("disabled").is_some();
        let idx = items.len();
        items.push(SelectItem::Option {
          node_id: node.node_id,
          label: option_label_from_node(node),
          value: option_value_from_node(node),
          selected: node.node.get_attribute_ref("selected").is_some(),
          disabled,
          in_optgroup,
          option_node_id: node.node_id,
        });
        option_item_indices.push(idx);
        continue;
      }

      if tag.eq_ignore_ascii_case("optgroup") {
        let disabled_attr = node.node.get_attribute_ref("disabled").is_some();
        let disabled = optgroup_disabled || disabled_attr;
        items.push(SelectItem::OptGroupLabel {
          label: optgroup_label_from_node(node),
          disabled,
        });
        for child in node.children.iter().rev() {
          stack.push((child, disabled, true));
        }
        continue;
      }
    }

    for child in node.children.iter().rev() {
      stack.push((child, optgroup_disabled, in_optgroup));
    }
  }

  let mut selected: Vec<usize> = Vec::new();
  if multiple {
    for &idx in option_item_indices.iter() {
      if let SelectItem::Option {
        selected: is_selected,
        ..
      } = &items[idx]
      {
        if *is_selected {
          selected.push(idx);
        }
      }
    }
  } else {
    let mut chosen: Option<usize> = None;
    for &idx in option_item_indices.iter() {
      if let SelectItem::Option {
        selected: is_selected,
        ..
      } = &items[idx]
      {
        if *is_selected {
          chosen = Some(idx);
        }
      }
    }

    if chosen.is_none() {
      for &idx in option_item_indices.iter() {
        if let SelectItem::Option { disabled, .. } = &items[idx] {
          if !*disabled {
            chosen = Some(idx);
            break;
          }
        }
      }
    }

    if chosen.is_none() {
      chosen = option_item_indices.first().copied();
    }

    for &idx in option_item_indices.iter() {
      if let Some(SelectItem::Option { selected, .. }) = items.get_mut(idx) {
        *selected = Some(idx) == chosen;
      }
    }

    if let Some(chosen) = chosen {
      selected.push(chosen);
    }
  }

  SelectControl {
    multiple,
    size,
    items: Arc::new(items),
    selected,
  }
}

fn select_placeholder_label_option_index(control: &SelectControl, required: bool) -> Option<usize> {
  if !required || control.multiple || control.size != 1 {
    return None;
  }

  // HTML: the placeholder label option exists when the first option in tree order has an empty
  // value attribute and is a direct child of `<select>` (i.e. not under `<optgroup>`).
  for (idx, item) in control.items.iter().enumerate() {
    match item {
      SelectItem::OptGroupLabel { .. } => continue,
      SelectItem::Option {
        value,
        in_optgroup,
        ..
      } => {
        if !*in_optgroup && value.is_empty() {
          return Some(idx);
        }
        return None;
      }
    }
  }

  None
}

fn input_label(node: &DomNode, input_type: &str) -> String {
  if let Some(value) = node.get_attribute_ref("value").filter(|v| !v.is_empty()) {
    return value.to_string();
  }

  if input_type.eq_ignore_ascii_case("submit") {
    "Submit".to_string()
  } else if input_type.eq_ignore_ascii_case("reset") {
    "Reset".to_string()
  } else if input_type.eq_ignore_ascii_case("button") {
    "Button".to_string()
  } else {
    input_type.to_ascii_uppercase()
  }
}

fn button_label(node: &StyledNode) -> String {
  let text = collect_text_content(node);
  let trimmed = trim_ascii_whitespace(&text);
  if !trimmed.is_empty() {
    return trimmed.to_string();
  }

  node
    .node
    .get_attribute_ref("value")
    .filter(|v| !v.is_empty())
    .map(|v| v.to_string())
    .unwrap_or_default()
}

fn create_form_control_replaced(styled: &StyledNode) -> Option<FormControl> {
  let tag = styled.node.tag_name()?;
  let appearance = styled.styles.appearance.clone();

  if !tag.eq_ignore_ascii_case("input")
    && !tag.eq_ignore_ascii_case("textarea")
    && !tag.eq_ignore_ascii_case("select")
    && !tag.eq_ignore_ascii_case("button")
    && !tag.eq_ignore_ascii_case("progress")
    && !tag.eq_ignore_ascii_case("meter")
  {
    return None;
  }

  // Buttons can contain arbitrary HTML (icons, rich text). Treat them as form controls only when
  // they have no element children so we can still lay out icon buttons correctly.
  if tag.eq_ignore_ascii_case("button")
    && styled
      .children
      .iter()
      .any(|child| child.node.tag_name().is_some())
  {
    return None;
  }
  let parse_f32_attr = |node: &DomNode, name: &str| -> Option<f32> {
    node
      .get_attribute_ref(name)
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .and_then(|v| v.parse::<f32>().ok())
      .filter(|v| v.is_finite())
  };

  let disabled = styled.node.get_attribute_ref("disabled").is_some();
  let inert = styled.node.get_attribute_ref("inert").is_some()
    || styled
      .node
      .get_attribute_ref("data-fastr-inert")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false);
  let focus_flag = styled
    .node
    .get_attribute_ref("data-fastr-focus")
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false);
  let focus_visible_flag = styled
    .node
    .get_attribute_ref("data-fastr-focus-visible")
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false);
  let mut focused = (focus_flag || focus_visible_flag) && !inert;
  let mut focus_visible = focus_visible_flag && !inert;
  if !focused {
    focus_visible = false;
  }
  if disabled {
    focused = false;
    focus_visible = false;
  }
  let textarea_value = tag
    .eq_ignore_ascii_case("textarea")
    .then(|| {
      crate::dom::textarea_current_value_from_text_content(&styled.node, collect_text_content(styled))
    });
  let mut select_control: Option<SelectControl> = None;
  if tag.eq_ignore_ascii_case("select") {
    select_control = Some(build_select_control(styled));
  }
  let element_ref = ElementRef::new(&styled.node);
  let required = element_ref.accessibility_required() && !disabled;
  let mut invalid = element_ref.accessibility_supports_validation() && !disabled;
  if invalid {
    if tag.eq_ignore_ascii_case("textarea") {
      invalid = required && textarea_value.as_deref().unwrap_or_default().is_empty();
    } else if tag.eq_ignore_ascii_case("select") {
      if !required {
        invalid = false;
      } else if let Some(control) = select_control.as_ref() {
        invalid = if control.multiple || control.size != 1 {
          !control.selected.iter().any(|&idx| matches!(
            control.items.get(idx),
            Some(SelectItem::Option { disabled: false, .. })
          ))
        } else {
          control.selected.is_empty()
            || select_placeholder_label_option_index(control, required)
              .is_some_and(|idx| control.selected.as_slice() == [idx])
        };
      } else {
        invalid = false;
      }
    } else {
      invalid = !element_ref.accessibility_is_valid();
    }
  }

  if tag.eq_ignore_ascii_case("progress") {
    let max = parse_f32_attr(&styled.node, "max")
      .filter(|v| *v > 0.0)
      .unwrap_or(1.0);
    let value = match styled.node.get_attribute_ref("value") {
      None => -1.0,
      Some(raw) => {
        let parsed = trim_ascii_whitespace(raw)
          .parse::<f32>()
          .ok()
          .filter(|v| v.is_finite());
        match parsed {
          Some(v) => v.clamp(0.0, max),
          None => -1.0,
        }
      }
    };

    return Some(FormControl {
      control: FormControlKind::Progress { value, max },
      appearance,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: styled.progress_bar_styles.clone(),
      progress_value_style: styled.progress_value_styles.clone(),
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled,
      focused,
      focus_visible,
      required: false,
      invalid: false,
    });
  }

  if tag.eq_ignore_ascii_case("meter") {
    let mut min = parse_f32_attr(&styled.node, "min").unwrap_or(0.0);
    if !min.is_finite() {
      min = 0.0;
    }
    let mut max = parse_f32_attr(&styled.node, "max").unwrap_or(1.0);
    if !max.is_finite() {
      max = 1.0;
    }
    if max < min {
      max = min;
    }

    let value = parse_f32_attr(&styled.node, "value").unwrap_or(min).clamp(min, max);
    let mut low = parse_f32_attr(&styled.node, "low").map(|v| v.clamp(min, max));
    let mut high = parse_f32_attr(&styled.node, "high").map(|v| v.clamp(min, max));
    if let (Some(low_v), Some(high_v)) = (low, high) {
      if low_v > high_v {
        low = Some(high_v);
      }
    }
    let optimum = parse_f32_attr(&styled.node, "optimum").map(|v| v.clamp(min, max));

    return Some(FormControl {
      control: FormControlKind::Meter {
        value,
        min,
        max,
        low,
        high,
        optimum,
      },
      appearance,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: styled.meter_bar_styles.clone(),
      meter_optimum_value_style: styled.meter_optimum_value_styles.clone(),
      meter_suboptimum_value_style: styled.meter_suboptimum_value_styles.clone(),
      meter_even_less_good_value_style: styled.meter_even_less_good_value_styles.clone(),
      file_selector_button_style: None,
      disabled,
      focused,
      focus_visible,
      required: false,
      invalid: false,
    });
  }

  if tag.eq_ignore_ascii_case("input") {
    let input_type = styled.node.get_attribute_ref("type").unwrap_or("text");
    if input_type.eq_ignore_ascii_case("hidden") {
      return None;
    }
    // `<input type="image">` is a graphical submit button and is rendered as an image replaced
    // element (like `<img>`), not as a native form control.
    if input_type.eq_ignore_ascii_case("image") {
      return None;
    }

    let control = if input_type.eq_ignore_ascii_case("checkbox") {
      FormControlKind::Checkbox {
        is_radio: false,
        checked: styled.node.get_attribute_ref("checked").is_some(),
        indeterminate: styled
          .node
          .get_attribute_ref("indeterminate")
          .map(|v| v.eq_ignore_ascii_case("true"))
          .unwrap_or(false)
          || styled
            .node
            .get_attribute_ref("aria-checked")
            .map(|v| v.eq_ignore_ascii_case("mixed"))
            .unwrap_or(false),
      }
    } else if input_type.eq_ignore_ascii_case("radio") {
      FormControlKind::Checkbox {
        is_radio: true,
        checked: styled.node.get_attribute_ref("checked").is_some(),
        indeterminate: false,
      }
    } else if input_type.eq_ignore_ascii_case("button")
      || input_type.eq_ignore_ascii_case("submit")
      || input_type.eq_ignore_ascii_case("reset")
    {
      FormControlKind::Button {
        label: input_label(&styled.node, input_type),
      }
    } else if input_type.eq_ignore_ascii_case("range") {
      let (min, max) = crate::dom::input_range_bounds(&styled.node).unwrap_or((0.0, 100.0));
      let value = crate::dom::input_range_value(&styled.node).unwrap_or_else(|| (min + max) / 2.0);
      FormControlKind::Range {
        value: value as f32,
        min: min as f32,
        max: max as f32,
      }
    } else if input_type.eq_ignore_ascii_case("color") {
      let raw_value = styled
        .node
        .get_attribute_ref("value")
        .filter(|v| !v.is_empty());
      let sanitized = crate::dom::input_color_value_string(&styled.node)
        .unwrap_or_else(|| "#000000".to_string());
      let color_value = parse_color_attribute(&sanitized).unwrap_or(Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 1.0,
      });
      FormControlKind::Color {
        value: color_value,
        raw: raw_value.map(|v| v.to_string()),
      }
    } else if input_type.eq_ignore_ascii_case("file") {
      let value = styled
        .node
        .get_attribute_ref("value")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
      FormControlKind::File { value }
    } else {
      let size_attr = styled
        .node
        .get_attribute_ref("size")
        .and_then(|s| s.parse::<u32>().ok());
      let mut placeholder = styled
        .node
        .get_attribute_ref("placeholder")
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string());
      let value = styled
        .node
        .get_attribute_ref("value")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .unwrap_or_default();

      let kind = if input_type.eq_ignore_ascii_case("password") {
        TextControlKind::Password
      } else if input_type.eq_ignore_ascii_case("number") {
        TextControlKind::Number
      } else if input_type.eq_ignore_ascii_case("date") {
        placeholder.get_or_insert_with(|| "yyyy-mm-dd".to_string());
        TextControlKind::Date
      } else if input_type.eq_ignore_ascii_case("datetime-local") {
        placeholder.get_or_insert_with(|| "yyyy-mm-dd hh:mm".to_string());
        TextControlKind::Date
      } else if input_type.eq_ignore_ascii_case("month") {
        placeholder.get_or_insert_with(|| "yyyy-mm".to_string());
        TextControlKind::Date
      } else if input_type.eq_ignore_ascii_case("week") {
        placeholder.get_or_insert_with(|| "yyyy-Www".to_string());
        TextControlKind::Date
      } else if input_type.eq_ignore_ascii_case("time") {
        placeholder.get_or_insert_with(|| "hh:mm".to_string());
        TextControlKind::Date
      } else if input_type.is_empty()
        || input_type.eq_ignore_ascii_case("text")
        || input_type.eq_ignore_ascii_case("search")
        || input_type.eq_ignore_ascii_case("url")
        || input_type.eq_ignore_ascii_case("tel")
        || input_type.eq_ignore_ascii_case("email")
      {
        TextControlKind::Plain
      } else {
        let label = placeholder
          .or_else(|| (!value.is_empty()).then_some(value))
          .or_else(|| Some(input_type.to_ascii_uppercase()));
        return Some(FormControl {
          control: FormControlKind::Unknown { label },
          appearance,
          placeholder_style: None,
          slider_thumb_style: None,
          slider_track_style: None,
          progress_bar_style: None,
          progress_value_style: None,
          meter_bar_style: None,
          meter_optimum_value_style: None,
          meter_suboptimum_value_style: None,
          meter_even_less_good_value_style: None,
          file_selector_button_style: None,
          disabled,
          focused,
          focus_visible,
          required,
          invalid,
        });
      };

      FormControlKind::Text {
        value,
        placeholder,
        placeholder_style: styled.placeholder_styles.clone(),
        size_attr,
        kind,
      }
    };

    let (placeholder_style, slider_thumb_style, slider_track_style, file_selector_button_style) =
      match &control {
        FormControlKind::Text { .. } => (styled.placeholder_styles.clone(), None, None, None),
        FormControlKind::Range { .. } => (
          None,
          styled.slider_thumb_styles.clone(),
          styled.slider_track_styles.clone(),
          None,
        ),
        FormControlKind::File { .. } => (None, None, None, styled.file_selector_button_styles.clone()),
        _ => (None, None, None, None),
    };

    Some(FormControl {
      control,
      appearance,
      placeholder_style,
      slider_thumb_style,
      slider_track_style,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style,
      disabled,
      focused,
      focus_visible,
      required,
      invalid,
    })
  } else if tag.eq_ignore_ascii_case("textarea") {
    let placeholder = styled
      .node
      .get_attribute_ref("placeholder")
      .map(trim_ascii_whitespace)
      .filter(|p| !p.is_empty())
      .map(|p| p.to_string());
    Some(FormControl {
      control: FormControlKind::TextArea {
        value: textarea_value.unwrap_or_default(),
        placeholder,
        placeholder_style: styled.placeholder_styles.clone(),
        rows: styled
          .node
          .get_attribute_ref("rows")
          .and_then(|r| r.parse::<u32>().ok()),
        cols: styled
          .node
          .get_attribute_ref("cols")
          .and_then(|c| c.parse::<u32>().ok()),
      },
      appearance,
      placeholder_style: styled.placeholder_styles.clone(),
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled,
      focused,
      focus_visible,
      required,
      invalid,
    })
  } else if tag.eq_ignore_ascii_case("select") {
    let control = select_control.unwrap_or_else(|| build_select_control(styled));
    Some(FormControl {
      control: FormControlKind::Select(control),
      appearance,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled,
      focused,
      focus_visible,
      required,
      invalid,
    })
  } else if tag.eq_ignore_ascii_case("button") {
    Some(FormControl {
      control: FormControlKind::Button {
        label: button_label(styled),
      },
      appearance,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled,
      focused,
      focus_visible,
      required,
      invalid,
    })
  } else {
    None
  }
}

/// Checks if an element is a replaced element
///
/// Replaced elements are those whose content is replaced by an external resource,
/// such as images, videos, iframes, etc. These elements have intrinsic dimensions.
pub fn is_replaced_element(tag: &str) -> bool {
  tag.eq_ignore_ascii_case("img")
    || tag.eq_ignore_ascii_case("video")
    || tag.eq_ignore_ascii_case("canvas")
    || tag.eq_ignore_ascii_case("svg")
    || tag.eq_ignore_ascii_case("iframe")
    || tag.eq_ignore_ascii_case("embed")
    || tag.eq_ignore_ascii_case("object")
    || tag.eq_ignore_ascii_case("audio")
    || tag.eq_ignore_ascii_case("math")
}

fn object_has_renderable_external_content(styled: &StyledNode) -> bool {
  let Some(tag) = styled.node.tag_name() else {
    return false;
  };
  if !tag.eq_ignore_ascii_case("object") {
    return false;
  }

  let data = styled
    .node
    .get_attribute_ref("data")
    .map(trim_ascii_whitespace)
    .unwrap_or("");
  if data.is_empty() {
    return false;
  }

  // FastRender supports rendering `<object>` external resources when they are images, or when the
  // resource is an HTML document that can be rendered as an embedded iframe.
  fn is_supported_object_mime(mime: &str) -> bool {
    let normalized = trim_ascii_whitespace(mime.split(';').next().unwrap_or("")).to_ascii_lowercase();
    if normalized.is_empty() {
      return false;
    }
    is_supported_image_mime(&normalized)
      || matches!(
        normalized.as_str(),
        "text/html" | "application/xhtml+xml" | "application/html"
      )
      || normalized.contains("+html")
  }

  // When no `type` attribute is provided, infer basic support from the data URL mediatype.
  // For non-data URLs, we keep the previous behavior of treating the external content as renderable.
  let infer_supported_from_data_url = || {
    if !data
      .get(..5)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
      return None;
    }
    let rest = &data["data:".len()..];
    let (metadata, _payload) = match rest.split_once(',') {
      Some(split) => split,
      None => return Some(false),
    };
    let mediatype = trim_ascii_whitespace(metadata.split(';').next().unwrap_or(""));
    let mediatype = if mediatype.is_empty() {
      "text/plain"
    } else {
      mediatype
    };
    Some(is_supported_object_mime(mediatype))
  };

  let type_attr = styled
    .node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .unwrap_or("");
  if type_attr.is_empty() {
    return infer_supported_from_data_url().unwrap_or(true);
  }

  is_supported_object_mime(type_attr)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaElementKind {
  Video,
  Audio,
}

fn media_src_is_unusable(src: &str) -> bool {
  let trimmed = trim_ascii_whitespace(src);
  if trimmed.is_empty() || trimmed.starts_with('#') {
    return true;
  }
  const ABOUT_BLANK: &str = "about:blank";
  if trimmed
    .get(..ABOUT_BLANK.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(ABOUT_BLANK))
  {
    return matches!(
      trimmed.as_bytes().get(ABOUT_BLANK.len()),
      None | Some(b'#') | Some(b'?')
    );
  }
  false
}

fn media_src_from_source_children(styled: &StyledNode, kind: MediaElementKind) -> Option<String> {
  let preferred_prefix = match kind {
    MediaElementKind::Video => "video/",
    MediaElementKind::Audio => "audio/",
  };

  let mut first_any: Option<String> = None;
  for child in &styled.children {
    let Some(tag) = child.node.tag_name() else {
      continue;
    };
    if !tag.eq_ignore_ascii_case("source") {
      continue;
    }

    let Some(src_attr) = child.node.get_attribute_ref("src") else {
      continue;
    };
    let src_trimmed = trim_ascii_whitespace(src_attr);
    if src_trimmed.is_empty() {
      continue;
    }

    if first_any.is_none() {
      first_any = Some(src_trimmed.to_string());
    }

    // Prefer sources whose type hints match the parent element. This preserves some semantics from
    // HTML media selection without needing full codec/`media` evaluation.
    if let Some(type_attr) = child.node.get_attribute_ref("type") {
      let type_trimmed = trim_ascii_whitespace(type_attr);
      if type_trimmed
        .get(..preferred_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(preferred_prefix))
      {
        return Some(src_trimmed.to_string());
      }
    }
  }

  first_any
}

fn effective_media_src(styled: &StyledNode, kind: MediaElementKind) -> String {
  let src = styled
    .node
    .get_attribute_ref("src")
    .map(trim_ascii_whitespace)
    .unwrap_or("");
  if !media_src_is_unusable(src) {
    return src.to_string();
  }

  media_src_from_source_children(styled, kind).unwrap_or_default()
}

/// Creates a BoxNode for a replaced element from a StyledNode
fn create_replaced_box_from_styled(
  styled: &StyledNode,
  style: Arc<ComputedStyle>,
  document_css: &str,
  svg_document_css_style_element: Option<&Arc<str>>,
  picture_sources: Vec<PictureSource>,
  site_compat: bool,
) -> Option<BoxNode> {
  let tag = styled.node.tag_name().unwrap_or("img");

  // Determine replaced type
  let replaced_type = if tag.eq_ignore_ascii_case("img") {
    let src = styled
      .node
      .get_attribute_ref("src")
      .map(trim_ascii_whitespace)
      .unwrap_or("")
      .to_string();
    let alt = styled
      .node
      .get_attribute_ref("alt")
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let crossorigin = match styled.node.get_attribute_ref("crossorigin") {
      None => CrossOriginAttribute::None,
      Some(value) => {
        let value = trim_ascii_whitespace(value);
        if value.eq_ignore_ascii_case("use-credentials") {
          CrossOriginAttribute::UseCredentials
        } else {
          // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
          CrossOriginAttribute::Anonymous
        }
      }
    };
    let srcset = styled
      .node
      .get_attribute_ref("srcset")
      .map(parse_srcset)
      .unwrap_or_default();
    let sizes = styled.node.get_attribute_ref("sizes").and_then(parse_sizes);
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);
    ReplacedType::Image {
      src,
      alt,
      crossorigin,
      referrer_policy,
      srcset,
      sizes,
      picture_sources,
    }
  } else if tag.eq_ignore_ascii_case("video") {
    let src = effective_media_src(styled, MediaElementKind::Video);
    let mut poster = styled
      .node
      .get_attribute_ref("poster")
      .map(trim_ascii_whitespace)
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    if poster.is_none() && site_compat {
      poster = styled
        .node
        .get_attribute_ref("gnt-gl-ps")
        .map(trim_ascii_whitespace)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    }
    ReplacedType::Video { src, poster }
  } else if tag.eq_ignore_ascii_case("audio") {
    let src = effective_media_src(styled, MediaElementKind::Audio);
    ReplacedType::Audio { src }
  } else if tag.eq_ignore_ascii_case("canvas") {
    ReplacedType::Canvas
  } else if tag.eq_ignore_ascii_case("svg") {
    ReplacedType::Svg {
      content: serialize_svg_subtree(styled, document_css, svg_document_css_style_element),
    }
  } else if tag.eq_ignore_ascii_case("iframe") {
    // HTML iframes default to about:blank when `src` is missing/empty.
    // This avoids painting a UA placeholder for the common pattern of creating an iframe and
    // populating it via JS (which FastRender does not execute).
    let src = styled
      .node
      .get_attribute_ref("src")
      .map(trim_ascii_whitespace)
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string())
      .unwrap_or_else(|| "about:blank".to_string());
    let srcdoc = styled
      .node
      .get_attribute_ref("srcdoc")
      .map(|s| s.to_string());
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);
    ReplacedType::Iframe {
      src,
      srcdoc,
      referrer_policy,
    }
  } else if tag.eq_ignore_ascii_case("embed") {
    let src = styled
      .node
      .get_attribute_ref("src")
      .map(trim_ascii_whitespace)
      .unwrap_or("")
      .to_string();
    ReplacedType::Embed { src }
  } else if tag.eq_ignore_ascii_case("object") {
    // HTML <object> falls back to its children when it has no usable external resource.
    if !object_has_renderable_external_content(styled) {
      return None;
    }
    let data = styled
      .node
      .get_attribute_ref("data")
      .map(trim_ascii_whitespace)
      .unwrap_or("")
      .to_string();
    ReplacedType::Object { data }
  } else {
    let src = styled
      .node
      .get_attribute_ref("src")
      .map(trim_ascii_whitespace)
      .unwrap_or("")
      .to_string();
    let alt = styled
      .node
      .get_attribute_ref("alt")
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    ReplacedType::Image {
      src,
      alt,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      sizes: None,
      srcset: Vec::new(),
      picture_sources: Vec::new(),
    }
  };

  let width_attr = styled.node.get_attribute_ref("width");
  let height_attr = styled.node.get_attribute_ref("height");

  let (mut intrinsic_size, mut aspect_ratio, no_intrinsic_ratio) = match &replaced_type {
    ReplacedType::Svg { .. } => {
      let view_box_attr = styled.node.get_attribute_ref("viewBox");
      let preserve_aspect_ratio_attr = styled.node.get_attribute_ref("preserveAspectRatio");
      let svg_intrinsic = svg_intrinsic_dimensions_from_attributes(
        width_attr,
        height_attr,
        view_box_attr,
        preserve_aspect_ratio_attr,
        styled.styles.font_size,
        styled.styles.root_font_size,
      );

      let size = match (svg_intrinsic.width, svg_intrinsic.height) {
        (Some(w), Some(h)) => Size::new(w, h),
        (Some(w), None) => Size::new(w, 150.0),
        (None, Some(h)) => Size::new(300.0, h),
        (None, None) => Size::new(300.0, 150.0),
      };
      (
        Some(size),
        svg_intrinsic.aspect_ratio,
        svg_intrinsic.aspect_ratio_none,
      )
    }
    _ => {
      let intrinsic_width = width_attr
        .and_then(|w| w.parse::<f32>().ok())
        // HTML width/height content attributes are non-negative integers, but we treat 0 and
        // non-finite values as "missing" so they don't get recorded as an intrinsic size and
        // suppress later intrinsic sizing fallbacks (e.g. alt-text sizing when the image cannot be
        // loaded). (This also matches our internal convention where an intrinsic axis is only
        // considered known when it is a finite, positive number.)
        .filter(|w| w.is_finite() && *w > 0.0);

      let intrinsic_height = height_attr
        .and_then(|h| h.parse::<f32>().ok())
        .filter(|h| h.is_finite() && *h > 0.0);

      let intrinsic_size = match (intrinsic_width, intrinsic_height) {
        (Some(w), Some(h)) => Some(Size::new(w, h)),
        (Some(w), None) => Some(Size::new(w, 0.0)),
        (None, Some(h)) => Some(Size::new(0.0, h)),
        (None, None) => None,
      };

      let aspect_ratio = match (intrinsic_width, intrinsic_height) {
        (Some(w), Some(h)) if h > 0.0 => Some(w / h),
        _ => None,
      };

      (intrinsic_size, aspect_ratio, false)
    }
  };

  if intrinsic_size.is_none() && aspect_ratio.is_none() {
    match &replaced_type {
      ReplacedType::Canvas
      | ReplacedType::Video { .. }
      | ReplacedType::Iframe { .. }
      | ReplacedType::Embed { .. }
      | ReplacedType::Object { .. } => {
        intrinsic_size = Some(Size::new(300.0, 150.0));
        aspect_ratio = Some(2.0);
      }
      ReplacedType::Audio { .. } => {
        intrinsic_size = Some(Size::new(300.0, 32.0));
        aspect_ratio = Some(300.0 / 32.0);
      }
      _ => {}
    }
  }

  let replaced_box = ReplacedBox {
    replaced_type,
    intrinsic_size,
    aspect_ratio,
    no_intrinsic_ratio,
  };

  Some(BoxNode {
    box_type: BoxType::Replaced(replaced_box),
    style,
    starting_style: None,
    children: vec![],
    footnote_body: None,
    id: 0,
    debug_info: None,
    styled_node_id: None,
    generated_pseudo: None,
    table_cell_span: None,
    table_column_span: None,
    first_line_style: None,
    first_letter_style: None,
  })
}

#[cfg(test)]
mod tests {
  use super::generate_box_tree as generate_box_tree_result;
  use super::generate_box_tree_with_anonymous_fixup as generate_box_tree_with_anonymous_fixup_result;
  use super::*;
  use crate::dom;
  use crate::dom::HTML_NAMESPACE;
  use crate::geometry::Size;
  use crate::style;
  use crate::style::cascade::StartingStyleSet;
  use crate::style::counter_styles::{CounterStyleRegistry, CounterStyleRule, CounterSystem};
  use crate::style::counters::CounterSet;
  use crate::style::types::Appearance;
  use crate::tree::box_tree::FormControl;
  use crate::tree::box_tree::FormControlKind;
  use crate::tree::box_tree::MarkerContent;
  use crate::tree::box_tree::ReplacedType;
  use crate::tree::box_tree::TextControlKind;

  fn default_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  fn styled_element(tag: &str) -> StyledNode {
    StyledNode {
      node_id: 0,
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: tag.to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }
  }

  fn generate_box_tree(styled: &StyledNode) -> BoxTree {
    generate_box_tree_result(styled).expect("box generation failed")
  }

  fn generate_box_tree_with_anonymous_fixup(styled: &StyledNode) -> BoxTree {
    generate_box_tree_with_anonymous_fixup_result(styled).expect("anonymous box generation failed")
  }

  fn count_object_replacements(node: &BoxNode) -> usize {
    let mut count = 0;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::Object { .. }) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_object_replacements(child);
    }
    count
  }

  fn first_object_data(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Object { data } = &repl.replaced_type {
        return Some(data.clone());
      }
    }
    node.children.iter().find_map(first_object_data)
  }

  fn first_image_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Image { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(first_image_src)
  }

  fn first_video_src_and_poster(node: &BoxNode) -> Option<(String, Option<String>)> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, poster } = &repl.replaced_type {
        return Some((src.clone(), poster.clone()));
      }
    }
    node.children.iter().find_map(first_video_src_and_poster)
  }

  fn collect_text(node: &BoxNode, out: &mut Vec<String>) {
    if let BoxType::Text(text) = &node.box_type {
      out.push(text.text.clone());
    }
    for child in node.children.iter() {
      collect_text(child, out);
    }
  }

  fn first_select_control_from_html(html: &str) -> FormControl {
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_select(node: &BoxNode) -> Option<FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::Select(_)) {
            return Some(control.clone());
          }
        }
      }
      node.children.iter().find_map(find_select)
    }

    find_select(&box_tree.root).expect("expected select form control")
  }

  fn select_selected_value(select: &SelectControl) -> Option<&str> {
    let idx = select.selected.first().copied()?;
    match select.items.get(idx)? {
      SelectItem::Option { value, .. } => Some(value.as_str()),
      _ => None,
    }
  }

  fn collect_pseudo_text(
    node: &BoxNode,
    styled_node_id: usize,
    pseudo: GeneratedPseudoElement,
    out: &mut Vec<String>,
  ) {
    if node.styled_node_id == Some(styled_node_id)
      && node.generated_pseudo == Some(pseudo)
      && matches!(node.box_type, BoxType::Text(_))
    {
      if let BoxType::Text(text) = &node.box_type {
        out.push(text.text.clone());
      }
    }
    for child in node.children.iter() {
      collect_pseudo_text(child, styled_node_id, pseudo, out);
    }
  }

  fn pseudo_text(node: &BoxNode, styled_node_id: usize, pseudo: GeneratedPseudoElement) -> String {
    let mut parts = Vec::new();
    collect_pseudo_text(node, styled_node_id, pseudo, &mut parts);
    parts.join("")
  }

  fn marker_leading_decimal(marker: &str) -> i32 {
    let trimmed = marker.trim_start();
    let mut bytes = trimmed.as_bytes().iter().copied().peekable();
    let mut sign = 1i32;
    if matches!(bytes.peek(), Some(b'-')) {
      sign = -1;
      bytes.next();
    }

    let mut value: i32 = 0;
    let mut saw_digit = false;
    while let Some(b) = bytes.peek().copied() {
      if !b.is_ascii_digit() {
        break;
      }
      saw_digit = true;
      value = value.saturating_mul(10).saturating_add((b - b'0') as i32);
      bytes.next();
    }

    assert!(
      saw_digit,
      "expected marker to start with a decimal integer, got {:?}",
      marker
    );
    sign * value
  }

  #[test]
  fn img_intrinsic_size_tracks_single_dimension_attributes() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut img_width = styled_element("img");
    img_width.node_id = 1;
    set_attr(&mut img_width, "src", "test.png");
    set_attr(&mut img_width, "width", "120");

    let mut img_height = styled_element("img");
    img_height.node_id = 2;
    set_attr(&mut img_height, "src", "test.png");
    set_attr(&mut img_height, "height", "80");

    let mut root = styled_element("div");
    root.children = vec![img_width, img_height];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 2);

    let width_box = &tree.root.children[0];
    match &width_box.box_type {
      BoxType::Replaced(replaced) => {
        assert_eq!(replaced.intrinsic_size, Some(Size::new(120.0, 0.0)));
      }
      other => panic!("expected replaced box, got {other:?}"),
    }

    let height_box = &tree.root.children[1];
    match &height_box.box_type {
      BoxType::Replaced(replaced) => {
        assert_eq!(replaced.intrinsic_size, Some(Size::new(0.0, 80.0)));
      }
      other => panic!("expected replaced box, got {other:?}"),
    }
  }

  #[test]
  fn img_intrinsic_size_ignores_invalid_dimension_attributes() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut zero = styled_element("img");
    zero.node_id = 1;
    set_attr(&mut zero, "src", "test.png");
    set_attr(&mut zero, "width", "0");

    let mut nan = styled_element("img");
    nan.node_id = 2;
    set_attr(&mut nan, "src", "test.png");
    set_attr(&mut nan, "width", "NaN");

    let mut negative = styled_element("img");
    negative.node_id = 3;
    set_attr(&mut negative, "src", "test.png");
    set_attr(&mut negative, "width", "-10");
    set_attr(&mut negative, "height", "-20");

    let mut negative_width_only = styled_element("img");
    negative_width_only.node_id = 4;
    set_attr(&mut negative_width_only, "src", "test.png");
    set_attr(&mut negative_width_only, "width", "-10");
    set_attr(&mut negative_width_only, "height", "80");

    let mut root = styled_element("div");
    root.children = vec![zero, nan, negative, negative_width_only];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 4);

    for idx in 0..3 {
      let node = &tree.root.children[idx];
      let BoxType::Replaced(replaced) = &node.box_type else {
        panic!("expected replaced box, got {:?}", node.box_type);
      };
      assert_eq!(replaced.intrinsic_size, None, "unexpected intrinsic size for img {idx}");
      assert_eq!(replaced.aspect_ratio, None, "unexpected aspect ratio for img {idx}");
    }

    let node = &tree.root.children[3];
    let BoxType::Replaced(replaced) = &node.box_type else {
      panic!("expected replaced box, got {:?}", node.box_type);
    };
    assert_eq!(replaced.intrinsic_size, Some(Size::new(0.0, 80.0)));
    assert_eq!(replaced.aspect_ratio, None);
  }

  #[test]
  fn appearance_none_form_controls_generate_fallback_children() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut input_style = ComputedStyle::default();
    input_style.appearance = Appearance::None;

    let mut input = styled_element("input");
    input.node_id = 1;
    input.styles = Arc::new(input_style);
    set_attr(&mut input, "value", "x");
    set_attr(&mut input, "placeholder", "placeholder");

    let mut root = styled_element("div");
    root.children = vec![input];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 1);

    let child = &tree.root.children[0];

    fn count_form_controls(node: &BoxNode) -> usize {
      let mut count = 0usize;
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          count += 1;
        }
      }
      for child in node.children.iter() {
        count += count_form_controls(child);
      }
      count
    }

    assert_eq!(
      count_form_controls(&tree.root),
      0,
      "appearance:none should disable native form control replacement"
    );

    fn has_text(node: &BoxNode, value: &str) -> bool {
      if node.text().is_some_and(|text| text == value) {
        return true;
      }
      node.children.iter().any(|child| has_text(child, value))
    }

    assert!(
      has_text(child, "x"),
      "expected appearance:none input to expose its value as a text node"
    );
  }

  #[test]
  fn img_crossorigin_attribute_parses() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut img_none = styled_element("img");
    img_none.node_id = 1;
    set_attr(&mut img_none, "src", "/a.png");

    let mut img_empty = styled_element("img");
    img_empty.node_id = 2;
    set_attr(&mut img_empty, "src", "/a.png");
    set_attr(&mut img_empty, "crossorigin", "");

    let mut img_anonymous = styled_element("img");
    img_anonymous.node_id = 3;
    set_attr(&mut img_anonymous, "src", "/a.png");
    set_attr(&mut img_anonymous, "crossorigin", "anonymous");

    let mut img_invalid = styled_element("img");
    img_invalid.node_id = 4;
    set_attr(&mut img_invalid, "src", "/a.png");
    set_attr(&mut img_invalid, "crossorigin", "iNvAlId");

    let mut img_creds = styled_element("img");
    img_creds.node_id = 5;
    set_attr(&mut img_creds, "src", "/a.png");
    set_attr(&mut img_creds, "crossorigin", "UsE-CrEdEnTiAlS");

    let mut root = styled_element("div");
    root.children = vec![img_none, img_empty, img_anonymous, img_invalid, img_creds];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 5);

    let expected = [
      CrossOriginAttribute::None,
      CrossOriginAttribute::Anonymous,
      CrossOriginAttribute::Anonymous,
      CrossOriginAttribute::Anonymous,
      CrossOriginAttribute::UseCredentials,
    ];

    for (idx, want) in expected.into_iter().enumerate() {
      let node = &tree.root.children[idx];
      match &node.box_type {
        BoxType::Replaced(replaced) => match &replaced.replaced_type {
          ReplacedType::Image { crossorigin, .. } => assert_eq!(*crossorigin, want),
          other => panic!("expected image replaced type, got {other:?}"),
        },
        other => panic!("expected replaced box, got {other:?}"),
      }
    }
  }

  #[test]
  fn non_ascii_whitespace_img_crossorigin_does_not_trim_nbsp() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let nbsp = "\u{00A0}";
    let crossorigin = format!("{nbsp}use-credentials");

    let mut img = styled_element("img");
    img.node_id = 1;
    set_attr(&mut img, "src", "/a.png");
    set_attr(&mut img, "crossorigin", &crossorigin);

    let mut root = styled_element("div");
    root.children = vec![img];

    let tree = generate_box_tree(&root);
    let node = &tree.root.children[0];
    match &node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Image { crossorigin, .. } => {
          assert_eq!(*crossorigin, CrossOriginAttribute::Anonymous)
        }
        other => panic!("expected image replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }

  #[test]
  fn box_generation_reuses_computed_style_arcs() {
    let root_style = Arc::new(ComputedStyle::default());
    let text_style = Arc::new(ComputedStyle::default());
    assert!(
      !Arc::ptr_eq(&root_style, &text_style),
      "test setup requires distinct style arcs"
    );

    let text_dom = DomNode {
      node_type: DomNodeType::Text {
        content: "hello".to_string(),
      },
      children: vec![],
    };
    let text_node = StyledNode {
      node_id: 1,
      node: text_dom,
      styles: Arc::clone(&text_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let root_dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };
    let root_node = StyledNode {
      node_id: 0,
      node: root_dom,
      styles: Arc::clone(&root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node],
    };

    fn find_text_box<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
      if matches!(node.box_type, BoxType::Text(_)) {
        return Some(node);
      }
      node.children.iter().find_map(find_text_box)
    }

    let tree = generate_box_tree(&root_node);

    assert!(
      Arc::ptr_eq(&tree.root.style, &root_style),
      "expected box tree to reuse the computed style Arc for element boxes"
    );

    let text_box = find_text_box(&tree.root).expect("expected a text box");
    assert!(
      Arc::ptr_eq(&text_box.style, &text_style),
      "expected box tree to reuse the computed style Arc for text boxes"
    );
  }

  #[test]
  fn generate_box_tree_includes_marker_from_marker_styles() {
    use style::color::Rgba;
    use style::display::Display;
    use style::types::ListStyleType;

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;

    let mut marker_style = ComputedStyle::default();
    marker_style.display = Display::Inline;
    marker_style.content = "✱".to_string();
    marker_style.color = Rgba::RED;

    let text_dom = dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: "Item".to_string(),
      },
      children: vec![],
    };
    let text_node = StyledNode {
      node_id: 0,
      node: text_dom.clone(),
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![text_dom],
    };

    let li = StyledNode {
      node_id: 0,
      node: li_dom,
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_style)),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node],
    };

    let tree = generate_box_tree(&li);
    assert_eq!(tree.root.children.len(), 2);
    let marker = tree.root.children.first().expect("marker");
    assert!(matches!(marker.box_type, BoxType::Marker(_)));
    assert_eq!(marker.text(), Some("✱"));
    assert_eq!(marker.style.color, Rgba::RED);
  }

  #[test]
  fn audio_generates_replaced_box_with_fallback_size() {
    let html = "<html><body><audio controls src=\"sound.mp3\"></audio></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_audio(node: &BoxNode, out: &mut Vec<ReplacedBox>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::Audio { .. }) {
          out.push(repl.clone());
        }
      }
      for child in node.children.iter() {
        find_audio(child, out);
      }
    }

    let mut audios = Vec::new();
    find_audio(&box_tree.root, &mut audios);
    assert_eq!(audios.len(), 1, "expected one audio replaced box");
    let audio = &audios[0];
    assert_eq!(
      audio.intrinsic_size,
      Some(Size::new(300.0, 32.0)),
      "audio should get default UA size when none provided"
    );
  }

  #[test]
  fn object_without_data_renders_children_instead_of_replacement() {
    // When <object> lacks a data attribute, it should fall back to its children.
    let html = "<html><body><object><p id=\"fallback\">hi</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      0,
      "object without data should render fallback content"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("hi")),
      "fallback text from object children should be present"
    );
  }

  #[test]
  fn object_with_whitespace_data_renders_children() {
    let html = "<html><body><object data=\"   \"><p>fallback</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      0,
      "object with whitespace-only data should render fallback content"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("fallback")),
      "fallback text should be present"
    );
  }

  #[test]
  fn object_with_unsupported_type_renders_children_instead_of_replacement() {
    let html = "<html><body><object data=\"doc.pdf\" type=\"application/pdf\"><p>hi</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      0,
      "object with unsupported type should render fallback content"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("hi")),
      "fallback text from object children should be present"
    );
  }

  #[test]
  fn object_with_supported_image_type_still_replaced() {
    let html = "<html><body><object data=\"img.png\" type=\"image/png\"></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      1,
      "object with supported image type should render as replaced content"
    );
  }

  #[test]
  fn object_replaced_data_is_trimmed() {
    let html =
      "<html><body><object data=\"  img.png  \" type=\"image/png\"></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      1,
      "object should be a replaced element"
    );
    assert_eq!(
      first_object_data(&box_tree.root).as_deref(),
      Some("img.png"),
      "replaced object data should be whitespace-trimmed"
    );
  }

  #[test]
  fn object_with_unsupported_data_url_renders_children_when_type_missing() {
    let html =
      "<html><body><object data=\"data:application/pdf,hello\"><p>fallback</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      0,
      "object data URLs with unsupported mediatypes should render fallback children when `type` is missing"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("fallback")),
      "fallback text should be present"
    );
  }

  #[test]
  fn object_with_image_data_url_is_replaced_when_type_missing() {
    let html = "<html><body><object data=\"data:image/png,hello\"><p>fallback</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      1,
      "object data URLs with supported image mediatypes should create replaced content even when `type` is missing"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      !texts.iter().any(|t| t.contains("fallback")),
      "fallback text should not be present when object is replaced"
    );
  }

  #[test]
  fn img_replaced_src_is_trimmed() {
    let html = "<html><body><img src=\"  img.png  \"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      first_image_src(&box_tree.root).as_deref(),
      Some("img.png"),
      "img src should be whitespace-trimmed"
    );
  }

  #[test]
  fn img_replaced_src_preserves_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!("<html><body><img src=\"img.png{nbsp}\"></body></html>");
    let dom = crate::dom::parse_html(&html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      first_image_src(&box_tree.root),
      Some(format!("img.png{nbsp}")),
      "img src should not trim non-ASCII whitespace like NBSP"
    );
  }

  #[test]
  fn video_src_and_poster_are_trimmed() {
    let html =
      "<html><body><video src=\"  v.mp4  \" poster=\"  poster.png  \"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    let (src, poster) =
      first_video_src_and_poster(&box_tree.root).expect("expected a replaced video");
    assert_eq!(src, "v.mp4", "video src should be whitespace-trimmed");
    assert_eq!(
      poster.as_deref(),
      Some("poster.png"),
      "video poster should be whitespace-trimmed"
    );
  }

  #[test]
  fn video_src_and_poster_preserve_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!(
      "<html><body><video src=\"v.mp4{nbsp}\" poster=\"poster.png{nbsp}\"></video></body></html>"
    );
    let dom = crate::dom::parse_html(&html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    let (src, poster) =
      first_video_src_and_poster(&box_tree.root).expect("expected a replaced video");
    assert_eq!(src, format!("v.mp4{nbsp}"), "video src should preserve NBSP");
    assert_eq!(
      poster,
      Some(format!("poster.png{nbsp}")),
      "video poster should preserve NBSP"
    );
  }

  #[test]
  fn object_with_supported_html_type_still_replaced() {
    let html =
      "<html><body><object data=\"doc.html\" type=\"text/html\"><p>hi</p></object></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      1,
      "object with supported HTML type should render as replaced content"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      !texts.iter().any(|t| t.contains("hi")),
      "fallback text should not be present when object is replaced"
    );
  }

  #[test]
  fn non_ascii_whitespace_object_type_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let html = format!(
      "<html><body><object data=\"doc.html\" type=\"{nbsp}text/html\"><p>fallback</p></object></body></html>"
    );
    let dom = crate::dom::parse_html(&html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    assert_eq!(
      count_object_replacements(&box_tree.root),
      0,
      "NBSP must not be treated as whitespace when checking object type hints"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("fallback")),
      "fallback text should be present when object is not replaced"
    );
  }

  #[test]
  fn form_controls_generate_replaced_boxes() {
    let html = "<html><body>
      <input id=\"text\" value=\"hello\">
      <input type=\"checkbox\" checked>
      <input type=\"radio\" checked>
      <select><option selected>One</option><option>Two</option></select>
      <button>Go</button>
      <textarea>note</textarea>
    </body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn collect_controls(node: &BoxNode, out: &mut Vec<ReplacedType>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          out.push(repl.replaced_type.clone());
        }
      }
      for child in node.children.iter() {
        collect_controls(child, out);
      }
    }

    let mut controls = Vec::new();
    collect_controls(&box_tree.root, &mut controls);
    assert_eq!(
      controls.len(),
      5,
      "<button> should not generate a replaced form control"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t.contains("Go")),
      "expected <button> descendants to generate text boxes"
    );

    assert!(controls.iter().any(|c| matches!(
      c,
      ReplacedType::FormControl(FormControl {
        control: FormControlKind::Checkbox {
          is_radio: false,
          checked: true,
          ..
        },
        ..
      })
    )));

    assert!(controls.iter().any(|c| matches!(
      c,
      ReplacedType::FormControl(FormControl {
        control: FormControlKind::Select(select),
        ..
      }) if select.selected.first().copied().is_some_and(|idx| matches!(
        select.items.get(idx),
        Some(SelectItem::Option { label, .. }) if label == "One"
      ))
    )));
  }

  #[test]
  fn input_image_generates_image_replaced_box() {
    let html =
      "<html><body><input type=\"image\" src=\"https://example.com/a.png\" alt=\"Logo\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn collect_replaced(node: &BoxNode, out: &mut Vec<ReplacedType>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        out.push(repl.replaced_type.clone());
      }
      for child in node.children.iter() {
        collect_replaced(child, out);
      }
    }

    let mut replaced = Vec::new();
    collect_replaced(&box_tree.root, &mut replaced);

    assert!(
      replaced.iter().any(|kind| matches!(
        kind,
        ReplacedType::Image { src, alt, .. }
          if src == "https://example.com/a.png" && alt.as_deref() == Some("Logo")
      )),
      "expected `<input type=image>` to generate an Image replaced box"
    );
    assert!(
      !replaced
        .iter()
        .any(|kind| matches!(kind, ReplacedType::FormControl(_))),
      "expected `<input type=image>` to not generate a FormControl replaced box"
    );
  }

  #[test]
  fn range_form_controls_capture_slider_thumb_pseudo_styles() {
    use crate::css::parser::parse_stylesheet;
    use crate::style::values::Length;

    let html = "<html><body><input class=\"range\" type=\"range\" value=\"50\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let stylesheet =
      parse_stylesheet(".range::-webkit-slider-thumb { width: 18px; }").expect("parse css");
    let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled);

    fn find_range_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::Range { .. }) {
            return Some(control);
          }
        }
      }
      node.children.iter().find_map(find_range_control)
    }

    let control = find_range_control(&box_tree.root).expect("range control");
    let thumb_style = control
      .slider_thumb_style
      .as_ref()
      .expect("thumb pseudo styles should be captured");
    assert_eq!(thumb_style.width, Some(Length::px(18.0)));
  }

  #[test]
  fn range_form_controls_capture_slider_track_pseudo_styles() {
    use crate::css::parser::parse_stylesheet;
    use crate::style::color::Rgba;
    use crate::style::values::Length;

    let html = "<html><body><input class=\"range\" type=\"range\" value=\"50\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let stylesheet = parse_stylesheet(
      ".range::-webkit-slider-runnable-track { height: 6px; background-color: rgb(1, 2, 3); }",
    )
    .expect("parse css");
    let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled);

    fn find_range_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::Range { .. }) {
            return Some(control);
          }
        }
      }
      node.children.iter().find_map(find_range_control)
    }

    let control = find_range_control(&box_tree.root).expect("range control");
    let track_style = control
      .slider_track_style
      .as_ref()
      .expect("track pseudo styles should be captured");
    assert_eq!(track_style.height, Some(Length::px(6.0)));
    assert_eq!(track_style.background_color, Rgba::new(1, 2, 3, 1.0));
  }

  #[test]
  fn file_form_controls_capture_file_selector_button_pseudo_styles() {
    use crate::css::parser::parse_stylesheet;
    use crate::style::color::Rgba;

    let html = "<html><body><input class=\"file\" type=\"file\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let stylesheet = parse_stylesheet(
      ".file::-webkit-file-upload-button { background-color: rgb(11, 22, 33); }",
    )
    .expect("parse css");
    let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled);

    fn find_file_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::File { .. }) {
            return Some(control);
          }
        }
      }
      node.children.iter().find_map(find_file_control)
    }

    let control = find_file_control(&box_tree.root).expect("file input control");
    let button_style = control
      .file_selector_button_style
      .as_ref()
      .expect("file-selector-button pseudo styles should be captured");
    assert_eq!(button_style.background_color, Rgba::new(11, 22, 33, 1.0));
  }

  #[test]
  fn text_inputs_capture_placeholder_pseudo_styles() {
    use crate::css::parser::parse_stylesheet;
    use crate::style::color::Rgba;

    let html = "<html><body><input placeholder=\"hello\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let stylesheet =
      parse_stylesheet("input::placeholder { color: rgb(11, 22, 33); }").expect("parse css");
    let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled);

    fn find_text_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::Text { .. }) {
            return Some(control);
          }
        }
      }
      node.children.iter().find_map(find_text_control)
    }

    let control = find_text_control(&box_tree.root).expect("text input control");
    let placeholder_style = control
      .placeholder_style
      .as_ref()
      .expect("placeholder pseudo styles should be captured");
    assert_eq!(placeholder_style.color, Rgba::new(11, 22, 33, 1.0));
  }

  #[test]
  fn textareas_capture_placeholder_pseudo_styles() {
    use crate::css::parser::parse_stylesheet;
    use crate::style::color::Rgba;

    let html = "<html><body><textarea placeholder=\"hello\"></textarea></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let stylesheet =
      parse_stylesheet("textarea::placeholder { color: rgb(44, 55, 66); }").expect("parse css");
    let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
    let box_tree = generate_box_tree(&styled);

    fn find_textarea_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          if matches!(control.control, FormControlKind::TextArea { .. }) {
            return Some(control);
          }
        }
      }
      node.children.iter().find_map(find_textarea_control)
    }

    let control = find_textarea_control(&box_tree.root).expect("textarea control");
    let placeholder_style = control
      .placeholder_style
      .as_ref()
      .expect("placeholder pseudo styles should be captured");
    assert_eq!(placeholder_style.color, Rgba::new(44, 55, 66, 1.0));
  }

  #[test]
  fn select_single_last_selected_wins() {
    let control = first_select_control_from_html(
      "<html><body><select><option selected>One</option><option selected>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert_eq!(select.selected.len(), 1);
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "Two"
    )));
    assert_eq!(
      select
        .items
        .iter()
        .filter(|item| matches!(item, SelectItem::Option { selected: true, .. }))
        .count(),
      1
    );
  }

  #[test]
  fn select_single_defaults_to_first_non_disabled_option() {
    let control = first_select_control_from_html(
      "<html><body><select><option disabled>One</option><option>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "Two"
    )));
  }

  #[test]
  fn select_single_defaults_to_first_option_if_all_disabled() {
    let control = first_select_control_from_html(
      "<html><body><select><option disabled>One</option><option disabled>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "One"
    )));
  }

  #[test]
  fn required_select_with_disabled_selected_placeholder_stays_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select required><option disabled selected value=\"\">Choose…</option><option value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);

    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, value, .. }) if label == "Choose…" && value.is_empty()
    )));
  }

  #[test]
  fn required_select_with_first_option_in_optgroup_empty_value_is_valid() {
    let control = first_select_control_from_html(
      "<html><body><select required><optgroup label=\"g\"><option selected value=\"\">Empty</option></optgroup><option value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(!control.invalid);
  }

  #[test]
  fn required_select_with_hidden_placeholder_option_is_valid() {
    let control = first_select_control_from_html(
      "<html><body><select required><option hidden value=\"\">Hidden</option><option value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(!control.invalid);
  }

  #[test]
  fn required_multiple_select_without_selection_is_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><option>One</option><option>Two</option></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);
  }

  #[test]
  fn required_multiple_select_with_only_hidden_selected_is_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><option hidden selected value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);
  }

  #[test]
  fn required_multiple_select_with_selected_in_hidden_optgroup_is_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><optgroup hidden label=\"g\"><option selected value=\"a\">A</option></optgroup></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);
  }

  #[test]
  fn required_multiple_select_with_only_disabled_selected_is_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><option selected disabled value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);
  }

  #[test]
  fn required_multiple_select_with_selected_in_disabled_optgroup_is_invalid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><optgroup disabled label=\"g\"><option selected value=\"a\">A</option></optgroup></select></body></html>",
    );
    assert!(control.required);
    assert!(control.invalid);
  }

  #[test]
  fn required_multiple_select_with_selected_empty_value_is_valid() {
    let control = first_select_control_from_html(
      "<html><body><select multiple required><option selected value=\"\">A</option><option disabled selected value=\"b\">B</option></select></body></html>",
    );
    assert!(control.required);
    assert!(!control.invalid);
  }

  #[test]
  fn required_select_with_selected_empty_value_not_placeholder_is_valid() {
    let control = first_select_control_from_html(
      "<html><body><select required><option value=\"a\">A</option><option selected value=\"\">Empty</option></select></body></html>",
    );
    assert!(control.required);
    assert!(!control.invalid);
  }

  #[test]
  fn required_select_with_size_gt_1_selected_empty_value_is_valid() {
    let control = first_select_control_from_html(
      "<html><body><select required size=\"2\"><option selected value=\"\">Empty</option><option value=\"a\">A</option></select></body></html>",
    );
    assert!(control.required);
    assert!(!control.invalid);
  }

  #[test]
  fn select_ignores_display_none_options() {
    let control = first_select_control_from_html(
      "<html><body><select size=\"4\"><option selected style=\"display:none\">One</option><option>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert_eq!(select.items.len(), 1);
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "Two"
    )));
  }

  #[test]
  fn select_ignores_hidden_attribute_options() {
    let control = first_select_control_from_html(
      "<html><body><select size=\"4\"><option hidden>One</option><option>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert_eq!(select.items.len(), 1);
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "Two"
    )));
  }

  #[test]
  fn select_ignores_hidden_optgroup_and_descendants() {
    let control = first_select_control_from_html(
      "<html><body><select size=\"4\"><optgroup hidden label=\"g\"><option>One</option></optgroup><option>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert_eq!(select.items.len(), 1);
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "Two"
    )));
  }

  #[test]
  fn optgroup_disabled_propagates_and_does_not_clear_selected() {
    let control = first_select_control_from_html(
      "<html><body><select><optgroup label=\"g\" disabled><option selected>One</option></optgroup><option>Two</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert!(select.items.iter().any(|item| matches!(
      item,
      SelectItem::OptGroupLabel { label, .. } if label == "g"
    )));
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, disabled: true, .. }) if label == "One"
    )));
    assert!(
      select.items.iter().any(|item| matches!(
        item,
        SelectItem::Option { label, in_optgroup: true, .. } if label == "One"
      )),
      "options under optgroup should be tagged as in_optgroup"
    );
    assert!(
      select.items.iter().any(|item| matches!(
        item,
        SelectItem::Option { label, in_optgroup: false, .. } if label == "Two"
      )),
      "options outside optgroup should not be tagged as in_optgroup"
    );
  }

  #[test]
  fn select_multiple_keeps_all_selected_and_defaults_size() {
    let control = first_select_control_from_html(
      "<html><body><select multiple><option selected>One</option><option>Two</option><option selected disabled>Three</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };

    assert!(select.multiple);
    assert_eq!(select.size, 4);
    assert_eq!(select.selected.len(), 2);
    assert!(select.selected.iter().all(|&idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { selected: true, .. })
    )));
    assert!(select.items.iter().any(|item| matches!(
      item,
      SelectItem::Option { label, selected: true, disabled: true, .. } if label == "Three"
    )));
  }

  #[test]
  fn select_size_attribute_defaults_and_parses() {
    let control = first_select_control_from_html(
      "<html><body><select multiple size=\"0\"><option selected>One</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert_eq!(select.size, 4);

    let control = first_select_control_from_html(
      "<html><body><select size=\"0\"><option>One</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert_eq!(select.size, 1);

    let control = first_select_control_from_html(
      "<html><body><select size=\"5\"><option>One</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert_eq!(select.size, 5);
  }

  #[test]
  fn select_option_label_prefers_label_attribute_over_text() {
    let control = first_select_control_from_html(
      "<html><body><select><option label=\"L\" value=\"v\">Text</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert_eq!(select.items.len(), 1);
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "L" && value == "v"
    ));
  }

  #[test]
  fn select_option_label_does_not_fallback_to_value() {
    let control = first_select_control_from_html(
      "<html><body><select><option value=\"v\"></option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert_eq!(select.items.len(), 1);
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label.is_empty() && value == "v"
    ));
  }

  #[test]
  fn select_option_label_strips_and_collapses_ascii_whitespace() {
    let control = first_select_control_from_html(
      "<html><body><select><option>  Foo \n  Bar\tBaz  </option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, .. }) if label == "Foo Bar Baz"
    ));
  }

  #[test]
  fn select_option_label_empty_label_attribute_falls_back_to_text_even_with_value() {
    let control = first_select_control_from_html(
      "<html><body><select><option label=\"\" value=\"v\">Text</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "Text" && value == "v"
    ));
  }

  #[test]
  fn select_option_value_defaults_to_stripped_text_content() {
    let control = first_select_control_from_html(
      "<html><body><select><option>  Foo \n Bar  </option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { value, .. }) if value == "Foo Bar"
    ));
  }

  #[test]
  fn select_option_label_empty_label_attribute_falls_back_to_text() {
    let control = first_select_control_from_html(
      "<html><body><select><option label=\"\">Text</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "Text" && value == "Text"
    ));
  }

  #[test]
  fn select_option_label_collapses_internal_whitespace() {
    let control = first_select_control_from_html(
      "<html><body><select><option>Foo\n  Bar\tBaz</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, .. }) if label == "Foo Bar Baz"
    ));
  }

  #[test]
  fn select_option_value_falls_back_to_option_text() {
    let control = first_select_control_from_html(
      "<html><body><select><option label=\"L\">  Foo \n</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "L" && value == "Foo"
    ));
  }

  #[test]
  fn select_option_text_ignores_script_descendants() {
    fn styled_text(content: &str) -> StyledNode {
      StyledNode {
        node_id: 0,
        node: DomNode {
          node_type: DomNodeType::Text {
            content: content.to_string(),
          },
          children: vec![],
        },
        styles: Arc::new(ComputedStyle::default()),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }
    }

    let mut select = styled_element("select");
    let mut option = styled_element("option");
    option.children = vec![
      styled_text("Foo "),
      {
        let mut script = styled_element("script");
        script.children = vec![styled_text("BAR")];
        script
      },
      styled_text(" Baz"),
    ];
    select.children = vec![option];

    let control = create_form_control_replaced(&select)
      .expect("select should generate a form control")
      .control;
    let FormControlKind::Select(select) = &control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "Foo Baz" && value == "Foo Baz"
    ));
  }

  #[test]
  fn select_option_label_attribute_is_verbatim() {
    let control = first_select_control_from_html(
      "<html><body><select><option label=\"  Foo  Bar  \">Text</option></select></body></html>",
    );
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select form control kind");
    };
    assert!(matches!(
      select.items.first(),
      Some(SelectItem::Option { label, value, .. }) if label == "  Foo  Bar  " && value == "Text"
    ));
  }

  #[test]
  fn new_form_control_input_types_are_identified() {
    let html = "<html><body>
      <input type=\"password\" value=\"abc\">
      <input type=\"number\" value=\"5\" data-fastr-focus=\"true\" data-fastr-focus-visible=\"true\">
      <input type=\"color\" value=\"#00ff00\">
      <input type=\"color\" value=\"not-a-color\">
      <input type=\"color\" value=\"not-a-color-disabled\" disabled>
      <input type=\"date\" required>
      <input type=\"datetime-local\">
      <input type=\"month\">
      <input type=\"week\">
      <input type=\"time\">
      <input type=\"number\" size=\"7\" placeholder=\"sized number\">
      <input type=\"checkbox\" indeterminate=\"true\">
      <input type=\"file\" value=\"C:\\\\fakepath\\\\hello.txt\">
      <input type=\"foo\" placeholder=\"mystery\" data-fastr-focus-visible=\"true\">
      <input size=\"5\" value=\"sized\">
      <textarea rows=\"4\" cols=\"10\">hi</textarea>
    </body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          out.push(control.clone());
        }
      }
      for child in node.children.iter() {
        collect_controls(child, out);
      }
    }

    let mut controls = Vec::new();
    collect_controls(&box_tree.root, &mut controls);

    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Password,
          ..
        }
      )),
      "password input should map to password text control"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Number,
          value,
          ..
        } if value == "5"
      ) && c.focus_visible),
      "number input should be recognized and keep focus-visible hint"
    );
    assert!(
      controls
        .iter()
        .any(|c| matches!(&c.control, FormControlKind::Color { .. })),
      "color input should generate a color control"
    );
    assert!(
      controls.iter().any(|c| {
        matches!(
          &c.control,
          FormControlKind::Color { raw, .. } if raw.as_deref() == Some("not-a-color")
        ) && !c.disabled
          && !c.invalid
      }),
      "color inputs sanitize invalid values and stay valid for painting"
    );
    assert!(
      controls.iter().any(|c| {
        matches!(
          &c.control,
          FormControlKind::Color { raw, .. } if raw.as_deref() == Some("not-a-color-disabled")
        ) && c.disabled
          && !c.invalid
      }),
      "disabled color inputs with invalid values should stay valid for painting"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          ..
        }
      ) && c.required
        && c.invalid),
      "required date input without value should be marked invalid"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          placeholder,
          ..
        } if placeholder.as_deref() == Some("yyyy-mm-dd hh:mm")
      )),
      "datetime-local inputs should synthesize a datetime placeholder"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          placeholder,
          ..
        } if placeholder.as_deref() == Some("yyyy-mm")
      )),
      "month inputs should synthesize a month placeholder"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          placeholder,
          ..
        } if placeholder.as_deref() == Some("yyyy-Www")
      )),
      "week inputs should synthesize a week placeholder"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          placeholder,
          ..
        } if placeholder.as_deref() == Some("hh:mm")
      )),
      "time inputs should synthesize a time placeholder"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Checkbox {
          indeterminate: true,
          ..
        }
      )),
      "indeterminate checkbox should be captured"
    );
    assert!(
      controls
        .iter()
        .any(|c| matches!(&c.control, FormControlKind::File { value }
        if value.as_deref() == Some("C:\\\\fakepath\\\\hello.txt"))),
      "file inputs should be captured as file form controls"
    );
    assert!(
      controls
        .iter()
        .any(|c| matches!(&c.control, FormControlKind::Unknown { label }
        if label.as_deref() == Some("mystery"))
          && c.focus_visible
          && c.focused),
      "unknown types should fall back to labeled control and keep focus-visible hint"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          size_attr: Some(5),
          kind: TextControlKind::Plain,
          ..
        }
      )),
      "size attribute should be preserved on text-like inputs"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Text {
          size_attr: Some(7),
          kind: TextControlKind::Number,
          placeholder,
          ..
        } if placeholder.as_deref() == Some("sized number")
      )),
      "number inputs should keep size hints and placeholder text"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::TextArea { rows, cols, .. } if rows == &Some(4) && cols == &Some(10)
      )),
      "rows/cols should be captured on textarea for intrinsic sizing"
    );
  }

  #[test]
  fn progress_and_meter_generate_form_control_replaced_boxes() {
    let html = "<html><body>
      <progress></progress>
      <progress value=\"15\" max=\"10\"></progress>
      <progress value=\"not-a-number\" max=\"10\"></progress>
      <meter value=\"200\" min=\"0\" max=\"100\" low=\"80\" high=\"20\" optimum=\"50\"></meter>
    </body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          out.push(control.clone());
        }
      }
      for child in node.children.iter() {
        collect_controls(child, out);
      }
    }

    let mut controls = Vec::new();
    collect_controls(&box_tree.root, &mut controls);

    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Progress { value, max } if *value < 0.0 && *max == 1.0
      )),
      "progress without value attribute should be represented as indeterminate"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Progress { value, max } if *value == 10.0 && *max == 10.0
      )),
      "progress values should clamp into [0,max]"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Progress { value, max } if *value < 0.0 && *max == 10.0
      )),
      "invalid progress value attribute should be treated as indeterminate"
    );
    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Meter { value, min, max, low, high, optimum }
          if *value == 100.0
            && *min == 0.0
            && *max == 100.0
            && *low == Some(20.0)
            && *high == Some(20.0)
            && *optimum == Some(50.0)
      )),
      "meter attributes should clamp and maintain low/high ordering"
    );
  }

  #[test]
  fn button_elements_with_element_children_do_not_generate_replaced_form_controls() {
    let html = "<html><body><button><span>Icon</span></button></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn contains_form_control(node: &BoxNode) -> bool {
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          return true;
        }
      }
      node.children.iter().any(contains_form_control)
    }

    assert!(
      !contains_form_control(&box_tree.root),
      "expected <button> with element children to generate normal boxes, not a replaced form control"
    );

    let mut texts = Vec::new();
    collect_text(&box_tree.root, &mut texts);
    assert!(
      texts.iter().any(|t| t == "Icon"),
      "expected <button> contents to be preserved in the box tree (texts={texts:?})"
    );
  }

  #[test]
  fn form_control_values_use_html_sanitization_algorithms() {
    let html = "<html><body>
      <input type=\"range\" min=\"0\" max=\"10\" step=\"4\" value=\"10\">
      <textarea>\nhello\r\nworld</textarea>
      <textarea required> </textarea>
    </body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::FormControl(control) = &repl.replaced_type {
          out.push(control.clone());
        }
      }
      for child in node.children.iter() {
        collect_controls(child, out);
      }
    }

    let mut controls = Vec::new();
    collect_controls(&box_tree.root, &mut controls);

    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::Range { min, max, value } if min == &0.0 && max == &10.0 && value == &8.0
      )),
      "range input should snap value to step and clamp within bounds"
    );

    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::TextArea { value, .. } if value == "hello\nworld"
      )),
      "textarea values should normalize CRLF and strip the single leading newline"
    );

    assert!(
      controls.iter().any(|c| matches!(
        &c.control,
        FormControlKind::TextArea { value, .. } if value == " "
      ) && c.required
        && !c.invalid),
      "whitespace-only textarea should not fail required validation"
    );
  }

  #[test]
  fn appearance_none_disables_form_control_replacement_and_generates_placeholder_text() {
    let html =
      "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"appearance: none; border: 0\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn count_replaced(node: &BoxNode) -> usize {
      let mut count = 0;
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          count += 1;
        }
      }
      for child in node.children.iter() {
        count += count_replaced(child);
      }
      count
    }

    assert_eq!(
      count_replaced(&box_tree.root),
      0,
      "appearance:none should disable native control replacement"
    );

    fn find_placeholder(node: &BoxNode) -> Option<&BoxNode> {
      if node.generated_pseudo == Some(GeneratedPseudoElement::Placeholder) && node.text() == Some("hello") {
        return Some(node);
      }
      node.children.iter().find_map(find_placeholder)
    }
    assert!(
      find_placeholder(&box_tree.root).is_some(),
      "expected placeholder text to be represented in the box tree when appearance:none"
    );
  }

  #[test]
  fn webkit_appearance_none_disables_form_control_replacement() {
    let html =
      "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"-webkit-appearance: none; border: 0\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn count_replaced(node: &BoxNode) -> usize {
      let mut count = 0;
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          count += 1;
        }
      }
      for child in node.children.iter() {
        count += count_replaced(child);
      }
      count
    }

    assert_eq!(
      count_replaced(&box_tree.root),
      0,
      "-webkit-appearance:none should disable native control replacement"
    );

    fn find_input_box<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
      if node
        .debug_info
        .as_ref()
        .and_then(|info| info.tag_name.as_deref())
        == Some("input")
        && matches!(node.style.appearance, Appearance::None)
      {
        return Some(node);
      }
      node.children.iter().find_map(find_input_box)
    }
    assert!(
      find_input_box(&box_tree.root).is_some(),
      "expected -webkit-appearance:none to compute to appearance:none"
    );
  }

  #[test]
  fn moz_appearance_none_disables_form_control_replacement() {
    let html =
      "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"-moz-appearance: none; border: 0\"></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn count_replaced(node: &BoxNode) -> usize {
      let mut count = 0;
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
          count += 1;
        }
      }
      for child in node.children.iter() {
        count += count_replaced(child);
      }
      count
    }

    assert_eq!(
      count_replaced(&box_tree.root),
      0,
      "-moz-appearance:none should disable native control replacement"
    );

    fn find_input_box<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
      if node
        .debug_info
        .as_ref()
        .and_then(|info| info.tag_name.as_deref())
        == Some("input")
        && matches!(node.style.appearance, Appearance::None)
      {
        return Some(node);
      }
      node.children.iter().find_map(find_input_box)
    }
    assert!(
      find_input_box(&box_tree.root).is_some(),
      "expected -moz-appearance:none to compute to appearance:none"
    );
  }

  #[test]
  fn replaced_media_defaults_to_300_by_150() {
    let style = default_style();

    for tag in ["canvas", "video", "iframe", "embed", "object"] {
      let mut styled = styled_element(tag);
      if tag == "object" {
        match &mut styled.node.node_type {
          DomNodeType::Element { attributes, .. } => {
            attributes.push(("data".to_string(), "data.bin".to_string()));
          }
          _ => panic!("expected element"),
        }
      }
      let box_node = create_replaced_box_from_styled(&styled, style.clone(), "", None, Vec::new(), false)
        .expect("expected replaced box");
      match &box_node.box_type {
        BoxType::Replaced(replaced) => {
          assert_eq!(
            replaced.intrinsic_size,
            Some(Size::new(300.0, 150.0)),
            "{tag} should default to 300x150"
          );
          assert_eq!(
            replaced.aspect_ratio,
            Some(2.0),
            "{tag} should default to 2:1 ratio"
          );
        }
        other => panic!("expected replaced box for {tag}, got {:?}", other),
      }
    }
  }

  #[test]
  fn video_poster_falls_back_to_gnt_gl_ps_when_site_compat_enabled() {
    let mut styled = styled_element("video");
    match &mut styled.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push(("gnt-gl-ps".to_string(), "poster.png".to_string()));
      }
      _ => panic!("expected element"),
    }

    let box_node =
      create_replaced_box_from_styled(&styled, default_style(), "", None, Vec::new(), true)
        .expect("expected replaced box");
    match &box_node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Video { poster, .. } => {
          assert_eq!(poster.as_deref(), Some("poster.png"));
        }
        other => panic!("expected video replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }

  #[test]
  fn video_poster_does_not_fall_back_to_gnt_gl_ps_when_site_compat_disabled() {
    let mut styled = styled_element("video");
    match &mut styled.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push(("gnt-gl-ps".to_string(), "poster.png".to_string()));
      }
      _ => panic!("expected element"),
    }

    let box_node =
      create_replaced_box_from_styled(&styled, default_style(), "", None, Vec::new(), false)
        .expect("expected replaced box");
    match &box_node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Video { poster, .. } => {
          assert_eq!(poster.as_deref(), None);
        }
        other => panic!("expected video replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }

  #[test]
  fn video_src_falls_back_to_source_children() {
    let html =
      "<html><body><video><source src=\"a.mp4\"><source src=\"b.webm\"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
  }

  #[test]
  fn video_src_placeholder_falls_back_to_source_children() {
    let html = "<html><body><video src=\"#\"><source src=\"a.mp4\"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
  }

  #[test]
  fn video_src_fragment_only_falls_back_to_source_children() {
    let html = "<html><body><video src=\"#t=10\"><source src=\"a.mp4\"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
  }

  #[test]
  fn audio_src_falls_back_to_source_children() {
    let html =
      "<html><body><audio controls><source src=\"a.mp3\"><source src=\"b.ogg\"></audio></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_audio_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Audio { src } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_audio_src)
    }

    assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
  }

  #[test]
  fn audio_src_placeholder_falls_back_to_source_children() {
    let html =
      "<html><body><audio controls src=\"about:blank\"><source src=\"a.mp3\"></audio></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_audio_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Audio { src } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_audio_src)
    }

    assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
  }

  #[test]
  fn audio_src_about_blank_fragment_falls_back_to_source_children() {
    let html =
      "<html><body><audio controls src=\"about:blank#foo\"><source src=\"a.mp3\"></audio></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_audio_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Audio { src } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_audio_src)
    }

    assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
  }

  #[test]
  fn video_src_prefers_source_type_prefix() {
    let html = "<html><body><video>
      <source src=\"wrong.mp4\" type=\"audio/mp3\">
      <source src=\"right.webm\" type=\"video/webm\">
    </video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(
      find_video_src(&box_tree.root).as_deref(),
      Some("right.webm")
    );
  }

  #[test]
  fn audio_src_prefers_source_type_prefix() {
    let html = "<html><body><audio controls>
      <source src=\"wrong.mp3\" type=\"video/mp4\">
      <source src=\"right.ogg\" type=\"audio/ogg\">
    </audio></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_audio_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Audio { src } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_audio_src)
    }

    assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("right.ogg"));
  }

  #[test]
  fn video_src_attribute_wins_over_source_children() {
    let html =
      "<html><body><video src=\"parent.mp4\"><source src=\"child.mp4\"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(
      find_video_src(&box_tree.root).as_deref(),
      Some("parent.mp4")
    );
  }

  #[test]
  fn video_src_with_media_fragment_wins_over_source_children() {
    let html = "<html><body><video src=\"parent.mp4#t=10\"><source src=\"child.mp4\"></video></body></html>";
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_video_src(node: &BoxNode) -> Option<String> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if let ReplacedType::Video { src, .. } = &repl.replaced_type {
          return Some(src.clone());
        }
      }
      node.children.iter().find_map(find_video_src)
    }

    assert_eq!(
      find_video_src(&box_tree.root).as_deref(),
      Some("parent.mp4#t=10")
    );
  }

  #[test]
  fn display_contents_splices_children_into_parent() {
    let mut root = styled_element("div");
    root.node_id = 1;
    Arc::make_mut(&mut root.styles).display = Display::Block;

    let mut contents = styled_element("section");
    contents.node_id = 2;
    Arc::make_mut(&mut contents.styles).display = Display::Contents;

    let mut child1 = styled_element("p");
    child1.node_id = 3;
    Arc::make_mut(&mut child1.styles).display = Display::Block;
    let mut child2 = styled_element("p");
    child2.node_id = 4;
    Arc::make_mut(&mut child2.styles).display = Display::Block;

    contents.children = vec![child1, child2];
    root.children = vec![contents];

    let tree = generate_box_tree(&root);
    assert_eq!(
      tree.root.children.len(),
      2,
      "contents element should not create a box"
    );
    let styled_ids: Vec<_> = tree
      .root
      .children
      .iter()
      .map(|c| c.styled_node_id)
      .collect();
    assert_eq!(styled_ids, vec![Some(3), Some(4)]);
  }

  #[test]
  fn whitespace_text_nodes_are_preserved() {
    let mut root = styled_element("div");
    Arc::make_mut(&mut root.styles).display = Display::Block;
    let mut child = styled_element("span");
    Arc::make_mut(&mut child.styles).display = Display::Inline;
    child.node = dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: "   ".to_string(),
      },
      children: vec![],
    };
    root.children = vec![child];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 1);
    assert!(
      tree.root.children[0].is_text(),
      "whitespace text should produce a text box"
    );
  }

  #[test]
  fn non_ascii_whitespace_effective_content_value_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let mut style = ComputedStyle::default();
    style.content_value = ContentValue::Normal;
    style.content = format!("{nbsp}none");
    assert_eq!(
      effective_content_value(&style),
      ContentValue::Items(vec![ContentItem::String(format!("{nbsp}none"))]),
      "NBSP must not be treated as CSS whitespace when interpreting legacy content strings"
    );
  }

  // =============================================================================
  // Utility Method Tests
  // =============================================================================

  #[cfg(any(test, feature = "box_generation_demo"))]
  #[test]
  fn test_count_boxes_utility() {
    let style = default_style();

    // Single box
    let single = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    assert_eq!(BoxGenerator::count_boxes(&single), 1);

    // Parent with 3 children
    let children = vec![
      BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]),
      BoxNode::new_inline(style.clone(), vec![]),
      BoxNode::new_text(style.clone(), "text".to_string()),
    ];
    let parent = BoxNode::new_block(style, FormattingContextType::Block, children);
    assert_eq!(BoxGenerator::count_boxes(&parent), 4);
  }

  #[cfg(any(test, feature = "box_generation_demo"))]
  #[test]
  fn test_find_boxes_by_predicate() {
    let style = default_style();

    let text1 = BoxNode::new_text(style.clone(), "Hello".to_string());
    let text2 = BoxNode::new_text(style.clone(), "World".to_string());
    let inline = BoxNode::new_inline(style.clone(), vec![text1]);
    let block = BoxNode::new_block(
      style.clone(),
      FormattingContextType::Block,
      vec![inline, text2],
    );

    // Find all text boxes
    let text_boxes = BoxGenerator::find_boxes_by_predicate(&block, |b| b.is_text());
    assert_eq!(text_boxes.len(), 2);

    // Find inline boxes (inline + 2 text boxes since text is inline-level)
    let inline_boxes = BoxGenerator::find_boxes_by_predicate(&block, |b| b.is_inline_level());
    assert_eq!(inline_boxes.len(), 3); // 1 inline + 2 text boxes
  }

  #[test]
  fn test_find_block_boxes() {
    let style = default_style();

    let text = BoxNode::new_text(style.clone(), "text".to_string());
    let inline = BoxNode::new_inline(style.clone(), vec![]);
    let block1 = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let block2 = BoxNode::new_block(style.clone(), FormattingContextType::Flex, vec![]);
    let root = BoxNode::new_block(
      style,
      FormattingContextType::Block,
      vec![text, inline, block1, block2],
    );

    let blocks = BoxGenerator::find_block_boxes(&root);
    assert_eq!(blocks.len(), 3); // root + block1 + block2
  }

  #[test]
  fn test_find_inline_boxes() {
    let style = default_style();

    let text = BoxNode::new_text(style.clone(), "text".to_string());
    let inline1 = BoxNode::new_inline(style.clone(), vec![text]);
    let inline2 = BoxNode::new_inline(style.clone(), vec![]);
    let root = BoxNode::new_block(style, FormattingContextType::Block, vec![inline1, inline2]);

    // inline1 + inline2 + text (text is also inline-level)
    let inlines = BoxGenerator::find_inline_boxes(&root);
    assert_eq!(inlines.len(), 3);
  }

  #[test]
  fn test_find_text_boxes() {
    let style = default_style();

    let text1 = BoxNode::new_text(style.clone(), "Hello".to_string());
    let text2 = BoxNode::new_text(style.clone(), "World".to_string());
    let inline = BoxNode::new_inline(style.clone(), vec![text1]);
    let root = BoxNode::new_block(style, FormattingContextType::Block, vec![inline, text2]);

    let texts = BoxGenerator::find_text_boxes(&root);
    assert_eq!(texts.len(), 2);
  }

  #[test]
  fn test_find_replaced_boxes_utility() {
    let style = default_style();

    let img = BoxNode::new_replaced(
      style.clone(),
      ReplacedType::Image {
        src: "test.png".to_string(),
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
      Some(Size::new(100.0, 100.0)),
      Some(1.0),
    );
    let video = BoxNode::new_replaced(
      style.clone(),
      ReplacedType::Video {
        src: "test.mp4".to_string(),
        poster: None,
      },
      None,
      None,
    );
    let text = BoxNode::new_text(style.clone(), "text".to_string());
    let root = BoxNode::new_block(style, FormattingContextType::Block, vec![img, video, text]);

    let replaced = BoxGenerator::find_replaced_boxes(&root);
    assert_eq!(replaced.len(), 2);
  }
  #[test]
  fn fallback_marker_resets_box_model_but_inherits_color() {
    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.color = crate::style::color::Rgba::RED;
    li_style.padding_left = crate::style::values::Length::px(20.0);
    li_style.margin_left = Some(crate::style::values::Length::px(10.0));

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let tree = generate_box_tree(&styled);
    let marker = match tree.root.children.first().expect("marker").box_type {
      BoxType::Marker(_) => tree.root.children.first().unwrap(),
      _ => panic!("expected marker as first child"),
    };
    assert_eq!(marker.style.color, crate::style::color::Rgba::RED);
    assert!(marker.style.padding_left.is_zero());
    assert!(marker.style.margin_left.unwrap().is_zero());
    assert_eq!(
      marker.style.background_color,
      crate::style::color::Rgba::TRANSPARENT
    );
  }

  #[test]
  fn marker_styles_keep_text_decorations_and_shadows() {
    use crate::css::types::TextShadow;
    use crate::style::color::Rgba;
    use crate::style::counters::CounterManager;
    use crate::style::types::ListStyleType;
    use crate::style::types::TextDecorationLine;
    use crate::style::types::TextDecorationStyle;
    use crate::style::types::TextDecorationThickness;
    use crate::style::values::Length;

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;

    let mut marker_styles = ComputedStyle::default();
    marker_styles.display = Display::Inline;
    marker_styles.list_style_type = ListStyleType::Decimal;
    marker_styles.text_decoration.lines = TextDecorationLine::UNDERLINE;
    marker_styles.text_decoration.style = TextDecorationStyle::Wavy;
    marker_styles.text_decoration.color = Some(Rgba::BLUE);
    marker_styles.text_decoration.thickness = TextDecorationThickness::Length(Length::px(2.0));
    marker_styles.text_shadow = vec![TextShadow {
      offset_x: Length::px(1.0),
      offset_y: Length::px(2.0),
      blur_radius: Length::px(3.0),
      color: Some(Rgba::GREEN),
    }]
    .into();
    marker_styles.padding_left = Length::px(8.0);
    marker_styles.margin_left = Some(Length::px(4.0));
    marker_styles.background_color = Rgba::rgb(255, 0, 255);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_styles)),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth)
      .expect("marker should be generated");
    let style = marker_box.style.as_ref();
    assert!(style
      .text_decoration
      .lines
      .contains(TextDecorationLine::UNDERLINE));
    assert_eq!(style.text_decoration.style, TextDecorationStyle::Wavy);
    assert_eq!(style.text_decoration.color, Some(Rgba::BLUE));
    assert_eq!(
      style.text_decoration.thickness,
      TextDecorationThickness::Length(Length::px(2.0))
    );
    assert_eq!(style.text_shadow.len(), 1);
    assert_eq!(style.text_shadow[0].offset_x, Length::px(1.0));
    assert_eq!(style.text_shadow[0].offset_y, Length::px(2.0));
    assert_eq!(style.text_shadow[0].blur_radius, Length::px(3.0));
    assert_eq!(style.text_shadow[0].color, Some(Rgba::GREEN));

    // Layout-affecting properties are reset even when authored on ::marker.
    assert!(style.padding_left.is_zero());
    assert!(style.margin_left.unwrap().is_zero());
    assert_eq!(style.background_color, Rgba::TRANSPARENT);
  }

  #[test]
  fn marker_styles_preserve_text_transform() {
    use crate::style::counters::CounterManager;
    use crate::style::types::CaseTransform;
    use crate::style::types::ListStyleType;

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::String("abc".to_string());

    let mut marker_styles = ComputedStyle::default();
    marker_styles.display = Display::Inline;
    marker_styles.list_style_type = ListStyleType::String("abc".to_string());
    marker_styles.text_transform = TextTransform::with_case(CaseTransform::Uppercase);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_styles)),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth)
      .expect("marker should be generated");
    assert!(matches!(marker_box.box_type, BoxType::Marker(_)));
    assert_eq!(
      marker_box.style.text_transform,
      TextTransform::with_case(CaseTransform::Uppercase)
    );
  }

  #[test]
  fn pseudo_content_respects_quotes_property() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut before_style = ComputedStyle::default();
    before_style.content_value = ContentValue::Items(vec![
      ContentItem::OpenQuote,
      ContentItem::String("hi".to_string()),
    ]);
    before_style.quotes = vec![("«".to_string(), "»".to_string())].into();

    let base_style = ComputedStyle::default();
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(base_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(before_style)),
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    let mut quote_depth = 0usize;
    let before_box = create_pseudo_element_box(
      &styled,
      styled.before_styles.as_ref().unwrap(),
      clone_starting_style(&styled.starting_styles.before),
      "before",
      &mut counters,
      &mut quote_depth,
    )
    .expect("before box");
    counters.leave_scope();

    assert_eq!(before_box.children.len(), 1);
    if let BoxType::Text(text) = &before_box.children[0].box_type {
      assert_eq!(text.text, "«hi");
    } else {
      panic!("expected text child");
    }
  }

  #[test]
  fn pseudo_content_supports_attr_counter_and_image() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut before_style = ComputedStyle::default();
    before_style.content_value = ContentValue::Items(vec![
      ContentItem::Attr {
        name: "data-label".to_string(),
        type_or_unit: None,
        fallback: Some("fallback".to_string()),
      },
      ContentItem::String(": ".to_string()),
      ContentItem::Counter {
        name: "item".to_string(),
        style: Some(CounterStyle::UpperRoman.into()),
      },
      ContentItem::String(" ".to_string()),
      ContentItem::Url("icon.png".to_string()),
    ]);

    let base_style = ComputedStyle::default();
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("data-label".to_string(), "hello".to_string())],
        },
        children: vec![],
      },
      styles: Arc::new(base_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(before_style)),
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("item", 3));

    let mut quote_depth = 0usize;
    let before_box = create_pseudo_element_box(
      &styled,
      styled.before_styles.as_ref().unwrap(),
      clone_starting_style(&styled.starting_styles.before),
      "before",
      &mut counters,
      &mut quote_depth,
    )
    .expect("before box");
    counters.leave_scope();

    // Expect inline container with text + image children
    assert_eq!(before_box.children.len(), 2);
    if let BoxType::Text(text) = &before_box.children[0].box_type {
      assert_eq!(text.text, "hello: III ");
    } else {
      panic!("expected text child");
    }
    if let BoxType::Replaced(replaced) = &before_box.children[1].box_type {
      match &replaced.replaced_type {
        ReplacedType::Image { src, .. } => assert_eq!(src, "icon.png"),
        _ => panic!("expected image replaced content"),
      }
    } else {
      panic!("expected replaced child");
    }
  }

  #[test]
  fn pseudo_content_url_does_not_treat_nbsp_as_empty() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut before_style = ComputedStyle::default();
    before_style.content_value = ContentValue::Items(vec![ContentItem::Url("\u{00A0}".to_string())]);

    let base_style = ComputedStyle::default();
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(base_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(before_style)),
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    let mut quote_depth = 0usize;
    let before_box = create_pseudo_element_box(
      &styled,
      styled.before_styles.as_ref().unwrap(),
      clone_starting_style(&styled.starting_styles.before),
      "before",
      &mut counters,
      &mut quote_depth,
    )
    .expect("before box");
    counters.leave_scope();

    assert_eq!(before_box.children.len(), 1);
    if let BoxType::Replaced(replaced) = &before_box.children[0].box_type {
      match &replaced.replaced_type {
        ReplacedType::Image { src, .. } => assert_eq!(src, "\u{00A0}"),
        _ => panic!("expected image replaced content"),
      }
    } else {
      panic!("expected replaced child");
    }
  }

  #[test]
  fn marker_content_url_does_not_treat_nbsp_as_empty() {
    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;

    let mut marker_style = ComputedStyle::default();
    marker_style.content_value = ContentValue::Items(vec![ContentItem::Url("\u{00A0}".to_string())]);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_style)),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut quote_depth = 0usize;
    let marker_box =
      create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth).expect("marker box");
    let BoxType::Marker(marker) = &marker_box.box_type else {
      panic!("expected marker box");
    };
    match &marker.content {
      MarkerContent::Image(replaced) => match &replaced.replaced_type {
        ReplacedType::Image { src, .. } => assert_eq!(src, "\u{00A0}"),
        other => panic!("expected image marker content, got {other:?}"),
      },
      other => panic!("expected image marker content, got {other:?}"),
    }
  }

  fn before_pseudo_text(node: &BoxNode) -> String {
    let before = node
      .children
      .iter()
      .find(|child| child.generated_pseudo == Some(GeneratedPseudoElement::Before))
      .expect("expected ::before box");
    before
      .children
      .iter()
      .filter_map(|child| child.text())
      .collect()
  }

  #[test]
  fn before_counter_increment_affects_element_children() {
    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Block;
    container_style.counters.counter_reset = Some(CounterSet::single("x", 0));

    let mut container_before_style = ComputedStyle::default();
    container_before_style.content_value =
      ContentValue::Items(vec![ContentItem::String(String::new())]);
    container_before_style.counters.counter_increment = Some(CounterSet::single("x", 1));

    let mut span_before_style = ComputedStyle::default();
    span_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "x".to_string(),
      style: None,
    }]);
    let span_before_style = Arc::new(span_before_style);

    let mut span_a = styled_element("span");
    span_a.node_id = 1;
    span_a.before_styles = Some(Arc::clone(&span_before_style));

    let mut span_b = styled_element("span");
    span_b.node_id = 2;
    span_b.before_styles = Some(span_before_style);

    let mut container = styled_element("div");
    container.node_id = 0;
    container.styles = Arc::new(container_style);
    container.before_styles = Some(Arc::new(container_before_style));
    container.children = vec![span_a, span_b];

    let tree = generate_box_tree(&container);
    assert_eq!(tree.root.children.len(), 3);
    assert_eq!(before_pseudo_text(&tree.root.children[1]), "1");
    assert_eq!(before_pseudo_text(&tree.root.children[2]), "1");
  }

  #[test]
  fn after_counter_increment_happens_after_children() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("x", 0));

    let mut container_after_style = ComputedStyle::default();
    container_after_style.content_value =
      ContentValue::Items(vec![ContentItem::String(String::new())]);
    container_after_style.counters.counter_increment = Some(CounterSet::single("x", 1));

    let mut before_counter_style = ComputedStyle::default();
    before_counter_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "x".to_string(),
      style: None,
    }]);
    let before_counter_style = Arc::new(before_counter_style);

    let mut span_a = styled_element("span");
    span_a.node_id = 2;
    span_a.before_styles = Some(Arc::clone(&before_counter_style));

    let mut span_b = styled_element("span");
    span_b.node_id = 3;
    span_b.before_styles = Some(Arc::clone(&before_counter_style));

    let mut container = styled_element("div");
    container.node_id = 1;
    container.styles = Arc::new({
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style
    });
    container.after_styles = Some(Arc::new(container_after_style));
    container.children = vec![span_a, span_b];

    let mut sibling = styled_element("div");
    sibling.node_id = 4;
    sibling.styles = Arc::new({
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style
    });
    sibling.before_styles = Some(before_counter_style);

    let mut root = styled_element("div");
    root.node_id = 0;
    root.styles = Arc::new(root_style);
    root.children = vec![container, sibling];

    let tree = generate_box_tree(&root);
    assert_eq!(tree.root.children.len(), 2);

    let container_box = &tree.root.children[0];
    assert_eq!(before_pseudo_text(&container_box.children[0]), "0");
    assert_eq!(before_pseudo_text(&container_box.children[1]), "0");

    let sibling_box = &tree.root.children[1];
    assert_eq!(before_pseudo_text(sibling_box), "1");
  }

  #[test]
  fn marker_counter_increment_affects_list_item_children() {
    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.counters.counter_reset = Some(CounterSet::single("x", 0));

    let mut marker_style = ComputedStyle::default();
    marker_style.content_value = ContentValue::Items(vec![ContentItem::String(String::new())]);
    marker_style.counters.counter_increment = Some(CounterSet::single("x", 1));

    let mut before_style = ComputedStyle::default();
    before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "x".to_string(),
      style: None,
    }]);

    let mut li = styled_element("li");
    li.node_id = 0;
    li.styles = Arc::new(li_style);
    li.marker_styles = Some(Arc::new(marker_style));
    li.before_styles = Some(Arc::new(before_style));

    let tree = generate_box_tree(&li);
    assert_eq!(tree.root.children.len(), 2);
    assert!(matches!(tree.root.children[0].box_type, BoxType::Marker(_)));
    assert_eq!(before_pseudo_text(&tree.root), "1");
  }

  #[test]
  fn before_counter_increment_affects_descendant_generated_content() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

    let mut parent_before_style = ComputedStyle::default();
    parent_before_style.content_value = ContentValue::Items(Vec::new());
    parent_before_style.counters.counter_increment = Some(CounterSet::single("section", 1));

    let mut child_before_style = ComputedStyle::default();
    child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "section".to_string(),
      style: None,
    }]);

    let child = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(child_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let parent = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(parent_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![parent],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "1",
      "descendant ::before should see counter increment from ancestor ::before"
    );
  }

  #[test]
  fn display_contents_nodes_do_not_affect_counters() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

    let mut contents_style = ComputedStyle::default();
    contents_style.display = Display::Contents;
    contents_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

    let mut probe_before_style = ComputedStyle::default();
    probe_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "foo".to_string(),
      style: None,
    }]);

    let contents_node = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(contents_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let probe_node = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(probe_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![contents_node, probe_node],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "0",
      "display:contents elements must not apply counter-increment"
    );
  }

  #[test]
  fn pseudo_element_content_none_does_not_affect_counters() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

    let mut suppressed_before_style = ComputedStyle::default();
    suppressed_before_style.content_value = ContentValue::None;
    suppressed_before_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

    let mut child_before_style = ComputedStyle::default();
    child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "foo".to_string(),
      style: None,
    }]);

    let child = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(child_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let parent = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(suppressed_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![parent],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "0",
      "::before with content:none must not apply counter-increment"
    );
  }

  #[test]
  fn marker_content_none_does_not_affect_counters() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;
    use crate::style::types::ListStyleType;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;

    let mut marker_style = ComputedStyle::default();
    marker_style.content_value = ContentValue::None;
    marker_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

    let mut child_before_style = ComputedStyle::default();
    child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "foo".to_string(),
      style: None,
    }]);

    let child = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(child_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_style)),
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "0",
      "::marker with content:none must not apply counter-increment"
    );
  }

  #[test]
  fn marker_counter_increment_affects_list_item_descendants() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;
    use crate::style::types::ListStyleType;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;

    let mut marker_style = ComputedStyle::default();
    marker_style.display = Display::Inline;
    marker_style.content_value = ContentValue::Items(vec![ContentItem::String("M".to_string())]);
    marker_style.counters.counter_increment = Some(CounterSet::single("section", 1));

    let mut child_before_style = ComputedStyle::default();
    child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "section".to_string(),
      style: None,
    }]);

    let child = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(child_before_style)),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(li_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_style)),
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "1",
      "list item descendant should see counter increment from ::marker"
    );
  }

  #[test]
  fn after_counter_increment_occurs_after_descendants() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

    let mut child_before_style = ComputedStyle::default();
    child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "section".to_string(),
      style: None,
    }]);

    let mk_counter_span = |node_id: usize| StyledNode {
      node_id,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "span".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(Arc::new(child_before_style.clone())),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut after_style = ComputedStyle::default();
    after_style.content_value = ContentValue::Items(Vec::new());
    after_style.counters.counter_increment = Some(CounterSet::single("section", 1));

    let target = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: Some(Arc::new(after_style)),
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_counter_span(2)],
    };

    let sibling = StyledNode {
      node_id: 3,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "p".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_counter_span(4)],
    };

    let root = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(root_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![target, sibling],
    };

    let tree = generate_box_tree(&root);
    assert_eq!(
      pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
      "0",
      "descendants should observe counter before ::after increments run"
    );
    assert_eq!(
      pseudo_text(&tree.root, 4, GeneratedPseudoElement::Before),
      "1",
      "following siblings should observe counter after ::after increments run"
    );
  }

  #[test]
  fn marker_content_resolves_structured_content() {
    use crate::dom::DomNodeType;
    use crate::style::content::ContentItem;
    use crate::style::content::ContentValue;

    let mut marker_style = ComputedStyle::default();
    marker_style.content_value = ContentValue::Items(vec![
      ContentItem::String("[".to_string()),
      ContentItem::Counter {
        name: "item".to_string(),
        style: Some(CounterStyle::LowerRoman.into()),
      },
      ContentItem::String("]".to_string()),
    ]);

    let base_style = ComputedStyle::default();
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::new(base_style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: Some(Arc::new(marker_style)),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("item", 2));
    counters.apply_increment(&CounterSet::single("item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "[iii]"),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn marker_uses_string_list_style_type() {
    let mut style = ComputedStyle::default();
    style.list_style_type = ListStyleType::String("★".to_string());
    style.display = Display::ListItem;
    let style = Arc::new(style);
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "★"),
        MarkerContent::Image(_) => panic!("expected text marker from string list-style-type"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn marker_uses_symbols_list_style_type() {
    let mut style = ComputedStyle::default();
    style.display = Display::ListItem;
    style.list_style_type = ListStyleType::Symbols(SymbolsCounterStyle {
      system: SymbolsType::Symbolic,
      symbols: vec!["*".to_string(), "†".to_string()],
    });
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 3));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "** "),
        MarkerContent::Image(_) => panic!("expected text marker from symbols() list-style-type"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn disclosure_closed_marker_points_right_in_ltr() {
    let mut style = ComputedStyle::default();
    style.display = Display::ListItem;
    style.list_style_type = ListStyleType::DisclosureClosed;
    style.direction = Direction::Ltr;
    style.writing_mode = WritingMode::HorizontalTb;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "▸ "),
        MarkerContent::Image(_) => panic!("expected text marker from disclosure-closed"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn disclosure_closed_marker_points_left_in_rtl() {
    let mut style = ComputedStyle::default();
    style.display = Display::ListItem;
    style.list_style_type = ListStyleType::DisclosureClosed;
    style.direction = Direction::Rtl;
    style.writing_mode = WritingMode::HorizontalTb;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "◂ "),
        MarkerContent::Image(_) => panic!("expected text marker from disclosure-closed"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn disc_marker_has_space_suffix() {
    let mut style = ComputedStyle::default();
    style.display = Display::ListItem;
    style.list_style_type = ListStyleType::Disc;
    let style = Arc::new(style);
    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "• "),
        MarkerContent::Image(_) => panic!("expected text marker from disc"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn marker_uses_custom_counter_style_definition() {
    let mut registry = CounterStyleRegistry::with_builtins();
    let mut rule = CounterStyleRule::new("custom-mark");
    rule.system = Some(CounterSystem::Cyclic);
    rule.symbols = Some(vec!["◇".into()]);
    registry.register(rule);
    let registry = Arc::new(registry);

    let mut style = ComputedStyle::default();
    style.counter_styles = registry.clone();
    style.list_style_type = ListStyleType::Custom("custom-mark".to_string());
    style.display = Display::ListItem;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new_with_styles(registry);
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "◇. "),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn decimal_marker_has_dot_suffix() {
    let mut style = ComputedStyle::default();
    style.list_style_type = ListStyleType::Decimal;
    style.display = Display::ListItem;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: None,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new();
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "1. "),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn custom_counter_style_prefix_suffix_descriptors_are_used_for_marker() {
    let mut registry = CounterStyleRegistry::with_builtins();
    let mut rule = CounterStyleRule::new("custom-mark");
    rule.system = Some(CounterSystem::Cyclic);
    rule.symbols = Some(vec!["◇".into()]);
    rule.prefix = Some("(".to_string());
    rule.suffix = Some(") ".to_string());
    registry.register(rule);
    let registry = Arc::new(registry);

    let mut style = ComputedStyle::default();
    style.counter_styles = registry.clone();
    style.list_style_type = ListStyleType::Custom("custom-mark".to_string());
    style.display = Display::ListItem;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new_with_styles(registry);
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "(◇) "),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn custom_counter_style_extends_inherits_prefix_suffix_for_marker() {
    let mut registry = CounterStyleRegistry::with_builtins();
    let mut base = CounterStyleRule::new("base-mark");
    base.system = Some(CounterSystem::Cyclic);
    base.symbols = Some(vec!["◇".into()]);
    base.prefix = Some("(".to_string());
    base.suffix = Some(") ".to_string());
    registry.register(base);

    let mut derived = CounterStyleRule::new("derived-mark");
    derived.system = Some(CounterSystem::Extends("base-mark".to_string()));
    derived.symbols = Some(vec!["◆".into()]);
    registry.register(derived);
    let registry = Arc::new(registry);

    let mut style = ComputedStyle::default();
    style.counter_styles = registry.clone();
    style.list_style_type = ListStyleType::Custom("derived-mark".to_string());
    style.display = Display::ListItem;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new_with_styles(registry);
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "(◆) "),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn custom_counter_style_extends_cycle_falls_back_to_decimal_marker() {
    let mut registry = CounterStyleRegistry::with_builtins();
    let mut a = CounterStyleRule::new("cycle-a");
    a.system = Some(CounterSystem::Extends("cycle-b".to_string()));
    a.symbols = Some(vec!["A".into()]);
    registry.register(a);
    let mut b = CounterStyleRule::new("cycle-b");
    b.system = Some(CounterSystem::Extends("cycle-a".to_string()));
    b.symbols = Some(vec!["B".into()]);
    registry.register(b);
    let registry = Arc::new(registry);

    let mut style = ComputedStyle::default();
    style.counter_styles = registry.clone();
    style.list_style_type = ListStyleType::Custom("cycle-a".to_string());
    style.display = Display::ListItem;
    let style = Arc::new(style);

    let styled = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: style.clone(),
      marker_styles: Some(style.clone()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut counters = CounterManager::new_with_styles(registry);
    counters.enter_scope();
    counters.apply_reset(&CounterSet::single("list-item", 1));

    let mut quote_depth = 0usize;
    let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
    counters.leave_scope();

    match &marker_box.box_type {
      BoxType::Marker(marker) => match &marker.content {
        MarkerContent::Text(t) => assert_eq!(t.as_str(), "1. "),
        MarkerContent::Image(_) => panic!("expected text marker"),
      },
      _ => panic!("expected marker box"),
    }
  }

  #[test]
  fn unordered_list_decimal_starts_at_one() {
    let mut ul_style = ComputedStyle::default();
    ul_style.display = Display::Block;
    let ul_style = Arc::new(ul_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let mk_li = |node_id: usize| StyledNode {
      node_id,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let ul = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ul".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: ul_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_li(1), mk_li(2), mk_li(3)],
    };

    let tree = generate_box_tree(&ul);

    fn collect_marker_numbers(node: &BoxNode, out: &mut Vec<i32>) {
      if let BoxType::Marker(marker) = &node.box_type {
        if let MarkerContent::Text(text) = &marker.content {
          out.push(marker_leading_decimal(text));
        }
      }
      for child in node.children.iter() {
        collect_marker_numbers(child, out);
      }
    }

    let mut markers = Vec::new();
    collect_marker_numbers(&tree.root, &mut markers);
    assert_eq!(markers, vec![1, 2, 3]);
  }

  #[test]
  fn ordered_list_start_attribute_sets_initial_counter() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("start".to_string(), "5".to_string())],
      },
      children: vec![],
    };

    let mk_li = |text: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![5, 6, 7]);
  }

  #[test]
  fn ordered_list_defaults_start_at_one() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let mk_li = |text: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![1, 2, 3]);
  }

  #[test]
  fn list_item_implicit_increment_survives_counter_increment_none() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    li_style.counters.counter_increment = Some(CounterSet::new());
    let li_style = Arc::new(li_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("one"), li("two")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![1, 2]);
  }

  #[test]
  fn list_item_implicit_increment_survives_counter_increment_other_counter() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    li_style.counters.counter_increment = Some(CounterSet::single("foo", 1));
    let li_style = Arc::new(li_style);

    let mut before_style = ComputedStyle::default();
    before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
      name: "foo".to_string(),
      style: None,
    }]);
    let before_style = Arc::new(before_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: Some(before_style.clone()),
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("one"), li("two")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![1, 2]);

    let before_values: Vec<i32> = tree
      .root
      .children
      .iter()
      .map(|li| {
        let before = li
          .children
          .iter()
          .find(|child| child.generated_pseudo == Some(GeneratedPseudoElement::Before))
          .expect("expected ::before pseudo-element box");
        let text = before
          .children
          .iter()
          .find_map(|child| child.text())
          .expect("expected ::before to contain text");
        text.parse::<i32>().expect("parse foo counter text")
      })
      .collect();

    assert_eq!(before_values, vec![1, 1]);
  }

  #[test]
  fn display_none_list_items_do_not_increment_list_item_counter() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let mut li_hidden_style = ComputedStyle::default();
    li_hidden_style.display = Display::None;
    li_hidden_style.list_style_type = ListStyleType::Decimal;
    let li_hidden_style = Arc::new(li_hidden_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = |content: &str, hidden: bool| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: if hidden {
        li_hidden_style.clone()
      } else {
        li_style.clone()
      },
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("one", false), li("two", true), li("three", false)],
    };

    let tree = generate_box_tree(&ol);
    assert_eq!(
      tree.root.children.len(),
      2,
      "hidden list items should not generate boxes"
    );

    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![1, 2]);
  }

  #[test]
  fn reversed_ordered_list_counts_down() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    };

    let mk_li = |text: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![3, 2, 1]);
  }

  #[test]
  fn reversed_list_default_start_ignores_display_none_items() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let mut li_hidden_style = ComputedStyle::default();
    li_hidden_style.display = Display::None;
    li_hidden_style.list_style_type = ListStyleType::Decimal;
    let li_hidden_style = Arc::new(li_hidden_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = |content: &str, hidden: bool| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: if hidden {
        li_hidden_style.clone()
      } else {
        li_style.clone()
      },
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("reversed".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("one", false), li("two", true), li("three", false)],
    };

    let tree = generate_box_tree(&ol);
    assert_eq!(
      tree.root.children.len(),
      2,
      "hidden list items should not generate boxes"
    );

    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![2, 1]);
  }

  #[test]
  fn reversed_list_default_start_counts_items_inside_display_contents_lists() {
    use crate::dom::DomNodeType;
    use crate::style::types::ListStyleType;

    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let mut contents_ol_style = ComputedStyle::default();
    contents_ol_style.display = Display::Contents;
    let contents_ol_style = Arc::new(contents_ol_style);

    let nested_li = StyledNode {
      node_id: 4,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let nested_ol = StyledNode {
      node_id: 3,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: contents_ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![nested_li],
    };

    let li1 = StyledNode {
      node_id: 1,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li2 = StyledNode {
      node_id: 2,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![nested_ol],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("reversed".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li1, li2],
    };

    let tree = generate_box_tree(&ol);

    fn collect_marker_numbers(node: &BoxNode, out: &mut Vec<i32>) {
      if let BoxType::Marker(marker) = &node.box_type {
        if let MarkerContent::Text(text) = &marker.content {
          out.push(marker_leading_decimal(text));
        }
      }
      for child in node.children.iter() {
        collect_marker_numbers(child, out);
      }
    }

    let mut markers = Vec::new();
    collect_marker_numbers(&tree.root, &mut markers);
    assert_eq!(markers, vec![3, 2, 1]);
  }

  #[test]
  fn li_value_attribute_sets_counter_for_that_item() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    };

    let mk_li = |text: &str, value: Option<&str>| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: value
            .map(|v| vec![("value".to_string(), v.to_string())])
            .unwrap_or_else(Vec::new),
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![
        mk_li("one", None),
        mk_li("two", Some("10")),
        mk_li("three", None),
      ],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![1, 10, 11]);
  }

  #[test]
  fn reversed_list_value_attribute_counts_down_from_value() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    };

    let mk_li = |text: &str, value: Option<&str>| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: value
            .map(|v| vec![("value".to_string(), v.to_string())])
            .unwrap_or_else(Vec::new),
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: text.to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![
        mk_li("one", None),
        mk_li("two", Some("10")),
        mk_li("three", None),
      ],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![3, 10, 9]);
  }

  #[test]
  fn reversed_list_ignores_nested_items_for_default_start() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let nested_ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("reversed".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: ol_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Element {
            tag_name: "li".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![dom::DomNode {
            node_type: dom::DomNodeType::Text {
              content: "inner".to_string(),
            },
            children: vec![],
          }],
        },
        styles: li_style.clone(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![StyledNode {
          node_id: 0,
          node: dom::DomNode {
            node_type: dom::DomNodeType::Text {
              content: "inner".to_string(),
            },
            children: vec![],
          },
          styles: default_style(),
          starting_styles: StartingStyleSet::default(),
          before_styles: None,
          after_styles: None,
          marker_styles: None,
          placeholder_styles: None,
          file_selector_button_styles: None,
          footnote_call_styles: None,
          footnote_marker_styles: None,
          first_line_styles: None,
          first_letter_styles: None,
          slider_thumb_styles: None,
          slider_track_styles: None,
          progress_bar_styles: None,
          progress_value_styles: None,
          meter_bar_styles: None,
          meter_optimum_value_styles: None,
          meter_suboptimum_value_styles: None,
          meter_even_less_good_value_styles: None,
          assigned_slot: None,
          slotted_node_ids: Vec::new(),
          children: vec![],
        }],
      }],
    };

    let ol_dom = dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    };

    let outer_li = |child: StyledNode| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![child.node.clone()],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };

    let ol = StyledNode {
      node_id: 0,
      node: ol_dom,
      styles: ol_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![
        outer_li(StyledNode {
          node_id: 0,
          node: dom::DomNode {
            node_type: dom::DomNodeType::Text {
              content: "outer1".to_string(),
            },
            children: vec![],
          },
          styles: default_style(),
          starting_styles: StartingStyleSet::default(),
          before_styles: None,
          after_styles: None,
          marker_styles: None,
          placeholder_styles: None,
          file_selector_button_styles: None,
          footnote_call_styles: None,
          footnote_marker_styles: None,
          first_line_styles: None,
          first_letter_styles: None,
          slider_thumb_styles: None,
          slider_track_styles: None,
          progress_bar_styles: None,
          progress_value_styles: None,
          meter_bar_styles: None,
          meter_optimum_value_styles: None,
          meter_suboptimum_value_styles: None,
          meter_even_less_good_value_styles: None,
          assigned_slot: None,
          slotted_node_ids: Vec::new(),
          children: vec![],
        }),
        outer_li(nested_ol),
      ],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|li| {
        li.children.first().and_then(|child| match &child.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        })
      })
      .collect();

    assert_eq!(markers, vec![2, 1]);
  }

  #[test]
  fn reversed_list_skips_menu_items_for_default_start() {
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    ol_style.list_style_type = ListStyleType::Decimal;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let mut menu_style = ComputedStyle::default();
    menu_style.display = Display::Block;
    menu_style.list_style_type = ListStyleType::Disc;
    let menu_style = Arc::new(menu_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let li = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let menu = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "menu".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: menu_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("skip-1"), li("skip-2")],
    };

    let ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("reversed".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![li("one"), menu, li("two")],
    };

    let tree = generate_box_tree(&ol);
    let markers: Vec<i32> = tree
      .root
      .children
      .iter()
      .filter_map(|child| match &child.box_type {
        BoxType::Block(_) => child.children.first().and_then(|c| match &c.box_type {
          BoxType::Marker(m) => match &m.content {
            MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
            MarkerContent::Image(_) => None,
          },
          _ => None,
        }),
        _ => None,
      })
      .collect();

    // Only top-level list items should contribute to the count: markers 2, 1.
    assert_eq!(markers, vec![2, 1]);
  }

  #[test]
  fn nested_list_resets_increment_to_default() {
    // Outer reversed list should not force inner list to count down; inner list starts at 1.
    let mut ol_style = ComputedStyle::default();
    ol_style.display = Display::Block;
    ol_style.list_style_type = ListStyleType::Decimal;
    let ol_style = Arc::new(ol_style);

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_type = ListStyleType::Decimal;
    let li_style = Arc::new(li_style);

    let text_node = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mk_li = |content: &str| StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node(content)],
    };

    let inner_ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: ol_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![mk_li("inner-one"), mk_li("inner-two")],
    };

    let outer_li = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![text_node("outer"), inner_ol],
    };

    let outer_ol = StyledNode {
      node_id: 0,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "ol".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("reversed".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: ol_style,
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![outer_li],
    };

    let tree = generate_box_tree(&outer_ol);
    let inner_box = tree.root.children[0]
      .children
      .iter()
      .find(|child| matches!(child.box_type, BoxType::Block(_)))
      .expect("inner list block");

    let first_inner_marker = match &inner_box.children[0].children[0].box_type {
      BoxType::Marker(marker) => marker.content.clone(),
      other => panic!("expected marker for first inner item, got {:?}", other),
    };
    let second_inner_marker = match &inner_box.children[1].children[0].box_type {
      BoxType::Marker(marker) => marker.content.clone(),
      other => panic!("expected marker for second inner item, got {:?}", other),
    };

    let first_text = match first_inner_marker {
      MarkerContent::Text(t) => t,
      MarkerContent::Image(_) => panic!("expected text marker, got Image"),
    };
    let second_text = match second_inner_marker {
      MarkerContent::Text(t) => t,
      MarkerContent::Image(_) => panic!("expected text marker, got Image"),
    };

    assert_eq!(first_text, "1. ");
    assert_eq!(second_text, "2. ");
  }

  #[test]
  fn inline_svg_carries_serialized_content() {
    let html = r#"<html><body><svg width="10" height="10"><rect width="10" height="10" fill="red"/></svg></body></html>"#;
    let dom = crate::dom::parse_html(html).expect("parse");
    let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
    let box_tree = generate_box_tree(&styled);

    fn find_svg(node: &BoxNode) -> Option<&ReplacedBox> {
      if let BoxType::Replaced(repl) = &node.box_type {
        if matches!(repl.replaced_type, ReplacedType::Svg { .. }) {
          return Some(repl);
        }
      }
      for child in node.children.iter() {
        if let Some(found) = find_svg(child) {
          return Some(found);
        }
      }
      None
    }

    let svg = find_svg(&box_tree.root).expect("svg replaced box");
    match &svg.replaced_type {
      ReplacedType::Svg { content } => {
        assert!(
          content.svg.contains("<rect") && content.svg.contains("fill=\"red\""),
          "serialized SVG should include child elements"
        );
      }
      other => panic!("expected svg replaced type, got {:?}", other),
    }
  }

  #[test]
  fn deep_box_generation_does_not_overflow_stack() {
    use style::display::Display;

    let depth = 100_000usize;
    let mut computed = ComputedStyle::default();
    computed.display = Display::Block;
    let style = Arc::new(computed);

    let mut node = StyledNode {
      node_id: depth,
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::clone(&style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    for node_id in (0..depth).rev() {
      node = StyledNode {
        node_id,
        node: DomNode {
          node_type: DomNodeType::Element {
            tag_name: "div".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
        styles: Arc::clone(&style),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![node],
      };
    }

    let _tree = generate_box_tree_with_anonymous_fixup_result(&node)
      .expect("deep box generation should not overflow stack");
  }

  #[test]
  fn deep_select_form_control_generation_does_not_overflow_stack() {
    use style::display::Display;

    let depth = 100_000usize;
    let mut computed = ComputedStyle::default();
    computed.display = Display::InlineBlock;
    let style = Arc::new(computed);

    let mut option = StyledNode {
      node_id: depth + 1,
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "option".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![
            ("value".to_string(), "chosen".to_string()),
            ("label".to_string(), "chosen".to_string()),
            ("selected".to_string(), String::new()),
          ],
        },
        children: vec![],
      },
      styles: Arc::clone(&style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    for node_id in (1..=depth).rev() {
      option = StyledNode {
        node_id,
        node: DomNode {
          node_type: DomNodeType::Element {
            tag_name: "optgroup".to_string(),
            namespace: HTML_NAMESPACE.to_string(),
            attributes: vec![],
          },
          children: vec![],
        },
        styles: Arc::clone(&style),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![option],
      };
    }

    let select = StyledNode {
      node_id: 0,
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "select".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![("required".to_string(), String::new())],
        },
        children: vec![],
      },
      styles: Arc::clone(&style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![option],
    };

    let tree = generate_box_tree(&select);
    let BoxType::Replaced(replaced) = &tree.root.box_type else {
      panic!("expected select to generate a replaced box");
    };
    let ReplacedType::FormControl(FormControl { control, .. }) = &replaced.replaced_type else {
      panic!("expected form control replaced type");
    };
    let FormControlKind::Select(select) = control else {
      panic!("expected select form control kind");
    };
    assert!(!select.multiple);
    assert!(select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "chosen"
    )));
  }

  #[test]
  fn select_fallback_selects_first_option_when_all_disabled() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut first = styled_element("option");
    set_attr(&mut first, "disabled", "");
    set_attr(&mut first, "label", "First");
    set_attr(&mut first, "value", "a");

    let mut second = styled_element("option");
    set_attr(&mut second, "disabled", "");
    set_attr(&mut second, "label", "Second");
    set_attr(&mut second, "value", "b");

    let mut select = styled_element("select");
    select.children = vec![first, second];

    let control = create_form_control_replaced(&select).expect("select form control");
    assert!(!control.invalid);
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(!select.multiple);
    assert_eq!(select.size, 1);

    let idx = select.selected.first().copied().expect("selected option");
    let SelectItem::Option {
      label,
      value,
      selected,
      disabled,
      ..
    } = &select.items[idx]
    else {
      panic!("expected option item");
    };
    assert_eq!(label, "First");
    assert_eq!(value, "a");
    assert!(*selected);
    assert!(*disabled);
    assert_eq!(select_selected_value(select), Some("a"));
  }

  #[test]
  fn required_select_with_disabled_selected_placeholder_remains_selected_and_invalid() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let placeholder_text = StyledNode {
      node_id: 0,
      node: DomNode {
        node_type: DomNodeType::Text {
          content: "Choose".to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    };

    let mut placeholder = styled_element("option");
    set_attr(&mut placeholder, "disabled", "");
    set_attr(&mut placeholder, "selected", "");
    set_attr(&mut placeholder, "value", "");
    placeholder.children.push(placeholder_text);

    let mut enabled = styled_element("option");
    set_attr(&mut enabled, "value", "x");

    let mut select = styled_element("select");
    set_attr(&mut select, "required", "");
    select.children = vec![placeholder, enabled];

    let control = create_form_control_replaced(&select).expect("select form control");
    assert!(control.required);
    assert!(control.invalid);
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(!select.multiple);
    assert_eq!(select.size, 1);

    let idx = select.selected.first().copied().expect("selected option");
    let SelectItem::Option {
      label,
      value,
      selected,
      disabled,
      ..
    } = &select.items[idx]
    else {
      panic!("expected option item");
    };
    assert_eq!(label, "Choose");
    assert_eq!(value, "");
    assert!(*selected);
    assert!(*disabled);
    assert_eq!(select_selected_value(select), Some(""));
  }

  #[test]
  fn select_size_parsing_controls_effective_row_count() {
    fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
      match &mut node.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push((name.to_string(), value.to_string()));
        }
        _ => panic!("expected element node"),
      }
    }

    let mut dropdown_size0 = styled_element("select");
    set_attr(&mut dropdown_size0, "size", "0");
    let control = create_form_control_replaced(&dropdown_size0).expect("select form control");
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(!select.multiple);
    assert_eq!(select.size, 1);

    let mut multi_default = styled_element("select");
    set_attr(&mut multi_default, "multiple", "");
    let control = create_form_control_replaced(&multi_default).expect("select form control");
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(select.multiple);
    assert_eq!(select.size, 4);

    let mut multi_invalid = styled_element("select");
    set_attr(&mut multi_invalid, "multiple", "");
    set_attr(&mut multi_invalid, "size", "abc");
    let control = create_form_control_replaced(&multi_invalid).expect("select form control");
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(select.multiple);
    assert_eq!(select.size, 4);

    let mut multi_size3 = styled_element("select");
    set_attr(&mut multi_size3, "multiple", "");
    set_attr(&mut multi_size3, "size", "3");
    let control = create_form_control_replaced(&multi_size3).expect("select form control");
    let FormControlKind::Select(select) = &control.control else {
      panic!("expected select control kind");
    };
    assert!(select.multiple);
    assert_eq!(select.size, 3);
  }

  #[test]
  fn pseudo_element_content_ignores_empty_url() {
    let styled = styled_element("div");
    let mut counters = CounterManager::new();
    let mut quote_depth = 0usize;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.content_value = ContentValue::Items(vec![ContentItem::Url(String::new())]);
    let pseudo_style = Arc::new(pseudo_style);

    let pseudo_box =
      create_pseudo_element_box(&styled, &pseudo_style, None, "before", &mut counters, &mut quote_depth).expect(
        "pseudo-element boxes should still be generated when content isn't none/normal, even if the resolved url is empty",
      );
    assert!(
      pseudo_box.children.is_empty(),
      "empty url() content items should not generate replaced children"
    );
  }

  #[test]
  fn pseudo_element_content_generates_box_for_empty_string() {
    let styled = styled_element("div");
    let mut counters = CounterManager::new();
    let mut quote_depth = 0usize;

    let mut pseudo_style = ComputedStyle::default();
    pseudo_style.content_value = ContentValue::Items(vec![ContentItem::String(String::new())]);
    let pseudo_style = Arc::new(pseudo_style);

    let pseudo_box =
      create_pseudo_element_box(&styled, &pseudo_style, None, "before", &mut counters, &mut quote_depth)
        .expect("empty string content should still generate the pseudo-element box");
    assert!(
      pseudo_box.children.is_empty(),
      "empty string content shouldn't implicitly create placeholder text children"
    );
  }

  #[test]
  fn marker_content_ignores_empty_url() {
    let styled = styled_element("li");
    let counters = CounterManager::new();
    let mut quote_depth = 0usize;

    let mut marker_style = ComputedStyle::default();
    marker_style.content_value = ContentValue::Items(vec![ContentItem::Url(String::new())]);

    assert!(
      marker_content_from_style(&styled, &marker_style, &counters, &mut quote_depth).is_none(),
      "empty url() content items should not generate marker images"
    );
  }
}

fn list_marker_text(marker_style: &ComputedStyle, counters: &CounterManager) -> String {
  let value = counters.get_or_zero("list-item");
  let registry = marker_style.counter_styles.as_ref();
  match &marker_style.list_style_type {
    ListStyleType::None => String::new(),
    ListStyleType::String(text) => text.clone(),
    ListStyleType::Disc => registry.format_marker_string(value, CounterStyle::Disc),
    ListStyleType::Circle => registry.format_marker_string(value, CounterStyle::Circle),
    ListStyleType::Square => registry.format_marker_string(value, CounterStyle::Square),
    ListStyleType::Decimal => registry.format_marker_string(value, CounterStyle::Decimal),
    ListStyleType::DecimalLeadingZero => {
      registry.format_marker_string(value, CounterStyle::DecimalLeadingZero)
    }
    ListStyleType::LowerRoman => registry.format_marker_string(value, CounterStyle::LowerRoman),
    ListStyleType::UpperRoman => registry.format_marker_string(value, CounterStyle::UpperRoman),
    ListStyleType::LowerAlpha => registry.format_marker_string(value, CounterStyle::LowerAlpha),
    ListStyleType::UpperAlpha => registry.format_marker_string(value, CounterStyle::UpperAlpha),
    ListStyleType::Armenian => registry.format_marker_string(value, CounterStyle::Armenian),
    ListStyleType::LowerArmenian => {
      registry.format_marker_string(value, CounterStyle::LowerArmenian)
    }
    ListStyleType::Georgian => registry.format_marker_string(value, CounterStyle::Georgian),
    ListStyleType::LowerGreek => registry.format_marker_string(value, CounterStyle::LowerGreek),
    ListStyleType::DisclosureOpen => registry.format_marker_string(value, CounterStyle::DisclosureOpen),
    ListStyleType::DisclosureClosed => {
      let symbol = match marker_style.writing_mode {
        WritingMode::HorizontalTb => match marker_style.direction {
          Direction::Ltr => "▸",
          Direction::Rtl => "◂",
        },
        _ => "▸",
      };
      let (prefix, suffix) = registry.marker_affixes(CounterStyle::DisclosureClosed);
      let mut out = String::with_capacity(prefix.len() + symbol.len() + suffix.len());
      out.push_str(&prefix);
      out.push_str(symbol);
      out.push_str(&suffix);
      out
    }
    ListStyleType::Custom(name) => {
      registry.format_marker_string(value, CounterStyleName::Custom(name.clone()))
    }
    ListStyleType::Symbols(symbols) => format_symbols_marker_string(value, symbols),
  }
}

fn format_symbols_marker_string(value: i32, symbols: &SymbolsCounterStyle) -> String {
  // CSS Counter Styles 3 §7.1 `symbols()` defaults:
  // - prefix: "" (empty string)
  // - suffix: " " (U+0020 SPACE)
  let mut repr = format_symbols_representation(value, symbols);
  repr.push(' ');
  repr
}

fn format_symbols_representation(value: i32, symbols: &SymbolsCounterStyle) -> String {
  // `symbols()` declares a fixed set of descriptors with fallback `decimal`.
  let fallback = || value.to_string();
  if symbols.symbols.is_empty() {
    return fallback();
  }

  let value_i64 = value as i64;
  let in_range = match symbols.system {
    SymbolsType::Alphabetic | SymbolsType::Symbolic => value_i64 >= 1,
    SymbolsType::Fixed => value_i64 >= 1 && value_i64 <= symbols.symbols.len() as i64,
    SymbolsType::Cyclic | SymbolsType::Numeric => true,
  };
  if !in_range {
    return fallback();
  }

  let uses_negative_sign = matches!(
    symbols.system,
    SymbolsType::Numeric | SymbolsType::Alphabetic | SymbolsType::Symbolic
  );
  let negative_value = value_i64 < 0;
  let initial_value = if negative_value && uses_negative_sign {
    value_i64.abs()
  } else {
    value_i64
  };

  let repr = match format_symbols_positive(initial_value, symbols.system, &symbols.symbols) {
    Some(r) => r,
    None => return fallback(),
  };

  if negative_value && uses_negative_sign {
    format!("-{repr}")
  } else {
    repr
  }
}

fn format_symbols_positive(value: i64, system: SymbolsType, symbols: &[String]) -> Option<String> {
  match system {
    SymbolsType::Cyclic => format_symbols_cyclic(value, symbols),
    SymbolsType::Fixed => format_symbols_fixed(value, 1, symbols),
    SymbolsType::Numeric => format_symbols_numeric(value, symbols),
    SymbolsType::Alphabetic => format_symbols_alphabetic(value, symbols),
    SymbolsType::Symbolic => format_symbols_symbolic(value, symbols),
  }
}

fn format_symbols_cyclic(value: i64, symbols: &[String]) -> Option<String> {
  if symbols.is_empty() {
    return None;
  }
  let len = symbols.len() as i64;
  let idx = (value - 1).rem_euclid(len) as usize;
  symbols.get(idx).cloned()
}

fn format_symbols_fixed(value: i64, start: i64, symbols: &[String]) -> Option<String> {
  if symbols.is_empty() {
    return None;
  }
  let idx = value - start;
  if idx < 0 || idx >= symbols.len() as i64 {
    return None;
  }
  symbols.get(idx as usize).cloned()
}

fn format_symbols_numeric(mut value: i64, symbols: &[String]) -> Option<String> {
  if symbols.len() < 2 || value < 0 {
    return None;
  }
  let base = symbols.len() as i64;
  if value == 0 {
    return symbols.get(0).cloned();
  }
  let mut out = Vec::new();
  while value > 0 {
    let digit = (value % base) as usize;
    out.push(symbols.get(digit)?.clone());
    value /= base;
  }
  out.reverse();
  Some(out.join(""))
}

fn format_symbols_alphabetic(mut value: i64, symbols: &[String]) -> Option<String> {
  if symbols.len() < 2 || value <= 0 {
    return None;
  }
  let base = symbols.len() as i64;
  let mut out = Vec::new();
  while value > 0 {
    value -= 1;
    let digit = (value % base) as usize;
    out.push(symbols.get(digit)?.clone());
    value /= base;
  }
  out.reverse();
  Some(out.join(""))
}

fn format_symbols_symbolic(value: i64, symbols: &[String]) -> Option<String> {
  if symbols.is_empty() || value <= 0 {
    return None;
  }
  let n = symbols.len() as i64;
  let idx = ((value - 1).rem_euclid(n)) as usize;
  let repeat = ((value + n - 1) / n) as usize;
  Some(symbols.get(idx)?.repeat(repeat))
}
