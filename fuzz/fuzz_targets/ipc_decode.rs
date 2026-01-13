#![no_main]

use bincode::Options;
use fastrender_ipc as ipc;
use libfuzzer_sys::fuzz_target;
use serde::de::DeserializeOwned;

const MAX_INPUT_BYTES: usize = ipc::MAX_IPC_MESSAGE_BYTES;

#[inline]
fn decode_with_production_bincode_opts<T: DeserializeOwned>(payload: &[u8]) {
  if payload.len() > MAX_INPUT_BYTES {
    return;
  }

  // Match the stdio IPC transport in `crates/fastrender-renderer`: the bincode byte limit is set
  // to the frame length (and the frame length itself is bounded by `MAX_IPC_MESSAGE_BYTES`).
  let opts = bincode::DefaultOptions::new().with_limit(payload.len() as u64);
  let mut cursor = std::io::Cursor::new(payload);
  let Ok(_msg) = opts.deserialize_from::<_, T>(&mut cursor) else {
    return;
  };
  // Production treats trailing bytes as protocol desync.
  if cursor.position() != payload.len() as u64 {
    return;
  }
}

fuzz_target!(|data: &[u8]| {
  // Keep the harness bounded: production decoding is size-limited, but truncating the fuzzer input
  // avoids spending cycles parsing megabytes of trailing garbage.
  let data = if data.len() > MAX_INPUT_BYTES {
    &data[..MAX_INPUT_BYTES]
  } else {
    data
  };

  // Browser ↔ renderer (multiprocess) messages.
  decode_with_production_bincode_opts::<ipc::BrowserToRenderer>(data);
  decode_with_production_bincode_opts::<ipc::RendererToBrowser>(data);

  // Renderer ↔ network-process messages.
  decode_with_production_bincode_opts::<ipc::WebSocketCommand>(data);
  decode_with_production_bincode_opts::<ipc::WebSocketEvent>(data);
});
