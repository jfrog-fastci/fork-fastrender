//! Box Tree - Represents CSS boxes before layout
//!
//! The box tree is generated from the styled DOM tree and represents
//! the CSS box model. It's independent of layout - it only represents
//! what boxes exist and their styling, not where they're positioned.
//!
//! # Separation of Concerns
//!
//! **Box Tree (this module)**:
//! - Immutable
//! - No positions or final sizes
//! - Represents "what to layout"
//! - Generated once from DOM
//!
//! **Fragment Tree**:
//! - Result of layout
//! - Has positions and sizes
//! - Represents "what was laid out"
//! - Generated per layout pass
//!
//! Reference: CSS Display Module Level 3
//! <https://www.w3.org/TR/css-display-3/>

use crate::geometry::Size;
use crate::math::MathLayout;
use crate::resource::ReferrerPolicy;
use crate::style::color::Rgba;
use crate::style::display::FormattingContextType;
use crate::style::media::MediaQuery;
use crate::style::types::Appearance;
use crate::style::types::Overflow;
use crate::style::ComputedStyle;
use crate::tree::debug::DebugInfo;
use std::fmt;
use std::sync::Arc;

pub use crate::html::images::{ImageSelectionContext, SelectedImageSource};

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'))
}

/// Parsed `crossorigin` attribute state for `<img>` elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CrossOriginAttribute {
  /// No `crossorigin` attribute present (default: non-CORS fetch).
  #[default]
  None,
  /// `crossorigin` present with empty/`anonymous`/unknown value.
  Anonymous,
  /// `crossorigin="use-credentials"`.
  UseCredentials,
}

/// A block-level box
///
/// Block boxes stack vertically and establish block formatting contexts.
/// Examples: div, p, h1, section
///
/// Reference: CSS 2.1 Section 9.2.1
#[derive(Debug, Clone)]
pub struct BlockBox {
  /// What formatting context does this establish?
  pub formatting_context: FormattingContextType,
}

/// An inline-level box
///
/// Inline boxes flow horizontally within lines.
/// Examples: span, a, em, strong
///
/// Reference: CSS 2.1 Section 9.2.2
#[derive(Debug, Clone)]
pub struct InlineBox {
  /// For inline-block, this establishes a formatting context
  /// For regular inline, this is None
  pub formatting_context: Option<FormattingContextType>,
}

/// A text box containing actual text content
///
/// Text boxes are always inline-level and contain strings to be shaped.
///
/// # Note
///
/// The text stored here is the raw text. It will be shaped (with font,
/// bidi, script analysis) during inline layout.
#[derive(Debug, Clone)]
pub struct TextBox {
  /// The text content
  ///
  /// This is UTF-8 text that may contain multiple scripts, emojis, etc.
  pub text: String,
}

/// A forced line-break box (`<br>`)
///
/// `<br>` participates in the inline formatting context at its DOM position but forces a new line
/// in the inline layout algorithm.
#[derive(Debug, Clone)]
pub struct LineBreakBox;

#[derive(Debug, Clone)]
pub enum MarkerContent {
  Text(String),
  Image(ReplacedBox),
}

/// A list marker box
///
/// Generated for list items. Carries marker text but participates as its own
/// box type so inline/layout can treat markers specially (e.g., position
/// outside the principal block).
#[derive(Debug, Clone)]
pub struct MarkerBox {
  /// Marker payload (text or image)
  pub content: MarkerContent,
}

/// A form control description used by the painter when rendering native controls.
#[derive(Debug, Clone)]
pub struct FormControl {
  /// Specific control type and metadata
  pub control: FormControlKind,
  /// Resolved appearance value (Auto/None/Keyword)
  pub appearance: Appearance,
  /// Computed style for the ::placeholder pseudo-element, when relevant.
  pub placeholder_style: Option<Arc<ComputedStyle>>,
  /// Computed style for the range thumb pseudo-element, when relevant.
  pub slider_thumb_style: Option<Arc<ComputedStyle>>,
  /// Computed style for the range track pseudo-element, when relevant.
  pub slider_track_style: Option<Arc<ComputedStyle>>,
  /// Computed style for the `::file-selector-button` pseudo-element, when relevant.
  pub file_selector_button_style: Option<Arc<ComputedStyle>>,
  /// Whether the control is disabled
  pub disabled: bool,
  /// Whether the control is focused (data-fastr-focus hint)
  pub focused: bool,
  /// Whether the control is focus-visible (data-fastr-focus-visible hint)
  pub focus_visible: bool,
  /// Whether the control is marked as required
  pub required: bool,
  /// Whether the control currently fails HTML constraint validation
  pub invalid: bool,
}

impl PartialEq for FormControl {
  fn eq(&self, other: &Self) -> bool {
    self.control == other.control
      && self.appearance == other.appearance
      && self.disabled == other.disabled
      && self.focused == other.focused
      && self.focus_visible == other.focus_visible
      && self.required == other.required
      && self.invalid == other.invalid
  }
}

/// Specific form control kinds
#[derive(Debug, Clone, PartialEq)]
pub enum FormControlKind {
  /// Text-like control (<input type=text>, search, etc.)
  Text {
    /// Current value attribute
    value: String,
    /// Placeholder text to render when value is empty
    placeholder: Option<String>,
    /// Computed styles for the `::placeholder` pseudo-element, if any.
    placeholder_style: Option<Arc<ComputedStyle>>,
    /// Optional size attribute hint for intrinsic width
    size_attr: Option<u32>,
    /// What kind of text control to render
    kind: TextControlKind,
  },
  /// Multiline control (<textarea>)
  TextArea {
    /// Raw text content
    value: String,
    /// Placeholder text to render when value is empty
    placeholder: Option<String>,
    /// Computed styles for the `::placeholder` pseudo-element, if any.
    placeholder_style: Option<Arc<ComputedStyle>>,
    /// Optional rows hint (default 2)
    rows: Option<u32>,
    /// Optional cols hint (default 20)
    cols: Option<u32>,
  },
  /// Button control (<button> and <input type=button|submit|reset>)
  Button { label: String },
  /// Selection control (<select>)
  Select(SelectControl),
  /// Checkbox or radio input
  Checkbox {
    /// Whether this represents a radio input (circle) instead of checkbox (square)
    is_radio: bool,
    /// Current checked state
    checked: bool,
    /// Whether the control is explicitly indeterminate (checkbox only)
    indeterminate: bool,
  },
  /// Range control (<input type=range>)
  Range {
    /// Current numeric value
    value: f32,
    /// Minimum value (defaults to 0)
    min: f32,
    /// Maximum value (defaults to 100; clamped to `min` when `max < min`)
    max: f32,
  },
  /// Color input (<input type=color>)
  Color {
    /// Resolved color value (defaults to black)
    value: crate::style::color::Rgba,
    /// Raw value attribute for fallback text
    raw: Option<String>,
  },
  /// File input (<input type=file>)
  File {
    /// Raw value attribute (if present); used as a best-effort "selected file" hint.
    value: Option<String>,
  },
  /// Fallback for unknown input types
  Unknown { label: Option<String> },
}

impl FormControlKind {
  /// Returns a concise, stable label for debug snapshots.
  ///
  /// Snapshots should remain small and predictable even if control variants grow
  /// to store large internal state (e.g. `<select>` option lists).
  pub fn snapshot_label(&self) -> String {
    match self {
      // `<select>` may carry a full option/optgroup model for listbox painting.
      // Snapshot output must avoid dumping the entire tree.
      FormControlKind::Select(select) => {
        const MAX_SELECTED_LABEL_CHARS: usize = 80;

        fn truncate_for_snapshot(s: &str, max_chars: usize) -> String {
          if max_chars == 0 {
            return "...".to_string();
          }

          let mut end = s.len();
          for (i, (idx, _)) in s.char_indices().enumerate() {
            if i == max_chars {
              end = idx;
              break;
            }
          }

          if end == s.len() {
            s.to_string()
          } else {
            let mut out = String::with_capacity(end + 3);
            out.push_str(&s[..end]);
            out.push_str("...");
            out
          }
        }

        let mode = if select.multiple || select.size > 1 {
          "listbox"
        } else {
          "dropdown"
        };

        let option_count = select
          .items
          .iter()
          .filter(|item| matches!(item, SelectItem::Option { .. }))
          .count();
        let optgroup_count = select
          .items
          .iter()
          .filter(|item| matches!(item, SelectItem::OptGroupLabel { .. }))
          .count();

        let mut selected_option_count = 0usize;
        let mut min_selected_idx: Option<usize> = None;
        for &idx in &select.selected {
          if matches!(select.items.get(idx), Some(SelectItem::Option { .. })) {
            selected_option_count += 1;
            min_selected_idx = Some(min_selected_idx.map(|min| min.min(idx)).unwrap_or(idx));
          }
        }
        let first_selected = min_selected_idx.and_then(|idx| match select.items.get(idx) {
          Some(SelectItem::Option { label, value, .. }) => Some((label.as_str(), value.as_str())),
          _ => None,
        });

        let selected_summary = if selected_option_count == 0 {
          "none".to_string()
        } else if selected_option_count == 1 {
          let (label, value) = first_selected.unwrap_or(("", ""));
          let text = if trim_ascii_whitespace(label).is_empty() { value } else { label };
          let text = truncate_for_snapshot(text, MAX_SELECTED_LABEL_CHARS);
          format!("{text:?}")
        } else {
          format!("{selected_option_count} selected")
        };

        format!(
          "Select{{mode={mode}, multiple={}, size={}, selected={selected_summary}, options={option_count}, optgroups={optgroup_count}}}",
          select.multiple, select.size
        )
      }
      // Other control kinds are currently small, so reuse the derived Debug output.
      _ => format!("{self:?}"),
    }
  }
}

/// Specific text-like controls that share sizing and placeholder rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum TextControlKind {
  /// Default text-like input
  Plain,
  /// Password input (masked)
  Password,
  /// Numeric input without custom range painting
  Number,
  /// Date-like input that renders a simple date placeholder
  Date,
}

/// Flattened model of a `<select>` control used for intrinsic sizing + painting.
#[derive(Clone, PartialEq)]
pub struct SelectControl {
  /// Whether multiple selections are allowed.
  pub multiple: bool,
  /// Computed visible row count (`size` attribute semantics).
  pub size: u32,
  /// Flattened rows in tree order (includes optgroup labels).
  ///
  /// Stored behind an `Arc` because this snapshot can be large and is cloned into the fragment
  /// tree; sharing avoids quadratic memory use for large `<select>` lists.
  pub items: Arc<Vec<SelectItem>>,
  /// Indices into `items` for selected `<option>` rows (tree order).
  pub selected: Vec<usize>,
}

impl fmt::Debug for SelectControl {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("SelectControl")
      .field("multiple", &self.multiple)
      .field("size", &self.size)
      .field("items_len", &self.items.len())
      .field("selected", &self.selected)
      .finish()
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
  OptGroupLabel {
    label: String,
    disabled: bool,
  },
  Option {
    /// Stable DOM pre-order id of the underlying `<option>` element.
    ///
    /// This matches the `StyledNode.node_id` assigned during the CSS cascade (see
    /// [`crate::dom::enumerate_dom_ids`]). It allows listbox hit-testing / interaction code to map
    /// painted rows back to DOM nodes.
    node_id: usize,
    label: String,
    value: String,
    selected: bool,
    disabled: bool,
    in_optgroup: bool,
  },
}

/// A replaced element box
///
/// Replaced elements have intrinsic dimensions provided by external content.
/// Examples: img, canvas, video, iframe
///
/// Reference: CSS 2.1 Section 10.3.2
#[derive(Debug, Clone, PartialEq)]
pub struct ReplacedBox {
  /// Type of replaced element
  pub replaced_type: ReplacedType,

  /// Intrinsic size (if known)
  ///
  /// Some replaced elements have intrinsic dimensions (images with width/height),
  /// others don't (iframes without size attributes).
  pub intrinsic_size: Option<Size>,

  /// Intrinsic aspect ratio (width / height)
  ///
  /// Used for sizing when only one dimension is specified.
  pub aspect_ratio: Option<f32>,

  /// True when the replaced resource explicitly declares that it has *no* intrinsic aspect ratio.
  ///
  /// Notably, inline/external SVG content with `preserveAspectRatio="none"` must not contribute an
  /// intrinsic ratio even when it has intrinsic width/height. Layout must avoid re-deriving a ratio
  /// from `intrinsic_size` when this flag is set.
  pub no_intrinsic_ratio: bool,
}

/// MathML replaced content with cached layout.
#[derive(Debug, Clone)]
pub struct MathReplaced {
  pub root: crate::math::MathNode,
  pub layout: Option<Arc<MathLayout>>,
}

impl PartialEq for MathReplaced {
  fn eq(&self, other: &Self) -> bool {
    self.root == other.root
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SrcsetDescriptor {
  Density(f32),
  Width(u32),
  WidthHeight { width: u32, height: u32 },
}

impl fmt::Display for SrcsetDescriptor {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      SrcsetDescriptor::Density(d) => {
        if d.fract() == 0.0 {
          write!(f, "{}x", *d as i32)
        } else {
          write!(f, "{:.2}x", d)
        }
      }
      SrcsetDescriptor::Width(w) => write!(f, "{}w", w),
      SrcsetDescriptor::WidthHeight { width, height } => write!(f, "{}w {}h", width, height),
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SrcsetCandidate {
  pub url: String,
  pub descriptor: SrcsetDescriptor,
}

/// A length value from the HTML `sizes` attribute.
///
/// Unlike CSS computed lengths, `sizes` values can include `min()`/`max()`/`clamp()` expressions
/// whose results depend on runtime context (viewport size, font sizes). We keep a small AST so we
/// can resolve these functions during responsive image selection.
#[derive(Debug, Clone, PartialEq)]
pub enum SizesLength {
  Length(crate::style::values::Length),
  Min(Vec<SizesLength>),
  Max(Vec<SizesLength>),
  Clamp {
    min: Box<SizesLength>,
    preferred: Box<SizesLength>,
    max: Box<SizesLength>,
  },
  /// Arithmetic from a `calc()` expression that can't be represented as a linear `CalcLength`.
  ///
  /// We only need runtime evaluation for responsive image selection, so we store a small AST
  /// instead of trying to normalize the expression into a single `Length`.
  Add(Box<SizesLength>, Box<SizesLength>),
  Sub(Box<SizesLength>, Box<SizesLength>),
}

impl From<crate::style::values::Length> for SizesLength {
  fn from(length: crate::style::values::Length) -> Self {
    SizesLength::Length(length)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizesEntry {
  pub media: Option<Vec<crate::style::media::MediaQuery>>,
  pub length: SizesLength,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizesList {
  pub entries: Vec<SizesEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PictureSource {
  pub srcset: Vec<SrcsetCandidate>,
  pub sizes: Option<SizesList>,
  pub media: Option<Vec<MediaQuery>>,
  pub mime_type: Option<String>,
}

/// Serialized SVG content plus any inlined foreignObject metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SvgDocumentCssInjection {
  /// CDATA-wrapped `<style>` element containing document-level CSS to inject into the SVG root.
  pub style_element: Arc<str>,
  /// Byte offset within the serialized SVG string where `style_element` should be inserted.
  ///
  /// This is typically immediately after the root element's `>` delimiter.
  pub insert_pos: usize,
}

/// Serialized SVG content plus any inlined foreignObject metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SvgContent {
  /// Serialized SVG markup (may include foreignObject placeholders).
  pub svg: String,
  /// Placeholder rendering when nested HTML rendering fails.
  pub fallback_svg: String,
  /// Serialized `<foreignObject>` subtrees to be rendered separately.
  pub foreign_objects: Vec<ForeignObjectInfo>,
  /// Document-level CSS collected while serializing the SVG subtree.
  pub shared_css: String,
  /// Optional injection of document-level CSS into the SVG root.
  pub document_css_injection: Option<SvgDocumentCssInjection>,
}

impl SvgContent {
  /// Creates SVG content without any foreignObject handling.
  pub fn raw(svg: impl Into<String>) -> Self {
    let svg = svg.into();
    Self {
      svg: svg.clone(),
      fallback_svg: svg,
      foreign_objects: Vec::new(),
      shared_css: String::new(),
      document_css_injection: None,
    }
  }
}

/// Captured details for a `<foreignObject>` subtree that should be rendered via the HTML
/// pipeline and injected back into the SVG during painting.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignObjectInfo {
  /// Placeholder token emitted into the serialized SVG.
  pub placeholder: String,
  /// Original attributes (x/y/width/height/etc.).
  pub attributes: Vec<(String, String)>,
  pub x: f32,
  pub y: f32,
  pub width: f32,
  pub height: f32,
  pub opacity: f32,
  pub background: Option<Rgba>,
  pub html: String,
  pub style: Arc<ComputedStyle>,
  pub overflow_x: Overflow,
  pub overflow_y: Overflow,
}

/// Types of replaced elements
#[derive(Debug, Clone, PartialEq)]
pub enum ReplacedType {
  /// Image element
  Image {
    /// Source URL or data URI
    src: String,
    /// Alternative text for fallback rendering
    alt: Option<String>,
    /// Parsed `crossorigin` attribute, used to drive CORS-mode image requests.
    crossorigin: CrossOriginAttribute,
    /// Optional parsed `referrerpolicy` attribute.
    referrer_policy: Option<ReferrerPolicy>,
    /// Srcset candidates for density-aware selection
    srcset: Vec<SrcsetCandidate>,
    /// Sizes attribute values for width-descriptor selection
    sizes: Option<SizesList>,
    /// Ordered `<source>` elements from a parent `<picture>` (if any).
    picture_sources: Vec<PictureSource>,
  },

  /// Video element
  Video {
    /// Source URL
    src: String,
    /// Poster image URL or data URI
    poster: Option<String>,
  },
  /// Audio element
  Audio {
    /// Source URL
    src: String,
  },

  /// Canvas element
  Canvas,

  /// SVG embedded content
  Svg {
    /// SVG content (inline or reference)
    content: SvgContent,
  },

  /// Iframe (nested browsing context)
  Iframe {
    /// Source URL
    src: String,
    /// Inline HTML content overriding src
    srcdoc: Option<String>,
    /// Optional parsed `referrerpolicy` attribute.
    referrer_policy: Option<ReferrerPolicy>,
  },

  /// `<embed>` element
  Embed {
    /// Source URL
    src: String,
  },

  /// `<object>` element
  Object {
    /// Data URL
    data: String,
  },

  /// MathML content
  Math(MathReplaced),

  /// Native form controls (input/select/textarea/button)
  FormControl(FormControl),
}

impl ReplacedType {
  /// Returns a placeholder label for non-image replaced content.
  pub fn placeholder_label(&self) -> Option<&str> {
    match self {
      ReplacedType::Video { .. } => Some("video"),
      ReplacedType::Audio { .. } => Some("audio"),
      ReplacedType::Iframe { srcdoc, .. } => srcdoc.as_deref().or(Some("iframe")),
      ReplacedType::Canvas => Some("canvas"),
      ReplacedType::Embed { .. } => Some("embed"),
      ReplacedType::Object { .. } => Some("object"),
      ReplacedType::FormControl(_) => Some("control"),
      ReplacedType::Math(_) => Some("math"),
      _ => None,
    }
  }
}

impl SizesList {
  pub fn evaluate(
    &self,
    media_ctx: &crate::style::media::MediaContext,
    viewport: crate::geometry::Size,
    font_size: f32,
    root_font_size: f32,
  ) -> f32 {
    // https://html.spec.whatwg.org/multipage/images.html#parse-a-sizes-attribute
    //
    // The HTML `sizes` attribute algorithm returns the first `<source-size>` entry whose
    // `<media-condition>` evaluates to true. If no entry matches (including when all entries have
    // media conditions that evaluate to false), the fallback is `100vw`.
    for entry in &self.entries {
      let media_matches = entry
        .media
        .as_ref()
        .map(|q| media_ctx.evaluate_list(q))
        .unwrap_or(true);
      if !media_matches {
        continue;
      }

      if let Some(resolved) = entry
        .length
        .resolve(viewport, font_size, root_font_size)
        .filter(|v| v.is_finite())
      {
        return resolved.max(0.0);
      }
    }

    // Spec fallback: 100vw when the list is empty or when all entries are invalid/non-matching.
    resolve_sizes_length_value(
      crate::style::values::Length::new(100.0, crate::style::values::LengthUnit::Vw),
      viewport,
      font_size,
      root_font_size,
    )
    .unwrap_or(viewport.width)
  }
}

impl SizesLength {
  fn resolve(
    &self,
    viewport: crate::geometry::Size,
    font_size: f32,
    root_font_size: f32,
  ) -> Option<f32> {
    match self {
      SizesLength::Length(length) => {
        resolve_sizes_length_value(*length, viewport, font_size, root_font_size)
      }
      SizesLength::Min(values) => {
        let mut iter = values.iter();
        let first = iter
          .next()
          .and_then(|value| value.resolve(viewport, font_size, root_font_size))?;
        if !first.is_finite() {
          return None;
        }
        let mut min = first;
        for value in iter {
          let resolved = value.resolve(viewport, font_size, root_font_size)?;
          if !resolved.is_finite() {
            return None;
          }
          if resolved < min {
            min = resolved;
          }
        }
        Some(min)
      }
      SizesLength::Max(values) => {
        let mut iter = values.iter();
        let first = iter
          .next()
          .and_then(|value| value.resolve(viewport, font_size, root_font_size))?;
        if !first.is_finite() {
          return None;
        }
        let mut max = first;
        for value in iter {
          let resolved = value.resolve(viewport, font_size, root_font_size)?;
          if !resolved.is_finite() {
            return None;
          }
          if resolved > max {
            max = resolved;
          }
        }
        Some(max)
      }
      SizesLength::Clamp {
        min,
        preferred,
        max,
      } => {
        let min_value = min.resolve(viewport, font_size, root_font_size)?;
        let preferred_value = preferred.resolve(viewport, font_size, root_font_size)?;
        let max_value = max.resolve(viewport, font_size, root_font_size)?;
        if !min_value.is_finite() || !preferred_value.is_finite() || !max_value.is_finite() {
          return None;
        }
        let upper = if max_value < min_value {
          min_value
        } else {
          max_value
        };
        Some(preferred_value.max(min_value).min(upper))
      }
      SizesLength::Add(left, right) => {
        let left_value = left.resolve(viewport, font_size, root_font_size)?;
        let right_value = right.resolve(viewport, font_size, root_font_size)?;
        let sum = left_value + right_value;
        sum.is_finite().then_some(sum)
      }
      SizesLength::Sub(left, right) => {
        let left_value = left.resolve(viewport, font_size, root_font_size)?;
        let right_value = right.resolve(viewport, font_size, root_font_size)?;
        let diff = left_value - right_value;
        diff.is_finite().then_some(diff)
      }
    }
  }
}

fn resolve_sizes_length_value(
  length: crate::style::values::Length,
  viewport: crate::geometry::Size,
  font_size: f32,
  root_font_size: f32,
) -> Option<f32> {
  use crate::style::values::LengthUnit;
  match length.unit {
    LengthUnit::Percent => length.resolve_against(viewport.width),
    LengthUnit::Vw
    | LengthUnit::Vh
    | LengthUnit::Vmin
    | LengthUnit::Vmax
    | LengthUnit::Dvw
    | LengthUnit::Dvh
    | LengthUnit::Dvmin
    | LengthUnit::Dvmax => length.resolve_with_viewport(viewport.width, viewport.height),
    LengthUnit::Em => Some(font_size * length.value),
    LengthUnit::Rem => Some(root_font_size * length.value),
    LengthUnit::Ex | LengthUnit::Ch => Some(font_size * length.value * 0.5),
    LengthUnit::Calc => length.resolve_with_context(
      Some(viewport.width),
      viewport.width,
      viewport.height,
      font_size,
      root_font_size,
    ),
    _ if length.unit.is_absolute() => Some(length.to_px()),
    // Unsupported units in sizes make the entry invalid; caller will fall back.
    _ => None,
  }
}

/// An anonymous box generated by the layout algorithm
///
/// Anonymous boxes don't correspond to DOM elements. They're inserted
/// to satisfy CSS layout rules.
///
/// Example: When a block container has both block and inline children,
/// anonymous block boxes wrap the inline children.
///
/// Reference: CSS 2.1 Section 9.2.1.1 (Anonymous block boxes)
#[derive(Debug, Clone)]
pub struct AnonymousBox {
  /// What kind of anonymous box?
  pub anonymous_type: AnonymousType,
}

/// Types of anonymous boxes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonymousType {
  /// Anonymous block box
  ///
  /// Generated when a block container has mixed inline/block children.
  Block,

  /// Anonymous inline box
  ///
  /// Generated to wrap text nodes that aren't in explicit inline elements.
  Inline,

  /// Anonymous table wrapper box
  ///
  /// Generated around tables to contain captions.
  TableWrapper,

  /// Anonymous table row group box (tbody)
  ///
  /// Generated when rows aren't in explicit row groups.
  TableRowGroup,

  /// Anonymous table row box
  ///
  /// Generated when table cells aren't in explicit rows.
  TableRow,

  /// Anonymous table cell box
  ///
  /// Generated when content isn't in explicit cells.
  TableCell,
}

/// Always-present HTML table metadata for a table cell box.
///
/// Note: This is **not** debug-only. Table layout must not depend on `DebugInfo`, since debug info
/// is disabled by default in `--release` builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableCellSpan {
  pub colspan: u16,
  pub rowspan: u16,
}

#[inline]
fn clamp_table_span(span: usize) -> u16 {
  // Span attributes must be positive integers. Clamp to at least 1 and avoid overflow when the
  // source contains absurdly large values.
  span.max(1).min(u16::MAX as usize) as u16
}

/// Different types of boxes in the box tree
///
/// This enum discriminates between the different kinds of CSS boxes.
/// Each variant contains type-specific data.
#[derive(Debug, Clone)]
pub enum BoxType {
  /// Block-level box (div, p, h1, etc.)
  Block(BlockBox),

  /// Inline-level box (span, a, em, etc.)
  Inline(InlineBox),

  /// Forced line break (`<br>`)
  LineBreak(LineBreakBox),

  /// Text box (actual text content)
  Text(TextBox),

  /// List marker box
  Marker(MarkerBox),

  /// Replaced element (img, video, canvas, etc.)
  Replaced(ReplacedBox),

  /// Anonymous box (generated by layout algorithm)
  Anonymous(AnonymousBox),
}

impl BoxType {
  /// Returns true if this box type is block-level
  pub fn is_block_level(&self) -> bool {
    match self {
      BoxType::Block(_) | BoxType::Replaced(_) => true,
      BoxType::Anonymous(anon) => matches!(
        anon.anonymous_type,
        AnonymousType::Block
          | AnonymousType::TableWrapper
          | AnonymousType::TableRowGroup
          | AnonymousType::TableRow
          | AnonymousType::TableCell
      ),
      _ => false,
    }
  }

  /// Returns true if this box type is inline-level
  pub fn is_inline_level(&self) -> bool {
    match self {
      BoxType::Inline(_) | BoxType::LineBreak(_) | BoxType::Text(_) | BoxType::Marker(_) => true,
      BoxType::Anonymous(anon) => matches!(anon.anonymous_type, AnonymousType::Inline),
      _ => false,
    }
  }

  /// Returns true if this is a text box
  pub fn is_text(&self) -> bool {
    matches!(self, BoxType::Text(_) | BoxType::Marker(_))
  }

  /// Returns true if this is a list marker box
  pub fn is_marker(&self) -> bool {
    matches!(self, BoxType::Marker(_))
  }

  /// Returns true if this is a replaced element
  pub fn is_replaced(&self) -> bool {
    matches!(self, BoxType::Replaced(_))
  }

  /// Returns true if this is an anonymous box
  pub fn is_anonymous(&self) -> bool {
    matches!(self, BoxType::Anonymous(_))
  }

  /// Gets the formatting context this box establishes (if any)
  pub fn formatting_context(&self) -> Option<FormattingContextType> {
    match self {
      BoxType::Block(block) => Some(block.formatting_context),
      BoxType::Inline(inline) => inline.formatting_context,
      BoxType::Replaced(_) => Some(FormattingContextType::Block),
      _ => None,
    }
  }
}

impl fmt::Display for BoxType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      BoxType::Block(_) => write!(f, "Block"),
      BoxType::Inline(_) => write!(f, "Inline"),
      BoxType::LineBreak(_) => write!(f, "LineBreak"),
      BoxType::Text(_) => write!(f, "Text"),
      BoxType::Marker(_) => write!(f, "Marker"),
      BoxType::Replaced(_) => write!(f, "Replaced"),
      BoxType::Anonymous(anon) => match anon.anonymous_type {
        AnonymousType::Block => write!(f, "AnonymousBlock"),
        AnonymousType::Inline => write!(f, "AnonymousInline"),
        AnonymousType::TableWrapper => write!(f, "AnonymousTableWrapper"),
        AnonymousType::TableRowGroup => write!(f, "AnonymousTableRowGroup"),
        AnonymousType::TableRow => write!(f, "AnonymousTableRow"),
        AnonymousType::TableCell => write!(f, "AnonymousTableCell"),
      },
    }
  }
}

/// A single box in the box tree
///
/// Represents a CSS box (could be element, text, anonymous, etc.)
///
/// # Important Properties
///
/// - **Immutable**: Once created, a BoxNode doesn't change
/// - **No Position**: Boxes don't know where they'll be positioned (that's fragments)
/// - **Shared Styles**: ComputedStyle is Arc-ed and shared with fragments
/// - **Recursive**: Children form a tree
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use fastrender::{BoxNode, FormattingContextType};
/// use fastrender::ComputedStyle;
///
/// let style = Arc::new(ComputedStyle::default());
/// let box_node = BoxNode::new_block(
///     style,
///     FormattingContextType::Block,
///     vec![],
/// );
///
/// assert!(box_node.is_block_level());
/// ```
#[derive(Debug, Clone)]
pub struct BoxNode {
  /// Computed style for this box (shared with fragments)
  ///
  /// Using Arc because:
  /// - Shared between box and its fragments
  /// - Immutable after computation
  /// - Reduces memory usage for cloned trees
  pub style: Arc<ComputedStyle>,
  /// Optional starting style snapshot for transitions.
  pub starting_style: Option<Arc<ComputedStyle>>,

  /// What kind of box is this?
  pub box_type: BoxType,

  /// Child boxes in document order
  pub children: Vec<BoxNode>,

  /// Footnote body box tree for `float: footnote` call markers.
  ///
  /// When present, this `BoxNode` represents a `::footnote-call` pseudo-element inserted in the
  /// main flow, while the stored subtree is laid out separately in the per-page footnote area.
  pub footnote_body: Option<Box<BoxNode>>,

  /// Unique identifier for caching and debugging
  pub id: usize,

  /// Debug information (element name, class, id)
  ///
  /// Optional - only populated in debug builds or with dev tools enabled.
  ///
  /// Do not rely on this for layout semantics; `DebugInfo` is typically disabled in `--release`
  /// builds. Semantic metadata lives on always-present fields such as `generated_pseudo` and the
  /// HTML table span metadata.
  pub debug_info: Option<DebugInfo>,

  /// Styled node identifier that produced this box (pre-order traversal id).
  pub styled_node_id: Option<usize>,

  /// Generated pseudo-element kind for this box, if any.
  ///
  /// This is semantic metadata used to distinguish generated boxes (e.g. `::before`/`::after`/`::backdrop`)
  /// from their originating element without relying on `debug_info`, which is optional in
  /// release builds.
  pub generated_pseudo: Option<GeneratedPseudoElement>,

  /// HTML table cell span metadata (for `<td>` / `<th>`), if applicable.
  pub table_cell_span: Option<TableCellSpan>,

  /// HTML table column / column group span metadata (for `<col>` / `<colgroup>`), if applicable.
  pub table_column_span: Option<u16>,

  /// Optional computed style overrides for `::first-line`.
  pub first_line_style: Option<Arc<ComputedStyle>>,

  /// Optional computed style overrides for `::first-letter`.
  pub first_letter_style: Option<Arc<ComputedStyle>>,
}

/// Generated pseudo-elements that create their own boxes in the box tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratedPseudoElement {
  Before,
  After,
  Backdrop,
  FootnoteCall,
  FootnoteMarker,
}

impl Drop for BoxNode {
  fn drop(&mut self) {
    // Dropping a deeply-nested `BoxNode` tree via Rust's default recursive drop can overflow the
    // stack (e.g. degenerate 100k-depth trees). Drain children iteratively so each node is dropped
    // with an empty `children` vec.
    if self.children.is_empty() && self.footnote_body.is_none() {
      return;
    }

    let mut stack: Vec<BoxNode> = std::mem::take(&mut self.children);
    if let Some(body) = self.footnote_body.take() {
      stack.push(*body);
    }
    while let Some(mut node) = stack.pop() {
      if let Some(body) = node.footnote_body.take() {
        stack.push(*body);
      }
      stack.append(&mut node.children);
      // `node` is dropped here with an empty `children` vec, so this `Drop` implementation becomes
      // a cheap no-op for all non-root nodes in the iterative drain.
    }
  }
}

impl BoxNode {
  /// Creates a new block box
  ///
  /// # Examples
  ///
  /// ```
  /// use std::sync::Arc;
  /// use fastrender::{BoxNode, FormattingContextType};
  /// use fastrender::ComputedStyle;
  ///
  /// let style = Arc::new(ComputedStyle::default());
  /// let box_node = BoxNode::new_block(
  ///     style,
  ///     FormattingContextType::Block,
  ///     vec![],
  /// );
  ///
  /// assert!(box_node.is_block_level());
  /// ```
  pub fn new_block(
    style: Arc<ComputedStyle>,
    fc: FormattingContextType,
    children: Vec<BoxNode>,
  ) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Block(BlockBox {
        formatting_context: fc,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new inline box
  pub fn new_inline(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Inline(InlineBox {
        formatting_context: None,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new forced line break box (`<br>`)
  pub fn new_line_break(style: Arc<ComputedStyle>) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::LineBreak(LineBreakBox),
      children: Vec::new(),
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new inline-block box
  pub fn new_inline_block(
    style: Arc<ComputedStyle>,
    fc: FormattingContextType,
    children: Vec<BoxNode>,
  ) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Inline(InlineBox {
        formatting_context: Some(fc),
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new text box
  pub fn new_text(style: Arc<ComputedStyle>, text: String) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Text(TextBox { text }),
      children: Vec::new(),
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new list marker box
  pub fn new_marker(style: Arc<ComputedStyle>, content: MarkerContent) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Marker(MarkerBox { content }),
      children: Vec::new(),
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates a new replaced box
  pub fn new_replaced(
    style: Arc<ComputedStyle>,
    replaced_type: ReplacedType,
    intrinsic_size: Option<Size>,
    aspect_ratio: Option<f32>,
  ) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Replaced(ReplacedBox {
        replaced_type,
        intrinsic_size,
        aspect_ratio,
        no_intrinsic_ratio: false,
      }),
      children: Vec::new(),
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates an anonymous block box
  pub fn new_anonymous_block(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::Block,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates an anonymous inline box
  pub fn new_anonymous_inline(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> Self {
    Self {
      style,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::Inline,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Adds debug information
  ///
  /// This is a builder-style method for convenience.
  pub fn with_debug_info(mut self, info: DebugInfo) -> Self {
    self.debug_info = Some(info);
    self
  }

  /// Sets HTML table cell span metadata (`colspan`/`rowspan`).
  pub fn with_table_cell_spans(mut self, colspan: usize, rowspan: usize) -> Self {
    let colspan = clamp_table_span(colspan);
    let rowspan = clamp_table_span(rowspan);
    self.table_cell_span = Some(TableCellSpan { colspan, rowspan });
    self
  }

  /// Sets HTML `<col>/<colgroup span=...>` metadata.
  pub fn with_table_column_span(mut self, span: usize) -> Self {
    self.table_column_span = Some(clamp_table_span(span));
    self
  }

  /// Sets semantic pseudo-element identity for generated boxes.
  pub fn with_generated_pseudo(mut self, pseudo: GeneratedPseudoElement) -> Self {
    self.generated_pseudo = Some(pseudo);
    self
  }

  /// Unique identifier for this box within the box tree.
  pub fn id(&self) -> usize {
    self.id
  }

  // Type query methods

  /// Returns true if this is a block-level box
  ///
  /// Block-level boxes participate in block formatting context.
  pub fn is_block_level(&self) -> bool {
    if matches!(self.box_type, BoxType::Replaced(_)) {
      return !self.style.display.is_inline_level();
    }

    self.box_type.is_block_level()
  }

  /// Returns true if this is an inline-level box
  ///
  /// Inline-level boxes participate in inline formatting context.
  pub fn is_inline_level(&self) -> bool {
    if matches!(self.box_type, BoxType::Replaced(_)) {
      return self.style.display.is_inline_level();
    }

    self.box_type.is_inline_level()
  }

  /// Returns true if this is a text box
  pub fn is_text(&self) -> bool {
    self.box_type.is_text()
  }

  /// Returns true if this is a replaced element
  pub fn is_replaced(&self) -> bool {
    self.box_type.is_replaced()
  }

  /// Returns true if this is an anonymous box
  pub fn is_anonymous(&self) -> bool {
    self.box_type.is_anonymous()
  }

  /// Gets the formatting context this box establishes (if any)
  ///
  /// Returns None for inline and text boxes that don't establish contexts.
  pub fn formatting_context(&self) -> Option<FormattingContextType> {
    self.box_type.formatting_context()
  }

  /// Returns true if this box is a block container
  ///
  /// Block containers can contain block-level children and establish
  /// a block formatting context (or participate in one).
  pub fn is_block_container(&self) -> bool {
    match &self.box_type {
      BoxType::Block(_) => true,
      BoxType::Inline(inline) => inline.formatting_context.is_some(), // inline-block
      BoxType::Anonymous(anon) => matches!(
        anon.anonymous_type,
        AnonymousType::Block | AnonymousType::TableCell
      ),
      _ => false,
    }
  }

  /// Returns true if this box is an inline container
  ///
  /// Inline containers contain inline-level children and participate
  /// in inline formatting context.
  pub fn is_inline_container(&self) -> bool {
    matches!(&self.box_type, BoxType::Inline(_))
  }

  /// Returns true if this box generates a formatting context
  ///
  /// Boxes that generate formatting contexts are independent layout roots.
  /// Their internal layout doesn't affect outside, and vice versa.
  pub fn generates_formatting_context(&self) -> bool {
    self.formatting_context().is_some()
  }

  /// Table cell colspan (`<td colspan>` / `<th colspan>`).
  ///
  /// Defaults to `1` when not present/applicable.
  pub fn table_colspan(&self) -> usize {
    match self.table_cell_span {
      Some(span) if span.colspan > 0 => span.colspan as usize,
      _ => 1,
    }
  }

  /// Table cell rowspan (`<td rowspan>` / `<th rowspan>`).
  ///
  /// Defaults to `1` when not present/applicable.
  pub fn table_rowspan(&self) -> usize {
    match self.table_cell_span {
      Some(span) if span.rowspan > 0 => span.rowspan as usize,
      _ => 1,
    }
  }

  /// Table column span (`<col span>` / `<colgroup span>`).
  ///
  /// Defaults to `1` when not present/applicable.
  pub fn table_column_span(&self) -> usize {
    match self.table_column_span {
      Some(span) if span > 0 => span as usize,
      _ => 1,
    }
  }

  /// Returns true if this is a table-internal box
  ///
  /// Table-internal boxes participate in table layout algorithms.
  pub fn is_table_internal(&self) -> bool {
    match &self.box_type {
      BoxType::Anonymous(anon) => matches!(
        anon.anonymous_type,
        AnonymousType::TableWrapper
          | AnonymousType::TableRowGroup
          | AnonymousType::TableRow
          | AnonymousType::TableCell
      ),
      _ => false,
    }
  }

  /// Gets text content if this is a text box
  pub fn text(&self) -> Option<&str> {
    match &self.box_type {
      BoxType::Text(text_box) => Some(&text_box.text),
      BoxType::Marker(marker_box) => match &marker_box.content {
        MarkerContent::Text(text) => Some(text.as_str()),
        MarkerContent::Image(_) => None,
      },
      _ => None,
    }
  }

  /// Returns the number of children
  pub fn child_count(&self) -> usize {
    self.children.len()
  }

  /// Returns an iterator over children
  pub fn children_iter(&self) -> impl Iterator<Item = &BoxNode> {
    self.children.iter()
  }
}

/// A tree of CSS boxes
///
/// The box tree is generated from styled DOM and consumed by layout algorithms.
/// It's immutable after construction.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use fastrender::{BoxTree, BoxNode, FormattingContextType};
/// use fastrender::ComputedStyle;
///
/// let style = Arc::new(ComputedStyle::default());
/// let root = BoxNode::new_block(
///     style,
///     FormattingContextType::Block,
///     vec![],
/// );
///
/// let tree = BoxTree::new(root);
/// assert!(tree.root.is_block_level());
/// ```
#[derive(Debug, Clone)]
pub struct BoxTree {
  /// The root box (typically the root element's principal box)
  pub root: BoxNode,
}

fn assign_box_ids(root: &mut BoxNode, next_id: &mut usize) {
  let mut stack: Vec<*mut BoxNode> = vec![root as *mut _];
  while let Some(node_ptr) = stack.pop() {
    // SAFETY: We only push pointers to nodes owned by `root`, and we never move the
    // underlying `BoxNode`s while this traversal runs. Each pointer is popped and
    // used to assign a unique `id` before pushing its children.
    unsafe {
      let node = &mut *node_ptr;
      node.id = *next_id;
      *next_id += 1;
      // Preserve the previous recursive pre-order traversal by visiting children
      // from left-to-right.
      if let Some(body) = node.footnote_body.as_deref_mut() {
        stack.push(body as *mut _);
      }
      for child in node.children.iter_mut().rev() {
        stack.push(child as *mut _);
      }
    }
  }
}

impl BoxTree {
  /// Creates a new box tree with the given root
  pub fn new(root: BoxNode) -> Self {
    let mut root = root;
    let mut next_id = 1;
    assign_box_ids(&mut root, &mut next_id);
    Self { root }
  }

  /// Counts total boxes in the tree (including root)
  pub fn count_boxes(&self) -> usize {
    fn count_recursive(node: &BoxNode) -> usize {
      1 + node.children.iter().map(count_recursive).sum::<usize>()
        + node
          .footnote_body
          .as_deref()
          .map(count_recursive)
          .unwrap_or(0)
    }
    count_recursive(&self.root)
  }

  /// Counts text boxes in the tree
  pub fn count_text_boxes(&self) -> usize {
    fn count_recursive(node: &BoxNode) -> usize {
      let self_count = usize::from(node.is_text());
      self_count
        + node.children.iter().map(count_recursive).sum::<usize>()
        + node
          .footnote_body
          .as_deref()
          .map(count_recursive)
          .unwrap_or(0)
    }
    count_recursive(&self.root)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Size;
  use crate::style::display::FormattingContextType;
  use crate::style::media::MediaContext;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;

  fn default_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  #[test]
  fn non_ascii_whitespace_select_snapshot_label_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let control = FormControlKind::Select(SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(vec![SelectItem::Option {
        node_id: 1,
        label: nbsp.to_string(),
        value: "fallback".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      }]),
      selected: vec![0],
    });

    let snapshot = control.snapshot_label();
    assert!(
      snapshot.contains(&format!("{:?}", nbsp)),
      "NBSP label must not be treated as whitespace in select snapshots: {snapshot}"
    );
  }

  #[test]
  fn test_create_block_box() {
    let box_node = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    assert!(box_node.is_block_level());
    assert!(!box_node.is_inline_level());
    assert_eq!(
      box_node.formatting_context(),
      Some(FormattingContextType::Block)
    );
  }

  #[test]
  fn test_create_inline_box() {
    let box_node = BoxNode::new_inline(default_style(), vec![]);

    assert!(box_node.is_inline_level());
    assert!(!box_node.is_block_level());
    assert_eq!(box_node.formatting_context(), None);
  }

  #[test]
  fn test_create_text_box() {
    let box_node = BoxNode::new_text(default_style(), "Hello".to_string());

    assert!(box_node.is_inline_level());
    assert!(box_node.is_text());
    assert_eq!(box_node.text(), Some("Hello"));
    assert_eq!(box_node.children.len(), 0);
  }

  #[test]
  fn test_create_replaced_box() {
    let box_node = BoxNode::new_replaced(
      default_style(),
      ReplacedType::Image {
        src: "image.png".to_string(),
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
      Some(Size::new(100.0, 50.0)),
      Some(2.0),
    );

    assert!(box_node.is_replaced());
    assert!(box_node.is_inline_level());
    assert!(!box_node.is_block_level());

    let mut block_style = ComputedStyle::default();
    block_style.display = crate::Display::Block;
    let block_node = BoxNode::new_replaced(
      Arc::new(block_style),
      ReplacedType::Image {
        src: "image.png".to_string(),
        alt: None,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
      Some(Size::new(100.0, 50.0)),
      Some(2.0),
    );
    assert!(block_node.is_block_level());
    assert!(!block_node.is_inline_level());
  }

  #[test]
  fn test_create_inline_block() {
    let box_node = BoxNode::new_inline_block(default_style(), FormattingContextType::Block, vec![]);

    assert!(box_node.is_inline_level());
    assert_eq!(
      box_node.formatting_context(),
      Some(FormattingContextType::Block)
    );
  }

  #[test]
  fn test_box_hierarchy() {
    let text1 = BoxNode::new_text(default_style(), "Text 1".to_string());
    let text2 = BoxNode::new_text(default_style(), "Text 2".to_string());

    let inline_box = BoxNode::new_inline(default_style(), vec![text1, text2]);

    let block_box = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline_box],
    );

    assert_eq!(block_box.children.len(), 1);
    assert_eq!(block_box.children[0].children.len(), 2);
    assert_eq!(block_box.child_count(), 1);
  }

  #[test]
  fn test_debug_info() {
    let debug_info = DebugInfo::new(
      Some("div".to_string()),
      Some("header".to_string()),
      vec!["navbar".to_string(), "sticky".to_string()],
    );

    assert_eq!(debug_info.to_selector(), "div#header.navbar.sticky");

    let box_node = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![])
      .with_debug_info(debug_info);

    assert!(box_node.debug_info.is_some());
  }

  #[test]
  fn test_box_tree() {
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![
        BoxNode::new_text(default_style(), "Text".to_string()),
        BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]),
      ],
    );

    let tree = BoxTree::new(root);

    assert_eq!(tree.count_boxes(), 3); // root + text + block
    assert_eq!(tree.count_text_boxes(), 1);
  }

  #[test]
  fn test_anonymous_block_box() {
    let box_node = BoxNode::new_anonymous_block(default_style(), vec![]);

    assert!(box_node.is_anonymous());
    assert!(box_node.is_block_level());
  }

  #[test]
  fn test_children_iterator() {
    let text1 = BoxNode::new_text(default_style(), "Text 1".to_string());
    let text2 = BoxNode::new_text(default_style(), "Text 2".to_string());
    let box_node = BoxNode::new_inline(default_style(), vec![text1, text2]);

    let count = box_node.children_iter().count();
    assert_eq!(count, 2);
  }

  #[test]
  fn test_is_block_container() {
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline_block =
      BoxNode::new_inline_block(default_style(), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(default_style(), vec![]);
    let text = BoxNode::new_text(default_style(), "text".to_string());

    assert!(block.is_block_container());
    assert!(inline_block.is_block_container());
    assert!(!inline.is_block_container());
    assert!(!text.is_block_container());
  }

  #[test]
  fn test_is_inline_container() {
    let inline = BoxNode::new_inline(default_style(), vec![]);
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text = BoxNode::new_text(default_style(), "text".to_string());

    assert!(inline.is_inline_container());
    assert!(!block.is_inline_container());
    assert!(!text.is_inline_container());
  }

  #[test]
  fn test_generates_formatting_context() {
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline_block =
      BoxNode::new_inline_block(default_style(), FormattingContextType::Flex, vec![]);
    let inline = BoxNode::new_inline(default_style(), vec![]);
    let text = BoxNode::new_text(default_style(), "text".to_string());
    let replaced = BoxNode::new_replaced(
      default_style(),
      ReplacedType::Image {
        src: "img.png".to_string(),
        alt: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
      },
      None,
      None,
    );

    assert!(block.generates_formatting_context());
    assert!(inline_block.generates_formatting_context());
    assert!(replaced.generates_formatting_context());
    assert!(!inline.generates_formatting_context());
    assert!(!text.generates_formatting_context());
  }

  #[test]
  fn test_is_table_internal() {
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let anon_block = BoxNode::new_anonymous_block(default_style(), vec![]);

    assert!(!block.is_table_internal());
    assert!(!anon_block.is_table_internal());
  }

  #[test]
  fn test_block_container_with_flex() {
    let flex_block = BoxNode::new_block(default_style(), FormattingContextType::Flex, vec![]);

    assert!(flex_block.is_block_container());
    assert!(flex_block.generates_formatting_context());
    assert_eq!(
      flex_block.formatting_context(),
      Some(FormattingContextType::Flex)
    );
  }

  #[test]
  fn test_block_container_with_grid() {
    let grid_block = BoxNode::new_block(default_style(), FormattingContextType::Grid, vec![]);

    assert!(grid_block.is_block_container());
    assert!(grid_block.generates_formatting_context());
    assert_eq!(
      grid_block.formatting_context(),
      Some(FormattingContextType::Grid)
    );
  }

  #[test]
  fn test_inline_block_formatting_context() {
    let inline_block_flex =
      BoxNode::new_inline_block(default_style(), FormattingContextType::Flex, vec![]);
    let inline_block_grid =
      BoxNode::new_inline_block(default_style(), FormattingContextType::Grid, vec![]);

    assert!(inline_block_flex.is_block_container());
    assert!(inline_block_flex.generates_formatting_context());
    assert!(inline_block_flex.is_inline_level());
    assert_eq!(
      inline_block_flex.formatting_context(),
      Some(FormattingContextType::Flex)
    );

    assert!(inline_block_grid.is_block_container());
    assert!(inline_block_grid.generates_formatting_context());
    assert!(inline_block_grid.is_inline_level());
    assert_eq!(
      inline_block_grid.formatting_context(),
      Some(FormattingContextType::Grid)
    );
  }

  #[test]
  fn test_box_type_display() {
    let block = BoxType::Block(BlockBox {
      formatting_context: FormattingContextType::Block,
    });
    let inline = BoxType::Inline(InlineBox {
      formatting_context: None,
    });
    let text = BoxType::Text(TextBox {
      text: "hello".to_string(),
    });
    let anon_block = BoxType::Anonymous(AnonymousBox {
      anonymous_type: AnonymousType::Block,
    });
    let anon_inline = BoxType::Anonymous(AnonymousBox {
      anonymous_type: AnonymousType::Inline,
    });

    assert_eq!(format!("{}", block), "Block");
    assert_eq!(format!("{}", inline), "Inline");
    assert_eq!(format!("{}", text), "Text");
    assert_eq!(format!("{}", anon_block), "AnonymousBlock");
    assert_eq!(format!("{}", anon_inline), "AnonymousInline");
  }

  #[test]
  fn sizes_list_resolves_rem_against_root_font_size() {
    let list = SizesList {
      entries: vec![SizesEntry {
        media: None,
        length: Length::new(10.0, LengthUnit::Rem).into(),
      }],
    };

    let viewport = Size::new(800.0, 600.0);
    let media_ctx = MediaContext::screen(viewport.width, viewport.height);

    // 10rem should resolve against the root font size, not the element font size.
    assert_eq!(list.evaluate(&media_ctx, viewport, 10.0, 20.0), 200.0);
  }

  #[test]
  fn sizes_list_falls_back_to_100vw_when_no_media_condition_matches() {
    // https://html.spec.whatwg.org/multipage/images.html#parse-a-sizes-attribute
    //
    // If none of the entries' media conditions evaluate to true, the fallback is 100vw (viewport
    // width).
    let list = SizesList {
      entries: vec![SizesEntry {
        media: Some(MediaQuery::parse_list("(min-width: 1000px)").unwrap()),
        length: Length::px(50.0).into(),
      }],
    };
    let viewport = Size::new(400.0, 300.0);
    let media_ctx = MediaContext::screen(viewport.width, viewport.height);
    assert!((list.evaluate(&media_ctx, viewport, 16.0, 16.0) - viewport.width).abs() < 0.001);
  }

  #[test]
  fn image_source_prefers_width_descriptor_with_sizes() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::Width(100),
        },
        SrcsetCandidate {
          url: "300w".to_string(),
          descriptor: SrcsetDescriptor::Width(300),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::new(50.0, LengthUnit::Vw).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "300w");
  }

  #[test]
  fn sizes_calc_expression_controls_width_descriptor_selection() {
    use crate::style::values::CalcLength;

    let calc = CalcLength::single(LengthUnit::Vw, 50.0)
      .add_scaled(&CalcLength::single(LengthUnit::Px, 20.0), -1.0)
      .expect("calc length fits term budget");
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "200w".to_string(),
          descriptor: SrcsetDescriptor::Width(200),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          // Viewport=200px => calc(50vw - 20px) = 80px slot width.
          length: Length::calc(calc).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "200w",
      "calc() sizes should reduce slot width and keep the smaller width candidate"
    );
  }

  #[test]
  fn sizes_calc_with_nested_min_function_controls_width_descriptor_selection() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "200w".to_string(),
          descriptor: SrcsetDescriptor::Width(200),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          // Viewport=200px => calc(min(100vw, 80px) - 20px) = 60px slot width.
          length: SizesLength::Sub(
            Box::new(SizesLength::Min(vec![
              Length::new(100.0, LengthUnit::Vw).into(),
              Length::px(80.0).into(),
            ])),
            Box::new(Length::px(20.0).into()),
          ),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "200w",
      "calc(min(...)) sizes should reduce slot width and keep the smaller width candidate"
    );
  }

  #[test]
  fn image_source_selects_width_height_descriptor_candidates() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 100,
            height: 50,
          },
        },
        SrcsetCandidate {
          url: "300w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 300,
            height: 150,
          },
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::new(50.0, LengthUnit::Vw).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let selected = img.selected_image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(selected.url, "300w");
    assert_eq!(
      selected.descriptor,
      Some(SrcsetDescriptor::WidthHeight {
        width: 300,
        height: 150,
      })
    );
  }

  #[test]
  fn image_source_selects_smaller_width_height_candidate_for_matching_slot() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 100,
            height: 50,
          },
        },
        SrcsetCandidate {
          url: "300w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 300,
            height: 150,
          },
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::px(100.0).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(800.0, 600.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let selected = img.selected_image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(selected.url, "100w");
    assert_eq!(
      selected.descriptor,
      Some(SrcsetDescriptor::WidthHeight {
        width: 100,
        height: 50,
      })
    );
  }

  #[test]
  fn sizes_min_function_controls_width_descriptor_selection() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "200w".to_string(),
          descriptor: SrcsetDescriptor::Width(200),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          // Viewport=200px => min(100vw, 80px) = 80px slot width.
          length: SizesLength::Min(vec![
            Length::new(100.0, LengthUnit::Vw).into(),
            Length::px(80.0).into(),
          ]),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "200w",
      "min() sizes should reduce slot width and keep the smaller width candidate"
    );
  }

  #[test]
  fn sizes_clamp_function_controls_width_descriptor_selection() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "200w".to_string(),
          descriptor: SrcsetDescriptor::Width(200),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          // Viewport=200px => clamp(0px, 100vw, 80px) = 80px slot width.
          length: SizesLength::Clamp {
            min: Box::new(Length::px(0.0).into()),
            preferred: Box::new(Length::new(100.0, LengthUnit::Vw).into()),
            max: Box::new(Length::px(80.0).into()),
          },
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "200w",
      "clamp() sizes should cap slot width and keep the smaller width candidate"
    );
  }

  #[test]
  fn image_source_prefers_width_height_descriptors_with_sizes() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 100,
            height: 50,
          },
        },
        SrcsetCandidate {
          url: "300w".to_string(),
          descriptor: SrcsetDescriptor::WidthHeight {
            width: 300,
            height: 150,
          },
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::px(100.0).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(800.0, 600.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "100w");
  }

  #[test]
  fn sizes_default_to_last_entry_when_no_media_match() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::Width(100),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![
          SizesEntry {
            media: Some(vec![crate::style::media::MediaQuery::parse(
              "(max-width: 10px)",
            )
            .unwrap()]),
            length: Length::new(50.0, LengthUnit::Vw).into(),
          },
          SizesEntry {
            media: None,
            length: Length::px(300.0).into(),
          },
        ],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(1200.0, 800.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "400w",
      "last sizes entry (300px) should drive selection"
    );
  }

  #[test]
  fn sizes_resolve_ex_ch_lengths() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::Width(100),
        },
        SrcsetCandidate {
          url: "300w".to_string(),
          descriptor: SrcsetDescriptor::Width(300),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          // 10ch at 16px font size = 80px slot width; smallest density >=1 is 100w.
          length: Length::new(10.0, LengthUnit::Ch).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "100w");
  }

  #[test]
  fn sizes_rem_uses_root_font_size() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "150w".to_string(),
          descriptor: SrcsetDescriptor::Width(150),
        },
        SrcsetCandidate {
          url: "250w".to_string(),
          descriptor: SrcsetDescriptor::Width(250),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::rem(10.0).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(500.0, 300.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(10.0),
      root_font_size: Some(20.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "250w",
      "10rem should resolve against root font size (20px) => 200px slot width"
    );
  }

  #[test]
  fn sizes_calc_rem_uses_root_font_size() {
    let calc_length =
      crate::css::properties::parse_length("calc(10rem + 1px)").expect("calc length parsed");
    assert!(matches!(calc_length.unit, LengthUnit::Calc));

    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "150w".to_string(),
          descriptor: SrcsetDescriptor::Width(150),
        },
        SrcsetCandidate {
          url: "250w".to_string(),
          descriptor: SrcsetDescriptor::Width(250),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: calc_length.into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(500.0, 300.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(10.0),
      root_font_size: Some(20.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "250w",
      "calc(10rem + 1px) should resolve rem against root font size (20px) => 201px slot width"
    );
  }

  #[test]
  fn sizes_calc_is_evaluated_for_srcset_width_selection() {
    let calc_length =
      crate::css::properties::parse_length("calc(50vw - 10px)").expect("calc length parsed");
    assert!(matches!(calc_length.unit, LengthUnit::Calc));

    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "80w".to_string(),
          descriptor: SrcsetDescriptor::Width(80),
        },
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::Width(100),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: calc_length.into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    // Viewport width 200 => 50vw = 100; minus 10px => slot width 90px.
    let viewport = Size::new(200.0, 100.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "100w");
  }

  #[test]
  fn width_descriptors_default_to_viewport_when_no_sizes_and_no_slot() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
        SrcsetCandidate {
          url: "800w".to_string(),
          descriptor: SrcsetDescriptor::Width(800),
        },
      ],
      sizes: None,
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    // With viewport width 500 and DPR 2, density candidates are 0.8 and 1.6.
    let viewport = Size::new(500.0, 300.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "800w",
      "viewport fallback should make 800w best for DPR=2"
    );
  }

  #[test]
  fn non_positive_slot_width_falls_back_to_viewport_for_width_descriptors() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "100w".to_string(),
          descriptor: SrcsetDescriptor::Width(100),
        },
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
      ],
      sizes: None,
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    // Slot width is zero (e.g., auto-sized placeholder), so selection should fall back to viewport.
    let viewport = Size::new(400.0, 300.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: Some(0.0),
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      chosen, "400w",
      "zero-width slot should use viewport width for width descriptors"
    );
  }

  #[test]
  fn image_sources_with_fallback_prioritizes_selected_then_src() {
    let img = ReplacedType::Image {
      src: "base".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "2x".to_string(),
          descriptor: SrcsetDescriptor::Density(2.0),
        },
        SrcsetCandidate {
          url: "1x".to_string(),
          descriptor: SrcsetDescriptor::Density(1.0),
        },
      ],
      sizes: None,
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let media_ctx = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    let sources = img.image_sources_with_fallback(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: Some(400.0),
      viewport: Some(Size::new(800.0, 600.0)),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      sources[0].url, "2x",
      "selected srcset candidate should lead"
    );
    assert_eq!(sources[0].density, Some(2.0));
    assert!(
      sources.iter().any(|s| s.url == "base"),
      "base src should remain available as fallback"
    );
    assert_eq!(
      sources.len(),
      2,
      "only selected + base fallback should remain"
    );
  }

  #[test]
  fn image_sources_with_fallback_trims_urls() {
    let img = ReplacedType::Image {
      src: "  base  ".to_string(),
      alt: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };
    let sources = img.image_sources_with_fallback(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: None,
      media_context: None,
      font_size: None,
      root_font_size: None,
      base_url: None,
    });
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].url, "base");
  }

  #[test]
  fn video_image_sources_with_fallback_filters_whitespace_poster() {
    let video = ReplacedType::Video {
      src: String::new(),
      poster: Some("   ".to_string()),
    };
    let sources = video.image_sources_with_fallback(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: None,
      media_context: None,
      font_size: None,
      root_font_size: None,
      base_url: None,
    });
    assert!(
      sources.is_empty(),
      "whitespace posters should not become image candidates"
    );
  }

  #[test]
  fn embed_image_sources_with_fallback_filters_fragment_only_src() {
    let embed = ReplacedType::Embed {
      src: "#".to_string(),
    };
    let sources = embed.image_sources_with_fallback(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: None,
      media_context: None,
      font_size: None,
      root_font_size: None,
      base_url: None,
    });
    assert!(
      sources.is_empty(),
      "fragment-only src values should not become image candidates"
    );
  }

  #[test]
  fn picture_source_respects_media_and_type_ordering() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![],
      sizes: None,
      picture_sources: vec![
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "skip-me".to_string(),
            descriptor: SrcsetDescriptor::Density(1.0),
          }],
          sizes: None,
          media: None,
          mime_type: Some("application/json".to_string()),
        },
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "avif-2x".to_string(),
            descriptor: SrcsetDescriptor::Density(2.0),
          }],
          sizes: None,
          media: Some(vec![crate::style::media::MediaQuery::parse(
            "(max-width: 500px)",
          )
          .unwrap()]),
          mime_type: Some("image/avif".to_string()),
        },
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "webp-2x".to_string(),
            descriptor: SrcsetDescriptor::Density(2.0),
          }],
          sizes: None,
          media: None,
          mime_type: Some("image/webp; codecs=vp8".to_string()),
        },
      ],
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let small_viewport = Size::new(400.0, 300.0);
    let small_media = MediaContext::screen(small_viewport.width, small_viewport.height)
      .with_device_pixel_ratio(2.0);
    let chosen_small = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(small_viewport),
      media_context: Some(&small_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });
    assert_eq!(chosen_small, "avif-2x");

    let large_viewport = Size::new(1200.0, 800.0);
    let large_media = MediaContext::screen(large_viewport.width, large_viewport.height)
      .with_device_pixel_ratio(2.0);
    let chosen_large = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(large_viewport),
      media_context: Some(&large_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });
    assert_eq!(chosen_large, "webp-2x");
  }

  #[test]
  fn picture_source_accepts_common_mime_aliases() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: vec![],
      sizes: None,
      picture_sources: vec![
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "jpg-source".to_string(),
            descriptor: SrcsetDescriptor::Density(1.0),
          }],
          sizes: None,
          media: None,
          mime_type: Some("image/jpg".to_string()),
        },
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "png-source".to_string(),
            descriptor: SrcsetDescriptor::Density(1.0),
          }],
          sizes: None,
          media: None,
          mime_type: Some("image/png".to_string()),
        },
      ],
    };

    let viewport = Size::new(800.0, 600.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "jpg-source");
  }

  #[test]
  fn picture_source_uses_sizes_with_width_descriptors() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![],
      sizes: None,
      picture_sources: vec![PictureSource {
        srcset: vec![
          SrcsetCandidate {
            url: "400w".to_string(),
            descriptor: SrcsetDescriptor::Width(400),
          },
          SrcsetCandidate {
            url: "800w".to_string(),
            descriptor: SrcsetDescriptor::Width(800),
          },
        ],
        sizes: Some(SizesList {
          entries: vec![SizesEntry {
            media: None,
            length: Length::new(50.0, LengthUnit::Vw).into(),
          }],
        }),
        media: None,
        mime_type: None,
      }],
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(800.0, 600.0);
    let media_ctx =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);
    let chosen = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_ctx),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen, "800w");
  }

  #[test]
  fn width_descriptors_shift_with_dpr() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "400w".to_string(),
          descriptor: SrcsetDescriptor::Width(400),
        },
        SrcsetCandidate {
          url: "800w".to_string(),
          descriptor: SrcsetDescriptor::Width(800),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![SizesEntry {
          media: None,
          length: Length::new(100.0, LengthUnit::Vw).into(),
        }],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let viewport = Size::new(400.0, 200.0);
    let media_1x =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(1.0);
    let media_2x =
      MediaContext::screen(viewport.width, viewport.height).with_device_pixel_ratio(2.0);

    let at_1x = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_1x),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });
    let at_2x = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: None,
      viewport: Some(viewport),
      media_context: Some(&media_2x),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(at_1x, "400w");
    assert_eq!(at_2x, "800w");
  }

  #[test]
  fn sizes_media_conditions_control_selection() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![
        SrcsetCandidate {
          url: "200w".to_string(),
          descriptor: SrcsetDescriptor::Width(200),
        },
        SrcsetCandidate {
          url: "800w".to_string(),
          descriptor: SrcsetDescriptor::Width(800),
        },
      ],
      sizes: Some(SizesList {
        entries: vec![
          SizesEntry {
            media: Some(vec![MediaQuery::parse("(max-width: 500px)").unwrap()]),
            length: Length::px(200.0).into(),
          },
          SizesEntry {
            media: None,
            length: Length::px(400.0).into(),
          },
        ],
      }),
      picture_sources: Vec::new(),
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let small_viewport = Size::new(300.0, 400.0);
    let small_media = MediaContext::screen(small_viewport.width, small_viewport.height)
      .with_device_pixel_ratio(1.0);
    let large_viewport = Size::new(800.0, 600.0);
    let large_media = MediaContext::screen(large_viewport.width, large_viewport.height)
      .with_device_pixel_ratio(1.0);

    let chosen_small = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(small_viewport),
      media_context: Some(&small_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });
    let chosen_large = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(large_viewport),
      media_context: Some(&large_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen_small, "200w", "media-matching sizes should apply");
    assert_eq!(
      chosen_large, "800w",
      "fallback sizes entry should drive selection when media does not match"
    );
  }

  #[test]
  fn picture_requires_media_and_type_to_match() {
    let img = ReplacedType::Image {
      src: "fallback".to_string(),
      alt: None,
      srcset: vec![],
      sizes: None,
      picture_sources: vec![
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "webp-small".to_string(),
            descriptor: SrcsetDescriptor::Density(1.0),
          }],
          sizes: None,
          media: Some(vec![MediaQuery::parse("(max-width: 500px)").unwrap()]),
          mime_type: Some("image/webp".to_string()),
        },
        PictureSource {
          srcset: vec![SrcsetCandidate {
            url: "png-large".to_string(),
            descriptor: SrcsetDescriptor::Density(1.0),
          }],
          sizes: None,
          media: Some(vec![MediaQuery::parse("(min-width: 501px)").unwrap()]),
          mime_type: Some("image/png".to_string()),
        },
      ],
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
    };

    let small_viewport = Size::new(400.0, 300.0);
    let small_media = MediaContext::screen(small_viewport.width, small_viewport.height)
      .with_device_pixel_ratio(1.0);
    let large_viewport = Size::new(800.0, 600.0);
    let large_media = MediaContext::screen(large_viewport.width, large_viewport.height)
      .with_device_pixel_ratio(1.0);

    let chosen_small = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(small_viewport),
      media_context: Some(&small_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });
    let chosen_large = img.image_source_for_context(ImageSelectionContext {
      device_pixel_ratio: 1.0,
      slot_width: None,
      viewport: Some(large_viewport),
      media_context: Some(&large_media),
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(chosen_small, "webp-small");
    assert_eq!(chosen_large, "png-large");
  }

  #[test]
  fn iframe_srcdoc_prefers_inline_html_for_placeholder() {
    let iframe = ReplacedType::Iframe {
      src: "https://example.com".to_string(),
      srcdoc: Some("hello world".to_string()),
      referrer_policy: None,
    };
    assert_eq!(iframe.placeholder_label(), Some("hello world"));

    let iframe_no_srcdoc = ReplacedType::Iframe {
      src: "https://example.com".to_string(),
      srcdoc: None,
      referrer_policy: None,
    };
    assert_eq!(iframe_no_srcdoc.placeholder_label(), Some("iframe"));
  }

  #[test]
  fn video_image_sources_prefer_poster_only() {
    let video = ReplacedType::Video {
      src: "video.mp4".to_string(),
      poster: Some("thumb.png".to_string()),
    };
    let sources = video.image_sources_with_fallback(ImageSelectionContext {
      device_pixel_ratio: 2.0,
      slot_width: Some(400.0),
      viewport: Some(Size::new(800.0, 600.0)),
      media_context: None,
      font_size: Some(16.0),
      root_font_size: Some(16.0),
      base_url: None,
    });

    assert_eq!(
      sources.iter().map(|s| s.url).collect::<Vec<_>>(),
      vec!["thumb.png"],
      "video should only expose poster for imaging"
    );
  }
}
