use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

mod common;

#[inline]
fn xorshift32(state: &mut u32) -> u32 {
  // Deterministic, cheap RNG suitable for synthetic benchmark data.
  let mut x = *state;
  x ^= x << 13;
  x ^= x >> 17;
  x ^= x << 5;
  *state = x;
  x
}

fn gen_interleaved_f32(frames: usize, channels: usize, seed: u32) -> Vec<f32> {
  let mut out = vec![0.0f32; frames * channels];
  let mut state = seed;
  for sample in &mut out {
    let bits = xorshift32(&mut state) >> 8; // 24 bits -> exactly representable as f32 mantissa.
    let unit = bits as f32 * (1.0 / 16_777_216.0); // [0, 1)
    *sample = unit * 2.0 - 1.0; // [-1, 1)
  }
  out
}

fn mix_interleaved_f32(output: &mut [f32], inputs: &[&[f32]], gains: &[f32]) {
  debug_assert_eq!(inputs.len(), gains.len());
  output.fill(0.0);

  for (input, &gain) in inputs.iter().zip(gains.iter()) {
    debug_assert_eq!(input.len(), output.len());
    for (out, &sample) in output.iter_mut().zip(input.iter()) {
      *out += sample * gain;
    }
  }
}

fn required_input_frames_for_linear_resample(
  src_rate: u32,
  dst_rate: u32,
  out_frames: usize,
) -> usize {
  if out_frames == 0 {
    return 0;
  }
  if src_rate == dst_rate {
    return out_frames;
  }
  let step = src_rate as f64 / dst_rate as f64;
  let max_pos = (out_frames - 1) as f64 * step;
  let max_idx = max_pos.floor() as usize;
  max_idx + 2
}

fn resample_passthrough_interleaved_f32(output: &mut [f32], input: &[f32]) {
  debug_assert_eq!(output.len(), input.len());
  output.copy_from_slice(input);
}

fn resample_linear_interleaved_f32(
  output: &mut [f32],
  input: &[f32],
  channels: usize,
  src_rate: u32,
  dst_rate: u32,
) {
  debug_assert_ne!(src_rate, dst_rate, "use the passthrough fast path instead");
  debug_assert!(channels > 0);
  debug_assert_eq!(output.len() % channels, 0);
  debug_assert_eq!(input.len() % channels, 0);

  let out_frames = output.len() / channels;
  let step = src_rate as f64 / dst_rate as f64;

  for out_frame in 0..out_frames {
    let pos = out_frame as f64 * step;
    let idx = pos as usize; // floor(pos)
    let frac = (pos - idx as f64) as f32;

    let base = idx * channels;
    let next = base + channels;
    let out_base = out_frame * channels;
    debug_assert!(next + channels <= input.len());

    for ch in 0..channels {
      let a = input[base + ch];
      let b = input[next + ch];
      output[out_base + ch] = a + (b - a) * frac;
    }
  }
}

fn bench_mix_streams(c: &mut Criterion) {
  common::bench_print_config_once("audio_benches", &[]);
  let mut group = c.benchmark_group("audio_mix_f32");

  const CHANNELS: usize = 2; // stereo output
  const STREAM_COUNTS: &[usize] = &[2, 8, 16];
  const FRAME_COUNTS: &[usize] = &[512, 1024];

  for &frames in FRAME_COUNTS {
    for &streams in STREAM_COUNTS {
      let samples = frames * CHANNELS;
      let inputs: Vec<Vec<f32>> = (0..streams)
        .map(|idx| gen_interleaved_f32(frames, CHANNELS, 0xA001_0001 ^ idx as u32))
        .collect();
      let input_slices: Vec<&[f32]> = inputs.iter().map(|buf| buf.as_slice()).collect();
      let gains: Vec<f32> = (0..streams)
        .map(|idx| 0.1 + (idx as f32) * 0.9 / (streams.max(1) as f32))
        .collect();
      let mut output = vec![0.0f32; samples];

      group.bench_with_input(
        BenchmarkId::new(
          format!("{CHANNELS}ch_{frames}f"),
          format!("{streams}_streams"),
        ),
        &streams,
        |b, _| {
          b.iter(|| {
            mix_interleaved_f32(&mut output, &input_slices, &gains);
            black_box(output[0]);
            black_box(output[output.len() - 1]);
          })
        },
      );
    }
  }

  group.finish();
}

fn bench_resample(c: &mut Criterion) {
  common::bench_print_config_once("audio_benches", &[]);
  let mut group = c.benchmark_group("audio_resample_f32");

  const FRAME_COUNTS: &[usize] = &[512, 1024];
  const CHANNELS: &[usize] = &[1, 2];

  // 48k -> 48k: pass-through.
  for &frames in FRAME_COUNTS {
    for &channels in CHANNELS {
      let input = gen_interleaved_f32(frames, channels, 0xA100_0000 ^ channels as u32);
      let mut output = vec![0.0f32; frames * channels];

      group.bench_with_input(
        BenchmarkId::new(format!("{channels}ch_{frames}f"), "48k_to_48k_passthrough"),
        &channels,
        |b, _| {
          b.iter(|| {
            resample_passthrough_interleaved_f32(&mut output, &input);
            black_box(output[0]);
          })
        },
      );
    }
  }

  // 44.1k -> 48k: linear interpolation.
  const SRC_RATE: u32 = 44_100;
  const DST_RATE: u32 = 48_000;
  for &out_frames in FRAME_COUNTS {
    for &channels in CHANNELS {
      let in_frames = required_input_frames_for_linear_resample(SRC_RATE, DST_RATE, out_frames);
      let input = gen_interleaved_f32(in_frames, channels, 0xA200_0000 ^ channels as u32);
      let mut output = vec![0.0f32; out_frames * channels];

      group.bench_with_input(
        BenchmarkId::new(format!("{channels}ch_{out_frames}f"), "44.1k_to_48k_linear"),
        &channels,
        |b, _| {
          b.iter(|| {
            resample_linear_interleaved_f32(&mut output, &input, channels, SRC_RATE, DST_RATE);
            black_box(output[0]);
          })
        },
      );
    }
  }

  group.finish();
}

criterion_group! {
  name = audio_benches;
  config = common::perf_criterion();
  targets = bench_mix_streams, bench_resample
}
criterion_main!(audio_benches);
