use super::ResourceFetcher;
use http::header::{HeaderValue, COOKIE, SET_COOKIE};
use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::client::Response as ClientResponse;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;
use url::Url;

pub type ClientSocket = WebSocket<MaybeTlsStream<TcpStream>>;

#[cfg(not(test))]
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

fn rustls_client_config() -> Arc<rustls::ClientConfig> {
  static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
  Arc::clone(CONFIG.get_or_init(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
      rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
  }))
}

struct DnsLookupRequest {
  host: String,
  port: u16,
  resp: mpsc::Sender<io::Result<Vec<SocketAddr>>>,
}

const MAX_QUEUED_DNS_LOOKUPS: usize = 128;

static DNS_LOOKUP_TX: OnceLock<mpsc::SyncSender<DnsLookupRequest>> = OnceLock::new();

fn dns_lookup_tx() -> mpsc::SyncSender<DnsLookupRequest> {
  DNS_LOOKUP_TX
    .get_or_init(|| {
      // DNS resolution can block inside system resolvers. Run it on a dedicated worker thread so
      // a compromised renderer/network client cannot spawn unbounded threads by issuing many
      // concurrent connects.
      let (tx, rx) = mpsc::sync_channel::<DnsLookupRequest>(MAX_QUEUED_DNS_LOOKUPS);
      // Best-effort: if thread creation fails (resource exhaustion), the receiver is dropped and
      // sends will fail with `Disconnected`, which we surface as an error instead of panicking.
      let _ = std::thread::Builder::new()
        .name("ws-dns-resolve".to_string())
        .spawn(move || {
          while let Ok(req) = rx.recv() {
            let res = (req.host.as_str(), req.port)
              .to_socket_addrs()
              .map(|iter| iter.collect::<Vec<_>>());
            let _ = req.resp.send(res);
          }
        });
      tx
    })
    .clone()
}

fn resolve_socket_addrs_with_timeout(
  host: &str,
  port: u16,
  timeout: Duration,
) -> tungstenite::Result<Vec<SocketAddr>> {
  if let Ok(ip) = host.parse::<IpAddr>() {
    return Ok(vec![SocketAddr::new(ip, port)]);
  }

  let (tx, rx) = mpsc::channel::<io::Result<Vec<SocketAddr>>>();
  match dns_lookup_tx().try_send(DnsLookupRequest {
    host: host.to_string(),
    port,
    resp: tx,
  }) {
    Ok(()) => {}
    Err(mpsc::TrySendError::Full(_)) => {
      return Err(tungstenite::Error::Io(io::Error::new(
        io::ErrorKind::Other,
        "WebSocket DNS resolution queue is full",
      )))
    }
    Err(mpsc::TrySendError::Disconnected(_)) => {
      return Err(tungstenite::Error::Io(io::Error::new(
        io::ErrorKind::Other,
        "WebSocket DNS resolver is unavailable",
      )))
    }
  }

  match rx.recv_timeout(timeout) {
    Ok(Ok(addrs)) => {
      if addrs.is_empty() {
        Err(tungstenite::Error::Io(io::Error::new(
          io::ErrorKind::NotFound,
          "WebSocket host did not resolve to any addresses",
        )))
      } else {
        Ok(addrs)
      }
    }
    Ok(Err(err)) => Err(tungstenite::Error::Io(err)),
    Err(mpsc::RecvTimeoutError::Timeout) => Err(tungstenite::Error::Io(io::Error::new(
      io::ErrorKind::TimedOut,
      "WebSocket DNS resolution timed out",
    ))),
    Err(mpsc::RecvTimeoutError::Disconnected) => Err(tungstenite::Error::Io(io::Error::new(
      io::ErrorKind::Other,
      "WebSocket DNS resolution thread disconnected",
    ))),
  }
}

pub(crate) fn connect_with_timeout(
  url: &Url,
  request: tungstenite::handshake::client::Request,
  timeout: Duration,
) -> tungstenite::Result<(ClientSocket, ClientResponse)> {
  let host = url.host_str().ok_or_else(|| {
    tungstenite::Error::Io(io::Error::new(
      io::ErrorKind::InvalidInput,
      "WebSocket URL missing host",
    ))
  })?;

  let scheme = url.scheme();
  let tls = matches!(scheme, "wss" | "https");
  let port = url
    .port_or_known_default()
    .or_else(|| if tls { Some(443) } else { Some(80) })
    .ok_or_else(|| {
      tungstenite::Error::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        "WebSocket URL missing port",
      ))
    })?;

  let deadline = Instant::now() + timeout;
  let mut last_err: Option<io::Error> = None;

  let remaining = deadline.saturating_duration_since(Instant::now());
  if remaining.is_zero() {
    return Err(tungstenite::Error::Io(io::Error::new(
      io::ErrorKind::TimedOut,
      "WebSocket connect timed out",
    )));
  }

  let addrs = resolve_socket_addrs_with_timeout(host, port, remaining)?;

  for addr in addrs {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      break;
    }

    match TcpStream::connect_timeout(&addr, remaining) {
      Ok(stream) => {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
          // `TcpStream::set_{read,write}_timeout(Some(Duration::ZERO))` is invalid and would leave
          // the socket in a potentially-blocking state, defeating the goal of bounded teardown.
          return Err(tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "WebSocket handshake timed out",
          )));
        }
        // Apply the same wall-clock budget to the tungstenite HTTP handshake and the rustls TLS
        // handshake (wss://) so renderer teardown cannot hang joining threads blocked on network I/O.
        stream
          .set_read_timeout(Some(remaining))
          .map_err(tungstenite::Error::Io)?;
        stream
          .set_write_timeout(Some(remaining))
          .map_err(tungstenite::Error::Io)?;

        let stream = if tls {
          // `ClientConnection` requires an owned `'static` server name; allocate because `host` is
          // borrowed from the URL.
          let server_name: rustls::pki_types::ServerName<'static> =
            host.to_string().try_into().map_err(|_| {
              tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebSocket host is not a valid TLS server name",
              ))
            })?;
          let conn = rustls::ClientConnection::new(rustls_client_config(), server_name)
            .map_err(|err| tungstenite::Error::Io(io::Error::new(io::ErrorKind::Other, err)))?;
          let tls_stream = rustls::StreamOwned::new(conn, stream);
          MaybeTlsStream::Rustls(tls_stream)
        } else {
          MaybeTlsStream::Plain(stream)
        };

        return tungstenite::client::client(request, stream).map_err(|err| match err {
          tungstenite::handshake::HandshakeError::Failure(err) => err,
          tungstenite::handshake::HandshakeError::Interrupted(_) => tungstenite::Error::Io(
            io::Error::new(io::ErrorKind::TimedOut, "WebSocket handshake timed out"),
          ),
        });
      }
      Err(err) => last_err = Some(err),
    }
  }

  let err = last_err
    .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "WebSocket connect timed out"));
  Err(tungstenite::Error::Io(err))
}

fn cookie_url_for_ws_url(ws_url: &Url) -> Option<Url> {
  let mut cookie_url = ws_url.clone();
  cookie_url.set_fragment(None);
  match ws_url.scheme() {
    "ws" => {
      cookie_url.set_scheme("http").ok()?;
    }
    "wss" => {
      cookie_url.set_scheme("https").ok()?;
    }
    "http" | "https" => {}
    _ => return None,
  }
  Some(cookie_url)
}

/// Perform a client WebSocket handshake, integrating with the provided `fetcher`'s cookie jar.
///
/// The handshake behaves like a browser:
/// - Adds a `Cookie` header based on the fetcher's stored cookies (when available).
/// - Persists any `Set-Cookie` headers returned in the handshake response.
///
/// `ws_url` must use `ws:`/`wss:` (or `http:`/`https:` which are treated as their WS equivalents).
pub(crate) fn connect_websocket_with_cookies(
  fetcher: &dyn ResourceFetcher,
  ws_url: &str,
  protocols_header: Option<&str>,
) -> tungstenite::Result<(ClientSocket, ClientResponse)> {
  // Treat renderer-supplied WebSocket URLs as untrusted: validate and canonicalize here even if the
  // renderer performed its own resolution/validation.
  let parsed = crate::ipc::websocket::validate_and_normalize_url(ws_url).map_err(
    |err: crate::ipc::websocket::WebSocketValidationError| {
      tungstenite::Error::Io(io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))
    },
  )?;
  let cookie_url = cookie_url_for_ws_url(&parsed);

  let mut request = parsed.clone().into_client_request()?;

  if let Some(cookie_url) = cookie_url.as_ref() {
    if let Some(cookie_header_value) = fetcher.cookie_header_value(cookie_url.as_str()) {
      if !cookie_header_value.is_empty() {
        if let Ok(header_value) = HeaderValue::from_str(&cookie_header_value) {
          request.headers_mut().insert(COOKIE, header_value);
        }
      }
    }
  }

  if let Some(header) = protocols_header {
    if let Ok(value) = HeaderValue::from_str(header) {
      request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", value);
    }
  }

  let (socket, response) = connect_with_timeout(&parsed, request, WS_CONNECT_TIMEOUT)?;

  if let Some(cookie_url) = cookie_url.as_ref() {
    for value in response.headers().get_all(SET_COOKIE) {
      if let Ok(raw) = value.to_str() {
        fetcher.store_cookie_from_document(cookie_url.as_str(), raw);
      }
    }
  }

  Ok((socket, response))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::HttpFetcher;
  use crate::resource::ResourceFetcher;
  use std::collections::HashMap;
  use std::net::TcpListener;
  use std::time::{Duration, Instant};
  use tungstenite::handshake::server::{Request, Response};

  fn parse_cookie_header(header: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in header.split(';') {
      let part = part.trim();
      if part.is_empty() {
        continue;
      }
      let Some((name, value)) = part.split_once('=') else {
        continue;
      };
      out.insert(name.trim().to_string(), value.trim().to_string());
    }
    out
  }

  fn accept_with_deadline(listener: &TcpListener, deadline: Instant) -> std::io::Result<TcpStream> {
    use std::io::ErrorKind;

    loop {
      match listener.accept() {
        Ok((stream, _)) => return Ok(stream),
        Err(err) if err.kind() == ErrorKind::WouldBlock => {
          if Instant::now() >= deadline {
            return Err(std::io::Error::new(ErrorKind::TimedOut, "accept timed out"));
          }
          std::thread::sleep(Duration::from_millis(10));
        }
        Err(err) => return Err(err),
      }
    }
  }

  #[test]
  fn websocket_handshake_integrates_cookie_jar() {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
      // Some sandboxed CI environments may forbid binding sockets; skip in that case.
      return;
    };
    listener
      .set_nonblocking(true)
      .expect("set listener nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
      // Handle two sequential handshakes so we can assert cookies persist across connections.
      for attempt in 0..2 {
        let stream = accept_with_deadline(&listener, Instant::now() + Duration::from_secs(5))
          .expect("accept stream");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

        let expected = match attempt {
          0 => vec![("a", "1")],
          _ => vec![("a", "1"), ("b", "2")],
        };

        let mut ws = tungstenite::accept_hdr(stream, |req: &Request, mut res: Response| {
          let raw = req
            .headers()
            .get(COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
          let cookies = parse_cookie_header(raw);
          for (name, value) in &expected {
            assert_eq!(
              cookies.get(*name).map(String::as_str),
              Some(*value),
              "attempt {attempt}: missing cookie {name}={value} (got {raw:?})"
            );
          }

          if attempt == 0 {
            res
              .headers_mut()
              .append(SET_COOKIE, HeaderValue::from_static("b=2; Path=/"));
          }
          Ok(res)
        })
        .expect("accept websocket");
        let _ = ws.close(None);
      }
    });

    let fetcher = HttpFetcher::new();
    let http_url = format!("http://{addr}/");
    fetcher.store_cookie_from_document(&http_url, "a=1; Path=/");
    let ws_url = format!("ws://{addr}/");

    {
      let (mut sock, _res) =
        connect_websocket_with_cookies(&fetcher, &ws_url, None).expect("connect");
      let _ = sock.close(None);
    }

    let cookie_header = fetcher
      .cookie_header_value(&http_url)
      .expect("cookie jar should be observable");
    let cookies = parse_cookie_header(&cookie_header);
    assert_eq!(cookies.get("a").map(String::as_str), Some("1"));
    assert_eq!(
      cookies.get("b").map(String::as_str),
      Some("2"),
      "expected Set-Cookie from handshake to persist"
    );

    {
      let (mut sock, _res) =
        connect_websocket_with_cookies(&fetcher, &ws_url, None).expect("reconnect");
      let _ = sock.close(None);
    }

    server.join().expect("server thread panicked");
  }
}
