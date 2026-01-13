//! Network-side server for the `resource::ipc_fetcher` protocol.
//!
//! The renderer-side [`crate::resource::ipc_fetcher::IpcResourceFetcher`] uses a length-prefixed
//! JSON protocol over a bidirectional byte stream (typically a TCP socket).
//!
//! ## Wire format
//! - `u32` little-endian length prefix
//! - JSON payload (`serde_json`)
//!
//! ## Handshake
//! - Client sends `IpcRequest::Hello { token }` (unenveloped).
//! - Server replies with `IpcResponse::HelloAck` (unenveloped).
//!
//! ## Requests / responses
//! After authentication, each request is wrapped in a [`BrowserToNetwork`] envelope so the client
//! can correlate replies with a `RequestId`. Responses are sent as [`NetworkToBrowser`] messages via
//! the [`crate::resource::ipc_fetcher::NetworkService`] writer helper, which supports chunked body
//! transfer for large fetch responses.

use crate::resource::ipc_fetcher::{
  validate_ipc_request, BrowserToNetwork, IpcCacheSourceMetadata, IpcError, IpcFetchRequest,
  IpcRequest, IpcResponse, IpcResult, NetworkService as IpcResponseWriter,
  IPC_AUTH_TOKEN_ENV, IPC_MAX_INBOUND_FRAME_BYTES, IPC_MAX_OUTBOUND_FRAME_BYTES,
};
use crate::resource::{
  CacheArtifactKind, FetchContextKind, FetchedResource, HttpFetcher, HttpRequest, ResourceFetcher,
};
use base64::Engine as _;
use std::io::{self, Read, Write};

const IPC_FRAME_LEN_BYTES: usize = 4;

fn read_ipc_frame<R: Read>(reader: &mut R, max_frame_bytes: usize) -> io::Result<Option<Vec<u8>>> {
  let mut len_buf = [0u8; IPC_FRAME_LEN_BYTES];
  match reader.read_exact(&mut len_buf) {
    Ok(()) => {}
    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
    Err(err) => return Err(err),
  }

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
  reader.read_exact(&mut buf)?;
  Ok(Some(buf))
}

fn write_ipc_frame<W: Write>(
  writer: &mut W,
  payload: &[u8],
  max_frame_bytes: usize,
) -> io::Result<()> {
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
  writer.write_all(&len)?;
  writer.write_all(payload)?;
  writer.flush()?;
  Ok(())
}

/// Network-side server for `IpcResourceFetcher` RPCs.
///
/// This is intended to run in a trusted "network process" and execute requests using an
/// in-process [`HttpFetcher`].
pub struct IpcFetchServer {
  fetcher: HttpFetcher,
  auth_token: String,
}

impl IpcFetchServer {
  /// Create a new server that expects `auth_token` during the hello handshake.
  pub fn new(fetcher: HttpFetcher, auth_token: impl Into<String>) -> io::Result<Self> {
    let auth_token = auth_token.into();
    validate_ipc_request(&IpcRequest::Hello {
      token: auth_token.clone(),
    })
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    Ok(Self { fetcher, auth_token })
  }

  /// Create a new server, reading the auth token from [`IPC_AUTH_TOKEN_ENV`].
  pub fn new_from_env(fetcher: HttpFetcher) -> io::Result<Self> {
    let token = std::env::var(IPC_AUTH_TOKEN_ENV).map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("missing IPC auth token: set {IPC_AUTH_TOKEN_ENV}"),
      )
    })?;
    Self::new(fetcher, token)
  }

  pub fn run<R: Read, W: Write>(&mut self, mut reader: R, mut writer: W) -> io::Result<()> {
    // -------------------------------------------------------------------------
    // Hello/auth handshake (unenveloped).
    // -------------------------------------------------------------------------
    let Some(frame) = read_ipc_frame(&mut reader, IPC_MAX_INBOUND_FRAME_BYTES)? else {
      // Clean EOF: peer dropped the connection before sending anything.
      return Ok(());
    };
    let hello: IpcRequest = serde_json::from_slice(&frame).map_err(|err| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid IPC hello request JSON: {err}"),
      )
    })?;
    validate_ipc_request(&hello)
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    match hello {
      IpcRequest::Hello { token } => {
        if token != self.auth_token {
          // Wrong token: close the connection without sending a response.
          return Ok(());
        }
      }
      other => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          format!("expected IPC hello request, got {other:?}"),
        ))
      }
    }

    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).map_err(|err| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("failed to serialize IPC hello ack: {err}"),
      )
    })?;
    write_ipc_frame(&mut writer, &hello_ack, IPC_MAX_OUTBOUND_FRAME_BYTES)?;

    // -------------------------------------------------------------------------
    // Enveloped request loop.
    // -------------------------------------------------------------------------
    loop {
      let Some(frame) = read_ipc_frame(&mut reader, IPC_MAX_INBOUND_FRAME_BYTES)? else {
        break;
      };
      let env: BrowserToNetwork = serde_json::from_slice(&frame).map_err(|err| {
        io::Error::new(
          io::ErrorKind::InvalidData,
          format!("invalid IPC request JSON: {err}"),
        )
      })?;
      validate_ipc_request(&env.request)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

      let mut service = IpcResponseWriter::new(&mut writer);

      match env.request {
        // Protocol violation: Hello is only allowed during the initial handshake.
        IpcRequest::Hello { .. } => break,

        IpcRequest::Fetch { url } => {
          service.send_fetch_result(env.id, self.fetcher.fetch(&url))?;
        }
        IpcRequest::FetchWithRequest { req } => {
          service.send_fetch_result(env.id, self.fetcher.fetch_with_request(req.as_fetch_request()))?;
        }
        IpcRequest::FetchWithRequestAndValidation {
          req,
          etag,
          last_modified,
        } => {
          service.send_fetch_result(
            env.id,
            self.fetcher.fetch_with_request_and_validation(
              req.as_fetch_request(),
              etag.as_deref(),
              last_modified.as_deref(),
            ),
          )?;
        }
        IpcRequest::FetchHttpRequest { req } => {
          let body = match req.decode_body() {
            Ok(body) => body,
            Err(msg) => {
              service.send_fetch_result(env.id, Err(crate::Error::Other(msg)))?;
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
          service.send_fetch_result(env.id, self.fetcher.fetch_http_request(http_req))?;
        }
        IpcRequest::FetchPartialWithContext {
          kind,
          url,
          max_bytes,
        } => {
          let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
          service.send_fetch_result(
            env.id,
            self.fetcher.fetch_partial_with_context(kind, &url, max_bytes),
          )?;
        }
        IpcRequest::FetchPartialWithRequest { req, max_bytes } => {
          let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
          service.send_fetch_result(
            env.id,
            self.fetcher.fetch_partial_with_request(req.as_fetch_request(), max_bytes),
          )?;
        }

        IpcRequest::RequestHeaderValue { req, header_name } => {
          let value = self
            .fetcher
            .request_header_value(req.as_fetch_request(), &header_name);
          service.send_response(env.id, IpcResponse::MaybeString(IpcResult::Ok(value)))?;
        }
        IpcRequest::CookieHeaderValue { url } => {
          service.send_response(
            env.id,
            IpcResponse::MaybeString(IpcResult::Ok(self.fetcher.cookie_header_value(&url))),
          )?;
        }
        IpcRequest::StoreCookieFromDocument { url, cookie_string } => {
          self.fetcher.store_cookie_from_document(&url, &cookie_string);
          service.send_response(env.id, IpcResponse::Unit(IpcResult::Ok(())))?;
        }

        IpcRequest::ReadCacheArtifact { kind, url, artifact } => {
          let value = self
            .fetcher
            .read_cache_artifact(kind, &url, artifact)
            .map(Into::into);
          service.send_response(
            env.id,
            IpcResponse::MaybeFetched(IpcResult::Ok(value)),
          )?;
        }
        IpcRequest::ReadCacheArtifactWithRequest { req, artifact } => {
          let value = self
            .fetcher
            .read_cache_artifact_with_request(req.as_fetch_request(), artifact)
            .map(Into::into);
          service.send_response(
            env.id,
            IpcResponse::MaybeFetched(IpcResult::Ok(value)),
          )?;
        }
        IpcRequest::WriteCacheArtifact {
          kind,
          url,
          artifact,
          bytes_b64,
          source,
        } => {
          let response =
            self.handle_write_cache_artifact(kind, &url, artifact, &bytes_b64, source);
          service.send_response(env.id, response)?;
        }
        IpcRequest::WriteCacheArtifactWithRequest {
          req,
          artifact,
          bytes_b64,
          source,
        } => {
          let response = self.handle_write_cache_artifact_with_request(req, artifact, &bytes_b64, source);
          service.send_response(env.id, response)?;
        }
        IpcRequest::RemoveCacheArtifact { kind, url, artifact } => {
          self.fetcher.remove_cache_artifact(kind, &url, artifact);
          service.send_response(env.id, IpcResponse::Unit(IpcResult::Ok(())))?;
        }
        IpcRequest::RemoveCacheArtifactWithRequest { req, artifact } => {
          self
            .fetcher
            .remove_cache_artifact_with_request(req.as_fetch_request(), artifact);
          service.send_response(env.id, IpcResponse::Unit(IpcResult::Ok(())))?;
        }
      }
    }

    Ok(())
  }

  fn handle_write_cache_artifact(
    &self,
    kind: FetchContextKind,
    url: &str,
    artifact: CacheArtifactKind,
    bytes_b64: &str,
    source: Option<IpcCacheSourceMetadata>,
  ) -> IpcResponse {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(bytes_b64.as_bytes()) {
      Ok(bytes) => bytes,
      Err(err) => {
        return IpcResponse::Unit(IpcResult::Err(IpcError {
          message: format!("invalid base64 cache artifact bytes: {err}"),
          content_type: None,
          status: None,
          final_url: None,
          etag: None,
          last_modified: None,
        }));
      }
    };
    let source = source.map(ipc_cache_source_to_fetched);
    self
      .fetcher
      .write_cache_artifact(kind, url, artifact, &bytes, source.as_ref());
    IpcResponse::Unit(IpcResult::Ok(()))
  }

  fn handle_write_cache_artifact_with_request(
    &self,
    req: IpcFetchRequest,
    artifact: CacheArtifactKind,
    bytes_b64: &str,
    source: Option<IpcCacheSourceMetadata>,
  ) -> IpcResponse {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(bytes_b64.as_bytes()) {
      Ok(bytes) => bytes,
      Err(err) => {
        return IpcResponse::Unit(IpcResult::Err(IpcError {
          message: format!("invalid base64 cache artifact bytes: {err}"),
          content_type: None,
          status: None,
          final_url: None,
          etag: None,
          last_modified: None,
        }));
      }
    };
    let source = source.map(ipc_cache_source_to_fetched);
    self.fetcher.write_cache_artifact_with_request(
      req.as_fetch_request(),
      artifact,
      &bytes,
      source.as_ref(),
    );
    IpcResponse::Unit(IpcResult::Ok(()))
  }
}

fn ipc_cache_source_to_fetched(meta: IpcCacheSourceMetadata) -> FetchedResource {
  let mut res = FetchedResource::new(Vec::new(), None);
  res.status = meta.status;
  res.nosniff = meta.nosniff;
  res.etag = meta.etag;
  res.last_modified = meta.last_modified;
  res.access_control_allow_origin = meta.access_control_allow_origin;
  res.timing_allow_origin = meta.timing_allow_origin;
  res.vary = meta.vary;
  res.access_control_allow_credentials = meta.access_control_allow_credentials;
  res.final_url = meta.final_url;
  res.cache_policy = meta.cache_policy.map(Into::into);
  res
}
