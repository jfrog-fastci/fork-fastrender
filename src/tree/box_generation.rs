//! Box generation - transforms styled DOM into BoxTree
//!
//! Implements the CSS box generation algorithm that determines what boxes
//! are created from DOM elements.
//!
//! CSS Specification: CSS 2.1 Section 9.2 - Box Generation
//! <https://www.w3.org/TR/CSS21/visuren.html#box-gen>

use crate::compat::CompatProfile;
use crate::css::types::TranslateValue;
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
use crate::interaction::form_controls;
use crate::interaction::InteractionState;
use crate::render_control::check_active_periodic;
use crate::resource::ReferrerPolicy;
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
use crate::style::media::{MediaContext, MediaQuery, MediaType, Scripting};
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
use crate::svg::parse_svg_length;
use crate::svg::parse_svg_length_px;
use crate::svg::parse_svg_view_box;
use crate::svg::svg_intrinsic_dimensions_from_attributes;
use crate::svg::SvgLength;
use crate::tree::anonymous::inherited_style;
use crate::tree::anonymous::AnonymousBoxCreator;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::tree::box_tree::ForeignObjectInfo;
use crate::tree::box_tree::FormControl;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::ImePreeditPaintState;
use crate::tree::box_tree::GeneratedPseudoElement;
use crate::tree::box_tree::ImageDecodingAttribute;
use crate::tree::box_tree::ImageLoadingAttribute;
use crate::tree::box_tree::IframeSandboxAttribute;
use crate::tree::box_tree::MarkerContent;
use crate::tree::box_tree::MathReplaced;
use crate::tree::box_tree::PictureSource;
use crate::tree::box_tree::ReplacedBox;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::SelectControl;
use crate::tree::box_tree::SelectItem;
use crate::tree::box_tree::SizesList;
use crate::tree::box_tree::SrcsetCandidate;
use crate::tree::box_tree::SrcsetDescriptor;
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
  value.trim_end_matches(|c: char| {
    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
  })
}
fn srcset_from_override_resolution(
  image: &crate::style::types::BackgroundImageUrl,
) -> Vec<SrcsetCandidate> {
  match image
    .override_resolution
    .filter(|d| d.is_finite() && *d > 0.0)
  {
    Some(density) => vec![SrcsetCandidate {
      url: image.url.clone(),
      descriptor: SrcsetDescriptor::Density(density),
    }],
    None => Vec::new(),
  }
}

// ============================================================================
// StyledNode-based Box Generation (for real DOM/style pipeline)
// ============================================================================

use crate::style::cascade::StyledNode;

/// Options that control how the box tree is generated from styled DOM.
#[derive(Debug, Clone, Copy, PartialEq)]
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

  /// Viewport size in CSS pixels used to evaluate media queries in box generation.
  ///
  /// This is currently used for `<video>/<audio>` `<source media="...">` selection.
  ///
  /// When `None`, media conditions are treated as matching so callers that do not provide viewport
  /// information (e.g. unit tests) retain backwards-compatible behavior.
  pub viewport: Option<Size>,

  /// Device pixel ratio used when evaluating media queries.
  pub device_pixel_ratio: f32,

  /// Media type used when evaluating media queries (e.g. screen vs print).
  pub media_type: MediaType,
}

impl Default for BoxGenerationOptions {
  fn default() -> Self {
    Self {
      compat_profile: CompatProfile::Standards,
      enable_footnote_floats: false,
      dom_scripting_enabled: false,
      viewport: None,
      device_pixel_ratio: 1.0,
      media_type: MediaType::Screen,
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

  /// Sets the viewport size used for media query evaluation in box generation.
  pub fn with_viewport(mut self, viewport: Size) -> Self {
    self.viewport = Some(viewport);
    self
  }

  /// Sets the device pixel ratio used for media query evaluation in box generation.
  pub fn with_device_pixel_ratio(mut self, dpr: f32) -> Self {
    self.device_pixel_ratio = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
    self
  }

  /// Sets the media type used for media query evaluation in box generation.
  pub fn with_media_type(mut self, media_type: MediaType) -> Self {
    self.media_type = media_type;
    self
  }

  fn site_compat_hacks_enabled(&self) -> bool {
    self.compat_profile.site_compat_hacks_enabled()
  }

  fn media_context(&self) -> Option<MediaContext> {
    let viewport = self.viewport?;
    let scripting = if self.dom_scripting_enabled {
      Scripting::Enabled
    } else {
      Scripting::None
    };

    let base = if self.media_type == MediaType::Print {
      MediaContext::print(viewport.width, viewport.height)
    } else {
      MediaContext::screen(viewport.width, viewport.height).with_media_type(self.media_type)
    };

    Some(
      base
        .with_device_pixel_ratio(self.device_pixel_ratio)
        .with_scripting(scripting)
        .with_env_overrides(),
    )
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
  interaction_state: Option<&InteractionState>,
  deadline_counter: &mut usize,
) -> Result<BoxNode> {
  // The styled tree's root is the document node, but the document element (<html>) establishes the
  // writing-mode and direction used for layout and fragmentation.
  let document_axes = if matches!(styled.node.node_type, DomNodeType::Document { .. }) {
    styled
      .children
      .iter()
      .find_map(|child| match child.node.node_type {
        DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
          Some((child.styles.writing_mode, child.styles.direction))
        }
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
    interaction_state,
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
  generate_box_tree_with_options_and_interaction_state(styled, options, None)
}

pub(crate) fn generate_box_tree_with_options_and_interaction_state(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
  interaction_state: Option<&InteractionState>,
) -> Result<BoxTree> {
  let mut deadline_counter = 0usize;
  let mut root = build_box_tree_root(styled, options, interaction_state, &mut deadline_counter)?;
  propagate_root_axes_from_root_element(styled, &mut root);
  Ok(BoxTree::new(root))
}

fn root_element_axes(styled: &StyledNode) -> Option<(usize, WritingMode, Direction)> {
  let mut stack: Vec<&StyledNode> = styled.children.iter().rev().collect();
  while let Some(node) = stack.pop() {
    if matches!(node.node.node_type, DomNodeType::Element { .. }) {
      return Some((
        node.node_id,
        node.styles.writing_mode,
        node.styles.direction,
      ));
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

pub(crate) fn generate_box_tree_with_anonymous_fixup_with_options_and_interaction_state(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
  interaction_state: Option<&InteractionState>,
) -> Result<BoxTree> {
  let timings_enabled = runtime::runtime_toggles().truthy("FASTR_RENDER_TIMINGS");
  let mut deadline_counter = 0usize;
  let build_start = timings_enabled.then(Instant::now);
  let root = build_box_tree_root(styled, options, interaction_state, &mut deadline_counter)?;
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

/// Generates a BoxTree from a StyledNode tree, applies anonymous box fixup, and
/// allows customizing generation behavior via options.
pub fn generate_box_tree_with_anonymous_fixup_with_options(
  styled: &StyledNode,
  options: &BoxGenerationOptions,
) -> Result<BoxTree> {
  generate_box_tree_with_anonymous_fixup_with_options_and_interaction_state(styled, options, None)
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
      _ => {
        debug_assert!(false, "memchr2 returned non-matching byte");
        out.push(bytes[idx] as char);
      }
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
      _ => {
        debug_assert!(false, "memchr3 returned non-matching byte");
        out.push(bytes[idx] as char);
      }
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

  fn repair_unterminated_css_blocks(style: &str) -> Option<String> {
    // Inline SVG serialization appends computed presentation properties into the element's `style=""`
    // attribute. If the author style is malformed (e.g. `fill: var(--x;` missing a closing `)`),
    // appending text can accidentally land inside an unterminated block, causing the injected
    // declarations to be ignored by downstream SVG renderers (usvg/resvg).
    //
    // Browsers generally recover from unterminated blocks by treating EOF / a trailing `;` as the end
    // of the block token. Mirror that recovery by closing any still-open (), [], {} blocks and
    // unterminated strings/comments before trailing semicolons/whitespace so we can safely append
    // new declarations.
    let bytes = style.as_bytes();
    if !bytes
      .iter()
      .any(|b| matches!(b, b'(' | b'[' | b'{' | b'"' | b'\'' | b'/' | b'\\'))
    {
      return None;
    }

    let mut stack: Vec<u8> = Vec::new();
    let mut in_comment = false;
    let mut in_string: Option<u8> = None;
    let mut string_escape = false;
    let mut i = 0usize;
    while i < bytes.len() {
      let b = bytes[i];
      if in_comment {
        if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
          in_comment = false;
          i += 2;
          continue;
        }
        i += 1;
        continue;
      }

      if let Some(quote) = in_string {
        if string_escape {
          string_escape = false;
          i += 1;
          continue;
        }
        if b == b'\\' {
          string_escape = true;
          i += 1;
          continue;
        }
        if b == quote {
          in_string = None;
          i += 1;
          continue;
        }
        i += 1;
        continue;
      }

      // Not inside a string/comment.
      if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
        in_comment = true;
        i += 2;
        continue;
      }

      if b == b'"' || b == b'\'' {
        in_string = Some(b);
        i += 1;
        continue;
      }

      if b == b'\\' {
        // Outside strings, backslash escapes the next codepoint. Skip a single byte here; we're only
        // interested in ASCII delimiters.
        i = (i + 2).min(bytes.len());
        continue;
      }

      match b {
        b'(' | b'[' | b'{' => stack.push(b),
        b')' => {
          if stack.last() == Some(&b'(') {
            stack.pop();
          }
        }
        b']' => {
          if stack.last() == Some(&b'[') {
            stack.pop();
          }
        }
        b'}' => {
          if stack.last() == Some(&b'{') {
            stack.pop();
          }
        }
        _ => {}
      }

      i += 1;
    }

    if stack.is_empty() && !in_comment && in_string.is_none() {
      return None;
    }

    let mut end = trim_ascii_whitespace_end(style).len();
    while end > 0 && style.as_bytes()[end - 1] == b';' {
      end -= 1;
    }

    let mut out = String::with_capacity(style.len() + stack.len() + 4);
    out.push_str(&style[..end]);

    if in_comment {
      out.push_str("*/");
    }
    if let Some(quote) = in_string {
      if string_escape {
        // Trailing backslash escapes the next codepoint; insert a spacer so the closing quote isn't
        // swallowed by the escape.
        out.push(' ');
      }
      out.push(quote as char);
    }

    for opener in stack.iter().rev() {
      out.push(match opener {
        b'(' => ')',
        b'[' => ']',
        b'{' => '}',
        _ => continue,
      });
    }
    out.push_str(&style[end..]);
    Some(out)
  }
  if let Some((_, value)) = attrs
    .iter_mut()
    .find(|(name, _)| name.eq_ignore_ascii_case("style"))
  {
    if let Some(repaired) = repair_unterminated_css_blocks(value) {
      *value = repaired;
    }
    if !trim_ascii_whitespace_end(value).ends_with(';') && !trim_ascii_whitespace(value).is_empty()
    {
      value.push(';');
    }
    value.push_str(extra);
  } else {
    attrs.push(("style".to_string(), extra.to_string()));
  }
}

fn svg_inlined_presentation_attr(name: &str) -> bool {
  name.eq_ignore_ascii_case("fill")
    || name.eq_ignore_ascii_case("color")
    || name.eq_ignore_ascii_case("stroke")
    || name.eq_ignore_ascii_case("stroke-width")
    || name.eq_ignore_ascii_case("fill-rule")
    || name.eq_ignore_ascii_case("clip-rule")
    || name.eq_ignore_ascii_case("stroke-linecap")
    || name.eq_ignore_ascii_case("stroke-linejoin")
    || name.eq_ignore_ascii_case("stroke-miterlimit")
    || name.eq_ignore_ascii_case("stroke-dasharray")
    || name.eq_ignore_ascii_case("stroke-dashoffset")
    || name.eq_ignore_ascii_case("fill-opacity")
    || name.eq_ignore_ascii_case("stroke-opacity")
    || name.eq_ignore_ascii_case("stop-color")
    || name.eq_ignore_ascii_case("stop-opacity")
    || name.eq_ignore_ascii_case("marker-start")
    || name.eq_ignore_ascii_case("marker-mid")
    || name.eq_ignore_ascii_case("marker-end")
    || name.eq_ignore_ascii_case("display")
    || name.eq_ignore_ascii_case("visibility")
    || name.eq_ignore_ascii_case("opacity")
    || name.eq_ignore_ascii_case("font-family")
    || name.eq_ignore_ascii_case("font-size")
    || name.eq_ignore_ascii_case("font-weight")
    || name.eq_ignore_ascii_case("font-style")
    || name.eq_ignore_ascii_case("letter-spacing")
    || name.eq_ignore_ascii_case("word-spacing")
    || name.eq_ignore_ascii_case("text-anchor")
    || name.eq_ignore_ascii_case("dominant-baseline")
    || name.eq_ignore_ascii_case("baseline-shift")
    || name.eq_ignore_ascii_case("shape-rendering")
    || name.eq_ignore_ascii_case("vector-effect")
    || name.eq_ignore_ascii_case("color-rendering")
    || name.eq_ignore_ascii_case("color-interpolation")
    || name.eq_ignore_ascii_case("color-interpolation-filters")
    || name.eq_ignore_ascii_case("mask-type")
    || name.eq_ignore_ascii_case("dominant-baseline")
    || name.eq_ignore_ascii_case("baseline-shift")
}

fn attrs_need_svg_inlined_presentation_stripping(attrs: &[(String, String)]) -> bool {
  attrs
    .iter()
    .any(|(name, _)| svg_inlined_presentation_attr(name))
}

fn strip_svg_inlined_presentation_attrs(attrs: &mut Vec<(String, String)>) {
  attrs.retain(|(name, _)| !svg_inlined_presentation_attr(name));
}

fn svg_transform_attribute_name(tag_name: &str) -> &'static str {
  if tag_name.eq_ignore_ascii_case("pattern") {
    "patternTransform"
  } else if tag_name.eq_ignore_ascii_case("linearGradient")
    || tag_name.eq_ignore_ascii_case("radialGradient")
  {
    "gradientTransform"
  } else {
    "transform"
  }
}

fn apply_svg_transform_presentation_attribute_override(
  attrs: &mut Vec<(String, String)>,
  transform_attr_name: &str,
  style: &ComputedStyle,
) {
  // Elements like <pattern>/<linearGradient>/<radialGradient> use dedicated attributes
  // (`patternTransform`/`gradientTransform`) rather than the generic `transform`.
  //
  // If the element expects one of those attributes, strip any accidental `transform=""` so we
  // don't emit invalid SVG markup.
  if transform_attr_name != "transform" {
    attrs.retain(|(name, _)| !name.eq_ignore_ascii_case("transform"));
  }

  let has_transform_attr = attrs
    .iter()
    .any(|(name, _)| name.eq_ignore_ascii_case(transform_attr_name));

  if !style.has_transform() {
    // `transform: none` cancels any SVG transform presentation attribute.
    if has_transform_attr {
      attrs.retain(|(name, _)| !name.eq_ignore_ascii_case(transform_attr_name));
    }
  } else if let Some(transform) = svg_transform_attribute(style) {
    attrs.retain(|(name, _)| !name.eq_ignore_ascii_case(transform_attr_name));
    attrs.push((transform_attr_name.to_string(), transform));
  } else if let Some(extra) = svg_transform_style_declaration(style) {
    // If we can't represent the computed transform as an SVG `*Transform=""` attribute (e.g.
    // percent/calc/viewport-relative/3D transforms), emit it as a CSS declaration. resvg/usvg
    // currently ignores CSS `transform`, but preserving it keeps the serialized SVG faithful
    // for other consumers and lets us avoid dropping any authored SVG `*Transform=""`.
    merge_style_attribute(attrs, &extra);
  }
}

fn svg_transform_attribute(style: &ComputedStyle) -> Option<String> {
  use crate::css::types::{RotateValue, ScaleValue, Transform as CssTransform, TranslateValue};
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
  let push_sep = |out: &mut String| {
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

fn svg_transform_style_declaration(style: &ComputedStyle) -> Option<String> {
  use crate::css::types::{RotateValue, ScaleValue, Transform as CssTransform, TranslateValue};
  use std::fmt::Write as _;

  if !style.has_transform() {
    return None;
  }

  let mut list = String::new();
  let mut push_sep = |out: &mut String| {
    if !out.is_empty() {
      out.push(' ');
    }
  };

  // CSS Transforms Level 2: translate → rotate → scale → transform list.
  if let TranslateValue::Values { x, y, z } = style.translate {
    push_sep(&mut list);
    if z.is_zero() {
      let _ = write!(&mut list, "translate({} {})", x, y);
    } else {
      let _ = write!(&mut list, "translate3d({} {} {})", x, y, z);
    }
  }

  match style.rotate {
    RotateValue::None => {}
    RotateValue::Angle(deg) => {
      push_sep(&mut list);
      let _ = write!(&mut list, "rotate({deg}deg)");
    }
    RotateValue::AxisAngle { x, y, z, angle } => {
      push_sep(&mut list);
      let _ = write!(&mut list, "rotate3d({x} {y} {z} {angle}deg)");
    }
  }

  if let ScaleValue::Values { x, y, z } = style.scale {
    push_sep(&mut list);
    if (z - 1.0).abs() <= 1e-6 {
      let _ = write!(&mut list, "scale({x} {y})");
    } else {
      let _ = write!(&mut list, "scale3d({x} {y} {z})");
    }
  }

  for component in &style.transform {
    match *component {
      CssTransform::Translate(x, y) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "translate({} {})", x, y);
      }
      CssTransform::TranslateX(x) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "translateX({})", x);
      }
      CssTransform::TranslateY(y) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "translateY({})", y);
      }
      CssTransform::TranslateZ(z) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "translateZ({})", z);
      }
      CssTransform::Translate3d(x, y, z) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "translate3d({} {} {})", x, y, z);
      }
      CssTransform::Scale(x, y) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "scale({x} {y})");
      }
      CssTransform::ScaleX(x) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "scaleX({x})");
      }
      CssTransform::ScaleY(y) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "scaleY({y})");
      }
      CssTransform::ScaleZ(z) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "scaleZ({z})");
      }
      CssTransform::Scale3d(x, y, z) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "scale3d({x} {y} {z})");
      }
      CssTransform::Rotate(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "rotate({deg}deg)");
      }
      CssTransform::RotateZ(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "rotateZ({deg}deg)");
      }
      CssTransform::RotateX(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "rotateX({deg}deg)");
      }
      CssTransform::RotateY(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "rotateY({deg}deg)");
      }
      CssTransform::Rotate3d(x, y, z, angle) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "rotate3d({x} {y} {z} {angle}deg)");
      }
      CssTransform::SkewX(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "skewX({deg}deg)");
      }
      CssTransform::SkewY(deg) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "skewY({deg}deg)");
      }
      CssTransform::Skew(ax, ay) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "skew({ax}deg {ay}deg)");
      }
      CssTransform::Perspective(p) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "perspective({})", p);
      }
      CssTransform::Matrix(a, b, c, d, e, f) => {
        push_sep(&mut list);
        let _ = write!(&mut list, "matrix({a} {b} {c} {d} {e} {f})");
      }
      CssTransform::Matrix3d(values) => {
        push_sep(&mut list);
        list.push_str("matrix3d(");
        for (idx, value) in values.iter().enumerate() {
          if idx > 0 {
            list.push(' ');
          }
          let _ = write!(&mut list, "{value}");
        }
        list.push(')');
      }
    }
  }

  if list.is_empty() {
    Some("transform: none".to_string())
  } else {
    Some(format!("transform: {list}"))
  }
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

  let mut push_color_or_none =
    |out: &mut String, value: &ColorOrNone, current_color: Rgba| match value {
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
      ColorOrNone::Url(url) => {
        out.push_str("url(");
        out.push_str(url.as_ref());
        out.push(')');
      }
    };

  let effective_color_or_none = |value: &ColorOrNone, current_color: Rgba| -> ColorOrNone {
    match value {
      ColorOrNone::CurrentColor => ColorOrNone::Color(current_color),
      ColorOrNone::Color(color) => ColorOrNone::Color(*color),
      ColorOrNone::None => ColorOrNone::None,
      ColorOrNone::Url(url) => ColorOrNone::Url(url.clone()),
    }
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

  if let Some(fill) = style.svg_fill.as_ref() {
    let effective = effective_color_or_none(fill, style.color);
    let parent_effective = parent.and_then(|p| {
      p.svg_fill
        .as_ref()
        .map(|value| effective_color_or_none(value, p.color))
    });
    if parent_effective.as_ref() != Some(&effective) {
      start_decl(&mut out, &mut any);
      out.push_str("fill: ");
      push_color_or_none(&mut out, fill, style.color);
    }
  }

  if let Some(stroke) = style.svg_stroke.as_ref() {
    let effective = effective_color_or_none(stroke, style.color);
    let parent_effective = parent.and_then(|p| {
      p.svg_stroke
        .as_ref()
        .map(|value| effective_color_or_none(value, p.color))
    });
    if parent_effective.as_ref() != Some(&effective) {
      start_decl(&mut out, &mut any);
      out.push_str("stroke: ");
      push_color_or_none(&mut out, stroke, style.color);
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

  if let Some(color) = style.svg_stop_color {
    if parent.and_then(|p| p.svg_stop_color) != Some(color) {
      start_decl(&mut out, &mut any);
      out.push_str("stop-color: ");
      let _ = write!(
        &mut out,
        "rgba({},{},{},{:.3})",
        color.r,
        color.g,
        color.b,
        color.a.clamp(0.0, 1.0)
      );
    }
  }

  if let Some(opacity) = style.svg_stop_opacity {
    if parent.and_then(|p| p.svg_stop_opacity) != Some(opacity) {
      start_decl(&mut out, &mut any);
      let _ = write!(&mut out, "stop-opacity: {:.3}", opacity);
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

fn svg_rendering_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  let mut out = String::new();
  let mut any = false;

  let mut start_decl = |out: &mut String, any: &mut bool| {
    if *any {
      out.push_str("; ");
    } else {
      *any = true;
    }
  };

  if let Some(value) = style.svg_shape_rendering {
    if parent.and_then(|p| p.svg_shape_rendering) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("shape-rendering: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_vector_effect {
    if parent.and_then(|p| p.svg_vector_effect) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("vector-effect: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_color_rendering {
    if parent.and_then(|p| p.svg_color_rendering) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("color-rendering: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_color_interpolation {
    if parent.and_then(|p| p.svg_color_interpolation) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("color-interpolation: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_color_interpolation_filters {
    if parent.and_then(|p| p.svg_color_interpolation_filters) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("color-interpolation-filters: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_mask_type {
    if parent.and_then(|p| p.svg_mask_type) != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("mask-type: ");
      out.push_str(value.as_css_str());
    }
  }

  any.then_some(out)
}

fn svg_paint_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  use crate::style::display::Display;
  use crate::style::types::{ClipPath, FilterFunction};
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
      Visibility::Collapse => {
        debug_assert!(false, "collapse is normalized to hidden");
        out.push_str("hidden")
      }
    }
  }

  if style.opacity.is_finite() && style.opacity != 1.0 {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "opacity: {:.3}", style.opacity.clamp(0.0, 1.0));
  }

  if let ClipPath::Url(url, _) = &style.clip_path {
    start_decl(&mut out, &mut any);
    out.push_str("clip-path: url(");
    out.push_str(url);
    out.push(')');
  }

  if let [FilterFunction::Url(url)] = style.filter.as_slice() {
    start_decl(&mut out, &mut any);
    out.push_str("filter: url(");
    out.push_str(url);
    out.push(')');
  }

  any.then_some(out)
}

fn svg_attrs_have_overflow_declaration(attrs: &[(String, String)]) -> bool {
  if attrs
    .iter()
    .any(|(name, _)| name.eq_ignore_ascii_case("overflow"))
  {
    return true;
  }

  let Some((_, style)) = attrs
    .iter()
    .find(|(name, _)| name.eq_ignore_ascii_case("style"))
  else {
    return false;
  };

  let declarations = crate::css::parser::parse_declarations(style);
  declarations.iter().any(|decl| {
    matches!(
      decl.property.as_str(),
      "overflow" | "overflow-x" | "overflow-y"
    )
  })
}

fn svg_overflow_style(
  tag_name: &str,
  style: &ComputedStyle,
  attrs: &[(String, String)],
) -> Option<String> {
  use crate::style::types::Overflow;

  // `overflow` affects only SVG viewport-establishing elements. Most real-world diffs that crop up
  // when document CSS injection is disabled come from nested `<svg>` viewports that rely on
  // `overflow: visible` via CSS classes.
  if !tag_name.eq_ignore_ascii_case("svg") && !tag_name.eq_ignore_ascii_case("foreignObject") {
    return None;
  }

  let overflow_x = style.overflow_x;
  let overflow_y = style.overflow_y;

  // usvg/resvg defaults match browsers: SVG viewports clip their contents by default.
  let default = Overflow::Hidden;
  if overflow_x == default && overflow_y == default && !svg_attrs_have_overflow_declaration(attrs) {
    return None;
  }

  let mut out = String::new();
  out.push_str("overflow: ");
  let keyword = |value: Overflow| match value {
    Overflow::Visible => "visible",
    Overflow::Hidden => "hidden",
    Overflow::Scroll => "scroll",
    Overflow::Auto => "auto",
    Overflow::Clip => "clip",
  };
  out.push_str(keyword(overflow_x));
  if overflow_x != overflow_y {
    out.push(' ');
    out.push_str(keyword(overflow_y));
  }
  Some(out)
}

fn svg_mask_style(style: &ComputedStyle) -> Option<String> {
  use crate::style::types::BackgroundImage;

  // MVP: support a single `url(#id)` mask layer, which is the common pattern for SVG masks
  // applied via CSS classes.
  if style.mask_layers.len() != 1 {
    return None;
  }
  let layer = style.mask_layers.first()?;
  let image = layer.image.as_ref()?;
  let BackgroundImage::Url(src) = image else {
    return None;
  };
  let id = trim_ascii_whitespace(&src.url)
    .strip_prefix('#')
    .filter(|id| !id.is_empty())?;
  Some(format!("mask: url(#{id})"))
}

fn push_css_font_family_list(out: &mut String, families: &[String]) {
  for (idx, family) in families.iter().enumerate() {
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

fn svg_text_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  use crate::style::types::{SvgBaselineShift, SvgTextAnchor};
  use std::fmt::Write as _;

  let parent = parent?;

  let mut out = String::new();
  let mut any = false;

  let mut start_decl = |out: &mut String, any: &mut bool| {
    if *any {
      out.push_str("; ");
    } else {
      *any = true;
    }
  };

  if style.font_family != parent.font_family && !style.font_family.is_empty() {
    start_decl(&mut out, &mut any);
    out.push_str("font-family: ");
    push_css_font_family_list(&mut out, &style.font_family);
  }

  if style.font_size.is_finite() && style.font_size != parent.font_size {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "font-size: {:.2}px", style.font_size);
  }

  if style.font_weight != parent.font_weight {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "font-weight: {}", style.font_weight.to_u16());
  }

  if style.font_style != parent.font_style {
    start_decl(&mut out, &mut any);
    out.push_str("font-style: ");
    match style.font_style {
      FontStyle::Italic => out.push_str("italic"),
      FontStyle::Oblique(Some(angle)) => {
        let _ = write!(&mut out, "oblique {}deg", angle);
      }
      FontStyle::Oblique(None) => out.push_str("oblique"),
      FontStyle::Normal => out.push_str("normal"),
    }
  }

  if style.letter_spacing.is_finite() && style.letter_spacing != parent.letter_spacing {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "letter-spacing: {:.2}px", style.letter_spacing);
  }

  if style.word_spacing.is_finite() && style.word_spacing != parent.word_spacing {
    start_decl(&mut out, &mut any);
    let _ = write!(&mut out, "word-spacing: {:.2}px", style.word_spacing);
  }

  if let Some(anchor) = style.svg_text_anchor {
    if parent.svg_text_anchor != Some(anchor) {
      start_decl(&mut out, &mut any);
      out.push_str("text-anchor: ");
      match anchor {
        SvgTextAnchor::Start => out.push_str("start"),
        SvgTextAnchor::Middle => out.push_str("middle"),
        SvgTextAnchor::End => out.push_str("end"),
      }
    }
  }

  if let Some(value) = style.svg_dominant_baseline {
    if parent.svg_dominant_baseline != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("dominant-baseline: ");
      out.push_str(value.as_css_str());
    }
  }

  if let Some(value) = style.svg_baseline_shift {
    if parent.svg_baseline_shift != Some(value) {
      start_decl(&mut out, &mut any);
      out.push_str("baseline-shift: ");
      match value {
        SvgBaselineShift::Baseline => out.push_str("baseline"),
        SvgBaselineShift::Sub => out.push_str("sub"),
        SvgBaselineShift::Super => out.push_str("super"),
        SvgBaselineShift::Length(len) => out.push_str(&len.to_css()),
      }
    }
  }

  any.then_some(out)
}

fn svg_color_style(style: &ComputedStyle, parent: Option<&ComputedStyle>) -> Option<String> {
  let parent = parent?;
  (style.color != parent.color).then(|| format!("color: {}", format_css_color(style.color)))
}

fn svg_fill_from_root_current_color_injection(
  style: &ComputedStyle,
  parent: Option<&ComputedStyle>,
  root_injection_active: bool,
  is_root: bool,
) -> Option<String> {
  if !root_injection_active || style.svg_fill.is_some() {
    return None;
  }

  if is_root {
    return Some(format!("fill: {}", format_css_color(style.color)));
  }

  let parent = parent?;
  (style.color != parent.color).then(|| format!("fill: {}", format_css_color(style.color)))
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
      if current_ns == SVG_NAMESPACE {
        let transform_attr_name = svg_transform_attribute_name(tag_name);
        apply_svg_transform_presentation_attribute_override(
          &mut attrs,
          transform_attr_name,
          &styled.styles,
        );
        if attrs_need_svg_inlined_presentation_stripping(&attrs) {
          strip_svg_inlined_presentation_attrs(&mut attrs);
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
        if is_root {
          merge_style_attribute(
            &mut attrs,
            &format!("color: {}", format_css_color(styled.styles.color)),
          );
        } else if let Some(extra) = svg_color_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_presentation_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_rendering_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_text_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_paint_style(&styled.styles, parent_svg_styles) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_overflow_style(tag_name, &styled.styles, &attrs) {
          merge_style_attribute(&mut attrs, &extra);
        }
        if let Some(extra) = svg_mask_style(&styled.styles) {
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

/// Collect raw SVG defs referenced across sibling `<svg>` roots (e.g. sprite-sheet `<symbol>`
/// definitions referenced via `<use href="#...">`).
///
/// Unlike [`collect_svg_id_defs`], these fragments are serialized without inlining computed SVG
/// presentation properties. This is important for constructs such as `fill="currentColor"` /
/// `stroke="currentColor"` inside sprite sheets, which must inherit `color` from the referencing
/// SVG root rather than being frozen to the sprite sheet's computed `color`.
///
/// Namespace declarations from ancestor elements are preserved to keep prefixed attributes valid.
pub fn collect_svg_id_defs_raw(styled: &StyledNode) -> HashMap<String, String> {
  use crate::style::types::{ColorOrNone, SvgUrlOrNone};

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

  fn collect_local_fragment_ref(raw: &str, refs: &mut HashSet<String>) {
    let trimmed = trim_ascii_whitespace(raw);
    if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
      refs.insert(id.to_string());
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

  fn collect_svg_ids_and_refs_within_root(
    styled: &StyledNode,
    in_svg_style: bool,
    ids: &mut HashSet<String>,
    refs: &mut HashSet<String>,
  ) {
    match &styled.node.node_type {
      crate::dom::DomNodeType::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        let is_svg = namespace == SVG_NAMESPACE;
        if is_svg {
          if let Some(id) = styled
            .node
            .get_attribute_ref("id")
            .filter(|id| !id.is_empty())
          {
            ids.insert(id.to_string());
          }

          if let Some(ColorOrNone::Url(url)) = styled.styles.svg_fill.as_ref() {
            collect_local_fragment_ref(url.as_ref(), refs);
          }
          if let Some(ColorOrNone::Url(url)) = styled.styles.svg_stroke.as_ref() {
            collect_local_fragment_ref(url.as_ref(), refs);
          }
          if let Some(SvgUrlOrNone::Url(url)) = styled.styles.svg_marker_start.as_ref() {
            collect_local_fragment_ref(url.as_ref(), refs);
          }
          if let Some(SvgUrlOrNone::Url(url)) = styled.styles.svg_marker_mid.as_ref() {
            collect_local_fragment_ref(url.as_ref(), refs);
          }
          if let Some(SvgUrlOrNone::Url(url)) = styled.styles.svg_marker_end.as_ref() {
            collect_local_fragment_ref(url.as_ref(), refs);
          }

          let is_style = tag_name.eq_ignore_ascii_case("style");
          let next_in_svg_style = in_svg_style || is_style;

          for (name, value) in attributes {
            if is_href_attr(name) {
              let trimmed = trim_ascii_whitespace(value);
              if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
                refs.insert(id.to_string());
              }
            }
            extract_url_fragment_ids(value, refs);
          }

          for child in &styled.children {
            collect_svg_ids_and_refs_within_root(child, next_in_svg_style, ids, refs);
          }
        } else {
          for child in &styled.children {
            collect_svg_ids_and_refs_within_root(child, in_svg_style, ids, refs);
          }
        }
      }
      crate::dom::DomNodeType::Text { content } => {
        if in_svg_style {
          extract_url_fragment_ids(content, refs);
        }
      }
      _ => {
        for child in &styled.children {
          collect_svg_ids_and_refs_within_root(child, in_svg_style, ids, refs);
        }
      }
    }
  }

  fn collect_requested_cross_root_ids(
    styled: &StyledNode,
    parent_ns: Option<&str>,
    out: &mut HashSet<String>,
  ) {
    if let crate::dom::DomNodeType::Element {
      tag_name,
      namespace,
      ..
    } = &styled.node.node_type
    {
      let is_svg_root = namespace == SVG_NAMESPACE
        && tag_name.eq_ignore_ascii_case("svg")
        && parent_ns != Some(SVG_NAMESPACE);
      if is_svg_root {
        let mut ids = HashSet::new();
        let mut refs = HashSet::new();
        collect_svg_ids_and_refs_within_root(styled, false, &mut ids, &mut refs);
        for reference in refs {
          if !ids.contains(&reference) {
            out.insert(reference);
          }
        }
      }
      let next_parent = Some(namespace.as_str());
      for child in &styled.children {
        collect_requested_cross_root_ids(child, next_parent, out);
      }
    } else {
      for child in &styled.children {
        collect_requested_cross_root_ids(child, parent_ns, out);
      }
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
        if let Some(id) = styled
          .node
          .get_attribute_ref("id")
          .filter(|id| !id.is_empty())
        {
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

  fn collect_referenced_svg_ids(
    styled: &StyledNode,
    in_svg_style: bool,
    out: &mut HashSet<String>,
  ) {
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
  collect_requested_cross_root_ids(styled, None, &mut requested);
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
    serialize_node_with_namespaces(entry.node, &entry.namespaces, &mut serialized);
    defs.insert(id, serialized);
  }

  defs
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
    let owned_namespaces =
      if let crate::dom::DomNodeType::Element { attributes, .. } = &styled.node.node_type {
        attributes
          .iter()
          .any(|(name, _)| name.starts_with("xmlns"))
          .then(|| {
            let mut updated = inherited_xmlns.to_vec();
            for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
              if let Some(existing) = updated
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
              {
                existing.1 = value.clone();
              } else {
                updated.push((name.clone(), value.clone()));
              }
            }
            updated
          })
      } else {
        None
      };
    let namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);
    if let crate::dom::DomNodeType::Element { tag_name, .. } = &styled.node.node_type {
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
    let owned_namespaces =
      if let crate::dom::DomNodeType::Element { attributes, .. } = &styled.node.node_type {
        attributes
          .iter()
          .any(|(name, _)| name.starts_with("xmlns"))
          .then(|| {
            let mut updated = inherited_xmlns.to_vec();
            for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
              if let Some(existing) = updated
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
              {
                existing.1 = value.clone();
              } else {
                updated.push((name.clone(), value.clone()));
              }
            }
            updated
          })
      } else {
        None
      };
    let namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);
    if let crate::dom::DomNodeType::Element { tag_name, .. } = &styled.node.node_type {
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

/// Collect serialized SVG id definitions required by fragment-only CSS masks, clip paths, and
/// inline SVG cross-root references.
///
/// This powers `mask-image: url(#id)` and `clip-path: url(#id)` by serializing the referenced SVG
/// element (and any other defs it references via `href="#..."`, `url(#...)`, etc.).
///
/// It also supports SVG sprite-sheet patterns where a rendered inline `<svg>` references an `id`
/// defined in a different `<svg>` element in the same HTML document (e.g. `<use href="#icon">`,
/// `fill="url(#grad)"`). FastRender rasterizes each `<svg>` as an isolated SVG document, so we
/// collect referenced ids at document scope and later inject the required fragments before
/// rasterizing.
///
/// We inline computed SVG presentation properties (fill/stroke/opacity/etc.) during serialization
/// so downstream rasterizers (resvg) do not need access to the full document CSS cascade.
///
/// Namespace declarations from ancestor elements are preserved to keep prefixed attributes valid.
pub fn collect_svg_id_defs(styled: &StyledNode) -> HashMap<String, String> {
  use crate::style::types::{BackgroundImage, ClipPath, FilterFunction};

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

  fn collect_requested_svg_id_defs(styled: &StyledNode, out: &mut HashSet<String>) {
    // Inline SVG elements (most commonly `<use href="#id">`) can reference document-global SVG
    // sprites (e.g. a hidden `<svg><symbol id="...">` map). When we serialize each inline `<svg>`
    // element into an isolated document for resvg, those fragment-only references would otherwise
    // become unresolvable. Collect the referenced IDs so we can serialize their definitions into
    // `svg_id_defs` for later injection during paint.
    if let crate::dom::DomNodeType::Element {
      namespace,
      attributes,
      ..
    } = &styled.node.node_type
    {
      if namespace == SVG_NAMESPACE {
        for (name, value) in attributes {
          if is_href_attr(name) {
            let trimmed = trim_ascii_whitespace(value);
            if let Some(id) = trimmed.strip_prefix('#').filter(|id| !id.is_empty()) {
              out.insert(id.to_string());
            }
          }
        }
      }
    }
    for layer in styled.styles.mask_layers.iter() {
      let Some(image) = layer.image.as_ref() else {
        continue;
      };
      let BackgroundImage::Url(src) = image else {
        continue;
      };
      if let Some(id) = trim_ascii_whitespace(&src.url)
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
      {
        out.insert(id.to_string());
      }
    }
    if let ClipPath::Url(src, _) = &styled.styles.clip_path {
      if let Some(id) = trim_ascii_whitespace(src)
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
      {
        out.insert(id.to_string());
      }
    }
    for func in styled.styles.filter.iter() {
      if let FilterFunction::Url(src) = func {
        if let Some(id) = trim_ascii_whitespace(src)
          .strip_prefix('#')
          .filter(|id| !id.is_empty())
        {
          out.insert(id.to_string());
        }
      }
    }
    for child in &styled.children {
      collect_requested_svg_id_defs(child, out);
    }
  }

  fn collect_defined_svg_ids(styled: &StyledNode, out: &mut HashSet<String>) {
    if let crate::dom::DomNodeType::Element { namespace, .. } = &styled.node.node_type {
      if namespace == SVG_NAMESPACE {
        if let Some(id) = styled
          .node
          .get_attribute_ref("id")
          .filter(|id| !id.is_empty())
        {
          out.insert(id.to_string());
        }
      }
    }
    for child in &styled.children {
      collect_defined_svg_ids(child, out);
    }
  }

  fn collect_requested_svg_ids_from_replaced_inline_svgs(
    styled: &StyledNode,
    out: &mut HashSet<String>,
  ) {
    // Inline `<svg>` elements are treated as replaced content by box generation (except when
    // `display: contents`). Since FastRender rasterizes each `<svg>` subtree as an isolated SVG
    // document, same-document fragment references that point outside the subtree need to be
    // resolved via a document-level registry and injected at paint time.
    let mut stack: Vec<&StyledNode> = vec![styled];
    while let Some(node) = stack.pop() {
      if node.styles.display == Display::None {
        continue;
      }
      if let crate::dom::DomNodeType::Element { tag_name, .. } = &node.node.node_type {
        if tag_name.eq_ignore_ascii_case("svg") && node.styles.display != Display::Contents {
          let mut referenced = HashSet::new();
          collect_referenced_svg_ids(node, false, &mut referenced);
          if !referenced.is_empty() {
            let mut defined = HashSet::new();
            collect_defined_svg_ids(node, &mut defined);
            for id in referenced {
              if !defined.contains(&id) {
                out.insert(id);
              }
            }
          }
          // Do not descend into children of a replaced `<svg>`; its subtree will be rasterized as
          // a single image.
          continue;
        }
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
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
    let owned_namespaces =
      if let crate::dom::DomNodeType::Element { attributes, .. } = &styled.node.node_type {
        attributes
          .iter()
          .any(|(name, _)| name.starts_with("xmlns"))
          .then(|| {
            let mut updated = inherited_xmlns.to_vec();
            for (name, value) in attributes.iter().filter(|(n, _)| n.starts_with("xmlns")) {
              if let Some(existing) = updated
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
              {
                existing.1 = value.clone();
              } else {
                updated.push((name.clone(), value.clone()));
              }
            }
            updated
          })
      } else {
        None
      };
    let namespaces = owned_namespaces.as_deref().unwrap_or(inherited_xmlns);

    if let crate::dom::DomNodeType::Element { namespace, .. } = &styled.node.node_type {
      if namespace == SVG_NAMESPACE {
        if let Some(id) = styled
          .node
          .get_attribute_ref("id")
          .filter(|id| !id.is_empty())
        {
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

  fn collect_referenced_svg_ids(
    styled: &StyledNode,
    in_svg_style: bool,
    out: &mut HashSet<String>,
  ) {
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
  collect_requested_svg_id_defs(styled, &mut requested);
  collect_requested_svg_ids_from_replaced_inline_svgs(styled, &mut requested);
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
    let is_symbol = matches!(
      &entry.node.node.node_type,
      crate::dom::DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("symbol")
    );
    if is_symbol {
      serialize_node_with_namespaces(entry.node, &entry.namespaces, &mut serialized);
    } else {
      serialize_svg_mask_subtree_with_namespaces(
        entry.node,
        &entry.namespaces,
        None,
        None,
        true,
        &mut serialized,
      );
    }
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
    use crate::style::types::{SvgBaselineShift, SvgTextAnchor};
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
      push_css_font_family_list(&mut out, &style.font_family);
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

    if style.letter_spacing.is_finite() && style.letter_spacing != 0.0 {
      let _ = write!(&mut out, "; letter-spacing: {:.2}px", style.letter_spacing);
    }
    if style.word_spacing.is_finite() && style.word_spacing != 0.0 {
      let _ = write!(&mut out, "; word-spacing: {:.2}px", style.word_spacing);
    }
    if let Some(anchor) = style.svg_text_anchor {
      out.push_str("; text-anchor: ");
      match anchor {
        SvgTextAnchor::Start => out.push_str("start"),
        SvgTextAnchor::Middle => out.push_str("middle"),
        SvgTextAnchor::End => out.push_str("end"),
      }
    }

    if let Some(value) = style.svg_dominant_baseline {
      out.push_str("; dominant-baseline: ");
      out.push_str(value.as_css_str());
    }

    if let Some(value) = style.svg_baseline_shift {
      out.push_str("; baseline-shift: ");
      match value {
        SvgBaselineShift::Baseline => out.push_str("baseline"),
        SvgBaselineShift::Sub => out.push_str("sub"),
        SvgBaselineShift::Super => out.push_str("super"),
        SvgBaselineShift::Length(len) => out.push_str(&len.to_css()),
      }
    }

    if style.opacity.is_finite() && style.opacity != 1.0 {
      out.push_str("; opacity: 1 !important");
    }

    if style.has_transform() {
      // The root inline SVG is rasterized as a replaced element, so the outer renderer already
      // applies its computed transform as a CSS box transform. Ensure resvg/usvg does not apply it
      // again when rendering the serialized SVG markup.
      out.push_str("; transform: none !important");
      out.push_str("; translate: none !important");
      out.push_str("; rotate: none !important");
      out.push_str("; scale: none !important");
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
    !declarations
      .iter()
      .any(|decl| decl.property.as_str() == "fill")
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
      crate::string_match::contains_ascii_case_insensitive(name, "xlink:")
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

  fn resolve_svg_attribute_var_calls(
    attrs: &mut Vec<(String, String)>,
    custom_properties: &crate::style::custom_property_store::CustomPropertyStore,
  ) {
    use crate::css::types::PropertyValue;
    use crate::style::var_resolution::{resolve_var_for_property, VarResolutionResult};

    fn attr_might_contain_css_value(name: &str) -> bool {
      let local = name
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(name);
      // Only scan attributes that are parsed as CSS values by SVG/CSS presentation attributes.
      // Many SVG elements contain large `d=` path data, and running var() detection over those
      // strings is needlessly expensive.
      local.eq_ignore_ascii_case("style")
        || local.eq_ignore_ascii_case("fill")
        || local.eq_ignore_ascii_case("stroke")
        || local.eq_ignore_ascii_case("color")
        || local.eq_ignore_ascii_case("stop-color")
        || local.eq_ignore_ascii_case("stop-opacity")
        || local.eq_ignore_ascii_case("opacity")
        || local.eq_ignore_ascii_case("clip-path")
        || local.eq_ignore_ascii_case("mask")
        || local.eq_ignore_ascii_case("filter")
        || local.eq_ignore_ascii_case("marker-start")
        || local.eq_ignore_ascii_case("marker-mid")
        || local.eq_ignore_ascii_case("marker-end")
        || local.eq_ignore_ascii_case("x")
        || local.eq_ignore_ascii_case("y")
        || local.eq_ignore_ascii_case("width")
        || local.eq_ignore_ascii_case("height")
    }

    let mut idx = 0usize;
    while idx < attrs.len() {
      let (name, value) = &attrs[idx];
      if !attr_might_contain_css_value(name) || !crate::style::var_resolution::contains_var(value) {
        idx += 1;
        continue;
      }

      // Avoid rewriting URL-bearing attributes; `var(` in href values is almost certainly literal
      // text and should not trigger CSS variable substitution.
      if name.eq_ignore_ascii_case("href")
        || name
          .rsplit_once(':')
          .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
      {
        idx += 1;
        continue;
      }

      let is_style_attr = name.eq_ignore_ascii_case("style");
      let raw = PropertyValue::Keyword(value.clone());
      match resolve_var_for_property(&raw, custom_properties, "") {
        VarResolutionResult::Resolved { css_text, .. } => {
          attrs[idx].1 = css_text.into_owned();
          idx += 1;
        }
        // If the var() call cannot be resolved, treat the attribute as invalid and drop it.
        // Leaving an unresolved `var()` token around leads to resvg/usvg treating SVG paint
        // attributes as `none`, which can make the entire graphic disappear.
        //
        // `style=""` attributes can contain many declarations; dropping them entirely would lose
        // unrelated values, so keep the original string in that case.
        VarResolutionResult::NotFound(_) | VarResolutionResult::RecursionLimitExceeded => {
          if is_style_attr {
            idx += 1;
          } else {
            attrs.remove(idx);
          }
        }
        VarResolutionResult::InvalidSyntax(_) => {
          if is_style_attr {
            // Style attributes can contain many declarations; we normally keep them when we can't
            // resolve `var()`. However, syntactically-invalid substitution functions (e.g. an
            // unterminated `var(`) can prevent resvg/usvg from parsing *subsequently merged*
            // declarations (like the computed `fill`/`stroke` we inline below) once we strip SVG
            // presentation attributes.
            //
            // Clear the broken style attribute so later `merge_style_attribute` calls produce a
            // syntactically-valid declaration list.
            attrs[idx].1.clear();
            idx += 1;
          } else {
            attrs.remove(idx);
          }
        }
      }
    }
  }

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
    root_fill_current_color_injection: bool,
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
            root_fill_current_color_injection,
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
            root_fill_current_color_injection,
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

        let root_fill_current_color_injection = if is_root && current_ns == SVG_NAMESPACE {
          // Only apply the SVG root `fill: currentColor` hack when the author did not specify any
          // fill paint server via CSS/attributes. (SVG defaults to black.)
          styled.styles.svg_fill.is_none() && root_style_includes_fill_current_color(attributes)
        } else {
          root_fill_current_color_injection
        };

        let mut owned_attrs: Option<Vec<(String, String)>> = None;
        if is_root {
          let mut attrs = attributes.clone();
          if current_ns == SVG_NAMESPACE {
            // If the authored SVG root has a `transform` attribute, it participates in the CSS
            // cascade as a presentation hint. The replaced-element renderer will apply it as a CSS
            // box transform, so remove it from the serialized SVG markup to avoid double-applying it
            // internally.
            attrs.retain(|(name, _)| !name.eq_ignore_ascii_case("transform"));
            if attrs_need_svg_inlined_presentation_stripping(&attrs) {
              strip_svg_inlined_presentation_attrs(&mut attrs);
            }

            // Inline `<svg>` elements in HTML commonly rely on CSS `width`/`height` instead of SVG
            // attributes. When we rasterize the serialized SVG out-of-context (via `usvg`/`resvg`),
            // missing root dimensions can change how `<use>` instantiates `<symbol>` because
            // percentage sizing and viewBox mapping depend on the root viewport size.
            //
            // To keep rasterization consistent with the HTML box size, synthesize missing root
            // `width`/`height` attributes from computed CSS lengths when we can resolve them to
            // absolute pixels without needing layout (i.e. avoid percentages/container-query units).
            //
            // Note that for `box-sizing: border-box` the CSS width/height correspond to the border
            // box, whereas the SVG viewport is established by the content box. Account for that by
            // subtracting padding + border widths when possible so that the serialized SVG matches
            // the size it will be painted into.
            if tag_name.eq_ignore_ascii_case("svg") {
              let resolve_css_length_px = |len: Length, style: &ComputedStyle| -> Option<f32> {
                if !len.value.is_finite() {
                  return None;
                }
                if let Some(calc) = len.calc {
                  if calc.has_percentage()
                    || calc.has_viewport_relative()
                    || calc.has_container_query_relative()
                  {
                    return None;
                  }
                } else if len.unit.is_percentage()
                  || len.unit.is_viewport_relative()
                  || len.unit.is_container_query_relative()
                  || !(len.unit.is_absolute() || len.unit.is_font_relative())
                {
                  return None;
                }

                let px = len.resolve_with_context(
                  None,
                  0.0,
                  0.0,
                  style.font_size,
                  style.root_font_size,
                )?;
                px.is_finite().then_some(px)
              };

              let resolve_css_dimension_px =
                |len: Length, style: &ComputedStyle, is_width: bool| -> Option<String> {
                  let mut px = resolve_css_length_px(len, style)?;
                  if style.box_sizing == crate::style::types::BoxSizing::BorderBox {
                    let edge = |len: Length| resolve_css_length_px(len, style);
                    let edges = if is_width {
                      match (
                        edge(style.padding_left),
                        edge(style.padding_right),
                        edge(style.border_left_width),
                        edge(style.border_right_width),
                      ) {
                        (Some(p0), Some(p1), Some(b0), Some(b1)) => Some(p0 + p1 + b0 + b1),
                        _ => None,
                      }
                    } else {
                      match (
                        edge(style.padding_top),
                        edge(style.padding_bottom),
                        edge(style.border_top_width),
                        edge(style.border_bottom_width),
                      ) {
                        (Some(p0), Some(p1), Some(b0), Some(b1)) => Some(p0 + p1 + b0 + b1),
                        _ => None,
                      }
                    };
                    if let Some(edges) = edges {
                      px = (px - edges).max(0.0);
                    }
                  }

                  (px > 0.0).then_some(px.to_string())
                };

              if !attrs
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("width"))
              {
                if let Some(width) = styled
                  .styles
                  .width
                  .and_then(|len| resolve_css_dimension_px(len, &styled.styles, true))
                {
                  attrs.push(("width".to_string(), width));
                }
              }
              if !attrs
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("height"))
              {
                if let Some(height) = styled
                  .styles
                  .height
                  .and_then(|len| resolve_css_dimension_px(len, &styled.styles, false))
                {
                  attrs.push(("height".to_string(), height));
                }
              }
            }
          }
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
          if let Some(extra) = svg_fill_from_root_current_color_injection(
            &styled.styles,
            parent_svg_styles,
            root_fill_current_color_injection,
            true,
          ) {
            merge_style_attribute(&mut attrs, &extra);
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

        if !is_root
          && current_ns == SVG_NAMESPACE
          && attrs_need_svg_inlined_presentation_stripping(attributes)
        {
          let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
          strip_svg_inlined_presentation_attrs(attrs_mut);
        }

        if current_ns == SVG_NAMESPACE {
          // Resolve `var()` inside SVG attributes early, before we start merging computed style
          // declarations into `style=""`. This ensures that if we need to drop/clear a broken
          // `style=""` attribute we don't discard those merged declarations.
          if owned_attrs
            .as_deref()
            .unwrap_or(attributes)
            .iter()
            .any(|(name, value)| {
              let local = name
                .rsplit_once(':')
                .map(|(_, local)| local)
                .unwrap_or(name);
              (local.eq_ignore_ascii_case("style")
                || local.eq_ignore_ascii_case("fill")
                || local.eq_ignore_ascii_case("stroke")
                || local.eq_ignore_ascii_case("color")
                || local.eq_ignore_ascii_case("stop-color")
                || local.eq_ignore_ascii_case("stop-opacity")
                || local.eq_ignore_ascii_case("opacity")
                || local.eq_ignore_ascii_case("clip-path")
                || local.eq_ignore_ascii_case("mask")
                || local.eq_ignore_ascii_case("filter")
                || local.eq_ignore_ascii_case("marker-start")
                || local.eq_ignore_ascii_case("marker-mid")
                || local.eq_ignore_ascii_case("marker-end")
                || local.eq_ignore_ascii_case("x")
                || local.eq_ignore_ascii_case("y")
                || local.eq_ignore_ascii_case("width")
                || local.eq_ignore_ascii_case("height"))
                && crate::style::var_resolution::contains_var(value)
            })
          {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            resolve_svg_attribute_var_calls(attrs_mut, &styled.styles.custom_properties);
          }

          if let Some(extra) = svg_fill_from_root_current_color_injection(
            &styled.styles,
            parent_svg_styles,
            root_fill_current_color_injection,
            false,
          ) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if let Some(extra) = svg_color_style(&styled.styles, parent_svg_styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if let Some(extra) = svg_presentation_style(&styled.styles, parent_svg_styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if let Some(extra) = svg_rendering_style(&styled.styles, parent_svg_styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if let Some(extra) = svg_text_style(&styled.styles, parent_svg_styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if !is_root {
            if let Some(extra) = svg_paint_style(&styled.styles, parent_svg_styles) {
              let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
              merge_style_attribute(attrs_mut, &extra);
            }
            let transform_attr_name = svg_transform_attribute_name(tag_name);
            let attrs_src = owned_attrs.as_deref().unwrap_or(attributes);

            let has_transform_attr = attrs_src
              .iter()
              .any(|(name, _)| name.eq_ignore_ascii_case(transform_attr_name));
            let has_invalid_transform_attr = transform_attr_name != "transform"
              && attrs_src
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("transform"));

            if has_transform_attr || has_invalid_transform_attr || styled.styles.has_transform() {
              let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
              apply_svg_transform_presentation_attribute_override(
                attrs_mut,
                transform_attr_name,
                &styled.styles,
              );
            }
          }
          if let Some(extra) = svg_overflow_style(
            tag_name,
            &styled.styles,
            owned_attrs.as_deref().unwrap_or(attributes),
          ) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
          }
          if let Some(extra) = svg_mask_style(&styled.styles) {
            let attrs_mut = owned_attrs.get_or_insert_with(|| attributes.clone());
            merge_style_attribute(attrs_mut, &extra);
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
            root_fill_current_color_injection,
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
    false,
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
  interaction_state: Option<&InteractionState>,
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
    form_control: Option<Arc<FormControl>>,
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
        form_control: None,
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
    display.blockify()
  }

  fn blockify_display_for_out_of_flow_position(display: Display) -> Display {
    // CSS 2.1 §9.7 / CSS Display Level 3: floats and absolutely positioned elements are
    // blockified.
    //
    // Table-internal display types cannot participate in table layout once taken out-of-flow, so
    // they fall back to `block`.
    //
    // https://www.w3.org/TR/CSS21/visuren.html#dis-pos-flo
    // https://www.w3.org/TR/css-display-3/#transformations
    if display.is_table_internal() {
      Display::Block
    } else {
      display.blockify()
    }
  }

  fn blockify_display_for_float(display: Display) -> Display {
    // CSS 2.1 §9.7: the used display type is affected by `float`.
    //
    // Floats are blockified. Additionally, table-internal display types become `block` since they
    // cannot participate in table layout once taken out of flow.
    //
    // https://www.w3.org/TR/CSS21/visuren.html#dis-pos-flo
    blockify_display_for_out_of_flow_position(display)
  }

  fn transform_style_for_box_generation_if_needed<'a>(
    style: &Arc<ComputedStyle>,
    stack: &[Frame<'a>],
  ) -> Arc<ComputedStyle> {
    let mut display = style.display;

    // CSS Display Level 3: flex/grid items are blockified.
    //
    // Absolutely positioned children of flex/grid containers are out-of-flow and do not become
    // flex/grid items, so this blockification step does not apply to them.
    if !matches!(style.position, Position::Absolute | Position::Fixed) {
      let container_display = nearest_non_contents_container_display(stack);
      if matches!(
        container_display,
        Some(Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid)
      ) {
        display = blockify_flex_or_grid_item_display(display);
      }
    }

    // CSS 2.1 §9.7 / CSS Display Level 3: absolutely positioned and fixed elements are blockified.
    if matches!(style.position, Position::Absolute | Position::Fixed) {
      display = blockify_display_for_out_of_flow_position(display);
    }

    // CSS 2.1 §9.7: floats are blockified for box generation.
    //
    // This is critical for cases like `div { float:left; display:inline }`, where the used display
    // type is block-level; otherwise the box tree fixups treat block descendants as "block-in-inline"
    // and incorrectly split/hoist them out of the floated box.
    if style.float.is_floating() {
      display = blockify_display_for_float(display);
    }

    if display == style.display {
      return Arc::clone(style);
    }

    let mut owned = (**style).clone();
    owned.display = display;
    Arc::new(owned)
  }

  fn strip_ua_form_control_edge_styles_for_appearance_none(
    styled: &StyledNode,
    mut style: Arc<ComputedStyle>,
    form_control: Option<&Arc<FormControl>>,
  ) -> Arc<ComputedStyle> {
    use crate::style::cascade_order_origin;
    use crate::style::CascadeOrderOrigin;
    use crate::style::types::Appearance;
    use crate::style::types::BorderStyle;
    use crate::style::types::BoxSizing;

    if !matches!(style.appearance, Appearance::None) {
      return style;
    }

    let is_form_control_element = form_control.is_some()
      || match &styled.node.node_type {
        DomNodeType::Element {
          tag_name,
          namespace,
          ..
        } => (namespace.is_empty() || namespace == HTML_NAMESPACE)
          && (tag_name.eq_ignore_ascii_case("input")
            || tag_name.eq_ignore_ascii_case("textarea")
            || tag_name.eq_ignore_ascii_case("select")
            || tag_name.eq_ignore_ascii_case("button")),
        _ => false,
      };

    if !is_form_control_element {
      return style;
    }

    let ua = Some(CascadeOrderOrigin::UserAgent);

    // When `appearance: none` disables native replaced control rendering, browsers treat the form
    // control as a normal element and do not apply UA default border/padding/background-color. We
    // mimic Chromium by stripping these values *only* when they are currently winning from the UA
    // origin (i.e. do not clobber author styles).

    // Box sizing.
    //
    // Chromium's UA stylesheet sets `box-sizing: border-box` for native text controls, but when
    // `appearance: none` is specified (so the element is treated as a normal box), `width`/`height`
    // fall back to the standard `content-box` sizing model unless authors explicitly set
    // `box-sizing`.
    if cascade_order_origin(style.logical.box_sizing_order) == ua
      && matches!(style.box_sizing, BoxSizing::BorderBox)
    {
      let s = Arc::make_mut(&mut style);
      s.box_sizing = BoxSizing::ContentBox;
      s.logical.box_sizing_order = -1;
    }

    // Padding.
    if cascade_order_origin(style.logical.padding_orders.top) == ua {
      let s = Arc::make_mut(&mut style);
      s.padding_top = Length::px(0.0);
      s.logical.padding_orders.top = -1;
    }
    if cascade_order_origin(style.logical.padding_orders.right) == ua {
      let s = Arc::make_mut(&mut style);
      s.padding_right = Length::px(0.0);
      s.logical.padding_orders.right = -1;
    }
    if cascade_order_origin(style.logical.padding_orders.bottom) == ua {
      let s = Arc::make_mut(&mut style);
      s.padding_bottom = Length::px(0.0);
      s.logical.padding_orders.bottom = -1;
    }
    if cascade_order_origin(style.logical.padding_orders.left) == ua {
      let s = Arc::make_mut(&mut style);
      s.padding_left = Length::px(0.0);
      s.logical.padding_orders.left = -1;
    }

    // Border widths.
    if cascade_order_origin(style.logical.border_width_orders.top) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_top_width = Length::px(0.0);
      s.logical.border_width_orders.top = -1;
    }
    if cascade_order_origin(style.logical.border_width_orders.right) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_right_width = Length::px(0.0);
      s.logical.border_width_orders.right = -1;
    }
    if cascade_order_origin(style.logical.border_width_orders.bottom) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_bottom_width = Length::px(0.0);
      s.logical.border_width_orders.bottom = -1;
    }
    if cascade_order_origin(style.logical.border_width_orders.left) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_left_width = Length::px(0.0);
      s.logical.border_width_orders.left = -1;
    }

    // Border styles.
    if cascade_order_origin(style.logical.border_style_orders.top) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_top_style = BorderStyle::None;
      s.logical.border_style_orders.top = -1;
    }
    if cascade_order_origin(style.logical.border_style_orders.right) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_right_style = BorderStyle::None;
      s.logical.border_style_orders.right = -1;
    }
    if cascade_order_origin(style.logical.border_style_orders.bottom) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_bottom_style = BorderStyle::None;
      s.logical.border_style_orders.bottom = -1;
    }
    if cascade_order_origin(style.logical.border_style_orders.left) == ua {
      let s = Arc::make_mut(&mut style);
      s.border_left_style = BorderStyle::None;
      s.logical.border_style_orders.left = -1;
    }

    // Background color.
    if cascade_order_origin(style.logical.background_color_order) == ua {
      let s = Arc::make_mut(&mut style);
      s.background_color = Rgba::TRANSPARENT;
      s.background_color_is_system = false;
      s.logical.background_color_order = -1;
    }

    style
  }

  fn html_represents_nothing_element(tag: &str) -> bool {
    // HTML defines a set of elements that "represent nothing" and must never generate boxes,
    // regardless of any authored `display` overrides (unlike `[hidden]`, which is overrideable).
    //
    // This list mirrors the subset of HTML elements that FastRender can encounter in real pages
    // and that browsers suppress unconditionally during box generation.
    tag.eq_ignore_ascii_case("head")
      || tag.eq_ignore_ascii_case("style")
      || tag.eq_ignore_ascii_case("script")
      || tag.eq_ignore_ascii_case("meta")
      || tag.eq_ignore_ascii_case("link")
      || tag.eq_ignore_ascii_case("title")
      || tag.eq_ignore_ascii_case("base")
      || tag.eq_ignore_ascii_case("basefont")
      || tag.eq_ignore_ascii_case("datalist")
      || tag.eq_ignore_ascii_case("noembed")
      || tag.eq_ignore_ascii_case("noframes")
      || tag.eq_ignore_ascii_case("param")
      || tag.eq_ignore_ascii_case("area")
      || tag.eq_ignore_ascii_case("map")
      || tag.eq_ignore_ascii_case("source")
      || tag.eq_ignore_ascii_case("track")
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
              // In HTML documents, inter-element whitespace between `<head>` and `<body>` is parsed as
              // a text node that is a direct child of `<html>`. Browsers do not render this text, and
              // producing boxes for it breaks assumptions in later stages (e.g. identifying the body
              // box for canvas background propagation) and can affect the static position of
              // absolutely positioned `<body>`.
              if stack.iter().rev().nth(1).is_some_and(|parent| {
                matches!(
                  &parent.styled.node.node_type,
                  DomNodeType::Element {
                    tag_name,
                    namespace,
                    ..
                  } if tag_name.eq_ignore_ascii_case("html")
                    && (namespace.is_empty() || namespace == HTML_NAMESPACE)
                )
              }) {
                stack.pop();
                continue;
              }

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

        // HTML "represents nothing" elements must never generate boxes, even if author CSS tries
        // to force them visible with `display:block !important`.
        if let DomNodeType::Element {
          tag_name,
          namespace,
          ..
        } = &styled.node.node_type
        {
          if (namespace.is_empty() || namespace == HTML_NAMESPACE)
            && html_represents_nothing_element(tag_name)
          {
            stack.pop();
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

        let in_footnote = stack.last().map(|frame| frame.in_footnote).unwrap_or(false);

        counters.enter_scope();
        apply_counter_properties_from_style(
          styled,
          counters,
          in_footnote,
          options.enable_footnote_floats,
        );
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
              let popped = stack.pop();
              debug_assert!(popped.is_some(), "frame exists");
              counters.leave_scope();
              continue;
            }
          }
        }

        // display:none suppresses box generation entirely.
        if styled.styles.display == Display::None {
          let popped = stack.pop();
          debug_assert!(popped.is_some(), "frame exists");
          counters.leave_scope();
          continue;
        }

        // HTML <br> elements represent forced line breaks. Model them explicitly so inline layout can
        // force a new line even under `white-space: normal/nowrap` (i.e., without relying on a
        // newline character that could be collapsed to a space).
        if let Some(tag) = styled.node.tag_name() {
          if tag.eq_ignore_ascii_case("br") {
            let popped = stack.pop();
            debug_assert!(popped.is_some(), "frame exists");
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
            let popped = stack.pop();
            debug_assert!(popped.is_some(), "frame exists");
            counters.leave_scope();
            let style = transform_style_for_box_generation_if_needed(&styled.styles, &stack);
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
            box_node.original_display = styled.styles.display;
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
        let form_control = styled
          .node
          .tag_name()
          .is_some_and(|tag| {
            tag.eq_ignore_ascii_case("input")
              || tag.eq_ignore_ascii_case("textarea")
              || tag.eq_ignore_ascii_case("select")
              || tag.eq_ignore_ascii_case("progress")
              || tag.eq_ignore_ascii_case("meter")
          })
          .then(|| {
            let styled_ancestors: Vec<&StyledNode> = stack
              .iter()
              .take(stack.len().saturating_sub(1))
              .map(|frame| frame.styled)
              .collect();
            create_form_control_replaced(styled, styled_ancestors.as_slice(), interaction_state)
          })
          .flatten();

        if let Some(form_control) = form_control {
          if !matches!(
            form_control.appearance,
            crate::style::types::Appearance::None
          ) {
            // Form controls short-circuit box generation as replaced elements, but we still need to
            // honor authored ::before/::after pseudo-elements so real-world patterns like styled
            // search icons render. To keep layout complexity manageable, only attach out-of-flow
            // pseudo elements (position:absolute/fixed) since we do not run in-flow layout inside a
            // replaced element.
            let mut pseudo_children = Vec::new();
            if let Some(before_styles) = styled
              .before_styles
              .as_ref()
              .filter(|s| s.position.is_absolutely_positioned())
            {
              let before_start = clone_starting_style(&styled.starting_styles.before);
              if let Some(before_box) = create_pseudo_element_box(
                styled,
                before_styles,
                before_start,
                "before",
                counters,
                &mut quote_depth,
              ) {
                pseudo_children.push(before_box);
              }
            }
            if let Some(after_styles) = styled
              .after_styles
              .as_ref()
              .filter(|s| s.position.is_absolutely_positioned())
            {
              let after_start = clone_starting_style(&styled.starting_styles.after);
              if let Some(after_box) = create_pseudo_element_box(
                styled,
                after_styles,
                after_start,
                "after",
                counters,
                &mut quote_depth,
              ) {
                pseudo_children.push(after_box);
              }
            }

            let popped = stack.pop();
            debug_assert!(popped.is_some(), "frame exists");
            counters.leave_scope();
            let style = transform_style_for_box_generation_if_needed(&styled.styles, &stack);
            let mut box_node =
              BoxNode::new_replaced(style, ReplacedType::FormControl(form_control), None, None);
            box_node.original_display = styled.styles.display;
            box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
            box_node.children = pseudo_children;
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
            let popped = stack.pop();
            debug_assert!(popped.is_some(), "frame exists");
            counters.leave_scope();
            continue;
          }

          let is_input_image = tag.eq_ignore_ascii_case("input")
            && styled
              .node
              .get_attribute_ref("type")
              .is_some_and(|t| t.eq_ignore_ascii_case("image"));

          if (is_replaced_element(tag) || is_input_image)
            && styled.styles.display != Display::Contents
          {
            let picture_sources_for_img = if tag.eq_ignore_ascii_case("img") {
              picture_sources.take(styled.node_id)
            } else {
              Vec::new()
            };
            let ancestor_len = stack.len().saturating_sub(1);
            let style =
              transform_style_for_box_generation_if_needed(&styled.styles, &stack[..ancestor_len]);
            if let Some(box_node) = create_replaced_box_from_styled(
              styled,
              style,
              document_css,
              svg_document_css_style_element,
              picture_sources_for_img,
              options,
              site_compat,
            ) {
              let popped = stack.pop();
              debug_assert!(popped.is_some(), "frame exists");
              counters.leave_scope();
              let mut box_node = box_node;
              box_node.original_display = styled.styles.display;
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
        let Some(frame) = stack.last_mut() else {
          debug_assert!(false, "frame exists");
          break;
        };
        frame.form_control = appearance_none_form_control.map(Arc::new);
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
        let form_control = frame.form_control;
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
                DomNodeType::Element {
                  tag_name,
                  namespace,
                  ..
                } => namespace == HTML_NAMESPACE && tag_name.eq_ignore_ascii_case("legend"),
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
            let wrapper =
              BoxNode::new_anonymous_fieldset_content(Arc::new(wrapper_style), children);

            children = if let Some(legend) = legend {
              vec![legend, wrapper]
            } else {
              vec![wrapper]
            };
          }
        }

        let original_display = base_style.display;
        let style = strip_ua_form_control_edge_styles_for_appearance_none(
          styled,
          transform_style_for_box_generation_if_needed(&base_style, &stack),
          form_control.as_ref(),
        );
        let display = style.display;
        let fc_type = display
          .formatting_context_type()
          .unwrap_or(FormattingContextType::Block);

        // HTML `<button>` elements behave like atomic inline-level block containers even when
        // author styles force `display: inline` (Chrome/WebKit do not apply the generic CSS2
        // "block-in-inline splitting" to `<button>` descendants). If we generate `<button>` as a
        // normal inline box, anonymous-box fixup can hoist block descendants (e.g. `display: flex`
        // spans used as link-like buttons), shifting layout substantially (tesco.com consent banner).
        //
        // Model `<button style="display:inline">` as an inline-block-like box by attaching a block
        // formatting context to its inline box. This keeps descendants inside the button and makes
        // layout match browser behavior.
        let is_html_button = match &styled.node.node_type {
          DomNodeType::Element {
            tag_name,
            namespace,
            ..
          } => (namespace.is_empty() || namespace == HTML_NAMESPACE)
            && tag_name.eq_ignore_ascii_case("button"),
          _ => false,
        };

        let mut box_node = match display {
          Display::Block | Display::FlowRoot | Display::ListItem => {
            BoxNode::new_block(style, fc_type, children)
          }
          Display::Inline => {
            if is_html_button {
              BoxNode::new_inline_block(style, FormattingContextType::Block, children)
            } else {
              BoxNode::new_inline(style, children)
            }
          }
          Display::Ruby
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
          Display::None | Display::Contents => {
            debug_assert!(false, "display:none/contents should be handled above");
            BoxNode::new_block(style, FormattingContextType::Block, children)
          }
        };

        box_node.original_display = original_display;
        box_node.starting_style = clone_starting_style(&styled.starting_styles.base);
        box_node.first_line_style = styled.first_line_styles.as_ref().map(Arc::clone);
        box_node.first_letter_style = styled.first_letter_styles.as_ref().map(Arc::clone);
        box_node.form_control = form_control;

        if options.enable_footnote_floats && styled.styles.float == Float::Footnote && !in_footnote
        {
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
      box_node.table_column_span = Some(parse_html_table_span_attr_min_1(
        styled.node.get_attribute_ref("span"),
      ));
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

  let original_display = styles.display;
  counters.enter_scope();
  styles.counters.apply_to(counters);

  let generated_pseudo = match pseudo_name {
    "before" => Some(GeneratedPseudoElement::Before),
    "after" => Some(GeneratedPseudoElement::After),
    "footnote-call" => Some(GeneratedPseudoElement::FootnoteCall),
    "footnote-marker" => Some(GeneratedPseudoElement::FootnoteMarker),
    _ => None,
  };

  let mut pseudo_style = Arc::clone(styles);
  let mut display = pseudo_style.display;
  if pseudo_style.position.is_in_flow()
    && matches!(
      styled.styles.display,
      Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid
    )
  {
    // CSS Display Level 3: flex/grid items are blockified.
    //
    // https://www.w3.org/TR/css-display-3/#transformations
    let blockified = display.blockify();
    if blockified != display {
      let mut new_style = pseudo_style.as_ref().clone();
      new_style.display = blockified;
      pseudo_style = Arc::new(new_style);
      display = blockified;
    }
  }

  // CSS 2.1 §9.7 / CSS Display Level 3: absolutely positioned and fixed elements are blockified.
  //
  // Table-internal display types cannot participate in table layout once taken out-of-flow, so
  // they fall back to `block`.
  if matches!(pseudo_style.position, Position::Absolute | Position::Fixed) {
    let blockified = if display.is_table_internal() {
      Display::Block
    } else {
      display.blockify()
    };
    if blockified != display {
      let mut new_style = pseudo_style.as_ref().clone();
      new_style.display = blockified;
      pseudo_style = Arc::new(new_style);
      display = blockified;
    }
  }

  // CSS 2.1 §9.7: floats are blockified.
  if pseudo_style.float.is_floating() {
    let blockified = if display.is_table_internal() {
      Display::Block
    } else {
      display.blockify()
    };
    if blockified != display {
      let mut new_style = pseudo_style.as_ref().clone();
      new_style.display = blockified;
      pseudo_style = Arc::new(new_style);
      display = blockified;
    }
  }

  // Generated content items behave like anonymous child boxes of the pseudo-element. They inherit
  // inheritable properties (font, color, etc.) from the pseudo-element but should not copy
  // layout-affecting properties like `display` or `position`.
  let generated_content_style = Arc::new(crate::tree::anonymous::inherited_style(
    pseudo_style.as_ref(),
  ));

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
    debug_assert!(
      false,
      "non-empty pseudo-element content values must be ContentValue::Items"
    );
    counters.leave_scope();
    return None;
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
        flush_text(
          &mut text_buf,
          &generated_content_style,
          generated_pseudo,
          &mut children,
        );
      }
      ContentItem::Url(url) => {
        if trim_ascii_whitespace(&url.url).is_empty() {
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
            src: url.url.clone(),
            alt: None,
            loading: ImageLoadingAttribute::Auto,
            decoding: ImageDecodingAttribute::Auto,
            crossorigin: CrossOriginAttribute::None,
            referrer_policy: None,
            sizes: None,
            srcset: srcset_from_override_resolution(url),
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
  let fc_type = display
    .formatting_context_type()
    .unwrap_or(FormattingContextType::Block);

  // Wrap in appropriate box type based on display
  let mut pseudo_box = match display {
    Display::None => {
      debug_assert!(
        false,
        "display:none pseudo-elements are filtered before counter scope"
      );
      counters.leave_scope();
      return None;
    }
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

  pseudo_box.original_display = original_display;
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
  let fc_type = style
    .display
    .formatting_context_type()
    .unwrap_or(FormattingContextType::Block);

  match style.display {
    Display::None => None,
    Display::Block | Display::FlowRoot | Display::ListItem => {
      Some(BoxNode::new_block(style, fc_type, children))
    }
    Display::Inline
    | Display::Ruby
    | Display::RubyBase
    | Display::RubyText
    | Display::RubyBaseContainer
    | Display::RubyTextContainer => Some(BoxNode::new_inline(style, children)),
    Display::InlineBlock => Some(BoxNode::new_inline_block(style, fc_type, children)),
    Display::Flex => Some(BoxNode::new_block(
      style,
      FormattingContextType::Flex,
      children,
    )),
    Display::InlineFlex => Some(BoxNode::new_inline_block(
      style,
      FormattingContextType::Flex,
      children,
    )),
    Display::Grid => Some(BoxNode::new_block(
      style,
      FormattingContextType::Grid,
      children,
    )),
    Display::InlineGrid => Some(BoxNode::new_inline_block(
      style,
      FormattingContextType::Grid,
      children,
    )),
    Display::Table => Some(BoxNode::new_block(
      style,
      FormattingContextType::Table,
      children,
    )),
    Display::InlineTable => Some(BoxNode::new_inline_block(
      style,
      FormattingContextType::Table,
      children,
    )),
    Display::TableRow
    | Display::TableCell
    | Display::TableRowGroup
    | Display::TableHeaderGroup
    | Display::TableFooterGroup
    | Display::TableColumn
    | Display::TableColumnGroup
    | Display::TableCaption => Some(BoxNode::new_block(
      style,
      FormattingContextType::Block,
      children,
    )),
    Display::Contents => Some(BoxNode::new_inline(style, children)),
  }
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

  let push_line_height_strut = |children: &mut Vec<BoxNode>| {
    if !children.is_empty() {
      return;
    }

    // Use a bidi formatting mark to ensure the text shaping pipeline yields an empty glyph stream
    // (so nothing paints) while still producing a line box with the control's computed line height.
    let strut = if styled.styles.direction == Direction::Rtl {
      '\u{200f}'
    } else {
      '\u{200e}'
    };
    push_text(
      children,
      Arc::clone(&styled.styles),
      strut.to_string(),
      None,
    );
  };

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

  match &form_control.control {
    FormControlKind::Text {
      value,
      placeholder,
      placeholder_style,
      kind,
      caret,
      selection,
      ..
    } => {
      let preedit = form_control
        .ime_preedit
        .as_ref()
        .filter(|state| !state.text.is_empty());
      let mut text: Option<String> = None;
      let mut style = Arc::clone(&styled.styles);
      let mut pseudo = None;

      if !value.is_empty() {
        text = Some(value.clone());
      } else if preedit.is_none() {
        if let Some(ph) = placeholder.as_ref().filter(|p| !p.is_empty()) {
          text = Some(ph.clone());
          if let Some(ph_style) = placeholder_style
            .as_ref()
            .or(form_control.placeholder_style.as_ref())
          {
            style = Arc::clone(ph_style);
            pseudo = Some(GeneratedPseudoElement::Placeholder);
          }
        }
      }

      if matches!(kind, TextControlKind::Password) {
        // Password fields are rendered as a bullet mask. Approximate preedit by including it in the
        // masked character count (without underlining).
        let committed_len = value.chars().count();
        let preedit_len = preedit.map(|state| state.text.chars().count()).unwrap_or(0);
        let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
        let replace_start = replace_start.min(committed_len);
        let replace_end = replace_end.min(committed_len);
        let replaced_len = if preedit.is_some() {
          replace_end.saturating_sub(replace_start)
        } else {
          0
        };
        let total_len = committed_len.saturating_sub(replaced_len).saturating_add(preedit_len);
        if total_len > 0 {
          let mask_len = total_len.clamp(3, 50);
          push_text(
            &mut children,
            Arc::clone(&styled.styles),
            "•".repeat(mask_len),
            None,
          );
        } else if let Some(ph) = placeholder.as_ref().filter(|p| !p.is_empty()) {
          let (style, pseudo) = if let Some(ph_style) = placeholder_style
            .as_ref()
            .or(form_control.placeholder_style.as_ref())
          {
            (Arc::clone(ph_style), Some(GeneratedPseudoElement::Placeholder))
          } else {
            (Arc::clone(&styled.styles), None)
          };
          push_text(&mut children, style, ph.clone(), pseudo);
        }
      } else if let Some(preedit) = preedit {
        // Render committed value (if any) with an underlined preedit string inserted at the caret
        // (replacing any active selection).
        let committed_len = value.chars().count();
        let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
        let replace_start = replace_start.min(committed_len);
        let replace_end = replace_end.min(committed_len);
        let start_byte = byte_offset_for_char_idx(value, replace_start);
        let end_byte = byte_offset_for_char_idx(value, replace_end);
        let before = value.get(..start_byte).unwrap_or("");
        let after = value.get(end_byte..).unwrap_or("");

        if !before.is_empty() {
          push_text(
            &mut children,
            Arc::clone(&style),
            before.to_string(),
            pseudo,
          );
        }
        let mut underline_style = (*styled.styles).clone();
        underline_style
          .text_decoration
          .lines
          .insert(crate::style::types::TextDecorationLine::UNDERLINE);
        push_text(
          &mut children,
          Arc::new(underline_style),
          preedit.text.to_string(),
          None,
        );
        if !after.is_empty() {
          push_text(&mut children, Arc::clone(&style), after.to_string(), pseudo);
        }
      } else if let Some(text) = text {
        push_text(&mut children, style, text, pseudo);
      }

      push_line_height_strut(&mut children);
    }
    FormControlKind::TextArea {
      value,
      placeholder,
      placeholder_style,
      caret,
      selection,
      ..
    } => {
      suppress_dom_children = true;
      let preedit = form_control
        .ime_preedit
        .as_ref()
        .filter(|state| !state.text.is_empty());
      let mut text: Option<String> = None;
      let mut style = Arc::clone(&styled.styles);
      let mut pseudo = None;
      if !value.is_empty() {
        text = Some(value.clone());
      } else if preedit.is_none() {
        if let Some(ph) = placeholder.as_ref().filter(|p| !p.is_empty()) {
          text = Some(ph.clone());
          if let Some(ph_style) = placeholder_style
            .as_ref()
            .or(form_control.placeholder_style.as_ref())
          {
            style = Arc::clone(ph_style);
            pseudo = Some(GeneratedPseudoElement::Placeholder);
          }
        }
      }

      if let Some(preedit) = preedit {
        let committed_len = value.chars().count();
        let (replace_start, replace_end) = (*selection).unwrap_or((*caret, *caret));
        let replace_start = replace_start.min(committed_len);
        let replace_end = replace_end.min(committed_len);
        let start_byte = byte_offset_for_char_idx(value, replace_start);
        let end_byte = byte_offset_for_char_idx(value, replace_end);
        let before = value.get(..start_byte).unwrap_or("");
        let after = value.get(end_byte..).unwrap_or("");

        if !before.is_empty() {
          push_text(
            &mut children,
            Arc::clone(&style),
            before.to_string(),
            pseudo,
          );
        }
        let mut underline_style = (*styled.styles).clone();
        underline_style
          .text_decoration
          .lines
          .insert(crate::style::types::TextDecorationLine::UNDERLINE);
        push_text(
          &mut children,
          Arc::new(underline_style),
          preedit.text.to_string(),
          None,
        );
        if !after.is_empty() {
          push_text(&mut children, Arc::clone(&style), after.to_string(), pseudo);
        }
      } else if let Some(text) = text {
        push_text(&mut children, style, text, pseudo);
      }

      push_line_height_strut(&mut children);
    }
    FormControlKind::Button { label } => {
      if styled
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
        && !label.is_empty()
      {
        push_text(
          &mut children,
          Arc::clone(&styled.styles),
          label.clone(),
          None,
        );
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
            if trimmed.is_empty() {
              Some(value.as_str())
            } else {
              Some(label.as_str())
            }
          }
          _ => None,
        })
        .unwrap_or("Select");
      if !label.is_empty() {
        push_text(
          &mut children,
          Arc::clone(&styled.styles),
          label.to_string(),
          None,
        );
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
      let is_rtl = styled.styles.direction == crate::style::types::Direction::Rtl;

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
        if is_rtl {
          style.left = InsetValue::Auto;
          style.right = InsetValue::Length(Length::new(clamped_pct, LengthUnit::Percent));
        } else {
          style.left = InsetValue::Length(Length::new(clamped_pct, LengthUnit::Percent));
          style.right = InsetValue::Auto;
        }
        style.top = InsetValue::Length(Length::new(50.0, LengthUnit::Percent));
        style.bottom = InsetValue::Auto;
        style.translate = TranslateValue::Values {
          x: Length::new(
            if is_rtl { clamped_pct } else { -clamped_pct },
            LengthUnit::Percent,
          ),
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

        if let Some(mut button_node) =
          create_box_from_style(Arc::clone(button_style), button_children)
        {
          button_node.styled_node_id = Some(styled_id);
          button_node.generated_pseudo = Some(GeneratedPseudoElement::FileSelectorButton);
          children.push(button_node);
        }
      }

      push_text(
        &mut children,
        Arc::clone(&styled.styles),
        file_label.to_string(),
        None,
      );
    }
    FormControlKind::Unknown { label } => {
      if let Some(text) = label.as_ref().filter(|t| !t.is_empty()) {
        push_text(
          &mut children,
          Arc::clone(&styled.styles),
          text.clone(),
          None,
        );
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
    let mut image: Option<crate::style::types::BackgroundImageUrl> = None;

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
            if trim_ascii_whitespace(&url.url).is_empty() {
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
    if let Some(url) = image {
      let srcset = srcset_from_override_resolution(&url);
      let src = url.url;
      *quote_depth = context.quote_depth();
      let replaced = ReplacedBox {
        replaced_type: ReplacedType::Image {
          src,
          alt: None,
          loading: ImageLoadingAttribute::Auto,
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset,
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
      let srcset = srcset_from_override_resolution(url);
      let replaced = ReplacedBox {
        replaced_type: ReplacedType::Image {
          src: url.url.clone(),
          alt: None,
          loading: ImageLoadingAttribute::Auto,
          decoding: ImageDecodingAttribute::Auto,
          crossorigin: CrossOriginAttribute::None,
          referrer_policy: None,
          sizes: None,
          srcset,
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

fn replaced_image_src_from_content_property(
  style: &ComputedStyle,
) -> Option<crate::style::types::BackgroundImageUrl> {
  let ContentValue::Items(items) = effective_content_value(style) else {
    return None;
  };

  let mut src: Option<crate::style::types::BackgroundImageUrl> = None;
  for item in items {
    match item {
      ContentItem::Url(url) => {
        if trim_ascii_whitespace(&url.url).is_empty() {
          continue;
        }
        if src.is_some() {
          return None;
        }
        src = Some(url);
      }
      ContentItem::String(s) => {
        if !trim_ascii_whitespace(&s).is_empty() {
          return None;
        }
      }
      _ => return None,
    }
  }

  src
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

  // Elements that do not participate in rendering must not affect counters.
  //
  // Note: `display: contents` suppresses box generation for the element itself, but its descendants
  // still participate in layout/paint. Counters are defined over the element tree (not the box
  // tree), so counter-reset/set/increment must still apply for `display: contents` elements.
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
    let is_list = tag.is_some_and(|tag| {
      tag.eq_ignore_ascii_case("ol")
        || tag.eq_ignore_ascii_case("ul")
        || tag.eq_ignore_ascii_case("menu")
        || tag.eq_ignore_ascii_case("dir")
    });
    // Treat list containers as nested list boundaries when they generate boxes. A list container
    // with `display: contents` contributes its descendants without applying list counter resets, so
    // its list items participate in the ancestor list's counter sequence.
    let now_nested = in_nested_list || (is_list && node.styles.display != Display::Contents);
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
        value, in_optgroup, ..
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
  // HTML: for button-like inputs (`type=submit|reset|button`), the `value` attribute is the label
  // when present, even when the attribute value is the empty string. Only when the attribute is
  // missing does the user agent default label apply.
  if let Some(value) = node.get_attribute_ref("value") {
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

fn create_form_control_replaced(
  styled: &StyledNode,
  styled_ancestors: &[&StyledNode],
  interaction_state: Option<&InteractionState>,
) -> Option<FormControl> {
  let tag = styled.node.tag_name()?;
  let appearance = styled.styles.appearance.clone();

  if !tag.eq_ignore_ascii_case("input")
    && !tag.eq_ignore_ascii_case("textarea")
    && !tag.eq_ignore_ascii_case("select")
    && !tag.eq_ignore_ascii_case("progress")
    && !tag.eq_ignore_ascii_case("meter")
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

  let disabled = if styled.node.get_attribute_ref("disabled").is_some() {
    true
  } else if tag.eq_ignore_ascii_case("progress") || tag.eq_ignore_ascii_case("meter") {
    false
  } else {
    let mut disabled = false;
    for (i, ancestor) in styled_ancestors.iter().enumerate().rev() {
      if !ancestor
        .node
        .tag_name()
        .is_some_and(|a_tag| a_tag.eq_ignore_ascii_case("fieldset"))
      {
        continue;
      }
      if ancestor.node.get_attribute_ref("disabled").is_none() {
        continue;
      }

      let first_legend = ancestor.children.iter().find(|child| {
        child
          .node
          .tag_name()
          .is_some_and(|child_tag| child_tag.eq_ignore_ascii_case("legend"))
      });

      if let Some(first_legend) = first_legend {
        let in_legend = styled_ancestors[i + 1..]
          .iter()
          .any(|ancestor| ancestor.node_id == first_legend.node_id);
        if in_legend {
          continue;
        }
      }

      disabled = true;
      break;
    }
    disabled
  };
  let inert = styled.node.get_attribute_ref("inert").is_some()
    || styled
      .node
      .get_attribute_ref("data-fastr-inert")
      .map(|v| v.eq_ignore_ascii_case("true"))
      .unwrap_or(false);
  let mut focused = false;
  let mut focus_visible = false;
  if !inert && !disabled {
    if let Some(state) = interaction_state {
      focused = state.is_focused(styled.node_id);
      focus_visible = focused && state.focus_visible;
    }
  }
  let textarea_value = if tag.eq_ignore_ascii_case("textarea") {
    if let Some(value) = interaction_state.and_then(|state| state.form_state().value_for(styled.node_id))
    {
      Some(value.to_string())
    } else {
      Some(crate::dom::textarea_current_value_from_text_content(
        &styled.node,
        collect_text_content(styled),
      ))
    }
  } else {
    None
  };
  let mut select_control: Option<SelectControl> = None;
  if tag.eq_ignore_ascii_case("select") {
    let mut control = build_select_control(styled);
    if let Some(selected_set) = interaction_state.and_then(|state| {
      state
        .form_state()
        .select_selected_options(styled.node_id)
    }) {
      let mut items = (*control.items).clone();
      let mut selected = Vec::new();
      for (idx, item) in items.iter_mut().enumerate() {
        if let SelectItem::Option {
          node_id,
          selected: item_selected,
          ..
        } = item
        {
          let is_selected = selected_set.contains(node_id);
          *item_selected = is_selected;
          if is_selected {
            selected.push(idx);
          }
        }
      }
      control.items = Arc::new(items);
      control.selected = selected;
    }
    select_control = Some(control);
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
          !control.selected.iter().any(|&idx| {
            matches!(
              control.items.get(idx),
              Some(SelectItem::Option {
                disabled: false,
                ..
              })
            )
          })
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
      ime_preedit: None,
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

    let value = parse_f32_attr(&styled.node, "value")
      .unwrap_or(min)
      .clamp(min, max);
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
      ime_preedit: None,
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
      let checked = interaction_state
        .and_then(|state| state.form_state().checked_for(styled.node_id))
        .unwrap_or_else(|| styled.node.get_attribute_ref("checked").is_some());
      FormControlKind::Checkbox {
        is_radio: false,
        checked,
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
      let checked = interaction_state
        .and_then(|state| state.form_state().checked_for(styled.node_id))
        .unwrap_or_else(|| styled.node.get_attribute_ref("checked").is_some());
      FormControlKind::Checkbox {
        is_radio: true,
        checked,
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
      let sanitized =
        crate::dom::input_color_value_string(&styled.node).unwrap_or_else(|| "#000000".to_string());
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
      let value = form_controls::file_input_display_value(interaction_state, styled.node_id);
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
      let value = interaction_state
        .and_then(|state| state.form_state().value_for(styled.node_id))
        .map(|v| v.to_string())
        .unwrap_or_else(|| element_ref.accessibility_value().unwrap_or_default());

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
       } else {
         // HTML: invalid/unknown `<input type="...">` values fall back to the text state.
         TextControlKind::Plain
       };

       let value_char_len = value.chars().count();
       let (caret, caret_affinity, selection) = form_controls::text_edit_state_for_value_char_len(
         interaction_state,
         styled.node_id,
         value_char_len,
       );

       FormControlKind::Text {
         value,
         placeholder,
        placeholder_style: styled.placeholder_styles.clone(),
        size_attr,
        kind,
        caret,
        caret_affinity,
        selection,
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
        FormControlKind::File { .. } => {
          (None, None, None, styled.file_selector_button_styles.clone())
        }
       _ => (None, None, None, None),
      };

    let ime_preedit = if matches!(&control, FormControlKind::Text { .. }) {
      form_controls::ime_preedit_for_node(interaction_state, styled.node_id)
    } else {
      None
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
      ime_preedit,
    })
  } else if tag.eq_ignore_ascii_case("textarea") {
    let placeholder = styled
      .node
      .get_attribute_ref("placeholder")
      .map(trim_ascii_whitespace)
      .filter(|p| !p.is_empty())
      .map(|p| p.to_string());
    let value = textarea_value.unwrap_or_default();
    let value_char_len = value.chars().count();
    let (caret, caret_affinity, selection) = form_controls::text_edit_state_for_value_char_len(
      interaction_state,
      styled.node_id,
      value_char_len,
    );
    Some(FormControl {
      control: FormControlKind::TextArea {
        value,
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
        caret,
        caret_affinity,
        selection,
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
      ime_preedit: form_controls::ime_preedit_for_node(interaction_state, styled.node_id),
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
      ime_preedit: None,
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
    let normalized =
      trim_ascii_whitespace(mime.split(';').next().unwrap_or("")).to_ascii_lowercase();
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

fn parse_html_dimension_attr(raw: Option<&str>) -> Option<f32> {
  // HTML "dimension" content attributes (e.g. `<img width>`, `<iframe height>`) are defined as
  // non-negative integers in *CSS pixels*.
  //
  // Real-world markup commonly includes a `px` suffix (e.g. `width="50px"`). Browsers parse the
  // leading integer and ignore the rest, so do the same here.
  let raw = raw?;
  let bytes = raw.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() && bytes[i].is_ascii_whitespace() {
    i += 1;
  }
  if i >= bytes.len() {
    return None;
  }

  let mut value: u32 = 0;
  let mut saw_digit = false;
  while i < bytes.len() {
    let b = bytes[i];
    if !b.is_ascii_digit() {
      break;
    }
    saw_digit = true;
    value = value.saturating_mul(10).saturating_add((b - b'0') as u32);
    i += 1;
  }

  if !saw_digit || value == 0 {
    return None;
  }

  Some(value as f32)
}

/// Creates a BoxNode for a replaced element from a StyledNode
fn create_replaced_box_from_styled(
  styled: &StyledNode,
  style: Arc<ComputedStyle>,
  document_css: &str,
  svg_document_css_style_element: Option<&Arc<str>>,
  mut picture_sources: Vec<PictureSource>,
  options: &BoxGenerationOptions,
  site_compat: bool,
) -> Option<BoxNode> {
  let tag = styled.node.tag_name().unwrap_or("img");

  // Determine replaced type
  let replaced_type = if tag.eq_ignore_ascii_case("img") {
    let mut src = styled
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
    let loading = styled
      .node
      .get_attribute_ref("loading")
      .map(ImageLoadingAttribute::from_attribute)
      .unwrap_or_default();
    let decoding = styled
      .node
      .get_attribute_ref("decoding")
      .map(ImageDecodingAttribute::from_attribute)
      .unwrap_or_default();
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
    let mut srcset = styled
      .node
      .get_attribute_ref("srcset")
      .map(parse_srcset)
      .unwrap_or_default();
    let mut sizes = styled.node.get_attribute_ref("sizes").and_then(parse_sizes);
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);

    // Chrome applies `content: url(...)` to replaced elements like `<img>` as a way to override the
    // replaced content. Real-world pages use this to set images purely from CSS (e.g. language
    // flags, theme-based icon swaps).
    //
    // FastRender only supports this behavior when the `content` property is a pure URL (no mixed
    // text/counters/etc), matching the common authoring pattern.
    if let Some(content_src) = replaced_image_src_from_content_property(&style) {
      srcset = srcset_from_override_resolution(&content_src);
      src = content_src.url;
      sizes = None;
      picture_sources.clear();
    }
    ReplacedType::Image {
      src,
      alt,
      loading,
      decoding,
      crossorigin,
      referrer_policy,
      srcset,
      sizes,
      picture_sources,
    }
  } else if tag.eq_ignore_ascii_case("video") {
    let media_ctx = options.media_context();
    let src = crate::html::media::effective_media_src(
      styled,
      crate::html::media::MediaElementKind::Video,
      media_ctx.as_ref(),
    );
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
    let controls = styled.node.get_attribute_ref("controls").is_some();
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
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);
    ReplacedType::Video {
      src,
      poster,
      crossorigin,
      referrer_policy,
      controls,
    }
  } else if tag.eq_ignore_ascii_case("audio") {
    let media_ctx = options.media_context();
    let src = crate::html::media::effective_media_src(
      styled,
      crate::html::media::MediaElementKind::Audio,
      media_ctx.as_ref(),
    );
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
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);
    ReplacedType::Audio {
      src,
      crossorigin,
      referrer_policy,
    }
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
    let sandbox = IframeSandboxAttribute::from_attribute(styled.node.get_attribute_ref("sandbox"));
    let referrer_policy = styled
      .node
      .get_attribute_ref("referrerpolicy")
      .and_then(ReferrerPolicy::from_attribute);
    ReplacedType::Iframe {
      src,
      srcdoc,
      sandbox,
      referrer_policy,
      // Use the styled DOM node id as a stable per-document token. This id is derived from
      // `dom::enumerate_dom_ids` and remains stable across layout/paint passes as long as the DOM is
      // unchanged.
      frame_token: Some(styled.node_id as u64),
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
      loading: ImageLoadingAttribute::Auto,
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      sizes: None,
      srcset: Vec::new(),
      picture_sources: Vec::new(),
    }
  };

  let width_attr = styled.node.get_attribute_ref("width");
  let height_attr = styled.node.get_attribute_ref("height");

  let (mut intrinsic_size, mut aspect_ratio, mut no_intrinsic_ratio) = match &replaced_type {
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
      let intrinsic_width = parse_html_dimension_attr(width_attr)
        // HTML width/height content attributes are non-negative integers, but we treat 0 and
        // non-finite values as "missing" so they don't get recorded as an intrinsic size and
        // suppress later intrinsic sizing fallbacks (e.g. alt-text sizing when the image cannot be
        // loaded). (This also matches our internal convention where an intrinsic axis is only
        // considered known when it is a finite, positive number.)
        .filter(|w| w.is_finite() && *w > 0.0);

      let intrinsic_height =
        parse_html_dimension_attr(height_attr).filter(|h| h.is_finite() && *h > 0.0);

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
      // Canvas elements have intrinsic dimensions and therefore an intrinsic ratio derived from
      // their default 300x150 size.
      ReplacedType::Canvas => {
        intrinsic_size = Some(Size::new(300.0, 150.0));
        aspect_ratio = Some(2.0);
      }
      // Many other replaced elements have a default UA size but *no intrinsic ratio*. The
      // fallback size should not force the used height to scale when only a width is specified
      // (or vice versa).
      //
      // This matches web behavior for elements like `<video>` (without metadata),
      // `<iframe>`, `<embed>`, and `<object>`, which default to a 300×150 rectangle but do not
      // preserve a 2:1 aspect ratio unless an explicit `aspect-ratio` property (or actual
      // intrinsic data) is available.
      ReplacedType::Video { .. }
      | ReplacedType::Iframe { .. }
      | ReplacedType::Embed { .. }
      | ReplacedType::Object { .. } => {
        intrinsic_size = Some(Size::new(300.0, 150.0));
        aspect_ratio = None;
        no_intrinsic_ratio = true;
      }
      ReplacedType::Audio { .. } => {
        intrinsic_size = Some(Size::new(300.0, 32.0));
        aspect_ratio = None;
        no_intrinsic_ratio = true;
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

  let original_display = style.display;
  Some(BoxNode {
    box_type: BoxType::Replaced(replaced_box),
    style,
    original_display,
    starting_style: None,
    children: vec![],
    footnote_body: None,
    id: 0,
    debug_info: None,
    styled_node_id: None,
    generated_pseudo: None,
    implicit_anchor_box_id: None,
    form_control: None,
    table_cell_span: None,
    table_column_span: None,
    first_line_style: None,
    first_letter_style: None,
  })
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
    ListStyleType::DisclosureOpen => {
      registry.format_marker_string(value, CounterStyle::DisclosureOpen)
    }
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

#[cfg(test)]
mod tests;
