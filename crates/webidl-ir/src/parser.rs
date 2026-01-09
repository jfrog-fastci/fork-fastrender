use crate::default_value::{DefaultValue, NumericLiteral};
use crate::idl_type::{IdlType, NamedType, NamedTypeKind, NumericType, StringType, TypeAnnotation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
  pub message: String,
  pub offset: usize,
}

impl std::fmt::Display for ParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{} at byte {}", self.message, self.offset)
  }
}

impl std::error::Error for ParseError {}

struct Parser<'a> {
  input: &'a str,
  pos: usize,
}

impl<'a> Parser<'a> {
  fn new(input: &'a str) -> Self {
    Self { input, pos: 0 }
  }

  fn rest(&self) -> &'a str {
    &self.input[self.pos..]
  }

  fn is_eof(&self) -> bool {
    self.pos >= self.input.len()
  }

  fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
      message: message.into(),
      offset: self.pos,
    })
  }

  fn skip_ws(&mut self) {
    while let Some(b) = self.input.as_bytes().get(self.pos).copied() {
      if !b.is_ascii_whitespace() {
        break;
      }
      self.pos += 1;
    }
  }

  fn peek_byte(&self) -> Option<u8> {
    self.input.as_bytes().get(self.pos).copied()
  }

  fn consume_byte(&mut self, b: u8) -> bool {
    self.skip_ws();
    if self.peek_byte() == Some(b) {
      self.pos += 1;
      true
    } else {
      false
    }
  }

  fn expect_byte(&mut self, b: u8, what: &str) -> Result<(), ParseError> {
    if self.consume_byte(b) {
      Ok(())
    } else {
      self.err(format!("expected {what}"))
    }
  }

  fn consume_keyword(&mut self, kw: &str) -> bool {
    self.skip_ws();
    let rest = self.rest();
    if !rest.starts_with(kw) {
      return false;
    }
    let after = &rest[kw.len()..];
    if after
      .as_bytes()
      .first()
      .copied()
      .is_some_and(|c| is_ident_continue_byte(c))
    {
      return false;
    }
    self.pos += kw.len();
    true
  }

  fn parse_identifier(&mut self) -> Result<String, ParseError> {
    self.skip_ws();
    let bytes = self.rest().as_bytes();
    let Some(b0) = bytes.first().copied() else {
      return self.err("expected identifier");
    };
    if !is_ident_start_byte(b0) {
      return self.err("expected identifier");
    }
    let mut len = 1usize;
    while len < bytes.len() && is_ident_continue_byte(bytes[len]) {
      len += 1;
    }
    let s = &self.rest()[..len];
    self.pos += len;
    Ok(s.to_string())
  }

  fn parse_annotations(&mut self) -> Result<Vec<TypeAnnotation>, ParseError> {
    let mut out = Vec::new();
    loop {
      self.skip_ws();
      if self.peek_byte() != Some(b'[') {
        break;
      }
      self.pos += 1;
      let inside = self.consume_until_matching_bracket(b']')?;
      out.extend(parse_ext_attr_list(inside));
    }
    Ok(out)
  }

  fn consume_until_matching_bracket(&mut self, closing: u8) -> Result<&'a str, ParseError> {
    let bytes = self.input.as_bytes();
    let start = self.pos;
    let mut i = self.pos;
    let mut in_string: Option<u8> = None;
    let mut escape = false;

    while i < bytes.len() {
      let b = bytes[i];
      if let Some(q) = in_string {
        if escape {
          escape = false;
          i += 1;
          continue;
        }
        if b == b'\\' {
          escape = true;
          i += 1;
          continue;
        }
        if b == q {
          in_string = None;
        }
        i += 1;
        continue;
      }

      match b {
        b'"' | b'\'' => {
          in_string = Some(b);
          i += 1;
        }
        _ if b == closing => {
          let inside = &self.input[start..i];
          self.pos = i + 1;
          return Ok(inside);
        }
        _ => i += 1,
      }
    }

    self.err("unterminated extended attribute list")
  }

  fn parse_type(&mut self) -> Result<IdlType, ParseError> {
    let annotations = self.parse_annotations()?;
    let mut ty = self.parse_type_without_annotations()?;
    self.skip_ws();
    if self.consume_byte(b'?') {
      ty = IdlType::Nullable(Box::new(ty));
    }
    if !annotations.is_empty() {
      ty = IdlType::Annotated {
        annotations,
        inner: Box::new(ty),
      };
    }
    Ok(ty)
  }

  fn parse_type_without_annotations(&mut self) -> Result<IdlType, ParseError> {
    self.skip_ws();
    match self.peek_byte() {
      Some(b'(') => self.parse_union_type(),
      _ => self.parse_non_union_type(),
    }
  }

  fn parse_union_type(&mut self) -> Result<IdlType, ParseError> {
    self.expect_byte(b'(', "'('")?;
    let first = self.parse_type()?;
    if !self.consume_keyword("or") {
      return self.err("expected 'or' in union type");
    }
    let second = self.parse_type()?;
    let mut members = vec![first, second];
    while self.consume_keyword("or") {
      members.push(self.parse_type()?);
    }
    self.expect_byte(b')', "')'")?;
    Ok(IdlType::Union(members))
  }

  fn parse_non_union_type(&mut self) -> Result<IdlType, ParseError> {
    if self.consume_keyword("sequence") {
      let inner = self.parse_generic_one()?;
      return Ok(IdlType::Sequence(Box::new(inner)));
    }
    if self.consume_keyword("FrozenArray") {
      let inner = self.parse_generic_one()?;
      return Ok(IdlType::FrozenArray(Box::new(inner)));
    }
    if self.consume_keyword("async") {
      if !self.consume_keyword("sequence") {
        return self.err("expected `sequence` after `async`");
      }
      let inner = self.parse_generic_one()?;
      return Ok(IdlType::AsyncSequence(Box::new(inner)));
    }
    if self.consume_keyword("async_sequence") {
      let inner = self.parse_generic_one()?;
      return Ok(IdlType::AsyncSequence(Box::new(inner)));
    }
    if self.consume_keyword("record") {
      self.expect_byte(b'<', "'<'")?;
      let key = self.parse_type()?;
      self.expect_byte(b',', "','")?;
      let value = self.parse_type()?;
      self.expect_byte(b'>', "'>'")?;
      return Ok(IdlType::Record(Box::new(key), Box::new(value)));
    }
    if self.consume_keyword("Promise") {
      let inner = self.parse_generic_one()?;
      return Ok(IdlType::Promise(Box::new(inner)));
    }

    if self.consume_keyword("any") {
      return Ok(IdlType::Any);
    }
    if self.consume_keyword("undefined") {
      return Ok(IdlType::Undefined);
    }
    if self.consume_keyword("boolean") {
      return Ok(IdlType::Boolean);
    }
    if self.consume_keyword("bigint") {
      return Ok(IdlType::BigInt);
    }
    if self.consume_keyword("DOMString") {
      return Ok(IdlType::String(StringType::DomString));
    }
    if self.consume_keyword("ByteString") {
      return Ok(IdlType::String(StringType::ByteString));
    }
    if self.consume_keyword("USVString") {
      return Ok(IdlType::String(StringType::UsvString));
    }
    if self.consume_keyword("object") {
      return Ok(IdlType::Object);
    }
    if self.consume_keyword("symbol") {
      return Ok(IdlType::Symbol);
    }

    if self.consume_keyword("unsigned") {
      let ty = self.parse_unsigned_integer_type()?;
      return Ok(IdlType::Numeric(ty));
    }
    if self.consume_keyword("unrestricted") {
      let ty = self.parse_unrestricted_float_type()?;
      return Ok(IdlType::Numeric(ty));
    }

    if self.consume_keyword("byte") {
      return Ok(IdlType::Numeric(NumericType::Byte));
    }
    if self.consume_keyword("octet") {
      return Ok(IdlType::Numeric(NumericType::Octet));
    }
    if self.consume_keyword("short") {
      return Ok(IdlType::Numeric(NumericType::Short));
    }
    if self.consume_keyword("long") {
      if self.consume_keyword("long") {
        return Ok(IdlType::Numeric(NumericType::LongLong));
      }
      return Ok(IdlType::Numeric(NumericType::Long));
    }
    if self.consume_keyword("float") {
      return Ok(IdlType::Numeric(NumericType::Float));
    }
    if self.consume_keyword("double") {
      return Ok(IdlType::Numeric(NumericType::Double));
    }

    let name = self.parse_identifier()?;
    Ok(IdlType::Named(NamedType {
      name,
      kind: NamedTypeKind::Unresolved,
    }))
  }

  fn parse_unsigned_integer_type(&mut self) -> Result<NumericType, ParseError> {
    if self.consume_keyword("short") {
      return Ok(NumericType::UnsignedShort);
    }
    if self.consume_keyword("long") {
      if self.consume_keyword("long") {
        return Ok(NumericType::UnsignedLongLong);
      }
      return Ok(NumericType::UnsignedLong);
    }
    self.err("expected 'short' or 'long' after 'unsigned'")
  }

  fn parse_unrestricted_float_type(&mut self) -> Result<NumericType, ParseError> {
    if self.consume_keyword("float") {
      return Ok(NumericType::UnrestrictedFloat);
    }
    if self.consume_keyword("double") {
      return Ok(NumericType::UnrestrictedDouble);
    }
    self.err("expected 'float' or 'double' after 'unrestricted'")
  }

  fn parse_generic_one(&mut self) -> Result<IdlType, ParseError> {
    self.expect_byte(b'<', "'<'")?;
    let inner = self.parse_type()?;
    self.expect_byte(b'>', "'>'")?;
    Ok(inner)
  }
}

fn is_ident_start_byte(b: u8) -> bool {
  b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue_byte(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_'
}

fn parse_ext_attr_list(content: &str) -> Vec<TypeAnnotation> {
  let mut out = Vec::new();
  for item in split_top_level_commas(content) {
    let item = item.trim();
    if item.is_empty() {
      continue;
    }
    out.push(parse_one_ext_attr(item));
  }
  out
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
  let mut out = Vec::new();
  let mut start = 0usize;
  let mut depth = 0u32;
  let mut in_string: Option<u8> = None;
  let mut escape = false;
  let bytes = s.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];
    if let Some(q) = in_string {
      if escape {
        escape = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        escape = true;
        i += 1;
        continue;
      }
      if b == q {
        in_string = None;
      }
      i += 1;
      continue;
    }
    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'(' => {
        depth += 1;
        i += 1;
      }
      b')' => {
        depth = depth.saturating_sub(1);
        i += 1;
      }
      b',' if depth == 0 => {
        out.push(&s[start..i]);
        start = i + 1;
        i += 1;
      }
      _ => i += 1,
    }
  }
  out.push(&s[start..]);
  out
}

fn parse_one_ext_attr(item: &str) -> TypeAnnotation {
  let item = item.trim();
  let (name, rhs) = if let Some((lhs, rhs)) = item.split_once('=') {
    (lhs.trim(), Some(rhs.trim().to_string()))
  } else {
    let name = item
      .split_once('(')
      .map(|(lhs, _)| lhs.trim())
      .unwrap_or(item);
    (name, None)
  };

  match name {
    "Clamp" => TypeAnnotation::Clamp,
    "EnforceRange" => TypeAnnotation::EnforceRange,
    "LegacyNullToEmptyString" => TypeAnnotation::LegacyNullToEmptyString,
    "LegacyTreatNonObjectAsNull" => TypeAnnotation::LegacyTreatNonObjectAsNull,
    "AllowShared" => TypeAnnotation::AllowShared,
    "AllowResizable" => TypeAnnotation::AllowResizable,
    other => TypeAnnotation::Other {
      name: other.to_string(),
      rhs,
    },
  }
}

pub fn parse_idl_type(input: &str) -> Result<(IdlType, &str), ParseError> {
  let mut p = Parser::new(input);
  let ty = p.parse_type()?;
  Ok((ty, p.rest()))
}

pub fn parse_idl_type_complete(input: &str) -> Result<IdlType, ParseError> {
  let (ty, rest) = parse_idl_type(input)?;
  if rest.trim().is_empty() {
    Ok(ty)
  } else {
    Err(ParseError {
      message: "unexpected trailing input".to_string(),
      offset: input.len() - rest.len(),
    })
  }
}

pub fn parse_default_value(input: &str) -> Result<DefaultValue, ParseError> {
  let mut p = Parser::new(input);
  p.skip_ws();

  let value = match p.peek_byte() {
    Some(b'"') | Some(b'\'') => parse_string_literal(&mut p)?,
    Some(b'[') => {
      p.pos += 1;
      p.skip_ws();
      if p.peek_byte() != Some(b']') {
        return p.err("expected ']'");
      }
      p.pos += 1;
      DefaultValue::EmptySequence
    }
    Some(b'{') => {
      p.pos += 1;
      p.skip_ws();
      if p.peek_byte() != Some(b'}') {
        return p.err("expected '}'");
      }
      p.pos += 1;
      DefaultValue::EmptyDictionary
    }
    _ => {
      if p.consume_keyword("true") {
        DefaultValue::Boolean(true)
      } else if p.consume_keyword("false") {
        DefaultValue::Boolean(false)
      } else if p.consume_keyword("null") {
        DefaultValue::Null
      } else if p.consume_keyword("undefined") {
        DefaultValue::Undefined
      } else {
        parse_numeric_literal(&mut p)?
      }
    }
  };

  p.skip_ws();
  if !p.is_eof() {
    return Err(ParseError {
      message: "unexpected trailing input".to_string(),
      offset: p.pos,
    });
  }
  Ok(value)
}

fn parse_string_literal(p: &mut Parser<'_>) -> Result<DefaultValue, ParseError> {
  let quote = p.peek_byte().unwrap();
  p.pos += 1;
  let bytes = p.input.as_bytes();
  let mut out = String::new();
  let mut i = p.pos;
  while i < bytes.len() {
    let b = bytes[i];
    if b == quote {
      p.pos = i + 1;
      return Ok(DefaultValue::String(out));
    }
    if b == b'\\' {
      i += 1;
      if i >= bytes.len() {
        return p.err("unterminated escape sequence in string literal");
      }
      let esc = bytes[i];
      match esc {
        b'\\' => out.push('\\'),
        b'"' => out.push('"'),
        b'\'' => out.push('\''),
        b'n' => out.push('\n'),
        b'r' => out.push('\r'),
        b't' => out.push('\t'),
        b'b' => out.push('\u{0008}'),
        b'f' => out.push('\u{000C}'),
        b'0' => out.push('\0'),
        b'u' => {
          if bytes.get(i + 1) == Some(&b'{') {
            i += 2;
            let start = i;
            while i < bytes.len() && bytes[i] != b'}' {
              i += 1;
            }
            if i >= bytes.len() {
              return p.err("unterminated \\u{...} escape");
            }
            let hex = std::str::from_utf8(&bytes[start..i]).map_err(|_| ParseError {
              message: "invalid UTF-8 in escape".to_string(),
              offset: start,
            })?;
            let cp = u32::from_str_radix(hex, 16).map_err(|_| ParseError {
              message: "invalid hex in \\u{...} escape".to_string(),
              offset: start,
            })?;
            let ch = char::from_u32(cp).ok_or(ParseError {
              message: "invalid code point in \\u{...} escape".to_string(),
              offset: start,
            })?;
            out.push(ch);
          } else {
            let start = i + 1;
            let end = start + 4;
            if end > bytes.len() {
              return p.err("unterminated \\uXXXX escape");
            }
            let hex = std::str::from_utf8(&bytes[start..end]).map_err(|_| ParseError {
              message: "invalid UTF-8 in escape".to_string(),
              offset: start,
            })?;
            let cp = u16::from_str_radix(hex, 16).map_err(|_| ParseError {
              message: "invalid hex in \\uXXXX escape".to_string(),
              offset: start,
            })?;
            out.push(char::from_u32(cp as u32).unwrap_or('\u{FFFD}'));
            i = end - 1;
          }
        }
        b'x' => {
          let start = i + 1;
          let end = start + 2;
          if end > bytes.len() {
            return p.err("unterminated \\xXX escape");
          }
          let hex = std::str::from_utf8(&bytes[start..end]).map_err(|_| ParseError {
            message: "invalid UTF-8 in escape".to_string(),
            offset: start,
          })?;
          let cp = u8::from_str_radix(hex, 16).map_err(|_| ParseError {
            message: "invalid hex in \\xXX escape".to_string(),
            offset: start,
          })?;
          out.push(cp as char);
          i = end - 1;
        }
        other => out.push(other as char),
      }
      i += 1;
      continue;
    }
    if b.is_ascii() {
      out.push(b as char);
      i += 1;
    } else {
      let rest = &p.input[i..];
      let ch = rest.chars().next().expect("non-empty");
      out.push(ch);
      i += ch.len_utf8();
    }
  }

  p.err("unterminated string literal")
}

fn parse_numeric_literal(p: &mut Parser<'_>) -> Result<DefaultValue, ParseError> {
  p.skip_ws();
  let start = p.pos;
  let bytes = p.input.as_bytes();
  while p.pos < bytes.len() && !bytes[p.pos].is_ascii_whitespace() {
    p.pos += 1;
  }
  let token = &p.input[start..p.pos];
  if token.is_empty() {
    return p.err("expected default value");
  }

  match token {
    "Infinity" => {
      return Ok(DefaultValue::Number(NumericLiteral::Infinity {
        negative: false,
      }))
    }
    "-Infinity" => {
      return Ok(DefaultValue::Number(NumericLiteral::Infinity {
        negative: true,
      }))
    }
    "NaN" => return Ok(DefaultValue::Number(NumericLiteral::NaN)),
    _ => {}
  }

  if token.bytes().any(|b| b == b'.' || b == b'e' || b == b'E') {
    token.parse::<f64>().map_err(|_| ParseError {
      message: "invalid decimal literal".to_string(),
      offset: start,
    })?;
    return Ok(DefaultValue::Number(NumericLiteral::Decimal(
      token.to_string(),
    )));
  }

  if is_valid_integer_literal(token) {
    return Ok(DefaultValue::Number(NumericLiteral::Integer(
      token.to_string(),
    )));
  }

  Err(ParseError {
    message: "invalid default value token".to_string(),
    offset: start,
  })
}

fn is_valid_integer_literal(token: &str) -> bool {
  let token = token.trim();
  if token.is_empty() {
    return false;
  }

  // WebIDL integer token regex:
  // -?([1-9][0-9]*|0[Xx][0-9A-Fa-f]+|0[0-7]*)
  let mut signless = token;
  if let Some(rest) = signless.strip_prefix('-') {
    signless = rest;
  } else if signless.starts_with('+') {
    // Unlike JavaScript numeric literals, WebIDL `integer` does not allow a leading `+`.
    return false;
  }

  if let Some(hex) = signless
    .strip_prefix("0x")
    .or_else(|| signless.strip_prefix("0X"))
  {
    return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
  }

  if signless.starts_with('0') {
    // Octal (including "0").
    return signless.bytes().all(|b| matches!(b, b'0'..=b'7'));
  }

  let bytes = signless.as_bytes();
  let Some(first) = bytes.first().copied() else {
    return false;
  };
  if !matches!(first, b'1'..=b'9') {
    return false;
  }
  bytes.iter().skip(1).all(|b| b.is_ascii_digit())
}
