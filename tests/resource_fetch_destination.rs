use fastrender::debug::runtime;
use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher};
use fastrender::resource::ResourceFetcher as _;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn http_request_headers_for_video_destination() {
  let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
    "FASTR_HTTP_BROWSER_HEADERS".to_string(),
    "1".to_string(),
  )])));
  runtime::with_thread_runtime_toggles(toggles, || {
    let fetcher = HttpFetcher::new();
    let url = "https://example.com/video.mp4";
    let req = FetchRequest::new(url, FetchDestination::Video);

    assert_eq!(
      fetcher.request_header_value(req, "accept").as_deref(),
      Some("video/webm,video/ogg,video/mp4,video/*;q=0.9,application/ogg;q=0.7,*/*;q=0.5")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-dest").as_deref(),
      Some("video")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-mode").as_deref(),
      Some("no-cors")
    );
  });
}

#[test]
fn http_request_headers_for_video_cors_destination() {
  let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
    "FASTR_HTTP_BROWSER_HEADERS".to_string(),
    "1".to_string(),
  )])));
  runtime::with_thread_runtime_toggles(toggles, || {
    let fetcher = HttpFetcher::new();
    let url = "https://example.com/video.mp4";
    let req = FetchRequest::new(url, FetchDestination::VideoCors);

    assert_eq!(
      fetcher.request_header_value(req, "accept").as_deref(),
      Some("video/webm,video/ogg,video/mp4,video/*;q=0.9,application/ogg;q=0.7,*/*;q=0.5")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-dest").as_deref(),
      Some("video")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-mode").as_deref(),
      Some("cors")
    );
  });
}

#[test]
fn http_request_headers_for_audio_destination() {
  let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
    "FASTR_HTTP_BROWSER_HEADERS".to_string(),
    "1".to_string(),
  )])));
  runtime::with_thread_runtime_toggles(toggles, || {
    let fetcher = HttpFetcher::new();
    let url = "https://example.com/audio.mp3";
    let req = FetchRequest::new(url, FetchDestination::Audio);

    assert_eq!(
      fetcher.request_header_value(req, "accept").as_deref(),
      Some("audio/webm,audio/ogg,audio/mp4,audio/*;q=0.9,application/ogg;q=0.7,*/*;q=0.5")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-dest").as_deref(),
      Some("audio")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-mode").as_deref(),
      Some("no-cors")
    );
  });
}

#[test]
fn http_request_headers_for_audio_cors_destination() {
  let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
    "FASTR_HTTP_BROWSER_HEADERS".to_string(),
    "1".to_string(),
  )])));
  runtime::with_thread_runtime_toggles(toggles, || {
    let fetcher = HttpFetcher::new();
    let url = "https://example.com/audio.mp3";
    let req = FetchRequest::new(url, FetchDestination::AudioCors);

    assert_eq!(
      fetcher.request_header_value(req, "accept").as_deref(),
      Some("audio/webm,audio/ogg,audio/mp4,audio/*;q=0.9,application/ogg;q=0.7,*/*;q=0.5")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-dest").as_deref(),
      Some("audio")
    );
    assert_eq!(
      fetcher.request_header_value(req, "sec-fetch-mode").as_deref(),
      Some("cors")
    );
  });
}
