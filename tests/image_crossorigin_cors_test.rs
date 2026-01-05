use base64::{engine::general_purpose, Engine as _};
use fastrender::api::ResourceContext;
use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::error::{Error, ImageError};
use fastrender::image_loader::ImageCache;
use fastrender::resource::{origin_from_url, HttpFetcher, ResourceAccessPolicy, ResourceFetcher};
use fastrender::tree::box_tree::CrossOriginAttribute;
use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod test_support;
use test_support::net::try_bind_localhost;

const MAX_WAIT: Duration = Duration::from_secs(3);

fn spawn_server<F>(listener: TcpListener, max_requests: usize, mut handler: F) -> thread::JoinHandle<()>
where
  F: FnMut(usize, Vec<u8>, &mut std::net::TcpStream) + Send + 'static,
{
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    let mut handled = 0;
    while handled < max_requests && start.elapsed() < MAX_WAIT {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let mut buf = Vec::new();
          let mut tmp = [0u8; 1024];
          loop {
            match stream.read(&mut tmp) {
              Ok(0) => break,
              Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                  break;
                }
              }
              Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
              }
              Err(_) => break,
            }
          }
          handled += 1;
          handler(handled, buf, &mut stream);
        }
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  })
}

fn tiny_png() -> Vec<u8> {
  general_purpose::STANDARD
    .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNoaGj4DwAFhAKAjM1mJgAAAABJRU5ErkJggg==")
    .expect("decode base64 png")
}

fn image_cache_for_document(doc_url: &str) -> ImageCache {
  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut cache = ImageCache::with_fetcher(fetcher);
  let policy = ResourceAccessPolicy::default().for_origin(origin_from_url(doc_url));
  cache.set_resource_context(Some(ResourceContext {
    document_url: Some(doc_url.to_string()),
    policy,
    ..Default::default()
  }));
  cache
}

fn enforce_cors_toggles() -> Arc<RuntimeToggles> {
  Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])))
}

#[test]
fn enforce_cors_blocks_cross_origin_img_without_acao() {
  let Some(listener) = try_bind_localhost("enforce_cors_blocks_cross_origin_img_without_acao")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let png = tiny_png();
  let handle = spawn_server(listener, 1, move |_count, req, stream| {
    let request = String::from_utf8_lossy(&req).to_ascii_lowercase();
    assert!(
      request.contains("sec-fetch-mode: cors"),
      "expected cors mode request, got: {request}"
    );
    assert!(
      request.contains("origin: http://example.test"),
      "expected origin header on cors-mode image request, got: {request}"
    );
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      png.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&png);
  });

  let doc_url = "http://example.test/page.html";
  let image_url = format!("http://{addr}/image.png");
  let err = with_thread_runtime_toggles(enforce_cors_toggles(), || {
    let cache = image_cache_for_document(doc_url);
    match cache.load_with_crossorigin(&image_url, CrossOriginAttribute::Anonymous) {
      Ok(_) => panic!("expected CORS enforcement to fail"),
      Err(err) => err,
    }
  });
  match err {
    Error::Image(ImageError::LoadFailed { reason, .. }) => {
      assert!(
        reason.contains("Access-Control-Allow-Origin"),
        "unexpected error message: {reason}"
      );
    }
    other => panic!("expected image load error, got {other:?}"),
  }
  handle.join().unwrap();
}

#[test]
fn enforce_cors_allows_cross_origin_img_with_acao_star() {
  let Some(listener) = try_bind_localhost("enforce_cors_allows_cross_origin_img_with_acao_star")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let png = tiny_png();
  let handle = spawn_server(listener, 1, move |_count, req, stream| {
    let request = String::from_utf8_lossy(&req).to_ascii_lowercase();
    assert!(
      request.contains("sec-fetch-mode: cors"),
      "expected cors mode request, got: {request}"
    );
    assert!(
      request.contains("origin: http://example.test"),
      "expected origin header on cors-mode image request, got: {request}"
    );
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      png.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&png);
  });

  let doc_url = "http://example.test/page.html";
  let image_url = format!("http://{addr}/image.png");
  let image = with_thread_runtime_toggles(enforce_cors_toggles(), || {
    let cache = image_cache_for_document(doc_url);
    cache
      .load_with_crossorigin(&image_url, CrossOriginAttribute::Anonymous)
      .expect("cors image should load with ACAO star")
  });
  assert_eq!(image.dimensions(), (1, 1));
  handle.join().unwrap();
}

#[test]
fn enforce_cors_allows_cross_origin_img_with_matching_acao() {
  let Some(listener) =
    try_bind_localhost("enforce_cors_allows_cross_origin_img_with_matching_acao")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let png = tiny_png();
  let handle = spawn_server(listener, 1, move |_count, req, stream| {
    let request = String::from_utf8_lossy(&req).to_ascii_lowercase();
    assert!(
      request.contains("sec-fetch-mode: cors"),
      "expected cors mode request, got: {request}"
    );
    assert!(
      request.contains("origin: http://example.test"),
      "expected origin header on cors-mode image request, got: {request}"
    );
    // Note: `DocumentOrigin` display includes default ports (e.g. `:80`), but ACAO values typically
    // omit them. This response intentionally omits the default port so the test will fail if
    // callers compare origins by string rather than via parsed origin semantics.
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nAccess-Control-Allow-Origin: http://example.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      png.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&png);
  });

  let doc_url = "http://example.test/page.html";
  let image_url = format!("http://{addr}/image.png");
  let image = with_thread_runtime_toggles(enforce_cors_toggles(), || {
    let cache = image_cache_for_document(doc_url);
    cache
      .load_with_crossorigin(&image_url, CrossOriginAttribute::Anonymous)
      .expect("cors image should load with matching ACAO")
  });
  assert_eq!(image.dimensions(), (1, 1));
  handle.join().unwrap();
}

#[test]
fn enforce_cors_does_not_affect_no_cors_images() {
  let Some(listener) = try_bind_localhost("enforce_cors_does_not_affect_no_cors_images") else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let png = tiny_png();
  let handle = spawn_server(listener, 1, move |_count, req, stream| {
    let request = String::from_utf8_lossy(&req).to_ascii_lowercase();
    assert!(
      request.contains("sec-fetch-mode: no-cors"),
      "expected no-cors request for default <img>, got: {request}"
    );
    assert!(
      !request.contains("\r\norigin:"),
      "expected no Origin header for no-cors image request, got: {request}"
    );
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      png.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&png);
  });

  let doc_url = "http://example.test/page.html";
  let image_url = format!("http://{addr}/image.png");
  let image = with_thread_runtime_toggles(enforce_cors_toggles(), || {
    let cache = image_cache_for_document(doc_url);
    cache.load(&image_url).expect("no-cors image should load")
  });
  assert_eq!(image.dimensions(), (1, 1));
  handle.join().unwrap();
}
