#![forbid(unsafe_code)]

use fastrender_ipc::{
  AffineTransform, BrowserToRenderer, ClipItem, CursorKind, FrameBuffer, FrameId, HoverState,
  IframeNavigation, IpcTransport, NavigationContext, ReferrerPolicy, RendererToBrowser, SandboxFlags,
  SiteLock, SubframeEffects, SubframeInfo,
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
  pub navigation: Option<IframeNavigation>,
  pub navigation_context: NavigationContext,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  hover_hint: HoverState,
  last_hover_sent: Option<HoverState>,
}

impl FrameState {
  pub fn new() -> Self {
    Self {
      navigation: None,
      navigation_context: NavigationContext::default(),
      viewport_css: DEFAULT_VIEWPORT,
      dpr: DEFAULT_DPR,
      hover_hint: HoverState::default(),
      last_hover_sent: None,
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
    let url_hash = match self.navigation.as_ref() {
      Some(IframeNavigation::Url(url)) => Self::url_hash(url),
      Some(IframeNavigation::AboutBlank) => Self::url_hash("about:blank"),
      Some(IframeNavigation::Srcdoc { content_hash }) => {
        let folded = (*content_hash as u32) ^ ((*content_hash >> 32) as u32);
        Self::url_hash("about:srcdoc") ^ folded
      }
      None => 0,
    };
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

fn hover_hint_from_html(base: &Url, html: &str) -> HoverState {
  // Very small heuristic hover/cursor inference for the placeholder renderer:
  // - Any <input> / <textarea> => text cursor
  // - Else first <a href> => pointer cursor + hovered_url
  //
  // In a real renderer this should use DOM/layout hit testing for the current pointer position.
  if !find_start_tags(html, "input").is_empty() || !find_start_tags(html, "textarea").is_empty() {
    return HoverState {
      hovered_url: None,
      cursor: CursorKind::Text,
    };
  }

  for tag in find_start_tags(html, "a") {
    let attrs = parse_tag_attributes(tag);
    let Some(href) = attrs.get("href") else {
      continue;
    };
    let href = trim_ascii_whitespace(href);
    if href.is_empty() {
      continue;
    }
    let resolved = Url::parse(href).ok().or_else(|| base.join(href).ok());
    if let Some(url) = resolved {
      return HoverState {
        hovered_url: Some(url.to_string()),
        cursor: CursorKind::Pointer,
      };
    }
  }

  HoverState::default()
}

fn should_fetch_url(url: &Url) -> bool {
  if !matches!(url.scheme(), "http" | "https") {
    // Keep the renderer loop deterministic/offline for now; unit tests use non-http URLs as
    // sentinel values. Multiprocess navigation/fetch integration tests use localhost HTTP(S).
    //
    // Note: this placeholder renderer does **not** implement TLS; `https://localhost` is treated
    // as a plain TCP/HTTP connection. This is acceptable for localhost-only tests that need to
    // exercise scheme-based policy logic (e.g. mixed-content checks).
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
  // Match browser behavior: only synthesize a `Referer` header for network referrers.
  if !matches!(referrer_url.scheme(), "http" | "https") {
    return None;
  }
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

fn http_get_raw(url: &Url, referer: Option<&str>) -> Result<Vec<u8>, String> {
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

  Ok(response)
}

#[derive(Debug, Clone)]
struct HttpResponse {
  headers: Vec<(String, String)>,
  body: Vec<u8>,
  final_url: Url,
}

fn parse_http_response(bytes: &[u8]) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
  let sep = bytes
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .ok_or_else(|| "invalid HTTP response (missing header separator)".to_string())?;

  let headers_raw = &bytes[..sep];
  let body = bytes[(sep + 4)..].to_vec();

  let headers_text = String::from_utf8_lossy(headers_raw);
  let mut lines = headers_text.lines();
  let status_line = lines
    .next()
    .ok_or_else(|| "invalid HTTP response (missing status line)".to_string())?;
  let status = status_line
    .split_whitespace()
    .nth(1)
    .ok_or_else(|| format!("invalid HTTP status line: {status_line:?}"))?
    .parse::<u16>()
    .map_err(|_| format!("invalid HTTP status line: {status_line:?}"))?;

  // Very small/forgiving HTTP header parser sufficient for tests and CSP propagation.
  let mut headers = Vec::<(String, String)>::new();
  for line in lines {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
      continue;
    }
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    headers.push((name.trim().to_string(), value.trim().to_string()));
  }

  Ok((status, headers, body))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
  headers
    .iter()
    .find(|(h, _)| h.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

fn is_redirect_status(status: u16) -> bool {
  matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn http_get(url: &Url, referer: Option<&str>) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
  let raw = http_get_raw(url, referer)?;
  parse_http_response(&raw)
}

fn http_get_follow_redirects(
  initial_url: &Url,
  raw_referrer: Option<&str>,
  policy: ReferrerPolicy,
) -> Result<HttpResponse, String> {
  const MAX_REDIRECTS: usize = 10;

  let mut url = initial_url.clone();
  for _ in 0..=MAX_REDIRECTS {
    let referer_header = raw_referrer.and_then(|raw| compute_referer_header_value(raw, &url, policy));
    let (status, headers, body) = http_get(&url, referer_header.as_deref())?;

    if is_redirect_status(status) {
      let Some(location) = header_value(&headers, "Location") else {
        return Err(format!("redirect {status} missing Location header"));
      };
      let location = trim_ascii_whitespace(location);
      if location.is_empty() {
        return Err(format!("redirect {status} missing Location header"));
      }
      let next_url = Url::parse(location)
        .ok()
        .or_else(|| url.join(location).ok())
        .ok_or_else(|| format!("invalid redirect Location: {location:?}"))?;
      // Keep this placeholder renderer deterministic/offline: do not follow redirects to
      // non-localhost destinations.
      if matches!(next_url.scheme(), "http" | "https")
        && !matches!(
          next_url.host_str(),
          Some("127.0.0.1") | Some("localhost") | Some("::1")
        )
      {
        return Err(format!("redirect to non-localhost URL blocked: {}", next_url.as_str()));
      }
      // For redirects to non-network schemes (`file:`, `about:`, `data:`), stop here and report the
      // final URL without attempting a fetch. This lets the browser-side policy/CSP enforcement
      // layer observe the real redirect destination.
      if !matches!(next_url.scheme(), "http" | "https") {
        return Ok(HttpResponse {
          headers: Vec::new(),
          body: Vec::new(),
          final_url: next_url,
        });
      }

      url = next_url;
      continue;
    }

    return Ok(HttpResponse {
      headers,
      body,
      final_url: url,
    });
  }

  Err("too many redirects".to_string())
}

fn csp_values_from_http_headers(headers: &[(String, String)]) -> Vec<String> {
  headers
    .iter()
    .filter_map(|(name, value)| {
      name
        .eq_ignore_ascii_case("Content-Security-Policy")
        .then(|| value.clone())
    })
    .collect()
}

fn csp_values_from_html_meta(html: &str) -> Vec<String> {
  let meta_tags = find_start_tags(html, "meta");
  let mut out = Vec::new();
  for tag in meta_tags {
    let attrs = parse_tag_attributes(tag);
    let Some(http_equiv) = attrs.get("http-equiv") else {
      continue;
    };
    if !http_equiv.eq_ignore_ascii_case("content-security-policy") {
      continue;
    }
    let Some(content) = attrs.get("content") else {
      continue;
    };
    if content.trim().is_empty() {
      continue;
    }
    out.push(content.clone());
  }
  out
}

fn find_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  if needle.is_empty() || haystack.len() < needle.len() {
    return None;
  }
  haystack
    .windows(needle.len())
    .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Extract the effective document base URL from `<base href>`, when present.
///
/// This is a minimal approximation of FastRender's in-process base URL tracker: it considers only
/// the first `<base href>` with a non-empty, non-fragment-only href found in the document head
/// (approximated here as the prefix before `</head>` or `<body>`). Later base tags are ignored.
fn base_url_from_html(document_url: &Url, html: &str) -> Option<String> {
  let bytes = html.as_bytes();
  let scan_end = find_case_insensitive(bytes, b"</head")
    .or_else(|| find_case_insensitive(bytes, b"<body"))
    .unwrap_or(bytes.len());
  let head_html = &html[..scan_end];

  let base_tags = find_start_tags(head_html, "base");
  for tag in base_tags {
    let attrs = parse_tag_attributes(tag);
    let Some(href_raw) = attrs.get("href") else {
      continue;
    };
    let href = trim_ascii_whitespace(href_raw);
    if href.is_empty() || href.starts_with('#') {
      continue;
    }

    let resolved = Url::parse(href).ok().or_else(|| document_url.join(href).ok());
    return resolved.map(|url| url.to_string());
  }

  None
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParsedTransform {
  Identity,
  /// Axis-aligned 2D transform (translate/scale), suitable for the MVP compositor.
  Supported(AffineTransform),
  /// Non-axis-aligned / otherwise unsupported transform (rotate/skew/3D/etc).
  Unsupported,
}

impl Default for ParsedTransform {
  fn default() -> Self {
    Self::Identity
  }
}

impl ParsedTransform {
  fn has_transform(self) -> bool {
    !matches!(self, Self::Identity)
  }
}

#[derive(Debug, Clone, Copy, Default)]
struct IframeInteractionStyle {
  pointer_events_none: bool,
  visibility_hidden: bool,
  /// Opacity group affecting the embedding point.
  has_opacity: bool,
  /// Non-normal mix-blend-mode affecting the embedding point.
  has_blend_mode: bool,
  /// Filters and/or masks affecting the embedding point.
  has_filters_or_masks: bool,
  transform: ParsedTransform,
}

fn is_ident_char(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn selector_mentions_iframe(selector: &str) -> bool {
  let lower = selector.to_ascii_lowercase();
  let bytes = lower.as_bytes();
  let needle = b"iframe";
  if needle.len() > bytes.len() {
    return false;
  }
  for i in 0..=bytes.len() - needle.len() {
    if !bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
      continue;
    }
    let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
    let after_i = i + needle.len();
    let after_ok = after_i == bytes.len() || !is_ident_char(bytes[after_i]);
    if before_ok && after_ok {
      return true;
    }
  }
  false
}

fn apply_style_declarations(style: &mut IframeInteractionStyle, decls: &str) {
  for decl in decls.split(';') {
    let Some((raw_name, raw_value)) = decl.split_once(':') else {
      continue;
    };
    let name = raw_name.trim().to_ascii_lowercase();
    let value = raw_value.trim();
    if name == "pointer-events" {
      let token = value.split_ascii_whitespace().next().unwrap_or("");
      style.pointer_events_none = token.eq_ignore_ascii_case("none");
    } else if name == "visibility" {
      let token = value.split_ascii_whitespace().next().unwrap_or("");
      style.visibility_hidden = token.eq_ignore_ascii_case("hidden");
    } else if name == "opacity" {
      style.has_opacity = match parse_css_number(value) {
        Some(v) => (v - 1.0).abs() > 1e-6,
        None => true,
      };
    } else if name == "mix-blend-mode" {
      let token = value.split_ascii_whitespace().next().unwrap_or("");
      style.has_blend_mode = !token.is_empty() && !token.eq_ignore_ascii_case("normal");
    } else if name == "filter" {
      style.has_filters_or_masks = !value.is_empty() && !value.eq_ignore_ascii_case("none");
    } else if name == "mask"
      || name.starts_with("mask-")
      || name == "-webkit-mask"
      || name.starts_with("-webkit-mask-")
    {
      style.has_filters_or_masks = !value.is_empty() && !value.eq_ignore_ascii_case("none");
    } else if name == "transform" {
      style.transform = parse_transform(value);
    }
  }
}

fn parse_css_number(raw: &str) -> Option<f32> {
  raw.trim().parse::<f32>().ok()
}

fn parse_css_length_px(raw: &str) -> Option<f32> {
  let raw = raw.trim();
  if raw.eq_ignore_ascii_case("0") || raw.eq_ignore_ascii_case("0.0") {
    return Some(0.0);
  }
  if let Some(px) = raw.strip_suffix("px") {
    return px.trim().parse::<f32>().ok();
  }
  // Be conservative: refuse to guess other units/percentages.
  if raw.ends_with('%') || raw.ends_with("em") || raw.ends_with("rem") {
    return None;
  }
  raw.parse::<f32>().ok()
}

fn split_transform_args(args: &str) -> Vec<&str> {
  args
    .split(|c: char| c == ',' || c.is_ascii_whitespace())
    .filter(|s| !s.is_empty())
    .collect()
}

fn affine_mul(a: AffineTransform, b: AffineTransform) -> AffineTransform {
  // 2D affine multiply for matrices in the form:
  //   [ a c e ]
  //   [ b d f ]
  //   [ 0 0 1 ]
  AffineTransform {
    a: a.a * b.a + a.c * b.b,
    b: a.b * b.a + a.d * b.b,
    c: a.a * b.c + a.c * b.d,
    d: a.b * b.c + a.d * b.d,
    e: a.a * b.e + a.c * b.f + a.e,
    f: a.b * b.e + a.d * b.f + a.f,
  }
}

fn parse_transform(raw: &str) -> ParsedTransform {
  let raw = raw.trim();
  if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
    return ParsedTransform::Identity;
  }

  let mut cursor = raw;
  let mut out = AffineTransform::IDENTITY;

  while !cursor.trim_start().is_empty() {
    cursor = cursor.trim_start();
    let Some(open_rel) = cursor.find('(') else {
      return ParsedTransform::Unsupported;
    };
    let name = cursor[..open_rel].trim().to_ascii_lowercase();
    let after_open = &cursor[open_rel + 1..];
    let Some(close_rel) = after_open.find(')') else {
      return ParsedTransform::Unsupported;
    };
    let args_raw = &after_open[..close_rel];
    cursor = &after_open[close_rel + 1..];

    let matrix = match name.as_str() {
      "translate" => {
        let args = split_transform_args(args_raw);
        if args.is_empty() || args.len() > 2 {
          return ParsedTransform::Unsupported;
        }
        let Some(tx) = parse_css_length_px(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        let ty = if args.len() == 2 {
          let Some(ty) = parse_css_length_px(args[1]) else {
            return ParsedTransform::Unsupported;
          };
          ty
        } else {
          0.0
        };
        AffineTransform {
          a: 1.0,
          b: 0.0,
          c: 0.0,
          d: 1.0,
          e: tx,
          f: ty,
        }
      }
      "translatex" => {
        let args = split_transform_args(args_raw);
        if args.len() != 1 {
          return ParsedTransform::Unsupported;
        }
        let Some(tx) = parse_css_length_px(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        AffineTransform {
          a: 1.0,
          b: 0.0,
          c: 0.0,
          d: 1.0,
          e: tx,
          f: 0.0,
        }
      }
      "translatey" => {
        let args = split_transform_args(args_raw);
        if args.len() != 1 {
          return ParsedTransform::Unsupported;
        }
        let Some(ty) = parse_css_length_px(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        AffineTransform {
          a: 1.0,
          b: 0.0,
          c: 0.0,
          d: 1.0,
          e: 0.0,
          f: ty,
        }
      }
      "scale" => {
        let args = split_transform_args(args_raw);
        if args.is_empty() || args.len() > 2 {
          return ParsedTransform::Unsupported;
        }
        let Some(sx) = parse_css_number(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        let sy = if args.len() == 2 {
          let Some(sy) = parse_css_number(args[1]) else {
            return ParsedTransform::Unsupported;
          };
          sy
        } else {
          sx
        };
        AffineTransform {
          a: sx,
          b: 0.0,
          c: 0.0,
          d: sy,
          e: 0.0,
          f: 0.0,
        }
      }
      "scalex" => {
        let args = split_transform_args(args_raw);
        if args.len() != 1 {
          return ParsedTransform::Unsupported;
        }
        let Some(sx) = parse_css_number(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        AffineTransform {
          a: sx,
          b: 0.0,
          c: 0.0,
          d: 1.0,
          e: 0.0,
          f: 0.0,
        }
      }
      "scaley" => {
        let args = split_transform_args(args_raw);
        if args.len() != 1 {
          return ParsedTransform::Unsupported;
        }
        let Some(sy) = parse_css_number(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        AffineTransform {
          a: 1.0,
          b: 0.0,
          c: 0.0,
          d: sy,
          e: 0.0,
          f: 0.0,
        }
      }
      "matrix" => {
        let args = split_transform_args(args_raw);
        if args.len() != 6 {
          return ParsedTransform::Unsupported;
        }
        let Some(a) = parse_css_number(args[0]) else {
          return ParsedTransform::Unsupported;
        };
        let Some(b) = parse_css_number(args[1]) else {
          return ParsedTransform::Unsupported;
        };
        let Some(c) = parse_css_number(args[2]) else {
          return ParsedTransform::Unsupported;
        };
        let Some(d) = parse_css_number(args[3]) else {
          return ParsedTransform::Unsupported;
        };
        let Some(e) = parse_css_number(args[4]) else {
          return ParsedTransform::Unsupported;
        };
        let Some(f) = parse_css_number(args[5]) else {
          return ParsedTransform::Unsupported;
        };

        // MVP compositor only supports axis-aligned matrices.
        const EPS: f32 = 1e-6;
        if b.abs() > EPS || c.abs() > EPS {
          return ParsedTransform::Unsupported;
        }

        AffineTransform {
          a,
          b: 0.0,
          c: 0.0,
          d,
          e,
          f,
        }
      }
      _ => return ParsedTransform::Unsupported,
    };

    out = affine_mul(out, matrix);
  }

  if !out.a.is_finite()
    || !out.d.is_finite()
    || !out.e.is_finite()
    || !out.f.is_finite()
    || out.a == 0.0
    || out.d == 0.0
  {
    return ParsedTransform::Unsupported;
  }

  // Treat an exact identity matrix as no transform so callers don't need to special-case.
  if out.a == 1.0 && out.b == 0.0 && out.c == 0.0 && out.d == 1.0 && out.e == 0.0 && out.f == 0.0 {
    ParsedTransform::Identity
  } else {
    ParsedTransform::Supported(out)
  }
}

fn parse_iframe_style_rules_from_stylesheet(style: &mut IframeInteractionStyle, css: &str) {
  // Extremely small CSS "parser" sufficient for tests:
  // - split into `{ ... }` rule blocks
  // - treat any selector containing the word `iframe` as applying to iframe elements
  let mut i = 0usize;
  while let Some(open_rel) = css[i..].find('{') {
    let open = i + open_rel;
    let selector = css[i..open].trim();
    let Some(close_rel) = css[open + 1..].find('}') else {
      break;
    };
    let close = open + 1 + close_rel;
    let block = &css[open + 1..close];
    if selector_mentions_iframe(selector) {
      apply_style_declarations(style, block);
    }
    i = close + 1;
  }
}

fn find_style_blocks<'a>(html: &'a str) -> Vec<&'a str> {
  let bytes = html.as_bytes();
  let open_pat = b"<style";
  let close_pat = b"</style>";
  let mut out = Vec::new();
  let mut i = 0usize;
  while i + open_pat.len() <= bytes.len() {
    if bytes[i] == b'<' && bytes[i..i + open_pat.len()].eq_ignore_ascii_case(open_pat) {
      let Some(rel_end) = bytes[i..].iter().position(|&b| b == b'>') else {
        break;
      };
      let content_start = i + rel_end + 1;
      let mut j = content_start;
      let mut found = None;
      while j + close_pat.len() <= bytes.len() {
        if bytes[j] == b'<' && bytes[j..j + close_pat.len()].eq_ignore_ascii_case(close_pat) {
          found = Some(j);
          break;
        }
        j += 1;
      }
      let Some(close_start) = found else {
        break;
      };
      out.push(&html[content_start..close_start]);
      i = close_start + close_pat.len();
      continue;
    }
    i += 1;
  }
  out
}

fn iframe_interaction_style_defaults_from_html(html: &str) -> IframeInteractionStyle {
  let mut style = IframeInteractionStyle::default();
  for css in find_style_blocks(html) {
    parse_iframe_style_rules_from_stylesheet(&mut style, css);
  }
  style
}

/// Best-effort iframe discovery from an HTML response body.
///
/// This is a development-only helper used by the `fastrender-renderer` placeholder implementation.
/// It is intentionally **not** a full HTML/CSS engine.
pub fn subframes_from_html(frame_id: FrameId, html: &str) -> Vec<SubframeInfo> {
  let defaults = iframe_interaction_style_defaults_from_html(html);
  let iframe_tags = find_start_tags(html, "iframe");
  iframe_tags
    .into_iter()
    .enumerate()
    .filter_map(|(idx, tag)| {
      let attrs = parse_tag_attributes(tag);
      let src = attrs
        .get("src")
        .map(|v| trim_ascii_whitespace(v).to_string())
        .filter(|v| !v.is_empty());
      let referrer_policy = attrs
        .get("referrerpolicy")
        .and_then(|v| ReferrerPolicy::from_attribute(v));
      let sandbox_present = attrs.contains_key("sandbox");
      let sandbox_flags = attrs
        .get("sandbox")
        .map(|v| parse_sandbox_flags(v))
        .unwrap_or(SandboxFlags::NONE);
      let opaque_origin = sandbox_present && !sandbox_flags.contains(SandboxFlags::ALLOW_SAME_ORIGIN);
      let inert = attrs.contains_key("inert");

      let mut style = defaults;
      if let Some(inline_style) = attrs.get("style") {
        apply_style_declarations(&mut style, inline_style);
      }
      let hit_testable = !style.pointer_events_none && !style.visibility_hidden && !inert;

      let effects = SubframeEffects {
        has_transform: style.transform.has_transform(),
        has_opacity: style.has_opacity,
        has_blend_mode: style.has_blend_mode,
        has_filters_or_masks: style.has_filters_or_masks,
      };

      // MVP compositor limitation: only axis-aligned translate/scale transforms are supported.
      // If any other stacking/effect state is present, fall back to inline rendering by omitting
      // the placement from the OOPIF subframe list.
      if effects.has_opacity || effects.has_blend_mode || effects.has_filters_or_masks {
        return None;
      }

      let transform = match style.transform {
        ParsedTransform::Identity => AffineTransform::IDENTITY,
        ParsedTransform::Supported(t) => t,
        ParsedTransform::Unsupported => return None,
      };

      Some(SubframeInfo {
        child: FrameId(frame_id.0.saturating_mul(1000).saturating_add(idx as u64 + 1)),
        src,
        transform,
        clip_stack: Vec::<ClipItem>::new(),
        z_index: idx as u64,
        hit_testable,
        referrer_policy,
        sandbox_flags,
        opaque_origin,
        effects,
      })
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
  site_lock: Option<SiteLock>,
}

impl<T: IpcTransport> RendererMainLoop<T> {
  pub fn new(transport: T) -> Self {
    Self {
      transport,
      frames: HashMap::new(),
      site_lock: None,
    }
  }

  pub fn run(mut self) -> Result<(), T::Error> {
    while let Some(msg) = self.transport.recv()? {
      match msg {
        BrowserToRenderer::SetSiteLock { lock } => {
          // Only allow setting the process lock once. The browser should send this during renderer
          // initialization; changing the lock would defeat the purpose of defense-in-depth.
          if self.site_lock.is_some() {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: None,
              message: "SetSiteLock received after lock already set".to_string(),
            });
            continue;
          }
          self.site_lock = Some(lock);
        }
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
          navigation,
          context,
        } => {
          let Some(frame) = self.frames.get_mut(&frame_id) else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "Navigate for unknown frame".to_string(),
            });
            continue;
          };

          if self
            .site_lock
            .as_ref()
            .is_some_and(|lock| !lock.matches_url(navigation.effective_url(), &context.site_key))
          {
            let url = navigation.effective_url().to_string();
            let _ = self.transport.send(RendererToBrowser::NavigationFailed {
              frame_id,
              url,
              error: "site lock violation".to_string(),
            });

            if cfg!(feature = "site_lock_violation_abort") {
              std::process::abort();
            }

            continue;
          }

          frame.navigation = Some(navigation);
          frame.navigation_context = context;
          frame.hover_hint = HoverState::default();
          frame.last_hover_sent = None;
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
          let Some(frame) = self.frames.get_mut(&frame_id) else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "RequestRepaint for unknown frame".to_string(),
            });
            continue;
          };

          let mut subframes = Vec::new();
          if let Some(IframeNavigation::Url(raw_url)) = frame.navigation.clone() {
            if let Ok(url) = Url::parse(&raw_url) {
              if should_fetch_url(&url) {
                match http_get_follow_redirects(
                  &url,
                  frame.navigation_context.referrer_url.as_deref(),
                  frame.navigation_context.referrer_policy,
                ) {
                  Ok(response) => {
                    // When the navigation redirects, commit the final URL so the browser can apply
                    // embedder policy/CSP checks against the real destination.
                    let committed_url = response.final_url.to_string();
                    frame.navigation = Some(IframeNavigation::Url(committed_url.clone()));

                    let html = String::from_utf8_lossy(&response.body);
                    frame.hover_hint = hover_hint_from_html(&response.final_url, html.as_ref());
                    subframes = subframes_from_html(frame_id, html.as_ref());

                    let mut csp_values = csp_values_from_http_headers(&response.headers);
                    csp_values.extend(csp_values_from_html_meta(html.as_ref()));
                    let base_url = base_url_from_html(&response.final_url, html.as_ref());
                    // Report the committed CSP values to the browser so it can enforce parent
                    // `frame-src` on out-of-process iframe navigations.
                    self.transport.send(RendererToBrowser::NavigationCommitted {
                      frame_id,
                      url: committed_url.clone(),
                      base_url,
                      csp: csp_values,
                    })?;

                    // Opportunistically fetch <img> subresources so integration tests can assert
                    // referrer policy behavior without implementing full layout/paint in the
                    // multiprocess renderer yet.
                    for img_url in image_urls_from_html(&response.final_url, html.as_ref()) {
                      let referer = compute_referer_header_value(
                        response.final_url.as_str(),
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
        BrowserToRenderer::PointerMove {
          frame_id,
          x_css,
          y_css,
          seq,
        } => {
          let Some(frame) = self.frames.get_mut(&frame_id) else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "PointerMove for unknown frame".to_string(),
            });
            continue;
          };

          // Ack input delivery. In the real renderer this would likely happen after event dispatch.
          let _ = self
            .transport
            .send(RendererToBrowser::InputAck { frame_id, seq });

          let (w, h) = frame.viewport_css;
          let inside = x_css.is_finite()
            && y_css.is_finite()
            && x_css >= 0.0
            && y_css >= 0.0
            && x_css < (w as f32)
            && y_css < (h as f32);

          let state = if inside {
            frame.hover_hint.clone()
          } else {
            HoverState::default()
          };

          if frame.last_hover_sent.as_ref() != Some(&state) {
            frame.last_hover_sent = Some(state.clone());
            let _ = self.transport.send(RendererToBrowser::HoverChanged {
              frame_id,
              seq,
              hovered_url: state.hovered_url.clone(),
              cursor: state.cursor,
            });
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
  use fastrender_ipc::{site_key_for_navigation, SiteIsolationMode};
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
        navigation: IframeNavigation::Url("https://example.test/".to_string()),
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
        navigation: IframeNavigation::Url("https://example.test/a".to_string()),
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

  #[test]
  fn reports_iframe_sandbox_flags_and_opaque_origin() {
    let html = r#"<!doctype html>
      <iframe sandbox></iframe>
      <iframe sandbox="allow-same-origin allow-scripts"></iframe>
    "#;

    let subframes = subframes_from_html(FrameId(1), html);
    assert_eq!(subframes.len(), 2);

    assert_eq!(subframes[0].sandbox_flags, SandboxFlags::NONE);
    assert!(
      subframes[0].opaque_origin,
      "sandbox without allow-same-origin should force opaque origin"
    );

    assert!(subframes[1]
      .sandbox_flags
      .contains(SandboxFlags::ALLOW_SAME_ORIGIN));
    assert!(subframes[1]
      .sandbox_flags
      .contains(SandboxFlags::ALLOW_SCRIPTS));
    assert!(
      !subframes[1].opaque_origin,
      "allow-same-origin should disable opaque-origin forcing"
    );
  }

  #[test]
  fn iframe_src_is_trimmed_like_html_url_attributes() {
    let html = r#"<!doctype html><iframe src="   /frame.html	 "></iframe>"#;
    let subframes = subframes_from_html(FrameId(1), html);
    assert_eq!(subframes.len(), 1);
    assert_eq!(subframes[0].src.as_deref(), Some("/frame.html"));
  }

  #[test]
  fn iframe_with_rotate_transform_is_not_reported_for_oopif_compositing() {
    let html = r#"<!doctype html><iframe style="transform: rotate(10deg)" src="https://child.test/"></iframe>"#;
    let subframes = subframes_from_html(FrameId(1), html);
    assert!(
      subframes.is_empty(),
      "non-axis-aligned transforms should fall back to inline rendering"
    );
  }

  #[test]
  fn iframe_with_translate_transform_reports_axis_aligned_matrix() {
    let html = r#"<!doctype html><iframe style="transform: translate(10px, 20px)" src="https://child.test/"></iframe>"#;
    let subframes = subframes_from_html(FrameId(1), html);
    assert_eq!(subframes.len(), 1);
    let t = subframes[0].transform;
    assert!(t.is_axis_aligned());
    assert_eq!(t.e, 10.0);
    assert_eq!(t.f, 20.0);
    assert!(
      subframes[0].effects.has_transform,
      "expected transform presence to be reflected in SubframeEffects"
    );
  }

  #[test]
  fn site_lock_rejects_cross_site_navigation() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let lock_site_key = site_key_for_navigation("https://a.test/", None);
    let lock = SiteLock::from_site_key(&lock_site_key, SiteIsolationMode::PerOrigin);
    to_renderer_tx
      .send(BrowserToRenderer::SetSiteLock { lock })
      .unwrap();

    let frame = FrameId(1);
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
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();
    let baseline = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
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

    let disallowed_url = "https://b.test/".to_string();

    // Simulate a buggy browser: it sends a cross-site URL but forgets to update `site_key`.
    to_renderer_tx
      .send(BrowserToRenderer::Navigate {
        frame_id: frame,
        navigation: IframeNavigation::Url(disallowed_url.clone()),
        context: NavigationContext {
          site_key: lock_site_key.clone(),
          ..Default::default()
        },
      })
      .unwrap();

    let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    match msg {
      RendererToBrowser::NavigationFailed { frame_id, url, error } => {
        assert_eq!(frame_id, frame);
        assert_eq!(url, disallowed_url);
        assert!(error.contains("site lock"));
      }
      other => panic!("expected NavigationFailed, got {other:?}"),
    }

    // Even when the browser reports the correct `site_key`, the renderer must still reject.
    let disallowed_site_key = site_key_for_navigation(&disallowed_url, None);
    to_renderer_tx
      .send(BrowserToRenderer::Navigate {
        frame_id: frame,
        navigation: IframeNavigation::Url(disallowed_url.clone()),
        context: NavigationContext {
          site_key: disallowed_site_key,
          ..Default::default()
        },
      })
      .unwrap();

    let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    match msg {
      RendererToBrowser::NavigationFailed { frame_id, url, error } => {
        assert_eq!(frame_id, frame);
        assert_eq!(url, disallowed_url);
        assert!(error.contains("site lock"));
      }
      other => panic!("expected NavigationFailed, got {other:?}"),
    }

    // Repaint should still succeed, but should not reflect the rejected URL.
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame })
      .unwrap();
    let after = match to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
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
      baseline.rgba8, after.rgba8,
      "rejected navigation must not affect render output"
    );

    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }
}
