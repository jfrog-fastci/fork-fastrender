#![no_main]

use arbitrary::Arbitrary;
use fastrender::audio::{AudioMixer, AudioStreamHandle};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

const MAX_STREAMS: usize = 16;
const MAX_OPS: usize = 256;
const MAX_FRAMES: usize = 8192;

#[derive(Arbitrary, Debug)]
struct MixerInput {
  sample_rate_hz: u32,
  channels: u8,
  stream_count: u8,
  ops: Vec<MixerOp>,
}

#[derive(Arbitrary, Debug)]
struct MixerOp {
  stream: u8,
  action: u8,
  frames: u16,
  dur_nanos: u64,
  gain_bits: u32,
  samples: Vec<f32>,
}

fn bounded_channels(channels: u8) -> usize {
  (channels as usize % 8) + 1
}

fn bounded_sample_rate(sample_rate_hz: u32) -> u32 {
  (sample_rate_hz % 192_000).saturating_add(1)
}

fn sanitize_gain(bits: u32) -> f32 {
  let gain = f32::from_bits(bits);
  if gain.is_finite() {
    gain.clamp(0.0, 2.0)
  } else {
    0.0
  }
}

fn truncate_interleaved(mut samples: Vec<f32>, channels: usize) -> Vec<f32> {
  let max_samples = MAX_FRAMES.saturating_mul(channels);
  if samples.len() > max_samples {
    samples.truncate(max_samples);
  }
  let usable = samples.len() - (samples.len() % channels);
  samples.truncate(usable);
  samples
}

fuzz_target!(|input: MixerInput| {
  let channels = bounded_channels(input.channels);
  let sample_rate_hz = bounded_sample_rate(input.sample_rate_hz);

  let mixer = AudioMixer::new(sample_rate_hz, channels);
  let stream_count = (input.stream_count as usize).min(MAX_STREAMS);

  let mut streams: Vec<Option<AudioStreamHandle>> = (0..stream_count)
    .map(|_| Some(mixer.create_stream()))
    .collect();

  for op in input.ops.into_iter().take(MAX_OPS) {
    if streams.is_empty() {
      // Still exercise `mix_into` on an empty mixer.
      let frames = (op.frames as usize).min(MAX_FRAMES);
      let mut out = vec![0.0f32; frames.saturating_mul(channels)];
      mixer.mix_into(&mut out);
      continue;
    }

    let idx = (op.stream as usize) % streams.len();
    let action = op.action % 7;

    match action {
      0 => {
        if let Some(stream) = streams[idx].as_ref() {
          stream.play();
        }
      }
      1 => {
        if let Some(stream) = streams[idx].as_ref() {
          stream.pause();
        }
      }
      2 => {
        if let Some(stream) = streams[idx].as_ref() {
          stream.flush();
        }
      }
      3 => {
        if let Some(stream) = streams[idx].as_ref() {
          stream.seek_to(Duration::from_nanos(op.dur_nanos));
        }
      }
      4 => {
        if let Some(stream) = streams[idx].as_ref() {
          let gain = sanitize_gain(op.gain_bits);
          let mut samples = truncate_interleaved(op.samples, channels);
          for s in &mut samples {
            *s *= gain;
          }
          let _ = stream.enqueue_samples(samples);
        }
      }
      5 => {
        let frames = (op.frames as usize).min(MAX_FRAMES);
        let mut out = vec![0.0f32; frames.saturating_mul(channels)];

        // Seed the output buffer with some fuzz-controlled values to exercise the additive path.
        let seed = truncate_interleaved(op.samples, channels);
        let copy_len = seed.len().min(out.len());
        out[..copy_len].copy_from_slice(&seed[..copy_len]);

        mixer.mix_into(&mut out);
      }
      _ => {
        // Drop the stream handle to ensure `AudioMixer::mix_into` cleans up dead weak refs.
        streams[idx] = None;
      }
    }
  }

  // Always do at least one final mix to hit the cleanup path.
  let mut out = vec![0.0f32; 256 * channels];
  mixer.mix_into(&mut out);
});
