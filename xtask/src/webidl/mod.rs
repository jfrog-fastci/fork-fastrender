//! Minimal WebIDL support used by the bindings/codegen pipeline.
//!
//! This module intentionally implements a small, forgiving subset of WebIDL parsing sufficient
//! for WHATWG Bikeshed sources and deterministic resolution (partials/includes). We do *not*
//! attempt full semantic validation here; the goal is to provide a stable, queryable API surface.
//!
//! The resolver lives in [`resolve`].

use anyhow::Result;

pub mod analyze;
pub mod ast;
pub mod generate;
pub mod load;
pub mod overload_ir;
pub mod parse;
pub mod parse_dictionary;
pub mod resolve;
pub mod type_resolution;
pub mod semantic;

pub use analyze::{
  analyze_resolved_world, AnalyzedInterface, AnalyzedInterfaceMember, AnalyzedInterfaceMixin,
  AnalyzedWebIdlWorld,
};
pub use ast::{Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember, SpecialOperation};
pub use parse::{parse_idl_type, parse_interface_member};
pub use parse_dictionary::{parse_dictionary_member, ParsedDictionaryMember};
pub use semantic::{SemanticDiagnostic, SemanticWorld};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedAttribute {
  pub name: String,
  pub value: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedWebIdlWorld {
  /// Definitions in file/statement appearance order.
  pub definitions: Vec<ParsedDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedDefinition {
  Interface(ParsedInterface),
  InterfaceMixin(ParsedInterfaceMixin),
  Includes(ParsedIncludes),
  Dictionary(ParsedDictionary),
  Enum(ParsedEnum),
  Typedef(ParsedTypedef),
  /// A `callback Foo = ...;` definition.
  Callback(ParsedCallback),
  /// Unrecognized/unsupported top-level definition. Stored so callers can decide whether to warn.
  Other {
    raw: String,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIncludes {
  pub target: String,
  pub mixin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInterface {
  pub name: String,
  pub inherits: Option<String>,
  pub partial: bool,
  pub callback: bool,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<ParsedMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInterfaceMixin {
  pub name: String,
  pub partial: bool,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<ParsedMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDictionary {
  pub name: String,
  pub inherits: Option<String>,
  pub partial: bool,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<ParsedMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEnum {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTypedef {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  /// The RHS type as a raw string (no semantic validation yet).
  pub type_: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCallback {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  /// The RHS type as a raw string (no semantic validation yet).
  pub type_: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMember {
  pub ext_attrs: Vec<ExtendedAttribute>,
  /// Member text without trailing `;`.
  pub raw: String,
  /// Best-effort extracted member name (for codegen queries).
  pub name: Option<String>,
}

/// Extract WebIDL blocks from a spec source file.
///
/// Supports:
/// - Bikeshed sources: `<pre class=idl> ... </pre>`
/// - WHATWG HTML sources: `<code class=idl> ... </code>` (typically nested in `<pre>`)
///
/// Returned blocks are plain WebIDL text with basic HTML entities decoded (`&lt;`, `&gt;`, `&amp;`,
/// `&quot;`, `&#39;`, `&nbsp;`). For HTML `<code class=idl>` blocks, inline HTML tags (e.g. `<span>`,
/// `<dfn>`, `<var>`) are stripped while preserving inner text.
pub fn extract_webidl_blocks(source: &str) -> Vec<String> {
  let mut out = Vec::new();
  out.extend(extract_webidl_blocks_from_bikeshed(source));
  out.extend(extract_webidl_blocks_from_whatwg_html(source));
  out
}

/// Extract WebIDL `<pre class=idl> ... </pre>` blocks from a Bikeshed source file (`*.bs`).
///
/// Returns raw IDL blocks with basic HTML entities decoded (`&lt;`, `&gt;`, `&amp;`, `&quot;`, `&#39;`).
pub fn extract_webidl_blocks_from_bikeshed(source: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut in_idl = false;
  let mut current = String::new();

  for line in source.lines() {
    if !in_idl {
      if let Some(idx) = line.find("<pre") {
        // Only treat `<pre>` tags with `class=idl` as Bikeshed IDL blocks.
        //
        // Important: WHATWG HTML embeds IDL in `<pre><code class="idl">...</code></pre>`. The
        // `<pre>` start tag itself typically has no `class`, but the `<code>` tag does. A naive
        // substring search would incorrectly treat those blocks as Bikeshed IDL and capture raw
        // HTML markup/comments, which then breaks statement splitting/parsing.
        let tag_end = find_html_tag_end(line, idx);
        let pre_tag = tag_end.map(|end| &line[idx..=end]).unwrap_or(&line[idx..]);
        let is_idl_pre = if tag_end.is_some() {
          html_start_tag_has_class_token(pre_tag, "idl")
        } else {
          // Bikeshed sources occasionally omit the closing `>` when the IDL begins immediately
          // after the opening `<pre class=idl` tag, e.g.:
          //   `<pre class=idl>[Exposed=(Window,Worker)]`
          // In that case, inspect only the start tag fragment (up to the first `[` if present).
          let tag_head = pre_tag.split_once('[').map(|(h, _)| h).unwrap_or(pre_tag);
          tag_head.contains("class=idl")
            || tag_head.contains("class='idl'")
            || tag_head.contains("class=\"idl\"")
        };

        if is_idl_pre {
          // Everything after the first '>' is IDL content (some blocks place it on the same line).
          let tag = &line[idx..];
          let after = if let Some(gt) = tag.find('>') {
            Some(&tag[gt + 1..])
          } else if let Some(bracket) = tag.find('[') {
            Some(&tag[bracket..])
          } else {
            None
          };
          if let Some(after) = after {
            if !after.is_empty() {
              current.push_str(after);
              current.push('\n');
            }
          }
          in_idl = true;
          continue;
        }
      }
      continue;
    }

    if let Some(end) = line.find("</pre>") {
      current.push_str(&line[..end]);
      current.push('\n');
      out.push(decode_basic_html_entities(current.trim()).to_string());
      current.clear();
      in_idl = false;
      continue;
    }

    current.push_str(line);
    current.push('\n');
  }

  out
}

/// Extract WebIDL blocks from the WHATWG HTML `source` file format.
///
/// WHATWG HTML embeds IDL in `<pre><code class="idl"> ... </code></pre>` blocks. The IDL itself
/// includes inline HTML markup (`<dfn>`, `<span>`, `<a>`, …) that must be stripped while
/// preserving inner text.
///
/// Returns raw IDL blocks with basic HTML entities decoded (`&lt;`, `&gt;`, `&amp;`, `&quot;`, `&#39;`,
/// `&nbsp;`).
pub fn extract_webidl_blocks_from_whatwg_html(source: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut i = 0usize;

  while let Some(rel_start) = source[i..].find("<code") {
    let start = i + rel_start;

    // Avoid matching tags like `<codec>`.
    let after_name = start + "<code".len();
    if source
      .get(after_name..)
      .and_then(|s| s.chars().next())
      .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
    {
      i = after_name;
      continue;
    }

    // Skip closing tags.
    if source[start..].starts_with("</code") {
      i = start + "</code".len();
      continue;
    }

    let Some(tag_end) = find_html_tag_end(source, start) else {
      break;
    };
    let tag = &source[start..=tag_end];
    if !html_start_tag_has_class_token(tag, "idl") {
      i = tag_end + 1;
      continue;
    }

    let content_start = tag_end + 1;
    let Some((content_end, close_end)) = find_matching_code_close(source, content_start) else {
      break;
    };
    let raw = &source[content_start..content_end];
    let stripped = strip_html_tags_preserve_text(raw);
    let decoded = decode_basic_html_entities(stripped.trim());
    if !decoded.is_empty() {
      out.push(decoded);
    }

    i = close_end;
  }

  out
}

/// Find the outer `</code>` closing tag corresponding to an opening `<code ...>` tag.
///
/// WHATWG HTML nests `<code>...</code>` tags *inside* `<code class="idl">...</code>` blocks for
/// formatting (e.g. `static <code>Document</code> parse();`). A naive search for the first
/// `</code>` would terminate the IDL block too early.
///
/// Returns `(close_start, close_end)`, where `close_start` is the start index of `</code...>` and
/// `close_end` is the index immediately after the closing `>`.
fn find_matching_code_close(source: &str, content_start: usize) -> Option<(usize, usize)> {
  let mut depth = 1u32;
  let mut scan = content_start;

  while let Some(rel_lt) = source[scan..].find('<') {
    let start = scan + rel_lt;
    let tail = &source[start..];

    if tail.starts_with("</code") {
      let tag_end = find_html_tag_end(source, start)?;
      depth = depth.saturating_sub(1);
      if depth == 0 {
        return Some((start, tag_end + 1));
      }
      scan = tag_end + 1;
      continue;
    }

    if tail.starts_with("<code") {
      let tag_end = find_html_tag_end(source, start)?;
      depth += 1;
      scan = tag_end + 1;
      continue;
    }

    let tag_end = find_html_tag_end(source, start)?;
    scan = tag_end + 1;
  }

  None
}

pub fn parse_webidl(idl: &str) -> Result<ParsedWebIdlWorld> {
  let mut world = ParsedWebIdlWorld::default();
  for stmt in split_top_level_statements(idl) {
    if let Some(def) = parse_definition(&stmt) {
      world.definitions.push(def);
    }
  }
  Ok(world)
}

fn parse_definition(stmt: &str) -> Option<ParsedDefinition> {
  let stmt = strip_trailing_semicolon(stmt).trim();
  if stmt.is_empty() {
    return None;
  }

  let (ext_attrs, mut rest) = parse_leading_ext_attrs(stmt);
  rest = strip_leading_ws_and_comments(rest);

  let mut partial = false;
  let mut callback = false;

  if let Some(after) = consume_keyword(rest, "partial") {
    partial = true;
    rest = strip_leading_ws_and_comments(after);
  }

  if let Some(after) = consume_keyword(rest, "callback") {
    callback = true;
    rest = strip_leading_ws_and_comments(after);
  }

  rest = strip_leading_ws_and_comments(rest);
  if rest.is_empty() {
    return None;
  }

  // `callback interface Foo { ... };`
  if let Some(after) = consume_keyword(rest, "interface") {
    let after = strip_leading_ws_and_comments(after);
    if let Some(after_mixin) = consume_keyword(after, "mixin") {
      // interface mixin
      let after_mixin = strip_leading_ws_and_comments(after_mixin);
      return parse_interface_mixin(after_mixin, partial, ext_attrs);
    }
    // interface (regular or callback interface depending on `callback` flag)
    return parse_interface(after, partial, callback, ext_attrs);
  }

  if let Some(after) = consume_keyword(rest, "dictionary") {
    let after = strip_leading_ws_and_comments(after);
    return parse_dictionary(after, partial, ext_attrs);
  }

  if let Some(after) = consume_keyword(rest, "enum") {
    let after = strip_leading_ws_and_comments(after);
    return parse_enum(after, ext_attrs);
  }

  if let Some(after) = consume_keyword(rest, "typedef") {
    let after = strip_leading_ws_and_comments(after);
    return parse_typedef(after, ext_attrs).map(ParsedDefinition::Typedef);
  }

  if callback {
    // `callback Foo = ...;` (already stripped the `callback` keyword above).
    return parse_callback(rest, ext_attrs).map(ParsedDefinition::Callback);
  }

  // `<A> includes <B>;`
  if let Some(def) = parse_includes(rest) {
    return Some(def);
  }

  Some(ParsedDefinition::Other {
    raw: stmt.to_string(),
  })
}

fn parse_interface(
  header_and_body: &str,
  partial: bool,
  callback: bool,
  ext_attrs: Vec<ExtendedAttribute>,
) -> Option<ParsedDefinition> {
  let (header, body) = extract_curly_body(header_and_body)?;

  let (name, inherits) = parse_name_and_inherits(header)?;

  let members = parse_members(body);
  Some(ParsedDefinition::Interface(ParsedInterface {
    name,
    inherits,
    partial,
    callback,
    ext_attrs,
    members,
  }))
}

fn parse_interface_mixin(
  header_and_body: &str,
  partial: bool,
  ext_attrs: Vec<ExtendedAttribute>,
) -> Option<ParsedDefinition> {
  let (header, body) = extract_curly_body(header_and_body)?;
  let (name, _inherits) = parse_name_and_inherits(header)?;
  let members = parse_members(body);
  Some(ParsedDefinition::InterfaceMixin(ParsedInterfaceMixin {
    name,
    partial,
    ext_attrs,
    members,
  }))
}

fn parse_dictionary(
  header_and_body: &str,
  partial: bool,
  ext_attrs: Vec<ExtendedAttribute>,
) -> Option<ParsedDefinition> {
  let (header, body) = extract_curly_body(header_and_body)?;
  let (name, inherits) = parse_name_and_inherits(header)?;
  let members = parse_members(body);
  Some(ParsedDefinition::Dictionary(ParsedDictionary {
    name,
    inherits,
    partial,
    ext_attrs,
    members,
  }))
}

fn parse_enum(
  header_and_body: &str,
  ext_attrs: Vec<ExtendedAttribute>,
) -> Option<ParsedDefinition> {
  let (header, body) = extract_curly_body(header_and_body)?;
  let (name, _inherits) = parse_name_and_inherits(header)?;
  let values = parse_enum_values(body);
  Some(ParsedDefinition::Enum(ParsedEnum {
    name,
    ext_attrs,
    values,
  }))
}

fn parse_typedef(rest: &str, ext_attrs: Vec<ExtendedAttribute>) -> Option<ParsedTypedef> {
  // `typedef <type> <name>`
  let rest = strip_leading_ws_and_comments(rest).trim();
  if rest.is_empty() {
    return None;
  }
  let (name, type_) = split_trailing_identifier(rest)?;
  Some(ParsedTypedef {
    name: name.to_string(),
    ext_attrs,
    type_: type_.to_string(),
  })
}

fn parse_callback(rest: &str, ext_attrs: Vec<ExtendedAttribute>) -> Option<ParsedCallback> {
  // `<name> = <type>`
  let rest = strip_leading_ws_and_comments(rest).trim();
  let (name, rhs) = rest.split_once('=')?;
  let name = name.trim();
  let rhs = rhs.trim();
  if name.is_empty() || rhs.is_empty() {
    return None;
  }
  Some(ParsedCallback {
    name: name.to_string(),
    ext_attrs,
    type_: rhs.to_string(),
  })
}

fn parse_includes(rest: &str) -> Option<ParsedDefinition> {
  let rest = strip_leading_ws_and_comments(rest).trim();
  let mut iter = rest.split_whitespace();
  let target = iter.next()?;
  if iter.next()? != "includes" {
    return None;
  }
  let mixin = iter.next()?;
  Some(ParsedDefinition::Includes(ParsedIncludes {
    target: target.to_string(),
    mixin: mixin.to_string(),
  }))
}

fn parse_name_and_inherits(header: &str) -> Option<(String, Option<String>)> {
  // `Name` or `Name : Parent`
  let header = strip_leading_ws_and_comments(header).trim();
  let (name, rest) = parse_identifier_prefix(header)?;
  let rest = strip_leading_ws_and_comments(rest).trim_start();
  if let Some(rest) = rest.strip_prefix(':') {
    let rest = strip_leading_ws_and_comments(rest).trim_start();
    let (parent, _rest) = parse_identifier_prefix(rest)?;
    return Some((name.to_string(), Some(parent.to_string())));
  }
  Some((name.to_string(), None))
}

fn parse_members(body: &str) -> Vec<ParsedMember> {
  let mut out = Vec::new();
  for stmt in split_inner_statements(body) {
    let stmt = strip_trailing_semicolon(&stmt).trim();
    if stmt.is_empty() {
      continue;
    }
    let (ext_attrs, rest) = parse_leading_ext_attrs(stmt);
    let raw = strip_leading_ws_and_comments(rest).trim();
    if raw.is_empty() {
      continue;
    }
    out.push(ParsedMember {
      name: extract_member_name(raw),
      ext_attrs,
      raw: raw.to_string(),
    });
  }
  out
}

fn parse_enum_values(body: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut in_string = false;
  let mut escape = false;
  let mut current = String::new();

  for ch in body.chars() {
    if !in_string {
      if ch == '"' {
        in_string = true;
        current.clear();
      }
      continue;
    }

    if escape {
      current.push(ch);
      escape = false;
      continue;
    }
    if ch == '\\' {
      escape = true;
      continue;
    }
    if ch == '"' {
      in_string = false;
      out.push(current.clone());
      current.clear();
      continue;
    }
    current.push(ch);
  }

  out
}

fn decode_basic_html_entities(s: &str) -> String {
  // Order matters: decode the more specific entities first, then `&amp;`.
  s.replace("&lt;", "<")
    .replace("&gt;", ">")
    .replace("&quot;", "\"")
    .replace("&#39;", "'")
    .replace("&nbsp;", " ")
    .replace("&amp;", "&")
}

fn find_html_tag_end(input: &str, start: usize) -> Option<usize> {
  let bytes = input.as_bytes();
  if bytes.get(start)? != &b'<' {
    return None;
  }

  // HTML comments (`<!-- ... -->`) can contain arbitrary text, including `'` and `"` characters.
  // Treat them specially so we don't interpret those as quoted attribute delimiters and accidentally
  // skip the rest of an IDL block.
  if input.get(start..)?.starts_with("<!--") {
    let after = start + "<!--".len();
    let rel_end = input.get(after..)?.find("-->")?;
    return Some(after + rel_end + "-->".len() - 1);
  }

  let mut i = start + 1;
  let mut in_quote: Option<u8> = None;
  while i < bytes.len() {
    let b = bytes[i];
    if let Some(q) = in_quote {
      if b == q {
        in_quote = None;
      }
      i += 1;
      continue;
    }
    match b {
      b'\'' | b'"' => {
        in_quote = Some(b);
        i += 1;
      }
      b'>' => return Some(i),
      _ => i += 1,
    }
  }
  None
}

fn html_start_tag_has_class_token(tag: &str, token: &str) -> bool {
  // Parse a single start tag's attributes; forgiving and sufficient for WHATWG HTML sources.
  let bytes = tag.as_bytes();
  if bytes.first().copied() != Some(b'<') {
    return false;
  }

  let mut i = 1usize;
  // Skip tag name.
  while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
    i += 1;
  }

  while i < bytes.len() {
    // Skip whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }

    // Parse attribute name.
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>'
    {
      i += 1;
    }
    let name = &tag[name_start..i];

    // Skip whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value: Option<&str> = None;
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      if i >= bytes.len() {
        break;
      }

      if bytes[i] == b'"' || bytes[i] == b'\'' {
        let quote = bytes[i];
        i += 1;
        let value_start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = Some(&tag[value_start..i]);
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let value_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = Some(&tag[value_start..i]);
      }
    }

    if name.eq_ignore_ascii_case("class") {
      if let Some(v) = value {
        if v.split_whitespace().any(|t| t == token) {
          return true;
        }
      }
    }
  }

  false
}

fn strip_html_tags_preserve_text(input: &str) -> String {
  let bytes = input.as_bytes();
  let mut out = String::with_capacity(input.len());

  let mut i = 0usize;
  let mut last_text = 0usize;
  while i < bytes.len() {
    if bytes[i] != b'<' {
      i += 1;
      continue;
    }

    // Flush any text before this tag.
    if last_text < i {
      out.push_str(&input[last_text..i]);
    }

    // HTML comment: `<!-- ... -->`.
    //
    // Comments can contain `'` and `"` characters, so don't use the generic tag scanner (which
    // treats those as quoted attribute delimiters).
    if input[i..].starts_with("<!--") {
      if let Some(rel_end) = input[i + "<!--".len()..].find("-->") {
        i = i + "<!--".len() + rel_end + "-->".len();
        last_text = i;
        continue;
      }
      // Unclosed comment: strip to end.
      last_text = input.len();
      break;
    }

    // Skip the tag itself (forgiving).
    //
    // Important: HTML IDL blocks sometimes contain HTML comments (`<!-- ... -->`) for spec notes.
    // Comment bodies can contain `'`/`"` characters that are *not* attribute quotes; treating them
    // as quotes would cause us to scan past the closing `-->` and potentially drop large parts of
    // the IDL block (e.g. `HTMLSelectElement` in WHATWG HTML).
    if input[i..].starts_with("<!--") {
      if let Some(end_rel) = input[i + "<!--".len()..].find("-->") {
        i = i + "<!--".len() + end_rel + "-->".len();
      } else {
        // Unterminated comment; drop the rest.
        i = bytes.len();
      }
      last_text = i;
      continue;
    }

    i += 1;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
      let b = bytes[i];
      if let Some(q) = in_quote {
        if b == q {
          in_quote = None;
        }
        i += 1;
        continue;
      }
      match b {
        b'\'' | b'"' => {
          in_quote = Some(b);
          i += 1;
        }
        b'>' => {
          i += 1;
          break;
        }
        _ => i += 1,
      }
    }

    last_text = i;
  }

  if last_text < input.len() {
    out.push_str(&input[last_text..]);
  }

  out
}

fn strip_trailing_semicolon(s: &str) -> &str {
  let s = s.trim_end();
  s.strip_suffix(';').unwrap_or(s).trim_end()
}

fn strip_leading_ws_and_comments(mut s: &str) -> &str {
  loop {
    let trimmed = s.trim_start();
    if let Some(rest) = trimmed.strip_prefix("//") {
      if let Some(nl) = rest.find('\n') {
        s = &rest[nl + 1..];
        continue;
      }
      return "";
    }
    if let Some(rest) = trimmed.strip_prefix("/*") {
      if let Some(end) = rest.find("*/") {
        s = &rest[end + 2..];
        continue;
      }
      return "";
    }
    return trimmed;
  }
}

fn consume_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
  let s = strip_leading_ws_and_comments(s);
  if !s.starts_with(kw) {
    return None;
  }
  let after = &s[kw.len()..];
  if after
    .chars()
    .next()
    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
  {
    return None;
  }
  Some(after)
}

fn parse_leading_ext_attrs(mut s: &str) -> (Vec<ExtendedAttribute>, &str) {
  let mut out = Vec::new();
  loop {
    s = strip_leading_ws_and_comments(s);
    if !s.starts_with('[') {
      break;
    }
    if let Some((inside, rest)) = consume_bracket_block(s) {
      out.extend(parse_ext_attr_list(inside));
      s = rest;
      continue;
    }
    break;
  }
  (out, s)
}

fn consume_bracket_block(s: &str) -> Option<(&str, &str)> {
  let bytes = s.as_bytes();
  if bytes.first().copied()? != b'[' {
    return None;
  }
  let mut idx = 1usize;
  let mut depth = 1u32;
  let mut in_string: Option<u8> = None;
  let mut in_line_comment = false;
  let mut in_block_comment = false;
  let mut escape = false;

  while idx < bytes.len() {
    let b = bytes[idx];
    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      idx += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
        in_block_comment = false;
        idx += 2;
        continue;
      }
      idx += 1;
      continue;
    }
    if let Some(q) = in_string {
      if escape {
        escape = false;
        idx += 1;
        continue;
      }
      if b == b'\\' {
        escape = true;
        idx += 1;
        continue;
      }
      if b == q {
        in_string = None;
      }
      idx += 1;
      continue;
    }

    if b == b'/' && idx + 1 < bytes.len() {
      if bytes[idx + 1] == b'/' {
        in_line_comment = true;
        idx += 2;
        continue;
      }
      if bytes[idx + 1] == b'*' {
        in_block_comment = true;
        idx += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        idx += 1;
      }
      b'[' => {
        depth += 1;
        idx += 1;
      }
      b']' => {
        depth -= 1;
        if depth == 0 {
          let inside = &s[1..idx];
          let rest = &s[idx + 1..];
          return Some((inside, rest));
        }
        idx += 1;
      }
      _ => idx += 1,
    }
  }

  None
}

fn parse_ext_attr_list(content: &str) -> Vec<ExtendedAttribute> {
  let mut out = Vec::new();
  let mut start = 0usize;
  let mut depth = 0u32; // paren depth
  let mut in_string: Option<char> = None;
  let mut escape = false;
  let chars: Vec<(usize, char)> = content.char_indices().collect();

  for (idx, ch) in chars.iter().copied() {
    if let Some(q) = in_string {
      if escape {
        escape = false;
        continue;
      }
      if ch == '\\' {
        escape = true;
        continue;
      }
      if ch == q {
        in_string = None;
      }
      continue;
    }

    match ch {
      '"' | '\'' => in_string = Some(ch),
      '(' => depth += 1,
      ')' => depth = depth.saturating_sub(1),
      ',' if depth == 0 => {
        let seg = content[start..idx].trim();
        if !seg.is_empty() {
          out.push(parse_ext_attr(seg));
        }
        start = idx + 1;
      }
      _ => {}
    }
  }

  let tail = content[start..].trim();
  if !tail.is_empty() {
    out.push(parse_ext_attr(tail));
  }

  out
}

fn parse_ext_attr(item: &str) -> ExtendedAttribute {
  let item = item.trim();
  if let Some((name, rhs)) = item.split_once('=') {
    return ExtendedAttribute {
      name: name.trim().to_string(),
      value: Some(rhs.trim().to_string()),
    };
  }
  ExtendedAttribute {
    name: item.to_string(),
    value: None,
  }
}

fn extract_curly_body(s: &str) -> Option<(&str, &str)> {
  let bytes = s.as_bytes();

  let mut i = 0usize;
  let mut in_string: Option<u8> = None;
  let mut in_line_comment = false;
  let mut in_block_comment = false;
  let mut escape = false;

  // Find the first `{`.
  while i < bytes.len() {
    let b = bytes[i];
    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      i += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_block_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
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

    if b == b'/' && i + 1 < bytes.len() {
      if bytes[i + 1] == b'/' {
        in_line_comment = true;
        i += 2;
        continue;
      }
      if bytes[i + 1] == b'*' {
        in_block_comment = true;
        i += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'{' => break,
      _ => i += 1,
    }
  }

  if i >= bytes.len() || bytes[i] != b'{' {
    return None;
  }

  let header = &s[..i];
  let body_start = i + 1;

  // Find matching `}`.
  i = body_start;
  let mut depth = 1u32;
  in_string = None;
  in_line_comment = false;
  in_block_comment = false;
  escape = false;

  while i < bytes.len() {
    let b = bytes[i];
    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      i += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_block_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
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

    if b == b'/' && i + 1 < bytes.len() {
      if bytes[i + 1] == b'/' {
        in_line_comment = true;
        i += 2;
        continue;
      }
      if bytes[i + 1] == b'*' {
        in_block_comment = true;
        i += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'{' => {
        depth += 1;
        i += 1;
      }
      b'}' => {
        depth -= 1;
        if depth == 0 {
          let body = &s[body_start..i];
          return Some((header, body));
        }
        i += 1;
      }
      _ => i += 1,
    }
  }

  None
}

fn split_top_level_statements(idl: &str) -> Vec<String> {
  split_semicolon_terminated(idl)
}

fn split_inner_statements(body: &str) -> Vec<String> {
  split_semicolon_terminated(body)
}

fn split_semicolon_terminated(input: &str) -> Vec<String> {
  let bytes = input.as_bytes();
  let mut out = Vec::new();
  let mut start = 0usize;
  let mut i = 0usize;

  let mut curly = 0u32;
  let mut bracket = 0u32;
  let mut paren = 0u32;

  let mut in_string: Option<u8> = None;
  let mut in_line_comment = false;
  let mut in_block_comment = false;
  let mut escape = false;

  while i < bytes.len() {
    let b = bytes[i];

    if in_line_comment {
      if b == b'\n' {
        in_line_comment = false;
      }
      i += 1;
      continue;
    }
    if in_block_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_block_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
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

    if b == b'/' && i + 1 < bytes.len() {
      if bytes[i + 1] == b'/' {
        in_line_comment = true;
        i += 2;
        continue;
      }
      if bytes[i + 1] == b'*' {
        in_block_comment = true;
        i += 2;
        continue;
      }
    }

    match b {
      b'"' | b'\'' => {
        in_string = Some(b);
        i += 1;
      }
      b'{' => {
        curly += 1;
        i += 1;
      }
      b'}' => {
        curly = curly.saturating_sub(1);
        i += 1;
      }
      b'[' => {
        bracket += 1;
        i += 1;
      }
      b']' => {
        bracket = bracket.saturating_sub(1);
        i += 1;
      }
      b'(' => {
        paren += 1;
        i += 1;
      }
      b')' => {
        paren = paren.saturating_sub(1);
        i += 1;
      }
      b';' => {
        i += 1;
        // Be forgiving: in spec sources we occasionally see malformed fragments (especially when
        // scraping HTML) that leave `[`/`(`/`)` counters unbalanced. Those should not prevent us
        // from splitting the overall IDL stream at top-level statement boundaries.
        //
        // Curly braces still gate splitting so interface/dictionary bodies (and `{}` default
        // values) don't get broken up.
        if curly == 0 {
          let seg = input[start..i].trim();
          if !seg.is_empty() {
            out.push(seg.to_string());
          }
          start = i;
          bracket = 0;
          paren = 0;
        }
      }
      _ => i += 1,
    }
  }

  let tail = input[start..].trim();
  if !tail.is_empty() {
    out.push(tail.to_string());
  }

  out
}

fn parse_identifier_prefix(s: &str) -> Option<(&str, &str)> {
  let s = strip_leading_ws_and_comments(s);
  let bytes = s.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];
    let ok = if i == 0 {
      b.is_ascii_alphabetic() || b == b'_'
    } else {
      b.is_ascii_alphanumeric() || b == b'_'
    };
    if !ok {
      break;
    }
    i += 1;
  }
  if i == 0 {
    return None;
  }
  Some((&s[..i], &s[i..]))
}

fn split_trailing_identifier(s: &str) -> Option<(&str, &str)> {
  // Splits `... <name>` into (`name`, `...`).
  let s = s.trim();
  let mut end = s.len();
  while end > 0 && s.as_bytes()[end - 1].is_ascii_whitespace() {
    end -= 1;
  }
  let mut start = end;
  while start > 0 {
    let b = s.as_bytes()[start - 1];
    if !(b.is_ascii_alphanumeric() || b == b'_') {
      break;
    }
    start -= 1;
  }
  if start == end {
    return None;
  }
  let name = &s[start..end];
  let type_ = s[..start].trim_end();
  Some((name, type_))
}

fn extract_member_name(raw: &str) -> Option<String> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }

  // Fast paths.
  if raw.starts_with("constructor") {
    return Some("constructor".to_string());
  }

  // Attributes: take the last identifier (e.g. `readonly attribute DOMString type` → `type`).
  if raw.contains(" attribute ")
    || raw.starts_with("attribute ")
    || raw.starts_with("readonly attribute ")
  {
    return split_trailing_identifier(raw).map(|(name, _)| name.to_string());
  }

  // Operations: pick the last identifier before the first `(`.
  if let Some((before, _after)) = raw.split_once('(') {
    return split_trailing_identifier(before).map(|(name, _)| name.to_string());
  }

  // Dictionary members / constants / misc statements: take the last identifier, ignoring defaults.
  if let Some((before, _after)) = raw.split_once('=') {
    return split_trailing_identifier(before).map(|(name, _)| name.to_string());
  }

  split_trailing_identifier(raw).map(|(name, _)| name.to_string())
}
