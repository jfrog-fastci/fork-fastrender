//! Standalone "network process" binary used by multiprocess integration tests.
//!
//! This process speaks the IPC protocol implemented by [`fastrender::IpcResourceFetcher`] (see
//! `src/resource/ipc_fetcher.rs`) over a TCP socket. The renderer side (`IpcResourceFetcher`) does
//! **not** implement CORS enforcement; instead, the network process must enforce it before returning
//! response bytes.

use fastrender::resource::ipc_fetcher::{
  validate_ipc_request, BrowserToNetwork, IpcRequest, IpcResponse, NetworkService, IPC_AUTH_TOKEN_ENV,
  IPC_MAX_AUTH_TOKEN_BYTES, IPC_MAX_INBOUND_FRAME_BYTES, IPC_MAX_OUTBOUND_FRAME_BYTES,
};
use fastrender::resource::{HttpFetcher, HttpRequest, ResourceFetcher};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

const IPC_FRAME_LEN_BYTES: usize = 4;

fn read_frame(stream: &mut TcpStream, max_frame_bytes: usize) -> io::Result<Vec<u8>> {
  let mut len_buf = [0u8; IPC_FRAME_LEN_BYTES];
  stream.read_exact(&mut len_buf)?;
  let len = u32::from_le_bytes(len_buf) as usize;
  if len == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "IPC frame declared length is zero",
    ));
  }
  if len > max_frame_bytes {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame too large: {len} bytes (max {max_frame_bytes})"),
    ));
  }
  let mut buf = Vec::new();
  buf.try_reserve_exact(len).map_err(|err| {
    io::Error::new(
      io::ErrorKind::Other,
      format!("IPC frame allocation failed (len={len}): {err:?}"),
    )
  })?;
  buf.resize(len, 0);
  stream.read_exact(&mut buf)?;
  Ok(buf)
}

fn write_frame(stream: &mut TcpStream, payload: &[u8], max_frame_bytes: usize) -> io::Result<()> {
  if payload.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "IPC frame payload cannot be empty",
    ));
  }
  if payload.len() > max_frame_bytes {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "IPC frame too large: {} bytes (max {max_frame_bytes})",
        payload.len()
      ),
    ));
  }
  if payload.len() > u32::MAX as usize {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, "IPC frame too large"));
  }
  let len = (payload.len() as u32).to_le_bytes();
  stream.write_all(&len)?;
  stream.write_all(payload)?;
  stream.flush()?;
  Ok(())
}

fn handle_client(mut stream: TcpStream, fetcher: HttpFetcher, auth_token: &str) -> io::Result<()> {
  let _ = stream.set_nodelay(true);
  let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
  let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

  let hello_bytes = read_frame(&mut stream, IPC_MAX_INBOUND_FRAME_BYTES)?;
  let hello: IpcRequest = serde_json::from_slice(&hello_bytes)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
  match hello {
    IpcRequest::Hello { token } => {
      if token.len() > IPC_MAX_AUTH_TOKEN_BYTES || token != auth_token {
        // Wrong token: close the connection without sending a response.
        return Ok(());
      }
    }
    _ => {
      // Protocol violation: first request must be Hello.
      return Ok(());
    }
  }

  let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
  write_frame(&mut stream, &hello_ack, IPC_MAX_OUTBOUND_FRAME_BYTES)?;

  loop {
    let req_bytes = match read_frame(&mut stream, IPC_MAX_INBOUND_FRAME_BYTES) {
      Ok(bytes) => bytes,
      Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
      Err(err) => return Err(err),
    };
    let env: BrowserToNetwork = serde_json::from_slice(&req_bytes)
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    // Treat validation errors as protocol violations and close the connection.
    validate_ipc_request(&env.request).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let mut service = NetworkService::new(&mut stream);
    match env.request {
      IpcRequest::Hello { .. } => break,
      IpcRequest::Fetch { url } => {
        service.send_fetch_result(env.id, fetcher.fetch(&url))?;
      }
      IpcRequest::FetchWithRequest { req } => {
        let fetch_req = req.as_fetch_request();
        service.send_fetch_result(env.id, fetcher.fetch_with_request(fetch_req))?;
      }
      IpcRequest::FetchWithRequestAndValidation {
        req,
        etag,
        last_modified,
      } => {
        let fetch_req = req.as_fetch_request();
        service.send_fetch_result(
          env.id,
          fetcher.fetch_with_request_and_validation(
            fetch_req,
            etag.as_deref(),
            last_modified.as_deref(),
          ),
        )?;
      }
      IpcRequest::FetchHttpRequest { req } => {
        let body = match req.decode_body() {
          Ok(body) => body,
          Err(msg) => {
            let err = fastrender::Error::Other(msg);
            service.send_fetch_result(env.id, Err(err))?;
            continue;
          }
        };
        let fetch = req.fetch.as_fetch_request();
        let http_req = HttpRequest {
          fetch,
          method: &req.method,
          redirect: req.redirect,
          headers: &req.headers,
          body: body.as_deref(),
        };
        service.send_fetch_result(env.id, fetcher.fetch_http_request(http_req))?;
      }
      IpcRequest::FetchPartialWithContext { kind, url, max_bytes } => {
        let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        service.send_fetch_result(env.id, fetcher.fetch_partial_with_context(kind, &url, max_bytes))?;
      }
      IpcRequest::FetchPartialWithRequest { req, max_bytes } => {
        let fetch_req = req.as_fetch_request();
        let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        service.send_fetch_result(env.id, fetcher.fetch_partial_with_request(fetch_req, max_bytes))?;
      }
      // Only the subset needed by our integration tests is implemented.
      other => {
        let response = IpcResponse::Unit(fastrender::resource::ipc_fetcher::IpcResult::Err(
          fastrender::resource::ipc_fetcher::IpcError::from(fastrender::Error::Other(format!(
            "unimplemented IPC request in network_process: {other:?}"
          ))),
        ));
        service.send_response(env.id, response)?;
      }
    }
  }

  Ok(())
}

fn main() -> io::Result<()> {
  // Minimal arg parser: `network_process --bind 127.0.0.1:0 --auth-token <token>`
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
        eprintln!("Usage: network_process [--bind <addr>] [--auth-token <token>]");
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
    .or_else(|| std::env::var(IPC_AUTH_TOKEN_ENV).ok())
    .ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("missing --auth-token (or {IPC_AUTH_TOKEN_ENV})"),
      )
    })?;
  if auth_token.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "auth token is empty",
    ));
  }
  if auth_token.len() > IPC_MAX_AUTH_TOKEN_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "auth token too large: {} bytes (max {})",
        auth_token.len(),
        IPC_MAX_AUTH_TOKEN_BYTES
      ),
    ));
  }

  let listener = TcpListener::bind(&bind_addr)?;
  let addr = listener.local_addr()?;
  // Keep test runs deterministic: fail quickly if something goes wrong.
  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));

  // Print the listening address as the startup handshake for the test harness.
  println!("{addr}");
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
        eprintln!("network_process accept error: {err}");
        break;
      }
    }
  }

  Ok(())
}
