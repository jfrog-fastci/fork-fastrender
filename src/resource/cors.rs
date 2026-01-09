use crate::debug::runtime;
use url::Url;

use super::DocumentOrigin;
use super::FetchCredentialsMode;
use super::FetchedResource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorsMode {
  Anonymous,
  UseCredentials,
}

impl CorsMode {
  /// Maps an HTML "CORS settings attribute" value (e.g. `<img crossorigin>`) to the Fetch
  /// credentials mode.
  ///
  /// Per the HTML spec, `crossorigin="anonymous"` uses the `same-origin` credentials mode: include
  /// credentials only for same-origin requests, omitting them for cross-origin requests. This is
  /// why browsers still send cookies for same-origin anonymous CORS requests.
  pub const fn credentials_mode(self) -> FetchCredentialsMode {
    match self {
      Self::Anonymous => FetchCredentialsMode::SameOrigin,
      Self::UseCredentials => FetchCredentialsMode::Include,
    }
  }
}

/// Returns true when subresource CORS enforcement is enabled.
///
/// Controlled by `FASTR_FETCH_ENFORCE_CORS` (truthy/falsey). Defaults to `true`.
pub fn cors_enforcement_enabled() -> bool {
  let toggles = runtime::runtime_toggles();
  let Some(raw) = toggles.get("FASTR_FETCH_ENFORCE_CORS") else {
    return true;
  };
  !matches!(
    raw.trim().to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

/// Validate the `Access-Control-Allow-Origin` response header for a fetched resource.
///
/// This is a best-effort approximation of Chromium's CORS checks for resources like web fonts.
/// It is intentionally strict about invalid header values (notably comma-separated origins).
pub fn validate_cors_allow_origin(
  resource: &FetchedResource,
  requested_url: &str,
  request_origin: Option<&DocumentOrigin>,
  credentials_mode: FetchCredentialsMode,
) -> std::result::Result<(), String> {
  let Some(request_origin) = request_origin else {
    // Without an origin for the initiating settings object, avoid over-blocking by skipping CORS
    // enforcement. This matches how other policy checks behave when origin data is unavailable.
    return Ok(());
  };

  let effective_url = resource.final_url.as_deref().unwrap_or(requested_url);
  let parsed = match Url::parse(effective_url).or_else(|_| Url::parse(requested_url)) {
    Ok(parsed) => parsed,
    Err(_) => return Ok(()),
  };

  if !matches!(parsed.scheme(), "http" | "https") {
    // CORS enforcement is defined for HTTP(S) responses; other schemes have no response header
    // surface to validate.
    return Ok(());
  }

  let target_origin = DocumentOrigin::from_parsed_url(&parsed);
  if target_origin.same_origin(request_origin) {
    return Ok(());
  }

  let credentialed = credentials_mode == FetchCredentialsMode::Include;

  let raw = resource
    .access_control_allow_origin
    .as_deref()
    .map(super::trim_http_whitespace)
    .filter(|v| !v.is_empty())
    .ok_or_else(|| "blocked by CORS: missing Access-Control-Allow-Origin".to_string())?;

  if raw == "*" {
    if credentialed {
      return Err(
        "blocked by CORS: Access-Control-Allow-Origin * is not allowed for credentialed requests"
          .to_string(),
      );
    }
    return Ok(());
  }

  // Chromium treats multiple origins as invalid even if one matches.
  if raw.contains(',') {
    return Err(format!(
      "blocked by CORS: invalid Access-Control-Allow-Origin (multiple values): {raw}"
    ));
  }

  if raw.eq_ignore_ascii_case("null") {
    if credentialed && !resource.access_control_allow_credentials {
      return Err("blocked by CORS: missing Access-Control-Allow-Credentials: true".to_string());
    }
    if !request_origin.is_http_like() {
      return Ok(());
    }
    return Err(format!(
      "blocked by CORS: Access-Control-Allow-Origin null does not match request origin {request_origin}"
    ));
  }

  let parsed_origin =
    Url::parse(raw).map_err(|_| format!("blocked by CORS: invalid Access-Control-Allow-Origin: {raw}"))?;
  let allowed_origin = DocumentOrigin::from_parsed_url(&parsed_origin);
  if !allowed_origin.same_origin(request_origin) {
    return Err(format!(
      "blocked by CORS: Access-Control-Allow-Origin {allowed_origin} does not match request origin {request_origin}"
    ));
  }

  if credentialed && !resource.access_control_allow_credentials {
    return Err("blocked by CORS: missing Access-Control-Allow-Credentials: true".to_string());
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{cors_enforcement_enabled, validate_cors_allow_origin};
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use crate::resource::{origin_from_url, FetchCredentialsMode, FetchedResource};
  use std::collections::HashMap;
  use std::sync::Arc;

  #[test]
  fn cors_enforcement_enabled_defaults_to_true() {
    with_thread_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::new())), || {
      assert!(cors_enforcement_enabled());
    });
  }

  #[test]
  fn cors_enforcement_enabled_honors_falsey_values() {
    for raw in ["0", "false", "no", "off", "  OFF  "] {
      let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
        "FASTR_FETCH_ENFORCE_CORS".to_string(),
        raw.to_string(),
      )])));
      with_thread_runtime_toggles(toggles, || {
        assert!(
          !cors_enforcement_enabled(),
          "expected {raw:?} to disable CORS enforcement"
        );
      });
    }
  }

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

    validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::SameOrigin,
    )
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

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::Include,
    )
    .expect_err("expected credentialed null origin without ACAC to fail");
    assert!(
      err.contains("Access-Control-Allow-Credentials"),
      "unexpected error message: {err}"
    );

    resource.access_control_allow_credentials = true;
    validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::Include,
    )
    .expect("credentialed null origin should succeed with ACAC=true");
  }

  #[test]
  fn rejects_null_origin_for_http_documents() {
    let doc_origin = origin_from_url("https://example.com/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("null".to_string());

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::SameOrigin,
    )
    .expect_err("expected null ACAO to be rejected for http origins");
    assert!(
      err.contains("does not match request origin"),
      "unexpected error: {err}"
    );
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

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::SameOrigin,
    )
    .expect_err("NBSP-prefixed ACAO wildcard must not be accepted");
    assert!(
      err.contains("invalid Access-Control-Allow-Origin"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn anonymous_request_accepts_wildcard() {
    let doc_origin = origin_from_url("https://client.example/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("*".to_string());

    validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::SameOrigin,
    )
    .expect("wildcard ACAO should be accepted for non-credentialed requests");
  }

  #[test]
  fn credentialed_request_rejects_wildcard() {
    let doc_origin = origin_from_url("https://client.example/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("*".to_string());
    resource.access_control_allow_credentials = true;

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::Include,
    )
    .expect_err("credentialed request must reject ACAO=*");
    assert!(
      err.contains("not allowed for credentialed"),
      "unexpected error message: {err}"
    );
  }

  #[test]
  fn credentialed_request_requires_allow_credentials() {
    let doc_origin = origin_from_url("https://client.example/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin = Some("https://client.example".to_string());
    resource.access_control_allow_credentials = false;

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::Include,
    )
    .expect_err("credentialed request without ACAC must fail");
    assert!(
      err.contains("Access-Control-Allow-Credentials"),
      "unexpected error message: {err}"
    );
  }

  #[test]
  fn comma_separated_allow_origin_is_invalid() {
    let doc_origin = origin_from_url("https://client.example/").expect("origin");
    let url = "https://cross.example/image.png";
    let mut resource = FetchedResource::with_final_url(
      vec![1, 2, 3],
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.access_control_allow_origin =
      Some("https://client.example, https://other.example".to_string());

    let err = validate_cors_allow_origin(
      &resource,
      url,
      Some(&doc_origin),
      FetchCredentialsMode::SameOrigin,
    )
    .expect_err("comma-separated ACAO must be rejected");
    assert!(
      err.contains("multiple values"),
      "unexpected error message: {err}"
    );
  }
}
