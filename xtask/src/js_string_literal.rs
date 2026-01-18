use anyhow::{bail, Result};

fn hex_value(b: u8) -> Option<u32> {
  match b {
    b'0'..=b'9' => Some((b - b'0') as u32),
    b'a'..=b'f' => Some((b - b'a' + 10) as u32),
    b'A'..=b'F' => Some((b - b'A' + 10) as u32),
    _ => None,
  }
}

fn push_code_point_utf16(out: &mut Vec<u16>, cp: u32) -> Result<()> {
  if cp > 0x10FFFF {
    bail!("Unicode code point out of range: 0x{cp:X}");
  }
  if cp <= 0xFFFF {
    out.push(cp as u16);
    return Ok(());
  }
  let v = cp - 0x1_0000;
  let high = 0xD800 + ((v >> 10) as u16);
  let low = 0xDC00 + ((v & 0x3FF) as u16);
  out.push(high);
  out.push(low);
  Ok(())
}

/// Decode a JavaScript string literal (including the surrounding quotes) into UTF-16 code units.
///
/// This is intentionally minimal and only supports the escape forms used in the upstream
/// test262-generated Unicode string-property lists:
/// - `\\uXXXX`
/// - `\\u{...}`
/// - `\\xXX`
///
/// Other escapes are supported for completeness (`\\n`, `\\t`, `\\\"`, etc.), but octal escapes are
/// rejected.
pub fn decode_js_string_literal_to_utf16(literal: &str) -> Result<Vec<u16>> {
  let bytes = literal.as_bytes();
  if bytes.len() < 2 {
    bail!("invalid JS string literal: too short");
  }

  let quote = bytes[0];
  if quote != b'"' && quote != b'\'' {
    bail!("invalid JS string literal: expected leading quote");
  }
  if bytes[bytes.len() - 1] != quote {
    bail!("invalid JS string literal: unterminated string (missing closing quote)");
  }

  let mut out: Vec<u16> = Vec::new();
  let mut i = 1usize;
  let end = bytes.len() - 1;
  while i < end {
    let b = bytes[i];
    if b == b'\\' {
      i += 1;
      if i >= end {
        bail!("invalid JS string literal: trailing backslash escape");
      }
      match bytes[i] {
        b'\\' => {
          out.push(b'\\' as u16);
          i += 1;
        }
        b'"' => {
          out.push(b'"' as u16);
          i += 1;
        }
        b'\'' => {
          out.push(b'\'' as u16);
          i += 1;
        }
        b'n' => {
          out.push(b'\n' as u16);
          i += 1;
        }
        b'r' => {
          out.push(b'\r' as u16);
          i += 1;
        }
        b't' => {
          out.push(b'\t' as u16);
          i += 1;
        }
        b'b' => {
          out.push(0x0008);
          i += 1;
        }
        b'f' => {
          out.push(0x000C);
          i += 1;
        }
        b'v' => {
          out.push(0x000B);
          i += 1;
        }
        b'0' => {
          // `\0` (but reject octal escapes like `\01`).
          if i + 1 < end && matches!(bytes[i + 1], b'0'..=b'9') {
            bail!("invalid JS string literal: octal escapes are not supported");
          }
          out.push(0);
          i += 1;
        }
        b'x' => {
          // `\xXX`
          if i + 2 >= end {
            bail!("invalid JS string literal: incomplete \\xXX escape");
          }
          let h1 = bytes[i + 1];
          let h2 = bytes[i + 2];
          let Some(v1) = hex_value(h1) else {
            bail!("invalid JS string literal: invalid hex digit in \\xXX escape");
          };
          let Some(v2) = hex_value(h2) else {
            bail!("invalid JS string literal: invalid hex digit in \\xXX escape");
          };
          out.push(((v1 << 4) | v2) as u16);
          i += 3;
        }
        b'u' => {
          // `\uXXXX` or `\u{...}`
          if i + 1 < end && bytes[i + 1] == b'{' {
            // `\u{...}`
            i += 2; // skip `u{`
            if i >= end {
              bail!("invalid JS string literal: unterminated \\u{{...}} escape");
            }
            let mut value: u32 = 0;
            let mut saw_digit = false;
            while i < end {
              let c = bytes[i];
              if c == b'}' {
                i += 1;
                break;
              }
              let Some(d) = hex_value(c) else {
                bail!("invalid JS string literal: invalid hex digit in \\u{{...}} escape");
              };
              saw_digit = true;
              value = value.saturating_mul(16).saturating_add(d);
              if value > 0x10FFFF {
                bail!("invalid JS string literal: Unicode code point out of range in \\u{{...}}");
              }
              i += 1;
            }
            if !saw_digit {
              bail!("invalid JS string literal: empty \\u{{...}} escape");
            }
            if i <= end && bytes.get(i.wrapping_sub(1)) != Some(&b'}') {
              bail!("invalid JS string literal: unterminated \\u{{...}} escape");
            }
            push_code_point_utf16(&mut out, value)?;
          } else {
            // `\uXXXX`
            if i + 4 >= end {
              bail!("invalid JS string literal: incomplete \\uXXXX escape");
            }
            let mut value: u32 = 0;
            for j in 0..4 {
              let c = bytes[i + 1 + j];
              let Some(d) = hex_value(c) else {
                bail!("invalid JS string literal: invalid hex digit in \\uXXXX escape");
              };
              value = (value << 4) | d;
            }
            out.push(value as u16);
            i += 5;
          }
        }
        other => {
          bail!(
            "invalid JS string literal: unsupported escape sequence: \\{}",
            other as char
          );
        }
      }
      continue;
    }

    // Non-escaped code point.
    let ch = literal[i..end].chars().next().expect("valid UTF-8");
    if ch == '\n' || ch == '\r' {
      bail!("invalid JS string literal: unescaped line terminator");
    }
    push_code_point_utf16(&mut out, ch as u32)?;
    i += ch.len_utf8();
  }

  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::decode_js_string_literal_to_utf16;

  #[test]
  fn decodes_u_xxxx_escape() {
    let out = decode_js_string_literal_to_utf16("\"\\u0041\"").unwrap();
    assert_eq!(out, vec![0x0041]);
  }

  #[test]
  fn decodes_u_braced_escape() {
    let out = decode_js_string_literal_to_utf16("\"\\u{41}\"").unwrap();
    assert_eq!(out, vec![0x0041]);
  }

  #[test]
  fn decodes_x_xx_escape() {
    let out = decode_js_string_literal_to_utf16("\"\\x41\"").unwrap();
    assert_eq!(out, vec![0x0041]);
  }

  #[test]
  fn decodes_mixed_literal_and_escapes() {
    let out = decode_js_string_literal_to_utf16("\"7\\uFE0F\\u20E3\"").unwrap();
    assert_eq!(out, vec![0x0037, 0xFE0F, 0x20E3]);
  }

  #[test]
  fn decodes_non_bmp_to_surrogate_pair() {
    let out = decode_js_string_literal_to_utf16("\"\\u{1F600}\"").unwrap();
    assert_eq!(out, vec![0xD83D, 0xDE00]); // 😀
  }

  #[test]
  fn rejects_invalid_escapes() {
    let err = decode_js_string_literal_to_utf16("\"\\u{}\"").unwrap_err();
    assert!(
      err.to_string().contains("empty"),
      "expected error to mention empty escape; got: {err}"
    );

    let err = decode_js_string_literal_to_utf16("\"\\x0\"").unwrap_err();
    assert!(
      err.to_string().contains("incomplete"),
      "expected error to mention incomplete escape; got: {err}"
    );

    let err = decode_js_string_literal_to_utf16("\"\\u{110000}\"").unwrap_err();
    assert!(
      err.to_string().contains("out of range"),
      "expected error to mention out of range; got: {err}"
    );
  }
}
