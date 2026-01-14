//! CSS Custom Property (var()) Resolution
//!
//! Implements token-based resolution of `var()` references using `cssparser`
//! so that nested functions, fallbacks with commas, and repeated substitutions
//! are handled correctly.

use crate::css::properties::{parse_length, parse_property_value_after_var_resolution};
use crate::css::types::PropertyValue;
use crate::dom::DomNode;
use crate::geometry::Size;
use crate::style::color::{Color, Rgba};
use crate::style::custom_property_store::CustomPropertyStore;
use crate::style::media::{ColorScheme, MediaContext, MediaQuery};
use crate::style::ComputedStyle;
use cssparser::ParseError;
use cssparser::ParseErrorKind;
use cssparser::Parser;
use cssparser::ParserInput;
use cssparser::ToCss;
use cssparser::Token;
use rustc_hash::FxHashSet;
use std::borrow::Cow;
use std::cell::Cell;
#[cfg(test)]
use std::cell::Cell as TestCell;
use std::sync::Arc;

/// Maximum depth for recursive var()/if()/attr()/first-valid()/toggle() resolution.
///
/// This serves as a hard safety cap against hostile inputs (extremely deep nesting or chains)
/// while still supporting real-world CSS frameworks that routinely generate long *acyclic*
/// `--a: var(--b)` custom-property chains.
const MAX_RECURSION_DEPTH: usize = 64;

/// Separator inserted when serializing token-spliced values via string concatenation.
///
/// CSS `var()` substitution operates on token streams; adjacent substitutions can create token
/// sequences that are not representable by naïvely concatenating the source text (e.g.
/// `0` + `calc(...)` would become `0calc(...)` which tokenizes as a dimension).
///
/// We model token splicing by inserting a minimal whitespace separator only when required to avoid
/// token merging during a later re-tokenization pass.
const TOKEN_SPLICE_SEPARATOR: &str = " ";

#[cfg(test)]
std::thread_local! {
  static TOKEN_RESOLVER_ENTRY_COUNT: TestCell<usize> = TestCell::new(0);
}

#[derive(Default)]
struct VarResolutionStack {
  stack: Vec<Arc<str>>,
  set: FxHashSet<Arc<str>>,
}

impl VarResolutionStack {
  #[inline]
  fn clear(&mut self) {
    self.stack.clear();
    self.set.clear();
  }

  #[inline]
  fn contains(&self, name: &str) -> bool {
    self.set.contains(name)
  }

  #[inline]
  fn push(&mut self, name: &str) {
    let name: Arc<str> = Arc::from(name);
    self.stack.push(name.clone());
    self.set.insert(name);
  }

  #[inline]
  fn pop(&mut self) {
    if let Some(name) = self.stack.pop() {
      self.set.remove(&name);
    }
  }
}

#[derive(Clone, Copy)]
struct SubstitutionContext {
  element: *const DomNode,
  parent_style: *const ComputedStyle,
  viewport: Size,
  color_scheme_pref: ColorScheme,
}

// During the cascade we resolve `var()`/`if()`/`attr()` in property values. Both `if(media(...))`
// and typed `attr()` need access to per-node/per-render context (the styled element + viewport).
//
// Thread-local storage keeps the var-resolution API stable (call sites already exist across the
// engine) while still allowing these newer substitution functions to consult the current context.
std::thread_local! {
  static SUBSTITUTION_CONTEXT: Cell<Option<SubstitutionContext>> = Cell::new(None);
}

pub(crate) struct SubstitutionContextGuard {
  prev: Option<SubstitutionContext>,
}

impl Drop for SubstitutionContextGuard {
  fn drop(&mut self) {
    SUBSTITUTION_CONTEXT.with(|cell| cell.set(self.prev));
  }
}

pub(crate) fn push_substitution_context(
  element: &DomNode,
  parent_style: &ComputedStyle,
  viewport: Size,
  color_scheme_pref: ColorScheme,
) -> SubstitutionContextGuard {
  SUBSTITUTION_CONTEXT.with(|cell| {
    let prev = cell.get();
    cell.set(Some(SubstitutionContext {
      element: element as *const DomNode,
      parent_style: parent_style as *const ComputedStyle,
      viewport,
      color_scheme_pref,
    }));
    SubstitutionContextGuard { prev }
  })
}

pub(crate) fn with_substitution_context<R>(
  element: &DomNode,
  parent_style: &ComputedStyle,
  viewport: Size,
  color_scheme_pref: ColorScheme,
  f: impl FnOnce() -> R,
) -> R {
  let _guard = push_substitution_context(element, parent_style, viewport, color_scheme_pref);
  f()
}

fn current_substitution_context() -> Option<SubstitutionContext> {
  SUBSTITUTION_CONTEXT.with(|cell| cell.get())
}

fn current_style_element() -> Option<&'static DomNode> {
  let ctx = current_substitution_context()?;
  if ctx.element.is_null() {
    return None;
  }
  // Safety: the pointer was installed by `with_substitution_context` and is only accessed while
  // that guard is live on the same thread.
  Some(unsafe { &*ctx.element })
}

fn current_parent_style() -> Option<&'static ComputedStyle> {
  let ctx = current_substitution_context()?;
  if ctx.parent_style.is_null() {
    return None;
  }
  // Safety: the pointer was installed by `with_substitution_context` and is only accessed while
  // that guard is live on the same thread.
  Some(unsafe { &*ctx.parent_style })
}

fn current_viewport() -> Option<Size> {
  current_substitution_context().map(|ctx| ctx.viewport)
}

fn current_color_scheme_pref() -> Option<ColorScheme> {
  current_substitution_context().map(|ctx| ctx.color_scheme_pref)
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
  b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') || b >= 0x80
}

#[inline]
fn is_css_whitespace_byte(b: u8) -> bool {
  matches!(b, b' ' | b'\n' | b'\t' | b'\r' | b'\x0C')
}

#[inline]
fn is_css_whitespace_char(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_css_whitespace(value: &str) -> &str {
  value.trim_matches(is_css_whitespace_char)
}

pub(crate) fn value_has_unbalanced_delimiters(value: &str) -> bool {
  // `cssparser` will often treat EOF as implicitly closing open blocks/functions. Browsers do not
  // apply var()/if()/attr() substitution when the authored value contains unterminated blocks (e.g.
  // `fill: var(--c;`), so we reject such values up-front to match CSS syntax validity.
  //
  // This scan ignores delimiters inside comments/strings and honors CSS escape sequences so
  // `\\(` does not start a parenthesis block.
  let bytes = value.as_bytes();
  let mut i = 0usize;
  let mut in_comment = false;
  let mut in_string: Option<u8> = None;
  let mut string_escape = false;
  let mut parens = 0usize;
  let mut brackets = 0usize;
  let mut braces = 0usize;

  fn is_css_ascii_whitespace_byte(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
  }

  while i < bytes.len() {
    let b = bytes[i];

    if in_comment {
      if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
        in_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if let Some(quote) = in_string {
      if string_escape {
        string_escape = false;
        i += 1;
        continue;
      }
      if b == b'\\' {
        string_escape = true;
        i += 1;
        continue;
      }
      if b == quote {
        in_string = None;
        i += 1;
        continue;
      }
      i += 1;
      continue;
    }

    // Not inside a string/comment.
    if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
      in_comment = true;
      i += 2;
      continue;
    }

    if b == b'"' || b == b'\'' {
      in_string = Some(b);
      i += 1;
      continue;
    }

    // Consume CSS escapes so escaped delimiters don't affect the balance.
    if b == b'\\' {
      i += 1;
      if i >= bytes.len() {
        // Trailing backslash => invalid escape => invalid syntax.
        return true;
      }

      let next = bytes[i];
      // Escaped newline is removed.
      if next == b'\n' {
        i += 1;
        continue;
      }

      if next.is_ascii_hexdigit() {
        // Consume up to 6 hex digits.
        let mut consumed = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() && consumed < 6 {
          i += 1;
          consumed += 1;
        }
        // Optional whitespace after hex escape.
        if i < bytes.len() && is_css_ascii_whitespace_byte(bytes[i]) {
          i += 1;
        }
        continue;
      }

      // Escape the next codepoint.
      i += 1;
      continue;
    }

    match b {
      b'(' => parens += 1,
      b')' => {
        if parens == 0 {
          return true;
        }
        parens -= 1;
      }
      b'[' => brackets += 1,
      b']' => {
        if brackets == 0 {
          return true;
        }
        brackets -= 1;
      }
      b'{' => braces += 1,
      b'}' => {
        if braces == 0 {
          return true;
        }
        braces -= 1;
      }
      _ => {}
    }

    i += 1;
  }

  in_comment || in_string.is_some() || parens > 0 || brackets > 0 || braces > 0
}

/// Returns true when the token stream starts with a CSS-wide keyword and contains additional
/// non-whitespace/comment tokens.
///
/// CSS-wide keywords (`initial`, `inherit`, `unset`, `revert`, `revert-layer`) are only valid as
/// the *entire* property value. When a custom property resolves to `initial <other tokens>` it is
/// guaranteed to be invalid for any non-custom property, so `var(--x, fallback)` must select the
/// fallback instead.
///
/// This pattern is relied upon by tooling such as the csstools `light-dark()` polyfill.
fn starts_with_css_wide_keyword_with_trailing_tokens(value: &str) -> bool {
  let trimmed = trim_css_whitespace(value);
  if trimmed.is_empty() {
    return false;
  }

  const KEYWORDS: [&str; 5] = ["initial", "inherit", "unset", "revert", "revert-layer"];
  for keyword in KEYWORDS {
    if trimmed.len() < keyword.len() {
      continue;
    }

    let Some(head) = trimmed.get(..keyword.len()) else {
      continue;
    };
    if !head.eq_ignore_ascii_case(keyword) {
      continue;
    }

    // Ensure the keyword matches a full ident token, not a longer identifier like `initial-value`.
    if trimmed
      .as_bytes()
      .get(keyword.len())
      .is_some_and(|&b| is_ident_byte(b))
    {
      continue;
    }

    let bytes = trimmed.as_bytes();
    let mut idx = keyword.len();
    loop {
      while idx < bytes.len() && is_css_whitespace_byte(bytes[idx]) {
        idx += 1;
      }

      if idx + 1 < bytes.len() && bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
        // Skip comment blocks, which act like whitespace.
        idx += 2;
        while idx + 1 < bytes.len() {
          if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
            idx += 2;
            break;
          }
          idx += 1;
        }
        continue;
      }

      break;
    }

    return idx < bytes.len();
  }

  false
}

#[inline]
fn needs_token_splice_separator(prev: u8, next: u8, next_next: Option<u8>) -> bool {
  if is_css_whitespace_byte(prev) || is_css_whitespace_byte(next) {
    return false;
  }

  // Starting a comment across a splice boundary (`/*`) would drastically change the token stream.
  if prev == b'/' && next == b'*' {
    return true;
  }

  // `@foo` and `#foo` are single tokens. If the splice boundary would otherwise create them,
  // separate the tokens.
  if (prev == b'@' || prev == b'#') && is_ident_byte(next) {
    return true;
  }

  // Identifier / number adjacency is where most token-merging bugs happen:
  // - `0` + `calc(...)` => dimension token `0calc`
  // - `0` + `0` => number token `00`
  // - `px` + `calc(...)` => ident `pxcalc` (unit merging)
  if is_ident_byte(prev) && is_ident_byte(next) {
    return true;
  }

  // A bare identifier immediately followed by `(` becomes a function token. Token-stream splicing
  // can create `Ident` + `ParenthesisBlock` sequences that must not be re-tokenized as a function.
  if is_ident_byte(prev) && next == b'(' {
    return true;
  }

  // `+` / `-` / `.` can start a number token. If a splice boundary would cause them to be
  // re-tokenized as part of the following number, insert a separator.
  if (prev == b'+' || prev == b'-') && (next.is_ascii_digit() || next == b'.') {
    // For the `+.` / `-.` cases, only treat it as a number if there is a digit afterwards.
    if next != b'.' || next_next.is_some_and(|b| b.is_ascii_digit()) {
      return true;
    }
  }

  if prev == b'.' && next.is_ascii_digit() {
    return true;
  }

  false
}

#[inline]
fn push_css_with_token_splice_boundary(out: &mut String, chunk: &str) {
  if chunk.is_empty() {
    return;
  }

  if out.is_empty() {
    out.push_str(chunk);
    return;
  }

  let Some(prev) = out.as_bytes().last().copied() else {
    out.push_str(chunk);
    return;
  };
  let next_bytes = chunk.as_bytes();
  let next = next_bytes[0];
  let next_next = next_bytes.get(1).copied();

  if needs_token_splice_separator(prev, next, next_next) {
    out.push_str(TOKEN_SPLICE_SEPARATOR);
  }
  out.push_str(chunk);
}

#[inline]
fn contains_ascii_case_insensitive_var_call(raw: &str) -> bool {
  let bytes = raw.as_bytes();
  if bytes.len() < 4 {
    return false;
  }

  let mut idx = 0usize;
  while idx + 3 < bytes.len() {
    let b0 = bytes[idx];
    if b0 == b'v' || b0 == b'V' {
      let b1 = bytes[idx + 1];
      let b2 = bytes[idx + 2];
      if (b1 == b'a' || b1 == b'A') && (b2 == b'r' || b2 == b'R') && bytes[idx + 3] == b'(' {
        return true;
      }
    }
    idx += 1;
  }
  false
}

#[inline]
fn contains_ascii_case_insensitive_substitution_call(raw: &str) -> bool {
  // Cheap check used on the cascade hot path. This intentionally does *not* understand CSS strings
  // or comments; false positives are acceptable (they just take the slow-path), but false negatives
  // are not (except for escaped function names, which are handled by the backslash+paren guard).
  let bytes = raw.as_bytes();
  if bytes.len() < 3 {
    return false;
  }

  let mut idx = 0usize;
  while idx < bytes.len() {
    match bytes[idx].to_ascii_lowercase() {
      b'v' => {
        if idx + 3 < bytes.len()
          && bytes[idx + 1].to_ascii_lowercase() == b'a'
          && bytes[idx + 2].to_ascii_lowercase() == b'r'
          && bytes[idx + 3] == b'('
        {
          return true;
        }
      }
      b'i' => {
        if idx + 2 < bytes.len()
          && bytes[idx + 1].to_ascii_lowercase() == b'f'
          && bytes[idx + 2] == b'('
        {
          return true;
        }
      }
      b'f' => {
        if idx + 11 < bytes.len()
          && bytes[idx + 1].to_ascii_lowercase() == b'i'
          && bytes[idx + 2].to_ascii_lowercase() == b'r'
          && bytes[idx + 3].to_ascii_lowercase() == b's'
          && bytes[idx + 4].to_ascii_lowercase() == b't'
          && bytes[idx + 5] == b'-'
          && bytes[idx + 6].to_ascii_lowercase() == b'v'
          && bytes[idx + 7].to_ascii_lowercase() == b'a'
          && bytes[idx + 8].to_ascii_lowercase() == b'l'
          && bytes[idx + 9].to_ascii_lowercase() == b'i'
          && bytes[idx + 10].to_ascii_lowercase() == b'd'
          && bytes[idx + 11] == b'('
        {
          return true;
        }
      }
      b'a' => {
        if idx + 4 < bytes.len()
          && bytes[idx + 1].to_ascii_lowercase() == b't'
          && bytes[idx + 2].to_ascii_lowercase() == b't'
          && bytes[idx + 3].to_ascii_lowercase() == b'r'
          && bytes[idx + 4] == b'('
        {
          return true;
        }
      }
      b't' => {
        if idx + 6 < bytes.len()
          && bytes[idx + 1].to_ascii_lowercase() == b'o'
          && bytes[idx + 2].to_ascii_lowercase() == b'g'
          && bytes[idx + 3].to_ascii_lowercase() == b'g'
          && bytes[idx + 4].to_ascii_lowercase() == b'l'
          && bytes[idx + 5].to_ascii_lowercase() == b'e'
          && bytes[idx + 6] == b'('
        {
          return true;
        }
      }
      _ => {}
    }
    idx += 1;
  }

  false
}

#[inline]
fn parse_simple_var_call<'a>(raw: &'a str) -> Option<(&'a str, Option<&'a str>)> {
  let trimmed = trim_css_whitespace(raw);
  if trimmed.len() < 6
    || !trimmed
      .get(..4)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("var("))
    || !trimmed.ends_with(')')
  {
    return None;
  }

  // Reject anything with nested parentheses; those require a full tokenizer to interpret.
  let inner = trimmed.get(4..trimmed.len().saturating_sub(1))?;
  // Comments can appear inside var() arguments (`var(--x/*comment*/)`); treat those as "complex"
  // so we fall back to cssparser tokenization where comments are correctly ignored.
  if inner.as_bytes().contains(&b'/') && inner.contains("/*") {
    return None;
  }
  if inner.contains('(') || inner.contains(')') {
    return None;
  }

  let inner = trim_css_whitespace(inner);
  let (name_chunk, fallback_chunk) = inner
    .split_once(',')
    .map(|(name, fallback)| (name, Some(fallback)))
    .unwrap_or((inner, None));

  let name = trim_css_whitespace(name_chunk);
  // The fast path only supports unescaped "simple" custom property names. Enforce the CSS
  // identifier byte rules so `var(--bad!, 1px)` (invalid syntax) doesn't get treated as a missing
  // variable with a valid fallback.
  if !name.starts_with("--")
    || name.len() <= 2
    || name.as_bytes()[2..].iter().any(|&b| !is_ident_byte(b))
    || name.contains(is_css_whitespace_char)
  {
    return None;
  }

  let fallback = fallback_chunk.map(trim_css_whitespace);
  if let Some(fallback) = fallback {
    // `var(--x,)` uses an *empty* fallback. This is distinct from omitting the fallback entirely
    // (`var(--x)`) because empty fallbacks are valid in contexts where the substituted token stream
    // can disappear (e.g. Tailwind-style `transform: var(--tw-rotate-x,) ...`).
    if fallback.is_empty() {
      return Some((name, Some("")));
    }
    // Only support a single comma here; multiple commas require tokenization to disambiguate.
    if fallback_chunk.is_some_and(|rest| rest.contains(',')) {
      return None;
    }
    return Some((name, Some(fallback)));
  }

  Some((name, None))
}

#[inline]
fn try_resolve_var_calls_without_tokenizer<'a>(
  raw: &'a str,
  custom_properties: &'a CustomPropertyStore,
  depth: usize,
  property_name: &str,
) -> Option<Result<String, VarResolutionResult<'a>>> {
  if depth >= MAX_RECURSION_DEPTH {
    return Some(Err(VarResolutionResult::RecursionLimitExceeded));
  }

  // The fast path only handles unescaped values. If the raw string contains backslashes it may
  // hide `var(` via escapes; fall back to cssparser in that case for correctness.
  if raw.as_bytes().contains(&b'\\') {
    return None;
  }

  // This fast path only understands `var()` token splicing.
  //
  // CSS Values 5 `if()`, `first-valid()`, and typed `attr()` have *lazy* semantics: the branch /
  // candidate / fallback that is not chosen must not be evaluated (e.g.
  // `if(...: var(--missing); <else>)` must not fail just because the unselected branch contains an
  // unresolved var()).
  //
  // The full tokenizer-based resolver implements that laziness; this substring-based var splicer
  // does not. Conservatively disable the fast path when `if()`, `first-valid()`, or `attr()`
  // appears in the value.
  {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut in_comment = false;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
      let b = bytes[i];
      if in_comment {
        if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
          in_comment = false;
          i += 2;
          continue;
        }
        i += 1;
        continue;
      }

      if let Some(quote) = in_string {
        if b == quote {
          in_string = None;
        }
        i += 1;
        continue;
      }

      if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
        in_comment = true;
        i += 2;
        continue;
      }

      if b == b'"' || b == b'\'' {
        in_string = Some(b);
        i += 1;
        continue;
      }

      // `if(`
      if i + 2 < bytes.len()
        && b.to_ascii_lowercase() == b'i'
        && bytes[i + 1].to_ascii_lowercase() == b'f'
        && bytes[i + 2] == b'('
      {
        let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
        if !prev.is_some_and(is_ident_byte) {
          return None;
        }
      }

      // `attr(`
      if i + 4 < bytes.len()
        && b.to_ascii_lowercase() == b'a'
        && bytes[i + 1].to_ascii_lowercase() == b't'
        && bytes[i + 2].to_ascii_lowercase() == b't'
        && bytes[i + 3].to_ascii_lowercase() == b'r'
        && bytes[i + 4] == b'('
      {
        let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
        if !prev.is_some_and(is_ident_byte) {
          return None;
        }
      }

      // `first-valid(`
      if i + 11 < bytes.len()
        && b.to_ascii_lowercase() == b'f'
        && bytes[i + 1].to_ascii_lowercase() == b'i'
        && bytes[i + 2].to_ascii_lowercase() == b'r'
        && bytes[i + 3].to_ascii_lowercase() == b's'
        && bytes[i + 4].to_ascii_lowercase() == b't'
        && bytes[i + 5] == b'-'
        && bytes[i + 6].to_ascii_lowercase() == b'v'
        && bytes[i + 7].to_ascii_lowercase() == b'a'
        && bytes[i + 8].to_ascii_lowercase() == b'l'
        && bytes[i + 9].to_ascii_lowercase() == b'i'
        && bytes[i + 10].to_ascii_lowercase() == b'd'
        && bytes[i + 11] == b'('
      {
        let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
        if !prev.is_some_and(is_ident_byte) {
          return None;
        }
      }

      i += 1;
    }
  }

  // Cheap check: most values don't contain var() at all.
  if !contains_ascii_case_insensitive_var_call(raw) {
    return None;
  }

  let bytes = raw.as_bytes();
  let mut i = 0usize;
  let mut last = 0usize;
  let mut output = String::new();
  let mut in_comment = false;
  let mut in_string: Option<u8> = None;
  let mut any = false;
  // Reuse the same recursion stack for each top-level var() call to avoid repeated allocations.
  let mut stack = VarResolutionStack::default();

  while i < bytes.len() {
    let b = bytes[i];

    if in_comment {
      if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if let Some(quote) = in_string {
      if b == quote {
        in_string = None;
      }
      i += 1;
      continue;
    }

    if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
      in_comment = true;
      i += 2;
      continue;
    }

    if b == b'"' || b == b'\'' {
      in_string = Some(b);
      i += 1;
      continue;
    }

    if i + 3 < bytes.len()
      && b.to_ascii_lowercase() == b'v'
      && bytes[i + 1].to_ascii_lowercase() == b'a'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3] == b'('
    {
      // Ensure the match isn't part of a longer identifier (e.g. `somevar(`).
      let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
      if prev.map_or(false, is_ident_byte) {
        i += 1;
        continue;
      }

      // Find the end of the `var(...)` call. We only accept a "simple" var() call here with no
      // nested parentheses inside the argument list (nested blocks/fallback functions require a
      // full tokenizer).
      let mut end = i + 4;
      let mut saw_nested_paren = false;
      while end < bytes.len() {
        match bytes[end] {
          b')' => break,
          b'(' => {
            saw_nested_paren = true;
            break;
          }
          b'"' | b'\'' => return None,
          b'/' if end + 1 < bytes.len() && bytes[end + 1] == b'*' => return None,
          _ => end += 1,
        }
      }

      if saw_nested_paren || end >= bytes.len() {
        return None;
      }

      let var_call = raw.get(i..end + 1)?;
      let Some((name, fallback)) = parse_simple_var_call(var_call) else {
        return None;
      };

      if !any {
        output.reserve(raw.len());
      }
      push_css_with_token_splice_boundary(&mut output, raw.get(last..i)?);

      stack.clear();
      match resolve_variable_reference(
        name,
        fallback.map(Cow::Borrowed),
        custom_properties,
        &mut stack,
        depth,
        property_name,
      ) {
        Ok(resolved) => push_css_with_token_splice_boundary(&mut output, resolved.as_ref()),
        Err(err) => return Some(Err(err)),
      }

      any = true;
      last = end + 1;
      i = end + 1;
      continue;
    }

    i += 1;
  }

  if !any {
    return None;
  }

  push_css_with_token_splice_boundary(&mut output, raw.get(last..)?);
  Some(Ok(output))
}

/// Result of a var() resolution attempt
#[derive(Debug, Clone)]
pub enum VarResolutionResult<'a> {
  /// Successfully resolved to a value
  Resolved {
    value: ResolvedPropertyValue<'a>,
    css_text: Cow<'a, str>,
  },
  /// The variable was not found and no fallback was provided (or the fallback failed to resolve)
  NotFound(String),
  /// Recursion depth exceeded (possible circular reference)
  RecursionLimitExceeded,
  /// Invalid var() syntax
  InvalidSyntax(String),
}

#[derive(Debug, Clone)]
pub enum ResolvedPropertyValue<'a> {
  Borrowed(&'a PropertyValue),
  Owned(PropertyValue),
}

impl<'a> ResolvedPropertyValue<'a> {
  #[inline]
  pub fn as_ref(&self) -> &PropertyValue {
    match self {
      ResolvedPropertyValue::Borrowed(value) => value,
      ResolvedPropertyValue::Owned(value) => value,
    }
  }

  #[inline]
  pub fn into_owned(self) -> PropertyValue {
    match self {
      ResolvedPropertyValue::Borrowed(value) => value.clone(),
      ResolvedPropertyValue::Owned(value) => value,
    }
  }
}

impl<'a> AsRef<PropertyValue> for ResolvedPropertyValue<'a> {
  #[inline]
  fn as_ref(&self) -> &PropertyValue {
    Self::as_ref(self)
  }
}

impl<'a> VarResolutionResult<'a> {
  /// Returns the resolved value if successful, otherwise returns the original value
  pub fn unwrap_or(self, default: PropertyValue) -> PropertyValue {
    match self {
      VarResolutionResult::Resolved { value, .. } => value.into_owned(),
      _ => default,
    }
  }

  /// Returns true if the resolution was successful
  pub fn is_resolved(&self) -> bool {
    matches!(self, VarResolutionResult::Resolved { .. })
  }

  /// Returns the CSS serialization of the resolved value if available.
  ///
  /// For invalid syntax, this returns the resolved string that failed to parse,
  /// which is still useful for consumers that need the token-stream result.
  pub fn css_text(&self) -> Option<&str> {
    match self {
      VarResolutionResult::Resolved { css_text, .. } => Some(css_text.as_ref()),
      VarResolutionResult::InvalidSyntax(text) => Some(text.as_str()),
      _ => None,
    }
  }
}

/// Resolves CSS `var()` references using the provided custom properties.
///
/// This helper performs property-agnostic resolution (parses fallback/results without knowing
/// the destination property). For property-aware parsing, use `resolve_var_for_property`.
pub fn resolve_var(
  value: &PropertyValue,
  custom_properties: &CustomPropertyStore,
) -> PropertyValue {
  match resolve_var_recursive(value, custom_properties, 0, "") {
    VarResolutionResult::Resolved { value, .. } => value.into_owned(),
    other => other.unwrap_or(value.clone()),
  }
}

/// Resolves CSS `var()` references with knowledge of the target property.
///
/// Passing the property name allows the resolver to parse the substituted value using the
/// appropriate grammar (e.g., background layers with commas), rather than the generic parser.
pub fn resolve_var_for_property<'a>(
  value: &'a PropertyValue,
  custom_properties: &'a CustomPropertyStore,
  property_name: &str,
) -> VarResolutionResult<'a> {
  match value {
    PropertyValue::Keyword(raw) | PropertyValue::Custom(raw) => {
      // Most declarations are simple keywords (display, position, etc.) and do not contain any
      // arbitrary substitution functions. Avoid feeding such values through cssparser tokenization
      // by doing a cheap ASCII-case-insensitive substring check for
      // `var(`/`if(`/`first-valid(`/`attr(` first.
      //
      // Note: If the value contains a backslash escape, conservatively fall back to token parsing
      // so we don't miss an escaped `var()`/`if()`/`first-valid()`/`attr()` function name. Function
      // tokens require
      // a literal `(`, so values without any `(` can skip the slow-path even if they contain
      // backslashes.
      if !contains_ascii_case_insensitive_substitution_call(raw)
        && (!raw.as_bytes().contains(&b'\\') || !raw.as_bytes().contains(&b'('))
      {
        return VarResolutionResult::Resolved {
          value: ResolvedPropertyValue::Borrowed(value),
          css_text: Cow::Borrowed(""),
        };
      }

      // Fast path: `var(--x)` is extremely common (especially for color/spacing tokens). Avoid a
      // full `cssparser` token walk when the entire value is a single var() call with no fallback.
      //
      // This also avoids allocating/building an output string for the outer token stream; we only
      // materialize the referenced custom property's value.
      if !raw.as_bytes().contains(&b'\\') {
        if let Some((name, fallback)) = parse_simple_var_call(raw) {
          let mut stack = VarResolutionStack::default();
          match resolve_variable_reference(
            name,
            fallback.map(Cow::Borrowed),
            custom_properties,
            &mut stack,
            0,
            property_name,
          ) {
            Ok(resolved) => match parse_value_after_resolution(resolved.as_ref(), property_name) {
              Some(parsed) => {
                return VarResolutionResult::Resolved {
                  value: ResolvedPropertyValue::Owned(parsed),
                  css_text: resolved,
                };
              }
              None => return VarResolutionResult::InvalidSyntax(resolved.into_owned()),
            },
            Err(err) => return err,
          }
        }

        if let Some(result) =
          try_resolve_var_calls_without_tokenizer(raw, custom_properties, 0, property_name)
        {
          match result {
            Ok(resolved) => match parse_value_after_resolution(&resolved, property_name) {
              Some(parsed) => {
                return VarResolutionResult::Resolved {
                  value: ResolvedPropertyValue::Owned(parsed),
                  css_text: Cow::Owned(resolved),
                };
              }
              None => return VarResolutionResult::InvalidSyntax(resolved),
            },
            Err(err) => return err,
          }
        }
      }
    }
    _ => {}
  }
  resolve_var_recursive(value, custom_properties, 0, property_name)
}

/// Resolves var() references with explicit depth tracking
///
/// This function is useful when you need to track the recursion depth,
/// for example when implementing custom resolution strategies.
pub fn resolve_var_with_depth(
  value: &PropertyValue,
  custom_properties: &CustomPropertyStore,
  depth: usize,
) -> PropertyValue {
  match resolve_var_recursive(value, custom_properties, depth, "") {
    VarResolutionResult::Resolved { value, .. } => value.into_owned(),
    other => other.unwrap_or(value.clone()),
  }
}

/// Internal recursive implementation of var() resolution
fn resolve_var_recursive<'a>(
  value: &'a PropertyValue,
  custom_properties: &'a CustomPropertyStore,
  depth: usize,
  property_name: &str,
) -> VarResolutionResult<'a> {
  if depth >= MAX_RECURSION_DEPTH {
    return VarResolutionResult::RecursionLimitExceeded;
  }

  match value {
    PropertyValue::Keyword(raw) => {
      resolve_from_string(raw, custom_properties, depth, property_name)
    }
    PropertyValue::Custom(raw) => resolve_from_string(raw, custom_properties, depth, property_name),
    _ => VarResolutionResult::Resolved {
      value: ResolvedPropertyValue::Borrowed(value),
      css_text: Cow::Borrowed(""),
    },
  }
}

fn resolve_from_string<'a>(
  raw: &'a str,
  custom_properties: &'a CustomPropertyStore,
  depth: usize,
  property_name: &str,
) -> VarResolutionResult<'a> {
  let mut stack = VarResolutionStack::default();
  match resolve_value_tokens(raw, custom_properties, &mut stack, depth, property_name) {
    Ok(resolved) => match parse_value_after_resolution(&resolved, property_name) {
      Some(value) => VarResolutionResult::Resolved {
        value: ResolvedPropertyValue::Owned(value),
        css_text: Cow::Owned(resolved),
      },
      None => VarResolutionResult::InvalidSyntax(resolved),
    },
    Err(err) => err,
  }
}

fn resolve_value_tokens<'a, 'i>(
  value: &'i str,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  if depth >= MAX_RECURSION_DEPTH {
    return Err(VarResolutionResult::RecursionLimitExceeded);
  }

  if value_has_unbalanced_delimiters(value) {
    return Err(VarResolutionResult::InvalidSyntax(value.to_string()));
  }

  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);
  resolve_tokens_from_parser(&mut parser, custom_properties, stack, depth, property_name)
}

fn resolve_tokens_from_parser<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  #[cfg(test)]
  TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(count.get() + 1));

  let mut output = String::new();

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name) if name.eq_ignore_ascii_case("var") => {
        let nested = parser.parse_nested_block(|nested| {
          parse_var_function(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "var")?;
        push_css_with_token_splice_boundary(&mut output, resolved.as_ref());
      }
      Token::Function(name) if name.eq_ignore_ascii_case("if") => {
        let nested = parser.parse_nested_block(|nested| {
          parse_if_function(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "if")?;
        push_css_with_token_splice_boundary(&mut output, resolved.as_str());
      }
      Token::Function(name) if name.eq_ignore_ascii_case("first-valid") => {
        let nested = parser.parse_nested_block(|nested| {
          parse_first_valid_function(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "first-valid")?;
        push_css_with_token_splice_boundary(&mut output, resolved.as_str());
      }
      Token::Function(name) if name.eq_ignore_ascii_case("attr") => {
        // Typed `attr()` (CSS Values 5) is an "arbitrary substitution function" that resolves at
        // computed-value time, but `content: attr(...)` (CSS Generated Content) is part of the
        // property grammar and is resolved later during pseudo-element generation.
        //
        // Preserve `attr(...)` when parsing the `content` property so downstream `ContentValue`
        // parsing can see it.
        if property_name.eq_ignore_ascii_case("content") {
          push_css_with_token_splice_boundary(&mut output, name.as_ref());
          output.push('(');
          let nested = parser.parse_nested_block(|nested| {
            resolve_tokens_from_parser(nested, custom_properties, stack, depth, property_name)
              .map_err(|err| nested.new_custom_error(err))
          });
          let resolved = map_nested_result(nested, "attr")?;
          output.push_str(&resolved);
          output.push(')');
        } else {
          let nested = parser.parse_nested_block(|nested| {
            parse_attr_function(nested, custom_properties, stack, depth, property_name)
              .map_err(|err| nested.new_custom_error(err))
          });
          let resolved = map_nested_result(nested, "attr")?;
          push_css_with_token_splice_boundary(&mut output, resolved.as_str());
        }
      }
      Token::Function(name) if name.eq_ignore_ascii_case("toggle") => {
        let nested = parser.parse_nested_block(|nested| {
          parse_toggle_function(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "toggle")?;
        push_css_with_token_splice_boundary(&mut output, resolved.as_str());
      }
      Token::Function(name) => {
        push_css_with_token_splice_boundary(&mut output, name.as_ref());
        output.push('(');
        let nested = parser.parse_nested_block(|nested| {
          resolve_tokens_from_parser(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        // Avoid keeping `name` (which borrows from the parser input) live across the nested parse.
        // If the nested block is invalid, we still surface a generic hint.
        let resolved = map_nested_result(nested, "fn")?;
        output.push_str(&resolved);
        output.push(')');
      }
      Token::ParenthesisBlock => {
        push_css_with_token_splice_boundary(&mut output, "(");
        let nested = parser.parse_nested_block(|nested| {
          resolve_tokens_from_parser(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "()")?;
        output.push_str(&resolved);
        output.push(')');
      }
      Token::SquareBracketBlock => {
        push_css_with_token_splice_boundary(&mut output, "[");
        let nested = parser.parse_nested_block(|nested| {
          resolve_tokens_from_parser(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "[]")?;
        output.push_str(&resolved);
        output.push(']');
      }
      Token::CurlyBracketBlock => {
        push_css_with_token_splice_boundary(&mut output, "{");
        let nested = parser.parse_nested_block(|nested| {
          resolve_tokens_from_parser(nested, custom_properties, stack, depth, property_name)
            .map_err(|err| nested.new_custom_error(err))
        });
        let resolved = map_nested_result(nested, "{}")?;
        output.push_str(&resolved);
        output.push('}');
      }
      other => push_token_to_css(&mut output, &other),
    }
  }

  Ok(output)
}

fn map_nested_result<'a, 'i, T>(
  result: Result<T, ParseError<'i, VarResolutionResult<'a>>>,
  hint: &str,
) -> Result<T, VarResolutionResult<'a>> {
  match result {
    Ok(tokens) => Ok(tokens),
    Err(err) => match err.kind {
      ParseErrorKind::Custom(inner) => Err(inner),
      _ => Err(VarResolutionResult::InvalidSyntax(hint.to_string())),
    },
  }
}

fn parse_var_function<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<Cow<'a, str>, VarResolutionResult<'a>>
where
  'a: 'i,
{
  let (var_name, fallback) = parse_var_function_arguments(parser)?;
  resolve_variable_reference(
    &var_name,
    fallback.map(Cow::Owned),
    custom_properties,
    stack,
    depth,
    property_name,
  )
}

#[derive(Debug, Clone)]
struct IfBranch {
  condition: Option<String>,
  value: String,
}

fn parse_if_function<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  let branches =
    parse_if_branches(parser).map_err(|_| VarResolutionResult::InvalidSyntax("if".into()))?;
  if branches.is_empty() {
    return Err(VarResolutionResult::InvalidSyntax("if".into()));
  }

  let mut selected: Option<&str> = None;
  for branch in &branches {
    match branch.condition.as_deref() {
      Some(cond) => {
        let cond_resolved =
          resolve_value_tokens(cond, custom_properties, stack, depth + 1, property_name)
            .map_err(|err| err)?;
        if eval_if_condition(&cond_resolved) {
          selected = Some(branch.value.as_str());
          break;
        }
      }
      None => {
        // Else branch.
        if selected.is_none() {
          selected = Some(branch.value.as_str());
        }
        break;
      }
    }
  }

  let Some(selected) = selected else {
    return Err(VarResolutionResult::InvalidSyntax("if".into()));
  };

  if !contains_ascii_case_insensitive_substitution_call(selected)
    && (!selected.as_bytes().contains(&b'\\') || !selected.as_bytes().contains(&b'('))
  {
    return Ok(selected.to_string());
  }

  resolve_value_tokens(selected, custom_properties, stack, depth + 1, property_name)
}

fn parse_first_valid_function<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  let candidates = parse_first_valid_candidates(parser)
    .map_err(|_| VarResolutionResult::InvalidSyntax("first-valid".into()))?;

  // Reject `first-valid()` since it is always invalid at computed-value time.
  if candidates.is_empty() {
    return Err(VarResolutionResult::InvalidSyntax("first-valid".into()));
  }

  for candidate in candidates {
    if candidate.is_empty() {
      continue;
    }

    let candidate_resolved = if !contains_ascii_case_insensitive_substitution_call(&candidate)
      && (!candidate.as_bytes().contains(&b'\\') || !candidate.as_bytes().contains(&b'('))
    {
      candidate
    } else {
      match resolve_value_tokens(
        &candidate,
        custom_properties,
        stack,
        depth + 1,
        property_name,
      ) {
        Ok(resolved) => resolved,
        Err(_) => continue,
      }
    };

    if parse_value_after_resolution(&candidate_resolved, property_name).is_some() {
      return Ok(candidate_resolved);
    }
  }

  Err(VarResolutionResult::InvalidSyntax("first-valid".into()))
}

fn parse_toggle_function<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  fn parse_toggle_values<'i, 't>(
    parser: &mut Parser<'i, 't>,
  ) -> Result<Vec<String>, ParseError<'i, ()>> {
    let mut values: Vec<String> = Vec::new();
    let mut current = String::new();

    fn flush_value<'i, 't>(
      parser: &Parser<'i, 't>,
      values: &mut Vec<String>,
      current: &mut String,
    ) -> Result<(), ParseError<'i, ()>> {
      let trimmed = trim_css_whitespace(current);
      if trimmed.is_empty() {
        return Err(parser.new_custom_error(()));
      }
      values.push(trimmed.to_string());
      current.clear();
      Ok(())
    }

    while let Ok(token) = parser.next_including_whitespace_and_comments() {
      match token {
        Token::Comma => {
          flush_value(parser, &mut values, &mut current)?;
        }
        Token::Function(name) => {
          push_css_with_token_splice_boundary(&mut current, name.as_ref());
          current.push('(');
          let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
          current.push_str(&nested);
          current.push(')');
        }
        Token::ParenthesisBlock => {
          push_css_with_token_splice_boundary(&mut current, "(");
          let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
          current.push_str(&nested);
          current.push(')');
        }
        Token::SquareBracketBlock => {
          push_css_with_token_splice_boundary(&mut current, "[");
          let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
          current.push_str(&nested);
          current.push(']');
        }
        Token::CurlyBracketBlock => {
          push_css_with_token_splice_boundary(&mut current, "{");
          let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
          current.push_str(&nested);
          current.push('}');
        }
        other => push_token_to_css(&mut current, &other),
      }
    }

    flush_value(parser, &mut values, &mut current)?;
    Ok(values)
  }

  // CSS Values 5: `toggle()` is an arbitrary substitution function whose result depends on the
  // parent's computed value for the property being computed.
  //
  // FastRender currently only supports computed-value matching for `<color>` properties; other
  // uses are treated as invalid at computed-value time so the declaration computes to `unset`.
  let parent =
    current_parent_style().ok_or_else(|| VarResolutionResult::InvalidSyntax("toggle".into()))?;

  let base_color = if property_name.eq_ignore_ascii_case("background-color") {
    Some(parent.background_color)
  } else if property_name.eq_ignore_ascii_case("color") {
    Some(parent.color)
  } else if property_name.eq_ignore_ascii_case("border-top-color") {
    Some(parent.border_top_color)
  } else if property_name.eq_ignore_ascii_case("border-right-color") {
    Some(parent.border_right_color)
  } else if property_name.eq_ignore_ascii_case("border-bottom-color") {
    Some(parent.border_bottom_color)
  } else if property_name.eq_ignore_ascii_case("border-left-color") {
    Some(parent.border_left_color)
  } else {
    None
  }
  .ok_or_else(|| VarResolutionResult::InvalidSyntax("toggle".into()))?;

  let raw_values =
    parse_toggle_values(parser).map_err(|_| VarResolutionResult::InvalidSyntax("toggle".into()))?;
  if raw_values.is_empty() {
    return Err(VarResolutionResult::InvalidSyntax("toggle".into()));
  }

  let mut resolved_values: Vec<String> = Vec::with_capacity(raw_values.len());
  let mut computed_values: Vec<Option<Rgba>> = Vec::with_capacity(raw_values.len());

  for raw in raw_values {
    let raw = trim_css_whitespace(&raw);
    let resolved = if !contains_ascii_case_insensitive_substitution_call(raw)
      && (!raw.as_bytes().contains(&b'\\') || !raw.as_bytes().contains(&b'('))
    {
      raw.to_string()
    } else {
      resolve_value_tokens(raw, custom_properties, stack, depth + 1, property_name)?
    };

    let computed = Color::parse(trim_css_whitespace(&resolved))
      .ok()
      .map(|color| {
        color.to_rgba_with_scheme_and_forced_colors(
          parent.color,
          parent.used_dark_color_scheme,
          parent.forced_colors,
        )
      });

    resolved_values.push(resolved);
    computed_values.push(computed);
  }

  let mut match_index: Option<usize> = None;
  for (idx, value) in computed_values.iter().enumerate() {
    if value.is_some_and(|color| color == base_color) {
      match_index = Some(idx);
    }
  }

  let selected_index = match_index
    .map(|idx| (idx + 1) % computed_values.len())
    .unwrap_or(0);

  if computed_values[selected_index].is_none() {
    return Err(VarResolutionResult::InvalidSyntax("toggle".into()));
  }

  let selected = resolved_values
    .get(selected_index)
    .map(|s| trim_css_whitespace(s).to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| VarResolutionResult::InvalidSyntax("toggle".into()))?;

  // Ensure nested substitution functions inside the selected value are resolved before returning.
  if !contains_ascii_case_insensitive_substitution_call(&selected)
    && (!selected.as_bytes().contains(&b'\\') || !selected.as_bytes().contains(&b'('))
  {
    return Ok(selected);
  }

  resolve_value_tokens(
    &selected,
    custom_properties,
    stack,
    depth + 1,
    property_name,
  )
}

fn serialize_css_string_token(value: &str) -> String {
  // Prefer a quote character that avoids introducing CSS escape sequences. Most downstream
  // property parsers in FastRender treat string contents as raw and do not interpret escapes, so
  // only fall back to escaping when the value contains both quote characters.
  if !value.contains('\'') {
    return format!("'{value}'");
  }
  if !value.contains('"') {
    return format!("\"{value}\"");
  }

  // Fall back to a double-quoted string with minimal escaping for embedded quotes/backslashes so
  // the output remains a single valid CSS string token.
  let mut out = String::with_capacity(value.len() + 2);
  out.push('"');
  for ch in value.chars() {
    if ch == '"' || ch == '\\' {
      out.push('\\');
    }
    out.push(ch);
  }
  out.push('"');
  out
}

fn parse_attr_function<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<String, VarResolutionResult<'a>>
where
  'a: 'i,
{
  let (name, ty, fallback_value) = parse_attr_function_arguments(parser)
    .map_err(|_| VarResolutionResult::InvalidSyntax("attr".into()))?;

  let resolve_fallback = |fallback: &str,
                          custom_properties: &'a CustomPropertyStore,
                          stack: &mut VarResolutionStack,
                          depth: usize|
   -> Result<String, VarResolutionResult<'a>> {
    if !contains_ascii_case_insensitive_substitution_call(fallback)
      && (!fallback.as_bytes().contains(&b'\\') || !fallback.as_bytes().contains(&b'('))
    {
      return Ok(fallback.to_string());
    }
    resolve_value_tokens(fallback, custom_properties, stack, depth + 1, property_name)
  };

  let mut attr_value = current_style_element()
    .and_then(|el| el.get_attribute_ref(&name))
    .unwrap_or("")
    .to_string();
  attr_value = trim_css_whitespace(&attr_value).to_string();

  // Missing attribute.
  if attr_value.is_empty()
    && current_style_element()
      .and_then(|el| el.get_attribute_ref(&name))
      .is_none()
  {
    if let Some(fallback_text) = fallback_value.as_deref() {
      return resolve_fallback(fallback_text, custom_properties, stack, depth);
    }
    return Err(VarResolutionResult::InvalidSyntax("attr".into()));
  }

  // Resolve substitution functions inside the attribute value before type parsing.
  if contains_ascii_case_insensitive_substitution_call(&attr_value)
    || (attr_value.as_bytes().contains(&b'\\') && attr_value.as_bytes().contains(&b'('))
  {
    match resolve_value_tokens(
      &attr_value,
      custom_properties,
      stack,
      depth + 1,
      property_name,
    ) {
      Ok(resolved) => attr_value = resolved,
      Err(_) => {
        if let Some(fallback_text) = fallback_value.as_deref() {
          return resolve_fallback(fallback_text, custom_properties, stack, depth);
        }
        return Err(VarResolutionResult::InvalidSyntax("attr".into()));
      }
    }
  }

  let resolved = match ty.as_deref().map(trim_css_whitespace) {
    None | Some("") => Some(serialize_css_string_token(&attr_value)),
    Some(raw_ty) if raw_ty.eq_ignore_ascii_case("string") => {
      Some(serialize_css_string_token(&attr_value))
    }
    Some(raw_ty) => resolve_typed_attr_value(&attr_value, raw_ty),
  };

  if let Some(resolved) = resolved {
    return Ok(resolved);
  }

  if let Some(fallback_text) = fallback_value.as_deref() {
    return resolve_fallback(fallback_text, custom_properties, stack, depth);
  }

  Err(VarResolutionResult::InvalidSyntax("attr".into()))
}

fn parse_var_function_arguments<'a, 'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<(String, Option<String>), VarResolutionResult<'a>> {
  let mut var_name: Option<String> = None;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::WhiteSpace(_) | Token::Comment(_) => continue,
      Token::Ident(ident) => {
        let name = ident.as_ref().to_string();
        if !name.starts_with("--") {
          return Err(VarResolutionResult::InvalidSyntax(name));
        }
        var_name = Some(name);
        break;
      }
      other => {
        return Err(VarResolutionResult::InvalidSyntax(token_to_css_string(
          &other,
        )))
      }
    }
  }

  let Some(name) = var_name else {
    return Err(VarResolutionResult::InvalidSyntax(String::new()));
  };

  let fallback_start = loop {
    match parser.next_including_whitespace_and_comments() {
      Ok(Token::WhiteSpace(_) | Token::Comment(_)) => continue,
      Ok(Token::Comma) => break parser.position(),
      Ok(other) => {
        return Err(VarResolutionResult::InvalidSyntax(token_to_css_string(
          &other,
        )))
      }
      Err(_) => return Ok((name, None)),
    }
  };

  while let Ok(_) = parser.next_including_whitespace_and_comments() {}
  let fallback_slice = parser.slice_from(fallback_start);
  Ok((name, Some(fallback_slice.to_string())))
}

fn parse_attr_function_arguments<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<(String, Option<String>, Option<String>), ParseError<'i, ()>> {
  let mut name: Option<String> = None;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::WhiteSpace(_) | Token::Comment(_) => continue,
      Token::Ident(ident) => {
        name = Some(ident.as_ref().to_string());
        break;
      }
      _ => return Err(parser.new_custom_error::<(), ()>(())),
    }
  }

  let Some(name) = name else {
    return Err(parser.new_custom_error::<(), ()>(()));
  };

  // Optional type/unit + optional fallback.
  let token = loop {
    match parser.next_including_whitespace_and_comments() {
      Ok(Token::WhiteSpace(_) | Token::Comment(_)) => continue,
      Ok(token) => break Some(token),
      Err(_) => break None,
    }
  };

  let mut ty: Option<String> = None;
  let mut fallback: Option<String> = None;

  match token {
    None => return Ok((name, None, None)),
    Some(Token::Comma) => {
      let start = parser.position();
      while let Ok(_) = parser.next_including_whitespace_and_comments() {}
      fallback = Some(parser.slice_from(start).to_string());
      return Ok((name, None, fallback));
    }
    Some(Token::Ident(ident)) => {
      ty = Some(ident.as_ref().to_string());
    }
    _ => return Err(parser.new_custom_error::<(), ()>(())),
  }

  // After type, expect optional comma + fallback or end.
  let token = loop {
    match parser.next_including_whitespace_and_comments() {
      Ok(Token::WhiteSpace(_) | Token::Comment(_)) => continue,
      Ok(token) => break Some(token),
      Err(_) => break None,
    }
  };

  match token {
    None => Ok((name, ty, None)),
    Some(Token::Comma) => {
      let start = parser.position();
      while let Ok(_) = parser.next_including_whitespace_and_comments() {}
      fallback = Some(parser.slice_from(start).to_string());
      Ok((name, ty, fallback))
    }
    _ => Err(parser.new_custom_error::<(), ()>(())),
  }
}

fn resolve_typed_attr_value(attr_value: &str, ty: &str) -> Option<String> {
  let ty = trim_css_whitespace(ty);
  if ty.is_empty() {
    return None;
  }
  let attr_value = trim_css_whitespace(attr_value);
  let ty_lower = ty.to_ascii_lowercase();

  match ty_lower.as_str() {
    "length" | "length-percentage" => parse_length(attr_value).map(|_| attr_value.to_string()),
    "number" => crate::css::properties::parse_function_number(attr_value)
      .filter(|v| v.is_finite())
      .map(|_| attr_value.to_string()),
    "integer" => attr_value.parse::<i32>().ok().map(|v| v.to_string()),
    "color" => Color::parse(attr_value)
      .ok()
      .map(|_| attr_value.to_string()),
    "url" => {
      if attr_value.is_empty() {
        return None;
      }
      // Typed `attr(... url)` resolves the element attribute as a `<url>` token.
      //
      // Treat the attribute value as the raw URL string (i.e. authors do *not* write `url(...)` in
      // the attribute) and serialize it as a single `url(...)` token. Escape backslashes and the
      // quote delimiter so the produced token is always valid CSS and cannot inject additional
      // tokens into the property value stream.
      //
      // Note: our property-value parser unescapes CSS escapes inside `url(...)`, so the stored URL
      // string matches the original attribute value.
      let quote = if !attr_value.contains('\'') {
        '\''
      } else {
        '"'
      };
      let mut out = String::with_capacity(attr_value.len() + 6);
      out.push_str("url(");
      out.push(quote);
      for ch in attr_value.chars() {
        match ch {
          '\\' => out.push_str("\\\\"),
          '\n' => out.push_str("\\A "),
          '\r' => out.push_str("\\D "),
          '\u{000C}' => out.push_str("\\C "),
          ch if ch == quote => {
            out.push('\\');
            out.push(ch);
          }
          other => out.push(other),
        }
      }
      out.push(quote);
      out.push(')');
      Some(out)
    }
    unit => {
      // Unit-form typed attr: `attr(data-w px, 10px)` (treat attribute value as a number in the
      // given unit).
      if !is_length_unit_ident(unit) {
        return None;
      }
      if parse_length(attr_value).is_some() {
        return Some(attr_value.to_string());
      }
      let num = crate::css::properties::parse_function_number(attr_value)?;
      if !num.is_finite() {
        return None;
      }
      Some(format!("{num}{unit}"))
    }
  }
}

fn is_length_unit_ident(ident: &str) -> bool {
  matches!(
    ident,
    "dvmin"
      | "dvmax"
      | "svmin"
      | "svmax"
      | "lvmin"
      | "lvmax"
      | "dvw"
      | "dvh"
      | "dvi"
      | "dvb"
      | "cqmin"
      | "cqmax"
      | "cqw"
      | "cqh"
      | "cqi"
      | "cqb"
      | "svi"
      | "svb"
      | "lvi"
      | "lvb"
      | "svw"
      | "svh"
      | "lvw"
      | "lvh"
      | "vi"
      | "vb"
      | "vmin"
      | "vmax"
      | "vw"
      | "vh"
      | "rem"
      | "em"
      | "ex"
      | "ch"
      | "lh"
      | "px"
      | "pc"
      | "pt"
      | "cm"
      | "mm"
      | "q"
      | "in"
  )
}

fn parse_if_branches<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<Vec<IfBranch>, ParseError<'i, ()>> {
  let mut branches: Vec<IfBranch> = Vec::new();
  let mut condition = String::new();
  let mut value = String::new();
  let mut saw_colon = false;

  fn flush_branch<'i, 't>(
    parser: &Parser<'i, 't>,
    branches: &mut Vec<IfBranch>,
    condition: &mut String,
    value: &mut String,
    saw_colon: &mut bool,
  ) -> Result<(), ParseError<'i, ()>> {
    let cond_trimmed = trim_css_whitespace(condition);
    if *saw_colon && cond_trimmed.is_empty() {
      return Err(parser.new_custom_error::<(), ()>(()));
    }

    if *saw_colon {
      branches.push(IfBranch {
        condition: Some(cond_trimmed.to_string()),
        value: trim_css_whitespace(value).to_string(),
      });
    } else {
      // Else branch: value only (may be empty).
      branches.push(IfBranch {
        condition: None,
        value: cond_trimmed.to_string(),
      });
    }

    condition.clear();
    value.clear();
    *saw_colon = false;
    Ok(())
  }

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Semicolon => {
        // Only conditional branches use `;` separators. An else branch (no colon) must be the final
        // branch and therefore cannot be terminated by `;`.
        if !saw_colon {
          return Err(parser.new_custom_error::<(), ()>(()));
        }
        flush_branch(
          parser,
          &mut branches,
          &mut condition,
          &mut value,
          &mut saw_colon,
        )?;
      }
      Token::Colon if !saw_colon => {
        saw_colon = true;
      }
      Token::Function(name) => {
        let target = if saw_colon {
          &mut value
        } else {
          &mut condition
        };
        push_css_with_token_splice_boundary(target, name.as_ref());
        target.push('(');
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        target.push_str(&nested);
        target.push(')');
      }
      Token::ParenthesisBlock => {
        let target = if saw_colon {
          &mut value
        } else {
          &mut condition
        };
        push_css_with_token_splice_boundary(target, "(");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        target.push_str(&nested);
        target.push(')');
      }
      Token::SquareBracketBlock => {
        let target = if saw_colon {
          &mut value
        } else {
          &mut condition
        };
        push_css_with_token_splice_boundary(target, "[");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        target.push_str(&nested);
        target.push(']');
      }
      Token::CurlyBracketBlock => {
        let target = if saw_colon {
          &mut value
        } else {
          &mut condition
        };
        push_css_with_token_splice_boundary(target, "{");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        target.push_str(&nested);
        target.push('}');
      }
      other => {
        let target = if saw_colon {
          &mut value
        } else {
          &mut condition
        };
        push_token_to_css(target, &other);
      }
    }
  }

  flush_branch(
    parser,
    &mut branches,
    &mut condition,
    &mut value,
    &mut saw_colon,
  )?;

  if branches.iter().all(|b| b.condition.is_none()) {
    // Reject `if(<else-value>)` since it's indistinguishable from authoring the else value
    // directly, and browsers currently treat it as invalid.
    return Err(parser.new_custom_error::<(), ()>(()));
  }

  Ok(branches)
}

fn parse_first_valid_candidates<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<Vec<String>, ParseError<'i, ()>> {
  let mut candidates = Vec::new();
  let mut current = String::new();

  fn flush(candidates: &mut Vec<String>, current: &mut String) {
    candidates.push(trim_css_whitespace(current).to_string());
    current.clear();
  }

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Comma => flush(&mut candidates, &mut current),
      Token::Function(name) => {
        push_css_with_token_splice_boundary(&mut current, name.as_ref());
        current.push('(');
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        current.push_str(&nested);
        current.push(')');
      }
      Token::ParenthesisBlock => {
        push_css_with_token_splice_boundary(&mut current, "(");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        current.push_str(&nested);
        current.push(')');
      }
      Token::SquareBracketBlock => {
        push_css_with_token_splice_boundary(&mut current, "[");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        current.push_str(&nested);
        current.push(']');
      }
      Token::CurlyBracketBlock => {
        push_css_with_token_splice_boundary(&mut current, "{");
        let nested = parser.parse_nested_block(|nested| stringify_tokens(nested))?;
        current.push_str(&nested);
        current.push('}');
      }
      other => push_token_to_css(&mut current, &other),
    }
  }

  flush(&mut candidates, &mut current);
  Ok(candidates)
}

fn stringify_tokens<'i, 't, E>(parser: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, E>> {
  let mut output = String::new();
  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name) => {
        push_css_with_token_splice_boundary(&mut output, name.as_ref());
        output.push('(');
        let inner = parser.parse_nested_block(stringify_tokens)?;
        output.push_str(&inner);
        output.push(')');
      }
      Token::ParenthesisBlock => {
        push_css_with_token_splice_boundary(&mut output, "(");
        let inner = parser.parse_nested_block(stringify_tokens)?;
        output.push_str(&inner);
        output.push(')');
      }
      Token::SquareBracketBlock => {
        push_css_with_token_splice_boundary(&mut output, "[");
        let inner = parser.parse_nested_block(stringify_tokens)?;
        output.push_str(&inner);
        output.push(']');
      }
      Token::CurlyBracketBlock => {
        push_css_with_token_splice_boundary(&mut output, "{");
        let inner = parser.parse_nested_block(stringify_tokens)?;
        output.push_str(&inner);
        output.push('}');
      }
      other => push_token_to_css(&mut output, &other),
    }
  }
  Ok(output)
}

fn eval_if_condition(condition: &str) -> bool {
  let mut input = ParserInput::new(condition);
  let mut parser = Parser::new(&mut input);
  match parse_if_condition(&mut parser) {
    Ok(result) => {
      parser.skip_whitespace();
      parser.is_exhausted() && result
    }
    Err(_) => false,
  }
}

fn parse_if_condition<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<bool, ParseError<'i, ()>> {
  parse_if_disjunction(parser)
}

fn parse_if_disjunction<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<bool, ParseError<'i, ()>> {
  let mut value = parse_if_conjunction(parser)?;
  loop {
    parser.skip_whitespace();
    if parser.try_parse(|p| p.expect_ident_matching("or")).is_ok() {
      parser.skip_whitespace();
      value = value || parse_if_conjunction(parser)?;
    } else {
      break;
    }
  }
  Ok(value)
}

fn parse_if_conjunction<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<bool, ParseError<'i, ()>> {
  let mut value = parse_if_negation(parser)?;
  loop {
    parser.skip_whitespace();
    if parser.try_parse(|p| p.expect_ident_matching("and")).is_ok() {
      parser.skip_whitespace();
      value = value && parse_if_negation(parser)?;
    } else {
      break;
    }
  }
  Ok(value)
}

fn parse_if_negation<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<bool, ParseError<'i, ()>> {
  parser.skip_whitespace();
  if parser.try_parse(|p| p.expect_ident_matching("not")).is_ok() {
    parser.skip_whitespace();
    return Ok(!parse_if_negation(parser)?);
  }
  parse_if_term(parser)
}

fn parse_if_term<'i, 't>(parser: &mut Parser<'i, 't>) -> Result<bool, ParseError<'i, ()>> {
  parser.skip_whitespace();
  match parser.next()? {
    Token::Function(name) => {
      let name = name.to_string();
      let args = parser.parse_nested_block(stringify_tokens)?;
      eval_if_test_function(&name, &args)
    }
    Token::Ident(ident) => {
      if ident.eq_ignore_ascii_case("true") {
        Ok(true)
      } else if ident.eq_ignore_ascii_case("false") {
        Ok(false)
      } else {
        Err(parser.new_custom_error::<(), ()>(()))
      }
    }
    Token::ParenthesisBlock => parser.parse_nested_block(|nested| {
      let value = parse_if_condition(nested)?;
      nested.skip_whitespace();
      if nested.is_exhausted() {
        Ok(value)
      } else {
        Err(nested.new_custom_error::<(), ()>(()))
      }
    }),
    _ => Err(parser.new_custom_error::<(), ()>(())),
  }
}

fn eval_if_test_function<'i>(name: &str, args: &str) -> Result<bool, ParseError<'i, ()>> {
  let args = trim_css_whitespace(args);
  if name.eq_ignore_ascii_case("media") {
    let viewport = current_viewport().unwrap_or(Size::new(0.0, 0.0));
    let mut ctx = MediaContext::screen(viewport.width, viewport.height);
    ctx.prefers_color_scheme = current_color_scheme_pref();
    let Ok(queries) = MediaQuery::parse_list(args) else {
      return Ok(false);
    };
    return Ok(ctx.evaluate_list(&queries));
  }

  if name.eq_ignore_ascii_case("supports") {
    let cond = crate::css::parser::parse_supports_prelude(args);
    return Ok(cond.matches());
  }

  // Unsupported if() test function.
  Ok(false)
}

fn resolve_variable_reference<'a>(
  name: &str,
  fallback: Option<Cow<'a, str>>,
  custom_properties: &'a CustomPropertyStore,
  stack: &mut VarResolutionStack,
  depth: usize,
  property_name: &str,
) -> Result<Cow<'a, str>, VarResolutionResult<'a>> {
  if depth >= MAX_RECURSION_DEPTH {
    return Err(VarResolutionResult::RecursionLimitExceeded);
  }

  let resolve_fallback = |fallback_value: Cow<'a, str>,
                          stack: &mut VarResolutionStack|
   -> Result<Cow<'a, str>, VarResolutionResult<'a>> {
    // Same fast-path as below for literal fallback tokens.
    if !contains_ascii_case_insensitive_substitution_call(fallback_value.as_ref())
      && (!fallback_value.as_ref().as_bytes().contains(&b'\\')
        || !fallback_value.as_ref().as_bytes().contains(&b'('))
    {
      return Ok(fallback_value);
    }

    resolve_value_tokens(
      fallback_value.as_ref(),
      custom_properties,
      stack,
      depth + 1,
      property_name,
    )
    .map(Cow::Owned)
    .map_err(|err| match err {
      VarResolutionResult::NotFound(_) => VarResolutionResult::NotFound(name.to_string()),
      other => other,
    })
  };

  if stack.contains(name) {
    if let Some(fallback_value) = fallback {
      return resolve_fallback(fallback_value, stack);
    }
    return Err(VarResolutionResult::RecursionLimitExceeded);
  }

  if let Some(value) = custom_properties.get(name) {
    // Fast path: if the custom property value can't possibly contain var() references (including
    // escape-hiding), we can skip a full cssparser token walk and just substitute the raw tokens.
    let raw = value.value.as_str();
    if !contains_ascii_case_insensitive_substitution_call(raw)
      && (!raw.as_bytes().contains(&b'\\') || !raw.as_bytes().contains(&b'('))
    {
      let resolved = Cow::Borrowed(raw);
      if starts_with_css_wide_keyword_with_trailing_tokens(resolved.as_ref()) {
        if let Some(fallback_value) = fallback {
          return resolve_fallback(fallback_value, stack);
        }
        return Err(VarResolutionResult::NotFound(name.to_string()));
      }
      return Ok(resolved);
    }

    stack.push(name);
    let resolved = if !raw.as_bytes().contains(&b'\\') {
      if let Some((nested_name, nested_fallback)) = parse_simple_var_call(raw) {
        resolve_variable_reference(
          nested_name,
          nested_fallback.map(Cow::Borrowed),
          custom_properties,
          stack,
          depth + 1,
          property_name,
        )
      } else {
        resolve_value_tokens(raw, custom_properties, stack, depth + 1, property_name)
          .map(Cow::Owned)
      }
    } else {
      resolve_value_tokens(raw, custom_properties, stack, depth + 1, property_name).map(Cow::Owned)
    };
    stack.pop();

    match resolved {
      Ok(resolved) => {
        if starts_with_css_wide_keyword_with_trailing_tokens(resolved.as_ref()) {
          if let Some(fallback_value) = fallback {
            return resolve_fallback(fallback_value, stack);
          }
          return Err(VarResolutionResult::NotFound(name.to_string()));
        }
        return Ok(resolved);
      }
      Err(err) => {
        // If the referenced custom property is present but its computed value is invalid (for
        // example because it references a missing variable), treat this like an "invalid at
        // computed value time" custom property and fall back to the var() fallback argument.
        if let Some(fallback_value) = fallback {
          return resolve_fallback(fallback_value, stack);
        }
        return Err(match err {
          VarResolutionResult::NotFound(_) => VarResolutionResult::NotFound(name.to_string()),
          other => other,
        });
      }
    }
  }

  if let Some(fallback_value) = fallback {
    return resolve_fallback(fallback_value, stack);
  }

  Err(VarResolutionResult::NotFound(name.to_string()))
}

fn contains_var_or_if_substitution_function(value: &str) -> bool {
  let bytes = value.as_bytes();
  if bytes.len() < 3 {
    return false;
  }

  #[inline]
  fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') || b >= 0x80
  }

  let mut in_string: Option<u8> = None;
  let mut in_comment = false;
  let mut has_backslash = false;
  let mut has_open_paren = false;

  let mut i = 0usize;
  while i < bytes.len() {
    let byte = bytes[i];

    if in_comment {
      if byte == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if let Some(quote) = in_string {
      if byte == b'\\' {
        i = (i + 2).min(bytes.len());
        continue;
      }
      if byte == quote {
        in_string = None;
      }
      i += 1;
      continue;
    }

    // Not inside a string/comment.
    if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
      in_comment = true;
      i += 2;
      continue;
    }

    if byte == b'"' || byte == b'\'' {
      in_string = Some(byte);
      i += 1;
      continue;
    }

    if byte == b'\\' {
      has_backslash = true;
    } else if byte == b'(' {
      has_open_paren = true;
    }

    let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
    let prev_is_ident = prev.is_some_and(is_ident_byte);

    // `if(`
    if !prev_is_ident
      && i + 2 < bytes.len()
      && byte.to_ascii_lowercase() == b'i'
      && bytes[i + 1].to_ascii_lowercase() == b'f'
      && bytes[i + 2] == b'('
    {
      return true;
    }

    // `var(`
    if !prev_is_ident
      && i + 3 < bytes.len()
      && byte.to_ascii_lowercase() == b'v'
      && bytes[i + 1].to_ascii_lowercase() == b'a'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3] == b'('
    {
      return true;
    }

    // `first-valid(`
    if !prev_is_ident
      && i + 11 < bytes.len()
      && byte.to_ascii_lowercase() == b'f'
      && bytes[i + 1].to_ascii_lowercase() == b'i'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3].to_ascii_lowercase() == b's'
      && bytes[i + 4].to_ascii_lowercase() == b't'
      && bytes[i + 5] == b'-'
      && bytes[i + 6].to_ascii_lowercase() == b'v'
      && bytes[i + 7].to_ascii_lowercase() == b'a'
      && bytes[i + 8].to_ascii_lowercase() == b'l'
      && bytes[i + 9].to_ascii_lowercase() == b'i'
      && bytes[i + 10].to_ascii_lowercase() == b'd'
      && bytes[i + 11] == b'('
    {
      return true;
    }

    // `toggle(`
    if !prev_is_ident
      && i + 6 < bytes.len()
      && byte.to_ascii_lowercase() == b't'
      && bytes[i + 1].to_ascii_lowercase() == b'o'
      && bytes[i + 2].to_ascii_lowercase() == b'g'
      && bytes[i + 3].to_ascii_lowercase() == b'g'
      && bytes[i + 4].to_ascii_lowercase() == b'l'
      && bytes[i + 5].to_ascii_lowercase() == b'e'
      && bytes[i + 6] == b'('
    {
      return true;
    }

    i += 1;
  }

  if has_backslash && has_open_paren {
    return contains_var_or_if_substitution_function_via_cssparser(value);
  }

  false
}

fn contains_var_or_if_substitution_function_via_cssparser(value: &str) -> bool {
  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);
  contains_var_or_if_substitution_function_in_parser(&mut parser)
}

fn contains_var_or_if_substitution_function_in_parser<'i, 't>(parser: &mut Parser<'i, 't>) -> bool {
  let mut found = false;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name)
        if name.eq_ignore_ascii_case("var")
          || name.eq_ignore_ascii_case("if")
          || name.eq_ignore_ascii_case("first-valid")
          || name.eq_ignore_ascii_case("toggle") =>
      {
        found = true;
        let _ = parser.parse_nested_block(|nested| {
          Ok::<_, ParseError<'i, ()>>(contains_var_or_if_substitution_function_in_parser(nested))
        });
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        if let Ok(nested_found) = parser.parse_nested_block(|nested| {
          Ok::<_, ParseError<'i, ()>>(contains_var_or_if_substitution_function_in_parser(nested))
        }) {
          if nested_found {
            found = true;
          }
        }
      }
      _ => {}
    }
  }

  found
}

fn parse_value_after_resolution(value: &str, property_name: &str) -> Option<PropertyValue> {
  if property_name.eq_ignore_ascii_case("content") {
    if contains_var_or_if_substitution_function(value) {
      return None;
    }
  } else if contains_arbitrary_substitution_function(value) {
    return None;
  }

  if property_name.is_empty() {
    Some(parse_untyped_value(value))
  } else {
    parse_property_value_after_var_resolution(property_name, value)
  }
}

fn parse_untyped_value(value: &str) -> PropertyValue {
  let trimmed = trim_css_whitespace(value);
  if let Some(len) = parse_length(trimmed) {
    return PropertyValue::Length(len);
  }
  if let Ok(num) = trimmed.parse::<f32>() {
    return PropertyValue::Number(num);
  }
  if trimmed.ends_with('%') {
    if let Ok(num) = trimmed[..trimmed.len() - 1].parse::<f32>() {
      return PropertyValue::Percentage(num);
    }
  }
  PropertyValue::Keyword(trimmed.to_string())
}

#[inline]
fn push_token_to_css(out: &mut String, token: &Token) {
  let needs_boundary = match token {
    Token::WhiteSpace(_) | Token::Comment(_) => false,
    _ => true,
  };

  if needs_boundary {
    if let Some(prev) = out.as_bytes().last().copied() {
      // Compute the first byte of the serialized token so we can decide whether a token-splice
      // boundary separator is required.
      let (first, second) = match token {
        Token::Ident(ident) => (ident.as_bytes()[0], ident.as_bytes().get(1).copied()),
        Token::AtKeyword(_) => (b'@', None),
        Token::Hash(_) | Token::IDHash(_) => (b'#', None),
        Token::QuotedString(_) => (b'"', None),
        Token::UnquotedUrl(_) | Token::BadUrl(_) => (b'u', Some(b'r')),
        Token::Number {
          has_sign, value, ..
        }
        | Token::Percentage {
          has_sign,
          unit_value: value,
          ..
        }
        | Token::Dimension {
          has_sign, value, ..
        } => {
          if value.is_sign_negative() {
            (b'-', None)
          } else if *has_sign {
            (b'+', None)
          } else {
            (b'0', None)
          }
        }
        Token::Delim(ch) => (*ch as u8, None),
        Token::Colon => (b':', None),
        Token::Semicolon => (b';', None),
        Token::Comma => (b',', None),
        Token::IncludeMatch => (b'~', Some(b'=')),
        Token::DashMatch => (b'|', Some(b'=')),
        Token::PrefixMatch => (b'^', Some(b'=')),
        Token::SuffixMatch => (b'$', Some(b'=')),
        Token::SubstringMatch => (b'*', Some(b'=')),
        Token::CDO => (b'<', Some(b'!')),
        Token::CDC => (b'-', Some(b'-')),
        // Fallback: this token kind isn't important for our splice-boundary heuristic.
        _ => (b'?', None),
      };
      if needs_token_splice_separator(prev, first, second) {
        out.push_str(TOKEN_SPLICE_SEPARATOR);
      }
    }
  }

  match token {
    Token::WhiteSpace(ws) => out.push_str(ws.as_ref()),
    Token::Comment(text) => {
      out.push_str("/*");
      out.push_str(text.as_ref());
      out.push_str("*/");
    }
    // `cssparser`'s `to_css_string()` escapes quotes inside strings/URLs to guarantee the output is
    // valid CSS. Our property-value parser, however, consumes the resolved string without
    // interpreting CSS string escapes (it expects the raw token contents). This mismatch can turn
    // `"` into `\"` inside `data:` URLs (e.g. SVG XML), breaking downstream consumers like `usvg`.
    //
    // Prefer emitting quoted strings with a quote character that does not appear in the content so
    // we can preserve the raw value without adding backslash escapes.
    Token::QuotedString(text) => {
      let raw = text.as_ref();
      if !raw.contains('\'') {
        out.push('\'');
        out.push_str(raw);
        out.push('\'');
      } else if !raw.contains('"') {
        out.push('"');
        out.push_str(raw);
        out.push('"');
      } else {
        let _ = token.to_css(out);
      }
    }
    other => {
      let _ = other.to_css(out);
    }
  }
}

fn token_to_css_string(token: &Token) -> String {
  match token {
    Token::WhiteSpace(ws) => ws.to_string(),
    Token::Comment(text) => format!("/*{}*/", text),
    Token::QuotedString(text) => {
      let raw = text.as_ref();
      if !raw.contains('\'') {
        format!("'{raw}'")
      } else if !raw.contains('"') {
        format!("\"{raw}\"")
      } else {
        let mut out = String::new();
        let _ = token.to_css(&mut out);
        out
      }
    }
    _ => {
      let mut out = String::new();
      let _ = token.to_css(&mut out);
      out
    }
  }
}

/// Checks if a string contains any var() references (case-insensitive)
pub fn contains_var(value: &str) -> bool {
  // `parse_known_property_value` calls this for *every* declaration value while parsing CSS.
  // Tokenizing each value with `cssparser` is very expensive for large stylesheets, so use a
  // cheap substring-based detector with a rare correctness slow-path.
  //
  // Function tokens cannot contain whitespace between the name and `(`, so the literal `var(`
  // is sufficient for the fast path (with ASCII-case-insensitive matching).
  let bytes = value.as_bytes();
  if bytes.len() < 4 {
    return false;
  }

  #[inline]
  fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') || b >= 0x80
  }

  let mut in_string: Option<u8> = None;
  let mut in_comment = false;
  let mut has_backslash = false;
  let mut has_open_paren = false;

  let mut i = 0usize;
  while i < bytes.len() {
    let byte = bytes[i];

    if in_comment {
      if byte == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if let Some(quote) = in_string {
      if byte == b'\\' {
        // Skip the escaped byte so `\"` doesn't terminate the string.
        i = (i + 2).min(bytes.len());
        continue;
      }
      if byte == quote {
        in_string = None;
      }
      i += 1;
      continue;
    }

    // Not inside a string/comment.
    if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
      in_comment = true;
      i += 2;
      continue;
    }

    if byte == b'"' || byte == b'\'' {
      in_string = Some(byte);
      i += 1;
      continue;
    }

    if byte == b'\\' {
      has_backslash = true;
    } else if byte == b'(' {
      has_open_paren = true;
    }

    if i + 3 < bytes.len()
      && byte.to_ascii_lowercase() == b'v'
      && bytes[i + 1].to_ascii_lowercase() == b'a'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3] == b'('
    {
      // `var(` must be the full function name, so ensure the match is not preceded by an
      // identifier character (e.g. `somevar(` should not match).
      let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
      if prev.map_or(true, |b| !is_ident_byte(b)) {
        return true;
      }
    }

    i += 1;
  }

  // Escaped function names (e.g. `v\61 r(`) require a proper tokenizer to interpret escapes.
  //
  // A function token also requires a literal `(` delimiter, so if the raw string contains
  // backslashes but *no* `(` at all, it's impossible for it to contain a `var()` call.
  if has_backslash && has_open_paren {
    return contains_var_via_cssparser(value);
  }

  false
}

/// Checks if a string contains any "arbitrary substitution functions" that FastRender resolves at
/// computed-value time.
///
/// This is a superset of [`contains_var`] that additionally detects CSS Values 5 `if()`,
/// `first-valid()`, typed `attr()`, and `toggle()` functions. Like `var()`, these functions must not
/// be eagerly parsed at stylesheet parse time because their substitution result is only knowable at
/// computed-value time.
///
/// The detector ignores occurrences inside comments and strings, and has a correctness slow-path
/// for escaped function names.
pub fn contains_arbitrary_substitution_function(value: &str) -> bool {
  let bytes = value.as_bytes();
  if bytes.len() < 3 {
    return false;
  }

  #[inline]
  fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') || b >= 0x80
  }

  let mut in_string: Option<u8> = None;
  let mut in_comment = false;
  let mut has_backslash = false;
  let mut has_open_paren = false;

  let mut i = 0usize;
  while i < bytes.len() {
    let byte = bytes[i];

    if in_comment {
      if byte == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
        in_comment = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }

    if let Some(quote) = in_string {
      if byte == b'\\' {
        i = (i + 2).min(bytes.len());
        continue;
      }
      if byte == quote {
        in_string = None;
      }
      i += 1;
      continue;
    }

    // Not inside a string/comment.
    if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
      in_comment = true;
      i += 2;
      continue;
    }

    if byte == b'"' || byte == b'\'' {
      in_string = Some(byte);
      i += 1;
      continue;
    }

    if byte == b'\\' {
      has_backslash = true;
    } else if byte == b'(' {
      has_open_paren = true;
    }

    // Fast path: look for ASCII-case-insensitive function names followed by a literal `(`.
    //
    // Function tokens cannot contain whitespace between the name and `(`, so scanning for the
    // literal substring is sufficient on the non-escape path.
    let prev = i.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
    let prev_is_ident = prev.is_some_and(is_ident_byte);

    // `if(`
    if !prev_is_ident
      && i + 2 < bytes.len()
      && byte.to_ascii_lowercase() == b'i'
      && bytes[i + 1].to_ascii_lowercase() == b'f'
      && bytes[i + 2] == b'('
    {
      return true;
    }

    // `var(`
    if !prev_is_ident
      && i + 3 < bytes.len()
      && byte.to_ascii_lowercase() == b'v'
      && bytes[i + 1].to_ascii_lowercase() == b'a'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3] == b'('
    {
      return true;
    }

    // `first-valid(`
    if !prev_is_ident
      && i + 11 < bytes.len()
      && byte.to_ascii_lowercase() == b'f'
      && bytes[i + 1].to_ascii_lowercase() == b'i'
      && bytes[i + 2].to_ascii_lowercase() == b'r'
      && bytes[i + 3].to_ascii_lowercase() == b's'
      && bytes[i + 4].to_ascii_lowercase() == b't'
      && bytes[i + 5] == b'-'
      && bytes[i + 6].to_ascii_lowercase() == b'v'
      && bytes[i + 7].to_ascii_lowercase() == b'a'
      && bytes[i + 8].to_ascii_lowercase() == b'l'
      && bytes[i + 9].to_ascii_lowercase() == b'i'
      && bytes[i + 10].to_ascii_lowercase() == b'd'
      && bytes[i + 11] == b'('
    {
      return true;
    }

    // `attr(`
    if !prev_is_ident
      && i + 4 < bytes.len()
      && byte.to_ascii_lowercase() == b'a'
      && bytes[i + 1].to_ascii_lowercase() == b't'
      && bytes[i + 2].to_ascii_lowercase() == b't'
      && bytes[i + 3].to_ascii_lowercase() == b'r'
      && bytes[i + 4] == b'('
    {
      return true;
    }

    // `toggle(`
    if !prev_is_ident
      && i + 6 < bytes.len()
      && byte.to_ascii_lowercase() == b't'
      && bytes[i + 1].to_ascii_lowercase() == b'o'
      && bytes[i + 2].to_ascii_lowercase() == b'g'
      && bytes[i + 3].to_ascii_lowercase() == b'g'
      && bytes[i + 4].to_ascii_lowercase() == b'l'
      && bytes[i + 5].to_ascii_lowercase() == b'e'
      && bytes[i + 6] == b'('
    {
      return true;
    }

    i += 1;
  }

  // Escaped function names (e.g. `v\\61 r(`) require a proper tokenizer to interpret escapes.
  if has_backslash && has_open_paren {
    return contains_arbitrary_substitution_function_via_cssparser(value);
  }

  false
}

pub(crate) fn contains_arbitrary_substitution_function_via_cssparser(value: &str) -> bool {
  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);
  contains_arbitrary_substitution_function_in_parser(&mut parser)
}

fn contains_arbitrary_substitution_function_in_parser<'i, 't>(parser: &mut Parser<'i, 't>) -> bool {
  let mut found = false;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name)
        if name.eq_ignore_ascii_case("var")
          || name.eq_ignore_ascii_case("if")
          || name.eq_ignore_ascii_case("attr")
          || name.eq_ignore_ascii_case("first-valid")
          || name.eq_ignore_ascii_case("toggle") =>
      {
        found = true;
        let _ = parser.parse_nested_block(|nested| {
          Ok::<_, ParseError<'i, ()>>(contains_arbitrary_substitution_function_in_parser(nested))
        });
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        if let Ok(nested_found) = parser.parse_nested_block(|nested| {
          Ok::<_, ParseError<'i, ()>>(contains_arbitrary_substitution_function_in_parser(nested))
        }) {
          if nested_found {
            found = true;
          }
        }
      }
      _ => {}
    }
  }

  found
}

pub(crate) fn contains_var_via_cssparser(value: &str) -> bool {
  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);
  contains_var_in_parser(&mut parser)
}

fn contains_var_in_parser<'i, 't>(parser: &mut Parser<'i, 't>) -> bool {
  let mut found = false;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name) if name.eq_ignore_ascii_case("var") => {
        found = true;
        let _ = parser
          .parse_nested_block(|nested| Ok::<_, ParseError<'i, ()>>(contains_var_in_parser(nested)));
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        if let Ok(nested_found) = parser
          .parse_nested_block(|nested| Ok::<_, ParseError<'i, ()>>(contains_var_in_parser(nested)))
        {
          if nested_found {
            found = true;
          }
        }
      }
      _ => {}
    }
  }

  found
}

/// Extracts all custom property names referenced in a value
pub fn extract_var_references(value: &str) -> Vec<String> {
  let mut refs = Vec::new();
  let mut input = ParserInput::new(value);
  let mut parser = Parser::new(&mut input);
  collect_var_references_from_parser(&mut parser, &mut refs);
  refs
}

fn collect_var_references_from_parser<'i, 't>(parser: &mut Parser<'i, 't>, refs: &mut Vec<String>) {
  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::Function(name) if name.eq_ignore_ascii_case("var") => {
        let _ = parser.parse_nested_block(|nested| {
          if let Ok((name, fallback)) = parse_var_function_arguments(nested) {
            refs.push(name);
            if let Some(fallback_value) = fallback {
              let mut input = ParserInput::new(&fallback_value);
              let mut nested_parser = Parser::new(&mut input);
              collect_var_references_from_parser(&mut nested_parser, refs);
            }
          }
          Ok::<_, ParseError<'i, ()>>(())
        });
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        let _ = parser.parse_nested_block(|nested| {
          collect_var_references_from_parser(nested, refs);
          Ok::<_, ParseError<'i, ()>>(())
        });
      }
      _ => {}
    }
  }
}

/// Validates that a custom property name follows CSS naming rules
pub fn is_valid_custom_property_name(name: &str) -> bool {
  if !name.starts_with("--") {
    return false;
  }

  if name.len() <= 2 {
    return false; // Just "--" is not valid
  }

  // The rest can be any character except whitespace
  // (CSS spec allows almost any character in custom property names)
  !name[2..].chars().any(is_css_whitespace_char)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Transform;
  use crate::style::color::Color;
  use crate::style::values::CustomPropertyValue;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;

  #[test]
  fn non_ascii_whitespace_var_resolution_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";

    assert_eq!(parse_simple_var_call(" var(--x) "), Some(("--x", None)));
    assert_eq!(parse_simple_var_call(&format!("{nbsp}var(--x)")), None);
    assert!(is_valid_custom_property_name(&format!("--a{nbsp}b")));

    let raw = format!("var(--x,{nbsp}fallback)");
    let expected_fallback = format!("{nbsp}fallback");
    let (_, fallback) = parse_simple_var_call(&raw).expect("expected simple var() call");
    assert_eq!(fallback, Some(expected_fallback.as_str()));
  }

  #[test]
  fn css_wide_keyword_guard_does_not_panic_on_utf8_boundaries() {
    // Regression test: avoid panics when slicing `&str` by byte-length inside
    // `starts_with_css_wide_keyword_with_trailing_tokens`.
    let value = format!("abcd\u{FFFD}zzz");
    assert!(!starts_with_css_wide_keyword_with_trailing_tokens(&value));
  }

  fn make_props(pairs: &[(&str, &str)]) -> CustomPropertyStore {
    let mut store = CustomPropertyStore::default();
    for (name, value) in pairs.iter().copied() {
      store.insert(name.into(), CustomPropertyValue::new(value, None));
    }
    store
  }

  #[test]
  fn toggle_function_resolves_based_on_parent_computed_value_for_background_color() {
    use crate::dom::DomNodeType;
    use crate::style::media::ColorScheme;
    use crate::style::ComputedStyle;

    let node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    };

    let mut parent = ComputedStyle::default();
    parent.background_color = Rgba::rgb(0, 128, 0);

    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("toggle(green, red)".to_string());

    let result = with_substitution_context(
      &node,
      &parent,
      Size::new(0.0, 0.0),
      ColorScheme::Light,
      || resolve_var_for_property(&value, &props, "background-color"),
    );

    let resolved = match result {
      VarResolutionResult::Resolved { value, .. } => value.into_owned(),
      other => panic!("expected resolved toggle(), got {other:?}"),
    };

    match resolved {
      PropertyValue::Color(Color::Rgba(rgba)) => assert_eq!(rgba, Rgba::RED),
      other => panic!("expected `red`, got {other:?}"),
    }
  }

  // Basic var() resolution tests
  #[test]
  fn test_resolve_simple_var() {
    let props = make_props(&[("--color", "#ff0000")]);
    let value = PropertyValue::Keyword("var(--color)".to_string());
    let resolved = resolve_var(&value, &props);

    // Should resolve to a color keyword when property context is missing
    matches!(resolved, PropertyValue::Keyword(ref kw) if kw == "#ff0000");
  }

  #[test]
  fn test_resolve_var_with_length() {
    let props = make_props(&[("--size", "16px")]);
    let value = PropertyValue::Keyword("var(--size)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Length(len) = resolved {
      assert_eq!(len.value, 16.0);
    } else {
      panic!("Expected Length, got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_var_not_found() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--missing)".to_string());
    let resolved = resolve_var(&value, &props);

    // Should return the original var() call
    if let PropertyValue::Keyword(kw) = resolved {
      assert!(kw.contains("var(--missing)"));
    } else {
      panic!("Expected Keyword, got {:?}", resolved);
    }
  }

  // Fallback value tests
  #[test]
  fn test_resolve_var_with_fallback_not_needed() {
    let props = make_props(&[("--color", "blue")]);
    let value = PropertyValue::Keyword("var(--color, red)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "blue");
    } else {
      panic!("Expected Keyword, got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_var_with_fallback_used() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--missing, red)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "red");
    } else {
      panic!("Expected Keyword 'red', got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_var_with_fallback_used_when_defined_custom_property_is_invalid() {
    // `--a` exists but is invalid at computed-value time because its value references a missing
    // variable with no fallback. var() should treat this like an invalid custom property and use
    // the fallback argument.
    let props = make_props(&[("--a", "var(--missing)")]);
    let value = PropertyValue::Keyword("var(--a, red)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "red");
    } else {
      panic!("Expected Keyword 'red', got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_var_with_fallback_length() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--spacing, 10px)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Length(len) = resolved {
      assert_eq!(len.value, 10.0);
    } else {
      panic!("Expected Length, got {:?}", resolved);
    }
  }

  // Nested var() tests
  #[test]
  fn test_resolve_nested_var_in_fallback() {
    let props = make_props(&[("--fallback-color", "green")]);
    let value = PropertyValue::Keyword("var(--color, var(--fallback-color))".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "green");
    } else {
      panic!("Expected Keyword 'green', got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_chained_vars() {
    let props = make_props(&[("--primary", "var(--base)"), ("--base", "#0000ff")]);
    let value = PropertyValue::Keyword("var(--primary)".to_string());
    let resolved = resolve_var(&value, &props);

    // Should resolve through the chain
    matches!(resolved, PropertyValue::Keyword(ref kw) if kw == "#0000ff");
  }

  // Embedded var() tests
  #[test]
  fn test_resolve_embedded_var_in_calc() {
    let props = make_props(&[("--size", "10px")]);
    let value = PropertyValue::Keyword("calc(var(--size) + 5px)".to_string());
    let resolved = resolve_var(&value, &props);

    assert!(
      matches!(resolved, PropertyValue::Length(len) if (len.value - 15.0).abs() < f32::EPSILON && len.unit == LengthUnit::Px),
      "Expected resolved calc length, got {:?}",
      resolved
    );
  }

  #[test]
  fn test_resolve_multiple_embedded_vars() {
    let props = make_props(&[("--x", "10px"), ("--y", "20px")]);
    let value = PropertyValue::Keyword("var(--x) var(--y)".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert!(kw.contains("10px"));
      assert!(kw.contains("20px"));
    } else {
      panic!("Expected Keyword, got {:?}", resolved);
    }
  }

  #[test]
  fn test_resolve_var_uses_property_specific_parser() {
    let props = make_props(&[("--bg", "url(image.png), linear-gradient(red, blue)")]);
    let value = PropertyValue::Keyword("var(--bg)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "background-image");

    if let VarResolutionResult::Resolved { value, .. } = resolved {
      let list = match value.into_owned() {
        PropertyValue::Multiple(list) => list,
        other => panic!("Expected Multiple for background layers, got {:?}", other),
      };
      assert_eq!(list.len(), 3); // url, comma token, gradient
      assert!(matches!(list[0], PropertyValue::Url(ref u) if u == "image.png"));
      assert!(matches!(
        list[2],
        PropertyValue::LinearGradient { .. } | PropertyValue::RepeatingLinearGradient { .. }
      ));
    } else {
      panic!(
        "Expected Multiple for background layers, got {:?}",
        resolved
      );
    }
  }

  #[test]
  fn first_valid_picks_first_parseable_candidate_and_is_lazy() {
    // The first candidate resolves successfully but is invalid for the property.
    // The second candidate is valid.
    // The third candidate would fail var() resolution entirely, but must not be evaluated.
    let props = make_props(&[("--invalid", "10px"), ("--valid", "rgb(0, 160, 0)")]);
    let value = PropertyValue::Keyword(
      "first-valid(var(--invalid), var(--valid), var(--missing))".to_string(),
    );

    let resolved = resolve_var_for_property(&value, &props, "background-color");
    let VarResolutionResult::Resolved { value, .. } = resolved else {
      panic!("expected first-valid() to resolve, got {resolved:?}");
    };

    let expected = Color::parse("rgb(0, 160, 0)").unwrap();
    assert!(
      matches!(value.as_ref(), PropertyValue::Color(color) if color == &expected),
      "expected first-valid() to pick the green candidate, got {:?}",
      value.as_ref()
    );
  }

  #[test]
  fn unresolved_var_marks_declaration_invalid() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--missing)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "color");
    assert!(matches!(resolved, VarResolutionResult::NotFound(_)));
  }

  #[test]
  fn unresolved_fallback_var_marks_declaration_invalid() {
    let props = make_props(&[("--fallback", "var(--still-missing)")]);
    let value = PropertyValue::Keyword("var(--missing, var(--fallback))".to_string());
    let resolved = resolve_var_for_property(&value, &props, "color");
    assert!(matches!(resolved, VarResolutionResult::NotFound(_)));
  }

  #[test]
  fn unterminated_var_function_is_invalid_syntax() {
    // Real-world pages sometimes ship malformed CSS like `fill: var(--x;` (missing the closing `)`).
    // Browsers treat this as invalid syntax and ignore the declaration; var() substitution must not
    // "repair" it just because the referenced custom property exists.
    let props = make_props(&[("--x", "red")]);
    let value = PropertyValue::Keyword("fill: var(--x;".to_string());
    let resolved = resolve_var_for_property(&value, &props, "");
    assert!(
      matches!(resolved, VarResolutionResult::InvalidSyntax(_)),
      "expected unterminated var() to be invalid syntax, got {resolved:?}"
    );
  }

  #[test]
  fn fallback_used_when_resolved_custom_property_starts_with_css_wide_keyword_sentinel() {
    let props = make_props(&[("--x", "initial #000")]);
    let value = PropertyValue::Keyword("var(--x, blue)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "color");
    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected fallback resolution to succeed, got {resolved:?}");
    };
    let expected = Color::parse("blue").unwrap();
    assert_eq!(css_text.as_ref(), "blue");
    assert!(
      matches!(value.as_ref(), PropertyValue::Color(c) if c == &expected),
      "expected parsed color 'blue', got {:?}",
      value.as_ref()
    );
  }

  #[test]
  fn fallback_used_when_resolving_existing_custom_property_fails_due_to_missing_dependency() {
    let props = make_props(&[("--a", "var(--b)")]);
    let value = PropertyValue::Keyword("var(--a, red)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "color");
    let VarResolutionResult::Resolved { css_text, .. } = resolved else {
      panic!("expected fallback resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.as_ref(), "red");
  }

  #[test]
  fn fallback_used_when_resolving_existing_custom_property_hits_cycle() {
    let props = make_props(&[("--a", "var(--a)")]);
    let value = PropertyValue::Keyword("var(--a, red)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "color");
    let VarResolutionResult::Resolved { css_text, .. } = resolved else {
      panic!("expected fallback resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.as_ref(), "red");
  }

  #[test]
  fn resolved_width_parses_length_after_var_resolution() {
    let props = make_props(&[("--x", "10px")]);
    let value = PropertyValue::Keyword("var(--x)".to_string());
    let resolved = resolve_var_for_property(&value, &props, "width");

    let VarResolutionResult::Resolved { value, .. } = resolved else {
      panic!("expected successful var() resolution, got {resolved:?}");
    };

    match value.as_ref() {
      PropertyValue::Length(len) => {
        assert!((len.value - 10.0).abs() < f32::EPSILON);
        assert_eq!(len.unit, LengthUnit::Px);
      }
      other => panic!("expected Length(10px), got {other:?}"),
    }
  }

  #[test]
  fn resolves_var_in_transform_calc_product_percentages() {
    let props = make_props(&[("--direction-multiplier", "1")]);
    let value =
      PropertyValue::Keyword("translateX(calc(var(--direction-multiplier,1) * -100%))".to_string());
    let resolved = resolve_var_for_property(&value, &props, "transform");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected successful var() resolution, got {resolved:?}");
    };
    assert!(
      !css_text.as_ref().is_empty(),
      "expected resolved css_text to be populated for a var() call"
    );
    assert!(
      !css_text.as_ref().contains("var("),
      "expected resolved css_text to contain no var(), got {:?}",
      css_text.as_ref()
    );

    match value.as_ref() {
      PropertyValue::Transform(transforms) => {
        assert_eq!(transforms.len(), 1);
        assert!(
          matches!(&transforms[0], Transform::TranslateX(x) if *x == Length::percent(-100.0)),
          "expected translateX(-100%), got {transforms:?}"
        );
      }
      other => panic!(
        "expected parsed transform value, got {other:?} (css_text={:?})",
        css_text.as_ref()
      ),
    }
  }

  #[test]
  fn parse_value_after_resolution_rejects_unresolved_var_function() {
    assert!(
      parse_value_after_resolution("var(--x)", "width").is_none(),
      "unresolved var() should invalidate the resolved value"
    );
  }

  #[test]
  fn parse_value_after_resolution_detects_escaped_var_function_name() {
    assert!(
      parse_value_after_resolution("v\\61 r(--x)", "width").is_none(),
      "escaped var() should invalidate the resolved value"
    );
  }

  // Recursion limit tests
  #[test]
  fn resolves_deep_non_cyclic_custom_property_chain() {
    // Regression test: frameworks like Tailwind can generate long `--a: var(--b)` chains. We must
    // resolve chains deeper than the historical recursion cap (10) as long as they are acyclic.
    const CHAIN_LEN: usize = 40;
    assert!(
      MAX_RECURSION_DEPTH >= CHAIN_LEN,
      "test assumes MAX_RECURSION_DEPTH >= {CHAIN_LEN}, got {MAX_RECURSION_DEPTH}"
    );
    let mut store = CustomPropertyStore::default();
    for i in 1..CHAIN_LEN {
      let name = format!("--a{i}");
      let value = format!("var(--a{})", i + 1);
      store.insert(name.into(), CustomPropertyValue::new(value, None));
    }
    store.insert(
      format!("--a{CHAIN_LEN}").into(),
      CustomPropertyValue::new("10px", None),
    );

    let value = PropertyValue::Keyword("var(--a1)".to_string());
    let resolved = resolve_var_for_property(&value, &store, "width");

    let VarResolutionResult::Resolved { value, .. } = resolved else {
      panic!("expected deep var() chain to resolve, got {resolved:?}");
    };

    match value.as_ref() {
      PropertyValue::Length(len) => {
        assert!((len.value - 10.0).abs() < f32::EPSILON);
        assert_eq!(len.unit, LengthUnit::Px);
      }
      other => panic!("expected Length(10px), got {other:?}"),
    }
  }

  #[test]
  fn test_recursion_limit() {
    // Create a circular reference
    let props = make_props(&[
      ("--a", "var(--b)"),
      ("--b", "var(--c)"),
      ("--c", "var(--a)"), // Circular!
    ]);
    let value = PropertyValue::Keyword("var(--a)".to_string());

    // Should not stack overflow - recursion limit should kick in
    let _resolved = resolve_var(&value, &props);
    // If we get here without panicking, the test passes
  }

  // Utility function tests
  #[test]
  fn test_contains_var() {
    assert!(contains_var("var(--x)"));
    assert!(contains_var("calc(var(--x) + 1px)"));
    assert!(contains_var("var(--color)"));
    assert!(contains_var("calc(var(--size) + 10px)"));
    assert!(contains_var("0 0 var(--blur) black"));
    assert!(contains_var("v\\61 r(--x)"));
    assert!(
      contains_var("url(var(--x))"),
      "var() inside url() should be detected"
    );
    assert!(!contains_var("10px"));
    assert!(!contains_var("red"));
    assert!(!contains_var("color: red"));
    assert!(!contains_var(""));
  }

  #[test]
  fn contains_var_ignores_strings_and_comments() {
    assert!(
      !contains_var("\"var(--x)\""),
      "var() inside quoted strings is not a var() token"
    );
    assert!(
      !contains_var("/* var(--x) */"),
      "var() inside comments is not a var() token"
    );
    assert!(
      !contains_var("url(\"var(--x)\")"),
      "var() inside url()'s quoted string is not a var() token"
    );
  }

  #[test]
  fn test_extract_var_references() {
    let refs = extract_var_references("var(--color)");
    assert_eq!(refs, vec!["--color"]);

    let refs = extract_var_references("calc(var(--size) + var(--margin))");
    assert_eq!(refs, vec!["--size", "--margin"]);

    let refs = extract_var_references("var(--x, var(--y))");
    assert_eq!(refs, vec!["--x", "--y"]);

    let refs = extract_var_references("10px");
    assert!(refs.is_empty());
  }

  #[test]
  fn test_is_valid_custom_property_name() {
    assert!(is_valid_custom_property_name("--color"));
    assert!(is_valid_custom_property_name("--color-primary"));
    assert!(is_valid_custom_property_name("--_internal"));
    assert!(is_valid_custom_property_name("--123"));
    assert!(is_valid_custom_property_name("--myVar"));

    assert!(!is_valid_custom_property_name("color"));
    assert!(!is_valid_custom_property_name("-color"));
    assert!(!is_valid_custom_property_name("--"));
    assert!(!is_valid_custom_property_name("--has space"));
  }

  // Edge cases
  #[test]
  fn test_empty_var() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var()".to_string());
    let resolved = resolve_var(&value, &props);

    // Should return the original malformed var()
    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "var()");
    }
  }

  #[test]
  fn test_var_with_whitespace() {
    let props = make_props(&[("--color", "blue")]);
    let value = PropertyValue::Keyword("var(  --color  )".to_string());
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Keyword(kw) = resolved {
      assert_eq!(kw, "blue");
    } else {
      panic!("Expected Keyword 'blue', got {:?}", resolved);
    }
  }

  #[test]
  fn test_non_var_value_unchanged() {
    let props = make_props(&[("--color", "blue")]);
    let value = PropertyValue::Length(Length::px(10.0));
    let resolved = resolve_var(&value, &props);

    if let PropertyValue::Length(len) = resolved {
      assert_eq!(len.value, 10.0);
    } else {
      panic!("Expected Length, got {:?}", resolved);
    }
  }

  #[test]
  fn test_keyword_without_var_skips_tokenization() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("block".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "display");

    match resolved {
      VarResolutionResult::Resolved { value, css_text } => {
        assert!(css_text.is_empty());
        assert!(
          matches!(value, ResolvedPropertyValue::Borrowed(_)),
          "expected var-free resolution to borrow the original PropertyValue"
        );
        assert!(matches!(
          value.as_ref(),
          PropertyValue::Keyword(ref kw) if kw == "block"
        ));
      }
      other => panic!("Expected Resolved, got {:?}", other),
    }

    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn test_simple_var_call_skips_tokenization() {
    let props = make_props(&[("--x", "10px")]);
    let value = PropertyValue::Keyword("var(--x)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "width");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected var() resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.as_ref(), "10px");
    assert!(matches!(
      value.as_ref(),
      PropertyValue::Length(len) if (len.value - 10.0).abs() < f32::EPSILON && len.unit == LengthUnit::Px
    ));

    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn test_simple_var_call_with_fallback_skips_tokenization() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--missing, 10px)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "width");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected fallback var() resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.trim(), "10px");
    assert!(matches!(
      value.as_ref(),
      PropertyValue::Length(len) if (len.value - 10.0).abs() < f32::EPSILON && len.unit == LengthUnit::Px
    ));

    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn parse_simple_var_call_empty_fallback_is_preserved() {
    let (name, fallback) = parse_simple_var_call("var(--x,)").expect("expected simple var call");
    assert_eq!(name, "--x");
    assert_eq!(fallback, Some(""));

    let (_, fallback) = parse_simple_var_call("var(--x,   )").expect("expected simple var call");
    assert_eq!(fallback, Some(""));

    let (_, fallback) = parse_simple_var_call("var(--x)").expect("expected simple var call");
    assert_eq!(fallback, None);
  }

  #[test]
  fn test_simple_var_call_with_empty_fallback_resolves_to_empty() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--missing,)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "");

    let VarResolutionResult::Resolved { css_text, .. } = resolved else {
      panic!("expected empty-fallback var() resolution to succeed, got {resolved:?}");
    };

    assert_eq!(css_text.as_ref(), "");
    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn test_simple_var_call_with_comment_in_name_falls_back_to_tokenizer() {
    let props = make_props(&[("--x", "10px")]);
    let value = PropertyValue::Keyword("var(--x/*comment*/)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "width");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected var() resolution to succeed, got {resolved:?}");
    };

    assert_eq!(css_text.trim(), "10px");
    assert!(matches!(
      value.as_ref(),
      PropertyValue::Length(len)
        if (len.value - 10.0).abs() < f32::EPSILON && len.unit == LengthUnit::Px
    ));
    assert!(
      TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()) > 0,
      "comment-containing var() call should fall back to tokenization"
    );
  }

  #[test]
  fn test_simple_var_call_with_invalid_custom_property_name_does_not_apply_fallback() {
    let props = CustomPropertyStore::default();
    let value = PropertyValue::Keyword("var(--bad!, 10px)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "width");

    assert!(
      matches!(resolved, VarResolutionResult::InvalidSyntax(_)),
      "invalid var() syntax should not apply the fallback, got {resolved:?}"
    );
    assert!(
      TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()) > 0,
      "invalid var() name should fall back to tokenization"
    );
  }

  #[test]
  fn token_splice_preserves_boundaries_between_adjacent_vars() {
    let props = make_props(&[("--x", "0"), ("--y", "calc(1px)")]);
    let value = PropertyValue::Keyword("var(--x)var(--y)".to_string());

    let VarResolutionResult::Resolved { css_text, .. } =
      resolve_var_for_property(&value, &props, "")
    else {
      panic!("expected var() resolution to succeed");
    };

    let mut input = ParserInput::new(css_text.as_ref());
    let mut parser = Parser::new(&mut input);

    let mut seen_number = false;
    let mut seen_calc = false;
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
      match token {
        Token::WhiteSpace(_) | Token::Comment(_) => continue,
        Token::Number { int_value, .. } if !seen_number => {
          assert_eq!(int_value, &Some(0));
          seen_number = true;
        }
        Token::Function(name) if seen_number && !seen_calc => {
          assert!(
            name.eq_ignore_ascii_case("calc"),
            "expected `calc()` function token after number, got {name:?}"
          );
          seen_calc = true;
          break;
        }
        other => panic!("unexpected token after var() splice: {other:?}"),
      }
    }

    assert!(
      seen_number,
      "expected a number token at start of spliced result"
    );
    assert!(seen_calc, "expected a calc() function token after number");
  }

  #[test]
  fn test_multi_var_calls_skip_tokenization() {
    let props = make_props(&[("--x", "10px"), ("--y", "20px")]);
    let value = PropertyValue::Keyword("translate(var(--x), var(--y))".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected multi-var() resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.as_ref(), "translate(10px, 20px)");
    assert!(matches!(
      value.as_ref(),
      PropertyValue::Keyword(ref kw) if kw == "translate(10px, 20px)"
    ));

    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn test_chained_simple_var_calls_skip_tokenization() {
    let props = make_props(&[("--a", "var(--b)"), ("--b", "10px")]);
    let value = PropertyValue::Keyword("var(--a)".to_string());

    TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.set(0));
    let resolved = resolve_var_for_property(&value, &props, "width");

    let VarResolutionResult::Resolved { value, css_text } = resolved else {
      panic!("expected chained var() resolution to succeed, got {resolved:?}");
    };
    assert_eq!(css_text.as_ref(), "10px");
    assert!(matches!(
      value.as_ref(),
      PropertyValue::Length(len)
        if (len.value - 10.0).abs() < f32::EPSILON && len.unit == LengthUnit::Px
    ));

    assert_eq!(TOKEN_RESOLVER_ENTRY_COUNT.with(|count| count.get()), 0);
  }

  #[test]
  fn test_resolve_var_result_methods() {
    let resolved: VarResolutionResult<'static> = VarResolutionResult::Resolved {
      value: ResolvedPropertyValue::Owned(PropertyValue::Keyword("blue".to_string())),
      css_text: Cow::Borrowed("blue"),
    };
    assert!(resolved.is_resolved());

    let default = PropertyValue::Keyword("red".to_string());
    let result = VarResolutionResult::NotFound("--missing".to_string());
    let value = result.unwrap_or(default.clone());
    if let PropertyValue::Keyword(kw) = value {
      assert_eq!(kw, "red");
    }
  }
}
