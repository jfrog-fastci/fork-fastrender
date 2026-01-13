use fastrender::js::chrome_api::{
  validate_chrome_navigation_url, ChromeApiError, MAX_CHROME_NAVIGATION_URL_CODE_UNITS,
};

#[test]
fn validate_chrome_navigation_url_rejects_javascript_scheme() {
  let err = validate_chrome_navigation_url("javascript:alert(1)")
    .expect_err("javascript: scheme should be rejected");
  assert!(
    matches!(err, ChromeApiError::RejectedScheme(ref scheme) if scheme == "javascript"),
    "expected RejectedScheme(javascript), got {err:?}"
  );
}

#[test]
fn validate_chrome_navigation_url_rejects_overlong_url() {
  let overlong = "a".repeat(MAX_CHROME_NAVIGATION_URL_CODE_UNITS + 1);
  let err = validate_chrome_navigation_url(&overlong).expect_err("overlong url should be rejected");
  assert!(
    matches!(err, ChromeApiError::UrlTooLong),
    "expected UrlTooLong, got {err:?}"
  );
}
