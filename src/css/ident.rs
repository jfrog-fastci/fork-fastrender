use cssparser::{ParseError, Parser, Token};

/// Parse the contents of an `ident()` function into its computed identifier value.
///
/// CSS Values 5 defines `ident()` as a way to construct `<<custom-ident>>` values from a sequence
/// of string/integer/ident tokens. FastRender currently implements the subset needed by container
/// queries: static identifiers without variable substitution.
pub(crate) fn parse_ident_function_contents<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<String, ParseError<'i, ()>> {
  let mut out = String::new();
  let mut saw_arg = false;

  while let Ok(token) = parser.next_including_whitespace_and_comments() {
    match token {
      Token::WhiteSpace(_) | Token::Comment(_) => continue,
      Token::Ident(ident) => {
        saw_arg = true;
        out.push_str(ident.as_ref());
      }
      Token::QuotedString(text) => {
        saw_arg = true;
        out.push_str(text.as_ref());
      }
      Token::Number {
        int_value: Some(value),
        ..
      } => {
        saw_arg = true;
        out.push_str(&value.to_string());
      }
      _ => return Err(parser.new_custom_error(())),
    }
  }

  if !saw_arg {
    return Err(parser.new_custom_error(()));
  }

  Ok(out)
}

