use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};

fn is_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

/// Verify Subresource Integrity (SRI) metadata for a fetched resource.
///
/// This is a small, deterministic subset of the SRI spec surface:
/// - Supports `sha256-...` tokens only.
/// - Accepts the resource if *any* valid `sha256` token matches (matches browser behavior).
/// - Treats an `integrity` attribute with no supported/valid tokens as a failure.
pub(crate) fn verify_integrity_sha256(bytes: &[u8], integrity: &str) -> std::result::Result<(), String> {
  let mut expected: Vec<[u8; 32]> = Vec::new();

  for token in integrity.split(is_ascii_whitespace) {
    let token = token.trim_matches(is_ascii_whitespace);
    if token.is_empty() {
      continue;
    }

    let Some((alg, digest_b64)) = token.split_once('-') else {
      continue;
    };
    if !alg.eq_ignore_ascii_case("sha256") {
      continue;
    }
    let decoded = match BASE64_STANDARD.decode(digest_b64) {
      Ok(decoded) => decoded,
      Err(_) => continue,
    };
    let Ok(arr) = <[u8; 32]>::try_from(decoded.as_slice()) else {
      continue;
    };
    expected.push(arr);
  }

  if expected.is_empty() {
    return Err("SRI integrity metadata did not contain any supported sha256 digests".to_string());
  }

  let actual = Sha256::digest(bytes);
  if expected
    .iter()
    .any(|digest| digest.as_slice() == actual.as_slice())
  {
    return Ok(());
  }

  Err("SRI integrity check failed (sha256 mismatch)".to_string())
}

#[cfg(test)]
mod tests {
  use super::verify_integrity_sha256;
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use sha2::{Digest, Sha256};

  #[test]
  fn accepts_matching_sha256_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha256::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha256-{b64}");
    verify_integrity_sha256(bytes, &integrity).expect("integrity should match");
  }

  #[test]
  fn rejects_mismatched_sha256_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha256::digest(b"other");
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha256-{b64}");
    verify_integrity_sha256(bytes, &integrity).expect_err("integrity should mismatch");
  }

  #[test]
  fn rejects_integrity_with_no_supported_hashes() {
    verify_integrity_sha256(b"x", "sha512-abc").expect_err("sha512 should be unsupported");
    verify_integrity_sha256(b"x", "").expect_err("empty integrity should fail");
  }
}

