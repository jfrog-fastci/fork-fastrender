#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::audio::AudioRingBuffer;
use libfuzzer_sys::fuzz_target;

const MAX_OPS: usize = 256;
const MAX_SAMPLES_PER_OP: usize = 8192 * 8; // 8192 frames @ 8 channels
const MAX_CAPACITY: usize = 8192 * 8;

#[derive(Arbitrary, Debug)]
struct QueueInput {
  capacity: u16,
  ops: Vec<QueueOp>,
}

#[derive(Arbitrary, Debug)]
struct QueueOp {
  kind: u8,
  len: u16,
  gain_bits: u32,
  data: Vec<f32>,
}

fn sanitize_gain(bits: u32) -> f32 {
  let gain = f32::from_bits(bits);
  if gain.is_finite() {
    // Gains outside a reasonable range can cause extreme float values; clamp.
    gain.clamp(-4.0, 4.0)
  } else {
    0.0
  }
}

fuzz_target!(|input: QueueInput| {
  let capacity = (input.capacity as usize).max(1).min(MAX_CAPACITY);
  let rb = AudioRingBuffer::new(capacity);

  for op in input.ops.into_iter().take(MAX_OPS) {
    let len = (op.len as usize).min(MAX_SAMPLES_PER_OP);
    match op.kind % 2 {
      0 => {
        let mut data = op.data;
        if data.len() > MAX_SAMPLES_PER_OP {
          data.truncate(MAX_SAMPLES_PER_OP);
        }
        if len < data.len() {
          data.truncate(len);
        }
        let _ = rb.push(&data);
      }
      _ => {
        let mut dst = vec![0.0f32; len];
        let gain = sanitize_gain(op.gain_bits);
        rb.pop_add_into(&mut dst, gain);
      }
    }
  }
});
