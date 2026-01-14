//! FFmpeg CLI media backend (`ffprobe` + `ffmpeg`).
//!
//! This backend exists as a pragmatic fallback for environments where we cannot compile/link native
//! codec libraries (FFmpeg headers, openh264, libvpx, etc). It shells out to the system `ffprobe`
//! and `ffmpeg` binaries and decodes to:
//! - video: raw RGBA frames
//! - audio: interleaved f32 PCM (`f32le`)
//!
//! The API is intentionally minimal and synchronous; higher layers are expected to run it on a
//! background thread and cache decoded frames for paint.
#![allow(clippy::too_many_lines)]

use crate::error::RenderStage;
use crate::media::{
  DecodedAudioChunk, DecodedItem, DecodedVideoFrame, MediaAudioInfo, MediaBackend, MediaError,
  MediaResult, MediaSession, MediaVideoInfo,
};
use crate::media::video_limits;
use crate::render_control;
use serde::Deserialize;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const STDERR_MAX_BYTES: usize = 64 * 1024;
const FFPROBE_JSON_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_AUDIO_CHUNK_BYTES: usize = 8 * 1024 * 1024;

pub fn ffmpeg_available() -> bool {
  static AVAILABLE: OnceLock<bool> = OnceLock::new();
  *AVAILABLE.get_or_init(|| {
    Command::new("ffmpeg")
      .arg("-version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .map(|status| status.success())
      .unwrap_or(false)
  })
}

pub fn ffprobe_available() -> bool {
  static AVAILABLE: OnceLock<bool> = OnceLock::new();
  *AVAILABLE.get_or_init(|| {
    Command::new("ffprobe")
      .arg("-version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .map(|status| status.success())
      .unwrap_or(false)
  })
}

#[derive(Debug, Default)]
pub struct FfmpegCliBackend;

impl FfmpegCliBackend {
  #[must_use]
  pub fn new() -> Self {
    Self
  }
}

impl MediaBackend for FfmpegCliBackend {
  fn name(&self) -> &'static str {
    "ffmpeg_cli"
  }

  fn available(&self) -> bool {
    ffmpeg_available() && ffprobe_available()
  }

  fn open(&self, bytes: Arc<[u8]>) -> MediaResult<Box<dyn MediaSession>> {
    Ok(Box::new(FfmpegCliSession::new(bytes)?))
  }
}

#[derive(Debug, Clone)]
struct VideoStreamMeta {
  info: MediaVideoInfo,
  fps: f64,
  rate: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct ProbeMeta {
  duration_ns: Option<u64>,
  video: Option<VideoStreamMeta>,
  audio: Option<MediaAudioInfo>,
}

#[derive(Debug)]
struct StderrCapture {
  handle: Option<JoinHandle<String>>,
}

impl StderrCapture {
  fn spawn(mut stderr: ChildStderr) -> Self {
    let handle = thread::spawn(move || {
      let mut buf = [0u8; 8192];
      let mut out = Vec::new();
      loop {
        match stderr.read(&mut buf) {
          Ok(0) => break,
          Ok(n) => {
            if out.len() < STDERR_MAX_BYTES {
              let remaining = STDERR_MAX_BYTES - out.len();
              let take = remaining.min(n);
              out.extend_from_slice(&buf[..take]);
            }
          }
          Err(_) => break,
        }
      }
      String::from_utf8_lossy(&out).to_string()
    });
    Self {
      handle: Some(handle),
    }
  }

  fn join(&mut self) -> String {
    self
      .handle
      .take()
      .and_then(|h| h.join().ok())
      .unwrap_or_default()
  }
}

#[derive(Debug)]
struct DecoderProcess {
  kind: &'static str,
  child: Child,
  stdout: ChildStdout,
  stderr: StderrCapture,
}

impl DecoderProcess {
  fn spawn(mut cmd: Command, kind: &'static str) -> MediaResult<Self> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| {
      if err.kind() == io::ErrorKind::NotFound {
        MediaError::Unsupported("ffmpeg not available".into())
      } else {
        MediaError::Io(err)
      }
    })?;

    let stdout = child.stdout.take().ok_or_else(|| {
      let _ = child.kill();
      MediaError::Decode(format!("{kind} stdout pipe unavailable"))
    })?;
    #[cfg(unix)]
    set_nonblocking(&stdout, true)?;

    let stderr = child.stderr.take().ok_or_else(|| {
      let _ = child.kill();
      MediaError::Decode(format!("{kind} stderr pipe unavailable"))
    })?;

    Ok(Self {
      kind,
      child,
      stdout,
      stderr: StderrCapture::spawn(stderr),
    })
  }

  fn kill(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    let _ = self.stderr.join();
  }

  fn wait_for_exit(&mut self) -> Option<ExitStatus> {
    self.child.wait().ok()
  }

  fn take_stderr(&mut self) -> String {
    self.stderr.join()
  }

  fn read_exact_or_eof(
    &mut self,
    bytes: usize,
    allow_partial_eof: bool,
  ) -> MediaResult<Option<Vec<u8>>> {
    if bytes == 0 {
      return Ok(Some(Vec::new()));
    }

    if self.kind == "ffmpeg(video)" && bytes > video_limits::MAX_VIDEO_FRAME_BYTES {
      self.kill();
      return Err(MediaError::Decode(format!(
        "video frame size ({bytes} bytes) exceeds hard cap ({} bytes)",
        video_limits::MAX_VIDEO_FRAME_BYTES
      )));
    }
    if self.kind == "ffmpeg(audio)" && bytes > MAX_AUDIO_CHUNK_BYTES {
      self.kill();
      return Err(MediaError::Decode(format!(
        "audio chunk size ({bytes} bytes) exceeds hard cap ({MAX_AUDIO_CHUNK_BYTES} bytes)"
      )));
    }

    let mut out = vec![0u8; bytes];
    let mut filled = 0usize;
    let mut counter = 0usize;
    while filled < bytes {
      counter = counter.wrapping_add(1);
      if counter % 256 == 0 {
        if let Err(err) = render_control::check_root(RenderStage::Paint) {
          self.kill();
          return Err(MediaError::Render(err));
        }
      }

      match self.stdout.read(&mut out[filled..]) {
        Ok(0) => {
          if filled == 0 {
            let status = self.wait_for_exit();
            let stderr = self.take_stderr();
            if let Some(status) = status {
              if !status.success() {
                return Err(MediaError::Decode(format!(
                  "{} failed (exit={:?}): {}",
                  self.kind,
                  status.code(),
                  stderr.trim()
                )));
              }
            }
            return Ok(None);
          }

          if allow_partial_eof {
            out.truncate(filled);
            return Ok(Some(out));
          }

          let status = self.wait_for_exit();
          let stderr = self.take_stderr();
          return Err(MediaError::Decode(format!(
            "{} produced truncated output (got {filled}/{bytes} bytes; exit={:?}): {}",
            self.kind,
            status.and_then(|s| s.code()),
            stderr.trim()
          )));
        }
        Ok(n) => filled = filled.saturating_add(n),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
          #[cfg(unix)]
          wait_readable(&self.stdout, Some(Duration::from_millis(100)))?;
          #[cfg(not(unix))]
          std::thread::sleep(Duration::from_millis(1));
        }
        Err(err) => {
          self.kill();
          return Err(MediaError::Io(err));
        }
      }
    }

    Ok(Some(out))
  }
}

impl Drop for DecoderProcess {
  fn drop(&mut self) {
    self.kill();
  }
}

#[cfg(unix)]
fn set_nonblocking(stdout: &ChildStdout, enabled: bool) -> io::Result<()> {
  use std::os::unix::io::AsRawFd as _;
  let fd = stdout.as_raw_fd();
  unsafe {
    let flags = libc::fcntl(fd, libc::F_GETFL);
    if flags < 0 {
      return Err(io::Error::last_os_error());
    }
    let next = if enabled {
      flags | libc::O_NONBLOCK
    } else {
      flags & !libc::O_NONBLOCK
    };
    if libc::fcntl(fd, libc::F_SETFL, next) < 0 {
      return Err(io::Error::last_os_error());
    }
  }
  Ok(())
}

#[cfg(unix)]
fn wait_readable(stdout: &ChildStdout, timeout: Option<Duration>) -> io::Result<()> {
  use std::os::unix::io::AsRawFd as _;
  let fd = stdout.as_raw_fd();
  let mut pfd = libc::pollfd {
    fd,
    events: libc::POLLIN,
    revents: 0,
  };
  let timeout_ms = timeout
    .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
    .unwrap_or(-1);
  let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn parse_rate(value: &str) -> Option<(u64, u64)> {
  let trimmed = value.trim();
  if trimmed.is_empty() {
    return None;
  }
  let (num, den) = trimmed.split_once('/')?;
  let num = num.parse::<u64>().ok()?;
  let den = den.parse::<u64>().ok()?;
  if num == 0 || den == 0 {
    return None;
  }
  Some((num, den))
}

fn rate_to_f64((num, den): (u64, u64)) -> f64 {
  (num as f64) / (den as f64)
}

fn secs_str_to_ns(value: &str) -> Option<u64> {
  let secs = value.trim().parse::<f64>().ok()?;
  if !secs.is_finite() || secs < 0.0 {
    return None;
  }
  Some((secs * 1_000_000_000.0).round().clamp(0.0, u64::MAX as f64) as u64)
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
  #[serde(default)]
  streams: Vec<FfprobeStream>,
  format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
  duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
  codec_type: Option<String>,
  width: Option<u32>,
  height: Option<u32>,
  avg_frame_rate: Option<String>,
  r_frame_rate: Option<String>,
  sample_rate: Option<String>,
  channels: Option<u16>,
}

fn run_ffprobe_json(path: &Path) -> MediaResult<Vec<u8>> {
  if !ffprobe_available() {
    return Err(MediaError::Unsupported("ffprobe not available".into()));
  }

  let mut cmd = Command::new("ffprobe");
  cmd.args(["-v", "error", "-show_streams", "-show_format", "-of", "json"]);
  cmd.arg(path);
  cmd.stdin(Stdio::null());
  cmd.stdout(Stdio::piped());
  cmd.stderr(Stdio::piped());
  let mut child = cmd.spawn().map_err(|err| {
    if err.kind() == io::ErrorKind::NotFound {
      MediaError::Unsupported("ffprobe not available".into())
    } else {
      MediaError::Io(err)
    }
  })?;

  let mut stdout = child.stdout.take().ok_or_else(|| {
    let _ = child.kill();
    MediaError::Decode("ffprobe stdout pipe unavailable".to_string())
  })?;
  #[cfg(unix)]
  set_nonblocking(&stdout, true)?;

  let stderr = child.stderr.take().ok_or_else(|| {
    let _ = child.kill();
    MediaError::Decode("ffprobe stderr pipe unavailable".to_string())
  })?;
  let mut stderr_cap = StderrCapture::spawn(stderr);

  let mut out = Vec::new();
  let mut buf = [0u8; 8192];
  let mut counter = 0usize;
  loop {
    counter = counter.wrapping_add(1);
    if counter % 256 == 0 {
      if let Err(err) = render_control::check_root(RenderStage::Paint) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_cap.join();
        return Err(MediaError::Render(err));
      }
    }
    if out.len() > FFPROBE_JSON_MAX_BYTES {
      let _ = child.kill();
      let _ = child.wait();
      let _ = stderr_cap.join();
      return Err(MediaError::Decode(format!(
        "ffprobe json output exceeds hard cap ({FFPROBE_JSON_MAX_BYTES} bytes)"
      )));
    }
    match stdout.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => out.extend_from_slice(&buf[..n]),
      Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
        #[cfg(unix)]
        wait_readable(&stdout, Some(Duration::from_millis(100)))?;
        #[cfg(not(unix))]
        std::thread::sleep(Duration::from_millis(1));
      }
      Err(err) => {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_cap.join();
        return Err(MediaError::Io(err));
      }
    }
  }

  let status = child.wait().ok();
  let stderr = stderr_cap.join();
  if let Some(status) = status {
    if !status.success() {
      return Err(MediaError::Decode(format!(
        "ffprobe failed (exit={:?}): {}",
        status.code(),
        stderr.trim()
      )));
    }
  }

  Ok(out)
}

fn probe(path: &Path) -> MediaResult<ProbeMeta> {
  let raw = run_ffprobe_json(path)?;
  let parsed: FfprobeOutput = serde_json::from_slice(&raw)
    .map_err(|e| MediaError::Decode(format!("failed to parse ffprobe json: {e}")))?;

  let duration_ns = parsed
    .format
    .as_ref()
    .and_then(|f| f.duration.as_deref())
    .and_then(secs_str_to_ns);

  let video_stream = parsed
    .streams
    .iter()
    .find(|s| s.codec_type.as_deref() == Some("video"));
  let video = if let Some(stream) = video_stream {
    let width = stream
      .width
      .ok_or_else(|| MediaError::Decode("ffprobe missing video width".into()))?;
    let height = stream
      .height
      .ok_or_else(|| MediaError::Decode("ffprobe missing video height".into()))?;

    let rate = stream
      .avg_frame_rate
      .as_deref()
      .and_then(parse_rate)
      .or_else(|| stream.r_frame_rate.as_deref().and_then(parse_rate));
    let fps = rate.map(rate_to_f64).unwrap_or(30.0).max(1.0);

    Some(VideoStreamMeta {
      info: MediaVideoInfo { width, height },
      fps,
      rate,
    })
  } else {
    None
  };

  let audio_stream = parsed
    .streams
    .iter()
    .find(|s| s.codec_type.as_deref() == Some("audio"));
  let audio = if let Some(stream) = audio_stream {
    let sample_rate = stream
      .sample_rate
      .as_deref()
      .and_then(|s| s.trim().parse::<u32>().ok())
      .ok_or_else(|| MediaError::Decode("ffprobe missing audio sample_rate".into()))?;
    let channels = stream
      .channels
      .ok_or_else(|| MediaError::Decode("ffprobe missing audio channels".into()))?;
    Some(MediaAudioInfo { sample_rate, channels })
  } else {
    None
  };

  Ok(ProbeMeta {
    duration_ns,
    video,
    audio,
  })
}

fn audio_chunk_frames(sample_rate: u32) -> u32 {
  (sample_rate / 50).max(1)
}

#[derive(Debug)]
pub struct FfmpegCliSession {
  _temp_dir: tempfile::TempDir,
  path: PathBuf,
  meta: ProbeMeta,

  video: Option<DecoderProcess>,
  audio: Option<DecoderProcess>,

  seek_base_ns: u64,
  video_frame_index: u64,
  audio_frames_decoded: u64,

  pending_video: Option<DecodedVideoFrame>,
  pending_audio: Option<DecodedAudioChunk>,
}

impl FfmpegCliSession {
  pub fn new(bytes: Arc<[u8]>) -> MediaResult<Self> {
    if !ffmpeg_available() {
      return Err(MediaError::Unsupported("ffmpeg not available".into()));
    }
    if !ffprobe_available() {
      return Err(MediaError::Unsupported("ffprobe not available".into()));
    }

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("media.bin");
    std::fs::write(&path, bytes.as_ref())?;

    let meta = probe(&path)?;

    let mut session = Self {
      _temp_dir: dir,
      path,
      meta,
      video: None,
      audio: None,
      seek_base_ns: 0,
      video_frame_index: 0,
      audio_frames_decoded: 0,
      pending_video: None,
      pending_audio: None,
    };
    session.restart_decoders(Duration::ZERO)?;
    Ok(session)
  }

  fn restart_decoders(&mut self, seek: Duration) -> MediaResult<()> {
    self.stop_decoders();

    if let Some(vmeta) = self.meta.video.as_ref() {
      let frame_bytes = (vmeta.info.width as usize)
        .checked_mul(vmeta.info.height as usize)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| MediaError::Decode("video dimensions too large".to_string()))?;
      if frame_bytes > video_limits::MAX_VIDEO_FRAME_BYTES {
        return Err(MediaError::Decode(format!(
          "video frame size ({frame_bytes} bytes) exceeds hard cap ({} bytes)",
          video_limits::MAX_VIDEO_FRAME_BYTES
        )));
      }

      let mut cmd = Command::new("ffmpeg");
      cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
      if !seek.is_zero() {
        cmd.arg("-ss");
        cmd.arg(format!("{:.3}", seek.as_secs_f64()));
      }
      cmd.arg("-i");
      cmd.arg(&self.path);
      cmd.args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
      self.video = Some(DecoderProcess::spawn(cmd, "ffmpeg(video)")?);
    }

    if let Some(ameta) = self.meta.audio.as_ref() {
      let frames_per_chunk = audio_chunk_frames(ameta.sample_rate);
      let chunk_bytes = (frames_per_chunk as usize)
        .checked_mul(ameta.channels as usize)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| MediaError::Decode("audio parameters too large".to_string()))?;
      if chunk_bytes > MAX_AUDIO_CHUNK_BYTES {
        return Err(MediaError::Decode(format!(
          "audio chunk size ({chunk_bytes} bytes) exceeds hard cap ({MAX_AUDIO_CHUNK_BYTES} bytes)"
        )));
      }

      let mut cmd = Command::new("ffmpeg");
      cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
      if !seek.is_zero() {
        cmd.arg("-ss");
        cmd.arg(format!("{:.3}", seek.as_secs_f64()));
      }
      cmd.arg("-i");
      cmd.arg(&self.path);
      cmd.args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"]);
      self.audio = Some(DecoderProcess::spawn(cmd, "ffmpeg(audio)")?);
    }

    Ok(())
  }

  fn stop_decoders(&mut self) {
    if let Some(mut proc) = self.video.take() {
      proc.kill();
    }
    if let Some(mut proc) = self.audio.take() {
      proc.kill();
    }
  }

  fn frame_pts_ns(&self, frame_index: u64) -> u64 {
    let Some(vmeta) = self.meta.video.as_ref() else {
      return self.seek_base_ns;
    };

    if let Some((num, den)) = vmeta.rate {
      let offset_ns = ((frame_index as u128)
        .saturating_mul(den as u128)
        .saturating_mul(1_000_000_000u128)
        / (num as u128))
        .min(u128::from(u64::MAX)) as u64;
      return self.seek_base_ns.saturating_add(offset_ns);
    }

    let fps = vmeta.fps.max(1.0);
    self
      .seek_base_ns
      .saturating_add(((frame_index as f64) * 1_000_000_000.0 / fps) as u64)
  }

  fn read_video_frame(&mut self) -> MediaResult<Option<DecodedVideoFrame>> {
    let Some(vmeta) = self.meta.video.as_ref() else {
      return Ok(None);
    };
    let Some(proc) = self.video.as_mut() else {
      return Ok(None);
    };

    let frame_bytes = (vmeta.info.width as usize)
      .checked_mul(vmeta.info.height as usize)
      .and_then(|v| v.checked_mul(4))
      .ok_or_else(|| MediaError::Decode("video dimensions too large".to_string()))?;

    let Some(rgba) = proc.read_exact_or_eof(frame_bytes, false)? else {
      return Ok(None);
    };

    let frame_index = self.video_frame_index;
    self.video_frame_index = self.video_frame_index.saturating_add(1);

    Ok(Some(DecodedVideoFrame {
      pts_ns: self.frame_pts_ns(frame_index),
      width: vmeta.info.width,
      height: vmeta.info.height,
      rgba,
    }))
  }

  fn read_audio_chunk(&mut self) -> MediaResult<Option<DecodedAudioChunk>> {
    let Some(ameta) = self.meta.audio.as_ref() else {
      return Ok(None);
    };
    let Some(proc) = self.audio.as_mut() else {
      return Ok(None);
    };

    let frames_target = audio_chunk_frames(ameta.sample_rate) as usize;
    let bytes_target = frames_target
      .checked_mul(ameta.channels as usize)
      .and_then(|v| v.checked_mul(4))
      .ok_or_else(|| MediaError::Decode("audio parameters too large".to_string()))?;

    let Some(raw) = proc.read_exact_or_eof(bytes_target, true)? else {
      return Ok(None);
    };
    if raw.is_empty() {
      return Ok(None);
    }

    if raw.len() % 4 != 0 {
      proc.kill();
      return Err(MediaError::Decode(format!(
        "audio output is not aligned to f32 samples ({} bytes)",
        raw.len()
      )));
    }
    let bytes_per_frame = (ameta.channels as usize)
      .checked_mul(4)
      .ok_or_else(|| MediaError::Decode("audio channels too large".to_string()))?;
    if bytes_per_frame == 0 || raw.len() % bytes_per_frame != 0 {
      proc.kill();
      return Err(MediaError::Decode(format!(
        "audio output size ({}) is not aligned to channel count ({})",
        raw.len(),
        ameta.channels
      )));
    }

    let frames_in_chunk = (raw.len() / bytes_per_frame) as u64;
    let pts_ns = self.seek_base_ns.saturating_add(
      ((self.audio_frames_decoded as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(ameta.sample_rate as u128)
        .unwrap_or(0)
        .min(u128::from(u64::MAX))) as u64,
    );
    self.audio_frames_decoded = self.audio_frames_decoded.saturating_add(frames_in_chunk);

    let duration_ns = ((frames_in_chunk as u128)
      .saturating_mul(1_000_000_000u128)
      .checked_div(ameta.sample_rate as u128)
      .unwrap_or(0)
      .min(u128::from(u64::MAX))) as u64;

    let mut samples = Vec::with_capacity(raw.len() / 4);
    for chunk in raw.chunks_exact(4) {
      samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }

    Ok(Some(DecodedAudioChunk {
      pts_ns,
      duration_ns,
      sample_rate_hz: ameta.sample_rate,
      channels: ameta.channels,
      samples,
    }))
  }
}

impl Drop for FfmpegCliSession {
  fn drop(&mut self) {
    self.stop_decoders();
  }
}

impl MediaSession for FfmpegCliSession {
  fn next_decoded(&mut self) -> MediaResult<Option<DecodedItem>> {
    if self.pending_video.is_none() {
      self.pending_video = self.read_video_frame()?;
    }
    if self.pending_audio.is_none() {
      self.pending_audio = self.read_audio_chunk()?;
    }

    match (self.pending_video.as_ref(), self.pending_audio.as_ref()) {
      (None, None) => Ok(None),
      (Some(_), None) => {
        let v = self.pending_video.take().expect("checked Some");
        Ok(Some(DecodedItem::Video(v)))
      }
      (None, Some(_)) => {
        let a = self.pending_audio.take().expect("checked Some");
        Ok(Some(DecodedItem::Audio(a)))
      }
      (Some(v), Some(a)) => {
        if v.pts_ns <= a.pts_ns {
          let v = self.pending_video.take().expect("checked Some");
          Ok(Some(DecodedItem::Video(v)))
        } else {
          let a = self.pending_audio.take().expect("checked Some");
          Ok(Some(DecodedItem::Audio(a)))
        }
      }
    }
  }

  fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    self.seek_base_ns = time_ns;
    self.video_frame_index = 0;
    self.audio_frames_decoded = 0;
    self.pending_video = None;
    self.pending_audio = None;
    self.restart_decoders(Duration::from_nanos(time_ns))?;
    Ok(())
  }
}
