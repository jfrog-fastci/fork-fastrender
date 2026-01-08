//! cURL-based HTTP backend for [`HttpFetcher`].
//!
//! This backend exists as a last-resort fallback for sites that fail with the Rust HTTP/TLS stack
//! (e.g. due to TLS fingerprinting or HTTP/2 quirks). It shells out to the system `curl` binary
//! via [`std::process::Command`].
//!
//! Notes:
//! - This module is intentionally dependency-light (no libcurl bindings).
//! - Cookies are handled by the parent [`HttpFetcher`] via a shared in-memory jar; this backend
//!   only receives an explicit `Cookie` header and forwards `Set-Cookie` responses back to the jar.
//! - Redirects are handled in Rust (not `--location`) so [`ResourcePolicy`] checks apply to every
//!   hop, matching the primary backend semantics.
#![allow(clippy::too_many_lines)]

use super::HttpFetcher;
use crate::error::{Error, RenderError, ResourceError, Result};
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::render_control;
use http::HeaderMap;
use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

const CURL_STATUS_LINE_MAX_BYTES: usize = 8 * 1024;
const CURL_HEADER_LINE_MAX_BYTES: usize = 32 * 1024;
const CURL_HEADER_BLOCK_MAX_BYTES: usize = 256 * 1024;
const CURL_MAX_HEADER_LINES: usize = 1024;
const CURL_MAX_HEADER_BLOCKS: usize = 8;

pub(super) fn curl_available() -> bool {
  static AVAILABLE: OnceLock<bool> = OnceLock::new();
  *AVAILABLE.get_or_init(|| {
    Command::new("curl")
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .map(|status| status.success())
      .unwrap_or(false)
  })
}

#[derive(Debug)]
struct CurlResponse {
  status: u16,
  headers: HeaderMap,
  body: Vec<u8>,
}

#[derive(Debug)]
enum CurlError {
  Failure(CurlFailure),
  BodyTooLarge {
    status: u16,
    observed: usize,
    limit: usize,
  },
}

#[derive(Debug)]
struct CurlFailure {
  exit_status: Option<ExitStatus>,
  stderr: String,
  spawn_error: Option<io::Error>,
}

impl CurlFailure {
  fn message(&self) -> String {
    if let Some(err) = &self.spawn_error {
      return err.to_string();
    }
    let code = self
      .exit_status
      .as_ref()
      .and_then(|s| s.code())
      .map(|c| c.to_string())
      .unwrap_or_else(|| "<signal>".to_string());
    if self.stderr.trim().is_empty() {
      format!("curl failed (exit code {code})")
    } else {
      format!("curl failed (exit code {code}): {}", self.stderr.trim())
    }
  }

  fn retryable(&self) -> bool {
    let Some(status) = self.exit_status.as_ref().and_then(|s| s.code()) else {
      // Signaled/killed: don't retry.
      return false;
    };
    // Common transient curl exit codes. We err on the side of retrying since higher-level budgets
    // and deadlines cap total time.
    matches!(status, 5 | 6 | 7 | 18 | 28 | 35 | 52 | 56 | 92)
      || self
        .stderr
        .to_ascii_lowercase()
        .contains("connection reset")
      || self.stderr.to_ascii_lowercase().contains("timed out")
      || self.stderr.to_ascii_lowercase().contains("timeout")
      || self.stderr.to_ascii_lowercase().contains("http2")
  }
}

fn sanitize_header_value(value: &str) -> String {
  value
    .chars()
    .map(|c| match c {
      '\r' | '\n' | '\0' => ' ',
      other => other,
    })
    .collect::<String>()
    .trim()
    .to_string()
}

pub(super) fn build_curl_args(
  url: &str,
  timeout: Option<Duration>,
  headers: &[(String, String)],
  force_http1: bool,
) -> Vec<String> {
  let mut args = Vec::new();
  args.push("-q".to_string());
  args.push("--globoff".to_string());
  args.push("--silent".to_string());
  args.push("--show-error".to_string());
  args.push("--dump-header".to_string());
  args.push("-".to_string());
  if force_http1 {
    args.push("--http1.1".to_string());
  }
  if let Some(timeout) = timeout.filter(|t| !t.is_zero()) {
    args.push("--max-time".to_string());
    args.push(format!("{:.3}", timeout.as_secs_f64()));
  }
  for (name, value) in headers {
    args.push("--header".to_string());
    args.push(format!("{}: {}", name, sanitize_header_value(value)));
  }
  args.push("--".to_string());
  args.push(url.to_string());
  args
}

fn parse_status_line(line: &str) -> Option<u16> {
  let trimmed = line.trim();
  let mut parts = trimmed.split_whitespace();
  let proto = parts.next()?;
  if !proto
    .get(.."http/".len())
    .map(|prefix| prefix.eq_ignore_ascii_case("http/"))
    .unwrap_or(false)
  {
    return None;
  }
  let code = parts.next()?;
  code.parse::<u16>().ok()
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
  let mut start = 0usize;
  let mut end = bytes.len();
  while start < end && bytes[start].is_ascii_whitespace() {
    start += 1;
  }
  while end > start && bytes[end - 1].is_ascii_whitespace() {
    end -= 1;
  }
  &bytes[start..end]
}

fn trim_crlf(bytes: &[u8]) -> &[u8] {
  let mut end = bytes.len();
  while end > 0 && matches!(bytes[end - 1], b'\r' | b'\n') {
    end -= 1;
  }
  &bytes[..end]
}

fn parse_header_line(line: &[u8], headers: &mut HeaderMap) {
  let trimmed = trim_crlf(line);
  let Some(pos) = trimmed.iter().position(|b| *b == b':') else {
    return;
  };
  let name = trim_ascii(&trimmed[..pos]);
  let value = trim_ascii(&trimmed[pos + 1..]);
  if name.is_empty() {
    return;
  }
  let Ok(name) = http::header::HeaderName::from_bytes(name) else {
    return;
  };
  let Ok(value) = http::HeaderValue::from_bytes(value) else {
    return;
  };
  headers.append(name, value);
}

fn read_bounded_line<R: BufRead>(
  reader: &mut R,
  max_bytes: usize,
  context: &'static str,
) -> io::Result<Option<Vec<u8>>> {
  let mut out = FallibleVecWriter::new(max_bytes, context);
  let mut read_any = false;
  loop {
    let buf = reader.fill_buf()?;
    if buf.is_empty() {
      let bytes = out.into_inner();
      return Ok(read_any.then_some(bytes));
    }
    read_any = true;
    if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
      out.write_all(&buf[..=pos])?;
      reader.consume(pos + 1);
      return Ok(Some(out.into_inner()));
    }
    out.write_all(buf)?;
    let len = buf.len();
    reader.consume(len);
  }
}

fn read_curl_headers<R: BufRead>(reader: &mut R) -> io::Result<(String, u16, HeaderMap)> {
  // curl may emit multiple header blocks (e.g. 100 Continue, proxy CONNECT). We read blocks until
  // we hit the final response head.
  let mut blocks = 0usize;
  loop {
    if blocks >= CURL_MAX_HEADER_BLOCKS {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many HTTP header blocks from curl",
      ));
    }
    blocks += 1;

    let status_bytes = read_bounded_line(reader, CURL_STATUS_LINE_MAX_BYTES, "curl status line")?;
    let Some(status_bytes) = status_bytes else {
      return Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "curl produced no HTTP headers",
      ));
    };

    let status_line = String::from_utf8_lossy(&status_bytes).to_string();

    let status = parse_status_line(&status_line).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid HTTP status line from curl: {}", status_line.trim()),
      )
    })?;

    let mut headers = HeaderMap::new();
    let mut header_bytes = status_bytes.len();
    let mut header_lines = 0usize;
    loop {
      if header_lines >= CURL_MAX_HEADER_LINES {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "too many HTTP header lines from curl",
        ));
      }
      let line = match read_bounded_line(reader, CURL_HEADER_LINE_MAX_BYTES, "curl header line")? {
        Some(line) => line,
        None => break,
      };
      header_bytes = header_bytes.saturating_add(line.len());
      if header_bytes > CURL_HEADER_BLOCK_MAX_BYTES {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          format!("curl headers exceed {} bytes", CURL_HEADER_BLOCK_MAX_BYTES),
        ));
      }
      if trim_crlf(&line).is_empty() {
        break;
      }
      parse_header_line(&line, &mut headers);
      header_lines += 1;
    }

    let status_lower = status_line.to_ascii_lowercase();
    let provisional =
      (100..200).contains(&status) || status_lower.contains("connection established");
    if provisional {
      continue;
    }

    return Ok((status_line, status, headers));
  }
}

#[derive(Debug)]
enum CurlBodyReadError {
  Io(io::Error),
  TooLarge { observed: usize, limit: usize },
}

fn read_curl_body<R: Read>(
  reader: &mut R,
  body_limit: usize,
  should_discard_body: bool,
) -> std::result::Result<Vec<u8>, CurlBodyReadError> {
  let mut body = FallibleVecWriter::new(body_limit, "curl body");
  let mut body_len = 0usize;
  let mut buf = [0u8; 8192];
  loop {
    let read = reader.read(&mut buf).map_err(CurlBodyReadError::Io)?;
    if read == 0 {
      break;
    }

    if should_discard_body {
      // Drain without buffering to match the primary backend (redirect bodies are ignored).
      continue;
    }

    let next_len = body_len.saturating_add(read);
    if next_len > body_limit {
      return Err(CurlBodyReadError::TooLarge {
        observed: next_len,
        limit: body_limit,
      });
    }

    body
      .write_all(&buf[..read])
      .map_err(CurlBodyReadError::Io)?;
    body_len = next_len;
  }

  Ok(body.into_inner())
}

fn run_curl(
  url: &str,
  timeout: Option<Duration>,
  headers: &[(String, String)],
  body_limit: usize,
  force_http1: bool,
) -> std::result::Result<CurlResponse, CurlError> {
  let args = build_curl_args(url, timeout, headers, force_http1);
  let mut command = Command::new("curl");
  command.args(&args);
  command.stdout(Stdio::piped());
  command.stderr(Stdio::piped());

  let mut child = match command.spawn() {
    Ok(child) => child,
    Err(err) => {
      return Err(CurlError::Failure(CurlFailure {
        exit_status: None,
        stderr: String::new(),
        spawn_error: Some(err),
      }));
    }
  };

  let stderr = child.stderr.take().expect("stderr piped");
  let stderr_handle = thread::spawn(move || {
    const STDERR_MAX_BYTES: u64 = 64 * 1024;
    let mut limited = stderr.take(STDERR_MAX_BYTES);
    let mut buf = FallibleVecWriter::new(STDERR_MAX_BYTES as usize, "curl stderr");
    let _ = io::copy(&mut limited, &mut buf);
    String::from_utf8_lossy(&buf.into_inner()).to_string()
  });

  let stdout = child.stdout.take().expect("stdout piped");
  let mut reader = BufReader::new(stdout);

  let (status_line, status, headers) = match read_curl_headers(&mut reader) {
    Ok(parsed) => parsed,
    Err(err) => {
      let _ = child.kill();
      let exit_status = child.wait().ok();
      let stderr = stderr_handle.join().unwrap_or_default();
      return Err(CurlError::Failure(CurlFailure {
        exit_status,
        stderr: format!("{stderr}\n{err}").trim().to_string(),
        spawn_error: None,
      }));
    }
  };

  let has_location = headers.get("location").is_some();
  let should_discard_body = (300..400).contains(&status) && has_location;

  let body = match read_curl_body(&mut reader, body_limit, should_discard_body) {
    Ok(body) => body,
    Err(CurlBodyReadError::TooLarge { observed, limit }) => {
      let _ = child.kill();
      let _ = child.wait();
      let _ = stderr_handle.join();
      return Err(CurlError::BodyTooLarge {
        status,
        observed,
        limit,
      });
    }
    Err(CurlBodyReadError::Io(err)) => {
      let _ = child.kill();
      let exit_status = child.wait().ok();
      let stderr = stderr_handle.join().unwrap_or_default();
      return Err(CurlError::Failure(CurlFailure {
        exit_status,
        stderr: format!("{stderr}\n{err}").trim().to_string(),
        spawn_error: None,
      }));
    }
  };

  let exit_status = child.wait().ok();
  let stderr = stderr_handle.join().unwrap_or_default();

  let Some(exit_status) = exit_status else {
    return Err(CurlError::Failure(CurlFailure {
      exit_status: None,
      stderr,
      spawn_error: None,
    }));
  };

  if !exit_status.success() {
    return Err(CurlError::Failure(CurlFailure {
      exit_status: Some(exit_status),
      stderr: format!("{stderr}\n{status_line}").trim().to_string(),
      spawn_error: None,
    }));
  }

  Ok(CurlResponse {
    status,
    headers,
    body,
  })
}

pub(super) fn fetch_http_with_accept_inner<'a>(
  fetcher: &HttpFetcher,
  kind: super::FetchContextKind,
  destination: super::FetchDestination,
  url: &str,
  accept_encoding: Option<&str>,
  validators: Option<super::HttpCacheValidators<'a>>,
  client_origin: Option<&super::DocumentOrigin>,
  referrer_url: Option<&str>,
  referrer_policy: super::ReferrerPolicy,
  credentials_mode: super::FetchCredentialsMode,
  deadline: &Option<render_control::RenderDeadline>,
  started: Instant,
) -> Result<super::FetchedResource> {
  let mut current = url.to_string();
  let mut validators = validators;
  let mut effective_referrer_policy = referrer_policy;
  let mut redirect_referrer_policy: Option<super::ReferrerPolicy> = None;

  let timeout_budget = fetcher.timeout_budget(deadline);
  let max_attempts = if deadline
    .as_ref()
    .and_then(render_control::RenderDeadline::timeout_limit)
    .is_some()
    && timeout_budget.is_none()
  {
    1
  } else {
    fetcher.retry_policy.max_attempts.max(1)
  };

  let budget_exhausted_error = |current_url: &str, attempt: usize| -> Error {
    let budget = timeout_budget.expect("budget mode should be active");
    let elapsed = started.elapsed();
    Error::Resource(
      ResourceError::new(
        current_url.to_string(),
        format!(
          "overall HTTP timeout budget exceeded (budget={budget:?}, elapsed={elapsed:?}){}",
          super::format_attempt_suffix(attempt, max_attempts)
        ),
      )
      .with_final_url(current_url.to_string()),
    )
  };

  let mut force_http1 = false;

  'redirects: for _ in 0..fetcher.policy.max_redirects {
    fetcher.policy.ensure_url_allowed(&current)?;

    for attempt in 1..=max_attempts {
      fetcher.policy.ensure_url_allowed(&current)?;

      let stage_hint = super::render_stage_hint_for_context(kind, &current);
      if let Some(deadline) = deadline.as_ref().filter(|d| d.is_enabled()) {
        deadline.check(stage_hint).map_err(Error::Render)?;
      }

      let allowed_limit = fetcher.policy.allowed_response_limit()?;
      let per_request_timeout =
        fetcher.deadline_aware_timeout(kind, deadline.as_ref(), &current)?;
      let mut effective_timeout = per_request_timeout.unwrap_or(fetcher.policy.request_timeout);

      if let Some(budget) = timeout_budget {
        match budget.checked_sub(started.elapsed()) {
          Some(remaining) if remaining > super::HTTP_DEADLINE_BUFFER => {
            let budget_timeout = remaining.saturating_sub(super::HTTP_DEADLINE_BUFFER);
            effective_timeout = effective_timeout.min(budget_timeout);
          }
          _ => return Err(budget_exhausted_error(&current, attempt)),
        }
      }

      let accept_encoding_value = accept_encoding.unwrap_or(super::SUPPORTED_ACCEPT_ENCODING);
      let mut headers = super::build_http_header_pairs(
        &current,
        &fetcher.user_agent,
        &fetcher.accept_language,
        accept_encoding_value,
        validators,
        destination,
        client_origin,
        referrer_url,
        effective_referrer_policy,
      );
      if super::cookies_allowed_for_request(credentials_mode, &current, client_origin) {
        if let Some(value) = fetcher.cookie_header_value(&current) {
          headers.push(("Cookie".to_string(), value));
        }
      }

      let network_timer = super::start_network_fetch_diagnostics();
      let response = run_curl(
        &current,
        (!effective_timeout.is_zero()).then_some(effective_timeout),
        &headers,
        allowed_limit,
        force_http1,
      );
      super::finish_network_fetch_diagnostics(network_timer);

      let response = match response {
        Ok(res) => res,
        Err(CurlError::BodyTooLarge {
          status,
          observed,
          limit,
        }) => {
          if let Some(remaining) = fetcher.policy.remaining_budget() {
            if observed > remaining {
              let err = ResourceError::new(
                current.clone(),
                format!(
                  "total bytes budget exceeded ({} > {} bytes remaining)",
                  observed, remaining
                ),
              )
              .with_status(status)
              .with_final_url(current.clone());
              return Err(Error::Resource(err));
            }
          }

          let err = ResourceError::new(
            current.clone(),
            format!("response too large ({} > {} bytes)", observed, limit),
          )
          .with_status(status)
          .with_final_url(current.clone());
          return Err(Error::Resource(err));
        }
        Err(CurlError::Failure(failure)) => {
          if !force_http1
            && failure
              .exit_status
              .as_ref()
              .and_then(|s| s.code())
              .is_some_and(|code| code == 92)
          {
            // Retry HTTP/2 INTERNAL_ERROR responses over HTTP/1.1. Some sites/CDNs fail
            // intermittently when negotiating H2 but succeed over H1.
            force_http1 = true;
          }
          if attempt < max_attempts && failure.retryable() {
            let mut backoff = super::compute_backoff(&fetcher.retry_policy, attempt, &current);
            let mut can_retry = true;
            if let Some(deadline) = deadline.as_ref() {
              if deadline.timeout_limit().is_some() {
                match deadline.remaining_timeout() {
                  Some(remaining) if !remaining.is_zero() => {
                    let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                    backoff = backoff.min(max_sleep);
                  }
                  _ => can_retry = false,
                }
              }
            }
            if let Some(budget) = timeout_budget {
              match budget.checked_sub(started.elapsed()) {
                Some(remaining) if remaining > super::HTTP_DEADLINE_BUFFER => {
                  let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                  backoff = backoff.min(max_sleep);
                }
                _ => can_retry = false,
              }
            }
            if can_retry {
              super::log_http_retry(&failure.message(), attempt, max_attempts, &current, backoff);
              if !backoff.is_zero() {
                super::sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                  .map_err(Error::Render)?;
              }
              continue;
            }
            if timeout_budget.is_some() {
              return Err(budget_exhausted_error(&current, attempt));
            }
          }

          let overall_timeout = deadline.as_ref().and_then(|d| d.timeout_limit());
          let mut message = failure.message();
          let lower = message.to_ascii_lowercase();
          if lower.contains("timeout") || lower.contains("timed out") {
            if let Some(overall) = overall_timeout {
              message.push_str(&format!(
                " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout={overall:?})"
              ));
            } else if let Some(budget) = timeout_budget {
              message.push_str(&format!(
                " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?}, overall_timeout_budget={budget:?})"
              ));
            } else {
              message.push_str(&format!(
                " (attempt {attempt}/{max_attempts}, per_attempt_timeout={effective_timeout:?})"
              ));
            }
          } else {
            message.push_str(&super::format_attempt_suffix(attempt, max_attempts));
          }

          let mut err =
            ResourceError::new(current.clone(), message).with_final_url(current.clone());
          if let Some(source) = failure.spawn_error {
            err = err.with_source(source);
          }
          return Err(Error::Resource(err));
        }
      };

      if super::cookies_allowed_for_request(credentials_mode, &current, client_origin) {
        fetcher.store_cookies_from_headers(&current, &response.headers);
      }

      let status_code = response.status;
      if (300..400).contains(&status_code) {
        if let Some(loc) = response
          .headers
          .get("location")
          .and_then(|h| h.to_str().ok())
        {
          if let Some(policy) = super::header_values_joined(&response.headers, "referrer-policy")
            .as_deref()
            .and_then(super::ReferrerPolicy::parse_value_list)
          {
            effective_referrer_policy = policy;
            redirect_referrer_policy = Some(policy);
          }
          let next = Url::parse(&current)
            .ok()
            .and_then(|base| base.join(loc).ok())
            .map(|u| u.to_string())
            .unwrap_or_else(|| loc.to_string());
          fetcher.policy.ensure_url_allowed(&next)?;
          current = next;
          validators = None;
          continue 'redirects;
        }
      }

      let retry_after =
        if fetcher.retry_policy.respect_retry_after && super::retryable_http_status(status_code) {
          super::parse_retry_after(&response.headers)
        } else {
          None
        };

      let encodings = super::parse_content_encodings(&response.headers);
      let mut content_type = response
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
      let nosniff = super::header_has_nosniff(&response.headers);
      let mut decode_stage = super::decode_stage_for_content_type(content_type.as_deref());
      let etag = response
        .headers
        .get("etag")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
      let last_modified = response
        .headers
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
      let (access_control_allow_origin, access_control_allow_credentials) =
        super::parse_cors_response_headers(&response.headers);
      let timing_allow_origin =
        super::header_values_joined(&response.headers, "timing-allow-origin");
      let response_referrer_policy = super::header_values_joined(&response.headers, "referrer-policy")
        .as_deref()
        .and_then(super::ReferrerPolicy::parse_value_list)
        .or(redirect_referrer_policy);
      let cache_policy = super::parse_http_cache_policy(&response.headers);
      let vary = super::parse_vary_headers(&response.headers);
      let response_headers = super::collect_response_headers(&response.headers);

      let substitute_empty_image_body =
        super::should_substitute_empty_image_body(kind, status_code, &response.headers)
          || super::should_substitute_akamai_pixel_empty_image_body(
            kind,
            &current,
            status_code,
            &response.headers,
          );
      let substitute_captcha_image =
        super::should_substitute_captcha_image_response(kind, status_code, &current);
      let mut bytes = match super::decode_content_encodings(
        response.body,
        &encodings,
        allowed_limit,
        decode_stage,
      ) {
        Ok(decoded) => decoded,
        Err(super::ContentDecodeError::DeadlineExceeded { stage, elapsed, .. }) => {
          return Err(Error::Render(RenderError::Timeout { stage, elapsed }));
        }
        Err(super::ContentDecodeError::DecompressionFailed { .. }) if accept_encoding.is_none() => {
          return fetch_http_with_accept_inner(
            fetcher,
            kind,
            destination,
            url,
            Some("identity"),
            validators,
            client_origin,
            referrer_url,
            referrer_policy,
            credentials_mode,
            deadline,
            started,
          );
        }
        Err(err) => {
          let err = err.into_resource_error(current.clone(), status_code, current.clone());
          return Err(Error::Resource(err));
        }
      };

      super::record_network_fetch_bytes(bytes.len());
      if bytes.is_empty() && substitute_empty_image_body {
        bytes = super::OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
        content_type = Some(super::OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
        decode_stage = super::decode_stage_for_content_type(content_type.as_deref());
      }
      if super::should_substitute_markup_image_body(
        kind,
        url,
        &current,
        content_type.as_deref(),
        &bytes,
      ) {
        bytes = super::OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
        content_type = Some(super::OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
        decode_stage = super::decode_stage_for_content_type(content_type.as_deref());
      }
      if substitute_captcha_image {
        bytes = super::OFFLINE_FIXTURE_PLACEHOLDER_PNG.to_vec();
        content_type = Some(super::OFFLINE_FIXTURE_PLACEHOLDER_PNG_MIME.to_string());
        decode_stage = super::decode_stage_for_content_type(content_type.as_deref());
      }
      let is_retryable_status = super::retryable_http_status(status_code);
      let allows_empty_body =
        super::http_response_allows_empty_body(kind, status_code, &response.headers);

      if bytes.is_empty() && super::http_empty_body_is_error(status_code, allows_empty_body) {
        let mut can_retry = attempt < max_attempts;
        if can_retry {
          let mut backoff = super::compute_backoff(&fetcher.retry_policy, attempt, &current);
          if let Some(retry_after) = retry_after {
            backoff = backoff.max(retry_after);
          }

          if let Some(deadline) = deadline.as_ref() {
            if deadline.timeout_limit().is_some() {
              match deadline.remaining_timeout() {
                Some(remaining) if !remaining.is_zero() => {
                  let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                  backoff = backoff.min(max_sleep);
                }
                _ => can_retry = false,
              }
            }
          }
          if let Some(budget) = timeout_budget {
            match budget.checked_sub(started.elapsed()) {
              Some(remaining) if remaining > super::HTTP_DEADLINE_BUFFER => {
                let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                backoff = backoff.min(max_sleep);
              }
              _ => can_retry = false,
            }
          }

          if can_retry {
            super::log_http_retry(
              &format!("empty body (status {status_code})"),
              attempt,
              max_attempts,
              &current,
              backoff,
            );
            if !backoff.is_zero() {
              super::sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                .map_err(Error::Render)?;
            }
            continue;
          }
          if timeout_budget.is_some() {
            return Err(budget_exhausted_error(&current, attempt));
          }
        }

        let mut message = "empty HTTP response body".to_string();
        if attempt < max_attempts {
          message.push_str(" (retry aborted: render deadline exceeded)");
        }
        message.push_str(&super::format_attempt_suffix(attempt, max_attempts));
        let err = ResourceError::new(current.clone(), message)
          .with_status(status_code)
          .with_final_url(current.clone());
        return Err(Error::Resource(err));
      }

      if is_retryable_status {
        let mut can_retry = attempt < max_attempts;
        if can_retry {
          let mut backoff = super::compute_backoff(&fetcher.retry_policy, attempt, &current);
          if let Some(retry_after) = retry_after {
            backoff = backoff.max(retry_after);
          }
          if let Some(deadline) = deadline.as_ref() {
            if deadline.timeout_limit().is_some() {
              match deadline.remaining_timeout() {
                Some(remaining) if !remaining.is_zero() => {
                  let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                  backoff = backoff.min(max_sleep);
                }
                _ => can_retry = false,
              }
            }
          }
          if let Some(budget) = timeout_budget {
            match budget.checked_sub(started.elapsed()) {
              Some(remaining) if remaining > super::HTTP_DEADLINE_BUFFER => {
                let max_sleep = remaining.saturating_sub(Duration::from_millis(1));
                backoff = backoff.min(max_sleep);
              }
              _ => can_retry = false,
            }
          }
          if can_retry {
            super::log_http_retry(
              &format!("status {status_code}"),
              attempt,
              max_attempts,
              &current,
              backoff,
            );
            if !backoff.is_zero() {
              super::sleep_with_deadline(deadline.as_ref(), stage_hint, backoff)
                .map_err(Error::Render)?;
            }
            continue;
          }
          if timeout_budget.is_some() {
            return Err(budget_exhausted_error(&current, attempt));
          }
        }

        if status_code != 202 {
          let mut message = if attempt < max_attempts {
            "retryable HTTP status (retry aborted: render deadline exceeded)".to_string()
          } else {
            "retryable HTTP status (retries exhausted)".to_string()
          };
          message.push_str(&super::format_attempt_suffix(attempt, max_attempts));
          let err = ResourceError::new(current.clone(), message)
            .with_status(status_code)
            .with_final_url(current.clone());
          return Err(Error::Resource(err));
        }
      }

      if bytes.len() > allowed_limit {
        if let Some(remaining) = fetcher.policy.remaining_budget() {
          if bytes.len() > remaining {
            let err = ResourceError::new(
              current.clone(),
              format!(
                "total bytes budget exceeded ({} > {} bytes remaining)",
                bytes.len(),
                remaining
              ),
            )
            .with_status(status_code)
            .with_final_url(current.clone());
            return Err(Error::Resource(err));
          }
        }
        let err = ResourceError::new(
          current.clone(),
          format!(
            "response too large ({} > {} bytes)",
            bytes.len(),
            allowed_limit
          ),
        )
        .with_status(status_code)
        .with_final_url(current.clone());
        return Err(Error::Resource(err));
      }

      fetcher.policy.reserve_budget(bytes.len())?;
      let mut resource =
        super::FetchedResource::with_final_url(bytes, content_type, Some(current.clone()));
      resource.response_headers = Some(response_headers);
      if !encodings.is_empty() {
        resource.content_encoding = Some(encodings.join(", "));
      }
      resource.nosniff = nosniff;
      if !substitute_captcha_image {
        resource.status = Some(status_code);
      }
      resource.etag = etag;
      resource.last_modified = last_modified;
      resource.access_control_allow_origin = access_control_allow_origin;
      resource.timing_allow_origin = timing_allow_origin;
      resource.vary = vary;
      resource.response_referrer_policy = response_referrer_policy;
      resource.access_control_allow_credentials = access_control_allow_credentials;
      resource.cache_policy = cache_policy;
      render_control::check_active(decode_stage).map_err(Error::Render)?;
      return Ok(resource);
    }
  }

  Err(Error::Resource(
    ResourceError::new(
      url,
      format!(
        "too many redirects (limit {})",
        fetcher.policy.max_redirects
      ),
    )
    .with_final_url(current),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::DEFAULT_ACCEPT_LANGUAGE;
  use crate::resource::DEFAULT_USER_AGENT;
  use std::io::Cursor;

  #[test]
  fn header_parsing_skips_provisional_blocks() {
    let input = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/html\r\nETag: \"abc\"\r\n\r\n";
    let mut reader = BufReader::new(&input[..]);
    let (_status_line, status, headers) = read_curl_headers(&mut reader).expect("parse");
    assert_eq!(status, 200);
    assert_eq!(
      headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap(),
      "text/html"
    );
    assert_eq!(
      headers.get("etag").and_then(|h| h.to_str().ok()).unwrap(),
      "\"abc\""
    );
  }

  #[test]
  fn header_parsing_skips_proxy_connect_block() {
    let input =
      b"HTTP/1.1 200 Connection established\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
    let mut reader = BufReader::new(&input[..]);
    let (_status_line, status, headers) = read_curl_headers(&mut reader).expect("parse");
    assert_eq!(status, 200);
    assert_eq!(
      headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap(),
      "text/html"
    );
  }

  #[test]
  fn header_parsing_rejects_oversized_header_lines() {
    let mut input = Vec::new();
    input.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    input.extend_from_slice(b"X-Test: ");
    input.extend(std::iter::repeat(b'a').take(CURL_HEADER_LINE_MAX_BYTES));
    input.extend_from_slice(b"\r\n\r\n");

    let mut reader = BufReader::new(&input[..]);
    let err = read_curl_headers(&mut reader).expect_err("expected oversized header error");
    assert!(
      err.to_string().contains("curl header line"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn build_args_are_separate_and_include_headers() {
    let headers = super::super::build_http_header_pairs(
      "https://example.com/",
      DEFAULT_USER_AGENT,
      DEFAULT_ACCEPT_LANGUAGE,
      "gzip, deflate, br",
      None,
      super::super::FetchContextKind::Other.into(),
      None,
      None,
      super::super::ReferrerPolicy::default(),
    );
    let args = build_curl_args("https://example.com/", Some(Duration::from_secs(3)), &headers, false);
    assert!(args.contains(&"--silent".to_string()));
    assert!(args.contains(&"--show-error".to_string()));
    assert!(args.contains(&"--dump-header".to_string()));
    assert!(args.contains(&"--globoff".to_string()));
    assert!(args.contains(&"--max-time".to_string()));
    assert!(args.iter().any(|a| a.contains("User-Agent:")));
    assert!(args.iter().any(|a| a == "--"));
    assert_eq!(args.last().unwrap(), "https://example.com/");
  }

  #[test]
  fn build_args_sanitizes_header_values() {
    let headers = vec![("X-Test".to_string(), "a\r\nb\0c".to_string())];
    let args = build_curl_args("https://example.com/", None, &headers, false);
    let header_value = args
      .iter()
      .skip_while(|v| *v != "--header")
      .nth(1)
      .expect("expected header value");
    assert!(!header_value.contains('\r'));
    assert!(!header_value.contains('\n'));
    assert!(!header_value.contains('\0'));
  }

  #[test]
  fn status_line_parses_http2() {
    assert_eq!(parse_status_line("HTTP/2 204\r\n"), Some(204));
  }

  #[test]
  fn build_args_can_force_http1() {
    let headers = vec![("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string())];
    let args = build_curl_args("https://example.com/", None, &headers, true);
    assert!(args.contains(&"--http1.1".to_string()));
  }

  #[test]
  fn body_reader_returns_bytes_within_limit() {
    let input = b"abc".to_vec();
    let mut reader = Cursor::new(input.clone());
    let body = read_curl_body(&mut reader, input.len(), false).expect("read body");
    assert_eq!(body, input);
  }

  #[test]
  fn body_reader_rejects_responses_over_limit() {
    let limit = 16;
    let input = vec![0u8; limit + 1];
    let mut reader = Cursor::new(input);
    match read_curl_body(&mut reader, limit, false) {
      Err(CurlBodyReadError::TooLarge {
        observed,
        limit: got_limit,
      }) => {
        assert_eq!(got_limit, limit);
        assert_eq!(observed, limit + 1);
      }
      other => panic!("expected TooLarge error, got {other:?}"),
    }
  }

  #[test]
  fn body_reader_discards_redirect_bodies_without_enforcing_limit() {
    let input = vec![0u8; 32];
    let expected_len = input.len();
    let mut reader = Cursor::new(input);
    let body = read_curl_body(&mut reader, 0, true).expect("discard body");
    assert!(body.is_empty());
    assert_eq!(reader.position() as usize, expected_len);
  }
}
