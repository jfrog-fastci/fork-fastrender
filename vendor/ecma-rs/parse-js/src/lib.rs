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

  #[test]
  fn cover_grammar_object_pattern_shorthand_flags_are_preserved() {
    use crate::ast::class_or_object::ClassOrObjKey;
    use crate::ast::expr::pat::Pat;
    use crate::ast::expr::Expr;
    use crate::ast::stmt::Stmt;
    use crate::operator::OperatorName;

    fn assert_first_prop(
      src: &str,
      expected_shorthand: bool,
      expected_target: &str,
      expect_default: bool,
    ) {
      let top = parse_with_options(src, ecma_script_opts()).unwrap();
      let stmt = top.stx.body.first().expect("expected one statement");
      let Stmt::Expr(expr_stmt) = &*stmt.stx else {
        panic!("expected expression statement");
      };
      let Expr::Binary(bin) = expr_stmt.stx.expr.stx.as_ref() else {
        panic!("expected binary expression");
      };
      assert_eq!(bin.stx.operator, OperatorName::Assignment);
      let Expr::ObjPat(obj_pat) = bin.stx.left.stx.as_ref() else {
        panic!("expected object pattern on LHS");
      };
      let prop = obj_pat
        .stx
        .properties
        .first()
        .expect("expected at least one property");
      assert_eq!(
        prop.stx.shorthand, expected_shorthand,
        "unexpected shorthand flag for `{src}`"
      );
      let ClassOrObjKey::Direct(key) = &prop.stx.key else {
        panic!("expected direct key");
      };
      assert_eq!(key.stx.key, "a");
      match prop.stx.target.stx.as_ref() {
        Pat::Id(id) => assert_eq!(id.stx.name, expected_target),
        other => panic!("expected identifier pattern, got {other:?}"),
      }
      assert_eq!(
        prop.stx.default_value.is_some(),
        expect_default,
        "unexpected default_value presence for `{src}`"
      );
    }

    // `{ a: b }` must remain a non-shorthand property in the resulting object pattern.
    assert_first_prop("({ a: b } = obj);", false, "b", false);

    // `{ a = 1 }` is syntactic shorthand-with-default (CoverInitializedName) and must keep
    // `shorthand=true` so emit/minify passes can reproduce it.
    assert_first_prop("({ a = 1 } = obj);", true, "a", true);

    // `{ a: b = 1 }` is a `key: value` property with an assignment expression on the RHS; it is
    // *not* syntactic shorthand and must preserve `shorthand=false`.
    assert_first_prop("({ a: b = 1 } = obj);", false, "b", true);
  }

  #[test]
  fn class_initialization_code_disallows_arguments_identifier() {
    let opts = ecma_script_opts();

    let expected = SyntaxErrorType::ArgumentsNotAllowedInClassInit;

    // `arguments` is an early error in class field initializers.
    let err = parse_with_options("class C { x = arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    // The restriction applies equally to static fields and private fields.
    let err = parse_with_options("class C { static x = arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    let err = parse_with_options("class C { #x = arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    let err = parse_with_options("class C { static #x = arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    // `arguments` is also an early error in `static {}` blocks.
    let err = parse_with_options("class C { static { arguments; } }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    // Arrow functions do not introduce an `arguments` binding, so references remain disallowed.
    let err = parse_with_options("class C { x = () => arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    let err = parse_with_options("class C { static x = () => arguments; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    // Arrow parameter initializers similarly have no `arguments` binding.
    let err = parse_with_options("class C { x = (y = arguments) => y; }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    let err = parse_with_options(
      "class C { static { ((x = arguments) => x); } }",
      opts,
    )
    .unwrap_err();
    assert_eq!(err.typ, expected);

    // Non-arrow functions (and arrow functions nested within them) may reference their own
    // `arguments` binding.
    parse_with_options("class C { x = function () { return arguments; }; }", opts).unwrap();
    parse_with_options(
      "class C { x = function(){ return arguments[0]; }; }",
      opts,
    )
    .unwrap();
    parse_with_options(
      "class C { x = function () { return () => arguments; }; }",
      opts,
    )
    .unwrap();
    parse_with_options(
      "class C { x = function(){ return (() => arguments[0])(); }; }",
      opts,
    )
    .unwrap();
    parse_with_options("class C { x = function (a = arguments) {}; }", opts).unwrap();

    // Even when a class is nested in a function with an `arguments` binding, class initialization
    // code must not inherit it.
    let err = parse_with_options("function f() { class C { x = arguments; } }", opts).unwrap_err();
    assert_eq!(err.typ, expected);

    // Similarly, class initialization code must not inherit `arguments` from functions nested inside
    // other class initialization code.
    let err = parse_with_options(
      "class Outer { x = function () { class Inner { y = arguments; } }; }",
      opts,
    )
    .unwrap_err();
    assert_eq!(err.typ, expected);

    let err = parse_with_options(
      "class Outer { static { function f() { class Inner { y = arguments; } } } }",
      opts,
    )
    .unwrap_err();
    assert_eq!(err.typ, expected);
  }

  #[test]
  fn arguments_allowed_in_nested_function_in_static_block() {
    let opts = ecma_script_opts();
    parse_with_options(
      "class C { static { function f(){ return arguments[0]; } } }",
      opts,
    )
    .unwrap();
  }
}
