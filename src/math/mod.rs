//! MathML parsing and layout
//!
//! Provides a minimal MathML-to-layout pipeline that parses MathML elements
//! and produces a renderable math layout with glyph fragments and simple
//! vector primitives (rules for fraction bars/radicals).

mod operator_dict;

use crate::dom::{DomNode, DomNodeType, MATHML_NAMESPACE};
use crate::geometry::{Point, Rect, Size};
use crate::style::types::FontStyle as CssFontStyle;
use crate::style::types::FontFeatureSetting;
use crate::style::types::FontWeight as CssFontWeight;
use crate::style::ComputedStyle;
use crate::text::font_db::{FontStretch, FontStyle, LoadedFont, ScaledMetrics};
use crate::text::font_loader::{FontContext, MathConstants, MathKernSide};
use crate::text::pipeline::{Direction as TextDirection, ShapedRun, ShapingPipeline};
use rustybuzz::ttf_parser;
use std::borrow::Cow;
use std::sync::Arc;

const SCRIPT_SCALE: f32 = 0.71;
const MAX_SCRIPT_LEVEL: u8 = 8;
const MIN_SCRIPT_FONT_SIZE_PX: f32 = 6.0;

fn is_ascii_whitespace_mathml(c: char) -> bool {
  matches!(
    c,
    '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
  )
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_mathml)
}

/// Math variant requested by MathML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathVariant {
  Normal,
  Bold,
  Italic,
  BoldItalic,
  DoubleStruck,
  Script,
  BoldScript,
  Fraktur,
  BoldFraktur,
  SansSerif,
  SansSerifBold,
  SansSerifItalic,
  SansSerifBoldItalic,
  Monospace,
}

impl MathVariant {
  fn uses_unicode_math_alphanumerics(self) -> bool {
    matches!(
      self,
      MathVariant::DoubleStruck
        | MathVariant::Script
        | MathVariant::BoldScript
        | MathVariant::Fraktur
        | MathVariant::BoldFraktur
        | MathVariant::SansSerif
        | MathVariant::SansSerifBold
        | MathVariant::SansSerifItalic
        | MathVariant::SansSerifBoldItalic
        | MathVariant::Monospace
    )
  }

  fn is_italic(self) -> bool {
    // Variants such as script/fraktur/sans-serif-italic are implemented via code point remapping to
    // Unicode Mathematical Alphanumeric Symbols; applying CSS italics would synthesize a slant on
    // top of already-styled glyphs (or trigger font fallback). Only apply CSS italics for the
    // weight/slant-only variants.
    matches!(self, MathVariant::Italic | MathVariant::BoldItalic)
  }

  fn is_bold(self) -> bool {
    // Similar to italics above: bold script/fraktur/etc are represented by dedicated code points
    // and should not request synthetic weight.
    matches!(self, MathVariant::Bold | MathVariant::BoldItalic)
  }
}

// Unicode Mathematical Alphanumeric Symbols mapping tables.
//
// Some MathML variants map to a contiguous range of code points, while others have gaps where
// legacy Letterlike Symbols were encoded prior to the Mathematical Alphanumeric Symbols block.
// For those variants (script/fraktur), we keep explicit tables matching MathML Core practice.
const SCRIPT_UPPER_MAP: [char; 26] = [
  '\u{1D49C}', // A
  '\u{212C}',  // B (ℬ)
  '\u{1D49E}', // C
  '\u{1D49F}', // D
  '\u{2130}',  // E (ℰ)
  '\u{2131}',  // F (ℱ)
  '\u{1D4A2}', // G
  '\u{210B}',  // H (ℋ)
  '\u{2110}',  // I (ℐ)
  '\u{1D4A5}', // J
  '\u{1D4A6}', // K
  '\u{2112}',  // L (ℒ)
  '\u{2133}',  // M (ℳ)
  '\u{1D4A9}', // N
  '\u{1D4AA}', // O
  '\u{1D4AB}', // P
  '\u{1D4AC}', // Q
  '\u{211B}',  // R (ℛ)
  '\u{1D4AE}', // S
  '\u{1D4AF}', // T
  '\u{1D4B0}', // U
  '\u{1D4B1}', // V
  '\u{1D4B2}', // W
  '\u{1D4B3}', // X
  '\u{1D4B4}', // Y
  '\u{1D4B5}', // Z
];

const SCRIPT_LOWER_MAP: [char; 26] = [
  '\u{1D4B6}', // a
  '\u{1D4B7}', // b
  '\u{1D4B8}', // c
  '\u{1D4B9}', // d
  '\u{212F}',  // e (ℯ)
  '\u{1D4BB}', // f
  '\u{210A}',  // g (ℊ)
  '\u{1D4BD}', // h
  '\u{1D4BE}', // i
  '\u{1D4BF}', // j
  '\u{1D4C0}', // k
  '\u{1D4C1}', // l
  '\u{1D4C2}', // m
  '\u{1D4C3}', // n
  '\u{2134}',  // o (ℴ)
  '\u{1D4C5}', // p
  '\u{1D4C6}', // q
  '\u{1D4C7}', // r
  '\u{1D4C8}', // s
  '\u{1D4C9}', // t
  '\u{1D4CA}', // u
  '\u{1D4CB}', // v
  '\u{1D4CC}', // w
  '\u{1D4CD}', // x
  '\u{1D4CE}', // y
  '\u{1D4CF}', // z
];

const FRAKTUR_UPPER_MAP: [char; 26] = [
  '\u{1D504}', // A
  '\u{1D505}', // B
  '\u{212D}',  // C (ℭ)
  '\u{1D507}', // D
  '\u{1D508}', // E
  '\u{1D509}', // F
  '\u{1D50A}', // G
  '\u{210C}',  // H (ℌ)
  '\u{2111}',  // I (ℑ)
  '\u{1D50D}', // J
  '\u{1D50E}', // K
  '\u{1D50F}', // L
  '\u{1D510}', // M
  '\u{1D511}', // N
  '\u{1D512}', // O
  '\u{1D513}', // P
  '\u{1D514}', // Q
  '\u{211C}',  // R (ℜ)
  '\u{1D516}', // S
  '\u{1D517}', // T
  '\u{1D518}', // U
  '\u{1D519}', // V
  '\u{1D51A}', // W
  '\u{1D51B}', // X
  '\u{1D51C}', // Y
  '\u{2128}',  // Z (ℨ)
];

fn map_math_alphanumeric_char(ch: char, variant: MathVariant) -> Option<char> {
  fn map_contiguous_letter(ch: char, start: u32, base: u32) -> char {
    let offset = (ch as u32).saturating_sub(start);
    // Safe: all targets are valid Unicode scalar values; assignment is handled by caller.
    char::from_u32(base + offset).unwrap_or(ch)
  }

  match variant {
    MathVariant::DoubleStruck => match ch {
      'C' => Some('\u{2102}'), // ℂ
      'H' => Some('\u{210D}'), // ℍ
      'N' => Some('\u{2115}'), // ℕ
      'P' => Some('\u{2119}'), // ℙ
      'Q' => Some('\u{211A}'), // ℚ
      'R' => Some('\u{211D}'), // ℝ
      'Z' => Some('\u{2124}'), // ℤ
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D538)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D552)),
      '0'..='9' => Some(map_contiguous_letter(ch, '0' as u32, 0x1D7D8)),
      _ => None,
    },
    MathVariant::Script => match ch {
      'A'..='Z' => Some(SCRIPT_UPPER_MAP[(ch as u8 - b'A') as usize]),
      'a'..='z' => Some(SCRIPT_LOWER_MAP[(ch as u8 - b'a') as usize]),
      _ => None,
    },
    MathVariant::BoldScript => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D4D0)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D4EA)),
      _ => None,
    },
    MathVariant::Fraktur => match ch {
      'A'..='Z' => Some(FRAKTUR_UPPER_MAP[(ch as u8 - b'A') as usize]),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D51E)),
      _ => None,
    },
    MathVariant::BoldFraktur => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D56C)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D586)),
      _ => None,
    },
    MathVariant::SansSerif => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D5A0)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D5BA)),
      '0'..='9' => Some(map_contiguous_letter(ch, '0' as u32, 0x1D7E2)),
      _ => None,
    },
    MathVariant::SansSerifBold => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D5D4)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D5EE)),
      '0'..='9' => Some(map_contiguous_letter(ch, '0' as u32, 0x1D7EC)),
      // Mathematical Sans-Serif Bold Greek (Unicode Mathematical Alphanumeric Symbols).
      //
      // This range is not a simple `U+0391..U+03A9` remap due to the legacy gap at U+03A2 and
      // the insertion of `CAPITAL THETA SYMBOL` and `NABLA` in the target block.
      '\u{0391}'..='\u{03A1}' => Some(map_contiguous_letter(ch, 0x0391, 0x1D756)),
      '\u{03A3}'..='\u{03A9}' => Some(map_contiguous_letter(ch, 0x03A3, 0x1D768)),
      '\u{03B1}'..='\u{03C1}' => Some(map_contiguous_letter(ch, 0x03B1, 0x1D770)),
      '\u{03C2}' => Some('\u{1D781}'), // final sigma
      '\u{03C3}'..='\u{03C9}' => Some(map_contiguous_letter(ch, 0x03C3, 0x1D782)),
      '\u{03F4}' => Some('\u{1D767}'), // capital theta symbol (ϴ)
      '\u{2207}' => Some('\u{1D76F}'), // nabla (∇)
      '\u{2202}' => Some('\u{1D789}'), // partial differential (∂)
      '\u{03F5}' => Some('\u{1D78A}'), // epsilon symbol (ϵ)
      '\u{03D1}' => Some('\u{1D78B}'), // theta symbol (ϑ)
      '\u{03F0}' => Some('\u{1D78C}'), // kappa symbol (ϰ)
      '\u{03D5}' => Some('\u{1D78D}'), // phi symbol (ϕ)
      '\u{03F1}' => Some('\u{1D78E}'), // rho symbol (ϱ)
      '\u{03D6}' => Some('\u{1D78F}'), // pi symbol (ϖ)
      _ => None,
    },
    MathVariant::SansSerifItalic => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D608)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D622)),
      _ => None,
    },
    MathVariant::SansSerifBoldItalic => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D63C)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D656)),
      // Mathematical Sans-Serif Bold Italic Greek.
      '\u{0391}'..='\u{03A1}' => Some(map_contiguous_letter(ch, 0x0391, 0x1D790)),
      '\u{03A3}'..='\u{03A9}' => Some(map_contiguous_letter(ch, 0x03A3, 0x1D7A2)),
      '\u{03B1}'..='\u{03C1}' => Some(map_contiguous_letter(ch, 0x03B1, 0x1D7AA)),
      '\u{03C2}' => Some('\u{1D7BB}'), // final sigma
      '\u{03C3}'..='\u{03C9}' => Some(map_contiguous_letter(ch, 0x03C3, 0x1D7BC)),
      '\u{03F4}' => Some('\u{1D7A1}'), // capital theta symbol (ϴ)
      '\u{2207}' => Some('\u{1D7A9}'), // nabla (∇)
      '\u{2202}' => Some('\u{1D7C3}'), // partial differential (∂)
      '\u{03F5}' => Some('\u{1D7C4}'), // epsilon symbol (ϵ)
      '\u{03D1}' => Some('\u{1D7C5}'), // theta symbol (ϑ)
      '\u{03F0}' => Some('\u{1D7C6}'), // kappa symbol (ϰ)
      '\u{03D5}' => Some('\u{1D7C7}'), // phi symbol (ϕ)
      '\u{03F1}' => Some('\u{1D7C8}'), // rho symbol (ϱ)
      '\u{03D6}' => Some('\u{1D7C9}'), // pi symbol (ϖ)
      _ => None,
    },
    MathVariant::Monospace => match ch {
      'A'..='Z' => Some(map_contiguous_letter(ch, 'A' as u32, 0x1D670)),
      'a'..='z' => Some(map_contiguous_letter(ch, 'a' as u32, 0x1D68A)),
      '0'..='9' => Some(map_contiguous_letter(ch, '0' as u32, 0x1D7F6)),
      _ => None,
    },
    _ => None,
  }
}

fn map_math_alphanumerics(text: &str, variant: MathVariant) -> Cow<'_, str> {
  if !variant.uses_unicode_math_alphanumerics() || text.is_empty() {
    return Cow::Borrowed(text);
  }
  let mut out = String::with_capacity(text.len());
  let mut changed = false;
  for ch in text.chars() {
    if let Some(mapped) = map_math_alphanumeric_char(ch, variant) {
      changed |= mapped != ch;
      out.push(mapped);
    } else {
      out.push(ch);
    }
  }
  if changed {
    Cow::Owned(out)
  } else {
    Cow::Borrowed(text)
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MathLength {
  Em(f32),
  Ex(f32),
  Px(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MathLengthOrKeyword {
  Length(MathLength),
  Thin,
  Medium,
  Thick,
  Zero,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAlign {
  Axis,
  Baseline,
  Center,
  Top,
  Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
  Left,
  Center,
  Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLineStyle {
  None,
  Solid,
  Dashed,
  Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorForm {
  Prefix,
  Infix,
  Postfix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MencloseNotation {
  Box,
  RoundedBox,
  Circle,
  Top,
  Bottom,
  Left,
  Right,
  HorizontalStrike,
  VerticalStrike,
  UpDiagonalStrike,
  DownDiagonalStrike,
  LongDiv,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MathSize {
  Scale(f32),
  Absolute(f32),
}

/// Represents a MathML Core `scriptlevel` override.
///
/// MathML Core allows both absolute (`scriptlevel="2"`) and relative
/// (`scriptlevel="+1"`, `scriptlevel="-1"`) adjustments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathScriptLevel {
  Absolute(u8),
  Relative(i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MathStyleOverrides {
  pub display_style: Option<bool>,
  pub script_level: Option<MathScriptLevel>,
  pub math_size: Option<MathSize>,
  pub math_variant: Option<MathVariant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MathTableCell {
  pub content: MathNode,
  pub row_align: Option<RowAlign>,
  pub column_align: Option<ColumnAlign>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MathTableRow {
  pub cells: Vec<MathTableCell>,
  pub row_align: Option<RowAlign>,
  pub column_aligns: Vec<ColumnAlign>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MathTable {
  pub rows: Vec<MathTableRow>,
  pub column_aligns: Vec<ColumnAlign>,
  pub row_aligns: Vec<RowAlign>,
  pub column_spacings: Option<Vec<MathLengthOrKeyword>>,
  pub row_spacings: Option<Vec<MathLengthOrKeyword>>,
  pub column_lines: Option<Vec<TableLineStyle>>,
  pub row_lines: Option<Vec<TableLineStyle>>,
  pub frame: Option<TableLineStyle>,
  pub frame_spacing: Option<(MathLength, MathLength)>,
}

/// Parsed MathML node
#[derive(Debug, Clone, PartialEq)]
pub enum MathNode {
  Math {
    /// Effective MathML `displaystyle` value for this subtree.
    ///
    /// For `<math>`, this defaults to `display="block"` unless `displaystyle` is set.
    display_style: bool,
    children: Vec<MathNode>,
  },
  Row(Vec<MathNode>),
  Identifier {
    text: String,
    variant: Option<MathVariant>,
  },
  Number {
    text: String,
    variant: Option<MathVariant>,
  },
  Operator {
    text: String,
    /// `<mo form="...">` override. If omitted, the form is inferred from row context.
    form: Option<OperatorForm>,
    /// `<mo stretchy="...">` override. If omitted, the operator dictionary default is used.
    stretchy: Option<bool>,
    /// `<mo symmetric="...">` override. If omitted, the operator dictionary default is used.
    symmetric: Option<bool>,
    /// `<mo minsize="...">` override.
    minsize: Option<MathLength>,
    /// `<mo maxsize="...">` override.
    maxsize: Option<MathLength>,
    /// `<mo lspace="...">` override.
    lspace: Option<MathLengthOrKeyword>,
    /// `<mo rspace="...">` override.
    rspace: Option<MathLengthOrKeyword>,
    variant: Option<MathVariant>,
  },
  Text {
    text: String,
    variant: Option<MathVariant>,
  },
  Space {
    width: MathLength,
    height: MathLength,
    depth: MathLength,
  },
  Fraction {
    numerator: Box<MathNode>,
    denominator: Box<MathNode>,
    linethickness: Option<MathLengthOrKeyword>,
    bevelled: bool,
    numalign: ColumnAlign,
    denomalign: ColumnAlign,
  },
  Sqrt(Box<MathNode>),
  Root {
    radicand: Box<MathNode>,
    index: Box<MathNode>,
  },
  Superscript {
    base: Box<MathNode>,
    superscript: Box<MathNode>,
  },
  Subscript {
    base: Box<MathNode>,
    subscript: Box<MathNode>,
  },
  SubSuperscript {
    base: Box<MathNode>,
    subscript: Box<MathNode>,
    superscript: Box<MathNode>,
  },
  Over {
    base: Box<MathNode>,
    over: Box<MathNode>,
    accent: bool,
  },
  Under {
    base: Box<MathNode>,
    under: Box<MathNode>,
    accentunder: bool,
  },
  UnderOver {
    base: Box<MathNode>,
    under: Box<MathNode>,
    over: Box<MathNode>,
    accent: bool,
    accentunder: bool,
  },
  Multiscripts {
    base: Box<MathNode>,
    prescripts: Vec<(Option<MathNode>, Option<MathNode>)>,
    postscripts: Vec<(Option<MathNode>, Option<MathNode>)>,
  },
  Style {
    overrides: MathStyleOverrides,
    children: Vec<MathNode>,
  },
  Enclose {
    notation: Vec<MencloseNotation>,
    child: Box<MathNode>,
  },
  Table(MathTable),
}

/// Renderable fragment produced by math layout.
#[derive(Debug, Clone)]
pub enum MathFragment {
  Glyph { origin: Point, run: ShapedRun },
  Rule(Rect),
  Line { from: Point, to: Point, width: f32 },
  StrokeRect { rect: Rect, radius: f32, width: f32 },
}

#[derive(Debug, Clone, Default)]
pub struct MathLayoutAnnotations {
  /// Metadata about the trailing glyph in this layout, used for script positioning.
  pub trailing_glyph: Option<MathGlyph>,
}

impl MathLayoutAnnotations {
  fn merge_trailing(&self, other: &MathLayoutAnnotations) -> MathLayoutAnnotations {
    if other.trailing_glyph.is_some() {
      other.clone()
    } else {
      self.clone()
    }
  }
}

#[derive(Debug, Clone)]
pub struct MathGlyph {
  pub font: Arc<LoadedFont>,
  pub glyph_id: u16,
  pub font_size: f32,
  pub italic_correction: f32,
}

/// Final math layout with positioned fragments.
#[derive(Debug, Clone)]
pub struct MathLayout {
  pub width: f32,
  pub height: f32,
  pub baseline: f32,
  pub fragments: Vec<MathFragment>,
  pub annotations: MathLayoutAnnotations,
}

impl MathFragment {
  fn translate(self, offset: Point) -> Self {
    match self {
      MathFragment::Glyph { origin, run } => MathFragment::Glyph {
        origin: Point::new(origin.x + offset.x, origin.y + offset.y),
        run,
      },
      MathFragment::Rule(rect) => MathFragment::Rule(rect.translate(offset)),
      MathFragment::Line { from, to, width } => MathFragment::Line {
        from: from.translate(offset),
        to: to.translate(offset),
        width,
      },
      MathFragment::StrokeRect {
        rect,
        radius,
        width,
      } => MathFragment::StrokeRect {
        rect: rect.translate(offset),
        radius,
        width,
      },
    }
  }
}

impl MathLayout {
  pub fn size(&self) -> Size {
    Size::new(self.width, self.height)
  }
}

/// Internal layout style carrying math-specific sizing state.
#[derive(Debug, Clone, Copy)]
struct MathStyle {
  font_size: f32,
  display_style: bool,
  default_variant: Option<MathVariant>,
  script_level: u8,
}

impl MathStyle {
  fn from_computed(style: &ComputedStyle) -> Self {
    Self {
      font_size: style.font_size,
      display_style: false,
      default_variant: None,
      script_level: 0,
    }
  }

  fn script_scale_down(constants: Option<&MathConstants>, current_level: u8) -> f32 {
    if current_level == 0 {
      constants
        .and_then(|c| c.script_percent_scale_down)
        .unwrap_or(SCRIPT_SCALE)
    } else {
      constants
        .and_then(|c| c.script_script_percent_scale_down)
        .unwrap_or(SCRIPT_SCALE)
    }
  }

  fn apply_script_delta(&self, delta: i32, constants: Option<&MathConstants>) -> Self {
    let mut out = *self;
    if delta > 0 {
      for _ in 0..delta {
        if out.script_level >= MAX_SCRIPT_LEVEL {
          break;
        }
        let scale = Self::script_scale_down(constants, out.script_level);
        out.font_size = (out.font_size * scale).max(MIN_SCRIPT_FONT_SIZE_PX);
        out.script_level = out.script_level.saturating_add(1);
      }
    } else if delta < 0 {
      for _ in 0..(-delta) {
        if out.script_level == 0 {
          break;
        }
        let prev_level = out.script_level.saturating_sub(1);
        let scale_down = Self::script_scale_down(constants, prev_level);
        if scale_down > 0.0 {
          out.font_size = (out.font_size / scale_down).max(1.0);
        }
        out.script_level = prev_level;
      }
    }
    out
  }

  fn with_script_level(&self, target: u8, constants: Option<&MathConstants>) -> Self {
    if target == self.script_level {
      *self
    } else if target > self.script_level {
      self.apply_script_delta((target - self.script_level) as i32, constants)
    } else {
      self.apply_script_delta(-((self.script_level - target) as i32), constants)
    }
  }

  fn script_with_constants(&self, constants: Option<&MathConstants>) -> Self {
    let mut out = self.apply_script_delta(1, constants);
    // Script layout always forces `displaystyle` off.
    out.display_style = false;
    out
  }
}

fn normalized_text(node: &DomNode, preserve_space: bool) -> Option<String> {
  let mut buf = String::new();
  collect_text(node, &mut buf);
  if preserve_space {
    if buf.is_empty() {
      None
    } else {
      Some(buf)
    }
  } else {
    let trimmed = trim_ascii_whitespace(&buf);
    if trimmed.is_empty() {
      None
    } else {
      Some(trimmed.to_string())
    }
  }
}

fn collect_text(node: &DomNode, out: &mut String) {
  match &node.node_type {
    DomNodeType::Text { content } => out.push_str(content),
    DomNodeType::Element { .. }
    | DomNodeType::Slot { .. }
    | DomNodeType::Document { .. }
    | DomNodeType::ShadowRoot { .. } => {
      for child in node.children.iter() {
        collect_text(child, out);
      }
    }
  }
}

fn parse_mathvariant(node: &DomNode) -> Option<MathVariant> {
  let value = node.get_attribute_ref("mathvariant")?;
  match value.to_ascii_lowercase().as_str() {
    "normal" | "upright" => Some(MathVariant::Normal),
    "bold" => Some(MathVariant::Bold),
    "italic" | "oblique" => Some(MathVariant::Italic),
    "bold-italic" | "bold-oblique" => Some(MathVariant::BoldItalic),
    "double-struck" | "doublestruck" => Some(MathVariant::DoubleStruck),
    "script" => Some(MathVariant::Script),
    "bold-script" | "boldscript" => Some(MathVariant::BoldScript),
    "fraktur" => Some(MathVariant::Fraktur),
    "bold-fraktur" | "boldfraktur" => Some(MathVariant::BoldFraktur),
    "sans-serif" | "sansserif" => Some(MathVariant::SansSerif),
    "sans-serif-bold" | "bold-sans-serif" | "boldsansserif" => Some(MathVariant::SansSerifBold),
    "sans-serif-italic" | "sans-serif-oblique" | "sansserifitalic" | "sansserifoblique" => {
      Some(MathVariant::SansSerifItalic)
    }
    "sans-serif-bold-italic"
    | "bold-sans-serif-italic"
    | "sans-serif-bold-oblique"
    | "bold-sans-serif-oblique"
    | "boldsansserifitalic" => Some(MathVariant::SansSerifBoldItalic),
    "monospace" | "typewriter" => Some(MathVariant::Monospace),
    _ => None,
  }
}

fn parse_math_length(raw: Option<&str>) -> Option<MathLength> {
  let value = trim_ascii_whitespace(raw?);
  if value.is_empty() {
    return None;
  }
  if let Some(v) = value.strip_suffix("ex") {
    return trim_ascii_whitespace(v)
      .parse::<f32>()
      .ok()
      .map(MathLength::Ex);
  }
  if let Some(v) = value.strip_suffix("em") {
    return trim_ascii_whitespace(v)
      .parse::<f32>()
      .ok()
      .map(MathLength::Em);
  }
  if let Some(v) = value.strip_suffix("px") {
    return trim_ascii_whitespace(v)
      .parse::<f32>()
      .ok()
      .map(MathLength::Px);
  }
  value.parse::<f32>().ok().map(MathLength::Em)
}

fn parse_math_length_or_keyword(raw: Option<&str>) -> Option<MathLengthOrKeyword> {
  let value = trim_ascii_whitespace(raw?);
  if value.is_empty() {
    return None;
  }
  match value.to_ascii_lowercase().as_str() {
    "thin" => Some(MathLengthOrKeyword::Thin),
    "medium" => Some(MathLengthOrKeyword::Medium),
    "thick" => Some(MathLengthOrKeyword::Thick),
    "0" => Some(MathLengthOrKeyword::Zero),
    _ => parse_math_length(Some(value)).map(MathLengthOrKeyword::Length),
  }
}

fn parse_math_size(raw: &str) -> Option<MathSize> {
  match trim_ascii_whitespace(raw).to_ascii_lowercase().as_str() {
    "small" => Some(MathSize::Scale(0.8)),
    "normal" => Some(MathSize::Scale(1.0)),
    "big" => Some(MathSize::Scale(1.2)),
    other => {
      if let Some(v) = other.strip_suffix('%') {
        if let Ok(pct) = trim_ascii_whitespace(v).parse::<f32>() {
          return Some(MathSize::Scale(pct / 100.0));
        }
      }
      if let Some(v) = other.strip_suffix("px") {
        return trim_ascii_whitespace(v)
          .parse::<f32>()
          .ok()
          .map(MathSize::Absolute);
      }
      if let Some(v) = other.strip_suffix("em") {
        return trim_ascii_whitespace(v)
          .parse::<f32>()
          .ok()
          .map(|v| MathSize::Scale(v));
      }
      if let Ok(val) = other.parse::<f32>() {
        Some(MathSize::Scale(val))
      } else {
        None
      }
    }
  }
}

fn parse_display_style(value: Option<&str>) -> Option<bool> {
  let raw = trim_ascii_whitespace(value?);
  if raw.is_empty() {
    return None;
  }
  if raw.eq_ignore_ascii_case("true") || raw == "1" {
    Some(true)
  } else if raw.eq_ignore_ascii_case("false") || raw == "0" {
    Some(false)
  } else {
    None
  }
}

fn parse_script_level(value: Option<&str>) -> Option<MathScriptLevel> {
  let raw = trim_ascii_whitespace(value?);
  if raw.is_empty() {
    return None;
  }
  let (kind, digits) = match raw.as_bytes().first().copied() {
    Some(b'+') => (Some('+'), &raw[1..]),
    Some(b'-') => (Some('-'), &raw[1..]),
    _ => (None, raw),
  };
  let parsed = trim_ascii_whitespace(digits).parse::<i32>().ok()?;
  match kind {
    Some('+') => Some(MathScriptLevel::Relative(parsed)),
    Some('-') => Some(MathScriptLevel::Relative(-parsed)),
    None => Some(MathScriptLevel::Absolute(
      parsed.clamp(0, MAX_SCRIPT_LEVEL as i32) as u8,
    )),
    _ => None,
  }
}

fn parse_operator_form(value: Option<&str>) -> Option<OperatorForm> {
  let raw = trim_ascii_whitespace(value?);
  if raw.is_empty() {
    return None;
  }
  match raw.to_ascii_lowercase().as_str() {
    "prefix" => Some(OperatorForm::Prefix),
    "infix" => Some(OperatorForm::Infix),
    "postfix" => Some(OperatorForm::Postfix),
    _ => None,
  }
}

fn parse_math_space(raw: Option<&str>) -> Option<MathLengthOrKeyword> {
  let value = trim_ascii_whitespace(raw?);
  if value.is_empty() {
    return None;
  }
  match value.to_ascii_lowercase().as_str() {
    "thinmathspace" | "thin" => Some(MathLengthOrKeyword::Thin),
    "mediummathspace" | "medium" => Some(MathLengthOrKeyword::Medium),
    "thickmathspace" | "thick" => Some(MathLengthOrKeyword::Thick),
    "0" => Some(MathLengthOrKeyword::Zero),
    other => parse_math_length(Some(other)).map(MathLengthOrKeyword::Length),
  }
}

const DEFAULT_TABLE_COLUMN_SPACING: MathLengthOrKeyword = MathLengthOrKeyword::Length(MathLength::Em(0.8));
const DEFAULT_TABLE_ROW_SPACING: MathLengthOrKeyword = MathLengthOrKeyword::Length(MathLength::Em(0.2));

const DEFAULT_TABLE_FRAME_SPACING_X: MathLength = MathLength::Em(0.4);
const DEFAULT_TABLE_FRAME_SPACING_Y: MathLength = MathLength::Ex(0.5);

fn parse_math_space_list(value: Option<&str>, default: MathLengthOrKeyword) -> Option<Vec<MathLengthOrKeyword>> {
  let raw = value?;
  let parsed: Vec<MathLengthOrKeyword> = raw
    .split(|c| c == ' ' || c == ',')
    .filter_map(|item| parse_math_space(Some(item)))
    .collect();
  if parsed.is_empty() {
    Some(vec![default])
  } else {
    Some(parsed)
  }
}

fn parse_table_line_style(raw: Option<&str>) -> Option<TableLineStyle> {
  let value = trim_ascii_whitespace(raw?);
  if value.is_empty() {
    return None;
  }
  match value.to_ascii_lowercase().as_str() {
    "none" => Some(TableLineStyle::None),
    "solid" => Some(TableLineStyle::Solid),
    "dashed" => Some(TableLineStyle::Dashed),
    "dotted" => Some(TableLineStyle::Dotted),
    _ => None,
  }
}

fn parse_table_line_list(value: Option<&str>) -> Option<Vec<TableLineStyle>> {
  let raw = value?;
  let parsed: Vec<TableLineStyle> = raw
    .split(|c| c == ' ' || c == ',')
    .filter_map(|item| parse_table_line_style(Some(item)))
    .collect();
  if parsed.is_empty() {
    Some(vec![TableLineStyle::None])
  } else {
    Some(parsed)
  }
}

fn parse_frame_spacing(value: Option<&str>) -> Option<(MathLength, MathLength)> {
  let raw = value?;
  let mut parts = raw.split(|c| c == ' ' || c == ',').filter(|s| !s.is_empty());
  let first = parse_math_length(parts.next())?;
  let second = parts.next().and_then(|v| parse_math_length(Some(v))).unwrap_or(first);
  Some((first, second))
}

fn parse_row_align_list(value: Option<&str>) -> Vec<RowAlign> {
  value
    .map(|v| {
      v.split(|c| c == ' ' || c == ',')
        .filter_map(
          |item| match trim_ascii_whitespace(item).to_ascii_lowercase().as_str() {
            "axis" => Some(RowAlign::Axis),
            "top" => Some(RowAlign::Top),
            "bottom" => Some(RowAlign::Bottom),
            "center" | "centre" | "middle" => Some(RowAlign::Center),
            "baseline" => Some(RowAlign::Baseline),
            _ => None,
          },
        )
        .collect()
    })
    .unwrap_or_default()
}

fn parse_column_align_list(value: Option<&str>) -> Vec<ColumnAlign> {
  value
    .map(|v| {
      v.split(|c| c == ' ' || c == ',')
        .filter_map(
          |item| match trim_ascii_whitespace(item).to_ascii_lowercase().as_str() {
            "left" => Some(ColumnAlign::Left),
            "center" | "centre" => Some(ColumnAlign::Center),
            "right" => Some(ColumnAlign::Right),
            _ => None,
          },
        )
        .collect()
    })
    .unwrap_or_default()
}

fn parse_column_align(value: Option<&str>) -> Option<ColumnAlign> {
  value.and_then(|v| parse_column_align_list(Some(v)).into_iter().next())
}

fn parse_menclose_notation(value: Option<&str>) -> Vec<MencloseNotation> {
  let Some(raw) = value else {
    return vec![MencloseNotation::Box];
  };
  let parsed: Vec<MencloseNotation> = raw
    .split(|c| c == ' ' || c == ',')
    .filter_map(
      |item| match trim_ascii_whitespace(item).to_ascii_lowercase().as_str() {
        "box" => Some(MencloseNotation::Box),
        "roundedbox" => Some(MencloseNotation::RoundedBox),
        "circle" => Some(MencloseNotation::Circle),
        "top" => Some(MencloseNotation::Top),
        "bottom" => Some(MencloseNotation::Bottom),
        "left" => Some(MencloseNotation::Left),
        "right" => Some(MencloseNotation::Right),
        "horizontalstrike" => Some(MencloseNotation::HorizontalStrike),
        "verticalstrike" => Some(MencloseNotation::VerticalStrike),
        "updiagonalstrike" => Some(MencloseNotation::UpDiagonalStrike),
        "downdiagonalstrike" => Some(MencloseNotation::DownDiagonalStrike),
        "longdiv" => Some(MencloseNotation::LongDiv),
        _ => None,
      },
    )
    .collect();
  if parsed.is_empty() {
    vec![MencloseNotation::Box]
  } else {
    parsed
  }
}

fn parse_style_overrides(node: &DomNode) -> MathStyleOverrides {
  MathStyleOverrides {
    display_style: parse_display_style(node.get_attribute_ref("displaystyle")),
    script_level: parse_script_level(node.get_attribute_ref("scriptlevel")),
    math_size: node
      .get_attribute_ref("mathsize")
      .and_then(|v| parse_math_size(v)),
    math_variant: parse_mathvariant(node),
  }
}

fn has_style_overrides(overrides: &MathStyleOverrides) -> bool {
  overrides.display_style.is_some()
    || overrides.script_level.is_some()
    || overrides.math_size.is_some()
    || overrides.math_variant.is_some()
}

fn apply_presentation_attributes(node: &DomNode, tag: &str, parsed: MathNode) -> MathNode {
  if matches!(&parsed, MathNode::Style { .. }) {
    return parsed;
  }

  let mut overrides = parse_style_overrides(node);

  // `<math>` already carries an explicit `display_style` field, so avoid wrapping the node only
  // because of `displaystyle`. Other presentation attributes still apply via an outer wrapper.
  if tag.eq_ignore_ascii_case("math") {
    overrides.display_style = None;
  }

  // Token elements already store `mathvariant` as an explicit override on the token node itself.
  // Skip wrapping tokens solely because they have `mathvariant`, but still allow other style
  // attributes like `scriptlevel` and `mathsize`.
  if matches!(
    &parsed,
    MathNode::Identifier { .. }
      | MathNode::Number { .. }
      | MathNode::Operator { .. }
      | MathNode::Text { .. }
  ) {
    overrides.math_variant = None;
  }

  if has_style_overrides(&overrides) {
    MathNode::Style {
      overrides,
      children: vec![parsed],
    }
  } else {
    parsed
  }
}

fn repeating_value<T: Copy>(values: &[T], index: usize) -> Option<T> {
  if values.is_empty() {
    None
  } else {
    Some(*values.get(index).unwrap_or(&values[values.len() - 1]))
  }
}

fn wrap_row_or_single(mut children: Vec<MathNode>) -> Option<MathNode> {
  if children.is_empty() {
    None
  } else if children.len() == 1 {
    Some(children.remove(0))
  } else {
    Some(MathNode::Row(children))
  }
}

fn parse_children(node: &DomNode) -> Vec<MathNode> {
  node.children.iter().filter_map(parse_mathml).collect()
}

fn is_annotation_tag(tag: &str) -> bool {
  matches!(tag, "annotation" | "annotation-xml")
}

fn parse_scripts(children: &[DomNode]) -> Vec<(Option<MathNode>, Option<MathNode>)> {
  let mut pairs = Vec::new();
  let mut idx = 0;
  while idx < children.len() {
    if let Some(tag) = children[idx].tag_name() {
      if tag.eq_ignore_ascii_case("mprescripts") {
        idx += 1;
        continue;
      }
    }
    let sub = children.get(idx).and_then(parse_mathml);
    let sup = children.get(idx + 1).and_then(parse_mathml);
    pairs.push((sub, sup));
    idx += 2;
  }
  pairs
}

fn empty_text_node() -> MathNode {
  MathNode::Text {
    text: String::new(),
    variant: None,
  }
}

/// Parse a DomNode subtree into a MathNode tree.
pub fn parse_mathml(node: &DomNode) -> Option<MathNode> {
  match &node.node_type {
    DomNodeType::Text { content } => {
      let trimmed = trim_ascii_whitespace(content);
      if trimmed.is_empty() {
        None
      } else {
        Some(MathNode::Text {
          text: trimmed.to_string(),
          variant: None,
        })
      }
    }
    DomNodeType::Slot { .. } | DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. } => {
      wrap_row_or_single(parse_children(node))
    }
    DomNodeType::Element {
      tag_name,
      namespace,
      ..
    } => {
      let tag = tag_name.to_ascii_lowercase();
      let in_math_ns = namespace == MATHML_NAMESPACE;
      let parsed = match tag.as_str() {
        "annotation" | "annotation-xml" => None,
        "semantics" => {
          let mut first_child = None;
          for child in node.children.iter() {
            match &child.node_type {
              DomNodeType::Element { tag_name, .. } => {
                if is_annotation_tag(&tag_name.to_ascii_lowercase()) {
                  continue;
                }
              }
              DomNodeType::Text { content } => {
                if trim_ascii_whitespace(content).is_empty() {
                  continue;
                }
              }
              _ => {}
            }
            first_child = Some(child);
            break;
          }
          first_child.and_then(parse_mathml)
        }
        "math" if in_math_ns || namespace.is_empty() => {
          // MathML Core: `displaystyle` defaults to `true` for display math.
          let display_attr_is_block = node
            .get_attribute_ref("display")
            .map(|v| v.eq_ignore_ascii_case("block"))
            .unwrap_or(false);
          let display_style = parse_display_style(node.get_attribute_ref("displaystyle"))
            .unwrap_or(display_attr_is_block);
          let children = parse_children(node);
          Some(MathNode::Math {
            display_style,
            children,
          })
        }
        "none" => None,
        "mrow" => Some(MathNode::Row(parse_children(node))),
        "mi" => normalized_text(node, false).map(|text| MathNode::Identifier {
          text,
          variant: parse_mathvariant(node),
        }),
        "mn" => normalized_text(node, false).map(|text| MathNode::Number {
          text,
          variant: parse_mathvariant(node),
        }),
        "mo" => normalized_text(node, false).map(|text| {
          let stretchy = parse_display_style(node.get_attribute_ref("stretchy"));
          let symmetric = parse_display_style(node.get_attribute_ref("symmetric"));
          let minsize = parse_math_length(node.get_attribute_ref("minsize"));
          let maxsize = parse_math_length(node.get_attribute_ref("maxsize"));
          let form = parse_operator_form(node.get_attribute_ref("form"));
          let lspace = parse_math_space(node.get_attribute_ref("lspace"));
          let rspace = parse_math_space(node.get_attribute_ref("rspace"));
          MathNode::Operator {
            text,
            form,
            stretchy,
            symmetric,
            minsize,
            maxsize,
            lspace,
            rspace,
            variant: parse_mathvariant(node),
          }
        }),
        "ms" => normalized_text(node, true).map(|text| {
          let lquote = node.get_attribute_ref("lquote").unwrap_or("\"");
          let rquote = node.get_attribute_ref("rquote").unwrap_or("\"");
          let mut quoted = String::with_capacity(lquote.len() + text.len() + rquote.len());
          quoted.push_str(lquote);
          quoted.push_str(&text);
          quoted.push_str(rquote);
          MathNode::Text {
            text: quoted,
            variant: parse_mathvariant(node),
          }
        }),
        "mtext" => normalized_text(node, true).map(|text| MathNode::Text {
          text,
          variant: parse_mathvariant(node),
        }),
        "mspace" => Some(MathNode::Space {
          width: parse_math_length(node.get_attribute_ref("width")).unwrap_or(MathLength::Em(0.0)),
          height: parse_math_length(node.get_attribute_ref("height"))
            .unwrap_or(MathLength::Em(0.0)),
          depth: parse_math_length(node.get_attribute_ref("depth")).unwrap_or(MathLength::Em(0.0)),
        }),
        "mstyle" => Some(MathNode::Style {
          overrides: parse_style_overrides(node),
          children: parse_children(node),
        }),
        "merror" => wrap_row_or_single(parse_children(node)),
        "mfrac" => {
          let mut children = parse_children(node).into_iter();
          let num = children.next().unwrap_or_else(empty_text_node);
          let den = children.next().unwrap_or_else(empty_text_node);
          let linethickness = parse_math_length_or_keyword(node.get_attribute_ref("linethickness"));
          let bevelled = parse_display_style(node.get_attribute_ref("bevelled")).unwrap_or(false);
          let numalign =
            parse_column_align(node.get_attribute_ref("numalign")).unwrap_or(ColumnAlign::Center);
          let denomalign =
            parse_column_align(node.get_attribute_ref("denomalign")).unwrap_or(ColumnAlign::Center);
          Some(MathNode::Fraction {
            numerator: Box::new(num),
            denominator: Box::new(den),
            linethickness,
            bevelled,
            numalign,
            denomalign,
          })
        }
        "msqrt" => {
          let mut children = parse_children(node);
          let child = match children.len() {
            0 => empty_text_node(),
            1 => children.remove(0),
            _ => MathNode::Row(children),
          };
          Some(MathNode::Sqrt(Box::new(child)))
        }
        "mroot" => {
          let mut children = parse_children(node).into_iter();
          let radicand = children.next().unwrap_or_else(empty_text_node);
          let index = children.next().unwrap_or_else(empty_text_node);
          Some(MathNode::Root {
            radicand: Box::new(radicand),
            index: Box::new(index),
          })
        }
        "msup" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let sup = children.next().unwrap_or_else(empty_text_node);
          Some(MathNode::Superscript {
            base: Box::new(base),
            superscript: Box::new(sup),
          })
        }
        "msub" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let sub = children.next().unwrap_or_else(empty_text_node);
          Some(MathNode::Subscript {
            base: Box::new(base),
            subscript: Box::new(sub),
          })
        }
        "msubsup" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let sub = children.next().unwrap_or_else(empty_text_node);
          let sup = children.next().unwrap_or_else(empty_text_node);
          Some(MathNode::SubSuperscript {
            base: Box::new(base),
            subscript: Box::new(sub),
            superscript: Box::new(sup),
          })
        }
        "mover" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let over = children.next().unwrap_or_else(empty_text_node);
          let accent = parse_display_style(node.get_attribute_ref("accent")).unwrap_or(false);
          Some(MathNode::Over {
            base: Box::new(base),
            over: Box::new(over),
            accent,
          })
        }
        "munder" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let under = children.next().unwrap_or_else(empty_text_node);
          let accentunder =
            parse_display_style(node.get_attribute_ref("accentunder")).unwrap_or(false);
          Some(MathNode::Under {
            base: Box::new(base),
            under: Box::new(under),
            accentunder,
          })
        }
        "munderover" => {
          let mut children = parse_children(node).into_iter();
          let base = children.next().unwrap_or_else(empty_text_node);
          let under = children.next().unwrap_or_else(empty_text_node);
          let over = children.next().unwrap_or_else(empty_text_node);
          let accent = parse_display_style(node.get_attribute_ref("accent")).unwrap_or(false);
          let accentunder =
            parse_display_style(node.get_attribute_ref("accentunder")).unwrap_or(false);
          Some(MathNode::UnderOver {
            base: Box::new(base),
            under: Box::new(under),
            over: Box::new(over),
            accent,
            accentunder,
          })
        }
        "mmultiscripts" => {
          let mut iter = node.children.iter();
          let base = iter
            .next()
            .and_then(parse_mathml)
            .unwrap_or_else(empty_text_node);
          let mut pre = Vec::new();
          let mut post_nodes = Vec::new();
          let mut in_pre = false;
          for child in node.children.iter().skip(1) {
            if child
              .tag_name()
              .map(|t| t.eq_ignore_ascii_case("mprescripts"))
              .unwrap_or(false)
            {
              in_pre = true;
              continue;
            }
            if in_pre {
              pre.push(child.clone());
            } else {
              post_nodes.push(child.clone());
            }
          }
          let postscripts = parse_scripts(&post_nodes);
          let prescripts = parse_scripts(&pre);
          Some(MathNode::Multiscripts {
            base: Box::new(base),
            prescripts,
            postscripts,
          })
        }
        "mfenced" => {
          let open = node
            .get_attribute_ref("open")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(".to_string());
          let close = node
            .get_attribute_ref("close")
            .map(|s| s.to_string())
            .unwrap_or_else(|| ")".to_string());
          let separators = match node.get_attribute_ref("separators") {
            None => Some(vec![',']),
            Some(raw) => {
              let parsed: Vec<char> = raw
                .chars()
                .filter(|c| !is_ascii_whitespace_mathml(*c))
                .collect();
              if parsed.is_empty() {
                None
              } else {
                Some(parsed)
              }
            }
          };
          let inner = parse_children(node);
          if inner.is_empty() {
            return None;
          }
          let mut row = Vec::new();
          row.push(MathNode::Operator {
            text: open,
            form: None,
            stretchy: Some(true),
            symmetric: None,
            minsize: None,
            maxsize: None,
            lspace: None,
            rspace: None,
            variant: Some(MathVariant::Normal),
          });
          for (idx, child) in inner.into_iter().enumerate() {
            if idx > 0 {
              if let Some(separators) = &separators {
                let sep = separators
                  .get(idx - 1)
                  .or_else(|| separators.last())
                  .copied()
                  .unwrap_or(',');
                row.push(MathNode::Operator {
                  text: sep.to_string(),
                  form: None,
                  stretchy: Some(false),
                  symmetric: None,
                  minsize: None,
                  maxsize: None,
                  lspace: None,
                  rspace: None,
                  variant: Some(MathVariant::Normal),
                });
              }
            }
            row.push(child);
          }
          row.push(MathNode::Operator {
            text: close,
            form: None,
            stretchy: Some(true),
            symmetric: None,
            minsize: None,
            maxsize: None,
            lspace: None,
            rspace: None,
            variant: Some(MathVariant::Normal),
          });
          Some(MathNode::Row(row))
        }
        "menclose" => {
          let notation = parse_menclose_notation(node.get_attribute_ref("notation"));
          let child = wrap_row_or_single(parse_children(node)).unwrap_or_else(empty_text_node);
          Some(MathNode::Enclose {
            notation,
            child: Box::new(child),
          })
        }
        "mtr" => Some(MathNode::Row(
          node.children.iter().filter_map(parse_mathml).collect(),
        )),
        "mtd" => Some(MathNode::Row(parse_children(node))),
        "mtable" => {
          let table_row_aligns = parse_row_align_list(node.get_attribute_ref("rowalign"));
          let table_col_aligns = parse_column_align_list(node.get_attribute_ref("columnalign"));
          let column_spacings =
            parse_math_space_list(node.get_attribute_ref("columnspacing"), DEFAULT_TABLE_COLUMN_SPACING);
          let row_spacings =
            parse_math_space_list(node.get_attribute_ref("rowspacing"), DEFAULT_TABLE_ROW_SPACING);
          let column_lines = parse_table_line_list(node.get_attribute_ref("columnlines"));
          let row_lines = parse_table_line_list(node.get_attribute_ref("rowlines"));
          let frame = parse_table_line_style(node.get_attribute_ref("frame"));
          let frame_spacing = parse_frame_spacing(node.get_attribute_ref("framespacing"));
          let mut rows = Vec::new();
          for child in node.children.iter() {
            let Some(tag) = child.tag_name() else {
              continue;
            };
            if tag.eq_ignore_ascii_case("mtr") || tag.eq_ignore_ascii_case("mtd") {
              let row_aligns = parse_row_align_list(child.get_attribute_ref("rowalign"));
              let row_col_aligns = parse_column_align_list(child.get_attribute_ref("columnalign"));
              let mut cells = Vec::new();
              let cell_nodes: Vec<&DomNode> = if tag.eq_ignore_ascii_case("mtd") {
                vec![child]
              } else {
                child
                  .children
                  .iter()
                  .filter(|n| {
                    n.tag_name()
                      .map(|t| t.eq_ignore_ascii_case("mtd") || t.eq_ignore_ascii_case("mth"))
                      .unwrap_or(false)
                  })
                  .collect()
              };
              for cell_node in cell_nodes {
                let cell_align = cell_node
                  .get_attribute_ref("columnalign")
                  .and_then(|v| parse_column_align_list(Some(v)).into_iter().next());
                let row_align = cell_node
                  .get_attribute_ref("rowalign")
                  .and_then(|v| parse_row_align_list(Some(v)).into_iter().next());
                let content = parse_mathml(cell_node).unwrap_or_else(empty_text_node);
                cells.push(MathTableCell {
                  content,
                  row_align,
                  column_align: cell_align,
                });
              }
              rows.push(MathTableRow {
                cells,
                row_align: row_aligns.get(0).cloned(),
                column_aligns: row_col_aligns,
              });
            }
          }
          Some(MathNode::Table(MathTable {
            rows,
            column_aligns: table_col_aligns,
            row_aligns: table_row_aligns,
            column_spacings,
            row_spacings,
            column_lines,
            row_lines,
            frame,
            frame_spacing,
          }))
        }
        _ => Some(MathNode::Row(parse_children(node))),
      };

      parsed.map(|parsed_node| apply_presentation_attributes(node, tag.as_str(), parsed_node))
    }
  }
}

/// Layout engine for math trees.
pub struct MathLayoutContext {
  pipeline: ShapingPipeline,
  font_ctx: FontContext,
}

enum StretchOrientation {
  Vertical { target: f32 },
  Horizontal { target: f32 },
}

#[derive(Debug, Clone, Copy)]
struct OperatorProperties {
  fence: bool,
  separator: bool,
  stretchy: bool,
  large_op: bool,
  movable_limits: bool,
  symmetric: bool,
  minsize: Option<MathLength>,
  maxsize: Option<MathLength>,
  lspace: MathLengthOrKeyword,
  rspace: MathLengthOrKeyword,
}

#[derive(Debug, Clone, Copy)]
struct OperatorLike<'a> {
  text: &'a str,
  form: Option<OperatorForm>,
  stretchy: Option<bool>,
  symmetric: Option<bool>,
  minsize: Option<MathLength>,
  maxsize: Option<MathLength>,
  lspace: Option<MathLengthOrKeyword>,
  rspace: Option<MathLengthOrKeyword>,
}

impl OperatorProperties {
  fn empty() -> Self {
    Self {
      fence: false,
      separator: false,
      stretchy: false,
      large_op: false,
      movable_limits: false,
      symmetric: false,
      minsize: None,
      maxsize: None,
      lspace: MathLengthOrKeyword::Zero,
      rspace: MathLengthOrKeyword::Zero,
    }
  }
}

impl StretchOrientation {
  fn target(&self) -> f32 {
    match self {
      StretchOrientation::Vertical { target } | StretchOrientation::Horizontal { target } => {
        *target
      }
    }
  }

  fn main_dimension(&self, layout: &MathLayout) -> f32 {
    match self {
      StretchOrientation::Vertical { .. } => layout.height,
      StretchOrientation::Horizontal { .. } => layout.width,
    }
  }
}

impl MathLayoutContext {
  pub fn new(font_ctx: FontContext) -> Self {
    Self {
      pipeline: ShapingPipeline::new(),
      font_ctx,
    }
  }

  fn rule_thickness(style: &MathStyle) -> f32 {
    let base = (style.font_size * 0.06).clamp(1.0, style.font_size * 0.5);
    if style.display_style {
      base * 1.1
    } else {
      base
    }
  }

  fn axis_height(
    metrics: &ScaledMetrics,
    style: &MathStyle,
    constants: Option<&MathConstants>,
  ) -> f32 {
    if let Some(c) = constants.and_then(|c| c.axis_height) {
      return c;
    }
    metrics
      .x_height
      .unwrap_or(style.font_size * 0.5)
      .max(style.font_size * 0.2)
      * 0.5
  }

  fn script_gap(style: &MathStyle) -> f32 {
    style.font_size * if style.display_style { 0.12 } else { 0.1 }
  }

  fn frac_gap(style: &MathStyle) -> f32 {
    style.font_size * if style.display_style { 0.25 } else { 0.18 }
  }

  fn sqrt_padding(style: &MathStyle) -> f32 {
    style.font_size * if style.display_style { 0.14 } else { 0.1 }
  }

  fn table_spacing(style: &MathStyle) -> (f32, f32) {
    (style.font_size * 0.5, style.font_size * 0.25)
  }

  fn is_open_fence(text: &str) -> bool {
    let prefix = operator_dict::lookup(text, OperatorForm::Prefix)
      .map(|p| p.fence)
      .unwrap_or(false);
    let postfix = operator_dict::lookup(text, OperatorForm::Postfix)
      .map(|p| p.fence)
      .unwrap_or(false);
    prefix && !postfix
  }

  fn is_close_fence(text: &str) -> bool {
    let prefix = operator_dict::lookup(text, OperatorForm::Prefix)
      .map(|p| p.fence)
      .unwrap_or(false);
    let postfix = operator_dict::lookup(text, OperatorForm::Postfix)
      .map(|p| p.fence)
      .unwrap_or(false);
    postfix && !prefix
  }

  fn is_always_postfix_operator(text: &str) -> bool {
    matches!(text, "!" | "′" | "″" | "‴")
  }

  fn operator_default_properties(text: &str, form: OperatorForm) -> OperatorProperties {
    operator_dict::lookup(text, form)
      .or_else(|| {
        if form != OperatorForm::Infix {
          operator_dict::lookup(text, OperatorForm::Infix)
        } else {
          None
        }
      })
      .map(|props| OperatorProperties {
        fence: props.fence,
        separator: props.separator,
        stretchy: props.stretchy,
        large_op: props.large_op,
        movable_limits: props.movable_limits,
        // MathML Core: common delimiters default symmetric stretching.
        symmetric: props.fence,
        minsize: None,
        maxsize: None,
        lspace: props.lspace,
        rspace: props.rspace,
      })
      .unwrap_or_else(OperatorProperties::empty)
  }

  fn operator_like<'a>(node: &'a MathNode) -> Option<OperatorLike<'a>> {
    match node {
      MathNode::Operator {
        text,
        form,
        stretchy,
        symmetric,
        minsize,
        maxsize,
        lspace,
        rspace,
        ..
      } => Some(OperatorLike {
        text: text.as_str(),
        form: *form,
        stretchy: *stretchy,
        symmetric: *symmetric,
        minsize: *minsize,
        maxsize: *maxsize,
        lspace: *lspace,
        rspace: *rspace,
      }),
      MathNode::Style { children, .. } if children.len() == 1 => Self::operator_like(&children[0]),
      MathNode::Row(children) if children.len() == 1 => Self::operator_like(&children[0]),
      _ => None,
    }
  }

  fn is_form_ignorable(node: &MathNode) -> bool {
    match node {
      MathNode::Space { .. } => true,
      MathNode::Text { text, .. } => trim_ascii_whitespace(text).is_empty(),
      _ => false,
    }
  }

  fn inferred_operator_form(children: &[MathNode], index: usize) -> OperatorForm {
    let Some(op) = Self::operator_like(&children[index]) else {
      return OperatorForm::Infix;
    };
    if let Some(form) = op.form {
      return form;
    }
    if Self::is_open_fence(op.text) {
      return OperatorForm::Prefix;
    }
    if Self::is_close_fence(op.text) {
      return OperatorForm::Postfix;
    }
    if Self::is_always_postfix_operator(op.text) {
      return OperatorForm::Postfix;
    }

    let prev = (0..index)
      .rev()
      .find(|idx| !Self::is_form_ignorable(&children[*idx]));
    let next = ((index + 1)..children.len()).find(|idx| !Self::is_form_ignorable(&children[*idx]));

    if prev.is_none() {
      return OperatorForm::Prefix;
    }
    if next.is_none() {
      return OperatorForm::Postfix;
    }

    if let Some(prev_idx) = prev {
      if let Some(prev_op) = Self::operator_like(&children[prev_idx]) {
        if !Self::is_close_fence(prev_op.text) {
          return OperatorForm::Prefix;
        }
      }
    }
    if let Some(next_idx) = next {
      if let Some(next_op) = Self::operator_like(&children[next_idx]) {
        if !Self::is_open_fence(next_op.text) {
          return OperatorForm::Postfix;
        }
      }
    }

    OperatorForm::Infix
  }

  fn resolve_math_font(
    &self,
    base_style: &ComputedStyle,
    math_style: &MathStyle,
    variant: MathVariant,
  ) -> Option<Arc<LoadedFont>> {
    let mut style = base_style.clone();
    style.font_size = math_style.font_size;
    style.font_family = self
      .preferred_math_families_for_variant(base_style, variant)
      .into();
    style.font_style = if variant.is_italic() {
      CssFontStyle::Italic
    } else {
      CssFontStyle::Normal
    };
    if variant.is_bold() {
      style.font_weight = CssFontWeight::Bold;
    }
    let stretch = FontStretch::from_percentage(style.font_stretch.to_percentage());
    self
      .font_ctx
      .get_font_full(
        &style.font_family,
        style.font_weight.to_u16(),
        match style.font_style {
          CssFontStyle::Normal => FontStyle::Normal,
          CssFontStyle::Italic => FontStyle::Italic,
          CssFontStyle::Oblique(_) => FontStyle::Oblique,
        },
        stretch,
      )
      .map(Arc::new)
  }

  fn math_constants_for_layout(
    &self,
    layout: &MathLayout,
    style: &MathStyle,
    base_style: &ComputedStyle,
    fallback_variant: MathVariant,
  ) -> Option<MathConstants> {
    if let Some(glyph) = &layout.annotations.trailing_glyph {
      if let Some(constants) = self.font_ctx.math_constants(&glyph.font, glyph.font_size) {
        return Some(constants);
      }
    }
    self.default_math_constants(style, base_style, fallback_variant)
  }

  fn default_math_constants(
    &self,
    style: &MathStyle,
    base_style: &ComputedStyle,
    variant: MathVariant,
  ) -> Option<MathConstants> {
    let variant = style.default_variant.unwrap_or(variant);
    let font = self.resolve_math_font(base_style, style, variant)?;
    self.font_ctx.math_constants(&font, style.font_size)
  }

  fn layout_glyph_by_id(
    &self,
    font: Arc<LoadedFont>,
    glyph_id: u16,
    font_size: f32,
  ) -> Option<MathLayout> {
    let face = crate::text::face_cache::get_ttf_face(&font)?;
    let face = face.face();
    let glyph = ttf_parser::GlyphId(glyph_id);
    let metrics = font.metrics().ok()?.scale(font_size);
    let bbox = face.glyph_bounding_box(glyph);
    let advance = face
      .glyph_hor_advance(glyph)
      .map(|v| v as f32 * metrics.scale)
      .unwrap_or(0.0);
    let (ascent, descent, width) = if let Some(bbox) = bbox {
      (
        bbox.y_max as f32 * metrics.scale,
        -(bbox.y_min as f32) * metrics.scale,
        advance.max((bbox.x_max - bbox.x_min) as f32 * metrics.scale),
      )
    } else {
      (
        metrics.ascent,
        metrics.descent,
        advance.max(metrics.font_size * 0.5),
      )
    };
    let glyph_pos = crate::text::pipeline::GlyphPosition {
      glyph_id: glyph_id as u32,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: advance,
      y_advance: 0.0,
    };
    let run = ShapedRun {
      text: String::new(),
      start: 0,
      end: 0,
      glyphs: vec![glyph_pos],
      direction: TextDirection::LeftToRight,
      level: 0,
      advance,
      font: font.clone(),
      font_size,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: crate::text::pipeline::RunRotation::None,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    };
    let mut annotations = MathLayoutAnnotations::default();
    let italic_correction = self
      .font_ctx
      .math_italic_correction(&font, glyph_id, font_size)
      .unwrap_or(0.0);
    annotations.trailing_glyph = Some(MathGlyph {
      font,
      glyph_id,
      font_size,
      italic_correction,
    });
    Some(MathLayout {
      width,
      height: ascent + descent,
      baseline: ascent,
      fragments: vec![MathFragment::Glyph {
        origin: Point::new(0.0, ascent),
        run,
      }],
      annotations,
    })
  }

  fn align_stretch(
    &self,
    layout: MathLayout,
    target_ascent: f32,
    target_descent: f32,
    baseline_shift: f32,
  ) -> MathLayout {
    let desired_height = (target_ascent + target_descent).max(layout.height);
    let offset_y =
      (target_ascent - baseline_shift) - layout.baseline + (desired_height - layout.height) * 0.5;
    let fragments = layout
      .fragments
      .into_iter()
      .map(|f| f.translate(Point::new(0.0, offset_y)))
      .collect();
    MathLayout {
      baseline: target_ascent,
      height: desired_height,
      width: layout.width,
      fragments,
      annotations: layout.annotations,
    }
  }

  fn build_glyph_construction(
    &self,
    font: Arc<LoadedFont>,
    construction: ttf_parser::math::GlyphConstruction<'static>,
    min_overlap: f32,
    orientation: StretchOrientation,
    font_size: f32,
  ) -> Option<MathLayout> {
    let scale = font.metrics().ok()?.scale(font_size).scale;
    let target_main = orientation.target();
    let mut best_variant: Option<(MathLayout, f32)> = None;
    for idx in 0..(construction.variants.len() as usize) {
      let Some(var) = construction.variants.get(idx as u16) else {
        continue;
      };
      let layout = self.layout_glyph_by_id(font.clone(), var.variant_glyph.0, font_size)?;
      let layout_main = orientation.main_dimension(&layout);
      if layout_main >= target_main
        && best_variant.as_ref().map(|(_, h)| *h).unwrap_or(f32::MAX) > layout_main
      {
        best_variant = Some((layout, layout_main));
      }
    }
    if let Some((layout, _)) = best_variant {
      return Some(layout);
    }
    let Some(assembly) = construction.assembly else {
      return None;
    };
    let parts_len = assembly.parts.len() as usize;
    if parts_len == 0 {
      return None;
    }
    let mut parts: Vec<ttf_parser::math::GlyphPart> = Vec::new();
    for idx in 0..parts_len {
      if let Some(part) = assembly.parts.get(idx as u16) {
        parts.push(part);
      }
    }
    let mut extender: Option<ttf_parser::math::GlyphPart> = None;
    let mut base_advance: f32 = 0.0;
    for part in &parts {
      let advance = part.full_advance as f32 * scale;
      if part.part_flags.extender() && extender.is_none() {
        extender = Some(*part);
      }
      base_advance += advance;
    }
    let overlap = min_overlap;
    let base_height = base_advance - overlap * (parts.len().saturating_sub(1)) as f32;
    let extender_advance = extender
      .as_ref()
      .map(|p| (p.full_advance as f32 * scale - overlap).max(0.0))
      .unwrap_or(0.0);
    let repeat_count = if target_main > 0.0 && extender_advance > 0.0 && base_height < target_main {
      ((target_main - base_height) / extender_advance)
        .ceil()
        .max(0.0) as usize
    } else {
      0
    };
    let mut assembly_parts: Vec<ttf_parser::math::GlyphPart> = Vec::new();
    for part in &parts {
      assembly_parts.push(*part);
      if part.part_flags.extender() && repeat_count > 0 {
        for _ in 0..repeat_count {
          assembly_parts.push(*part);
        }
      }
    }
    let mut fragments = Vec::new();
    let mut max_width: f32 = 0.0;
    let mut annotations = MathLayoutAnnotations::default();
    match orientation {
      StretchOrientation::Vertical { .. } => {
        let mut laid_out_parts = Vec::new();
        for part in &assembly_parts {
          let layout = self.layout_glyph_by_id(font.clone(), part.glyph_id.0, font_size)?;
          max_width = max_width.max(layout.width);
          laid_out_parts.push((layout, *part));
        }
        let mut cursor = 0.0;
        for (idx, (layout, part)) in laid_out_parts.into_iter().enumerate() {
          let x = (max_width - layout.width) * 0.5;
          for frag in layout.fragments {
            fragments.push(frag.translate(Point::new(x, cursor)));
          }
          annotations = annotations.merge_trailing(&layout.annotations);
          if idx + 1 < assembly_parts.len() {
            cursor += (part.full_advance as f32 * scale) - overlap;
          } else {
            cursor += part.full_advance as f32 * scale;
          }
        }
        Some(MathLayout {
          width: max_width,
          height: cursor,
          baseline: cursor / 2.0,
          fragments,
          annotations,
        })
      }
      StretchOrientation::Horizontal { .. } => {
        let mut laid_out_parts = Vec::new();
        let mut baseline: f32 = 0.0;
        let mut max_height: f32 = 0.0;
        for part in &assembly_parts {
          let layout = self.layout_glyph_by_id(font.clone(), part.glyph_id.0, font_size)?;
          max_height = max_height.max(layout.height);
          baseline = baseline.max(layout.baseline);
          laid_out_parts.push((layout, *part));
        }
        let mut cursor = 0.0;
        for (idx, (layout, part)) in laid_out_parts.into_iter().enumerate() {
          let y_offset = baseline - layout.baseline;
          for frag in layout.fragments {
            fragments.push(frag.translate(Point::new(cursor, y_offset)));
          }
          annotations = annotations.merge_trailing(&layout.annotations);
          max_height = max_height.max(y_offset + layout.height);
          let advance = part.full_advance as f32 * scale;
          if idx + 1 < assembly_parts.len() {
            cursor += advance - overlap;
          } else {
            cursor += advance;
          }
        }
        Some(MathLayout {
          width: cursor,
          height: max_height,
          baseline,
          fragments,
          annotations,
        })
      }
    }
  }

  fn stretch_operator_vertical(
    &mut self,
    text: &str,
    variant: MathVariant,
    target_ascent: f32,
    target_descent: f32,
    style: &MathStyle,
    base_style: &ComputedStyle,
    apply_delimited_min_height: bool,
    apply_display_operator_min_height: bool,
    baseline_shift: f32,
  ) -> Option<MathLayout> {
    let required_height = target_ascent + target_descent;
    let (runs, _base_metrics) = self.shape_text(text, base_style, style, variant);
    let metrics = runs
      .get(0)
      .and_then(|run| self.font_ctx.get_scaled_metrics(&run.font, style.font_size))
      .unwrap_or_else(|| self.base_font_metrics(base_style, style.font_size));
    let Some(first_run) = runs.first() else {
      return None;
    };
    let glyph_id = first_run.glyphs.first().map(|g| g.glyph_id as u16)?;
    let font = first_run.font.clone();
    let target_height = if apply_delimited_min_height || apply_display_operator_min_height {
      self
        .font_ctx
        .math_constants(&font, style.font_size)
        .and_then(|c| {
          let mut h = required_height;
          if apply_delimited_min_height {
            if let Some(min) = c.delimited_sub_formula_min_height {
              h = h.max(min);
            }
          }
          if apply_display_operator_min_height {
            if let Some(min) = c.display_operator_min_height {
              h = h.max(min);
            }
          }
          Some(h)
        })
        .unwrap_or(required_height)
    } else {
      required_height
    };
    if let Some((construction, min_overlap)) =
      self
        .font_ctx
        .math_glyph_construction(&font, glyph_id, true, style.font_size)
    {
      if let Some(layout) = self.build_glyph_construction(
        font.clone(),
        construction,
        min_overlap,
        StretchOrientation::Vertical {
          target: target_height,
        },
        style.font_size,
      ) {
        return Some(self.align_stretch(layout, target_ascent, target_descent, baseline_shift));
      }
    }
    let current_height = metrics.ascent + metrics.descent;
    let factor = if current_height > 0.0 {
      (target_height / current_height).clamp(1.0, 8.0)
    } else {
      1.0
    };
    if factor > 1.01 {
      let mut stretch_style = *style;
      stretch_style.font_size *= factor;
      let resolved_variant =
        self.resolve_variant(Some(variant), &stretch_style, MathVariant::Normal);
      let layout = self.layout_glyphs(text, base_style, &stretch_style, resolved_variant);
      return Some(self.align_stretch(layout, target_ascent, target_descent, baseline_shift));
    }
    None
  }

  fn stretch_operator_horizontal(
    &mut self,
    text: &str,
    variant: MathVariant,
    target_width: f32,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> Option<MathLayout> {
    if target_width <= 0.0 {
      return None;
    }
    let (runs, _base_metrics) = self.shape_text(text, base_style, style, variant);
    let Some(first_run) = runs.first() else {
      return None;
    };
    let glyph_id = first_run.glyphs.first().map(|g| g.glyph_id as u16)?;
    let font = first_run.font.clone();
    if let Some((construction, min_overlap)) =
      self
        .font_ctx
        .math_glyph_construction(&font, glyph_id, false, style.font_size)
    {
      if let Some(layout) = self.build_glyph_construction(
        font.clone(),
        construction,
        min_overlap,
        StretchOrientation::Horizontal {
          target: target_width,
        },
        style.font_size,
      ) {
        if layout.width >= target_width * 0.99 {
          return Some(layout);
        }
      }
    }
    let current_width: f32 = runs.iter().map(|r| r.advance).sum();
    let factor = if current_width > 0.0 {
      (target_width / current_width).clamp(1.0, 8.0)
    } else {
      1.0
    };
    if factor > 1.01 {
      let mut stretch_style = *style;
      stretch_style.font_size *= factor;
      let resolved_variant =
        self.resolve_variant(Some(variant), &stretch_style, MathVariant::Normal);
      let layout = self.layout_glyphs(text, base_style, &stretch_style, resolved_variant);
      return Some(layout);
    }
    None
  }

  fn resolve_variant(
    &self,
    explicit: Option<MathVariant>,
    style: &MathStyle,
    fallback: MathVariant,
  ) -> MathVariant {
    explicit.or(style.default_variant).unwrap_or(fallback)
  }

  fn resolve_length(&self, len: MathLength, style: &MathStyle, metrics: &ScaledMetrics) -> f32 {
    match len {
      MathLength::Em(v) => v * style.font_size,
      MathLength::Ex(v) => v * metrics.x_height.unwrap_or(style.font_size * 0.5),
      MathLength::Px(v) => v,
    }
  }

  fn resolve_math_space(
    &self,
    space: MathLengthOrKeyword,
    style: &MathStyle,
    metrics: &ScaledMetrics,
  ) -> f32 {
    // MathML Core keywords match TeX mu spacings: 3/18, 4/18, 5/18 em.
    // https://w3c.github.io/mathml-core/#dfn-thinmathspace
    match space {
      MathLengthOrKeyword::Thin => style.font_size * (3.0 / 18.0),
      MathLengthOrKeyword::Medium => style.font_size * (4.0 / 18.0),
      MathLengthOrKeyword::Thick => style.font_size * (5.0 / 18.0),
      MathLengthOrKeyword::Zero => 0.0,
      MathLengthOrKeyword::Length(len) => self.resolve_length(len, style, metrics),
    }
  }

  fn apply_style_overrides(
    &self,
    style: &MathStyle,
    overrides: &MathStyleOverrides,
    base_style: &ComputedStyle,
  ) -> MathStyle {
    let mut next = *style;
    if let Some(display) = overrides.display_style {
      next.display_style = display;
    }
    if let Some(variant) = overrides.math_variant {
      next.default_variant = Some(variant);
    }
    if let Some(size) = overrides.math_size {
      next.font_size = match size {
        MathSize::Scale(f) => (style.font_size * f).max(1.0),
        MathSize::Absolute(px) => px.max(1.0),
      };
    }
    if let Some(script_level) = overrides.script_level {
      let target = match script_level {
        MathScriptLevel::Absolute(level) => level.min(MAX_SCRIPT_LEVEL),
        MathScriptLevel::Relative(delta) => {
          let raw = next.script_level as i32 + delta;
          raw.clamp(0, MAX_SCRIPT_LEVEL as i32) as u8
        }
      };
      let constants = self.default_math_constants(&next, base_style, MathVariant::Normal);
      next = next.with_script_level(target, constants.as_ref());
    }
    next
  }

  fn base_font_metrics(&self, style: &ComputedStyle, size: f32) -> ScaledMetrics {
    let mut clone = style.clone();
    clone.font_size = size;
    let italic = matches!(clone.font_style, CssFontStyle::Italic);
    let oblique = matches!(clone.font_style, CssFontStyle::Oblique(_));
    let stretch = FontStretch::from_percentage(clone.font_stretch.to_percentage());
    self
      .font_ctx
      .get_font_full(
        &clone.font_family,
        clone.font_weight.to_u16(),
        if italic {
          FontStyle::Italic
        } else if oblique {
          FontStyle::Oblique
        } else {
          FontStyle::Normal
        },
        stretch,
      )
      .and_then(|font| self.font_ctx.get_scaled_metrics(&font, size))
      .unwrap_or_else(|| ScaledMetrics {
        font_size: size,
        scale: 1.0,
        ascent: size * 0.8,
        descent: size * 0.2,
        line_gap: 0.0,
        line_height: size,
        x_height: Some(size * 0.5),
        cap_height: Some(size * 0.7),
        underline_position: size * 0.05,
        underline_thickness: size * 0.05,
      })
  }

  fn preferred_math_families(&self, style: &ComputedStyle) -> Vec<String> {
    let mut families: Vec<String> = Vec::new();
    families.push("math".to_string());
    families.extend(self.font_ctx.math_family_names());
    for fam in style.font_family.iter() {
      if !families.iter().any(|f| f.eq_ignore_ascii_case(fam)) {
        families.push(fam.clone());
      }
    }
    families
  }

  fn variant_preferred_families(&self, variant: MathVariant) -> Vec<String> {
    let mut families = Vec::new();
    match variant {
      MathVariant::SansSerif
      | MathVariant::SansSerifBold
      | MathVariant::SansSerifItalic
      | MathVariant::SansSerifBoldItalic => families.push("sans-serif".to_string()),
      MathVariant::Monospace => families.push("monospace".to_string()),
      MathVariant::DoubleStruck => {
        families.push("math-doublestruck".to_string());
        families.push("double-struck".to_string());
      }
      MathVariant::Script | MathVariant::BoldScript => {
        families.push("math-script".to_string());
        families.push("script".to_string());
      }
      MathVariant::Fraktur | MathVariant::BoldFraktur => {
        families.push("math-fraktur".to_string());
        families.push("fraktur".to_string());
      }
      _ => {}
    }
    families
  }

  fn preferred_math_families_for_variant(
    &self,
    style: &ComputedStyle,
    variant: MathVariant,
  ) -> Vec<String> {
    let mut families = self.variant_preferred_families(variant);
    for fam in self.preferred_math_families(style) {
      if !families.iter().any(|f| f.eq_ignore_ascii_case(&fam)) {
        families.push(fam);
      }
    }
    families
  }

  fn shape_text(
    &mut self,
    text: &str,
    base_style: &ComputedStyle,
    math_style: &MathStyle,
    variant: MathVariant,
  ) -> (Vec<ShapedRun>, ScaledMetrics) {
    let mapped_text = map_math_alphanumerics(text, variant);
    let text = mapped_text.as_ref();
    let mut style = base_style.clone();
    style.font_size = math_style.font_size;
    style.font_family = self
      .preferred_math_families_for_variant(base_style, variant)
      .into();
    style.font_style = if variant.is_italic() {
      CssFontStyle::Italic
    } else {
      CssFontStyle::Normal
    };
    if variant.is_bold() {
      style.font_weight = CssFontWeight::Bold;
    }
    if math_style.script_level > 0 {
      let ssty_value = if math_style.script_level == 1 { 1 } else { 2 };
      let mut feature_settings = style.font_feature_settings.as_ref().to_vec();
      feature_settings.retain(|setting| setting.tag != *b"ssty");
      feature_settings.push(FontFeatureSetting {
        tag: *b"ssty",
        value: ssty_value,
      });
      style.font_feature_settings = feature_settings.into();
    }

    let metrics = self.base_font_metrics(&style, style.font_size);
    let runs = match self.pipeline.shape_with_direction(
      text,
      &style,
      &self.font_ctx,
      TextDirection::LeftToRight,
    ) {
      Ok(mut r) => {
        crate::layout::contexts::inline::line_builder::TextItem::apply_spacing_to_runs(
          &mut r,
          text,
          style.letter_spacing,
          style.word_spacing,
        );
        r
      }
      Err(_) => Vec::new(),
    };
    (runs, metrics)
  }

  fn layout_glyphs(
    &mut self,
    text: &str,
    base_style: &ComputedStyle,
    math_style: &MathStyle,
    variant: MathVariant,
  ) -> MathLayout {
    let (runs, base_metrics) = self.shape_text(text, base_style, math_style, variant);
    if runs.is_empty() {
      let height = math_style.font_size;
      return MathLayout {
        width: math_style.font_size * text.len() as f32 * 0.6,
        height,
        baseline: height * 0.8,
        fragments: vec![],
        annotations: MathLayoutAnnotations::default(),
      };
    }

    let metrics = runs
      .get(0)
      .and_then(|run| {
        self
          .font_ctx
          .get_scaled_metrics(&run.font, math_style.font_size)
      })
      .unwrap_or(base_metrics);
    let ascent = metrics.ascent;
    let descent = metrics.descent;
    let width: f32 = runs.iter().map(|r| r.advance).sum();
    let mut fragments = Vec::new();
    let mut pen_x = 0.0;
    let mut annotations = MathLayoutAnnotations::default();
    if let Some(last_run) = runs.last() {
      if let Some(last_glyph) = last_run.glyphs.last() {
        let italic_correction = self
          .font_ctx
          .math_italic_correction(
            &last_run.font,
            last_glyph.glyph_id as u16,
            math_style.font_size,
          )
          .unwrap_or(0.0);
        annotations.trailing_glyph = Some(MathGlyph {
          font: last_run.font.clone(),
          glyph_id: last_glyph.glyph_id as u16,
          font_size: math_style.font_size,
          italic_correction,
        });
      }
    }
    for run in runs {
      let origin = Point::new(pen_x, ascent);
      pen_x += run.advance;
      fragments.push(MathFragment::Glyph { origin, run });
    }
    MathLayout {
      width,
      height: ascent + descent,
      baseline: ascent,
      fragments,
      annotations,
    }
  }

  fn layout_row_children_and_properties(
    &mut self,
    children: &[MathNode],
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> (Vec<MathLayout>, Vec<Option<OperatorProperties>>) {
    let mut layouts = Vec::with_capacity(children.len());
    for child in children {
      layouts.push(self.layout_node(child, style, base_style));
    }

    // Determine default operator properties. These drive both stretching and spacing.
    let mut operator_props: Vec<Option<OperatorProperties>> = vec![None; children.len()];
    for (idx, child) in children.iter().enumerate() {
      let Some(op) = Self::operator_like(child) else {
        continue;
      };
      let form = Self::inferred_operator_form(children, idx);
      let mut props = Self::operator_default_properties(op.text, form);
      props.stretchy = op.stretchy.unwrap_or(props.stretchy);
      props.symmetric = op.symmetric.unwrap_or(props.symmetric);
      props.minsize = op.minsize.or(props.minsize);
      props.maxsize = op.maxsize.or(props.maxsize);
      props.lspace = op.lspace.unwrap_or(props.lspace);
      props.rspace = op.rspace.unwrap_or(props.rspace);
      operator_props[idx] = Some(props);
    }

    // Size large operators in display style even when they're not marked stretchy. OpenType MATH
    // fonts provide vertical variants (or an explicit minimum via `display_operator_min_height`)
    // for operators such as ∑/∫/⋂. Unlike delimiters, these should not stretch to surrounding
    // content; they only need to meet the display operator minimum.
    if style.display_style {
      for (idx, props) in operator_props.iter().enumerate() {
        let Some(props) = props else {
          continue;
        };
        if !props.large_op || props.stretchy {
          continue;
        }
        let Some(MathNode::Operator { text, variant, .. }) = children.get(idx) else {
          continue;
        };
        let Some(current) = layouts.get(idx) else {
          continue;
        };
        let target_ascent = current.baseline;
        let target_descent = current.height - current.baseline;
        let resolved_variant = self.resolve_variant(*variant, style, MathVariant::Normal);
        if let Some(layout) = self.stretch_operator_vertical(
          text,
          resolved_variant,
          target_ascent,
          target_descent,
          style,
          base_style,
          false,
          true,
          0.0,
        ) {
          if let Some(slot) = layouts.get_mut(idx) {
            *slot = layout;
          }
        }
      }
    }

    // Stretch operators after seeing surrounding content.
    let stretchy_indices: Vec<usize> = operator_props
      .iter()
      .enumerate()
      .filter_map(|(idx, props)| {
        if !matches!(children.get(idx), Some(MathNode::Operator { .. })) {
          return None;
        }
        props.filter(|props| props.stretchy).map(|_| idx)
      })
      .collect();

    if !stretchy_indices.is_empty() {
      let metrics = self.base_font_metrics(base_style, style.font_size);
      let mut stretchy_mask = vec![false; layouts.len()];
      for idx in &stretchy_indices {
        if let Some(slot) = stretchy_mask.get_mut(*idx) {
          *slot = true;
        }
      }
      let mut target_ascent: f32 = 0.0;
      let mut target_descent: f32 = 0.0;
      for (idx, layout) in layouts.iter().enumerate() {
        if stretchy_mask.get(idx).copied().unwrap_or(false) {
          continue;
        }
        target_ascent = target_ascent.max(layout.baseline);
        target_descent = target_descent.max(layout.height - layout.baseline);
      }
      if target_ascent == 0.0 && target_descent == 0.0 {
        for layout in &layouts {
          target_ascent = target_ascent.max(layout.baseline);
          target_descent = target_descent.max(layout.height - layout.baseline);
        }
      }
      let pad = Self::rule_thickness(style) * 0.5;
      target_ascent += pad;
      target_descent += pad;
      if target_ascent == 0.0 && target_descent == 0.0 {
        target_ascent = style.font_size * 0.8;
        target_descent = style.font_size * 0.2;
      }

      let base_total = target_ascent + target_descent;

      for idx in stretchy_indices {
        let Some(props) = operator_props.get(idx).and_then(|p| *p) else {
          continue;
        };
        let MathNode::Operator { text, variant, .. } = &children[idx] else {
          continue;
        };

        let resolved_variant = self.resolve_variant(*variant, style, MathVariant::Normal);
        let constants = self.default_math_constants(style, base_style, resolved_variant);

        let mut total = base_total;
        if props.fence {
          if let Some(min) = constants
            .as_ref()
            .and_then(|c| c.delimited_sub_formula_min_height)
          {
            total = total.max(min);
          }
        }
        if props.large_op && style.display_style {
          if let Some(min) = constants
            .as_ref()
            .and_then(|c| c.display_operator_min_height)
          {
            total = total.max(min);
          }
        }
        if let Some(min) = props.minsize {
          total = total.max(self.resolve_length(min, style, &metrics));
        }
        if let Some(max) = props.maxsize {
          total = total.min(self.resolve_length(max, style, &metrics));
        }

        let (target_ascent, target_descent, baseline_shift) = if props.symmetric {
          let axis = constants
            .as_ref()
            .and_then(|c| c.axis_height)
            .unwrap_or(0.0);
          let half = (total * 0.5).max(0.0);
          let axis = axis.min(half);
          (half + axis, half - axis, axis)
        } else if base_total > 0.0 {
          let factor = total / base_total;
          (target_ascent * factor, target_descent * factor, 0.0)
        } else {
          (target_ascent, target_descent, 0.0)
        };

        if let Some(layout) = self.stretch_operator_vertical(
          text,
          resolved_variant,
          target_ascent,
          target_descent,
          style,
          base_style,
          false,
          false,
          baseline_shift,
        ) {
          layouts[idx] = layout;
        }
      }
    }

    (layouts, operator_props)
  }

  fn layout_row(
    &mut self,
    children: &[MathNode],
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let (layouts, operator_props) =
      self.layout_row_children_and_properties(children, style, base_style);
    if layouts.is_empty() {
      return self.layout_glyphs("", base_style, style, MathVariant::Normal);
    }

    let mut max_ascent: f32 = 0.0;
    let mut max_descent: f32 = 0.0;
    for layout in &layouts {
      max_ascent = max_ascent.max(layout.baseline);
      max_descent = max_descent.max(layout.height - layout.baseline);
    }
    let baseline = max_ascent;
    let mut x = 0.0;
    let mut fragments = Vec::new();
    let metrics = self.base_font_metrics(base_style, style.font_size);
    let trailing_annotations = layouts
      .last()
      .map(|l| l.annotations.clone())
      .unwrap_or_default();
    for (idx, layout) in layouts.into_iter().enumerate() {
      if idx > 0 {
        let mut gap = 0.0;
        if let Some(prev) = operator_props.get(idx - 1).and_then(|p| *p) {
          gap += self.resolve_math_space(prev.rspace, style, &metrics);
        }
        if let Some(curr) = operator_props.get(idx).and_then(|p| *p) {
          gap += self.resolve_math_space(curr.lspace, style, &metrics);
        }
        x += gap;
      }
      let y = baseline - layout.baseline;
      for frag in layout.fragments {
        fragments.push(frag.translate(Point::new(x, y)));
      }
      x += layout.width;
    }
    MathLayout {
      width: x,
      height: baseline + max_descent,
      baseline,
      fragments,
      annotations: trailing_annotations,
    }
  }

  fn layout_space(
    &mut self,
    width: MathLength,
    height: MathLength,
    depth: MathLength,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let metrics = self.base_font_metrics(base_style, style.font_size);
    let w = self.resolve_length(width, style, &metrics).max(0.0);
    let h = self.resolve_length(height, style, &metrics).max(0.0);
    let d = self.resolve_length(depth, style, &metrics).max(0.0);

    let total_h = h + d;
    let (height, baseline) = if total_h == 0.0 {
      (0.0, 0.0)
    } else {
      (total_h, h)
    };
    MathLayout {
      width: w,
      height,
      baseline,
      fragments: Vec::new(),
      annotations: MathLayoutAnnotations::default(),
    }
  }

  fn layout_fraction(
    &mut self,
    num: &MathNode,
    den: &MathNode,
    linethickness: Option<MathLengthOrKeyword>,
    bevelled: bool,
    numalign: ColumnAlign,
    denomalign: ColumnAlign,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    fn align_x(align: ColumnAlign, container_width: f32, child_width: f32) -> f32 {
      let free = (container_width - child_width).max(0.0);
      match align {
        ColumnAlign::Left => 0.0,
        ColumnAlign::Center => free * 0.5,
        ColumnAlign::Right => free,
      }
    }

    let metrics = self.base_font_metrics(base_style, style.font_size);
    let constants = self.default_math_constants(style, base_style, MathVariant::Normal);

    let child_style = if style.display_style {
      let mut next = *style;
      next.display_style = false;
      next
    } else {
      style.script_with_constants(constants.as_ref())
    };
    let numerator = self.layout_node(num, &child_style, base_style);
    let denominator = self.layout_node(den, &child_style, base_style);

    if bevelled {
      let x_height = metrics.x_height.unwrap_or(style.font_size * 0.5);
      let sup_shift = constants
        .as_ref()
        .and_then(|c| c.superscript_shift_up)
        .unwrap_or_else(|| {
          (metrics.ascent * 0.6)
            .max(x_height * 0.65)
            .max(style.font_size * if style.display_style { 0.4 } else { 0.34 })
        });
      let sub_shift = constants
        .as_ref()
        .and_then(|c| c.subscript_shift_down)
        .unwrap_or_else(|| (metrics.descent * 0.8 + x_height * 0.2).max(style.font_size * 0.24));

      let num_ascent = numerator.baseline;
      let num_descent = numerator.height - numerator.baseline;
      let den_ascent = denominator.baseline;
      let den_descent = denominator.height - denominator.baseline;

      let num_top = sup_shift + num_ascent;
      let num_bottom = sup_shift - num_descent;
      let den_top = -sub_shift + den_ascent;
      let den_bottom = -sub_shift - den_descent;

      let target_ascent = num_top.max(den_top).max(0.0);
      let target_descent = (-num_bottom.min(den_bottom)).max(0.0);
      let slash_text = "∕";
      let slash_layout = self
        .stretch_operator_vertical(
          slash_text,
          MathVariant::Normal,
          target_ascent,
          target_descent,
          style,
          base_style,
          false,
          false,
          0.0,
        )
        .unwrap_or_else(|| self.layout_glyphs(slash_text, base_style, style, MathVariant::Normal));

      let slash_ascent = slash_layout.baseline;
      let slash_descent = slash_layout.height - slash_layout.baseline;
      let slash_top = slash_ascent;
      let slash_bottom = -slash_descent;

      let ascent = num_top.max(den_top).max(slash_top).max(0.0);
      let descent = (-num_bottom.min(den_bottom).min(slash_bottom).min(0.0)).max(0.0);
      let baseline = ascent;

      let gap = Self::script_gap(style) * 0.5;
      let num_x = 0.0;
      let slash_x = numerator.width + gap;
      let den_x = slash_x + slash_layout.width + gap;
      let width = den_x + denominator.width;

      let num_y = (baseline - sup_shift) - numerator.baseline;
      let slash_y = baseline - slash_layout.baseline;
      let den_y = (baseline + sub_shift) - denominator.baseline;

      let mut fragments = Vec::new();
      for frag in numerator.fragments {
        fragments.push(frag.translate(Point::new(num_x, num_y)));
      }
      for frag in slash_layout.fragments {
        fragments.push(frag.translate(Point::new(slash_x, slash_y)));
      }
      for frag in denominator.fragments {
        fragments.push(frag.translate(Point::new(den_x, den_y)));
      }

      let annotations = numerator
        .annotations
        .merge_trailing(&slash_layout.annotations)
        .merge_trailing(&denominator.annotations);
      return MathLayout {
        width,
        height: baseline + descent,
        baseline,
        fragments,
        annotations,
      };
    }

    let axis = Self::axis_height(&metrics, style, constants.as_ref());

    let default_rule = constants
      .as_ref()
      .and_then(|c| c.fraction_rule_thickness)
      .unwrap_or_else(|| Self::rule_thickness(style));
    let mut rule = match linethickness {
      None | Some(MathLengthOrKeyword::Medium) => default_rule,
      Some(MathLengthOrKeyword::Thin) => default_rule * 0.5,
      Some(MathLengthOrKeyword::Thick) => default_rule * 2.0,
      Some(MathLengthOrKeyword::Zero) => 0.0,
      Some(MathLengthOrKeyword::Length(len)) => self.resolve_length(len, style, &metrics),
    };
    if rule <= 0.0 {
      rule = 0.0;
    }
    let has_rule = rule > 0.0;

    let num_gap = if style.display_style {
      constants
        .as_ref()
        .and_then(|c| c.fraction_num_display_style_gap_min)
        .unwrap_or_else(|| Self::frac_gap(style))
    } else {
      constants
        .as_ref()
        .and_then(|c| c.fraction_numerator_gap_min)
        .unwrap_or_else(|| Self::frac_gap(style))
    };
    let den_gap = if style.display_style {
      constants
        .as_ref()
        .and_then(|c| c.fraction_denom_display_style_gap_min)
        .unwrap_or_else(|| Self::frac_gap(style))
    } else {
      constants
        .as_ref()
        .and_then(|c| c.fraction_denominator_gap_min)
        .unwrap_or_else(|| Self::frac_gap(style))
    };

    let num_ascent = numerator.baseline;
    let num_descent = numerator.height - numerator.baseline;
    let den_ascent = denominator.baseline;
    let den_descent = denominator.height - denominator.baseline;

    let mut shift_up = constants
      .as_ref()
      .and_then(|c| {
        if style.display_style {
          c.fraction_numerator_display_style_shift_up
        } else {
          c.fraction_numerator_shift_up
        }
      })
      .unwrap_or(0.0);
    let mut shift_down = constants
      .as_ref()
      .and_then(|c| {
        if style.display_style {
          c.fraction_denominator_display_style_shift_down
        } else {
          c.fraction_denominator_shift_down
        }
      })
      .unwrap_or(0.0);

    let min_shift_up = num_descent + num_gap + rule * 0.5;
    let min_shift_down = den_ascent + den_gap + rule * 0.5;
    shift_up = shift_up.max(min_shift_up);
    shift_down = shift_down.max(min_shift_down);

    let num_baseline = axis + shift_up;
    let den_baseline = axis - shift_down;

    let num_top = num_baseline + num_ascent;
    let num_bottom = num_baseline - num_descent;
    let den_top = den_baseline + den_ascent;
    let den_bottom = den_baseline - den_descent;
    let rule_top = axis + rule * 0.5;
    let rule_bottom = axis - rule * 0.5;

    let ascent = num_top.max(den_top).max(rule_top).max(0.0);
    let descent = (-num_bottom.min(den_bottom).min(rule_bottom).min(0.0)).max(0.0);
    let baseline = ascent;

    let width = numerator.width.max(denominator.width);
    let axis_y = baseline - axis;

    let num_x = align_x(numalign, width, numerator.width);
    let den_x = align_x(denomalign, width, denominator.width);
    let num_y = (axis_y - shift_up) - numerator.baseline;
    let den_y = (axis_y + shift_down) - denominator.baseline;

    let mut fragments = Vec::new();
    for frag in numerator.fragments {
      fragments.push(frag.translate(Point::new(num_x, num_y)));
    }
    for frag in denominator.fragments {
      fragments.push(frag.translate(Point::new(den_x, den_y)));
    }
    if has_rule {
      fragments.push(MathFragment::Rule(Rect::from_xywh(
        0.0,
        axis_y - rule * 0.5,
        width,
        rule,
      )));
    }

    let annotations = numerator
      .annotations
      .merge_trailing(&denominator.annotations);
    MathLayout {
      width,
      height: baseline + descent,
      baseline,
      fragments,
      annotations,
    }
  }

  fn layout_superscript(
    &mut self,
    base: &MathNode,
    sup: Option<&MathNode>,
    sub: Option<&MathNode>,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let base_layout = self.layout_node(base, style, base_style);
    let constants =
      self.math_constants_for_layout(&base_layout, style, base_style, MathVariant::Normal);
    let script_style = style.script_with_constants(constants.as_ref());
    let sup_layout = sup.map(|n| self.layout_node(n, &script_style, base_style));
    let sub_layout = sub.map(|n| self.layout_node(n, &script_style, base_style));
    let base_metrics = self.base_font_metrics(base_style, style.font_size);
    let x_height = base_metrics.x_height.unwrap_or(style.font_size * 0.5);
    let sup_shift = constants
      .as_ref()
      .and_then(|c| {
        if sub.is_some() {
          c.superscript_shift_up_cramped
        } else {
          c.superscript_shift_up
        }
      })
      .filter(|v| *v > 0.0)
      .unwrap_or_else(|| {
        (base_metrics.ascent * 0.6)
          .max(x_height * 0.65)
          .max(style.font_size * if style.display_style { 0.4 } else { 0.34 })
      });
    let sub_shift = constants
      .as_ref()
      .and_then(|c| c.subscript_shift_down)
      .filter(|v| *v > 0.0)
      .unwrap_or_else(|| (base_metrics.descent * 0.8 + x_height * 0.2).max(style.font_size * 0.24));
    let min_gap = constants
      .as_ref()
      .and_then(|c| c.sub_superscript_gap_min)
      .unwrap_or_else(|| {
        (Self::script_gap(style) + Self::rule_thickness(style)).max(style.font_size * 0.06)
      });
    let sup_bottom_max_with_sub = constants
      .as_ref()
      .and_then(|c| c.superscript_bottom_max_with_subscript);
    let sub_baseline_drop_min = constants
      .as_ref()
      .and_then(|c| c.subscript_baseline_drop_min);

    let mut width = base_layout.width;
    let mut fragments = Vec::new();
    let mut max_ascent = base_layout.baseline;
    let mut max_descent = base_layout.height - base_layout.baseline;

    let mut script_width: f32 = 0.0;
    if let Some(layout) = &sup_layout {
      script_width = script_width.max(layout.width);
    }
    if let Some(layout) = &sub_layout {
      script_width = script_width.max(layout.width);
    }
    if script_width > 0.0 {
      width += constants
        .as_ref()
        .and_then(|c| c.space_after_script)
        .unwrap_or_else(|| Self::script_gap(style))
        + script_width;
    }
    let mut max_width = width;

    // Base fragments
    for frag in base_layout.fragments {
      fragments.push(frag);
    }

    let gap = if script_width > 0.0 {
      constants
        .as_ref()
        .and_then(|c| c.space_after_script)
        .unwrap_or_else(|| Self::script_gap(style))
    } else {
      0.0
    };
    let x = base_layout.width + gap;
    let base_descent = base_layout.height - base_layout.baseline;
    let italic_correction = base_layout
      .annotations
      .trailing_glyph
      .as_ref()
      .map(|g| g.italic_correction)
      .unwrap_or(0.0);
    let mut trailing_annotations = base_layout.annotations.clone();
    let mut sup_y = None;
    if let Some(layout) = sup_layout {
      let mut y = base_layout.baseline - sup_shift - layout.baseline;
      if let Some(bottom_min) = constants.as_ref().and_then(|c| c.superscript_bottom_min) {
        let sup_bottom = y + layout.height - layout.baseline;
        let allowed = base_layout.baseline - bottom_min;
        if sup_bottom > allowed {
          y -= sup_bottom - allowed;
        }
      }
      if let (Some(limit), true) = (sup_bottom_max_with_sub, sub.is_some()) {
        let sup_bottom = y + layout.height - layout.baseline;
        let allowed = base_layout.baseline - limit;
        if sup_bottom > allowed {
          y -= sup_bottom - allowed;
        }
      }
      let sup_kern = base_layout
        .annotations
        .trailing_glyph
        .as_ref()
        .map(|g| {
          self.font_ctx.math_kern(
            &g.font,
            g.glyph_id,
            layout.baseline,
            g.font_size,
            true,
            MathKernSide::Right,
          )
        })
        .unwrap_or(0.0);
      let sup_x = x + italic_correction + sup_kern;
      for frag in layout.fragments {
        fragments.push(frag.translate(Point::new(sup_x, y)));
      }
      max_width = max_width.max(sup_x + layout.width);
      max_ascent = max_ascent.max(layout.baseline - y);
      max_descent = max_descent.max(layout.height - (layout.baseline - y));
      sup_y = Some((y, layout.height));
      trailing_annotations = trailing_annotations.merge_trailing(&layout.annotations);
    }

    if let Some(layout) = sub_layout {
      let mut y = base_layout.baseline + base_descent + sub_shift - layout.baseline;
      if let Some((sup_y, sup_h)) = sup_y {
        let sup_bottom = sup_y + sup_h;
        let gap = y - sup_bottom;
        if gap < min_gap {
          y += min_gap - gap;
        }
      }
      if let Some(top_max) = constants.as_ref().and_then(|c| c.subscript_top_max) {
        let sub_top = y + layout.baseline;
        let min_top = base_layout.baseline + top_max;
        if sub_top < min_top {
          y += min_top - sub_top;
        }
      }
      if let Some(min_drop) = sub_baseline_drop_min {
        let drop = y + layout.baseline - base_layout.baseline;
        if drop < min_drop {
          y += min_drop - drop;
        }
      }
      let sub_kern = base_layout
        .annotations
        .trailing_glyph
        .as_ref()
        .map(|g| {
          self.font_ctx.math_kern(
            &g.font,
            g.glyph_id,
            layout.height - layout.baseline,
            g.font_size,
            false,
            MathKernSide::Right,
          )
        })
        .unwrap_or(0.0);
      let sub_x = x + sub_kern;
      for frag in layout.fragments {
        fragments.push(frag.translate(Point::new(sub_x, y)));
      }
      max_width = max_width.max(sub_x + layout.width);
      max_ascent = max_ascent.max(layout.baseline - y);
      max_descent = max_descent.max(layout.height - (layout.baseline - y));
      trailing_annotations = trailing_annotations.merge_trailing(&layout.annotations);
    }

    MathLayout {
      width: max_width,
      height: max_ascent + max_descent,
      baseline: max_ascent,
      fragments,
      annotations: trailing_annotations,
    }
  }

  fn layout_under_over(
    &mut self,
    base: &MathNode,
    under: Option<(&MathNode, bool)>,
    over: Option<(&MathNode, bool)>,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let (under, under_is_accent) = under
      .map(|(node, is_accent)| (Some(node), is_accent))
      .unwrap_or((None, false));
    let (over, over_is_accent) = over
      .map(|(node, is_accent)| (Some(node), is_accent))
      .unwrap_or((None, false));

    if !style.display_style && !(under_is_accent || over_is_accent) {
      if let Some(op) = Self::operator_like(base) {
        // MathML Core operator dictionary: large operators such as ∑ have movable limits in
        // display style, but become scripts in inline style.
        let form = op.form.unwrap_or(OperatorForm::Infix);
        if Self::operator_default_properties(op.text, form).movable_limits {
          return self.layout_superscript(base, over, under, style, base_style);
        }
      }
    }

    let base_layout = self.layout_node(base, style, base_style);
    let constants =
      self.math_constants_for_layout(&base_layout, style, base_style, MathVariant::Normal);
    let script_style = style.script_with_constants(constants.as_ref());
    let italic_correction = base_layout
      .annotations
      .trailing_glyph
      .as_ref()
      .map(|g| g.italic_correction)
      .unwrap_or(0.0);
    let align_base_width = if under_is_accent || over_is_accent {
      base_layout.width + italic_correction
    } else {
      base_layout.width
    };
    let stretch_target = align_base_width + Self::rule_thickness(style);

    let base_metrics = base_layout
      .annotations
      .trailing_glyph
      .as_ref()
      .and_then(|g| self.font_ctx.get_scaled_metrics(&g.font, g.font_size))
      .unwrap_or_else(|| self.base_font_metrics(base_style, style.font_size));
    let x_height = base_metrics.x_height.unwrap_or(style.font_size * 0.5);

    let mut layout_script = |n: &MathNode, node_style: &MathStyle| -> MathLayout {
      match n {
        MathNode::Operator {
          text,
          form,
          stretchy,
          minsize,
          maxsize,
          variant,
          ..
        } => {
          let resolved = self.resolve_variant(*variant, node_style, MathVariant::Normal);
          let mut props =
            Self::operator_default_properties(text, (*form).unwrap_or(OperatorForm::Infix));
          props.stretchy = (*stretchy).unwrap_or(props.stretchy);
          props.minsize = (*minsize).or(props.minsize);
          props.maxsize = (*maxsize).or(props.maxsize);
          if props.stretchy {
            let metrics = self.base_font_metrics(base_style, node_style.font_size);
            let mut target = stretch_target;
            if let Some(min) = props.minsize {
              target = target.max(self.resolve_length(min, node_style, &metrics));
            }
            if let Some(max) = props.maxsize {
              target = target.min(self.resolve_length(max, node_style, &metrics));
            }
            self
              .stretch_operator_horizontal(text, resolved, target, node_style, base_style)
              .unwrap_or_else(|| self.layout_node(n, node_style, base_style))
          } else {
            self.layout_node(n, node_style, base_style)
          }
        }
        _ => self.layout_node(n, node_style, base_style),
      }
    };

    let under_layout =
      under.map(|n| layout_script(n, if under_is_accent { style } else { &script_style }));
    let over_layout =
      over.map(|n| layout_script(n, if over_is_accent { style } else { &script_style }));
    let over_gap = constants
      .as_ref()
      .and_then(|c| c.stretch_stack_gap_above_min)
      .unwrap_or_else(|| Self::frac_gap(style));
    let under_gap = constants
      .as_ref()
      .and_then(|c| c.stretch_stack_gap_below_min)
      .unwrap_or_else(|| Self::frac_gap(style));

    let base_ascent = base_layout.baseline;
    let base_descent = base_layout.height - base_layout.baseline;
    // Use the math baseline as origin (y=0) to simplify baseline-correct stacking.
    let base_top = -base_ascent;
    let base_bottom = base_descent;

    let accent_min_gap = Self::rule_thickness(style);

    let mut width = align_base_width;
    if let Some(layout) = &under_layout {
      width = width.max(layout.width);
    }
    if let Some(layout) = &over_layout {
      width = width.max(layout.width);
    }

    let base_x = (width - align_base_width) * 0.5;
    let base_y = base_top;

    let over_pos = over_layout.as_ref().map(|layout| {
      let x = (width - layout.width) * 0.5;
      let mut y = if over_is_accent {
        let use_flattened = constants
          .as_ref()
          .and_then(|c| c.flattened_accent_base_height)
          .is_some()
          && layout.width >= align_base_width * 1.1;
        let base_height = if use_flattened {
          constants
            .as_ref()
            .and_then(|c| c.flattened_accent_base_height)
            .or_else(|| constants.as_ref().and_then(|c| c.accent_base_height))
            .unwrap_or(x_height)
        } else {
          constants
            .as_ref()
            .and_then(|c| c.accent_base_height)
            .unwrap_or(x_height)
        };
        let delta = (base_ascent - base_height).max(0.0);
        // Raise the accent baseline when the base is taller than the accent base height.
        let baseline_y = -delta;
        baseline_y - layout.baseline
      } else {
        base_top - over_gap - layout.height
      };

      if over_is_accent {
        // Ensure the accent sits outside the base box with a small clearance.
        let desired_bottom = base_top - accent_min_gap;
        let current_bottom = y + layout.height;
        if current_bottom > desired_bottom {
          y -= current_bottom - desired_bottom;
        }
      }

      (x, y)
    });

    let under_pos = under_layout.as_ref().map(|layout| {
      let x = (width - layout.width) * 0.5;
      let mut y = if under_is_accent {
        let use_flattened = constants
          .as_ref()
          .and_then(|c| c.flattened_accent_base_height)
          .is_some()
          && layout.width >= align_base_width * 1.1;
        let base_height = if use_flattened {
          constants
            .as_ref()
            .and_then(|c| c.flattened_accent_base_height)
            .or_else(|| constants.as_ref().and_then(|c| c.accent_base_height))
            .unwrap_or(x_height)
        } else {
          constants
            .as_ref()
            .and_then(|c| c.accent_base_height)
            .unwrap_or(x_height)
        };
        let delta = (base_descent - base_height).max(0.0);
        // Lower the accent baseline when the base has deep descenders.
        let baseline_y = delta;
        baseline_y - layout.baseline
      } else {
        base_bottom + under_gap
      };

      if under_is_accent {
        // Ensure the accent sits outside the base box with a small clearance.
        let desired_top = base_bottom + accent_min_gap;
        if y < desired_top {
          y += desired_top - y;
        }
      }

      (x, y)
    });

    let mut min_y = base_y;
    let mut max_y = base_y + base_layout.height;
    if let Some((_, y)) = over_pos {
      if let Some(layout) = &over_layout {
        min_y = min_y.min(y);
        max_y = max_y.max(y + layout.height);
      }
    }
    if let Some((_, y)) = under_pos {
      if let Some(layout) = &under_layout {
        min_y = min_y.min(y);
        max_y = max_y.max(y + layout.height);
      }
    }

    let baseline = -min_y;
    let height = max_y - min_y;

    let mut fragments = Vec::new();
    let mut annotations = base_layout.annotations.clone();

    // Base fragments.
    let base_offset = Point::new(base_x, base_y - min_y);
    for frag in base_layout.fragments {
      fragments.push(frag.translate(base_offset));
    }

    if let (Some(layout), Some((x, y))) = (over_layout, over_pos) {
      let offset = Point::new(x, y - min_y);
      for frag in layout.fragments {
        fragments.push(frag.translate(offset));
      }
      annotations = annotations.merge_trailing(&layout.annotations);
    }

    if let (Some(layout), Some((x, y))) = (under_layout, under_pos) {
      let offset = Point::new(x, y - min_y);
      for frag in layout.fragments {
        fragments.push(frag.translate(offset));
      }
      annotations = annotations.merge_trailing(&layout.annotations);
    }

    MathLayout {
      width,
      height,
      baseline,
      fragments,
      annotations,
    }
  }

  fn layout_sqrt(
    &mut self,
    body: &MathNode,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let content = self.layout_node(body, style, base_style);
    let constants =
      self.math_constants_for_layout(&content, style, base_style, MathVariant::Normal);
    let padding = Self::sqrt_padding(style);
    let rule = constants
      .as_ref()
      .and_then(|c| c.radical_rule_thickness)
      .unwrap_or_else(|| Self::rule_thickness(style));
    let gap = constants
      .as_ref()
      .map(|c| {
        if style.display_style {
          c.radical_display_style_vertical_gap
        } else {
          c.radical_vertical_gap
        }
      })
      .flatten()
      .unwrap_or_else(|| Self::sqrt_padding(style));
    let extra_ascender = constants
      .as_ref()
      .and_then(|c| c.radical_extra_ascender)
      .unwrap_or(0.0);
    let target_height = content.height + gap + rule + extra_ascender;
    let target_descent = (content.height - content.baseline).max(0.0);
    let target_ascent = target_height - target_descent;
    let radical_variant = self.resolve_variant(None, style, MathVariant::Normal);
    let mut radical = self
      .stretch_operator_vertical(
        "√",
        radical_variant,
        target_ascent,
        target_descent,
        style,
        base_style,
        true,
        false,
        0.0,
      )
      .unwrap_or_else(|| self.layout_glyphs("√", base_style, style, radical_variant));
    if (radical.height - target_height).abs() > style.font_size * 0.05 {
      radical = self.align_stretch(radical, target_ascent, target_descent, 0.0);
    }

    let offset_x = radical.width + padding;
    let content_y = gap + rule;
    let baseline = content.baseline + content_y;
    let mut fragments = Vec::new();
    // Radical glyph
    for frag in radical.fragments {
      fragments.push(frag.translate(Point::new(0.0, baseline - radical.baseline)));
    }

    // Content
    for frag in content.fragments {
      fragments.push(frag.translate(Point::new(offset_x, content_y)));
    }

    fragments.push(MathFragment::Rule(Rect::from_xywh(
      offset_x,
      content_y - rule,
      content.width,
      rule,
    )));

    let height = (content_y + content.height).max(radical.height + (baseline - radical.baseline));
    MathLayout {
      width: offset_x + content.width,
      height: height + padding,
      baseline,
      fragments,
      annotations: content.annotations,
    }
  }

  fn layout_root(
    &mut self,
    radicand: &MathNode,
    index: &MathNode,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let constants = self.default_math_constants(style, base_style, MathVariant::Normal);
    let index_style = style.script_with_constants(constants.as_ref());
    let index_layout = self.layout_node(index, &index_style, base_style);
    let sqrt_layout = self.layout_sqrt(radicand, style, base_style);
    let base_gap = constants
      .as_ref()
      .and_then(|c| c.radical_kern_before_degree)
      .unwrap_or_else(|| Self::script_gap(style));

    let offset_x = index_layout.width + base_gap;
    let mut fragments = Vec::new();

    let raise_percent = constants
      .as_ref()
      .and_then(|c| c.radical_degree_bottom_raise_percent)
      .unwrap_or(0.0)
      / 100.0;
    let raise = sqrt_layout.baseline * raise_percent;
    let index_y = (sqrt_layout.baseline - sqrt_layout.height * 0.6) - index_layout.baseline - raise;
    for frag in index_layout.fragments {
      fragments.push(frag.translate(Point::new(0.0, index_y)));
    }

    for frag in sqrt_layout.fragments {
      fragments.push(frag.translate(Point::new(offset_x, 0.0)));
    }

    MathLayout {
      width: offset_x + sqrt_layout.width,
      height: sqrt_layout.height.max(index_y + index_layout.height),
      baseline: sqrt_layout.baseline,
      fragments,
      annotations: sqrt_layout.annotations,
    }
  }

  fn layout_enclose(
    &mut self,
    notation: &[MencloseNotation],
    body: &MathNode,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let content = self.layout_node(body, style, base_style);
    let stroke = Self::rule_thickness(style);
    let padding = Self::sqrt_padding(style) + stroke * 0.5;
    let width = content.width + padding * 2.0 + stroke;
    let height = content.height + padding * 2.0 + stroke;
    let baseline = content.baseline + padding + stroke * 0.5;
    let content_offset = Point::new(padding + stroke * 0.5, padding + stroke * 0.5);
    let annotations = content.annotations.clone();

    let mut fragments: Vec<MathFragment> = content
      .fragments
      .into_iter()
      .map(|f| f.translate(content_offset))
      .collect();

    let outer_rect = Rect::from_xywh(0.0, 0.0, width.max(0.0), height.max(0.0));
    for note in notation {
      match note {
        MencloseNotation::Box => fragments.push(MathFragment::StrokeRect {
          rect: outer_rect,
          radius: 0.0,
          width: stroke,
        }),
        MencloseNotation::RoundedBox => fragments.push(MathFragment::StrokeRect {
          rect: outer_rect,
          radius: padding,
          width: stroke,
        }),
        MencloseNotation::Circle => {
          let radius = (outer_rect.width().min(outer_rect.height()) / 2.0).max(0.0);
          fragments.push(MathFragment::StrokeRect {
            rect: outer_rect,
            radius,
            width: stroke,
          });
        }
        MencloseNotation::Top => {
          fragments.push(MathFragment::Rule(Rect::from_xywh(0.0, 0.0, width, stroke)))
        }
        MencloseNotation::Bottom => fragments.push(MathFragment::Rule(Rect::from_xywh(
          0.0,
          height - stroke,
          width,
          stroke,
        ))),
        MencloseNotation::Left => fragments.push(MathFragment::Rule(Rect::from_xywh(
          0.0, 0.0, stroke, height,
        ))),
        MencloseNotation::Right => fragments.push(MathFragment::Rule(Rect::from_xywh(
          width - stroke,
          0.0,
          stroke,
          height,
        ))),
        MencloseNotation::HorizontalStrike => fragments.push(MathFragment::Rule(Rect::from_xywh(
          0.0,
          height / 2.0 - stroke * 0.5,
          width,
          stroke,
        ))),
        MencloseNotation::VerticalStrike => fragments.push(MathFragment::Rule(Rect::from_xywh(
          width / 2.0 - stroke * 0.5,
          0.0,
          stroke,
          height,
        ))),
        MencloseNotation::UpDiagonalStrike => fragments.push(MathFragment::Line {
          from: Point::new(0.0, height),
          to: Point::new(width, 0.0),
          width: stroke,
        }),
        MencloseNotation::DownDiagonalStrike => fragments.push(MathFragment::Line {
          from: Point::new(0.0, 0.0),
          to: Point::new(width, height),
          width: stroke,
        }),
        MencloseNotation::LongDiv => {
          fragments.push(MathFragment::Rule(Rect::from_xywh(0.0, 0.0, width, stroke)));
          fragments.push(MathFragment::Rule(Rect::from_xywh(
            0.0, 0.0, stroke, height,
          )));
        }
      }
    }

    MathLayout {
      width,
      height,
      baseline,
      fragments,
      annotations,
    }
  }

  fn layout_table(
    &mut self,
    table: &MathTable,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    if table.rows.is_empty() {
      return self.layout_glyphs("", base_style, style, MathVariant::Normal);
    }
    let metrics = self.base_font_metrics(base_style, style.font_size);
    let mut cell_layouts: Vec<Vec<MathLayout>> = Vec::new();
    let mut col_widths: Vec<f32> = Vec::new();
    let mut row_baselines: Vec<f32> = Vec::new();
    let mut row_heights: Vec<f32> = Vec::new();

    for row in &table.rows {
      let mut layouts = Vec::new();
      let mut baseline: f32 = 0.0;
      let mut max_descent: f32 = 0.0;
      let mut max_height: f32 = 0.0;
      for (col, cell) in row.cells.iter().enumerate() {
        let layout = self.layout_node(&cell.content, style, base_style);
        if col >= col_widths.len() {
          col_widths.push(layout.width);
        } else {
          col_widths[col] = col_widths[col].max(layout.width);
        }
        baseline = baseline.max(layout.baseline);
        max_descent = max_descent.max(layout.height - layout.baseline);
        max_height = max_height.max(layout.height);
        layouts.push(layout);
      }
      if row.cells.is_empty() {
        baseline = metrics.ascent;
        max_height = metrics.line_height;
      } else {
        max_height = max_height.max(baseline + max_descent);
      }
      row_baselines.push(baseline);
      row_heights.push(max_height);
      cell_layouts.push(layouts);
    }

    let col_gap_count = col_widths.len().saturating_sub(1);
    let row_gap_count = table.rows.len().saturating_sub(1);
    let (heuristic_col_spacing, heuristic_row_spacing) = Self::table_spacing(style);
    let col_gaps: Vec<f32> = if let Some(values) = table.column_spacings.as_deref() {
      (0..col_gap_count)
        .map(|idx| {
          let value = repeating_value(values, idx).unwrap_or(DEFAULT_TABLE_COLUMN_SPACING);
          self.resolve_math_space(value, style, &metrics)
        })
        .collect()
    } else {
      vec![heuristic_col_spacing; col_gap_count]
    };
    let row_gaps: Vec<f32> = if let Some(values) = table.row_spacings.as_deref() {
      (0..row_gap_count)
        .map(|idx| {
          let value = repeating_value(values, idx).unwrap_or(DEFAULT_TABLE_ROW_SPACING);
          self.resolve_math_space(value, style, &metrics)
        })
        .collect()
    } else {
      vec![heuristic_row_spacing; row_gap_count]
    };

    let width: f32 =
      col_widths.iter().copied().sum::<f32>() + col_gaps.iter().copied().sum::<f32>();
    let mut y = 0.0;
    let mut cell_fragments = Vec::new();
    let mut table_baseline = 0.0;
    let mut trailing_annotations = MathLayoutAnnotations::default();
    for (row_idx, (row, layouts)) in table.rows.iter().zip(cell_layouts.into_iter()).enumerate() {
      let row_height = row_heights[row_idx];
      let row_baseline = row_baselines[row_idx];
      let row_align_pref = repeating_value(&table.row_aligns, row_idx).or(row.row_align);
      if row_idx == 0 {
        table_baseline = row_baseline + y;
      }
      let mut x = 0.0;
      for (col_idx, (cell, layout)) in row.cells.iter().zip(layouts.into_iter()).enumerate() {
        let MathLayout {
          width: cell_width,
          height: cell_height,
          baseline: cell_baseline,
          fragments: cell_frags,
          annotations: cell_annotations,
        } = layout;
        let col_align_default =
          repeating_value(&table.column_aligns, col_idx).unwrap_or(ColumnAlign::Center);
        let col_align = cell
          .column_align
          .or_else(|| repeating_value(&row.column_aligns, col_idx))
          .unwrap_or(col_align_default);
        let cell_row_align = cell
          .row_align
          .or(row_align_pref)
          .unwrap_or(RowAlign::Baseline);
        let baseline_target = match cell_row_align {
          RowAlign::Axis => Self::axis_height(&metrics, style, None) + style.font_size * 0.5,
          _ => row_baseline,
        };
        let offset_y = match cell_row_align {
          RowAlign::Baseline | RowAlign::Axis => y + (baseline_target - cell_baseline),
          RowAlign::Top => y,
          RowAlign::Bottom => y + (row_height - cell_height),
          RowAlign::Center => y + (row_height - cell_height) / 2.0,
        };
        let width_available = col_widths.get(col_idx).copied().unwrap_or(cell_width);
        let offset_x = x
          + match col_align {
            ColumnAlign::Left => 0.0,
            ColumnAlign::Center => (width_available - cell_width) / 2.0,
            ColumnAlign::Right => (width_available - cell_width).max(0.0),
          };
        for frag in cell_frags {
          cell_fragments.push(frag.translate(Point::new(offset_x, offset_y)));
        }
        x += width_available;
        if col_idx + 1 < row.cells.len() {
          x += col_gaps.get(col_idx).copied().unwrap_or(0.0);
        }
        trailing_annotations = trailing_annotations.merge_trailing(&cell_annotations);
      }
      y += row_height;
      if row_idx + 1 < table.rows.len() {
        y += row_gaps.get(row_idx).copied().unwrap_or(0.0);
      }
    }

    let height = y;

    let mut line_fragments: Vec<MathFragment> = Vec::new();
    let stroke = Self::rule_thickness(style);

    if let Some(lines) = table.column_lines.as_deref() {
      let mut x_cursor = 0.0;
      for (gap_idx, col_width) in col_widths.iter().copied().enumerate() {
        x_cursor += col_width;
        if gap_idx < col_gaps.len() {
          let line_style = repeating_value(lines, gap_idx).unwrap_or(TableLineStyle::None);
          if !matches!(line_style, TableLineStyle::None) {
            let gap = col_gaps[gap_idx];
            let x = x_cursor + gap * 0.5 - stroke * 0.5;
            line_fragments.push(MathFragment::Rule(Rect::from_xywh(
              x,
              0.0,
              stroke,
              height,
            )));
          }
          x_cursor += col_gaps[gap_idx];
        }
      }
    }

    if let Some(lines) = table.row_lines.as_deref() {
      let mut y_cursor = 0.0;
      for (gap_idx, row_height) in row_heights.iter().copied().enumerate() {
        y_cursor += row_height;
        if gap_idx < row_gaps.len() {
          let line_style = repeating_value(lines, gap_idx).unwrap_or(TableLineStyle::None);
          if !matches!(line_style, TableLineStyle::None) {
            let gap = row_gaps[gap_idx];
            let y = y_cursor + gap * 0.5 - stroke * 0.5;
            line_fragments.push(MathFragment::Rule(Rect::from_xywh(
              0.0,
              y,
              width,
              stroke,
            )));
          }
          y_cursor += row_gaps[gap_idx];
        }
      }
    }

    // Ensure rules paint behind cell content.
    let mut fragments = line_fragments;
    fragments.extend(cell_fragments);

    let mut out_width = width;
    let mut out_height = height;
    let mut out_baseline = table_baseline;

    if let Some(frame) = table.frame {
      if !matches!(frame, TableLineStyle::None) {
        let (pad_x, pad_y) = table
          .frame_spacing
          .unwrap_or((DEFAULT_TABLE_FRAME_SPACING_X, DEFAULT_TABLE_FRAME_SPACING_Y));
        let pad_x = self.resolve_length(pad_x, style, &metrics);
        let pad_y = self.resolve_length(pad_y, style, &metrics);
        let offset = Point::new(pad_x + stroke * 0.5, pad_y + stroke * 0.5);
        fragments = fragments
          .into_iter()
          .map(|frag| frag.translate(offset))
          .collect();
        out_width = out_width + pad_x * 2.0 + stroke;
        out_height = out_height + pad_y * 2.0 + stroke;
        out_baseline = out_baseline + offset.y;

        let outer_rect = Rect::from_xywh(0.0, 0.0, out_width.max(0.0), out_height.max(0.0));
        fragments.push(MathFragment::StrokeRect {
          rect: outer_rect,
          radius: 0.0,
          width: stroke,
        });
      }
    }

    MathLayout {
      width: out_width,
      height: out_height,
      baseline: out_baseline,
      fragments,
      annotations: trailing_annotations,
    }
  }

  fn layout_multiscripts(
    &mut self,
    base: &MathNode,
    pre: &[(Option<MathNode>, Option<MathNode>)],
    post: &[(Option<MathNode>, Option<MathNode>)],
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    let base_layout = self.layout_node(base, style, base_style);
    let constants =
      self.math_constants_for_layout(&base_layout, style, base_style, MathVariant::Normal);
    let script_style = style.script_with_constants(constants.as_ref());
    let mut fragments = Vec::new();
    let mut ascent = base_layout.baseline;
    let mut descent = base_layout.height - base_layout.baseline;

    let script_gap = constants
      .as_ref()
      .and_then(|c| c.space_after_script)
      .unwrap_or_else(|| Self::script_gap(style));
    let base_metrics = self.base_font_metrics(base_style, style.font_size);
    let x_height = base_metrics.x_height.unwrap_or(style.font_size * 0.5);
    let sup_fallback = || {
      (base_metrics.ascent * 0.6)
        .max(x_height * 0.65)
        .max(style.font_size * if style.display_style { 0.4 } else { 0.34 })
    };
    let sup_shift_up = constants
      .as_ref()
      .and_then(|c| c.superscript_shift_up)
      .unwrap_or_else(sup_fallback);
    let sup_shift_up_cramped = constants
      .as_ref()
      .and_then(|c| c.superscript_shift_up_cramped)
      .unwrap_or_else(sup_fallback);
    let sub_shift = constants
      .as_ref()
      .and_then(|c| c.subscript_shift_down)
      .unwrap_or_else(|| (base_metrics.descent * 0.8 + x_height * 0.2).max(style.font_size * 0.24));
    let min_gap = constants
      .as_ref()
      .and_then(|c| c.sub_superscript_gap_min)
      .unwrap_or_else(|| {
        (Self::script_gap(style) + Self::rule_thickness(style)).max(style.font_size * 0.06)
      });
    let sup_bottom_min = constants.as_ref().and_then(|c| c.superscript_bottom_min);
    let sup_bottom_max_with_sub = constants
      .as_ref()
      .and_then(|c| c.superscript_bottom_max_with_subscript);
    let sub_top_max = constants.as_ref().and_then(|c| c.subscript_top_max);
    let sub_baseline_drop_min = constants
      .as_ref()
      .and_then(|c| c.subscript_baseline_drop_min);
    let base_descent = base_layout.height - base_layout.baseline;
    let italic_correction = base_layout
      .annotations
      .trailing_glyph
      .as_ref()
      .map(|g| g.italic_correction)
      .unwrap_or(0.0);

    let build_block = |scripts: &[(Option<MathNode>, Option<MathNode>)],
                       side: MathKernSide,
                       apply_italic: bool,
                       ctx: &mut Self|
     -> (f32, f32, f32, Vec<MathFragment>, MathLayoutAnnotations) {
      let mut block_width: f32 = 0.0;
      let mut block_ascent: f32 = 0.0;
      let mut block_descent: f32 = 0.0;
      let mut frags = Vec::new();
      let mut annotations = MathLayoutAnnotations::default();
      let mut first_pair = true;
      for pair in scripts {
        let sup_layout = pair
          .1
          .as_ref()
          .map(|n| ctx.layout_node(n, &script_style, base_style));
        let sub_layout = pair
          .0
          .as_ref()
          .map(|n| ctx.layout_node(n, &script_style, base_style));
        if sup_layout.is_none() && sub_layout.is_none() {
          continue;
        }
        if !first_pair {
          block_width += script_gap;
        }
        first_pair = false;
        let x_start = block_width;
        let mut pair_end = block_width;
        let mut pair_ascent: f32 = 0.0;
        let mut pair_descent: f32 = 0.0;
        let mut sup_pos: Option<(f32, f32)> = None;
        if let Some(layout) = &sup_layout {
          let has_sub = sub_layout.is_some();
          let mut y = base_layout.baseline
            - if has_sub {
              sup_shift_up_cramped
            } else {
              sup_shift_up
            }
            - layout.baseline;
          if let Some(bottom_min) = sup_bottom_min {
            let sup_bottom = y + layout.height - layout.baseline;
            let allowed = base_layout.baseline - bottom_min;
            if sup_bottom > allowed {
              y -= sup_bottom - allowed;
            }
          }
          if let (Some(limit), true) = (sup_bottom_max_with_sub, has_sub) {
            let sup_bottom = y + layout.height - layout.baseline;
            let allowed = base_layout.baseline - limit;
            if sup_bottom > allowed {
              y -= sup_bottom - allowed;
            }
          }
          let sup_kern = base_layout
            .annotations
            .trailing_glyph
            .as_ref()
            .map(|g| {
              ctx.font_ctx.math_kern(
                &g.font,
                g.glyph_id,
                layout.baseline,
                g.font_size,
                true,
                side,
              )
            })
            .unwrap_or(0.0);
          let sup_x = x_start + if apply_italic { italic_correction } else { 0.0 } + sup_kern;
          for frag in &layout.fragments {
            frags.push(frag.clone().translate(Point::new(sup_x, y)));
          }
          pair_end = pair_end.max(sup_x + layout.width);
          pair_ascent = pair_ascent.max(layout.baseline - y);
          pair_descent = pair_descent.max(layout.height - (layout.baseline - y));
          sup_pos = Some((y, layout.height));
          annotations = annotations.merge_trailing(&layout.annotations);
        }
        if let Some(layout) = &sub_layout {
          let mut y = base_layout.baseline + base_descent + sub_shift - layout.baseline;
          if let Some((sup_y, sup_h)) = sup_pos {
            let sup_bottom = sup_y + sup_h;
            let gap = y - sup_bottom;
            if gap < min_gap {
              y += min_gap - gap;
            }
          }
          if let Some(top_max) = sub_top_max {
            let sub_top = y + layout.baseline;
            let min_top = base_layout.baseline + top_max;
            if sub_top < min_top {
              y += min_top - sub_top;
            }
          }
          if let Some(min_drop) = sub_baseline_drop_min {
            let drop = y + layout.baseline - base_layout.baseline;
            if drop < min_drop {
              y += min_drop - drop;
            }
          }
          let sub_kern = base_layout
            .annotations
            .trailing_glyph
            .as_ref()
            .map(|g| {
              ctx.font_ctx.math_kern(
                &g.font,
                g.glyph_id,
                layout.height - layout.baseline,
                g.font_size,
                false,
                side,
              )
            })
            .unwrap_or(0.0);
          let sub_x = x_start + sub_kern;
          for frag in &layout.fragments {
            frags.push(frag.clone().translate(Point::new(sub_x, y)));
          }
          pair_end = pair_end.max(sub_x + layout.width);
          pair_ascent = pair_ascent.max(layout.baseline - y);
          pair_descent = pair_descent.max(layout.height - (layout.baseline - y));
          annotations = annotations.merge_trailing(&layout.annotations);
        }
        block_ascent = block_ascent.max(pair_ascent);
        block_descent = block_descent.max(pair_descent);
        block_width = pair_end;
      }
      if !first_pair {
        block_width += script_gap;
      }
      (block_width, block_ascent, block_descent, frags, annotations)
    };

    let (pre_width, pre_ascent, pre_descent, pre_frags, pre_annot) =
      build_block(pre, MathKernSide::Left, false, self);
    let (post_width, post_ascent, post_descent, post_frags, post_annot) =
      build_block(post, MathKernSide::Right, true, self);
    let width_left = pre_width;
    let width_right = post_width;
    ascent = ascent.max(pre_ascent).max(post_ascent);
    descent = descent.max(pre_descent).max(post_descent);

    // Position fragments
    for frag in pre_frags {
      fragments.push(frag);
    }
    for frag in base_layout.fragments {
      fragments.push(frag.translate(Point::new(width_left, 0.0)));
    }
    for frag in post_frags {
      fragments.push(frag.translate(Point::new(width_left + base_layout.width, 0.0)));
    }

    MathLayout {
      width: width_left + base_layout.width + width_right,
      height: ascent + descent,
      baseline: ascent,
      fragments,
      annotations: post_annot
        .merge_trailing(&base_layout.annotations)
        .merge_trailing(&pre_annot),
    }
  }

  fn layout_node(
    &mut self,
    node: &MathNode,
    style: &MathStyle,
    base_style: &ComputedStyle,
  ) -> MathLayout {
    match node {
      MathNode::Math {
        display_style,
        children,
      } => {
        let mut style = *style;
        style.display_style = *display_style;
        self.layout_row(children, &style, base_style)
      }
      MathNode::Row(children) => self.layout_row(children, style, base_style),
      MathNode::Identifier { text, variant } => {
        let non_ws_count = text
          .chars()
          .filter(|c| !is_ascii_whitespace_mathml(*c))
          .take(2)
          .count();
        let fallback = if non_ws_count <= 1 {
          MathVariant::Italic
        } else {
          MathVariant::Normal
        };
        let resolved = self.resolve_variant(*variant, style, fallback);
        self.layout_glyphs(text, base_style, style, resolved)
      }
      MathNode::Number { text, variant } => {
        let resolved = self.resolve_variant(*variant, style, MathVariant::Normal);
        self.layout_glyphs(text, base_style, style, resolved)
      }
      MathNode::Operator {
        text,
        form,
        stretchy,
        variant,
        ..
      } => {
        let resolved_variant = self.resolve_variant(*variant, style, MathVariant::Normal);
        // Stretching to surrounding content is handled during row aggregation, but display style
        // large operators (e.g. ∑, ∫) also need to select vertical variants based on
        // `display_operator_min_height` even when they're not stretchy.
        let mut layout = self.layout_glyphs(text, base_style, style, resolved_variant);
        if style.display_style {
          let default =
            Self::operator_default_properties(text, (*form).unwrap_or(OperatorForm::Infix));
          if default.large_op && !(*stretchy).unwrap_or(default.stretchy) {
            let target_ascent = layout.baseline;
            let target_descent = layout.height - layout.baseline;
            if let Some(stretched) = self.stretch_operator_vertical(
              text,
              resolved_variant,
              target_ascent,
              target_descent,
              style,
              base_style,
              false,
              true,
              0.0,
            ) {
              layout = stretched;
            }
          }
        }
        layout
      }
      MathNode::Text { text, variant } => {
        let resolved = self.resolve_variant(*variant, style, MathVariant::Normal);
        self.layout_glyphs(text, base_style, style, resolved)
      }
      MathNode::Space {
        width,
        height,
        depth,
      } => self.layout_space(*width, *height, *depth, style, base_style),
      MathNode::Fraction {
        numerator,
        denominator,
        linethickness,
        bevelled,
        numalign,
        denomalign,
      } => self.layout_fraction(
        numerator,
        denominator,
        *linethickness,
        *bevelled,
        *numalign,
        *denomalign,
        style,
        base_style,
      ),
      MathNode::Sqrt(body) => self.layout_sqrt(body, style, base_style),
      MathNode::Root { radicand, index } => self.layout_root(radicand, index, style, base_style),
      MathNode::Superscript { base, superscript } => {
        self.layout_superscript(base, Some(superscript.as_ref()), None, style, base_style)
      }
      MathNode::Subscript { base, subscript } => {
        self.layout_superscript(base, None, Some(subscript.as_ref()), style, base_style)
      }
      MathNode::SubSuperscript {
        base,
        subscript,
        superscript,
      } => self.layout_superscript(
        base,
        Some(superscript.as_ref()),
        Some(subscript.as_ref()),
        style,
        base_style,
      ),
      MathNode::Over {
        base,
        over,
        accent,
      } => {
        self.layout_under_over(base, None, Some((over.as_ref(), *accent)), style, base_style)
      }
      MathNode::Under {
        base,
        under,
        accentunder,
      } => {
        self.layout_under_over(base, Some((under.as_ref(), *accentunder)), None, style, base_style)
      }
      MathNode::UnderOver {
        base,
        under,
        over,
        accent,
        accentunder,
      } => self.layout_under_over(
        base,
        Some((under.as_ref(), *accentunder)),
        Some((over.as_ref(), *accent)),
        style,
        base_style,
      ),
      MathNode::Style {
        overrides,
        children,
      } => {
        let next_style = self.apply_style_overrides(style, overrides, base_style);
        self.layout_row(children, &next_style, base_style)
      }
      MathNode::Enclose { notation, child } => {
        self.layout_enclose(notation, child, style, base_style)
      }
      MathNode::Table(table) => self.layout_table(table, style, base_style),
      MathNode::Multiscripts {
        base,
        prescripts,
        postscripts,
      } => self.layout_multiscripts(base, prescripts, postscripts, style, base_style),
    }
  }

  /// Public entrypoint: layout a MathNode tree using the provided style.
  pub fn layout(&mut self, node: &MathNode, style: &ComputedStyle) -> MathLayout {
    let math_style = MathStyle::from_computed(style);
    self.layout_node(node, &math_style, style)
  }
}

/// Layout MathML using the provided style and font context.
pub fn layout_mathml(node: &MathNode, style: &ComputedStyle, font_ctx: &FontContext) -> MathLayout {
  let mut ctx = MathLayoutContext::new(font_ctx.clone());
  ctx.layout(node, style)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use std::path::PathBuf;

  fn find_math_element<'a>(node: &'a crate::dom::DomNode) -> Option<&'a crate::dom::DomNode> {
    if node
      .tag_name()
      .map(|t| t.eq_ignore_ascii_case("math"))
      .unwrap_or(false)
    {
      return Some(node);
    }
    node.children.iter().find_map(find_math_element)
  }

  fn parse_math_from_html(markup: &str) -> MathNode {
    let dom = crate::dom::parse_html(markup).expect("dom");
    let math_node = find_math_element(&dom).expect("math element");
    parse_mathml(math_node).expect("math parsed")
  }

  fn bundled_math_font_context() -> FontContext {
    FontContext::with_config(
      FontConfig::new()
        .with_system_fonts(false)
        .with_bundled_fonts(true),
    )
  }

  fn first_glyph_run(layout: &MathLayout) -> &ShapedRun {
    layout
      .fragments
      .iter()
      .find_map(|frag| match frag {
        MathFragment::Glyph { run, .. } => Some(run),
        _ => None,
      })
      .expect("expected at least one glyph fragment")
  }

  fn concat_glyph_text(layout: &MathLayout) -> String {
    let mut out = String::new();
    for frag in &layout.fragments {
      if let MathFragment::Glyph { run, .. } = frag {
        out.push_str(&run.text);
      }
    }
    out
  }

  #[test]
  fn mspace_zero_height_produces_zero_height_layout() {
    let style = ComputedStyle::default();
    let node = MathNode::Space {
      width: MathLength::Em(1.0),
      height: MathLength::Em(0.0),
      depth: MathLength::Em(0.0),
    };
    let layout = layout_mathml(&node, &style, &FontContext::empty());
    assert_eq!(layout.height, 0.0);
    assert_eq!(layout.baseline, 0.0);
  }

  #[test]
  fn mspace_width_only_does_not_affect_row_vertical_metrics() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(crate::text::font_db::FontConfig::bundled_only());

    let with_space =
      parse_math_from_html("<math><mrow><mi>x</mi><mspace width=\"2em\"/><mi>y</mi></mrow></math>");
    let without_space = parse_math_from_html("<math><mrow><mi>x</mi><mi>y</mi></mrow></math>");

    let with_layout = layout_mathml(&with_space, &style, &ctx);
    let without_layout = layout_mathml(&without_space, &style, &ctx);

    let eps = 0.001;
    assert!(
      (with_layout.height - without_layout.height).abs() < eps,
      "mspace must not affect row height: {} vs {}",
      with_layout.height,
      without_layout.height
    );
    assert!(
      (with_layout.baseline - without_layout.baseline).abs() < eps,
      "mspace must not affect row baseline: {} vs {}",
      with_layout.baseline,
      without_layout.baseline
    );
  }

  #[test]
  fn mspace_clamps_negative_and_nan_lengths_to_zero() {
    let style = ComputedStyle::default();

    let nan_node = MathNode::Space {
      width: MathLength::Em(f32::NAN),
      height: MathLength::Em(f32::NAN),
      depth: MathLength::Em(f32::NAN),
    };
    let layout = layout_mathml(&nan_node, &style, &FontContext::empty());
    assert_eq!(layout.width, 0.0);
    assert_eq!(layout.height, 0.0);
    assert_eq!(layout.baseline, 0.0);

    let negative_node = MathNode::Space {
      width: MathLength::Em(-1.0),
      height: MathLength::Em(-2.0),
      depth: MathLength::Em(-3.0),
    };
    let layout = layout_mathml(&negative_node, &style, &FontContext::empty());
    assert_eq!(layout.width, 0.0);
    assert_eq!(layout.height, 0.0);
    assert_eq!(layout.baseline, 0.0);

    // If only one of height/depth is positive, the negative length must clamp to 0 without
    // affecting the other dimension.
    let mixed_node = MathNode::Space {
      width: MathLength::Em(-1.0),
      height: MathLength::Em(-1.0),
      depth: MathLength::Em(1.0),
    };
    let layout = layout_mathml(&mixed_node, &style, &FontContext::empty());
    assert_eq!(layout.width, 0.0);
    assert_eq!(layout.baseline, 0.0);
    assert!((layout.height - style.font_size).abs() < 0.001);
  }

  fn math_font_context() -> FontContext {
    let mut cfg = FontConfig::bundled_only();
    cfg.font_dirs.push(
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts"),
    );
    FontContext::with_config(cfg)
  }

  fn find_run<'a>(layout: &'a MathLayout, needle: &str) -> &'a ShapedRun {
    layout
      .fragments
      .iter()
      .filter_map(|frag| match frag {
        MathFragment::Glyph { run, .. } => Some(run),
        _ => None,
      })
      .find(|run| run.text.contains(needle))
      .unwrap_or_else(|| panic!("missing run containing {}", needle))
  }

  #[test]
  fn scripted_runs_enable_ssty_feature() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><msup><mi>x</mi><mi>i</mi></msup></math>");
    let layout = layout_mathml(&node, &style, &ctx);

    let mut base_run: Option<&ShapedRun> = None;
    let mut script_run: Option<&ShapedRun> = None;
    for frag in &layout.fragments {
      if let MathFragment::Glyph { run, .. } = frag {
        if base_run.is_none() && run.text == "x" {
          base_run = Some(run);
        } else if script_run.is_none() && run.text == "i" {
          script_run = Some(run);
        }
      }
    }
    let base_run = base_run.expect("base run");
    let script_run = script_run.expect("script run");

    let script_ssty = script_run
      .features
      .iter()
      .find(|f| f.tag.to_bytes() == *b"ssty")
      .map(|f| f.value);
    assert!(
      script_ssty.unwrap_or(0) >= 1,
      "expected superscript to enable ssty feature, got {:?}",
      script_ssty
    );

    assert!(
      base_run.features.iter().all(|f| f.tag.to_bytes() != *b"ssty"),
      "base run should not set ssty"
    );
  }

  #[test]
  fn mathvariant_double_struck_maps_to_unicode_math_alphanumerics() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><mi mathvariant=\"double-struck\">AB</mi></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let shaped_text = concat_glyph_text(&layout);
    assert!(
      shaped_text.contains('\u{1D538}') && shaped_text.contains('\u{1D539}'),
      "expected double-struck A/B (U+1D538/U+1D539) in shaped text, got {shaped_text:?}"
    );
  }

  #[test]
  fn mathvariant_script_maps_to_unicode_math_alphanumerics() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><mi mathvariant=\"script\">Az</mi></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let shaped_text = concat_glyph_text(&layout);
    assert!(
      shaped_text.contains('\u{1D49C}') && shaped_text.contains('\u{1D4CF}'),
      "expected script A/z (U+1D49C/U+1D4CF) in shaped text, got {shaped_text:?}"
    );
  }

  #[test]
  fn mathvariant_fraktur_maps_to_unicode_math_alphanumerics() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><mi mathvariant=\"fraktur\">Az</mi></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let shaped_text = concat_glyph_text(&layout);
    assert!(
      shaped_text.contains('\u{1D504}') && shaped_text.contains('\u{1D537}'),
      "expected fraktur A/z (U+1D504/U+1D537) in shaped text, got {shaped_text:?}"
    );
  }

  #[test]
  fn mathvariant_sans_serif_bold_maps_greek_to_unicode_math_alphanumerics() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html(
      "<math><mi mathvariant=\"sans-serif-bold\">&#x391;&#x3B2;</mi></math>",
    );
    let layout = layout_mathml(&node, &style, &ctx);
    let shaped_text = concat_glyph_text(&layout);
    assert!(
      shaped_text.contains('\u{1D756}') && shaped_text.contains('\u{1D771}'),
      "expected sans-serif bold Greek Α/β (U+1D756/U+1D771) in shaped text, got {shaped_text:?}"
    );
  }

  #[test]
  fn mathvariant_sans_serif_bold_italic_maps_greek_to_unicode_math_alphanumerics() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html(
      "<math><mi mathvariant=\"sans-serif-bold-italic\">&#x391;&#x3B2;</mi></math>",
    );
    let layout = layout_mathml(&node, &style, &ctx);
    let shaped_text = concat_glyph_text(&layout);
    assert!(
      shaped_text.contains('\u{1D790}') && shaped_text.contains('\u{1D7AB}'),
      "expected sans-serif bold italic Greek Α/β (U+1D790/U+1D7AB) in shaped text, got {shaped_text:?}"
    );
  }

  #[test]
  fn mathvariant_mapping_shapes_to_nonempty_runs() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html(
      "<math><mrow><mi mathvariant=\"double-struck\">A</mi><mi mathvariant=\"monospace\">B</mi></mrow></math>",
    );
    let layout = layout_mathml(&node, &style, &ctx);
    assert!(layout.width > 0.0);
    assert!(layout.height > 0.0);
    let total_glyphs: usize = layout
      .fragments
      .iter()
      .filter_map(|frag| match frag {
        MathFragment::Glyph { run, .. } => Some(run.glyphs.len()),
        _ => None,
      })
      .sum();
    assert!(
      total_glyphs > 0,
      "expected shaped glyphs (non-empty runs) for mapped variants"
    );
  }

  fn first_row_operator_height(
    node: &MathNode,
    style: &ComputedStyle,
    font_ctx: &FontContext,
    operator_text: &str,
  ) -> f32 {
    let MathNode::Math {
      display_style,
      children,
    } = node
    else {
      panic!("expected math root");
    };
    let row_children = match children.get(0) {
      Some(MathNode::Row(children)) => children.as_slice(),
      Some(other) => std::slice::from_ref(other),
      None => panic!("expected math content"),
    };

    let mut ctx = MathLayoutContext::new(font_ctx.clone());
    let mut math_style = MathStyle::from_computed(style);
    math_style.display_style = *display_style;

    let (layouts, _) = ctx.layout_row_children_and_properties(row_children, &math_style, style);
    for (node, layout) in row_children.iter().zip(layouts.iter()) {
      if let Some(op) = MathLayoutContext::operator_like(node) {
        if op.text == operator_text {
          return layout.height;
        }
      }
    }
    panic!("operator {operator_text:?} not found in row");
  }

  #[test]
  fn table_layout_completes() {
    let style = ComputedStyle::default();
    let node = MathNode::Table(MathTable {
      rows: vec![
        MathTableRow {
          cells: vec![MathTableCell {
            content: MathNode::Identifier {
              text: "a".into(),
              variant: None,
            },
            row_align: None,
            column_align: None,
          }],
          row_align: None,
          column_aligns: Vec::new(),
        },
        MathTableRow {
          cells: vec![MathTableCell {
            content: MathNode::Identifier {
              text: "b".into(),
              variant: None,
            },
            row_align: None,
            column_align: None,
          }],
          row_align: None,
          column_aligns: Vec::new(),
        },
      ],
      column_aligns: Vec::new(),
      row_aligns: Vec::new(),
      column_spacings: None,
      row_spacings: None,
      column_lines: None,
      row_lines: None,
      frame: None,
      frame_spacing: None,
    });
    let layout = layout_mathml(&node, &style, &FontContext::empty());
    assert!(layout.width > 0.0);
    assert!(layout.height > 0.0);
    assert!(layout.baseline > 0.0);
  }

  #[test]
  fn table_layout_with_font_db() {
    let style = ComputedStyle::default();
    let node = MathNode::Table(MathTable {
      rows: vec![
        MathTableRow {
          cells: vec![
            MathTableCell {
              content: MathNode::Number {
                text: "1".into(),
                variant: None,
              },
              row_align: None,
              column_align: None,
            },
            MathTableCell {
              content: MathNode::Number {
                text: "2".into(),
                variant: None,
              },
              row_align: None,
              column_align: None,
            },
          ],
          row_align: None,
          column_aligns: Vec::new(),
        },
        MathTableRow {
          cells: vec![
            MathTableCell {
              content: MathNode::Number {
                text: "3".into(),
                variant: None,
              },
              row_align: None,
              column_align: None,
            },
            MathTableCell {
              content: MathNode::Number {
                text: "4".into(),
                variant: None,
              },
              row_align: None,
              column_align: None,
            },
          ],
          row_align: None,
          column_aligns: Vec::new(),
        },
      ],
      column_aligns: Vec::new(),
      row_aligns: Vec::new(),
      column_spacings: None,
      row_spacings: None,
      column_lines: None,
      row_lines: None,
      frame: None,
      frame_spacing: None,
    });
    let ctx = FontContext::new();
    let layout = layout_mathml(&node, &style, &ctx);
    assert!(layout.width > 0.0);
    assert!(layout.height > 0.0);
  }

  #[test]
  fn display_fraction_children_shrink_nested_fractions() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let parsed = parse_math_from_html(
      "<math display=\"block\"><mfrac><mi>a</mi><mfrac><mi>b</mi><mi>c</mi></mfrac></mfrac></math>",
    );
    let mut style = ComputedStyle::default();
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let layout = layout_mathml(&parsed, &style, &ctx);

    let mut sizes = std::collections::HashMap::<String, Vec<f32>>::new();
    for fragment in &layout.fragments {
      if let MathFragment::Glyph { run, .. } = fragment {
        sizes.entry(run.text.clone()).or_default().push(run.font_size);
      }
    }

    let a_size = sizes
      .get("a")
      .and_then(|v| v.iter().copied().reduce(f32::max))
      .expect("glyph run for a");
    let b_size = sizes
      .get("b")
      .and_then(|v| v.iter().copied().reduce(f32::max))
      .expect("glyph run for b");
    let c_size = sizes
      .get("c")
      .and_then(|v| v.iter().copied().reduce(f32::max))
      .expect("glyph run for c");

    assert!(
      b_size < a_size && b_size <= a_size * 0.8,
      "expected b to be script-sized relative to a (a_size={a_size}, b_size={b_size})"
    );
    assert!(
      c_size < a_size && c_size <= a_size * 0.8,
      "expected c to be script-sized relative to a (a_size={a_size}, c_size={c_size})"
    );
  }

  #[test]
  fn table_spacing_attributes_shrink_layout() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let default_node = parse_math_from_html(
      "<math><mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr></mtable></math>",
    );
    let zero_node = parse_math_from_html(
      "<math><mtable columnspacing=\"0\" rowspacing=\"0\"><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr></mtable></math>",
    );
    let default_layout = layout_mathml(&default_node, &style, &ctx);
    let zero_layout = layout_mathml(&zero_node, &style, &ctx);
    assert!(
      default_layout.width > zero_layout.width + 0.1,
      "expected columnspacing=0 to reduce table width ({} vs {})",
      default_layout.width,
      zero_layout.width
    );
    assert!(
      default_layout.height > zero_layout.height + 0.1,
      "expected rowspacing=0 to reduce table height ({} vs {})",
      default_layout.height,
      zero_layout.height
    );
  }

  #[test]
  fn table_spacing_lists_repeat_last_value() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let short = parse_math_from_html(
      "<math><mtable columnspacing=\"0.2em 0.4em\" rowspacing=\"0.1em\"><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr><mtr><mtd><mi>e</mi></mtd><mtd><mi>f</mi></mtd><mtd><mi>g</mi></mtd><mtd><mi>h</mi></mtd></mtr><mtr><mtd><mi>i</mi></mtd><mtd><mi>j</mi></mtd><mtd><mi>k</mi></mtd><mtd><mi>l</mi></mtd></mtr></mtable></math>",
    );
    let explicit = parse_math_from_html(
      "<math><mtable columnspacing=\"0.2em 0.4em 0.4em\" rowspacing=\"0.1em 0.1em\"><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr><mtr><mtd><mi>e</mi></mtd><mtd><mi>f</mi></mtd><mtd><mi>g</mi></mtd><mtd><mi>h</mi></mtd></mtr><mtr><mtd><mi>i</mi></mtd><mtd><mi>j</mi></mtd><mtd><mi>k</mi></mtd><mtd><mi>l</mi></mtd></mtr></mtable></math>",
    );
    let layout_short = layout_mathml(&short, &style, &ctx);
    let layout_explicit = layout_mathml(&explicit, &style, &ctx);
    assert!(
      (layout_short.width - layout_explicit.width).abs() < 0.01,
      "expected repeated columnspacing to match explicit list ({} vs {})",
      layout_short.width,
      layout_explicit.width
    );
    assert!(
      (layout_short.height - layout_explicit.height).abs() < 0.01,
      "expected repeated rowspacing to match explicit list ({} vs {})",
      layout_short.height,
      layout_explicit.height
    );
  }

  #[test]
  fn table_lines_emit_rule_fragments() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let node = parse_math_from_html(
      "<math><mtable columnlines=\"solid\" rowlines=\"solid\"><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr><mtr><mtd><mi>c</mi></mtd><mtd><mi>d</mi></mtd></mtr></mtable></math>",
    );
    let layout = layout_mathml(&node, &style, &ctx);
    let rules: Vec<&Rect> = layout
      .fragments
      .iter()
      .filter_map(|f| match f {
        MathFragment::Rule(r) => Some(r),
        _ => None,
      })
      .collect();
    assert_eq!(rules.len(), 2, "expected one columnline and one rowline");
    assert!(
      rules.iter().any(|r| r.width() < r.height()),
      "expected a vertical rule (columnline), got {rules:?}"
    );
    assert!(
      rules.iter().any(|r| r.width() > r.height()),
      "expected a horizontal rule (rowline), got {rules:?}"
    );
  }

  #[test]
  fn table_frame_emits_stroke_rect_fragment() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let without_frame = parse_math_from_html(
      "<math><mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr></mtable></math>",
    );
    let with_frame = parse_math_from_html(
      "<math><mtable frame=\"solid\" framespacing=\"0 0\"><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr></mtable></math>",
    );
    let layout_without = layout_mathml(&without_frame, &style, &ctx);
    let layout_with = layout_mathml(&with_frame, &style, &ctx);
    assert!(
      layout_with
        .fragments
        .iter()
        .any(|f| matches!(f, MathFragment::StrokeRect { .. })),
      "expected frame to emit a StrokeRect fragment"
    );
    assert!(
      layout_with.width > layout_without.width,
      "expected frame to increase table width ({} -> {})",
      layout_without.width,
      layout_with.width
    );
    assert!(
      layout_with.height > layout_without.height,
      "expected frame to increase table height ({} -> {})",
      layout_without.height,
      layout_with.height
    );
  }

  #[test]
  fn mathvariant_controls_token_style() {
    let parsed = parse_math_from_html("<math><mi mathvariant=\"normal\">x</mi></math>");
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Identifier { variant, text } = &children[0] else {
      panic!("expected identifier child");
    };
    assert_eq!(text, "x");
    assert!(matches!(variant, Some(MathVariant::Normal)));
  }

  #[test]
  fn mathsize_on_math_scales_font_size() {
    let style = ComputedStyle::default();
    let ctx = bundled_math_font_context();
    let baseline = parse_math_from_html("<math><mi>x</mi></math>");
    let scaled = parse_math_from_html("<math mathsize=\"200%\"><mi>x</mi></math>");

    let baseline_layout = layout_mathml(&baseline, &style, &ctx);
    let scaled_layout = layout_mathml(&scaled, &style, &ctx);

    let baseline_size = first_glyph_run(&baseline_layout).font_size;
    let scaled_size = first_glyph_run(&scaled_layout).font_size;

    assert!(
      (scaled_size - baseline_size * 2.0).abs() < 0.01,
      "expected mathsize=200% to scale font size ~2x ({} -> {})",
      baseline_size,
      scaled_size
    );
  }

  #[test]
  fn scriptlevel_on_token_element_scales_down_font_size() {
    let style = ComputedStyle::default();
    let ctx = bundled_math_font_context();
    let baseline = parse_math_from_html("<math><mi>x</mi></math>");
    let scripted = parse_math_from_html("<math><mi scriptlevel=\"+1\">x</mi></math>");

    let baseline_layout = layout_mathml(&baseline, &style, &ctx);
    let scripted_layout = layout_mathml(&scripted, &style, &ctx);

    let baseline_size = first_glyph_run(&baseline_layout).font_size;
    let scripted_size = first_glyph_run(&scripted_layout).font_size;

    assert!(
      scripted_size < baseline_size,
      "expected scriptlevel=+1 to reduce font size ({} -> {})",
      baseline_size,
      scripted_size
    );
  }

  #[test]
  fn mathvariant_on_container_sets_upright_default_for_identifiers() {
    let style = ComputedStyle::default();
    let ctx = bundled_math_font_context();
    let baseline = parse_math_from_html("<math><mi>x</mi></math>");
    let upright = parse_math_from_html("<math mathvariant=\"normal\"><mi>x</mi></math>");

    let baseline_layout = layout_mathml(&baseline, &style, &ctx);
    let upright_layout = layout_mathml(&upright, &style, &ctx);

    let baseline_run = first_glyph_run(&baseline_layout);
    let upright_run = first_glyph_run(&upright_layout);

    assert!(
      baseline_run.synthetic_oblique > 0.0,
      "expected default <mi> to request italic and synthesize an oblique slant, got {}",
      baseline_run.synthetic_oblique
    );
    assert!(
      upright_run.synthetic_oblique.abs() < 0.000_001,
      "expected mathvariant=normal on <math> to select upright glyphs without synthetic slant, got {}",
      upright_run.synthetic_oblique
    );
  }

  #[test]
  fn ms_wraps_text_in_default_quotes() {
    let parsed = parse_math_from_html("<math><ms>abc</ms></math>");
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Text { text, .. } = &children[0] else {
      panic!("expected text child");
    };
    assert_eq!(text, "\"abc\"");
  }

  #[test]
  fn ms_wraps_text_in_custom_quotes() {
    let parsed = parse_math_from_html("<math><ms lquote=\"[\" rquote=\"]\">abc</ms></math>");
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Text { text, .. } = &children[0] else {
      panic!("expected text child");
    };
    assert_eq!(text, "[abc]");
  }

  #[test]
  fn semantics_ignores_annotation_children() {
    let markup = r#"<math>
        <semantics>
          <mrow><mi>x</mi><mo>=</mo><mn>1</mn></mrow>
          <annotation encoding="application/x-tex">x=1</annotation>
          <annotation-xml encoding="application/mathml+xml"><mi>y</mi></annotation-xml>
        </semantics>
      </math>"#;
    let parsed = parse_math_from_html(markup);
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    assert_eq!(
      children.len(),
      1,
      "only presentation child should be parsed"
    );
    let row_children = match &children[0] {
      MathNode::Row(children) => children,
      other => panic!("expected row child, got {:?}", other),
    };
    assert_eq!(
      row_children.len(),
      3,
      "annotation content should be skipped"
    );
    assert!(
      !row_children
        .iter()
        .any(|child| { matches!(child, MathNode::Text { text, .. } if text.contains("x=1")) }),
      "annotation text should not appear in parsed output",
    );
    let dom = crate::dom::parse_html(markup).expect("dom");
    let semantics_node = find_math_element(&dom)
      .and_then(|math| {
        math.children.iter().find(|child| {
          child
            .tag_name()
            .map(|t| t.eq_ignore_ascii_case("semantics"))
            .unwrap_or(false)
        })
      })
      .expect("semantics element");
    let annotation_node = semantics_node
      .children
      .iter()
      .find(|child| {
        child
          .tag_name()
          .map(|t| t.eq_ignore_ascii_case("annotation"))
          .unwrap_or(false)
      })
      .expect("annotation child");
    assert!(
      parse_mathml(annotation_node).is_none(),
      "annotation nodes should be ignored entirely"
    );
  }

  #[test]
  fn parses_none_in_multiscripts_as_absent_slot() {
    let parsed = parse_math_from_html(
      "<math><mmultiscripts><mi>x</mi><none/><mi>a</mi></mmultiscripts></math>",
    );
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Multiscripts { postscripts, .. } = &children[0] else {
      panic!("expected multiscripts child");
    };
    assert_eq!(postscripts.len(), 1);
    let (sub, sup) = &postscripts[0];
    assert!(sub.is_none(), "expected omitted subscript to be None");
    let MathNode::Identifier { text, .. } = sup.as_ref().expect("expected superscript") else {
      panic!("expected identifier superscript");
    };
    assert_eq!(text, "a");
  }

  #[test]
  fn none_scripts_do_not_affect_multiscript_width() {
    let style = ComputedStyle::default();
    let ctx = FontContext::empty();
    let with_none =
      parse_math_from_html("<math><mmultiscripts><mi>x</mi><none/><none/></mmultiscripts></math>");
    let without_none =
      parse_math_from_html("<math><mmultiscripts><mi>x</mi></mmultiscripts></math>");
    let with_layout = layout_mathml(&with_none, &style, &ctx);
    let without_layout = layout_mathml(&without_none, &style, &ctx);
    assert!(
      (with_layout.width - without_layout.width).abs() < 0.001,
      "none placeholder should not change width: {} vs {}",
      with_layout.width,
      without_layout.width
    );
  }

  #[test]
  fn non_ascii_whitespace_mathml_normalized_text_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let markup = format!("<math><mi>{nbsp}x{nbsp}</mi></math>");
    let parsed = parse_math_from_html(&markup);
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Identifier { text, .. } = &children[0] else {
      panic!("expected identifier child");
    };
    assert_eq!(text, &format!("{nbsp}x{nbsp}"));
  }

  #[test]
  fn non_ascii_whitespace_mathml_mspace_width_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let markup = format!("<math><mspace width=\"{nbsp}1em{nbsp}\"/></math>");
    let parsed = parse_math_from_html(&markup);
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Space { width, .. } = &children[0] else {
      panic!("expected mspace child");
    };
    assert_eq!(
      *width,
      MathLength::Em(0.0),
      "NBSP must not be treated as HTML/MathML whitespace when parsing length attributes"
    );
  }

  #[test]
  fn mfenced_empty_separators_suppresses_default_commas() {
    let parsed =
      parse_math_from_html("<math><mfenced separators=\"\"><mi>a</mi><mi>b</mi></mfenced></math>");
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Row(row) = &children[0] else {
      panic!("expected mfenced to parse to a row");
    };
    assert_eq!(row.len(), 4, "expected no separator operators to be inserted");
    assert!(matches!(&row[0], MathNode::Operator { text, .. } if text == "("));
    assert!(matches!(&row[1], MathNode::Identifier { text, .. } if text == "a"));
    assert!(matches!(&row[2], MathNode::Identifier { text, .. } if text == "b"));
    assert!(matches!(&row[3], MathNode::Operator { text, .. } if text == ")"));
    assert!(
      !row
        .iter()
        .any(|node| matches!(node, MathNode::Operator { text, .. } if text == ",")),
      "no comma operator should be inserted when separators is explicitly empty"
    );
  }

  #[test]
  fn mfenced_default_separators_inserts_commas() {
    let parsed = parse_math_from_html("<math><mfenced><mi>a</mi><mi>b</mi></mfenced></math>");
    let MathNode::Math { children, .. } = parsed else {
      panic!("expected math root");
    };
    let MathNode::Row(row) = &children[0] else {
      panic!("expected mfenced to parse to a row");
    };
    assert_eq!(
      row.len(),
      5,
      "expected a single comma separator to be inserted by default"
    );
    assert!(matches!(&row[0], MathNode::Operator { text, .. } if text == "("));
    assert!(matches!(&row[1], MathNode::Identifier { text, .. } if text == "a"));
    assert!(matches!(&row[2], MathNode::Operator { text, .. } if text == ","));
    assert!(matches!(&row[3], MathNode::Identifier { text, .. } if text == "b"));
    assert!(matches!(&row[4], MathNode::Operator { text, .. } if text == ")"));
  }

  #[test]
  fn operator_dict_pipe_defaults_match_mathml_core() {
    let props = MathLayoutContext::operator_default_properties("|", OperatorForm::Infix);
    assert!(props.fence, "`|` should default to a fence");
    assert!(props.stretchy, "`|` should default to stretchy");
    assert_eq!(
      props.lspace,
      MathLengthOrKeyword::Zero,
      "`|` should default to lspace=0"
    );
    assert_eq!(
      props.rspace,
      MathLengthOrKeyword::Zero,
      "`|` should default to rspace=0"
    );
  }

  #[test]
  fn operator_dict_integral_is_largeop_and_stretchy() {
    let props = MathLayoutContext::operator_default_properties("∫", OperatorForm::Prefix);
    assert!(props.large_op, "integral should default to largeop");
    assert!(props.stretchy, "integral should default to stretchy");
  }

  #[test]
  fn operator_dict_sum_is_largeop_with_movable_limits() {
    let props = MathLayoutContext::operator_default_properties("∑", OperatorForm::Prefix);
    assert!(props.large_op, "sum should default to largeop");
    assert!(props.movable_limits, "sum should default to movablelimits");
  }

  fn bundled_font_context() -> FontContext {
    FontContext::with_config(FontConfig::bundled_only())
  }

  #[test]
  fn identifier_default_variant_is_upright_for_multiple_characters() {
    let ctx = bundled_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><mi>sin</mi></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let obliques: Vec<f32> = layout
      .fragments
      .iter()
      .filter_map(|f| match f {
        MathFragment::Glyph { run, .. } => Some(run.synthetic_oblique),
        _ => None,
      })
      .collect();
    assert!(!obliques.is_empty(), "expected at least one glyph run");
    assert!(
      obliques.iter().all(|v| *v == 0.0),
      "expected upright run for multi-character identifier: {:?}",
      obliques
    );
  }

  #[test]
  fn accent_mover_does_not_shrink() {
    let ctx = math_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();

    let node =
      parse_math_from_html("<math><mover accent=\"true\"><mi>x</mi><mo>¯</mo></mover></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let base_run = find_run(&layout, "x");
    let accent_run = find_run(&layout, "¯");
    assert!(
      (accent_run.font_size - base_run.font_size).abs() < 0.01,
      "accent should not be shrunk (accent={}, base={})",
      accent_run.font_size,
      base_run.font_size
    );
  }

  #[test]
  fn mo_maxsize_caps_vertical_stretching() {
    let ctx = bundled_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();

    let with_maxsize = parse_math_from_html(
      r#"<math display="block"><mrow>
          <mo stretchy="true" maxsize="1em">(</mo>
          <mfrac><mrow><mi>a</mi><mi>b</mi></mrow><mrow><mi>c</mi><mi>d</mi></mrow></mfrac>
          <mo stretchy="true" maxsize="1em">)</mo>
        </mrow></math>"#,
    );
    let without_maxsize = parse_math_from_html(
      r#"<math display="block"><mrow>
          <mo stretchy="true">(</mo>
          <mfrac><mrow><mi>a</mi><mi>b</mi></mrow><mrow><mi>c</mi><mi>d</mi></mrow></mfrac>
          <mo stretchy="true">)</mo>
        </mrow></math>"#,
    );

    let capped = first_row_operator_height(&with_maxsize, &style, &ctx, "(");
    let uncapped = first_row_operator_height(&without_maxsize, &style, &ctx, "(");
    assert!(
      capped < uncapped * 0.75,
      "expected maxsize to cap stretchy delimiter height: capped={}, uncapped={}",
      capped,
      uncapped
    );
  }

  #[test]
  fn identifier_default_variant_is_italic_for_single_character() {
    let ctx = bundled_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();
    let node = parse_math_from_html("<math><mi>x</mi></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let obliques: Vec<f32> = layout
      .fragments
      .iter()
      .filter_map(|f| match f {
        MathFragment::Glyph { run, .. } => Some(run.synthetic_oblique),
        _ => None,
      })
      .collect();
    assert!(!obliques.is_empty(), "expected at least one glyph run");
    assert!(
      obliques.iter().any(|v| *v != 0.0),
      "expected synthetic oblique for single-character identifier: {:?}",
      obliques
    );
  }

  #[test]
  fn non_accent_mover_shrinks() {
    let ctx = math_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();

    let node = parse_math_from_html("<math><mover><mi>x</mi><mi>n</mi></mover></math>");
    let layout = layout_mathml(&node, &style, &ctx);
    let base_run = find_run(&layout, "x");
    let over_run = find_run(&layout, "n");
    assert!(
      over_run.font_size < base_run.font_size,
      "non-accent overscript should be shrunk (over={}, base={})",
      over_run.font_size,
      base_run.font_size
    );
  }

  #[test]
  fn mo_minsize_enforces_minimum_vertical_stretching() {
    let ctx = bundled_font_context();
    let mut style = ComputedStyle::default();
    style.font_size = 24.0;
    style.font_family = vec!["STIX Two Math".to_string()].into();

    let with_minsize = parse_math_from_html(
      r#"<math display="block"><mrow>
          <mo stretchy="true" minsize="3em">(</mo>
          <mi>x</mi>
        </mrow></math>"#,
    );
    let without_minsize = parse_math_from_html(
      r#"<math display="block"><mrow>
          <mo stretchy="true">(</mo>
          <mi>x</mi>
        </mrow></math>"#,
    );

    let enforced = first_row_operator_height(&with_minsize, &style, &ctx, "(");
    let baseline = first_row_operator_height(&without_minsize, &style, &ctx, "(");
    assert!(
      enforced > baseline * 1.25,
      "expected minsize to increase stretchy delimiter height: enforced={}, baseline={}",
      enforced,
      baseline
    );
  }
}
