use crate::webidl::ast::{
  Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember, SpecialOperation,
};
use crate::webidl::ExtendedAttribute;
use anyhow::{anyhow, bail, Result};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
  Ident(String),
  Number(String),
  String(String),
  Ellipsis,
  Punct(char),
  Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
  kind: TokenKind,
  span: Range<usize>,
}

fn is_delim_byte(b: u8) -> bool {
  b.is_ascii_whitespace()
    || matches!(
      b,
      b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'<' | b'>' | b',' | b';' | b'?' | b'='
    )
}

fn lex(input: &str) -> Result<Vec<Token>> {
  let bytes = input.as_bytes();
  let mut out = Vec::<Token>::new();
  let mut i = 0usize;

  while i < bytes.len() {
    let b = bytes[i];
    if b.is_ascii_whitespace() {
      i += 1;
      continue;
    }

    // Line comment.
    if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
      i += 2;
      while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
      }
      continue;
    }
    // Block comment.
    if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
      i += 2;
      while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
          i += 2;
          break;
        }
        i += 1;
      }
      continue;
    }

    // Ellipsis.
    if b == b'.' && i + 2 < bytes.len() && bytes[i + 1] == b'.' && bytes[i + 2] == b'.' {
      out.push(Token {
        kind: TokenKind::Ellipsis,
        span: i..i + 3,
      });
      i += 3;
      continue;
    }

    // Punctuation.
    if matches!(
      b,
      b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'<' | b'>' | b',' | b';' | b'?' | b'='
    ) {
      out.push(Token {
        kind: TokenKind::Punct(b as char),
        span: i..i + 1,
      });
      i += 1;
      continue;
    }

    // String literal.
    if b == b'"' || b == b'\'' {
      let quote = b;
      let start = i;
      i += 1;
      let mut s = String::new();
      let mut escape = false;
      while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if escape {
          // Keep escapes simple: take the next char verbatim.
          s.push(b as char);
          escape = false;
          continue;
        }
        if b == b'\\' {
          escape = true;
          continue;
        }
        if b == quote {
          break;
        }
        s.push(b as char);
      }
      if i > bytes.len() || bytes[i.saturating_sub(1)] != quote {
        bail!(format_error(input, start, "unterminated string literal"));
      }
      out.push(Token {
        kind: TokenKind::String(s),
        span: start..i,
      });
      continue;
    }

    // Identifier.
    if b.is_ascii_alphabetic() || b == b'_' {
      let start = i;
      i += 1;
      while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' {
          i += 1;
        } else {
          break;
        }
      }
      out.push(Token {
        kind: TokenKind::Ident(input[start..i].to_string()),
        span: start..i,
      });
      continue;
    }

    // Number literal (used by defaults/consts). Keep as raw text.
    if b.is_ascii_digit() || (b == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()) {
      let start = i;
      i += 1;
      while i < bytes.len() && !is_delim_byte(bytes[i]) {
        i += 1;
      }
      out.push(Token {
        kind: TokenKind::Number(input[start..i].to_string()),
        span: start..i,
      });
      continue;
    }

    bail!(format_error(
      input,
      i,
      &format!("unexpected character `{}`", b as char)
    ));
  }

  out.push(Token {
    kind: TokenKind::Eof,
    span: input.len()..input.len(),
  });
  Ok(out)
}

fn format_error(input: &str, pos: usize, msg: &str) -> anyhow::Error {
  let pos = pos.min(input.len());
  let line_start = input[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
  let line_end = input[pos..]
    .find('\n')
    .map(|i| pos + i)
    .unwrap_or(input.len());
  let line = &input[line_start..line_end];
  let col = pos - line_start;
  let mut caret = String::new();
  caret.push_str(&" ".repeat(col));
  caret.push('^');
  anyhow!("{msg} at byte {pos}\n{line}\n{caret}")
}

struct Parser<'a> {
  input: &'a str,
  tokens: Vec<Token>,
  idx: usize,
}

impl<'a> Parser<'a> {
  fn new(input: &'a str) -> Result<Self> {
    Ok(Self {
      input,
      tokens: lex(input)?,
      idx: 0,
    })
  }

  fn peek(&self) -> &Token {
    &self.tokens[self.idx]
  }

  fn next(&mut self) -> &Token {
    let tok = &self.tokens[self.idx];
    if tok.kind != TokenKind::Eof {
      self.idx += 1;
    }
    tok
  }

  fn eat_punct(&mut self, ch: char) -> bool {
    matches!(&self.peek().kind, TokenKind::Punct(c) if *c == ch)
      .then(|| self.next())
      .is_some()
  }

  fn expect_punct(&mut self, ch: char) -> Result<()> {
    if self.eat_punct(ch) {
      return Ok(());
    }
    bail!(self.error_here(&format!("expected `{ch}`")));
  }

  fn eat_ident(&mut self, s: &str) -> bool {
    matches!(&self.peek().kind, TokenKind::Ident(id) if id == s)
      .then(|| self.next())
      .is_some()
  }

  fn parse_ident(&mut self) -> Result<String> {
    let tok = self.peek().clone();
    match tok.kind {
      TokenKind::Ident(s) => {
        self.next();
        Ok(s)
      }
      _ => bail!(self.error_here("expected identifier")),
    }
  }

  fn eat_ellipsis(&mut self) -> bool {
    matches!(&self.peek().kind, TokenKind::Ellipsis)
      .then(|| self.next())
      .is_some()
  }

  fn error_here(&self, msg: &str) -> anyhow::Error {
    format_error(self.input, self.peek().span.start, msg)
  }

  fn expect_eof(&mut self) -> Result<()> {
    if matches!(self.peek().kind, TokenKind::Eof) {
      return Ok(());
    }
    bail!(self.error_here("unexpected trailing tokens"));
  }

}

pub fn parse_idl_type(input: &str) -> Result<IdlType> {
  let mut p = Parser::new(input.trim())?;
  let ty = parse_type(&mut p)?;
  p.expect_eof()?;
  Ok(ty)
}

fn parse_type(p: &mut Parser<'_>) -> Result<IdlType> {
  // Union member types can start with an extended-attributes list, e.g.
  // `(TrustedHTML or [LegacyNullToEmptyString] DOMString)`.
  //
  // Most places in the pipeline ignore these attributes, but some (notably
  // `LegacyNullToEmptyString`) affect runtime conversion semantics, so we retain them in the AST.
  let mut ext_attrs = Vec::<ExtendedAttribute>::new();
  while matches!(&p.peek().kind, TokenKind::Punct('[')) {
    ext_attrs.extend(parse_ext_attr_block(p)?);
  }

  let mut base = if p.eat_punct('(') {
    let mut members = Vec::new();
    members.push(parse_type(p)?);
    if !p.eat_ident("or") {
      bail!(p.error_here("expected `or` in union type"));
    }
    loop {
      members.push(parse_type(p)?);
      if p.eat_ident("or") {
        continue;
      }
      break;
    }
    p.expect_punct(')')?;
    IdlType::Union(members)
  } else {
    parse_non_union_type(p)?
  };

  if p.eat_punct('?') {
    base = IdlType::Nullable(Box::new(base));
  }

  if !ext_attrs.is_empty() {
    base = IdlType::Annotated {
      ext_attrs,
      inner: Box::new(base),
    };
  }

  Ok(base)
}

fn parse_non_union_type(p: &mut Parser<'_>) -> Result<IdlType> {
  let first = p.parse_ident()?;

  // Multi-word numeric types.
  let builtin = match first.as_str() {
    "undefined" => Some(BuiltinType::Undefined),
    "any" => Some(BuiltinType::Any),
    "boolean" => Some(BuiltinType::Boolean),
    "byte" => Some(BuiltinType::Byte),
    "octet" => Some(BuiltinType::Octet),
    "short" => Some(BuiltinType::Short),
    "long" => {
      // `long long`
      if p.eat_ident("long") {
        Some(BuiltinType::LongLong)
      } else {
        Some(BuiltinType::Long)
      }
    }
    "unsigned" => {
      if p.eat_ident("short") {
        Some(BuiltinType::UnsignedShort)
      } else if p.eat_ident("long") {
        if p.eat_ident("long") {
          Some(BuiltinType::UnsignedLongLong)
        } else {
          Some(BuiltinType::UnsignedLong)
        }
      } else {
        bail!(p.error_here("expected `short` or `long` after `unsigned`"));
      }
    }
    "float" => Some(BuiltinType::Float),
    "double" => Some(BuiltinType::Double),
    "unrestricted" => {
      if p.eat_ident("float") {
        Some(BuiltinType::UnrestrictedFloat)
      } else if p.eat_ident("double") {
        Some(BuiltinType::UnrestrictedDouble)
      } else {
        bail!(p.error_here("expected `float` or `double` after `unrestricted`"));
      }
    }
    "DOMString" => Some(BuiltinType::DOMString),
    "USVString" => Some(BuiltinType::USVString),
    "ByteString" => Some(BuiltinType::ByteString),
    "object" => Some(BuiltinType::Object),
    _ => None,
  };

  if let Some(b) = builtin {
    return Ok(IdlType::Builtin(b));
  }

  // Generic types.
  match first.as_str() {
    "sequence" => {
      if p.eat_punct('<') {
        let inner = parse_type(p)?;
        p.expect_punct('>')?;
        return Ok(IdlType::Sequence(Box::new(inner)));
      }
    }
    "FrozenArray" => {
      if p.eat_punct('<') {
        let inner = parse_type(p)?;
        p.expect_punct('>')?;
        return Ok(IdlType::FrozenArray(Box::new(inner)));
      }
    }
    "Promise" => {
      if p.eat_punct('<') {
        let inner = parse_type(p)?;
        p.expect_punct('>')?;
        return Ok(IdlType::Promise(Box::new(inner)));
      }
    }
    "record" => {
      if p.eat_punct('<') {
        let key = parse_type(p)?;
        p.expect_punct(',')?;
        let value = parse_type(p)?;
        p.expect_punct('>')?;
        return Ok(IdlType::Record {
          key: Box::new(key),
          value: Box::new(value),
        });
      }
    }
    _ => {}
  }

  Ok(IdlType::Named(first))
}

pub fn parse_interface_member(input: &str) -> Result<InterfaceMember> {
  let mut s = input.trim();
  s = s.strip_suffix(';').unwrap_or(s).trim();

  // WebIDL allows a `stringifier;` shorthand form (e.g. `Range` in the WHATWG DOM snapshot). This is
  // equivalent to `stringifier DOMString toString();`.
  if s == "stringifier" {
    return Ok(InterfaceMember::Operation {
      name: Some("toString".to_string()),
      return_type: IdlType::Builtin(BuiltinType::DOMString),
      arguments: Vec::new(),
      static_: false,
      stringifier: true,
      special: None,
    });
  }

  let mut p = Parser::new(s)?;

  // Fast-path variants that must come first.
  if p.eat_ident("async") {
    if p.eat_ident("iterable") {
      let member = parse_iterable(&mut p, true)?;
      p.expect_eof()?;
      return Ok(member);
    }
    bail!(p.error_here("expected `iterable` after `async`"));
  }

  if p.eat_ident("iterable") {
    let member = parse_iterable(&mut p, false)?;
    p.expect_eof()?;
    return Ok(member);
  }

  if p.eat_ident("constructor") {
    let arguments = parse_argument_list(&mut p)?;
    p.expect_eof()?;
    return Ok(InterfaceMember::Constructor { arguments });
  }

  if p.eat_ident("const") {
    let type_ = parse_type(&mut p)?;
    let name = p.parse_ident()?;
    p.expect_punct('=')?;
    let value = parse_literal(&mut p)?;
    p.expect_eof()?;
    return Ok(InterfaceMember::Constant { name, type_, value });
  }

  // Common modifier prefixes.
  let mut static_ = false;
  let mut stringifier = false;
  loop {
    if p.eat_ident("static") {
      static_ = true;
      continue;
    }
    if p.eat_ident("stringifier") {
      stringifier = true;
      continue;
    }
    break;
  }

  // Attribute-only flags. If we don't see `attribute` after consuming them, treat as unparsed.
  let mut readonly = false;
  let mut inherit = false;
  loop {
    if p.eat_ident("readonly") {
      readonly = true;
      continue;
    }
    if p.eat_ident("inherit") {
      inherit = true;
      continue;
    }
    break;
  }

  if p.eat_ident("attribute") {
    let type_ = parse_type(&mut p)?;
    let name = p.parse_ident()?;
    p.expect_eof()?;
    return Ok(InterfaceMember::Attribute {
      name,
      type_,
      readonly,
      inherit,
      stringifier,
      static_,
    });
  }

  // Special operations.
  let special = if p.eat_ident("getter") {
    Some(SpecialOperation::Getter)
  } else if p.eat_ident("setter") {
    Some(SpecialOperation::Setter)
  } else if p.eat_ident("deleter") {
    Some(SpecialOperation::Deleter)
  } else {
    None
  };

  // If we consumed attribute-only modifiers but did not see `attribute`, this is likely something
  // else (`readonly maplike<...>`). Don't misparse it as an operation.
  if (readonly || inherit) && special.is_none() {
    return Ok(InterfaceMember::Unparsed { raw: s.to_string() });
  }

  // Operation signature.
  let return_type = parse_type(&mut p)?;
  let name = match p.peek().kind.clone() {
    TokenKind::Ident(n) => {
      if !matches!(
        p.tokens.get(p.idx + 1).map(|t| &t.kind),
        Some(TokenKind::Punct('('))
      ) {
        bail!(p.error_here("expected `(` after operation name"));
      }
      p.next();
      Some(n)
    }
    TokenKind::Punct('(') => None,
    _ => bail!(p.error_here("expected operation name or `(`")),
  };

  let arguments = parse_argument_list(&mut p)?;
  p.expect_eof()?;
  Ok(InterfaceMember::Operation {
    name,
    return_type,
    arguments,
    static_,
    stringifier,
    special,
  })
}

fn parse_iterable(p: &mut Parser<'_>, async_: bool) -> Result<InterfaceMember> {
  p.expect_punct('<')?;
  let first = parse_type(p)?;
  let (key_type, value_type) = if p.eat_punct(',') {
    let value = parse_type(p)?;
    (Some(first), value)
  } else {
    (None, first)
  };
  p.expect_punct('>')?;
  Ok(InterfaceMember::Iterable {
    async_,
    key_type,
    value_type,
  })
}

fn parse_argument_list(p: &mut Parser<'_>) -> Result<Vec<Argument>> {
  p.expect_punct('(')?;
  if p.eat_punct(')') {
    return Ok(Vec::new());
  }

  let mut args = Vec::new();
  loop {
    args.push(parse_argument(p)?);
    if p.eat_punct(',') {
      continue;
    }
    break;
  }
  p.expect_punct(')')?;
  Ok(args)
}

fn parse_argument(p: &mut Parser<'_>) -> Result<Argument> {
  let mut ext_attrs = Vec::<ExtendedAttribute>::new();
  while matches!(&p.peek().kind, TokenKind::Punct('[')) {
    ext_attrs.extend(parse_ext_attr_block(p)?);
  }

  let optional = p.eat_ident("optional");
  let type_ = parse_type(p)?;
  let variadic = p.eat_ellipsis();
  let name = p.parse_ident()?;
  let default = if p.eat_punct('=') {
    Some(parse_literal(p)?)
  } else {
    None
  };
  Ok(Argument {
    ext_attrs,
    name,
    type_,
    optional,
    variadic,
    default,
  })
}

fn parse_ext_attr_block(p: &mut Parser<'_>) -> Result<Vec<ExtendedAttribute>> {
  p.expect_punct('[')?;
  let mut depth = 1u32;
  let mut content = String::new();

  loop {
    let tok = p.next().clone();
    match tok.kind {
      TokenKind::Punct('[') => {
        depth += 1;
        content.push_str(&p.input[tok.span]);
      }
      TokenKind::Punct(']') => {
        depth -= 1;
        if depth == 0 {
          break;
        }
        content.push(']');
      }
      TokenKind::Eof => bail!(p.error_here("unterminated extended attribute block")),
      _ => content.push_str(&p.input[tok.span]),
    }
  }

  Ok(super::parse_ext_attr_list(&content))
}

fn parse_literal(p: &mut Parser<'_>) -> Result<IdlLiteral> {
  let tok = p.next().clone();
  match tok.kind {
    TokenKind::Ident(id) => match id.as_str() {
      "null" => Ok(IdlLiteral::Null),
      "undefined" => Ok(IdlLiteral::Undefined),
      "true" => Ok(IdlLiteral::Boolean(true)),
      "false" => Ok(IdlLiteral::Boolean(false)),
      _ => Ok(IdlLiteral::Identifier(id)),
    },
    TokenKind::Number(n) => Ok(IdlLiteral::Number(n)),
    TokenKind::String(s) => Ok(IdlLiteral::String(s)),
    TokenKind::Punct('{') => {
      if !p.eat_punct('}') {
        bail!(p.error_here("expected `}` for `{}` literal"));
      }
      Ok(IdlLiteral::EmptyObject)
    }
    TokenKind::Punct('[') => {
      if !p.eat_punct(']') {
        bail!(p.error_here("expected `]` for `[]` literal"));
      }
      Ok(IdlLiteral::EmptyArray)
    }
    _ => bail!(format_error(p.input, tok.span.start, "expected literal")),
  }
}
