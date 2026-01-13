#![cfg(feature = "direct_websocket")]

use fastrender::ipc::IpcError;
use fastrender::ipc::websocket::{WebSocketCommand, WebSocketEvent};
use fastrender::js::window_websocket::{
  read_websocket_ipc_command_frame, read_websocket_ipc_event_frame, write_websocket_ipc_command_frame,
  write_websocket_ipc_event_frame, WebSocketIpcCommand, WebSocketIpcEvent, MAX_WEBSOCKET_IPC_FRAME_BYTES,
};
use std::io::Cursor;

#[test]
fn websocket_ipc_command_frame_roundtrips() {
  let cmd = WebSocketIpcCommand::WebSocket {
    conn_id: 1,
    cmd: WebSocketCommand::SendBinary { data: vec![1, 2, 3] },
  };
  let mut buf = Vec::new();
  write_websocket_ipc_command_frame(&mut buf, &cmd).expect("write command frame");

  let mut cursor = Cursor::new(buf);
  let got = read_websocket_ipc_command_frame(&mut cursor).expect("read command frame");
  assert_eq!(got, cmd);
}

#[test]
fn websocket_ipc_event_frame_roundtrips() {
  let event = WebSocketIpcEvent::WebSocket {
    conn_id: 7,
    event: WebSocketEvent::MessageBinary { data: vec![9, 8, 7] },
  };
  let mut buf = Vec::new();
  write_websocket_ipc_event_frame(&mut buf, &event).expect("write event frame");

  let mut cursor = Cursor::new(buf);
  let got = read_websocket_ipc_event_frame(&mut cursor).expect("read event frame");
  assert_eq!(got, event);
}

#[test]
fn websocket_ipc_rejects_frame_len_over_cap_without_allocating_payload() {
  // If the decoder attempts to allocate this payload size, the test process will likely OOM/abort.
  let oversized_len: u32 = (MAX_WEBSOCKET_IPC_FRAME_BYTES + 1)
    .try_into()
    .expect("MAX_WEBSOCKET_IPC_FRAME_BYTES should fit in u32");
  let mut buf = Vec::new();
  buf.extend_from_slice(&oversized_len.to_le_bytes());

  let mut cursor = Cursor::new(buf);
  let err = read_websocket_ipc_command_frame(&mut cursor).unwrap_err();
  assert!(
    matches!(err, IpcError::MessageTooLarge { .. }),
    "unexpected error: {err:?}"
  );
}
