use fastrender::media::audio::legacy::{audio_backend_from_env, AudioBackend};
use fastrender::debug::runtime::{with_runtime_toggles, RuntimeToggles};
use fastrender::clock::VirtualClock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn wav_audio_backend_writes_valid_header_and_length() {
  let tmp = tempfile::tempdir().expect("tempdir");
  let wav_path = tmp.path().join("out.wav");

  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_AUDIO_BACKEND".to_string(), "wav".to_string()),
    (
      "FASTR_AUDIO_WAV_PATH".to_string(),
      wav_path.to_string_lossy().to_string(),
    ),
  ]));

  let sample_rate_hz = 48_000u32;
  let channels = 2usize;

  // Render 10ms of audio.
  let render_duration = Duration::from_millis(10);
  let frames = (u128::from(sample_rate_hz) * render_duration.as_nanos() / 1_000_000_000u128)
    as usize;
  assert_eq!(frames, 480);

  let mut tone = Vec::with_capacity(frames * channels);
  for _ in 0..frames {
    tone.push(0.25);
    tone.push(-0.25);
  }

  with_runtime_toggles(Arc::new(toggles), || {
    let clock = VirtualClock::new();
    let backend =
      audio_backend_from_env(sample_rate_hz, channels).expect("create backend from env");
    let stream = backend.create_stream();
    stream.enqueue_samples(tone).expect("enqueue samples");
    stream.play();

    let mut last_time = Duration::ZERO;
    clock.advance(render_duration);
    backend
      .render_for_clock(&clock, &mut last_time)
      .expect("render");
    // Drop backend to finalize WAV header.
  });

  assert!(wav_path.exists(), "expected WAV file to be created");
  let bytes = std::fs::read(&wav_path).expect("read wav");
  assert!(bytes.len() >= 44, "wav too short ({} bytes)", bytes.len());

  assert_eq!(&bytes[0..4], b"RIFF");
  assert_eq!(&bytes[8..12], b"WAVE");
  assert_eq!(&bytes[12..16], b"fmt ");
  assert_eq!(&bytes[36..40], b"data");

  let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
  let expected_data_bytes = (frames * channels * 2) as u32;
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
