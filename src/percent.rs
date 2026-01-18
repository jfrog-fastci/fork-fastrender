use std::borrow::Cow;
use std::str::Utf8Error;

/// Percent-decode `input` using the tolerant semantics of the `percent-encoding` crate.
///
/// - Decodes `%HH` sequences where `H` are ASCII hex digits.
/// - Leaves malformed `%` sequences unchanged (e.g. `%`, `%G1`, `%1`).
/// - Does **not** treat `+` specially.
pub(crate) fn percent_decode_str(input: &str) -> PercentDecode<'_> {
  PercentDecode { input }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PercentDecode<'a> {
  input: &'a str,
}

impl<'a> PercentDecode<'a> {
  pub(crate) fn decode_utf8(self) -> Result<Cow<'a, str>, Utf8Error> {
    let bytes = percent_decode_bytes(self.input);
    match bytes {
      Cow::Borrowed(_) => Ok(Cow::Borrowed(self.input)),
      Cow::Owned(bytes) => match String::from_utf8(bytes) {
        Ok(decoded) => Ok(Cow::Owned(decoded)),
        Err(err) => Err(err.utf8_error()),
      },
    }
  }

  pub(crate) fn decode_utf8_lossy(self) -> Cow<'a, str> {
    let bytes = percent_decode_bytes(self.input);
    match bytes {
      Cow::Borrowed(_) => Cow::Borrowed(self.input),
      Cow::Owned(bytes) => match String::from_utf8(bytes) {
        Ok(decoded) => Cow::Owned(decoded),
        Err(err) => {
          let bytes = err.into_bytes();
          Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
        }
      },
    }
  }
}

fn percent_decode_bytes(input: &str) -> Cow<'_, [u8]> {
  let bytes = input.as_bytes();
  let mut i = 0usize;

  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
        let mut out = Vec::with_capacity(bytes.len());
        out.extend_from_slice(&bytes[..i]);
        out.push((hi << 4) | lo);
        i += 3;

        while i < bytes.len() {
          if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
              out.push((hi << 4) | lo);
              i += 3;
              continue;
            }
          }

          out.push(bytes[i]);
          i += 1;
        }

        return Cow::Owned(out);
      }
    }

    i += 1;
  }

  Cow::Borrowed(bytes)
}

fn hex_value(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn percent_decode_does_not_treat_plus_as_space() {
    assert_eq!(
      percent_decode_str("a+b%20c").decode_utf8_lossy(),
      "a+b c"
    );
  }

  #[test]
  fn percent_decode_tolerates_invalid_percent_then_decodes_valid_escape() {
    assert_eq!(
      percent_decode_str("a%%20").decode_utf8_lossy(),
      "a% "
    );
    assert!(percent_decode_str("a%%FF").decode_utf8().is_err());
  }

  #[test]
  fn decode_utf8_is_borrowed_when_input_is_unchanged() {
    let decoded = percent_decode_str("hello%2").decode_utf8().expect("decode");
    assert!(matches!(decoded, Cow::Borrowed(_)));
    assert_eq!(decoded, "hello%2");
  }
}

