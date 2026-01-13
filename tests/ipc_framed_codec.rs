use std::io;
use std::io::Cursor;

use fastrender::ipc::framed_codec;
use fastrender::ipc::protocol::network::BrowserToNetwork;

#[test]
fn framed_codec_roundtrip_browser_to_network_message() {
  let msg = BrowserToNetwork::Fetch {
    request_id: 1,
    url: "https://example.com/".to_owned(),
  };

  let mut buf = Vec::new();
  framed_codec::write_msg(&mut buf, &msg).unwrap();

  let mut cursor = Cursor::new(buf);
  let decoded: BrowserToNetwork = framed_codec::read_msg(&mut cursor).unwrap();
  assert_eq!(decoded, msg);
  assert_eq!(cursor.position() as usize, cursor.get_ref().len());
}

#[test]
fn framed_codec_rejects_oversized_frame_without_allocating_payload() {
  struct LimitedRead {
    buf: [u8; 4],
    pos: usize,
  }

  impl io::Read for LimitedRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
      if self.pos >= self.buf.len() {
        panic!("read_msg attempted to read beyond the length prefix");
      }
      let n = (self.buf.len() - self.pos).min(out.len());
      out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
      self.pos += n;
      Ok(n)
    }
  }

  let mut r = LimitedRead {
    buf: u32::MAX.to_le_bytes(),
    pos: 0,
  };

  let err = framed_codec::read_msg::<_, BrowserToNetwork>(&mut r).unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn framed_codec_errors_on_truncated_frame() {
  let declared_len: u32 = 10;
  let mut bytes = Vec::new();
  bytes.extend_from_slice(&declared_len.to_le_bytes());
  bytes.extend_from_slice(&[1, 2, 3, 4, 5]);

  let mut cursor = Cursor::new(bytes);
  let err = framed_codec::read_msg::<_, BrowserToNetwork>(&mut cursor).unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}
