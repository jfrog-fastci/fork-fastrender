use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

fn opts() -> ParseOptions {
  ParseOptions {
    dialect: Dialect::Js,
    source_type: SourceType::Script,
  }
}

#[test]
fn let_newline_await_in_async_function_is_syntax_error() {
  // `let` is a contextual keyword: it can start a lexical declaration or be an identifier.
  //
  // ECMAScript disambiguation uses a token lookahead that forbids ASI splitting after `let` when the
  // next token could continue a lexical declaration. When that next token is `await` inside an
  // async function body, the input must be parsed as a (then-invalid) `let <binding>` rather than
  // as `let; await 0;`.
  let src = "async function f() { let\\nawait 0; }";
  assert!(parse_with_options(src, opts()).is_err());
}

#[test]
fn let_newline_yield_in_generator_function_is_syntax_error() {
  // Same disambiguation rule as above, but for `yield` in generator bodies.
  let src = "function* g() { let\\nyield 0; }";
  assert!(parse_with_options(src, opts()).is_err());
}

