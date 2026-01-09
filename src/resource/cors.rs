use crate::debug::runtime;
use url::Url;

use super::DocumentOrigin;
use super::FetchedResource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorsMode {
  Anonymous,
  UseCredentials,
}

/// Returns true when subresource CORS enforcement is enabled.
///
/// Controlled by `FASTR_FETCH_ENFORCE_CORS` (truthy/falsey). Defaults to `false`.
pub fn cors_enforcement_enabled() -> bool {
  runtime::runtime_toggles().truthy_with_default("FASTR_FETCH_ENFORCE_CORS", false)
}

/// Validate the `Access-Control-Allow-Origin` response header for a fetched resource.
///
/// This is a best-effort approximation of Chromium's CORS checks for resources like web fonts.
/// It is intentionally strict about invalid header values (notably comma-separated origins).
pub fn validate_cors_allow_origin(
  request_origin: &DocumentOrigin,
  resource: &FetchedResource,
  requested_url: &str,
  mode: CorsMode,
) -> std::result::Result<(), String> {
  let effective_url = resource.final_url.as_deref().unwrap_or(requested_url);
  let parsed = match Url::parse(effective_url) {
    Ok(parsed) => parsed,
    Err(_) => return Ok(()),
  };

  if !matches!(parsed.scheme(), "http" | "https") {
    return Ok(());
  }

  let target_origin = DocumentOrigin::from_parsed_url(&parsed);
  if target_origin.same_origin(request_origin) {
    return Ok(());
  }

  let raw = resource
    .access_control_allow_origin
    .as_deref()
    .map(super::trim_http_whitespace)
    .filter(|v| !v.is_empty())
    .ok_or_else(|| "blocked by CORS: missing Access-Control-Allow-Origin".to_string())?;

  if raw == "*" {
    return match mode {
      CorsMode::Anonymous => Ok(()),
      CorsMode::UseCredentials => Err(
        "blocked by CORS: Access-Control-Allow-Origin * is not allowed for credentialed requests"
          .to_string(),
      ),
    };
  }

  // Chromium treats multiple origins as invalid even if one matches.
  if raw.contains(',') {
    return Err(format!(
      "blocked by CORS: invalid Access-Control-Allow-Origin (multiple values): {raw}"
    ));
  }

  if raw.eq_ignore_ascii_case("null") {
    if matches!(mode, CorsMode::UseCredentials) && !resource.access_control_allow_credentials {
      return Err("blocked by CORS: missing Access-Control-Allow-Credentials: true".to_string());
    }
    if !request_origin.is_http_like() {
      return Ok(());
    }
    return Err(format!(
      "blocked by CORS: Access-Control-Allow-Origin null does not match document origin {request_origin}"
    ));
  }

  let parsed_origin = Url::parse(raw)
    .map_err(|_| format!("blocked by CORS: invalid Access-Control-Allow-Origin: {raw}"))?;
  let allowed_origin = DocumentOrigin::from_parsed_url(&parsed_origin);
  if !allowed_origin.same_origin(request_origin) {
    return Err(format!(
      "blocked by CORS: Access-Control-Allow-Origin {allowed_origin} does not match document origin {request_origin}"
    ));
  }

  if matches!(mode, CorsMode::UseCredentials) && !resource.access_control_allow_credentials {
    return Err("blocked by CORS: missing Access-Control-Allow-Credentials: true".to_string());
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{validate_cors_allow_origin, CorsMode};
  use crate::resource::{origin_from_url, FetchedResource};

  #[test]
  fn allows_null_origin_for_anonymous_non_http_documents() {
    let doc_origin = origin_from_url("file:///fixture.html").expect("origin");
    let url = "https://example.com/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("null".to_string());

    validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::Anonymous)
      .expect("null origin should be accepted for anonymous file origins");
  }

  #[test]
  fn requires_allow_credentials_for_credentialed_null_origin() {
    let doc_origin = origin_from_url("file:///fixture.html").expect("origin");
    let url = "https://example.com/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("null".to_string());
    resource.access_control_allow_credentials = false;

    let err = validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::UseCredentials)
      .expect_err("expected credentialed null origin without ACAC to fail");
    assert!(
      err.contains("Access-Control-Allow-Credentials"),
      "unexpected error message: {err}"
    );

    resource.access_control_allow_credentials = true;
    validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::UseCredentials)
      .expect("credentialed null origin should succeed with ACAC=true");
  }

  #[test]
  fn wildcard_not_allowed_for_credentialed_requests() {
    let doc_origin = origin_from_url("https://example.com/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("*".to_string());
    resource.access_control_allow_credentials = true;

    let err = validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::UseCredentials)
      .expect_err("expected wildcard ACAO to be rejected for credentialed requests");
    assert!(
      err.contains("not allowed for credentialed requests"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn multiple_allow_origin_values_are_rejected() {
    let doc_origin = origin_from_url("https://example.com/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin =
      Some("https://example.com, https://evil.example".to_string());

    let err = validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::Anonymous)
      .expect_err("expected multiple ACAO values to be rejected");
    assert!(err.contains("multiple values"), "unexpected error: {err}");
  }

  #[test]
  fn non_ascii_whitespace_validate_cors_allow_origin_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let doc_origin = origin_from_url("https://example.com/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some(format!("{nbsp}*"));

    let err = validate_cors_allow_origin(&doc_origin, &resource, url, CorsMode::Anonymous)
      .expect_err("NBSP-prefixed ACAO wildcard must not be accepted");
    assert!(
      err.contains("invalid Access-Control-Allow-Origin"),
      "unexpected error: {err}"
    );
  }
}
