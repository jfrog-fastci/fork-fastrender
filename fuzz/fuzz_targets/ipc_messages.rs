#![no_main]

use fastrender::ipc;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
  // Keep the input bounded so JSON parsing cannot be forced to scan arbitrarily large byte slices.
  let max_total = ipc::framing::MAX_IPC_MESSAGE_BYTES.saturating_add(ipc::framing::IPC_LENGTH_PREFIX_BYTES);
  let data = if data.len() > max_total { &data[..max_total] } else { data };

  // Split + validate the length-prefixed frame. This enforces MAX_IPC_MESSAGE_BYTES and never
  // allocates based on the declared length.
  let Ok(frame) = ipc::framing::decode_frame_from_bytes(data) else {
    return;
  };

  // Deserialize the JSON payload. If it successfully parses into the expected IPC message type,
  // validate protocol-level invariants.
  if let Ok(msg) = ipc::connection::decode_renderer_to_browser_json(frame.message_bytes) {
    let ctx = ipc::protocol::RendererToBrowserValidationContext {
      expected_protocol_version: ipc::protocol::IPC_PROTOCOL_VERSION,
      frame_buffers: None,
    };
    let _ = msg.validate(&ctx);
  }
});
