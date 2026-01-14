#![cfg(feature = "audio_wav")]

use fastrender::media::audio::{
  duration_to_frames_floor, AudioBackend, AudioStreamConfig, WavAudioBackend,
};
use std::time::Duration;

#[test]
fn wav_audio_backend_writes_valid_header_and_length() {
  let tmp = tempfile::tempdir().expect("tempdir");
  let wav_path = tmp.path().join("out.wav");

  // Render 10ms of audio.
  let render_duration = Duration::from_millis(10);
  let sample_rate_hz = 48_000u32;
  let channels = 2u16;
  let frames = duration_to_frames_floor(sample_rate_hz, render_duration) as usize;
  assert_eq!(frames, 480);

  {
    let output_config = AudioStreamConfig::new(sample_rate_hz, channels);
    let backend = WavAudioBackend::new_with_output_config(
      &wav_path,
      output_config,
      Duration::from_secs(1),
    )
    .expect("create wav backend");
    let sink = backend.create_sink();
    let config = backend.output_config();
    assert_eq!(config.sample_rate_hz, sample_rate_hz);
    assert_eq!(config.channels, channels);

    let channels_usize = usize::from(channels);
    let mut tone = Vec::with_capacity(frames * channels_usize);
    for _ in 0..frames {
      // Interleaved stereo: L then R.
      tone.push(0.25);
      tone.push(-0.25);
    }
    let accepted = sink.push_interleaved_f32(&tone);
    assert_eq!(accepted, tone.len());

    backend.render(frames).expect("render");
    // Drop backend to flush + finalize file handle.
  }

  assert!(wav_path.exists(), "expected WAV file to be created");
  let bytes = std::fs::read(&wav_path).expect("read wav");
  assert!(bytes.len() >= 44, "wav too short ({} bytes)", bytes.len());

  assert_eq!(&bytes[0..4], b"RIFF");
  assert_eq!(&bytes[8..12], b"WAVE");
  assert_eq!(&bytes[12..16], b"fmt ");
  assert_eq!(&bytes[36..40], b"data");

  let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
  let expected_data_bytes = (frames * channels_usize * 2) as u32;
  assert_eq!(
    data_bytes, expected_data_bytes,
    "unexpected WAV data chunk length"
  );

  let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
  assert_eq!(
    riff_size,
    36 + expected_data_bytes,
    "unexpected RIFF chunk size"
  );
  assert_eq!(
    bytes.len(),
    44 + expected_data_bytes as usize,
    "unexpected file size"
  );
}
