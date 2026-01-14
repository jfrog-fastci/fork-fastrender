use crate::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn for_of_rhs_requires_assignment_expression() {
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };

  // Top-level comma/SequenceExpression is not permitted as the `of` RHS.
  assert!(parse_with_options("for (x of [], []) {}", opts).is_err());
  assert!(parse_with_options("for (var x of [], []) {}", opts).is_err());
  assert!(parse_with_options("for (let x of [], []) {}", opts).is_err());
  assert!(parse_with_options("for (const x of [], []) {}", opts).is_err());

  // Parenthesized comma expressions are valid.
  assert!(parse_with_options("for (x of ([], [])) {}", opts).is_ok());
}
