//! Standalone "network process" binary used by multiprocess integration tests.
//!
//! This process speaks the IPC protocol implemented by [`fastrender::IpcResourceFetcher`] (see
//! `src/resource/ipc_fetcher.rs`) over a TCP socket and dispatches requests to an in-process
//! [`fastrender::resource::HttpFetcher`].
//!
//! Note: the renderer-side proxy (`IpcResourceFetcher`) does **not** implement CORS enforcement;
//! instead, the network process must enforce it before returning response bytes. `HttpFetcher`
//! provides this enforcement based on the `FetchRequest` metadata sent across IPC.

use fastrender::ipc::IpcFetchServer;
use fastrender::resource::ipc_fetcher::{IPC_AUTH_TOKEN_ENV, IPC_MAX_AUTH_TOKEN_BYTES};
use fastrender::resource::HttpFetcher;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn handle_client(stream: TcpStream, fetcher: HttpFetcher, auth_token: &str) -> io::Result<()> {
  let _ = stream.set_nodelay(true);
  let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
  let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

  let mut server = IpcFetchServer::new(fetcher, auth_token)?;
  let reader = stream.try_clone()?;
  server.run(reader, stream)
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
