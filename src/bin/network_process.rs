//! Standalone "network process" binary used by multiprocess integration tests.
//!
//! This process speaks the IPC protocol implemented by [`fastrender::IpcResourceFetcher`] (see
//! `src/resource/ipc_fetcher.rs`) over a TCP socket and dispatches requests to an in-process
//! [`fastrender::resource::HttpFetcher`].
//!
//! Note: the renderer-side proxy (`IpcResourceFetcher`) does **not** implement CORS enforcement;
//! instead, the network process must enforce it before returning response bytes. `HttpFetcher`
//! provides this enforcement based on the `FetchRequest` metadata sent across IPC.

#[cfg(feature = "renderer_tools")]
fn main() {
  eprintln!(
    "network_process is disabled when built with the `renderer_tools` feature (IPC is unavailable)."
  );
  eprintln!("Rebuild without `renderer_tools` to use this tool.");
  std::process::exit(2);
}

#[cfg(not(feature = "renderer_tools"))]
include!("_real/network_process.rs");
