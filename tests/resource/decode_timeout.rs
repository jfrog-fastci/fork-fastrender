use std::io;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use fastrender::api::{FastRender, RenderOptions};
use fastrender::error::{Error, RenderError};
use fastrender::render_control::{with_deadline, RenderDeadline};
use fastrender::resource::HttpFetcher;
use fastrender::ResourceFetcher;
use crate::test_support;
use test_support::net::{net_test_lock, try_bind_localhost};

const MAX_WAIT: Duration = Duration::from_secs(3);

struct EnvGuard(&'static str);

impl EnvGuard {
  fn set(key: &'static str, value: &str) -> Self {
    std::env::set_var(key, value);
    Self(key)
  }
}

impl Drop for EnvGuard {
  fn drop(&mut self) {
    std::env::remove_var(self.0);
  }
}

#[test]
fn compressed_resource_respects_render_timeout() {
  let _net_guard = net_test_lock();
  let _guard = EnvGuard::set("FASTR_TEST_RENDER_DELAY_MS", "10");
  let compressed = include_bytes!("../fixtures/large_timeout_payload.gz");

  let Some(listener) = try_bind_localhost("compressed_resource_respects_render_timeout") else {
    return;
  };
  let addr = listener.local_addr().expect("local addr");
  let done = Arc::new(AtomicBool::new(false));
  let done_thread = done.clone();
  let handle = thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < MAX_WAIT && !done_thread.load(Ordering::SeqCst) {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let mut buf = [0u8; 1024];
          let _ = stream.read(&mut buf);
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Encoding: gzip\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
            compressed.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(compressed).unwrap();
          break;
        }
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  });

  let fetcher = HttpFetcher::new();
  let options = RenderOptions::default().with_timeout(Some(Duration::from_millis(40)));
  let deadline = RenderDeadline::new(options.timeout, None);
  let url = format!("http://{}/image.png", addr);

  let result = with_deadline(Some(&deadline), || fetcher.fetch(&url));
  done.store(true, Ordering::SeqCst);
  let err = result.expect_err("expected timeout");

  match err {
    Error::Render(RenderError::Timeout { .. }) => {}
    Error::Resource(res) => {
      let lower = res.message.to_ascii_lowercase();
      assert!(
        lower.contains("timeout") || lower.contains("deadline"),
        "unexpected resource error: {}",
        res.message
      );
    }
    other => panic!("expected timeout error, got {other:?}"),
  }

  handle.join().unwrap();
}
#[test]
fn renderer_times_out_while_decompressing_image() {
  let _net_guard = net_test_lock();
  // Use a delay larger than the overall timeout so a single deadline check reliably triggers a
  // timeout regardless of host speed/caching.
  let _guard = EnvGuard::set("FASTR_TEST_RENDER_DELAY_MS", "50");
  let compressed = include_bytes!("../fixtures/large_timeout_payload.gz");

  let Some(listener) = try_bind_localhost("renderer_times_out_while_decompressing_image") else {
    return;
  };
  let addr = listener.local_addr().expect("local addr");
  let done = Arc::new(AtomicBool::new(false));
  let done_thread = done.clone();
  let handle = thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < MAX_WAIT && !done_thread.load(Ordering::SeqCst) {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let mut buf = [0u8; 1024];
          let _ = stream.read(&mut buf);
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Encoding: gzip\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
            compressed.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.write_all(compressed).unwrap();
          break;
        }
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  });

  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::default()
    .with_viewport(16, 16)
    .with_timeout(Some(Duration::from_millis(20)));
  let url = format!("http://{}/image.png", addr);
  let html = format!("<img src=\"{}\" />", url);

  let result = renderer.render_html_with_options(&html, options);
  done.store(true, Ordering::SeqCst);
  let err = result.expect_err("expected render timeout");

  match err {
    Error::Resource(res) => {
      assert!(
        res.message.to_ascii_lowercase().contains("decompress"),
        "unexpected resource error: {}",
        res.message
      );
    }
    Error::Render(RenderError::Timeout { .. }) => {}
    other => panic!("expected timeout error, got {other:?}"),
  }

  handle.join().unwrap();
}
