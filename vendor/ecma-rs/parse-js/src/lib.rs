//! JavaScript/TypeScript parser for the ecma-rs toolchain.
//!
//! All source text is treated as UTF-8; spans and diagnostics use UTF-8 byte
//! offsets.
//!
//! # Runnable example
//!
//! ```bash
//! bash scripts/cargo_agent.sh run -p parse-js --example basic
//! ```

use ast::node::Node;
use ast::stx::TopLevel;
use error::{SyntaxError, SyntaxErrorType, SyntaxResult};
use lex::Lexer;
use parse::Parser;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
  Js,
  Jsx,
  Ts,
  Tsx,
  Dts,
  /// Strict ECMAScript parsing with recovery disabled.
  ///
  /// This mode is intended for conformance suites (e.g. test262) where the
  /// parser must reject syntax errors instead of attempting TypeScript-style
  /// recovery.
  Ecma,
}

impl Dialect {
  pub fn allows_jsx(self) -> bool {
    matches!(self, Dialect::Jsx | Dialect::Tsx)
  }

  pub fn is_typescript(self) -> bool {
    matches!(self, Dialect::Ts | Dialect::Tsx | Dialect::Dts)
  }

  pub fn allows_angle_bracket_type_assertions(self) -> bool {
    matches!(self, Dialect::Ts | Dialect::Dts)
  }

  pub fn is_strict_ecmascript(self) -> bool {
    matches!(self, Dialect::Ecma)
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceType {
  Script,
  Module,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseOptions {
  pub dialect: Dialect,
  pub source_type: SourceType,
}

impl Default for ParseOptions {
  fn default() -> Self {
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    }
  }
}

pub mod ast;
pub mod char;
pub mod error;
pub mod lex;
pub mod loc;
pub mod num;
pub mod operator;
pub mod parse;
pub mod token;
pub mod utf16;
pub mod util;

#[derive(Debug)]
struct ParseCancelled;

/// Parse JavaScript or TypeScript source as UTF-8 and return the top-level AST.
///
/// Spans and diagnostics are expressed in UTF-8 byte offsets. Callers starting
/// from raw bytes should validate/convert to `&str` at the I/O boundary before
/// invoking the parser.
pub fn parse(source: &str) -> SyntaxResult<Node<TopLevel>> {
  parse_with_options(source, ParseOptions::default())
}

/// Parse JavaScript or TypeScript source with explicit [`ParseOptions`].
///
/// The source **must** be valid UTF-8; span math assumes byte offsets into the
/// original string. See [`parse`] for the recommended boundary validation
/// strategy when starting from raw bytes.
pub fn parse_with_options(source: &str, opts: ParseOptions) -> SyntaxResult<Node<TopLevel>> {
  let lexer = Lexer::new(source);
  let mut parser = Parser::new(lexer, opts);
  parser.parse_top_level()
}

/// Parse JavaScript or TypeScript source with explicit [`ParseOptions`], allowing
/// cooperative cancellation via `cancel`.
///
/// This is primarily intended for long-running conformance suites where a
/// misbehaving input could otherwise stall the whole runner. Cancellation is
/// best-effort: the parser checks `cancel` during tokenization/parsing and
/// returns [`SyntaxErrorType::Cancelled`] once observed.
pub fn parse_with_options_cancellable(
  source: &str,
  opts: ParseOptions,
  cancel: Arc<AtomicBool>,
) -> SyntaxResult<Node<TopLevel>> {
  let lexer = Lexer::new(source);
  let mut parser = Parser::new_cancellable(lexer, opts, Some(cancel));

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.parse_top_level()));
  match result {
    Ok(result) => result,
    Err(payload) => {
      if payload.is::<ParseCancelled>() {
        return Err(SyntaxError::new(
          SyntaxErrorType::Cancelled,
          loc::Loc(source.len(), source.len()),
          None,
        ));
      }
      std::panic::resume_unwind(payload)
    }
  }
}

/// Parse JavaScript or TypeScript source with explicit [`ParseOptions`], allowing
/// cooperative cancellation via `cancel_check`.
///
/// This is a callback-based variant of [`parse_with_options_cancellable`] that does not require
/// threads/atomics. Cancellation is best-effort: `cancel_check` is invoked periodically during
/// tokenization/parsing and the parser returns [`SyntaxErrorType::Cancelled`] once observed.
pub fn parse_with_options_cancellable_by<'a>(
  source: &'a str,
  opts: ParseOptions,
  cancel_check: impl FnMut() -> bool + 'a,
) -> SyntaxResult<Node<TopLevel>> {
  parse_with_options_cancellable_by_with_init(source, opts, cancel_check, |_| {})
}

/// Parse JavaScript or TypeScript source with explicit [`ParseOptions`], allowing cooperative
/// cancellation via `cancel_check` and an optional parser initialization hook.
///
/// This is a variant of [`parse_with_options_cancellable_by`] intended for embedders that need to
/// mutate the parser's initial state before parsing begins (for example, when parsing source
/// snippets extracted from a larger program).
pub fn parse_with_options_cancellable_by_with_init<'a>(
  source: &'a str,
  opts: ParseOptions,
  cancel_check: impl FnMut() -> bool + 'a,
  init: impl FnOnce(&mut Parser<'a>),
) -> SyntaxResult<Node<TopLevel>> {
  let lexer = Lexer::new(source);
  let mut parser = Parser::new_cancellable_by(lexer, opts, Box::new(cancel_check));
  init(&mut parser);

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.parse_top_level()));
  match result {
    Ok(result) => result,
    Err(payload) => {
      if payload.is::<ParseCancelled>() {
        return Err(SyntaxError::new(
          SyntaxErrorType::Cancelled,
          loc::Loc(source.len(), source.len()),
          None,
        ));
      }
      std::panic::resume_unwind(payload)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::SyntaxErrorType;

  fn ecma_script_opts() -> ParseOptions {
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    }
  }

  #[test]
  fn meta_properties_in_arrow_functions_require_enclosing_context() {
    let opts = ecma_script_opts();

    // `new.target` is not allowed in arrow functions unless an enclosing function provides the
    // binding.
    let err = parse_with_options("(() => new.target)", opts).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("new.target expression not allowed outside functions")
    );

    // Seed the initial grammar context as if the snippet were enclosed by a function/class element.
    parse_with_options_cancellable_by_with_init(
      "(() => new.target)",
      opts,
      || false,
      |p| p.set_initial_meta_property_context(true, true, true),
    )
    .unwrap();

    // `super` is similarly only allowed when inherited from an enclosing method/constructor.
    let err = parse_with_options("(() => super.foo)", opts).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax(
        "super property access not allowed outside methods and class elements"
      )
    );

    parse_with_options_cancellable_by_with_init(
      "(() => super.foo)",
      opts,
      || false,
      |p| p.set_initial_meta_property_context(true, true, true),
    )
    .unwrap();

    let err = parse_with_options("(() => super())", opts).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("super call not allowed outside derived constructors")
    );

    parse_with_options_cancellable_by_with_init(
      "(() => super())",
      opts,
      || false,
      |p| p.set_initial_meta_property_context(true, true, true),
    )
    .unwrap();
  }

  #[test]
  fn unicode_mode_disallows_unescaped_right_bracket_outside_charset() {
    let opts = ecma_script_opts();

    // In Unicode mode (`u`/`v`), `]` is a SyntaxCharacter and cannot appear as an unescaped
    // PatternCharacter outside of a character class.
    let err = parse_with_options("let re = /]/u;", opts).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression")
    );
    let err = parse_with_options("let re = /]/v;", opts).unwrap_err();
    assert_eq!(
      err.typ,
      SyntaxErrorType::ExpectedSyntax("valid regular expression")
    );

    // In non-Unicode mode, Annex B permits treating `]` as a literal.
    parse_with_options("let re = /]/;", opts).unwrap();

    // Escaped `]` remains valid in Unicode mode.
    parse_with_options(r"let re = /\]/u;", opts).unwrap();
    parse_with_options(r"let re = /\]/v;", opts).unwrap();
  }

  #[test]
  fn strict_mode_disallows_reserved_words_in_object_shorthand_properties() {
    let opts = ecma_script_opts();

    // Strict mode reserved words (e.g. `yield`, `let`) are not valid `IdentifierReference`s in
    // object literal shorthand properties.
    for word in [
      "yield",
      "let",
      "static",
      "implements",
      "interface",
      "package",
      "private",
      "protected",
      "public",
    ] {
      let src = format!("'use strict'; ({{ {word} }})");
      let err = parse_with_options(&src, opts).unwrap_err();
      assert_eq!(
        err.typ,
        SyntaxErrorType::ExpectedSyntax("identifier"),
        "expected `{word}` to be rejected as a strict mode reserved word"
      );
    }

    // `eval` and `arguments` are restricted in binding positions, but remain valid identifier
    // references (and therefore valid shorthand property names).
    parse_with_options("'use strict'; ({ eval, arguments })", opts).unwrap();
  }
}
