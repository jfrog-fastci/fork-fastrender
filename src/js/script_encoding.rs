use encoding_rs::Encoding;

/// Decode an external classic script's raw bytes into source text.
///
/// This follows the WHATWG HTML "fetch a classic script" decoding rules in spirit:
/// - A BOM (UTF-8/UTF-16) overrides any other encoding signals.
/// - Otherwise, honor an HTTP `Content-Type` charset parameter when present.
/// - Otherwise, use the provided fallback encoding (typically from `<script charset>` or the
///   document encoding).
/// - Decoding is always lossy; invalid sequences are replaced.
pub fn decode_classic_script_bytes(
  bytes: &[u8],
  content_type: Option<&str>,
  fallback: &'static Encoding,
) -> String {
  if bytes.is_empty() {
    return String::new();
  }

  // HTML: BOM overrides any other encoding signal (response charset, fallback, etc).
  if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
    return enc
      .decode_without_bom_handling(&bytes[bom_len..])
      .0
      .into_owned();
  }

  if let Some(label) = content_type.and_then(crate::html::encoding::charset_from_content_type) {
    if let Some(enc) = Encoding::for_label(label.as_bytes()) {
      return enc.decode_without_bom_handling(bytes).0.into_owned();
    }
  }

  fallback.decode_without_bom_handling(bytes).0.into_owned()
}

#[cfg(test)]
mod tests {
  use super::decode_classic_script_bytes;

  use encoding_rs::{SHIFT_JIS, UTF_8, WINDOWS_1252};

  #[test]
  fn content_type_charset_is_used() {
    let encoded = SHIFT_JIS.encode("console.log('デ')").0;
    let decoded =
      decode_classic_script_bytes(&encoded, Some("text/javascript; charset=shift_jis"), UTF_8);
    assert!(
      decoded.contains('デ'),
      "decoded script should contain kana when Content-Type declares shift_jis: {decoded:?}"
    );
  }

  #[test]
  fn content_type_charset_overrides_fallback() {
    let encoded = WINDOWS_1252.encode("£").0;
    let decoded = decode_classic_script_bytes(
      &encoded,
      Some("text/javascript; charset=windows-1252"),
      SHIFT_JIS,
    );
    assert_eq!(decoded, "£");
  }

  #[test]
  fn bom_overrides_content_type_charset() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(UTF_8.encode("£").0.as_ref());
    let decoded = decode_classic_script_bytes(
      &bytes,
      Some("text/javascript; charset=windows-1252"),
      WINDOWS_1252,
    );
    assert_eq!(decoded, "£");
  }
}
