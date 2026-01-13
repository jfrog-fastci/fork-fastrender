use super::pat::is_valid_pattern_identifier;
use super::Asi;
use super::ParseCtx;
use super::Parser;
use crate::ast::class_or_object::ClassOrObjKey;
use crate::ast::class_or_object::ClassOrObjMemberDirectKey;
use crate::ast::class_or_object::ClassOrObjVal;
use crate::ast::class_or_object::ObjMember;
use crate::ast::class_or_object::ObjMemberType;
use crate::ast::expr::lit::LitArrElem;
use crate::ast::expr::lit::LitArrExpr;
use crate::ast::expr::lit::LitBigIntExpr;
use crate::ast::expr::lit::LitBoolExpr;
use crate::ast::expr::lit::LitNullExpr;
use crate::ast::expr::lit::LitNumExpr;
use crate::ast::expr::lit::LitObjExpr;
use crate::ast::expr::lit::LitRegexExpr;
use crate::ast::expr::lit::LitStrExpr;
use crate::ast::expr::lit::LitTemplateExpr;
use crate::ast::expr::lit::LitTemplatePart;
use crate::ast::expr::BinaryExpr;
use crate::ast::expr::IdExpr;
use crate::ast::node::InvalidTemplateEscapeSequence;
use crate::ast::node::CoverInitializedName;
use crate::ast::node::TrailingCommaAfterRestElement;
use crate::ast::node::LeadingZeroDecimalLiteral;
use crate::ast::node::LegacyOctalEscapeSequence;
use crate::ast::node::LegacyOctalNumberLiteral;
use crate::ast::node::LiteralStringCodeUnits;
use crate::ast::node::Node;
use crate::ast::node::TemplateStringParts;
use crate::char::is_line_terminator;
use crate::error::SyntaxError;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::lex::LexMode;
use crate::lex::KEYWORDS_MAPPING;
use crate::loc::Loc;
use crate::num::JsNumber;
use crate::operator::OperatorName;
use crate::token::keyword_from_str;
use crate::token::TT;
use num_bigint::BigInt;
use std::collections::HashMap;
use unicode_ident::is_xid_continue;
use unicode_ident::is_xid_start;
pub fn normalise_literal_number(raw: &str) -> Option<JsNumber> {
  JsNumber::from_literal(raw)
}

pub fn normalise_literal_bigint(raw: &str) -> Option<String> {
  // Canonicalise BigInt literals while preserving their original radix. Prefixes are normalised
  // to lowercase (`0b`/`0o`/`0x`), numeric separators are stripped, and hex digits are emitted in
  // lowercase via `char::from_digit`. The value is returned as a canonical decimal string (without
  // the trailing `n`) so downstream consumers can parse it deterministically.
  if !raw.ends_with('n') {
    return None;
  }
  let body = &raw[..raw.len().saturating_sub(1)];
  let (radix, digits) =
    if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
      (2, rest)
    } else if let Some(rest) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
      (8, rest)
    } else if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
      (16, rest)
    } else {
      (10, body)
    };

  let mut normalised_digits = String::with_capacity(digits.len());
  let mut prev_sep = false;
  let mut saw_digit = false;
  for ch in digits.chars() {
    if ch == '_' {
      // Separators must be sandwiched between digits.
      if prev_sep || !saw_digit {
        return None;
      }
      prev_sep = true;
      continue;
    }
    let value = ch.to_digit(radix)?;
    let digit = char::from_digit(value, radix)?;
    normalised_digits.push(digit);
    saw_digit = true;
    prev_sep = false;
  }
  if prev_sep || !saw_digit {
    return None;
  }

  if radix == 10 && normalised_digits.len() > 1 && normalised_digits.starts_with('0') {
    // Decimal BigInt literals cannot use a leading zero.
    return None;
  }

  let value = BigInt::parse_bytes(normalised_digits.as_bytes(), radix)?;
  Some(value.to_str_radix(10))
}

#[derive(Clone, Copy, Debug)]
enum LiteralErrorKind {
  InvalidEscape,
  UnexpectedEnd,
  LineTerminator,
}

#[derive(Clone, Copy, Debug)]
struct LiteralError {
  kind: LiteralErrorKind,
  offset: usize,
  len: usize,
}

fn decode_escape_sequence(
  raw: &str,
  escape_start: usize,
) -> Result<(usize, Option<char>), LiteralError> {
  let mut chars = raw.chars();
  let Some(first) = chars.next() else {
    return Err(LiteralError {
      kind: LiteralErrorKind::UnexpectedEnd,
      offset: escape_start,
      len: 0,
    });
  };
  match first {
    '\r' => {
      let mut consumed = first.len_utf8();
      if raw[first.len_utf8()..].starts_with('\n') {
        consumed += '\n'.len_utf8();
      }
      Ok((consumed, None))
    }
    '\n' | '\u{2028}' | '\u{2029}' => Ok((first.len_utf8(), None)),
    'b' => Ok((1, Some('\x08'))),
    'f' => Ok((1, Some('\x0c'))),
    'n' => Ok((1, Some('\n'))),
    'r' => Ok((1, Some('\r'))),
    't' => Ok((1, Some('\t'))),
    'v' => Ok((1, Some('\x0b'))),
    '0'..='7' => {
      let mut consumed = first.len_utf8();
      let mut value = first.to_digit(8).unwrap();
      for ch in raw[consumed..].chars().take(2) {
        if ('0'..='7').contains(&ch) {
          consumed += ch.len_utf8();
          value = (value << 3) + ch.to_digit(8).unwrap();
        } else {
          break;
        }
      }
      let Some(c) = char::from_u32(value) else {
        return Err(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        });
      };
      Ok((consumed, Some(c)))
    }
    'x' => {
      let mut hex_iter = raw[first.len_utf8()..].chars();
      let Some(h1) = hex_iter.next() else {
        return Err(LiteralError {
          kind: LiteralErrorKind::UnexpectedEnd,
          offset: escape_start,
          len: 0,
        });
      };
      let Some(h2) = hex_iter.next() else {
        return Err(LiteralError {
          kind: LiteralErrorKind::UnexpectedEnd,
          offset: escape_start,
          len: 0,
        });
      };
      if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
        return Err(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        });
      }
      let cp = u32::from_str_radix(&format!("{h1}{h2}"), 16).unwrap();
      let Some(c) = char::from_u32(cp) else {
        return Err(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        });
      };
      let consumed = first.len_utf8() + h1.len_utf8() + h2.len_utf8();
      Ok((consumed, Some(c)))
    }
    'u' => {
      let after_u = &raw[first.len_utf8()..];
      if after_u.starts_with('{') {
        let Some(end) = after_u.find('}') else {
          return Err(LiteralError {
            kind: LiteralErrorKind::UnexpectedEnd,
            offset: escape_start,
            len: 0,
          });
        };
        let hex = &after_u[1..end];
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        let value = u32::from_str_radix(hex, 16).ok().ok_or(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        })?;
        if value > 0x10FFFF {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        // JavaScript strings are UTF-16; allow surrogate code points by mapping
        // them to U+FFFD so we can represent them in Rust `String`s.
        let cp = char::from_u32(value).unwrap_or('\u{FFFD}');
        let consumed = first.len_utf8() + end + 1;
        Ok((consumed, Some(cp)))
      } else {
        let mut hex = String::new();
        let mut consumed = first.len_utf8();
        for ch in after_u.chars().take(4) {
          hex.push(ch);
          consumed += ch.len_utf8();
        }
        if hex.len() < 4 {
          return Err(LiteralError {
            kind: LiteralErrorKind::UnexpectedEnd,
            offset: escape_start,
            len: 0,
          });
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        let value = u32::from_str_radix(&hex, 16).ok().ok_or(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        })?;
        // Combine surrogate pairs when possible so sequences like `\\uD83D\\uDE00`
        // decode to a valid Unicode scalar.
        if (0xD800..=0xDBFF).contains(&value) {
          let rest = &after_u[4..];
          if rest.len() >= 6 && rest.starts_with("\\u") {
            let low_hex = &rest[2..6];
            if low_hex.chars().all(|c| c.is_ascii_hexdigit()) {
              let low = u32::from_str_radix(low_hex, 16).unwrap();
              if (0xDC00..=0xDFFF).contains(&low) {
                let high_ten = (value - 0xD800) << 10;
                let low_ten = low - 0xDC00;
                let combined = 0x10000 + high_ten + low_ten;
                if let Some(cp) = char::from_u32(combined) {
                  return Ok((consumed + 6, Some(cp)));
                }
              }
            }
          }
        }
        let cp = char::from_u32(value).unwrap_or('\u{FFFD}');
        Ok((consumed, Some(cp)))
      }
    }
    c => Ok((c.len_utf8(), Some(c))),
  }
}

#[derive(Clone, Copy, Debug)]
enum Utf16Escape {
  None,
  One(u16),
  Two(u16, u16),
}

fn decode_escape_sequence_utf16(
  raw: &str,
  escape_start: usize,
) -> Result<(usize, Utf16Escape), LiteralError> {
  let mut chars = raw.chars();
  let Some(first) = chars.next() else {
    return Err(LiteralError {
      kind: LiteralErrorKind::UnexpectedEnd,
      offset: escape_start,
      len: 0,
    });
  };
  match first {
    '\r' => {
      let mut consumed = first.len_utf8();
      if raw[first.len_utf8()..].starts_with('\n') {
        consumed += '\n'.len_utf8();
      }
      Ok((consumed, Utf16Escape::None))
    }
    '\n' | '\u{2028}' | '\u{2029}' => Ok((first.len_utf8(), Utf16Escape::None)),
    'b' => Ok((1, Utf16Escape::One(0x08))),
    'f' => Ok((1, Utf16Escape::One(0x0c))),
    'n' => Ok((1, Utf16Escape::One(0x0a))),
    'r' => Ok((1, Utf16Escape::One(0x0d))),
    't' => Ok((1, Utf16Escape::One(0x09))),
    'v' => Ok((1, Utf16Escape::One(0x0b))),
    '0'..='7' => {
      let mut consumed = first.len_utf8();
      let mut value = first.to_digit(8).unwrap() as u16;
      for ch in raw[consumed..].chars().take(2) {
        if ('0'..='7').contains(&ch) {
          consumed += ch.len_utf8();
          value = (value << 3) + ch.to_digit(8).unwrap() as u16;
        } else {
          break;
        }
      }
      Ok((consumed, Utf16Escape::One(value)))
    }
    'x' => {
      let mut hex_iter = raw[first.len_utf8()..].chars();
      let Some(h1) = hex_iter.next() else {
        return Err(LiteralError {
          kind: LiteralErrorKind::UnexpectedEnd,
          offset: escape_start,
          len: 0,
        });
      };
      let Some(h2) = hex_iter.next() else {
        return Err(LiteralError {
          kind: LiteralErrorKind::UnexpectedEnd,
          offset: escape_start,
          len: 0,
        });
      };
      if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
        return Err(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        });
      }
      let value = u16::from_str_radix(&format!("{h1}{h2}"), 16).unwrap();
      let consumed = first.len_utf8() + h1.len_utf8() + h2.len_utf8();
      Ok((consumed, Utf16Escape::One(value)))
    }
    'u' => {
      let after_u = &raw[first.len_utf8()..];
      if after_u.starts_with('{') {
        let Some(end) = after_u.find('}') else {
          return Err(LiteralError {
            kind: LiteralErrorKind::UnexpectedEnd,
            offset: escape_start,
            len: 0,
          });
        };
        let hex = &after_u[1..end];
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        let value = u32::from_str_radix(hex, 16).ok().ok_or(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        })?;
        if value > 0x10FFFF {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        let consumed = first.len_utf8() + end + 1;
        Ok(if value <= 0xFFFF {
          (consumed, Utf16Escape::One(value as u16))
        } else {
          let v = value - 0x10000;
          let high = 0xD800 + ((v >> 10) as u16);
          let low = 0xDC00 + ((v & 0x3FF) as u16);
          (consumed, Utf16Escape::Two(high, low))
        })
      } else {
        let mut hex = String::new();
        let mut consumed = first.len_utf8();
        for ch in after_u.chars().take(4) {
          hex.push(ch);
          consumed += ch.len_utf8();
        }
        if hex.len() < 4 {
          return Err(LiteralError {
            kind: LiteralErrorKind::UnexpectedEnd,
            offset: escape_start,
            len: 0,
          });
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
          return Err(LiteralError {
            kind: LiteralErrorKind::InvalidEscape,
            offset: escape_start,
            len: 1,
          });
        }
        let value = u16::from_str_radix(&hex, 16).ok().ok_or(LiteralError {
          kind: LiteralErrorKind::InvalidEscape,
          offset: escape_start,
          len: 1,
        })?;
        Ok((consumed, Utf16Escape::One(value)))
      }
    }
    c => {
      let mut buf = [0u16; 2];
      let encoded = c.encode_utf16(&mut buf);
      let addition = if encoded.len() == 1 {
        Utf16Escape::One(encoded[0])
      } else {
        Utf16Escape::Two(encoded[0], encoded[1])
      };
      Ok((c.len_utf8(), addition))
    }
  }
}

fn decode_literal(raw: &str, allow_line_terminators: bool) -> Result<String, LiteralError> {
  let mut norm = String::new();
  let mut offset = 0;
  while offset < raw.len() {
    let mut iter = raw[offset..].char_indices();
    let (rel, ch) = iter.next().unwrap();
    debug_assert_eq!(rel, 0);
    if ch == '\\' {
      let escape_start = offset;
      let after_backslash = offset + ch.len_utf8();
      let (consumed, addition) = decode_escape_sequence(&raw[after_backslash..], escape_start)?;
      if let Some(c) = addition {
        norm.push(c);
      }
      offset = after_backslash + consumed;
    } else {
      // ECMAScript 2019 permits U+2028/U+2029 (line/paragraph separators) in
      // string literals; only CR/LF terminate literal lines.
      if !allow_line_terminators && matches!(ch, '\n' | '\r') {
        return Err(LiteralError {
          kind: LiteralErrorKind::LineTerminator,
          offset,
          len: ch.len_utf8(),
        });
      }
      // When line terminators are allowed (template literals and similar contexts),
      // ECMAScript normalizes CR and CRLF to a single LF code point.
      if allow_line_terminators && ch == '\r' {
        norm.push('\n');
        offset += ch.len_utf8();
        if raw[offset..].starts_with('\n') {
          offset += '\n'.len_utf8();
        }
      } else {
        norm.push(ch);
        offset += ch.len_utf8();
      }
    }
  }
  Ok(norm)
}

fn decode_literal_utf16(raw: &str, allow_line_terminators: bool) -> Result<Vec<u16>, LiteralError> {
  let mut norm = Vec::<u16>::new();
  let mut offset = 0;
  while offset < raw.len() {
    let mut iter = raw[offset..].char_indices();
    let (rel, ch) = iter.next().unwrap();
    debug_assert_eq!(rel, 0);
    if ch == '\\' {
      let escape_start = offset;
      let after_backslash = offset + ch.len_utf8();
      let (consumed, addition) =
        decode_escape_sequence_utf16(&raw[after_backslash..], escape_start)?;
      match addition {
        Utf16Escape::None => {}
        Utf16Escape::One(v) => norm.push(v),
        Utf16Escape::Two(a, b) => {
          norm.push(a);
          norm.push(b);
        }
      }
      offset = after_backslash + consumed;
    } else {
      // ECMAScript 2019 permits U+2028/U+2029 (line/paragraph separators) in
      // string literals; only CR/LF terminate literal lines.
      if !allow_line_terminators && matches!(ch, '\n' | '\r') {
        return Err(LiteralError {
          kind: LiteralErrorKind::LineTerminator,
          offset,
          len: ch.len_utf8(),
        });
      }
      // When line terminators are allowed (template literals and similar contexts),
      // ECMAScript normalizes CR and CRLF to a single LF code point.
      if allow_line_terminators && ch == '\r' {
        norm.push(0x0a);
        offset += ch.len_utf8();
        if raw[offset..].starts_with('\n') {
          offset += '\n'.len_utf8();
        }
      } else {
        let mut buf = [0u16; 2];
        let encoded = ch.encode_utf16(&mut buf);
        norm.extend_from_slice(encoded);
        offset += ch.len_utf8();
      }
    }
  }
  Ok(norm)
}

fn encode_template_raw_utf16(raw: &str) -> Box<[u16]> {
  // Template raw strings include backslashes and escape sequences verbatim, but
  // line terminator sequences are normalized: CR and CRLF become LF.
  let mut norm = Vec::<u16>::new();
  let mut offset = 0;
  while offset < raw.len() {
    let mut iter = raw[offset..].char_indices();
    let (rel, ch) = iter.next().unwrap();
    debug_assert_eq!(rel, 0);

    if ch == '\r' {
      norm.push(0x0a);
      offset += ch.len_utf8();
      if raw[offset..].starts_with('\n') {
        offset += '\n'.len_utf8();
      }
      continue;
    }

    let mut buf = [0u16; 2];
    let encoded = ch.encode_utf16(&mut buf);
    norm.extend_from_slice(encoded);
    offset += ch.len_utf8();
  }
  norm.into_boxed_slice()
}

fn find_legacy_escape_sequence(raw: &str) -> Option<(usize, usize)> {
  let bytes = raw.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] != b'\\' {
      i += 1;
      continue;
    }
    if i + 1 >= bytes.len() {
      break;
    }
    match bytes[i + 1] {
      b'0' => {
        if i + 2 >= bytes.len() {
          i += 2;
          continue;
        }
        let next = bytes[i + 2];
        if !next.is_ascii_digit() {
          // `\0` (null escape) is permitted; only `\0` followed by a decimal
          // digit counts as a legacy escape sequence.
          i += 2;
          continue;
        }
        if next == b'8' || next == b'9' {
          return Some((i, 3));
        }
        let mut len = 2;
        let mut digits = 1;
        let mut j = i + 2;
        while digits < 3 && j < bytes.len() {
          let c = bytes[j];
          if !(b'0'..=b'7').contains(&c) {
            break;
          }
          len += 1;
          digits += 1;
          j += 1;
        }
        return Some((i, len));
      }
      b'1'..=b'7' => {
        let mut len = 2;
        let mut digits = 1;
        let mut j = i + 2;
        while digits < 3 && j < bytes.len() {
          let c = bytes[j];
          if !(b'0'..=b'7').contains(&c) {
            break;
          }
          len += 1;
          digits += 1;
          j += 1;
        }
        return Some((i, len));
      }
      b'8' | b'9' => return Some((i, 2)),
      _ => {}
    }
    i += 2;
  }
  None
}

fn find_non_octal_decimal_escape_sequence(raw: &str) -> Option<(usize, usize)> {
  let bytes = raw.as_bytes();
  let mut i = 0;
  while i + 1 < bytes.len() {
    if bytes[i] != b'\\' {
      i += 1;
      continue;
    }
    match bytes[i + 1] {
      b'8' | b'9' => return Some((i, 2)),
      _ => {
        // Skip the escaped character so we don't treat it as the start of a new escape sequence
        // (e.g. `\\9`).
        i += 2;
      }
    }
  }
  None
}

fn literal_error_to_syntax(
  err: LiteralError,
  base: usize,
  token: TT,
  line_error: SyntaxErrorType,
) -> SyntaxError {
  let typ = match err.kind {
    LiteralErrorKind::InvalidEscape => SyntaxErrorType::InvalidCharacterEscape,
    LiteralErrorKind::UnexpectedEnd => SyntaxErrorType::UnexpectedEnd,
    LiteralErrorKind::LineTerminator => line_error,
  };
  let start = base + err.offset;
  let end = start + err.len;
  Loc(start, end).error(typ, Some(token))
}

fn template_content(raw: &str, is_end: bool) -> Option<(usize, &str)> {
  let mut start = 0;
  let mut end = raw.len();
  if raw.starts_with('`') && raw.len() > '`'.len_utf8() {
    start += '`'.len_utf8();
  }
  if is_end {
    if !raw.ends_with('`') {
      return None;
    }
    end = end.saturating_sub('`'.len_utf8());
  } else {
    if !raw.ends_with("${") {
      return None;
    }
    end = end.saturating_sub("${".len());
  }
  if end < start {
    return None;
  }
  raw.get(start..end).map(|body| (start, body))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegexErrorKind {
  LineTerminator,
  Unterminated,
  InvalidFlag,
  DuplicateFlag,
  InvalidPattern,
}

#[derive(Debug)]
struct RegexError {
  kind: RegexErrorKind,
  offset: usize,
  len: usize,
}

fn validate_regex_flags(raw: &str, start: usize) -> Result<(), RegexError> {
  const U_FLAG: u16 = 1 << 5;
  const V_FLAG: u16 = 1 << 6;
  let mut seen_flags: u16 = 0;
  for (offset, ch) in raw[start..].char_indices() {
    let bit = match ch {
      'd' => 1 << 0,
      'g' => 1 << 1,
      'i' => 1 << 2,
      'm' => 1 << 3,
      's' => 1 << 4,
      'u' => U_FLAG,
      'v' => V_FLAG,
      'y' => 1 << 7,
      _ => {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidFlag,
          offset: start + offset,
          len: ch.len_utf8(),
        })
      }
    };
    // `u` and `v` are mutually exclusive.
    if (bit == U_FLAG && (seen_flags & V_FLAG) != 0) || (bit == V_FLAG && (seen_flags & U_FLAG) != 0) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidFlag,
        offset: start + offset,
        len: ch.len_utf8(),
      });
    }
    if seen_flags & bit != 0 {
      return Err(RegexError {
        kind: RegexErrorKind::DuplicateFlag,
        offset: start + offset,
        len: ch.len_utf8(),
      });
    }
    seen_flags |= bit;
  }
  Ok(())
}

fn regex_capture_info(pattern: &str, unicode_sets_mode: bool) -> (usize, bool) {
  let bytes = pattern.as_bytes();
  let mut i = 0usize;
  let mut charset_depth = 0usize;
  let mut count = 0usize;
  let mut has_named_groups = false;
  while i < pattern.len() {
    let ch = pattern[i..].chars().next().unwrap();
    if ch == '\\' {
      // Skip the escaped code point.
      i += '\\'.len_utf8();
      if i >= pattern.len() {
        break;
      }
      i += pattern[i..].chars().next().unwrap().len_utf8();
      continue;
    }
    if charset_depth > 0 {
      if unicode_sets_mode && ch == '[' {
        charset_depth += 1;
      } else if ch == ']' {
        charset_depth = charset_depth.saturating_sub(1);
      }
      i += ch.len_utf8();
      continue;
    }
    if ch == '[' {
      charset_depth = 1;
      i += '['.len_utf8();
      continue;
    }
    if ch == '(' {
      // Capture groups are:
      // - plain `(...)`
      // - named groups `(?<name>...)`
      if i + 1 < bytes.len() && bytes[i + 1] == b'?' {
        if i + 2 < bytes.len() && bytes[i + 2] == b'<' && i + 3 < bytes.len() {
          match bytes[i + 3] {
            b'=' | b'!' => {}
            _ => {
              count += 1;
              has_named_groups = true;
            }
          }
        }
      } else {
        count += 1;
      }
    }
    i += ch.len_utf8();
  }
  (count, has_named_groups)
}

fn regex_is_other_id_start(c: char) -> bool {
  // ECMAScript augments `ID_Start`/`XID_Start` with a small, fixed set of additional characters
  // (`Other_ID_Start`).
  matches!(
    c,
    '\u{1885}' | '\u{1886}' | '\u{2118}' | '\u{212e}' | '\u{309b}' | '\u{309c}'
  )
}

fn regex_is_other_id_continue(c: char) -> bool {
  // ECMAScript augments `ID_Continue`/`XID_Continue` with `Other_ID_Continue`.
  matches!(
    c,
    '\u{00b7}' | '\u{0387}' | '\u{1369}'..='\u{1371}' | '\u{19da}'
  )
}

fn regex_is_identifier_start(c: char) -> bool {
  if c.is_ascii() {
    matches!(c, '$' | '_' | 'a'..='z' | 'A'..='Z')
  } else {
    is_xid_start(c) || regex_is_other_id_start(c)
  }
}

fn regex_is_identifier_continue(c: char) -> bool {
  if c.is_ascii() {
    matches!(c, '$' | '_' | '0'..='9' | 'a'..='z' | 'A'..='Z')
  } else {
    is_xid_continue(c) || regex_is_other_id_continue(c) || c == '\u{200c}' || c == '\u{200d}'
  }
}

fn regex_parse_unicode_escape_in_identifier(
  pattern: &str,
  escape_start: usize,
) -> Result<(char, usize /* consumed */), RegexError> {
  debug_assert_eq!(pattern.as_bytes()[escape_start], b'\\');
  let bytes = pattern.as_bytes();
  let after_backslash = escape_start + 1;
  if after_backslash >= bytes.len() {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: escape_start,
      len: 1,
    });
  }
  if bytes[after_backslash] != b'u' {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: escape_start,
      len: 1,
    });
  }
  let after_u = after_backslash + 1;
  if after_u >= bytes.len() {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: escape_start,
      len: after_u.saturating_sub(escape_start),
    });
  }

  let value: u32;
  let end: usize;
  if bytes[after_u] == b'{' {
    // `\u{HexDigits}` (always permitted in identifier names).
    let mut j = after_u + 1;
    let mut saw_digit = false;
    // Overflow-safe parse allowing arbitrarily many leading zeros.
    let mut started = false;
    let mut significant_digits: usize = 0;
    let mut v: u32 = 0;
    while j < bytes.len() && bytes[j] != b'}' {
      let b = bytes[j];
      if !(b as char).is_ascii_hexdigit() {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: escape_start,
          len: j + 1 - escape_start,
        });
      }
      saw_digit = true;
      let digit: u32 = match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'f' => (b - b'a' + 10) as u32,
        b'A'..=b'F' => (b - b'A' + 10) as u32,
        _ => unreachable!(),
      };

      if !started {
        if digit != 0 {
          started = true;
          significant_digits = 1;
          v = digit;
        }
      } else {
        significant_digits += 1;
        // 0x10FFFF fits in 6 hex digits; any additional significant digit is definitely out of
        // range.
        if significant_digits > 6 {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: escape_start,
            len: j + 1 - escape_start,
          });
        }
        v = (v << 4) | digit;
        if v > 0x10FFFF {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: escape_start,
            len: j + 1 - escape_start,
          });
        }
      }

      j += 1;
    }

    if j >= bytes.len() || !saw_digit {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: escape_start,
        len: j.saturating_sub(escape_start),
      });
    }
    debug_assert_eq!(bytes[j], b'}');
    value = if started { v } else { 0 };
    end = j + 1;
  } else {
    // `\uXXXX`
    let mut v: u32 = 0;
    let mut j = after_u;
    for _ in 0..4 {
      if j >= bytes.len() || !(bytes[j] as char).is_ascii_hexdigit() {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: escape_start,
          len: j.saturating_sub(escape_start),
        });
      }
      v = (v << 4) | (bytes[j] as char).to_digit(16).unwrap();
      j += 1;
    }
    value = v;
    end = j;
  }

  if value > 0x10FFFF {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: escape_start,
      len: end - escape_start,
    });
  }

  let Some(c) = char::from_u32(value) else {
    // Surrogate code points are not valid Unicode scalar values and cannot appear in identifier
    // names.
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: escape_start,
      len: end - escape_start,
    });
  };
  Ok((c, end - escape_start))
}

fn regex_parse_group_name(
  pattern: &str,
  start: usize,
) -> Result<(usize /* index after `>` */, String /* name */), RegexError> {
  let bytes = pattern.as_bytes();
  let mut i = start;
  if i >= bytes.len() {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: start,
      len: 0,
    });
  }

  let mut name = String::new();

  // Parse the first IdentifierStart.
  let ch = pattern[i..].chars().next().unwrap();
  if ch == '>' {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: start,
      len: 1,
    });
  }
  if ch == '\\' {
    let (c, consumed) = regex_parse_unicode_escape_in_identifier(pattern, i)?;
    if !regex_is_identifier_start(c) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: i,
        len: consumed,
      });
    }
    name.push(c);
    i += consumed;
  } else if regex_is_identifier_start(ch) {
    name.push(ch);
    i += ch.len_utf8();
  } else {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: i,
      len: ch.len_utf8(),
    });
  }

  // Parse IdentifierContinue until `>`.
  loop {
    if i >= bytes.len() {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: start,
        len: i.saturating_sub(start),
      });
    }
    let ch = pattern[i..].chars().next().unwrap();
    if ch == '>' {
      return Ok((i + ch.len_utf8(), name));
    }
    if ch == '\\' {
      let (c, consumed) = regex_parse_unicode_escape_in_identifier(pattern, i)?;
      if !regex_is_identifier_continue(c) {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: i,
          len: consumed,
        });
      }
      name.push(c);
      i += consumed;
    } else {
      if !regex_is_identifier_continue(ch) {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: i,
          len: ch.len_utf8(),
        });
      }
      name.push(ch);
      i += ch.len_utf8();
    }
  }
}

fn regex_group_prefix_info(
  pattern: &str,
  start: usize,
  unicode_mode: bool,
) -> Result<(bool /*quantifiable*/, usize /*consumed*/, Option<String>), RegexError> {
  debug_assert_eq!(pattern.as_bytes()[start], b'(');
  let bytes = pattern.as_bytes();
  if start + 1 >= bytes.len() || bytes[start + 1] != b'?' {
    // Capturing group.
    return Ok((true, 1, None));
  }

  if start + 2 >= bytes.len() {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: start,
      len: 1,
    });
  }

  match bytes[start + 2] {
    b':' => Ok((true, 3, None)), // (?:...) non-capturing group
    // In non-unicode mode, Annex B allows quantifying lookahead groups. Unicode mode (`u`/`v`)
    // uses the stricter grammar where lookaheads are not quantifiable.
    b'=' | b'!' => Ok((!unicode_mode, 3, None)), // (?=...) / (?!...) lookahead assertions
    b'<' => {
      if start + 3 >= bytes.len() {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: start,
          len: 1,
        });
      }
      match bytes[start + 3] {
        // Annex B only permits quantifying lookahead assertions; lookbehind assertions are never
        // quantifiable in ECMAScript regular expressions.
        b'=' | b'!' => Ok((false, 4, None)), // (?<=...) / (?<!...) lookbehind assertions
        _ => {
          // Named capturing group: (?<name>...)
          let after_prefix = start + 3;
          let (end, name) = regex_parse_group_name(pattern, after_prefix)?;
          Ok((true, end - start, Some(name)))
        }
      }
    }
    _ => Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: start,
      len: 1,
    }),
  }
}

fn is_regex_syntax_character(ch: char) -> bool {
  matches!(
    ch,
    '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
  )
}

fn is_regex_identity_escape_unicode_mode(ch: char) -> bool {
  is_regex_syntax_character(ch) || ch == '/'
}

#[derive(Clone, Copy, Debug)]
enum RegexClassAtom {
  /// Denotes exactly one character.
  Single(u32),
  /// Denotes a character class escape / property escape, i.e. does not contain exactly one
  /// character.
  NotSingle,
}

impl RegexClassAtom {
  #[inline]
  fn single(self) -> Option<u32> {
    match self {
      Self::Single(v) => Some(v),
      Self::NotSingle => None,
    }
  }
}

#[inline]
fn regex_hex_value(b: u8) -> Option<u32> {
  match b {
    b'0'..=b'9' => Some((b - b'0') as u32),
    b'a'..=b'f' => Some((b - b'a' + 10) as u32),
    b'A'..=b'F' => Some((b - b'A' + 10) as u32),
    _ => None,
  }
}

fn validate_regex_pattern(
  pattern: &str,
  base_offset: usize,
  unicode_mode: bool,
  unicode_sets_mode: bool,
  has_named_groups: bool,
  capture_groups: usize,
) -> Result<(), RegexError> {
  let bytes = pattern.as_bytes();
  let mut i = 0usize;
  let mut in_charset = false;
  let mut charset_first = false;
  let mut charset_negated = false;
  let mut charset_prev_atom: Option<RegexClassAtom> = None;
  let mut group_stack: Vec<bool> = Vec::new();
  let mut prev_can_be_quantified = false;
  // Whether the immediately preceding token was a quantifier that can accept a `?` non-greedy
  // modifier.
  let mut quantifier_allows_lazy = false;
  let mut named_capture_groups: HashMap<String, Vec<Vec<(u32, u32)>>> = HashMap::new();
  let mut named_backreferences: Vec<(String, usize /* offset */, usize /* len */)> = Vec::new();
  // Track alternation branches so we can approximate `DuplicateNamedCapturingGroups` (ECMA-262).
  // Duplicate named groups are only allowed when they cannot both participate in the same match
  // result (i.e. they are in disjoint alternation branches).
  let mut alt_stack: Vec<(u32 /* frame id */, u32 /* branch */)> = vec![(0, 0)];
  let mut next_alt_frame_id: u32 = 1;

  fn signatures_mutually_exclusive(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    let len = a.len().min(b.len());
    for idx in 0..len {
      if a[idx].0 != b[idx].0 {
        break;
      }
      if a[idx].1 != b[idx].1 {
        return true;
      }
    }
    false
  }

  fn unicode_property_of_strings(name: &str) -> bool {
    super::regex_unicode_property::is_unicode_property_of_strings(name)
  }

  fn validate_unicode_property_escape(
    pattern: &str,
    base_offset: usize,
    escape_start: usize,
    p_index: usize,
    unicode_sets_mode: bool,
  ) -> Result<(usize, bool), RegexError> {
    debug_assert_eq!(pattern.as_bytes()[escape_start], b'\\');
    let esc = pattern[p_index..].chars().next().unwrap();
    debug_assert!(esc == 'p' || esc == 'P');
    let esc_len = esc.len_utf8();
    let brace_start = p_index + esc_len;
    let bytes = pattern.as_bytes();
    if brace_start >= bytes.len() || bytes[brace_start] != b'{' {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: brace_start.saturating_sub(escape_start),
      });
    }

    let mut j = brace_start + 1;
    let mut seen_eq = false;
    while j < bytes.len() && bytes[j] != b'}' {
      let b = bytes[j];
      if b == b'=' {
        if seen_eq {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: j + 1 - escape_start,
          });
        }
        seen_eq = true;
        j += 1;
        continue;
      }
      let c = b as char;
      if !(c.is_ascii_alphanumeric() || c == '_') {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: j + 1 - escape_start,
        });
      }
      j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'}' || j == brace_start + 1 {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: j.saturating_sub(escape_start),
      });
    }

    let content = &pattern[brace_start + 1..j];
    if !super::regex_unicode_property::validate_unicode_property_value_expression(
      content,
      unicode_sets_mode,
    ) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: j + 1 - escape_start,
      });
    }
    let prop_name = content.split('=').next().unwrap_or("");
    let is_strings_prop = unicode_property_of_strings(prop_name);
    if is_strings_prop {
      if esc == 'P' {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: j + 1 - escape_start,
        });
      }
      if !unicode_sets_mode {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: j + 1 - escape_start,
        });
      }
      return Ok((j + 1, true));
    }

    Ok((j + 1, false))
  }

  fn validate_class_string_disjunction_escape(
    pattern: &str,
    base_offset: usize,
    escape_start: usize,
    q_index: usize,
  ) -> Result<(usize, bool), RegexError> {
    let esc = pattern[q_index..].chars().next().unwrap();
    debug_assert_eq!(esc, 'q');
    let esc_len = esc.len_utf8();
    let brace_start = q_index + esc_len;
    let bytes = pattern.as_bytes();
    if brace_start >= bytes.len() || bytes[brace_start] != b'{' {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: brace_start.saturating_sub(escape_start),
      });
    }
    // In UnicodeSetsMode, `\q{...}` yields a disjunction of `ClassString`s. Track whether any
    // alternative contains something other than exactly one `ClassSetCharacter` (including the empty
    // string), which corresponds to `MayContainStrings` in the spec.
    let mut j = brace_start + 1;
    let mut segment_characters: usize = 0;
    let mut may_contain_strings = false;
    while j < bytes.len() {
      let b = bytes[j];
      if b == b'}' {
        if segment_characters != 1 {
          may_contain_strings = true;
        }
        return Ok((j + 1, may_contain_strings));
      }
      if b == b'|' {
        if segment_characters != 1 {
          may_contain_strings = true;
        }
        segment_characters = 0;
        j += 1;
        continue;
      }
      if b == b'{' {
        // `{` is a ClassSetSyntaxCharacter and may only appear as the opening delimiter for
        // `\q{...}`. Inside the disjunction it must be escaped.
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + j,
          len: 1,
        });
      }
      if b == b'\\' {
        // Validate escapes inside the disjunction using the UnicodeMode escape grammar.
        let esc_start = j;
        j += 1;
        if j >= bytes.len() {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: esc_start + 1 - escape_start,
          });
        }
        let esc = pattern[j..].chars().next().unwrap();
        let esc_len = esc.len_utf8();
        match esc {
          'u' => {
            let after_u = j + esc_len;
            if after_u >= bytes.len() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: after_u.saturating_sub(escape_start),
              });
            }
            if bytes[after_u] == b'{' {
              let mut k = after_u + 1;
              let mut digits = 0usize;
              let mut value: u32 = 0;
              while k < bytes.len() && bytes[k] != b'}' {
                let b = bytes[k];
                let d = match b {
                  b'0'..=b'9' => (b - b'0') as u32,
                  b'a'..=b'f' => (b - b'a' + 10) as u32,
                  b'A'..=b'F' => (b - b'A' + 10) as u32,
                  _ => {
                    return Err(RegexError {
                      kind: RegexErrorKind::InvalidPattern,
                      offset: base_offset + escape_start,
                      len: k + 1 - escape_start,
                    });
                  }
                };
                digits += 1;
                value = value.saturating_mul(16).saturating_add(d);
                if value > 0x10FFFF {
                  return Err(RegexError {
                    kind: RegexErrorKind::InvalidPattern,
                    offset: base_offset + escape_start,
                    len: k + 1 - escape_start,
                  });
                }
                k += 1;
              }
              if k >= bytes.len() || digits == 0 {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: k.saturating_sub(escape_start),
                });
              }
              j = k + 1;
              segment_characters += 1;
              continue;
            }
            // `\uXXXX` (optionally followed by a second `\uXXXX` to form a surrogate pair).
            let mut k = after_u;
            let mut v: u32 = 0;
            for _ in 0..4 {
              if k >= bytes.len() || !(bytes[k] as char).is_ascii_hexdigit() {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: k.saturating_sub(escape_start),
                });
              }
              v = (v << 4) | (bytes[k] as char).to_digit(16).unwrap();
              k += 1;
            }
            if (0xD800..=0xDBFF).contains(&v)
              && k + 6 <= bytes.len()
              && bytes[k] == b'\\'
              && bytes[k + 1] == b'u'
            {
              let mut k2 = k + 2;
              let mut low: u32 = 0;
              let mut ok = true;
              for _ in 0..4 {
                if k2 >= bytes.len() || !(bytes[k2] as char).is_ascii_hexdigit() {
                  ok = false;
                  break;
                }
                low = (low << 4) | (bytes[k2] as char).to_digit(16).unwrap();
                k2 += 1;
              }
              if ok && (0xDC00..=0xDFFF).contains(&low) {
                k = k2;
              }
            }
            j = k;
            segment_characters += 1;
            continue;
          }
          'x' => {
            let after_x = j + esc_len;
            let mut k = after_x;
            for _ in 0..2 {
              if k >= bytes.len() || !(bytes[k] as char).is_ascii_hexdigit() {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: k.saturating_sub(escape_start),
                });
              }
              k += 1;
            }
            j = k;
            segment_characters += 1;
            continue;
          }
          // ControlEscape (CharacterEscape)
          'f' | 'n' | 'r' | 't' | 'v' => {
            j += esc_len;
            segment_characters += 1;
            continue;
          }
          // `\b` is explicitly allowed in ClassSetCharacter.
          'b' => {
            j += esc_len;
            segment_characters += 1;
            continue;
          }
          // `\0` (null escape) is allowed only when not followed by a decimal digit.
          '0' => {
            let after = j + esc_len;
            let next_is_digit = after < pattern.len()
              && pattern[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit());
            if next_is_digit {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: 2 + esc_len,
              });
            }
            j += esc_len;
            segment_characters += 1;
            continue;
          }
          // `\c` AsciiLetter
          'c' => {
            let after_c = j + esc_len;
            if after_c >= pattern.len() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: pattern.len().saturating_sub(escape_start),
              });
            }
            let ctrl = pattern[after_c..].chars().next().unwrap();
            if !ctrl.is_ascii_alphabetic() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: after_c + ctrl.len_utf8() - escape_start,
              });
            }
            j = after_c + ctrl.len_utf8();
            segment_characters += 1;
            continue;
          }
          // ClassSetReservedPunctuator
          '&' | '-' | '!' | '#' | '%' | ',' | ':' | ';' | '<' | '=' | '>' | '@' | '`' | '~' => {
            j += esc_len;
            segment_characters += 1;
            continue;
          }
          // IdentityEscape[+UnicodeMode]
          _ if is_regex_identity_escape_unicode_mode(esc) => {
            j += esc_len;
            segment_characters += 1;
            continue;
          }
          _ => {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: esc_start + 1 - escape_start,
            });
          }
        }
      }
      // Any other source character inside the disjunction counts as one `ClassSetCharacter`.
      if j + 1 < bytes.len() && bytes[j] == bytes[j + 1] {
        // ClassSetCharacter disallows SourceCharacters that begin a ClassSetReservedDoublePunctuator.
        // These can still be expressed by escaping each punctuator (e.g. `\&\&`).
        if matches!(
          bytes[j],
          b'&'
            | b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'*'
            | b'+'
            | b','
            | b'.'
            | b':'
            | b';'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'@'
            | b'^'
            | b'`'
            | b'~'
        ) {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + j,
            len: 2,
          });
        }
      }
      let ch = pattern[j..].chars().next().unwrap();
      match ch {
        '(' | ')' | '[' | ']' | '/' | '-' => {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + j,
            len: ch.len_utf8(),
          });
        }
        _ => {}
      }
      segment_characters += 1;
      j += ch.len_utf8();
    }
    Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: base_offset + escape_start,
      len: pattern.len().saturating_sub(escape_start),
    })
  }

  fn validate_unicode_sets_class(
    pattern: &str,
    base_offset: usize,
    start: usize,
    unicode_mode: bool,
    unicode_sets_mode: bool,
  ) -> Result<(usize, bool), RegexError> {
    debug_assert_eq!(pattern.as_bytes()[start], b'[');
    let bytes = pattern.as_bytes();
    let mut i = start + 1;
    let mut this_negated = false;
    if i < bytes.len() && bytes[i] == b'^' {
      this_negated = true;
      i += 1;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PrevToken {
      None,
      Operand { range_value: Option<u32> },
      Operator,
    }

    fn reserved_double_punctuator(rest: &str) -> bool {
      matches!(
        rest.as_bytes().get(0..2),
        Some(b"!!")
          | Some(b"##")
          | Some(b"$$")
          | Some(b"%%")
          | Some(b"**")
          | Some(b"++")
          | Some(b",,")
          | Some(b"..")
          | Some(b"::")
          | Some(b";;")
          | Some(b"<<")
          | Some(b"==")
          | Some(b">>")
          | Some(b"??")
          | Some(b"@@")
          | Some(b"^^")
          | Some(b"``")
          | Some(b"~~")
      )
    }

    fn parse_operand(
      pattern: &str,
      base_offset: usize,
      mut i: usize,
      unicode_mode: bool,
      unicode_sets_mode: bool,
    ) -> Result<(usize, Option<u32>, bool), RegexError> {
      let ch = pattern[i..].chars().next().unwrap();
      if ch == '\\' {
        let escape_start = i;
        i += 1;
        if i >= pattern.len() {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: 1,
          });
        }
        let esc = pattern[i..].chars().next().unwrap();
        let esc_len = esc.len_utf8();
        if unicode_mode && matches!(esc, 'p' | 'P') {
          let (end, may_contain_strings) = validate_unicode_property_escape(
            pattern,
            base_offset,
            escape_start,
            i,
            unicode_sets_mode,
          )?;
          return Ok((end, None, may_contain_strings));
        }
        if unicode_sets_mode && esc == 'q' {
          let (end, may_contain_strings) = validate_class_string_disjunction_escape(
            pattern,
            base_offset,
            escape_start,
            i,
          )?;
          return Ok((end, None, may_contain_strings));
        }
        if esc == 'u' {
          let after_u = i + esc_len;
          let bytes = pattern.as_bytes();
          if after_u >= bytes.len() {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1,
            });
          }
          if bytes[after_u] == b'{' {
            let mut j = after_u + 1;
            let mut saw_digit = false;
            // Overflow-safe numeric parse with arbitrary leading zeros.
            // `\u{...}` escapes are only valid up to 0x10FFFF.
            let mut started = false;
            let mut significant_digits: usize = 0;
            let mut value: u32 = 0;
            while j < bytes.len() && bytes[j] != b'}' {
              let b = bytes[j];
              if !(b as char).is_ascii_hexdigit() {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: j + 1 - escape_start,
                });
              }
              saw_digit = true;
              let digit: u32 = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => unreachable!(),
              };

              if !started {
                if digit != 0 {
                  started = true;
                  significant_digits = 1;
                  value = digit;
                }
              } else {
                significant_digits += 1;
                // 0x10FFFF fits in 6 hex digits; any additional significant digit is definitely out
                // of range.
                if significant_digits > 6 {
                  return Err(RegexError {
                    kind: RegexErrorKind::InvalidPattern,
                    offset: base_offset + escape_start,
                    len: j + 1 - escape_start,
                  });
                }
                value = (value << 4) | digit;
                if value > 0x10FFFF {
                  return Err(RegexError {
                    kind: RegexErrorKind::InvalidPattern,
                    offset: base_offset + escape_start,
                    len: j + 1 - escape_start,
                  });
                }
              }
              j += 1;
            }
            // Require at least one hex digit and a closing `}`.
            if j >= bytes.len() || !saw_digit {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j.saturating_sub(escape_start),
              });
            }
            return Ok((j + 1, Some(value), false));
          }
          // `\uXXXX`
          let mut j = after_u;
          let mut value: u32 = 0;
          for _ in 0..4 {
            if j >= bytes.len() || !(bytes[j] as char).is_ascii_hexdigit() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j.saturating_sub(escape_start),
              });
            }
            value = (value << 4) + regex_hex_value(bytes[j]).unwrap();
            j += 1;
          }
          // In UnicodeMode, a `\u` escape that yields a leading surrogate may be followed by
          // another `\u` escape yielding a trailing surrogate; treat the pair as a single
          // code point (e.g. `\uD83D\uDE00` → U+1F600).
          if (0xD800..=0xDBFF).contains(&value)
            && j + 6 <= bytes.len()
            && bytes[j] == b'\\'
            && bytes[j + 1] == b'u'
          {
            let mut k = j + 2;
            let mut low: u32 = 0;
            let mut ok = true;
            for _ in 0..4 {
              let Some(digit) = regex_hex_value(bytes[k]) else {
                ok = false;
                break;
              };
              low = (low << 4) | digit;
              k += 1;
            }
            if ok && (0xDC00..=0xDFFF).contains(&low) {
              value = 0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00);
              j = k;
            }
          }
          return Ok((j, Some(value), false));
        }
        if esc == 'x' {
          let after_x = i + esc_len;
          let bytes = pattern.as_bytes();
          let mut j = after_x;
          let mut value: u32 = 0;
          for _ in 0..2 {
            if j >= bytes.len() || !(bytes[j] as char).is_ascii_hexdigit() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j.saturating_sub(escape_start),
              });
            }
            value = (value << 4) + regex_hex_value(bytes[j]).unwrap();
            j += 1;
          }
          return Ok((j, Some(value), false));
        }
        if unicode_mode {
          // UnicodeMode (`/v`) uses the strict escape grammar.

          // `\0` is only valid when not followed by a decimal digit.
          if esc.is_ascii_digit() {
            if esc == '0' {
              let after = i + esc_len;
              let next_is_digit = after < pattern.len()
                && pattern[after..]
                  .chars()
                  .next()
                  .is_some_and(|c| c.is_ascii_digit());
              if next_is_digit {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: 2 + esc_len,
                });
              }
              return Ok((i + esc_len, Some(0), false));
            }
            // Decimal escapes/backreferences are not valid inside character classes.
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }

          // ControlEscape
          if matches!(esc, 'f' | 'n' | 'r' | 't' | 'v') {
            let value = match esc {
              'f' => 0x0C,
              'n' => 0x0A,
              'r' => 0x0D,
              't' => 0x09,
              'v' => 0x0B,
              _ => unreachable!(),
            };
            return Ok((i + esc_len, Some(value), false));
          }

          // `\c` AsciiLetter
          if esc == 'c' {
            let after_c = i + esc_len;
            if after_c >= pattern.len() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: pattern.len().saturating_sub(escape_start),
              });
            }
            let ctrl = pattern[after_c..].chars().next().unwrap();
            if !ctrl.is_ascii_alphabetic() {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: after_c + ctrl.len_utf8() - escape_start,
              });
            }
            let value = (ctrl.to_ascii_uppercase() as u8 & 0x1F) as u32;
            return Ok((after_c + ctrl.len_utf8(), Some(value), false));
          }

          // `\b` is backspace inside character classes.
          if esc == 'b' {
            return Ok((i + esc_len, Some(0x08), false));
          }

          // `\B` and `\k<name>` are not valid inside character classes.
          if esc == 'B' || esc == 'k' {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }

          // `\-` is a ClassEscape in UnicodeMode.
          if esc == '-' {
            return Ok((i + esc_len, Some('-' as u32), false));
          }
 
          // ClassSetReservedPunctuator escapes are only valid in UnicodeSets mode (`/v`) inside a
          // class set. They allow representing punctuators that would otherwise form a reserved
          // double punctuator/operator (e.g. `[\&\&]`, `[\!\!]`).
          if unicode_sets_mode
            && matches!(
              esc,
              '&' | '!' | '#' | '%' | ',' | ':' | ';' | '<' | '=' | '>' | '@' | '`' | '~'
            )
          {
            return Ok((i + esc_len, Some(esc as u32), false));
          }

          // CharacterClassEscape (not valid as range endpoint).
          if matches!(esc, 'd' | 'D' | 's' | 'S' | 'w' | 'W') {
            return Ok((i + esc_len, None, false));
          }

          // IdentityEscape[+UnicodeMode] is limited to SyntaxCharacter or `/`.
          if !is_regex_identity_escape_unicode_mode(esc) {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }
          return Ok((i + esc_len, Some(esc as u32), false));
        }

        // Non-UnicodeMode: Escapes that represent character classes are not valid range endpoints.
        if matches!(esc, 'd' | 'D' | 's' | 'S' | 'w' | 'W') {
          return Ok((i + esc_len, None, false));
        }
        Ok((i + esc_len, Some(esc as u32), false))
      } else if ch == '[' {
        let (end, may_contain_strings) = validate_unicode_sets_class(
          pattern,
          base_offset,
          i,
          unicode_mode,
          unicode_sets_mode,
        )?;
        Ok((end, None, may_contain_strings))
      } else {
        // Disallowed syntax characters in UnicodeSets mode when unescaped.
        match ch {
          '(' | '{' | '}' | '/' | '|' | ')' | ']' => {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: ch.len_utf8(),
            });
          }
          '-' => {
            // A bare `-` is neither a valid operand nor a valid range (it is parsed by the caller).
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          }
          _ => {}
        }
        Ok((i + ch.len_utf8(), Some(ch as u32), false))
      }
    }

    fn validate_unicode_sets_expression(
      pattern: &str,
      base_offset: usize,
      mut i: usize,
      unicode_mode: bool,
      unicode_sets_mode: bool,
      terminator: char,
    ) -> Result<(usize, bool, bool), RegexError> {
      let mut prev = PrevToken::None;
      #[derive(Clone, Copy, PartialEq, Eq)]
      enum Mode {
        Unknown,
        Union,
        Intersection,
        Subtraction,
      }
      let mut mode = Mode::Unknown;
      let mut saw_operand = false;
      // Compute `MayContainStrings` for the leftmost disjunction operand. Subtraction (`--`) cannot
      // introduce strings, so once we encounter `--` we can stop updating the result.
      let mut computing_may = true;
      let mut union_may = false;
      let mut intersection_may = true;
      let mut may_result: Option<bool> = None;
      while i < pattern.len() {
        let rest = &pattern[i..];
        let ch = rest.chars().next().unwrap();
        if ch == terminator {
          if prev == PrevToken::Operator {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          }
          let may_contain_strings = if saw_operand {
            if may_result.is_none() {
              if computing_may {
                intersection_may &= union_may;
              }
              may_result = Some(intersection_may);
            }
            may_result.unwrap_or(false)
          } else {
            false
          };
          return Ok((i + ch.len_utf8(), saw_operand, may_contain_strings));
        }

        if rest.starts_with("&&") {
          match mode {
            Mode::Unknown => mode = Mode::Intersection,
            Mode::Intersection => {}
            _ => {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + i,
                len: 2,
              });
            }
          }
          if !matches!(prev, PrevToken::Operand { .. }) {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 2,
            });
          }
          // `&&` is the intersection operator and must not be followed by another `&`
          // (see the grammar lookahead restriction).
          if rest.as_bytes().get(2) == Some(&b'&') {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 2,
            });
          }
          if computing_may {
            intersection_may &= union_may;
            union_may = false;
          }
          prev = PrevToken::Operator;
          i += 2;
          continue;
        }

        if rest.starts_with("--") {
          match mode {
            Mode::Unknown => mode = Mode::Subtraction,
            Mode::Subtraction => {}
            _ => {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + i,
                len: 2,
              });
            }
          }
          if !matches!(prev, PrevToken::Operand { .. }) {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 2,
            });
          }
          if computing_may {
            intersection_may &= union_may;
            union_may = false;
            if may_result.is_none() {
              may_result = Some(intersection_may);
              computing_may = false;
            }
          }
          prev = PrevToken::Operator;
          i += 2;
          continue;
        }

        if reserved_double_punctuator(rest) {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + i,
            len: 2,
          });
        }

        if ch == '-' {
          // Ranges (`a-b`) are only valid in `ClassUnion` expressions.
          if matches!(mode, Mode::Intersection | Mode::Subtraction) {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          }
          // Single `-` can only appear as a range marker.
          let PrevToken::Operand {
            range_value: Some(lhs_value),
          } = prev
          else {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          };
          let (end, rhs_value, rhs_may) =
            parse_operand(pattern, base_offset, i + 1, unicode_mode, unicode_sets_mode)?;
          let Some(rhs_value) = rhs_value else {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          };
          if lhs_value > rhs_value {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + i,
              len: 1,
            });
          }
          if computing_may {
            // Range endpoints must be ClassSetCharacters (i.e., they cannot contain strings).
            union_may |= rhs_may;
          }
          i = end;
          prev = PrevToken::Operand { range_value: None };
          mode = Mode::Union;
          saw_operand = true;
          continue;
        }

        // Parse an operand.
        if matches!(prev, PrevToken::Operand { .. }) {
          // Adjacency forms a `ClassUnion`. Once we enter intersection/subtraction mode, union
          // operands must be wrapped in a NestedClass (`[...]`).
          match mode {
            Mode::Unknown => mode = Mode::Union,
            Mode::Union => {}
            _ => {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + i,
                len: ch.len_utf8(),
              });
            }
          }
        }
        let (end, range_value, operand_may) =
          parse_operand(pattern, base_offset, i, unicode_mode, unicode_sets_mode)?;
        if computing_may {
          union_may |= operand_may;
        }
        i = end;
        prev = PrevToken::Operand { range_value };
        saw_operand = true;
      }

      Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + pattern.len(),
        len: 0,
      })
    }

    let (end, _saw_operand, may_contain_strings) = validate_unicode_sets_expression(
      pattern,
      base_offset,
      i,
      unicode_mode,
      unicode_sets_mode,
      ']',
    )?;

    // Early Error: a negated class may not contain strings.
    if this_negated && may_contain_strings {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + start,
        len: 1,
      });
    }

    let class_may_contain_strings = if this_negated {
      false
    } else {
      may_contain_strings
    };

    Ok((end, class_may_contain_strings))
  }

  fn parse_legacy_class_atom(
    pattern: &str,
    base_offset: usize,
    start: usize,
    unicode_mode: bool,
    unicode_sets_mode: bool,
    has_named_groups: bool,
    _in_negated_class: bool,
  ) -> Result<(usize, RegexClassAtom), RegexError> {
    let ch = pattern[start..].chars().next().unwrap();
    if ch != '\\' {
      return Ok((start + ch.len_utf8(), RegexClassAtom::Single(ch as u32)));
    }
    let escape_start = start;
    let after_backslash = start + '\\'.len_utf8();
    if after_backslash >= pattern.len() {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: 1,
      });
    }

    let esc = pattern[after_backslash..].chars().next().unwrap();
    let esc_len = esc.len_utf8();
    let after_esc = after_backslash + esc_len;
    let bytes = pattern.as_bytes();

    if unicode_mode && matches!(esc, 'p' | 'P') {
      let (end, _) = validate_unicode_property_escape(
        pattern,
        base_offset,
        escape_start,
        after_backslash,
        unicode_sets_mode,
      )?;
      return Ok((end, RegexClassAtom::NotSingle));
    }

    // `\k<name>` is not valid inside character classes.
    if esc == 'k' && (unicode_mode || has_named_groups) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: after_esc.saturating_sub(escape_start),
      });
    }

    if esc == 'u' {
      let after_u = after_esc;
      if unicode_mode {
        if after_u >= bytes.len() {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: after_u.saturating_sub(escape_start),
          });
        }
        if bytes[after_u] == b'{' {
          let mut j = after_u + 1;
          let mut saw_digit = false;
          let mut value: u32 = 0;
          while j < bytes.len() && bytes[j] != b'}' {
            let Some(digit) = regex_hex_value(bytes[j]) else {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j + 1 - escape_start,
              });
            };
            saw_digit = true;
            if value > (0x10FFFFu32 - digit) / 16 {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j + 1 - escape_start,
              });
            }
            value = value * 16 + digit;
            j += 1;
          }
          if j >= bytes.len() || !saw_digit {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: j.saturating_sub(escape_start),
            });
          }
          debug_assert_eq!(bytes[j], b'}');
          return Ok((j + 1, RegexClassAtom::Single(value)));
        }
        // `\uXXXX`
        let mut j = after_u;
        let mut value: u32 = 0;
        for _ in 0..4 {
          if j >= bytes.len() {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: j.saturating_sub(escape_start),
            });
          }
          let Some(digit) = regex_hex_value(bytes[j]) else {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: j.saturating_sub(escape_start),
            });
          };
          value = (value << 4) | digit;
          j += 1;
        }
        // In UnicodeMode, a `\u` escape that yields a leading surrogate may be followed by another
        // `\u` escape yielding a trailing surrogate; treat the pair as a single code point.
        if (0xD800..=0xDBFF).contains(&value)
          && j + 6 <= bytes.len()
          && bytes[j] == b'\\'
          && bytes[j + 1] == b'u'
        {
          let mut k = j + 2;
          let mut low: u32 = 0;
          let mut ok = true;
          for _ in 0..4 {
            let Some(digit) = regex_hex_value(bytes[k]) else {
              ok = false;
              break;
            };
            low = (low << 4) | digit;
            k += 1;
          }
          if ok && (0xDC00..=0xDFFF).contains(&low) {
            value = 0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00);
            j = k;
          }
        }
        return Ok((j, RegexClassAtom::Single(value)));
      }

      // Non-UnicodeMode: treat `\uXXXX` as a Unicode escape only when exactly 4 hex digits follow.
      if after_u + 4 <= bytes.len()
        && bytes[after_u..after_u + 4]
          .iter()
          .all(|b| regex_hex_value(*b).is_some())
      {
        let mut value: u32 = 0;
        for b in &bytes[after_u..after_u + 4] {
          value = (value << 4) | regex_hex_value(*b).unwrap();
        }
        return Ok((after_u + 4, RegexClassAtom::Single(value)));
      }
      return Ok((after_esc, RegexClassAtom::Single(esc as u32)));
    }

    if esc == 'x' {
      let after_x = after_esc;
      if after_x + 2 <= bytes.len() {
        if let (Some(h1), Some(h2)) = (regex_hex_value(bytes[after_x]), regex_hex_value(bytes[after_x + 1]))
        {
          return Ok((after_x + 2, RegexClassAtom::Single((h1 << 4) | h2)));
        }
      }
      if unicode_mode {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: after_x.saturating_sub(escape_start),
        });
      }
      return Ok((after_esc, RegexClassAtom::Single(esc as u32)));
    }

    // Digit escapes are disallowed in UnicodeMode character classes (except `\0`).
    if esc.is_ascii_digit() {
      if unicode_mode {
        if esc == '0' {
          let next_is_digit = after_esc < pattern.len()
            && pattern[after_esc..]
              .chars()
              .next()
              .is_some_and(|c| c.is_ascii_digit());
          if next_is_digit {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 2 + esc_len,
            });
          }
          return Ok((after_esc, RegexClassAtom::Single(0)));
        }
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: 1 + esc_len,
        });
      }

      // Non-UnicodeMode: treat octal escapes (`\0`..`\777`) as a single character.
      if ('0'..='7').contains(&esc) {
        let mut j = after_backslash;
        let mut value: u32 = 0;
        for _ in 0..3 {
          if j >= pattern.len() {
            break;
          }
          let c = pattern[j..].chars().next().unwrap();
          if !('0'..='7').contains(&c) {
            break;
          }
          value = (value << 3) + ((c as u8 - b'0') as u32);
          j += c.len_utf8();
        }
        return Ok((j, RegexClassAtom::Single(value)));
      }
      // `\8`/`\9` are treated as identity escapes in non-UnicodeMode.
      return Ok((after_esc, RegexClassAtom::Single(esc as u32)));
    }

    // ControlEscape.
    match esc {
      'f' => return Ok((after_esc, RegexClassAtom::Single(0x0c))),
      'n' => return Ok((after_esc, RegexClassAtom::Single(0x0a))),
      'r' => return Ok((after_esc, RegexClassAtom::Single(0x0d))),
      't' => return Ok((after_esc, RegexClassAtom::Single(0x09))),
      'v' => return Ok((after_esc, RegexClassAtom::Single(0x0b))),
      _ => {}
    }

    // `\c` AsciiLetter / Annex B extensions.
    if esc == 'c' {
      if after_esc >= pattern.len() {
        // In UnicodeMode, `\c` must be followed by an ASCII letter (and we already know no
        // character follows).
        if unicode_mode {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: pattern.len().saturating_sub(escape_start),
          });
        }
        // Non-UnicodeMode: Annex B permits treating an incomplete control escape as literal pattern
        // characters (a literal backslash followed by `c`).
        return Ok((after_backslash, RegexClassAtom::Single('\\' as u32)));
      }
      let ctrl = pattern[after_esc..].chars().next().unwrap();
      if ctrl.is_ascii_alphabetic() {
        let upper = ctrl.to_ascii_uppercase();
        let value = (upper as u32).saturating_sub(0x40);
        return Ok((after_esc + ctrl.len_utf8(), RegexClassAtom::Single(value)));
      }
      if !unicode_mode && (ctrl.is_ascii_digit() || ctrl == '_') {
        // Annex B ClassEscape extension: `\c` ClassControlLetter.
        let value = (ctrl as u32) % 32;
        return Ok((after_esc + ctrl.len_utf8(), RegexClassAtom::Single(value)));
      }
      if !unicode_mode {
        // Non-UnicodeMode: Annex B permits treating invalid `\c` sequences as literal pattern
        // characters (a literal backslash followed by `c`).
        return Ok((after_backslash, RegexClassAtom::Single('\\' as u32)));
      }
      // UnicodeMode: `\c` is only valid when followed by an ASCII letter.
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: after_esc + ctrl.len_utf8() - escape_start,
      });
    }

    // `\b` is backspace inside character classes.
    if esc == 'b' {
      return Ok((after_esc, RegexClassAtom::Single(0x08)));
    }

    // `\-` is a ClassEscape in UnicodeMode.
    if esc == '-' {
      return Ok((after_esc, RegexClassAtom::Single('-' as u32)));
    }

    // CharacterClassEscape.
    if matches!(esc, 'd' | 'D' | 's' | 'S' | 'w' | 'W') {
      return Ok((after_esc, RegexClassAtom::NotSingle));
    }

    if unicode_mode && !is_regex_identity_escape_unicode_mode(esc) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset: base_offset + escape_start,
        len: 1 + esc_len,
      });
    }

    Ok((after_esc, RegexClassAtom::Single(esc as u32)))
  }

  while i < pattern.len() {
    let ch = pattern[i..].chars().next().unwrap();
    let ch_len = ch.len_utf8();

    if in_charset {
      if ch == ']' {
        in_charset = false;
        charset_first = false;
        charset_negated = false;
        charset_prev_atom = None;
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }

      if charset_first && ch == '^' {
        charset_first = false;
        charset_negated = true;
        i += ch_len;
        continue;
      }

      // Ranges: <ClassAtom> `-` <ClassAtom>
      if ch == '-' && charset_prev_atom.is_some() && i + 1 < bytes.len() && bytes[i + 1] != b']' {
        let left = charset_prev_atom.take().unwrap();
        let dash_index = i;
        let (end, right) = parse_legacy_class_atom(
          pattern,
          base_offset,
          i + 1,
          unicode_mode,
          unicode_sets_mode,
          has_named_groups,
          charset_negated,
        )?;

        if unicode_mode && (left.single().is_none() || right.single().is_none()) {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + dash_index,
            len: 1,
          });
        }
        if let (Some(start), Some(end_cp)) = (left.single(), right.single()) {
          if start > end_cp {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + dash_index,
              len: 1,
            });
          }
        }

        i = end;
        charset_first = false;
        continue;
      }

      let (end, atom) = parse_legacy_class_atom(
        pattern,
        base_offset,
        i,
        unicode_mode,
        unicode_sets_mode,
        has_named_groups,
        charset_negated,
      )?;
      charset_prev_atom = Some(atom);
      i = end;
      charset_first = false;
      continue;
    }

    if unicode_sets_mode && !in_charset && ch == '[' {
      let (end, _) = validate_unicode_sets_class(pattern, base_offset, i, unicode_mode, unicode_sets_mode)?;
      prev_can_be_quantified = true;
      quantifier_allows_lazy = false;
      i = end;
      continue;
    }

    if ch == '\\' {
      let escape_start = i;
      i += '\\'.len_utf8();
      if i >= pattern.len() {
        return Err(RegexError {
          kind: RegexErrorKind::InvalidPattern,
          offset: base_offset + escape_start,
          len: 1,
        });
      }
      let esc = pattern[i..].chars().next().unwrap();
      let esc_len = esc.len_utf8();
      if unicode_mode && matches!(esc, 'p' | 'P') {
        let (end, _) = validate_unicode_property_escape(
          pattern,
          base_offset,
          escape_start,
          i,
          unicode_sets_mode,
        )?;
        if !in_charset {
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
        }
        i = end;
        continue;
      }

      if esc == 'u' {
        let after_u = i + esc_len;
        let bytes = pattern.as_bytes();
        if unicode_mode {
          // In UnicodeMode (`u`/`v`), `\u` must be a valid RegExpUnicodeEscapeSequence.
          if after_u >= bytes.len() {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: after_u.saturating_sub(escape_start),
            });
          }

          if bytes[after_u] == b'{' {
            // `\u{HexDigits}`
            let mut j = after_u + 1;
            let mut saw_digit = false;
            // Track the numeric value but avoid overflow and allow arbitrarily many leading zeros.
            let mut value: u32 = 0;
            let mut significant_digits: usize = 0;
            let mut started = false;
            while j < bytes.len() && bytes[j] != b'}' {
              let b = bytes[j];
              if !(b as char).is_ascii_hexdigit() {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: j + 1 - escape_start,
                });
              }
              saw_digit = true;
              let digit: u32 = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => unreachable!(),
              };

              if !started {
                if digit != 0 {
                  started = true;
                  significant_digits = 1;
                  value = digit;
                }
              } else {
                significant_digits += 1;
                // Max valid value is 0x10FFFF, which fits in 6 hex digits. Any more significant
                // digits are definitely out of range.
                if significant_digits > 6 {
                  return Err(RegexError {
                    kind: RegexErrorKind::InvalidPattern,
                    offset: base_offset + escape_start,
                    len: j + 1 - escape_start,
                  });
                }
                value = (value << 4) | digit;
                if value > 0x10FFFF {
                  return Err(RegexError {
                    kind: RegexErrorKind::InvalidPattern,
                    offset: base_offset + escape_start,
                    len: j + 1 - escape_start,
                  });
                }
              }

              j += 1;
            }

            // Require at least one hex digit and a closing `}`.
            if j >= bytes.len() || !saw_digit {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j.saturating_sub(escape_start),
              });
            }
            debug_assert_eq!(bytes[j], b'}');

            // All-zero escapes are fine (value stays 0).
            if started && value > 0x10FFFF {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: j + 1 - escape_start,
              });
            }

            // Skip the closing `}`.
            i = j + 1;
          } else {
            // `\uXXXX`
            let mut j = after_u;
            for _ in 0..4 {
              if j >= bytes.len() || !(bytes[j] as char).is_ascii_hexdigit() {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + escape_start,
                  len: j.saturating_sub(escape_start),
                });
              }
              j += 1;
            }
            i = j;
          }
        } else {
          // In non-UnicodeMode, only treat `\uXXXX` as a Unicode escape when exactly 4 hex
          // digits follow. Otherwise `\u` is an identity escape and the subsequent characters
          // (including `{...}`) are parsed normally.
          let is_u4 = after_u + 4 <= bytes.len()
            && bytes[after_u..after_u + 4].iter().all(|b| (*b as char).is_ascii_hexdigit());
          if is_u4 {
            i = after_u + 4;
          } else {
            i += esc_len;
          }
        }
        if !in_charset {
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
        }
        continue;
      }

      if esc == 'x' {
        let after_x = i + esc_len;
        let bytes = pattern.as_bytes();
        let is_x2 = after_x + 2 <= bytes.len()
          && (bytes[after_x] as char).is_ascii_hexdigit()
          && (bytes[after_x + 1] as char).is_ascii_hexdigit();
        if is_x2 {
          i = after_x + 2;
        } else if unicode_mode {
          // In UnicodeMode, invalid hex escapes are early errors.
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: after_x.saturating_sub(escape_start),
          });
        } else {
          // In non-UnicodeMode, invalid `\x` escapes are treated as identity escapes.
          i += esc_len;
        }
        if !in_charset {
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
        }
        continue;
      }

      if esc == 'k' {
        let after_k = i + esc_len;
        let should_treat_k_as_named_ref = unicode_mode || has_named_groups;
        if should_treat_k_as_named_ref {
          if in_charset {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: after_k.saturating_sub(escape_start),
            });
          }
          if after_k >= pattern.len() || !pattern[after_k..].starts_with('<') {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: after_k.saturating_sub(escape_start),
            });
          }
          let after_angle = after_k + '<'.len_utf8();
          let (end, name) =
            regex_parse_group_name(pattern, after_angle).map_err(|mut err| {
              err.offset += base_offset;
              err
            })?;
          named_backreferences.push((name, base_offset + escape_start, end - escape_start));

          // Skip until the end of the `\k<name>` sequence.
          i = end;
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
          continue;
        }
      }

      if !in_charset && esc.is_ascii_digit() {
        if esc == '0' {
          let after = i + esc_len;
          let next_is_digit = after < pattern.len()
            && pattern[after..]
              .chars()
              .next()
              .is_some_and(|c| c.is_ascii_digit());
          if unicode_mode && next_is_digit {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 2 + esc_len,
            });
          }
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
          i += esc_len;
          continue;
        }

        let mut j = i;
        let mut value: usize = 0;
        while j < pattern.len() {
          let c = pattern[j..].chars().next().unwrap();
          if !c.is_ascii_digit() {
            break;
          }
          value = value.saturating_mul(10).saturating_add((c as u8 - b'0') as usize);
          j += c.len_utf8();
        }

        if unicode_mode && value > capture_groups {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: j - escape_start,
          });
        }

        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        i = j;
        continue;
      }

      // `\c` AsciiLetter / Annex B class-control escape / Annex B extended atom.
      if esc == 'c' {
        let after_c = i + esc_len;
        let control = pattern[after_c..].chars().next();
        if let Some(control) = control {
          if control.is_ascii_alphabetic() {
            i = after_c + control.len_utf8();
            prev_can_be_quantified = true;
            quantifier_allows_lazy = false;
            continue;
          }
          if unicode_mode {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: after_c + control.len_utf8() - escape_start,
            });
          }
        } else if unicode_mode {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: after_c.saturating_sub(escape_start),
          });
        }
        // Non-UnicodeMode: Annex B permits treating invalid/incomplete control escapes as literal
        // pattern characters (`\c`).
        i += esc_len;
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        continue;
      }

      if unicode_mode {
        // In UnicodeMode (`u`/`v`), escapes must match the strict grammar.
        //
        // We validate the shapes we explicitly support and then fall back to
        // `IdentityEscape`, which is restricted to SyntaxCharacter or `/`.

        // `\0` inside character classes is a null escape only when not followed by a decimal digit.
        if in_charset && esc.is_ascii_digit() {
          if esc == '0' {
            let after = i + esc_len;
            let next_is_digit = after < pattern.len()
              && pattern[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit());
            if next_is_digit {
              return Err(RegexError {
                kind: RegexErrorKind::InvalidPattern,
                offset: base_offset + escape_start,
                len: 2 + esc_len,
              });
            }
            i += esc_len;
            continue;
          }
          // Decimal escapes/backreferences are not valid inside character classes in UnicodeMode.
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: 1 + esc_len,
          });
        }

        // ControlEscape
        if matches!(esc, 'f' | 'n' | 'r' | 't' | 'v') {
          if !in_charset {
            prev_can_be_quantified = true;
            quantifier_allows_lazy = false;
          }
          i += esc_len;
          continue;
        }

        // CharacterClassEscape (valid both inside and outside character classes).
        if matches!(esc, 'd' | 'D' | 's' | 'S' | 'w' | 'W') {
          if !in_charset {
            prev_can_be_quantified = true;
            quantifier_allows_lazy = false;
          }
          i += esc_len;
          continue;
        }

        // Assertion escapes (`\b`, `\B`). Inside character classes, `\b` is backspace and `\B`
        // is not a valid escape in UnicodeMode.
        if esc == 'b' {
          if !in_charset {
            prev_can_be_quantified = false;
            quantifier_allows_lazy = false;
          }
          i += esc_len;
          continue;
        }
        if esc == 'B' {
          if in_charset {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }
          prev_can_be_quantified = false;
          quantifier_allows_lazy = false;
          i += esc_len;
          continue;
        }

        // Named backreference `\k<name>` is not valid inside character classes.
        if esc == 'k' {
          if in_charset {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }
          let after_k = i + esc_len;
          if after_k >= bytes.len() || bytes[after_k] != b'<' {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: 1 + esc_len,
            });
          }
          let mut j = after_k + 1;
          while j < bytes.len() && bytes[j] != b'>' {
            j += 1;
          }
          if j >= bytes.len() || j == after_k + 1 {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + escape_start,
              len: j.saturating_sub(escape_start),
            });
          }
          i = j + 1;
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
          continue;
        }

        // In UnicodeMode, `\-` is a ClassEscape.
        if in_charset && esc == '-' {
          i += esc_len;
          continue;
        }

        // IdentityEscape[+UnicodeMode] is limited to SyntaxCharacter or `/`.
        if !is_regex_identity_escape_unicode_mode(esc) {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + escape_start,
            len: 1 + esc_len,
          });
        }

        if !in_charset {
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
        }
        i += esc_len;
        continue;
      }

      // Non-UnicodeMode: Annex B identity escapes are permissive; treat everything else as an atom.
      if !in_charset {
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
      }

      i += esc_len;
      continue;
    }

    match ch {
      '[' => {
        in_charset = true;
        charset_first = true;
        charset_negated = false;
        charset_prev_atom = None;
        prev_can_be_quantified = false;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
      ']' => {
        // In Unicode mode (`u`/`v`), Annex B extensions are disabled and `]` is a SyntaxCharacter,
        // so it cannot appear as an unescaped PatternCharacter outside of a character class.
        if unicode_mode {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + i,
            len: 1,
          });
        }
        // In non-unicode mode, Annex B allows treating `]` as a literal outside charsets.
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
      '(' => {
        let (quantifiable, consumed, capture_name) =
          regex_group_prefix_info(pattern, i, unicode_mode).map_err(|mut err| {
            // regex_group_prefix_info reports offsets relative to the pattern slice; translate.
            err.offset += base_offset;
            err
          })?;
        if let Some(name) = capture_name {
          let signature = alt_stack.clone();
          if let Some(existing) = named_capture_groups.get(&name) {
            // Early error: duplicate named capture groups are disallowed unless they are in disjoint
            // alternation branches.
            for prev in existing {
              if !signatures_mutually_exclusive(prev, &signature) {
                return Err(RegexError {
                  kind: RegexErrorKind::InvalidPattern,
                  offset: base_offset + i,
                  len: consumed,
                });
              }
            }
          }
          named_capture_groups
            .entry(name)
            .or_default()
            .push(signature);
        }
        group_stack.push(quantifiable);
        alt_stack.push((next_alt_frame_id, 0));
        next_alt_frame_id = next_alt_frame_id.wrapping_add(1);
        prev_can_be_quantified = false;
        quantifier_allows_lazy = false;
        i += consumed;
        continue;
      }
      ')' => {
        let Some(quantifiable) = group_stack.pop() else {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + i,
            len: 1,
          });
        };
        alt_stack.pop();
        prev_can_be_quantified = quantifiable;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
      '^' | '$' => {
        // Assertions cannot be quantified.
        prev_can_be_quantified = false;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
      '|' => {
        // Alternation cannot be quantified. Track the branch index for duplicate named group
        // analysis.
        prev_can_be_quantified = false;
        quantifier_allows_lazy = false;
        if let Some((_id, branch)) = alt_stack.last_mut() {
          *branch = branch.wrapping_add(1);
        }
        i += ch_len;
        continue;
      }
      '*' | '+' | '?' => {
        if quantifier_allows_lazy && ch == '?' {
          // Non-greedy modifier.
          quantifier_allows_lazy = false;
          i += ch_len;
          continue;
        }
        if !prev_can_be_quantified {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + i,
            len: 1,
          });
        }
        prev_can_be_quantified = false;
        quantifier_allows_lazy = true;
        i += ch_len;
        continue;
      }
      '{' => {
        let quant_start = i;
        let mut j = i + 1;
        let mut digits = 0usize;
        let mut min: usize = 0;
        while j < bytes.len() {
          let c = bytes[j];
          if !c.is_ascii_digit() {
            break;
          }
          min = min.saturating_mul(10).saturating_add((c - b'0') as usize);
          digits += 1;
          j += 1;
        }

        if digits == 0 {
          if unicode_mode {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + quant_start,
              len: 1,
            });
          }
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
          i += ch_len;
          continue;
        }

        let mut max: Option<usize> = None;
        if j < bytes.len() && bytes[j] == b',' {
          j += 1;
          let mut max_digits = 0usize;
          let mut max_val: usize = 0;
          while j < bytes.len() {
            let c = bytes[j];
            if !c.is_ascii_digit() {
              break;
            }
            max_val = max_val.saturating_mul(10).saturating_add((c - b'0') as usize);
            max_digits += 1;
            j += 1;
          }
          if max_digits > 0 {
            max = Some(max_val);
          }
        }

        if j >= bytes.len() || bytes[j] != b'}' {
          if unicode_mode {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + quant_start,
              len: 1,
            });
          }
          prev_can_be_quantified = true;
          quantifier_allows_lazy = false;
          i += ch_len;
          continue;
        }

        if !prev_can_be_quantified {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + quant_start,
            len: 1,
          });
        }
        if let Some(max) = max {
          if max < min {
            return Err(RegexError {
              kind: RegexErrorKind::InvalidPattern,
              offset: base_offset + quant_start,
              len: 1,
            });
          }
        }

        prev_can_be_quantified = false;
        quantifier_allows_lazy = true;
        i = j + 1;
        continue;
      }
      '}' => {
        if unicode_mode {
          return Err(RegexError {
            kind: RegexErrorKind::InvalidPattern,
            offset: base_offset + i,
            len: 1,
          });
        }
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
      _ => {
        // Treat anything else as a literal atom.
        prev_can_be_quantified = true;
        quantifier_allows_lazy = false;
        i += ch_len;
        continue;
      }
    }
  }

  if in_charset || !group_stack.is_empty() {
    return Err(RegexError {
      kind: RegexErrorKind::InvalidPattern,
      offset: base_offset + pattern.len(),
      len: 0,
    });
  }

  for (name, offset, len) in named_backreferences {
    if !named_capture_groups.contains_key(&name) {
      return Err(RegexError {
        kind: RegexErrorKind::InvalidPattern,
        offset,
        len,
      });
    }
  }

  Ok(())
}

fn validate_regex_literal(raw: &str) -> Result<(), RegexError> {
  fn scan_terminator(raw: &str, nested: bool) -> Result<Option<usize>, RegexError> {
    let mut escaped = false;
    let mut class_depth: usize = 0;
    for (i, ch) in raw.char_indices().skip(1) {
      if escaped {
        escaped = false;
        continue;
      }
      match ch {
        '\\' => escaped = true,
        '[' => {
          if nested {
            class_depth = class_depth.saturating_add(1);
          } else if class_depth == 0 {
            class_depth = 1;
          }
        }
        ']' if class_depth > 0 => {
          if nested {
            class_depth -= 1;
          } else {
            class_depth = 0;
          }
        }
        '/' if class_depth == 0 => return Ok(Some(i)),
        c if is_line_terminator(c) => {
          return Err(RegexError {
            kind: RegexErrorKind::LineTerminator,
            offset: i,
            len: ch.len_utf8(),
          });
        }
        _ => {}
      }
    }

    // If the literal ends immediately after a backslash escape, treat it as unterminated (matching
    // the previous validator behaviour).
    if escaped {
      return Err(RegexError {
        kind: RegexErrorKind::Unterminated,
        offset: raw.len(),
        len: 0,
      });
    }

    Ok(None)
  }

  // Stage 1: scan using classic RegExp character-class termination rules to locate the pattern
  // terminator and extract the flags suffix.
  let Some(mut end_pat) = scan_terminator(raw, false)? else {
    return Err(RegexError {
      kind: RegexErrorKind::Unterminated,
      offset: raw.len(),
      len: 0,
    });
  };

  // If the raw flags suffix contains `v`, do a nested character-class aware scan so we don't
  // mis-detect the closing `/` when Unicode Sets mode contains nested character classes.
  //
  // If the nested scan fails to find a terminator (e.g. unbalanced `[`/`]` pairs under nesting
  // semantics), fall back to the classic terminator position; the pattern validator will report
  // the error.
  let has_v_flag = raw[end_pat + 1..].as_bytes().iter().any(|b| *b == b'v');
  if has_v_flag {
    if let Some(i) = scan_terminator(raw, true)? {
      end_pat = i;
    }
  }

  let flags_start = end_pat + '/'.len_utf8();
  validate_regex_flags(raw, flags_start)?;
  let flags = &raw[flags_start..];
  let unicode_mode = flags.as_bytes().iter().any(|b| *b == b'u' || *b == b'v');
  let unicode_sets_mode = flags.as_bytes().iter().any(|b| *b == b'v');
  let pattern = &raw['/'.len_utf8()..end_pat];
  let (capture_groups, has_named_groups) = regex_capture_info(pattern, unicode_sets_mode);
  validate_regex_pattern(
    pattern,
    '/'.len_utf8(),
    unicode_mode,
    unicode_sets_mode,
    has_named_groups,
    capture_groups,
  )?;
  Ok(())
}

fn regex_error_to_syntax(err: RegexError, token_start: usize) -> SyntaxError {
  let typ = match err.kind {
    RegexErrorKind::LineTerminator => SyntaxErrorType::LineTerminatorInRegex,
    RegexErrorKind::Unterminated => SyntaxErrorType::UnexpectedEnd,
    RegexErrorKind::InvalidPattern => SyntaxErrorType::ExpectedSyntax("valid regular expression"),
    RegexErrorKind::InvalidFlag | RegexErrorKind::DuplicateFlag => {
      SyntaxErrorType::ExpectedSyntax("valid regex flags")
    }
  };
  let start = token_start + err.offset;
  let end = start + err.len;
  Loc(start, end).error(typ, Some(TT::LiteralRegex))
}

#[cfg(test)]
mod regex_validation_tests {
  use super::validate_regex_literal;

  fn assert_valid(raw: &str) {
    validate_regex_literal(raw).unwrap();
  }

  fn assert_invalid(raw: &str) {
    assert!(
      validate_regex_literal(raw).is_err(),
      "expected regex to be rejected: {raw}",
    );
  }

  fn assert_invalid_kind(raw: &str, kind: super::RegexErrorKind) {
    let err = validate_regex_literal(raw).expect_err("expected regex to be rejected");
    assert_eq!(err.kind, kind, "unexpected error kind for {raw}");
  }

  #[test]
  fn duplicate_named_capture_groups_are_allowed() {
    assert_valid(r"/(?:(?<x>a)|(?<x>b)|c)\k<x>/");
    assert_valid(r"/(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/");
  }

  #[test]
  fn duplicate_named_capture_groups_in_same_alternative_are_rejected() {
    assert_invalid(r"/(?<x>a)(?<x>b)/");
    // A duplicate in any disjunction alternative is a syntax error.
    assert_invalid(r"/(?<x>a)|(?<x>b)(?<x>c)/");
    // Duplicate between the outer named group and a nested named group in one alternative.
    assert_invalid(r"/(?<x>(?<x>a)|b)/");
  }

  #[test]
  fn unicode_property_escapes_are_valid_in_unicode_mode() {
    // Basic u-mode property escapes should be accepted by the RegExp literal validator so they can
    // reach runtime (where `vm-js` performs the actual Unicode property name/value resolution).
    for raw in [
      r"/\p{ASCII}/u",
      r"/\P{ASCII}/u",
      r"/\p{Lu}/u",
      r"/\p{Script=Greek}/u",
    ] {
      validate_regex_literal(raw).unwrap_or_else(|e| panic!("{raw}: {e:?}"));
    }
  }

  #[test]
  fn unicode_sets_mode_accepts_class_string_disjunction() {
    assert_valid(r"/^[\q{0|2|4|9\uFE0F\u20E3}_]+$/v");
    assert_valid(r"/^[[0-9]\q{0|2|4|9\uFE0F\u20E3}]+$/v");
    assert_valid(r"/^[\p{ASCII_Hex_Digit}&&\q{0|2|4|9\uFE0F\u20E3}]+$/v");
    assert_valid(r"/\p{Script=Han}/v");
    assert_valid(r"/[\q{}]/v");
    assert_valid(r"/[\q{|a}]/v");
    assert_valid(r"/[\q{a||b}]/v"); // empty disjunction arm is allowed outside negated classes
    assert_valid(r"/[\q{a\|b}]/v");
    assert_valid(r"/[\q{a\}b}]/v");
    assert_valid(r"/[\q{a\\b}]/v");
    assert_valid(r"/[\q{a\{b}]/v");
    assert_valid(r"/[\q{\u{41}}]/v");
    assert_valid(r"/[\q{\&\&}]/v");
  }

  #[test]
  fn unicode_mode_accepts_unicode_property_escapes() {
    assert_valid(r"/\p{ASCII}/u");
    assert_valid(r"/\P{ASCII}/u");
    assert_valid(r"/\p{Lu}/u");
    assert_valid(r"/\p{Script=Greek}/u");
  }

  #[test]
  fn unicode_sets_mode_allows_non_string_disjunction_in_negated_class() {
    assert_valid(r"/[^\q{a|b}]/v");
    assert_valid(r"/[^\q{\u{41}}]/v"); // braced unicode escape counts as a single ClassSetCharacter
    assert_valid(r"/[^\q{\uD83D\uDE00}]/v"); // surrogate pair counts as a single ClassSetCharacter
    assert_invalid(r"/[^\q{ab}]/v");
    assert_invalid(r"/[^\q{a||b}]/v");
    assert_invalid(r"/[^\q{}]/v");
  }

  #[test]
  fn unicode_sets_mode_rejects_unescaped_class_set_syntax_characters() {
    assert_invalid(r"/[{]/v");
    assert_invalid(r"/[|]/v");
    assert_valid(r"/[\{]/v");
    assert_valid(r"/[\|]/v");
  }

  #[test]
  fn unicode_sets_mode_rejects_reserved_punctuator_literals() {
    // These patterns are accepted in non-`v` modes but are early errors in Unicode Sets mode.
    for pat in [
      r"/[(]/v",
      r"/[)]/v",
      r"/[[]/v",
      r"/[{]/v",
      r"/[}]/v",
      r"/[/]/v",
      r"/[-]/v",
      r"/[|]/v",
    ] {
      assert_invalid(pat);
    }

    // Spot-check that the same patterns are still accepted in `u` mode (and thus aren't rejected
    // by the generic regex validator).
    assert_valid(r"/[(]/u");
    assert_valid(r"/[&&]/u");
    assert_valid(r"/[``]/u");
  }

  #[test]
  fn duplicate_named_capture_groups_are_only_allowed_in_disjoint_alternatives() {
    // `DuplicateNamedCapturingGroups` (ECMA-262): duplicates are permitted when they cannot both
    // participate in the same match result.
    assert_valid(r"/(?:(?<x>a)|(?<x>b))/");
    assert_valid(r"/(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/");

    // Duplicates in a single alternative are early errors.
    assert_invalid(r"/(?<x>a)(?<x>b)/");
    assert_invalid(r"/(?:(?<x>a)(?<x>b)|c)/");
  }

  #[test]
  fn unicode_sets_mode_rejects_reserved_double_punctuators() {
    for pat in [
      r"/[&&]/v",
      r"/[!!]/v",
      r"/[##]/v",
      r"/[$$]/v",
      r"/[%%]/v",
      r"/[**]/v",
      r"/[++]/v",
      r"/[,,]/v",
      r"/[..]/v",
      r"/[::]/v",
      r"/[;;]/v",
      r"/[<<]/v",
      r"/[==]/v",
      r"/[>>]/v",
      r"/[??]/v",
      r"/[@@]/v",
      r"/[``]/v",
      r"/[~~]/v",
      r"/[^^^]/v",
      r"/[_^^]/v",
    ] {
      assert_invalid(pat);
    }
  }

  #[test]
  fn unicode_sets_mode_accepts_escaped_reserved_punctuators() {
    // These punctuators can be escaped in `/v` mode to avoid being parsed as reserved operators
    // (e.g. `&&` is intersection) / reserved double punctuators (e.g. `!!`).
    assert_valid(r"/[\&\&]/v");
    assert_valid(r"/[\!\!]/v");
    assert_valid(r"/[\#\#]/v");
    assert_valid(r"/[\~\~]/v");

    // Single punctuators should also be accepted when escaped.
    assert_valid(r"/[\&]/v");
    assert_valid(r"/[\!]/v");
  }

  #[test]
  fn unicode_sets_mode_accepts_nested_classes_and_set_operators() {
    assert_valid(r"/^[[0-9]\p{ASCII_Hex_Digit}]+$/v");
    assert_valid(r"/^[\p{ASCII_Hex_Digit}--[0-9]]+$/v");
    assert_valid(r"/^[[0-9]&&\p{ASCII_Hex_Digit}]+$/v");
  }

  #[test]
  fn unicode_sets_mode_accepts_surrogate_pair_operands_and_ranges() {
    // In UnicodeMode (`u`/`v`), surrogate pairs expressed as consecutive `\uXXXX` escapes form a
    // single character. This matters for set-operator grammar and class range validation.
    assert_valid(r"/[\uD83D\uDE00&&a]/v");
    assert_valid(r"/[\uD83D\uDE00--a]/v");
    assert_valid(r"/[\uD83D\uDE00-\uD83D\uDE01]/v");
  }

  #[test]
  fn unicode_sets_mode_accepts_reserved_punctuator_escapes() {
    // `\!` / `\&` are not IdentityEscapes in UnicodeMode, but are allowed inside UnicodeSets
    // character classes via ClassSetReservedPunctuator.
    assert_valid(r"/[\!!]/v");
    assert_valid(r"/[\&\&]/v");
    assert_valid(r"/[^\!]/v");
    assert_valid(r"/[^\&]/v");
  }

  #[test]
  fn unicode_sets_mode_scans_regex_terminator_with_nested_classes() {
    // The classic scan treats the first `]` as closing the class, so it would incorrectly split
    // flags at `]/v` and report an invalid-flag error. Ensure we instead report an invalid pattern.
    assert_invalid_kind(r"/[[0-9]/]/v", super::RegexErrorKind::InvalidPattern);
  }

  #[test]
  fn unicode_sets_mode_nested_scan_falls_back_to_classic_terminator() {
    // `/[[]/v` is a well-terminated literal, but is an invalid pattern in UnicodeSets mode. A
    // fully nested scan would treat it as unterminated (because `[` would start a nested class),
    // so ensure we fall back to the classic terminator position and still report an invalid
    // pattern.
    assert_invalid_kind(r"/[[]/v", super::RegexErrorKind::InvalidPattern);
  }

  #[test]
  fn unicode_sets_mode_rejects_malformed_class_string_disjunction() {
    assert_invalid(r"/[\q]/v"); // missing `{`
    assert_invalid(r"/[\q{a|b]/v"); // unterminated `}`
    assert_invalid(r"/[\q{a{b}|c}]/v"); // raw `{` inside disjunction
    assert_invalid(r"/[\q{&&}]/v"); // reserved double punctuator
    assert_invalid(r"/[\q{\d}]/v"); // character class escape not allowed in ClassString
    assert_invalid(r"/[\q{\q}]/v"); // invalid escape in UnicodeMode
  }

  #[test]
  fn unicode_sets_mode_rejects_mixed_union_and_set_operators() {
    // `&&` / `--` operate on ClassSetOperand, not on implicit unions. Unions must be wrapped in a
    // nested class (`[...]`) before being combined with set operators.
    assert_invalid(r"/[ab&&c]/v");
    assert_invalid(r"/[a&&bc]/v");
    assert_invalid(r"/[ab--c]/v");
    assert_invalid(r"/[a--bc]/v");
    assert_invalid(r"/[a-b&&c]/v");
  }

  #[test]
  fn unicode_property_escape_ascii_is_valid_in_u_mode() {
    assert_valid(r"/\p{ASCII}/u");
    assert_valid(r"/\P{ASCII}/u");
  }

  #[test]
  fn unicode_property_escape_general_category_is_valid_in_u_mode() {
    assert_valid(r"/\p{Lu}/u");
  }

  #[test]
  fn unicode_property_escape_script_is_valid_in_u_mode() {
    assert_valid(r"/\p{Script=Greek}/u");
  }

  #[test]
  fn unicode_property_value_expression_script_is_valid() {
    assert!(
      super::super::regex_unicode_property::validate_unicode_property_value_expression(
        "Script=Greek",
        false,
      )
    );
  }
}

pub fn normalise_literal_string_or_template_inner(raw: &str) -> Option<String> {
  decode_literal(raw, true).ok()
}

pub fn normalise_literal_string(raw: &str) -> Option<String> {
  if raw.len() < 2 {
    return None;
  }
  decode_literal(&raw[1..raw.len() - 1], false).ok()
}

impl<'a> Parser<'a> {
  pub fn lit_arr(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<LitArrExpr>> {
    let start = self.checkpoint();
    self.require(TT::BracketOpen)?;
    let mut elements = Vec::<LitArrElem>::new();
    let mut trailing_comma_after_rest = false;
    loop {
      if self.consume_if(TT::Comma).is_match() {
        elements.push(LitArrElem::Empty);
        continue;
      };
      if self.peek().typ == TT::BracketClose {
        break;
      };
      let rest = self.consume_if(TT::DotDotDot).is_match();
      let value = self.expr(ctx, [TT::Comma, TT::BracketClose])?;
      elements.push(if rest {
        LitArrElem::Rest(value)
      } else {
        LitArrElem::Single(value)
      });
      if self.peek().typ == TT::BracketClose {
        break;
      };
      self.require(TT::Comma)?;
      if rest && self.peek().typ == TT::BracketClose {
        trailing_comma_after_rest = true;
      }
    }
    self.require(TT::BracketClose)?;
    let mut node = Node::new(self.since_checkpoint(&start), LitArrExpr { elements });
    if trailing_comma_after_rest {
      node.assoc.set(TrailingCommaAfterRestElement);
    }
    Ok(node)
  }

  pub fn lit_bigint(&mut self) -> SyntaxResult<Node<LitBigIntExpr>> {
    self.with_loc(|p| {
      let value = p.lit_bigint_val()?;
      Ok(LitBigIntExpr { value })
    })
  }

  pub fn lit_bigint_val(&mut self) -> SyntaxResult<String> {
    let t = self.require(TT::LiteralBigInt)?;
    normalise_literal_bigint(self.str(t.loc))
      .ok_or_else(|| t.loc.error(SyntaxErrorType::MalformedLiteralBigInt, None))
  }

  pub fn lit_bool(&mut self) -> SyntaxResult<Node<LitBoolExpr>> {
    self.with_loc(|p| {
      if p.consume_if(TT::LiteralTrue).is_match() {
        Ok(LitBoolExpr { value: true })
      } else {
        p.require(TT::LiteralFalse)?;
        Ok(LitBoolExpr { value: false })
      }
    })
  }

  pub fn lit_null(&mut self) -> SyntaxResult<Node<LitNullExpr>> {
    self.with_loc(|p| {
      p.require(TT::LiteralNull)?;
      Ok(LitNullExpr {})
    })
  }

  pub fn lit_num(&mut self) -> SyntaxResult<Node<LitNumExpr>> {
    let t = self.require(TT::LiteralNumber)?;
    let raw = self.str(t.loc);
    if self.is_strict_ecmascript()
      && self.is_strict_mode()
      && (crate::num::is_legacy_octal_literal(raw)
        || crate::num::is_leading_zero_decimal_literal(raw))
    {
      return Err(t.error(SyntaxErrorType::ExpectedSyntax(
        "numeric literals with leading zeros are not allowed in strict mode",
      )));
    }
    let value = normalise_literal_number(raw)
      .ok_or_else(|| t.loc.error(SyntaxErrorType::MalformedLiteralNumber, None))?;

    let mut node = Node::new(t.loc, LitNumExpr { value });
    if crate::num::is_legacy_octal_literal(raw) {
      node.assoc.set(LegacyOctalNumberLiteral);
    } else if crate::num::is_leading_zero_decimal_literal(raw) {
      node.assoc.set(LeadingZeroDecimalLiteral);
    }
    Ok(node)
  }

  pub fn lit_num_val(&mut self) -> SyntaxResult<JsNumber> {
    let t = self.require(TT::LiteralNumber)?;
    normalise_literal_number(self.str(t.loc))
      .ok_or_else(|| t.loc.error(SyntaxErrorType::MalformedLiteralNumber, None))
  }

  pub fn lit_obj(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<LitObjExpr>> {
    self.with_loc(|p| {
      p.require(TT::BraceOpen)?;
      let mut members = Vec::new();
      while p.peek().typ != TT::BraceClose && p.peek().typ != TT::EOF {
        let member_start = p.peek().loc;
        // TypeScript-style recovery: class declarations in object literals are
        // not allowed, but try to skip them so the rest of the literal can be
        // parsed.
        let looks_like_nested_class = p.should_recover()
          && p.peek().typ == TT::KeywordClass
          && matches!(
            p.peek_n::<3>()[1].typ,
            TT::Identifier | TT::BraceOpen | TT::KeywordExtends
          );
        if looks_like_nested_class {
          // Skip the entire class declaration
          p.consume(); // consume 'class'
                       // Skip optional class name
          if p.peek().typ == TT::Identifier {
            p.consume();
          }
          // Skip optional type parameters
          if p.peek().typ == TT::ChevronLeft {
            let mut depth = 0;
            while p.peek().typ != TT::EOF {
              if p.peek().typ == TT::ChevronLeft {
                depth += 1;
                p.consume();
              } else if p.peek().typ == TT::ChevronRight {
                p.consume();
                depth -= 1;
                if depth == 0 {
                  break;
                }
              } else if p.peek().typ == TT::BraceOpen {
                break;
              } else {
                p.consume();
              }
            }
          }
          // Skip optional extends clause
          if p.consume_if(TT::KeywordExtends).is_match() {
            // Skip until we reach the class body
            while p.peek().typ != TT::BraceOpen && p.peek().typ != TT::EOF {
              p.consume();
            }
          }
          // Skip optional implements clause
          if p.consume_if(TT::KeywordImplements).is_match() {
            // Skip until we reach the class body
            while p.peek().typ != TT::BraceOpen && p.peek().typ != TT::EOF {
              p.consume();
            }
          }
          // Skip the class body
          if p.peek().typ == TT::BraceOpen {
            let mut depth = 0;
            while p.peek().typ != TT::EOF {
              if p.peek().typ == TT::BraceOpen {
                depth += 1;
                p.consume();
              } else if p.peek().typ == TT::BraceClose {
                p.consume();
                depth -= 1;
                if depth == 0 {
                  break;
                }
              } else {
                p.consume();
              }
            }
          }
          // Return a dummy member (will be ignored by error recovery)
          // Use an empty shorthand property as a placeholder
          use crate::ast::expr::IdExpr;
          use crate::loc::Loc;
          members.push(Node::new(
            member_start,
            ObjMember {
              typ: ObjMemberType::Shorthand {
                id: Node::new(
                  Loc(0, 0),
                  IdExpr {
                    name: String::new(),
                  },
                ),
              },
            },
          ));
          continue;
        }

        let rest = p.consume_if(TT::DotDotDot).is_match();
        let mut allow_semicolon_separator = false;
        if rest {
          let value = p.expr(ctx, [TT::Comma, TT::Semicolon, TT::BraceClose])?;
          members.push(Node::new(
            member_start,
            ObjMember {
              typ: ObjMemberType::Rest { val: value },
            },
          ));
        } else {
          let (key, value) = p.class_or_obj_member(
            ctx,
            TT::Colon,
            TT::Comma,
            &mut Asi::no(),
            false, // Object literals don't have abstract methods
          )?;
          allow_semicolon_separator = matches!(value, ClassOrObjVal::IndexSignature(_));
          let typ = match value {
            ClassOrObjVal::Prop(None) => {
              // This property had no value, so it's a shorthand property. Therefore, check
              // that it's a valid identifier name.
              match key {
                ClassOrObjKey::Computed(expr) => {
                  if !p.should_recover() {
                    return Err(
                      expr.error(SyntaxErrorType::ExpectedSyntax("object literal value")),
                    );
                  }
                  // TypeScript-style recovery - computed properties without value like `{ [e] }`.
                  let loc = expr.loc;
                  let synthetic_value = Node::new(
                    loc,
                    IdExpr {
                      name: "undefined".to_string(),
                    },
                  )
                  .into_wrapped();
                  ObjMemberType::Valued {
                    key: ClassOrObjKey::Computed(expr),
                    val: ClassOrObjVal::Prop(Some(synthetic_value)),
                  }
                }
                ClassOrObjKey::Direct(direct_key) => {
                  if p.should_recover() {
                    // TypeScript-style recovery: accept malformed shorthand properties
                    // like `{ while }` / `{ "while" }` / `{ 1 }`.
                    if direct_key.stx.tt != TT::Identifier
                      && !KEYWORDS_MAPPING.contains_key(&direct_key.stx.tt)
                      && direct_key.stx.tt != TT::LiteralString
                      && direct_key.stx.tt != TT::LiteralNumber
                      && direct_key.stx.tt != TT::LiteralBigInt
                    {
                      return Err(direct_key.error(SyntaxErrorType::ExpectedNotFound));
                    };
                    // TypeScript-style recovery: definite assignment assertion (e.g., `{ a! }`).
                     let _definite_assignment = p.consume_if(TT::Exclamation).is_match();
                     // TypeScript-style recovery: default value (e.g., `{ c = 1 }`).
                     if p.consume_if(TT::Equals).is_match() {
                       let key_name = direct_key.stx.key.clone();
                       let key_loc = direct_key.loc;
                       p.validate_arguments_not_disallowed_in_class_init(key_loc, &key_name)?;
                       let default_val = p.expr(ctx, [TT::Comma, TT::Semicolon, TT::BraceClose])?;
                        let id_expr = Node::new(
                          key_loc,
                          IdExpr {
                            name: key_name.clone(),
                         },
                       )
                       .into_wrapped();
                       let mut bin_expr = Node::new(
                         key_loc + default_val.loc,
                         BinaryExpr {
                           operator: OperatorName::Assignment,
                           left: id_expr,
                           right: default_val,
                         },
                       )
                       .into_wrapped();
                       bin_expr.assoc.set(CoverInitializedName);
                       ObjMemberType::Valued {
                         key: ClassOrObjKey::Direct(Node::new(
                           key_loc,
                           ClassOrObjMemberDirectKey {
                            key: key_name,
                            tt: TT::Identifier,
                          },
                        )),
                         val: ClassOrObjVal::Prop(Some(bin_expr)),
                        }
                      } else {
                       let key_loc = direct_key.loc;
                       let key_name = direct_key.stx.key.clone();
                       p.validate_arguments_not_disallowed_in_class_init(key_loc, &key_name)?;
                        ObjMemberType::Shorthand {
                          id: direct_key.map_stx(|n| IdExpr { name: n.key }),
                        }
                      }
                    } else {
                      if !is_valid_pattern_identifier(direct_key.stx.tt, ctx.rules) {
                        return Err(direct_key.error(SyntaxErrorType::ExpectedSyntax("identifier")));
                      }
                      if direct_key.stx.tt == TT::Identifier {
                        if let Some(keyword_tt) = keyword_from_str(&direct_key.stx.key) {
                          if !is_valid_pattern_identifier(keyword_tt, ctx.rules) {
                            return Err(direct_key.error(SyntaxErrorType::ExpectedSyntax(
                              "identifier",
                            )));
                          }
                        }
                      }
                      if p.is_strict_ecmascript()
                        && p.is_strict_mode()
                        && Parser::is_strict_mode_reserved_word(&direct_key.stx.key)
                      {
                        return Err(direct_key.error(SyntaxErrorType::ExpectedSyntax("identifier")));
                      }
                      if p.consume_if(TT::Equals).is_match() {
                        let key_name = direct_key.stx.key.clone();
                        let key_loc = direct_key.loc;
                        p.validate_arguments_not_disallowed_in_class_init(key_loc, &key_name)?;
                        let default_val = p.expr(ctx, [TT::Comma, TT::Semicolon, TT::BraceClose])?;
                        let id_expr = Node::new(
                         key_loc,
                          IdExpr {
                            name: key_name.clone(),
                         },
                       )
                       .into_wrapped();
                       let mut bin_expr = Node::new(
                         key_loc + default_val.loc,
                         BinaryExpr {
                           operator: OperatorName::Assignment,
                           left: id_expr,
                           right: default_val,
                         },
                       )
                       .into_wrapped();
                       bin_expr.assoc.set(CoverInitializedName);
                       ObjMemberType::Valued {
                          key: ClassOrObjKey::Direct(direct_key),
                          val: ClassOrObjVal::Prop(Some(bin_expr)),
                        }
                      } else {
                       let key_loc = direct_key.loc;
                       let key_name = direct_key.stx.key.clone();
                       p.validate_arguments_not_disallowed_in_class_init(key_loc, &key_name)?;
                        ObjMemberType::Shorthand {
                          id: direct_key.map_stx(|n| IdExpr { name: n.key }),
                        }
                      }
                    }
                  }
                }
              }
            _ => ObjMemberType::Valued { key, val: value },
          };
          members.push(Node::new(member_start, ObjMember { typ }));
        }
        if p.consume_if(TT::Comma).is_match() {
          continue;
        }
        if p.peek().typ == TT::Semicolon {
          let semi = p.consume();
          if allow_semicolon_separator {
            continue;
          }
          return Err(semi.error(SyntaxErrorType::ExpectedSyntax("`,`")));
        }
        if p.peek().typ == TT::BraceClose {
          break;
        }
        return Err(p.peek().error(SyntaxErrorType::ExpectedSyntax("`,`")));
      }
      p.require(TT::BraceClose)?;
      Ok(LitObjExpr { members })
    })
  }

  pub fn lit_regex(&mut self) -> SyntaxResult<Node<LitRegexExpr>> {
    self.with_loc(|p| {
      let t = match p.peek().typ {
        TT::LiteralRegex | TT::Invalid => p.consume_with_mode(LexMode::SlashIsRegex),
        _ => p.require_with_mode(TT::LiteralRegex, LexMode::SlashIsRegex)?,
      };
      let value = p.string(t.loc);
      validate_regex_literal(&value).map_err(|err| regex_error_to_syntax(err, t.loc.0))?;
      Ok(LitRegexExpr { value })
    })
  }

  pub fn lit_str(&mut self) -> SyntaxResult<Node<LitStrExpr>> {
    let (loc, value, escape_loc, code_units) =
      self.lit_str_val_with_mode_and_legacy_escape(LexMode::Standard)?;
    if self.is_strict_ecmascript() && self.is_strict_mode() {
      if let Some(escape_loc) = escape_loc {
        return Err(escape_loc.error(
          SyntaxErrorType::ExpectedSyntax("octal escape sequences not allowed in strict mode"),
          Some(TT::LiteralString),
        ));
      }
    }
    let mut node = Node::new(loc, LitStrExpr { value });
    node
      .assoc
      .set(LiteralStringCodeUnits(code_units.into_boxed_slice()));
    if let Some(escape_loc) = escape_loc {
      node.assoc.set(LegacyOctalEscapeSequence(escape_loc));
    }
    Ok(node)
  }

  /// Parses a literal string and returns the raw string value normalized (e.g. escapes decoded).
  /// Does *not* return a node; use `lit_str` for that.
  pub fn lit_str_val(&mut self) -> SyntaxResult<String> {
    self.lit_str_val_with_mode(LexMode::Standard)
  }

  pub fn lit_str_val_with_mode(&mut self, mode: LexMode) -> SyntaxResult<String> {
    self
      .lit_str_val_with_mode_and_legacy_escape(mode)
      .map(|(_, value, _, _)| value)
  }

  pub(crate) fn lit_str_val_with_mode_and_legacy_escape(
    &mut self,
    mode: LexMode,
  ) -> SyntaxResult<(Loc, String, Option<Loc>, Vec<u16>)> {
    let peek = self.peek_with_mode(mode);
    let t = if matches!(peek.typ, TT::LiteralString | TT::Invalid)
      && self
        .str(peek.loc)
        .starts_with(['"', '\''])
    {
      self.consume_with_mode(mode)
    } else {
      self.require_with_mode(TT::LiteralString, mode)?
    };
    let raw = self.bytes(t.loc);
    let quote = raw
      .chars()
      .next()
      .ok_or_else(|| t.error(SyntaxErrorType::UnexpectedEnd))?;
    let quote_len = quote.len_utf8();
    // A lone quote token (e.g. just `'`/`"`) should be treated as unterminated.
    let has_closing = raw.len() >= quote_len.saturating_mul(2) && raw.ends_with(quote);
    let body_start = t.loc.0 + quote.len_utf8();
    let body_end = if has_closing {
      t.loc.1.saturating_sub(quote.len_utf8())
    } else {
      t.loc.1
    };
    let body = self.bytes(Loc(body_start, body_end));
    if self.is_strict_ecmascript() {
      if let Some((offset, len)) = find_non_octal_decimal_escape_sequence(body) {
        return Err(Loc(body_start + offset, body_start + offset + len).error(
          SyntaxErrorType::InvalidCharacterEscape,
          Some(TT::LiteralString),
        ));
      }
    }
    let escape_loc = find_legacy_escape_sequence(body)
      .map(|(offset, len)| Loc(body_start + offset, body_start + offset + len));

    if mode == LexMode::JsxTag {
      if !has_closing {
        return Err(
          Loc(body_end, body_end).error(SyntaxErrorType::UnexpectedEnd, Some(TT::LiteralString)),
        );
      }
      let code_units = body.encode_utf16().collect();
      return Ok((t.loc, body.to_string(), escape_loc, code_units));
    }
    let code_units = decode_literal_utf16(body, false).map_err(|err| {
      literal_error_to_syntax(
        err,
        body_start,
        TT::LiteralString,
        SyntaxErrorType::LineTerminatorInString,
      )
    })?;
    let decoded = String::from_utf16_lossy(&code_units);
    if !has_closing {
      return Err(
        Loc(body_end, body_end).error(SyntaxErrorType::UnexpectedEnd, Some(TT::LiteralString)),
      );
    }
    Ok((t.loc, decoded, escape_loc, code_units))
  }

  pub fn lit_template(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<LitTemplateExpr>> {
    let start = self.checkpoint();
    let (parts, template_parts, invalid_escape) =
      self.lit_template_parts_with_invalid_escape(ctx, false)?;
    let loc = self.since_checkpoint(&start);
    let mut node = Node::new(loc, LitTemplateExpr { parts });
    node.assoc.set(template_parts);
    if let Some(invalid_escape) = invalid_escape {
      node
        .assoc
        .set(InvalidTemplateEscapeSequence(invalid_escape));
    }
    Ok(node)
  }

  // NOTE: The next token must definitely be LiteralTemplatePartString{,End}.
  // ES2018: Tagged templates can have invalid escape sequences (cooked value is undefined, raw is available)
  // TypeScript: All templates allow invalid escapes (permissive parsing, semantic errors caught later)
  pub fn lit_template_parts(
    &mut self,
    ctx: ParseCtx,
    tagged: bool,
  ) -> SyntaxResult<Vec<LitTemplatePart>> {
    self
      .lit_template_parts_with_template_data(ctx, tagged)
      .map(|(parts, _)| parts)
  }

  pub(crate) fn lit_template_parts_with_template_data(
    &mut self,
    ctx: ParseCtx,
    tagged: bool,
  ) -> SyntaxResult<(Vec<LitTemplatePart>, TemplateStringParts)> {
    self
      .lit_template_parts_with_invalid_escape(ctx, tagged)
      .map(|(parts, template_parts, _)| (parts, template_parts))
  }

  fn lit_template_parts_with_invalid_escape(
    &mut self,
    ctx: ParseCtx,
    tagged: bool,
  ) -> SyntaxResult<(Vec<LitTemplatePart>, TemplateStringParts, Option<Loc>)> {
    let t = self.consume();
    let is_end = match t.typ {
      TT::LiteralTemplatePartString => false,
      TT::LiteralTemplatePartStringEnd => true,
      TT::Invalid => return Err(t.error(SyntaxErrorType::UnexpectedEnd)),
      _ => return Err(t.error(SyntaxErrorType::ExpectedSyntax("template string part"))),
    };

    let mut parts = Vec::new();
    let mut invalid_escape = None;
    let mut raw_parts = Vec::<Box<[u16]>>::new();
    let mut cooked_parts = Vec::<Option<Box<[u16]>>>::new();
    let raw = self.bytes(t.loc);
    let (content_offset, first_content) =
      template_content(raw, is_end).ok_or_else(|| t.error(SyntaxErrorType::UnexpectedEnd))?;
    let first_raw = encode_template_raw_utf16(first_content);
    raw_parts.push(first_raw);
    let first_legacy_escape = find_legacy_escape_sequence(first_content);
    if !tagged {
      if let Some((rel, len)) = first_legacy_escape {
        let loc = Loc(
          t.loc.0 + content_offset + rel,
          t.loc.0 + content_offset + rel + len,
        );
        // ECMAScript template literals reject legacy escape sequences. Tagged templates are the
        // sole exception (their cooked value becomes `undefined` and the raw value remains).
        if self.is_strict_ecmascript() {
          return Err(loc.error(SyntaxErrorType::InvalidCharacterEscape, Some(t.typ)));
        }
        if invalid_escape.is_none() {
          invalid_escape = Some(loc);
        }
      }
    }
    let decoded_first = decode_literal_utf16(first_content, true);
    let (first_str, first_cooked) = match decoded_first {
      Ok(code_units) => {
        let str_val = String::from_utf16_lossy(&code_units);
        let cooked = if tagged && first_legacy_escape.is_some() {
          None
        } else {
          Some(code_units.into_boxed_slice())
        };
        (str_val, cooked)
      }
      Err(_err) if tagged => (String::new(), None),
      Err(err) => {
        return Err(literal_error_to_syntax(
          err,
          t.loc.0 + content_offset,
          t.typ,
          SyntaxErrorType::InvalidCharacterEscape,
        ))
      }
    };
    cooked_parts.push(first_cooked);
    parts.push(LitTemplatePart::String(first_str));
    if !is_end {
      loop {
        let substitution = self.expr(ctx, [TT::BraceClose])?;
        self.require(TT::BraceClose)?;
        parts.push(LitTemplatePart::Substitution(substitution));
        let string = self.consume_with_mode(LexMode::TemplateStrContinue);
        let string_is_end = match string.typ {
          TT::LiteralTemplatePartString => false,
          TT::LiteralTemplatePartStringEnd => true,
          TT::Invalid => {
            return Err(Loc(string.loc.1, string.loc.1).error(
              SyntaxErrorType::UnexpectedEnd,
              Some(TT::LiteralTemplatePartString),
            ))
          }
          _ => {
            return Err(string.error(SyntaxErrorType::ExpectedSyntax("template string part")));
          }
        };
        let raw = self.bytes(string.loc);
        let (offset, content) = template_content(raw, string_is_end)
          .ok_or_else(|| string.error(SyntaxErrorType::UnexpectedEnd))?;
        let raw_part = encode_template_raw_utf16(content);
        raw_parts.push(raw_part);
        let legacy_escape = find_legacy_escape_sequence(content);
        if !tagged {
          if let Some((rel, len)) = legacy_escape {
            let loc = Loc(string.loc.0 + offset + rel, string.loc.0 + offset + rel + len);
            if self.is_strict_ecmascript() {
              return Err(loc.error(SyntaxErrorType::InvalidCharacterEscape, Some(string.typ)));
            }
            if invalid_escape.is_none() {
              invalid_escape = Some(loc);
            }
          }
        }

        let decoded_part = decode_literal_utf16(content, true);
        let (part_str, part_cooked) = match decoded_part {
          Ok(code_units) => {
            let str_val = String::from_utf16_lossy(&code_units);
            let cooked = if tagged && legacy_escape.is_some() {
              None
            } else {
              Some(code_units.into_boxed_slice())
            };
            (str_val, cooked)
          }
          Err(_err) if tagged => (String::new(), None),
          Err(err) => {
            return Err(literal_error_to_syntax(
              err,
              string.loc.0 + offset,
              string.typ,
              SyntaxErrorType::InvalidCharacterEscape,
            ))
          }
        };
        cooked_parts.push(part_cooked);
        parts.push(LitTemplatePart::String(part_str));
        if string_is_end {
          break;
        };
      }
    };

    Ok((
      parts,
      TemplateStringParts {
        raw: raw_parts.into_boxed_slice(),
        cooked: cooked_parts.into_boxed_slice(),
      },
      invalid_escape,
    ))
  }
}
