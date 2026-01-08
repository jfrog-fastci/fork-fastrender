use super::FetchedResource;
use crate::error::{Error, ImageError, Result};
use crate::fallible_vec_writer::FallibleVecWriter;
use base64::Engine;
use std::io::{self, Read, Write as _};

const DATA_URL_PREFIX: &str = "data:";
const DEFAULT_MEDIA_TYPE: &str = "text/plain";
const DEFAULT_CHARSET: &str = "charset=US-ASCII";

/// Decode a data: URL into bytes and content type following RFC 2397 semantics.
pub(crate) fn decode_data_url(url: &str) -> Result<FetchedResource> {
  decode_data_url_prefix(url, usize::MAX)
}

/// Decode up to the first `max_bytes` of a data: URL payload.
///
/// This is intended for probing image metadata without decoding the entire inline payload.
/// The returned bytes are always a prefix of the fully-decoded data.
pub(crate) fn decode_data_url_prefix(url: &str, max_bytes: usize) -> Result<FetchedResource> {
  if !url
    .get(..DATA_URL_PREFIX.len())
    .map(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX))
    .unwrap_or(false)
  {
    return Err(Error::Image(ImageError::InvalidDataUrl {
      reason: "URL does not start with 'data:'".to_string(),
    }));
  }

  let rest = &url[DATA_URL_PREFIX.len()..];
  let (metadata, data) = rest.split_once(',').ok_or_else(|| {
    Error::Image(ImageError::InvalidDataUrl {
      reason: "Missing comma in data URL".to_string(),
    })
  })?;

  let parsed = parse_metadata(metadata);

  let bytes = if max_bytes == 0 {
    Vec::new()
  } else if parsed.is_base64 {
    decode_base64_prefix(data, max_bytes)?
  } else {
    percent_decode_prefix(data, max_bytes)?
  };

  Ok(FetchedResource::new(bytes, Some(parsed.content_type)))
}

struct DataUrlMetadata {
  content_type: String,
  is_base64: bool,
}

fn parse_metadata(metadata: &str) -> DataUrlMetadata {
  let mut parts = metadata.split(';');
  let mediatype = parts.next().unwrap_or("").trim();

  let mut is_base64 = false;
  let mut params: Vec<String> = Vec::new();

  for param in parts {
    let trimmed = param.trim();
    if trimmed.is_empty() {
      continue;
    }
    if trimmed.eq_ignore_ascii_case("base64") {
      is_base64 = true;
      continue;
    }
    params.push(trimmed.to_string());
  }

  let mut content_type = if mediatype.is_empty() {
    DEFAULT_MEDIA_TYPE.to_string()
  } else {
    mediatype.to_string()
  };

  if mediatype.is_empty() && !has_charset(&params) {
    params.insert(0, DEFAULT_CHARSET.to_string());
  }

  if !params.is_empty() {
    content_type.push(';');
    content_type.push_str(&params.join(";"));
  }

  DataUrlMetadata {
    content_type,
    is_base64,
  }
}

fn has_charset(params: &[String]) -> bool {
  params.iter().any(|param| match param.split_once('=') {
    Some((name, _)) => name.trim().eq_ignore_ascii_case("charset"),
    None => param.trim().eq_ignore_ascii_case("charset"),
  })
}

enum Base64DecodeError {
  InvalidBase64(io::Error),
  Output(io::Error),
}

struct WhitespaceStrippingReader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> WhitespaceStrippingReader<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }
}

impl<'a> Read for WhitespaceStrippingReader<'a> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
      return Ok(0);
    }

    let mut written = 0usize;
    while written < buf.len() && self.pos < self.bytes.len() {
      let byte = self.bytes[self.pos];
      self.pos += 1;
      if byte.is_ascii_whitespace() {
        continue;
      }
      buf[written] = byte;
      written += 1;
    }
    Ok(written)
  }
}

fn decode_base64_prefix(data: &str, max_bytes: usize) -> Result<Vec<u8>> {
  if max_bytes == 0 {
    return Ok(Vec::new());
  }

  fn decode_base64_prefix_with_engine<E: base64::Engine>(
    data: &str,
    max_bytes: usize,
    engine: &E,
  ) -> std::result::Result<Vec<u8>, Base64DecodeError> {
    let mut stripped = WhitespaceStrippingReader::new(data.as_bytes());
    let mut decoder = base64::read::DecoderReader::new(&mut stripped, engine);

    let mut out = FallibleVecWriter::new(max_bytes, "data URL base64 decode");
    let mut written = 0usize;
    let mut buf = [0u8; 8 * 1024];
    while written < max_bytes {
      let remaining = max_bytes - written;
      let to_read = remaining.min(buf.len());
      let read = decoder
        .read(&mut buf[..to_read])
        .map_err(Base64DecodeError::InvalidBase64)?;
      if read == 0 {
        break;
      }
      out
        .write_all(&buf[..read])
        .map_err(Base64DecodeError::Output)?;
      written += read;
    }
    Ok(out.into_inner())
  }

  let mut last_invalid: Option<String> = None;

  for attempt in [
    &base64::engine::general_purpose::STANDARD,
    &base64::engine::general_purpose::STANDARD_NO_PAD,
    &base64::engine::general_purpose::URL_SAFE,
    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
  ] {
    match decode_base64_prefix_with_engine(data, max_bytes, attempt) {
      Ok(decoded) => return Ok(decoded),
      Err(Base64DecodeError::Output(err)) => {
        return Err(Error::Image(ImageError::InvalidDataUrl {
          reason: err.to_string(),
        }));
      }
      Err(Base64DecodeError::InvalidBase64(err)) => {
        last_invalid = Some(err.to_string());
      }
    }
  }

  Err(Error::Image(ImageError::InvalidDataUrl {
    reason: format!(
      "Invalid base64: {}",
      last_invalid.unwrap_or_else(|| "unknown error".to_string())
    ),
  }))
}

fn percent_decode_prefix(input: &str, max_bytes: usize) -> Result<Vec<u8>> {
  if max_bytes == 0 {
    return Ok(Vec::new());
  }

  let mut out = FallibleVecWriter::new(max_bytes, "data URL percent decode");
  let mut written = 0usize;
  let mut scratch = [0u8; 8 * 1024];
  let mut scratch_len = 0usize;
  let bytes = input.as_bytes();
  let mut i = 0;

  while i < bytes.len() && written < max_bytes {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      let hi = (bytes[i + 1] as char).to_digit(16);
      let lo = (bytes[i + 2] as char).to_digit(16);
      if let (Some(hi), Some(lo)) = (hi, lo) {
        scratch[scratch_len] = ((hi << 4) | lo) as u8;
        scratch_len += 1;
        i += 3;
        written += 1;
        if scratch_len == scratch.len() || written == max_bytes {
          out.write_all(&scratch[..scratch_len]).map_err(|err| {
            Error::Image(ImageError::InvalidDataUrl {
              reason: err.to_string(),
            })
          })?;
          scratch_len = 0;
        }
        continue;
      }
    }
    scratch[scratch_len] = bytes[i];
    scratch_len += 1;
    i += 1;
    written += 1;
    if scratch_len == scratch.len() || written == max_bytes {
      out.write_all(&scratch[..scratch_len]).map_err(|err| {
        Error::Image(ImageError::InvalidDataUrl {
          reason: err.to_string(),
        })
      })?;
      scratch_len = 0;
    }
  }

  if scratch_len != 0 {
    out.write_all(&scratch[..scratch_len]).map_err(|err| {
      Error::Image(ImageError::InvalidDataUrl {
        reason: err.to_string(),
      })
    })?;
  }

  Ok(out.into_inner())
}

pub(crate) fn encode_base64_data_url(media_type: &str, data: &[u8]) -> Option<String> {
  // Base64 expands input bytes by ~4/3; build the final URL in a single `String` allocation so we
  // can fail gracefully on OOM instead of aborting.
  let input_len = u64::try_from(data.len()).ok()?;
  let base64_len = input_len
    .checked_add(2)?
    .checked_div(3)?
    .checked_mul(4)?;
  let base64_len = usize::try_from(base64_len).ok()?;

  let total_len = "data:"
    .len()
    .checked_add(media_type.len())?
    .checked_add(";base64,".len())?
    .checked_add(base64_len)?;

  let mut url = String::new();
  url.try_reserve_exact(total_len).ok()?;
  url.push_str("data:");
  url.push_str(media_type);
  url.push_str(";base64,");

  let start = url.len();
  debug_assert_eq!(start, total_len - base64_len);
  unsafe {
    // SAFETY: We pre-reserved the final capacity and then extend the backing buffer to make room
    // for the base64 payload. `encode_slice` writes ASCII, so the final buffer is valid UTF-8.
    let buf = url.as_mut_vec();
    buf.set_len(start + base64_len);
    let written = match base64::engine::general_purpose::STANDARD
      .encode_slice(data, &mut buf[start..])
    {
      Ok(written) => written,
      Err(_) => {
        buf.set_len(start);
        return None;
      }
    };
    buf.truncate(start + written);
  }

  debug_assert_eq!(url.len(), total_len);
  Some(url)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decodes_data_url_case_insensitive_scheme() {
    let res = decode_data_url("DATA:text/plain;base64,aGk=").expect("decode data url");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn decodes_base64_without_padding() {
    let res = decode_data_url("data:text/plain;base64,aGk").expect("decode data url");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn decodes_base64_without_padding_with_whitespace() {
    let res = decode_data_url("data:text/plain;base64,a Gk").expect("decode data url");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn data_url_prefix_decodes_unpadded_base64() {
    let res = decode_data_url_prefix("data:text/plain;base64,aGk", 16).expect("decode prefix");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn data_url_prefix_decodes_unpadded_base64_with_whitespace() {
    let res = decode_data_url_prefix("data:text/plain;base64,a Gk\n", 16).expect("decode prefix");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn decodes_url_safe_base64_without_padding() {
    // 0xff encodes to `/w==` in standard base64, and `_w==` in URL-safe base64.
    let res =
      decode_data_url("data:application/octet-stream;base64,_w").expect("decode url-safe data url");
    assert_eq!(res.bytes, [0xff]);
  }

  #[test]
  fn data_url_prefix_decodes_url_safe_base64_without_padding() {
    let res =
      decode_data_url_prefix("data:application/octet-stream;base64,_w", 8).expect("decode prefix");
    assert_eq!(res.bytes, [0xff]);
  }

  #[test]
  fn data_url_prefix_decodes_base64_prefix() {
    let bytes: Vec<u8> = (0..128u8).collect();
    let url = encode_base64_data_url("application/octet-stream", &bytes).expect("encode data url");

    let decoded = decode_data_url_prefix(&url, 13).expect("decode prefix");
    assert_eq!(
      decoded.content_type.as_deref(),
      Some("application/octet-stream")
    );
    assert_eq!(decoded.bytes, bytes[..13]);
  }

  #[test]
  fn data_url_prefix_decodes_base64_with_whitespace() {
    let bytes: Vec<u8> = (0..64u8).collect();
    let mut url = encode_base64_data_url("application/octet-stream", &bytes).expect("encode data url");
    let (_, payload) = url.split_once(',').expect("comma");
    let injected = payload
      .as_bytes()
      .chunks(16)
      .map(|chunk| std::str::from_utf8(chunk).expect("utf8"))
      .collect::<Vec<_>>()
      .join("\n");
    url.truncate(url.find(',').expect("comma") + 1);
    url.push_str(&injected);

    let decoded = decode_data_url_prefix(&url, 17).expect("decode prefix");
    assert_eq!(decoded.bytes, bytes[..17]);
  }

  #[test]
  fn data_url_prefix_decodes_percent_prefix() {
    let url = "data:text/plain,hello%20world";
    let decoded = decode_data_url_prefix(url, 5).expect("decode prefix");
    assert_eq!(decoded.content_type.as_deref(), Some("text/plain"));
    assert_eq!(decoded.bytes, b"hello");
  }
}
