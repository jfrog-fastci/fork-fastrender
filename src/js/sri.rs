use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, STANDARD_NO_PAD as BASE64_STANDARD_NO_PAD};
use base64::Engine;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Maximum number of bytes we will store from a raw `integrity` attribute.
///
/// This is a deterministic bound to prevent pathological attribute values from forcing large
/// allocations.
pub(crate) const MAX_INTEGRITY_ATTRIBUTE_BYTES: usize = 4096;

fn is_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

/// Verify Subresource Integrity (SRI) metadata for a fetched resource.
///
/// This is a small, deterministic subset of the SRI spec surface:
/// - Supports `sha256-...`, `sha384-...`, and `sha512-...` tokens.
/// - Selects the strongest supported algorithm present (sha512 > sha384 > sha256) and accepts the
///   resource if any digest token for that algorithm matches.
/// - Treats an `integrity` attribute with no supported/valid tokens as a failure.
/// - Ignores optional SRI metadata parameters (`?foo`) after the base64 digest.
pub(crate) fn verify_integrity(bytes: &[u8], integrity: &str) -> std::result::Result<(), String> {
  let mut expected_sha256: Vec<[u8; 32]> = Vec::new();
  let mut expected_sha384: Vec<[u8; 48]> = Vec::new();
  let mut expected_sha512: Vec<[u8; 64]> = Vec::new();

  for token in integrity.split(is_ascii_whitespace) {
    let token = token.trim_matches(is_ascii_whitespace);
    if token.is_empty() {
      continue;
    }

    let Some((alg, digest_b64)) = token.split_once('-') else {
      continue;
    };
    let digest_b64 = digest_b64.split('?').next().unwrap_or(digest_b64);
    let decoded = match BASE64_STANDARD
      .decode(digest_b64)
      .ok()
      .or_else(|| BASE64_STANDARD_NO_PAD.decode(digest_b64).ok())
    {
      Some(decoded) => decoded,
      None => continue,
    };

    if alg.eq_ignore_ascii_case("sha256") {
      let Ok(arr) = <[u8; 32]>::try_from(decoded.as_slice()) else {
        continue;
      };
      expected_sha256.push(arr);
    } else if alg.eq_ignore_ascii_case("sha384") {
      let Ok(arr) = <[u8; 48]>::try_from(decoded.as_slice()) else {
        continue;
      };
      expected_sha384.push(arr);
    } else if alg.eq_ignore_ascii_case("sha512") {
      let Ok(arr) = <[u8; 64]>::try_from(decoded.as_slice()) else {
        continue;
      };
      expected_sha512.push(arr);
    }
  }

  // Per SRI's "get the strongest metadata from set", prefer stronger algorithms when multiple are
  // supplied. Do not fall back to weaker algorithms if a stronger algorithm is present but does not
  // match.
  if !expected_sha512.is_empty() {
    let actual = Sha512::digest(bytes);
    if expected_sha512
      .iter()
      .any(|digest| digest.as_slice() == actual.as_slice())
    {
      return Ok(());
    }
    return Err("SRI integrity check failed (sha512 mismatch)".to_string());
  }
  if !expected_sha384.is_empty() {
    let actual = Sha384::digest(bytes);
    if expected_sha384
      .iter()
      .any(|digest| digest.as_slice() == actual.as_slice())
    {
      return Ok(());
    }
    return Err("SRI integrity check failed (sha384 mismatch)".to_string());
  }
  if !expected_sha256.is_empty() {
    let actual = Sha256::digest(bytes);
    if expected_sha256
      .iter()
      .any(|digest| digest.as_slice() == actual.as_slice())
    {
      return Ok(());
    }
    return Err("SRI integrity check failed (sha256 mismatch)".to_string());
  }

  Err("SRI integrity metadata did not contain any supported sha256/sha384/sha512 digests".to_string())
}

#[cfg(test)]
mod tests {
  use super::verify_integrity;
  use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
  use base64::Engine;
  use sha2::{Digest, Sha256, Sha384, Sha512};

  #[test]
  fn accepts_matching_sha256_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha256::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha256-{b64}");
    verify_integrity(bytes, &integrity).expect("integrity should match");
  }

  #[test]
  fn accepts_matching_sha384_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha384::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha384-{b64}");
    verify_integrity(bytes, &integrity).expect("integrity should match");
  }

  #[test]
  fn accepts_matching_sha512_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha512::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha512-{b64}");
    verify_integrity(bytes, &integrity).expect("integrity should match");
  }

  #[test]
  fn accepts_when_any_digest_matches_for_a_single_algorithm() {
    let bytes = b"console.log('ok');";
    let sha256 = BASE64_STANDARD.encode(Sha256::digest(bytes));
    let sha256_wrong = BASE64_STANDARD.encode(Sha256::digest(b"other"));
    let integrity = format!("sha256-{sha256_wrong} sha256-{sha256}");
    verify_integrity(bytes, &integrity).expect("expected one matching digest to succeed");
  }

  #[test]
  fn rejects_when_stronger_algorithm_is_present_but_mismatched() {
    let bytes = b"console.log('ok');";
    let sha256 = BASE64_STANDARD.encode(Sha256::digest(bytes));
    let sha512_wrong = BASE64_STANDARD.encode(Sha512::digest(b"other"));
    let integrity = format!("sha512-{sha512_wrong} sha256-{sha256}");
    verify_integrity(bytes, &integrity).expect_err("sha512 should be preferred over sha256");
  }

  #[test]
  fn accepts_sha256_with_metadata_parameters() {
    let bytes = b"console.log('ok');";
    let digest = Sha256::digest(bytes);
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha256-{b64}?foo=bar");
    verify_integrity(bytes, &integrity).expect("integrity should match even with metadata params");
  }

  #[test]
  fn rejects_mismatched_sha256_digest() {
    let bytes = b"console.log('ok');";
    let digest = Sha256::digest(b"other");
    let b64 = BASE64_STANDARD.encode(digest);
    let integrity = format!("sha256-{b64}");
    verify_integrity(bytes, &integrity).expect_err("integrity should mismatch");
  }

  #[test]
  fn rejects_integrity_with_no_supported_hashes() {
    verify_integrity(b"x", "sha1-abc").expect_err("sha1 should be unsupported");
    verify_integrity(b"x", "").expect_err("empty integrity should fail");
  }
}
