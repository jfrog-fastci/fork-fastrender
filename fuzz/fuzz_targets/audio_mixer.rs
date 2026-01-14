#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::audio::{AudioMixer, AudioStreamId, AudioStreamParams, TimedAudioSegment};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

const MAX_STREAMS: usize = 16;
const MAX_OPS: usize = 256;
const MAX_FRAMES: usize = 8192;

#[derive(Arbitrary, Debug)]
struct MixerInput {
  sample_rate_hz: u32,
  channels: u8,
  max_buffered_ms: u16,
  stream_count: u8,
  ops: Vec<MixerOp>,
}

#[derive(Arbitrary, Debug)]
struct MixerOp {
  stream: u8,
  action: u8,
  frames: u16,
  device_pts_ns: u64,
  pts_offset_ns: i64,
  gain_bits: u32,
  segment_start_ns: u64,
  segment_sample_rate_hz: u32,
  segment_channels: u8,
  samples: Vec<f32>,
}

fn bounded_channels(channels: u8) -> u16 {
  ((channels as u16) % 8) + 1
}

fn bounded_sample_rate(sample_rate_hz: u32) -> u32 {
  (sample_rate_hz % 192_000).saturating_add(1)
}

fn sanitize_gain(bits: u32) -> f32 {
  let gain = f32::from_bits(bits);
  if gain.is_finite() {
    gain.clamp(-4.0, 4.0)
  } else {
    0.0
  }
}

fn clamp_offset_ns(offset_ns: i64) -> i64 {
  // Keep the absolute offset bounded so we don't spend time in huge Duration conversions.
  offset_ns.clamp(-10_000_000_000, 10_000_000_000) // +/- 10s
}

fn truncate_interleaved(mut samples: Vec<f32>, channels: usize, max_frames: usize) -> Vec<f32> {
  let max_samples = max_frames.saturating_mul(channels);
  if samples.len() > max_samples {
    samples.truncate(max_samples);
  }
  let usable = samples.len() - (samples.len() % channels);
  samples.truncate(usable);
  samples
}

fuzz_target!(|input: MixerInput| {
  let channels_u16 = bounded_channels(input.channels);
  let channels = channels_u16 as usize;
  let sample_rate_hz = bounded_sample_rate(input.sample_rate_hz);
  let max_buffered_ms = (input.max_buffered_ms as u64 % 2000).saturating_add(1);
  let max_buffered_duration = Duration::from_millis(max_buffered_ms);

  let mut mixer = AudioMixer::new(channels_u16, sample_rate_hz, max_buffered_duration);
  let stream_count = (input.stream_count as usize).min(MAX_STREAMS);
  let ids: Vec<AudioStreamId> = (0..stream_count).map(|i| (i as u64) + 1).collect();
  let mut present = vec![false; stream_count];

  for op in input.ops.into_iter().take(MAX_OPS) {
    let frames = (op.frames as usize).min(MAX_FRAMES);
    let needed = frames.saturating_mul(channels);
    let device_pts = Duration::from_nanos(op.device_pts_ns % 10_000_000_000);

    // If no streams were requested, just exercise mixing paths.
    if stream_count == 0 {
      let mut out = vec![0.0f32; needed];
      mixer.mix_into(&mut out, device_pts, frames);
      continue;
    }

    let idx = (op.stream as usize) % stream_count;
    let id = ids[idx];
    let action = op.action % 8;

    let params = AudioStreamParams {
      pts_offset_ns: clamp_offset_ns(op.pts_offset_ns),
      gain: sanitize_gain(op.gain_bits),
    };

    match action {
      0 => {
        // Add a stream if absent.
        if !present[idx] {
          mixer.add_stream(id, params);
          present[idx] = true;
        }
      }
      1 => {
        // Remove a stream (no-op if absent).
        mixer.remove_stream(id);
        present[idx] = false;
      }
      2 => {
        // Update params on an existing stream.
        if present[idx] {
          mixer.set_stream_params(id, params);
        }
      }
      3 => {
        // Push a segment into the stream queue.
        if present[idx] {
          if let Some(queue) = mixer.stream_queue_mut(id) {
            let seg_channels = bounded_channels(op.segment_channels);
            let seg_rate_hz = bounded_sample_rate(op.segment_sample_rate_hz);
            let seg_start = Duration::from_nanos(op.segment_start_ns % 10_000_000_000);
            let samples = truncate_interleaved(op.samples, seg_channels as usize, MAX_FRAMES);
            let seg = TimedAudioSegment {
              start_pts: seg_start,
              samples,
              channels: seg_channels,
              sample_rate: seg_rate_hz,
            };
            let _ = queue.push_segment(seg);
          }
        }
      }
      4 => {
        // Reset the stream cursor to an arbitrary time.
        if present[idx] {
          if let Some(queue) = mixer.stream_queue_mut(id) {
            queue.reset_cursor(Duration::from_nanos(op.segment_start_ns % 10_000_000_000));
          }
        }
      }
      5 => {
        // Clear stream buffered audio.
        if present[idx] {
          if let Some(queue) = mixer.stream_queue_mut(id) {
            queue.clear();
          }
        }
      }
      6 => {
        // Mix into a Duration-aligned output buffer.
        let mut out = vec![0.0f32; needed];
        mixer.mix_into(&mut out, device_pts, frames);
      }
      _ => {
        // Mix into a frame-aligned output buffer.
        let device_frame = op.device_pts_ns;
        let mut out = vec![0.0f32; needed];
        mixer.mix_into_frames(&mut out, device_frame, frames);
      }
    }
  }

  // Always do at least one final mix to hit the cleanup path.
  let frames = 256usize;
  let mut out = vec![0.0f32; frames * channels];
  mixer.mix_into(&mut out, Duration::from_nanos(0), frames);
});
