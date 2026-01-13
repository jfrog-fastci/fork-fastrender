#![forbid(unsafe_code)]

use fastrender_ipc::{
  AffineTransform, BrowserToRenderer, ClipItem, FrameBuffer, FrameId, IpcTransport,
  NavigationContext, ReferrerPolicy, RendererToBrowser, SandboxFlags, SubframeInfo,
};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;
use url::Url;

const DEFAULT_VIEWPORT: (u32, u32) = (800, 600);
const DEFAULT_DPR: f32 = 1.0;

// Keep allocations bounded even if the browser sends a pathological `Resize`.
//
// This is also constrained by the IPC transport, since we currently send the pixel buffer inline
// in the `FrameReady` message.
const MAX_FRAME_BYTES: usize = fastrender_ipc::MAX_IPC_MESSAGE_BYTES - 256;

const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const FETCH_IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct FrameState {
  pub url: Option<String>,
  pub navigation_context: NavigationContext,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
}

impl FrameState {
  pub fn new() -> Self {
    Self {
      url: None,
      navigation_context: NavigationContext::default(),
      viewport_css: DEFAULT_VIEWPORT,
      dpr: DEFAULT_DPR,
    }
  }

  fn url_hash(url: &str) -> u32 {
    // Deterministic 32-bit FNV-1a. We only need a stable mixing function to make navigations
    // observable in unit tests without pulling in extra hashing dependencies.
    let mut hash: u32 = 0x811c9dc5;
    for &b in url.as_bytes() {
      hash ^= u32::from(b);
      hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
  }

  pub fn render_placeholder(&self, frame_id: FrameId) -> Result<FrameBuffer, String> {
    let (width, height) = self.viewport_css;
    let len = (width as usize)
      .checked_mul(height as usize)
      .and_then(|px| px.checked_mul(4))
      .ok_or_else(|| "viewport size overflow".to_string())?;

    if len > MAX_FRAME_BYTES {
      return Err(format!(
        "requested frame buffer too large: {width}x{height} => {len} bytes"
      ));
    }

    // Deterministic per-frame fill color to help catch cross-talk in tests/debugging.
    let id = frame_id.0;
    let url_hash = self.url.as_deref().map(Self::url_hash).unwrap_or(0);
    let r = (id as u8) ^ (url_hash as u8);
    let g = ((id >> 8) as u8) ^ ((url_hash >> 8) as u8);
    let b = ((id >> 16) as u8) ^ ((url_hash >> 16) as u8);
    let a = 0xFF;

    let mut rgba8 = vec![0u8; len];
    for px in rgba8.chunks_exact_mut(4) {
      px[0] = r;
      px[1] = g;
      px[2] = b;
      px[3] = a;
    }

    Ok(FrameBuffer {
      width,
      height,
      rgba8,
    })
  }
}

fn should_fetch_url(url: &Url) -> bool {
  if url.scheme() != "http" {
    // Keep the renderer loop deterministic/offline for now; unit tests use non-http URLs as
    // sentinel values. Multiprocess navigation/fetch integration tests use localhost HTTP.
    return false;
  }
  matches!(
    url.host_str(),
    Some("127.0.0.1") | Some("localhost") | Some("::1")
  )
}

fn origin_only(url: &Url) -> Option<String> {
  let scheme = url.scheme();
  let host = url.host_str()?;
  let port = url.port_or_known_default();
  let default_port = match scheme {
    "http" => Some(80),
    "https" => Some(443),
    _ => None,
  };
  let port_part = match (port, default_port) {
    (Some(port), Some(default)) if port == default => String::new(),
    (Some(port), _) => format!(":{port}"),
    (None, _) => String::new(),
  };
  Some(format!("{scheme}://{host}{port_part}/"))
}

fn compute_referer_header_value(
  raw_referrer: &str,
  target_url: &Url,
  policy: ReferrerPolicy,
) -> Option<String> {
  let policy = match policy {
    ReferrerPolicy::EmptyString => ReferrerPolicy::StrictOriginWhenCrossOrigin,
    other => other,
  };
  if policy == ReferrerPolicy::NoReferrer {
    return None;
  }

  let mut referrer_url = Url::parse(raw_referrer).ok()?;
  referrer_url.set_fragment(None);
  let _ = referrer_url.set_username("");
  let _ = referrer_url.set_password(None);

  let downgrade = referrer_url.scheme() == "https" && target_url.scheme() == "http";
  let same_origin = referrer_url.origin() == target_url.origin();

  let origin_only_value = origin_only(&referrer_url)?;
  let full_value = referrer_url.to_string();

  match policy {
    ReferrerPolicy::EmptyString => None,
    ReferrerPolicy::NoReferrer => None,
    ReferrerPolicy::NoReferrerWhenDowngrade => (!downgrade).then_some(full_value),
    ReferrerPolicy::Origin => Some(origin_only_value),
    ReferrerPolicy::OriginWhenCrossOrigin => Some(if same_origin {
      full_value
    } else {
      origin_only_value
    }),
    ReferrerPolicy::SameOrigin => same_origin.then_some(full_value),
    ReferrerPolicy::StrictOrigin => (!downgrade).then_some(origin_only_value),
    ReferrerPolicy::StrictOriginWhenCrossOrigin => {
      if same_origin {
        Some(full_value)
      } else if downgrade {
        None
      } else {
        Some(origin_only_value)
      }
    }
    ReferrerPolicy::UnsafeUrl => Some(full_value),
  }
}

fn http_get(url: &Url, referer: Option<&str>) -> Result<Vec<u8>, String> {
  let host = url
    .host_str()
    .ok_or_else(|| format!("URL missing host: {url}"))?;
  let port = url.port_or_known_default().unwrap_or(80);
  let addr = format!("{host}:{port}");
  let socket: SocketAddr = addr
    .to_socket_addrs()
    .map_err(|err| format!("resolve {addr}: {err}"))?
    .next()
    .ok_or_else(|| format!("no socket addrs for {addr}"))?;

  let mut stream =
    TcpStream::connect_timeout(&socket, FETCH_CONNECT_TIMEOUT).map_err(|err| err.to_string())?;
  stream
    .set_read_timeout(Some(FETCH_IO_TIMEOUT))
    .map_err(|err| err.to_string())?;
  stream
    .set_write_timeout(Some(FETCH_IO_TIMEOUT))
    .map_err(|err| err.to_string())?;

  let mut path = url.path().to_string();
  if path.is_empty() {
    path.push('/');
  }
  if let Some(q) = url.query() {
    path.push('?');
    path.push_str(q);
  }

  let mut request = format!(
    "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nUser-Agent: fastrender-renderer\r\nAccept: */*\r\n"
  );
  if let Some(referer) = referer {
    request.push_str(&format!("Referer: {referer}\r\n"));
  }
  request.push_str("\r\n");

  stream
    .write_all(request.as_bytes())
    .map_err(|err| err.to_string())?;
  stream.flush().map_err(|err| err.to_string())?;

  let mut response = Vec::new();
  stream
    .read_to_end(&mut response)
    .map_err(|err| err.to_string())?;

  let sep = response
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .ok_or_else(|| "invalid HTTP response (missing header separator)".to_string())?;
  Ok(response[(sep + 4)..].to_vec())
}

fn parse_tag_attributes(tag: &str) -> HashMap<String, String> {
  fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':'
  }

  let bytes = tag.as_bytes();
  let mut attrs = HashMap::<String, String>::new();
  let mut i = 0usize;

  // Skip `<tagname`.
  while i < bytes.len() && bytes[i] != b'<' {
    i += 1;
  }
  if i < bytes.len() && bytes[i] == b'<' {
    i += 1;
  }
  while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
    i += 1;
  }

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }

    let name_start = i;
    while i < bytes.len() && is_name_char(bytes[i]) {
      i += 1;
    }
    if i == name_start {
      i += 1;
      continue;
    }
    let name = String::from_utf8_lossy(&bytes[name_start..i])
      .to_ascii_lowercase()
      .to_string();

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = String::new();
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      if i >= bytes.len() {
        attrs.insert(name, value);
        break;
      }
      let quote = bytes[i];
      if quote == b'"' || quote == b'\'' {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = String::from_utf8_lossy(&bytes[start..i]).to_string();
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = String::from_utf8_lossy(&bytes[start..i]).to_string();
      }
    }

    attrs.insert(name, value);
  }

  attrs
}

fn find_start_tags<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
  let bytes = html.as_bytes();
  let pattern = format!("<{tag}").into_bytes();
  let mut out = Vec::new();
  let mut i = 0usize;
  while i + pattern.len() <= bytes.len() {
    if bytes[i] == b'<' && bytes[i..i + pattern.len()].eq_ignore_ascii_case(&pattern) {
      let Some(rel_end) = bytes[i..].iter().position(|&b| b == b'>') else {
        break;
      };
      let end = i + rel_end + 1;
      out.push(&html[i..end]);
      i = end;
      continue;
    }
    i += 1;
  }
  out
}

fn parse_sandbox_flags(raw: &str) -> SandboxFlags {
  let mut flags = SandboxFlags::NONE;
  for token in raw.split_ascii_whitespace() {
    if token.eq_ignore_ascii_case("allow-same-origin") {
      flags.insert(SandboxFlags::ALLOW_SAME_ORIGIN);
    } else if token.eq_ignore_ascii_case("allow-scripts") {
      flags.insert(SandboxFlags::ALLOW_SCRIPTS);
    } else if token.eq_ignore_ascii_case("allow-forms") {
      flags.insert(SandboxFlags::ALLOW_FORMS);
    } else if token.eq_ignore_ascii_case("allow-popups") {
      flags.insert(SandboxFlags::ALLOW_POPUPS);
    } else if token.eq_ignore_ascii_case("allow-top-navigation") {
      flags.insert(SandboxFlags::ALLOW_TOP_NAVIGATION);
    }
  }
  flags
}

fn subframes_from_html(frame_id: FrameId, html: &str) -> Vec<SubframeInfo> {
  let iframe_tags = find_start_tags(html, "iframe");
  iframe_tags
    .into_iter()
    .enumerate()
    .map(|(idx, tag)| {
      let attrs = parse_tag_attributes(tag);
      let referrer_policy = attrs
        .get("referrerpolicy")
        .and_then(|v| ReferrerPolicy::from_attribute(v));
      let sandbox_present = attrs.contains_key("sandbox");
      let sandbox_flags = attrs
        .get("sandbox")
        .map(|v| parse_sandbox_flags(v))
        .unwrap_or(SandboxFlags::NONE);
      let opaque_origin = sandbox_present && !sandbox_flags.contains(SandboxFlags::ALLOW_SAME_ORIGIN);

      SubframeInfo {
        child: FrameId(frame_id.0.saturating_mul(1000).saturating_add(idx as u64 + 1)),
        transform: AffineTransform::IDENTITY,
        clip_stack: Vec::<ClipItem>::new(),
        z_index: idx as u64,
        referrer_policy,
        sandbox_flags,
        opaque_origin,
      }
    })
    .collect()
}

fn image_urls_from_html(base: &Url, html: &str) -> Vec<Url> {
  let img_tags = find_start_tags(html, "img");
  let mut out = Vec::new();
  for tag in img_tags {
    let attrs = parse_tag_attributes(tag);
    let Some(src) = attrs.get("src") else {
      continue;
    };
    let src_trimmed = src.trim();
    if src_trimmed.is_empty() {
      continue;
    }
    if let Ok(url) = base.join(src_trimmed) {
      out.push(url);
    }
  }
  out
}

pub struct RendererMainLoop<T: IpcTransport> {
  transport: T,
  frames: HashMap<FrameId, FrameState>,
}

impl<T: IpcTransport> RendererMainLoop<T> {
  pub fn new(transport: T) -> Self {
    Self {
      transport,
      frames: HashMap::new(),
    }
  }

  pub fn run(mut self) -> Result<(), T::Error> {
    while let Some(msg) = self.transport.recv()? {
      match msg {
        BrowserToRenderer::CreateFrame { frame_id } => {
          if self.frames.contains_key(&frame_id) {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "CreateFrame for existing frame".to_string(),
            });
            continue;
          }
          self.frames.insert(frame_id, FrameState::new());
        }
        BrowserToRenderer::DestroyFrame { frame_id } => {
          self.frames.remove(&frame_id);
        }
        BrowserToRenderer::Navigate {
          frame_id,
          url,
          context,
        } => {
          if let Some(frame) = self.frames.get_mut(&frame_id) {
            frame.url = Some(url);
            frame.navigation_context = context;
          } else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "Navigate for unknown frame".to_string(),
            });
          }
        }
        BrowserToRenderer::Resize {
          frame_id,
          width,
          height,
          dpr,
        } => {
          if let Some(frame) = self.frames.get_mut(&frame_id) {
            frame.viewport_css = (width, height);
            frame.dpr = dpr;
          } else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "Resize for unknown frame".to_string(),
            });
          }
        }
        BrowserToRenderer::RequestRepaint { frame_id } => {
          let Some(frame) = self.frames.get(&frame_id) else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "RequestRepaint for unknown frame".to_string(),
            });
            continue;
          };

          let mut subframes = Vec::new();
          if let Some(raw_url) = frame.url.as_deref() {
            if let Ok(url) = Url::parse(raw_url) {
              if should_fetch_url(&url) {
                let nav_referer = frame
                  .navigation_context
                  .referrer_url
                  .as_deref()
                  .and_then(|referrer| {
                    compute_referer_header_value(
                      referrer,
                      &url,
                      frame.navigation_context.referrer_policy,
                    )
                  });
                match http_get(&url, nav_referer.as_deref()) {
                  Ok(body) => {
                    let html = String::from_utf8_lossy(&body);
                    subframes = subframes_from_html(frame_id, html.as_ref());

                    // Opportunistically fetch <img> subresources so integration tests can assert
                    // referrer policy behavior without implementing full layout/paint in the
                    // multiprocess renderer yet.
                    for img_url in image_urls_from_html(&url, html.as_ref()) {
                      let referer = compute_referer_header_value(
                        url.as_str(),
                        &img_url,
                        frame.navigation_context.referrer_policy,
                      );
                      let _ = http_get(&img_url, referer.as_deref());
                    }
                  }
                  Err(err) => {
                    let _ = self.transport.send(RendererToBrowser::Error {
                      frame_id: Some(frame_id),
                      message: format!("navigation fetch failed: {err}"),
                    });
                  }
                }
              }
            }
          }

          match frame.render_placeholder(frame_id) {
            Ok(buffer) => {
              self.transport
                .send(RendererToBrowser::FrameReady {
                  frame_id,
                  buffer,
                  subframes,
                })?;
            }
            Err(message) => {
              let _ = self.transport.send(RendererToBrowser::Error {
                frame_id: Some(frame_id),
                message,
              });
            }
          }
        }
        BrowserToRenderer::Shutdown => break,
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;
  use std::time::Duration;

  struct ChannelTransport {
    rx: mpsc::Receiver<BrowserToRenderer>,
    tx: mpsc::Sender<RendererToBrowser>,
  }

  impl IpcTransport for ChannelTransport {
    type Error = ();

    fn recv(&mut self) -> Result<Option<BrowserToRenderer>, Self::Error> {
      match self.rx.recv() {
        Ok(msg) => Ok(Some(msg)),
        Err(_) => Ok(None),
      }
    }

    fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error> {
      self.tx.send(msg).map_err(|_| ())
    }
  }

  #[test]
  fn multiplex_two_frames_in_one_process() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let frame_a = FrameId(1);
    let frame_b = FrameId(2);

    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame_a })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame_b })
      .unwrap();

    // Use different sizes to catch accidental cross-talk.
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame_a,
        width: 2,
        height: 2,
        dpr: 1.0,
      })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame_b,
        width: 3,
        height: 1,
        dpr: 1.0,
      })
      .unwrap();

    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame_a })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame_b })
      .unwrap();

    let mut ready = vec![];
    for _ in 0..2 {
      let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
      match msg {
        RendererToBrowser::FrameReady {
          frame_id,
          buffer,
          subframes,
        } => {
          assert!(
            subframes.is_empty(),
            "renderer placeholder should not report subframes"
          );
          ready.push((frame_id, buffer));
        }
        other => panic!("unexpected message: {other:?}"),
      }
    }

    ready.sort_by_key(|(id, _)| id.0);
    assert_eq!(ready[0].0, frame_a);
    assert_eq!(ready[0].1.width, 2);
    assert_eq!(ready[0].1.height, 2);
    assert_eq!(ready[0].1.rgba8.len(), 2 * 2 * 4);
    assert_eq!(ready[1].0, frame_b);
    assert_eq!(ready[1].1.width, 3);
    assert_eq!(ready[1].1.height, 1);
    assert_eq!(ready[1].1.rgba8.len(), 3 * 1 * 4);

    // Shut down and join the renderer loop.
    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }

  #[test]
  fn navigate_affects_only_target_frame() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let frame = FrameId(42);
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame })
      .unwrap();
    // Keep the payload tiny so we can compare buffers cheaply.
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame,
        width: 1,
        height: 1,
        dpr: 1.0,
      })
      .unwrap();

    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();
    let first = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        subframes,
      } => {
        assert_eq!(frame_id, frame);
        assert!(subframes.is_empty());
        buffer
      }
      other => panic!("unexpected message: {other:?}"),
    };

    to_renderer_tx
      .send(BrowserToRenderer::Navigate {
        frame_id: frame,
        url: "https://example.test/".to_string(),
        context: NavigationContext::default(),
      })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();
    let second = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        subframes,
      } => {
        assert_eq!(frame_id, frame);
        assert!(subframes.is_empty());
        buffer
      }
      other => panic!("unexpected message: {other:?}"),
    };

    assert_ne!(
      first.rgba8, second.rgba8,
      "expected Navigate to affect per-frame render output"
    );

    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }

  #[test]
  fn destroyed_frame_does_not_render() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let frame = FrameId(7);
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::DestroyFrame { frame_id: frame })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();

    let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    match msg {
      RendererToBrowser::Error {
        frame_id: Some(got),
        message: _,
      } => {
        assert_eq!(got, frame);
      }
      other => panic!("expected Error for destroyed frame, got {other:?}"),
    }

    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }

  #[test]
  fn create_frame_is_not_destructive_when_repeated() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let frame = FrameId(9);
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame,
        width: 1,
        height: 1,
        dpr: 1.0,
      })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::Navigate {
        frame_id: frame,
        url: "https://example.test/a".to_string(),
        context: NavigationContext::default(),
      })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();
    let first = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        subframes,
      } => {
        assert_eq!(frame_id, frame);
        assert!(subframes.is_empty());
        buffer
      }
      other => panic!("unexpected message: {other:?}"),
    };

    // Re-sending CreateFrame should not reset per-frame state.
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();

    let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    match msg {
      RendererToBrowser::Error {
        frame_id: Some(got),
        message,
      } => {
        assert_eq!(got, frame);
        assert!(
          message.contains("CreateFrame"),
          "unexpected error message: {message}"
        );
      }
      other => panic!("expected Error for duplicate CreateFrame, got {other:?}"),
    }

    let second = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        subframes,
      } => {
        assert_eq!(frame_id, frame);
        assert!(subframes.is_empty());
        buffer
      }
      other => panic!("unexpected message: {other:?}"),
    };

    assert_eq!(
      first.rgba8, second.rgba8,
      "duplicate CreateFrame should not reset frame render output"
    );

    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }
}
