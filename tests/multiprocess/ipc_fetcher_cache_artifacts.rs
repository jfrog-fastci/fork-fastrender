use crate::common::net::{net_test_lock, try_bind_localhost};
use base64::Engine as _;
use fastrender::resource::ipc_fetcher::{
  validate_ipc_request, BrowserToNetwork, IpcRequest, IpcResponse, IpcResult, NetworkService,
};
use fastrender::resource::{CacheArtifactKind, FetchDestination, FetchRequest, FetchedResource};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const TEST_AUTH_TOKEN: &str = "fastrender-ipc-test-token";

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
  let len = (payload.len() as u32).to_le_bytes();
  stream.write_all(&len)?;
  stream.write_all(payload)?;
  stream.flush()?;
  Ok(())
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
  let mut len_buf = [0u8; 4];
  stream.read_exact(&mut len_buf)?;
  let len = u32::from_le_bytes(len_buf) as usize;
  let mut buf = vec![0u8; len];
  stream.read_exact(&mut buf)?;
  Ok(buf)
}

#[derive(Debug, Clone)]
struct StoredArtifact {
  bytes: Vec<u8>,
  source: Option<FetchedResource>,
}

fn resolve_alias(aliases: &HashMap<String, String>, url: &str) -> String {
  const MAX_HOPS: usize = 32;
  let mut current = url.to_string();
  for _ in 0..MAX_HOPS {
    let Some(next) = aliases.get(&current) else {
      break;
    };
    if next == &current {
      break;
    }
    current = next.clone();
  }
  current
}

fn spawn_ipc_server(listener: TcpListener) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let (mut stream, _) = listener.accept().expect("accept ipc client");
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    stream
      .set_write_timeout(Some(Duration::from_secs(5)))
      .unwrap();

    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).expect("read ipc hello frame");
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).expect("decode ipc hello request");
    validate_ipc_request(&hello).expect("validate ipc hello request");
    match hello {
      IpcRequest::Hello { token } => {
        assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token");
      }
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).expect("encode ipc hello ack");
    write_frame(&mut stream, &hello_ack).expect("write ipc hello ack");

    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut artifacts: HashMap<(String, CacheArtifactKind), StoredArtifact> = HashMap::new();

    loop {
      let req_bytes = match read_frame(&mut stream) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
        Err(err) => panic!("read ipc frame: {err}"),
      };
      let env: BrowserToNetwork =
        serde_json::from_slice(&req_bytes).expect("decode ipc request envelope");
      validate_ipc_request(&env.request).expect("validate ipc request");

      let response = match env.request {
        IpcRequest::WriteCacheArtifactWithRequest {
          req,
          artifact,
          bytes_b64,
          source,
        } => {
          assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
          assert_eq!(req.url, "https://example.test/image.png");

          let bytes = base64::engine::general_purpose::STANDARD
            .decode(bytes_b64.as_bytes())
            .expect("decode artifact body");

          let mut source_resource = source.map(|meta| {
            assert_eq!(
              meta.final_url.as_deref(),
              Some("https://cdn.example.test/image.png"),
              "final_url should be sent across IPC for artifact persistence"
            );
            assert_eq!(
              meta.access_control_allow_origin.as_deref(),
              Some("*"),
              "CORS metadata should be sent across IPC for artifact persistence"
            );
            let mut out = FetchedResource::new(Vec::new(), None);
            out.status = meta.status;
            out.nosniff = meta.nosniff;
            out.etag = meta.etag;
            out.last_modified = meta.last_modified;
            out.access_control_allow_origin = meta.access_control_allow_origin;
            out.timing_allow_origin = meta.timing_allow_origin;
            out.vary = meta.vary;
            out.access_control_allow_credentials = meta.access_control_allow_credentials;
            out.final_url = meta.final_url;
            out.cache_policy = meta.cache_policy.map(Into::into);
            out
          });

          let canonical = source_resource
            .as_ref()
            .and_then(|res| res.final_url.clone())
            .unwrap_or_else(|| req.url.clone());

          if canonical != req.url {
            aliases.insert(req.url.clone(), canonical.clone());
          }

          if let Some(source) = source_resource.as_mut() {
            // Mirror disk cache behavior: the stored artifact is keyed by canonical URL, so make the
            // returned metadata use that canonical URL as well.
            source.final_url = Some(canonical.clone());
          }

          artifacts.insert(
            (canonical, artifact),
            StoredArtifact {
              bytes,
              source: source_resource,
            },
          );
          IpcResponse::Unit(IpcResult::Ok(()))
        }
        IpcRequest::ReadCacheArtifactWithRequest { req, artifact } => {
          let resolved = resolve_alias(&aliases, &req.url);
          let value = artifacts.get(&(resolved.clone(), artifact)).map(|stored| {
            let mut res = FetchedResource::new(stored.bytes.clone(), None);
            if let Some(source) = stored.source.as_ref() {
              res.status = source.status;
              res.nosniff = source.nosniff;
              res.etag = source.etag.clone();
              res.last_modified = source.last_modified.clone();
              res.access_control_allow_origin = source.access_control_allow_origin.clone();
              res.timing_allow_origin = source.timing_allow_origin.clone();
              res.vary = source.vary.clone();
              res.access_control_allow_credentials = source.access_control_allow_credentials;
              res.cache_policy = source.cache_policy.clone();
              res.final_url = source.final_url.clone();
            }
            if res.final_url.is_none() {
              res.final_url = Some(resolved.clone());
            }
            res
          });

          IpcResponse::MaybeFetched(IpcResult::Ok(value.map(Into::into)))
        }
        IpcRequest::RemoveCacheArtifactWithRequest { req, artifact } => {
          let resolved = resolve_alias(&aliases, &req.url);
          aliases.remove(&req.url);
          aliases.retain(|_k, v| v != &resolved);
          artifacts.remove(&(resolved, artifact));
          IpcResponse::Unit(IpcResult::Ok(()))
        }
        other => panic!("unexpected IPC request: {other:?}"),
      };

      let mut service = NetworkService::new(&mut stream);
      service
        .send_response(env.id, response)
        .expect("write ipc response");
    }
  })
}

#[test]
fn ipc_fetcher_cache_artifacts_roundtrip_metadata() {
  let _net_guard = net_test_lock();
  let Some(ipc_listener) = try_bind_localhost("ipc_fetcher_cache_artifacts_roundtrip_metadata")
  else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let ipc_handle = spawn_ipc_server(ipc_listener);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = "https://example.test/image.png";
  let final_url = "https://cdn.example.test/image.png";

  let bytes = br#"{"dummy":"probe-metadata"}"#.to_vec();
  let mut source = FetchedResource::new(Vec::new(), Some("image/png".to_string()));
  source.final_url = Some(final_url.to_string());
  source.access_control_allow_origin = Some("*".to_string());

  let req = FetchRequest::new(url, FetchDestination::Image);

  fetcher.write_cache_artifact_with_request(
    req,
    CacheArtifactKind::ImageProbeMetadata,
    &bytes,
    Some(&source),
  );

  let read = fetcher
    .read_cache_artifact_with_request(req, CacheArtifactKind::ImageProbeMetadata)
    .expect("expected cached artifact to be readable");

  assert_eq!(read.bytes, bytes);
  assert_eq!(read.final_url.as_deref(), Some(final_url));
  assert_eq!(read.access_control_allow_origin.as_deref(), Some("*"));

  fetcher.remove_cache_artifact_with_request(req, CacheArtifactKind::ImageProbeMetadata);
  assert!(fetcher
    .read_cache_artifact_with_request(req, CacheArtifactKind::ImageProbeMetadata)
    .is_none());

  drop(fetcher);
  ipc_handle.join().unwrap();
}
