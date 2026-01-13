#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::audio::AudioRingBuffer;
use libfuzzer_sys::fuzz_target;
use std::collections::VecDeque;

const MAX_CAPACITY: usize = 1024;
const MAX_OPS: usize = 256;
const MAX_SAMPLES_PER_PUSH: usize = 256;
const MAX_POP_LEN: usize = 256;

#[derive(Arbitrary, Debug)]
struct AudioRingBufferInput {
  capacity: u16,
  ops: Vec<AudioRingBufferOp>,
}

#[derive(Arbitrary, Debug)]
enum AudioRingBufferOp {
  Push(Vec<i16>),
  Pop { len: u16, gain: u8 },
}

fn gain_from_byte(b: u8) -> f32 {
  match b % 4 {
    0 => 0.0,
    1 => 0.5,
    2 => 1.0,
    _ => 2.0,
  }
}

fuzz_target!(|input: AudioRingBufferInput| {
  let capacity = ((input.capacity as usize) % MAX_CAPACITY).max(1);
  let rb = AudioRingBuffer::new(capacity);

  // Reference model: FIFO queue of the samples we believe are buffered.
  let mut model: VecDeque<f32> = VecDeque::new();

  for op in input.ops.into_iter().take(MAX_OPS) {
    match op {
      AudioRingBufferOp::Push(samples) => {
        let mut samples_f32: Vec<f32> = Vec::new();
        for s in samples.into_iter().take(MAX_SAMPLES_PER_PUSH) {
          samples_f32.push(s as f32);
        }

        let accepted = rb.push(&samples_f32);
        assert!(accepted <= samples_f32.len());
        assert!(accepted <= capacity);

        for s in samples_f32.into_iter().take(accepted) {
          model.push_back(s);
        }
      }
      AudioRingBufferOp::Pop { len, gain } => {
        let len = (len as usize) % MAX_POP_LEN;
        let gain = gain_from_byte(gain);
        let mut dst = vec![0.0_f32; len];

        rb.pop_add_into(&mut dst, gain);

        // `AudioRingBuffer::pop_add_into` must drain buffered samples even when the effective
        // gain is 0 (muted). Only an empty destination should be a no-op.
        let to_read = if len == 0 { 0 } else { model.len().min(len) };

        for i in 0..to_read {
          let expected = model[i] * gain;
          assert_eq!(dst[i], expected);
        }
        for i in to_read..len {
          assert_eq!(dst[i], 0.0);
        }

        for _ in 0..to_read {
          model.pop_front();
        }
      }
    }
  }
});
