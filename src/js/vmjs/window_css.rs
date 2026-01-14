//! Minimal `CSS` namespace bindings for `vm-js` Window realms.
//!
//! This currently implements:
//! - `CSS.supports(property, value)`
//! - `CSS.supports(conditionText)`
//!
//! Real-world scripts frequently probe for selector / custom-property support via this API.
//! See: CSS Conditional Rules Module Level 3, § "The CSS namespace, and the supports() function".
//! (specs/csswg-drafts/css-conditional-3/Overview.bs)
 
use crate::css::parser::parse_supports_prelude;
use crate::css::properties::{
  is_global_keyword_str, is_known_style_property, parse_property_value, supports_parsed_declaration_is_valid,
  vendor_prefixed_property_alias,
};
use crate::css::selectors::{PseudoClassParser, PseudoElement};
use crate::css::types::SupportsCondition;
use crate::style::var_resolution::{contains_arbitrary_substitution_function, is_valid_custom_property_name};
use cssparser::{Parser, ParserInput, Token};
use selectors::parser::{ParseRelative, SelectorList};
use std::borrow::Cow;
use std::char::decode_utf16;
use vm_js::{
  GcObject, GcString, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};
 
/// Upper bound on the number of UTF-8 bytes accepted for `CSS.supports(..)` string arguments.
///
/// This is a DoS resistance measure: we should not allocate arbitrarily large host-side `String`s
/// just to answer a feature query.
const MAX_CSS_SUPPORTS_STRING_BYTES: usize = 64 * 1024;
 
const CSS_SUPPORTS_ARG_TOO_LONG_ERROR: &str = "CSS.supports argument exceeds size limit";
 
fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}
 
fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}
 
fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}
 
fn set_own_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str, value: Value) -> Result<(), VmError> {
  // Root `obj` + `value` while allocating the property key: string allocation can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value))
}
 
fn js_string_to_rust_string_limited(
  scope: &mut Scope<'_>,
  handle: GcString,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let js = scope.heap().get_string(handle)?;
 
  // UTF-8 output bytes are always >= UTF-16 code unit length (and can grow by up to 3 bytes per
  // code unit when decoding lone surrogates as U+FFFD). Reject overly large strings up-front to
  // prevent unbounded host allocations.
  let code_units_len = js.len_code_units();
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(err));
  }
 
  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;
 
  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(VmError::TypeError(err));
    }
    out.push(ch);
    out_len = next_len;
  }
 
  Ok(out)
}
 
fn value_to_limited_string(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
  max_bytes: usize,
  err: &'static str,
) -> Result<String, VmError> {
  let s: GcString = scope.to_string(vm, host, hooks, value)?;
  js_string_to_rust_string_limited(scope, s, max_bytes, err)
}
 
// CSS uses the CSS Syntax "whitespace" production (TAB/LF/FF/CR/SPACE). Avoid Rust's Unicode
// whitespace helpers (`str::trim`, `char::is_whitespace`, etc.) so non-ASCII whitespace like NBSP
// (U+00A0) is preserved.
#[inline]
fn is_ascii_whitespace_css(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}
 
fn trim_ascii_whitespace_css(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_css)
}
 
fn value_has_trailing_important(value: &str) -> bool {
  let trimmed = trim_ascii_whitespace_css(value);
  if !trimmed.as_bytes().contains(&b'!') {
    return false;
  }
 
  #[derive(Copy, Clone, Debug, PartialEq, Eq)]
  enum SignificantToken {
    Bang,
    ImportantIdent,
    Other,
  }
 
  fn skip_nested_block<'i, 't>(parser: &mut Parser<'i, 't>) {
    let _ = parser.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'i, ()>>(()));
  }
 
  let mut input = ParserInput::new(trimmed);
  let mut parser = Parser::new(&mut input);
  let mut second_last: Option<SignificantToken> = None;
  let mut last: Option<SignificantToken> = None;
 
  loop {
    let token = match parser.next_including_whitespace_and_comments() {
      Ok(token) => token,
      Err(_) => break,
    };
 
    let (kind, needs_skip) = match token {
      Token::WhiteSpace(_) | Token::Comment(_) => (None, false),
      Token::Delim('!') => (Some(SignificantToken::Bang), false),
      Token::Ident(ref ident) if ident.eq_ignore_ascii_case("important") => {
        (Some(SignificantToken::ImportantIdent), false)
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => (Some(SignificantToken::Other), true),
      _ => (Some(SignificantToken::Other), false),
    };
 
    if needs_skip {
      skip_nested_block(&mut parser);
    }
 
    if let Some(kind) = kind {
      second_last = last;
      last = Some(kind);
    }
  }
 
  matches!(last, Some(SignificantToken::ImportantIdent)) && matches!(second_last, Some(SignificantToken::Bang))
}
 
fn supports_property_value(property: &str, value: &str) -> bool {
  // Spec: no trimming / whitespace processing on the property name.
  if property.is_empty() {
    return false;
  }
 
  // `!important` flags are not part of property grammars and make parsing invalid.
  if value_has_trailing_important(value) {
    return false;
  }
 
  // Custom properties are case-sensitive and accept (almost) any value.
  if property.starts_with("--") {
    return is_valid_custom_property_name(property);
  }
 
  // CSS property names are ASCII case-insensitive (CSS Syntax).
  let normalized_property: Cow<'_, str> = if property
    .as_bytes()
    .iter()
    .any(|b| b.is_ascii_uppercase())
  {
    Cow::Owned(property.to_ascii_lowercase())
  } else {
    Cow::Borrowed(property)
  };
 
  let normalized_property = normalized_property.as_ref();
  let canonical_property = if is_known_style_property(normalized_property) {
    normalized_property
  } else if normalized_property.starts_with("-webkit-")
    || normalized_property.starts_with("-moz-")
    || normalized_property.starts_with("-ms-")
    || normalized_property.starts_with("-o-")
  {
    match vendor_prefixed_property_alias(normalized_property) {
      Some(alias) => alias,
      None => return false,
    }
  } else {
    return false;
  };
 
  let raw_value = trim_ascii_whitespace_css(value);
  if raw_value.is_empty() {
    return false;
  }
 
  // CSS-wide keywords are valid for all properties.
  if is_global_keyword_str(raw_value) {
    return true;
  }
 
  // Modern browsers do not support the legacy IE `-ms-grid` display keyword. FastRender parses it
  // as an alias for `grid` for compatibility in normal declarations, but support queries should
  // match modern Chromium baselines.
  if canonical_property == "display"
    && (raw_value.eq_ignore_ascii_case("-ms-grid") || raw_value.eq_ignore_ascii_case("-ms-inline-grid"))
  {
    return false;
  }
 
  // `var()`/`if()`/`attr()` are allowed in any value grammar.
  if contains_arbitrary_substitution_function(raw_value) {
    return true;
  }
 
  let parsed = match parse_property_value(canonical_property, raw_value) {
    Some(parsed) => parsed,
    None => return false,
  };
 
  supports_parsed_declaration_is_valid(canonical_property, raw_value, &parsed)
}
 
fn supports_condition_text(condition_text: &str) -> bool {
  let cond = parse_supports_prelude(condition_text);
  if supports_condition_matches(cond) {
    return true;
  }
  let wrapped = format!("({condition_text})");
  supports_condition_matches(parse_supports_prelude(&wrapped))
}
 
fn supports_condition_matches(cond: SupportsCondition) -> bool {
  // CSS.supports(conditionText) disallows namespaces in selector() arguments. Our core
  // `SupportsCondition::matches` uses the selector parser which supports namespaces when the CSS
  // parser namespace context is configured, so normalize selectors here.
  normalize_supports_condition_for_js(cond).matches()
}
 
fn normalize_supports_condition_for_js(cond: SupportsCondition) -> SupportsCondition {
  match cond {
    SupportsCondition::Selector { raw, .. } => SupportsCondition::Selector {
      supported: supports_selector_is_valid_no_namespaces(&raw),
      raw,
    },
    SupportsCondition::Not(mut inner) => {
      *inner = normalize_supports_condition_for_js(*inner);
      SupportsCondition::Not(inner)
    }
    SupportsCondition::And(conds) => SupportsCondition::And(
      conds
        .into_iter()
        .map(normalize_supports_condition_for_js)
        .collect(),
    ),
    SupportsCondition::Or(conds) => SupportsCondition::Or(
      conds
        .into_iter()
        .map(normalize_supports_condition_for_js)
        .collect(),
    ),
    other => other,
  }
}
 
fn supports_selector_is_valid_no_namespaces(selector_list: &str) -> bool {
  if selector_list.is_empty() {
    return false;
  }
 
  if selector_contains_namespace_syntax(selector_list) {
    return false;
  }
 
  for selector in split_selector_list(selector_list) {
    let mut input = ParserInput::new(&selector);
    let mut parser = Parser::new(&mut input);
    let Ok(list) = SelectorList::parse(&PseudoClassParser, &mut parser, ParseRelative::No) else {
      continue;
    };
 
    // See `SupportsCondition::matches`: vendor-prefixed pseudo-elements should not flip selector()
    // support queries to true because they are frequently used inside `not(...)` gates.
    if list
      .slice()
      .iter()
      .any(|selector| !matches!(selector.pseudo_element(), Some(PseudoElement::Vendor(_))))
    {
      return true;
    }
  }
 
  false
}
 
fn selector_contains_namespace_syntax(selector_list: &str) -> bool {
  fn inner<'i, 't>(parser: &mut Parser<'i, 't>) -> bool {
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
      match token {
        Token::Delim('|') => {
          // Allow the Selectors 4 column combinator `||`; reject all other uses (namespace syntax).
          if parser
            .try_parse(|p| {
              match p.next_including_whitespace_and_comments()? {
                Token::Delim('|') => Ok(()),
                _ => Err(p.new_custom_error::<(), ()>(())),
              }
            })
            .is_ok()
          {
            continue;
          }
          return true;
        }
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          if let Ok(found) = parser.parse_nested_block(|nested| Ok::<_, cssparser::ParseError<'i, ()>>(inner(nested)))
          {
            if found {
              return true;
            }
          }
        }
        _ => {}
      }
    }
    false
  }
 
  let mut input = ParserInput::new(selector_list);
  let mut parser = Parser::new(&mut input);
  inner(&mut parser)
}
 
fn split_selector_list(selector_list: &str) -> Vec<String> {
  fn consume_nested_tokens<'i, 't>(
    parser: &mut Parser<'i, 't>,
  ) -> Result<(), cssparser::ParseError<'i, ()>> {
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace() {
        Ok(token) => token,
        Err(_) => break,
      };
 
      match token {
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          if parser.parse_nested_block(consume_nested_tokens).is_err() {
            break;
          }
        }
        _ => {}
      }
    }
 
    Ok(())
  }
 
  let mut input = ParserInput::new(selector_list);
  let mut parser = Parser::new(&mut input);
  let mut parts = Vec::new();
  let mut segment_start = parser.position();
 
  while !parser.is_exhausted() {
    let token = match parser.next_including_whitespace() {
      Ok(token) => token,
      Err(_) => break,
    };
 
    match token {
      Token::Comma => {
        let raw = parser.slice_from(segment_start);
        if let Some(stripped) = raw.strip_suffix(',') {
          let trimmed = trim_ascii_whitespace_css(stripped);
          if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
          }
        }
        segment_start = parser.position();
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        if parser.parse_nested_block(consume_nested_tokens).is_err() {
          break;
        }
      }
      _ => {}
    }
  }
 
  let tail = trim_ascii_whitespace_css(parser.slice_from(segment_start));
  if !tail.is_empty() {
    parts.push(tail.to_string());
  }
 
  parts
}
 
fn css_supports_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  match args.len() {
    0 => Err(VmError::TypeError("CSS.supports requires at least one argument")),
    1 => {
      let condition_text = value_to_limited_string(
        vm,
        scope,
        host,
        hooks,
        args[0],
        MAX_CSS_SUPPORTS_STRING_BYTES,
        CSS_SUPPORTS_ARG_TOO_LONG_ERROR,
      )?;
      Ok(Value::Bool(supports_condition_text(&condition_text)))
    }
    _ => {
      let property = value_to_limited_string(
        vm,
        scope,
        host,
        hooks,
        args[0],
        MAX_CSS_SUPPORTS_STRING_BYTES,
        CSS_SUPPORTS_ARG_TOO_LONG_ERROR,
      )?;
      let value = value_to_limited_string(
        vm,
        scope,
        host,
        hooks,
        args[1],
        MAX_CSS_SUPPORTS_STRING_BYTES,
        CSS_SUPPORTS_ARG_TOO_LONG_ERROR,
      )?;
 
      Ok(Value::Bool(supports_property_value(&property, &value)))
    }
  }
}
 
pub(crate) fn install_window_css_bindings(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;
 
  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();
 
  let css_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(css_obj))?;
 
  // Object.prototype.toString branding ([object CSS]) via Symbol.toStringTag.
  let to_string_tag_key = PropertyKey::from_symbol(realm.well_known_symbols().to_string_tag);
  let css_tag = scope.alloc_string("CSS")?;
  scope.push_root(Value::String(css_tag))?;
  scope.define_property(
    css_obj,
    to_string_tag_key,
    read_only_data_desc(Value::String(css_tag)),
  )?;
 
  // CSS.supports
  let supports_call_id = vm.register_native_call(css_supports_native)?;
  let supports_name = scope.alloc_string("supports")?;
  scope.push_root(Value::String(supports_name))?;
  let supports_fn = scope.alloc_native_function(supports_call_id, None, supports_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(supports_fn, Some(func_proto))?;
  set_own_data_prop(&mut scope, css_obj, "supports", Value::Object(supports_fn))?;
 
  // Expose on global.
  let css_key = alloc_key(&mut scope, "CSS")?;
  scope.define_property(global, css_key, data_desc(Value::Object(css_obj)))?;
 
  Ok(())
}
