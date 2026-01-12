use base64::engine::general_purpose;
use base64::Engine as _;
use url::Url;

const MAX_INPUT_LEN: usize = 32 * 1024 * 1024;
const MAX_OUTPUT_LEN: usize = 32 * 1024 * 1024;

/// Compute HTML's serialized origin for a given URL.
///
/// The runner is not a full browser implementation; we intentionally follow the task's
/// conservative requirements:
/// - only `http:` and `https:` URLs return a tuple origin
/// - everything else (including opaque origins) returns `"null"`
pub(crate) fn serialized_origin_for_document_url(url: &str) -> String {
  let Ok(url) = Url::parse(url) else {
    return "null".to_string();
  };
  let scheme = url.scheme();
  if scheme != "http" && scheme != "https" {
    return "null".to_string();
  }
  let Some(host) = url.host() else {
    return "null".to_string();
  };
  let port = url.port_or_known_default().unwrap_or_default();
  let default_port = match scheme {
    "http" => 80,
    "https" => 443,
    _ => port,
  };
  if port == default_port {
    format!("{scheme}://{host}")
  } else {
    format!("{scheme}://{host}:{port}")
  }
}

/// Conservative `isSecureContext` implementation: `https:` is secure, and we optionally allow
/// `http://localhost` loopback origins.
pub(crate) fn is_secure_context_for_document_url(url: &str) -> bool {
  let Ok(url) = Url::parse(url) else {
    return false;
  };
  match url.scheme() {
    "https" => true,
    "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")),
    _ => false,
  }
}

pub(crate) fn forgiving_base64_encode(bytes: &[u8]) -> Result<String, ()> {
  if bytes.len() > MAX_OUTPUT_LEN {
    return Err(());
  }
  Ok(general_purpose::STANDARD.encode(bytes))
}

/// Implements HTML's "forgiving-base64 decode" algorithm for `atob`.
///
/// Notes:
/// - Strips ASCII whitespace.
/// - Accepts missing padding.
/// - Rejects invalid alphabet characters (including `=` after stripping).
pub(crate) fn forgiving_base64_decode(input: &str) -> Result<Vec<u8>, ()> {
  if input.len() > MAX_INPUT_LEN {
    return Err(());
  }

  // 1. Remove all ASCII whitespace.
  let mut stripped = String::with_capacity(input.len());
  for ch in input.chars() {
    if matches!(ch, '\t' | '\n' | '\u{000C}' | '\r' | ' ') {
      continue;
    }
    stripped.push(ch);
  }

  // 2. If length mod 4 is 0, remove up to two `=` from the end.
  if stripped.len() % 4 == 0 {
    let mut removed = 0usize;
    while removed < 2 && stripped.ends_with('=') {
      stripped.pop();
      removed += 1;
    }
  }

  // 3. If length mod 4 is 1, fail.
  if stripped.len() % 4 == 1 {
    return Err(());
  }

  // 4. If it contains a non-base64 character, fail.
  if stripped.bytes().any(|b| !is_base64_alphabet_byte(b)) {
    return Err(());
  }

  // 5. Pad with `=` until length mod 4 is 0.
  while stripped.len() % 4 != 0 {
    stripped.push('=');
  }

  // 6. Decode.
  let decoded = general_purpose::STANDARD
    .decode(stripped.as_bytes())
    .map_err(|_| ())?;
  if decoded.len() > MAX_OUTPUT_LEN {
    return Err(());
  }
  Ok(decoded)
}

pub(crate) fn latin1_encode(input: &str) -> Result<Vec<u8>, ()> {
  if input.len() > MAX_INPUT_LEN {
    return Err(());
  }
  let mut out = Vec::with_capacity(input.len());
  for ch in input.chars() {
    let cp = ch as u32;
    if cp > 0xFF {
      return Err(());
    }
    out.push(cp as u8);
  }
  if out.len() > MAX_OUTPUT_LEN {
    return Err(());
  }
  Ok(out)
}

fn is_base64_alphabet_byte(b: u8) -> bool {
  matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/')
}
