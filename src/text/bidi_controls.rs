//! Shared helpers for bidirectional (bidi) formatting characters.
//!
//! These characters are *default-ignorable* and should not perturb shaping output (kerning,
//! ligatures, etc.), even though they may influence bidi resolution.

/// Returns `true` if `ch` is a bidi *format* character that should be ignored for shaping.
///
/// Includes:
/// - U+202A..=U+202E (LRE/RLE/PDF/LRO/RLO)
/// - U+2066..=U+2069 (LRI/RLI/FSI/PDI)
/// - Bidi marks: U+200E (LRM), U+200F (RLM), U+061C (ALM)
///
/// Note: Do **not** add ZWJ/ZWNJ/variation selectors here; they are shaping-relevant.
#[inline]
pub fn is_bidi_format_char(ch: char) -> bool {
  matches!(
    ch,
    '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
  )
}

