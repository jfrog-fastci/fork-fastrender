use super::{
  DecodedAudioChunk, DecodedVideoFrame, MediaCodec, MediaError, MediaPacket, MediaResult,
  MediaTrackInfo,
};
#[cfg(feature = "codec_h264_openh264")]
use openh264::formats::YUVSource;

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
    MediaCodec::H264 => {
      #[cfg(feature = "codec_h264_openh264")]
      {
        Ok(Box::new(H264Decoder::from_codec_private(&track.codec_private)?))
      }
      #[cfg(not(feature = "codec_h264_openh264"))]
      {
        Err(MediaError::Unsupported(
          "`codec_h264_openh264` feature disabled (enable Cargo feature `codec_h264_openh264` or `media`)",
        ))
      }
    }
    MediaCodec::Vp9 => {
      #[cfg(feature = "codec_vp9_libvpx")]
      {
        let info = track
          .video
          .ok_or(MediaError::Unsupported("VP9 track missing video info"))?;
        Ok(Box::new(Vp9Decoder::new(info.width, info.height)))
      }
      #[cfg(not(feature = "codec_vp9_libvpx"))]
      {
        Err(MediaError::Unsupported(
          "`codec_vp9_libvpx` feature disabled (enable Cargo feature `codec_vp9_libvpx` or `media`)",
        ))
      }
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
    MediaCodec::Opus => Ok(Box::new(super::codecs::opus::OpusDecoder::new(
      &track.codec_private,
    )?)),
    _ => Err(MediaError::Unsupported("unsupported audio codec")),
  }
}

impl AudioDecoder for super::codecs::aac::AacDecoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedAudioChunk>> {
    let decoded = super::codecs::aac::AacDecoder::decode(self, packet)?;
    Ok(decoded.into_iter().collect())
  }
}

impl AudioDecoder for super::codecs::opus::OpusDecoder {
  fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedAudioChunk>> {
    let decoded = super::codecs::opus::OpusDecoder::decode(self, packet)?;
    Ok(decoded.into_iter().collect())
  }
}

// ============================================================================
// H264 (OpenH264)
// ============================================================================

#[cfg(feature = "codec_h264_openh264")]
#[derive(Clone, Debug)]
struct H264CodecConfig {
  nal_length_size: u8,
  sps: Vec<Vec<u8>>,
  pps: Vec<Vec<u8>>,
}

#[cfg(feature = "codec_h264_openh264")]
pub struct H264Decoder {
  decoder: openh264::decoder::Decoder,
  cfg: H264CodecConfig,
  sent_headers: bool,
  scratch: Vec<u8>,
}

#[cfg(feature = "codec_h264_openh264")]
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

#[cfg(feature = "codec_h264_openh264")]
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
#[cfg(feature = "codec_h264_openh264")]
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
#[cfg(feature = "codec_vp9_libvpx")]
pub struct Vp9Decoder {
  width: u32,
  height: u32,
}

#[cfg(feature = "codec_vp9_libvpx")]
impl Vp9Decoder {
  pub fn new(width: u32, height: u32) -> Self {
    Self { width, height }
  }
}

#[cfg(feature = "codec_vp9_libvpx")]
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
