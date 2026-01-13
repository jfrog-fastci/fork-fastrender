use fastrender::network_process::ipc;
use fastrender::resource::{HttpFetcher, ResourceFetcher};
use std::io;
use std::net::{TcpListener, TcpStream};

fn handle_client(mut stream: TcpStream) -> io::Result<()> {
  stream.set_nodelay(true)?;

  let req: ipc::NetworkRequest = match ipc::read_frame(&mut stream) {
    Ok(req) => req,
    Err(err) => {
      // If we cannot deserialize the request, just close the connection. This keeps the wire
      // protocol surface small and avoids leaking internal diagnostics across processes.
      return Err(err);
    }
  };

  match req {
    ipc::NetworkRequest::Fetch { url } => {
      let fetcher = HttpFetcher::new();
      match fetcher.fetch(&url) {
        Ok(resource) => ipc::write_frame(
          &mut stream,
          &ipc::NetworkResponse::FetchOk {
            resource: ipc::IpcFetchedResource::from_fetched(resource),
          },
        )?,
        Err(err) => ipc::write_frame(
          &mut stream,
          &ipc::NetworkResponse::Error {
            message: err.to_string(),
          },
        )?,
      }
    }
    ipc::NetworkRequest::Shutdown => {
      let _ = ipc::write_frame(&mut stream, &ipc::NetworkResponse::Ok);
      // Exit immediately; the parent process may also SIGKILL as a fallback.
      std::process::exit(0);
    }
  }

  Ok(())
}

fn main() -> io::Result<()> {
  // Minimal arg parser: `network --bind 127.0.0.1:0`
  let mut bind_addr = "127.0.0.1:0".to_string();
  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--bind" => {
        bind_addr = args
          .next()
          .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--bind requires a value"))?;
      }
      "--help" | "-h" => {
        eprintln!("Usage: network [--bind <addr>]");
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

  let listener = TcpListener::bind(&bind_addr)?;
  let addr = listener.local_addr()?;

  // Print the listening address as the startup handshake for `spawn_network_process`.
  println!("{addr}");
  use std::io::Write as _;
  let _ = std::io::stdout().flush();

  for conn in listener.incoming() {
    match conn {
      Ok(stream) => {
        std::thread::spawn(move || {
          let _ = handle_client(stream);
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

