use crate::style::values::substitute_calc_size_expr;

#[test]
fn calc_size_token_substitution_never_panics_on_gnarly_token_streams() {
  let cases: &[(&str, f32)] = &[
    // Basic ident replacement (case-insensitive).
    ("size", 42.0),
    ("SiZe", 42.0),
    // Delimiters + whitespace + comments.
    ("size/*comment*/ + 10px - 2%", 42.0),
    // Functions (Token::Function) + nested blocks.
    ("calc(size + 1px)", 42.0),
    ("min(size, calc(10px + (size)))", 42.0),
    ("foo(bar(baz(size)))", 42.0),
    // Parenthesis / square / curly blocks.
    ("(size + 1px)", 42.0),
    ("[size + 1px]", 42.0),
    ("{size + 1px}", 42.0),
    // Mixed nesting of all block types.
    ("calc([size + (1px)] + {size - 2px})", 42.0),
    // Other token kinds in the `other =>` branch: strings, hashes, and urls.
    ("size + \"string\" + #abc + url(data:text/plain,size)", 42.0),
    // Unbalanced blocks should error and return None, but never panic.
    ("calc(size + 10px", 42.0),
    ("[size", 42.0),
    ("{size", 42.0),
    // Non-finite size is rejected early.
    ("size + 1px", f32::NAN),
  ];

  for &(expr, size_px) in cases {
    let result = std::panic::catch_unwind(|| substitute_calc_size_expr(expr, size_px));
    assert!(
      result.is_ok(),
      "substitute_calc_size_expr panicked for `{expr}` (size_px={size_px:?})"
    );
  }
}
