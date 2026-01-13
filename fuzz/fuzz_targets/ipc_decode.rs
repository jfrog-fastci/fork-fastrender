#![no_main]

use fastrender::ipc;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = ipc::MAX_IPC_MESSAGE_BYTES;

fuzz_target!(|data: &[u8]| {
  // Keep the harness bounded: production decoding is size-limited, but truncating the fuzzer input
  // avoids spending cycles parsing megabytes of trailing garbage.
  let data = if data.len() > MAX_INPUT_BYTES {
    &data[..MAX_INPUT_BYTES]
  } else {
    data
  };

  // Browser ↔ renderer protocol.
  let _ = ipc::decode_bincode_payload::<ipc::protocol::BrowserToRenderer>(data);
  let _ = ipc::decode_bincode_payload::<ipc::protocol::RendererToBrowser>(data);
  let _ = ipc::decode_bincode_payload::<ipc::protocol::renderer::BrowserToRenderer>(data);
  let _ = ipc::decode_bincode_payload::<ipc::protocol::renderer::RendererToBrowser>(data);

  // Browser ↔ network-process protocol.
  let _ = ipc::decode_bincode_payload::<ipc::protocol::network::BrowserToNetwork>(data);
  let _ = ipc::decode_bincode_payload::<ipc::protocol::network::NetworkToBrowser>(data);
});
