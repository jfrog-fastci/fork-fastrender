//! CSS value types
//!
//! This module provides types for representing CSS values in their computed form.
//! These types are used throughout the style and layout systems.
//!
//! # Units
//!
//! CSS supports various length units. We categorize them as:
//! - **Absolute**: px, pt, pc, in, cm, mm
//! - **Font-relative**: em, rem, ex, ch, lh, cap, ic, rex, rch, rcap, ric, rlh
//! - **Viewport-relative**: vw, vh, vmin, vmax
//! - **Percentages**: Relative to containing block or font size
//!
//! Reference: CSS Values and Units Module Level 3
//! <https://www.w3.org/TR/css-values-3/>

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::fmt;
use std::sync::OnceLock;

use smallvec::SmallVec;

use crate::geometry::Size;
use crate::style::color::Rgba;
use crate::style::RootFontMetrics;
use cssparser::{ParseError, Parser, ParserInput, ToCss, Token};

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

  /// Cap-height units (cap) - relative to the cap-height of the font.
  Cap,

  /// Ideographic character units (ic) - relative to the inline-axis advance of a representative
  /// ideograph (typically U+6C34 '水').
  Ic,

  /// Root-ex units (rex) - relative to the root element's x-height.
  Rex,

  /// Root-ch units (rch) - relative to the root element's '0' advance.
  Rch,

  /// Root-cap units (rcap) - relative to the root element's cap-height.
  Rcap,

  /// Root-ic units (ric) - relative to the root element's ideograph advance.
  Ric,

  /// Root line-height units (rlh) - relative to the root element's computed line-height.
  Rlh,

  /// Viewport width percentage (vw) - 1% of viewport width
  Vw,

  /// Viewport height percentage (vh) - 1% of viewport height
  Vh,

  /// Viewport inline size percentage (vi) - 1% of viewport size in the box's inline axis
  ///
  /// CSS Values & Units 4 defines `vi` to resolve against the *inline axis* of the box's writing
  /// mode. In a horizontal writing mode this matches `vw`; in vertical writing modes it matches
  /// `vh`.
  Vi,

  /// Viewport block size percentage (vb) - 1% of viewport size in the box's block axis
  ///
  /// Resolves against the *block axis* of the box's writing mode. In a horizontal writing mode
  /// this matches `vh`; in vertical writing modes it matches `vw`.
  Vb,

  /// Viewport minimum (vmin) - 1% of smaller viewport dimension
  Vmin,

  /// Viewport maximum (vmax) - 1% of larger viewport dimension
  Vmax,

  /// Dynamic viewport width (dvw) - responds to UA UI changes
  Dvw,

  /// Dynamic viewport height (dvh)
  Dvh,

  /// Dynamic viewport inline size (dvi)
  Dvi,

  /// Dynamic viewport block size (dvb)
  Dvb,

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

  /// Returns true if this is a font-relative unit (em, rem, ex, ch, lh, cap, ic, rex, rch, rcap, ric, rlh)
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
    matches!(
      self,
      Self::Em
        | Self::Rem
        | Self::Ex
        | Self::Ch
        | Self::Lh
        | Self::Cap
        | Self::Ic
        | Self::Rex
        | Self::Rch
        | Self::Rcap
        | Self::Ric
        | Self::Rlh
    )
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
        | Self::Vi
        | Self::Vb
        | Self::Vmin
        | Self::Vmax
        | Self::Dvw
        | Self::Dvh
        | Self::Dvi
        | Self::Dvb
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
      Self::Cap => "cap",
      Self::Ic => "ic",
      Self::Rex => "rex",
      Self::Rch => "rch",
      Self::Rcap => "rcap",
      Self::Ric => "ric",
      Self::Rlh => "rlh",
      Self::Lh => "lh",
      Self::Vw => "vw",
      Self::Vh => "vh",
      Self::Vi => "vi",
      Self::Vb => "vb",
      Self::Vmin => "vmin",
      Self::Vmax => "vmax",
      Self::Dvw => "dvw",
      Self::Dvh => "dvh",
      Self::Dvi => "dvi",
      Self::Dvb => "dvb",
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

/// Calculated `<length>` value produced by CSS math functions such as `calc()`, `min()`, `max()`,
/// and `clamp()`.
///
/// FastRender stores most math as a **linear combination** of unit coefficients (e.g.
/// `50% + 10px - 2vw`). Modern sites also rely on non-linear selection functions like `max()`,
/// which cannot be represented as a single linear combination. To support those without heap
/// allocation, `CalcLength` can also encode a `min()`/`max()`/`clamp()` function whose arguments are
/// themselves linear combinations.
///
/// The encoding uses the same fixed-size term array, with `LengthUnit::Calc` acting as an
/// **argument separator** between linear argument term lists. This keeps `CalcLength` `Copy` while
/// still allowing common responsive expressions such as:
/// `max(1rem, calc(50vw - 720px + 1rem))`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalcLength {
  kind: CalcLengthKind,
  terms: [CalcTerm; MAX_CALC_TERMS],
  term_count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalcLengthKind {
  /// A plain `calc()` sum (linear combination of units).
  Linear,
  /// `min(<calc-sum>#)`
  Min,
  /// `max(<calc-sum>#)`
  Max,
  /// `clamp(<calc-sum>, <calc-sum>, <calc-sum>)`
  Clamp,
}

const ARG_SEPARATOR_TERM: CalcTerm = CalcTerm {
  unit: LengthUnit::Calc,
  value: 0.0,
};

// ============================================================================
// Non-linear calc/min/max/clamp support for <length-percentage>
// ============================================================================

/// Identifier for an interned non-linear `<length-percentage>` expression (e.g. `max(10px, 5vw)`).
///
/// These are stored in a global arena so `Length` can stay `Copy` while supporting expressions
/// that cannot be represented as a linear [`CalcLength`] combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CalcLengthExprId(u32);

impl CalcLengthExprId {
  #[inline]
  pub(crate) fn index(self) -> u32 {
    self.0
  }
}

/// `<length-percentage>` computed value for `calc()`/`min()`/`max()`/`clamp()`.
///
/// - [`LengthCalc::Linear`] stores a linear combination of units (`CalcLength`) as we did
///   historically.
/// - [`LengthCalc::Expr`] references an interned expression tree for non-linear cases such as
///   `max(180px, calc(100vw - 170px))`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LengthCalc {
  Linear(CalcLength),
  Expr(CalcLengthExprId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MinMaxOp {
  Min,
  Max,
}

#[derive(Debug, Clone)]
pub(crate) enum LengthCalcExpr {
  Add {
    left: LengthCalc,
    right: LengthCalc,
    /// Either `1.0` or `-1.0` (corresponding to `+` / `-`).
    sign: f32,
  },
  Scale {
    value: LengthCalc,
    factor: f32,
  },
  MinMax {
    op: MinMaxOp,
    values: Vec<LengthCalc>,
  },
  Clamp {
    min: LengthCalc,
    preferred: LengthCalc,
    max: LengthCalc,
  },
}

#[derive(Default, Debug, Clone, Copy)]
struct LengthCalcFlags {
  has_percentage: bool,
  has_viewport_relative: bool,
  has_font_relative: bool,
  has_container_query_relative: bool,
}

impl LengthCalcFlags {
  fn or(self, other: Self) -> Self {
    Self {
      has_percentage: self.has_percentage || other.has_percentage,
      has_viewport_relative: self.has_viewport_relative || other.has_viewport_relative,
      has_font_relative: self.has_font_relative || other.has_font_relative,
      has_container_query_relative: self.has_container_query_relative
        || other.has_container_query_relative,
    }
  }
}

#[derive(Debug, Clone)]
struct LengthCalcExprEntry {
  expr: LengthCalcExpr,
  /// Key used for interning/equality.
  key: String,
  /// Canonical CSS serialization of this expression.
  css_text: String,
  flags: LengthCalcFlags,
}

#[derive(Default)]
struct LengthCalcExprArena {
  entries: Vec<LengthCalcExprEntry>,
  index: FxHashMap<String, CalcLengthExprId>,
}

static LENGTH_CALC_EXPR_ARENA: OnceLock<RwLock<LengthCalcExprArena>> = OnceLock::new();

fn length_calc_expr_arena() -> &'static RwLock<LengthCalcExprArena> {
  LENGTH_CALC_EXPR_ARENA.get_or_init(|| RwLock::new(LengthCalcExprArena::default()))
}

// ============================================================================
// calc-size() expression interning (CSS Values 5)
// ============================================================================

/// Identifier for an interned `calc-size()` `<calc-sum>` expression.
///
/// `calc-size()` expressions are stored in a global arena so sizing keywords can stay `Copy` while
/// supporting arbitrary calc-sums containing the `size` placeholder token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CalcSizeExprId(u32);

#[derive(Debug, Clone)]
struct CalcSizeExprEntry {
  css_text: String,
  has_percentage: bool,
}

#[derive(Default)]
struct CalcSizeExprArena {
  entries: Vec<CalcSizeExprEntry>,
  index: FxHashMap<String, CalcSizeExprId>,
}

static CALC_SIZE_EXPR_ARENA: OnceLock<RwLock<CalcSizeExprArena>> = OnceLock::new();

fn calc_size_expr_arena() -> &'static RwLock<CalcSizeExprArena> {
  CALC_SIZE_EXPR_ARENA.get_or_init(|| RwLock::new(CalcSizeExprArena::default()))
}

pub(crate) fn intern_calc_size_expr(expr: &str) -> CalcSizeExprId {
  let arena_lock = calc_size_expr_arena();
  let mut arena = arena_lock.write();
  if let Some(id) = arena.index.get(expr) {
    return *id;
  }
  let id = CalcSizeExprId(arena.entries.len() as u32);
  arena.index.insert(expr.to_string(), id);
  arena.entries.push(CalcSizeExprEntry {
    css_text: expr.to_string(),
    has_percentage: expr.contains('%'),
  });
  id
}

pub(crate) fn calc_size_expr_css_text(id: CalcSizeExprId) -> String {
  let arena_lock = calc_size_expr_arena();
  let arena = arena_lock.read();
  arena
    .entries
    .get(id.0 as usize)
    .map(|e| e.css_text.clone())
    .unwrap_or_else(|| "size".to_string())
}

pub(crate) fn calc_size_expr_has_percentage(id: CalcSizeExprId) -> bool {
  let arena_lock = calc_size_expr_arena();
  let arena = arena_lock.read();
  arena
    .entries
    .get(id.0 as usize)
    .map(|e| e.has_percentage)
    .unwrap_or(false)
}

fn substitute_calc_size_tokens<'i, 't>(
  parser: &mut Parser<'i, 't>,
  replacement: &str,
) -> Result<String, ParseError<'i, ()>> {
  let mut output = String::new();
  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Ident(ident) if ident.eq_ignore_ascii_case("size") => {
        output.push_str(replacement);
      }
      Token::Function(name) => {
        output.push_str(name.as_ref());
        output.push('(');
        let nested =
          parser.parse_nested_block(|nested| substitute_calc_size_tokens(nested, replacement))?;
        output.push_str(&nested);
        output.push(')');
      }
      Token::ParenthesisBlock => {
        output.push('(');
        let nested =
          parser.parse_nested_block(|nested| substitute_calc_size_tokens(nested, replacement))?;
        output.push_str(&nested);
        output.push(')');
      }
      Token::SquareBracketBlock => {
        output.push('[');
        let nested =
          parser.parse_nested_block(|nested| substitute_calc_size_tokens(nested, replacement))?;
        output.push_str(&nested);
        output.push(']');
      }
      Token::CurlyBracketBlock => {
        output.push('{');
        let nested =
          parser.parse_nested_block(|nested| substitute_calc_size_tokens(nested, replacement))?;
        output.push_str(&nested);
        output.push('}');
      }
      other => {
        other.to_css(&mut output).map_err(|_| parser.new_custom_error::<(), ()>(()))?;
      }
    }
  }
  Ok(output)
}

/// Replace all `size` identifier tokens in a `calc-size()` calc-sum.
///
/// Returns a raw calc-sum string (not wrapped in `calc(...)`).
pub fn substitute_calc_size_expr(expr: &str, size_px: f32) -> Option<String> {
  if !size_px.is_finite() {
    return None;
  }
  let replacement = Length::px(size_px).to_css();
  let mut input = ParserInput::new(expr);
  let mut parser = Parser::new(&mut input);
  substitute_calc_size_tokens(&mut parser, &replacement).ok()
}

/// Returns the stored `calc-size()` `<calc-sum>` expression with all `size` identifier tokens
/// replaced by `<size_px>px`.
///
/// The returned string is a raw calc-sum (not wrapped in `calc(...)`).
pub fn calc_size_expr_with_size(id: CalcSizeExprId, size_px: f32) -> Option<String> {
  let expr = calc_size_expr_css_text(id);
  substitute_calc_size_expr(&expr, size_px)
}

fn linear_flags(calc: &CalcLength) -> LengthCalcFlags {
  LengthCalcFlags {
    has_percentage: calc.has_percentage(),
    has_viewport_relative: calc.has_viewport_relative(),
    has_font_relative: calc.has_font_relative(),
    has_container_query_relative: calc.has_container_query_relative(),
  }
}

fn value_flags(value: LengthCalc, arena: &LengthCalcExprArena) -> LengthCalcFlags {
  match value {
    LengthCalc::Linear(calc) => linear_flags(&calc),
    LengthCalc::Expr(id) => arena
      .entries
      .get(id.0 as usize)
      .map(|e| e.flags)
      .unwrap_or_default(),
  }
}

fn value_key(value: LengthCalc, arena: &LengthCalcExprArena) -> String {
  match value {
    LengthCalc::Linear(calc) => calc.to_css(),
    LengthCalc::Expr(id) => arena
      .entries
      .get(id.0 as usize)
      .map(|e| e.key.clone())
      .unwrap_or_else(|| "invalid-expr".to_string()),
  }
}

fn value_css(value: LengthCalc, arena: &LengthCalcExprArena) -> String {
  match value {
    LengthCalc::Linear(calc) => calc.to_css(),
    LengthCalc::Expr(id) => arena
      .entries
      .get(id.0 as usize)
      .map(|e| e.css_text.clone())
      .unwrap_or_else(|| "calc(0)".to_string()),
  }
}

fn intern_length_calc_expr(expr: LengthCalcExpr) -> CalcLengthExprId {
  // Build key + css text under a write lock so we can reuse existing entries' strings without
  // additional locking.
  let arena_lock = length_calc_expr_arena();
  let mut arena = arena_lock.write();

  // Sites often use nested `min()`/`max()` forms as a clamp polyfill:
  // - `max(MIN, min(PREFERRED, MAX))`
  // - `min(MAX, max(MIN, PREFERRED))`
  //
  // Canonicalize these to a single `clamp()` expression so `to_css()` produces stable output and
  // downstream code doesn't need to special-case the polyfill.
  let expr = match expr {
    LengthCalcExpr::MinMax { op, values } => {
      let lookup_two_args = |id: CalcLengthExprId| -> Option<(MinMaxOp, [LengthCalc; 2])> {
        let entry = arena.entries.get(id.index() as usize)?;
        let LengthCalcExpr::MinMax { op, values } = &entry.expr else {
          return None;
        };
        if values.len() != 2 {
          return None;
        }
        Some((*op, [values[0], values[1]]))
      };

      if values.len() == 2 {
        match op {
          MinMaxOp::Max => {
            let mut min_bound: Option<LengthCalc> = None;
            let mut inner_min: Option<[LengthCalc; 2]> = None;

            // `max(MIN, min(PREFERRED, MAX))` (either argument order).
            if let LengthCalc::Expr(id) = values[0] {
              if let Some((inner_op, inner_values)) = lookup_two_args(id) {
                if inner_op == MinMaxOp::Min {
                  min_bound = Some(values[1]);
                  inner_min = Some(inner_values);
                }
              }
            }
            if min_bound.is_none() {
              if let LengthCalc::Expr(id) = values[1] {
                if let Some((inner_op, inner_values)) = lookup_two_args(id) {
                  if inner_op == MinMaxOp::Min {
                    min_bound = Some(values[0]);
                    inner_min = Some(inner_values);
                  }
                }
              }
            }

            if let (Some(min), Some([preferred, max])) = (min_bound, inner_min) {
              LengthCalcExpr::Clamp {
                min,
                preferred,
                max,
              }
            } else {
              LengthCalcExpr::MinMax { op, values }
            }
          }
          MinMaxOp::Min => {
            let mut max_bound: Option<LengthCalc> = None;
            let mut inner_max: Option<[LengthCalc; 2]> = None;

            // `min(MAX, max(MIN, PREFERRED))` (either argument order).
            if let LengthCalc::Expr(id) = values[0] {
              if let Some((inner_op, inner_values)) = lookup_two_args(id) {
                if inner_op == MinMaxOp::Max {
                  max_bound = Some(values[1]);
                  inner_max = Some(inner_values);
                }
              }
            }
            if max_bound.is_none() {
              if let LengthCalc::Expr(id) = values[1] {
                if let Some((inner_op, inner_values)) = lookup_two_args(id) {
                  if inner_op == MinMaxOp::Max {
                    max_bound = Some(values[0]);
                    inner_max = Some(inner_values);
                  }
                }
              }
            }

            if let (Some(max), Some([min, preferred])) = (max_bound, inner_max) {
              LengthCalcExpr::Clamp {
                min,
                preferred,
                max,
              }
            } else {
              LengthCalcExpr::MinMax { op, values }
            }
          }
        }
      } else {
        LengthCalcExpr::MinMax { op, values }
      }
    }
    other => other,
  };

  let (key, css_text, flags) = match &expr {
    LengthCalcExpr::Add { left, right, sign } => {
      let left_key = value_key(*left, &arena);
      let right_key = value_key(*right, &arena);
      let left_css = value_css(*left, &arena);
      let right_css = value_css(*right, &arena);
      let op = if *sign < 0.0 { '-' } else { '+' };
      let key = format!("add({left_key},{op},{right_key})");
      let css_text = format!("calc({left_css} {op} {right_css})");
      let flags = value_flags(*left, &arena).or(value_flags(*right, &arena));
      (key, css_text, flags)
    }
    LengthCalcExpr::Scale { value, factor } => {
      let child_key = value_key(*value, &arena);
      let child_css = value_css(*value, &arena);
      let factor_bits = factor.to_bits();
      let key = format!("scale({factor_bits},{child_key})");
      // Wrap the child in `calc()` so the result stays a valid calc expression even when the child
      // is a bare length like `10px`.
      let css_text = format!("calc({factor} * ({child_css}))");
      let flags = value_flags(*value, &arena);
      (key, css_text, flags)
    }
    LengthCalcExpr::MinMax { op, values } => {
      let op_name = match op {
        MinMaxOp::Min => "min",
        MinMaxOp::Max => "max",
      };
      let mut key = String::new();
      key.push_str(op_name);
      key.push('(');
      let mut css_text = String::new();
      css_text.push_str(op_name);
      css_text.push('(');
      let mut flags = LengthCalcFlags::default();
      for (idx, v) in values.iter().enumerate() {
        if idx > 0 {
          key.push(',');
          css_text.push_str(", ");
        }
        key.push_str(&value_key(*v, &arena));
        css_text.push_str(&value_css(*v, &arena));
        flags = flags.or(value_flags(*v, &arena));
      }
      key.push(')');
      css_text.push(')');
      (key, css_text, flags)
    }
    LengthCalcExpr::Clamp {
      min,
      preferred,
      max,
    } => {
      let min_key = value_key(*min, &arena);
      let pref_key = value_key(*preferred, &arena);
      let max_key = value_key(*max, &arena);
      let min_css = value_css(*min, &arena);
      let pref_css = value_css(*preferred, &arena);
      let max_css = value_css(*max, &arena);
      let key = format!("clamp({min_key},{pref_key},{max_key})");
      let css_text = format!("clamp({min_css}, {pref_css}, {max_css})");
      let flags = value_flags(*min, &arena)
        .or(value_flags(*preferred, &arena))
        .or(value_flags(*max, &arena));
      (key, css_text, flags)
    }
  };

  if let Some(existing) = arena.index.get(&key) {
    return *existing;
  }

  let id = CalcLengthExprId(arena.entries.len().min(u32::MAX as usize) as u32);
  arena.entries.push(LengthCalcExprEntry {
    expr,
    key: key.clone(),
    css_text,
    flags,
  });
  arena.index.insert(key, id);
  id
}

impl LengthCalc {
  pub fn has_percentage(&self) -> bool {
    match self {
      LengthCalc::Linear(calc) => calc.has_percentage(),
      LengthCalc::Expr(id) => length_calc_expr_arena()
        .read()
        .entries
        .get(id.0 as usize)
        .is_some_and(|e| e.flags.has_percentage),
    }
  }

  pub fn has_viewport_relative(&self) -> bool {
    match self {
      LengthCalc::Linear(calc) => calc.has_viewport_relative(),
      LengthCalc::Expr(id) => length_calc_expr_arena()
        .read()
        .entries
        .get(id.0 as usize)
        .is_some_and(|e| e.flags.has_viewport_relative),
    }
  }

  pub fn has_font_relative(&self) -> bool {
    match self {
      LengthCalc::Linear(calc) => calc.has_font_relative(),
      LengthCalc::Expr(id) => length_calc_expr_arena()
        .read()
        .entries
        .get(id.0 as usize)
        .is_some_and(|e| e.flags.has_font_relative),
    }
  }

  pub fn has_container_query_relative(&self) -> bool {
    match self {
      LengthCalc::Linear(calc) => calc.has_container_query_relative(),
      LengthCalc::Expr(id) => length_calc_expr_arena()
        .read()
        .entries
        .get(id.0 as usize)
        .is_some_and(|e| e.flags.has_container_query_relative),
    }
  }

  pub fn to_css(&self) -> String {
    match self {
      LengthCalc::Linear(calc) => calc.to_css(),
      LengthCalc::Expr(id) => length_calc_expr_arena()
        .read()
        .entries
        .get(id.0 as usize)
        .map(|e| e.css_text.clone())
        .unwrap_or_else(|| "calc(0)".to_string()),
    }
  }

  pub fn resolve(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
  ) -> Option<f32> {
    self.resolve_with_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      None,
    )
  }

  pub fn resolve_with_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    match self {
      LengthCalc::Linear(calc) => calc.resolve_with_root_font_metrics(
        percentage_base,
        viewport_width,
        viewport_height,
        font_size_px,
        root_font_size_px,
        root_font_metrics,
      ),
      LengthCalc::Expr(id) => resolve_length_calc_expr(
        LengthCalc::Expr(*id),
        percentage_base,
        viewport_width,
        viewport_height,
        font_size_px,
        root_font_size_px,
        root_font_metrics,
      ),
    }
  }

  pub(crate) fn resolve_for_inline_axis(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    inline_axis_is_horizontal: bool,
  ) -> Option<f32> {
    self.resolve_for_inline_axis_with_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      inline_axis_is_horizontal,
      None,
    )
  }

  pub(crate) fn resolve_for_inline_axis_with_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    inline_axis_is_horizontal: bool,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    match self {
      LengthCalc::Linear(calc) => calc.resolve_for_inline_axis_with_root_font_metrics(
        percentage_base,
        viewport_width,
        viewport_height,
        font_size_px,
        root_font_size_px,
        inline_axis_is_horizontal,
        root_font_metrics,
      ),
      LengthCalc::Expr(_) => resolve_length_calc_with_resolver(
        *self,
        percentage_base,
        viewport_width,
        viewport_height,
        font_size_px,
        root_font_size_px,
        &|calc, pct, vw, vh, font_px, root_px| {
          calc.resolve_for_inline_axis_with_root_font_metrics(
            pct,
            vw,
            vh,
            font_px,
            root_px,
            inline_axis_is_horizontal,
            root_font_metrics,
          )
        },
      ),
    }
  }

  pub fn absolute_sum(&self) -> Option<f32> {
    match self {
      LengthCalc::Linear(calc) => calc.absolute_sum(),
      LengthCalc::Expr(_) => None,
    }
  }

  pub fn terms(&self) -> Option<&[CalcTerm]> {
    match self {
      LengthCalc::Linear(calc) => Some(calc.terms()),
      LengthCalc::Expr(_) => None,
    }
  }
}

pub(crate) fn length_calc_add_scaled(
  left: LengthCalc,
  right: LengthCalc,
  sign: f32,
) -> Option<LengthCalc> {
  match (left, right) {
    (LengthCalc::Linear(l), LengthCalc::Linear(r)) => {
      l.add_scaled(&r, sign).map(LengthCalc::Linear)
    }
    _ => Some(LengthCalc::Expr(intern_length_calc_expr(
      LengthCalcExpr::Add { left, right, sign },
    ))),
  }
}

pub(crate) fn length_calc_scale(value: LengthCalc, factor: f32) -> Option<LengthCalc> {
  match value {
    LengthCalc::Linear(calc) => calc.scale(factor).map(LengthCalc::Linear),
    _ => Some(LengthCalc::Expr(intern_length_calc_expr(
      LengthCalcExpr::Scale { value, factor },
    ))),
  }
}

pub(crate) fn length_calc_min_max(op: MinMaxOp, values: Vec<LengthCalc>) -> LengthCalc {
  LengthCalc::Expr(intern_length_calc_expr(LengthCalcExpr::MinMax {
    op,
    values,
  }))
}

pub(crate) fn length_calc_clamp(
  min: LengthCalc,
  preferred: LengthCalc,
  max: LengthCalc,
) -> LengthCalc {
  LengthCalc::Expr(intern_length_calc_expr(LengthCalcExpr::Clamp {
    min,
    preferred,
    max,
  }))
}

pub(crate) fn resolve_length_calc_with_resolver(
  value: LengthCalc,
  percentage_base: Option<f32>,
  viewport_width: f32,
  viewport_height: f32,
  font_size_px: f32,
  root_font_size_px: f32,
  resolve_linear: &impl Fn(&CalcLength, Option<f32>, f32, f32, f32, f32) -> Option<f32>,
) -> Option<f32> {
  fn resolve_inner(
    value: LengthCalc,
    arena: &LengthCalcExprArena,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    resolve_linear: &impl Fn(&CalcLength, Option<f32>, f32, f32, f32, f32) -> Option<f32>,
    depth: u32,
  ) -> Option<f32> {
    // Guard against pathological self-referential expression graphs (shouldn't happen with the
    // parser, but avoid unbounded recursion).
    if depth > 128 {
      return None;
    }

    match value {
      LengthCalc::Linear(calc) => resolve_linear(
        &calc,
        percentage_base,
        viewport_width,
        viewport_height,
        font_size_px,
        root_font_size_px,
      ),
      LengthCalc::Expr(id) => {
        let entry = arena.entries.get(id.index() as usize)?;
        match &entry.expr {
          LengthCalcExpr::Add { left, right, sign } => {
            let l = resolve_inner(
              *left,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let r = resolve_inner(
              *right,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let out = l + r * *sign;
            out.is_finite().then_some(out)
          }
          LengthCalcExpr::Scale { value, factor } => {
            let v = resolve_inner(
              *value,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let out = v * *factor;
            out.is_finite().then_some(out)
          }
          LengthCalcExpr::MinMax { op, values } => {
            let mut iter = values.iter();
            let first = *iter.next()?;
            let mut extremum = resolve_inner(
              first,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            for v in iter {
              let resolved = resolve_inner(
                *v,
                arena,
                percentage_base,
                viewport_width,
                viewport_height,
                font_size_px,
                root_font_size_px,
                resolve_linear,
                depth + 1,
              )?;
              extremum = match op {
                MinMaxOp::Min => extremum.min(resolved),
                MinMaxOp::Max => extremum.max(resolved),
              };
            }
            extremum.is_finite().then_some(extremum)
          }
          LengthCalcExpr::Clamp {
            min,
            preferred,
            max,
          } => {
            let min_v = resolve_inner(
              *min,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let pref_v = resolve_inner(
              *preferred,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let max_v = resolve_inner(
              *max,
              arena,
              percentage_base,
              viewport_width,
              viewport_height,
              font_size_px,
              root_font_size_px,
              resolve_linear,
              depth + 1,
            )?;
            let upper = if max_v < min_v { min_v } else { max_v };
            let out = pref_v.max(min_v).min(upper);
            out.is_finite().then_some(out)
          }
        }
      }
    }
  }

  let arena_guard = length_calc_expr_arena().read();
  resolve_inner(
    value,
    &arena_guard,
    percentage_base,
    viewport_width,
    viewport_height,
    font_size_px,
    root_font_size_px,
    resolve_linear,
    0,
  )
}

fn resolve_length_calc_expr(
  value: LengthCalc,
  percentage_base: Option<f32>,
  viewport_width: f32,
  viewport_height: f32,
  font_size_px: f32,
  root_font_size_px: f32,
  root_font_metrics: Option<RootFontMetrics>,
) -> Option<f32> {
  resolve_length_calc_with_resolver(
    value,
    percentage_base,
    viewport_width,
    viewport_height,
    font_size_px,
    root_font_size_px,
    &|calc, percentage_base, vw, vh, font_px, root_px| {
      calc.resolve_with_root_font_metrics(
        percentage_base,
        vw,
        vh,
        font_px,
        root_px,
        root_font_metrics,
      )
    },
  )
}

fn resolve_length_calc_container_query_units(
  value: LengthCalc,
  cqw_base: f32,
  cqh_base: f32,
  cqi_base: f32,
  cqb_base: f32,
) -> LengthCalc {
  fn inner(
    value: LengthCalc,
    cqw_base: f32,
    cqh_base: f32,
    cqi_base: f32,
    cqb_base: f32,
    depth: u32,
  ) -> LengthCalc {
    if depth > 128 {
      return LengthCalc::Linear(CalcLength::empty());
    }

    match value {
      LengthCalc::Linear(calc) => LengthCalc::Linear(
        calc.resolve_container_query_units(cqw_base, cqh_base, cqi_base, cqb_base),
      ),
      LengthCalc::Expr(id) => {
        let expr = {
          let arena_guard = length_calc_expr_arena().read();
          let Some(entry) = arena_guard.entries.get(id.index() as usize) else {
            return LengthCalc::Linear(CalcLength::empty());
          };
          entry.expr.clone()
        };

        match expr {
          LengthCalcExpr::Add { left, right, sign } => {
            let left = inner(left, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            let right = inner(right, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            length_calc_add_scaled(left, right, sign)
              .unwrap_or(LengthCalc::Linear(CalcLength::empty()))
          }
          LengthCalcExpr::Scale { value, factor } => {
            let value = inner(value, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            length_calc_scale(value, factor).unwrap_or(LengthCalc::Linear(CalcLength::empty()))
          }
          LengthCalcExpr::MinMax { op, values } => {
            let values = values
              .into_iter()
              .map(|v| inner(v, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1))
              .collect();
            length_calc_min_max(op, values)
          }
          LengthCalcExpr::Clamp {
            min,
            preferred,
            max,
          } => {
            let min = inner(min, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            let preferred = inner(preferred, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            let max = inner(max, cqw_base, cqh_base, cqi_base, cqb_base, depth + 1);
            length_calc_clamp(min, preferred, max)
          }
        }
      }
    }
  }

  inner(value, cqw_base, cqh_base, cqi_base, cqb_base, 0)
}

impl CalcLength {
  pub const fn empty() -> Self {
    Self {
      kind: CalcLengthKind::Linear,
      terms: [EMPTY_TERM; MAX_CALC_TERMS],
      term_count: 0,
    }
  }

  pub fn single(unit: LengthUnit, value: f32) -> Self {
    let mut calc = Self::empty();
    let _ = calc.push(unit, value);
    calc
  }

  #[inline]
  pub(crate) fn kind(&self) -> CalcLengthKind {
    self.kind
  }

  #[inline]
  pub(crate) fn kind_id(&self) -> u8 {
    match self.kind {
      CalcLengthKind::Linear => 0,
      CalcLengthKind::Min => 1,
      CalcLengthKind::Max => 2,
      CalcLengthKind::Clamp => 3,
    }
  }

  pub(crate) fn min_function(args: &[CalcLength]) -> Option<Self> {
    Self::build_function(CalcLengthKind::Min, args)
  }

  pub(crate) fn max_function(args: &[CalcLength]) -> Option<Self> {
    Self::build_function(CalcLengthKind::Max, args)
  }

  pub(crate) fn clamp_function(
    min: CalcLength,
    preferred: CalcLength,
    max: CalcLength,
  ) -> Option<Self> {
    Self::build_function(CalcLengthKind::Clamp, &[min, preferred, max])
  }

  fn build_function(kind: CalcLengthKind, args: &[CalcLength]) -> Option<Self> {
    debug_assert!(kind != CalcLengthKind::Linear);
    if args.is_empty() {
      return None;
    }
    if kind == CalcLengthKind::Clamp && args.len() != 3 {
      return None;
    }
    if args.len() == 1 {
      // `min()`/`max()` with one argument behaves as identity.
      return Some(args[0]);
    }

    let mut out = Self {
      kind,
      terms: [EMPTY_TERM; MAX_CALC_TERMS],
      term_count: 0,
    };

    let mut len = 0usize;
    for (idx, arg) in args.iter().enumerate() {
      if arg.kind != CalcLengthKind::Linear {
        // Avoid recursive function trees for now. Callers should flatten when possible.
        return None;
      }

      if idx > 0 {
        if len >= MAX_CALC_TERMS {
          return None;
        }
        out.terms[len] = ARG_SEPARATOR_TERM;
        len += 1;
      }

      for term in arg.terms() {
        if term.unit == LengthUnit::Calc {
          return None;
        }
        if len >= MAX_CALC_TERMS {
          return None;
        }
        out.terms[len] = *term;
        len += 1;
      }
    }

    out.term_count = len as u8;
    Some(out)
  }

  fn map_linear_args(
    &self,
    out_kind: CalcLengthKind,
    mut map: impl FnMut(CalcLength) -> Option<CalcLength>,
  ) -> Option<Self> {
    debug_assert!(self.kind != CalcLengthKind::Linear);
    debug_assert!(out_kind != CalcLengthKind::Linear);

    let mut out = Self {
      kind: out_kind,
      terms: [EMPTY_TERM; MAX_CALC_TERMS],
      term_count: 0,
    };

    let src = self.terms();
    let mut start = 0usize;
    let mut arg_index = 0usize;
    let mut len = 0usize;

    for i in 0..=src.len() {
      let is_boundary = i == src.len() || src[i].unit == LengthUnit::Calc;
      if !is_boundary {
        continue;
      }

      let mut arg = CalcLength::empty();
      for term in &src[start..i] {
        arg.push(term.unit, term.value).ok()?;
      }

      let mapped = map(arg)?;
      if mapped.kind != CalcLengthKind::Linear {
        return None;
      }

      if arg_index > 0 {
        if len >= MAX_CALC_TERMS {
          return None;
        }
        out.terms[len] = ARG_SEPARATOR_TERM;
        len += 1;
      }

      for term in mapped.terms() {
        if len >= MAX_CALC_TERMS {
          return None;
        }
        out.terms[len] = *term;
        len += 1;
      }

      arg_index += 1;
      start = i + 1;
    }

    out.term_count = len as u8;
    Some(out)
  }

  pub fn terms(&self) -> &[CalcTerm] {
    &self.terms[..self.term_count as usize]
  }

  fn push(&mut self, unit: LengthUnit, value: f32) -> Result<(), ()> {
    if self.kind != CalcLengthKind::Linear {
      return Err(());
    }
    if unit == LengthUnit::Calc {
      // Reserved for argument separators in non-linear min/max/clamp encodings.
      return Err(());
    }
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

  pub fn scale(&self, factor: f32) -> Option<Self> {
    if factor == 0.0 {
      return Some(Self::empty());
    }

    match self.kind {
      CalcLengthKind::Linear => {
        let mut out = Self::empty();
        for term in self.terms() {
          out.push(term.unit, term.value * factor).ok()?;
        }
        Some(out)
      }
      CalcLengthKind::Min | CalcLengthKind::Max => {
        let mut kind = self.kind;
        if factor.is_sign_negative() {
          kind = match kind {
            CalcLengthKind::Min => CalcLengthKind::Max,
            CalcLengthKind::Max => CalcLengthKind::Min,
            _ => kind,
          };
        }

        let mut out = Self {
          kind,
          terms: [EMPTY_TERM; MAX_CALC_TERMS],
          term_count: 0,
        };
        let mut len = 0usize;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if len >= MAX_CALC_TERMS {
              return None;
            }
            out.terms[len] = ARG_SEPARATOR_TERM;
            len += 1;
            continue;
          }

          let value = term.value * factor;
          if value == 0.0 {
            continue;
          }
          if len >= MAX_CALC_TERMS {
            return None;
          }
          out.terms[len] = CalcTerm {
            unit: term.unit,
            value,
          };
          len += 1;
        }
        out.term_count = len as u8;
        Some(out)
      }
      CalcLengthKind::Clamp => {
        if factor.is_sign_negative() {
          // `clamp()` is not closed under negative scaling without introducing nested min/max.
          // Reject so callers treat the expression as invalid rather than computing the wrong value.
          return None;
        }

        let mut out = Self {
          kind: CalcLengthKind::Clamp,
          terms: [EMPTY_TERM; MAX_CALC_TERMS],
          term_count: 0,
        };
        let mut len = 0usize;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if len >= MAX_CALC_TERMS {
              return None;
            }
            out.terms[len] = ARG_SEPARATOR_TERM;
            len += 1;
            continue;
          }

          let value = term.value * factor;
          if value == 0.0 {
            continue;
          }
          if len >= MAX_CALC_TERMS {
            return None;
          }
          out.terms[len] = CalcTerm {
            unit: term.unit,
            value,
          };
          len += 1;
        }
        out.term_count = len as u8;
        Some(out)
      }
    }
  }

  pub fn add_scaled(&self, other: &CalcLength, scale: f32) -> Option<Self> {
    if scale == 0.0 {
      return Some(*self);
    }

    match (self.kind, other.kind) {
      (CalcLengthKind::Linear, CalcLengthKind::Linear) => {
        let mut out = *self;
        for term in other.terms() {
          if out.push(term.unit, term.value * scale).is_err() {
            return None;
          }
        }
        Some(out)
      }
      (
        CalcLengthKind::Linear,
        CalcLengthKind::Min | CalcLengthKind::Max | CalcLengthKind::Clamp,
      ) => {
        // `L + s * f(args)` where `L` is linear and `f` is min/max/clamp.
        // Distribute the affine transform into the function arguments when possible.
        if other.kind == CalcLengthKind::Clamp && scale.is_sign_negative() {
          // `clamp()` is not closed under negative scaling without nesting.
          return None;
        }

        let mut out_kind = other.kind;
        if scale.is_sign_negative() {
          out_kind = match out_kind {
            CalcLengthKind::Min => CalcLengthKind::Max,
            CalcLengthKind::Max => CalcLengthKind::Min,
            _ => out_kind,
          };
        }

        other.map_linear_args(out_kind, |arg| self.add_scaled(&arg, scale))
      }
      (
        CalcLengthKind::Min | CalcLengthKind::Max | CalcLengthKind::Clamp,
        CalcLengthKind::Linear,
      ) => {
        // `f(args) + s * L` where `L` is linear. Translation is closed for min/max/clamp.
        self.map_linear_args(self.kind, |arg| arg.add_scaled(other, scale))
      }
      _ => None,
    }
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

    match self.kind {
      CalcLengthKind::Linear => {
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
      _ => {
        let mut out = Self {
          kind: self.kind,
          terms: [EMPTY_TERM; MAX_CALC_TERMS],
          term_count: 0,
        };

        let mut len = 0usize;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if len < MAX_CALC_TERMS {
              out.terms[len] = ARG_SEPARATOR_TERM;
              len += 1;
            }
            continue;
          }

          let (unit, value) = match term.unit {
            LengthUnit::Cqw => (LengthUnit::Px, (term.value / 100.0) * cqw_base),
            LengthUnit::Cqh => (LengthUnit::Px, (term.value / 100.0) * cqh_base),
            LengthUnit::Cqi => (LengthUnit::Px, (term.value / 100.0) * cqi_base),
            LengthUnit::Cqb => (LengthUnit::Px, (term.value / 100.0) * cqb_base),
            LengthUnit::Cqmin => (LengthUnit::Px, (term.value / 100.0) * cqmin_base),
            LengthUnit::Cqmax => (LengthUnit::Px, (term.value / 100.0) * cqmax_base),
            _ => (term.unit, term.value),
          };

          if value == 0.0 {
            continue;
          }
          if len >= MAX_CALC_TERMS {
            break;
          }
          out.terms[len] = CalcTerm { unit, value };
          len += 1;
        }

        out.term_count = len as u8;
        out
      }
    }
  }

  pub fn resolve(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
  ) -> Option<f32> {
    self.resolve_with_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      None,
    )
  }

  pub fn resolve_with_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    let needs_viewport = self.has_viewport_relative();
    if (needs_viewport && (!viewport_width.is_finite() || !viewport_height.is_finite()))
      || !font_size_px.is_finite()
      || !root_font_size_px.is_finite()
    {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());

    let resolve_term = |term: &CalcTerm| -> Option<f32> {
      match term.unit {
        LengthUnit::Percent => percentage_base.map(|base| (term.value / 100.0) * base),
        u if u.is_absolute() => Some(Length::new(term.value, u).to_px()),
        u if u.is_viewport_relative() => {
          Length::new(term.value, u).resolve_with_viewport(viewport_width, viewport_height)
        }
        LengthUnit::Em => Some(term.value * font_size_px),
        LengthUnit::Ex | LengthUnit::Ch => Some(term.value * font_size_px * 0.5),
        LengthUnit::Cap => Some(term.value * font_size_px * 0.7),
        LengthUnit::Ic => Some(term.value * font_size_px),
        LengthUnit::Rem => Some(term.value * root_font_size_px),
        LengthUnit::Rex => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_x_height_px)
              .unwrap_or(root_font_size_px * 0.5),
        ),
        LengthUnit::Rch => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_ch_advance_px)
              .unwrap_or(root_font_size_px * 0.5),
        ),
        LengthUnit::Rcap => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_cap_height_px)
              .unwrap_or(root_font_size_px * 0.7),
        ),
        LengthUnit::Ric => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_ic_advance_px)
              .unwrap_or(root_font_size_px),
        ),
        LengthUnit::Rlh => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_used_line_height_px)
              .unwrap_or(root_font_size_px * 1.2),
        ),
        // Without access to computed `line-height`, fall back to the `normal` approximation.
        // Layout code that has access to `ComputedStyle` should resolve `lh` more accurately.
        LengthUnit::Lh => Some(term.value * font_size_px * 1.2),
        LengthUnit::Calc => None,
        _ => None,
      }
    };

    match self.kind {
      CalcLengthKind::Linear => {
        let mut total = 0.0;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            return None;
          }
          total += resolve_term(term)?;
        }
        Some(total)
      }
      CalcLengthKind::Min | CalcLengthKind::Max => {
        let is_min = self.kind == CalcLengthKind::Min;
        let mut extremum = if is_min {
          f32::INFINITY
        } else {
          f32::NEG_INFINITY
        };
        let mut current = 0.0;
        let mut saw_any = false;

        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            extremum = if is_min {
              extremum.min(current)
            } else {
              extremum.max(current)
            };
            current = 0.0;
            saw_any = true;
            continue;
          }
          current += resolve_term(term)?;
        }

        // Final argument
        extremum = if is_min {
          extremum.min(current)
        } else {
          extremum.max(current)
        };
        if !saw_any && !extremum.is_finite() {
          return None;
        }
        Some(extremum)
      }
      CalcLengthKind::Clamp => {
        let mut values = [0.0f32; 3];
        let mut arg_index = 0usize;
        let mut current = 0.0;

        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if arg_index >= 3 {
              return None;
            }
            values[arg_index] = current;
            arg_index += 1;
            current = 0.0;
            continue;
          }
          current += resolve_term(term)?;
        }

        if arg_index != 2 {
          return None;
        }
        values[2] = current;

        let min = values[0];
        let preferred = values[1];
        let max = values[2];
        Some(min.max(preferred.min(max)))
      }
    }
  }

  pub(crate) fn resolve_for_inline_axis(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    inline_axis_is_horizontal: bool,
  ) -> Option<f32> {
    self.resolve_for_inline_axis_with_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      inline_axis_is_horizontal,
      None,
    )
  }

  pub(crate) fn resolve_for_inline_axis_with_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    inline_axis_is_horizontal: bool,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    let needs_viewport = self.has_viewport_relative();
    if (needs_viewport && (!viewport_width.is_finite() || !viewport_height.is_finite()))
      || !font_size_px.is_finite()
      || !root_font_size_px.is_finite()
    {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());

    let resolve_viewport = |unit: LengthUnit, value: f32| -> Option<f32> {
      let factor = value / 100.0;
      match unit {
        LengthUnit::Vw | LengthUnit::Dvw => Some(factor * viewport_width),
        LengthUnit::Vh | LengthUnit::Dvh => Some(factor * viewport_height),
        LengthUnit::Vi | LengthUnit::Dvi => Some(
          factor
            * if inline_axis_is_horizontal {
              viewport_width
            } else {
              viewport_height
            },
        ),
        LengthUnit::Vb | LengthUnit::Dvb => Some(
          factor
            * if inline_axis_is_horizontal {
              viewport_height
            } else {
              viewport_width
            },
        ),
        LengthUnit::Vmin | LengthUnit::Dvmin => Some(factor * viewport_width.min(viewport_height)),
        LengthUnit::Vmax | LengthUnit::Dvmax => Some(factor * viewport_width.max(viewport_height)),
        _ => None,
      }
    };

    let resolve_term = |term: &CalcTerm| -> Option<f32> {
      match term.unit {
        LengthUnit::Percent => percentage_base.map(|base| (term.value / 100.0) * base),
        u if u.is_absolute() => Some(Length::new(term.value, u).to_px()),
        u if u.is_viewport_relative() => resolve_viewport(u, term.value),
        LengthUnit::Em => Some(term.value * font_size_px),
        LengthUnit::Ex | LengthUnit::Ch => Some(term.value * font_size_px * 0.5),
        LengthUnit::Cap => Some(term.value * font_size_px * 0.7),
        LengthUnit::Ic => Some(term.value * font_size_px),
        LengthUnit::Rem => Some(term.value * root_font_size_px),
        LengthUnit::Rex => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_x_height_px)
              .unwrap_or(root_font_size_px * 0.5),
        ),
        LengthUnit::Rch => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_ch_advance_px)
              .unwrap_or(root_font_size_px * 0.5),
        ),
        LengthUnit::Rcap => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_cap_height_px)
              .unwrap_or(root_font_size_px * 0.7),
        ),
        LengthUnit::Ric => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_ic_advance_px)
              .unwrap_or(root_font_size_px),
        ),
        LengthUnit::Rlh => Some(
          term.value
            * root_font_metrics
              .map(|m| m.root_used_line_height_px)
              .unwrap_or(root_font_size_px * 1.2),
        ),
        // Without access to computed `line-height`, fall back to the `normal` approximation.
        // Layout code that has access to `ComputedStyle` should resolve `lh` more accurately.
        LengthUnit::Lh => Some(term.value * font_size_px * 1.2),
        LengthUnit::Calc => None,
        _ => None,
      }
    };

    match self.kind {
      CalcLengthKind::Linear => {
        let mut total = 0.0;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            return None;
          }
          total += resolve_term(term)?;
        }
        Some(total)
      }
      CalcLengthKind::Min | CalcLengthKind::Max => {
        let mut extremum = if self.kind == CalcLengthKind::Min {
          f32::INFINITY
        } else {
          debug_assert!(self.kind == CalcLengthKind::Max);
          f32::NEG_INFINITY
        };
        let mut current = 0.0;
        let mut saw_any = false;

        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            extremum = match self.kind {
              CalcLengthKind::Min => extremum.min(current),
              CalcLengthKind::Max => extremum.max(current),
              _ => extremum,
            };
            current = 0.0;
            saw_any = true;
            continue;
          }
          current += resolve_term(term)?;
        }

        extremum = match self.kind {
          CalcLengthKind::Min => extremum.min(current),
          CalcLengthKind::Max => extremum.max(current),
          _ => extremum,
        };
        if !saw_any && !extremum.is_finite() {
          return None;
        }
        Some(extremum)
      }
      CalcLengthKind::Clamp => {
        let mut values = [0.0f32; 3];
        let mut arg_index = 0usize;
        let mut current = 0.0;

        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if arg_index >= 3 {
              return None;
            }
            values[arg_index] = current;
            arg_index += 1;
            current = 0.0;
            continue;
          }
          current += resolve_term(term)?;
        }

        if arg_index != 2 {
          return None;
        }
        values[2] = current;

        let min = values[0];
        let preferred = values[1];
        let max = values[2];
        Some(min.max(preferred.min(max)))
      }
    }
  }

  pub fn single_term(&self) -> Option<CalcTerm> {
    if self.kind == CalcLengthKind::Linear && self.term_count == 1 {
      Some(self.terms[0])
    } else {
      None
    }
  }

  pub fn requires_context(&self) -> bool {
    self
      .terms()
      .iter()
      .any(|t| t.unit.is_percentage() || t.unit.is_viewport_relative() || t.unit.is_font_relative())
  }

  pub fn absolute_sum(&self) -> Option<f32> {
    match self.kind {
      CalcLengthKind::Linear => {
        let mut total = 0.0;
        for term in self.terms() {
          match term.unit {
            u if u.is_absolute() => total += Length::new(term.value, u).to_px(),
            _ => return None,
          }
        }
        Some(total)
      }
      CalcLengthKind::Min | CalcLengthKind::Max => {
        let is_min = self.kind == CalcLengthKind::Min;
        let mut extremum = if is_min {
          f32::INFINITY
        } else {
          f32::NEG_INFINITY
        };
        let mut current = 0.0;
        let mut saw_sep = false;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            extremum = if is_min {
              extremum.min(current)
            } else {
              extremum.max(current)
            };
            current = 0.0;
            saw_sep = true;
            continue;
          }
          if !term.unit.is_absolute() {
            return None;
          }
          current += Length::new(term.value, term.unit).to_px();
        }
        extremum = if is_min {
          extremum.min(current)
        } else {
          extremum.max(current)
        };
        if !saw_sep && !extremum.is_finite() {
          return None;
        }
        Some(extremum)
      }
      CalcLengthKind::Clamp => {
        let mut values = [0.0f32; 3];
        let mut arg_index = 0usize;
        let mut current = 0.0;
        for term in self.terms() {
          if term.unit == LengthUnit::Calc {
            if arg_index >= 3 {
              return None;
            }
            values[arg_index] = current;
            arg_index += 1;
            current = 0.0;
            continue;
          }
          if !term.unit.is_absolute() {
            return None;
          }
          current += Length::new(term.value, term.unit).to_px();
        }
        if arg_index != 2 {
          return None;
        }
        values[2] = current;
        let min = values[0];
        let preferred = values[1];
        let max = values[2];
        Some(min.max(preferred.min(max)))
      }
    }
  }

  fn write_css(&self, out: &mut impl fmt::Write) -> fmt::Result {
    if self.is_zero() {
      // Unitless zero is valid for `<length>` and `<length-percentage>`.
      return out.write_str("0");
    }

    if self.kind != CalcLengthKind::Linear {
      let func_name = match self.kind {
        CalcLengthKind::Min => "min(",
        CalcLengthKind::Max => "max(",
        CalcLengthKind::Clamp => "clamp(",
        CalcLengthKind::Linear => {
          debug_assert!(false, "calc-length kind should not be linear here");
          "calc("
        }
      };

      out.write_str(func_name)?;

      let mut first = true;
      let mut current = CalcLength::empty();
      for term in self.terms() {
        if term.unit == LengthUnit::Calc {
          if !first {
            out.write_str(", ")?;
          }
          current.write_css(out)?;
          current = CalcLength::empty();
          first = false;
          continue;
        }
        let _ = current.push(term.unit, term.value);
      }

      if !first {
        out.write_str(", ")?;
      }
      current.write_css(out)?;
      out.write_str(")")?;
      return Ok(());
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
    assert!(LengthUnit::Cap.is_font_relative());
    assert!(LengthUnit::Ic.is_font_relative());
    assert!(LengthUnit::Rex.is_font_relative());
    assert!(LengthUnit::Rch.is_font_relative());
    assert!(LengthUnit::Rcap.is_font_relative());
    assert!(LengthUnit::Ric.is_font_relative());
    assert!(LengthUnit::Rlh.is_font_relative());
    assert!(LengthUnit::Lh.is_font_relative());

    assert!(LengthUnit::Vw.is_viewport_relative());
    assert!(LengthUnit::Vh.is_viewport_relative());
    assert!(LengthUnit::Vi.is_viewport_relative());
    assert!(LengthUnit::Vb.is_viewport_relative());

    assert!(LengthUnit::Percent.is_percentage());
  }

  #[test]
  fn test_length_unit_as_str() {
    assert_eq!(LengthUnit::Px.as_str(), "px");
    assert_eq!(LengthUnit::Em.as_str(), "em");
    assert_eq!(LengthUnit::Cap.as_str(), "cap");
    assert_eq!(LengthUnit::Ic.as_str(), "ic");
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
  fn percent_and_calc_percent_px_resolve_without_finite_viewport() {
    let percent = Length::percent(50.0);
    assert_eq!(
      percent.resolve_with_context_for_writing_mode(
        Some(200.0),
        f32::INFINITY,
        600.0,
        16.0,
        16.0,
        crate::style::types::WritingMode::VerticalRl
      ),
      Some(100.0)
    );

    let calc = parse_length("calc(50% + 10px)").expect("expected calc length to parse");
    assert_eq!(
      calc.resolve_with_context_for_writing_mode(
        Some(200.0),
        f32::INFINITY,
        f32::INFINITY,
        16.0,
        16.0,
        crate::style::types::WritingMode::HorizontalTb
      ),
      Some(110.0)
    );
  }

  #[test]
  fn calc_with_viewport_units_still_requires_finite_viewport() {
    let calc = parse_length("calc(50% + 10vw)").expect("expected calc length to parse");
    assert_eq!(
      calc.resolve_with_context_for_writing_mode(
        Some(200.0),
        f32::INFINITY,
        600.0,
        16.0,
        16.0,
        crate::style::types::WritingMode::HorizontalTb
      ),
      None
    );
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

    let cap = Length::new(2.0, LengthUnit::Cap);
    assert_eq!(cap.resolve_with_font_size(10.0), Some(14.0));

    let ic = Length::new(2.0, LengthUnit::Ic);
    assert_eq!(ic.resolve_with_font_size(10.0), Some(20.0));
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

    // `vi`/`vb` use the initial writing-mode (horizontal-tb) when resolving without element
    // context, so they map to `vw`/`vh`.
    let vi = Length::new(50.0, LengthUnit::Vi);
    assert_eq!(vi.resolve_with_viewport(800.0, 600.0), Some(400.0));
    let vb = Length::new(50.0, LengthUnit::Vb);
    assert_eq!(vb.resolve_with_viewport(800.0, 600.0), Some(300.0));

    assert_eq!(
      Length::percent(50.0).resolve_with_viewport(800.0, 600.0),
      None
    );
    assert_eq!(Length::em(2.0).resolve_with_viewport(800.0, 600.0), None);
  }

  #[test]
  fn test_vi_vb_viewport_resolution_respects_writing_mode() {
    let vi = Length::new(50.0, LengthUnit::Vi);
    let vb = Length::new(50.0, LengthUnit::Vb);

    assert_eq!(
      vi.resolve_with_viewport_for_writing_mode(
        800.0,
        600.0,
        crate::style::types::WritingMode::HorizontalTb
      ),
      Some(400.0)
    );
    assert_eq!(
      vb.resolve_with_viewport_for_writing_mode(
        800.0,
        600.0,
        crate::style::types::WritingMode::HorizontalTb
      ),
      Some(300.0)
    );

    // In vertical writing modes, the inline axis runs vertically (height) and the block axis runs
    // horizontally (width).
    assert_eq!(
      vi.resolve_with_viewport_for_writing_mode(
        800.0,
        600.0,
        crate::style::types::WritingMode::VerticalRl
      ),
      Some(300.0)
    );
    assert_eq!(
      vb.resolve_with_viewport_for_writing_mode(
        800.0,
        600.0,
        crate::style::types::WritingMode::VerticalRl
      ),
      Some(400.0)
    );
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
  /// Optional calc()/min()/max()/clamp() expression (takes precedence over `value`/`unit`)
  pub calc: Option<LengthCalc>,
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
      calc: Some(LengthCalc::Linear(calc)),
    }
  }

  pub(crate) fn calc_expr(expr: CalcLengthExprId) -> Self {
    Self {
      value: 0.0,
      unit: LengthUnit::Calc,
      calc: Some(LengthCalc::Expr(expr)),
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
      match calc {
        LengthCalc::Linear(calc) => {
          if let Some(abs) = calc.absolute_sum() {
            return abs;
          }
          // Best-effort fallback when context is missing: treat unresolved units as raw values.
          return calc.terms().iter().map(|t| t.value).sum();
        }
        LengthCalc::Expr(id) => {
          return resolve_length_calc_expr(LengthCalc::Expr(id), None, 0.0, 0.0, 0.0, 0.0, None)
            .unwrap_or(0.0);
        }
      }
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
      match calc {
        LengthCalc::Linear(calc) => {
          let resolved = calc.resolve_container_query_units(cqw_base, cqh_base, cqi_base, cqb_base);
          if resolved.is_zero() {
            return Length::px(0.0);
          }
          if let Some(term) = resolved.single_term() {
            return Length::new(term.value, term.unit);
          }
          return Length::calc(resolved);
        }
        LengthCalc::Expr(_) => {
          if !calc.has_container_query_relative() {
            return self;
          }

          let resolved =
            resolve_length_calc_container_query_units(calc, cqw_base, cqh_base, cqi_base, cqb_base);
          match resolved {
            LengthCalc::Linear(calc) => {
              if calc.is_zero() {
                return Length::px(0.0);
              }
              if let Some(term) = calc.single_term() {
                return Length::new(term.value, term.unit);
              }
              return Length::calc(calc);
            }
            LengthCalc::Expr(id) => return Length::calc_expr(id),
          }
        }
      }
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

      return match calc {
        LengthCalc::Linear(calc) => calc.resolve(Some(percentage_base), 0.0, 0.0, 0.0, 0.0),
        LengthCalc::Expr(_) => self.resolve_with_context(Some(percentage_base), 0.0, 0.0, 0.0, 0.0),
      };
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

      let term_is_root_font_relative = |unit: LengthUnit| {
        matches!(
          unit,
          LengthUnit::Rex | LengthUnit::Rch | LengthUnit::Rcap | LengthUnit::Ric | LengthUnit::Rlh
        )
      };

      let calc_contains_root_font_relative = match calc {
        LengthCalc::Linear(calc) => calc
          .terms()
          .iter()
          .any(|t| t.value != 0.0 && term_is_root_font_relative(t.unit)),
        LengthCalc::Expr(id) => {
          let arena_lock = length_calc_expr_arena();
          let arena = arena_lock.read();

          fn has_root_terms(
            value: LengthCalc,
            arena: &LengthCalcExprArena,
            term_is_root_font_relative: &impl Fn(LengthUnit) -> bool,
            depth: u32,
          ) -> bool {
            if depth > 128 {
              // Cycle/degenerate expression; be conservative and require full context.
              return true;
            }
            match value {
              LengthCalc::Linear(calc) => calc
                .terms()
                .iter()
                .any(|t| t.value != 0.0 && term_is_root_font_relative(t.unit)),
              LengthCalc::Expr(id) => {
                let Some(entry) = arena.entries.get(id.index() as usize) else {
                  return true;
                };
                match &entry.expr {
                  LengthCalcExpr::Add { left, right, .. } => {
                    has_root_terms(*left, arena, term_is_root_font_relative, depth + 1)
                      || has_root_terms(*right, arena, term_is_root_font_relative, depth + 1)
                  }
                  LengthCalcExpr::Scale { value, .. } => {
                    has_root_terms(*value, arena, term_is_root_font_relative, depth + 1)
                  }
                  LengthCalcExpr::MinMax { values, .. } => values
                    .iter()
                    .copied()
                    .any(|v| has_root_terms(v, arena, term_is_root_font_relative, depth + 1)),
                  LengthCalcExpr::Clamp {
                    min,
                    preferred,
                    max,
                  } => {
                    has_root_terms(*min, arena, term_is_root_font_relative, depth + 1)
                      || has_root_terms(*preferred, arena, term_is_root_font_relative, depth + 1)
                      || has_root_terms(*max, arena, term_is_root_font_relative, depth + 1)
                  }
                }
              }
            }
          }

          has_root_terms(LengthCalc::Expr(id), &arena, &term_is_root_font_relative, 0)
        }
      };

      if calc_contains_root_font_relative {
        // Root font-relative units (rex/rch/rcap/ric/rlh) require access to the root font size.
        // `resolve_with_font_size` only has a single font size parameter, so refuse to guess.
        return None;
      }

      return match calc {
        LengthCalc::Linear(calc) => calc.resolve(None, 0.0, 0.0, font_size_px, font_size_px),
        LengthCalc::Expr(_) => {
          self.resolve_with_context(None, 0.0, 0.0, font_size_px, font_size_px)
        }
      };
    }
    match self.unit {
      LengthUnit::Em | LengthUnit::Rem => Some(self.value * font_size_px),
      // Approximate ex/ch with font metrics; fallback to 0.5em when actual x-height/zero-width is unknown.
      LengthUnit::Ex | LengthUnit::Ch => Some(self.value * font_size_px * 0.5),
      LengthUnit::Cap => Some(self.value * font_size_px * 0.7),
      LengthUnit::Ic => Some(self.value * font_size_px),
      LengthUnit::Rex | LengthUnit::Rch => Some(self.value * font_size_px * 0.5),
      LengthUnit::Rcap => Some(self.value * font_size_px * 0.7),
      LengthUnit::Ric => Some(self.value * font_size_px),
      // Without the computed `line-height` property, treat `lh`/`rlh` as `normal` (1.2 * font-size).
      LengthUnit::Lh | LengthUnit::Rlh => Some(self.value * font_size_px * 1.2),
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

      return match calc {
        LengthCalc::Linear(calc) => calc.resolve(None, viewport_width, viewport_height, 0.0, 0.0),
        LengthCalc::Expr(_) => {
          self.resolve_with_context(None, viewport_width, viewport_height, 0.0, 0.0)
        }
      };
    }
    match self.unit {
      LengthUnit::Vw => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Vh => Some((self.value / 100.0) * viewport_height),
      // In style-less contexts (e.g. media queries), the spec resolves `vi`/`vb` using the initial
      // `writing-mode` value, which is horizontal-tb (inline axis = viewport width).
      LengthUnit::Vi => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Vb => Some((self.value / 100.0) * viewport_height),
      LengthUnit::Vmin => Some((self.value / 100.0) * viewport_width.min(viewport_height)),
      LengthUnit::Vmax => Some((self.value / 100.0) * viewport_width.max(viewport_height)),
      LengthUnit::Dvw => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Dvh => Some((self.value / 100.0) * viewport_height),
      LengthUnit::Dvi => Some((self.value / 100.0) * viewport_width),
      LengthUnit::Dvb => Some((self.value / 100.0) * viewport_height),
      LengthUnit::Dvmin => Some((self.value / 100.0) * viewport_width.min(viewport_height)),
      LengthUnit::Dvmax => Some((self.value / 100.0) * viewport_width.max(viewport_height)),
      _ if self.unit.is_absolute() => Some(self.to_px()),
      _ => None,
    }
  }

  /// Resolves this length using viewport dimensions and the box's computed `writing-mode`.
  ///
  /// This differs from [`Length::resolve_with_viewport`] only for `vi`/`vb` (and their dynamic
  /// variants), which are relative to the box's inline/block axes.
  pub fn resolve_with_viewport_for_writing_mode(
    self,
    viewport_width: f32,
    viewport_height: f32,
    writing_mode: crate::style::types::WritingMode,
  ) -> Option<f32> {
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(writing_mode);
    if !self.value.is_finite() || !viewport_width.is_finite() || !viewport_height.is_finite() {
      return None;
    }
    if let Some(calc) = self.calc {
      if calc.has_percentage() || calc.has_font_relative() {
        return None;
      }

      return calc.resolve_for_inline_axis(
        None,
        viewport_width,
        viewport_height,
        0.0,
        0.0,
        inline_is_horizontal,
      );
    }

    match self.unit {
      LengthUnit::Vi => Some(
        (self.value / 100.0)
          * if inline_is_horizontal {
            viewport_width
          } else {
            viewport_height
          },
      ),
      LengthUnit::Vb => Some(
        (self.value / 100.0)
          * if inline_is_horizontal {
            viewport_height
          } else {
            viewport_width
          },
      ),
      LengthUnit::Dvi => Some(
        (self.value / 100.0)
          * if inline_is_horizontal {
            viewport_width
          } else {
            viewport_height
          },
      ),
      LengthUnit::Dvb => Some(
        (self.value / 100.0)
          * if inline_is_horizontal {
            viewport_height
          } else {
            viewport_width
          },
      ),
      // Delegate all other viewport units to the axis-agnostic resolver.
      _ if self.unit.is_viewport_relative() => {
        self.resolve_with_viewport(viewport_width, viewport_height)
      }
      _ if self.unit.is_absolute() => Some(self.to_px()),
      _ => None,
    }
  }

  /// Writing-mode-aware counterpart to [`Length::resolve_with_context`].
  pub fn resolve_with_context_for_writing_mode(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    writing_mode: crate::style::types::WritingMode,
  ) -> Option<f32> {
    self.resolve_with_context_for_writing_mode_and_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      writing_mode,
      None,
    )
  }

  pub fn resolve_with_context_for_writing_mode_and_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    writing_mode: crate::style::types::WritingMode,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    if !self.value.is_finite() {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());
    let needs_viewport =
      self.unit.is_viewport_relative() || self.calc.is_some_and(|calc| calc.has_viewport_relative());
    let (vw, vh) = if needs_viewport {
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
      (vw, vh)
    } else {
      // Viewport units are not present, but lower-level calc resolvers still expect finite
      // viewport sizes. Provide dummy finite values so percent/calc(% + px) can resolve even in
      // "indefinite viewport" contexts (e.g. intrinsic sizing passes).
      (0.0, 0.0)
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

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(writing_mode);

    if let Some(calc) = self.calc {
      return calc.resolve_for_inline_axis_with_root_font_metrics(
        percentage_base,
        vw,
        vh,
        font_px,
        root_px,
        inline_is_horizontal,
        root_font_metrics,
      );
    }

    if self.unit.is_percentage() {
      percentage_base.map(|base| (self.value / 100.0) * base)
    } else if self.unit.is_viewport_relative() {
      self.resolve_with_viewport_for_writing_mode(vw, vh, writing_mode)
    } else if self.unit.is_font_relative() {
      match self.unit {
        LengthUnit::Rex => root_font_metrics
          .map(|m| self.value * m.root_x_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rch => root_font_metrics
          .map(|m| self.value * m.root_ch_advance_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rcap => root_font_metrics
          .map(|m| self.value * m.root_cap_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Ric => root_font_metrics
          .map(|m| self.value * m.root_ic_advance_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rlh => root_font_metrics
          .map(|m| self.value * m.root_used_line_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rem => self.resolve_with_font_size(root_px),
        _ => self.resolve_with_font_size(font_px),
      }
    } else if self.unit.is_absolute() {
      Some(self.to_px())
    } else {
      Some(self.value)
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
    self.resolve_with_context_and_root_font_metrics(
      percentage_base,
      viewport_width,
      viewport_height,
      font_size_px,
      root_font_size_px,
      None,
    )
  }

  pub fn resolve_with_context_and_root_font_metrics(
    &self,
    percentage_base: Option<f32>,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    root_font_metrics: Option<RootFontMetrics>,
  ) -> Option<f32> {
    if !self.value.is_finite() {
      return None;
    }

    let percentage_base = percentage_base.filter(|b| b.is_finite());
    let needs_viewport =
      self.unit.is_viewport_relative() || self.calc.is_some_and(|calc| calc.has_viewport_relative());
    let (vw, vh) = if needs_viewport {
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
      (vw, vh)
    } else {
      // Viewport units are not present, but lower-level calc resolvers still expect finite
      // viewport sizes. Provide dummy finite values so percent/calc(% + px) can resolve even in
      // "indefinite viewport" contexts (e.g. intrinsic sizing passes).
      (0.0, 0.0)
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
      match calc {
        LengthCalc::Linear(calc) => {
          return calc.resolve_with_root_font_metrics(
            percentage_base,
            vw,
            vh,
            font_px,
            root_px,
            root_font_metrics,
          );
        }
        LengthCalc::Expr(id) => {
          return resolve_length_calc_expr(
            LengthCalc::Expr(id),
            percentage_base,
            vw,
            vh,
            font_px,
            root_px,
            root_font_metrics,
          );
        }
      }
    }

    if self.unit.is_percentage() {
      percentage_base.map(|base| (self.value / 100.0) * base)
    } else if self.unit.is_viewport_relative() {
      self.resolve_with_viewport(vw, vh)
    } else if self.unit.is_font_relative() {
      match self.unit {
        LengthUnit::Rex => root_font_metrics
          .map(|m| self.value * m.root_x_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rch => root_font_metrics
          .map(|m| self.value * m.root_ch_advance_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rcap => root_font_metrics
          .map(|m| self.value * m.root_cap_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Ric => root_font_metrics
          .map(|m| self.value * m.root_ic_advance_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rlh => root_font_metrics
          .map(|m| self.value * m.root_used_line_height_px)
          .or_else(|| self.resolve_with_font_size(root_px)),
        LengthUnit::Rem => self.resolve_with_font_size(root_px),
        _ => self.resolve_with_font_size(font_px),
      }
    } else if self.unit.is_absolute() {
      Some(self.to_px())
    } else {
      Some(self.value)
    }
  }

  /// Resolves any non-percentage components of this `<length-percentage>` into absolute pixels,
  /// preserving percentage terms.
  ///
  /// This is used for registered custom properties with `syntax: "<length-percentage>"` so their
  /// computed value behaves like a real property: font-relative, viewport-relative, and container
  /// query units are resolved in the declaration element's context, while `%` components are kept.
  pub(crate) fn resolve_non_percentage_terms_to_px(
    self,
    viewport_width: f32,
    viewport_height: f32,
    font_size_px: f32,
    root_font_size_px: f32,
    root_font_metrics: Option<RootFontMetrics>,
    cqw_base: f32,
    cqh_base: f32,
    cqi_base: f32,
    cqb_base: f32,
    writing_mode: crate::style::types::WritingMode,
  ) -> Self {
    let resolved = self.resolve_container_query_units(cqw_base, cqh_base, cqi_base, cqb_base);

    let resolve_non_percent_term_to_px = |unit: LengthUnit, value: f32| -> Option<f32> {
      if !value.is_finite() {
        return None;
      }
      let px = match unit {
        u if u.is_absolute() => Length::new(value, u).to_px(),
        u if u.is_viewport_relative() => Length::new(value, u)
          .resolve_with_viewport_for_writing_mode(viewport_width, viewport_height, writing_mode)?,
        LengthUnit::Em => value * font_size_px,
        LengthUnit::Ex | LengthUnit::Ch => value * font_size_px * 0.5,
        LengthUnit::Cap => value * font_size_px * 0.7,
        LengthUnit::Ic => value * font_size_px,
        LengthUnit::Rem => value * root_font_size_px,
        // Root font-relative units require root font metrics, which may not be available during
        // cascade (e.g. before web fonts are loaded). Preserve the authored unit until a metrics
        // context is available so later canonicalization can resolve it accurately.
        LengthUnit::Rex => value * root_font_metrics?.root_x_height_px,
        LengthUnit::Rch => value * root_font_metrics?.root_ch_advance_px,
        LengthUnit::Rcap => value * root_font_metrics?.root_cap_height_px,
        LengthUnit::Ric => value * root_font_metrics?.root_ic_advance_px,
        LengthUnit::Rlh => value * root_font_metrics?.root_used_line_height_px,
        // Treat `lh` as `normal` (1.2 * font-size) at computed-value time. This matches the
        // existing `Length::resolve_with_context` fallback for lack of full font metrics.
        LengthUnit::Lh => value * font_size_px * 1.2,
        // At this point container query units should have been resolved via
        // `resolve_container_query_units`. If any remain, bail out and preserve the original value.
        u if u.is_container_query_relative() => return None,
        LengthUnit::Calc => return None,
        // Unknown units: preserve original.
        _ => return None,
      };

      if !px.is_finite() {
        return None;
      }
      Some(px)
    };

    let build_length_from_px_and_percent =
      |mut px_total: f32, mut pct_total: f32| -> Option<Length> {
        // Normalize tiny floating point noise to deterministic zeros.
        if px_total.abs() <= 1e-6 {
          px_total = 0.0;
        }
        if pct_total.abs() <= 1e-6 {
          pct_total = 0.0;
        }

        if pct_total == 0.0 {
          return Some(Length::px(px_total));
        }
        if px_total == 0.0 {
          return Some(Length::percent(pct_total));
        }

        let mut calc = CalcLength::empty();
        calc.push(LengthUnit::Px, px_total).ok()?;
        calc.push(LengthUnit::Percent, pct_total).ok()?;
        if calc.is_zero() {
          return Some(Length::px(0.0));
        }
        if let Some(term) = calc.single_term() {
          return Some(Length::new(term.value, term.unit));
        }
        Some(Length::calc(calc))
      };

    if let Some(calc) = resolved.calc {
      // Convert each linear `<length-percentage>` leaf into a canonical `px + %` form where all
      // non-percentage terms are resolved to absolute pixels and `%` coefficients are preserved.
      //
      // Non-linear expressions (`min()`/`max()`/`clamp()` and nested combinations) are mapped
      // recursively, preserving their structure.
      let map_leaf = |calc: CalcLength| -> Option<LengthCalc> {
        if calc.kind() != CalcLengthKind::Linear {
          return None;
        }

        let mut pct_total: f32 = 0.0;
        let mut px_total: f32 = 0.0;
        for term in calc.terms() {
          if term.value == 0.0 {
            continue;
          }
          if term.unit == LengthUnit::Percent {
            pct_total += term.value;
            continue;
          }
          px_total += resolve_non_percent_term_to_px(term.unit, term.value)?;
        }

        // Normalize tiny floating point noise to deterministic zeros.
        if px_total.abs() <= 1e-6 {
          px_total = 0.0;
        }
        if pct_total.abs() <= 1e-6 {
          pct_total = 0.0;
        }

        let mut out = CalcLength::empty();
        out.push(LengthUnit::Px, px_total).ok()?;
        out.push(LengthUnit::Percent, pct_total).ok()?;
        Some(LengthCalc::Linear(out))
      };

      fn map_calc(
        value: LengthCalc,
        depth: u32,
        map_leaf: &impl Fn(CalcLength) -> Option<LengthCalc>,
      ) -> Option<LengthCalc> {
        if depth > 128 {
          return None;
        }

        match value {
          LengthCalc::Linear(calc) => map_leaf(calc),
          LengthCalc::Expr(id) => {
            let expr = {
              let arena_guard = length_calc_expr_arena().read();
              let entry = arena_guard.entries.get(id.index() as usize)?;
              entry.expr.clone()
            };

            match expr {
              LengthCalcExpr::Add { left, right, sign } => {
                let left = map_calc(left, depth + 1, map_leaf)?;
                let right = map_calc(right, depth + 1, map_leaf)?;
                length_calc_add_scaled(left, right, sign)
              }
              LengthCalcExpr::Scale { value, factor } => {
                let value = map_calc(value, depth + 1, map_leaf)?;
                length_calc_scale(value, factor)
              }
              LengthCalcExpr::MinMax { op, values } => {
                let mut mapped = Vec::with_capacity(values.len());
                for v in values {
                  mapped.push(map_calc(v, depth + 1, map_leaf)?);
                }
                Some(length_calc_min_max(op, mapped))
              }
              LengthCalcExpr::Clamp {
                min,
                preferred,
                max,
              } => {
                let min = map_calc(min, depth + 1, map_leaf)?;
                let preferred = map_calc(preferred, depth + 1, map_leaf)?;
                let max = map_calc(max, depth + 1, map_leaf)?;
                Some(length_calc_clamp(min, preferred, max))
              }
            }
          }
        }
      }

      let Some(mapped) = map_calc(calc, 0, &map_leaf) else {
        return resolved;
      };

      return match mapped {
        LengthCalc::Linear(calc) => {
          if calc.is_zero() {
            Length::px(0.0)
          } else if let Some(term) = calc.single_term() {
            Length::new(term.value, term.unit)
          } else {
            Length::calc(calc)
          }
        }
        LengthCalc::Expr(id) => Length::calc_expr(id),
      };
    }

    // Non-calc (single unit) value.
    let mut pct_total: f32 = 0.0;
    let mut px_total: f32 = 0.0;
    if resolved.unit == LengthUnit::Percent {
      pct_total += resolved.value;
    } else if resolved.value != 0.0 {
      let Some(px) = resolve_non_percent_term_to_px(resolved.unit, resolved.value) else {
        return resolved;
      };
      px_total += px;
    }

    build_length_from_px_and_percent(px_total, pct_total).unwrap_or(resolved)
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
      match calc {
        LengthCalc::Linear(calc) => return calc.is_zero(),
        LengthCalc::Expr(_) => return false,
      }
    }
    self.value == 0.0
  }

  fn write_css(&self, out: &mut impl fmt::Write) -> fmt::Result {
    if let Some(calc) = self.calc {
      match calc {
        LengthCalc::Linear(calc) => return calc.write_css(out),
        LengthCalc::Expr(id) => {
          let css = length_calc_expr_arena()
            .read()
            .entries
            .get(id.0 as usize)
            .map(|e| e.css_text.clone())
            .unwrap_or_else(|| "calc(0)".to_string());
          return out.write_str(&css);
        }
      }
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
  Integer,
  Percentage,
  Color,
  Angle,
  Time,
  Resolution,
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
  Integer(i32),
  Percentage(f32),
  Color(crate::style::color::Color),
  Angle(f32),
  /// Stored in milliseconds.
  TimeMs(f32),
  /// Stored in `dppx` (dots per pixel).
  ResolutionDppx(f32),
  List {
    separator: CustomPropertyListSeparator,
    items: Vec<CustomPropertyTypedValue>,
  },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CustomPropertyComputeContext {
  pub font_size: f32,
  pub root_font_size: f32,
  pub root_font_metrics: Option<RootFontMetrics>,
  pub line_height: f32,
  pub viewport: Size,
  pub current_color: Rgba,
  pub is_dark_color_scheme: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomPropertyListSeparator {
  Space,
  Comma,
}

impl CustomPropertyTypedValue {
  pub(crate) fn to_computed_value(&self, ctx: &CustomPropertyComputeContext) -> Self {
    match self {
      CustomPropertyTypedValue::Length(len) => {
        CustomPropertyTypedValue::Length(compute_custom_property_length(*len, ctx))
      }
      CustomPropertyTypedValue::Number(value) => {
        if *value == 0.0 {
          CustomPropertyTypedValue::Number(0.0)
        } else {
          CustomPropertyTypedValue::Number(*value)
        }
      }
      CustomPropertyTypedValue::Integer(value) => CustomPropertyTypedValue::Integer(*value),
      CustomPropertyTypedValue::Percentage(value) => {
        if *value == 0.0 {
          CustomPropertyTypedValue::Percentage(0.0)
        } else {
          CustomPropertyTypedValue::Percentage(*value)
        }
      }
      CustomPropertyTypedValue::Color(color) => {
        let rgba = color.to_rgba_with_scheme(ctx.current_color, ctx.is_dark_color_scheme);
        CustomPropertyTypedValue::Color(crate::style::color::Color::Rgba(rgba))
      }
      CustomPropertyTypedValue::Angle(deg) => {
        let normalized = if deg.is_finite() {
          deg.rem_euclid(360.0)
        } else {
          *deg
        };
        CustomPropertyTypedValue::Angle(normalized)
      }
      CustomPropertyTypedValue::TimeMs(ms) => {
        if *ms == 0.0 {
          CustomPropertyTypedValue::TimeMs(0.0)
        } else {
          CustomPropertyTypedValue::TimeMs(*ms)
        }
      }
      CustomPropertyTypedValue::ResolutionDppx(dppx) => {
        if *dppx == 0.0 {
          CustomPropertyTypedValue::ResolutionDppx(0.0)
        } else {
          CustomPropertyTypedValue::ResolutionDppx(*dppx)
        }
      }
      CustomPropertyTypedValue::List { separator, items } => CustomPropertyTypedValue::List {
        separator: *separator,
        items: items
          .iter()
          .map(|item| item.to_computed_value(ctx))
          .collect(),
      },
    }
  }

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
      CustomPropertyTypedValue::Integer(i) => i.to_string(),
      CustomPropertyTypedValue::Percentage(p) => format!("{p}%"),
      CustomPropertyTypedValue::Color(c) => c.to_string(),
      CustomPropertyTypedValue::Angle(deg) => {
        if deg.fract() == 0.0 {
          format!("{deg:.0}deg")
        } else {
          format!("{deg}deg")
        }
      }
      CustomPropertyTypedValue::TimeMs(ms) => {
        let ms = if *ms == 0.0 { 0.0 } else { *ms };
        if ms.fract() == 0.0 {
          format!("{ms:.0}ms")
        } else {
          format!("{ms}ms")
        }
      }
      CustomPropertyTypedValue::ResolutionDppx(dppx) => {
        let dppx = if *dppx == 0.0 { 0.0 } else { *dppx };
        if dppx.fract() == 0.0 {
          format!("{dppx:.0}dppx")
        } else {
          format!("{dppx}dppx")
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

fn compute_custom_property_length(length: Length, ctx: &CustomPropertyComputeContext) -> Length {
  if let Some(calc) = length.calc {
    let Some(computed) = compute_custom_property_length_calc(calc, ctx) else {
      return length;
    };

    return match computed {
      LengthCalc::Linear(calc) => {
        if calc.is_zero() {
          Length::px(0.0)
        } else if let Some(term) = calc.single_term() {
          Length::new(term.value, term.unit)
        } else {
          Length::calc(calc)
        }
      }
      LengthCalc::Expr(id) => Length::calc_expr(id),
    };
  }

  match length.unit {
    unit if unit.is_absolute() => Length::px(length.to_px()),
    LengthUnit::Em => {
      if ctx.font_size.is_finite() {
        Length::px(length.value * ctx.font_size)
      } else {
        length
      }
    }
    LengthUnit::Ex | LengthUnit::Ch => {
      if ctx.font_size.is_finite() {
        Length::px(length.value * ctx.font_size * 0.5)
      } else {
        length
      }
    }
    LengthUnit::Cap => {
      if ctx.font_size.is_finite() {
        Length::px(length.value * ctx.font_size * 0.7)
      } else {
        length
      }
    }
    LengthUnit::Ic => {
      if ctx.font_size.is_finite() {
        Length::px(length.value * ctx.font_size)
      } else {
        length
      }
    }
    LengthUnit::Rem => {
      if ctx.root_font_size.is_finite() {
        Length::px(length.value * ctx.root_font_size)
      } else {
        length
      }
    }
    LengthUnit::Rex => {
      // Root font-relative units depend on root font metrics, which are not always available
      // during cascade (web fonts load after cascade). Preserve the authored unit until a root
      // metrics context is available so later canonicalization can resolve it accurately.
      ctx
        .root_font_metrics
        .map(|m| Length::px(length.value * m.root_x_height_px))
        .unwrap_or(length)
    }
    LengthUnit::Rch => ctx
      .root_font_metrics
      .map(|m| Length::px(length.value * m.root_ch_advance_px))
      .unwrap_or(length),
    LengthUnit::Rcap => ctx
      .root_font_metrics
      .map(|m| Length::px(length.value * m.root_cap_height_px))
      .unwrap_or(length),
    LengthUnit::Ric => ctx
      .root_font_metrics
      .map(|m| Length::px(length.value * m.root_ic_advance_px))
      .unwrap_or(length),
    LengthUnit::Rlh => ctx
      .root_font_metrics
      .map(|m| Length::px(length.value * m.root_used_line_height_px))
      .unwrap_or(length),
    LengthUnit::Lh => {
      if ctx.line_height.is_finite() {
        Length::px(length.value * ctx.line_height)
      } else {
        length
      }
    }
    unit if unit.is_viewport_relative() => length
      .resolve_with_viewport(ctx.viewport.width, ctx.viewport.height)
      .map(Length::px)
      .unwrap_or(length),
    _ => length,
  }
}

fn compute_custom_property_length_calc(
  value: LengthCalc,
  ctx: &CustomPropertyComputeContext,
) -> Option<LengthCalc> {
  fn inner(
    value: LengthCalc,
    ctx: &CustomPropertyComputeContext,
    depth: u32,
  ) -> Option<LengthCalc> {
    if depth > 128 {
      return None;
    }

    match value {
      LengthCalc::Linear(calc) => Some(LengthCalc::Linear(compute_custom_property_calc_length(
        calc, ctx,
      ))),
      LengthCalc::Expr(id) => {
        let expr = {
          let arena_guard = length_calc_expr_arena().read();
          let entry = arena_guard.entries.get(id.index() as usize)?;
          entry.expr.clone()
        };

        match expr {
          LengthCalcExpr::Add { left, right, sign } => {
            let left = inner(left, ctx, depth + 1)?;
            let right = inner(right, ctx, depth + 1)?;
            length_calc_add_scaled(left, right, sign)
          }
          LengthCalcExpr::Scale { value, factor } => {
            let value = inner(value, ctx, depth + 1)?;
            length_calc_scale(value, factor)
          }
          LengthCalcExpr::MinMax { op, values } => {
            let mut mapped = Vec::with_capacity(values.len());
            for v in values {
              mapped.push(inner(v, ctx, depth + 1)?);
            }
            Some(length_calc_min_max(op, mapped))
          }
          LengthCalcExpr::Clamp {
            min,
            preferred,
            max,
          } => {
            let min = inner(min, ctx, depth + 1)?;
            let preferred = inner(preferred, ctx, depth + 1)?;
            let max = inner(max, ctx, depth + 1)?;
            Some(length_calc_clamp(min, preferred, max))
          }
        }
      }
    }
  }

  inner(value, ctx, 0)
}

fn compute_custom_property_calc_length(
  calc: CalcLength,
  ctx: &CustomPropertyComputeContext,
) -> CalcLength {
  use CalcLengthKind::*;
  match calc.kind() {
    Linear => compute_custom_property_calc_linear(calc, ctx).unwrap_or(calc),
    Min => compute_custom_property_calc_function(calc, ctx, CalcLengthKind::Min),
    Max => compute_custom_property_calc_function(calc, ctx, CalcLengthKind::Max),
    Clamp => compute_custom_property_calc_function(calc, ctx, CalcLengthKind::Clamp),
  }
}

fn compute_custom_property_calc_function(
  calc: CalcLength,
  ctx: &CustomPropertyComputeContext,
  kind: CalcLengthKind,
) -> CalcLength {
  debug_assert!(kind != CalcLengthKind::Linear);
  let mut args: Vec<CalcLength> = Vec::new();
  let mut current = CalcLength::empty();

  for term in calc.terms() {
    if term.unit == LengthUnit::Calc {
      args.push(current);
      current = CalcLength::empty();
      continue;
    }
    let Some(mapped) = compute_custom_property_calc_term(*term, ctx) else {
      return calc;
    };
    if current.push(mapped.unit, mapped.value).is_err() {
      return calc;
    }
  }
  args.push(current);

  match kind {
    CalcLengthKind::Min => CalcLength::min_function(&args).unwrap_or(calc),
    CalcLengthKind::Max => CalcLength::max_function(&args).unwrap_or(calc),
    CalcLengthKind::Clamp => {
      if args.len() != 3 {
        return calc;
      }
      CalcLength::clamp_function(args[0], args[1], args[2]).unwrap_or(calc)
    }
    CalcLengthKind::Linear => calc,
  }
}

fn compute_custom_property_calc_linear(
  calc: CalcLength,
  ctx: &CustomPropertyComputeContext,
) -> Option<CalcLength> {
  let mut out = CalcLength::empty();
  for term in calc.terms() {
    if term.unit == LengthUnit::Calc {
      return None;
    }
    let mapped = compute_custom_property_calc_term(*term, ctx)?;
    out.push(mapped.unit, mapped.value).ok()?;
  }
  Some(out)
}

fn compute_custom_property_calc_term(
  term: CalcTerm,
  ctx: &CustomPropertyComputeContext,
) -> Option<CalcTerm> {
  let mut value = term.value;
  let unit = match term.unit {
    LengthUnit::Percent => LengthUnit::Percent,
    unit if unit.is_absolute() => {
      value = Length::new(value, unit).to_px();
      LengthUnit::Px
    }
    unit if unit.is_viewport_relative() => {
      let px =
        Length::new(value, unit).resolve_with_viewport(ctx.viewport.width, ctx.viewport.height)?;
      value = px;
      LengthUnit::Px
    }
    LengthUnit::Em => {
      if !ctx.font_size.is_finite() {
        return Some(term);
      }
      value *= ctx.font_size;
      LengthUnit::Px
    }
    LengthUnit::Ex | LengthUnit::Ch => {
      if !ctx.font_size.is_finite() {
        return Some(term);
      }
      value *= ctx.font_size * 0.5;
      LengthUnit::Px
    }
    LengthUnit::Cap => {
      if !ctx.font_size.is_finite() {
        return Some(term);
      }
      value *= ctx.font_size * 0.7;
      LengthUnit::Px
    }
    LengthUnit::Ic => {
      if !ctx.font_size.is_finite() {
        return Some(term);
      }
      value *= ctx.font_size;
      LengthUnit::Px
    }
    LengthUnit::Rem => {
      if !ctx.root_font_size.is_finite() {
        return Some(term);
      }
      value *= ctx.root_font_size;
      LengthUnit::Px
    }
    LengthUnit::Rex => {
      let Some(metrics) = ctx.root_font_metrics else {
        return Some(term);
      };
      value *= metrics.root_x_height_px;
      LengthUnit::Px
    }
    LengthUnit::Rch => {
      let Some(metrics) = ctx.root_font_metrics else {
        return Some(term);
      };
      value *= metrics.root_ch_advance_px;
      LengthUnit::Px
    }
    LengthUnit::Rcap => {
      let Some(metrics) = ctx.root_font_metrics else {
        return Some(term);
      };
      value *= metrics.root_cap_height_px;
      LengthUnit::Px
    }
    LengthUnit::Ric => {
      let Some(metrics) = ctx.root_font_metrics else {
        return Some(term);
      };
      value *= metrics.root_ic_advance_px;
      LengthUnit::Px
    }
    LengthUnit::Rlh => {
      let Some(metrics) = ctx.root_font_metrics else {
        return Some(term);
      };
      value *= metrics.root_used_line_height_px;
      LengthUnit::Px
    }
    LengthUnit::Lh => {
      if !ctx.line_height.is_finite() {
        return Some(term);
      }
      value *= ctx.line_height;
      LengthUnit::Px
    }
    LengthUnit::Calc => LengthUnit::Calc,
    other => other,
  };

  Some(CalcTerm { unit, value })
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
      } else if token.eq_ignore_ascii_case("<integer>") {
        Some(CustomPropertySyntax::Integer)
      } else if token.eq_ignore_ascii_case("<percentage>") {
        Some(CustomPropertySyntax::Percentage)
      } else if token.eq_ignore_ascii_case("<color>") {
        Some(CustomPropertySyntax::Color)
      } else if token.eq_ignore_ascii_case("<angle>") {
        Some(CustomPropertySyntax::Angle)
      } else if token.eq_ignore_ascii_case("<time>") {
        Some(CustomPropertySyntax::Time)
      } else if token.eq_ignore_ascii_case("<resolution>") {
        Some(CustomPropertySyntax::Resolution)
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
    Some(CustomPropertySyntax::Union(
      members.into_vec().into_boxed_slice(),
    ))
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
        crate::css::properties::parse_length(trim_ascii_whitespace(value))
          .map(CustomPropertyTypedValue::Length)
      }
      CustomPropertySyntax::Number => trim_ascii_whitespace(value)
        .parse()
        .ok()
        .map(CustomPropertyTypedValue::Number),
      CustomPropertySyntax::Integer => trim_ascii_whitespace(value)
        .parse::<i32>()
        .ok()
        .map(CustomPropertyTypedValue::Integer),
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
      CustomPropertySyntax::Color => {
        crate::style::color::Color::parse(trim_ascii_whitespace(value))
          .ok()
          .map(CustomPropertyTypedValue::Color)
      }
      CustomPropertySyntax::Angle => {
        parse_angle_token(trim_ascii_whitespace(value)).map(CustomPropertyTypedValue::Angle)
      }
      CustomPropertySyntax::Time => {
        crate::style::properties::parse_time_ms(trim_ascii_whitespace(value))
          .filter(|ms| ms.is_finite())
          .map(CustomPropertyTypedValue::TimeMs)
      }
      CustomPropertySyntax::Resolution => {
        crate::style::media::Resolution::parse(trim_ascii_whitespace(value))
          .ok()
          .map(|res| res.to_dppx())
          .filter(|dppx| dppx.is_finite())
          .map(CustomPropertyTypedValue::ResolutionDppx)
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
          b'\t' | b'\n' | b'\r' | 0x0c | b' ' if depth == 0 && bracket == 0 && brace == 0 => {
            let part = trim_ascii_whitespace(&raw[start..idx]);
            if !part.is_empty() {
              items.push(inner.parse_value(part)?);
            }
            idx += 1;
            while idx < bytes.len() && matches!(bytes[idx], b'\t' | b'\n' | b'\r' | 0x0c | b' ') {
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
