use super::{
  DecodedAudioChunk, DecodedVideoFrame, MediaCodec, MediaError, MediaPacket, MediaResult,
  MediaTrackInfo,
};
use openh264::formats::YUVSource;
use std::ffi::CStr;

/// A video decoder consumes demuxed packets and outputs 0..N decoded frames.
pub trait VideoDecoder: Send {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedVideoFrame>>;
}

/// An audio decoder consumes demuxed packets and outputs 0..N decoded chunks.
pub trait AudioDecoder: Send {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedAudioChunk>>;
}

pub fn create_video_decoder(track: &MediaTrackInfo) -> MediaResult<Box<dyn VideoDecoder>> {
  match &track.codec {
    MediaCodec::H264 => Ok(Box::new(H264Decoder::from_codec_private(&track.codec_private)?)),
    MediaCodec::Vp9 => {
      let info = track
        .video
        .ok_or(MediaError::Unsupported("VP9 track missing video info"))?;
      Ok(Box::new(Vp9Decoder::new(info.width, info.height)))
    }
    _ => Err(MediaError::Unsupported("unsupported video codec")),
  }
}

pub fn create_audio_decoder(track: &MediaTrackInfo) -> MediaResult<Box<dyn AudioDecoder>> {
  match &track.codec {
    MediaCodec::Aac => {
      let info = track
        .audio
        .ok_or(MediaError::Unsupported("AAC track missing audio info"))?;
      let decoder = super::codecs::aac::AacDecoder::new(
        &track.codec_private,
        info.sample_rate,
        info.channels,
      )?;
      Ok(Box::new(decoder))
    }
    MediaCodec::Opus => Ok(Box::new(OpusDecoder::new(&track.codec_private)?)),
    _ => Err(MediaError::Unsupported("unsupported audio codec")),
  }
}

impl AudioDecoder for super::codecs::aac::AacDecoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedAudioChunk>> {
    let decoded = super::codecs::aac::AacDecoder::decode(self, packet)?;
    Ok(decoded.into_iter().collect())
  }
}

// ============================================================================
// H264 (OpenH264)
// ============================================================================

#[derive(Clone, Debug)]
struct H264CodecConfig {
  nal_length_size: u8,
  sps: Vec<Vec<u8>>,
  pps: Vec<Vec<u8>>,
}

pub struct H264Decoder {
  decoder: openh264::decoder::Decoder,
  cfg: H264CodecConfig,
  sent_headers: bool,
  scratch: Vec<u8>,
}

impl H264Decoder {
  pub fn from_codec_private(codec_private: &[u8]) -> MediaResult<Self> {
    let cfg = parse_h264_codec_private(codec_private)?;
    Self::new(cfg)
  }

  fn new(cfg: H264CodecConfig) -> MediaResult<Self> {
    let decoder = openh264::decoder::Decoder::new()
      .map_err(|e| MediaError::Decode(format!("openh264: init failed: {e}")))?;
    Ok(Self {
      decoder,
      cfg,
      sent_headers: false,
      scratch: Vec::new(),
    })
  }

  fn mp4_to_annexb(&mut self, packet: &[u8]) -> MediaResult<()> {
    self.scratch.clear();

    if !self.sent_headers {
      for sps in &self.cfg.sps {
        self.scratch.extend([0, 0, 0, 1]);
        self.scratch.extend(sps);
      }
      for pps in &self.cfg.pps {
        self.scratch.extend([0, 0, 0, 1]);
        self.scratch.extend(pps);
      }
      self.sent_headers = true;
    }

    let len_size = usize::from(self.cfg.nal_length_size);
    if len_size == 0 || len_size > 4 {
      return Err(MediaError::Decode(format!(
        "h264: unsupported NAL length size: {}",
        self.cfg.nal_length_size
      )));
    }

    let mut i = 0usize;
    while i + len_size <= packet.len() {
      let mut nal_len: usize = 0;
      for _ in 0..len_size {
        nal_len = (nal_len << 8) | usize::from(packet[i]);
        i += 1;
      }
      if nal_len == 0 {
        continue;
      }
      if i + nal_len > packet.len() {
        return Err(MediaError::Decode("h264: truncated NAL unit".into()));
      }
      self.scratch.extend([0, 0, 0, 1]);
      self.scratch.extend_from_slice(&packet[i..i + nal_len]);
      i += nal_len;
    }

    Ok(())
  }
}

impl VideoDecoder for H264Decoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedVideoFrame>> {
    self.mp4_to_annexb(packet.as_slice())?;

    let decoded = self
      .decoder
      .decode(&self.scratch)
      .map_err(|e| MediaError::Decode(format!("openh264: decode failed: {e}")))?;

    let Some(yuv) = decoded else {
      return Ok(vec![]);
    };

    let (w, h) = yuv.dimensions();
    let mut rgba = vec![0u8; w * h * 4];
    yuv.write_rgba8(&mut rgba);

    Ok(vec![DecodedVideoFrame {
      pts_ns: packet.pts_ns,
      width: w as u32,
      height: h as u32,
      rgba,
    }])
  }
}

/// Parses H264 codec-private data (extradata) into SPS/PPS + NAL length size.
///
/// We currently encode this in a minimal custom format produced by the MP4 demuxer:
///
/// ```text
/// u8  nal_length_size
/// u8  sps_count
/// [sps_count] { u16be len, [len] bytes }
/// u8  pps_count
/// [pps_count] { u16be len, [len] bytes }
/// ```
fn parse_h264_codec_private(data: &[u8]) -> MediaResult<H264CodecConfig> {
  let mut i = 0usize;
  let read_u8 = |data: &[u8], i: &mut usize| -> MediaResult<u8> {
    let v = *data
      .get(*i)
      .ok_or(MediaError::Decode("h264 extradata truncated".into()))?;
    *i += 1;
    Ok(v)
  };
  let read_u16be = |data: &[u8], i: &mut usize| -> MediaResult<u16> {
    let hi = read_u8(data, i)?;
    let lo = read_u8(data, i)?;
    Ok(u16::from_be_bytes([hi, lo]))
  };

  let nal_length_size = read_u8(data, &mut i)?;
  if nal_length_size == 0 || nal_length_size > 4 {
    return Err(MediaError::Decode(format!(
      "h264: invalid NAL length size: {nal_length_size}"
    )));
  }

  let sps_count = read_u8(data, &mut i)? as usize;
  let mut sps = Vec::with_capacity(sps_count);
  for _ in 0..sps_count {
    let len = read_u16be(data, &mut i)? as usize;
    let end = i.saturating_add(len);
    if end > data.len() {
      return Err(MediaError::Decode("h264 extradata truncated (sps)".into()));
    }
    sps.push(data[i..end].to_vec());
    i = end;
  }

  let pps_count = read_u8(data, &mut i)? as usize;
  let mut pps = Vec::with_capacity(pps_count);
  for _ in 0..pps_count {
    let len = read_u16be(data, &mut i)? as usize;
    let end = i.saturating_add(len);
    if end > data.len() {
      return Err(MediaError::Decode("h264 extradata truncated (pps)".into()));
    }
    pps.push(data[i..end].to_vec());
    i = end;
  }

  Ok(H264CodecConfig {
    nal_length_size,
    sps,
    pps,
  })
}

// ============================================================================
// VP9 (placeholder)
// ============================================================================

/// Minimal VP9 decoder placeholder.
///
/// For now this yields a black frame with the correct dimensions. This is enough to exercise the
/// demux→decode plumbing; the actual VP9 bitstream decode can be implemented behind this trait
/// later.
pub struct Vp9Decoder {
  width: u32,
  height: u32,
}

impl Vp9Decoder {
  pub fn new(width: u32, height: u32) -> Self {
    Self { width, height }
  }
}

impl VideoDecoder for Vp9Decoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedVideoFrame>> {
    let rgba_len = self
      .width
      .saturating_mul(self.height)
      .saturating_mul(4) as usize;
    Ok(vec![DecodedVideoFrame {
      pts_ns: packet.pts_ns,
      width: self.width,
      height: self.height,
      rgba: vec![0u8; rgba_len],
    }])
  }
}

// ============================================================================
// Opus (libopus via audiopus_sys)
// ============================================================================

pub struct OpusDecoder {
  st: *mut audiopus_sys::OpusDecoder,
  channels: u16,
}

unsafe impl Send for OpusDecoder {}

impl Drop for OpusDecoder {
  fn drop(&mut self) {
    unsafe {
      if !self.st.is_null() {
        audiopus_sys::opus_decoder_destroy(self.st);
      }
    }
  }
}

impl OpusDecoder {
  pub fn new(opus_head: &[u8]) -> MediaResult<Self> {
    let channels = parse_opus_channels(opus_head)?;

    let mut err = 0i32;
    let st = unsafe { audiopus_sys::opus_decoder_create(48_000, channels as i32, &mut err) };
    if st.is_null() || err != audiopus_sys::OPUS_OK {
      return Err(MediaError::Decode(format!(
        "opus: opus_decoder_create failed: {}",
        opus_strerror(err)
      )));
    }

    Ok(Self { st, channels })
  }
}

impl AudioDecoder for OpusDecoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedAudioChunk>> {
    const MAX_FRAME_SIZE: usize = 5760; // 120ms @ 48kHz
    let data = packet.as_slice();
    let ch = self.channels as usize;
    let mut out = vec![0f32; MAX_FRAME_SIZE * ch];

    let n = unsafe {
      audiopus_sys::opus_decode_float(
        self.st,
        data.as_ptr(),
        data.len() as i32,
        out.as_mut_ptr(),
        MAX_FRAME_SIZE as i32,
        0,
      )
    };

    if n < 0 {
      return Err(MediaError::Decode(format!(
        "opus: decode failed: {}",
        opus_strerror(n)
      )));
    }

    let frames = n as usize;
    out.truncate(frames * ch);

    if out.is_empty() {
      return Ok(vec![]);
    }

    let duration_ns = if packet.duration_ns != 0 {
      packet.duration_ns
    } else {
      // Best-effort fallback when the container didn't provide an explicit duration.
      let frames = frames as u128;
      ((frames
        .saturating_mul(1_000_000_000u128)
        .checked_div(48_000u128)
        .unwrap_or(0))
        .min(u128::from(u64::MAX))) as u64
    };

    Ok(vec![DecodedAudioChunk {
      pts_ns: packet.pts_ns,
      duration_ns,
      sample_rate_hz: 48_000,
      channels: self.channels,
      samples: out,
    }])
  }
}

fn parse_opus_channels(opus_head: &[u8]) -> MediaResult<u16> {
  const OPUS_HEAD_MAGIC: &[u8] = b"OpusHead";
  if opus_head.len() < 10 || &opus_head[..8] != OPUS_HEAD_MAGIC {
    return Err(MediaError::Decode("invalid OpusHead (missing magic)".into()));
  }
  let channels = opus_head[9];
  if channels == 0 {
    return Err(MediaError::Decode("invalid OpusHead (channels=0)".into()));
  }
  Ok(u16::from(channels))
}

fn opus_strerror(code: i32) -> String {
  unsafe {
    let ptr = audiopus_sys::opus_strerror(code);
    if ptr.is_null() {
      format!("opus error {code}")
    } else {
      CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
  }
}
