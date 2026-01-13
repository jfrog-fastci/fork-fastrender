#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::audio::convert;
use libfuzzer_sys::fuzz_target;

const MAX_FRAMES: usize = 8192;
const MAX_CHUNKS: usize = 64;

#[derive(Arbitrary, Debug)]
struct ResamplerInput {
  in_rate_hz: u32,
  out_rate_hz: u32,
  channels: u8,
  max_output_frames: u16,
  samples: Vec<f32>,
  chunk_frames: Vec<u16>,
}

fn bounded_channels(channels: u8) -> usize {
  (channels as usize % 8) + 1
}

fn bounded_sample_rate(sample_rate_hz: u32) -> u32 {
  (sample_rate_hz % 192_000).saturating_add(1)
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

fuzz_target!(|input: ResamplerInput| {
  let channels = bounded_channels(input.channels);
  let in_rate_hz = bounded_sample_rate(input.in_rate_hz);
  let out_rate_hz = bounded_sample_rate(input.out_rate_hz);
  let max_output_frames = (input.max_output_frames as usize).min(MAX_FRAMES).max(1);

  let samples = truncate_interleaved(input.samples, channels);

  // Stateless helpers.
  let _ = convert::resample_nearest_interleaved_f32(
    &samples,
    channels,
    in_rate_hz,
    out_rate_hz,
    max_output_frames,
  );
  let _ = convert::resample_linear_interleaved_f32(
    &samples,
    channels,
    in_rate_hz,
    out_rate_hz,
    max_output_frames,
  );

  // Stateful resampler: feed in chunks to exercise buffering and fractional positions.
  let mut resampler = convert::LinearResampler::new(in_rate_hz, out_rate_hz, channels);
  let mut offset = 0usize;
  let mut out = Vec::new();

  for frames in input.chunk_frames.into_iter().take(MAX_CHUNKS) {
    let frames = (frames as usize).min(MAX_FRAMES);
    let chunk_len = frames.saturating_mul(channels);
    if chunk_len == 0 || offset >= samples.len() {
      break;
    }
    let end = offset.saturating_add(chunk_len).min(samples.len());
    let end = end - (end % channels);
    if end <= offset {
      break;
    }

    resampler.push_interleaved_f32(&samples[offset..end]);
    out.clear();
    resampler.render_into(&mut out, max_output_frames);

    offset = end;
  }

  // Drain any remaining buffered frames.
  out.clear();
  resampler.render_into(&mut out, max_output_frames);
});

