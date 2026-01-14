//! Baseline alignment for inline layout
//!
//! This module implements baseline alignment for inline formatting context.
//! Baseline alignment determines how inline-level boxes are vertically positioned
//! within a line box.
//!
//! # CSS Specification
//!
//! CSS 2.1 Section 10.8 - Line height calculations:
//! <https://www.w3.org/TR/CSS21/visudet.html#line-height>
//!
//! # Vertical Alignment
//!
//! The `vertical-align` property affects inline-level boxes in several ways:
//!
//! - **baseline**: Align box baseline with parent baseline (default)
//! - **middle**: Align box vertical center with parent baseline + half x-height
//! - **sub**: Lower box baseline to parent subscript position
//! - **super**: Raise box baseline to parent superscript position
//! - **text-top**: Align box top with parent's text top
//! - **text-bottom**: Align box bottom with parent's text bottom
//! - **top**: Align box top with line box top
//! - **bottom**: Align box bottom with line box bottom
//! - **length**: Raise/lower by specified amount
//! - **percentage**: Raise/lower by percentage of line-height
//!
//! # Baseline Types
//!
//! Different boxes have different baselines:
//!
//! - **Text**: Font baseline (typically ~80% from top)
//! - **Inline boxes**: Baseline of first text child
//! - **Inline-block**: Bottom margin edge (or content baseline if has in-flow content)
//! - **Replaced elements**: Bottom margin edge

use crate::geometry::Size;
use crate::style::ComputedStyle;
use crate::text::font_db::FontMetrics;
use crate::text::root_font_metrics::RootFontMetrics;

/// Vertical alignment modes for inline elements
///
/// Corresponds to CSS `vertical-align` property values.
/// Reference: CSS 2.1 Section 10.8.1
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum VerticalAlign {
  /// Align baseline with parent's baseline (default)
  #[default]
  Baseline,

  /// Align vertical center with parent's baseline + half x-height
  Middle,

  /// Lower baseline to parent's subscript position
  Sub,

  /// Raise baseline to parent's superscript position
  Super,

  /// Align top with parent's text top
  TextTop,

  /// Align bottom with parent's text bottom
  TextBottom,

  /// Align top with line box top
  Top,

  /// Align bottom with line box bottom
  Bottom,

  /// Raise/lower by specific length (positive = up)
  Length(f32),

  /// Raise/lower by percentage of line-height
  Percentage(f32),
}

impl VerticalAlign {
  /// Returns true if this alignment is relative to the line box (not the parent)
  pub fn is_line_relative(&self) -> bool {
    matches!(self, VerticalAlign::Top | VerticalAlign::Bottom)
  }

  /// Returns true if this alignment uses the parent's baseline
  pub fn is_baseline_relative(&self) -> bool {
    matches!(
      self,
      VerticalAlign::Baseline
        | VerticalAlign::Middle
        | VerticalAlign::Sub
        | VerticalAlign::Super
        | VerticalAlign::TextTop
        | VerticalAlign::TextBottom
        | VerticalAlign::Length(_)
        | VerticalAlign::Percentage(_)
    )
  }
}

/// Metrics for baseline calculation
///
/// Contains all measurements needed to compute baseline positioning
/// for an inline item within a line box.
#[derive(Debug, Clone, Copy)]
pub struct BaselineMetrics {
  /// Distance from top of box to baseline
  pub baseline_offset: f32,

  /// Total height of the inline box
  pub height: f32,

  /// Font ascent (above baseline)
  pub ascent: f32,

  /// Font descent (below baseline, positive value)
  pub descent: f32,

  /// Line gap (extra spacing)
  pub line_gap: f32,

  /// Line height from CSS (may differ from ascent + descent)
  pub line_height: f32,

  /// Font x-height if available (used for middle alignment)
  pub x_height: Option<f32>,
}

impl BaselineMetrics {
  /// Creates metrics from font metrics and font size
  pub fn from_font_metrics(metrics: &FontMetrics, font_size: f32, line_height: f32) -> Self {
    let scaled = metrics.scale(font_size);
    // CSS 2.1 §10.8: distribute leading equally above and below the em box.
    // The baseline position within an inline box is ascent + half-leading.
    let half_leading = (line_height - (scaled.ascent + scaled.descent)) / 2.0;
    Self {
      baseline_offset: scaled.ascent + half_leading,
      height: line_height,
      ascent: scaled.ascent,
      descent: scaled.descent,
      line_gap: scaled.line_gap,
      line_height,
      x_height: scaled.x_height,
    }
  }

  /// Creates metrics for a replaced element (image, etc.)
  ///
  /// Replaced elements have their baseline at the bottom margin edge.
  pub fn for_replaced(height: f32) -> Self {
    Self {
      baseline_offset: height,
      height,
      ascent: height,
      descent: 0.0,
      line_gap: 0.0,
      line_height: height,
      x_height: None,
    }
  }

  /// Creates metrics for text with explicit values
  pub fn new(baseline_offset: f32, height: f32, ascent: f32, descent: f32) -> Self {
    Self {
      baseline_offset,
      height,
      ascent,
      descent,
      line_gap: 0.0,
      line_height: height,
      // When true font metrics are unavailable, approximate x-height as half the ascent so
      // vertical-align: middle still has a reasonable fallback.
      x_height: Some(ascent * 0.5),
    }
  }

  /// Half-leading above the text content
  ///
  /// Leading is the difference between line-height and the actual content height.
  /// It's split equally above and below.
  pub fn half_leading(&self) -> f32 {
    (self.line_height - (self.ascent + self.descent)) / 2.0
  }
}

/// Accumulated baseline information for a line
///
/// Tracks the maximum ascent and descent across all items in a line
/// to compute the final line height and baseline position.
#[derive(Debug, Clone, Default)]
pub struct LineBaselineAccumulator {
  /// Maximum ascent (distance above baseline)
  pub max_ascent: f32,

  /// Maximum descent (distance below baseline)
  pub max_descent: f32,

  /// Items with vertical-align: top/bottom need separate tracking
  pub top_aligned_height: f32,
  pub bottom_aligned_height: f32,

  /// Strut height (minimum from root inline box)
  pub strut_ascent: f32,
  pub strut_descent: f32,

  /// Whether any inline-level items have contributed to this line.
  ///
  /// The CSS2.1 line box algorithm determines line height from the inline-level boxes on that
  /// line, including the "strut" (root inline box metrics) which acts as a minimum ascent/descent
  /// even when the line contains other items.
  ///
  /// This flag is only used to detect truly empty lines (e.g. a line produced by `<br>` with no
  /// fragments) so we can size the line box from the strut alone.
  pub has_items: bool,
}

impl LineBaselineAccumulator {
  /// Creates a new accumulator with strut from the root inline box
  ///
  /// The strut is an imaginary zero-width inline box with the element's font and line height.
  /// It provides a minimum height for the line box even if it's empty.
  pub fn new(strut_metrics: &BaselineMetrics) -> Self {
    let half_leading = strut_metrics.half_leading();
    let strut_ascent = strut_metrics.ascent + half_leading;
    let strut_descent = strut_metrics.descent + half_leading;

    Self {
      // Seed baseline metrics with the "strut" (CSS 2.1 §10.8). This ensures inline content like
      // baseline-aligned replaced elements (`<img>`) still reserves the font's descender/leading,
      // matching browser behavior (the classic "inline images have a small gap underneath").
      //
      // The strut does **not** affect the baseline when a taller box is present (e.g. an image);
      // it only contributes when its ascent/descent exceed the line's existing extrema.
      max_ascent: strut_ascent,
      max_descent: strut_descent,
      top_aligned_height: 0.0,
      bottom_aligned_height: 0.0,
      strut_ascent,
      strut_descent,
      has_items: false,
    }
  }

  /// Creates an accumulator with default strut values
  pub fn with_default_strut(font_size: f32, line_height: f32) -> Self {
    // Approximate standard font metrics
    let ascent = font_size * 0.8;
    let descent = font_size * 0.2;
    let half_leading = (line_height - font_size) / 2.0;
    let strut_ascent = ascent + half_leading;
    let strut_descent = descent + half_leading;

    Self {
      max_ascent: strut_ascent,
      max_descent: strut_descent,
      top_aligned_height: 0.0,
      bottom_aligned_height: 0.0,
      strut_ascent,
      strut_descent,
      has_items: false,
    }
  }

  /// Adds an item to the accumulator with baseline-relative alignment
  ///
  /// Returns the Y offset for the item relative to the line's baseline.
  pub fn add_baseline_relative(
    &mut self,
    metrics: &BaselineMetrics,
    alignment: VerticalAlign,
    parent_metrics: Option<&BaselineMetrics>,
  ) -> f32 {
    self.has_items = true;
    // The returned shift is a **Y offset from the line baseline**, where positive values move the
    // item *down* (CSS coordinate system: y grows downward). This matches how inline layout later
    // positions fragments:
    //
    //   item_top_y = line_baseline_y + shift - item.baseline_offset
    //
    // For authored `vertical-align: <length>`, CSS defines positive lengths as "raise" (move up),
    // so `compute_baseline_shift` negates those values to convert them to our y-down offset.
    let baseline_shift = self.compute_baseline_shift(alignment, metrics, parent_metrics);

    // Compute this item's contribution to ascent/descent
    // With `item_top = baseline + shift - baseline_offset`:
    // - Ascent (distance above baseline) = baseline - item_top = baseline_offset - shift
    // - Descent (distance below baseline) = item_bottom - baseline
    //   = (item_top + height) - baseline = (height - baseline_offset) + shift
    let item_ascent = metrics.baseline_offset - baseline_shift;
    let item_descent = (metrics.height - metrics.baseline_offset) + baseline_shift;

    self.max_ascent = self.max_ascent.max(item_ascent);
    self.max_descent = self.max_descent.max(item_descent);

    baseline_shift
  }

  /// Adds a line-relative (top/bottom) aligned item
  ///
  /// These items don't affect the baseline calculation but may extend
  /// the line box height.
  pub fn add_line_relative(&mut self, metrics: &BaselineMetrics, alignment: VerticalAlign) {
    self.has_items = true;
    match alignment {
      VerticalAlign::Top => {
        self.top_aligned_height = self.top_aligned_height.max(metrics.height);
      }
      VerticalAlign::Bottom => {
        self.bottom_aligned_height = self.bottom_aligned_height.max(metrics.height);
      }
      _ => {}
    }
  }

  /// Computes the baseline shift for an alignment mode.
  ///
  /// The returned shift is a Y offset from the line baseline where positive values move the item
  /// *down* (CSS coordinate system).
  ///
  /// This is exposed to sibling modules (e.g. the line builder) so they can apply spec-accurate
  /// baseline shifts while still accounting for inline subtree bounds when computing the line box
  /// height (CSS 2.1 §10.8.1 / "Leading and half-leading").
  pub(crate) fn compute_baseline_shift(
    &self,
    alignment: VerticalAlign,
    metrics: &BaselineMetrics,
    parent_metrics: Option<&BaselineMetrics>,
  ) -> f32 {
    match alignment {
      VerticalAlign::Baseline => 0.0,

      VerticalAlign::Middle => {
        // Align the vertical midpoint of the box with the parent's baseline plus half the
        // parent's x-height (CSS 2.1 §10.8.1).
        //
        // The x-height is measured *above* the baseline, so the target point is
        // `-x_height/2` in the line's coordinate system.
        let x_height_half = parent_metrics
          // Blink/FreeType effectively snaps the x-height metric to whole CSS pixels at common
          // sizes due to font hinting. Using the raw float-scaled metric can accumulate into
          // noticeable vertical drift on text-heavy pages that rely on `vertical-align: middle`
          // (e.g. lobste.rs bylines with avatar images).
          .and_then(|m| m.x_height.map(|xh| xh.ceil() * 0.5))
          .or_else(|| parent_metrics.map(|m| m.ascent * 0.5))
          .unwrap_or(0.0);
        // `vertical-align` aligns the inline box itself, not its margins. `BaselineMetrics.height`
        // includes vertical margins for atomic inline boxes so they participate in line box
        // sizing, but the midpoint for `middle` should be computed from the border-box height.
        // Use `line_height` as the border-box proxy (see callers that preserve legacy
        // `vertical-align:<percentage>` semantics by setting it to the border-box height).
        metrics.baseline_offset - (metrics.line_height * 0.5) - x_height_half
      }

      VerticalAlign::Sub => {
        // Lower baseline by ~0.3em (typical subscript offset)
        let shift = parent_metrics
          .map(|m| m.ascent * 0.3)
          .unwrap_or(metrics.ascent * 0.3);
        shift
      }

      VerticalAlign::Super => {
        // Raise baseline by ~0.3em (typical superscript offset).
        //
        // Note: This is intentionally conservative. Footnote call markers use the UA default
        // `vertical-align: super`; if we raise too aggressively, tight `line-height` values can
        // balloon the line box and unexpectedly force pagination breaks.
        let shift = parent_metrics
          .map(|m| m.ascent * 0.3)
          .unwrap_or(metrics.ascent * 0.3);
        -shift
      }

      VerticalAlign::TextTop => {
        // Align top with parent's text top
        if let Some(parent) = parent_metrics {
          metrics.baseline_offset - parent.ascent
        } else {
          0.0
        }
      }

      VerticalAlign::TextBottom => {
        // Align bottom with parent's text bottom
        if let Some(parent) = parent_metrics {
          parent.descent - (metrics.height - metrics.baseline_offset)
        } else {
          0.0
        }
      }

      // `vertical-align: <length>`: positive is "raise" (move up), so negate to convert to
      // our y-down offset.
      VerticalAlign::Length(len) => -len,

      VerticalAlign::Percentage(pct) => {
        // Percentage of line-height; positive is "raise" (move up).
        -(metrics.line_height * (pct / 100.0))
      }

      // Top/Bottom are handled separately
      VerticalAlign::Top | VerticalAlign::Bottom => 0.0,
    }
  }

  /// Computes the final line height
  pub fn line_height(&self) -> f32 {
    if !self.has_items {
      return self.strut_ascent + self.strut_descent;
    }
    let baseline_height = self.max_ascent + self.max_descent;
    let top_bottom_height = self.top_aligned_height.max(self.bottom_aligned_height);
    baseline_height.max(top_bottom_height)
  }

  /// Computes the baseline position from the top of the line box
  pub fn baseline_position(&self) -> f32 {
    if !self.has_items {
      return self.strut_ascent;
    }
    self.max_ascent
  }

  /// Computes the Y offset for a top-aligned item
  pub fn top_aligned_offset(&self) -> f32 {
    0.0
  }

  /// Computes the Y offset for a bottom-aligned item from its top edge
  pub fn bottom_aligned_offset(&self, item_height: f32) -> f32 {
    self.line_height() - item_height
  }
}

/// Compute line height from CSS line-height value and font size
///
/// Supports CSS line-height values:
/// - `normal`: Use font's default line spacing (typically 1.2x font-size)
/// - `<number>`: Multiply font-size by the number
/// - `<length>`: Use the absolute value
/// - `<percentage>`: Multiply font-size by percentage/100
pub fn compute_line_height(style: &ComputedStyle) -> f32 {
  compute_line_height_with_metrics(style, None)
}

/// Computes line height, optionally using scaled font metrics and viewport size when available.
pub fn compute_line_height_with_metrics_viewport(
  style: &ComputedStyle,
  metrics: Option<&crate::text::font_db::ScaledMetrics>,
  viewport: Option<Size>,
  root_metrics: Option<RootFontMetrics>,
) -> f32 {
  use crate::style::types::LineHeight;
  use crate::style::values::LengthUnit;

  let font_size = style.font_size;
  let normal_line_height = metrics.map(|m| m.line_height).unwrap_or(font_size * 1.2);
  let (vw, vh) = viewport
    .and_then(|s| {
      if s.width.is_finite() && s.height.is_finite() {
        Some((s.width, s.height))
      } else {
        None
      }
    })
    .unwrap_or((1200.0, 800.0));

  match &style.line_height {
    LineHeight::Normal => normal_line_height,
    LineHeight::Number(n) => font_size * n,
    LineHeight::Length(len) => match len.unit {
      u if u.is_absolute() => len.to_px(),
      LengthUnit::Em => len.value * font_size,
      LengthUnit::Rem => len.value * style.root_font_size,
      LengthUnit::Ex => {
        let x_height = metrics.and_then(|m| m.x_height).unwrap_or(font_size * 0.5);
        len.value * x_height
      }
      LengthUnit::Ch => len.value * font_size * 0.5,
      LengthUnit::Cap => {
        let cap_height = metrics
          .and_then(|m| m.cap_height)
          .unwrap_or(font_size * 0.7);
        len.value * cap_height
      }
      LengthUnit::Ic => len.value * font_size,
      LengthUnit::Rex => {
        len.value
          * root_metrics
            .map(|m| m.root_x_height_px)
            .unwrap_or(style.root_font_size * 0.5)
      }
      LengthUnit::Rch => {
        len.value
          * root_metrics
            .map(|m| m.root_ch_advance_px)
            .unwrap_or(style.root_font_size * 0.5)
      }
      LengthUnit::Rcap => {
        len.value
          * root_metrics
            .map(|m| m.root_cap_height_px)
            .unwrap_or(style.root_font_size * 0.7)
      }
      LengthUnit::Ric => {
        len.value
          * root_metrics
            .map(|m| m.root_ic_advance_px)
            .unwrap_or(style.root_font_size)
      }
      LengthUnit::Rlh => {
        len.value
          * root_metrics
            .map(|m| m.root_used_line_height_px)
            .unwrap_or(style.root_font_size * 1.2)
      }
      // `lh` inside the `line-height` property is cyclic; approximate it using the UA's
      // `normal` line height.
      LengthUnit::Lh => len.value * normal_line_height,
      LengthUnit::Calc => len
        .calc
        .and_then(|calc| {
          crate::style::values::resolve_length_calc_with_resolver(
            calc,
            Some(font_size),
            vw,
            vh,
            font_size,
            style.root_font_size,
            &|linear, base, vw, vh, font_px, root_px| {
              let base = base.filter(|b| b.is_finite());
              let mut resolved = 0.0;
              for term in linear.terms() {
                resolved += match term.unit {
                  LengthUnit::Percent => base.map(|b| (term.value / 100.0) * b),
                  u if u.is_absolute() => {
                    Some(crate::style::values::Length::new(term.value, u).to_px())
                  }
                  LengthUnit::Em => Some(term.value * font_px),
                  LengthUnit::Rem => Some(term.value * root_px),
                  LengthUnit::Ex => {
                    let x_height = metrics.and_then(|m| m.x_height).unwrap_or(font_px * 0.5);
                    Some(term.value * x_height)
                  }
                  LengthUnit::Ch => Some(term.value * font_px * 0.5),
                  LengthUnit::Cap => {
                    let cap_height = metrics.and_then(|m| m.cap_height).unwrap_or(font_px * 0.7);
                    Some(term.value * cap_height)
                  }
                  LengthUnit::Ic => Some(term.value * font_px),
                  LengthUnit::Rex => Some(
                    term.value
                      * root_metrics
                        .map(|m| m.root_x_height_px)
                        .unwrap_or(root_px * 0.5),
                  ),
                  LengthUnit::Rch => Some(
                    term.value
                      * root_metrics
                        .map(|m| m.root_ch_advance_px)
                        .unwrap_or(root_px * 0.5),
                  ),
                  LengthUnit::Rcap => Some(
                    term.value
                      * root_metrics
                        .map(|m| m.root_cap_height_px)
                        .unwrap_or(root_px * 0.7),
                  ),
                  LengthUnit::Ric => Some(
                    term.value
                      * root_metrics
                        .map(|m| m.root_ic_advance_px)
                        .unwrap_or(root_px),
                  ),
                  LengthUnit::Rlh => Some(
                    term.value
                      * root_metrics
                        .map(|m| m.root_used_line_height_px)
                        .unwrap_or(root_px * 1.2),
                  ),
                  // `lh` inside `line-height` is cyclic; approximate it using the UA `normal` line height.
                  LengthUnit::Lh => Some(term.value * normal_line_height),
                  u if u.is_viewport_relative() => crate::style::values::Length::new(term.value, u)
                    .resolve_with_viewport_for_writing_mode(vw, vh, style.writing_mode),
                  _ => Some(term.value),
                }?;
              }
              Some(resolved)
            },
          )
        })
        .unwrap_or(len.value),
      u if u.is_viewport_relative() => len
        .resolve_with_viewport_for_writing_mode(vw, vh, style.writing_mode)
        .unwrap_or(len.value),
      _ => len.value,
    },
    LineHeight::Percentage(pct) => font_size * (pct / 100.0),
  }
}

/// Computes line height, optionally using scaled font metrics when available.
pub fn compute_line_height_with_metrics(
  style: &ComputedStyle,
  metrics: Option<&crate::text::font_db::ScaledMetrics>,
) -> f32 {
  compute_line_height_with_metrics_viewport(style, metrics, None, None)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_db::ScaledMetrics;

  #[test]
  fn from_font_metrics_includes_half_leading_in_baseline_offset() {
    // Construct a font with ascent+descent larger than the authored line-height so leading is
    // negative. BaselineMetrics should reflect the half-leading adjustment.
    let font = FontMetrics {
      units_per_em: 1000,
      ascent: 800,
      descent: -200,
      line_gap: 0,
      line_height: 1000,
      x_height: None,
      cap_height: None,
      underline_position: 0,
      underline_thickness: 0,
      strikeout_position: None,
      strikeout_thickness: None,
      is_bold: false,
      is_italic: false,
      is_monospace: false,
    };

    let metrics = BaselineMetrics::from_font_metrics(&font, 10.0, 6.0);
    assert!(
      (metrics.baseline_offset - 6.0).abs() < 1e-3,
      "baseline_offset should include half-leading when line-height is tight"
    );
  }

  #[test]
  fn test_vertical_align_default() {
    let align = VerticalAlign::default();
    assert_eq!(align, VerticalAlign::Baseline);
  }

  #[test]
  fn test_vertical_align_is_line_relative() {
    assert!(VerticalAlign::Top.is_line_relative());
    assert!(VerticalAlign::Bottom.is_line_relative());
    assert!(!VerticalAlign::Baseline.is_line_relative());
    assert!(!VerticalAlign::Middle.is_line_relative());
  }

  #[test]
  fn test_vertical_align_is_baseline_relative() {
    assert!(VerticalAlign::Baseline.is_baseline_relative());
    assert!(VerticalAlign::Middle.is_baseline_relative());
    assert!(VerticalAlign::Sub.is_baseline_relative());
    assert!(VerticalAlign::Super.is_baseline_relative());
    assert!(VerticalAlign::Length(5.0).is_baseline_relative());
    assert!(!VerticalAlign::Top.is_baseline_relative());
    assert!(!VerticalAlign::Bottom.is_baseline_relative());
  }

  #[test]
  fn test_baseline_metrics_from_values() {
    let metrics = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    assert_eq!(metrics.baseline_offset, 12.0);
    assert_eq!(metrics.height, 16.0);
    assert_eq!(metrics.ascent, 12.0);
    assert_eq!(metrics.descent, 4.0);
  }

  #[test]
  fn test_baseline_metrics_for_replaced() {
    let metrics = BaselineMetrics::for_replaced(100.0);
    assert_eq!(metrics.baseline_offset, 100.0);
    assert_eq!(metrics.height, 100.0);
  }

  #[test]
  fn test_baseline_metrics_half_leading() {
    let metrics = BaselineMetrics {
      baseline_offset: 12.0,
      height: 24.0,
      ascent: 12.0,
      descent: 4.0,
      line_gap: 0.0,
      line_height: 24.0,
      x_height: None,
    };
    // line_height=24, ascent+descent=16, leading=8, half=4
    assert_eq!(metrics.half_leading(), 4.0);
  }

  #[test]
  fn baseline_metrics_allow_negative_half_leading() {
    // line-height smaller than the font's ascent+descent should produce negative leading
    let metrics = BaselineMetrics {
      baseline_offset: 10.0,
      height: 8.0,
      ascent: 10.0,
      descent: 2.0,
      line_gap: 0.0,
      line_height: 8.0,
      x_height: None,
    };

    assert!((metrics.half_leading() + 2.0).abs() < 1e-3);
  }

  #[test]
  fn middle_alignment_prefers_parent_x_height() {
    let mut acc = LineBaselineAccumulator::new(&BaselineMetrics::new(10.0, 14.0, 10.0, 4.0));

    let parent_with_x = BaselineMetrics {
      baseline_offset: 20.0,
      height: 30.0,
      ascent: 20.0,
      descent: 10.0,
      line_gap: 0.0,
      line_height: 30.0,
      x_height: Some(6.0),
    };
    let parent_no_x = BaselineMetrics {
      x_height: None,
      ..parent_with_x
    };

    let item = BaselineMetrics {
      baseline_offset: 8.0,
      height: 10.0,
      ascent: 8.0,
      descent: 2.0,
      line_gap: 0.0,
      line_height: 10.0,
      x_height: None,
    };

    let shift_with_x =
      acc.add_baseline_relative(&item, VerticalAlign::Middle, Some(&parent_with_x));
    let shift_without_x =
      acc.add_baseline_relative(&item, VerticalAlign::Middle, Some(&parent_no_x));

    assert!(
      (shift_with_x - 0.0).abs() < 1e-3,
      "middle should use parent x-height midpoint"
    );
    assert!(
      (shift_without_x - -7.0).abs() < 1e-3,
      "fallback uses 0.5em proxy when x-height is absent"
    );
    assert!(
      shift_with_x > shift_without_x,
      "x-height should require less upward adjustment than the half-ascent fallback"
    );
  }

  #[test]
  fn middle_alignment_snaps_parent_x_height_to_whole_css_pixels() {
    // Blink/FreeType hinting tends to snap font metrics to whole pixels. Ensure we mirror that
    // behavior for baseline-relative middle alignment so small subpixel errors don't accumulate
    // into visible vertical drift on text-heavy pages (e.g. lobste.rs bylines).
    let parent = BaselineMetrics {
      baseline_offset: 10.0,
      height: 14.0,
      ascent: 10.0,
      descent: 4.0,
      line_gap: 0.0,
      line_height: 14.0,
      x_height: Some(7.1),
    };
    let replaced = BaselineMetrics::for_replaced(16.0);
    let acc = LineBaselineAccumulator::new(&parent);
    let shift = acc.compute_baseline_shift(VerticalAlign::Middle, &replaced, Some(&parent));
    // shift = 16 - 8 - ceil(7.1)/2 = 8 - 4 = 4
    assert!((shift - 4.0).abs() < 1e-3, "unexpected shift: {shift}");
  }

  #[test]
  fn middle_alignment_applies_to_replaced_elements() {
    // Replaced elements use their bottom edge as baseline; vertical-align: middle should still
    // use the parent's x-height to shift them relative to the line.
    let parent = BaselineMetrics {
      baseline_offset: 18.0,
      height: 24.0,
      ascent: 18.0,
      descent: 6.0,
      line_gap: 0.0,
      line_height: 24.0,
      x_height: Some(6.0),
    };

    let replaced = BaselineMetrics::for_replaced(12.0);
    let mut acc = LineBaselineAccumulator::new(&parent);
    let shift = acc.add_baseline_relative(&replaced, VerticalAlign::Middle, Some(&parent));

    // Expected shift: baseline_offset (12) minus half the box height (6) minus x-height/2 (3) = 3
    assert!(
      (shift - 3.0).abs() < 1e-3,
      "unexpected middle shift for replaced element: {}",
      shift
    );
  }

  #[test]
  fn middle_alignment_respects_parent_x_height_with_overflowing_inline_block() {
    // Overflow-hidden inline-block uses bottom-edge baseline; middle alignment should still
    // align its midpoint with the parent's x-height midpoint.
    let parent = BaselineMetrics {
      baseline_offset: 16.0,
      height: 22.0,
      ascent: 16.0,
      descent: 6.0,
      line_gap: 0.0,
      line_height: 22.0,
      x_height: Some(8.0),
    };
    let child = BaselineMetrics::for_replaced(20.0); // baseline at bottom edge
    let mut acc = LineBaselineAccumulator::new(&parent);
    let shift = acc.add_baseline_relative(&child, VerticalAlign::Middle, Some(&parent));
    // parent x-height midpoint = 4; child midpoint (baseline_offset - height/2) = 10; shift = 10 - 4 = 6.
    assert!(
      (shift - 6.0).abs() < 1e-3,
      "middle shift should reflect parent x-height midpoint and child height"
    );
  }

  #[test]
  fn middle_aligned_replaced_does_not_inflate_line_baseline() {
    // Regression: the line baseline accumulator must use the y-down baseline offset when
    // computing ascent/descent for vertical-align values like `middle`. Otherwise a middle-aligned
    // replaced element can incorrectly dominate the line's ascent and push content outside the
    // line box (as seen on gentoo.org's header logo when using bundled fonts).
    let parent = BaselineMetrics {
      baseline_offset: 10.0,
      height: 14.0,
      ascent: 10.0,
      descent: 4.0,
      line_gap: 0.0,
      line_height: 14.0,
      x_height: Some(6.0),
    };
    let replaced = BaselineMetrics::for_replaced(30.0);
    let mut acc = LineBaselineAccumulator::new(&parent);
    let shift = acc.add_baseline_relative(&replaced, VerticalAlign::Middle, Some(&parent));

    // shift = baseline_offset - height/2 - x-height/2 = 30 - 15 - 3 = 12
    assert!((shift - 12.0).abs() < 1e-3);

    // With the above shift, the replaced element's top aligns with the top of the line box.
    assert!((acc.baseline_position() - 18.0).abs() < 1e-3);
    assert!((acc.line_height() - 30.0).abs() < 1e-3);
    let top = acc.baseline_position() + shift - replaced.baseline_offset;
    assert!(top.abs() < 1e-3, "expected item top to be 0, got {top}");
  }

  #[test]
  fn test_line_accumulator_baseline_alignment() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let item = BaselineMetrics::new(10.0, 14.0, 10.0, 4.0);
    let shift = acc.add_baseline_relative(&item, VerticalAlign::Baseline, None);

    assert_eq!(shift, 0.0);
    // The line box height/baseline is determined by the items on the line, but the strut still
    // acts as a minimum ascent/descent (notably preserving the font's descender under inline
    // replaced elements).
  }

  #[test]
  fn test_line_accumulator_taller_item() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    // Add item with bigger ascent
    let item = BaselineMetrics::new(20.0, 24.0, 20.0, 4.0);
    acc.add_baseline_relative(&item, VerticalAlign::Baseline, None);

    // Line height should grow to accommodate
    assert!(acc.line_height() > 16.0);
    assert!(acc.max_ascent >= 20.0);
  }

  #[test]
  fn line_accumulator_tracks_mixed_font_runs() {
    let strut = BaselineMetrics::new(10.0, 16.0, 10.0, 6.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let small = BaselineMetrics::new(8.0, 14.0, 8.0, 6.0);
    let shift_small = acc.add_baseline_relative(&small, VerticalAlign::Baseline, None);
    assert_eq!(shift_small, 0.0);

    let large = BaselineMetrics::new(12.0, 18.0, 12.0, 6.0);
    let shift_large = acc.add_baseline_relative(&large, VerticalAlign::Baseline, None);
    assert_eq!(shift_large, 0.0);

    assert!((acc.max_ascent - 12.0).abs() < 1e-3);
    assert!((acc.max_descent - 6.0).abs() < 1e-3);
    assert!((acc.line_height() - 18.0).abs() < 1e-3);
  }

  #[test]
  fn test_line_accumulator_top_aligned() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let item = BaselineMetrics::new(30.0, 40.0, 30.0, 10.0);
    acc.add_line_relative(&item, VerticalAlign::Top);

    assert_eq!(acc.top_aligned_height, 40.0);
  }

  #[test]
  fn test_line_accumulator_bottom_aligned() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let item = BaselineMetrics::new(30.0, 40.0, 30.0, 10.0);
    acc.add_line_relative(&item, VerticalAlign::Bottom);

    assert_eq!(acc.bottom_aligned_height, 40.0);
  }

  #[test]
  fn test_baseline_shift_super() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let parent = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let item = BaselineMetrics::new(8.0, 10.0, 8.0, 2.0);
    let shift = acc.add_baseline_relative(&item, VerticalAlign::Super, Some(&parent));

    // Super should raise the box, i.e. produce a negative y-down offset.
    assert!(shift < 0.0);
  }

  #[test]
  fn test_baseline_shift_sub() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let parent = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let item = BaselineMetrics::new(8.0, 10.0, 8.0, 2.0);
    let shift = acc.add_baseline_relative(&item, VerticalAlign::Sub, Some(&parent));

    // Sub should lower the box, i.e. produce a positive y-down offset.
    assert!(shift > 0.0);
  }

  #[test]
  fn test_baseline_shift_length() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let mut acc = LineBaselineAccumulator::new(&strut);

    let item = BaselineMetrics::new(8.0, 10.0, 8.0, 2.0);
    let shift = acc.add_baseline_relative(&item, VerticalAlign::Length(5.0), None);

    // Positive lengths "raise" in CSS, so the y-down offset is negative.
    assert_eq!(shift, -5.0);
  }

  #[test]
  fn test_line_height_calculation() {
    let strut = BaselineMetrics::new(12.0, 16.0, 12.0, 4.0);
    let acc = LineBaselineAccumulator::new(&strut);

    // Line height should be at least strut height (considering half-leading)
    assert!(acc.line_height() >= 16.0);
  }

  #[test]
  fn test_empty_line_uses_strut() {
    let strut = BaselineMetrics::new(16.0, 20.0, 16.0, 4.0);
    let acc = LineBaselineAccumulator::new(&strut);

    // Even empty line has strut height
    assert!(acc.line_height() > 0.0);
    assert!(acc.baseline_position() > 0.0);
  }

  #[test]
  fn compute_line_height_prefers_scaled_metrics_for_normal() {
    let mut style = ComputedStyle::default();
    style.line_height = crate::style::types::LineHeight::Normal;
    style.font_size = 16.0;
    let metrics = ScaledMetrics {
      font_size: 16.0,
      scale: 1.0,
      ascent: 10.0,
      descent: 4.0,
      line_gap: 2.0,
      line_height: 16.0,
      x_height: Some(8.0),
      cap_height: Some(12.0),
      underline_position: 2.0,
      underline_thickness: 1.0,
    };

    assert!((compute_line_height(&style) - 19.2).abs() < 0.01);
    assert!((compute_line_height_with_metrics(&style, Some(&metrics)) - 16.0).abs() < 0.01);
  }

  #[test]
  fn compute_line_height_viewport_relative_uses_real_viewport() {
    use crate::style::values::Length;
    use crate::style::values::LengthUnit;

    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    style.line_height = crate::style::types::LineHeight::Length(Length::new(10.0, LengthUnit::Vh));

    // With a 500px high viewport, 10vh should be 50px. The viewport-aware helper should use
    // the provided size instead of the 1200x800 fallback.
    let vh_500 =
      compute_line_height_with_metrics_viewport(&style, None, Some(Size::new(800.0, 500.0)), None);
    assert!((vh_500 - 50.0).abs() < 0.001);

    // And with a different viewport height, the result should change accordingly.
    let vh_900 =
      compute_line_height_with_metrics_viewport(&style, None, Some(Size::new(800.0, 900.0)), None);
    assert!((vh_900 - 90.0).abs() < 0.001);
  }

  #[test]
  fn compute_line_height_resolves_root_font_relative_units() {
    use crate::style::values::CalcLength;
    use crate::style::values::Length;
    use crate::style::values::LengthUnit;

    let mut style = ComputedStyle::default();
    style.font_size = 20.0;
    style.root_font_size = 10.0;

    style.line_height = crate::style::types::LineHeight::Length(Length::new(1.0, LengthUnit::Rch));
    let rch = compute_line_height_with_metrics_viewport(&style, None, None, None);
    assert!((rch - 5.0).abs() < 0.001);

    style.line_height = crate::style::types::LineHeight::Length(Length::calc(CalcLength::single(
      LengthUnit::Rch,
      1.0,
    )));
    let rch_calc = compute_line_height_with_metrics_viewport(&style, None, None, None);
    assert!((rch_calc - 5.0).abs() < 0.001);
  }
}
