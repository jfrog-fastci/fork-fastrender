use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use fastrender::network_process::ipc;
use fastrender::resource::{HttpFetcher, ResourceFetcher};
use std::io;
use std::net::{TcpListener, TcpStream};

const ENV_NETWORK_AUTH_TOKEN_RENDERER: &str = "FASTR_NETWORK_AUTH_TOKEN";
const ENV_NETWORK_AUTH_TOKEN_BROWSER: &str = "FASTR_NETWORK_AUTH_TOKEN_BROWSER";

fn handle_client(
  stream: TcpStream,
  fetcher: HttpFetcher,
  renderer_auth_token: &str,
  browser_auth_token: &str,
) -> io::Result<()> {
  stream.set_nodelay(true)?;
  let mut conn = ipc::NetworkService::new(stream);

  let hello: ipc::NetworkRequest = match conn.recv_request() {
    Ok(req) => req,
    Err(err) => {
      // If we cannot deserialize the request, just close the connection. This keeps the wire
      // protocol surface small and avoids leaking internal diagnostics across processes.
      return Err(err);
    }
  };

  let role = match hello {
    ipc::NetworkRequest::Hello { token, role } => {
      if token.len() > ipc::MAX_AUTH_TOKEN_BYTES {
        // Treat invalid tokens as authentication failure: close without sending a response.
        return Ok(());
      }

      // Token determines the effective role; the claimed role must match so untrusted clients cannot
      // escalate privileges by lying about their role.
      let expected_role = if token == browser_auth_token {
        ipc::ClientRole::Browser
      } else if token == renderer_auth_token {
        ipc::ClientRole::Renderer
      } else {
        // Wrong token: close the connection without sending a response.
        return Ok(());
      };
      if role != expected_role {
        return Ok(());
      }

      conn.send_response(&ipc::NetworkResponse::HelloAck)?;
      expected_role
    }
    _ => {
      // Protocol violation: first frame must be Hello.
      return Ok(());
    }
  };

  let req: ipc::NetworkRequest = match conn.recv_request() {
    Ok(req) => req,
    Err(err) => return Err(err),
  };

  // Cap individual download chunks so base64 + JSON stays comfortably under the global frame cap.
  const MAX_DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;

  match req {
    // Protocol violation: `Hello` must only be sent once at the start of the connection.
    ipc::NetworkRequest::Hello { .. } => {}

    ipc::NetworkRequest::Fetch { url } => {
      if url.len() > ipc::MAX_URL_BYTES {
        let _ = conn.send_response(&ipc::NetworkResponse::Error {
          error: ipc::NetworkError::InvalidRequest {
            message: format!(
              "url too long: {} bytes (max {})",
              url.len(),
              ipc::MAX_URL_BYTES
            ),
          },
        });
        return Ok(());
      }
      match fetcher.fetch(&url) {
        Ok(resource) => conn.send_response(&ipc::NetworkResponse::FetchOk {
          resource: ipc::IpcFetchedResource::from_fetched(resource),
        })?,
        Err(err) => conn.send_response(&ipc::NetworkResponse::Error {
          error: ipc::NetworkError::InvalidRequest {
            message: err.to_string(),
          },
        })?,
      }
    }

    ipc::NetworkRequest::DownloadStart { url } => {
      if role != ipc::ClientRole::Browser {
        let _ = conn.send_response(&ipc::NetworkResponse::Error {
          error: ipc::NetworkError::PermissionDenied,
        });
        return Ok(());
      }

      if url.len() > ipc::MAX_URL_BYTES {
        let _ = conn.send_response(&ipc::NetworkResponse::Error {
          error: ipc::NetworkError::InvalidRequest {
            message: format!(
              "url too long: {} bytes (max {})",
              url.len(),
              ipc::MAX_URL_BYTES
            ),
          },
        });
        return Ok(());
      }

      match fetcher.fetch(&url) {
        Ok(resource) => {
          // A single connection handles at most one download, so the ID can be constant.
          let download_id: u64 = 1;
          let total_bytes = u64::try_from(resource.bytes.len()).ok();
          conn.send_response(&ipc::NetworkResponse::DownloadStarted {
            download_id,
            total_bytes,
          })?;

          let bytes = resource.bytes;
          if bytes.is_empty() {
            conn.send_response(&ipc::NetworkResponse::DownloadChunk {
              download_id,
              finished: true,
              bytes_base64: String::new(),
            })?;
            return Ok(());
          }

          let mut offset: usize = 0;
          while offset < bytes.len() {
            let end = (offset + MAX_DOWNLOAD_CHUNK_BYTES).min(bytes.len());
            let chunk = &bytes[offset..end];
            offset = end;
            let finished = offset >= bytes.len();
            conn.send_response(&ipc::NetworkResponse::DownloadChunk {
              download_id,
              finished,
              bytes_base64: BASE64_STANDARD.encode(chunk),
            })?;
          }
        }
        Err(err) => {
          conn.send_response(&ipc::NetworkResponse::Error {
            error: ipc::NetworkError::InvalidRequest {
              message: err.to_string(),
            },
          })?;
        }
      }
    }

    ipc::NetworkRequest::Shutdown => {
      if role != ipc::ClientRole::Browser {
        let _ = conn.send_response(&ipc::NetworkResponse::Error {
          error: ipc::NetworkError::PermissionDenied,
        });
        return Ok(());
      }

      let _ = conn.send_response(&ipc::NetworkResponse::Ok);
      // Exit immediately; the parent process may also SIGKILL as a fallback.
      std::process::exit(0);
    }
  }

  Ok(())
}

fn main() -> io::Result<()> {
  // Minimal arg parser:
  // - Renderer token: `--auth-token <token>` / `FASTR_NETWORK_AUTH_TOKEN`
  // - Browser token: `--browser-auth-token <token>` / `FASTR_NETWORK_AUTH_TOKEN_BROWSER`
  let mut bind_addr = "127.0.0.1:0".to_string();
  let mut renderer_auth_token: Option<String> = None;
  let mut browser_auth_token: Option<String> = None;
  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--bind" => {
        bind_addr = args
          .next()
          .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--bind requires a value"))?;
      }
      "--auth-token" => {
        renderer_auth_token = Some(args.next().ok_or_else(|| {
          io::Error::new(io::ErrorKind::InvalidInput, "--auth-token requires a value")
        })?);
      }
      "--browser-auth-token" => {
        browser_auth_token = Some(args.next().ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "--browser-auth-token requires a value",
          )
        })?);
      }
      "--help" | "-h" => {
        eprintln!(
          "Usage: network [--bind <addr>] [--auth-token <token>] [--browser-auth-token <token>]"
        );
        return Ok(());
      }
      other => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("unknown arg: {other}"),
        ));
      }
    }
  }

  let renderer_auth_token = renderer_auth_token
    .or_else(|| std::env::var(ENV_NETWORK_AUTH_TOKEN_RENDERER).ok())
    .ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "missing --auth-token (or {ENV_NETWORK_AUTH_TOKEN_RENDERER})",
        ),
      )
    })?;
  if renderer_auth_token.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "renderer auth token is empty",
    ));
  }
  if renderer_auth_token.len() > ipc::MAX_AUTH_TOKEN_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "renderer auth token too large: {} bytes (max {})",
        renderer_auth_token.len(),
        ipc::MAX_AUTH_TOKEN_BYTES
      ),
    ));
  }

  let browser_auth_token = browser_auth_token
    .or_else(|| std::env::var(ENV_NETWORK_AUTH_TOKEN_BROWSER).ok())
    .ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "missing --browser-auth-token (or {ENV_NETWORK_AUTH_TOKEN_BROWSER})",
        ),
      )
    })?;
  if browser_auth_token.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "browser auth token is empty",
    ));
  }
  if browser_auth_token.len() > ipc::MAX_AUTH_TOKEN_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "browser auth token too large: {} bytes (max {})",
        browser_auth_token.len(),
        ipc::MAX_AUTH_TOKEN_BYTES
      ),
    ));
  }

  let listener = TcpListener::bind(&bind_addr)?;
  let addr = listener.local_addr()?;
  let fetcher = HttpFetcher::new();

  // Print the listening address as the startup handshake for `spawn_network_process`.
  println!("{addr}");
  use std::io::Write as _;
  let _ = std::io::stdout().flush();

  for conn in listener.incoming() {
    match conn {
      Ok(stream) => {
        let fetcher = fetcher.clone();
        let renderer_auth_token = renderer_auth_token.clone();
        let browser_auth_token = browser_auth_token.clone();
        std::thread::spawn(move || {
          let _ = handle_client(stream, fetcher, &renderer_auth_token, &browser_auth_token);
        });
      }
      Err(err) => {
        eprintln!("network accept error: {err}");
        break;
      }
    }
  }

  Ok(())
}
