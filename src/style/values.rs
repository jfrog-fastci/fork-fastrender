//! CSS value types
//!
//! This module provides types for representing CSS values in their computed form.
//! These types are used throughout the style and layout systems.
//!
//! # Units
//!
//! CSS supports various length units. We categorize them as:
//! - **Absolute**: px, pt, pc, in, cm, mm
//! - **Font-relative**: em, rem, ex, ch, lh
//! - **Viewport-relative**: vw, vh, vmin, vmax
//! - **Percentages**: Relative to containing block or font size
//!
//! Reference: CSS Values and Units Module Level 3
//! <https://www.w3.org/TR/css-values-3/>

use std::fmt;

use smallvec::SmallVec;

/// CSS length units
///
/// Represents the unit portion of a CSS length value.
///
/// # Examples
///
/// ```
/// use fastrender::LengthUnit;
///
/// let unit = LengthUnit::Px;
/// assert!(unit.is_absolute());
///
/// let font_unit = LengthUnit::Em;
/// assert!(font_unit.is_font_relative());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LengthUnit {
  /// Pixels (px) - CSS reference unit, 1/96th of an inch
  Px,

  /// Points (pt) - 1/72nd of an inch
  Pt,

  /// Picas (pc) - 12 points
  Pc,

  /// Inches (in)
  In,

  /// Centimeters (cm)
  Cm,

  /// Millimeters (mm)
  Mm,

  /// Quarter-millimeters (Q)
  Q,

  /// Em units - relative to element's font size
  Em,

  /// Rem units - relative to root element's font size
  Rem,

  /// Ex units - relative to x-height of the font
  Ex,

  /// Ch units - relative to width of '0' character
  Ch,

  /// Line-height units (lh) - relative to the element's computed line-height
  Lh,

  /// Viewport width percentage (vw) - 1% of viewport width
  Vw,

  /// Viewport height percentage (vh) - 1% of viewport height
  Vh,

  /// Viewport minimum (vmin) - 1% of smaller viewport dimension
  Vmin,

  /// Viewport maximum (vmax) - 1% of larger viewport dimension
  Vmax,

  /// Dynamic viewport width (dvw) - responds to UA UI changes
  Dvw,

  /// Dynamic viewport height (dvh)
  Dvh,

  /// Dynamic viewport minimum (dvmin)
  Dvmin,

  /// Dynamic viewport maximum (dvmax)
  Dvmax,

  /// Container query width (cqw) - 1% of the query container's width
  Cqw,

  /// Container query height (cqh) - 1% of the query container's height
  Cqh,

  /// Container query inline size (cqi) - 1% of the query container's inline size
  Cqi,

  /// Container query block size (cqb) - 1% of the query container's block size
  Cqb,

  /// Container query minimum (cqmin) - 1% of the smaller of inline/block sizes
  Cqmin,

  /// Container query maximum (cqmax) - 1% of the larger of inline/block sizes
  Cqmax,

  /// Percentage (%) - relative to containing block or font size
  Percent,

  /// Calculated length from `calc()`, `min()`, `max()`, or `clamp()`
  Calc,
}

impl LengthUnit {
  /// Returns true if this is an absolute unit (px, pt, pc, in, cm, mm)
  ///
  /// Absolute units have fixed physical sizes and can be converted
  /// between each other without context.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthUnit;
  ///
  /// assert!(LengthUnit::Px.is_absolute());
  /// assert!(LengthUnit::In.is_absolute());
  /// assert!(!LengthUnit::Em.is_absolute());
  /// ```
  pub fn is_absolute(self) -> bool {
    matches!(
      self,
      Self::Px | Self::Pt | Self::Pc | Self::In | Self::Cm | Self::Mm | Self::Q
    )
  }

  /// Returns true if this is a font-relative unit (em, rem, ex, ch, lh)
  ///
  /// Font-relative units require font metrics to resolve.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthUnit;
  ///
  /// assert!(LengthUnit::Em.is_font_relative());
  /// assert!(LengthUnit::Rem.is_font_relative());
  /// assert!(!LengthUnit::Px.is_font_relative());
  /// ```
  pub fn is_font_relative(self) -> bool {
    matches!(self, Self::Em | Self::Rem | Self::Ex | Self::Ch | Self::Lh)
  }

  /// Returns true if this is a viewport-relative unit (vw, vh, vmin, vmax)
  ///
  /// Viewport-relative units require viewport dimensions to resolve.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthUnit;
  ///
  /// assert!(LengthUnit::Vw.is_viewport_relative());
  /// assert!(LengthUnit::Vh.is_viewport_relative());
  /// assert!(!LengthUnit::Px.is_viewport_relative());
  /// ```
  pub fn is_viewport_relative(self) -> bool {
    matches!(
      self,
      Self::Vw
        | Self::Vh
        | Self::Vmin
        | Self::Vmax
        | Self::Dvw
        | Self::Dvh
        | Self::Dvmin
        | Self::Dvmax
    )
  }

  pub fn is_container_query_relative(self) -> bool {
    matches!(
      self,
      Self::Cqw | Self::Cqh | Self::Cqi | Self::Cqb | Self::Cqmin | Self::Cqmax
    )
  }

  /// Returns true if this is a percentage
  pub fn is_percentage(self) -> bool {
    matches!(self, Self::Percent)
  }

  /// Returns the canonical string representation of this unit
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthUnit;
  ///
  /// assert_eq!(LengthUnit::Px.as_str(), "px");
  /// assert_eq!(LengthUnit::Em.as_str(), "em");
  /// assert_eq!(LengthUnit::Percent.as_str(), "%");
  /// ```
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Px => "px",
      Self::Pt => "pt",
      Self::Pc => "pc",
      Self::In => "in",
      Self::Cm => "cm",
      Self::Mm => "mm",
      Self::Q => "q",
      Self::Em => "em",
      Self::Rem => "rem",
      Self::Ex => "ex",
      Self::Ch => "ch",
      Self::Lh => "lh",
      Self::Vw => "vw",
      Self::Vh => "vh",
      Self::Vmin => "vmin",
      Self::Vmax => "vmax",
      Self::Dvw => "dvw",
      Self::Dvh => "dvh",
      Self::Dvmin => "dvmin",
      Self::Dvmax => "dvmax",
      Self::Cqw => "cqw",
      Self::Cqh => "cqh",
      Self::Cqi => "cqi",
      Self::Cqb => "cqb",
      Self::Cqmin => "cqmin",
      Self::Cqmax => "cqmax",
      Self::Percent => "%",
      Self::Calc => "calc",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalcTerm {
  pub unit: LengthUnit,
  pub value: f32,
}

const MAX_CALC_TERMS: usize = 8;
const EMPTY_TERM: CalcTerm = CalcTerm {
  unit: LengthUnit::Px,
  value: 0.0,
};

/// Linear combination of length units produced by `calc()`, `min()`, `max()`, or `clamp()`.
///
/// Terms are stored as unit coefficients (e.g., `50% + 10px - 2vw`), and resolved later with
/// the appropriate percentage base, viewport, and font metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalcLength {
  terms: [CalcTerm; MAX_CALC_TERMS],
  term_count: u8,
}

impl CalcLength {
  pub const fn empty() -> Self {
    Self {
      terms: [EMPTY_TERM; MAX_CALC_TERMS],
      term_count: 0,
    }
  }

  pub fn single(unit: LengthUnit, value: f32) -> Self {
    let mut calc = Self::empty();
    let _ = calc.push(unit, value);
    calc
  }

  pub fn terms(&self) -> &[CalcTerm] {
    &self.terms[..self.term_count as usize]
  }

  fn push(&mut self, unit: LengthUnit, value: f32) -> Result<(), ()> {
    if value == 0.0 {
      return Ok(());
    }
    if let Some(existing) = self.terms().iter().position(|t| t.unit == unit) {
      self.terms[existing].value += value;
      if self.terms[existing].value == 0.0 {
        // Remove zeroed term
        let len = self.term_count as usize;
        for i in existing..len - 1 {
          self.terms[i] = self.terms[i + 1];
        }
        self.terms[len - 1] = EMPTY_TERM;
        self.term_count -= 1;
      }
      return Ok(());
    }

    let len = self.term_count as usize;
    if len >= MAX_CALC_TERMS {
      return Err(()); // overflow; reject overly complex expressions
    }
    self.terms[len] = CalcTerm { unit, value };
    self.term_count += 1;
    Ok(())
  }

  pub fn scale(&self, factor: f32) -> Self {
    let mut out = Self::empty();
    for term in self.terms() {
      let _ = out.push(term.unit, term.value * factor);
    }
    out
  }

  pub fn add_scaled(&self, other: &CalcLength, scale: f32) -> Option<Self> {
    let mut out = *self;
    for term in other.terms() {
      if out.push(term.unit, term.value * scale).is_err() {
        return None;
      }
    }
    Some(out)
  }

  pub fn is_zero(&self) -> bool {
    self.term_count == 0 || self.terms().iter().all(|t| t.value == 0.0)
  }

  pub fn has_percentage(&self) -> bool {
    self.terms().iter().any(|t| t.unit == LengthUnit::Percent)
  }

  pub fn has_viewport_relative(&self) -> bool {
    self.terms().iter().any(|t| t.unit.is_viewport_relative())
  }

  pub fn has_font_relative(&self) -> bool {
    self.terms().iter().any(|t| t.unit.is_font_relative())
  }

  pub fn has_container_query_relative(&self) -> bool {
    self
      .terms()
      .iter()
      .any(|t| t.unit.is_container_query_relative())
  }

  pub fn resolve_container_query_units(
    &self,
    cqw_base: f32,
    cqh_base: f32,
    cqi_base: f32,
    cqb_base: f32,
  ) -> Self {
    let cqw_base = cqw_base.max(0.0);
    let cqh_base = cqh_base.max(0.0);
    let cqi_base = cqi_base.max(0.0);
    let cqb_base = cqb_base.max(0.0);
    let cqw_base = if cqw_base.is_finite() { cqw_base } else { 0.0 };
    let cqh_base = if cqh_base.is_finite() { cqh_base } else { 0.0 };
    let cqi_base = if cqi_base.is_finite() { cqi_base } else { 0.0 };
    let cqb_base = if cqb_base.is_finite() { cqb_base } else { 0.0 };
    let cqmin_base = cqi_base.min(cqb_base);
    let cqmax_base = cqi_base.max(cqb_base);

    let mut out = Self::empty();
    for term in self.terms() {
      match term.unit {
        LengthUnit::Cqw => {
          let px = (term.value / 100.0) * cqw_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        LengthUnit::Cqh => {
          let px = (term.value / 100.0) * cqh_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        LengthUnit::Cqi => {
          let px = (term.value / 100.0) * cqi_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        LengthUnit::Cqb => {
          let px = (term.value / 100.0) * cqb_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        LengthUnit::Cqmin => {
          let px = (term.value / 100.0) * cqmin_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        LengthUnit::Cqmax => {
          let px = (term.value / 100.0) * cqmax_base;
          let _ = out.push(LengthUnit::Px, px);
        }
        _ => {
          let _ = out.push(term.unit, term.value);
        }
      }
    }
    out
  }

  pub fn resolve(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
  ) -> Option<f32> {
    if !viewport_width.is_finite()
      || !viewport_height.is_finite()
      || !font_size_px.is_finite()
      || !root_font_size_px.is_finite()
    {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());
    let mut total = 0.0;
    for term in self.terms() {
      let resolved = match term.unit {
        LengthUnit::Percent => percentage_base.map(|base| (term.value / 100.0) * base),
        u if u.is_absolute() => Some(Length::new(term.value, u).to_px()),
        u if u.is_viewport_relative() => {
          Length::new(term.value, u).resolve_with_viewport(viewport_width, viewport_height)
        }
        LengthUnit::Em => Some(term.value * font_size_px),
        LengthUnit::Ex | LengthUnit::Ch => Some(term.value * font_size_px * 0.5),
        LengthUnit::Rem => Some(term.value * root_font_size_px),
        // Without access to computed `line-height`, fall back to the `normal` approximation.
        // Layout code that has access to `ComputedStyle` should resolve `lh` more accurately.
        LengthUnit::Lh => Some(term.value * font_size_px * 1.2),
        LengthUnit::Calc => None,
        _ => None,
      }?;
      total += resolved;
    }
    Some(total)
  }

  pub fn single_term(&self) -> Option<CalcTerm> {
    if self.term_count == 1 {
      Some(self.terms[0])
    } else {
      None
    }
  }

  pub fn requires_context(&self) -> bool {
    self.terms().iter().any(|t| {
      t.unit.is_percentage()
        || t.unit.is_viewport_relative()
        || t.unit.is_font_relative()
        || matches!(t.unit, LengthUnit::Calc)
    })
  }

  pub fn absolute_sum(&self) -> Option<f32> {
    let mut total = 0.0;
    for term in self.terms() {
      match term.unit {
        u if u.is_absolute() => total += Length::new(term.value, u).to_px(),
        _ => return None,
      }
    }
    Some(total)
  }

  fn write_css(&self, out: &mut impl fmt::Write) -> fmt::Result {
    if self.is_zero() {
      // Unitless zero is valid for `<length>` and `<length-percentage>`.
      return out.write_str("0");
    }

    if let Some(term) = self.single_term() {
      return write!(out, "{}{}", term.value, term.unit);
    }

    out.write_str("calc(")?;
    for (idx, term) in self.terms().iter().enumerate() {
      if idx == 0 {
        write!(out, "{}{}", term.value, term.unit)?;
        continue;
      }

      if term.value.is_sign_negative() {
        out.write_str(" - ")?;
        write!(out, "{}{}", term.value.abs(), term.unit)?;
      } else {
        out.write_str(" + ")?;
        write!(out, "{}{}", term.value, term.unit)?;
      }
    }
    out.write_str(")")
  }

  /// Serializes this calculated length to CSS text.
  ///
  /// This is used when synthesizing intermediate values for animations and for writing typed custom
  /// property values back into `CustomPropertyValue.value` for later `var()` substitution.
  pub fn to_css(&self) -> String {
    let mut out = String::new();
    let _ = self.write_css(&mut out);
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::properties::parse_length;

  #[test]
  fn non_ascii_whitespace_custom_property_number_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert_eq!(
      CustomPropertySyntax::Number.parse_value("1"),
      Some(CustomPropertyTypedValue::Number(1.0))
    );
    assert!(
      CustomPropertySyntax::Number
        .parse_value(&format!("{nbsp}1"))
        .is_none(),
      "NBSP must not be treated as CSS whitespace in registered custom properties"
    );
  }

  // LengthUnit tests
  #[test]
  fn test_length_unit_classification() {
    assert!(LengthUnit::Px.is_absolute());
    assert!(LengthUnit::Pt.is_absolute());
    assert!(LengthUnit::In.is_absolute());
    assert!(LengthUnit::Q.is_absolute());

    assert!(LengthUnit::Em.is_font_relative());
    assert!(LengthUnit::Rem.is_font_relative());
    assert!(LengthUnit::Lh.is_font_relative());

    assert!(LengthUnit::Vw.is_viewport_relative());
    assert!(LengthUnit::Vh.is_viewport_relative());

    assert!(LengthUnit::Percent.is_percentage());
  }

  #[test]
  fn test_length_unit_as_str() {
    assert_eq!(LengthUnit::Px.as_str(), "px");
    assert_eq!(LengthUnit::Em.as_str(), "em");
    assert_eq!(LengthUnit::Lh.as_str(), "lh");
    assert_eq!(LengthUnit::Percent.as_str(), "%");
  }

  // Length tests
  #[test]
  fn test_length_constructors() {
    let px = Length::px(100.0);
    assert_eq!(px.value, 100.0);
    assert_eq!(px.unit, LengthUnit::Px);

    let em = Length::em(2.0);
    assert_eq!(em.value, 2.0);
    assert_eq!(em.unit, LengthUnit::Em);
  }

  #[test]
  fn test_length_to_px() {
    assert_eq!(Length::px(100.0).to_px(), 100.0);
    assert_eq!(Length::inches(1.0).to_px(), 96.0);
    assert!((Length::pt(72.0).to_px() - 96.0).abs() < 0.1); // 72pt = 1in = 96px
  }

  #[test]
  fn test_length_unit_conversions() {
    // 1 inch = 96px
    assert_eq!(Length::inches(1.0).to_px(), 96.0);

    // 1 point = 1/72 inch
    let pt_to_px = Length::pt(72.0).to_px();
    assert!((pt_to_px - 96.0).abs() < 0.01);

    // 1 pica = 12 points = 16px
    assert_eq!(Length::pc(1.0).to_px(), 16.0);

    // 1 cm = 96/2.54 px
    let cm_to_px = Length::cm(2.54).to_px();
    assert!((cm_to_px - 96.0).abs() < 0.1);
  }

  #[test]
  fn test_length_percentage_resolution() {
    let percent = Length::percent(50.0);
    assert_eq!(percent.resolve_against(200.0), Some(100.0));
    assert_eq!(percent.resolve_against(100.0), Some(50.0));
  }

  #[test]
  fn test_length_resolution_without_context_returns_none() {
    let em = Length::em(2.0);
    assert_eq!(em.resolve_against(100.0), None);

    let vw = Length::new(10.0, LengthUnit::Vw);
    assert_eq!(vw.resolve_against(100.0), None);
  }

  #[test]
  fn test_length_font_size_resolution() {
    let em = Length::em(2.0);
    assert_eq!(em.resolve_with_font_size(16.0), Some(32.0));

    let rem = Length::rem(1.5);
    assert_eq!(rem.resolve_with_font_size(16.0), Some(24.0));

    let ex = Length::ex(2.0);
    assert_eq!(ex.resolve_with_font_size(16.0), Some(16.0));

    let ch = Length::ch(3.0);
    assert_eq!(ch.resolve_with_font_size(16.0), Some(24.0));
  }

  #[test]
  fn test_length_viewport_resolution() {
    let vw = Length::new(50.0, LengthUnit::Vw);
    assert_eq!(vw.resolve_with_viewport(800.0, 600.0), Some(400.0));

    let vh = Length::new(50.0, LengthUnit::Vh);
    assert_eq!(vh.resolve_with_viewport(800.0, 600.0), Some(300.0));

    let vmin = Length::new(10.0, LengthUnit::Vmin);
    assert_eq!(vmin.resolve_with_viewport(800.0, 600.0), Some(60.0)); // 10% of 600

    let vmax = Length::new(10.0, LengthUnit::Vmax);
    assert_eq!(vmax.resolve_with_viewport(800.0, 600.0), Some(80.0)); // 10% of 800

    assert_eq!(
      Length::percent(50.0).resolve_with_viewport(800.0, 600.0),
      None
    );
    assert_eq!(Length::em(2.0).resolve_with_viewport(800.0, 600.0), None);
  }

  #[test]
  fn test_modern_viewport_units_resolve_with_static_viewport() {
    // CSS Values and Units Level 4 adds "small" and "large" viewport units (sv*/lv*). FastRender
    // renders with a fixed headless viewport, so these resolve the same as the classic viewport
    // units.
    let svh = crate::css::properties::parse_length("100svh").expect("svh length");
    assert_eq!(svh.resolve_with_viewport(800.0, 600.0), Some(600.0));

    let lvw = crate::css::properties::parse_length("50lvw").expect("lvw length");
    assert_eq!(lvw.resolve_with_viewport(800.0, 600.0), Some(400.0));
  }

  #[test]
  fn test_length_is_zero() {
    assert!(Length::px(0.0).is_zero());
    assert!(Length::em(0.0).is_zero());
    assert!(!Length::px(0.1).is_zero());
  }

  #[test]
  fn test_length_to_px_relative_units_fallback() {
    assert_eq!(Length::em(2.0).to_px(), 2.0);
    assert_eq!(Length::percent(50.0).to_px(), 50.0);
  }

  // LengthOrAuto tests
  #[test]
  fn test_length_or_auto_constructors() {
    let auto = LengthOrAuto::Auto;
    assert!(auto.is_auto());

    let length = LengthOrAuto::px(100.0);
    assert!(!length.is_auto());
    assert_eq!(length.to_px(), Some(100.0));
  }

  #[test]
  fn test_length_or_auto_length() {
    let value = LengthOrAuto::px(100.0);
    assert_eq!(value.length(), Some(Length::px(100.0)));

    let auto = LengthOrAuto::Auto;
    assert_eq!(auto.length(), None);
  }

  #[test]
  fn test_length_or_auto_to_px() {
    assert_eq!(LengthOrAuto::px(100.0).to_px(), Some(100.0));
    assert_eq!(LengthOrAuto::Auto.to_px(), None);

    // Relative units return None (need context)
    let em = LengthOrAuto::Length(Length::em(2.0));
    assert_eq!(em.to_px(), None);
  }

  #[test]
  fn test_length_or_auto_resolve_against() {
    let percent = LengthOrAuto::percent(50.0);
    assert_eq!(percent.resolve_against(200.0), Some(100.0));

    let px = LengthOrAuto::px(75.0);
    assert_eq!(px.resolve_against(200.0), Some(75.0));

    let auto = LengthOrAuto::Auto;
    assert_eq!(auto.resolve_against(200.0), None);
  }

  #[test]
  fn test_length_or_auto_resolve_or() {
    assert_eq!(LengthOrAuto::px(100.0).resolve_or(50.0, 0.0), 100.0);
    assert_eq!(LengthOrAuto::Auto.resolve_or(50.0, 0.0), 50.0);

    let percent = LengthOrAuto::percent(25.0);
    assert_eq!(percent.resolve_or(0.0, 200.0), 50.0);
  }

  #[test]
  fn resolve_length_handles_non_finite_contexts() {
    assert_eq!(Length::percent(50.0).resolve_against(f32::NAN), None);
    assert_eq!(Length::em(2.0).resolve_with_font_size(f32::NAN), None);
    assert_eq!(
      Length::new(10.0, LengthUnit::Vw).resolve_with_viewport(f32::NAN, 800.0),
      None
    );
    assert_eq!(
      Length::percent(10.0).resolve_with_context(
        Some(f32::NAN),
        f32::NAN,
        f32::NAN,
        f32::NAN,
        f32::NAN
      ),
      None
    );
  }

  #[test]
  fn calc_length_requires_percentage_base() {
    let mut calc = CalcLength::empty();
    calc.push(LengthUnit::Percent, 50.0).unwrap();
    calc.push(LengthUnit::Px, 10.0).unwrap();

    let length = Length::calc(calc);
    assert_eq!(
      length.resolve_with_context(None, 800.0, 600.0, 16.0, 16.0),
      None
    );
    assert_eq!(
      length.resolve_with_context(Some(f32::NAN), 800.0, 600.0, 16.0, 16.0),
      None
    );
    assert_eq!(
      length.resolve_with_context(Some(200.0), 800.0, 600.0, 16.0, 16.0),
      Some(110.0)
    );

    // Non-finite viewport/font contexts should fail resolution.
    assert_eq!(
      length.resolve_with_context(Some(200.0), f32::NAN, 600.0, 16.0, 16.0),
      None
    );
    assert_eq!(
      length.resolve_with_context(Some(200.0), 800.0, f32::NAN, 16.0, 16.0),
      None
    );
    assert_eq!(
      length.resolve_with_context(Some(200.0), 800.0, 600.0, f32::NAN, 16.0),
      None
    );
    assert_eq!(
      length.resolve_with_context(Some(200.0), 800.0, 600.0, 16.0, f32::NAN),
      None
    );

    let mut absolute_calc = CalcLength::empty();
    absolute_calc.push(LengthUnit::Px, 5.0).unwrap();
    absolute_calc.push(LengthUnit::Em, 1.0).unwrap();

    assert_eq!(
      Length::calc(absolute_calc).resolve_with_context(None, 800.0, 600.0, 10.0, 10.0),
      Some(15.0)
    );
  }

  #[test]
  fn calc_length_percentage_base_rejects_infinite() {
    let mut calc = CalcLength::empty();
    calc.push(LengthUnit::Percent, 50.0).unwrap();
    let length = Length::calc(calc);

    assert_eq!(
      length.resolve_with_context(Some(f32::INFINITY), 800.0, 600.0, 16.0, 16.0),
      None,
      "infinite bases should reject percentage calc resolution"
    );
    assert_eq!(
      length.resolve_with_context(Some(f32::NEG_INFINITY), 800.0, 600.0, 16.0, 16.0),
      None,
      "-infinite bases should reject percentage calc resolution"
    );

    // Finite still works
    assert_eq!(
      length.resolve_with_context(Some(100.0), 800.0, 600.0, 16.0, 16.0),
      Some(50.0)
    );
  }

  #[test]
  fn calc_resolution_helpers_require_context() {
    // Percentage-based calcs need an explicit base even in viewport/font helpers.
    let mut percent_calc = CalcLength::empty();
    percent_calc.push(LengthUnit::Percent, 50.0).unwrap();
    let percent = Length::calc(percent_calc);

    assert_eq!(percent.resolve_with_viewport(800.0, 600.0), None);
    assert_eq!(percent.resolve_with_font_size(16.0), None);

    // Viewport-only calcs resolve with viewport context but stay unresolved without it.
    let mut viewport_calc = CalcLength::empty();
    viewport_calc.push(LengthUnit::Vw, 10.0).unwrap();
    let viewport_len = Length::calc(viewport_calc);

    assert_eq!(viewport_len.resolve_against(200.0), None);
    assert_eq!(viewport_len.resolve_with_viewport(500.0, 400.0), Some(50.0));

    // Font-relative calcs resolve with a font size but not with viewport-only context.
    let mut font_calc = CalcLength::empty();
    font_calc.push(LengthUnit::Em, 2.0).unwrap();
    let font_len = Length::calc(font_calc);

    assert_eq!(font_len.resolve_with_viewport(800.0, 600.0), None);
    assert_eq!(font_len.resolve_with_font_size(12.0), Some(24.0));
  }

  #[test]
  fn calc_length_to_css_round_trips() {
    let mut calc = CalcLength::empty();
    calc.push(LengthUnit::Px, 10.0).unwrap();
    calc.push(LengthUnit::Percent, 50.0).unwrap();

    let length = Length::calc(calc);
    let css = length.to_css();
    assert!(css.contains("calc("));
    assert!(css.contains(" + "));
    assert_eq!(parse_length(&css), Some(length));
  }

  #[test]
  fn calc_length_to_css_handles_negative_leading_term() {
    let mut calc = CalcLength::empty();
    calc.push(LengthUnit::Px, -10.0).unwrap();
    calc.push(LengthUnit::Percent, 50.0).unwrap();

    let length = Length::calc(calc);
    let css = length.to_css();
    assert!(css.starts_with("calc(-10px"));
    assert!(css.contains(" + "));
    assert_eq!(parse_length(&css), Some(length));
  }

  #[test]
  fn calc_length_to_css_single_term_matches_plain_length() {
    let calc = CalcLength::single(LengthUnit::Px, 10.0);
    let calc_length = Length::calc(calc);

    assert_eq!(calc_length.to_css(), Length::px(10.0).to_css());
    assert_eq!(parse_length(&calc_length.to_css()), Some(Length::px(10.0)));
  }

  #[test]
  fn custom_property_typed_length_calc_to_css_round_trips() {
    let mut calc = CalcLength::empty();
    calc.push(LengthUnit::Px, 10.0).unwrap();
    calc.push(LengthUnit::Percent, 50.0).unwrap();

    let typed = CustomPropertyTypedValue::Length(Length::calc(calc));
    let css = typed.to_css();
    assert_ne!(css, "0calc");
    assert!(css.contains("calc("));
    assert_eq!(parse_length(&css), Some(Length::calc(calc)));
  }

  #[test]
  fn test_length_or_auto_from_length() {
    let length = Length::px(100.0);
    let auto_length: LengthOrAuto = length.into();
    assert_eq!(auto_length, LengthOrAuto::Length(length));
  }

  #[test]
  fn test_length_display() {
    assert_eq!(format!("{}", Length::px(100.0)), "100px");
    assert_eq!(format!("{}", Length::em(2.5)), "2.5em");
    assert_eq!(format!("{}", Length::percent(50.0)), "50%");
  }

  #[test]
  fn test_length_or_auto_display() {
    assert_eq!(format!("{}", LengthOrAuto::Auto), "auto");
    assert_eq!(format!("{}", LengthOrAuto::px(100.0)), "100px");
  }
}

impl fmt::Display for LengthUnit {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.as_str())
  }
}

/// A CSS length value with a specific unit
///
/// Represents a computed length value that may need further resolution
/// depending on context (containing block size, font size, etc.).
///
/// # Examples
///
/// ```
/// use fastrender::{Length, LengthUnit};
///
/// let length = Length::px(100.0);
/// assert_eq!(length.value, 100.0);
/// assert_eq!(length.unit, LengthUnit::Px);
///
/// let em_length = Length::em(2.0);
/// let resolved = em_length.resolve_with_font_size(16.0);
/// assert_eq!(resolved, Some(32.0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Length {
  /// The numeric value
  pub value: f32,
  /// The unit
  pub unit: LengthUnit,
  /// Optional calc() expression (takes precedence over `value`/`unit`)
  pub calc: Option<CalcLength>,
}

impl Length {
  /// Creates a new length with the given value and unit
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::{Length, LengthUnit};
  ///
  /// let length = Length::new(10.0, LengthUnit::Px);
  /// assert_eq!(length.value, 10.0);
  /// ```
  pub const fn new(value: f32, unit: LengthUnit) -> Self {
    Self {
      value,
      unit,
      calc: None,
    }
  }

  /// Creates a length from a calc expression
  pub const fn calc(calc: CalcLength) -> Self {
    Self {
      value: 0.0,
      unit: LengthUnit::Calc,
      calc: Some(calc),
    }
  }

  // Convenience constructors for absolute units

  /// Creates a length in pixels
  pub const fn px(value: f32) -> Self {
    Self::new(value, LengthUnit::Px)
  }

  /// Creates a length in points (1pt = 1.333px)
  pub const fn pt(value: f32) -> Self {
    Self::new(value, LengthUnit::Pt)
  }

  /// Creates a length in picas (1pc = 16px)
  pub const fn pc(value: f32) -> Self {
    Self::new(value, LengthUnit::Pc)
  }

  /// Creates a length in inches (1in = 96px)
  pub const fn inches(value: f32) -> Self {
    Self::new(value, LengthUnit::In)
  }

  /// Creates a length in centimeters (1cm = 37.8px)
  pub const fn cm(value: f32) -> Self {
    Self::new(value, LengthUnit::Cm)
  }

  /// Creates a length in millimeters (1mm = 3.78px)
  pub const fn mm(value: f32) -> Self {
    Self::new(value, LengthUnit::Mm)
  }

  /// Creates a length in quarter-millimeters (1Q = 0.25mm)
  pub const fn q(value: f32) -> Self {
    Self::new(value, LengthUnit::Q)
  }

  // Convenience constructors for relative units

  /// Creates a length in em units
  pub const fn em(value: f32) -> Self {
    Self::new(value, LengthUnit::Em)
  }

  /// Creates a length in rem units
  pub const fn rem(value: f32) -> Self {
    Self::new(value, LengthUnit::Rem)
  }

  /// Creates a length in ex units
  pub const fn ex(value: f32) -> Self {
    Self::new(value, LengthUnit::Ex)
  }

  /// Creates a length in ch units
  pub const fn ch(value: f32) -> Self {
    Self::new(value, LengthUnit::Ch)
  }

  /// Creates a percentage value
  pub const fn percent(value: f32) -> Self {
    Self::new(value, LengthUnit::Percent)
  }

  // Unit conversion methods

  /// Converts this length to pixels
  ///
  /// For absolute units, this performs unit conversion. For relative or
  /// percentage units, this is a best-effort fallback that returns the raw
  /// numeric value when no context is available; use the context-aware
  /// resolve helpers when you need spec-accurate resolution.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::Length;
  ///
  /// assert_eq!(Length::px(100.0).to_px(), 100.0);
  /// assert_eq!(Length::pt(72.0).to_px(), 96.0); // 72pt = 1in = 96px
  /// assert_eq!(Length::em(2.0).to_px(), 2.0);   // relative units return raw value without context
  /// ```
  pub fn to_px(self) -> f32 {
    if let Some(calc) = self.calc {
      if let Some(abs) = calc.absolute_sum() {
        return abs;
      }
      // Best-effort fallback when context is missing: treat unresolved units as raw values.
      return calc.terms().iter().map(|t| t.value).sum();
    }
    match self.unit {
      LengthUnit::Px => self.value,
      LengthUnit::Pt => self.value * (96.0 / 72.0), // 1pt = 1/72 inch
      LengthUnit::Pc => self.value * 16.0,          // 1pc = 12pt = 16px
      LengthUnit::In => self.value * 96.0,          // 1in = 96px (CSS spec)
      LengthUnit::Cm => self.value * 37.795276,     // 1cm = 96px/2.54
      LengthUnit::Mm => self.value * 3.7795276,     // 1mm = 1/10 cm
      LengthUnit::Q => self.value * 0.944882,       // 1Q = 1/4 mm
      _ => self.value,
    }
  }

  pub fn resolve_container_query_units(
    self,
    cqw_base: f32,
    cqh_base: f32,
    cqi_base: f32,
    cqb_base: f32,
  ) -> Self {
    if let Some(calc) = self.calc {
      let resolved = calc.resolve_container_query_units(cqw_base, cqh_base, cqi_base, cqb_base);
      if resolved.is_zero() {
        return Length::px(0.0);
      }
      if let Some(term) = resolved.single_term() {
        return Length::new(term.value, term.unit);
      }
      return Length::calc(resolved);
    }

    let cqw_base = if cqw_base.is_finite() && cqw_base > 0.0 {
      cqw_base
    } else {
      0.0
    };
    let cqh_base = if cqh_base.is_finite() && cqh_base > 0.0 {
      cqh_base
    } else {
      0.0
    };
    let cqi_base = if cqi_base.is_finite() && cqi_base > 0.0 {
      cqi_base
    } else {
      0.0
    };
    let cqb_base = if cqb_base.is_finite() && cqb_base > 0.0 {
      cqb_base
    } else {
      0.0
    };
    let cqmin_base = cqi_base.min(cqb_base);
    let cqmax_base = cqi_base.max(cqb_base);

    match self.unit {
      LengthUnit::Cqw => Length::px((self.value / 100.0) * cqw_base),
      LengthUnit::Cqh => Length::px((self.value / 100.0) * cqh_base),
      LengthUnit::Cqi => Length::px((self.value / 100.0) * cqi_base),
      LengthUnit::Cqb => Length::px((self.value / 100.0) * cqb_base),
      LengthUnit::Cqmin => Length::px((self.value / 100.0) * cqmin_base),
      LengthUnit::Cqmax => Length::px((self.value / 100.0) * cqmax_base),
      _ => self,
    }
  }

  /// Resolves this length to pixels using a percentage base.
  ///
  /// Returns `None` when the unit cannot be resolved with the provided base
  /// (e.g., font-relative or viewport-relative units).
  ///
  /// Used when the length is relative to a containing block dimension.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::Length;
  ///
  /// let length = Length::percent(50.0);
  /// assert_eq!(length.resolve_against(200.0), Some(100.0));
  ///
  /// let px_length = Length::px(100.0);
  /// assert_eq!(px_length.resolve_against(200.0), Some(100.0)); // Absolute units ignore base
  /// ```
  pub fn resolve_against(self, percentage_base: f32) -> Option<f32> {
    if !self.value.is_finite() || !percentage_base.is_finite() {
      return None;
    }
    if let Some(calc) = self.calc {
      if calc.has_viewport_relative() || calc.has_font_relative() {
        return None;
      }

      return calc.resolve(Some(percentage_base), 0.0, 0.0, 0.0, 0.0);
    }
    match self.unit {
      LengthUnit::Percent => Some((self.value / 100.0) * percentage_base),
      _ if self.unit.is_absolute() => Some(self.to_px()),
      _ => None,
    }
  }

  /// Resolves this length using a font size (for em/rem units)
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::Length;
  ///
  /// let length = Length::em(2.0);
  /// assert_eq!(length.resolve_with_font_size(16.0), Some(32.0));
  ///
  /// let rem_length = Length::rem(1.5);
  /// assert_eq!(rem_length.resolve_with_font_size(16.0), Some(24.0));
  ///
  /// // ex/ch fallback to 0.5em when font metrics are unavailable
  /// let ex_length = Length::ex(2.0);
  /// assert_eq!(ex_length.resolve_with_font_size(16.0), Some(16.0));
  /// ```
  pub fn resolve_with_font_size(self, font_size_px: f32) -> Option<f32> {
    if !self.value.is_finite() || !font_size_px.is_finite() {
      return None;
    }
    if let Some(calc) = self.calc {
      if calc.has_percentage() || calc.has_viewport_relative() {
        return None;
      }

      return calc.resolve(None, 0.0, 0.0, font_size_px, font_size_px);
    }
    match self.unit {
      LengthUnit::Em | LengthUnit::Rem => Some(self.value * font_size_px),
      // Approximate ex/ch with font metrics; fallback to 0.5em when actual x-height/zero-width is unknown.
      LengthUnit::Ex | LengthUnit::Ch => Some(self.value * font_size_px * 0.5),
      // Without the computed `line-height` property, treat `lh` as `normal` (1.2 * font-size).
      LengthUnit::Lh => Some(self.value * font_size_px * 1.2),
      _ if self.unit.is_absolute() => Some(self.to_px()),
      _ => None,
    }
  }

  /// Resolves this length using viewport dimensions.
  ///
  /// Returns `None` for units that cannot be resolved with viewport information alone
  /// (percentages, font-relative units, etc.).
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::{Length, LengthUnit};
  ///
  /// let length = Length::new(50.0, LengthUnit::Vw);
  /// assert_eq!(length.resolve_with_viewport(800.0, 600.0), Some(400.0));
  ///
  /// let vh_length = Length::new(50.0, LengthUnit::Vh);
  /// assert_eq!(vh_length.resolve_with_viewport(800.0, 600.0), Some(300.0));
  /// ```
  pub fn resolve_with_viewport(self, viewport_width: f32, viewport_height: f32) -> Option<f32> {
    if !self.value.is_finite() || !viewport_width.is_finite() || !viewport_height.is_finite() {
      return None;
    }
    if let Some(calc) = self.calc {
      if calc.has_percentage() || calc.has_font_relative() {
        return None;
      }

      return calc.resolve(None, viewport_width, viewport_height, 0.0, 0.0);
    }
    match self.unit {
      LengthUnit::Vw => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Vh => Some((self.value / 100.0) * viewport_height),
      LengthUnit::Vmin => Some((self.value / 100.0) * viewport_width.min(viewport_height)),
      LengthUnit::Vmax => Some((self.value / 100.0) * viewport_width.max(viewport_height)),
      LengthUnit::Dvw => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Dvh => Some((self.value / 100.0) * viewport_height),
      LengthUnit::Dvmin => Some((self.value / 100.0) * viewport_width.min(viewport_height)),
      LengthUnit::Dvmax => Some((self.value / 100.0) * viewport_width.max(viewport_height)),
      _ if self.unit.is_absolute() => Some(self.to_px()),
      _ => None,
    }
  }

  /// Resolves a length (including calc expressions) with all available context.
  ///
  /// Returns `None` when a percentage-based term lacks a base.
  pub fn resolve_with_context(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
  ) -> Option<f32> {
    if !self.value.is_finite() {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());
    let vw = if viewport_width.is_finite() {
      viewport_width
    } else {
      return None;
    };
    let vh = if viewport_height.is_finite() {
      viewport_height
    } else {
      return None;
    };
    let font_px = if font_size_px.is_finite() {
      font_size_px
    } else {
      return None;
    };
    let root_px = if root_font_size_px.is_finite() {
      root_font_size_px
    } else {
      return None;
    };

    if let Some(calc) = self.calc {
      return calc.resolve(percentage_base, vw, vh, font_px, root_px);
    }

    if self.unit.is_percentage() {
      percentage_base.map(|base| (self.value / 100.0) * base)
    } else if self.unit.is_viewport_relative() {
      self.resolve_with_viewport(vw, vh)
    } else if self.unit.is_font_relative() {
      self.resolve_with_font_size(if self.unit == LengthUnit::Rem {
        root_px
      } else {
        font_px
      })
    } else if self.unit.is_absolute() {
      Some(self.to_px())
    } else {
      Some(self.value)
    }
  }

  /// Returns true if this length (or any calc term) uses a percentage component.
  ///
  /// Percentages in the block axis require a containing block height to resolve,
  /// so callers can use this to decide whether available block-size changes should
  /// invalidate cached measurements.
  pub fn has_percentage(&self) -> bool {
    if let Some(calc) = self.calc {
      calc.has_percentage()
    } else {
      self.unit.is_percentage()
    }
  }

  /// Returns true if this is a zero length
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::Length;
  ///
  /// assert!(Length::px(0.0).is_zero());
  /// assert!(!Length::px(0.1).is_zero());
  /// ```
  pub fn is_zero(self) -> bool {
    if let Some(calc) = self.calc {
      return calc.is_zero();
    }
    self.value == 0.0
  }

  fn write_css(&self, out: &mut impl fmt::Write) -> fmt::Result {
    if let Some(calc) = self.calc {
      return calc.write_css(out);
    }
    write!(out, "{}{}", self.value, self.unit)
  }

  /// Serializes this length to CSS text, including any stored `calc()` expression.
  pub fn to_css(&self) -> String {
    let mut out = String::new();
    let _ = self.write_css(&mut out);
    out
  }
}

impl fmt::Display for Length {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.write_css(f)
  }
}

/// A CSS length value or the `auto` keyword
///
/// Many CSS properties accept either a specific length or `auto`,
/// which means "compute automatically based on context".
///
/// # Examples
///
/// ```
/// use fastrender::{LengthOrAuto, Length};
///
/// let auto_width = LengthOrAuto::Auto;
/// assert!(auto_width.is_auto());
///
/// let fixed_width = LengthOrAuto::Length(Length::px(100.0));
/// assert_eq!(fixed_width.to_px().unwrap(), 100.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LengthOrAuto {
  /// A specific length value
  Length(Length),
  /// The `auto` keyword
  Auto,
}

impl LengthOrAuto {
  /// Creates a length in pixels
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthOrAuto;
  ///
  /// let width = LengthOrAuto::px(100.0);
  /// assert_eq!(width.to_px().unwrap(), 100.0);
  /// ```
  pub const fn px(value: f32) -> Self {
    Self::Length(Length::px(value))
  }

  /// Creates a percentage value
  pub const fn percent(value: f32) -> Self {
    Self::Length(Length::percent(value))
  }

  /// Returns true if this is `auto`
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthOrAuto;
  ///
  /// assert!(LengthOrAuto::Auto.is_auto());
  /// assert!(!LengthOrAuto::px(100.0).is_auto());
  /// ```
  pub fn is_auto(self) -> bool {
    matches!(self, Self::Auto)
  }

  /// Returns the length if this is not auto
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::{LengthOrAuto, Length};
  ///
  /// let value = LengthOrAuto::px(100.0);
  /// assert_eq!(value.length(), Some(Length::px(100.0)));
  ///
  /// assert_eq!(LengthOrAuto::Auto.length(), None);
  /// ```
  pub fn length(self) -> Option<Length> {
    match self {
      Self::Length(length) => Some(length),
      Self::Auto => None,
    }
  }

  /// Converts to pixels if this is an absolute length, otherwise returns None
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthOrAuto;
  ///
  /// assert_eq!(LengthOrAuto::px(100.0).to_px(), Some(100.0));
  /// assert_eq!(LengthOrAuto::Auto.to_px(), None);
  /// ```
  pub fn to_px(self) -> Option<f32> {
    match self {
      Self::Length(length) if length.unit.is_absolute() => Some(length.to_px()),
      _ => None,
    }
  }

  /// Resolves this value against a percentage base
  ///
  /// Returns None if this is Auto.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthOrAuto;
  ///
  /// let percent = LengthOrAuto::percent(50.0);
  /// assert_eq!(percent.resolve_against(200.0), Some(100.0));
  ///
  /// assert_eq!(LengthOrAuto::Auto.resolve_against(200.0), None);
  /// ```
  pub fn resolve_against(self, percentage_base: f32) -> Option<f32> {
    if !percentage_base.is_finite() {
      return None;
    }
    self
      .length()
      .and_then(|length| length.resolve_against(percentage_base))
      .or_else(|| self.length().map(|length| length.to_px()))
  }

  /// Resolves this value, substituting a default for Auto
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::LengthOrAuto;
  ///
  /// assert_eq!(LengthOrAuto::px(100.0).resolve_or(50.0, 0.0), 100.0);
  /// assert_eq!(LengthOrAuto::Auto.resolve_or(50.0, 0.0), 50.0);
  /// ```
  pub fn resolve_or(self, default: f32, percentage_base: f32) -> f32 {
    match self {
      Self::Length(length) => length.resolve_against(percentage_base).unwrap_or(default),
      Self::Auto => default,
    }
  }
}

impl From<Length> for LengthOrAuto {
  fn from(length: Length) -> Self {
    Self::Length(length)
  }
}

impl fmt::Display for LengthOrAuto {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Length(length) => write!(f, "{}", length),
      Self::Auto => write!(f, "auto"),
    }
  }
}

// ============================================================================
// Custom property registration and typed values
// ============================================================================

/// Supported syntaxes for registered custom properties.
///
/// This mirrors a small subset of the Properties & Values API:
/// - Primitive syntaxes like `<length>`
/// - Unions via `|`
/// - List syntaxes via `+` (space-separated) and `#` (comma-separated)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomPropertySyntax {
  Length,
  /// `<length-percentage>` from the Properties & Values API, represented using `Length` since
  /// `LengthUnit::Percent` and `CalcLength` already model percentage components.
  LengthPercentage,
  Number,
  Percentage,
  Color,
  Angle,
  Universal,
  /// Union of multiple syntaxes, e.g. `<length> | <color>`.
  Union(Box<[CustomPropertySyntax]>),
  /// Space-separated list of values matching the inner syntax (`+` multiplier).
  SpaceSeparatedList(Box<CustomPropertySyntax>),
  /// Comma-separated list of values matching the inner syntax (`#` multiplier).
  CommaSeparatedList(Box<CustomPropertySyntax>),
}

/// Parsed typed value for a registered custom property.
#[derive(Debug, Clone, PartialEq)]
pub enum CustomPropertyTypedValue {
  Length(Length),
  Number(f32),
  Percentage(f32),
  Color(crate::style::color::Color),
  Angle(f32),
  List {
    separator: CustomPropertyListSeparator,
    items: Vec<CustomPropertyTypedValue>,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomPropertyListSeparator {
  Space,
  Comma,
}

impl CustomPropertyTypedValue {
  /// Serializes the typed value back to CSS text.
  pub fn to_css(&self) -> String {
    match self {
      CustomPropertyTypedValue::Length(len) => len.to_css(),
      CustomPropertyTypedValue::Number(n) => {
        if n.fract() == 0.0 {
          format!("{n:.0}")
        } else {
          n.to_string()
        }
      }
      CustomPropertyTypedValue::Percentage(p) => format!("{p}%"),
      CustomPropertyTypedValue::Color(c) => c.to_string(),
      CustomPropertyTypedValue::Angle(deg) => {
        if deg.fract() == 0.0 {
          format!("{deg:.0}deg")
        } else {
          format!("{deg}deg")
        }
      }
      CustomPropertyTypedValue::List { separator, items } => {
        let mut out = String::new();
        for (idx, item) in items.iter().enumerate() {
          if idx > 0 {
            match separator {
              CustomPropertyListSeparator::Space => out.push(' '),
              CustomPropertyListSeparator::Comma => out.push_str(", "),
            }
          }
          out.push_str(&item.to_css());
        }
        out
      }
    }
  }
}

/// Stored value for a custom property, optionally carrying a parsed typed value
/// when the property is registered.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomPropertyValue {
  pub value: String,
  pub typed: Option<CustomPropertyTypedValue>,
}

impl CustomPropertyValue {
  pub fn new(value: impl Into<String>, typed: Option<CustomPropertyTypedValue>) -> Self {
    Self {
      value: value.into(),
      typed,
    }
  }
}

#[inline]
fn is_ascii_whitespace_html_css(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

impl CustomPropertySyntax {
  pub fn is_universal(&self) -> bool {
    matches!(self, CustomPropertySyntax::Universal)
  }

  /// Parses a syntax descriptor such as `<length>` or `*`.
  pub fn parse(s: &str) -> Option<Self> {
    let s = trim_ascii_whitespace(strip_quotes_custom_property_syntax(s));
    if s == "*" {
      return Some(CustomPropertySyntax::Universal);
    }

    fn parse_primitive(token: &str) -> Option<CustomPropertySyntax> {
      if token.eq_ignore_ascii_case("<length>") {
        Some(CustomPropertySyntax::Length)
      } else if token.eq_ignore_ascii_case("<length-percentage>") {
        Some(CustomPropertySyntax::LengthPercentage)
      } else if token.eq_ignore_ascii_case("<number>") {
        Some(CustomPropertySyntax::Number)
      } else if token.eq_ignore_ascii_case("<percentage>") {
        Some(CustomPropertySyntax::Percentage)
      } else if token.eq_ignore_ascii_case("<color>") {
        Some(CustomPropertySyntax::Color)
      } else if token.eq_ignore_ascii_case("<angle>") {
        Some(CustomPropertySyntax::Angle)
      } else {
        None
      }
    }

    let mut members: SmallVec<[CustomPropertySyntax; 4]> = SmallVec::new();
    for part in s.split('|') {
      let part = trim_ascii_whitespace(part);
      if part.is_empty() {
        return None;
      }

      let mut multiplier = None;
      let mut core = part;
      if let Some(stripped) = core.strip_suffix('+') {
        multiplier = Some(CustomPropertyListSeparator::Space);
        core = stripped;
      } else if let Some(stripped) = core.strip_suffix('#') {
        multiplier = Some(CustomPropertyListSeparator::Comma);
        core = stripped;
      }
      core = trim_ascii_whitespace(core);
      let Some(mut syntax) = parse_primitive(core) else {
        return None;
      };

      if let Some(sep) = multiplier {
        syntax = match sep {
          CustomPropertyListSeparator::Space => {
            CustomPropertySyntax::SpaceSeparatedList(Box::new(syntax))
          }
          CustomPropertyListSeparator::Comma => {
            CustomPropertySyntax::CommaSeparatedList(Box::new(syntax))
          }
        };
      }

      members.push(syntax);
    }

    if members.is_empty() {
      return None;
    }
    if members.len() == 1 {
      return members.pop();
    }
    if members.iter().any(|m| m.is_universal()) {
      // `*` is only supported as a standalone descriptor.
      return None;
    }
    Some(CustomPropertySyntax::Union(members.into_vec().into_boxed_slice()))
  }

  /// Attempts to parse a value string according to this syntax.
  pub fn parse_value(&self, value: &str) -> Option<CustomPropertyTypedValue> {
    match self {
      CustomPropertySyntax::Length => {
        let parsed = crate::css::properties::parse_length(trim_ascii_whitespace(value))?;
        // `<length>` must reject percentage-based values (including `calc()` that contains a `%`
        // term). `parse_length` is a shared helper that accepts `<length-percentage>` values, so
        // enforce the stricter syntax here.
        if parsed.has_percentage() {
          return None;
        }
        Some(CustomPropertyTypedValue::Length(parsed))
      }
      CustomPropertySyntax::LengthPercentage => {
        crate::css::properties::parse_length(trim_ascii_whitespace(value)).map(CustomPropertyTypedValue::Length)
      }
      CustomPropertySyntax::Number => trim_ascii_whitespace(value)
        .parse()
        .ok()
        .map(CustomPropertyTypedValue::Number),
      CustomPropertySyntax::Percentage => {
        let trimmed = trim_ascii_whitespace(value);
        if let Some(percent) = trimmed.strip_suffix('%') {
          trim_ascii_whitespace(percent)
            .parse::<f32>()
            .ok()
            .map(CustomPropertyTypedValue::Percentage)
        } else if trimmed
          .parse::<f32>()
          .ok()
          .is_some_and(|value| value == 0.0)
        {
          // Mirror CSS' general "unitless zero" allowance so that registrations like
          // `@property --switch-position { syntax: "<percentage>"; initial-value: 0 }` are kept.
          Some(CustomPropertyTypedValue::Percentage(0.0))
        } else {
          None
        }
      }
      CustomPropertySyntax::Color => crate::style::color::Color::parse(trim_ascii_whitespace(value))
        .ok()
        .map(CustomPropertyTypedValue::Color),
      CustomPropertySyntax::Angle => {
        parse_angle_token(trim_ascii_whitespace(value)).map(CustomPropertyTypedValue::Angle)
      }
      CustomPropertySyntax::Universal => None,
      CustomPropertySyntax::Union(members) => members.iter().find_map(|m| m.parse_value(value)),
      CustomPropertySyntax::SpaceSeparatedList(inner) => {
        parse_custom_property_list_value(value, inner.as_ref(), CustomPropertyListSeparator::Space)
      }
      CustomPropertySyntax::CommaSeparatedList(inner) => {
        parse_custom_property_list_value(value, inner.as_ref(), CustomPropertyListSeparator::Comma)
      }
    }
  }
}

fn strip_quotes_custom_property_syntax(value: &str) -> &str {
  let value = trim_ascii_whitespace(value);
  if value.len() >= 2 {
    let bytes = value.as_bytes();
    let last = value.len() - 1;
    if (bytes[0] == b'"' && bytes[last] == b'"') || (bytes[0] == b'\'' && bytes[last] == b'\'') {
      return &value[1..last];
    }
  }
  value
}

fn parse_custom_property_list_value(
  raw: &str,
  inner: &CustomPropertySyntax,
  separator: CustomPropertyListSeparator,
) -> Option<CustomPropertyTypedValue> {
  let raw = trim_ascii_whitespace(raw);
  let mut items: Vec<CustomPropertyTypedValue> = Vec::new();

  match separator {
    CustomPropertyListSeparator::Comma => {
      let mut depth = 0i32;
      let mut bracket = 0i32;
      let mut brace = 0i32;
      let mut in_string: Option<u8> = None;
      let mut idx = 0usize;
      let bytes = raw.as_bytes();
      let mut start = 0usize;

      while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(quote) = in_string {
          if b == b'\\' {
            idx = idx.saturating_add(2);
            continue;
          }
          if b == quote {
            in_string = None;
          }
          idx += 1;
          continue;
        }

        if b == b'\\' {
          idx = idx.saturating_add(2);
          continue;
        }

        if b == b'/' && bytes.get(idx + 1) == Some(&b'*') {
          idx += 2;
          while idx + 1 < bytes.len() {
            if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
              idx += 2;
              break;
            }
            idx += 1;
          }
          // Comments behave like whitespace in CSS; keep scanning.
          continue;
        }

        match b {
          b'(' => depth += 1,
          b')' => depth = (depth - 1).max(0),
          b'[' => bracket += 1,
          b']' => bracket = (bracket - 1).max(0),
          b'{' => brace += 1,
          b'}' => brace = (brace - 1).max(0),
          b'\'' | b'"' => in_string = Some(b),
          b',' if depth == 0 && bracket == 0 && brace == 0 => {
            let part = trim_ascii_whitespace(&raw[start..idx]);
            if part.is_empty() {
              return None;
            }
            let typed = inner.parse_value(part)?;
            items.push(typed);
            start = idx + 1;
          }
          _ => {}
        }
        idx += 1;
      }

      let tail = trim_ascii_whitespace(&raw[start..]);
      if tail.is_empty() {
        return None;
      }
      items.push(inner.parse_value(tail)?);
    }
    CustomPropertyListSeparator::Space => {
      let mut depth = 0i32;
      let mut bracket = 0i32;
      let mut brace = 0i32;
      let mut in_string: Option<u8> = None;
      let mut idx = 0usize;
      let bytes = raw.as_bytes();
      let mut start = 0usize;

      while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(quote) = in_string {
          if b == b'\\' {
            idx = idx.saturating_add(2);
            continue;
          }
          if b == quote {
            in_string = None;
          }
          idx += 1;
          continue;
        }

        if b == b'\\' {
          idx = idx.saturating_add(2);
          continue;
        }

        if b == b'/' && bytes.get(idx + 1) == Some(&b'*') {
          let comment_start = idx;
          idx += 2;
          while idx + 1 < bytes.len() {
            if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
              idx += 2;
              break;
            }
            idx += 1;
          }
          if depth == 0 && bracket == 0 && brace == 0 {
            let part = trim_ascii_whitespace(&raw[start..comment_start]);
            if !part.is_empty() {
              items.push(inner.parse_value(part)?);
            }
            start = idx;
          }
          continue;
        }

        match b {
          b'(' => depth += 1,
          b')' => depth = (depth - 1).max(0),
          b'[' => bracket += 1,
          b']' => bracket = (bracket - 1).max(0),
          b'{' => brace += 1,
          b'}' => brace = (brace - 1).max(0),
          b'\'' | b'"' => in_string = Some(b),
          b'\t' | b'\n' | b'\r' | 0x0c | b' '
            if depth == 0 && bracket == 0 && brace == 0 =>
          {
            let part = trim_ascii_whitespace(&raw[start..idx]);
            if !part.is_empty() {
              items.push(inner.parse_value(part)?);
            }
            idx += 1;
            while idx < bytes.len()
              && matches!(bytes[idx], b'\t' | b'\n' | b'\r' | 0x0c | b' ')
            {
              idx += 1;
            }
            start = idx;
            continue;
          }
          _ => {}
        }
        idx += 1;
      }

      let tail = trim_ascii_whitespace(&raw[start..]);
      if !tail.is_empty() {
        items.push(inner.parse_value(tail)?);
      }
    }
  }

  if items.is_empty() {
    return None;
  }
  Some(CustomPropertyTypedValue::List { separator, items })
}

fn parse_angle_token(token: &str) -> Option<f32> {
  let trimmed = trim_ascii_whitespace(token);
  if trimmed.ends_with("deg") {
    trim_ascii_whitespace(&trimmed[..trimmed.len() - 3])
      .parse::<f32>()
      .ok()
  } else if trimmed.ends_with("rad") {
    trim_ascii_whitespace(&trimmed[..trimmed.len() - 3])
      .parse::<f32>()
      .ok()
      .map(|r| r.to_degrees())
  } else if trimmed.ends_with("turn") {
    trim_ascii_whitespace(&trimmed[..trimmed.len() - 4])
      .parse::<f32>()
      .ok()
      .map(|t| t * 360.0)
  } else if trimmed.ends_with("grad") {
    trim_ascii_whitespace(&trimmed[..trimmed.len() - 4])
      .parse::<f32>()
      .ok()
      .map(|g| g * 0.9)
  } else {
    None
  }
}
