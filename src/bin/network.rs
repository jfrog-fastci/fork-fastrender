use fastrender::network_process::ipc;
use fastrender::resource::{HttpFetcher, ResourceFetcher};
use std::io;
use std::net::{TcpListener, TcpStream};

fn handle_client(stream: TcpStream, fetcher: HttpFetcher, auth_token: &str) -> io::Result<()> {
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

  match hello {
    ipc::NetworkRequest::Hello { token } => {
      if token.len() > ipc::MAX_AUTH_TOKEN_BYTES || token != auth_token {
        // Wrong token: close the connection without sending a response.
        return Ok(());
      }
      conn.send_response(&ipc::NetworkResponse::HelloAck)?;
    }
    _ => {
      // Protocol violation: first frame must be Hello.
      return Ok(());
    }
  }

  let req: ipc::NetworkRequest = match conn.recv_request() {
    Ok(req) => req,
    Err(err) => return Err(err),
  };

  match req {
    // Protocol violation: `Hello` must only be sent once at the start of the connection.
    ipc::NetworkRequest::Hello { .. } => {}
    ipc::NetworkRequest::Fetch { url } => {
      if url.len() > ipc::MAX_URL_BYTES {
        let _ = conn.send_response(
          &ipc::NetworkResponse::Error {
            message: format!("url too long: {} bytes (max {})", url.len(), ipc::MAX_URL_BYTES),
          },
        );
        return Ok(());
      }
      match fetcher.fetch(&url) {
        Ok(resource) => conn.send_response(
          &ipc::NetworkResponse::FetchOk {
            resource: ipc::IpcFetchedResource::from_fetched(resource),
          },
        )?,
        Err(err) => conn.send_response(
          &ipc::NetworkResponse::Error {
            message: err.to_string(),
          },
        )?,
      }
    }
    ipc::NetworkRequest::Shutdown => {
      let _ = conn.send_response(&ipc::NetworkResponse::Ok);
      // Exit immediately; the parent process may also SIGKILL as a fallback.
      std::process::exit(0);
    }
  }

  Ok(())
}

fn main() -> io::Result<()> {
  // Minimal arg parser: `network --bind 127.0.0.1:0 --auth-token <token>`
  let mut bind_addr = "127.0.0.1:0".to_string();
  let mut auth_token: Option<String> = None;
  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--bind" => {
        bind_addr = args
          .next()
          .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--bind requires a value"))?;
      }
      "--auth-token" => {
        auth_token = Some(args.next().ok_or_else(|| {
          io::Error::new(io::ErrorKind::InvalidInput, "--auth-token requires a value")
        })?);
      }
      "--help" | "-h" => {
        eprintln!("Usage: network [--bind <addr>] [--auth-token <token>]");
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

  let auth_token = auth_token
    .or_else(|| std::env::var("FASTR_NETWORK_AUTH_TOKEN").ok())
    .ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "missing --auth-token (or FASTR_NETWORK_AUTH_TOKEN)",
      )
    })?;
  if auth_token.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "auth token is empty",
    ));
  }
  if auth_token.len() > ipc::MAX_AUTH_TOKEN_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "auth token too large: {} bytes (max {})",
        auth_token.len(),
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
        let auth_token = auth_token.clone();
        std::thread::spawn(move || {
          let _ = handle_client(stream, fetcher, &auth_token);
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
