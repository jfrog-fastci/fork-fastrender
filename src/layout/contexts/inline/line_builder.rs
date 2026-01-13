//! Line building for inline formatting context
//!
//! This module handles the construction of line boxes from inline-level content.
//! It manages line breaking, inline item accumulation, and fragment positioning.
//!
//! # CSS Specification
//!
//! CSS 2.1 Section 9.4.2 - Inline formatting contexts:
//! <https://www.w3.org/TR/CSS21/visuren.html#inline-formatting>
//!
//! # Line Box Construction
//!
//! The line builder accumulates inline items (text runs, inline boxes, replaced elements)
//! and breaks them into lines when:
//!
//! 1. A mandatory line break occurs (newline character, `<br>`)
//! 2. Content exceeds available width and a break opportunity exists
//! 3. The inline content ends
//!
//! # Algorithm Overview
//!
//! ```text
//! For each inline item:
//!   1. Measure item width
//!   2. If item fits on current line -> add to line
//!   3. If item doesn't fit:
//!      a. Find break opportunity within item (for text)
//!      b. If no break found, check if line is empty
//!      c. If line empty, force item onto line (overflow)
//!      d. Otherwise, finalize line and start new one
//!   4. After all items, finalize last line
//! ```

use super::baseline::BaselineMetrics;
use super::baseline::LineBaselineAccumulator;
use super::baseline::VerticalAlign;
use crate::debug::runtime;
use crate::error::{RenderError, RenderStage};
use crate::geometry::Size;
use crate::layout::float_context::ClearSide;
use crate::layout::formatting_context::LayoutError;
use crate::layout::inline::float_integration::InlineFloatIntegration;
use crate::layout::inline::float_integration::LineSpaceOptions;
use crate::render_control::check_active_periodic;
use crate::style::display::Display;
use crate::style::types::Direction;
use crate::style::types::FootnotePolicy;
use crate::style::types::LineBreak;
use crate::style::types::ListStylePosition;
use crate::style::types::OverflowWrap;
use crate::style::types::TextWrap;
use crate::style::types::UnicodeBidi;
use crate::style::types::WhiteSpace;
use crate::style::types::WordBreak;
use crate::style::values::{Length, LengthUnit};
use crate::style::ComputedStyle;
use crate::text::font_loader::FontContext;
use crate::text::justify::allows_inter_character_expansion;
use crate::text::justify::InlineAxis;
use crate::text::line_break::BreakOpportunity;
use crate::text::line_break::BreakOpportunityKind;
use crate::text::line_break::BreakType;
use crate::text::pipeline::shaping_style_hash;
use crate::text::pipeline::ExplicitBidiContext;
use crate::text::pipeline::ShapedRun;
use crate::text::pipeline::ShapingPipeline;
use crate::tree::box_tree::ReplacedType;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::TextEmphasisOffset;
use rustc_hash::FxHashMap;
use rustc_hash::FxHasher;
use smallvec::SmallVec;
use std::cell::Cell;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use unicode_bidi::Level;

fn pipeline_dir_from_style(dir: Direction) -> crate::text::pipeline::Direction {
  match dir {
    Direction::Ltr => crate::text::pipeline::Direction::LeftToRight,
    Direction::Rtl => crate::text::pipeline::Direction::RightToLeft,
  }
}

fn explicit_bidi_eq(a: Option<ExplicitBidiContext>, b: Option<ExplicitBidiContext>) -> bool {
  match (a, b) {
    (None, None) => true,
    (Some(a), Some(b)) => a.level == b.level && a.override_all == b.override_all,
    _ => false,
  }
}

const LINE_BUILDER_DEADLINE_STRIDE: usize = 256;
const LINE_DEFAULT_ITEM_CAPACITY: usize = 8;
// Subpixel layout relies on floating point arithmetic, which can introduce tiny rounding errors when
// comparing item advances to the available line width. Without a tolerance, content that should
// "just fit" can be treated as overflowing, triggering emergency wrap opportunities like
// `word-break: break-word` and inflating line heights (e.g. single-word footer links wrapping).
const LINE_FIT_EPSILON: f32 = 0.01;
// Some layout code paths effectively snap to whole pixels (e.g. flex/grid intrinsic sizing probes),
// while inline content (text shaping, inline-block contents) retains subpixel advances. Allow a
// half-pixel tolerance so accumulated rounding doesn't spuriously flip wrap decisions without
// noticeably widening lines (which can change wrapping on real-world pages).
const LINE_PIXEL_FIT_EPSILON: f32 = 0.5;

fn check_layout_deadline(counter: &mut usize) -> Result<(), LayoutError> {
  if let Err(RenderError::Timeout { elapsed, .. }) =
    check_active_periodic(counter, LINE_BUILDER_DEADLINE_STRIDE, RenderStage::Layout)
  {
    return Err(LayoutError::Timeout { elapsed });
  }
  Ok(())
}

/// An item in the inline formatting context
///
/// Represents different types of content that can appear inline.
#[derive(Debug, Clone)]
pub enum InlineItem {
  /// Shaped text ready for layout
  Text(TextItem),

  /// Forced *soft* line break (used by `text-wrap: balance`/`pretty` to commit to chosen wrap points).
  ///
  /// Unlike `HardBreak`, this does **not** mark the end of a paragraph (`ends_with_hard_break` stays
  /// false), so bidi reordering continues across the break.
  SoftBreak,

  /// A tab character that expands to the next tab stop
  Tab(TabItem),

  /// Mandatory hard line break (e.g., from `<br>` or preserved newline characters)
  ///
  /// This item does not produce a fragment and does not paint a glyph. It exists purely to force
  /// the line builder to end the current line immediately.
  HardBreak(ClearSide),

  /// An inline box (span, a, em, etc.) with children
  InlineBox(InlineBoxItem),

  /// An inline-block box (atomic inline)
  InlineBlock(InlineBlockItem),

  /// A ruby annotation container (ruby, ruby-base, etc.)
  Ruby(RubyItem),

  /// A replaced element (img, canvas, etc.)
  Replaced(ReplacedItem),

  /// A floating box encountered in the inline stream
  Floating(FloatingItem),

  /// Zero-sized anchor for absolute/fixed positioned children
  StaticPositionAnchor(StaticPositionAnchor),
}

impl InlineItem {
  /// Returns the width of this item
  pub fn width(&self) -> f32 {
    match self {
      InlineItem::Text(t) => t.advance_for_layout,
      InlineItem::SoftBreak => 0.0,
      InlineItem::Tab(t) => t.width(),
      InlineItem::HardBreak(_) => 0.0,
      InlineItem::InlineBox(b) => b.total_width(),
      InlineItem::InlineBlock(b) => b.total_width(),
      InlineItem::Ruby(r) => r.width(),
      InlineItem::Replaced(r) => r.total_width(),
      InlineItem::Floating(_) => 0.0,
      InlineItem::StaticPositionAnchor(_) => 0.0,
    }
  }

  /// Returns the intrinsic width excluding margins (border/padding included)
  pub fn intrinsic_width(&self) -> f32 {
    match self {
      InlineItem::Text(t) => t.advance_for_layout,
      InlineItem::SoftBreak => 0.0,
      InlineItem::Tab(t) => t.width(),
      InlineItem::HardBreak(_) => 0.0,
      InlineItem::InlineBox(b) => b.width(),
      InlineItem::InlineBlock(b) => b.width,
      InlineItem::Ruby(r) => r.intrinsic_width(),
      InlineItem::Replaced(r) => r.intrinsic_width(),
      InlineItem::Floating(_) => 0.0,
      InlineItem::StaticPositionAnchor(_) => 0.0,
    }
  }

  /// Returns baseline metrics for this item
  pub fn baseline_metrics(&self) -> BaselineMetrics {
    match self {
      InlineItem::Text(t) => t.metrics,
      InlineItem::SoftBreak => BaselineMetrics::new(0.0, 0.0, 0.0, 0.0),
      InlineItem::Tab(t) => t.metrics,
      InlineItem::HardBreak(_) => StaticPositionAnchor::metrics(),
      InlineItem::InlineBox(b) => b.metrics,
      InlineItem::InlineBlock(b) => b.metrics,
      InlineItem::Ruby(r) => r.metrics,
      InlineItem::Replaced(r) => r.metrics,
      InlineItem::Floating(f) => f.metrics,
      InlineItem::StaticPositionAnchor(_) => StaticPositionAnchor::metrics(),
    }
  }

  /// Returns baseline metrics used for line box height calculation.
  ///
  /// For non-replaced inline elements (`display: inline`), vertical padding/borders contribute to
  /// ink overflow but do not participate in line box sizing in Chromium. Instead, those
  /// decorations can overflow above/below the line-height strut (and may be clipped by
  /// `overflow: hidden`).
  ///
  /// Parent line boxes should therefore size from the inline box's line-height strut / in-flow
  /// inline children (including `vertical-align` adjustments of descendants), not from the painted
  /// border box.
  pub fn line_metrics(&self) -> BaselineMetrics {
    match self {
      InlineItem::InlineBox(b) => b.metrics,
      _ => self.baseline_metrics(),
    }
  }

  /// Returns the vertical alignment for this item
  pub fn vertical_align(&self) -> VerticalAlign {
    match self {
      InlineItem::Text(t) => t.vertical_align,
      InlineItem::SoftBreak => VerticalAlign::Baseline,
      InlineItem::Tab(t) => t.vertical_align,
      InlineItem::HardBreak(_) => VerticalAlign::Baseline,
      InlineItem::InlineBox(b) => b.vertical_align,
      InlineItem::InlineBlock(b) => b.vertical_align,
      InlineItem::Ruby(r) => r.vertical_align,
      InlineItem::Replaced(r) => r.vertical_align,
      InlineItem::Floating(f) => f.vertical_align,
      InlineItem::StaticPositionAnchor(_) => VerticalAlign::Baseline,
    }
  }

  /// Returns true if this item can be broken (for text)
  pub fn is_breakable(&self) -> bool {
    matches!(self, InlineItem::Text(_) | InlineItem::InlineBox(_))
  }

  pub fn direction(&self) -> Direction {
    match self {
      InlineItem::Text(t) => t.style.direction,
      InlineItem::SoftBreak => Direction::Ltr,
      InlineItem::Tab(t) => t.direction,
      InlineItem::HardBreak(_) => Direction::Ltr,
      InlineItem::InlineBox(b) => b.direction,
      InlineItem::InlineBlock(b) => b.direction,
      InlineItem::Ruby(r) => r.direction,
      InlineItem::Replaced(r) => r.direction,
      InlineItem::Floating(f) => f.direction,
      InlineItem::StaticPositionAnchor(a) => a.direction,
    }
  }

  pub fn unicode_bidi(&self) -> UnicodeBidi {
    match self {
      InlineItem::Text(t) => t.style.unicode_bidi,
      InlineItem::SoftBreak => UnicodeBidi::Normal,
      InlineItem::Tab(t) => t.unicode_bidi,
      InlineItem::HardBreak(_) => UnicodeBidi::Normal,
      InlineItem::InlineBox(b) => b.unicode_bidi,
      InlineItem::InlineBlock(b) => b.unicode_bidi,
      InlineItem::Ruby(r) => r.unicode_bidi,
      InlineItem::Replaced(r) => r.unicode_bidi,
      InlineItem::Floating(f) => f.unicode_bidi,
      InlineItem::StaticPositionAnchor(a) => a.unicode_bidi,
    }
  }

  /// Resolves the width of this item when placed at `start_x`.
  ///
  /// Tabs depend on their starting position to find the next tab stop; other items are fixed width.
  pub fn resolve_width_at(mut self, start_x: f32) -> (Self, f32) {
    match &mut self {
      InlineItem::Tab(tab) => {
        let width = tab.resolve_width(start_x);
        (self, width)
      }
      InlineItem::HardBreak(_) => (self, 0.0),
      _ => {
        let width = self.width();
        (self, width)
      }
    }
  }

  fn contains_hard_break(&self) -> bool {
    match self {
      InlineItem::HardBreak(_) => true,
      InlineItem::InlineBox(b) => b.children.iter().any(|c| c.contains_hard_break()),
      InlineItem::Ruby(r) => r.segments.iter().any(|seg| {
        seg.base_items.iter().any(|c| c.contains_hard_break())
          || seg
            .annotation_top
            .as_ref()
            .is_some_and(|items| items.iter().any(|c| c.contains_hard_break()))
          || seg
            .annotation_bottom
            .as_ref()
            .is_some_and(|items| items.iter().any(|c| c.contains_hard_break()))
      }),
      InlineItem::Text(_)
      | InlineItem::SoftBreak
      | InlineItem::Tab(_)
      | InlineItem::InlineBlock(_)
      | InlineItem::Replaced(_)
      | InlineItem::Floating(_)
      | InlineItem::StaticPositionAnchor(_) => false,
    }
  }

  /// Hoists hard breaks nested inside container inline items (inline boxes, ruby) into a flat stream.
  ///
  /// `InlineItem::HardBreak` itself does not generate fragments; it must be consumed by `LineBuilder`
  /// at the top level. If hard breaks are left nested (e.g. inside an inline box), fragment
  /// creation will hit the debug assertions meant to enforce that invariant.
  fn hoist_hard_breaks(self) -> Vec<Self> {
    match self {
      InlineItem::HardBreak(clear) => vec![InlineItem::HardBreak(clear)],
      InlineItem::InlineBox(inline_box) => {
        let InlineBoxItem {
          box_id,
          children,
          justify_gaps: _,
          start_edge,
          end_edge,
          line_padding_start: _,
          line_padding_end: _,
          margin_left,
          margin_right,
          content_offset_y,
          border_left,
          border_right,
          border_top,
          border_bottom,
          bottom_inset,
          metrics: _,
          strut_metrics,
          vertical_align,
          box_index,
          direction,
          unicode_bidi,
          style,
        } = inline_box;

        let mut segments: Vec<Vec<InlineItem>> = Vec::new();
        let mut breaks: Vec<ClearSide> = Vec::new();
        let mut current: Vec<InlineItem> = Vec::new();

        for child in children {
          for part in child.hoist_hard_breaks() {
            if let InlineItem::HardBreak(clear) = part {
              segments.push(std::mem::take(&mut current));
              breaks.push(clear);
            } else {
              current.push(part);
            }
          }
        }
        segments.push(current);

        let non_empty: Vec<usize> = segments
          .iter()
          .enumerate()
          .filter_map(|(idx, segment)| (!segment.is_empty()).then_some(idx))
          .collect();
        let first_non_empty = non_empty.first().copied();
        let last_non_empty = non_empty.last().copied();

        let mut out = Vec::new();
        for (idx, segment_children) in segments.into_iter().enumerate() {
          if !segment_children.is_empty() {
            let is_first = first_non_empty.is_some_and(|first| first == idx);
            let is_last = last_non_empty.is_some_and(|last| last == idx);

            // Match `box-decoration-break: slice` semantics: the first fragment keeps the original
            // start edge; the last fragment keeps the original end edge; intermediate fragments
            // have no horizontal edges.
            let start_edge = if is_first { start_edge } else { 0.0 };
            let end_edge = if is_last { end_edge } else { 0.0 };
            let border_left = if is_first { border_left } else { 0.0 };
            let border_right = if is_last { border_right } else { 0.0 };
            let margin_left = if is_first { margin_left } else { 0.0 };
            let margin_right = if is_last { margin_right } else { 0.0 };

            let metrics = super::compute_inline_box_metrics(
              &segment_children,
              content_offset_y,
              bottom_inset,
              strut_metrics,
            );

            out.push(InlineItem::InlineBox(InlineBoxItem {
              box_id,
              children: segment_children,
              justify_gaps: Vec::new(),
              start_edge,
              end_edge,
              line_padding_start: 0.0,
              line_padding_end: 0.0,
              margin_left,
              margin_right,
              content_offset_y,
              border_left,
              border_right,
              border_top,
              border_bottom,
              bottom_inset,
              metrics,
              strut_metrics,
              vertical_align,
              box_index,
              direction,
              unicode_bidi,
              style: style.clone(),
            }));
          }
          if idx < breaks.len() {
            out.push(InlineItem::HardBreak(breaks[idx]));
          }
        }

        out
      }
      InlineItem::Ruby(ruby) => {
        let contains_hard_break = ruby.segments.iter().any(|seg| {
          seg.base_items.iter().any(|c| c.contains_hard_break())
            || seg
              .annotation_top
              .as_ref()
              .is_some_and(|items| items.iter().any(|c| c.contains_hard_break()))
            || seg
              .annotation_bottom
              .as_ref()
              .is_some_and(|items| items.iter().any(|c| c.contains_hard_break()))
        });
        if !contains_hard_break {
          return vec![InlineItem::Ruby(ruby)];
        }

        // Ruby is laid out as an atomic unit. If a hard break sneaks inside, degrade to emitting the
        // base items as a plain inline stream so mandatory breaks are still honored.
        let mut out = Vec::new();
        for segment in ruby.segments {
          for item in segment.base_items {
            out.extend(item.hoist_hard_breaks());
          }
        }
        out
      }
      other => vec![other],
    }
  }
}

/// Inline placeholder that marks where a positioned element would appear in flow.
#[derive(Debug, Clone)]
pub struct StaticPositionAnchor {
  pub box_id: usize,
  pub direction: Direction,
  pub unicode_bidi: UnicodeBidi,
  pub running: Option<RunningInfo>,
  pub footnote: Option<FootnoteInfo>,
}

impl StaticPositionAnchor {
  pub fn new(box_id: usize, direction: Direction, unicode_bidi: UnicodeBidi) -> Self {
    Self {
      box_id,
      direction,
      unicode_bidi,
      running: None,
      footnote: None,
    }
  }

  pub fn metrics() -> BaselineMetrics {
    BaselineMetrics::new(0.0, 0.0, 0.0, 0.0)
  }

  pub fn with_running(mut self, running: RunningInfo) -> Self {
    self.running = Some(running);
    self
  }

  pub fn with_footnote(mut self, footnote: FootnoteInfo) -> Self {
    self.footnote = Some(footnote);
    self
  }
}

#[derive(Debug, Clone)]
pub struct RunningInfo {
  pub name: String,
  pub snapshot: FragmentNode,
  pub style: Arc<ComputedStyle>,
}

#[derive(Debug, Clone)]
pub struct FootnoteInfo {
  pub snapshot: FragmentNode,
  pub policy: FootnotePolicy,
}

fn allows_soft_wrap(style: &ComputedStyle) -> bool {
  !matches!(style.white_space, WhiteSpace::Nowrap | WhiteSpace::Pre)
    && !matches!(style.text_wrap, TextWrap::NoWrap)
}

fn item_allows_soft_wrap(item: &InlineItem) -> bool {
  match item {
    InlineItem::Text(text) => allows_soft_wrap(text.style.as_ref()),
    InlineItem::InlineBox(inline_box) => allows_soft_wrap(inline_box.style.as_ref()),
    InlineItem::InlineBlock(inline_block) => inline_block
      .fragment
      .style
      .as_deref()
      .map_or(true, allows_soft_wrap),
    InlineItem::Ruby(ruby) => allows_soft_wrap(ruby.style.as_ref()),
    InlineItem::Replaced(replaced) => allows_soft_wrap(replaced.style.as_ref()),
    // Tabs already memoize whether wrapping is allowed for the active inline context.
    InlineItem::Tab(tab) => tab.allow_wrap(),
    // Soft breaks are inserted deliberately (e.g. `text-wrap`), so honor them regardless of
    // `white-space`. Hard breaks are handled earlier.
    InlineItem::SoftBreak | InlineItem::HardBreak(_) => true,
    // Floats/anchors do not participate in line breaking the way normal in-flow items do.
    InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_) => true,
  }
}

fn soft_wrap_style_for_item(item: &InlineItem) -> Option<&ComputedStyle> {
  match item {
    InlineItem::Text(text) => Some(text.style.as_ref()),
    InlineItem::InlineBox(inline_box) => Some(inline_box.style.as_ref()),
    InlineItem::InlineBlock(inline_block) => inline_block.fragment.style.as_deref(),
    InlineItem::Ruby(ruby) => Some(ruby.style.as_ref()),
    InlineItem::Replaced(replaced) => Some(replaced.style.as_ref()),
    InlineItem::Tab(tab) => Some(tab.style.as_ref()),
    InlineItem::SoftBreak
    | InlineItem::HardBreak(_)
    | InlineItem::Floating(_)
    | InlineItem::StaticPositionAnchor(_) => None,
  }
}

fn is_no_break_after_character(ch: char) -> bool {
  // Unicode line-breaking treats NO-BREAK SPACE and related "glue" characters as forbidding
  // a soft wrap opportunity after them (UAX#14 "GL"/"WJ" classes).
  //
  // This matters when inline content mixes text with atomic inlines (inline-block/replaced):
  // `Community&nbsp;<span class="caret"></span>` must not allow the caret to wrap onto a new line.
  // If the sequence doesn't fit, it should overflow as an unbreakable unit.
  matches!(ch, '\u{00A0}' | '\u{202F}' | '\u{2060}' | '\u{FEFF}')
}

fn last_text_char_for_soft_wrap(item: &InlineItem) -> Option<char> {
  match item {
    InlineItem::Text(text) => text.text.chars().next_back(),
    InlineItem::InlineBox(inline_box) => inline_box
      .children
      .iter()
      .rev()
      .find_map(last_text_char_for_soft_wrap),
    InlineItem::Ruby(ruby) => ruby.segments.iter().rev().find_map(|seg| {
      seg
        .base_items
        .iter()
        .rev()
        .find_map(last_text_char_for_soft_wrap)
    }),
    InlineItem::SoftBreak
    | InlineItem::Tab(_)
    | InlineItem::HardBreak(_)
    | InlineItem::InlineBlock(_)
    | InlineItem::Replaced(_)
    | InlineItem::Floating(_)
    | InlineItem::StaticPositionAnchor(_) => None,
  }
}

fn first_text_char_for_soft_wrap(item: &InlineItem) -> Option<char> {
  match item {
    InlineItem::Text(text) => text.text.chars().next(),
    InlineItem::InlineBox(inline_box) => inline_box
      .children
      .iter()
      .find_map(first_text_char_for_soft_wrap),
    InlineItem::Ruby(ruby) => ruby.segments.iter().find_map(|seg| {
      seg
        .base_items
        .iter()
        .find_map(first_text_char_for_soft_wrap)
        .or_else(|| {
          seg
            .annotation_top
            .as_ref()
            .and_then(|items| items.iter().find_map(first_text_char_for_soft_wrap))
        })
        .or_else(|| {
          seg
            .annotation_bottom
            .as_ref()
            .and_then(|items| items.iter().find_map(first_text_char_for_soft_wrap))
        })
    }),
    InlineItem::SoftBreak
    | InlineItem::Tab(_)
    | InlineItem::HardBreak(_)
    | InlineItem::InlineBlock(_)
    | InlineItem::Replaced(_)
    | InlineItem::Floating(_)
    | InlineItem::StaticPositionAnchor(_) => None,
  }
}

fn soft_wrap_opportunity_between_chars(prev: char, next: char) -> bool {
  if is_no_break_after_character(prev) {
    return false;
  }

  let mut text = String::new();
  text.push(prev);
  text.push(next);
  let boundary = prev.len_utf8();
  crate::text::line_break::find_break_opportunities(&text)
    .iter()
    .any(|brk| brk.byte_offset == boundary)
}

fn style_allows_non_uax_boundary_break(style: &ComputedStyle) -> bool {
  matches!(style.line_break, LineBreak::Anywhere)
    || matches!(
      style.word_break,
      WordBreak::BreakAll | WordBreak::BreakWord | WordBreak::Anywhere
    )
    || matches!(
      style.overflow_wrap,
      OverflowWrap::BreakWord | OverflowWrap::Anywhere
    )
}

fn soft_wrap_opportunity_between_chars_with_styles(
  prev: char,
  next: char,
  prev_style: Option<&ComputedStyle>,
  next_style: Option<&ComputedStyle>,
) -> bool {
  if soft_wrap_opportunity_between_chars(prev, next) {
    return true;
  }
  // `word-break` / `overflow-wrap` should not override explicit no-break glue characters like NBSP.
  if is_no_break_after_character(prev) {
    return false;
  }
  prev_style.is_some_and(style_allows_non_uax_boundary_break)
    || next_style.is_some_and(style_allows_non_uax_boundary_break)
}

pub(crate) fn log_line_width_enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_LOG_LINE_WIDTH")
}

/// A shaped text item
#[derive(Debug, Clone)]
pub struct TextItem {
  /// Identifier for the source text box node (0 when unknown/anonymous).
  pub box_id: usize,

  /// The shaped runs (bidi/script/font-aware)
  pub runs: Vec<ShapedRun>,

  /// Total horizontal advance
  pub advance: f32,
  /// Horizontal advance used for layout (may differ for markers)
  pub advance_for_layout: f32,

  /// Baseline metrics
  pub metrics: BaselineMetrics,

  /// Vertical alignment
  pub vertical_align: VerticalAlign,

  /// Break opportunities within this text
  pub break_opportunities: Vec<BreakOpportunity>,
  /// Offsets of forced breaks inserted during normalization (e.g., newlines)
  pub forced_break_offsets: Vec<usize>,
  /// Earliest mandatory break in `break_opportunities` (if any).
  first_mandatory_break: Option<BreakOpportunity>,

  /// Original text for fragment creation
  pub text: String,

  /// Font size used
  pub font_size: f32,

  /// Computed style for this text run
  pub style: Arc<ComputedStyle>,
  /// Base paragraph direction used for shaping
  pub base_direction: Direction,
  /// Explicit bidi context used during shaping (embedding level + override flag).
  pub explicit_bidi: Option<ExplicitBidiContext>,
  /// Whether this text item is a list marker
  pub is_marker: bool,
  /// Additional paint offset applied at fragment creation (used for outside markers)
  pub paint_offset: f32,

  /// Extra offset for emphasis mark placement.
  pub emphasis_offset: TextEmphasisOffset,
  /// Cumulative advances at cluster boundaries (text order)
  cluster_advances: Vec<ClusterBoundary>,
  /// Range of the original source text represented by this item.
  source_range: Range<usize>,
  /// Stable hash of the source text used for ephemeral caches.
  source_id: u64,
}

/// A floating box placeholder encountered in the inline stream
#[derive(Debug, Clone)]
pub struct FloatingItem {
  pub box_node: crate::tree::box_tree::BoxNode,
  pub metrics: BaselineMetrics,
  pub vertical_align: VerticalAlign,
  pub direction: Direction,
  pub unicode_bidi: UnicodeBidi,
}

#[derive(Debug, Clone)]
struct ClusterBoundary {
  /// Byte offset in the source text where this cluster starts
  byte_offset: usize,
  /// Advance width from the start of the item up to and including this cluster
  advance: f32,
  /// Index of the shaped run that owns this cluster boundary.
  run_index: Option<usize>,
  /// Glyph index at which the boundary occurs (exclusive).
  glyph_end: Option<usize>,
  /// Advance within the run up to this boundary.
  run_advance: f32,
}

thread_local! {
  static INLINE_RESHAPE_CACHE_DIAGNOSTICS_ENABLED: Cell<bool> = const { Cell::new(false) };
}
static INLINE_RESHAPE_CACHE_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
static INLINE_RESHAPE_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static INLINE_RESHAPE_CACHE_STORES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
  static APPLY_SPACING_SORT_COUNT: Cell<usize> = const { Cell::new(0) };
  static APPLY_SPACING_CLUSTER_COUNT: Cell<usize> = const { Cell::new(0) };
  static APPLY_SPACING_STREAM_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_apply_spacing_diagnostics() {
  APPLY_SPACING_SORT_COUNT.with(|count| count.set(0));
  APPLY_SPACING_CLUSTER_COUNT.with(|count| count.set(0));
  APPLY_SPACING_STREAM_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn take_apply_spacing_diagnostics() -> (usize, usize, usize) {
  let sorts = APPLY_SPACING_SORT_COUNT.with(|count| {
    let value = count.get();
    count.set(0);
    value
  });
  let clusters = APPLY_SPACING_CLUSTER_COUNT.with(|count| {
    let value = count.get();
    count.set(0);
    value
  });
  let streams = APPLY_SPACING_STREAM_COUNT.with(|count| {
    let value = count.get();
    count.set(0);
    value
  });
  (sorts, clusters, streams)
}

pub(crate) fn enable_inline_reshape_cache_diagnostics() {
  INLINE_RESHAPE_CACHE_LOOKUPS.store(0, Ordering::Relaxed);
  INLINE_RESHAPE_CACHE_HITS.store(0, Ordering::Relaxed);
  INLINE_RESHAPE_CACHE_STORES.store(0, Ordering::Relaxed);
  INLINE_RESHAPE_CACHE_DIAGNOSTICS_ENABLED.with(|enabled| enabled.set(true));
}

pub(crate) fn take_inline_reshape_cache_diagnostics() -> Option<(usize, usize, usize)> {
  let was_enabled = INLINE_RESHAPE_CACHE_DIAGNOSTICS_ENABLED.with(|enabled| {
    let prev = enabled.get();
    enabled.set(false);
    prev
  });
  if !was_enabled {
    return None;
  }

  Some((
    INLINE_RESHAPE_CACHE_LOOKUPS.load(Ordering::Relaxed),
    INLINE_RESHAPE_CACHE_HITS.load(Ordering::Relaxed),
    INLINE_RESHAPE_CACHE_STORES.load(Ordering::Relaxed),
  ))
}

fn inline_reshape_cache_diagnostics_enabled() -> bool {
  INLINE_RESHAPE_CACHE_DIAGNOSTICS_ENABLED.with(|enabled| enabled.get())
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Debug)]
pub struct ReshapeCache {
  runs: FxHashMap<ReshapeCacheKey, Arc<Vec<ShapedRun>>>,
  diagnostics_enabled: bool,
}

impl Default for ReshapeCache {
  fn default() -> Self {
    Self {
      runs: FxHashMap::default(),
      diagnostics_enabled: inline_reshape_cache_diagnostics_enabled(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReshapeCacheKey {
  style_hash: u64,
  font_generation: u64,
  text_id: u64,
  range_start: usize,
  range_end: usize,
  base_direction_rtl: bool,
  explicit_bidi: Option<(u8, bool)>,
  letter_spacing_bits: u32,
  word_spacing_bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HyphenAdvanceCacheKey {
  style_hash: u64,
  font_generation: u64,
  hyphen_hash: u64,
  base_direction_rtl: bool,
  explicit_bidi: Option<(u8, bool)>,
  letter_spacing_bits: u32,
  word_spacing_bits: u32,
}

impl TextItem {
  pub(crate) fn source_range(&self) -> Range<usize> {
    self.source_range.clone()
  }

  /// Creates a new text item
  pub fn new(
    runs: Vec<ShapedRun>,
    text: String,
    metrics: BaselineMetrics,
    break_opportunities: Vec<BreakOpportunity>,
    forced_break_offsets: Vec<usize>,
    style: Arc<ComputedStyle>,
    base_direction: Direction,
  ) -> Self {
    let cluster_advances = Self::compute_cluster_advances(&runs, text.as_str(), style.font_size);
    let aligned_breaks =
      Self::align_breaks_to_clusters(break_opportunities, &cluster_advances, text.len());
    let first_mandatory_break = Self::first_mandatory_break(&aligned_breaks);
    let advance: f32 = cluster_advances
      .last()
      .map(|c| c.advance)
      .unwrap_or_else(|| runs.iter().map(|r| r.advance).sum());
    let font_size = style.font_size;
    let source_id = Self::hash_text(&text);
    let source_range = 0..text.len();
    Self {
      box_id: 0,
      runs,
      advance,
      advance_for_layout: advance,
      metrics,
      vertical_align: VerticalAlign::Baseline,
      break_opportunities: aligned_breaks,
      forced_break_offsets,
      first_mandatory_break,
      text,
      font_size,
      style,
      base_direction,
      explicit_bidi: None,
      is_marker: false,
      paint_offset: 0.0,
      emphasis_offset: TextEmphasisOffset::default(),
      cluster_advances,
      source_range,
      source_id,
    }
  }

  pub(crate) fn text_emphasis_extra_extent(style: &ComputedStyle) -> Option<f32> {
    if style.text_emphasis_style.is_none() {
      return None;
    }

    let mark_size = (style.font_size * 0.5).max(1.0);
    let gap = mark_size * 0.3;
    Some(gap + mark_size)
  }

  pub(crate) fn apply_text_emphasis_metrics(metrics: &mut BaselineMetrics, style: &ComputedStyle) {
    let Some(extra) = Self::text_emphasis_extra_extent(style) else {
      return;
    };

    match crate::style::resolve_text_emphasis_block_side(
      style.writing_mode,
      style.text_emphasis_position,
    ) {
      crate::style::BlockSide::Start => {
        metrics.baseline_offset += extra;
        metrics.height += extra;
      }
      crate::style::BlockSide::End => {
        metrics.height += extra;
      }
    }
  }

  /// Byte offsets for each grapheme cluster boundary within the item.
  pub fn cluster_byte_offsets(&self) -> Vec<usize> {
    if self.text.is_empty() {
      return Vec::new();
    }

    let mut offsets: Vec<usize> = self
      .runs
      .iter()
      .flat_map(|run| {
        let mut run_offsets = Vec::new();

        if run.glyphs.is_empty() {
          run_offsets.push(Self::previous_char_boundary_in_text(&self.text, run.start));
          run_offsets.push(Self::previous_char_boundary_in_text(&self.text, run.end));
          return run_offsets;
        }

        for glyph in &run.glyphs {
          let raw_offset = run.start.saturating_add(glyph.cluster as usize);
          run_offsets.push(Self::previous_char_boundary_in_text(&self.text, raw_offset));
        }
        run_offsets.push(Self::previous_char_boundary_in_text(&self.text, run.end));
        run_offsets
      })
      .collect();

    if offsets.is_empty() {
      offsets = crate::text::segmentation::segment_grapheme_clusters(&self.text);
    } else {
      offsets.push(0);
      offsets.push(self.text.len());
      offsets.sort_unstable();
      offsets.dedup();
    }

    offsets
  }

  /// Add allowed break opportunities at every cluster boundary.
  pub fn add_breaks_at_clusters(&mut self) {
    if self.text.is_empty() {
      return;
    }
    let additional: Vec<BreakOpportunity> = self
      .cluster_byte_offsets()
      .into_iter()
      .filter(|offset| *offset > 0 && *offset < self.text.len())
      .map(|offset| BreakOpportunity::new(offset, BreakType::Allowed))
      .collect();
    if additional.is_empty() {
      return;
    }

    let mut base = std::mem::take(&mut self.break_opportunities);

    // Break opportunities should already be sorted here (constructed via `apply_break_properties`),
    // but keep this robust to future changes.
    let mut sorted = true;
    let mut prev_offset: Option<usize> = None;
    for brk in &base {
      if let Some(prev) = prev_offset {
        if brk.byte_offset < prev {
          sorted = false;
          break;
        }
      }
      prev_offset = Some(brk.byte_offset);
    }
    if !sorted {
      base.sort_unstable_by_key(|b| b.byte_offset);
      base.dedup_by(|a, b| {
        if a.byte_offset != b.byte_offset {
          return false;
        }
        let mandatory = matches!(a.break_type, BreakType::Mandatory)
          || matches!(b.break_type, BreakType::Mandatory);
        if mandatory {
          b.break_type = BreakType::Mandatory;
          b.kind = crate::text::line_break::BreakOpportunityKind::Normal;
        } else if matches!(
          a.kind,
          crate::text::line_break::BreakOpportunityKind::Normal
        ) || matches!(
          b.kind,
          crate::text::line_break::BreakOpportunityKind::Normal
        ) {
          b.kind = crate::text::line_break::BreakOpportunityKind::Normal;
        }
        b.adds_hyphen |= a.adds_hyphen;
        true
      });
    }

    // Both lists are sorted by byte offset; merge them to avoid sorting the combined vector.
    // Prefer existing breaks for duplicates so we preserve any mandatory/hyphen metadata.
    let mut merged = Vec::with_capacity(base.len() + additional.len());
    let mut base_iter = base.into_iter().peekable();
    let mut additional_iter = additional.into_iter().peekable();
    while base_iter.peek().is_some() || additional_iter.peek().is_some() {
      match (base_iter.peek(), additional_iter.peek()) {
        (Some(a), Some(b)) => {
          if a.byte_offset < b.byte_offset {
            let Some(next) = base_iter.next() else {
              break;
            };
            merged.push(next);
          } else if a.byte_offset > b.byte_offset {
            let Some(next) = additional_iter.next() else {
              break;
            };
            merged.push(next);
          } else {
            // Same byte offset: merge so normal/mandatory/hyphen metadata wins.
            let Some(mut existing) = base_iter.next() else {
              break;
            };
            let Some(added) = additional_iter.next() else {
              break;
            };
            if matches!(added.break_type, BreakType::Mandatory)
              || matches!(existing.break_type, BreakType::Mandatory)
            {
              existing.break_type = BreakType::Mandatory;
              existing.kind = crate::text::line_break::BreakOpportunityKind::Normal;
            } else {
              existing.kind = crate::text::line_break::BreakOpportunityKind::Normal;
            }
            existing.adds_hyphen |= added.adds_hyphen;
            merged.push(existing);
          }
        }
        (Some(_), None) => {
          let Some(next) = base_iter.next() else {
            break;
          };
          merged.push(next);
        }
        (None, Some(_)) => {
          let Some(next) = additional_iter.next() else {
            break;
          };
          merged.push(next);
        }
        (None, None) => break,
      }
    }
    self.break_opportunities = merged;
    self.first_mandatory_break = Self::first_mandatory_break(&self.break_opportunities);
  }

  pub fn recompute_cluster_advances(&mut self) {
    self.cluster_advances = Self::compute_cluster_advances(&self.runs, &self.text, self.font_size);
  }

  /// Derive baseline metrics from shaped runs and CSS line-height
  pub fn metrics_from_runs(
    font_context: &FontContext,
    runs: &[ShapedRun],
    line_height: f32,
    fallback_font_size: f32,
  ) -> BaselineMetrics {
    let mut ascent: f32 = 0.0;
    let mut descent: f32 = 0.0;
    let mut line_gap: f32 = 0.0;
    let mut x_height: Option<f32> = None;

    for run in runs {
      if let Some(scaled) = font_context.get_scaled_metrics_with_variations(
        run.font.as_ref(),
        run.font_size,
        &run.variations,
      ) {
        ascent = ascent.max(scaled.ascent);
        descent = descent.max(scaled.descent);
        line_gap = line_gap.max(scaled.line_gap);
        if x_height.is_none() {
          x_height = scaled.x_height;
        }
      }
    }

    if ascent == 0.0 && descent == 0.0 {
      ascent = fallback_font_size * 0.8;
      descent = fallback_font_size * 0.2;
    }

    // CSS 2.1 §10.8: inline box baselines include half-leading so negative leading (line-height
    // smaller than the font metrics) doesn't force the line box to expand to the font's full
    // ascent+descent.
    let half_leading = (line_height - (ascent + descent)) / 2.0;

    BaselineMetrics {
      baseline_offset: ascent + half_leading,
      height: line_height,
      ascent,
      descent,
      line_gap,
      line_height,
      x_height,
    }
  }

  pub fn metrics_from_first_available_font(
    primary_metrics: Option<&crate::text::font_db::ScaledMetrics>,
    line_height: f32,
    fallback_font_size: f32,
  ) -> BaselineMetrics {
    let (ascent, descent, line_gap, x_height) = if let Some(primary) = primary_metrics {
      (
        primary.ascent,
        primary.descent,
        primary.line_gap,
        primary.x_height,
      )
    } else {
      (
        fallback_font_size * 0.8,
        fallback_font_size * 0.2,
        0.0,
        Some(fallback_font_size * 0.4),
      )
    };
    let half_leading = (line_height - (ascent + descent)) / 2.0;

    BaselineMetrics {
      baseline_offset: ascent + half_leading,
      height: line_height,
      ascent,
      descent,
      line_gap,
      line_height,
      x_height,
    }
  }

  /// Sets the vertical alignment
  pub fn with_vertical_align(mut self, align: VerticalAlign) -> Self {
    self.vertical_align = align;
    self
  }

  /// Splits this text item at a byte offset, returning (before, after)
  ///
  /// This is used for line breaking within text.
  pub(crate) fn split_at(
    &self,
    byte_offset: usize,
    insert_hyphen: bool,
    shaper: &ShapingPipeline,
    font_context: &FontContext,
    reshape_cache: &mut ReshapeCache,
  ) -> Option<(TextItem, TextItem)> {
    let text_len = self.text.len();
    if byte_offset == 0 || byte_offset >= text_len {
      return None;
    }

    let target_offset = if self.text.is_char_boundary(byte_offset) {
      byte_offset
    } else {
      Self::previous_char_boundary_in_text(&self.text, byte_offset)
    };

    if target_offset == 0 || target_offset >= text_len {
      return None;
    }

    let split_offset = target_offset;

    if split_offset == 0 || split_offset >= text_len || !self.text.is_char_boundary(split_offset) {
      return None;
    }

    // Split the text
    let before_text = self.text.get(..split_offset)?;
    let after_text = self.text.get(split_offset..)?;

    // Forced breaks (e.g. newlines removed during white-space normalization) must not allow shaping
    // effects such as kerning to carry across the boundary. In those cases, always reshape each side
    // rather than slicing the existing glyph stream.
    let is_forced_break = self
      .forced_break_offsets
      .binary_search(&split_offset)
      .is_ok();
    let (mut before_runs, after_runs) = (!is_forced_break)
      .then(|| self.split_runs_preserving_shaping(split_offset))
      .flatten()
      .or_else(|| {
        let before_runs = reshape_cache.shape(self, 0..split_offset, shaper, font_context)?;
        let after_runs = reshape_cache.shape(self, split_offset..text_len, shaper, font_context)?;
        Some((before_runs, after_runs))
      })?;

    let before_text_owned: Option<String> = if insert_hyphen {
      let hyphen_text = self
        .style
        .hyphenate_character
        .as_deref()
        .unwrap_or("\u{2010}");
      if hyphen_text.is_empty() {
        None
      } else {
        let offset = before_text.len();
        let mut hyphen_runs = shaper
          .shape_with_context(
            hyphen_text,
            &self.style,
            font_context,
            pipeline_dir_from_style(self.base_direction),
            self.explicit_bidi,
          )
          .ok()?;
        TextItem::apply_spacing_to_runs(
          &mut hyphen_runs,
          hyphen_text,
          self.style.letter_spacing,
          self.style.word_spacing,
        );

        for run in &mut hyphen_runs {
          run.start += offset;
          run.end += offset;
          for glyph in &mut run.glyphs {
            glyph.cluster = glyph.cluster.saturating_add(offset as u32);
          }
        }

        before_runs.extend(hyphen_runs);
        let mut owned = before_text.to_string();
        owned.push_str(hyphen_text);
        Some(owned)
      }
    } else {
      None
    };

    let line_height = self.metrics.line_height;
    let (before_metrics, after_metrics) = if matches!(
      self.style.line_height,
      crate::style::types::LineHeight::Normal
    ) {
      let mut before_metrics =
        TextItem::metrics_from_runs(font_context, &before_runs, line_height, self.font_size);
      TextItem::apply_text_emphasis_metrics(&mut before_metrics, &self.style);
      let mut after_metrics =
        TextItem::metrics_from_runs(font_context, &after_runs, line_height, self.font_size);
      TextItem::apply_text_emphasis_metrics(&mut after_metrics, &self.style);
      (before_metrics, after_metrics)
    } else {
      (self.metrics, self.metrics)
    };

    let mut before_item = TextItem::new(
      before_runs,
      before_text_owned
        .clone()
        .unwrap_or_else(|| before_text.to_string()),
      before_metrics,
      self
        .break_opportunities
        .iter()
        .filter(|b| b.byte_offset <= split_offset)
        .copied()
        .collect(),
      self
        .forced_break_offsets
        .iter()
        .copied()
        .filter(|o| *o <= split_offset)
        .collect(),
      self.style.clone(),
      self.base_direction,
    )
    .with_vertical_align(self.vertical_align);
    before_item.box_id = self.box_id;
    before_item.explicit_bidi = self.explicit_bidi;
    before_item.source_id = self.source_id;
    before_item.source_range = self.source_range.start..self.source_range.start + split_offset;
    before_item.emphasis_offset = self.emphasis_offset;

    let mut after_item = TextItem::new(
      after_runs,
      after_text.to_string(),
      after_metrics,
      self
        .break_opportunities
        .iter()
        .filter(|b| b.byte_offset > split_offset)
        .map(|b| {
          BreakOpportunity::with_hyphen_and_kind(
            b.byte_offset - split_offset,
            b.break_type,
            b.adds_hyphen,
            b.kind,
          )
        })
        .collect(),
      self
        .forced_break_offsets
        .iter()
        .copied()
        .filter(|o| *o > split_offset)
        .map(|o| o - split_offset)
        .collect(),
      self.style.clone(),
      self.base_direction,
    )
    .with_vertical_align(self.vertical_align);
    after_item.box_id = self.box_id;
    after_item.explicit_bidi = self.explicit_bidi;
    after_item.source_id = self.source_id;
    after_item.source_range = self.source_range.start + split_offset..self.source_range.end;
    after_item.emphasis_offset = self.emphasis_offset;

    if before_item.advance <= 0.0 || after_item.advance <= 0.0 {
      let before_runs = reshape_cache.shape(self, 0..split_offset, shaper, font_context)?;
      let after_runs = reshape_cache.shape(self, split_offset..text_len, shaper, font_context)?;
      let (before_metrics, after_metrics) = if matches!(
        self.style.line_height,
        crate::style::types::LineHeight::Normal
      ) {
        let mut before_metrics =
          TextItem::metrics_from_runs(font_context, &before_runs, line_height, self.font_size);
        TextItem::apply_text_emphasis_metrics(&mut before_metrics, &self.style);
        let mut after_metrics =
          TextItem::metrics_from_runs(font_context, &after_runs, line_height, self.font_size);
        TextItem::apply_text_emphasis_metrics(&mut after_metrics, &self.style);
        (before_metrics, after_metrics)
      } else {
        (self.metrics, self.metrics)
      };
      before_item = TextItem::new(
        before_runs,
        before_text_owned.unwrap_or_else(|| before_text.to_string()),
        before_metrics,
        self
          .break_opportunities
          .iter()
          .filter(|b| b.byte_offset <= split_offset)
          .copied()
          .collect(),
        self
          .forced_break_offsets
          .iter()
          .copied()
          .filter(|o| *o <= split_offset)
          .collect(),
        self.style.clone(),
        self.base_direction,
      )
      .with_vertical_align(self.vertical_align);
      before_item.box_id = self.box_id;
      before_item.explicit_bidi = self.explicit_bidi;
      before_item.source_id = self.source_id;
      before_item.source_range = self.source_range.start..self.source_range.start + split_offset;
      before_item.emphasis_offset = self.emphasis_offset;

      after_item = TextItem::new(
        after_runs,
        after_text.to_string(),
        after_metrics,
        self
          .break_opportunities
          .iter()
          .filter(|b| b.byte_offset > split_offset)
          .map(|b| {
            BreakOpportunity::with_hyphen_and_kind(
              b.byte_offset - split_offset,
              b.break_type,
              b.adds_hyphen,
              b.kind,
            )
          })
          .collect(),
        self
          .forced_break_offsets
          .iter()
          .copied()
          .filter(|o| *o > split_offset)
          .map(|o| o - split_offset)
          .collect(),
        self.style.clone(),
        self.base_direction,
      )
      .with_vertical_align(self.vertical_align);
      after_item.box_id = self.box_id;
      after_item.explicit_bidi = self.explicit_bidi;
      after_item.source_id = self.source_id;
      after_item.source_range = self.source_range.start + split_offset..self.source_range.end;
      after_item.emphasis_offset = self.emphasis_offset;
    }

    if self.is_marker {
      before_item.is_marker = true;
      after_item.is_marker = true;
      before_item.paint_offset = self.paint_offset;
      after_item.paint_offset = self.paint_offset;
      before_item.advance_for_layout = self.advance_for_layout.min(before_item.advance_for_layout);
      after_item.advance_for_layout =
        (self.advance_for_layout - before_item.advance_for_layout).max(0.0);
    }

    Some((before_item, after_item))
  }

  /// Applies letter- and word-spacing to shaped runs.
  ///
  /// Spacing is added after each cluster (except the final cluster).
  /// Word spacing stacks on top of letter spacing for space-like clusters.
  pub fn apply_spacing_to_runs(
    runs: &mut [ShapedRun],
    text: &str,
    letter_spacing: f32,
    word_spacing: f32,
  ) {
    if runs.is_empty() || text.is_empty() {
      return;
    }
    if letter_spacing == 0.0 && word_spacing == 0.0 {
      return;
    }

    #[derive(Debug)]
    struct ClusterRef {
      run_idx: usize,
      glyph_end: usize,
      offset: usize,
      is_space: bool,
    }

    let wants_word_spacing = word_spacing != 0.0;
    let text_bytes = text.as_bytes();

    let is_space_like_at_offset = |offset: usize| -> bool {
      if offset >= text_bytes.len() {
        return false;
      }
      match text_bytes[offset] {
        b' ' | b'\t' => true,
        // U+00A0 NBSP = 0xC2 0xA0
        0xC2 => text_bytes.get(offset + 1) == Some(&0xA0),
        _ => false,
      }
    };

    // Fast path for the common case: all shaped runs are already in logical LTR order, so we can
    // stream clusters without collecting/sorting a temporary vec.
    //
    // (Cluster values inside a LTR HarfBuzz buffer are monotonic under the default cluster level.)
    let mut can_stream_without_sort = true;
    let mut saw_run = false;
    let mut last_run_start = 0usize;
    for run in runs.iter() {
      if run.glyphs.is_empty() {
        continue;
      }
      if run.direction != crate::text::pipeline::Direction::LeftToRight {
        can_stream_without_sort = false;
        break;
      }
      if saw_run && run.start < last_run_start {
        can_stream_without_sort = false;
        break;
      }
      last_run_start = run.start;
      saw_run = true;
    }

    if can_stream_without_sort {
      #[cfg(test)]
      let mut cluster_count = 0usize;

      let mut pending: Option<(usize, usize, bool, InlineAxis)> = None;
      for run_idx in 0..runs.len() {
        if runs[run_idx].glyphs.is_empty() {
          continue;
        }

        // If there is a cluster before this run, it is not the final cluster overall, so apply
        // spacing to it now.
        if let Some((pending_run_idx, glyph_end, is_space, axis)) = pending.take() {
          let extra = letter_spacing + if is_space { word_spacing } else { 0.0 };
          if extra != 0.0 {
            if let Some(run) = runs.get_mut(pending_run_idx) {
              if !run.glyphs.is_empty() {
                let glyph_idx = glyph_end
                  .saturating_sub(1)
                  .min(run.glyphs.len().saturating_sub(1));
                if let Some(glyph) = run.glyphs.get_mut(glyph_idx) {
                  match axis {
                    InlineAxis::Horizontal => {
                      glyph.x_advance += extra;
                      run.advance += extra;
                    }
                    InlineAxis::Vertical => {
                      let before = glyph_inline_advance(glyph, axis);
                      add_inline_advance(glyph, axis, extra);
                      let after = glyph_inline_advance(glyph, axis);
                      run.advance += after - before;
                    }
                  }
                } else {
                  run.advance += extra;
                }
              }
            }
          }
        }

        let axis = run_inline_axis(&runs[run_idx]);
        let run_start = runs[run_idx].start;
        let run_len = runs[run_idx].glyphs.len();

        let run = &mut runs[run_idx];
        let mut prev_cluster: Option<(usize, bool)> = None;
        let mut idx = 0;
        while idx < run_len {
          let cluster_value = run.glyphs[idx].cluster;
          while idx < run_len && run.glyphs[idx].cluster == cluster_value {
            idx += 1;
          }
          let glyph_end = idx;

          #[cfg(test)]
          {
            cluster_count = cluster_count.saturating_add(1);
          }

          let offset = run_start.saturating_add(cluster_value as usize);
          let is_space = wants_word_spacing && is_space_like_at_offset(offset);

          if let Some((prev_end, prev_is_space)) = prev_cluster.take() {
            let extra = letter_spacing + if prev_is_space { word_spacing } else { 0.0 };
            if extra != 0.0 {
              let glyph_idx = prev_end
                .saturating_sub(1)
                .min(run.glyphs.len().saturating_sub(1));
              if let Some(glyph) = run.glyphs.get_mut(glyph_idx) {
                match axis {
                  InlineAxis::Horizontal => {
                    glyph.x_advance += extra;
                    run.advance += extra;
                  }
                  InlineAxis::Vertical => {
                    let before = glyph_inline_advance(glyph, axis);
                    add_inline_advance(glyph, axis, extra);
                    let after = glyph_inline_advance(glyph, axis);
                    run.advance += after - before;
                  }
                }
              } else {
                run.advance += extra;
              }
            }
          }

          prev_cluster = Some((glyph_end, is_space));
        }

        if let Some((glyph_end, is_space)) = prev_cluster {
          pending = Some((run_idx, glyph_end, is_space, axis));
        }
      }

      #[cfg(test)]
      APPLY_SPACING_CLUSTER_COUNT.with(|count| {
        count.set(count.get().saturating_add(cluster_count));
      });

      #[cfg(test)]
      APPLY_SPACING_STREAM_COUNT.with(|count| {
        count.set(count.get().saturating_add(1));
      });

      return;
    }

    // Fallback: collect all clusters and sort if we observe any non-monotonic offsets. This handles
    // RTL runs, mixed-direction segments, and any unexpected shaping output ordering.
    //
    // Most runs are cluster-trivial (1 cluster per glyph). Reserve to avoid repeated reallocation on
    // long paragraphs.
    let mut clusters: Vec<ClusterRef> =
      Vec::with_capacity(runs.iter().map(|r| r.glyphs.len()).sum());
    let mut last_offset: Option<usize> = None;
    let mut needs_sort = false;
    let mut monotonic_decreasing = true;

    // Cluster offsets are defined in logical (source-text) order. When runs/glyphs are already
    // monotonic, we can avoid a full O(n log n) cluster sort by iterating runs in start order and
    // scanning RTL buffers backwards (so cluster offsets are discovered in increasing order).
    let mut run_starts_increasing = true;
    let mut run_starts_decreasing = true;
    let mut last_start: Option<usize> = None;
    for run in runs.iter() {
      if run.glyphs.is_empty() {
        continue;
      }
      if let Some(prev) = last_start {
        if run.start < prev {
          run_starts_increasing = false;
        }
        if run.start > prev {
          run_starts_decreasing = false;
        }
      }
      last_start = Some(run.start);
    }

    let mut sorted_run_indices: Vec<usize> = Vec::new();
    if !run_starts_increasing && !run_starts_decreasing {
      sorted_run_indices = (0..runs.len()).collect();
      sorted_run_indices.sort_by_key(|idx| runs[*idx].start);
    }

    let mut collect_for_run = |run_idx: usize| {
      let run = &runs[run_idx];
      if run.glyphs.is_empty() {
        return;
      }

      let run_start = run.start;
      let run_len = run.glyphs.len();
      debug_assert!(run_len > 0);

      // HarfBuzz may emit RTL runs in visual order with cluster values descending. Scan the buffer
      // from whichever end produces increasing cluster offsets so we can preserve monotonicity.
      let scan_forward = run.glyphs[0].cluster <= run.glyphs[run_len - 1].cluster;

      if scan_forward {
        let mut idx = 0;
        while idx < run_len {
          let cluster_value = run.glyphs[idx].cluster;
          while idx < run_len && run.glyphs[idx].cluster == cluster_value {
            idx += 1;
          }
          let glyph_end = idx;

          let offset = run_start.saturating_add(cluster_value as usize);
          let is_space = wants_word_spacing && is_space_like_at_offset(offset);

          if let Some(prev) = last_offset {
            if offset < prev {
              needs_sort = true;
            }
            if offset > prev {
              monotonic_decreasing = false;
            }
          }
          last_offset = Some(offset);

          clusters.push(ClusterRef {
            run_idx,
            glyph_end,
            offset,
            is_space,
          });
        }
      } else {
        let mut idx = run_len;
        while idx > 0 {
          let end = idx;
          let cluster_value = run.glyphs[end - 1].cluster;
          idx = idx.saturating_sub(1);
          while idx > 0 && run.glyphs[idx - 1].cluster == cluster_value {
            idx -= 1;
          }
          let glyph_end = end;

          let offset = run_start.saturating_add(cluster_value as usize);
          let is_space = wants_word_spacing && is_space_like_at_offset(offset);

          if let Some(prev) = last_offset {
            if offset < prev {
              needs_sort = true;
            }
            if offset > prev {
              monotonic_decreasing = false;
            }
          }
          last_offset = Some(offset);

          clusters.push(ClusterRef {
            run_idx,
            glyph_end,
            offset,
            is_space,
          });
        }
      }
    };

    if run_starts_increasing {
      for run_idx in 0..runs.len() {
        collect_for_run(run_idx);
      }
    } else if run_starts_decreasing {
      for run_idx in (0..runs.len()).rev() {
        collect_for_run(run_idx);
      }
    } else {
      for run_idx in sorted_run_indices {
        collect_for_run(run_idx);
      }
    }

    #[cfg(test)]
    APPLY_SPACING_CLUSTER_COUNT.with(|count| {
      count.set(count.get().saturating_add(clusters.len()));
    });

    if clusters.len() <= 1 {
      return;
    }

    if needs_sort {
      if monotonic_decreasing {
        clusters.reverse();
      } else {
        clusters.sort_unstable_by_key(|c| c.offset);
        #[cfg(test)]
        APPLY_SPACING_SORT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
      }
    }

    // Apply spacing to the last glyph of each cluster (except the final cluster).
    for cluster in clusters.iter().take(clusters.len().saturating_sub(1)) {
      let extra = letter_spacing + if cluster.is_space { word_spacing } else { 0.0 };
      if extra == 0.0 {
        continue;
      }

      let run_idx = cluster.run_idx;
      let Some(run) = runs.get_mut(run_idx) else {
        continue;
      };
      if run.glyphs.is_empty() {
        continue;
      }

      let axis = run_inline_axis(run);

      // `cluster.glyph_end` is derived from shaping output; clamp defensively so out-of-range
      // cluster indices (e.g. after glyph compaction) still apply spacing to the last glyph.
      debug_assert!(cluster.glyph_end > 0);
      let glyph_idx = cluster
        .glyph_end
        .saturating_sub(1)
        .min(run.glyphs.len().saturating_sub(1));
      let Some(glyph) = run.glyphs.get_mut(glyph_idx) else {
        continue;
      };

      match axis {
        InlineAxis::Horizontal => {
          glyph.x_advance += extra;
          run.advance += extra;
        }
        InlineAxis::Vertical => {
          let before = glyph_inline_advance(glyph, axis);
          add_inline_advance(glyph, axis, extra);
          let after = glyph_inline_advance(glyph, axis);
          run.advance += after - before;
        }
      }
    }
  }

  fn is_expandable_space_for_justify(ch: char) -> bool {
    matches!(ch, ' ' | '\t')
  }

  /// Counts internal justification opportunities for `text-justify: inter-word`.
  ///
  /// A word-boundary opportunity is counted after an **expandable space** cluster (`' '` or `'\t'`)
  /// when the next cluster/character is **not** an expandable space.
  ///
  /// Trailing spaces are not counted (no "next" character).
  pub fn count_inter_word_justify_opportunities(&self) -> usize {
    if self.text.is_empty() {
      return 0;
    }

    self
      .cluster_advances
      .iter()
      .filter_map(|b| {
        let offset = b.byte_offset;
        if offset == 0 || offset >= self.text.len() {
          return None;
        }
        // `cluster_advances` byte offsets are aligned to UTF-8 char boundaries.
        let prev = self.text[..offset].chars().next_back()?;
        let next = self.text[offset..].chars().next()?;
        Some((prev, next))
      })
      .filter(|(prev, next)| {
        Self::is_expandable_space_for_justify(*prev)
          && !Self::is_expandable_space_for_justify(*next)
      })
      .count()
  }

  /// Counts internal justification opportunities for `text-justify: inter-character|distribute`.
  ///
  /// Opportunities are evaluated at shaped cluster boundaries so we do not split grapheme clusters
  /// (combining marks, ZWJ emoji sequences, etc).
  pub fn count_inter_character_justify_opportunities(&self) -> usize {
    if self.text.is_empty() || self.is_marker {
      return 0;
    }

    self
      .cluster_advances
      .iter()
      .filter_map(|b| {
        let offset = b.byte_offset;
        if offset == 0 || offset >= self.text.len() {
          return None;
        }
        // `cluster_advances` byte offsets are aligned to UTF-8 char boundaries.
        let prev = self.text[..offset].chars().next_back()?;
        let next = self.text[offset..].chars().next()?;
        Some((prev, next))
      })
      .filter(|(prev, next)| allows_inter_character_expansion(Some(*prev), Some(*next)))
      .count()
  }

  /// Counts internal justification opportunities for `text-justify: distribute`.
  ///
  /// `distribute` combines inter-word (expandable ASCII spaces/tabs) and inter-character expansion.
  /// Evaluated at shaped cluster boundaries to avoid splitting grapheme clusters.
  pub fn count_distribute_justify_opportunities(&self) -> usize {
    if self.text.is_empty() || self.is_marker {
      return 0;
    }

    self
      .cluster_advances
      .iter()
      .filter_map(|b| {
        let offset = b.byte_offset;
        if offset == 0 || offset >= self.text.len() {
          return None;
        }
        // `cluster_advances` byte offsets are aligned to UTF-8 char boundaries.
        let prev = self.text[..offset].chars().next_back()?;
        let next = self.text[offset..].chars().next()?;
        Some((prev, next))
      })
      .filter(|(prev, next)| {
        let inter_word = Self::is_expandable_space_for_justify(*prev)
          && !Self::is_expandable_space_for_justify(*next);
        inter_word || allows_inter_character_expansion(Some(*prev), Some(*next))
      })
      .count()
  }

  /// Applies in-place `text-justify: inter-word` expansion inside a single shaped `TextItem`.
  ///
  /// Returns the number of opportunities expanded.
  pub fn apply_inter_word_justification(&mut self, gap_extra: f32) -> usize {
    self.apply_internal_justification(gap_extra, |prev, next| {
      Self::is_expandable_space_for_justify(prev) && !Self::is_expandable_space_for_justify(next)
    })
  }

  /// Applies in-place `text-justify: inter-character|distribute` expansion inside a single shaped
  /// `TextItem`.
  ///
  /// Returns the number of opportunities expanded.
  pub fn apply_inter_character_justification(&mut self, gap_extra: f32) -> usize {
    if self.is_marker {
      return 0;
    }
    self.apply_internal_justification(gap_extra, |prev, next| {
      allows_inter_character_expansion(Some(prev), Some(next))
    })
  }

  /// Applies in-place `text-justify: distribute` expansion inside a single shaped `TextItem`.
  ///
  /// `distribute` expands both inter-word (spaces/tabs) and inter-character opportunities.
  /// Returns the number of opportunities expanded.
  pub fn apply_distribute_justification(&mut self, gap_extra: f32) -> usize {
    if self.is_marker {
      return 0;
    }
    self.apply_internal_justification(gap_extra, |prev, next| {
      (Self::is_expandable_space_for_justify(prev) && !Self::is_expandable_space_for_justify(next))
        || allows_inter_character_expansion(Some(prev), Some(next))
    })
  }

  /// Applies extra justification space to the **trailing** expandable-space cluster of this text
  /// item.
  ///
  /// This is used when an inter-word justification opportunity spans inline item boundaries (for
  /// example, `<span>a </span><span>b</span>`). In that scenario the trailing space lives in this
  /// `TextItem`, but the "next" non-space character lives in a different item, so the standard
  /// internal scan (which only looks at cluster boundaries *inside* this text run) would miss the
  /// opportunity.
  ///
  /// Returning `true` indicates that the extra space was applied to the underlying shaped run so
  /// the owning inline box grows (backgrounds/borders cover the expanded space) instead of
  /// inserting an external gap between inline boxes.
  pub fn apply_trailing_space_boundary_justification(&mut self, gap_extra: f32) -> bool {
    if gap_extra == 0.0 || self.text.is_empty() || self.is_marker {
      return false;
    }
    let Some(last_char) = self.text.chars().next_back() else {
      return false;
    };
    if !Self::is_expandable_space_for_justify(last_char) {
      return false;
    }
    // When the item is *only* whitespace (e.g. an anonymous inline box generated from an
    // inter-element space), keep legacy behavior and let justification insert spacing between
    // items. This preserves display-list expectations that the whitespace glyph itself is not
    // widened.
    if self.text.chars().all(Self::is_expandable_space_for_justify) {
      return false;
    }

    if self.cluster_advances.is_empty() {
      self.advance += gap_extra;
      self.advance_for_layout = self.advance;
      return true;
    }

    // Fast path for synthetic items used in tests that don't carry shaped runs.
    if self.runs.is_empty() {
      self.advance += gap_extra;
      self.advance_for_layout = self.advance;
      let text_len = self.text.len();
      for boundary in &mut self.cluster_advances {
        if boundary.byte_offset == text_len {
          boundary.advance += gap_extra;
          boundary.run_advance += gap_extra;
        }
      }
      return true;
    }

    let Some((run_idx, glyph_end)) = self
      .cluster_advances
      .iter()
      .rev()
      .find_map(|b| b.run_index.zip(b.glyph_end))
    else {
      // Should not happen for shaped runs, but keep justification monotonic if cluster metadata is
      // missing.
      self.advance += gap_extra;
      self.advance_for_layout = self.advance;
      if let Some(last) = self.cluster_advances.last_mut() {
        last.advance += gap_extra;
        last.run_advance += gap_extra;
      }
      return true;
    };

    if let Some(run) = self.runs.get_mut(run_idx) {
      if run.glyphs.is_empty() {
        run.advance += gap_extra;
      } else {
        let axis = run_inline_axis(run);
        let last_glyph = glyph_end.saturating_sub(1);
        if let Some(glyph) = run.glyphs.get_mut(last_glyph) {
          add_inline_advance(glyph, axis, gap_extra);
        } else if let Some(glyph) = run.glyphs.last_mut() {
          add_inline_advance(glyph, axis, gap_extra);
        }
        run.advance += gap_extra;
      }
    }

    self.cluster_advances = Self::compute_cluster_advances(&self.runs, &self.text, self.font_size);
    let new_advance = self
      .cluster_advances
      .last()
      .map(|c| c.advance)
      .unwrap_or_else(|| self.runs.iter().map(|r| r.advance).sum());
    self.advance = new_advance;
    self.advance_for_layout = self.advance;
    true
  }

  fn apply_internal_justification(
    &mut self,
    gap_extra: f32,
    mut is_opportunity: impl FnMut(char, char) -> bool,
  ) -> usize {
    if gap_extra == 0.0 || self.text.is_empty() {
      return 0;
    }

    let outside_marker = self.is_marker && self.advance_for_layout.abs() <= f32::EPSILON;
    let marker_gap = (self.is_marker && !outside_marker)
      .then(|| (self.advance_for_layout - self.advance).max(0.0))
      .unwrap_or(0.0);

    // Fast path for synthetic items used in tests that don't carry shaped runs.
    if self.runs.is_empty() {
      let mut count = 0usize;
      let mut cumulative = 0.0;

      for boundary in &mut self.cluster_advances {
        let offset = boundary.byte_offset;
        if offset == 0 || offset >= self.text.len() {
          boundary.advance += cumulative;
          boundary.run_advance += cumulative;
          continue;
        }
        let Some(prev) = self.text[..offset].chars().next_back() else {
          boundary.advance += cumulative;
          boundary.run_advance += cumulative;
          continue;
        };
        let Some(next) = self.text[offset..].chars().next() else {
          boundary.advance += cumulative;
          boundary.run_advance += cumulative;
          continue;
        };

        if is_opportunity(prev, next) {
          count += 1;
          cumulative += gap_extra;
        }

        boundary.advance += cumulative;
        boundary.run_advance += cumulative;
      }

      if count == 0 {
        return 0;
      }

      self.advance += gap_extra * count as f32;
      if self.is_marker {
        if outside_marker {
          self.advance_for_layout = 0.0;
        } else {
          self.advance_for_layout = self.advance + marker_gap;
        }
      } else {
        self.advance_for_layout = self.advance;
      }

      return count;
    }

    let mut run_glyph_extras: Vec<Vec<(usize, f32)>> = vec![Vec::new(); self.runs.len()];
    let mut run_end_extras: Vec<f32> = vec![0.0; self.runs.len()];
    let mut count = 0usize;

    for boundary in &self.cluster_advances {
      let offset = boundary.byte_offset;
      if offset == 0 || offset >= self.text.len() {
        continue;
      }

      let Some(prev) = self.text[..offset].chars().next_back() else {
        continue;
      };
      let Some(next) = self.text[offset..].chars().next() else {
        continue;
      };

      if !is_opportunity(prev, next) {
        continue;
      }

      count += 1;

      let Some(run_idx) = boundary.run_index else {
        continue;
      };
      if let Some(glyph_end) = boundary.glyph_end {
        let last_glyph = glyph_end.saturating_sub(1);
        run_glyph_extras[run_idx].push((last_glyph, gap_extra));
      } else {
        run_end_extras[run_idx] += gap_extra;
      }
    }

    if count == 0 {
      return 0;
    }

    for (run_idx, run) in self.runs.iter_mut().enumerate() {
      if run.glyphs.is_empty() {
        let extra = run_end_extras.get(run_idx).copied().unwrap_or(0.0);
        if extra != 0.0 {
          run.advance += extra;
        }
        continue;
      }

      let axis = run_inline_axis(run);
      let mut extra_by_glyph = vec![0.0; run.glyphs.len()];
      for (glyph_idx, extra) in run_glyph_extras
        .get(run_idx)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
      {
        if *glyph_idx < extra_by_glyph.len() {
          extra_by_glyph[*glyph_idx] += *extra;
        }
      }

      let mut new_advance = 0.0;

      for (idx, glyph) in run.glyphs.iter_mut().enumerate() {
        let extra = extra_by_glyph[idx];
        if extra != 0.0 {
          add_inline_advance(glyph, axis, extra);
        }
        new_advance += glyph_inline_advance(glyph, axis);
      }

      run.advance = new_advance;
    }

    self.cluster_advances = Self::compute_cluster_advances(&self.runs, &self.text, self.font_size);
    let new_advance = self
      .cluster_advances
      .last()
      .map(|c| c.advance)
      .unwrap_or_else(|| self.runs.iter().map(|r| r.advance).sum());
    self.advance = new_advance;

    if self.is_marker {
      if outside_marker {
        self.advance_for_layout = 0.0;
      } else {
        self.advance_for_layout = self.advance + marker_gap;
      }
    } else {
      self.advance_for_layout = self.advance;
    }

    count
  }

  /// Gets the horizontal advance at a given byte offset
  pub fn advance_at_offset(&self, byte_offset: usize) -> f32 {
    if byte_offset == 0 {
      return 0.0;
    }
    if byte_offset >= self.text.len() {
      return self.advance;
    }

    if let Some(boundary) = self.cluster_boundary_at_or_before(byte_offset) {
      return boundary.advance;
    }

    if self.runs.is_empty() {
      let char_count = self.text.chars().count().max(1);
      let offset_chars = self.text[..byte_offset].chars().count();
      return self.advance * (offset_chars as f32 / char_count as f32);
    }

    // Fallback: linear approximation if cluster data is missing
    let char_count = self.text.chars().count().max(1);
    let offset_chars = self.text[..byte_offset].chars().count();
    self.advance * (offset_chars as f32 / char_count as f32)
  }

  /// Finds the best break point that fits within max_width
  pub fn find_break_point(&self, max_width: f32) -> Option<BreakOpportunity> {
    let max_width_with_epsilon = max_width + LINE_PIXEL_FIT_EPSILON;

    if let Some(mandatory) = self.first_mandatory_break {
      if self.effective_advance_at_break(&mandatory) <= max_width_with_epsilon {
        return Some(mandatory);
      }
    }

    // Scan all known break opportunities and keep the *latest* break that fits. We cannot rely on
    // `offset_for_width(max_width)` because that uses raw advances, which include trailing spaces.
    // Collapsible spaces are trimmed at line breaks, so a break that appears to overflow by the
    // width of a single space may still fit once that space is removed.
    let mut normal_allowed_break: Option<BreakOpportunity> = None;
    let mut emergency_allowed_break: Option<BreakOpportunity> = None;
    for brk in &self.break_opportunities {
      if !matches!(brk.break_type, BreakType::Allowed) || brk.byte_offset == 0 {
        continue;
      }
      if self.effective_advance_at_break(brk) <= max_width_with_epsilon {
        match brk.kind {
          BreakOpportunityKind::Normal => normal_allowed_break = Some(*brk),
          BreakOpportunityKind::Emergency => emergency_allowed_break = Some(*brk),
        }
      }
    }

    let mut best_break = normal_allowed_break.or(emergency_allowed_break);
    if best_break.is_some() || !allows_soft_wrap(self.style.as_ref()) {
      return best_break;
    }

    let can_overflow_break_word = matches!(
      self.style.word_break,
      WordBreak::BreakWord | WordBreak::Anywhere
    );
    let can_overflow_break_by_wrap = matches!(
      self.style.overflow_wrap,
      OverflowWrap::BreakWord | OverflowWrap::Anywhere
    );

    if (can_overflow_break_word || can_overflow_break_by_wrap) && best_break.is_none() {
      if let Some(mut offset) = self.offset_for_width(max_width) {
        // `offset_for_width` can return `text.len()` when `max_width` exceeds the full advance.
        // Returning a break opportunity at the end doesn't actually split the text and differs
        // from the legacy break scan (which never proposes `text.len()`).
        if offset >= self.text.len() {
          offset =
            Self::previous_char_boundary_in_text(&self.text, self.text.len().saturating_sub(1));
        }
        if offset > 0 {
          best_break = Some(BreakOpportunity::emergency(offset));
        }
      }
    }

    best_break
  }

  fn effective_advance_at_break(&self, brk: &BreakOpportunity) -> f32 {
    let mut offset = brk.byte_offset.min(self.text.len());
    offset = Self::previous_char_boundary_in_text(&self.text, offset);

    // CSS Text whitespace handling affects line fitting at break opportunities:
    //
    // - For `white-space: normal | nowrap | pre-line`, trailing *collapsible* spaces at line ends
    //   are removed.
    // - For `white-space: pre-wrap`, trailing preserved spaces can be *hanging*: they are not
    //   considered when measuring whether the line fits, even though they still paint.
    //
    // Spec: https://drafts.csswg.org/css-text-3/#white-space-processing
    let trims_or_hangs_trailing_spaces =
      matches!(
        self.style.white_space,
        WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
      ) || (matches!(self.style.white_space, WhiteSpace::PreWrap)
        && !matches!(self.style.text_wrap, TextWrap::NoWrap));
    if trims_or_hangs_trailing_spaces {
      if let Some(prefix) = self.text.get(..offset) {
        let trimmed_len = prefix.trim_end_matches(' ').len();
        return self.advance_at_offset(trimmed_len);
      }
    }

    self.advance_at_offset(offset)
  }

  fn offset_for_width(&self, max_width: f32) -> Option<usize> {
    if self.cluster_advances.is_empty() {
      return None;
    }

    let max_width = max_width + LINE_PIXEL_FIT_EPSILON;
    let idx = self
      .cluster_advances
      .partition_point(|b| b.advance <= max_width);
    if idx == 0 {
      return None;
    }

    self.cluster_advances.get(idx - 1).map(|b| b.byte_offset)
  }

  fn allowed_break_at_or_before_with_kind(
    &self,
    byte_offset: usize,
    kind: crate::text::line_break::BreakOpportunityKind,
  ) -> Option<BreakOpportunity> {
    if self.break_opportunities.is_empty() {
      return None;
    }

    let mut idx = self
      .break_opportunities
      .partition_point(|b| b.byte_offset <= byte_offset);
    while idx > 0 {
      idx -= 1;
      let brk = self.break_opportunities[idx];
      if matches!(brk.break_type, BreakType::Allowed) && brk.kind == kind {
        return Some(brk);
      }
    }

    None
  }

  fn cluster_boundary_at_or_before(&self, byte_offset: usize) -> Option<ClusterBoundary> {
    if self.cluster_advances.is_empty() {
      return None;
    }

    let mut idx = match self
      .cluster_advances
      .binary_search_by_key(&byte_offset, |c| c.byte_offset)
    {
      Ok(i) => i,
      Err(i) => i.checked_sub(1)?,
    };

    // If there are multiple entries for the same offset, keep the one with the largest advance
    while idx + 1 < self.cluster_advances.len()
      && self.cluster_advances[idx + 1].byte_offset == self.cluster_advances[idx].byte_offset
    {
      idx += 1;
    }

    self.cluster_advances.get(idx).cloned()
  }

  fn cluster_boundary_exact(&self, byte_offset: usize) -> Option<&ClusterBoundary> {
    if self.cluster_advances.is_empty() {
      return None;
    }

    let mut idx = self
      .cluster_advances
      .binary_search_by_key(&byte_offset, |c| c.byte_offset)
      .ok()?;
    while idx + 1 < self.cluster_advances.len()
      && self.cluster_advances[idx + 1].byte_offset == byte_offset
    {
      idx += 1;
    }

    self.cluster_advances.get(idx)
  }

  fn compute_cluster_advances(
    runs: &[ShapedRun],
    text: &str,
    fallback_font_size: f32,
  ) -> Vec<ClusterBoundary> {
    let text_len = text.len();
    if text_len == 0 {
      return Vec::new();
    }
    if runs.is_empty() {
      let estimated = (text.chars().count() as f32) * fallback_font_size * 0.5;
      return vec![ClusterBoundary {
        byte_offset: text_len,
        advance: estimated,
        run_index: None,
        glyph_end: None,
        run_advance: estimated,
      }];
    }

    // Cluster boundaries are at most "one per glyph" (cluster-trivial shaping) plus a small number
    // of run-ending sentinels. Reserving upfront avoids repeated reallocations on long paragraphs.
    let estimated_glyphs = runs
      .iter()
      .fold(0usize, |acc, run| acc.saturating_add(run.glyphs.len()));
    let mut advances: Vec<ClusterBoundary> =
      Vec::with_capacity(estimated_glyphs.saturating_add(runs.len()));
    let mut cumulative = 0.0;
    let mut process_run = |run_idx: usize, advances: &mut Vec<ClusterBoundary>, cumulative: &mut f32| {
      let run = &runs[run_idx];
      let axis = run_inline_axis(run);
      if run.glyphs.is_empty() {
        if run.advance > 0.0 {
          let run_advance = run.advance.max(0.0);
          *cumulative += run_advance;
          advances.push(ClusterBoundary {
            byte_offset: Self::previous_char_boundary_in_text(text, run.end).min(text_len),
            advance: *cumulative,
            run_index: Some(run_idx),
            glyph_end: None,
            run_advance,
          });
        }
        return;
      }

      let mut glyph_idx = 0;
      let mut run_advance = 0.0;
      while glyph_idx < run.glyphs.len() {
        let cluster_value = run.glyphs[glyph_idx].cluster as usize;
        let mut cluster_width = 0.0;
        while glyph_idx < run.glyphs.len()
          && run.glyphs[glyph_idx].cluster as usize == cluster_value
        {
          cluster_width += glyph_inline_advance(&run.glyphs[glyph_idx], axis);
          glyph_idx += 1;
        }

        run_advance += cluster_width;
        *cumulative += cluster_width;
        // `glyph.cluster` values point to the start of the cluster in logical text order. For
        // line-breaking we need cumulative advances *at* boundary positions, which are the end
        // offsets of clusters (the start of the next cluster, or the run end).
        let next_cluster_value = run
          .glyphs
          .get(glyph_idx)
          .map(|g| g.cluster as usize)
          .unwrap_or_else(|| run.text.len());
        let offset =
          Self::previous_char_boundary_in_text(text, run.start + next_cluster_value).min(text_len);
        advances.push(ClusterBoundary {
          byte_offset: offset,
          advance: *cumulative,
          run_index: Some(run_idx),
          glyph_end: Some(glyph_idx),
          run_advance,
        });
      }

      let run_end = Self::previous_char_boundary_in_text(text, run.end).min(text_len);
      if advances
        .last()
        .map(|b| b.byte_offset != run_end)
        .unwrap_or(true)
      {
        advances.push(ClusterBoundary {
          byte_offset: run_end,
          advance: *cumulative,
          run_index: Some(run_idx),
          glyph_end: Some(run.glyphs.len()),
          run_advance,
        });
      }
    };

    // Runs are usually already in increasing logical order for LTR text. Avoid allocating an
    // index vector + sorting in that common case.
    let mut needs_sort = false;
    let mut monotonic_decreasing = true;
    let mut last_start: Option<usize> = None;
    for run in runs.iter() {
      if let Some(prev) = last_start {
        if run.start < prev {
          needs_sort = true;
        }
        if run.start > prev {
          monotonic_decreasing = false;
        }
      }
      last_start = Some(run.start);
    }

    if !needs_sort {
      for run_idx in 0..runs.len() {
        process_run(run_idx, &mut advances, &mut cumulative);
      }
    } else if monotonic_decreasing {
      for run_idx in (0..runs.len()).rev() {
        process_run(run_idx, &mut advances, &mut cumulative);
      }
    } else {
      let mut run_indices: Vec<usize> = (0..runs.len()).collect();
      run_indices.sort_by_key(|idx| runs[*idx].start);
      for run_idx in run_indices {
        process_run(run_idx, &mut advances, &mut cumulative);
      }
    }

    // Deduplicate by byte offset, keeping the greatest advance so that cumulative width remains monotonic.
    let mut deduped: Vec<ClusterBoundary> = Vec::with_capacity(advances.len());
    for boundary in advances {
      if let Some(last) = deduped.last_mut() {
        if last.byte_offset == boundary.byte_offset {
          if boundary.advance > last.advance
            || last.run_index.is_none()
            || (last.glyph_end.is_none() && boundary.glyph_end.is_some())
          {
            *last = boundary;
          }
          continue;
        }
      }
      deduped.push(boundary);
    }

    if deduped
      .last()
      .map(|b| b.byte_offset < text_len)
      .unwrap_or(false)
    {
      deduped.push(ClusterBoundary {
        byte_offset: text_len,
        advance: deduped.last().map(|b| b.advance).unwrap_or(0.0),
        run_index: None,
        glyph_end: None,
        run_advance: deduped
          .last()
          .map(|b| b.run_advance)
          .unwrap_or(fallback_font_size),
      });
    }

    if deduped.is_empty() {
      deduped.push(ClusterBoundary {
        byte_offset: text_len,
        advance: fallback_font_size,
        run_index: None,
        glyph_end: None,
        run_advance: fallback_font_size,
      });
    }

    deduped
  }

  fn previous_char_boundary_in_text(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
      return text.len();
    }
    if text.is_char_boundary(offset) {
      return offset;
    }

    text
      .char_indices()
      .take_while(|(idx, _)| *idx < offset)
      .map(|(idx, _)| idx)
      .last()
      .unwrap_or(0)
  }

  fn align_breaks_to_clusters(
    breaks: Vec<BreakOpportunity>,
    clusters: &[ClusterBoundary],
    text_len: usize,
  ) -> Vec<BreakOpportunity> {
    if clusters.is_empty() || text_len == 0 {
      return breaks;
    }

    let mut aligned: Vec<BreakOpportunity> = Vec::new();
    for brk in breaks {
      let clamped_offset = brk.byte_offset.min(text_len);
      // Break opportunities are tracked in *byte offsets* (as produced by UAX#14). We try to align
      // them to cluster boundaries so `advance_at_offset` can use shaped advances without falling
      // back to linear approximations.
      //
      // However, mandatory breaks (e.g. preserved newlines under `white-space: pre-wrap`) must
      // never be shifted: they intentionally split shaping/kerning and are reshaped independently
      // when `split_at` is invoked.
      let offset = if matches!(brk.break_type, BreakType::Mandatory) {
        clamped_offset
      } else {
        Self::cluster_offset_at_or_before(clamped_offset, clusters).unwrap_or(clamped_offset)
      };

      if let Some(last) = aligned.last_mut() {
        if last.byte_offset == offset {
          if brk.break_type == BreakType::Mandatory || last.break_type == BreakType::Mandatory {
            last.break_type = BreakType::Mandatory;
            last.kind = crate::text::line_break::BreakOpportunityKind::Normal;
          } else if brk.kind == crate::text::line_break::BreakOpportunityKind::Normal {
            last.kind = crate::text::line_break::BreakOpportunityKind::Normal;
          }
          last.adds_hyphen |= brk.adds_hyphen;
          continue;
        }
      }

      aligned.push(BreakOpportunity::with_hyphen_and_kind(
        offset,
        brk.break_type,
        brk.adds_hyphen,
        brk.kind,
      ));
    }

    aligned
  }

  fn first_mandatory_break(breaks: &[BreakOpportunity]) -> Option<BreakOpportunity> {
    breaks
      .iter()
      .find(|b| matches!(b.break_type, BreakType::Mandatory))
      .copied()
  }

  fn hash_text(text: &str) -> u64 {
    let mut hasher = FxHasher::default();
    text.hash(&mut hasher);
    hasher.finish()
  }

  fn cluster_offset_at_or_before(target: usize, clusters: &[ClusterBoundary]) -> Option<usize> {
    if clusters.is_empty() {
      return None;
    }

    match clusters.binary_search_by_key(&target, |c| c.byte_offset) {
      Ok(idx) => clusters.get(idx).map(|c| c.byte_offset),
      Err(idx) => clusters.get(idx.checked_sub(1)?).map(|c| c.byte_offset),
    }
  }

  /// Splits existing shaped runs at a cluster boundary without reshaping, preserving ligatures/glyph IDs.
  fn split_runs_preserving_shaping(
    &self,
    split_offset: usize,
  ) -> Option<(Vec<ShapedRun>, Vec<ShapedRun>)> {
    let boundary = self.cluster_boundary_exact(split_offset)?;
    let run_idx = boundary.run_index?;
    let glyph_split = boundary.glyph_end?;
    let run = self.runs.get(run_idx)?;

    if split_offset < run.start || split_offset > run.end {
      return None;
    }

    let local = split_offset.checked_sub(run.start)?;
    if local > run.text.len() || !run.text.is_char_boundary(local) {
      return None;
    }

    let left_text = run.text.get(..local)?;
    let right_text = run.text.get(local..)?;

    let mut before_runs: Vec<ShapedRun> = self.runs.iter().take(run_idx).cloned().collect();
    let mut after_runs: Vec<ShapedRun> = self
      .runs
      .iter()
      .skip(run_idx + 1)
      .cloned()
      .map(|mut r| {
        r.start = r.start.saturating_sub(split_offset);
        r.end = r.end.saturating_sub(split_offset);
        r
      })
      .collect();

    let left_glyphs = run.glyphs[..glyph_split.min(run.glyphs.len())].to_vec();
    let mut right_glyphs = run.glyphs[glyph_split.min(run.glyphs.len())..].to_vec();

    let left_advance = boundary.run_advance;

    for glyph in &mut right_glyphs {
      glyph.cluster = glyph.cluster.saturating_sub(local as u32);
    }

    if !left_glyphs.is_empty() {
      let mut left_run = run.clone();
      left_run.text = left_text.to_string();
      left_run.end = split_offset;
      left_run.glyphs = left_glyphs;
      left_run.advance = left_advance;
      before_runs.push(left_run);
    }

    if !right_glyphs.is_empty() {
      let mut right_run = run.clone();
      right_run.text = right_text.to_string();
      right_run.start = 0;
      right_run.end = right_run.text.len();
      right_run.glyphs = right_glyphs;
      let run_axis = run_inline_axis(&right_run);
      right_run.advance = right_run
        .glyphs
        .iter()
        .map(|g| glyph_inline_advance(g, run_axis))
        .sum();
      after_runs.insert(0, right_run);
    }

    Some((before_runs, after_runs))
  }
}

impl ReshapeCache {
  pub(crate) fn clear(&mut self) {
    self.runs.clear();
  }

  pub fn shape(
    &mut self,
    item: &TextItem,
    range: Range<usize>,
    shaper: &ShapingPipeline,
    font_context: &FontContext,
  ) -> Option<Vec<ShapedRun>> {
    let text_slice = item.text.get(range.clone())?;
    let style_hash = shaping_style_hash(item.style.as_ref());
    let key = ReshapeCacheKey {
      style_hash,
      font_generation: font_context.font_generation(),
      text_id: item.source_id,
      range_start: item.source_range.start + range.start,
      range_end: item.source_range.start + range.end,
      base_direction_rtl: matches!(item.base_direction, Direction::Rtl),
      explicit_bidi: item
        .explicit_bidi
        .map(|ctx| (ctx.level.number(), ctx.override_all)),
      letter_spacing_bits: f32_to_canonical_bits(item.style.letter_spacing),
      word_spacing_bits: f32_to_canonical_bits(item.style.word_spacing),
    };

    if self.diagnostics_enabled {
      INLINE_RESHAPE_CACHE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    }

    if let Some(cached) = self.runs.get(&key) {
      if self.diagnostics_enabled {
        INLINE_RESHAPE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
      }
      // The shaping pipeline records diagnostics on its own cache hits, but this reshape cache
      // bypasses `ShapingPipeline::shape_with_context`. Mirror the accounting here so per-render
      // diagnostics counts remain stable even when line-breaking reuses cached substrings.
      if crate::text::pipeline::text_diagnostics_enabled() {
        let glyphs: usize = cached.iter().map(|run| run.glyphs.len()).sum();
        crate::text::pipeline::record_text_shape(None, cached.len(), glyphs);
      }
      return Some((**cached).clone());
    }

    let mut runs = shaper
      .shape_with_context_hashed(
        text_slice,
        &item.style,
        font_context,
        pipeline_dir_from_style(item.base_direction),
        item.explicit_bidi,
        style_hash,
      )
      .ok()?;
    TextItem::apply_spacing_to_runs(
      &mut runs,
      text_slice,
      item.style.letter_spacing,
      item.style.word_spacing,
    );
    self.runs.insert(key, Arc::new(runs.clone()));
    if self.diagnostics_enabled {
      INLINE_RESHAPE_CACHE_STORES.fetch_add(1, Ordering::Relaxed);
    }
    Some(runs)
  }
}

fn run_inline_axis(run: &ShapedRun) -> InlineAxis {
  if run.vertical {
    InlineAxis::Vertical
  } else {
    InlineAxis::Horizontal
  }
}

fn glyph_inline_advance(glyph: &crate::text::pipeline::GlyphPosition, axis: InlineAxis) -> f32 {
  match axis {
    InlineAxis::Horizontal => glyph.x_advance,
    InlineAxis::Vertical => {
      if glyph.y_advance.abs() > f32::EPSILON {
        glyph.y_advance
      } else {
        glyph.x_advance
      }
    }
  }
}

fn add_inline_advance(
  glyph: &mut crate::text::pipeline::GlyphPosition,
  axis: InlineAxis,
  delta: f32,
) {
  match axis {
    InlineAxis::Horizontal => glyph.x_advance += delta,
    InlineAxis::Vertical => glyph.y_advance += delta,
  }
}

/// An inline box item (non-atomic, contains children)
#[derive(Debug, Clone)]
pub struct InlineBoxItem {
  /// Identifier for the source box node (0 when unknown/anonymous)
  pub box_id: usize,

  /// Child items within this inline box
  pub children: Vec<InlineItem>,

  /// Additional inline-axis spacing inserted between consecutive children when justifying.
  ///
  /// Each entry corresponds to the gap **after** `children[i]` (so this is typically
  /// `children.len().saturating_sub(1)` long). The gap is part of this inline box's
  /// used width, but is *not* attributed to any particular child item, so child bounds
  /// do not inflate when justification inserts space between styled runs.
  pub justify_gaps: Vec<f32>,

  /// Opening edge width (left border + padding)
  pub start_edge: f32,

  /// Closing edge width (right border + padding)
  pub end_edge: f32,

  /// Extra inline-axis space inserted between the inline box's content edge and the adjacent
  /// inline-level content when this fragment sits at a line edge (`line-padding`).
  ///
  /// This is applied per-line fragment, and is separate from the element's real CSS padding so it
  /// does not affect padding-box geometry (e.g. absolute positioning containing blocks).
  pub line_padding_start: f32,

  /// Line-end counterpart to [`Self::line_padding_start`].
  pub line_padding_end: f32,

  /// Horizontal margins (only apply to the outermost fragments for `box-decoration-break: slice`)
  pub margin_left: f32,
  pub margin_right: f32,

  /// Vertical offset applied to children (padding + borders on top)
  pub content_offset_y: f32,

  /// Border widths used to derive the padding box for positioned descendants.
  pub border_left: f32,
  pub border_right: f32,
  pub border_top: f32,
  pub border_bottom: f32,

  pub bottom_inset: f32,

  /// Baseline metrics for this box
  pub metrics: BaselineMetrics,

  pub strut_metrics: BaselineMetrics,

  /// Vertical alignment
  pub vertical_align: VerticalAlign,

  /// Index for fragment creation
  pub box_index: usize,

  /// Bidi direction of this inline box
  pub direction: Direction,

  /// unicode-bidi behavior
  pub unicode_bidi: UnicodeBidi,

  /// Style for painting backgrounds/borders
  pub style: Arc<ComputedStyle>,
}

impl InlineBoxItem {
  /// Creates a new inline box item
  pub fn new(
    start_edge: f32,
    end_edge: f32,
    content_offset_y: f32,
    metrics: BaselineMetrics,
    style: Arc<ComputedStyle>,
    box_index: usize,
    direction: Direction,
    unicode_bidi: UnicodeBidi,
  ) -> Self {
    Self {
      box_id: 0,
      children: Vec::new(),
      justify_gaps: Vec::new(),
      start_edge,
      end_edge,
      line_padding_start: 0.0,
      line_padding_end: 0.0,
      margin_left: 0.0,
      margin_right: 0.0,
      content_offset_y,
      border_left: 0.0,
      border_right: 0.0,
      border_top: 0.0,
      border_bottom: 0.0,
      bottom_inset: 0.0,
      metrics,
      strut_metrics: metrics,
      vertical_align: VerticalAlign::Baseline,
      box_index,
      direction,
      unicode_bidi,
      style,
    }
  }

  /// Adds a child item
  pub fn add_child(&mut self, child: InlineItem) {
    self.children.push(child);
  }

  /// Returns a paint-time style snapshot whose border/padding widths match the inline fragment.
  ///
  /// Inline boxes can be fragmented (line wraps, bidi segmentation). Layout represents fragment
  /// edges via `start_edge`/`end_edge` and the resolved border widths; paint should use the same
  /// values so borders and backgrounds do not overlap content when an edge is suppressed for a
  /// continuation fragment.
  pub fn paint_style(&self) -> Arc<ComputedStyle> {
    let padding_left = (self.start_edge - self.border_left).max(0.0);
    let padding_right = (self.end_edge - self.border_right).max(0.0);
    let padding_top = (self.content_offset_y - self.border_top).max(0.0);
    let padding_bottom = (self.bottom_inset - self.border_bottom).max(0.0);
    let border_left = self.border_left.max(0.0);
    let border_right = self.border_right.max(0.0);
    let border_top = self.border_top.max(0.0);
    let border_bottom = self.border_bottom.max(0.0);

    fn px_matches(length: &Length, value: f32) -> bool {
      const EPS: f32 = 0.01;
      length.calc.is_none()
        && matches!(length.unit, LengthUnit::Px)
        && (length.value - value).abs() <= EPS
    }

    let style = self.style.as_ref();
    if px_matches(&style.padding_left, padding_left)
      && px_matches(&style.padding_right, padding_right)
      && px_matches(&style.padding_top, padding_top)
      && px_matches(&style.padding_bottom, padding_bottom)
      && px_matches(&style.border_left_width, border_left)
      && px_matches(&style.border_right_width, border_right)
      && px_matches(&style.border_top_width, border_top)
      && px_matches(&style.border_bottom_width, border_bottom)
    {
      return self.style.clone();
    }

    let mut updated = style.clone();
    updated.padding_left = Length::px(padding_left);
    updated.padding_right = Length::px(padding_right);
    updated.padding_top = Length::px(padding_top);
    updated.padding_bottom = Length::px(padding_bottom);
    updated.border_left_width = Length::px(border_left);
    updated.border_right_width = Length::px(border_right);
    updated.border_top_width = Length::px(border_top);
    updated.border_bottom_width = Length::px(border_bottom);
    Arc::new(updated)
  }

  /// Returns the total width of this inline box
  pub fn width(&self) -> f32 {
    let content_width: f32 = self.children.iter().map(|c| c.width()).sum();
    let gap_width: f32 = self.justify_gaps.iter().copied().sum();
    self.start_edge
      + self.line_padding_start
      + content_width
      + gap_width
      + self.line_padding_end
      + self.end_edge
  }

  pub fn total_width(&self) -> f32 {
    self.margin_left + self.width() + self.margin_right
  }
}

/// An inline-block item (atomic inline)
#[derive(Debug, Clone)]
pub struct InlineBlockItem {
  /// The laid-out fragment
  pub fragment: FragmentNode,

  /// Width of the inline-block
  pub width: f32,

  /// Min-content intrinsic width (border-box, excluding margins).
  pub intrinsic_min_width: f32,

  /// Max-content intrinsic width (border-box, excluding margins).
  pub intrinsic_max_width: f32,

  /// Height of the inline-block
  pub height: f32,

  /// Horizontal margins
  pub margin_left: f32,
  pub margin_right: f32,

  /// Vertical margins
  pub margin_top: f32,
  pub margin_bottom: f32,

  /// Baseline metrics
  pub metrics: BaselineMetrics,

  /// Vertical alignment
  pub vertical_align: VerticalAlign,

  /// Bidi direction
  pub direction: Direction,

  /// unicode-bidi behavior
  pub unicode_bidi: UnicodeBidi,
}

impl InlineBlockItem {
  /// Creates a new inline-block item
  pub fn new(
    fragment: FragmentNode,
    direction: Direction,
    unicode_bidi: UnicodeBidi,
    margin_left: f32,
    margin_right: f32,
    margin_top: f32,
    margin_bottom: f32,
    has_line_baseline: bool,
  ) -> Self {
    let width = fragment.bounds.width();
    let height = fragment.bounds.height();
    let margin_box_height = height + margin_top + margin_bottom;
    let mut first_baseline: Option<f32> = None;
    let mut last_baseline: Option<f32> = None;
    if has_line_baseline {
      collect_line_baselines(&fragment, 0.0, &mut first_baseline, &mut last_baseline);
    }

    let chosen_baseline = if let Some(style) = fragment.style.as_ref() {
      if matches!(style.display, Display::Table | Display::InlineTable) {
        first_baseline.or(last_baseline)
      } else {
        last_baseline
      }
    } else {
      last_baseline
    };

    // Inline-blocks participate in inline baseline calculations using their margin box, but the
    // fragment bounds are border-box sized. Track vertical margins separately so we can:
    // - compute line box heights from the margin box (spec)
    // - position the border box at `margin-top` within that margin box (paint)
    let metrics = chosen_baseline.map_or_else(
      || {
        // No in-flow line boxes: baseline is the bottom *margin* edge.
        // (CSS 2.1 §10.8.1 / inline-block baseline rules)
        let baseline_offset = margin_box_height;
        BaselineMetrics {
          baseline_offset,
          height: margin_box_height,
          ascent: baseline_offset,
          descent: 0.0,
          line_gap: 0.0,
          // Preserve legacy semantics for `vertical-align:<percentage>`, which previously used the
          // border-box height as the line-height proxy.
          line_height: height,
          x_height: None,
        }
      },
      |baseline| {
        // Baseline from the inline-block's last in-flow line box. Convert from border-box
        // coordinates to margin-box coordinates so vertical margins contribute to line box
        // metrics.
        let upper = height.max(0.0);
        let clamped_baseline = baseline.max(0.0).min(upper);
        let baseline_offset = margin_top + clamped_baseline;
        let descent = (margin_box_height - baseline_offset).max(0.0);
        BaselineMetrics {
          baseline_offset,
          height: margin_box_height,
          ascent: baseline_offset,
          descent,
          line_gap: 0.0,
          // Preserve legacy semantics for `vertical-align:<percentage>`.
          line_height: height,
          // Approximate x-height as half-ascent for middle alignment fallback.
          x_height: Some(baseline_offset * 0.5),
        }
      },
    );

    Self {
      fragment,
      width,
      intrinsic_min_width: width,
      intrinsic_max_width: width,
      height,
      margin_left,
      margin_right,
      margin_top,
      margin_bottom,
      metrics,
      vertical_align: VerticalAlign::Baseline,
      direction,
      unicode_bidi,
    }
  }

  pub fn with_intrinsic_widths(mut self, min_width: f32, max_width: f32) -> Self {
    self.intrinsic_min_width = min_width;
    self.intrinsic_max_width = max_width;
    self
  }

  /// Sets the vertical alignment
  pub fn with_vertical_align(mut self, align: VerticalAlign) -> Self {
    self.vertical_align = align;
    self
  }

  pub fn total_width(&self) -> f32 {
    self.margin_left + self.width + self.margin_right
  }

  pub fn intrinsic_min_total_width(&self) -> f32 {
    self.margin_left + self.intrinsic_min_width + self.margin_right
  }

  pub fn intrinsic_max_total_width(&self) -> f32 {
    self.margin_left + self.intrinsic_max_width + self.margin_right
  }
}

/// A laid out ruby segment (base + optional annotations)
#[derive(Debug, Clone)]
pub struct RubySegmentLayout {
  /// Base inline items for this segment
  pub base_items: Vec<InlineItem>,
  /// Optional annotation above the base
  pub annotation_top: Option<Vec<InlineItem>>,
  /// Optional annotation below the base
  pub annotation_bottom: Option<Vec<InlineItem>>,
  /// Metrics for the base line
  pub base_metrics: BaselineMetrics,
  /// Content height for the base line
  pub base_height: f32,
  /// Width of the base content
  pub base_width: f32,
  /// Metrics for the top annotation
  pub top_metrics: Option<BaselineMetrics>,
  /// Metrics for the bottom annotation
  pub bottom_metrics: Option<BaselineMetrics>,
  /// Width of the top annotation line
  pub top_width: f32,
  /// Width of the bottom annotation line
  pub bottom_width: f32,
  /// Total width for this segment (max of base/annotations)
  pub width: f32,
  /// Total height for this segment
  pub height: f32,
  /// Baseline offset for the base relative to the segment top
  pub baseline_offset: f32,
  /// Height occupied by the top annotation
  pub top_height: f32,
  /// Height occupied by the bottom annotation
  pub bottom_height: f32,
  /// Horizontal offset for the base within the segment
  pub base_x: f32,
  /// Horizontal offset for the top annotation
  pub top_x: f32,
  /// Optional spacing distribution for the top annotation line
  pub top_spacing: Option<RubyLineSpacing>,
  /// Horizontal offset for the bottom annotation
  pub bottom_x: f32,
  /// Optional spacing distribution for the bottom annotation line
  pub bottom_spacing: Option<RubyLineSpacing>,
  /// Horizontal offset of this segment within the ruby container
  pub offset_x: f32,
  /// Vertical offset applied to align baselines between segments
  pub offset_y: f32,
}

/// Spacing distribution for ruby annotation lines when using space-between/space-around alignment.
#[derive(Debug, Clone, Copy)]
pub struct RubyLineSpacing {
  /// Leading padding before the first item
  pub leading: f32,
  /// Gap inserted between consecutive annotation items
  pub gap: f32,
}

/// A ruby inline item representing a `<ruby>` container
#[derive(Debug, Clone)]
pub struct RubyItem {
  /// Segments that compose the ruby container
  pub segments: Vec<RubySegmentLayout>,
  /// Box start edge (padding+border)
  pub start_edge: f32,
  /// Box end edge (padding+border)
  pub end_edge: f32,
  /// Top padding/border inset
  pub content_offset_y: f32,
  /// Metrics for the ruby box
  pub metrics: BaselineMetrics,
  /// Horizontal margins
  pub margin_left: f32,
  pub margin_right: f32,
  /// Optional source box id
  pub box_id: Option<usize>,
  /// Fragment index for split ruby boxes
  pub fragment_index: usize,
  /// Vertical alignment
  pub vertical_align: VerticalAlign,
  /// Bidi direction
  pub direction: Direction,
  /// unicode-bidi behavior
  pub unicode_bidi: UnicodeBidi,
  /// Style used for painting backgrounds/borders
  pub style: Arc<ComputedStyle>,
}

impl RubyItem {
  pub fn width(&self) -> f32 {
    self.margin_left + self.intrinsic_width() + self.margin_right
  }

  pub fn intrinsic_width(&self) -> f32 {
    let segment_width: f32 = self.segments.iter().map(|s| s.width).sum();
    self.start_edge + segment_width + self.end_edge
  }
}

fn collect_line_baselines(
  fragment: &FragmentNode,
  y_offset: f32,
  first: &mut Option<f32>,
  last: &mut Option<f32>,
) {
  // When computing an inline-block's baseline (CSS 2.1 §10.8.1), we must consider only the last
  // *in-flow* line box. Out-of-flow positioned descendants (e.g. dropdown menus that are
  // `position: absolute`) and floats must not contribute. In-flow elements exclude both positioned
  // and floating boxes (CSS 2.1 §9.3.1), so including their line boxes can change inline-block
  // baseline selection (e.g. a floated child becomes the "last line box", preventing the inline-
  // block from falling back to the bottom margin edge).
  if let Some(style) = fragment.style.as_deref() {
    if matches!(
      style.position,
      crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
    ) {
      return;
    }
    if !matches!(style.float, crate::Float::None) || style.running_position.is_some() {
      return;
    }
  }
  let current_offset = y_offset + fragment.bounds.y();
  if let Some(baseline) = fragment.baseline {
    let absolute = current_offset + baseline;
    if first.is_none() {
      *first = Some(absolute);
    }
    *last = Some(absolute);
  }
  if let FragmentContent::Line { baseline } = fragment.content {
    let absolute = current_offset + baseline;
    if first.is_none() {
      *first = Some(absolute);
    }
    *last = Some(absolute);
    return;
  }
  for child in fragment.children.iter() {
    collect_line_baselines(child, current_offset, first, last);
  }
}

/// A replaced element item (img, canvas, etc.)
#[derive(Debug, Clone)]
pub struct ReplacedItem {
  /// Identifier for the source box node (0 when unknown/anonymous)
  pub box_id: usize,

  /// Width of the element
  pub width: f32,

  /// Height of the element
  pub height: f32,

  /// Horizontal margins
  pub margin_left: f32,
  pub margin_right: f32,

  /// Vertical margins
  pub margin_top: f32,
  pub margin_bottom: f32,

  /// Baseline metrics
  pub metrics: BaselineMetrics,

  /// Vertical alignment
  pub vertical_align: VerticalAlign,

  /// Horizontal advance used for layout (may differ for list markers)
  pub layout_advance: f32,

  /// Paint offset applied at fragment creation (used for outside markers)
  pub paint_offset: f32,

  /// True if this replaced item represents a list marker
  pub is_marker: bool,

  /// Original replaced type (img, video, etc.)
  pub replaced_type: ReplacedType,

  /// Computed style for painting
  pub style: Arc<ComputedStyle>,

  /// Bidi direction
  pub direction: Direction,

  /// unicode-bidi behavior
  pub unicode_bidi: UnicodeBidi,
}

impl ReplacedItem {
  /// Creates a new replaced item
  pub fn new(
    box_id: usize,
    size: Size,
    replaced_type: ReplacedType,
    style: Arc<ComputedStyle>,
    margin_left: f32,
    margin_right: f32,
    margin_top: f32,
    margin_bottom: f32,
  ) -> Self {
    let margin_box_height = size.height + margin_top + margin_bottom;
    let metrics = BaselineMetrics {
      baseline_offset: margin_box_height,
      height: margin_box_height,
      ascent: margin_box_height,
      descent: 0.0,
      line_gap: 0.0,
      // Preserve legacy semantics for `vertical-align:<percentage>`.
      line_height: size.height,
      x_height: None,
    };
    Self {
      box_id,
      width: size.width,
      height: size.height,
      margin_left,
      margin_right,
      margin_top,
      margin_bottom,
      metrics,
      vertical_align: VerticalAlign::Baseline,
      layout_advance: size.width + margin_left + margin_right,
      paint_offset: 0.0,
      is_marker: false,
      replaced_type,
      direction: style.direction,
      unicode_bidi: style.unicode_bidi,
      style,
    }
  }

  /// Overrides baseline metrics for replaced content.
  pub fn with_metrics(mut self, metrics: BaselineMetrics) -> Self {
    self.metrics = metrics;
    self
  }

  /// Sets the vertical alignment
  pub fn with_vertical_align(mut self, align: VerticalAlign) -> Self {
    self.vertical_align = align;
    self
  }

  /// Marks this replaced item as a list marker and adjusts layout/paint offsets accordingly.
  pub fn as_marker(mut self, gap: f32, position: ListStylePosition, direction: Direction) -> Self {
    let extent = self.width + gap;
    let sign = if direction == Direction::Rtl {
      1.0
    } else {
      -1.0
    };
    if matches!(position, ListStylePosition::Outside) {
      self.layout_advance = 0.0;
      self.paint_offset = sign * extent;
    } else {
      self.layout_advance = extent;
      self.paint_offset = 0.0;
    }
    self.margin_left = 0.0;
    self.margin_right = 0.0;
    self.margin_top = 0.0;
    self.margin_bottom = 0.0;
    self.is_marker = true;
    self
  }

  pub fn total_width(&self) -> f32 {
    self.layout_advance
  }

  pub fn intrinsic_width(&self) -> f32 {
    if self.is_marker {
      self.layout_advance
    } else {
      self.width
    }
  }
}

/// A tab character that expands to the next tab stop.
#[derive(Debug, Clone)]
pub struct TabItem {
  metrics: BaselineMetrics,
  vertical_align: VerticalAlign,
  style: Arc<ComputedStyle>,
  tab_interval: f32,
  resolved_width: f32,
  direction: Direction,
  unicode_bidi: UnicodeBidi,
  allow_wrap: bool,
}

impl TabItem {
  pub fn new(
    style: Arc<ComputedStyle>,
    metrics: BaselineMetrics,
    tab_interval: f32,
    allow_wrap: bool,
  ) -> Self {
    let direction = style.direction;
    let unicode_bidi = style.unicode_bidi;
    Self {
      metrics,
      vertical_align: VerticalAlign::Baseline,
      style,
      tab_interval,
      resolved_width: 0.0,
      direction,
      unicode_bidi,
      allow_wrap,
    }
  }

  pub fn with_vertical_align(mut self, align: VerticalAlign) -> Self {
    self.vertical_align = align;
    self
  }

  pub fn resolve_width(&mut self, start_x: f32) -> f32 {
    if !self.tab_interval.is_finite() || self.tab_interval <= 0.0 {
      self.resolved_width = 0.0;
      return 0.0;
    }
    let remainder = start_x.rem_euclid(self.tab_interval);
    let width = if remainder == 0.0 {
      self.tab_interval
    } else {
      self.tab_interval - remainder
    };
    self.resolved_width = width;
    width
  }

  pub fn width(&self) -> f32 {
    self.used_width()
  }

  pub fn interval(&self) -> f32 {
    self.tab_interval
  }

  pub fn style(&self) -> Arc<ComputedStyle> {
    self.style.clone()
  }

  pub fn metrics(&self) -> BaselineMetrics {
    self.metrics
  }

  pub fn allow_wrap(&self) -> bool {
    self.allow_wrap
  }

  pub fn set_direction(&mut self, direction: Direction) {
    self.direction = direction;
  }

  fn used_width(&self) -> f32 {
    if self.resolved_width > 0.0 {
      self.resolved_width
    } else {
      self.tab_interval.max(0.0)
    }
  }
}

/// A positioned item within a line
#[derive(Debug, Clone)]
pub struct PositionedItem {
  /// The inline item
  pub item: InlineItem,

  /// X position relative to line start
  pub x: f32,

  /// Y offset from line baseline (positive = down)
  pub baseline_offset: f32,
}

/// A completed line box
#[derive(Debug, Clone)]
pub struct Line {
  /// Positioned items in this line
  pub items: Vec<PositionedItem>,

  /// Resolved paragraph direction for this line (LTR/RTL from bidi base level)
  pub resolved_direction: Direction,

  /// Authored text-indent applied to this line in the inline-start direction (can be negative)
  pub indent: f32,

  /// Available width for this line after float shortening
  pub available_width: f32,

  /// Width of the line box (same as available width for now)
  pub box_width: f32,

  /// Horizontal offset of the line box start (used when floats shorten and shift the line)
  pub left_offset: f32,

  /// Vertical offset of the line box top within the inline formatting context
  pub y_offset: f32,

  /// Total width used by items
  pub width: f32,

  /// Line height
  pub height: f32,

  /// Baseline position from top of line box
  pub baseline: f32,

  /// Whether this line ends with a hard break
  pub ends_with_hard_break: bool,
}

impl Line {
  /// Creates an empty line
  pub fn new() -> Self {
    Self {
      items: Vec::with_capacity(LINE_DEFAULT_ITEM_CAPACITY),
      resolved_direction: Direction::Ltr,
      indent: 0.0,
      available_width: 0.0,
      box_width: 0.0,
      left_offset: 0.0,
      y_offset: 0.0,
      width: 0.0,
      height: 0.0,
      baseline: 0.0,
      ends_with_hard_break: false,
    }
  }

  /// Returns true if the line has no items
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }
}

impl Default for Line {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Debug, Clone)]
pub struct LineBuildResult {
  pub lines: Vec<Line>,
  pub truncated: bool,
  /// Maximum block-axis offset reached due to applying float clearance for hard breaks (e.g.
  /// `<br clear="both">`).
  ///
  /// This is used by the inline formatting context to advance past cleared floats even when no
  /// line box is emitted at the cleared position (for example when a clearing `<br>` is followed
  /// immediately by a block-level sibling, or when only static-position anchors remain after the
  /// break and are later discarded).
  pub max_clear_offset: f32,
}

enum SplitInlineBoxForLineResult {
  Split {
    fragment: InlineBoxItem,
    remainder: Option<InlineBoxItem>,
    ends_with_hard_break: bool,
    force_break: bool,
  },
  /// The inline box starts mid-line and nothing inside it can fit within the remaining width
  /// without using emergency breaks. In this case, the correct behavior is to break the line
  /// *before* the box and try again on a fresh line.
  BreakBefore { inline_box: InlineBoxItem },
}

/// Builder for constructing lines from inline items
///
/// Handles line breaking and item positioning.
pub struct LineBuilder<'a> {
  /// Available width for the first line
  first_line_width: f32,
  /// Available width for subsequent lines
  subsequent_line_width: f32,
  /// Authored text-indent length (can be negative)
  indent: f32,
  /// Whether hanging indentation is enabled
  indent_hanging: bool,
  /// Whether indentation applies after forced breaks
  indent_each_line: bool,
  /// Whether the next line to start is a paragraph start (first or after hard break)
  next_line_is_para_start: bool,
  /// Clear value requested by the most recently processed hard break (e.g. `<br clear=...>`).
  ///
  /// This is applied after the line ends so the next line starts below the relevant floats.
  pending_clear: Option<ClearSide>,
  /// Block-axis offset (relative to `float_base_y`) reached after applying float clearance for a
  /// hard break.
  ///
  /// `LineBuilder` normally advances vertical layout by emitting line boxes. When a clearing hard
  /// break is the final in-flow content, no subsequent line box is emitted at the cleared Y
  /// position; we record it here so the caller can still advance the containing block cursor.
  max_clear_offset: f32,
  /// Float-aware line width provider
  float_integration: Option<InlineFloatIntegration<'a>>,
  /// Current line space when floats are present
  current_line_space: Option<crate::layout::inline::float_integration::LineSpace>,
  /// Accumulated y offset for lines when floats shorten width
  current_y: f32,
  /// Absolute y offset of this inline formatting context within the containing block
  float_base_y: f32,
  /// Absolute x offset of this inline formatting context within the containing block
  float_base_x: f32,

  /// Current line being built
  current_line: Line,

  /// Current X position
  current_x: f32,

  /// Completed lines
  lines: Vec<Line>,

  /// Baseline accumulator for current line
  baseline_acc: LineBaselineAccumulator,

  /// Default strut metrics (from containing block)
  strut_metrics: BaselineMetrics,

  /// Shaping pipeline for text splitting
  shaper: ShapingPipeline,

  /// Font context for reshaping during line breaks
  font_context: FontContext,

  /// Cache for reshaping repeated substrings within this layout.
  reshape_cache: ReshapeCache,

  /// Cache of shaped hyphen advances used for validating hyphenation break opportunities.
  hyphen_advance_cache: FxHashMap<HyphenAdvanceCacheKey, f32>,

  /// Base paragraph level for bidi ordering (None = auto/first strong)
  base_level: Option<Level>,

  /// Root unicode-bidi on the paragraph container.
  root_unicode_bidi: UnicodeBidi,
  /// Root direction on the paragraph container.
  root_direction: Direction,

  line_clamp: Option<usize>,
  line_clamp_reached: bool,
  truncated: bool,

  deadline_counter: usize,
}

impl<'a> LineBuilder<'a> {
  fn hyphen_advance(
    &mut self,
    style: &ComputedStyle,
    base_direction: Direction,
    explicit_bidi: Option<ExplicitBidiContext>,
  ) -> f32 {
    let inserted = style.hyphenate_character.as_deref().unwrap_or("\u{2010}");
    if inserted.is_empty() {
      return 0.0;
    }
    let hyphen_hash = {
      let mut hasher = FxHasher::default();
      inserted.hash(&mut hasher);
      hasher.finish()
    };
    let style_hash = shaping_style_hash(style);
    let key = HyphenAdvanceCacheKey {
      style_hash,
      font_generation: self.font_context.font_generation(),
      hyphen_hash,
      base_direction_rtl: matches!(base_direction, Direction::Rtl),
      explicit_bidi: explicit_bidi.map(|ctx| (ctx.level.number(), ctx.override_all)),
      letter_spacing_bits: f32_to_canonical_bits(style.letter_spacing),
      word_spacing_bits: f32_to_canonical_bits(style.word_spacing),
    };

    if let Some(cached) = self.hyphen_advance_cache.get(&key) {
      return *cached;
    }

    let advance = match self.shaper.shape_with_context_hashed(
      inserted,
      style,
      &self.font_context,
      pipeline_dir_from_style(base_direction),
      explicit_bidi,
      style_hash,
    ) {
      Ok(mut runs) => {
        TextItem::apply_spacing_to_runs(
          &mut runs,
          inserted,
          style.letter_spacing,
          style.word_spacing,
        );
        runs.iter().map(|r| r.advance).sum()
      }
      Err(_) => style.font_size * 0.5,
    };

    self.hyphen_advance_cache.insert(key, advance);
    advance
  }

  fn break_fits_with_hyphen(
    &mut self,
    item: &TextItem,
    brk: &BreakOpportunity,
    max_width: f32,
  ) -> bool {
    let mut advance = item.advance_at_offset(brk.byte_offset);
    if brk.adds_hyphen {
      advance += self.hyphen_advance(item.style.as_ref(), item.base_direction, item.explicit_bidi);
    }
    advance <= max_width
  }

  fn find_fitting_break_at_or_before(
    &mut self,
    item: &TextItem,
    kind: BreakOpportunityKind,
    max_width: f32,
  ) -> Option<BreakOpportunity> {
    let mut offset = item.offset_for_width(max_width)?;
    loop {
      let brk = item.allowed_break_at_or_before_with_kind(offset, kind)?;
      if self.break_fits_with_hyphen(item, &brk, max_width) {
        return Some(brk);
      }
      if brk.byte_offset == 0 {
        return None;
      }
      offset = brk.byte_offset.saturating_sub(1);
    }
  }

  fn compute_indent_for_line(&self, is_para_start: bool, is_first_line: bool) -> f32 {
    if self.indent == 0.0 {
      return 0.0;
    }

    // CSS Text 3: `text-indent` applies to the first formatted line of the block container.
    // The `each-line` keyword extends this to each line after a *forced* line break (but not soft
    // wraps). The `hanging` keyword inverts which lines are affected.
    //
    // https://www.w3.org/TR/css-text-3/#text-indent-property
    let base_target = if self.indent_each_line {
      is_first_line || is_para_start
    } else {
      is_first_line
    };
    let should_indent = if self.indent_hanging {
      !base_target
    } else {
      base_target
    };

    if should_indent {
      self.indent
    } else {
      0.0
    }
  }

  fn is_collapsible_space_item(item: &InlineItem) -> bool {
    let InlineItem::Text(text) = item else {
      return false;
    };

    matches!(
      text.style.white_space,
      WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
    ) && !text.text.is_empty()
      && text.text.chars().all(|ch| ch == ' ')
      && !text.is_marker
  }

  fn line_is_at_start_for_whitespace_trim(&self) -> bool {
    // Leading collapsible whitespace is suppressed even if the line begins with placeholder items
    // like static-position anchors.
    self.current_line.items.iter().all(|p| {
      matches!(
        p.item,
        InlineItem::StaticPositionAnchor(_) | InlineItem::Floating(_)
      )
    })
  }

  fn start_new_line(&mut self) {
    let is_first_line = self.lines.is_empty();
    let para_start = self.next_line_is_para_start;
    self.next_line_is_para_start = false;
    let indent_for_line = self.compute_indent_for_line(para_start, is_first_line);
    let base_width = if is_first_line {
      self.first_line_width
    } else {
      self.subsequent_line_width
    };

    if let Some(integration) = self.float_integration.as_ref() {
      let query_y = self.float_base_y + self.current_y;
      let space = integration.find_line_space_in_containing_block(
        query_y,
        self.float_base_x,
        base_width,
        LineSpaceOptions::default()
          .line_height(self.strut_metrics.line_height)
          .allow_zero_width(false),
      );
      let space = self.clamp_float_space_to_line_width(base_width, space);
      if log_line_width_enabled() {
        eprintln!(
          "[line-space] query_y={:.2} base_width={:.2} float_base=({:.2},{:.2}) space=({:.2},{:.2}) width={:.2}",
          query_y,
          base_width,
          self.float_base_x,
          self.float_base_y,
          space.left_edge,
          space.right_edge,
          space.width
        );
      }
      self.current_line_space = Some(space);
      self.current_line.available_width = space.width;
      self.current_line.box_width = space.width;
      self.current_line.left_offset = space.left_edge;
      self.current_y = self.current_y.max((space.y - self.float_base_y).max(0.0));
    } else {
      self.current_line_space = None;
      self.current_line.available_width = base_width;
      self.current_line.box_width = base_width;
      self.current_line.left_offset = 0.0;
    }
    self.current_line.indent = indent_for_line;
  }

  fn clamp_float_space_to_line_width(
    &self,
    base_width: f32,
    space: crate::layout::inline::float_integration::LineSpace,
  ) -> crate::layout::inline::float_integration::LineSpace {
    // Convert the float-derived line range (which is in float context coordinates) into the local
    // coordinate space of this inline container, then clamp to the container width.
    //
    // This matters when we're querying an external float context (e.g., inherited from an ancestor
    // BFC) where the inline container is horizontally offset and/or narrower than the float
    // context's containing block.
    let left_edge = (space.left_edge - self.float_base_x).max(0.0);
    let right_edge = (space.right_edge - self.float_base_x).min(base_width);
    let width = (right_edge - left_edge).max(0.0);
    crate::layout::inline::float_integration::LineSpace::new(space.y, left_edge, width)
  }

  fn reposition_empty_line_for_floats(&mut self, min_width: f32) -> bool {
    if !self.current_line.is_empty() {
      return false;
    }
    let Some(integration) = self.float_integration.as_ref() else {
      return false;
    };

    let base_width = if self.lines.is_empty() {
      self.first_line_width
    } else {
      self.subsequent_line_width
    };
    let before_y = self.current_y;
    let before_width = self.current_line.available_width;

    let space = integration.find_line_space_in_containing_block(
      self.float_base_y + self.current_y,
      self.float_base_x,
      base_width,
      LineSpaceOptions::default()
        .min_width(min_width.max(0.0))
        .line_height(self.strut_metrics.line_height)
        .allow_zero_width(false),
    );
    let space = self.clamp_float_space_to_line_width(base_width, space);

    self.current_line_space = Some(space);
    self.current_line.available_width = space.width;
    self.current_line.box_width = space.width;
    self.current_line.left_offset = space.left_edge;
    self.current_y = self.current_y.max((space.y - self.float_base_y).max(0.0));

    (self.current_y - before_y).abs() > 0.0001
      || (self.current_line.available_width - before_width).abs() > 0.0001
  }

  /// Creates a new line builder
  pub fn new(
    first_line_width: f32,
    subsequent_line_width: f32,
    start_is_para_start: bool,
    _text_wrap: TextWrap,
    indent: f32,
    indent_hanging: bool,
    indent_each_line: bool,
    strut_metrics: BaselineMetrics,
    shaper: ShapingPipeline,
    font_context: FontContext,
    base_level: Option<Level>,
    root_unicode_bidi: UnicodeBidi,
    root_direction: Direction,
    float_integration: Option<InlineFloatIntegration<'a>>,
    float_base_x: f32,
    float_base_y: f32,
    line_clamp: Option<usize>,
  ) -> Self {
    let line_clamp = line_clamp.filter(|value| *value > 0);
    let initial_direction = if let Some(level) = base_level {
      if level.is_rtl() {
        Direction::Rtl
      } else {
        Direction::Ltr
      }
    } else {
      Direction::Ltr
    };
    let mut builder = Self {
      first_line_width,
      subsequent_line_width,
      indent,
      indent_hanging,
      indent_each_line,
      next_line_is_para_start: start_is_para_start,
      pending_clear: None,
      max_clear_offset: 0.0,
      float_integration,
      current_line_space: None,
      current_y: 0.0,
      float_base_y,
      float_base_x,
      current_line: Line {
        resolved_direction: initial_direction,
        ..Line::new()
      },
      current_x: 0.0,
      lines: Vec::new(),
      baseline_acc: LineBaselineAccumulator::new(&strut_metrics),
      strut_metrics,
      shaper,
      font_context,
      reshape_cache: ReshapeCache::default(),
      hyphen_advance_cache: FxHashMap::default(),
      base_level,
      root_unicode_bidi,
      root_direction,
      line_clamp,
      line_clamp_reached: false,
      truncated: false,
      deadline_counter: 0,
    };

    builder.start_new_line();

    builder
  }

  fn current_line_width(&self) -> f32 {
    self.current_line.available_width
  }

  /// Adds an inline item to the builder
  pub fn add_item(&mut self, item: InlineItem) -> Result<(), LayoutError> {
    if self.line_is_at_start_for_whitespace_trim() && Self::is_collapsible_space_item(&item) {
      return Ok(());
    }
    if let InlineItem::HardBreak(clear) = item {
      if clear.is_clearing() {
        self.pending_clear = Some(clear);
      }
      return self.force_break();
    }
    if self.line_clamp_reached {
      self.truncated = true;
      return Ok(());
    }
    if item.contains_hard_break() {
      for part in item.hoist_hard_breaks() {
        self.add_item(part)?;
      }
      return Ok(());
    }
    check_layout_deadline(&mut self.deadline_counter)?;
    match item {
      InlineItem::SoftBreak => self.finish_line(),
      other => self.add_item_internal(other),
    }
  }

  fn add_item_internal(&mut self, item: InlineItem) -> Result<(), LayoutError> {
    let mut item = item;
    loop {
      // Tabs resolve to the next tab stop, which is defined relative to the inline start edge of
      // the line box. `current_x` tracks the width used by items *before* indentation; indentation
      // is applied later during fragment placement. Include the authored indentation here so tab
      // stops line up correctly on indented lines.
      let start_x = self.current_x + self.current_line.indent;
      let (resolved, item_width) = item.resolve_width_at(start_x);
      let line_width = self.current_line_width();

      let kind = match &resolved {
        InlineItem::Text(_) => "text",
        InlineItem::SoftBreak => "soft-break",
        InlineItem::Tab(_) => "tab",
        InlineItem::HardBreak(_) => "hard-break",
        InlineItem::InlineBox(_) => "inline-box",
        InlineItem::InlineBlock(_) => "inline-block",
        InlineItem::Ruby(_) => "ruby",
        InlineItem::Replaced(_) => "replaced",
        InlineItem::Floating(_) => "floating",
        InlineItem::StaticPositionAnchor(_) => "anchor",
      };

      if log_line_width_enabled() {
        eprintln!(
          "[line-add] kind={} line_width={:.2} current_x={:.2} item_width={:.2} breakable={}",
          kind,
          line_width,
          self.current_x,
          item_width,
          resolved.is_breakable()
        );
      }

      // Check if item fits.
      //
      // Text advances come from shaping and can contain fractional values. In practice we sometimes
      // compare values that were rounded/truncated through different code paths (e.g. Taffy
      // constraints vs shaped glyph advances) and end up with a tiny "doesn't fit" overflow that
      // triggers an unexpected line break (notably in narrow ad placeholders).
      //
      // Allow a small epsilon for items that can legitimately accumulate subpixel width differences
      // (text shaping + atomic inline boxes). This avoids "almost fits" overflow flipping a line
      // wrap decision and drastically changing layout (e.g. footer nav links wrapping).
      //
      // NOTE: `current_x` can exceed `line_width` when non-wrapping content overflows a line.
      // Zero-width items like `StaticPositionAnchor` must not trigger a new line in that case
      // (e.g. absolutely positioned placeholders at the end of a `white-space: nowrap` list).
      let fit_eps = if resolved.is_breakable()
        || matches!(
          resolved,
          InlineItem::InlineBlock(_) | InlineItem::Ruby(_) | InlineItem::Replaced(_)
        ) {
        LINE_PIXEL_FIT_EPSILON
      } else {
        0.0
      };
      let remaining_width = (line_width - self.current_x).max(0.0);
      if item_width <= remaining_width + fit_eps {
        self.place_item_with_width(resolved, item_width);
        return Ok(());
      }

      // CSS 2.1 §9.5.1: line boxes must avoid floats. When floats intrude such that there's
      // insufficient horizontal room for the next piece of inline content, the line box is moved
      // down until it finds enough space or clears past the floats.
      if self.current_line.is_empty() {
        let required_width = match &resolved {
          InlineItem::Text(text) => self.min_required_width_for_text_item(text),
          InlineItem::InlineBox(inline_box) => {
            self.min_required_width_for_inline_box_item(inline_box)
          }
          _ => item_width,
        };
        if self.reposition_empty_line_for_floats(required_width) {
          item = resolved;
          continue;
        }
      }

      if resolved.is_breakable() {
        // Try to break the item even on an empty line; oversized items should
        // still honor break opportunities instead of overflowing the line.
        self.add_breakable_item(resolved)?;
        return Ok(());
      }

      // Item doesn't fit and can't be broken
      if self.current_line.is_empty() || !item_allows_soft_wrap(&resolved) {
        // No break possible (or wrapping is disabled); overflow this line.
        self.place_item_with_width(resolved, item_width);
        return Ok(());
      }

      // If the previous item ends with a non-breaking "glue" character (notably `&nbsp;`), there is
      // no soft wrap opportunity at the boundary. In that case we must overflow instead of pushing
      // the atomic item onto the next line.
      if let Some(prev) = self
        .current_line
        .items
        .iter()
        .rev()
        .find_map(|pos| match &pos.item {
          InlineItem::StaticPositionAnchor(_)
          | InlineItem::Floating(_)
          | InlineItem::HardBreak(_) => None,
          other => Some(other),
        })
      {
        if last_text_char_for_soft_wrap(prev).is_some_and(is_no_break_after_character) {
          self.place_item_with_width(resolved, item_width);
          return Ok(());
        }
      }

      self.finish_line()?;
      item = resolved;
    }
  }

  fn min_required_width_for_text_item(&self, item: &TextItem) -> f32 {
    if item.advance_for_layout <= 0.0 {
      return 0.0;
    }
    if !allows_soft_wrap(item.style.as_ref()) {
      return item.advance_for_layout;
    }

    // When a line starts adjacent to floats, the available width can become extremely narrow. If
    // we can't fit *any* prefix of the next text run (e.g., the first word with normal wrapping),
    // we must push the line below the floats instead of placing overflowing text alongside them.
    for brk in &item.break_opportunities {
      if brk.byte_offset == 0 {
        continue;
      }
      let width = item.advance_at_offset(brk.byte_offset);
      if width.is_finite() && width > 0.0 {
        return width;
      }
    }

    item.advance_for_layout
  }

  fn inline_item_can_fragment_for_float_reposition(&self, item: &InlineItem) -> bool {
    match item {
      InlineItem::Text(text) => {
        if !allows_soft_wrap(text.style.as_ref()) {
          return false;
        }

        // If the text has any break opportunity that would split it into a non-empty remainder,
        // treat it as fragmentable.
        if text
          .break_opportunities
          .iter()
          .any(|brk| brk.byte_offset > 0 && brk.byte_offset < text.text.len())
        {
          return true;
        }

        // When `overflow-wrap`/`word-break` permits emergency breaking, a multi-character run can
        // still be fragmented even if the normal break scan yields no opportunities.
        (matches!(
          text.style.word_break,
          WordBreak::BreakWord | WordBreak::Anywhere
        ) || matches!(
          text.style.overflow_wrap,
          OverflowWrap::BreakWord | OverflowWrap::Anywhere
        )) && text.text.chars().count() > 1
      }
      InlineItem::InlineBox(inline_box) => {
        if !allows_soft_wrap(inline_box.style.as_ref()) {
          return false;
        }

        let mut iter = inline_box.children.iter().filter(|child| {
          !matches!(
            child,
            InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_)
          )
        });
        let first = iter.next();
        if iter.next().is_some() {
          return true;
        }
        first.is_some_and(|child| self.inline_item_can_fragment_for_float_reposition(child))
      }
      InlineItem::SoftBreak | InlineItem::HardBreak(_) => true,
      InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_) => false,
      InlineItem::Tab(_)
      | InlineItem::InlineBlock(_)
      | InlineItem::Ruby(_)
      | InlineItem::Replaced(_) => false,
    }
  }

  fn min_required_width_for_inline_item_for_float_reposition(&self, item: &InlineItem) -> f32 {
    match item {
      InlineItem::Text(text) => self.min_required_width_for_text_item(text),
      InlineItem::InlineBox(inline_box) => {
        if !allows_soft_wrap(inline_box.style.as_ref()) {
          return inline_box.total_width();
        }
        self.min_required_width_for_inline_box_item(inline_box)
      }
      other => other.width(),
    }
  }

  fn min_required_width_for_inline_box_item(&self, inline_box: &InlineBoxItem) -> f32 {
    if !allows_soft_wrap(inline_box.style.as_ref()) {
      return inline_box.total_width();
    }

    // Inline boxes can be fragmented across lines. When floats shorten a line box so the *entire*
    // inline box doesn't fit, we still want to keep the line next to the float if we can place a
    // fragment of the inline box there (e.g. the first word of a long `<strong>`).
    //
    // Use the minimum width needed to place the first fragmentable chunk at the start of the line
    // instead of the inline box's full width, otherwise `reposition_empty_line_for_floats` can
    // spuriously "clear" below floats even though the inline box would wrap.
    let start_edge = inline_box.start_edge.max(0.0);
    let end_edge = inline_box.end_edge.max(0.0);
    let margin_left = inline_box.margin_left.max(0.0);
    let margin_right = inline_box.margin_right.max(0.0);

    let mut iter = inline_box.children.iter().filter(|child| {
      !matches!(
        child,
        InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_)
      )
    });
    let first_child = iter.next();
    let has_multiple_children = iter.next().is_some();

    let mut required = margin_left + start_edge;
    if let Some(first_child) = first_child {
      required += self.min_required_width_for_inline_item_for_float_reposition(first_child);
    }

    // If this inline box can't be fragmented at all, its first (and only) fragment must include
    // the closing edge as well.
    let can_fragment = if has_multiple_children {
      true
    } else if let Some(first_child) = first_child {
      self.inline_item_can_fragment_for_float_reposition(first_child)
    } else {
      false
    };
    if !can_fragment {
      required += end_edge + margin_right;
    }

    required
  }

  /// Adds a breakable item (text or inline box), handling line breaking
  fn add_breakable_item(&mut self, mut item: InlineItem) -> Result<(), LayoutError> {
    loop {
      if self.line_clamp_reached {
        self.truncated = true;
        return Ok(());
      }
      check_layout_deadline(&mut self.deadline_counter)?;

      if self.line_is_at_start_for_whitespace_trim() && Self::is_collapsible_space_item(&item) {
        return Ok(());
      }

      match item {
        InlineItem::Text(text_item) => {
          let remaining_width = (self.current_line_width() - self.current_x).max(0.0);

          if log_line_width_enabled() {
            eprintln!(
              "[line-width] remaining {:.2} advance {:.2} breaks {}",
              remaining_width,
              text_item.advance_for_layout,
              text_item.break_opportunities.len()
            );
          }

          let mut break_opportunity = text_item.find_break_point(remaining_width);
          if break_opportunity.is_none() && self.current_line.is_empty() {
            // No break fits within the remaining width, but the line is empty.
            // Split at the earliest opportunity to avoid keeping multiple words
            // on an overflowing line.
            break_opportunity = text_item.break_opportunities.first().copied();
            if break_opportunity.is_none()
              && allows_soft_wrap(text_item.style.as_ref())
              && (matches!(
                text_item.style.word_break,
                WordBreak::BreakWord | WordBreak::Anywhere
              ) || matches!(
                text_item.style.overflow_wrap,
                OverflowWrap::BreakWord | OverflowWrap::Anywhere
              ))
            {
              if let Some((idx, _)) = text_item.text.char_indices().nth(1) {
                break_opportunity = Some(BreakOpportunity::emergency(idx));
              }
            }
          }

          if let Some(mut candidate) = break_opportunity {
            if candidate.adds_hyphen
              && !self.break_fits_with_hyphen(&text_item, &candidate, remaining_width)
            {
              // The break was selected based on the pre-split advance, but inserting a hyphen
              // increases the "before" width. Walk backwards to earlier breaks that still fit
              // after accounting for the inserted hyphen width.
              if let Some(fitting) = self
                .find_fitting_break_at_or_before(
                  &text_item,
                  BreakOpportunityKind::Normal,
                  remaining_width,
                )
                .or_else(|| {
                  self.find_fitting_break_at_or_before(
                    &text_item,
                    BreakOpportunityKind::Emergency,
                    remaining_width,
                  )
                })
              {
                candidate = fitting;
              }
            }
            break_opportunity = Some(candidate);
          }

          if let Some(break_opportunity) = break_opportunity {
            // Split at break point
            if let Some((before, after)) = text_item.split_at(
              break_opportunity.byte_offset,
              break_opportunity.adds_hyphen,
              &self.shaper,
              &self.font_context,
              &mut self.reshape_cache,
            ) {
              let mut before = before;
              let mut drop_before = false;
              if matches!(break_opportunity.break_type, BreakType::Allowed)
                && matches!(
                  text_item.style.white_space,
                  WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
                )
              {
                let trimmed_len = before.text.trim_end_matches(' ').len();
                if trimmed_len < before.text.len() {
                  if trimmed_len == 0 {
                    drop_before = true;
                  } else if let Some((trimmed, _)) = before.split_at(
                    trimmed_len,
                    false,
                    &self.shaper,
                    &self.font_context,
                    &mut self.reshape_cache,
                  ) {
                    before = trimmed;
                  }
                }
              }

              // Place the part that fits
              if !drop_before && before.advance_for_layout > 0.0 {
                let width = before.advance_for_layout;
                self.place_item_with_width(InlineItem::Text(before), width);
              }

              // If splitting at an allowed break produces only collapsible whitespace for the
              // current line, drop that whitespace and keep trying to place the remainder on the
              // same line. Otherwise we can spuriously break immediately after an inline box when
              // the next text run begins with a collapsible space (e.g. `</code> CSS ...`).
              if drop_before
                && matches!(break_opportunity.break_type, BreakType::Allowed)
                && break_opportunity.byte_offset > 0
              {
                item = InlineItem::Text(after);
                continue;
              }

              // Start new line for the rest
              if matches!(break_opportunity.break_type, BreakType::Mandatory) {
                self.current_line.ends_with_hard_break = true;
              }
              self.finish_line()?;

              item = InlineItem::Text(after);
              continue;
            }

            // If splitting fails, fall back to placing the whole item
            let width = text_item.advance_for_layout;
            if self.current_line.is_empty() {
              self.place_item_with_width(InlineItem::Text(text_item), width);
            } else {
              self.finish_line()?;
              self.place_item_with_width(InlineItem::Text(text_item), width);
            }
            return Ok(());
          }

          // No break point found within remaining width
          if self.current_line.is_empty() || !allows_soft_wrap(text_item.style.as_ref()) {
            // Wrapping is disabled or nothing is on the line; overflow in place.
            let width = text_item.advance_for_layout;
            self.place_item_with_width(InlineItem::Text(text_item), width);
            return Ok(());
          }

          // Start new line and try again
          self.finish_line()?;
          item = InlineItem::Text(text_item);
        }
        InlineItem::InlineBox(inline_box) => {
          let remaining_width = (self.current_line_width() - self.current_x).max(0.0);
          let total_width = inline_box.total_width();

          if total_width <= remaining_width {
            self.place_item_with_width(InlineItem::InlineBox(inline_box), total_width);
            return Ok(());
          }

          // `white-space: nowrap` / `text-wrap: nowrap` suppresses soft wraps not just within text,
          // but also across atomic inline boundaries. If an inline box is itself non-wrapping, it
          // must not be fragmented to fit the line width; instead, it overflows the line.
          //
          // This matters for patterns like `span { white-space: nowrap } span::after { ... }`,
          // where the pseudo-element is an atomic inline (e.g. `display: inline-block`).
          if !allows_soft_wrap(inline_box.style.as_ref()) {
            self.place_item_with_width(InlineItem::InlineBox(inline_box), total_width);
            return Ok(());
          }

          // Inline boxes (e.g. `<span>`, `<a>`) participate in line breaking just like text. When an
          // inline box overflows the remaining line width, we must allow it to be fragmented such
          // that some of its contents can appear on the current line, with the remainder continuing
          // on the next line.
          //
          // Previously we forced a break *before* the inline box whenever it didn't fit entirely.
          // That left the current line underfull and could increase the total number of lines,
          // diverging from browser behavior (and from CSS2.1's greedy line breaking model).
          //
          // If there is no remaining width at all, fragmenting would overflow the line anyway, so
          // prefer starting a new line first.
          if remaining_width <= LINE_PIXEL_FIT_EPSILON && !self.current_line.is_empty() {
            self.finish_line()?;
          }
          self.add_fragmented_inline_box(inline_box)?;
          return Ok(());
        }
        other => {
          debug_assert!(
            !other.is_breakable(),
            "add_breakable_item called with non-breakable item"
          );
          return Ok(());
        }
      }
    }
  }

  fn split_inline_box_for_line(
    &mut self,
    inline_box: InlineBoxItem,
    box_start_x: f32,
    available_width: f32,
    allow_emergency_breaks: bool,
  ) -> Result<SplitInlineBoxForLineResult, LayoutError> {
    let box_id = inline_box.box_id;
    let box_index = inline_box.box_index;
    let direction = inline_box.direction;
    let unicode_bidi = inline_box.unicode_bidi;
    let vertical_align = inline_box.vertical_align;
    let content_offset_y = inline_box.content_offset_y;
    let border_top = inline_box.border_top;
    let border_bottom = inline_box.border_bottom;
    let bottom_inset = inline_box.bottom_inset;
    let strut_metrics = inline_box.strut_metrics;
    let style = inline_box.style.clone();

    let start_edge = inline_box.start_edge;
    let end_edge = inline_box.end_edge;
    let margin_left = inline_box.margin_left;
    let margin_right = inline_box.margin_right;
    let border_left = inline_box.border_left;
    let border_right = inline_box.border_right;

    let mut remaining: VecDeque<InlineItem> = inline_box.children.into();
    let mut fragment_children: Vec<InlineItem> = Vec::new();
    // `fragment_children` can contain out-of-flow placeholders (floats/static-position anchors)
    // that should not influence whether we treat the fragment as having "real" in-flow content
    // for fragmentation decisions.
    //
    // In particular, absolutely positioned descendants are represented as
    // `InlineItem::StaticPositionAnchor` with zero width. If we treat these as normal in-flow
    // children, we can incorrectly conclude that the fragment is non-empty and refuse to split
    // the next in-flow child inline box, producing a zero-width inline box fragment that creates
    // an empty line box before the first line of text (e.g. on theverge.com headlines).
    let mut fragment_has_in_flow_children = false;
    let mut ends_with_hard_break = false;
    let mut force_break = false;
    let mut used_width: f32 = 0.0;

    let available_children_width = (available_width - margin_left - start_edge).max(0.0);

    while let Some(next) = remaining.pop_front() {
      check_layout_deadline(&mut self.deadline_counter)?;

      match next {
        InlineItem::SoftBreak => {
          force_break = true;
          break;
        }
        InlineItem::HardBreak(_) => {
          ends_with_hard_break = true;
          force_break = true;
          break;
        }
        next => {
          let start_x = box_start_x + margin_left + start_edge + used_width;
          let (next, next_width) = next.resolve_width_at(start_x);

          let fit_epsilon = if next.is_breakable()
            || matches!(
              &next,
              InlineItem::InlineBlock(_) | InlineItem::Replaced(_) | InlineItem::Ruby(_)
            ) {
            LINE_PIXEL_FIT_EPSILON
          } else {
            LINE_FIT_EPSILON
          };
          if used_width + next_width <= available_children_width + fit_epsilon {
            fragment_has_in_flow_children |= !matches!(
              &next,
              InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_)
            );
            fragment_children.push(next);
            used_width += next_width;
            continue;
          }

          // If the previous inline item ends with a non-breaking glue character (e.g. `&nbsp;`),
          // do not allow an atomic inline to wrap onto the next line. Overflow the fragment
          // instead so the sequence stays together.
          if !next.is_breakable() {
            if let Some(prev) = fragment_children.iter().rev().find(|child| {
              !matches!(
                child,
                InlineItem::StaticPositionAnchor(_)
                  | InlineItem::Floating(_)
                  | InlineItem::HardBreak(_)
              )
            }) {
              if last_text_char_for_soft_wrap(prev).is_some_and(is_no_break_after_character) {
                fragment_has_in_flow_children |= !matches!(
                  &next,
                  InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_)
                );
                fragment_children.push(next);
                used_width += next_width;
                continue;
              }
            }
          }

          match next {
            InlineItem::Text(text_item) => {
              let remaining_width = (available_children_width - used_width).max(0.0);
              let mut break_opportunity = text_item.find_break_point(remaining_width);
              if !fragment_has_in_flow_children && !allow_emergency_breaks {
                // When the inline box starts mid-line, we should not apply emergency breaks to the
                // first child if the box would fit on the next line. Instead, break the line
                // *before* the box and try again.
                if break_opportunity.is_some_and(|brk| {
                  if brk.kind != BreakOpportunityKind::Emergency {
                    return false;
                  }
                  let offset = brk.byte_offset;
                  if offset == 0 || offset >= text_item.text.len() {
                    return false;
                  }
                  if !text_item.text.is_char_boundary(offset) {
                    return true;
                  }
                  let after = text_item.text[offset..].chars().next();
                  let before = text_item.text[..offset].chars().rev().next();
                  match (after, before) {
                    (Some(after), Some(before)) => {
                      !(after.is_whitespace() || before.is_whitespace())
                    }
                    _ => false,
                  }
                }) {
                  remaining.push_front(InlineItem::Text(text_item));
                  let remaining_children: Vec<InlineItem> = remaining.into();
                  let metrics = super::compute_inline_box_metrics(
                    &remaining_children,
                    content_offset_y,
                    bottom_inset,
                    strut_metrics,
                  );
                  let mut inline_box = InlineBoxItem::new(
                    start_edge,
                    end_edge,
                    content_offset_y,
                    metrics,
                    style.clone(),
                    box_index,
                    direction,
                    unicode_bidi,
                  );
                  inline_box.box_id = box_id;
                  inline_box.margin_left = margin_left;
                  inline_box.margin_right = margin_right;
                  inline_box.border_left = border_left;
                  inline_box.border_right = border_right;
                  inline_box.border_top = border_top;
                  inline_box.border_bottom = border_bottom;
                  inline_box.bottom_inset = bottom_inset;
                  inline_box.strut_metrics = strut_metrics;
                  inline_box.vertical_align = vertical_align;
                  inline_box.children = remaining_children;
                  return Ok(SplitInlineBoxForLineResult::BreakBefore { inline_box });
                }

                if break_opportunity.is_none() {
                  if let Some(candidate) = text_item.break_opportunities.first().copied() {
                    if candidate.kind != BreakOpportunityKind::Emergency {
                      if let Some((mut before, _)) = text_item.split_at(
                        candidate.byte_offset,
                        candidate.adds_hyphen,
                        &self.shaper,
                        &self.font_context,
                        &mut self.reshape_cache,
                      ) {
                        let mut drop_before = false;
                        if matches!(candidate.break_type, BreakType::Allowed)
                          && matches!(
                            text_item.style.white_space,
                            WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
                          )
                        {
                          let trimmed_len = before.text.trim_end_matches(' ').len();
                          if trimmed_len < before.text.len() {
                            if trimmed_len == 0 {
                              drop_before = true;
                            } else if let Some((trimmed, _)) = before.split_at(
                              trimmed_len,
                              false,
                              &self.shaper,
                              &self.font_context,
                              &mut self.reshape_cache,
                            ) {
                              before = trimmed;
                            }
                          }
                        }

                        if !drop_before
                          && before.advance_for_layout > 0.0
                          && before.advance_for_layout <= remaining_width + LINE_PIXEL_FIT_EPSILON
                        {
                          break_opportunity = Some(candidate);
                        }
                      }
                    }
                  }

                  if break_opportunity.is_none() {
                    remaining.push_front(InlineItem::Text(text_item));
                    let remaining_children: Vec<InlineItem> = remaining.into();
                    let metrics = super::compute_inline_box_metrics(
                      &remaining_children,
                      content_offset_y,
                      bottom_inset,
                      strut_metrics,
                    );
                    let mut inline_box = InlineBoxItem::new(
                      start_edge,
                      end_edge,
                      content_offset_y,
                      metrics,
                      style.clone(),
                      box_index,
                      direction,
                      unicode_bidi,
                    );
                    inline_box.box_id = box_id;
                    inline_box.margin_left = margin_left;
                    inline_box.margin_right = margin_right;
                    inline_box.border_left = border_left;
                    inline_box.border_right = border_right;
                    inline_box.border_top = border_top;
                    inline_box.border_bottom = border_bottom;
                    inline_box.bottom_inset = bottom_inset;
                    inline_box.strut_metrics = strut_metrics;
                    inline_box.vertical_align = vertical_align;
                    inline_box.children = remaining_children;
                    return Ok(SplitInlineBoxForLineResult::BreakBefore { inline_box });
                  }
                }
              }

              if break_opportunity.is_none() && !fragment_has_in_flow_children {
                break_opportunity = text_item.break_opportunities.first().copied();
                if break_opportunity.is_none()
                  && allows_soft_wrap(text_item.style.as_ref())
                  && (matches!(
                    text_item.style.word_break,
                    WordBreak::BreakWord | WordBreak::Anywhere
                  ) || matches!(
                    text_item.style.overflow_wrap,
                    OverflowWrap::BreakWord | OverflowWrap::Anywhere
                  ))
                {
                  if let Some((idx, _)) = text_item.text.char_indices().nth(1) {
                    break_opportunity = Some(BreakOpportunity::emergency(idx));
                  }
                }
              }

              if let Some(mut candidate) = break_opportunity {
                if candidate.adds_hyphen
                  && !self.break_fits_with_hyphen(&text_item, &candidate, remaining_width)
                {
                  if let Some(fitting) = self
                    .find_fitting_break_at_or_before(
                      &text_item,
                      BreakOpportunityKind::Normal,
                      remaining_width,
                    )
                    .or_else(|| {
                      self.find_fitting_break_at_or_before(
                        &text_item,
                        BreakOpportunityKind::Emergency,
                        remaining_width,
                      )
                    })
                  {
                    candidate = fitting;
                  }
                }
                break_opportunity = Some(candidate);
              }

              if let Some(break_opportunity) = break_opportunity {
                if let Some((before, after)) = text_item.split_at(
                  break_opportunity.byte_offset,
                  break_opportunity.adds_hyphen,
                  &self.shaper,
                  &self.font_context,
                  &mut self.reshape_cache,
                ) {
                  let mut before = before;
                  let mut drop_before = false;
                  if matches!(break_opportunity.break_type, BreakType::Allowed)
                    && matches!(
                      text_item.style.white_space,
                      WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
                    )
                  {
                    let trimmed_len = before.text.trim_end_matches(' ').len();
                    if trimmed_len < before.text.len() {
                      if trimmed_len == 0 {
                        drop_before = true;
                      } else if let Some((trimmed, _)) = before.split_at(
                        trimmed_len,
                        false,
                        &self.shaper,
                        &self.font_context,
                        &mut self.reshape_cache,
                      ) {
                        before = trimmed;
                      }
                    }
                  }

                  if drop_before
                    && matches!(break_opportunity.break_type, BreakType::Allowed)
                    && break_opportunity.byte_offset > 0
                  {
                    remaining.push_front(InlineItem::Text(after));
                    // Leading collapsible whitespace should not force a line break; keep trying to
                    // fit the remainder of the text item on this line.
                    continue;
                  }

                  if !drop_before && before.advance_for_layout > 0.0 {
                    fragment_children.push(InlineItem::Text(before));
                  }
                  remaining.push_front(InlineItem::Text(after));
                  if matches!(break_opportunity.break_type, BreakType::Mandatory) {
                    ends_with_hard_break = true;
                    force_break = true;
                  }
                  break;
                }
              }

              if !fragment_has_in_flow_children || !allows_soft_wrap(text_item.style.as_ref()) {
                let first_in_flow_child = !fragment_has_in_flow_children;
                // Even when the first text run overflows the line (no break opportunities fit), we
                // still need to keep processing subsequent inline items. This matters when the run
                // ends with a non-breaking glue character like `&nbsp;` and is immediately followed
                // by an atomic inline (e.g. a caret span): the sequence must stay together and
                // overflow as a unit rather than wrapping the atomic inline onto the next line.
                fragment_children.push(InlineItem::Text(text_item));
                fragment_has_in_flow_children = true;
                used_width += next_width;

                if first_in_flow_child {
                  if fragment_children
                    .last()
                    .and_then(last_text_char_for_soft_wrap)
                    .is_some_and(is_no_break_after_character)
                  {
                    continue;
                  }
                }
              } else {
                remaining.push_front(InlineItem::Text(text_item));
              }
              break;
            }
            InlineItem::InlineBox(child_box) => {
              let remaining_width = (available_children_width - used_width).max(0.0);
              // `white-space: nowrap` suppresses soft wraps inside this inline box. Treat it as
              // atomic: if the parent inline box already placed in-flow content, break before it;
              // otherwise only overflow it when the parent box itself starts a new line.
              if !allows_soft_wrap(child_box.style.as_ref()) {
                if fragment_has_in_flow_children {
                  remaining.push_front(InlineItem::InlineBox(child_box));
                  break;
                }
                if !allow_emergency_breaks {
                  remaining.push_front(InlineItem::InlineBox(child_box));
                  let remaining_children: Vec<InlineItem> = remaining.into();
                  let metrics = super::compute_inline_box_metrics(
                    &remaining_children,
                    content_offset_y,
                    bottom_inset,
                    strut_metrics,
                  );
                  let mut inline_box = InlineBoxItem::new(
                    start_edge,
                    end_edge,
                    content_offset_y,
                    metrics,
                    style.clone(),
                    box_index,
                    direction,
                    unicode_bidi,
                  );
                  inline_box.box_id = box_id;
                  inline_box.margin_left = margin_left;
                  inline_box.margin_right = margin_right;
                  inline_box.border_left = border_left;
                  inline_box.border_right = border_right;
                  inline_box.border_top = border_top;
                  inline_box.border_bottom = border_bottom;
                  inline_box.bottom_inset = bottom_inset;
                  inline_box.strut_metrics = strut_metrics;
                  inline_box.vertical_align = vertical_align;
                  inline_box.children = remaining_children;
                  return Ok(SplitInlineBoxForLineResult::BreakBefore { inline_box });
                }

                fragment_children.push(InlineItem::InlineBox(child_box));
                break;
              }

              let min_width = self.min_required_width_for_inline_box_item(&child_box);
              if min_width > remaining_width + LINE_PIXEL_FIT_EPSILON {
                if fragment_has_in_flow_children {
                  remaining.push_front(InlineItem::InlineBox(child_box));
                  break;
                }
                if !allow_emergency_breaks {
                  remaining.push_front(InlineItem::InlineBox(child_box));
                  let remaining_children: Vec<InlineItem> = remaining.into();
                  let metrics = super::compute_inline_box_metrics(
                    &remaining_children,
                    content_offset_y,
                    bottom_inset,
                    strut_metrics,
                  );
                  let mut inline_box = InlineBoxItem::new(
                    start_edge,
                    end_edge,
                    content_offset_y,
                    metrics,
                    style.clone(),
                    box_index,
                    direction,
                    unicode_bidi,
                  );
                  inline_box.box_id = box_id;
                  inline_box.margin_left = margin_left;
                  inline_box.margin_right = margin_right;
                  inline_box.border_left = border_left;
                  inline_box.border_right = border_right;
                  inline_box.border_top = border_top;
                  inline_box.border_bottom = border_bottom;
                  inline_box.bottom_inset = bottom_inset;
                  inline_box.strut_metrics = strut_metrics;
                  inline_box.vertical_align = vertical_align;
                  inline_box.children = remaining_children;
                  return Ok(SplitInlineBoxForLineResult::BreakBefore { inline_box });
                }
              }

              // Emergency breaks are only desirable when there's *nothing* before the inline box on
              // the line. If the parent inline box already placed content, prefer breaking before
              // this child box rather than splitting inside it at an arbitrary character boundary.
              let child_allow_emergency_breaks =
                allow_emergency_breaks && !fragment_has_in_flow_children;
              let split = self.split_inline_box_for_line(
                child_box,
                start_x,
                remaining_width,
                child_allow_emergency_breaks,
              )?;

              match split {
                SplitInlineBoxForLineResult::Split {
                  fragment,
                  remainder,
                  ends_with_hard_break: child_hard_break,
                  force_break: child_force_break,
                } => {
                  fragment_children.push(InlineItem::InlineBox(fragment));
                  if let Some(remainder) = remainder {
                    remaining.push_front(InlineItem::InlineBox(remainder));
                  }
                  if child_hard_break {
                    ends_with_hard_break = true;
                  }
                  if child_force_break {
                    force_break = true;
                  }
                  break;
                }
                SplitInlineBoxForLineResult::BreakBefore {
                  inline_box: child_box,
                } => {
                  // If we've already produced in-flow content for this line, keep it and push the
                  // child box to the next line.
                  if fragment_has_in_flow_children {
                    remaining.push_front(InlineItem::InlineBox(child_box));
                    break;
                  }

                  remaining.push_front(InlineItem::InlineBox(child_box));
                  let remaining_children: Vec<InlineItem> = remaining.into();
                  let metrics = super::compute_inline_box_metrics(
                    &remaining_children,
                    content_offset_y,
                    bottom_inset,
                    strut_metrics,
                  );
                  let mut inline_box = InlineBoxItem::new(
                    start_edge,
                    end_edge,
                    content_offset_y,
                    metrics,
                    style.clone(),
                    box_index,
                    direction,
                    unicode_bidi,
                  );
                  inline_box.box_id = box_id;
                  inline_box.margin_left = margin_left;
                  inline_box.margin_right = margin_right;
                  inline_box.border_left = border_left;
                  inline_box.border_right = border_right;
                  inline_box.border_top = border_top;
                  inline_box.border_bottom = border_bottom;
                  inline_box.bottom_inset = bottom_inset;
                  inline_box.strut_metrics = strut_metrics;
                  inline_box.vertical_align = vertical_align;
                  inline_box.children = remaining_children;
                  return Ok(SplitInlineBoxForLineResult::BreakBefore { inline_box });
                }
              };
            }
            other if !fragment_has_in_flow_children => {
              fragment_children.push(other);
              break;
            }
            other => {
              remaining.push_front(other);
              break;
            }
          }
        }
      }
    }

    let remaining_children: Vec<InlineItem> = remaining.into();
    let has_remainder = !remaining_children.is_empty();
    let fragment_end_edge = if has_remainder { 0.0 } else { end_edge };
    let fragment_border_right = if has_remainder { 0.0 } else { border_right };
    let fragment_margin_right = if has_remainder { 0.0 } else { margin_right };

    let fragment_metrics = super::compute_inline_box_metrics(
      &fragment_children,
      content_offset_y,
      bottom_inset,
      strut_metrics,
    );
    let mut fragment = InlineBoxItem::new(
      start_edge,
      fragment_end_edge,
      content_offset_y,
      fragment_metrics,
      style.clone(),
      box_index,
      direction,
      unicode_bidi,
    );
    fragment.box_id = box_id;
    fragment.margin_left = margin_left;
    fragment.margin_right = fragment_margin_right;
    fragment.border_left = border_left;
    fragment.border_right = fragment_border_right;
    fragment.border_top = border_top;
    fragment.border_bottom = border_bottom;
    fragment.bottom_inset = bottom_inset;
    fragment.strut_metrics = strut_metrics;
    fragment.vertical_align = vertical_align;
    fragment.children = fragment_children;

    let remainder = if has_remainder {
      let remainder_metrics = super::compute_inline_box_metrics(
        &remaining_children,
        content_offset_y,
        bottom_inset,
        strut_metrics,
      );
      let mut remainder = InlineBoxItem::new(
        0.0,
        end_edge,
        content_offset_y,
        remainder_metrics,
        style,
        box_index,
        direction,
        unicode_bidi,
      );
      remainder.box_id = box_id;
      remainder.margin_left = 0.0;
      remainder.margin_right = margin_right;
      remainder.border_left = 0.0;
      remainder.border_right = border_right;
      remainder.border_top = border_top;
      remainder.border_bottom = border_bottom;
      remainder.bottom_inset = bottom_inset;
      remainder.strut_metrics = strut_metrics;
      remainder.vertical_align = vertical_align;
      remainder.children = remaining_children;
      Some(remainder)
    } else {
      None
    };

    Ok(SplitInlineBoxForLineResult::Split {
      fragment,
      remainder,
      ends_with_hard_break,
      force_break,
    })
  }

  fn add_fragmented_inline_box(&mut self, inline_box: InlineBoxItem) -> Result<(), LayoutError> {
    let mut remaining_box = inline_box;
    loop {
      if self.line_clamp_reached {
        self.truncated = true;
        return Ok(());
      }
      check_layout_deadline(&mut self.deadline_counter)?;

      let available_width = (self.current_line_width() - self.current_x).max(0.0);
      let box_start_x = self.current_x;
      let split = self.split_inline_box_for_line(
        remaining_box,
        box_start_x,
        available_width,
        self.current_line.is_empty(),
      )?;

      let (fragment, remainder, ends_with_hard_break, force_break) = match split {
        SplitInlineBoxForLineResult::Split {
          fragment,
          remainder,
          ends_with_hard_break,
          force_break,
        } => (fragment, remainder, ends_with_hard_break, force_break),
        SplitInlineBoxForLineResult::BreakBefore { inline_box } => {
          if self.soft_wrap_opportunity_before_inline_box(&inline_box) {
            self.finish_line()?;
            remaining_box = inline_box;
            continue;
          }

          if let Some(moved) = self.rewind_line_break_before_glued_inline_box(&inline_box) {
            self.finish_line()?;
            for item in moved {
              self.add_item_internal(item)?;
            }
            remaining_box = inline_box;
            continue;
          }

          // No earlier break opportunity exists; keep the inline box glued to the previous content
          // and overflow this line instead of breaking at a prohibited position.
          let width = inline_box.width();
          self.place_item_with_width(InlineItem::InlineBox(inline_box), width);
          break;
        }
      };

      let fragment_width = fragment.total_width();
      self.place_item_with_width(InlineItem::InlineBox(fragment), fragment_width);

      if ends_with_hard_break {
        self.current_line.ends_with_hard_break = true;
      }

      if remainder.is_none() && !ends_with_hard_break && !force_break {
        break;
      }

      self.finish_line()?;

      if let Some(remainder) = remainder {
        remaining_box = remainder;
        continue;
      }
      break;
    }

    Ok(())
  }

  fn soft_wrap_opportunity_before_inline_box(&self, inline_box: &InlineBoxItem) -> bool {
    let Some(next_first) = inline_box
      .children
      .iter()
      .find_map(first_text_char_for_soft_wrap)
    else {
      return true;
    };

    let Some(prev) = self
      .current_line
      .items
      .iter()
      .rev()
      .find_map(|pos| match &pos.item {
        InlineItem::StaticPositionAnchor(_)
        | InlineItem::Floating(_)
        | InlineItem::HardBreak(_) => None,
        other => Some(other),
      })
    else {
      return true;
    };

    let Some(prev_last) = last_text_char_for_soft_wrap(prev) else {
      return true;
    };

    item_allows_soft_wrap(prev)
      && soft_wrap_opportunity_between_chars_with_styles(
        prev_last,
        next_first,
        soft_wrap_style_for_item(prev),
        Some(inline_box.style.as_ref()),
      )
  }

  fn split_inline_box_at_last_break_opportunity(
    &mut self,
    inline_box: &InlineBoxItem,
  ) -> Option<(InlineBoxItem, InlineBoxItem)> {
    if inline_box.children.is_empty() {
      return None;
    }

    let children = &inline_box.children;

    for idx in (0..children.len()).rev() {
      let InlineItem::Text(text_item) = &children[idx] else {
        continue;
      };
      if !allows_soft_wrap(text_item.style.as_ref()) {
        continue;
      }

      let break_opportunity = text_item
        .break_opportunities
        .iter()
        .rev()
        .find(|brk| {
          matches!(brk.break_type, BreakType::Allowed)
            && brk.kind == BreakOpportunityKind::Normal
            && brk.byte_offset > 0
            && brk.byte_offset < text_item.text.len()
        })
        .or_else(|| {
          text_item.break_opportunities.iter().rev().find(|brk| {
            matches!(brk.break_type, BreakType::Allowed)
              && brk.kind == BreakOpportunityKind::Emergency
              && brk.byte_offset > 0
              && brk.byte_offset < text_item.text.len()
          })
        })
        .copied();

      let Some(break_opportunity) = break_opportunity else {
        continue;
      };

      let (mut before, after) = text_item.split_at(
        break_opportunity.byte_offset,
        break_opportunity.adds_hyphen,
        &self.shaper,
        &self.font_context,
        &mut self.reshape_cache,
      )?;

      let mut drop_before = false;
      if matches!(break_opportunity.break_type, BreakType::Allowed)
        && matches!(
          text_item.style.white_space,
          WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
        )
      {
        let trimmed_len = before.text.trim_end_matches(' ').len();
        if trimmed_len < before.text.len() {
          if trimmed_len == 0 {
            drop_before = true;
          } else if let Some((trimmed, _)) = before.split_at(
            trimmed_len,
            false,
            &self.shaper,
            &self.font_context,
            &mut self.reshape_cache,
          ) {
            before = trimmed;
          }
        }
      }

      let mut before_children: Vec<InlineItem> = children[..idx].to_vec();
      if !drop_before && before.advance_for_layout > 0.0 {
        before_children.push(InlineItem::Text(before));
      }

      let mut after_children: Vec<InlineItem> = Vec::new();
      after_children.push(InlineItem::Text(after));
      after_children.extend_from_slice(&children[idx + 1..]);

      let has_in_flow_before = before_children.iter().any(|child| {
        !matches!(
          child,
          InlineItem::StaticPositionAnchor(_) | InlineItem::Floating(_) | InlineItem::HardBreak(_)
        )
      });
      if !has_in_flow_before {
        continue;
      }

      let split_idx = before_children.len();
      let mut before_gaps = Vec::new();
      let mut after_gaps = Vec::new();
      if !inline_box.justify_gaps.is_empty() {
        if split_idx > 1 {
          before_gaps.extend_from_slice(&inline_box.justify_gaps[..split_idx - 1]);
        }
        if split_idx < inline_box.justify_gaps.len() {
          after_gaps.extend_from_slice(&inline_box.justify_gaps[split_idx..]);
        }
      }

      let before_metrics = super::compute_inline_box_metrics(
        &before_children,
        inline_box.content_offset_y,
        inline_box.bottom_inset,
        inline_box.strut_metrics,
      );
      let after_metrics = super::compute_inline_box_metrics(
        &after_children,
        inline_box.content_offset_y,
        inline_box.bottom_inset,
        inline_box.strut_metrics,
      );

      let mut before_box = InlineBoxItem::new(
        inline_box.start_edge,
        0.0,
        inline_box.content_offset_y,
        before_metrics,
        inline_box.style.clone(),
        inline_box.box_index,
        inline_box.direction,
        inline_box.unicode_bidi,
      );
      before_box.box_id = inline_box.box_id;
      before_box.border_left = inline_box.border_left;
      before_box.border_right = 0.0;
      before_box.border_top = inline_box.border_top;
      before_box.border_bottom = inline_box.border_bottom;
      before_box.bottom_inset = inline_box.bottom_inset;
      before_box.strut_metrics = inline_box.strut_metrics;
      before_box.vertical_align = inline_box.vertical_align;
      before_box.children = before_children;
      before_box.justify_gaps = before_gaps;

      let mut after_box = InlineBoxItem::new(
        0.0,
        inline_box.end_edge,
        inline_box.content_offset_y,
        after_metrics,
        inline_box.style.clone(),
        inline_box.box_index,
        inline_box.direction,
        inline_box.unicode_bidi,
      );
      after_box.box_id = inline_box.box_id;
      after_box.border_left = 0.0;
      after_box.border_right = inline_box.border_right;
      after_box.border_top = inline_box.border_top;
      after_box.border_bottom = inline_box.border_bottom;
      after_box.bottom_inset = inline_box.bottom_inset;
      after_box.strut_metrics = inline_box.strut_metrics;
      after_box.vertical_align = inline_box.vertical_align;
      after_box.children = after_children;
      after_box.justify_gaps = after_gaps;

      return Some((before_box, after_box));
    }

    for split_idx in (1..children.len()).rev() {
      let prev = &children[split_idx - 1];
      let next = &children[split_idx];
      let (Some(prev_last), Some(next_first)) = (
        last_text_char_for_soft_wrap(prev),
        first_text_char_for_soft_wrap(next),
      ) else {
        continue;
      };
      if !item_allows_soft_wrap(prev) || !item_allows_soft_wrap(next) {
        continue;
      }
      if !soft_wrap_opportunity_between_chars_with_styles(
        prev_last,
        next_first,
        soft_wrap_style_for_item(prev),
        soft_wrap_style_for_item(next),
      ) {
        continue;
      }

      let before_children: Vec<InlineItem> = children[..split_idx].to_vec();
      let after_children: Vec<InlineItem> = children[split_idx..].to_vec();

      let split_idx = before_children.len();
      let mut before_gaps = Vec::new();
      let mut after_gaps = Vec::new();
      if !inline_box.justify_gaps.is_empty() {
        if split_idx > 1 {
          before_gaps.extend_from_slice(&inline_box.justify_gaps[..split_idx - 1]);
        }
        if split_idx < inline_box.justify_gaps.len() {
          after_gaps.extend_from_slice(&inline_box.justify_gaps[split_idx..]);
        }
      }

      let before_metrics = super::compute_inline_box_metrics(
        &before_children,
        inline_box.content_offset_y,
        inline_box.bottom_inset,
        inline_box.strut_metrics,
      );
      let after_metrics = super::compute_inline_box_metrics(
        &after_children,
        inline_box.content_offset_y,
        inline_box.bottom_inset,
        inline_box.strut_metrics,
      );

      let mut before_box = InlineBoxItem::new(
        inline_box.start_edge,
        0.0,
        inline_box.content_offset_y,
        before_metrics,
        inline_box.style.clone(),
        inline_box.box_index,
        inline_box.direction,
        inline_box.unicode_bidi,
      );
      before_box.box_id = inline_box.box_id;
      before_box.border_left = inline_box.border_left;
      before_box.border_right = 0.0;
      before_box.border_top = inline_box.border_top;
      before_box.border_bottom = inline_box.border_bottom;
      before_box.bottom_inset = inline_box.bottom_inset;
      before_box.strut_metrics = inline_box.strut_metrics;
      before_box.vertical_align = inline_box.vertical_align;
      before_box.children = before_children;
      before_box.justify_gaps = before_gaps;

      let mut after_box = InlineBoxItem::new(
        0.0,
        inline_box.end_edge,
        inline_box.content_offset_y,
        after_metrics,
        inline_box.style.clone(),
        inline_box.box_index,
        inline_box.direction,
        inline_box.unicode_bidi,
      );
      after_box.box_id = inline_box.box_id;
      after_box.border_left = 0.0;
      after_box.border_right = inline_box.border_right;
      after_box.border_top = inline_box.border_top;
      after_box.border_bottom = inline_box.border_bottom;
      after_box.bottom_inset = inline_box.bottom_inset;
      after_box.strut_metrics = inline_box.strut_metrics;
      after_box.vertical_align = inline_box.vertical_align;
      after_box.children = after_children;
      after_box.justify_gaps = after_gaps;

      return Some((before_box, after_box));
    }

    None
  }

  fn rewind_line_break_before_glued_inline_box(
    &mut self,
    inline_box: &InlineBoxItem,
  ) -> Option<Vec<InlineItem>> {
    if self.current_line.is_empty() {
      return None;
    }

    let mut group_first_char = inline_box
      .children
      .iter()
      .find_map(first_text_char_for_soft_wrap)?;

    let original_items = self.current_line.items.clone();
    let original_x = self.current_x;

    let mut moved_rev: Vec<InlineItem> = Vec::new();
    let mut found_break = false;

    loop {
      while matches!(
        self.current_line.items.last().map(|p| &p.item),
        Some(InlineItem::StaticPositionAnchor(_))
          | Some(InlineItem::Floating(_))
          | Some(InlineItem::HardBreak(_))
      ) {
        if let Some(popped) = self.current_line.items.pop() {
          moved_rev.push(popped.item);
        }
      }

      let Some(prev_item) = self.current_line.items.last().map(|p| p.item.clone()) else {
        break;
      };

      let Some(prev_last) = last_text_char_for_soft_wrap(&prev_item) else {
        found_break = !moved_rev.is_empty();
        break;
      };

      if item_allows_soft_wrap(&prev_item)
        && soft_wrap_opportunity_between_chars_with_styles(
          prev_last,
          group_first_char,
          soft_wrap_style_for_item(&prev_item),
          Some(inline_box.style.as_ref()),
        )
      {
        found_break = !moved_rev.is_empty();
        break;
      }

      if let InlineItem::Text(text_item) = &prev_item {
        if allows_soft_wrap(text_item.style.as_ref()) {
          let break_opportunity = text_item
            .break_opportunities
            .iter()
            .rev()
            .find(|brk| {
              matches!(brk.break_type, BreakType::Allowed)
                && brk.kind == BreakOpportunityKind::Normal
                && brk.byte_offset > 0
                && brk.byte_offset < text_item.text.len()
            })
            .or_else(|| {
              text_item.break_opportunities.iter().rev().find(|brk| {
                matches!(brk.break_type, BreakType::Allowed)
                  && brk.kind == BreakOpportunityKind::Emergency
                  && brk.byte_offset > 0
                  && brk.byte_offset < text_item.text.len()
              })
            })
            .copied();

          if let Some(break_opportunity) = break_opportunity {
            if let Some((mut before, after)) = text_item.split_at(
              break_opportunity.byte_offset,
              break_opportunity.adds_hyphen,
              &self.shaper,
              &self.font_context,
              &mut self.reshape_cache,
            ) {
              let mut drop_before = false;
              if matches!(break_opportunity.break_type, BreakType::Allowed)
                && matches!(
                  text_item.style.white_space,
                  WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
                )
              {
                let trimmed_len = before.text.trim_end_matches(' ').len();
                if trimmed_len < before.text.len() {
                  if trimmed_len == 0 {
                    drop_before = true;
                  } else if let Some((trimmed, _)) = before.split_at(
                    trimmed_len,
                    false,
                    &self.shaper,
                    &self.font_context,
                    &mut self.reshape_cache,
                  ) {
                    before = trimmed;
                  }
                }
              }

              if drop_before || before.advance_for_layout <= 0.0 {
                self.current_line.items.pop();
              } else if let Some(last_mut) = self.current_line.items.last_mut() {
                last_mut.item = InlineItem::Text(before);
              }

              moved_rev.push(InlineItem::Text(after));
              found_break = true;
              break;
            }
          }
        }
      }

      if let InlineItem::InlineBox(prev_box) = &prev_item {
        if let Some((before_box, after_box)) =
          self.split_inline_box_at_last_break_opportunity(prev_box)
        {
          if let Some(last_mut) = self.current_line.items.last_mut() {
            last_mut.item = InlineItem::InlineBox(before_box);
          }
          moved_rev.push(InlineItem::InlineBox(after_box));
          found_break = true;
          break;
        }
      }

      if let Some(popped) = self.current_line.items.pop() {
        if let Some(ch) = first_text_char_for_soft_wrap(&popped.item) {
          group_first_char = ch;
        }
        moved_rev.push(popped.item);
        continue;
      }

      break;
    }

    if !found_break || moved_rev.is_empty() {
      self.current_line.items = original_items;
      self.current_x = original_x;

      self.baseline_acc = LineBaselineAccumulator::new(&self.strut_metrics);
      for positioned in &mut self.current_line.items {
        let vertical_align = positioned.item.vertical_align();
        let metrics = if vertical_align.is_line_relative() {
          positioned.item.baseline_metrics()
        } else {
          positioned.item.line_metrics()
        };
        positioned.baseline_offset = if vertical_align.is_line_relative() {
          self
            .baseline_acc
            .add_line_relative(&metrics, vertical_align);
          0.0
        } else {
          self.baseline_acc.add_baseline_relative(
            &metrics,
            vertical_align,
            Some(&self.strut_metrics),
          )
        };
      }

      return None;
    }

    moved_rev.reverse();

    self.current_x = self.current_line.items.iter().map(|p| p.item.width()).sum();

    self.baseline_acc = LineBaselineAccumulator::new(&self.strut_metrics);
    for positioned in &mut self.current_line.items {
      let vertical_align = positioned.item.vertical_align();
      let metrics = if vertical_align.is_line_relative() {
        positioned.item.baseline_metrics()
      } else {
        positioned.item.line_metrics()
      };
      positioned.baseline_offset = if vertical_align.is_line_relative() {
        self
          .baseline_acc
          .add_line_relative(&metrics, vertical_align);
        0.0
      } else {
        self
          .baseline_acc
          .add_baseline_relative(&metrics, vertical_align, Some(&self.strut_metrics))
      };
    }

    Some(moved_rev)
  }

  /// Places an item on the current line without breaking
  fn place_item_with_width(&mut self, item: InlineItem, item_width: f32) {
    let vertical_align = item.vertical_align();
    let metrics = if vertical_align.is_line_relative() {
      item.baseline_metrics()
    } else {
      item.line_metrics()
    };

    // Calculate baseline offset
    let baseline_offset = if vertical_align.is_line_relative() {
      self
        .baseline_acc
        .add_line_relative(&metrics, vertical_align);
      0.0 // Will be adjusted in finalization
    } else {
      match &item {
        InlineItem::InlineBox(inline_box) => {
          // CSS 2.1 §10.8.1 defines baseline-relative `vertical-align` values for inline
          // non-replaced elements ("inline boxes") in terms of the element's own line-height box.
          // In particular, `vertical-align: middle` aligns the midpoint of the *line-height box*
          // with the parent's baseline plus half the parent's x-height.
          //
          // Our `InlineItem::line_metrics()` for inline boxes reflects the bounds of the aligned
          // subtree (so the line box height accounts for descendants like larger-font text and
          // inline replaced elements). Those subtree bounds can extend above/below the inline
          // element's own line-height box, but they must not affect how the element's baseline
          // shift is computed.
          //
          // Compute the baseline shift from the inline box's strut (its line-height box), then
          // apply that shift to the aligned subtree bounds when updating the line box height.
          self.baseline_acc.has_items = true;
          let shift = self.baseline_acc.compute_baseline_shift(
            vertical_align,
            &inline_box.strut_metrics,
            Some(&self.strut_metrics),
          );
          let item_ascent = metrics.baseline_offset - shift;
          let item_descent = (metrics.height - metrics.baseline_offset) + shift;
          self.baseline_acc.max_ascent = self.baseline_acc.max_ascent.max(item_ascent);
          self.baseline_acc.max_descent = self.baseline_acc.max_descent.max(item_descent);
          shift
        }
        _ => {
          // Baseline-relative alignments (e.g., middle/sub/super/text-top) depend on the parent's
          // font metrics. The strut represents the parent inline box, so thread it through to
          // compute x-height/ascent-based offsets correctly.
          // `LineBaselineAccumulator` returns a baseline shift in the same coordinate system used
          // for fragment placement (positive y = down), so store it directly.
          self.baseline_acc.add_baseline_relative(
            &metrics,
            vertical_align,
            Some(&self.strut_metrics),
          )
        }
      }
    };

    let positioned = PositionedItem {
      item,
      x: self.current_x,
      baseline_offset,
    };

    self.current_x += item_width;
    self.current_line.items.push(positioned);
  }

  pub(crate) fn split_text_item(
    &mut self,
    item: &TextItem,
    byte_offset: usize,
    insert_hyphen: bool,
  ) -> Option<(TextItem, TextItem)> {
    item.split_at(
      byte_offset,
      insert_hyphen,
      &self.shaper,
      &self.font_context,
      &mut self.reshape_cache,
    )
  }

  /// Forces a line break (e.g., from mandatory break)
  pub fn force_break(&mut self) -> Result<(), LayoutError> {
    if self.line_clamp_reached {
      self.truncated = true;
      return Ok(());
    }
    check_layout_deadline(&mut self.deadline_counter)?;
    self.current_line.ends_with_hard_break = true;
    self.finish_line()
  }

  fn trim_soft_wrap_trailing_spaces(&mut self) {
    if self.current_line.ends_with_hard_break || self.current_line.items.is_empty() {
      return;
    }

    let mut removed_any = false;
    let mut trailing_zero_width = Vec::new();
    loop {
      while matches!(
        self.current_line.items.last().map(|p| &p.item),
        Some(InlineItem::StaticPositionAnchor(_))
          | Some(InlineItem::Floating(_))
          | Some(InlineItem::HardBreak(_))
      ) {
        if let Some(item) = self.current_line.items.pop() {
          trailing_zero_width.push(item);
        } else {
          break;
        }
      }

      let Some(last) = self.current_line.items.last() else {
        break;
      };

      if let InlineItem::Text(text) = &last.item {
        let collapsible = matches!(
          text.style.white_space,
          WhiteSpace::Normal | WhiteSpace::Nowrap | WhiteSpace::PreLine
        );
        let all_spaces = !text.text.is_empty() && text.text.chars().all(|ch| ch == ' ');
        if collapsible && all_spaces && !text.is_marker {
          let width = last.item.width();
          self.current_x = (self.current_x - width).max(0.0);
          self.current_line.items.pop();
          removed_any = true;
          continue;
        }
      }

      break;
    }

    for item in trailing_zero_width.into_iter().rev() {
      self.current_line.items.push(item);
    }

    if !removed_any {
      return;
    }

    // The baseline accumulator is computed incrementally. If we remove items from the line, we
    // need to recompute it so the line box height/baseline do not include trimmed whitespace.
    self.baseline_acc = LineBaselineAccumulator::new(&self.strut_metrics);
    for positioned in &mut self.current_line.items {
      let vertical_align = positioned.item.vertical_align();
      let metrics = if vertical_align.is_line_relative() {
        positioned.item.baseline_metrics()
      } else {
        positioned.item.line_metrics()
      };
      positioned.baseline_offset = if vertical_align.is_line_relative() {
        self
          .baseline_acc
          .add_line_relative(&metrics, vertical_align);
        0.0
      } else {
        match &positioned.item {
          InlineItem::InlineBox(inline_box) => {
            self.baseline_acc.has_items = true;
            let shift = self.baseline_acc.compute_baseline_shift(
              vertical_align,
              &inline_box.strut_metrics,
              Some(&self.strut_metrics),
            );
            let item_ascent = metrics.baseline_offset - shift;
            let item_descent = (metrics.height - metrics.baseline_offset) + shift;
            self.baseline_acc.max_ascent = self.baseline_acc.max_ascent.max(item_ascent);
            self.baseline_acc.max_descent = self.baseline_acc.max_descent.max(item_descent);
            shift
          }
          _ => self.baseline_acc.add_baseline_relative(
            &metrics,
            vertical_align,
            Some(&self.strut_metrics),
          ),
        }
      };
    }
  }

  /// Finishes the current line and starts a new one
  fn finish_line(&mut self) -> Result<(), LayoutError> {
    check_layout_deadline(&mut self.deadline_counter)?;
    self.trim_soft_wrap_trailing_spaces();
    if !self.current_line.is_empty() || self.current_line.ends_with_hard_break {
      let bookkeeping_only_line = !self.current_line.items.is_empty()
        && !self.current_line.ends_with_hard_break
        && self.current_line.items.iter().all(|item| match &item.item {
          InlineItem::Floating(_) => true,
          InlineItem::StaticPositionAnchor(anchor) => {
            anchor.running.is_none() && anchor.footnote.is_none()
          }
          _ => false,
        });
      // Calculate final line metrics
      self.current_line.width = self.current_x;
      self.current_line.height = self.baseline_acc.line_height();
      self.current_line.baseline = self.baseline_acc.baseline_position();
      self.current_line.y_offset = self.current_y;
      let line_width = self.current_line_width();
      self.current_line.available_width = line_width;
      if self.current_line_space.is_some() {
        self.current_line.box_width = self
          .current_line_space
          .map(|space| space.width)
          .unwrap_or(line_width);
      } else {
        self.current_line.box_width = line_width;
      }

      // Adjust Y positions for top/bottom aligned items
      for positioned in &mut self.current_line.items {
        check_layout_deadline(&mut self.deadline_counter)?;
        let align = positioned.item.vertical_align();
        match align {
          VerticalAlign::Top => {
            positioned.baseline_offset =
              positioned.item.baseline_metrics().baseline_offset - self.current_line.baseline;
          }
          VerticalAlign::Bottom => {
            let metrics = positioned.item.baseline_metrics();
            positioned.baseline_offset = self.current_line.height
              - metrics.height
              - (self.current_line.baseline - metrics.baseline_offset);
          }
          _ => {}
        }
      }

      if bookkeeping_only_line {
        // `position:absolute`/`fixed` descendants and floats are out-of-flow and must not create an
        // empty line box that advances the block cursor (CSS 2.1 §9.5/§9.5.1). We keep the line in
        // the stream so static-position anchors can still resolve, but give it zero block-size so
        // it does not affect the formatting context's height.
        self.current_line.height = 0.0;
      }

      let ended_hard = self.current_line.ends_with_hard_break;
      let finished_height = self.current_line.height;
      self.lines.push(std::mem::take(&mut self.current_line));
      self.current_y += finished_height;
      if let Some(clear) = self.pending_clear.take() {
        if let Some(integration) = self.float_integration.as_ref() {
          let query_y = self.float_base_y + self.current_y;
          let cleared_y = integration.compute_clearance(query_y, clear);
          if cleared_y.is_finite() && query_y.is_finite() {
            self.current_y = self.current_y.max((cleared_y - self.float_base_y).max(0.0));
          }
          // Record the cleared offset only when clearance actually moved the cursor; callers use
          // this to advance the containing block even if no line box is emitted at the cleared
          // position.
          if self.current_y > (query_y - self.float_base_y) + 0.0001 {
            self.max_clear_offset = self.max_clear_offset.max(self.current_y);
          }
        }
      }
      self.next_line_is_para_start = ended_hard;

      if let Some(limit) = self.line_clamp {
        if self.lines.len() >= limit {
          self.line_clamp_reached = true;
        }
      }
    }

    // Reset for new line
    self.current_x = 0.0;
    self.baseline_acc = LineBaselineAccumulator::new(&self.strut_metrics);
    let next_direction = if let Some(level) = self.base_level {
      if level.is_rtl() {
        Direction::Rtl
      } else {
        Direction::Ltr
      }
    } else {
      Direction::Ltr
    };
    self.current_line = Line {
      resolved_direction: next_direction,
      ..Line::new()
    };
    self.start_new_line();
    Ok(())
  }

  /// Run bidi reordering across all built lines, respecting paragraph boundaries.
  ///
  /// The Unicode Bidi algorithm operates at paragraph scope; explicit embeddings/isolates can span
  /// line breaks. We therefore resolve bidi across each paragraph (lines separated by hard breaks)
  /// and then reorder each line using the paragraph-level embedding results.
  fn reorder_lines_for_bidi(&mut self) -> Result<(), LayoutError> {
    check_layout_deadline(&mut self.deadline_counter)?;
    if self.lines.is_empty() {
      return Ok(());
    }

    let mut ranges = Vec::with_capacity(4);
    let mut start = 0usize;
    for (idx, line) in self.lines.iter().enumerate() {
      check_layout_deadline(&mut self.deadline_counter)?;
      if line.ends_with_hard_break {
        ranges.push((start, idx + 1));
        start = idx + 1;
      }
    }

    if start < self.lines.len() {
      ranges.push((start, self.lines.len()));
    }

    let base_level = self.base_level;
    let shaper = self.shaper.clone();
    let font_context = self.font_context.clone();

    for (start, end) in ranges {
      check_layout_deadline(&mut self.deadline_counter)?;
      let paragraph_level = if self.root_unicode_bidi == UnicodeBidi::Plaintext
        || Self::paragraph_all_plaintext(&self.lines[start..end])
      {
        None
      } else {
        base_level
      };
      reorder_paragraph(
        &mut self.lines[start..end],
        &mut self.deadline_counter,
        paragraph_level,
        self.root_unicode_bidi,
        self.root_direction,
        &shaper,
        &font_context,
      )?;
    }
    Ok(())
  }

  fn paragraph_all_plaintext(lines: &[Line]) -> bool {
    let mut saw_plaintext = false;
    let all_plain = lines.iter().all(|line| {
      line
        .items
        .iter()
        .all(|p| Self::item_allows_plaintext(&p.item, &mut saw_plaintext))
    });
    all_plain && saw_plaintext
  }

  fn item_allows_plaintext(item: &InlineItem, saw_plaintext: &mut bool) -> bool {
    use crate::style::types::UnicodeBidi;
    match item {
      InlineItem::Text(t) => {
        if matches!(t.style.unicode_bidi, UnicodeBidi::Plaintext) {
          *saw_plaintext = true;
          true
        } else {
          false
        }
      }
      InlineItem::HardBreak(_) => true,
      InlineItem::InlineBox(b) => {
        if matches!(b.unicode_bidi, UnicodeBidi::Plaintext) {
          *saw_plaintext = true;
          true
        } else {
          b.children
            .iter()
            .all(|c| Self::item_allows_plaintext(c, saw_plaintext))
        }
      }
      InlineItem::Floating(f) => {
        if matches!(f.unicode_bidi, UnicodeBidi::Plaintext) {
          *saw_plaintext = true;
          true
        } else {
          false
        }
      }
      InlineItem::Ruby(r) => {
        if matches!(r.unicode_bidi, UnicodeBidi::Plaintext) {
          *saw_plaintext = true;
          true
        } else {
          r.segments.iter().all(|seg| {
            seg
              .base_items
              .iter()
              .all(|c| Self::item_allows_plaintext(c, saw_plaintext))
          })
        }
      }
      InlineItem::SoftBreak
      | InlineItem::InlineBlock(_)
      | InlineItem::Replaced(_)
      | InlineItem::Tab(_) => true,
      InlineItem::StaticPositionAnchor(_) => true,
    }
  }

  /// Finishes building and returns all lines
  pub fn finish(mut self) -> Result<LineBuildResult, LayoutError> {
    // Finish any remaining line
    self.finish_line()?;
    self.reorder_lines_for_bidi()?;
    apply_line_padding_to_lines(&mut self.lines, &mut self.deadline_counter)?;
    Ok(LineBuildResult {
      lines: self.lines,
      truncated: self.truncated,
      max_clear_offset: self.max_clear_offset,
    })
  }

  /// Returns the current line width
  pub fn current_width(&self) -> f32 {
    self.current_x
  }

  /// Returns true if current line is empty
  pub fn is_current_line_empty(&self) -> bool {
    self.current_line.is_empty()
  }

  pub fn is_line_clamp_reached(&self) -> bool {
    self.line_clamp_reached
  }

  pub fn mark_truncated(&mut self) {
    self.truncated = true;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualLineSide {
  Left,
  Right,
}

fn inline_item_has_line_padding_content(item: &InlineItem) -> bool {
  match item {
    InlineItem::Text(_)
    | InlineItem::Tab(_)
    | InlineItem::InlineBlock(_)
    | InlineItem::Ruby(_)
    | InlineItem::Replaced(_) => true,
    InlineItem::InlineBox(b) => b
      .children
      .iter()
      .any(|child| inline_item_has_line_padding_content(child)),
    InlineItem::Floating(_) | InlineItem::StaticPositionAnchor(_) => false,
    InlineItem::SoftBreak | InlineItem::HardBreak(_) => false,
  }
}

fn find_line_edge_content_index(items: &[PositionedItem], side: VisualLineSide) -> Option<usize> {
  match side {
    VisualLineSide::Left => items
      .iter()
      .position(|p| inline_item_has_line_padding_content(&p.item)),
    VisualLineSide::Right => items
      .iter()
      .rposition(|p| inline_item_has_line_padding_content(&p.item)),
  }
}

fn apply_line_padding_to_innermost_inline_box(
  item: &mut InlineItem,
  side: VisualLineSide,
  deadline_counter: &mut usize,
) -> Result<bool, LayoutError> {
  check_layout_deadline(deadline_counter)?;
  let InlineItem::InlineBox(inline_box) = item else {
    return Ok(false);
  };

  let len = inline_box.children.len();
  let mut child_index: Option<usize> = None;
  match side {
    VisualLineSide::Left => {
      for i in 0..len {
        if inline_item_has_line_padding_content(&inline_box.children[i]) {
          child_index = Some(i);
          break;
        }
      }
    }
    VisualLineSide::Right => {
      for i in (0..len).rev() {
        if inline_item_has_line_padding_content(&inline_box.children[i]) {
          child_index = Some(i);
          break;
        }
      }
    }
  }

  let Some(child_idx) = child_index else {
    return Ok(false);
  };

  if apply_line_padding_to_innermost_inline_box(
    &mut inline_box.children[child_idx],
    side,
    deadline_counter,
  )? {
    return Ok(true);
  }

  let padding = inline_box.style.line_padding;
  let padding = if padding.is_finite() { padding } else { 0.0 };
  match side {
    VisualLineSide::Left => inline_box.line_padding_start = padding,
    VisualLineSide::Right => inline_box.line_padding_end = padding,
  }
  Ok(true)
}

fn apply_line_padding_to_lines(
  lines: &mut [Line],
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  for line in lines.iter_mut() {
    check_layout_deadline(deadline_counter)?;
    if line.items.is_empty() {
      continue;
    }

    let (start_side, end_side) = match line.resolved_direction {
      Direction::Ltr => (VisualLineSide::Left, VisualLineSide::Right),
      Direction::Rtl => (VisualLineSide::Right, VisualLineSide::Left),
    };

    let start_index = find_line_edge_content_index(&line.items, start_side);
    let end_index = find_line_edge_content_index(&line.items, end_side);

    if let Some(index) = start_index {
      apply_line_padding_to_innermost_inline_box(
        &mut line.items[index].item,
        start_side,
        deadline_counter,
      )?;
    }

    if let Some(index) = end_index {
      apply_line_padding_to_innermost_inline_box(
        &mut line.items[index].item,
        end_side,
        deadline_counter,
      )?;
    }

    // Widths changed; recompute x positions and line width so downstream logic (e.g. overflow
    // detection) sees the updated geometry.
    let mut x = 0.0;
    for positioned in &mut line.items {
      positioned.x = x;
      x += positioned.item.width();
    }
    line.width = x;
  }
  Ok(())
}

/// Resolve bidi at paragraph scope and reorder each line using the paragraph embedding levels.
fn reorder_paragraph(
  lines: &mut [Line],
  deadline_counter: &mut usize,
  base_level: Option<Level>,
  root_unicode_bidi: UnicodeBidi,
  root_direction: Direction,
  shaper: &ShapingPipeline,
  font_context: &FontContext,
) -> Result<(), LayoutError> {
  check_layout_deadline(deadline_counter)?;
  if lines.is_empty() {
    return Ok(());
  }

  #[derive(Clone)]
  struct BidiScope {
    unicode_bidi: UnicodeBidi,
    key: Option<u64>,
    open: &'static [char],
    close: &'static [char],
  }

  impl PartialEq for BidiScope {
    fn eq(&self, other: &Self) -> bool {
      if self.unicode_bidi != other.unicode_bidi {
        return false;
      }

      // `unicode-bidi: plaintext` ignores the styled `direction`. Use a per-element key so
      // adjacent plaintext boxes don't merge into a single FSI scope (each plaintext element must
      // resolve its own first-strong direction), while still keeping all leaves within the same
      // plaintext element grouped.
      if matches!(self.unicode_bidi, UnicodeBidi::Plaintext) {
        return self.key == other.key;
      }

      self.open == other.open && self.close == other.close
    }
  }

  impl Eq for BidiScope {}

  #[derive(Clone, Copy)]
  struct ContentChar {
    leaf_index: usize,
    local_start: usize,
    local_end: usize,
    bidi_byte_index: usize,
  }

  #[derive(Clone, Copy)]
  struct BidiChar {
    /// Index into `content_map` when this entry corresponds to visible content; `None` for
    /// explicit embedding/isolate control characters injected into `paragraph_text`.
    content_index: Option<usize>,
    bidi_byte_index: usize,
  }

  #[derive(Clone, Copy)]
  struct VisualSegment {
    leaf_index: usize,
    local_start: usize,
    local_end: usize,
    level: Level,
  }

  fn scope_for(
    unicode_bidi: UnicodeBidi,
    direction: Direction,
    key: Option<u64>,
  ) -> Option<BidiScope> {
    use UnicodeBidi::*;

    const LRE: char = '\u{202A}';
    const RLE: char = '\u{202B}';
    const LRO: char = '\u{202D}';
    const RLO: char = '\u{202E}';
    const PDF: char = '\u{202C}';
    const LRI: char = '\u{2066}';
    const RLI: char = '\u{2067}';
    const FSI: char = '\u{2068}';
    const PDI: char = '\u{2069}';

    let (open, close, key) = match unicode_bidi {
      Normal => return None,
      // CSS `unicode-bidi: plaintext` behaves like an isolate with "direction: auto", which
      // corresponds to Unicode FSI/PDI (first-strong isolate).
      Plaintext => (&[FSI][..], &[PDI][..], key),
      Embed => {
        if matches!(direction, Direction::Rtl) {
          (&[RLE][..], &[PDF][..], None)
        } else {
          (&[LRE][..], &[PDF][..], None)
        }
      }
      BidiOverride => {
        if matches!(direction, Direction::Rtl) {
          (&[RLO][..], &[PDF][..], None)
        } else {
          (&[LRO][..], &[PDF][..], None)
        }
      }
      Isolate => {
        if matches!(direction, Direction::Rtl) {
          (&[RLI][..], &[PDI][..], None)
        } else {
          (&[LRI][..], &[PDI][..], None)
        }
      }
      IsolateOverride => {
        if matches!(direction, Direction::Rtl) {
          (&[RLI, RLO][..], &[PDF, PDI][..], None)
        } else {
          (&[LRI, LRO][..], &[PDF, PDI][..], None)
        }
      }
    };

    Some(BidiScope {
      unicode_bidi,
      key,
      open,
      close,
    })
  }

  let mut box_counter = 0usize;
  let mut line_leaves: Vec<Vec<BidiLeaf>> = Vec::with_capacity(lines.len());
  // Reuse the box stack buffer across items to avoid repeated Vec growth during flattening.
  let mut box_stack: Vec<BoxContext> = Vec::with_capacity(8);
  for line in lines.iter() {
    check_layout_deadline(deadline_counter)?;
    // Preallocate a small buffer to reduce churn, but cap it to avoid huge per-line allocations on
    // long runs of plain text.
    let mut leaves = Vec::with_capacity(line.items.len().max(LINE_DEFAULT_ITEM_CAPACITY).min(64));
    for positioned in &line.items {
      check_layout_deadline(deadline_counter)?;
      box_stack.clear();
      flatten_positioned_item(positioned, &mut box_stack, &mut box_counter, &mut leaves);
    }
    line_leaves.push(leaves);
  }

  let total_leaf_count: usize = line_leaves.iter().map(|leaves| leaves.len()).sum();
  let mut paragraph_leaves: Vec<ParagraphLeaf> = Vec::with_capacity(total_leaf_count);
  let mut leaf_contexts: Vec<Vec<(UnicodeBidi, Direction)>> = Vec::with_capacity(total_leaf_count);
  let mut paragraph_text = String::new();
  let mut open_scopes: Vec<BidiScope> = Vec::with_capacity(8);
  let mut content_map: Vec<ContentChar> = Vec::new();
  let mut bidi_chars: Vec<BidiChar> = Vec::new();
  let mut line_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(lines.len());
  let root_context = (root_unicode_bidi, root_direction);

  for leaves in &line_leaves {
    check_layout_deadline(deadline_counter)?;
    let line_start = bidi_chars.len();
    for leaf in leaves {
      check_layout_deadline(deadline_counter)?;
      let leaf_index = paragraph_leaves.len();
      let mut stack: Vec<(UnicodeBidi, Direction)> = Vec::with_capacity(leaf.box_stack.len() + 2);
      stack.push(root_context);
      stack.extend(leaf.box_stack.iter().map(|c| (c.unicode_bidi, c.direction)));
      let leaf_dir = match &leaf.item {
        InlineItem::Text(t) if matches!(t.style.unicode_bidi, UnicodeBidi::Plaintext) => {
          t.base_direction
        }
        _ => leaf.item.direction(),
      };
      stack.push((leaf.item.unicode_bidi(), leaf_dir));
      paragraph_leaves.push(ParagraphLeaf {
        leaf: leaf.clone(),
        bidi_context: None,
      });

      let plaintext_key_for_box = |ctx: &BoxContext| -> u64 {
        if ctx.box_id != 0 {
          ctx.box_id as u64
        } else {
          ctx.id as u64
        }
      };
      let plaintext_key_for_item = |item: &InlineItem| -> u64 {
        match item {
          InlineItem::Text(t) => {
            if t.box_id != 0 {
              t.box_id as u64
            } else {
              t.source_id
            }
          }
          InlineItem::SoftBreak => leaf_index as u64,
          InlineItem::Tab(_) => leaf_index as u64,
          InlineItem::HardBreak(_) => leaf_index as u64,
          InlineItem::InlineBox(_) => leaf_index as u64,
          InlineItem::InlineBlock(_) => leaf_index as u64,
          InlineItem::Ruby(_) => leaf_index as u64,
          InlineItem::Replaced(_) => leaf_index as u64,
          InlineItem::Floating(_) => leaf_index as u64,
          InlineItem::StaticPositionAnchor(_) => leaf_index as u64,
        }
      };

      let desired_scopes: SmallVec<[BidiScope; 8]> =
        std::iter::once((root_context.0, root_context.1, None))
          .chain(leaf.box_stack.iter().map(|ctx| {
            let key = matches!(ctx.unicode_bidi, UnicodeBidi::Plaintext)
              .then(|| plaintext_key_for_box(ctx));
            (ctx.unicode_bidi, ctx.direction, key)
          }))
          .chain(std::iter::once({
            let ub = leaf.item.unicode_bidi();
            let dir = leaf.item.direction();
            let key =
              matches!(ub, UnicodeBidi::Plaintext).then(|| plaintext_key_for_item(&leaf.item));
            (ub, dir, key)
          }))
          .filter_map(|(ub, dir, key)| scope_for(ub, dir, key))
          .collect();
      let common = desired_scopes
        .iter()
        .zip(open_scopes.iter())
        .take_while(|(a, b)| *a == *b)
        .count();

      for scope in open_scopes.drain(common..).rev() {
        check_layout_deadline(deadline_counter)?;
        for ch in scope.close {
          let bidi_byte_index = paragraph_text.len();
          paragraph_text.push(*ch);
          bidi_chars.push(BidiChar {
            content_index: None,
            bidi_byte_index,
          });
        }
      }

      for scope in desired_scopes.iter().skip(common) {
        check_layout_deadline(deadline_counter)?;
        for ch in scope.open {
          let bidi_byte_index = paragraph_text.len();
          paragraph_text.push(*ch);
          bidi_chars.push(BidiChar {
            content_index: None,
            bidi_byte_index,
          });
        }
        open_scopes.push(scope.clone());
      }

      leaf_contexts.push(stack);

      match &leaf.item {
        InlineItem::Text(t) => {
          for (byte_idx, ch) in t.text.char_indices() {
            check_layout_deadline(deadline_counter)?;
            let bidi_byte_index = paragraph_text.len();
            let content_index = content_map.len();
            content_map.push(ContentChar {
              leaf_index,
              local_start: byte_idx,
              local_end: byte_idx + ch.len_utf8(),
              bidi_byte_index,
            });
            paragraph_text.push(ch);
            bidi_chars.push(BidiChar {
              content_index: Some(content_index),
              bidi_byte_index,
            });
          }
        }
        InlineItem::Tab(_) => {
          let bidi_byte_index = paragraph_text.len();
          let content_index = content_map.len();
          content_map.push(ContentChar {
            leaf_index,
            local_start: 0,
            local_end: 0,
            bidi_byte_index,
          });
          paragraph_text.push('\t');
          bidi_chars.push(BidiChar {
            content_index: Some(content_index),
            bidi_byte_index,
          });
        }
        InlineItem::InlineBox(_) => {}
        _ => {
          let bidi_byte_index = paragraph_text.len();
          let content_index = content_map.len();
          content_map.push(ContentChar {
            leaf_index,
            local_start: 0,
            local_end: 0,
            bidi_byte_index,
          });
          paragraph_text.push('\u{FFFC}');
          bidi_chars.push(BidiChar {
            content_index: Some(content_index),
            bidi_byte_index,
          });
        }
      }
    }
    line_ranges.push(line_start..bidi_chars.len());
  }

  for scope in open_scopes.iter().rev() {
    check_layout_deadline(deadline_counter)?;
    for ch in scope.close {
      paragraph_text.push(*ch);
    }
  }

  if content_map.is_empty() {
    return Ok(());
  }

  let resolved_base = if let Some(level) = base_level {
    level
  } else if paragraph_text.is_empty() {
    Level::ltr()
  } else {
    // When `base_level` is omitted we are resolving `unicode-bidi: plaintext` paragraph direction
    // (first-strong). The Unicode bidi algorithm's paragraph base resolution does not look through
    // nested isolates (e.g. an outer plaintext element containing an inner plaintext element), so
    // using `BidiInfo::new(.., None)` here can incorrectly fall back to LTR. Match CSS plaintext by
    // scanning the visible content directly for the first strong bidi class.
    use unicode_bidi::BidiClass;
    paragraph_text
      .chars()
      .find_map(|ch| match unicode_bidi::bidi_class(ch) {
        BidiClass::L => Some(Level::ltr()),
        BidiClass::R | BidiClass::AL => Some(Level::rtl()),
        _ => None,
      })
      .unwrap_or_else(Level::ltr)
  };

  let bidi = unicode_bidi::BidiInfo::new(&paragraph_text, Some(resolved_base));
  let paragraph_direction = if resolved_base.is_rtl() {
    Direction::Rtl
  } else {
    Direction::Ltr
  };
  for line in lines.iter_mut() {
    check_layout_deadline(deadline_counter)?;
    line.resolved_direction = paragraph_direction;
  }

  for (leaf, stack) in paragraph_leaves.iter_mut().zip(leaf_contexts.iter()) {
    check_layout_deadline(deadline_counter)?;
    leaf.bidi_context =
      crate::layout::contexts::inline::explicit_bidi_context(paragraph_direction, stack);
  }

  let mut bidi_levels: Vec<Level> = Vec::with_capacity(bidi_chars.len());
  for entry in &bidi_chars {
    check_layout_deadline(deadline_counter)?;
    let lvl = bidi
      .levels
      .get(entry.bidi_byte_index)
      .copied()
      .unwrap_or(resolved_base);
    bidi_levels.push(lvl);
  }

  let push_segment = |entry: &ContentChar, level: Level, segments: &mut Vec<VisualSegment>| {
    if let Some(last) = segments.last_mut() {
      if last.leaf_index == entry.leaf_index && last.level == level {
        if last.local_end == entry.local_start {
          last.local_end = entry.local_end;
          return;
        }
        if entry.local_end == last.local_start {
          last.local_start = entry.local_start;
          return;
        }
      }
    }

    segments.push(VisualSegment {
      leaf_index: entry.leaf_index,
      local_start: entry.local_start,
      local_end: entry.local_end,
      level,
    });
  };

  let mut reshape_cache = ReshapeCache::default();

  for (line_idx, line_range) in line_ranges.into_iter().enumerate() {
    check_layout_deadline(deadline_counter)?;
    if line_range.is_empty() {
      continue;
    }

    let slice_levels = &bidi_levels[line_range.clone()];
    let order = unicode_bidi::BidiInfo::reorder_visual(slice_levels);
    let leaf_count = line_leaves
      .get(line_idx)
      .map(|leaves| leaves.len())
      .unwrap_or(0);
    let segment_capacity = leaf_count
      .saturating_mul(2)
      .max(LINE_DEFAULT_ITEM_CAPACITY)
      .min(64);
    let mut segments: Vec<VisualSegment> = Vec::with_capacity(segment_capacity);
    for visual_idx in order {
      check_layout_deadline(deadline_counter)?;
      let bidi_idx = line_range.start + visual_idx;
      if let Some(content_idx) = bidi_chars
        .get(bidi_idx)
        .and_then(|entry| entry.content_index)
      {
        if let Some(entry) = content_map.get(content_idx) {
          let lvl = bidi_levels.get(bidi_idx).copied().unwrap_or(resolved_base);
          push_segment(entry, lvl, &mut segments);
        }
      }
    }

    if segments.is_empty() {
      continue;
    }

    let mut visual_fragments: Vec<BidiLeaf> = Vec::with_capacity(segments.len());
    for seg in segments {
      check_layout_deadline(deadline_counter)?;
      if let Some(para_leaf) = paragraph_leaves.get(seg.leaf_index) {
        let item = match &para_leaf.leaf.item {
          InlineItem::Text(text_item) => {
            let slice_base_direction =
              if matches!(text_item.style.unicode_bidi, UnicodeBidi::Plaintext) {
                text_item.base_direction
              } else {
                paragraph_direction
              };
            let full_range = seg.local_start == 0 && seg.local_end == text_item.text.len();
            if full_range
              && text_item.base_direction == slice_base_direction
              && explicit_bidi_eq(text_item.explicit_bidi, para_leaf.bidi_context)
            {
              InlineItem::Text(text_item.clone())
            } else {
              let Some(sliced) = slice_text_item(
                text_item,
                seg.local_start..seg.local_end,
                shaper,
                font_context,
                slice_base_direction,
                para_leaf.bidi_context,
                &mut reshape_cache,
              ) else {
                continue;
              };
              InlineItem::Text(sliced)
            }
          }
          other => other.clone(),
        };
        visual_fragments.push(BidiLeaf {
          item,
          baseline_offset: para_leaf.leaf.baseline_offset,
          box_stack: para_leaf.leaf.box_stack.clone(),
        });
      }
    }

    let mut box_positions: FxHashMap<usize, (usize, usize)> = FxHashMap::default();
    for (vis_pos, frag) in visual_fragments.iter().enumerate() {
      check_layout_deadline(deadline_counter)?;
      for ctx in &frag.box_stack {
        check_layout_deadline(deadline_counter)?;
        box_positions
          .entry(ctx.id)
          .and_modify(|entry| entry.1 = vis_pos)
          .or_insert((vis_pos, vis_pos));
      }
    }

    fn coalesce_inline_boxes(
      items: Vec<PositionedItem>,
      deadline_counter: &mut usize,
    ) -> Result<Vec<PositionedItem>, LayoutError> {
      let mut out: Vec<PositionedItem> = Vec::with_capacity(items.len());
      for mut item in items {
        check_layout_deadline(deadline_counter)?;
        if let Some(last) = out.last_mut() {
          if let (InlineItem::InlineBox(prev), InlineItem::InlineBox(curr)) =
            (&mut last.item, &mut item.item)
          {
            // Coalesce fragments that belong to the same original inline box (e.g. after bidi
            // segmentation). `box_index` alone is not a stable identity (real inline boxes are
            // commonly constructed with `box_index = 0`), so include `box_id` when available to
            // avoid merging distinct sibling boxes.
            if prev.box_id == curr.box_id && prev.box_index == curr.box_index {
              prev.children.append(&mut curr.children);
              prev.end_edge = curr.end_edge;
              prev.margin_right = curr.margin_right;
              prev.border_right = curr.border_right;
              continue;
            }
          }
        }
        out.push(item);
      }
      Ok(out)
    }

    let mut reordered: Vec<PositionedItem> = Vec::with_capacity(visual_fragments.len());
    for (vis_pos, frag) in visual_fragments.into_iter().enumerate() {
      check_layout_deadline(deadline_counter)?;
      let mut item = frag.item;

      for ctx in frag.box_stack.iter().rev() {
        check_layout_deadline(deadline_counter)?;
        let (first, last) = box_positions
          .get(&ctx.id)
          .copied()
          .unwrap_or((vis_pos, vis_pos));
        let start_edge = if vis_pos == first {
          ctx.start_edge
        } else {
          0.0
        };
        let end_edge = if vis_pos == last { ctx.end_edge } else { 0.0 };
        let border_left = if vis_pos == first {
          ctx.border_left
        } else {
          0.0
        };
        let border_right = if vis_pos == last {
          ctx.border_right
        } else {
          0.0
        };
        let margin_left = if vis_pos == first {
          ctx.margin_left
        } else {
          0.0
        };
        let margin_right = if vis_pos == last {
          ctx.margin_right
        } else {
          0.0
        };

        let mut inline_box = InlineBoxItem::new(
          start_edge,
          end_edge,
          ctx.content_offset_y,
          ctx.metrics,
          ctx.style.clone(),
          ctx.box_index,
          ctx.direction,
          ctx.unicode_bidi,
        );
        inline_box.box_id = ctx.box_id;
        inline_box.margin_left = margin_left;
        inline_box.margin_right = margin_right;
        inline_box.border_left = border_left;
        inline_box.border_right = border_right;
        inline_box.border_top = ctx.border_top;
        inline_box.border_bottom = ctx.border_bottom;
        inline_box.bottom_inset = ctx.bottom_inset;
        inline_box.strut_metrics = ctx.strut_metrics;
        inline_box.vertical_align = ctx.vertical_align;
        inline_box.add_child(item);
        item = InlineItem::InlineBox(inline_box);
      }

      let positioned = PositionedItem {
        item,
        x: 0.0,
        baseline_offset: frag.baseline_offset,
      };
      reordered.push(positioned);
    }

    let mut reordered = coalesce_inline_boxes(reordered, deadline_counter)?;
    let mut x = 0.0;
    for positioned in &mut reordered {
      check_layout_deadline(deadline_counter)?;
      positioned.x = x;
      x += positioned.item.width();
    }

    let width: f32 = reordered.iter().map(|p| p.item.width()).sum();
    let line = &mut lines[line_idx];
    line.width = width;
    line.items = reordered;
  }

  Ok(())
}

fn slice_text_item(
  item: &TextItem,
  range: std::ops::Range<usize>,
  pipeline: &ShapingPipeline,
  font_context: &FontContext,
  base_direction: Direction,
  bidi_context: Option<crate::text::pipeline::ExplicitBidiContext>,
  reshape_cache: &mut ReshapeCache,
) -> Option<TextItem> {
  if range.start >= range.end || range.end > item.text.len() {
    return None;
  }

  // Fast path for synthetic items used in tests that don't carry shaped runs.
  if item.runs.is_empty() {
    let advance_at = |byte_offset: usize| -> f32 {
      item
        .cluster_advances
        .iter()
        .rev()
        .find(|b| b.byte_offset <= byte_offset)
        .map(|b| b.advance)
        .unwrap_or(0.0)
    };
    let start_adv = advance_at(range.start);
    let end_adv = advance_at(range.end);
    let width = (end_adv - start_adv).max(0.0);

    let cluster_advances = item
      .cluster_advances
      .iter()
      .filter(|b| b.byte_offset >= range.start && b.byte_offset <= range.end)
      .map(|b| ClusterBoundary {
        byte_offset: b.byte_offset - range.start,
        advance: (b.advance - start_adv).max(0.0),
        run_index: b.run_index,
        glyph_end: b.glyph_end,
        run_advance: (b.run_advance - start_adv).max(0.0),
      })
      .collect();

    let breaks: Vec<BreakOpportunity> = item
      .break_opportunities
      .iter()
      .filter(|b| b.byte_offset >= range.start && b.byte_offset <= range.end)
      .map(|b| {
        BreakOpportunity::with_hyphen_and_kind(
          b.byte_offset - range.start,
          b.break_type,
          b.adds_hyphen,
          b.kind,
        )
      })
      .collect();
    let first_mandatory_break = TextItem::first_mandatory_break(&breaks);
    let forced = item
      .forced_break_offsets
      .iter()
      .copied()
      .filter(|o| *o >= range.start && *o <= range.end)
      .map(|o| o - range.start)
      .collect();

    return Some(TextItem {
      box_id: item.box_id,
      runs: Vec::new(),
      advance: width,
      advance_for_layout: if item.is_marker {
        item.advance_for_layout.min(width)
      } else {
        width
      },
      metrics: item.metrics,
      vertical_align: item.vertical_align,
      break_opportunities: breaks,
      forced_break_offsets: forced,
      first_mandatory_break,
      text: item.text[range.clone()].to_string(),
      font_size: item.font_size,
      style: item.style.clone(),
      base_direction,
      explicit_bidi: bidi_context,
      is_marker: item.is_marker,
      paint_offset: item.paint_offset,
      emphasis_offset: item.emphasis_offset,
      cluster_advances,
      source_range: item.source_range.start + range.start..item.source_range.start + range.end,
      source_id: item.source_id,
    });
  }

  // When slicing within the same shaping context, prefer splitting the existing shaped runs over
  // shaping the substring again.
  let context_matches =
    item.base_direction == base_direction && explicit_bidi_eq(item.explicit_bidi, bidi_context);
  if context_matches {
    if range.start == 0 && range.end == item.text.len() {
      return Some(item.clone());
    }
    if range.start == 0 {
      return item
        .split_at(range.end, false, pipeline, font_context, reshape_cache)
        .map(|(before, _)| before);
    }
    if range.end == item.text.len() {
      return item
        .split_at(range.start, false, pipeline, font_context, reshape_cache)
        .map(|(_, after)| after);
    }
    let slice_len = range.end.checked_sub(range.start)?;
    if let Some((_, after)) =
      item.split_at(range.start, false, pipeline, font_context, reshape_cache)
    {
      if let Some((before, _)) =
        after.split_at(slice_len, false, pipeline, font_context, reshape_cache)
      {
        return Some(before);
      }
    }
  }

  let slice_text = &item.text[range.clone()];
  let mut runs = pipeline
    .shape_with_context(
      slice_text,
      &item.style,
      font_context,
      pipeline_dir_from_style(base_direction),
      bidi_context,
    )
    .ok()?;
  TextItem::apply_spacing_to_runs(
    &mut runs,
    slice_text,
    item.style.letter_spacing,
    item.style.word_spacing,
  );

  let metrics = if matches!(
    item.style.line_height,
    crate::style::types::LineHeight::Normal
  ) {
    let mut metrics = TextItem::metrics_from_runs(
      font_context,
      &runs,
      item.metrics.line_height,
      item.font_size,
    );
    TextItem::apply_text_emphasis_metrics(&mut metrics, &item.style);
    metrics
  } else {
    item.metrics
  };
  let breaks = item
    .break_opportunities
    .iter()
    .filter(|b| b.byte_offset >= range.start && b.byte_offset <= range.end)
    .map(|b| {
      BreakOpportunity::with_hyphen_and_kind(
        b.byte_offset - range.start,
        b.break_type,
        b.adds_hyphen,
        b.kind,
      )
    })
    .collect();
  let forced = item
    .forced_break_offsets
    .iter()
    .copied()
    .filter(|o| *o >= range.start && *o <= range.end)
    .map(|o| o - range.start)
    .collect();

  let mut new_item = TextItem::new(
    runs,
    slice_text.to_string(),
    metrics,
    breaks,
    forced,
    item.style.clone(),
    base_direction,
  )
  .with_vertical_align(item.vertical_align);
  new_item.box_id = item.box_id;
  new_item.explicit_bidi = bidi_context;
  new_item.source_id = item.source_id;
  new_item.source_range =
    item.source_range.start + range.start..item.source_range.start + range.end;
  if item.is_marker {
    new_item.is_marker = true;
    new_item.paint_offset = item.paint_offset;
    new_item.advance_for_layout = item.advance_for_layout.min(new_item.advance);
  }

  Some(new_item)
}

#[derive(Clone)]
struct BoxContext {
  id: usize,
  box_id: usize,
  start_edge: f32,
  end_edge: f32,
  margin_left: f32,
  margin_right: f32,
  content_offset_y: f32,
  border_left: f32,
  border_right: f32,
  border_top: f32,
  border_bottom: f32,
  bottom_inset: f32,
  metrics: BaselineMetrics,
  strut_metrics: BaselineMetrics,
  vertical_align: VerticalAlign,
  box_index: usize,
  direction: Direction,
  unicode_bidi: UnicodeBidi,
  style: Arc<ComputedStyle>,
}

#[derive(Clone)]
struct BidiLeaf {
  item: InlineItem,
  baseline_offset: f32,
  box_stack: Vec<BoxContext>,
}

#[derive(Clone)]
struct ParagraphLeaf {
  leaf: BidiLeaf,
  bidi_context: Option<crate::text::pipeline::ExplicitBidiContext>,
}

fn flatten_positioned_item(
  positioned: &PositionedItem,
  box_stack: &mut Vec<BoxContext>,
  box_counter: &mut usize,
  leaves: &mut Vec<BidiLeaf>,
) {
  match &positioned.item {
    InlineItem::InlineBox(inline_box) => {
      let id = *box_counter;
      *box_counter += 1;
      let ctx = BoxContext {
        id,
        box_id: inline_box.box_id,
        start_edge: inline_box.start_edge,
        end_edge: inline_box.end_edge,
        margin_left: inline_box.margin_left,
        margin_right: inline_box.margin_right,
        content_offset_y: inline_box.content_offset_y,
        border_left: inline_box.border_left,
        border_right: inline_box.border_right,
        border_top: inline_box.border_top,
        border_bottom: inline_box.border_bottom,
        bottom_inset: inline_box.bottom_inset,
        metrics: inline_box.metrics,
        strut_metrics: inline_box.strut_metrics,
        vertical_align: inline_box.vertical_align,
        box_index: inline_box.box_index,
        direction: inline_box.direction,
        unicode_bidi: inline_box.unicode_bidi,
        style: inline_box.style.clone(),
      };
      box_stack.push(ctx);
      for child in &inline_box.children {
        let child_positioned = PositionedItem {
          item: child.clone(),
          x: positioned.x,
          baseline_offset: positioned.baseline_offset,
        };
        flatten_positioned_item(&child_positioned, box_stack, box_counter, leaves);
      }
      box_stack.pop();
    }
    _ => {
      leaves.push(BidiLeaf {
        item: positioned.item.clone(),
        baseline_offset: positioned.baseline_offset,
        box_stack: box_stack.clone(),
      });
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::layout::contexts::inline::explicit_bidi_context;
  use crate::render_control::{with_deadline, RenderDeadline};
  use crate::style::types::FontKerning;
  use crate::style::types::TextOrientation;
  use crate::style::types::WritingMode;
  use crate::style::ComputedStyle;
  use crate::text::font_db::FontDatabase;
  use crate::text::font_db::FontFaceMetricsOverrides;
  use crate::text::font_db::FontStretch;
  use crate::text::font_db::FontStyle;
  use crate::text::font_db::FontWeight;
  use crate::text::font_db::LoadedFont;
  use crate::text::font_loader::FontContext;
  use crate::text::line_break::find_break_opportunities;
  use crate::text::pipeline::Direction as PipelineDirection;
  use crate::text::pipeline::GlyphPosition;
  use crate::text::pipeline::RunRotation;
  use crate::text::pipeline::ShapingPipeline;
  use crate::tree::box_tree::CrossOriginAttribute;
  use crate::tree::box_tree::ImageDecodingAttribute;
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;
  use unicode_bidi::level;
  use unicode_bidi::BidiInfo;
  use unicode_bidi::Level;

  fn make_strut_metrics() -> BaselineMetrics {
    BaselineMetrics::new(12.0, 16.0, 12.0, 4.0)
  }

  #[test]
  fn tight_line_height_uses_negative_leading_without_expanding_line_box() {
    let font_context = FontContext::new();
    let fallback_font_size = 10.0;
    let ascent = fallback_font_size * 0.8;
    let descent = fallback_font_size * 0.2;
    let line_height = 6.0;

    let metrics = TextItem::metrics_from_runs(&font_context, &[], line_height, fallback_font_size);
    let half_leading = (line_height - (ascent + descent)) / 2.0;
    assert!(
      (metrics.baseline_offset - (ascent + half_leading)).abs() < 1e-3,
      "baseline_offset should include half-leading for tight line-height"
    );

    let strut = BaselineMetrics::new(ascent + half_leading, line_height, ascent, descent);
    let mut acc = LineBaselineAccumulator::new(&strut);
    acc.add_baseline_relative(&metrics, VerticalAlign::Baseline, Some(&strut));
    assert!(
      (acc.line_height() - line_height).abs() < 1e-3,
      "line box should respect authored line-height even when font metrics are taller"
    );
  }

  #[test]
  fn inline_block_baseline_ignores_nested_line_fragments() {
    let nested_line = FragmentNode::new_line(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), 5.0, vec![]);
    let nested_inline_block =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 2.0, 0.0, 0.0), 1, vec![nested_line]);
    let outer_line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      10.0,
      vec![nested_inline_block],
    );
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 0.0, 0.0), vec![outer_line]);

    let mut first = None;
    let mut last = None;
    collect_line_baselines(&root, 0.0, &mut first, &mut last);

    assert_eq!(first, Some(10.0));
    assert_eq!(last, Some(10.0));
  }

  fn make_builder(width: f32) -> LineBuilder<'static> {
    let strut = make_strut_metrics();
    LineBuilder::new(
      width,
      width,
      true,
      TextWrap::Auto,
      0.0,
      false,
      false,
      strut,
      ShapingPipeline::new(),
      FontContext::new(),
      Some(Level::ltr()),
      UnicodeBidi::Normal,
      Direction::Ltr,
      None,
      0.0,
      0.0,
      None,
    )
  }

  fn make_builder_with_indent(width: f32, indent: f32) -> LineBuilder<'static> {
    let strut = make_strut_metrics();
    LineBuilder::new(
      width,
      width,
      true,
      TextWrap::Auto,
      indent,
      false,
      false,
      strut,
      ShapingPipeline::new(),
      FontContext::new(),
      Some(Level::ltr()),
      UnicodeBidi::Normal,
      Direction::Ltr,
      None,
      0.0,
      0.0,
      None,
    )
  }

  #[test]
  fn reshape_cache_canonicalizes_negative_zero_spacing() {
    let font_context = FontContext::new();
    let shaper = ShapingPipeline::new();
    let text = "Hello";

    let mut style = ComputedStyle::default();
    style.letter_spacing = 0.0;
    style.word_spacing = 0.0;
    let mut style_neg = style.clone();
    style_neg.letter_spacing = -0.0;
    style_neg.word_spacing = -0.0;

    let runs = shaper
      .shape(text, &style, &font_context)
      .expect("shape succeeds");
    let metrics =
      TextItem::metrics_from_runs(&font_context, &runs, style.font_size, style.font_size);
    let breaks = find_break_opportunities(text);

    let item = TextItem::new(
      runs.clone(),
      text.to_string(),
      metrics,
      breaks.clone(),
      Vec::new(),
      Arc::new(style),
      Direction::Ltr,
    );
    let item_neg = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      breaks,
      Vec::new(),
      Arc::new(style_neg),
      Direction::Ltr,
    );

    let mut reshape_cache = ReshapeCache::default();
    reshape_cache
      .shape(&item, 0..text.len(), &shaper, &font_context)
      .expect("expected reshape cache miss to shape");
    assert_eq!(reshape_cache.runs.len(), 1);
    reshape_cache
      .shape(&item_neg, 0..text.len(), &shaper, &font_context)
      .expect("expected reshape cache to reuse entry");
    assert_eq!(reshape_cache.runs.len(), 1);
  }

  #[test]
  fn reshape_cache_uses_prehashed_pipeline_entrypoint() {
    let font_context = FontContext::new();
    let shaper = ShapingPipeline::new();
    let text = "Hello";

    let style = Arc::new(ComputedStyle::default());
    let runs = shaper
      .shape(text, &style, &font_context)
      .expect("shape succeeds");
    let metrics =
      TextItem::metrics_from_runs(&font_context, &runs, style.font_size, style.font_size);
    let breaks = find_break_opportunities(text);

    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      breaks,
      Vec::new(),
      Arc::clone(&style),
      Direction::Ltr,
    );

    let mut reshape_cache = ReshapeCache::default();
    crate::text::pipeline::debug_reset_style_hash_calls();
    reshape_cache
      .shape(&item, 0..text.len(), &shaper, &font_context)
      .expect("expected reshape cache miss to shape");
    assert_eq!(
      crate::text::pipeline::debug_style_hash_calls(),
      1,
      "expected reshape cache to compute shaping_style_hash once (for its own key), not once again inside the shaping pipeline"
    );
  }

  #[test]
  fn hyphen_advance_cache_canonicalizes_negative_zero_spacing() {
    let mut builder = make_builder(200.0);
    let mut style = ComputedStyle::default();
    style.letter_spacing = 0.0;
    style.word_spacing = 0.0;
    let mut style_neg = style.clone();
    style_neg.letter_spacing = -0.0;
    style_neg.word_spacing = -0.0;

    let expected = builder.hyphen_advance(&style, Direction::Ltr, None);
    assert_eq!(builder.hyphen_advance_cache.len(), 1);
    let got = builder.hyphen_advance(&style_neg, Direction::Ltr, None);
    assert_eq!(builder.hyphen_advance_cache.len(), 1);
    assert_eq!(expected, got);
  }

  #[test]
  fn line_builder_times_out_in_hot_loop() {
    let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
    with_deadline(Some(&deadline), || {
      let mut builder = make_builder(1e9);
      let anchor = InlineItem::StaticPositionAnchor(StaticPositionAnchor::new(
        1,
        Direction::Ltr,
        UnicodeBidi::Normal,
      ));
      for _ in 0..(LINE_BUILDER_DEADLINE_STRIDE - 1) {
        builder.add_item(anchor.clone()).unwrap();
      }

      let err = builder.add_item(anchor).unwrap_err();
      assert!(matches!(err, LayoutError::Timeout { .. }));
    });
  }

  #[test]
  fn bookkeeping_anchors_do_not_create_empty_wrapped_lines() {
    // Regresses: a collapsible trailing space that doesn't fit can be wrapped onto its own line,
    // then trimmed away, leaving only bookkeeping static-position anchors. Such anchor-only lines
    // must not contribute line box height (they otherwise inflate the element's height).
    let mut builder = make_builder(50.0);
    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" ", 5.0)))
      .unwrap();
    builder
      .add_item(InlineItem::StaticPositionAnchor(StaticPositionAnchor::new(
        1,
        Direction::Ltr,
        UnicodeBidi::Normal,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    // Any line that contains only bookkeeping items (static-position anchors / floats) must not
    // advance the block cursor.
    for line in &lines {
      let bookkeeping_only = !line.items.is_empty()
        && line.items.iter().all(|p| {
          matches!(
            p.item,
            InlineItem::StaticPositionAnchor(_) | InlineItem::Floating(_)
          )
        });
      if bookkeeping_only {
        assert_eq!(
          line.height, 0.0,
          "unexpected bookkeeping-only line: {line:#?}"
        );
      }
    }
    assert_eq!(
      lines.iter().filter(|line| line.height > 0.0).count(),
      1,
      "expected exactly one non-zero-height line: {lines:#?}"
    );
    assert_eq!(
      lines
        .iter()
        .flat_map(|line| line.items.iter())
        .filter(|p| matches!(p.item, InlineItem::StaticPositionAnchor(_)))
        .count(),
      1
    );
    let flattened: String = lines
      .iter()
      .flat_map(|line| line.items.iter())
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(flattened, format!("Hello\u{FFFC}"));
  }

  fn pipeline_dir_from_style(dir: Direction) -> crate::text::pipeline::Direction {
    match dir {
      Direction::Ltr => crate::text::pipeline::Direction::LeftToRight,
      Direction::Rtl => crate::text::pipeline::Direction::RightToLeft,
    }
  }

  fn make_synthetic_font() -> Arc<LoadedFont> {
    Arc::new(LoadedFont {
      id: None,
      data: Arc::new(Vec::new()),
      index: 0,
      face_metrics_overrides: FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "Test".to_string(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    })
  }

  fn make_synthetic_run(text: &str, advance_per_glyph: f32) -> ShapedRun {
    let glyph_count = text.chars().count();
    let mut glyphs = Vec::with_capacity(glyph_count);
    for idx in 0..glyph_count {
      glyphs.push(GlyphPosition {
        glyph_id: idx as u32,
        cluster: idx as u32,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: advance_per_glyph,
        y_advance: 0.0,
      });
    }
    let advance = advance_per_glyph * glyph_count as f32;

    ShapedRun {
      text: text.to_string(),
      start: 0,
      end: text.len(),
      glyphs,
      direction: PipelineDirection::LeftToRight,
      level: 0,
      advance,
      font: make_synthetic_font(),
      font_size: 16.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    }
  }

  fn make_synthetic_run_with_byte_clusters(
    text: &str,
    start: usize,
    advance_per_glyph: f32,
    direction: PipelineDirection,
  ) -> ShapedRun {
    let glyph_count = text.chars().count();
    let mut glyphs = Vec::with_capacity(glyph_count);
    for (glyph_id, (cluster, _)) in text.char_indices().enumerate() {
      glyphs.push(GlyphPosition {
        glyph_id: glyph_id as u32,
        cluster: cluster as u32,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: advance_per_glyph,
        y_advance: 0.0,
      });
    }
    let advance = advance_per_glyph * glyph_count as f32;

    ShapedRun {
      text: text.to_string(),
      start,
      end: start + text.len(),
      glyphs,
      direction,
      level: 0,
      advance,
      font: make_synthetic_font(),
      font_size: 16.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    }
  }

  fn glyph_x_positions(runs: &[ShapedRun]) -> Vec<f32> {
    let mut out = Vec::new();
    let mut run_origin = 0.0;
    for run in runs {
      let mut pen_x = run_origin;
      for glyph in &run.glyphs {
        out.push(pen_x + glyph.x_offset);
        pen_x += glyph.x_advance;
      }
      run_origin += run.advance;
    }
    out
  }

  fn make_builder_with_base(width: f32, base: Level) -> LineBuilder<'static> {
    let strut = make_strut_metrics();
    LineBuilder::new(
      width,
      width,
      true,
      TextWrap::Auto,
      0.0,
      false,
      false,
      strut,
      ShapingPipeline::new(),
      FontContext::new(),
      Some(base),
      UnicodeBidi::Normal,
      Direction::Ltr,
      None,
      0.0,
      0.0,
      None,
    )
  }

  fn make_text_item(text: &str, advance: f32) -> TextItem {
    make_text_item_with_bidi(text, advance, UnicodeBidi::Normal)
  }

  #[test]
  fn line_fitting_allows_small_subpixel_overflow_to_avoid_unexpected_wraps() {
    // Regresses: `LineBuilder` compared shaped run widths against the available line width without
    // any tolerance. We often snap container sizes to whole pixels while text advances remain
    // subpixel. The accumulated rounding loss can cause near-boundary text like "My Visit"
    // (si.edu header) to wrap unexpectedly.
    let mut builder = make_builder(100.0);
    builder
      .add_item(InlineItem::Text(make_text_item("My Visit", 100.9)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let line_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(line_text, "My Visit");
  }

  #[test]
  fn collapsible_spaces_do_not_start_soft_wrapped_lines() {
    // CSS whitespace collapsing suppresses leading spaces on each line. This matters when inline
    // content ends exactly at the line width and the following collapsed space item is moved to
    // the start of the next line.
    let mut builder = make_builder(20.0);
    builder
      .add_item(InlineItem::Text(make_text_item("when", 20.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" ", 4.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("writing", 20.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);
    let line0_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let line1_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(line0_text, "when");
    assert_eq!(line1_text, "writing");
  }

  #[test]
  fn find_break_point_allows_small_subpixel_overflow_to_avoid_extra_wrap() {
    use crate::text::line_break::BreakOpportunityKind;
    use crate::text::line_break::BreakType;

    // Regresses: `TextItem::find_break_point` could round down the fitting offset and select an
    // earlier break opportunity when the available width was just shy of the true advance by
    // <1px. This can force an extra wrapped line for large headings (e.g. BBC hero headline).
    let item = make_text_item("aaa bbb ccc", 1000.0);

    // Ensure the expected normal break opportunities exist.
    let normal_breaks: Vec<usize> = item
      .break_opportunities
      .iter()
      .filter(|b| {
        matches!(b.break_type, BreakType::Allowed) && b.kind == BreakOpportunityKind::Normal
      })
      .map(|b| b.byte_offset)
      .collect();
    assert!(
      normal_breaks.contains(&4) && normal_breaks.contains(&8),
      "expected breaks at offsets 4 and 8, got {normal_breaks:?}"
    );

    let target_offset = 8;
    let target_advance = item.advance_at_offset(target_offset);
    let width = target_advance - 0.25;

    let brk = item
      .find_break_point(width)
      .expect("expected a fitting break point");
    assert_eq!(brk.byte_offset, target_offset);
  }

  #[test]
  fn find_break_point_ignores_trimmed_trailing_spaces_for_collapsible_whitespace() {
    // When `white-space` collapses spaces, the trailing space at the chosen wrap opportunity is
    // removed from the line box. Selecting the wrap point must therefore be based on the width
    // excluding that trimmed whitespace, otherwise line breaking can prematurely fall back to an
    // earlier break and produce an extra wrapped line.
    let item = make_text_item("hello world", 11.0);

    // Fits "hello" (offset 5), but not "hello " (offset 6).
    let width_without_space = item.advance_at_offset(5);
    let width_with_space = item.advance_at_offset(6);
    assert!(width_without_space < width_with_space);

    let max_width = width_without_space + 0.1;
    assert!(max_width < width_with_space);

    let brk = item
      .find_break_point(max_width)
      .expect("expected break at the space");
    assert_eq!(brk.byte_offset, 6);

    // For `white-space: pre-wrap`, trailing preserved spaces are hanging and are not considered
    // when measuring fit at a wrap opportunity.
    let mut prewrap = item.clone();
    Arc::make_mut(&mut prewrap.style).white_space = WhiteSpace::PreWrap;
    let brk = prewrap
      .find_break_point(max_width)
      .expect("pre-wrap should ignore trailing space width for fitting");
    assert_eq!(brk.byte_offset, 6);

    // `break-spaces` keeps spaces in-flow, so their width must be considered.
    let mut break_spaces = item.clone();
    Arc::make_mut(&mut break_spaces.style).white_space = WhiteSpace::BreakSpaces;
    assert!(
      break_spaces.find_break_point(max_width).is_none(),
      "break-spaces should not ignore trailing space width for fitting"
    );
  }

  #[test]
  fn pre_wrap_hanging_spaces_are_ignored_for_mandatory_break_fitting() {
    let mut item = make_text_item("hi ", 3.0);
    item.break_opportunities = vec![BreakOpportunity::mandatory(3)];
    item.first_mandatory_break = TextItem::first_mandatory_break(&item.break_opportunities);

    let width_without_space = item.advance_at_offset(2);
    let width_with_space = item.advance_at_offset(3);
    let max_width = width_without_space + 0.1;
    assert!(
      max_width < width_with_space,
      "test setup: max_width should not fit the trailing space"
    );

    let mut prewrap = item.clone();
    Arc::make_mut(&mut prewrap.style).white_space = WhiteSpace::PreWrap;
    let brk = prewrap
      .find_break_point(max_width)
      .expect("expected mandatory break to fit after ignoring hanging space width");
    assert_eq!(brk.break_type, BreakType::Mandatory);
    assert_eq!(brk.byte_offset, 3);

    let mut break_spaces = item.clone();
    Arc::make_mut(&mut break_spaces.style).white_space = WhiteSpace::BreakSpaces;
    assert!(
      break_spaces.find_break_point(max_width).is_none(),
      "break-spaces should not ignore trailing space width for fitting"
    );
  }

  #[test]
  fn add_breaks_at_clusters_preserves_existing_hyphen_breaks() {
    use crate::text::line_break::{BreakOpportunity, BreakType};

    let mut item = make_text_item("abc", 30.0);
    item.break_opportunities = vec![
      BreakOpportunity::with_hyphen(2, BreakType::Allowed, true),
      BreakOpportunity::allowed(3),
    ];
    item.first_mandatory_break = TextItem::first_mandatory_break(&item.break_opportunities);

    item.add_breaks_at_clusters();

    let offsets: Vec<usize> = item
      .break_opportunities
      .iter()
      .map(|b| b.byte_offset)
      .collect();
    assert_eq!(offsets, vec![1, 2, 3]);

    let brk2 = item
      .break_opportunities
      .iter()
      .find(|b| b.byte_offset == 2)
      .expect("break at offset 2");
    assert!(
      brk2.adds_hyphen,
      "cluster breaks should not clear hyphen flags"
    );
  }

  #[test]
  fn tab_width_accounts_for_text_indent_at_line_start() {
    let mut builder = make_builder_with_indent(200.0, 3.0);
    builder
      .add_item(InlineItem::Tab(TabItem::new(
        Arc::new(ComputedStyle::default()),
        make_strut_metrics(),
        8.0,
        true,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].indent, 3.0);
    assert_eq!(lines[0].items.len(), 1);

    let tab_width = match &lines[0].items[0].item {
      InlineItem::Tab(tab) => tab.width(),
      other => panic!("expected TabItem, got {other:?}"),
    };
    assert!((tab_width - 5.0).abs() < 0.001, "tab width was {tab_width}");
  }

  #[test]
  fn tab_width_accounts_for_text_indent_after_preceding_text() {
    let mut builder = make_builder_with_indent(200.0, 3.0);
    builder
      .add_item(InlineItem::Text(make_text_item("a", 4.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Tab(TabItem::new(
        Arc::new(ComputedStyle::default()),
        make_strut_metrics(),
        8.0,
        true,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].indent, 3.0);
    assert_eq!(lines[0].items.len(), 2);

    let tab_width = match &lines[0].items[1].item {
      InlineItem::Tab(tab) => tab.width(),
      other => panic!("expected TabItem, got {other:?}"),
    };
    assert!((tab_width - 1.0).abs() < 0.001, "tab width was {tab_width}");
  }

  fn make_text_item_with_bidi(text: &str, advance: f32, ub: UnicodeBidi) -> TextItem {
    let mut style = ComputedStyle::default();
    style.unicode_bidi = ub;
    let style = Arc::new(style);
    let mut cluster_advances = Vec::new();
    if !text.is_empty() {
      let step = advance / text.len() as f32;
      for i in 1..=text.len() {
        cluster_advances.push(ClusterBoundary {
          byte_offset: i,
          advance: step * i as f32,
          run_index: None,
          glyph_end: None,
          run_advance: step * i as f32,
        });
      }
    }
    let breaks = find_break_opportunities(text);
    TextItem {
      box_id: 0,
      runs: Vec::new(),
      advance,
      advance_for_layout: advance,
      metrics: make_strut_metrics(),
      vertical_align: VerticalAlign::Baseline,
      break_opportunities: breaks.clone(),
      forced_break_offsets: Vec::new(),
      first_mandatory_break: TextItem::first_mandatory_break(&breaks),
      text: text.to_string(),
      font_size: 16.0,
      style: style.clone(),
      base_direction: crate::style::types::Direction::Ltr,
      explicit_bidi: None,
      is_marker: false,
      paint_offset: 0.0,
      emphasis_offset: TextEmphasisOffset::default(),
      cluster_advances,
      source_range: 0..text.len(),
      source_id: TextItem::hash_text(text),
    }
  }

  #[test]
  fn hard_breaks_nested_in_inline_boxes_are_hoisted() {
    let mut builder = make_builder(200.0);
    let mut inline_box = InlineBoxItem::new(
      2.0,
      3.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );

    inline_box.add_child(InlineItem::Text(make_text_item("foo", 30.0)));
    inline_box.add_child(InlineItem::HardBreak(ClearSide::None));
    inline_box.add_child(InlineItem::Text(make_text_item("bar", 30.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with_hard_break);
    assert!(!lines[1].ends_with_hard_break);
    assert_eq!(lines[0].items.len(), 1);
    assert_eq!(lines[1].items.len(), 1);

    let InlineItem::InlineBox(b0) = &lines[0].items[0].item else {
      panic!("expected inline box fragment on line 0");
    };
    let InlineItem::InlineBox(b1) = &lines[1].items[0].item else {
      panic!("expected inline box fragment on line 1");
    };
    assert!((b0.start_edge - 2.0).abs() < f32::EPSILON);
    assert_eq!(b0.end_edge, 0.0);
    assert_eq!(b1.start_edge, 0.0);
    assert!((b1.end_edge - 3.0).abs() < f32::EPSILON);

    let line0_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let line1_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(line0_text, "foo");
    assert_eq!(line1_text, "bar");
  }

  #[test]
  fn nested_inline_boxes_can_split_after_prior_siblings() {
    let mut builder = make_builder(200.0);

    // Outer inline box with a short prefix and a nested inline box containing long, wrappable text.
    // The nested inline box should be able to fragment to fill the remaining width on the current
    // line (instead of always starting on a new line).
    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    outer.add_child(InlineItem::Text(make_text_item("Related:", 60.0)));

    let mut inner = InlineBoxItem::new(
      3.0, // mimic padding-left, like the ABC News "Related:" span
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    inner.add_child(InlineItem::Text(make_text_item(
      "Sheriff releases dash cam video that may show missing 19-year-old girl",
      190.0,
    )));
    outer.add_child(InlineItem::InlineBox(inner));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert!(
      lines.len() >= 2,
      "expected content to wrap to multiple lines"
    );

    let line0_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert!(line0_text.starts_with("Related:"));
    assert_ne!(line0_text, "Related:");
    assert!(
      line0_text.contains("Sheriff"),
      "expected nested inline box text to share first line"
    );

    let line1_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert!(
      !line1_text.starts_with("Related:"),
      "prefix should not repeat in continuation fragments"
    );
  }

  fn old_find_break_point(item: &TextItem, max_width: f32) -> Option<BreakOpportunity> {
    use crate::text::line_break::BreakOpportunityKind;
    let mut mandatory_break: Option<BreakOpportunity> = None;
    let mut normal_allowed_break: Option<BreakOpportunity> = None;
    let mut emergency_allowed_break: Option<BreakOpportunity> = None;

    for brk in &item.break_opportunities {
      let width_at_break = item.effective_advance_at_break(brk);
      if width_at_break <= max_width + LINE_PIXEL_FIT_EPSILON {
        match brk.break_type {
          BreakType::Mandatory => {
            if mandatory_break.is_none() {
              mandatory_break = Some(*brk);
            }
          }
          BreakType::Allowed => match brk.kind {
            BreakOpportunityKind::Normal => normal_allowed_break = Some(*brk),
            BreakOpportunityKind::Emergency => emergency_allowed_break = Some(*brk),
          },
        }
      }
    }

    let mut best_break = mandatory_break
      .or(normal_allowed_break)
      .or(emergency_allowed_break);
    if best_break.is_some() || !allows_soft_wrap(item.style.as_ref()) {
      return best_break;
    }

    let can_overflow_break_word = matches!(
      item.style.word_break,
      WordBreak::BreakWord | WordBreak::Anywhere
    );
    let can_overflow_break_by_wrap = matches!(
      item.style.overflow_wrap,
      OverflowWrap::BreakWord | OverflowWrap::Anywhere
    );

    if can_overflow_break_word || can_overflow_break_by_wrap {
      for (idx, _) in item.text.char_indices().skip(1) {
        let width_at_break = item.advance_at_offset(idx);
        if width_at_break <= max_width + LINE_PIXEL_FIT_EPSILON {
          best_break = Some(BreakOpportunity::emergency(idx));
        } else {
          break;
        }
      }
    }

    best_break
  }

  fn glyph_signature_sequence(item: &TextItem) -> Vec<(usize, u8, bool)> {
    item
      .runs
      .iter()
      .flat_map(|run| {
        let rtl = run.direction.is_rtl();
        run.glyphs.iter().map(move |glyph| {
          (
            item.source_range.start + run.start + glyph.cluster as usize,
            run.level,
            rtl,
          )
        })
      })
      .collect()
  }

  fn force_split_fallback(item: &mut TextItem) {
    for boundary in &mut item.cluster_advances {
      boundary.run_index = None;
      boundary.glyph_end = None;
    }
  }

  fn make_shaped_text_item(
    text: &str,
    style: Arc<ComputedStyle>,
    base_direction: Direction,
    explicit_bidi: Option<crate::text::pipeline::ExplicitBidiContext>,
    pipeline: &ShapingPipeline,
    font_context: &FontContext,
  ) -> TextItem {
    let mut runs = pipeline
      .shape_with_context(
        text,
        &style,
        font_context,
        pipeline_dir_from_style(base_direction),
        explicit_bidi,
      )
      .expect("text shaping should succeed in tests");
    TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
    let metrics = TextItem::metrics_from_runs(font_context, &runs, 16.0, style.font_size);
    let breaks = find_break_opportunities(text);
    let mut item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      breaks,
      Vec::new(),
      style,
      base_direction,
    );
    item.explicit_bidi = explicit_bidi;
    item
  }

  fn assert_split_matches_original(
    item: &TextItem,
    split_offset: usize,
    before: &TextItem,
    after: &TextItem,
  ) {
    const EPS: f32 = 0.05;
    assert!(
      (before.advance + after.advance - item.advance).abs() < EPS,
      "split advances should add up: {} + {} vs {}",
      before.advance,
      after.advance,
      item.advance
    );

    let signatures = glyph_signature_sequence(item);
    let expected_before: Vec<(usize, u8, bool)> = signatures
      .iter()
      .copied()
      .filter(|(cluster, _, _)| *cluster < split_offset)
      .collect();
    let expected_after: Vec<(usize, u8, bool)> = signatures
      .iter()
      .copied()
      .filter(|(cluster, _, _)| *cluster >= split_offset)
      .collect();

    assert_eq!(glyph_signature_sequence(before), expected_before);
    assert_eq!(glyph_signature_sequence(after), expected_after);
  }

  #[test]
  fn reshape_cache_preserves_base_direction_and_explicit_bidi() {
    let pipeline = ShapingPipeline::new();
    let font_context = FontContext::new();
    let mut style = ComputedStyle::default();
    style.direction = Direction::Ltr;
    style.font_kerning = FontKerning::None;
    let style = Arc::new(style);
    let mut reshape_cache = ReshapeCache::default();

    // Explicit override case: should reorder Latin text in RTL override.
    let text = "abcd";
    let explicit = explicit_bidi_context(
      Direction::Rtl,
      &[(UnicodeBidi::BidiOverride, Direction::Rtl)],
    )
    .expect("expected explicit bidi context");
    let mut overridden = make_shaped_text_item(
      text,
      style.clone(),
      Direction::Rtl,
      Some(explicit),
      &pipeline,
      &font_context,
    );
    overridden.box_id = 1;
    assert!(
      overridden.runs.iter().all(|run| run.direction.is_rtl()),
      "explicit bidi override should force RTL runs"
    );

    force_split_fallback(&mut overridden);
    let (before, after) = overridden
      .split_at(2, false, &pipeline, &font_context, &mut reshape_cache)
      .expect("split should succeed");
    let ctx_key = |item: &TextItem| {
      item
        .explicit_bidi
        .map(|ctx| (ctx.level.number(), ctx.override_all))
    };
    assert_eq!(ctx_key(&before), ctx_key(&overridden));
    assert_eq!(ctx_key(&after), ctx_key(&overridden));
    assert_split_matches_original(&overridden, 2, &before, &after);

    // Same text/style/base-direction but without explicit override should not reuse the cached override shaping.
    pipeline.clear_cache();
    let mut plain = make_shaped_text_item(
      text,
      style.clone(),
      Direction::Rtl,
      None,
      &pipeline,
      &font_context,
    );
    force_split_fallback(&mut plain);
    let (before_plain, after_plain) = plain
      .split_at(2, false, &pipeline, &font_context, &mut reshape_cache)
      .expect("split should succeed");
    assert_split_matches_original(&plain, 2, &before_plain, &after_plain);

    // Base-direction case: mixed LTR/RTL content should shape differently when the paragraph base differs.
    pipeline.clear_cache();
    let mixed = "ABC אבג";
    let split_offset = mixed
      .find('ב')
      .expect("expected hebrew letter in test string");
    let mut rtl_base = make_shaped_text_item(
      mixed,
      style.clone(),
      Direction::Rtl,
      None,
      &pipeline,
      &font_context,
    );
    pipeline.clear_cache();
    let mut ltr_base = make_shaped_text_item(
      mixed,
      style.clone(),
      Direction::Ltr,
      None,
      &pipeline,
      &font_context,
    );
    assert_ne!(
      glyph_signature_sequence(&rtl_base),
      glyph_signature_sequence(&ltr_base),
      "base direction should affect bidi levels/directions"
    );

    force_split_fallback(&mut rtl_base);
    let (before_rtl, after_rtl) = rtl_base
      .split_at(
        split_offset,
        false,
        &pipeline,
        &font_context,
        &mut reshape_cache,
      )
      .expect("split should succeed");
    assert_split_matches_original(&rtl_base, split_offset, &before_rtl, &after_rtl);

    pipeline.clear_cache();
    force_split_fallback(&mut ltr_base);
    let (before_ltr, after_ltr) = ltr_base
      .split_at(
        split_offset,
        false,
        &pipeline,
        &font_context,
        &mut reshape_cache,
      )
      .expect("split should succeed");
    assert_split_matches_original(&ltr_base, split_offset, &before_ltr, &after_ltr);
  }

  #[test]
  fn bidi_nested_plaintext_preserves_element_base_direction_when_slicing() {
    let pipeline = ShapingPipeline::new();
    let font_context = FontContext::new();

    let mut style_normal = ComputedStyle::default();
    style_normal.font_kerning = FontKerning::None;
    style_normal.direction = Direction::Ltr;
    style_normal.unicode_bidi = UnicodeBidi::Normal;
    let style_normal = Arc::new(style_normal);

    let mut style_plaintext = (*style_normal).clone();
    style_plaintext.unicode_bidi = UnicodeBidi::Plaintext;
    let style_plaintext = Arc::new(style_plaintext);

    // Shape the mixed-direction text with a plaintext base (first strong RTL).
    let mixed = "אבג ABC";
    let abc_start = mixed
      .find('A')
      .expect("expected Latin segment in plaintext test string");
    let abc_end = abc_start + "ABC".len();

    let expected_rtl = make_shaped_text_item(
      mixed,
      style_normal.clone(),
      Direction::Rtl,
      None,
      &pipeline,
      &font_context,
    );
    let expected_ltr = make_shaped_text_item(
      mixed,
      style_normal.clone(),
      Direction::Ltr,
      None,
      &pipeline,
      &font_context,
    );

    let expected_rtl_abc: Vec<(usize, u8, bool)> = glyph_signature_sequence(&expected_rtl)
      .into_iter()
      .filter(|(cluster, _, _)| *cluster >= abc_start && *cluster < abc_end)
      .collect();
    let expected_ltr_abc: Vec<(usize, u8, bool)> = glyph_signature_sequence(&expected_ltr)
      .into_iter()
      .filter(|(cluster, _, _)| *cluster >= abc_start && *cluster < abc_end)
      .collect();
    assert_ne!(
      expected_rtl_abc, expected_ltr_abc,
      "expected mixed-direction shaping to depend on the paragraph base direction"
    );

    let strut = make_strut_metrics();
    let mut builder = LineBuilder::new(
      500.0,
      500.0,
      true,
      TextWrap::Auto,
      0.0,
      false,
      false,
      strut,
      pipeline.clone(),
      font_context.clone(),
      Some(Level::ltr()),
      UnicodeBidi::Normal,
      Direction::Ltr,
      None,
      0.0,
      0.0,
      None,
    );

    // Add a non-plaintext prefix to keep the paragraph base fixed to LTR.
    let mut prefix = make_shaped_text_item(
      "X ",
      style_normal.clone(),
      Direction::Ltr,
      None,
      &pipeline,
      &font_context,
    );
    prefix.box_id = 1;
    builder.add_item(InlineItem::Text(prefix)).unwrap();

    let mut plaintext_item = make_shaped_text_item(
      mixed,
      style_plaintext,
      Direction::Rtl,
      None,
      &pipeline,
      &font_context,
    );
    plaintext_item.box_id = 2;
    builder.add_item(InlineItem::Text(plaintext_item)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].resolved_direction, Direction::Ltr);

    let mut observed_abc: Vec<(usize, u8, bool)> = Vec::new();
    for positioned in &lines[0].items {
      if let InlineItem::Text(item) = &positioned.item {
        if item.box_id == 2 {
          observed_abc.extend(
            glyph_signature_sequence(item)
              .into_iter()
              .filter(|(cluster, _, _)| *cluster >= abc_start && *cluster < abc_end),
          );
        }
      }
    }
    assert!(
      !observed_abc.is_empty(),
      "expected to find shaped glyphs for the nested plaintext segment"
    );
    assert_eq!(observed_abc, expected_rtl_abc);
    assert_ne!(observed_abc, expected_ltr_abc);
  }

  #[test]
  fn break_opportunities_stay_on_char_boundaries() {
    let text = "‘bruises and blood’ in ‘Christy’ fights";

    let style = Arc::new(ComputedStyle::default());
    let shaper = ShapingPipeline::new();
    let font_context = FontContext::new();
    let mut runs = shaper.shape(text, &style, &font_context).unwrap();
    TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
    let metrics =
      TextItem::metrics_from_runs(&font_context, &runs, style.font_size, style.font_size);

    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      find_break_opportunities(text),
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let mut cache = ReshapeCache::default();
    for brk in &item.break_opportunities {
      assert!(
        item.text.is_char_boundary(brk.byte_offset),
        "Break at {} is not a char boundary",
        brk.byte_offset
      );
      if brk.byte_offset == 0 || brk.byte_offset >= item.text.len() {
        continue;
      }

      assert!(
        item
          .split_at(brk.byte_offset, false, &shaper, &font_context, &mut cache)
          .is_some(),
        "Split failed at {}",
        brk.byte_offset
      );
    }
  }

  #[test]
  fn split_at_mid_codepoint_aligns_to_char_boundary() {
    let text = "a😊b";

    let style = Arc::new(ComputedStyle::default());
    let shaper = ShapingPipeline::new();
    let font_context = FontContext::new();
    let mut runs = shaper.shape(text, &style, &font_context).unwrap();
    TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);
    let metrics =
      TextItem::metrics_from_runs(&font_context, &runs, style.font_size, style.font_size);

    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      find_break_opportunities(text),
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let mut cache = ReshapeCache::default();
    // Byte offset 2 is inside the emoji; split_at should clamp to the previous char boundary.
    let (before, after) = item
      .split_at(2, false, &shaper, &font_context, &mut cache)
      .expect("split_at should succeed even at mid-codepoint offsets");
    assert_eq!(before.text, "a");
    assert_eq!(after.text, "😊b");
  }

  #[test]
  fn split_runs_preserving_shaping_adjusts_offsets_for_vertical_runs() {
    const EPS: f32 = 1e-4;
    let text = "A B";
    let split_offset = text
      .char_indices()
      .nth(1)
      .expect("split after first char")
      .0;

    let font_data = std::fs::read(
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf"),
    )
    .expect("fixture font should load");
    let mut db = FontDatabase::empty();
    db.load_font_data(font_data)
      .expect("fixture font should parse");
    db.refresh_generic_fallbacks();
    let font_ctx = FontContext::with_database(Arc::new(db));

    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.writing_mode = WritingMode::VerticalRl;
    style.text_orientation = TextOrientation::Upright;
    let style = Arc::new(style);

    let shaper = ShapingPipeline::new();
    let mut runs = shaper
      .shape(text, style.as_ref(), &font_ctx)
      .expect("text should shape");
    TextItem::apply_spacing_to_runs(&mut runs, text, style.letter_spacing, style.word_spacing);

    assert!(
      runs
        .iter()
        .flat_map(|run| &run.glyphs)
        .any(|glyph| glyph.y_advance.abs() > f32::EPSILON),
      "expected vertical shaping to populate y_advance"
    );

    let metrics = TextItem::metrics_from_runs(&font_ctx, &runs, style.font_size, style.font_size);
    let item = TextItem::new(
      runs.clone(),
      text.to_string(),
      metrics,
      find_break_opportunities(text),
      Vec::new(),
      style.clone(),
      style.direction,
    );

    let boundary = item
      .cluster_boundary_exact(split_offset)
      .expect("expected cluster boundary at split offset");
    let run_idx = boundary.run_index.expect("expected run index");
    let glyph_split = boundary.glyph_end.expect("expected glyph boundary");
    let left_advance = boundary.run_advance;
    assert!(
      left_advance.is_finite() && left_advance > 0.0,
      "expected positive left advance, got {}",
      left_advance
    );

    let run = item.runs.get(run_idx).expect("run exists");
    let axis = run_inline_axis(run);
    assert_eq!(axis, InlineAxis::Vertical);

    let original_right_glyphs = run
      .glyphs
      .get(glyph_split..)
      .expect("right glyph slice exists");
    assert!(
      !original_right_glyphs.is_empty(),
      "expected glyphs on the right side of split (run_start={}, run_end={}, split_offset={}, glyph_split={}, glyphs_len={}, clusters={:?}, boundaries={:?})",
      run.start,
      run.end,
      split_offset,
      glyph_split,
      run.glyphs.len(),
      run.glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>(),
      item
        .cluster_advances
        .iter()
        .map(|b| (b.byte_offset, b.run_index, b.glyph_end, b.run_advance, b.advance))
        .collect::<Vec<_>>()
    );

    let (before_runs, after_runs) = item
      .split_runs_preserving_shaping(split_offset)
      .expect("split_runs_preserving_shaping should succeed");

    let original_ids: Vec<u32> = item
      .runs
      .iter()
      .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
      .collect();
    let split_ids: Vec<u32> = before_runs
      .iter()
      .chain(after_runs.iter())
      .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
      .collect();
    assert_eq!(
      split_ids, original_ids,
      "preserving shaping should keep glyph IDs and ordering"
    );

    let right_run = after_runs.first().expect("expected right run");
    let local = split_offset
      .checked_sub(run.start)
      .expect("split offset should be within run");
    assert_eq!(
      right_run.glyphs.len(),
      original_right_glyphs.len(),
      "split should preserve right-side glyph count"
    );
    for (orig, new) in original_right_glyphs.iter().zip(right_run.glyphs.iter()) {
      assert_eq!(new.glyph_id, orig.glyph_id);
      assert!(
        (new.x_offset - orig.x_offset).abs() < EPS,
        "cross-axis (x) offsets should remain unchanged for vertical runs"
      );
      assert!(
        (new.y_offset - orig.y_offset).abs() < EPS,
        "inline-axis (y) offsets should remain unchanged for vertical runs"
      );
      assert_eq!(
        new.cluster,
        orig.cluster.saturating_sub(local as u32),
        "cluster indices should be rebased to the new run"
      );
    }

    let before_text = &text[..split_offset];
    let after_text = &text[split_offset..];
    let before_metrics =
      TextItem::metrics_from_runs(&font_ctx, &before_runs, style.font_size, style.font_size);
    let after_metrics =
      TextItem::metrics_from_runs(&font_ctx, &after_runs, style.font_size, style.font_size);
    let before_item = TextItem::new(
      before_runs,
      before_text.to_string(),
      before_metrics,
      find_break_opportunities(before_text),
      Vec::new(),
      style.clone(),
      style.direction,
    );
    let after_item = TextItem::new(
      after_runs,
      after_text.to_string(),
      after_metrics,
      find_break_opportunities(after_text),
      Vec::new(),
      style.clone(),
      style.direction,
    );
    assert!(
      before_item.advance.is_finite() && before_item.advance > 0.0,
      "before split TextItem advance should be finite and positive"
    );
    assert!(
      after_item.advance.is_finite() && after_item.advance > 0.0,
      "after split TextItem advance should be finite and positive"
    );
  }

  #[test]
  fn split_at_is_safe_on_non_char_boundaries() {
    let item = make_text_item("â€œenhancedâ€", 20.0);
    let pipeline = ShapingPipeline::new();
    let font_ctx = FontContext::new();
    let mut cache = ReshapeCache::default();
    assert!(item
      .split_at(1, false, &pipeline, &font_ctx, &mut cache)
      .is_none());
  }

  #[test]
  fn break_point_selection_matches_linear_scan() {
    let widths = [0.0, 0.5, 1.0, 2.0, 3.5, 10.0];
    let mut cases: Vec<TextItem> = Vec::new();

    let mut allowed_only = make_text_item("aaaa", 4.0);
    allowed_only.break_opportunities = vec![
      BreakOpportunity::allowed(1),
      BreakOpportunity::allowed(2),
      BreakOpportunity::allowed(3),
    ];
    allowed_only.first_mandatory_break =
      TextItem::first_mandatory_break(&allowed_only.break_opportunities);
    cases.push(allowed_only);

    let mut mandatory_first = make_text_item("aaaa", 4.0);
    mandatory_first.break_opportunities =
      vec![BreakOpportunity::mandatory(2), BreakOpportunity::allowed(3)];
    mandatory_first.first_mandatory_break =
      TextItem::first_mandatory_break(&mandatory_first.break_opportunities);
    cases.push(mandatory_first);

    let mut break_word = make_text_item("abcdef", 6.0);
    break_word.break_opportunities.clear();
    Arc::make_mut(&mut break_word.style).word_break = WordBreak::BreakWord;
    break_word.first_mandatory_break = None;
    cases.push(break_word);

    let mut overflow_wrap = make_text_item("abcdef", 6.0);
    overflow_wrap.break_opportunities.clear();
    Arc::make_mut(&mut overflow_wrap.style).overflow_wrap = OverflowWrap::BreakWord;
    overflow_wrap.first_mandatory_break = None;
    cases.push(overflow_wrap);

    let mut priority = make_text_item("abcd", 4.0);
    priority.break_opportunities =
      vec![BreakOpportunity::allowed(2), BreakOpportunity::emergency(3)];
    priority.first_mandatory_break = TextItem::first_mandatory_break(&priority.break_opportunities);
    cases.push(priority);

    for (idx, item) in cases.into_iter().enumerate() {
      for width in &widths {
        let expected = old_find_break_point(&item, *width);
        let actual = item.find_break_point(*width);
        assert_eq!(
          actual, expected,
          "case {} text {} width {}",
          idx, item.text, width
        );
      }
    }
  }

  #[test]
  fn find_break_point_prefers_normal_breaks_over_emergency() {
    use crate::text::line_break::BreakOpportunityKind;

    let mut item = make_text_item("hello world", 11.0);
    item.break_opportunities = vec![
      BreakOpportunity::allowed(6),   // after the space
      BreakOpportunity::emergency(7), // after "w"
      BreakOpportunity::allowed(11),  // end of text
    ];
    item.first_mandatory_break = None;

    // Enough room for "hello w", but a normal break at the space should win.
    let brk = item.find_break_point(7.0).expect("break point");
    assert_eq!(brk.byte_offset, 6);
    assert_eq!(brk.kind, BreakOpportunityKind::Normal);
  }

  #[test]
  fn find_break_point_allows_fitting_breaks_after_trimming_trailing_spaces() {
    // Regression test for CSS Text whitespace trimming:
    //
    // When a soft wrap occurs at an allowed break opportunity, trailing collapsible spaces are
    // trimmed from the line. Break selection must account for this trimming; otherwise we can wrap
    // earlier than necessary because we treated the trailing space width as "real".
    //
    // This mirrors the `si.edu` "at all locations" / "except" wrap: the space between "locations"
    // and "except" can be trimmed at the line end, allowing "locations" to fit.
    let mut item = make_text_item("foo bar baz", 11.0);
    item.break_opportunities = vec![BreakOpportunity::allowed(4), BreakOpportunity::allowed(8)];
    item.first_mandatory_break = TextItem::first_mandatory_break(&item.break_opportunities);

    // With our synthetic metrics each byte is 1px wide, so:
    // - raw width at offset 8 ("foo bar ") is 8px
    // - trimmed width (dropping the trailing space) is 7px
    //
    // Choose a max width that only fits after trimming.
    let brk = item.find_break_point(7.4).expect("break point");
    assert_eq!(brk.byte_offset, 8);
  }

  fn synthesize_breaks(
    text: &str,
    word_break: WordBreak,
    overflow_wrap: OverflowWrap,
  ) -> Vec<BreakOpportunity> {
    crate::layout::contexts::inline::apply_break_properties(
      text,
      "en",
      find_break_opportunities(text),
      crate::style::types::LineBreak::Auto,
      word_break,
      overflow_wrap,
      true,
    )
  }

  #[test]
  fn overflow_wrap_anywhere_breaks_are_emergency_only() {
    use crate::text::line_break::BreakOpportunityKind;

    // CSS Text (Level 3/4): `overflow-wrap:anywhere` is like `break-word`, except its introduced
    // wrap opportunities affect min-content sizing. Line breaking still only uses these intra-word
    // opportunities when there are no other acceptable break points in the line.
    let text = "hello world";
    let max_width = 7.0;

    let mut break_word = make_text_item(text, text.len() as f32);
    Arc::make_mut(&mut break_word.style).overflow_wrap = OverflowWrap::BreakWord;
    break_word.break_opportunities =
      synthesize_breaks(text, WordBreak::Normal, OverflowWrap::BreakWord);
    break_word.first_mandatory_break =
      TextItem::first_mandatory_break(&break_word.break_opportunities);

    let mut anywhere = make_text_item(text, text.len() as f32);
    Arc::make_mut(&mut anywhere.style).overflow_wrap = OverflowWrap::Anywhere;
    anywhere.break_opportunities =
      synthesize_breaks(text, WordBreak::Normal, OverflowWrap::Anywhere);
    anywhere.first_mandatory_break = TextItem::first_mandatory_break(&anywhere.break_opportunities);

    let brk7_break_word = break_word
      .break_opportunities
      .iter()
      .find(|b| b.byte_offset == 7)
      .copied()
      .expect("break-word should synthesize an intra-word break at offset 7");
    assert_eq!(brk7_break_word.kind, BreakOpportunityKind::Emergency);

    let brk7_anywhere = anywhere
      .break_opportunities
      .iter()
      .find(|b| b.byte_offset == 7)
      .copied()
      .expect("anywhere should synthesize an intra-word break at offset 7");
    assert_eq!(brk7_anywhere.kind, BreakOpportunityKind::Emergency);

    let chosen_break_word = break_word.find_break_point(max_width).expect("break point");
    assert_eq!(
      chosen_break_word.byte_offset, 6,
      "break-word prefers the space break"
    );

    let chosen_anywhere = anywhere.find_break_point(max_width).expect("break point");
    assert_eq!(
      chosen_anywhere.byte_offset, 6,
      "anywhere should not override the normal space break"
    );
    assert_eq!(chosen_anywhere.kind, BreakOpportunityKind::Normal);
  }

  #[test]
  fn word_break_anywhere_breaks_are_emergency_only() {
    use crate::text::line_break::BreakOpportunityKind;

    // Like `overflow-wrap:anywhere`, we treat `word-break:anywhere` as adding intra-word emergency
    // wrap opportunities (used only when no normal break fits), while still affecting intrinsic
    // sizing in min-content calculations.
    let text = "hello world";
    let max_width = 7.0;

    let mut break_word = make_text_item(text, text.len() as f32);
    Arc::make_mut(&mut break_word.style).word_break = WordBreak::BreakWord;
    break_word.break_opportunities =
      synthesize_breaks(text, WordBreak::BreakWord, OverflowWrap::Normal);
    break_word.first_mandatory_break =
      TextItem::first_mandatory_break(&break_word.break_opportunities);

    let mut anywhere = make_text_item(text, text.len() as f32);
    Arc::make_mut(&mut anywhere.style).word_break = WordBreak::Anywhere;
    anywhere.break_opportunities =
      synthesize_breaks(text, WordBreak::Anywhere, OverflowWrap::Normal);
    anywhere.first_mandatory_break = TextItem::first_mandatory_break(&anywhere.break_opportunities);

    let brk7_break_word = break_word
      .break_opportunities
      .iter()
      .find(|b| b.byte_offset == 7)
      .copied()
      .expect("break-word should synthesize an intra-word break at offset 7");
    assert_eq!(brk7_break_word.kind, BreakOpportunityKind::Emergency);

    let brk7_anywhere = anywhere
      .break_opportunities
      .iter()
      .find(|b| b.byte_offset == 7)
      .copied()
      .expect("anywhere should synthesize an intra-word break at offset 7");
    assert_eq!(brk7_anywhere.kind, BreakOpportunityKind::Emergency);

    let chosen_break_word = break_word.find_break_point(max_width).expect("break point");
    assert_eq!(
      chosen_break_word.byte_offset, 6,
      "break-word prefers the space break"
    );

    let chosen_anywhere = anywhere.find_break_point(max_width).expect("break point");
    assert_eq!(
      chosen_anywhere.byte_offset, 6,
      "anywhere should not override the normal space break"
    );
    assert_eq!(chosen_anywhere.kind, BreakOpportunityKind::Normal);
  }

  #[test]
  fn test_line_builder_single_item_fits() {
    let mut builder = make_builder(100.0);

    let item = make_text_item("Hello", 50.0);
    builder.add_item(InlineItem::Text(item)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].items.len(), 1);
    assert!(lines[0].width <= 100.0);
  }

  #[test]
  fn test_line_builder_multiple_items_fit() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" ", 5.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("World", 50.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].items.len(), 3);
  }

  #[test]
  fn test_line_builder_item_exceeds_width() {
    let mut builder = make_builder(80.0);

    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("World", 50.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    // Second item should go to new line
    assert_eq!(lines.len(), 2);
  }

  #[test]
  fn inline_boxes_fragment_to_fill_remaining_line_width() {
    // Regression: Inline boxes were only fragmented after forcing them onto a fresh line, causing
    // unnecessary extra line breaks (notably alongside floats).
    let mut builder = make_builder(100.0);
    builder
      .add_item(InlineItem::Text(make_text_item("Hello ", 60.0)))
      .unwrap();

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("World Wide", 60.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let line0_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let line1_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(line0_text, "Hello World");
    assert_eq!(line1_text, "Wide");
  }

  #[test]
  fn nested_inline_boxes_fragment_to_fill_remaining_line_width() {
    // Same as `inline_boxes_fragment_to_fill_remaining_line_width`, but exercises nested inline
    // boxes. Previously we would refuse to split a nested inline box once some content had already
    // been added to the current fragment, forcing the entire nested box onto the next line.
    let mut builder = make_builder(100.0);

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    inner.add_child(InlineItem::Text(make_text_item("World Wide", 60.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    outer.add_child(InlineItem::Text(make_text_item("Hello ", 60.0)));
    outer.add_child(InlineItem::InlineBox(inner));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);
    let line0_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let line1_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(line0_text, "Hello World");
    assert_eq!(line1_text, "Wide");
  }

  #[test]
  fn breakable_text_that_overflows_by_subpixel_stays_on_one_line() {
    let mut builder = make_builder(100.0);
    builder
      .add_item(InlineItem::Text(make_text_item("Compare Now", 100.25)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].items.len(), 1);
    assert_eq!(flatten_text(&lines[0].items[0].item), "Compare Now");
  }

  #[test]
  fn test_line_builder_force_break() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder.force_break().unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("World", 50.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with_hard_break);
  }

  #[test]
  fn test_line_builder_empty_result() {
    let builder = make_builder(100.0);

    let lines = builder.finish().unwrap().lines;
    assert!(lines.is_empty());
  }

  #[test]
  fn terminal_hard_break_does_not_create_trailing_empty_line() {
    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder
      .add_item(InlineItem::HardBreak(ClearSide::None))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with_hard_break);
    assert_eq!(flatten_text(&lines[0].items[0].item), "Hello");
  }

  #[test]
  fn hard_break_only_produces_single_empty_line() {
    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::HardBreak(ClearSide::None))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with_hard_break);
    assert!(lines[0].items.is_empty());
  }

  #[test]
  fn hard_break_followed_by_collapsible_whitespace_does_not_create_trailing_empty_line() {
    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::HardBreak(ClearSide::None))
      .unwrap();
    // This whitespace becomes the leading content of the next line and is therefore suppressed by
    // whitespace collapsing. The trailing line box must not be materialized.
    builder
      .add_item(InlineItem::Text(make_text_item(" ", 5.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with_hard_break);
    assert!(lines[0].items.is_empty());
  }

  #[test]
  fn consecutive_hard_breaks_produce_multiple_empty_lines() {
    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::HardBreak(ClearSide::None))
      .unwrap();
    builder
      .add_item(InlineItem::HardBreak(ClearSide::None))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with_hard_break);
    assert!(lines[1].ends_with_hard_break);
    assert!(lines[0].items.is_empty());
    assert!(lines[1].items.is_empty());
  }

  #[test]
  fn test_line_has_baseline() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].baseline > 0.0);
    assert!(lines[0].height > 0.0);
  }

  #[test]
  fn test_replaced_item() {
    let mut builder = make_builder(200.0);

    let replaced = ReplacedItem::new(
      1,
      Size::new(100.0, 50.0),
      ReplacedType::Image {
        src: String::new(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
      Arc::new(ComputedStyle::default()),
      0.0,
      0.0,
      0.0,
      0.0,
    );
    builder.add_item(InlineItem::Replaced(replaced)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].items[0].item.width(), 100.0);
  }

  #[test]
  fn replaced_item_does_not_force_strut_line_height() {
    // Regression: lines containing only a (small) replaced element should size to the replaced
    // element's height, not the containing block's strut/line-height. This affects patterns like
    // `<img height=10 width=0><table ...>` used as spacers.
    let mut builder = make_builder(200.0);

    let replaced = ReplacedItem::new(
      1,
      Size::new(0.0, 10.0),
      ReplacedType::Image {
        src: String::new(),
        alt: None,
        loading: Default::default(),
        decoding: ImageDecodingAttribute::Auto,
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
      },
      Arc::new(ComputedStyle::default()),
      0.0,
      0.0,
      0.0,
      0.0,
    );
    builder.add_item(InlineItem::Replaced(replaced)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(
      (lines[0].height - 10.0).abs() < 1e-3,
      "line height was {}",
      lines[0].height
    );
  }

  #[test]
  fn test_inline_block_item() {
    let mut builder = make_builder(200.0);

    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 80.0, 40.0), vec![]);
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );
    builder
      .add_item(InlineItem::InlineBlock(inline_block))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].items[0].item.width(), 80.0);
  }

  #[test]
  fn inline_block_items_do_not_wrap_when_white_space_nowrap() {
    let mut builder = make_builder(100.0);

    let mut style = ComputedStyle::default();
    style.white_space = WhiteSpace::Nowrap;
    let style = Arc::new(style);

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![],
      style,
    );
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![],
      Arc::new({
        let mut style = ComputedStyle::default();
        style.white_space = WhiteSpace::Nowrap;
        style
      }),
    );
    let inline_block2 = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    builder
      .add_item(InlineItem::InlineBlock(inline_block))
      .unwrap();
    builder
      .add_item(InlineItem::InlineBlock(inline_block2))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(
      lines.len(),
      1,
      "nowrap inline-blocks should overflow instead of wrapping"
    );
    assert!(
      lines[0].width > 100.0,
      "line width should reflect overflow (got {})",
      lines[0].width
    );
  }

  #[test]
  fn static_position_anchor_does_not_create_new_line_after_overflowing_nowrap_content() {
    // Regression test for yelp.com:
    // The header nav ends with an absolutely positioned overflow placeholder. We insert a
    // `StaticPositionAnchor` for it, but that anchor must not force a new line when the
    // `white-space: nowrap` nav items already overflow the line width.
    let mut builder = make_builder(100.0);

    let nowrap_style = Arc::new({
      let mut style = ComputedStyle::default();
      style.white_space = WhiteSpace::Nowrap;
      style
    });

    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![],
      nowrap_style.clone(),
    );
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![],
      nowrap_style,
    );
    let inline_block2 = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    builder
      .add_item(InlineItem::InlineBlock(inline_block))
      .unwrap();
    builder
      .add_item(InlineItem::InlineBlock(inline_block2))
      .unwrap();

    builder
      .add_item(InlineItem::StaticPositionAnchor(StaticPositionAnchor::new(
        1,
        Direction::Ltr,
        UnicodeBidi::Normal,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(
      lines.len(),
      1,
      "static position anchors must not introduce a new line after overflow"
    );
    assert!(
      lines[0].width > 100.0,
      "line width should reflect overflow (got {})",
      lines[0].width
    );
    assert!(
      lines[0]
        .items
        .iter()
        .any(|pos| matches!(pos.item, InlineItem::StaticPositionAnchor(_))),
      "expected anchor to be placed on the overflowing line"
    );
  }

  #[test]
  fn inline_block_vertical_margins_affect_line_height() {
    // GitLab's desktop nav uses `padding-bottom` with `margin-bottom: -Npx` on inline-block
    // dropdown wrappers to keep the visual hit area without inflating the header height.
    //
    // Ensure inline-block vertical margins contribute to the line box metrics so negative margins
    // can cancel padding.
    let mut builder = make_builder(200.0);
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 80.0, 30.0), vec![]);
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      -10.0,
      true,
    );
    builder
      .add_item(InlineItem::InlineBlock(inline_block))
      .unwrap();
    let lines = builder.finish().unwrap().lines;

    // Border box is 30px tall; margin-bottom:-10 shrinks the margin box to 20px.
    // With the default strut (ascent=12, descent=4), line height becomes 20 + 4 = 24.
    assert!(
      (lines[0].height - 24.0).abs() < 0.01,
      "unexpected line height: {}",
      lines[0].height
    );
  }

  #[test]
  fn inline_block_baseline_prefers_last_line_box() {
    // Create an inline-block fragment that contains a line box at y=5 with baseline 8 (relative to the line box).
    let line = FragmentNode::new_line(Rect::from_xywh(0.0, 5.0, 60.0, 10.0), 8.0, vec![]);
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), vec![line]);

    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    // Baseline should be derived from the line (5 + 8 = 13) rather than the bottom border edge (20).
    assert!((inline_block.metrics.baseline_offset - 13.0).abs() < 0.001);
    assert!((inline_block.metrics.descent - 7.0).abs() < 0.001);
  }

  #[test]
  fn inline_block_baseline_ignores_out_of_flow_positioned_descendants() {
    use crate::style::position::Position;

    // Inline-block with an in-flow line box at baseline 8, plus an absolutely positioned subtree
    // whose baselines should not affect the inline-block baseline.
    let in_flow_line = FragmentNode::new_line(Rect::from_xywh(0.0, 0.0, 60.0, 10.0), 8.0, vec![]);

    let abs_style = Arc::new({
      let mut style = ComputedStyle::default();
      style.position = Position::Absolute;
      style
    });
    let abs_line = FragmentNode::new_line(Rect::from_xywh(0.0, 0.0, 40.0, 8.0), 6.0, vec![]);
    let abs_block = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 100.0, 40.0, 8.0),
      FragmentContent::Block { box_id: None },
      vec![abs_line],
      abs_style,
    );

    let fragment = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      vec![in_flow_line, abs_block],
    );

    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    assert!(
      (inline_block.metrics.baseline_offset - 8.0).abs() < 0.001,
      "expected baseline from in-flow line box, got {}",
      inline_block.metrics.baseline_offset
    );
  }

  #[test]
  fn inline_block_baseline_falls_back_when_overflow_clips() {
    // Even with a line box present, non-visible overflow forces the baseline to the bottom margin edge.
    let line = FragmentNode::new_line(Rect::from_xywh(0.0, 2.0, 40.0, 8.0), 6.0, vec![]);
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 16.0), vec![line]);
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      false, // overflow != visible
    );

    assert!((inline_block.metrics.baseline_offset - 16.0).abs() < 0.001);
    assert!((inline_block.metrics.descent - 0.0).abs() < 0.001);
  }

  #[test]
  fn inline_block_baseline_falls_back_when_no_lines() {
    // Overflow visible but no in-flow line boxes: baseline should be the bottom edge.
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 30.0, 12.0), vec![]);
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    assert!((inline_block.metrics.baseline_offset - 12.0).abs() < 0.001);
    assert!((inline_block.metrics.descent - 0.0).abs() < 0.001);
  }

  #[test]
  fn inline_table_baseline_uses_first_row_baseline() {
    // Simulate an inline-table fragment with two lines; baseline should come from the first line.
    let line1 = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 20.0, 10.0),
      FragmentContent::Line { baseline: 4.0 },
      vec![],
    );
    let line2 = FragmentNode::new(
      Rect::from_xywh(0.0, 10.0, 20.0, 10.0),
      FragmentContent::Line { baseline: 6.0 },
      vec![],
    );
    let mut style = ComputedStyle::default();
    style.display = Display::InlineTable;
    let fragment = FragmentNode::new_with_style(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      FragmentContent::Block { box_id: None },
      vec![line1, line2],
      Arc::new(style),
    );
    let inline_block = InlineBlockItem::new(
      fragment,
      Direction::Ltr,
      UnicodeBidi::Normal,
      0.0,
      0.0,
      0.0,
      0.0,
      true,
    );

    assert!((inline_block.metrics.baseline_offset - 4.0).abs() < 0.001);
    assert!((inline_block.metrics.descent - 16.0).abs() < 0.001);
  }

  #[test]
  fn test_overflow_on_empty_line() {
    let mut builder = make_builder(30.0);

    // Item too wide but line is empty, so it must fit
    builder
      .add_item(InlineItem::Text(make_text_item("VeryLongWord", 100.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert!(lines[0].width > 30.0); // Overflow allowed
  }

  #[test]
  fn test_positioned_item_x_position() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("Hello", 50.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" ", 5.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("World", 50.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines[0].items[0].x, 0.0);
    assert_eq!(lines[0].items[1].x, 50.0);
    assert_eq!(lines[0].items[2].x, 55.0);
  }

  #[test]
  fn test_text_item_break_opportunities() {
    let item = make_text_item("Hello World Test", 160.0);

    // Should have break opportunities after spaces
    assert!(!item.break_opportunities.is_empty());
  }

  #[test]
  fn test_vertical_align_default() {
    let item = make_text_item("Test", 40.0);
    assert_eq!(item.vertical_align, VerticalAlign::Baseline);
  }

  #[test]
  fn vertical_align_middle_uses_parent_strut_metrics() {
    let mut item = make_text_item("Test", 40.0).with_vertical_align(VerticalAlign::Middle);
    // Give the item predictable metrics to compare against the parent strut (which has x-height 6).
    item.metrics = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);

    let mut builder = make_builder(200.0);
    builder.add_item(InlineItem::Text(item)).unwrap();
    let lines = builder.finish().unwrap().lines;

    // With parent x-height 6, middle shift = 12 - 8 - 3 = 1.
    let first = &lines[0].items[0];
    assert!((first.baseline_offset - 1.0).abs() < 1e-3);
  }

  #[test]
  fn test_line_default() {
    let line = Line::default();
    assert!(line.is_empty());
    assert_eq!(line.width, 0.0);
  }

  #[test]
  fn bidi_runs_use_byte_indices_for_levels() {
    // Hebrew characters are multi-byte; the RTL byte length must not confuse run-level lookup.
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("א", 10.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("a", 10.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("b", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(
      texts,
      vec!["א".to_string(), "a".to_string(), "b".to_string()]
    );
  }

  #[test]
  fn bidi_mixed_direction_splits_text_item() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("abc אבג", 70.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(texts, vec!["abc ".to_string(), "אבג".to_string()]);
  }

  #[test]
  fn bidi_plaintext_chooses_first_strong_base_direction() {
    let mut builder = make_builder_with_base(200.0, Level::rtl());
    builder
      .add_item(InlineItem::Text(make_text_item_with_bidi(
        "abc אבג",
        70.0,
        UnicodeBidi::Plaintext,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    // Base was RTL, but plaintext forces first-strong (LTR here), so visual order stays logical LTR then RTL.
    assert_eq!(texts, vec!["abc ".to_string(), "אבג".to_string()]);
  }

  #[test]
  fn bidi_plaintext_inline_preserves_paragraph_base_direction() {
    let mut builder = make_builder_with_base(200.0, Level::ltr());
    builder
      .add_item(InlineItem::Text(make_text_item_with_bidi(
        "אבג",
        30.0,
        UnicodeBidi::Plaintext,
      )))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" xyz", 30.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].resolved_direction, Direction::Ltr);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(texts, vec!["אבג".to_string(), " xyz".to_string()]);
  }

  #[test]
  fn bidi_plaintext_sets_base_direction_on_items() {
    let mut items = vec![
      InlineItem::Text(make_text_item_with_bidi(
        "אבג",
        30.0,
        UnicodeBidi::Plaintext,
      )),
      InlineItem::Tab(TabItem::new(
        Arc::new(ComputedStyle::default()),
        BaselineMetrics::new(10.0, 12.0, 8.0, 2.0),
        8.0,
        true,
      )),
    ];

    crate::layout::contexts::inline::apply_plaintext_paragraph_direction(
      &mut items,
      Direction::Rtl,
    );

    let dir_text = match &items[0] {
      InlineItem::Text(t) => t.base_direction,
      _ => Direction::Ltr,
    };
    let dir_tab = match &items[1] {
      InlineItem::Tab(t) => t.direction,
      _ => Direction::Ltr,
    };

    assert_eq!(dir_text, Direction::Rtl);
    assert_eq!(dir_tab, Direction::Rtl);
  }

  #[test]
  fn bidi_isolate_inline_box_prevents_surrounding_reordering() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("ABC ", 40.0)))
      .unwrap();

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("א", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("ב", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("ג", 10.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    builder
      .add_item(InlineItem::Text(make_text_item(" DEF", 40.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            _ => None,
          })
          .collect::<String>(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(
      texts,
      vec!["ABC ".to_string(), "גבא".to_string(), " DEF".to_string()]
    );
  }

  #[test]
  fn bidi_isolate_wraps_multiple_leaves_once() {
    let mut builder = make_builder(200.0);

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("א", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("ב", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("ג", 10.0)));

    builder
      .add_item(InlineItem::Text(make_text_item("L", 10.0)))
      .unwrap();
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" R", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            _ => None,
          })
          .collect::<String>(),
        _ => String::new(),
      })
      .collect();

    // Children stay adjacent and isolate prevents surrounding runs from interleaving; RTL order places the later
    // child earlier in visual order.
    assert_eq!(
      texts,
      vec!["L".to_string(), "גבא".to_string(), " R".to_string()]
    );
  }

  #[test]
  fn bidi_isolate_override_reverses_child_order() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("A ", 10.0)))
      .unwrap();

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::IsolateOverride,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("a", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("b", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("c", 10.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    builder
      .add_item(InlineItem::Text(make_text_item(" C", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            _ => None,
          })
          .collect::<String>(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(
      texts,
      vec!["A ".to_string(), "cba".to_string(), " C".to_string()]
    );
  }

  #[test]
  fn bidi_reorder_does_not_coalesce_distinct_inline_boxes_with_same_box_index() {
    let mut builder = make_builder(200.0);

    let mut first = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    first.box_id = 1;
    first.add_child(InlineItem::Text(make_text_item("one", 30.0)));

    let mut second = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    second.box_id = 2;
    second.add_child(InlineItem::Text(make_text_item("two", 30.0)));

    builder.add_item(InlineItem::InlineBox(first)).unwrap();
    builder.add_item(InlineItem::InlineBox(second)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    let top_level_boxes: Vec<&InlineBoxItem> = lines[0]
      .items
      .iter()
      .filter_map(|p| match &p.item {
        InlineItem::InlineBox(b) => Some(b),
        _ => None,
      })
      .collect();
    assert_eq!(top_level_boxes.len(), 2);
    assert_eq!(top_level_boxes[0].box_id, 1);
    assert_eq!(top_level_boxes[1].box_id, 2);
  }

  #[test]
  fn explicit_bidi_context_resets_override_on_isolate() {
    let ctx = explicit_bidi_context(
      Direction::Ltr,
      &[
        (UnicodeBidi::BidiOverride, Direction::Rtl),
        (UnicodeBidi::Isolate, Direction::Ltr),
      ],
    )
    .expect("should compute explicit context");
    assert!(!ctx.override_all, "override should not leak past isolates");
    assert!(
      ctx.level.number() % 2 == 0,
      "isolate should push an even level for LTR"
    );
  }

  #[test]
  fn explicit_bidi_context_sets_override_for_isolate_override() {
    let ctx = explicit_bidi_context(
      Direction::Ltr,
      &[(UnicodeBidi::IsolateOverride, Direction::Ltr)],
    )
    .expect("should compute explicit context");
    assert!(
      ctx.override_all,
      "isolate-override should force overriding status"
    );
    assert!(ctx.level.number() % 2 == 0);
  }

  #[test]
  fn excessive_embedding_depth_is_clamped() {
    // Build a deeply nested set of inline boxes that exceed unicode_bidi's max depth.
    let mut inner = InlineItem::Text(make_text_item("abc", 30.0));
    for idx in 0..(level::MAX_EXPLICIT_DEPTH as usize + 8) {
      let mut box_item = InlineBoxItem::new(
        0.0,
        0.0,
        0.0,
        make_strut_metrics(),
        Arc::new(ComputedStyle::default()),
        idx,
        Direction::Ltr,
        UnicodeBidi::Embed,
      );
      box_item.add_child(inner);
      inner = InlineItem::InlineBox(box_item);
    }

    let positioned = PositionedItem {
      item: inner,
      x: 0.0,
      baseline_offset: 0.0,
    };
    let mut line = Line::new();
    line.items.push(positioned);
    let mut lines = vec![line];

    let mut deadline_counter = 0usize;
    reorder_paragraph(
      &mut lines,
      &mut deadline_counter,
      Some(Level::ltr()),
      UnicodeBidi::Normal,
      Direction::Ltr,
      &ShapingPipeline::new(),
      &FontContext::new(),
    )
    .unwrap();

    assert!(
      !lines[0].items.is_empty(),
      "reordering should still produce items even when depth exceeds the limit"
    );
    let width: f32 = lines[0].items.iter().map(|p| p.item.width()).sum();
    assert!(
      width > 0.0,
      "items should keep their width after reordering"
    );
  }

  fn collect_text(item: &InlineItem, out: &mut String) {
    match item {
      InlineItem::Text(t) => out.push_str(&t.text),
      InlineItem::InlineBox(b) => {
        for child in &b.children {
          collect_text(child, out);
        }
      }
      _ => {}
    }
  }

  #[test]
  fn suppressed_controls_do_not_duplicate_text() {
    // When embeddings are suppressed (depth clamp), the logical text should remain intact.
    let mut inner = InlineItem::Text(make_text_item("abc", 30.0));
    for idx in 0..(level::MAX_EXPLICIT_DEPTH as usize + 8) {
      let mut box_item = InlineBoxItem::new(
        0.0,
        0.0,
        0.0,
        make_strut_metrics(),
        Arc::new(ComputedStyle::default()),
        idx + 1,
        Direction::Ltr,
        UnicodeBidi::Embed,
      );
      box_item.add_child(inner);
      inner = InlineItem::InlineBox(box_item);
    }

    let mut line = Line::new();
    line.items.push(PositionedItem {
      item: inner,
      x: 0.0,
      baseline_offset: 0.0,
    });
    let mut lines = vec![line];

    let mut deadline_counter = 0usize;
    reorder_paragraph(
      &mut lines,
      &mut deadline_counter,
      Some(Level::ltr()),
      UnicodeBidi::Normal,
      Direction::Ltr,
      &ShapingPipeline::new(),
      &FontContext::new(),
    )
    .unwrap();

    let mut collected = String::new();
    for item in &lines[0].items {
      collect_text(&item.item, &mut collected);
    }
    assert_eq!(collected, "abc");
  }

  #[test]
  fn bidi_plaintext_on_inline_box_forces_first_strong() {
    let mut builder = make_builder_with_base(200.0, Level::rtl());

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Plaintext,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("abc אבג", 70.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .flat_map(|p| match &p.item {
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            _ => None,
          })
          .collect::<Vec<_>>(),
        InlineItem::Text(t) => vec![t.text.clone()],
        _ => vec![],
      })
      .collect();

    assert_eq!(texts, vec!["abc ".to_string(), "אבג".to_string()]);
  }

  #[test]
  fn bidi_nested_isolates_close_properly() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("L", 10.0)))
      .unwrap();

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Isolate,
    );
    inner.add_child(InlineItem::Text(make_text_item("מ", 10.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("א", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" R", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            InlineItem::InlineBox(inner) => Some(
              inner
                .children
                .iter()
                .filter_map(|c| match c {
                  InlineItem::Text(t) => Some(t.text.clone()),
                  _ => None,
                })
                .collect::<String>(),
            ),
            _ => None,
          })
          .collect::<String>(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(
      texts,
      vec!["L".to_string(), "אמ".to_string(), " R".to_string()]
    );
  }

  #[test]
  fn bidi_plaintext_isolate_keeps_paragraph_base() {
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("A ", 10.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item_with_bidi(
        "אבג",
        15.0,
        UnicodeBidi::Plaintext,
      )))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" C", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    assert_eq!(
      texts,
      vec!["A ".to_string(), "אבג".to_string(), " C".to_string()]
    );
  }

  #[test]
  fn bidi_plaintext_inline_box_isolates_punctuation_and_wraps_descendants() {
    // Plaintext should behave like `dir=auto` + isolate, which maps to FSI/PDI.
    // The trailing ':' is outside the plaintext scope; without isolation it would attach to the RTL
    // run and reorder before it.
    let logical = format!("A {}\u{05d0}\u{05d1}\u{05d2}{}: B", '\u{2068}', '\u{2069}');
    let expected = reorder_with_controls(&logical, Some(Level::ltr()));

    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::Text(make_text_item("A ", 20.0)))
      .unwrap();

    let mut nested = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    nested.add_child(InlineItem::Text(make_text_item("\u{05d1}", 10.0)));

    let mut plaintext = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      // Plaintext ignores `direction`; the isolate base comes from first-strong text.
      Direction::Ltr,
      UnicodeBidi::Plaintext,
    );
    plaintext.add_child(InlineItem::Text(make_text_item("\u{05d0}", 10.0)));
    plaintext.add_child(InlineItem::InlineBox(nested));
    plaintext.add_child(InlineItem::Text(make_text_item("\u{05d2}", 10.0)));

    builder.add_item(InlineItem::InlineBox(plaintext)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(": ", 10.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("B", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();

    assert_eq!(actual, expected);
  }

  #[test]
  fn bidi_plaintext_adjacent_boxes_do_not_merge() {
    // Adjacent plaintext boxes must remain separate FSI/PDI scopes so each resolves its own
    // first-strong direction (`dir=auto` semantics). If the scopes merge, the second box would
    // inherit the first box's base direction.
    let logical_separate =
      "\u{05d3}\u{2068}x\u{2069}\u{2068}\u{05d0}\u{05d1}\u{05d2}\u{2069}\u{05d4}".to_string();
    let logical_merged = "\u{05d3}\u{2068}x\u{05d0}\u{05d1}\u{05d2}\u{2069}\u{05d4}".to_string();
    let expected_separate = reorder_with_controls(&logical_separate, Some(Level::rtl()));
    let expected_merged = reorder_with_controls(&logical_merged, Some(Level::rtl()));
    assert_ne!(
      expected_separate, expected_merged,
      "test strings should differ when plaintext scopes merge"
    );

    let mut builder = make_builder_with_base(200.0, Level::rtl());
    builder
      .add_item(InlineItem::Text(make_text_item("\u{05d3}", 10.0)))
      .unwrap();

    let mut first = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Plaintext,
    );
    first.add_child(InlineItem::Text(make_text_item("x", 10.0)));

    let mut second = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Plaintext,
    );
    second.add_child(InlineItem::Text(make_text_item("\u{05d0}", 10.0)));
    second.add_child(InlineItem::Text(make_text_item("\u{05d1}", 10.0)));
    second.add_child(InlineItem::Text(make_text_item("\u{05d2}", 10.0)));

    builder.add_item(InlineItem::InlineBox(first)).unwrap();
    builder.add_item(InlineItem::InlineBox(second)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("\u{05d4}", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(actual, expected_separate);
  }

  #[test]
  fn bidi_plaintext_does_not_disable_nested_override_context() {
    // `unicode-bidi: plaintext` establishes an isolate, but it should not suppress nested
    // embed/override contexts for shaping/reordering purposes.
    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::Text(make_text_item("L ", 20.0)))
      .unwrap();

    let mut inner_override = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    inner_override.add_child(InlineItem::Text(make_text_item("a", 10.0)));

    let mut plaintext = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Ltr,
      UnicodeBidi::Plaintext,
    );
    plaintext.add_child(InlineItem::InlineBox(inner_override));

    builder.add_item(InlineItem::InlineBox(plaintext)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" R", 20.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    fn find_text<'a>(item: &'a InlineItem, needle: &str) -> Option<&'a TextItem> {
      match item {
        InlineItem::Text(t) if t.text == needle => Some(t),
        InlineItem::InlineBox(b) => b.children.iter().find_map(|c| find_text(c, needle)),
        InlineItem::Ruby(r) => r
          .segments
          .iter()
          .find_map(|seg| seg.base_items.iter().find_map(|c| find_text(c, needle))),
        _ => None,
      }
    }

    let text_item = lines[0]
      .items
      .iter()
      .find_map(|p| find_text(&p.item, "a"))
      .expect("expected to find inner override text item");
    assert!(
      text_item.explicit_bidi.is_some_and(|ctx| ctx.override_all),
      "expected nested override to set explicit bidi context even inside plaintext"
    );
  }

  #[test]
  fn bidi_plaintext_uses_first_strong_rtl_when_text_starts_rtl() {
    let mut builder = make_builder_with_base(200.0, Level::ltr());
    builder
      .add_item(InlineItem::Text(make_text_item_with_bidi(
        "אבג abc",
        70.0,
        UnicodeBidi::Plaintext,
      )))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();

    // Base came from first strong RTL; visual order (left-to-right positions) places the LTR run left.
    assert_eq!(texts, vec!["abc".to_string(), "אבג ".to_string()]);
  }

  #[test]
  fn bidi_plaintext_paragraph_base_only_when_present() {
    // First paragraph uses plaintext and should pick first-strong (LTR) despite RTL base.
    let mut builder = make_builder_with_base(200.0, Level::rtl());
    builder
      .add_item(InlineItem::Text(make_text_item_with_bidi(
        "abc אבג",
        70.0,
        UnicodeBidi::Plaintext,
      )))
      .unwrap();
    builder.force_break().unwrap();

    // Second paragraph has no plaintext and keeps the RTL base.
    builder
      .add_item(InlineItem::Text(make_text_item("abc ", 30.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("אבג", 30.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let para1: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();
    assert_eq!(para1, vec!["abc ".to_string(), "אבג".to_string()]);

    let para2: Vec<String> = lines[1]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        _ => String::new(),
      })
      .collect();
    assert_eq!(
      para2,
      vec!["אבג".to_string(), " ".to_string(), "abc".to_string()]
    );
  }

  #[test]
  fn bidi_nested_isolate_override_reorders_only_inner_scope() {
    // Outer isolate keeps its children grouped; the inner isolate-override reverses its content.
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("L", 10.0)))
      .unwrap();

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Rtl,
      UnicodeBidi::IsolateOverride,
    );
    inner.add_child(InlineItem::Text(make_text_item("x", 10.0)));
    inner.add_child(InlineItem::Text(make_text_item("y", 10.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    outer.add_child(InlineItem::Text(make_text_item("a", 10.0)));
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("b", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" R", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    let texts: Vec<String> = lines[0]
      .items
      .iter()
      .map(|p| match &p.item {
        InlineItem::Text(t) => t.text.clone(),
        InlineItem::InlineBox(b) => b
          .children
          .iter()
          .filter_map(|c| match c {
            InlineItem::Text(t) => Some(t.text.clone()),
            InlineItem::InlineBox(inner) => Some(
              inner
                .children
                .iter()
                .filter_map(|c| match c {
                  InlineItem::Text(t) => Some(t.text.clone()),
                  _ => None,
                })
                .collect::<String>(),
            ),
            _ => None,
          })
          .collect::<String>(),
        _ => String::new(),
      })
      .collect();

    let expected_inner = reorder_with_controls(
      &format!(
        "{}a{}{}xy{}{}b{}",
        '\u{2067}', // RLI (outer isolate)
        '\u{2067}', // RLI (inner isolate)
        '\u{202e}', // RLO
        '\u{202c}', // PDF
        '\u{2069}', // PDI (inner isolate)
        '\u{2069}'  // PDI (outer isolate)
      ),
      Some(Level::ltr()),
    );
    assert_eq!(
      texts,
      vec!["L".to_string(), expected_inner, " R".to_string()]
    );
  }

  #[test]
  fn bidi_isolate_override_keeps_inner_isolate_atomic() {
    // An isolate-override should reverse its own content while keeping nested isolates grouped.
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("L", 10.0)))
      .unwrap();

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Isolate,
    );
    inner.add_child(InlineItem::Text(make_text_item("BD", 20.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::IsolateOverride,
    );
    outer.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("C", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item(" R", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();

    let logical = format!(
      "L{}{}A{}BD{}C{}{} R",
      '\u{2067}', // RLI (rtl isolate)
      '\u{202e}', // RLO (rtl override)
      '\u{2066}', // LRI (ltr isolate)
      '\u{2069}', // PDI
      '\u{202c}', // PDF
      '\u{2069}', // PDI
    );
    let expected = reorder_with_controls(&logical, Some(Level::ltr()));
    assert_eq!(actual, expected);
  }

  #[test]
  fn bidi_override_does_not_apply_inside_isolate() {
    let mut builder = make_builder(200.0);

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Isolate,
    );
    inner.add_child(InlineItem::Text(make_text_item("א", 10.0)));
    inner.add_child(InlineItem::Text(make_text_item("ב", 10.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    outer.add_child(InlineItem::Text(make_text_item("a", 10.0)));
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("b", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let mut visual = String::new();
    for positioned in &lines[0].items {
      collect_text(&positioned.item, &mut visual);
    }

    let logical = "\u{202E}a\u{2066}אב\u{2069}b\u{202C}";
    let info = BidiInfo::new(logical, Some(Level::ltr()));
    let para = &info.paragraphs[0];
    let expected: String = info
      .reorder_line(para, para.range.clone())
      .chars()
      .filter(|c| !is_bidi_control(*c))
      .collect();

    assert_eq!(visual, expected);
  }

  #[test]
  fn bidi_override_parent_preserves_isolate_contents() {
    let mut builder = make_builder(200.0);

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Isolate,
    );
    inner.add_child(InlineItem::Text(make_text_item("א", 10.0)));
    inner.add_child(InlineItem::Text(make_text_item("ב", 10.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    outer.add_child(InlineItem::Text(make_text_item("x", 10.0)));
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("y", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let mut visual = String::new();
    for positioned in &lines[0].items {
      collect_text(&positioned.item, &mut visual);
    }

    let logical = "\u{202E}x\u{2066}אב\u{2069}y\u{202C}";
    let info = BidiInfo::new(logical, Some(Level::ltr()));
    let para = &info.paragraphs[0];
    let expected: String = info
      .reorder_line(para, para.range.clone())
      .chars()
      .filter(|c| !is_bidi_control(*c))
      .collect();

    assert_eq!(visual, expected);
  }

  #[test]
  fn bidi_override_allows_embed_to_reset_override() {
    // Outer override should force RTL ordering for its direct children, but an inner
    // unicode-bidi: embed establishes a fresh embedding level without the override so its
    // content keeps logical order.
    let mut builder = make_builder(200.0);

    builder
      .add_item(InlineItem::Text(make_text_item("L ", 10.0)))
      .unwrap();

    let mut inner = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Embed,
    );
    inner.add_child(InlineItem::Text(make_text_item("a", 10.0)));
    inner.add_child(InlineItem::Text(make_text_item("b", 10.0)));
    inner.add_child(InlineItem::Text(make_text_item("c", 10.0)));

    let mut outer = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    outer.add_child(InlineItem::Text(make_text_item("X", 10.0)));
    outer.add_child(InlineItem::InlineBox(inner));
    outer.add_child(InlineItem::Text(make_text_item("Y", 10.0)));

    builder.add_item(InlineItem::InlineBox(outer)).unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("R", 10.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);

    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();

    let logical = format!(
      "L {}X{}abc{}Y{}R",
      '\u{202e}', // RLO
      '\u{202a}', // LRE
      '\u{202c}', // PDF
      '\u{202c}'  // PDF
    );
    let expected = reorder_with_controls(&logical, Some(Level::ltr()));
    assert_eq!(actual, expected);
  }

  #[test]
  fn bidi_override_does_not_cross_paragraph_boundary() {
    // An override in the first paragraph should not affect the following paragraph; embeds
    // in later paragraphs should resolve independently.
    let mut builder = make_builder(200.0);

    let mut para1 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    para1.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("B", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("C", 10.0)));
    builder.add_item(InlineItem::InlineBox(para1)).unwrap();
    builder.force_break().unwrap();

    let mut para2 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Embed,
    );
    para2.add_child(InlineItem::Text(make_text_item("XYZ", 30.0)));
    builder.add_item(InlineItem::InlineBox(para2)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let actual_para1: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para1 = reorder_with_controls(
      &format!("{}ABC{}", '\u{202e}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(actual_para1, expected_para1);

    let actual_para2: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para2 = reorder_with_controls(
      &format!("{}XYZ{}", '\u{202a}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(actual_para2, expected_para2);
  }

  #[test]
  fn bidi_override_stops_at_forced_break() {
    // An explicit override without a terminator should not leak across a hard break.
    let mut builder = make_builder(200.0);

    // First paragraph uses an override to reverse ABC.
    let mut para1 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    para1.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("B", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("C", 10.0)));
    builder.add_item(InlineItem::InlineBox(para1)).unwrap();
    builder.force_break().unwrap();

    // Second paragraph is plain LTR.
    builder
      .add_item(InlineItem::Text(make_text_item("DEF", 30.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let para1_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para1 = reorder_with_controls(
      &format!("{}ABC{}", '\u{202e}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(para1_text, expected_para1);

    let para2_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(para2_text, "DEF".to_string());
  }

  #[test]
  fn bidi_override_and_embed_across_paragraphs() {
    // Paragraph 1 uses an override; paragraph 2 uses an embed. Each should reorder independently.
    let mut builder = make_builder(200.0);

    let mut para1 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    para1.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("B", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("C", 10.0)));
    builder.add_item(InlineItem::InlineBox(para1)).unwrap();
    builder.force_break().unwrap();

    let mut para2 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Rtl,
      UnicodeBidi::Embed,
    );
    para2.add_child(InlineItem::Text(make_text_item("XYZ", 30.0)));
    builder.add_item(InlineItem::InlineBox(para2)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let para1_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para1 = reorder_with_controls(
      &format!("{}ABC{}", '\u{202e}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(para1_text, expected_para1);

    let para2_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para2 = reorder_with_controls(
      &format!("{}XYZ{}", '\u{202b}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(para2_text, expected_para2);
  }

  #[test]
  fn bidi_override_mixed_controls_across_paragraphs() {
    // Mixed override/embedding should not leak past a forced break; each paragraph reorders per its controls.
    let mut builder = make_builder(200.0);

    // First paragraph: RLO forces RTL, then an embedded LTR segment.
    let mut para1 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );
    para1.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("B", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item_with_bidi(
      "cd",
      20.0,
      UnicodeBidi::Embed,
    )));
    builder.add_item(InlineItem::InlineBox(para1)).unwrap();
    builder.force_break().unwrap();

    // Second paragraph: plain LTR embed.
    let mut para2 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Embed,
    );
    para2.add_child(InlineItem::Text(make_text_item("EF", 20.0)));
    builder.add_item(InlineItem::InlineBox(para2)).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let para1_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para1 = reorder_with_controls(
      &format!(
        "{}AB{}{}cd{}",
        '\u{202e}', '\u{202c}', '\u{202b}', '\u{202c}'
      ),
      Some(Level::ltr()),
    );
    assert_eq!(para1_text, expected_para1);

    let para2_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para2 = reorder_with_controls(
      &format!("{}EF{}", '\u{202a}', '\u{202c}'),
      Some(Level::ltr()),
    );
    assert_eq!(para2_text, expected_para2);
  }

  #[test]
  fn bidi_isolate_does_not_affect_following_paragraph() {
    // An isolate in the first paragraph should not alter the base direction of the next paragraph.
    let mut builder = make_builder(200.0);

    let mut para1 = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    para1.add_child(InlineItem::Text(make_text_item("A", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("B", 10.0)));
    para1.add_child(InlineItem::Text(make_text_item("C", 10.0)));
    builder.add_item(InlineItem::InlineBox(para1)).unwrap();
    builder.force_break().unwrap();

    builder
      .add_item(InlineItem::Text(make_text_item("DEF", 30.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let para1_text: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    let expected_para1 = reorder_with_controls(
      &format!("{}ABC{}", '\u{2067}', '\u{2069}'),
      Some(Level::ltr()),
    );
    assert_eq!(para1_text, expected_para1);

    let para2_text: String = lines[1]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();
    assert_eq!(para2_text, "DEF".to_string());
  }

  fn nested_inline_box_with_depth(
    depth: usize,
    ub: UnicodeBidi,
    direction: Direction,
    child: InlineItem,
  ) -> InlineItem {
    let mut current = child;
    for idx in 0..depth {
      let mut inline_box = InlineBoxItem::new(
        0.0,
        0.0,
        0.0,
        make_strut_metrics(),
        Arc::new(ComputedStyle::default()),
        idx,
        direction,
        ub,
      );
      inline_box.add_child(current);
      current = InlineItem::InlineBox(inline_box);
    }
    current
  }

  #[test]
  fn bidi_contexts_beyond_max_depth_are_ignored() {
    // Build a chain of isolate boxes deeper than MAX_EXPLICIT_DEPTH and ensure we still reorder safely.
    let mut builder = make_builder_with_base(200.0, Level::ltr());
    let deep = nested_inline_box_with_depth(
      unicode_bidi::level::MAX_EXPLICIT_DEPTH as usize + 5,
      UnicodeBidi::Isolate,
      Direction::Rtl,
      InlineItem::Text(make_text_item("abc", 30.0)),
    );
    builder.add_item(deep).unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let mut collected = String::new();
    for positioned in &lines[0].items {
      collect_text(&positioned.item, &mut collected);
    }
    assert_eq!(collected, "abc");
  }

  fn reorder_with_controls(text: &str, base: Option<Level>) -> String {
    let bidi = BidiInfo::new(text, base);
    let para = &bidi.paragraphs[0];
    bidi
      .reorder_line(para, para.range.clone())
      .chars()
      .filter(|c| {
        !matches!(
          c,
          '\u{202a}' // LRE
                    | '\u{202b}' // RLE
                    | '\u{202c}' // PDF
                    | '\u{202d}' // LRO
                    | '\u{202e}' // RLO
                    | '\u{2066}' // LRI
                    | '\u{2067}' // RLI
                    | '\u{2068}' // FSI
                    | '\u{2069}' // PDI
        )
      })
      .collect()
  }

  fn flatten_text(item: &InlineItem) -> String {
    match item {
      InlineItem::Text(t) => t.text.clone(),
      InlineItem::SoftBreak => "\n".to_string(),
      InlineItem::Tab(_) => "\t".to_string(),
      InlineItem::HardBreak(_) => "\n".to_string(),
      InlineItem::InlineBox(b) => b.children.iter().map(flatten_text).collect(),
      InlineItem::Ruby(r) => r
        .segments
        .iter()
        .map(|seg| seg.base_items.iter().map(flatten_text).collect::<String>())
        .collect(),
      InlineItem::InlineBlock(_)
      | InlineItem::Replaced(_)
      | InlineItem::Floating(_)
      | InlineItem::StaticPositionAnchor(_) => String::from("\u{FFFC}"),
    }
  }

  fn is_bidi_control(c: char) -> bool {
    matches!(
      c,
      '\u{202a}' // LRE
                | '\u{202b}' // RLE
                | '\u{202c}' // PDF
                | '\u{202d}' // LRO
                | '\u{202e}' // RLO
                | '\u{2066}' // LRI
                | '\u{2067}' // RLI
                | '\u{2068}' // FSI
                | '\u{2069}' // PDI
    )
  }

  #[test]
  fn bidi_isolate_spans_children_as_single_context() {
    // Logical text with a single RTL isolate containing both child segments.
    let expected = reorder_with_controls("A \u{2067}XY\u{2069} Z", Some(Level::ltr()));

    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::Text(make_text_item("A ", 20.0)))
      .unwrap();

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("X", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("Y", 10.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    builder
      .add_item(InlineItem::Text(make_text_item(" Z", 20.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();

    assert_eq!(actual, expected);
  }

  #[test]
  fn bidi_isolate_resolves_neutrals_across_children() {
    // Single isolate should resolve neutrals using surrounding strong types inside the isolate.
    let expected = "A באX Z".to_string();

    let mut builder = make_builder(200.0);
    builder
      .add_item(InlineItem::Text(make_text_item("A ", 20.0)))
      .unwrap();

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      0,
      Direction::Rtl,
      UnicodeBidi::Isolate,
    );
    inline_box.add_child(InlineItem::Text(make_text_item("X", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("א", 10.0)));
    inline_box.add_child(InlineItem::Text(make_text_item("ב", 10.0)));
    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();

    builder
      .add_item(InlineItem::Text(make_text_item(" Z", 20.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 1);
    let actual: String = lines[0]
      .items
      .iter()
      .map(|p| flatten_text(&p.item))
      .collect();

    assert_eq!(actual, expected);
  }

  #[test]
  fn bidi_explicit_embedding_survives_line_wrap() {
    // Explicit embedding (RLE) without a terminator should keep later lines at the same level.
    let mut builder = make_builder(40.0);
    let first = "\u{202b}abc ";
    builder
      .add_item(InlineItem::Text(make_text_item(first, 40.0)))
      .unwrap();
    builder
      .add_item(InlineItem::Text(make_text_item("DEF", 30.0)))
      .unwrap();

    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let logical = format!("{}{}", first, "DEF");
    let bidi = BidiInfo::new(&logical, Some(Level::ltr()));
    let para = &bidi.paragraphs[0];
    let line_ranges = [0..first.len(), first.len()..logical.len()];
    let expected: Vec<String> = line_ranges
      .iter()
      .map(|range| {
        bidi
          .reorder_line(para, range.clone())
          .chars()
          .filter(|c| !is_bidi_control(*c))
          .collect()
      })
      .collect();

    let actual: Vec<String> = lines
      .iter()
      .map(|line| {
        line
          .items
          .iter()
          .map(|p| flatten_text(&p.item))
          .collect::<String>()
          .chars()
          .filter(|c| !is_bidi_control(*c))
          .collect()
      })
      .collect();

    assert_eq!(actual, expected);
  }

  #[test]
  fn letter_spacing_increases_advance() {
    let mut style = ComputedStyle::default();
    let text = "abc";
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();

    let base_runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let base_width: f32 = base_runs.iter().map(|r| r.advance).sum();

    style.letter_spacing = 2.0;
    let mut spaced_runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    TextItem::apply_spacing_to_runs(
      &mut spaced_runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );
    let spaced_width: f32 = spaced_runs.iter().map(|r| r.advance).sum();

    let expected_extra = style.letter_spacing * (text.chars().count().saturating_sub(1) as f32);
    assert!((spaced_width - base_width - expected_extra).abs() < 0.01);
  }

  #[test]
  fn letter_spacing_does_not_shift_glyph_offsets() {
    let text = "abc";
    let mut runs = vec![make_synthetic_run(text, 10.0)];
    let base_positions = glyph_x_positions(&runs);

    TextItem::apply_spacing_to_runs(&mut runs, text, 2.0, 0.0);
    let spaced_positions = glyph_x_positions(&runs);

    assert_eq!(base_positions.len(), 3);
    assert_eq!(spaced_positions.len(), 3);
    assert_eq!(spaced_positions[0], base_positions[0]);
    assert!(
      (spaced_positions[1] - base_positions[1] - 2.0).abs() < f32::EPSILON,
      "expected second glyph shift to equal letter-spacing"
    );
    assert!(
      (spaced_positions[2] - base_positions[2] - 4.0).abs() < f32::EPSILON,
      "expected third glyph shift to equal 2 * letter-spacing"
    );

    assert!(
      runs[0].glyphs.iter().all(|g| g.x_offset == 0.0),
      "spacing should not rewrite glyph offsets; only advances should change"
    );
  }

  #[test]
  fn justification_does_not_shift_glyph_offsets() {
    let text = "a b";
    let runs = vec![make_synthetic_run(text, 10.0)];
    let mut item = TextItem::new(
      runs,
      text.to_string(),
      make_strut_metrics(),
      find_break_opportunities(text),
      Vec::new(),
      Arc::new(ComputedStyle::default()),
      Direction::Ltr,
    );
    let base_positions = glyph_x_positions(&item.runs);

    let count = item.apply_inter_word_justification(2.0);
    let justified_positions = glyph_x_positions(&item.runs);

    assert_eq!(count, 1);
    assert_eq!(base_positions.len(), 3);
    assert_eq!(justified_positions.len(), 3);
    assert_eq!(justified_positions[0], base_positions[0]);
    assert_eq!(justified_positions[1], base_positions[1]);
    assert!(
      (justified_positions[2] - base_positions[2] - 2.0).abs() < f32::EPSILON,
      "expected last glyph to shift by exactly the justification delta"
    );

    assert!(
      item.runs[0].glyphs.iter().all(|g| g.x_offset == 0.0),
      "justification should not rewrite glyph offsets; only advances should change"
    );
  }

  #[test]
  fn word_spacing_applies_to_spaces() {
    let mut style = ComputedStyle::default();
    style.letter_spacing = 1.0;
    style.word_spacing = 3.0;
    let text = "a b";
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();

    let base_runs = pipeline
      .shape_with_direction(
        text,
        &ComputedStyle::default(),
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let base_width: f32 = base_runs.iter().map(|r| r.advance).sum();

    let mut spaced_runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    TextItem::apply_spacing_to_runs(
      &mut spaced_runs,
      text,
      style.letter_spacing,
      style.word_spacing,
    );
    let spaced_width: f32 = spaced_runs.iter().map(|r| r.advance).sum();

    let char_gaps = text.chars().count().saturating_sub(1) as f32;
    let space_count = text
      .chars()
      .filter(|c| matches!(c, ' ' | '\u{00A0}' | '\t'))
      .count() as f32;
    let expected_extra = style.letter_spacing * char_gaps + style.word_spacing * space_count;

    assert!((spaced_width - base_width - expected_extra).abs() < 0.01);
  }

  #[test]
  fn apply_spacing_long_ltr_paragraph_does_not_sort() {
    let text = "The quick brown fox jumps over the lazy dog. "
      .repeat(256)
      .trim_end()
      .to_string();
    let letter_spacing = 1.25;
    let word_spacing = 0.75;

    let mut runs = vec![make_synthetic_run(&text, 10.0)];
    let base_width: f32 = runs.iter().map(|r| r.advance).sum();

    reset_apply_spacing_diagnostics();
    TextItem::apply_spacing_to_runs(&mut runs, &text, letter_spacing, word_spacing);
    let (sorts, clusters, streams) = take_apply_spacing_diagnostics();

    assert_eq!(sorts, 0, "expected monotonic LTR clusters to skip sorting");
    assert_eq!(
      streams, 1,
      "expected pure LTR runs to take the streaming no-allocation path"
    );
    assert_eq!(
      clusters,
      text.chars().count(),
      "expected one synthetic cluster per char"
    );

    let spaced_width: f32 = runs.iter().map(|r| r.advance).sum();
    let gap_count = text.chars().count().saturating_sub(1) as f32;
    let space_count = text
      .chars()
      .filter(|c| matches!(c, ' ' | '\u{00A0}' | '\t'))
      .count() as f32;
    let expected_extra = letter_spacing * gap_count + word_spacing * space_count;
    assert!(
      (spaced_width - base_width - expected_extra).abs() < 0.1,
      "expected total advance to increase by computed letter+word spacing"
    );
  }

  #[test]
  fn apply_spacing_mixed_direction_triggers_sort_and_updates_advance() {
    // Force a non-monotonic cluster discovery order so the fallback path has to perform a real
    // cluster sort (not just a reverse). This can happen with complex scripts or unexpected shaping
    // output; we simulate it by scrambling the RTL run's glyph order.
    let text = "abc אבג";
    let ltr_end = text.find('א').expect("has rtl char boundary");
    let (ltr_text, rtl_text) = text.split_at(ltr_end);

    let ltr_run =
      make_synthetic_run_with_byte_clusters(ltr_text, 0, 10.0, PipelineDirection::LeftToRight);
    let mut rtl_run = make_synthetic_run_with_byte_clusters(
      rtl_text,
      ltr_end,
      10.0,
      PipelineDirection::RightToLeft,
    );
    // HarfBuzz usually emits clusters monotonic within a run, but for this unit test we want a
    // non-monotonic sequence that cannot be fixed just by scanning from the opposite end.
    rtl_run.glyphs.rotate_right(1);

    let mut runs = vec![rtl_run, ltr_run];
    let base_width: f32 = runs.iter().map(|r| r.advance).sum();

    reset_apply_spacing_diagnostics();
    TextItem::apply_spacing_to_runs(&mut runs, text, 2.0, 3.0);
    let (sorts, clusters, streams) = take_apply_spacing_diagnostics();

    assert_eq!(
      sorts, 1,
      "expected mixed-direction / out-of-order runs to sort"
    );
    assert_eq!(streams, 0, "expected mixed-direction input to avoid streaming fast path");
    assert_eq!(clusters, text.chars().count());

    let spaced_width: f32 = runs.iter().map(|r| r.advance).sum();
    let gap_count = text.chars().count().saturating_sub(1) as f32;
    let space_count = text
      .chars()
      .filter(|c| matches!(c, ' ' | '\u{00A0}' | '\t'))
      .count() as f32;
    let expected_extra = 2.0 * gap_count + 3.0 * space_count;
    assert!(
      (spaced_width - base_width - expected_extra).abs() < 0.1,
      "expected spacing to apply in logical order across runs"
    );
  }

  #[test]
  fn apply_spacing_rtl_monotonic_decreasing_avoids_sorting() {
    // Simulate RTL shaping output where glyphs are in visual order but cluster offsets are
    // monotonic-decreasing.
    // Include a *leading* space so that if we accidentally skip reversing/sorting, we would treat
    // that space as the final cluster and incorrectly suppress word-spacing.
    let text = " אב";
    let mut run = make_synthetic_run_with_byte_clusters(
      text,
      0,
      10.0,
      PipelineDirection::RightToLeft,
    );
    run.glyphs.reverse();
    let base_width = run.advance;
    let mut runs = vec![run];

    reset_apply_spacing_diagnostics();
    TextItem::apply_spacing_to_runs(&mut runs, text, 2.0, 3.0);
    let (sorts, clusters, streams) = take_apply_spacing_diagnostics();

    assert_eq!(sorts, 0, "expected monotonic RTL clusters to avoid sorting");
    assert_eq!(streams, 0, "expected RTL runs to avoid the LTR streaming fast path");
    assert_eq!(clusters, text.chars().count());

    let spaced_width: f32 = runs.iter().map(|r| r.advance).sum();
    let gap_count = text.chars().count().saturating_sub(1) as f32;
    let space_count = text
      .chars()
      .filter(|c| matches!(c, ' ' | '\u{00A0}' | '\t'))
      .count() as f32;
    let expected_extra = 2.0 * gap_count + 3.0 * space_count;
    assert!(
      (spaced_width - base_width - expected_extra).abs() < 0.1,
      "expected spacing to apply after each logical cluster (except the final one)"
    );
  }

  #[test]
  fn split_at_can_insert_hyphen() {
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();
    let style = Arc::new(ComputedStyle::default());

    let runs = pipeline
      .shape_with_direction(
        "abc",
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let metrics = TextItem::metrics_from_runs(&font_ctx, &runs, 16.0, style.font_size);
    let breaks = vec![BreakOpportunity::with_hyphen(1, BreakType::Allowed, true)];
    let item = TextItem::new(
      runs,
      "abc".to_string(),
      metrics,
      breaks,
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let mut cache = ReshapeCache::default();
    let (before, after) = item
      .split_at(1, true, &pipeline, &font_ctx, &mut cache)
      .expect("split succeeds");

    assert_eq!(before.text, format!("a{}", '\u{2010}'));
    assert_eq!(after.text, "bc");
    let combined = before.text.replace('\u{2010}', "") + &after.text;
    assert_eq!(combined, "abc");
  }

  #[test]
  fn split_at_round_trips_text() {
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();
    let style = Arc::new(ComputedStyle::default());
    let text = "wrap this 😊 text";

    let runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let metrics = TextItem::metrics_from_runs(&font_ctx, &runs, 16.0, style.font_size);
    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      find_break_opportunities(text),
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let split_offset = text.find('😊').unwrap();
    let mut cache = ReshapeCache::default();
    let (before, after) = item
      .split_at(split_offset, false, &pipeline, &font_ctx, &mut cache)
      .expect("split succeeds");

    assert_eq!(before.text + &after.text, text);
  }

  #[test]
  fn split_at_rejects_non_char_boundary_offsets() {
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();
    let style = Arc::new(ComputedStyle::default());

    let text = "a😊b";
    let runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let metrics = TextItem::metrics_from_runs(&font_ctx, &runs, 16.0, style.font_size);
    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      Vec::new(),
      Vec::new(),
      style.clone(),
      Direction::Ltr,
    );

    // Offset 2 lands inside the multi-byte emoji; split_at should clamp to the previous
    // boundary rather than panicking.
    assert!(!text.is_char_boundary(2));
    let mut cache = ReshapeCache::default();
    let (before, after) = item
      .split_at(2, false, &pipeline, &font_ctx, &mut cache)
      .expect("clamps to prior boundary");
    assert_eq!(before.text, "a");
    assert_eq!(after.text, "😊b");

    // Explicit break offsets should also be rejected when they land mid-codepoint.
    let euro_text = "€uro";
    let euro_runs = pipeline
      .shape_with_direction(
        euro_text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let euro_metrics = TextItem::metrics_from_runs(&font_ctx, &euro_runs, 16.0, style.font_size);
    let breaks = vec![BreakOpportunity::new(1, BreakType::Allowed)];
    let euro_item = TextItem::new(
      euro_runs,
      euro_text.to_string(),
      euro_metrics,
      breaks,
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let mut euro_cache = ReshapeCache::default();
    assert!(euro_item
      .split_at(1, false, &pipeline, &font_ctx, &mut euro_cache)
      .is_none());
  }

  #[test]
  fn previous_char_boundary_handles_multibyte_offsets() {
    let text = "a😊b";
    // Offsets that land in the middle of a multibyte codepoint should step back to its start.
    assert_eq!(TextItem::previous_char_boundary_in_text(text, 2), 1);
    assert_eq!(TextItem::previous_char_boundary_in_text(text, 4), 1);

    // Offsets at boundaries remain unchanged.
    assert_eq!(TextItem::previous_char_boundary_in_text(text, 0), 0);
    assert_eq!(TextItem::previous_char_boundary_in_text(text, 1), 1);
    assert_eq!(TextItem::previous_char_boundary_in_text(text, 5), 5);
  }

  #[test]
  fn previous_char_boundary_clamps_past_end() {
    let text = "éclair";
    assert_eq!(
      TextItem::previous_char_boundary_in_text(text, text.len() + 10),
      text.len()
    );
  }

  #[test]
  fn split_at_handles_non_char_boundary_offsets() {
    let font_ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();
    let style = Arc::new(ComputedStyle::default());
    let text = "‘bruises and blood’ in ‘Christy’ fights";

    let runs = pipeline
      .shape_with_direction(
        text,
        &style,
        &font_ctx,
        pipeline_dir_from_style(Direction::Ltr),
      )
      .expect("shape");
    let metrics = TextItem::metrics_from_runs(&font_ctx, &runs, 16.0, style.font_size);
    let item = TextItem::new(
      runs,
      text.to_string(),
      metrics,
      Vec::new(),
      Vec::new(),
      style,
      Direction::Ltr,
    );

    let mut cache = ReshapeCache::default();
    let (before, after) = item
      .split_at(22, false, &pipeline, &font_ctx, &mut cache)
      .expect("split succeeds");

    assert_eq!(format!("{}{}", before.text, after.text), text);
    assert!(after.text.starts_with('’'));
  }

  #[test]
  fn inline_box_wrap_preserves_fragments_and_slice_edges() {
    // Force the inline box to wrap across multiple lines so we can assert that each line contains
    // an inline-box fragment rather than flattened children.
    let mut builder = make_builder(15.0);

    let mut inline_box = InlineBoxItem::new(
      2.0,
      3.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    inline_box.box_id = 42;
    for _ in 0..8 {
      inline_box.add_child(InlineItem::Text(make_text_item("x", 4.0)));
    }

    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();
    let result = builder.finish().unwrap();
    let lines = result.lines;
    assert_eq!(lines.len(), 3);

    let fragments: Vec<InlineBoxItem> = lines
      .iter()
      .map(|line| {
        assert_eq!(line.items.len(), 1);
        match &line.items[0].item {
          InlineItem::InlineBox(b) => b.clone(),
          other => panic!("expected inline box fragment, got {other:?}"),
        }
      })
      .collect();

    assert_eq!(fragments[0].box_id, 42);
    assert!((fragments[0].start_edge - 2.0).abs() < f32::EPSILON);
    assert_eq!(fragments[0].end_edge, 0.0);

    assert_eq!(fragments[1].start_edge, 0.0);
    assert_eq!(fragments[1].end_edge, 0.0);

    assert_eq!(fragments[2].start_edge, 0.0);
    assert!((fragments[2].end_edge - 3.0).abs() < f32::EPSILON);
  }

  #[test]
  fn inline_box_paint_style_uses_fragment_edges() {
    let mut style = ComputedStyle::default();
    style.padding_left = crate::style::values::Length::px(5.0);
    style.padding_right = crate::style::values::Length::px(6.0);
    style.border_left_width = crate::style::values::Length::px(2.0);
    style.border_right_width = crate::style::values::Length::px(3.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    let style = Arc::new(style);

    // Simulate a continuation fragment: layout has removed the left/right edges, but the authored
    // style still has border/padding. The paint style must follow the fragment edges so borders do
    // not overlap content.
    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      style,
      1,
      Direction::Ltr,
      UnicodeBidi::Normal,
    );
    inline_box.border_left = 0.0;
    inline_box.border_right = 0.0;

    let paint_style = inline_box.paint_style();
    assert_eq!(
      paint_style.padding_left,
      crate::style::values::Length::px(0.0)
    );
    assert_eq!(
      paint_style.padding_right,
      crate::style::values::Length::px(0.0)
    );
    assert_eq!(
      paint_style.border_left_width,
      crate::style::values::Length::px(0.0)
    );
    assert_eq!(
      paint_style.border_right_width,
      crate::style::values::Length::px(0.0)
    );
  }

  #[test]
  fn bidi_override_inline_box_wrap_keeps_scope_across_fragments() {
    // Regression: when an inline box wrapped, LineBuilder used to flatten it, dropping the
    // unicode-bidi scope. That meant the bidi override was not applied on subsequent lines.
    let mut builder = make_builder(30.0);

    let mut inline_box = InlineBoxItem::new(
      0.0,
      0.0,
      0.0,
      make_strut_metrics(),
      Arc::new(ComputedStyle::default()),
      1,
      Direction::Rtl,
      UnicodeBidi::BidiOverride,
    );

    for ch in ["a", "b", "c", "d", "e", "f"] {
      inline_box.add_child(InlineItem::Text(make_text_item(ch, 10.0)));
    }

    builder.add_item(InlineItem::InlineBox(inline_box)).unwrap();
    let lines = builder.finish().unwrap().lines;
    assert_eq!(lines.len(), 2);

    let line_texts: Vec<String> = lines
      .iter()
      .map(|line| {
        assert_eq!(line.items.len(), 1);
        let InlineItem::InlineBox(b) = &line.items[0].item else {
          panic!("expected inline box fragment");
        };
        b.children
          .iter()
          .filter_map(|child| match child {
            InlineItem::Text(t) => Some(t.text.clone()),
            _ => None,
          })
          .collect::<String>()
      })
      .collect();

    // Each line fragment should have the override applied, reversing the visual order of the LTR
    // text children within the RTL override scope.
    assert_eq!(line_texts, vec!["cba".to_string(), "fed".to_string()]);
  }
}
