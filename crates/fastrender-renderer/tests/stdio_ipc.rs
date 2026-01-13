use bincode::Options;
use fastrender_ipc::{BrowserToRenderer, FrameId, RendererToBrowser, MAX_IPC_MESSAGE_BYTES};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct ChildKillGuard(Option<Child>);

impl ChildKillGuard {
  fn new(child: Child) -> Self {
    Self(Some(child))
  }

  fn take(&mut self) -> Child {
    self.0.take().expect("child already taken")
  }
}

impl Drop for ChildKillGuard {
  fn drop(&mut self) {
    if let Some(mut child) = self.0.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

#[test]
fn multiplex_two_frames_over_stdio() {
  let exe = env!("CARGO_BIN_EXE_fastrender-renderer");
  let child = Command::new(exe)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn renderer binary");
  let mut child = ChildKillGuard::new(child);

  let mut stdin = child.0.as_mut().unwrap().stdin.take().expect("child stdin");
  let stdout = child.0.as_mut().unwrap().stdout.take().expect("child stdout");
  let mut stderr = child.0.as_mut().unwrap().stderr.take().expect("child stderr");

  let (msg_tx, msg_rx) = mpsc::channel::<RendererToBrowser>();
  let reader = std::thread::spawn(move || {
    let mut stdout = stdout;
    loop {
      let mut len_prefix = [0u8; 4];
      match stdout.read_exact(&mut len_prefix) {
        Ok(()) => {}
        Err(err) => {
          if err.kind() == std::io::ErrorKind::UnexpectedEof {
            break;
          }
          break;
        }
      }

      let len = u32::from_le_bytes(len_prefix) as usize;
      if len == 0 || len > MAX_IPC_MESSAGE_BYTES {
        break;
      }

      let mut limited = stdout.by_ref().take(len as u64);
      let msg = match bincode::DefaultOptions::new()
        .with_limit(len as u64)
        .deserialize_from::<_, RendererToBrowser>(&mut limited)
      {
        Ok(msg) => msg,
        Err(_) => break,
      };
      let _ = std::io::copy(&mut limited, &mut std::io::sink());
      if msg_tx.send(msg).is_err() {
        break;
      }
    }
  });

  let frame_a = FrameId(1);
  let frame_b = FrameId(2);

  for msg in [
    BrowserToRenderer::CreateFrame { frame_id: frame_a },
    BrowserToRenderer::CreateFrame { frame_id: frame_b },
    BrowserToRenderer::Resize {
      frame_id: frame_a,
      width: 2,
      height: 2,
      dpr: 1.0,
    },
    BrowserToRenderer::Resize {
      frame_id: frame_b,
      width: 3,
      height: 1,
      dpr: 1.0,
    },
    BrowserToRenderer::RequestRepaint { frame_id: frame_a },
    BrowserToRenderer::RequestRepaint { frame_id: frame_b },
  ] {
    let opts = bincode::DefaultOptions::new();
    let len = opts.serialized_size(&msg).expect("size message");
    assert!(len > 0);
    assert!(len <= (u32::MAX as u64));
    assert!(len <= (MAX_IPC_MESSAGE_BYTES as u64));
    stdin
      .write_all(&(len as u32).to_le_bytes())
      .expect("write length prefix");
    opts
      .serialize_into(&mut stdin, &msg)
      .expect("write message payload");
  }
  stdin.flush().expect("flush stdin");

  let mut ready = vec![];
  for _ in 0..2 {
    let msg = msg_rx
      .recv_timeout(Duration::from_secs(2))
      .unwrap_or_else(|_| panic!("timed out waiting for FrameReady from renderer"));
    match msg {
      RendererToBrowser::FrameReady {
        frame_id,
        buffer,
        ..
      } => ready.push((frame_id, buffer)),
      other => panic!("unexpected message: {other:?}"),
    }
  }

  ready.sort_by_key(|(id, _)| id.0);
  assert_eq!(ready[0].0, frame_a);
  assert_eq!(ready[0].1.width, 2);
  assert_eq!(ready[0].1.height, 2);
  assert_eq!(ready[1].0, frame_b);
  assert_eq!(ready[1].1.width, 3);
  assert_eq!(ready[1].1.height, 1);

  // Request graceful shutdown.
  {
    let msg = BrowserToRenderer::Shutdown;
    let opts = bincode::DefaultOptions::new();
    let len = opts.serialized_size(&msg).expect("size shutdown");
    stdin
      .write_all(&(len as u32).to_le_bytes())
      .expect("write shutdown length");
    opts
      .serialize_into(&mut stdin, &msg)
      .expect("write shutdown payload");
  }
  stdin.flush().expect("flush shutdown");
  drop(stdin);

  let mut child_inner = child.take();
  let status = child_inner.wait().expect("wait for child exit");
  assert!(
    status.success(),
    "renderer exited with {status:?} (stderr={})",
    {
      let mut buf = String::new();
      let _ = stderr.read_to_string(&mut buf);
      buf
    }
  );

  drop(stderr);
  reader.join().expect("join stdout reader");
}
