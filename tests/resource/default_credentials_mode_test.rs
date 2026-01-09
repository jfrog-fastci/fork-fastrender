use fastrender::resource::{FetchCredentialsMode, FetchDestination, FetchRequest};

#[test]
fn fetch_request_defaults_credentials_mode_by_destination() {
  let cases: &[(FetchDestination, FetchCredentialsMode)] = &[
    (FetchDestination::Fetch, FetchCredentialsMode::SameOrigin),
    (FetchDestination::StyleCors, FetchCredentialsMode::SameOrigin),
    (FetchDestination::ImageCors, FetchCredentialsMode::SameOrigin),
    (FetchDestination::Font, FetchCredentialsMode::SameOrigin),
    (FetchDestination::Document, FetchCredentialsMode::Include),
    (FetchDestination::DocumentNoUser, FetchCredentialsMode::Include),
    (FetchDestination::Iframe, FetchCredentialsMode::Include),
    (FetchDestination::Style, FetchCredentialsMode::Include),
    (FetchDestination::Image, FetchCredentialsMode::Include),
    (FetchDestination::Other, FetchCredentialsMode::Include),
  ];

  for &(destination, expected) in cases {
    let req = FetchRequest::new("https://example.com", destination);
    assert_eq!(
      req.credentials_mode, expected,
      "destination={destination:?}"
    );
  }
}
