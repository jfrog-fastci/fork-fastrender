//! MathML Core operator dictionary (subset).
//!
//! This module provides a data-driven replacement for the tiny hard-coded
//! operator defaults in `MathLayoutContext::operator_default_properties`.
//!
//! The MathML Core spec defines per-operator defaults for properties like:
//! `stretchy`, `fence`, and default spacing (`lspace`, `rspace`) depending on
//! the operator's `form` (prefix/infix/postfix).
//!
//! For now we embed a high-value subset that covers common real-world MathML
//! content (delimiters, basic arithmetic, relations, and large operators).

use super::{MathLengthOrKeyword, OperatorForm};

#[derive(Debug, Clone, Copy)]
pub(super) struct OperatorDictProperties {
  pub fence: bool,
  pub separator: bool,
  pub stretchy: bool,
  pub large_op: bool,
  pub movable_limits: bool,
  pub accent: bool,
  pub lspace: MathLengthOrKeyword,
  pub rspace: MathLengthOrKeyword,
}

#[derive(Debug, Clone, Copy)]
struct OperatorDictEntry {
  text: &'static str,
  form: OperatorForm,
  props: OperatorDictProperties,
}

const fn props(
  fence: bool,
  separator: bool,
  stretchy: bool,
  large_op: bool,
  movable_limits: bool,
  lspace: MathLengthOrKeyword,
  rspace: MathLengthOrKeyword,
) -> OperatorDictProperties {
  OperatorDictProperties {
    fence,
    separator,
    stretchy,
    large_op,
    movable_limits,
    accent: false,
    lspace,
    rspace,
  }
}

const fn props_accent(
  fence: bool,
  separator: bool,
  stretchy: bool,
  large_op: bool,
  movable_limits: bool,
  lspace: MathLengthOrKeyword,
  rspace: MathLengthOrKeyword,
) -> OperatorDictProperties {
  OperatorDictProperties {
    fence,
    separator,
    stretchy,
    large_op,
    movable_limits,
    accent: true,
    lspace,
    rspace,
  }
}

// NOTE: Keep this table small and focused. It is easy to extend as new fixtures
// or bug reports require additional operators.
static OPERATOR_DICT: &[OperatorDictEntry] = &[
  // -------------------------------------------------------------------------
  // Fences / delimiters
  // -------------------------------------------------------------------------
  // Parentheses.
  OperatorDictEntry {
    text: "(",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: ")",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Square brackets.
  OperatorDictEntry {
    text: "[",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "]",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Curly braces.
  OperatorDictEntry {
    text: "{",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "}",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Angle brackets.
  OperatorDictEntry {
    text: "⟨",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "⟩",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Double brackets.
  OperatorDictEntry {
    text: "⟦",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "⟧",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Ceiling / floor.
  OperatorDictEntry {
    text: "⌈",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "⌉",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "⌊",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "⌋",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  // Vertical bars (symmetric delimiters).
  OperatorDictEntry {
    text: "|",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "|",
    form: OperatorForm::Infix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "|",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "‖",
    form: OperatorForm::Prefix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "‖",
    form: OperatorForm::Infix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "‖",
    form: OperatorForm::Postfix,
    props: props(true, false, true, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },

  // -------------------------------------------------------------------------
  // Punctuation / separators
  // -------------------------------------------------------------------------
  OperatorDictEntry {
    text: ",",
    form: OperatorForm::Infix,
    props: props(false, true, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: ";",
    form: OperatorForm::Infix,
    props: props(false, true, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Thin),
  },

  // -------------------------------------------------------------------------
  // Accents
  // -------------------------------------------------------------------------
  OperatorDictEntry {
    text: "¯",
    form: OperatorForm::Infix,
    props: props_accent(
      false,
      false,
      false,
      false,
      false,
      MathLengthOrKeyword::Zero,
      MathLengthOrKeyword::Zero,
    ),
  },

  // -------------------------------------------------------------------------
  // Relations
  // -------------------------------------------------------------------------
  OperatorDictEntry {
    text: "=",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },
  OperatorDictEntry {
    text: "≠",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },
  OperatorDictEntry {
    text: "<",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },
  OperatorDictEntry {
    text: ">",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },
  OperatorDictEntry {
    text: "≤",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },
  OperatorDictEntry {
    text: "≥",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Thick, MathLengthOrKeyword::Thick),
  },

  // -------------------------------------------------------------------------
  // Binary / unary operators
  // -------------------------------------------------------------------------
  // Plus / minus: spacing depends on form.
  OperatorDictEntry {
    text: "+",
    form: OperatorForm::Prefix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "+",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "+",
    form: OperatorForm::Postfix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "-",
    form: OperatorForm::Prefix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "-",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "-",
    form: OperatorForm::Postfix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "−",
    form: OperatorForm::Prefix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "−",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "−",
    form: OperatorForm::Postfix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "±",
    form: OperatorForm::Prefix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "±",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "±",
    form: OperatorForm::Postfix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Zero, MathLengthOrKeyword::Zero),
  },
  OperatorDictEntry {
    text: "×",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "·",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },
  OperatorDictEntry {
    text: "÷",
    form: OperatorForm::Infix,
    props: props(false, false, false, false, false, MathLengthOrKeyword::Medium, MathLengthOrKeyword::Medium),
  },

  // -------------------------------------------------------------------------
  // Large operators
  // -------------------------------------------------------------------------
  OperatorDictEntry {
    text: "∑",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∏",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∐",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⋀",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⋁",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⋂",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⋃",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨀",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨁",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨂",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨃",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨄",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨅",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨆",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⨉",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "⫿",
    form: OperatorForm::Infix,
    props: props(false, false, false, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  // Integrals: large operators, typically stretchy in MathML Core.
  OperatorDictEntry {
    text: "∫",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∬",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∭",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∮",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∯",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
  OperatorDictEntry {
    text: "∰",
    form: OperatorForm::Infix,
    props: props(false, false, true, true, true, MathLengthOrKeyword::Thin, MathLengthOrKeyword::Thin),
  },
];

pub(super) fn lookup(text: &str, form: OperatorForm) -> Option<OperatorDictProperties> {
  OPERATOR_DICT
    .iter()
    .find(|entry| entry.form == form && entry.text == text)
    .map(|entry| entry.props)
}
