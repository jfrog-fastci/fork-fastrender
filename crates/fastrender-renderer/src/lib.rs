#![forbid(unsafe_code)]

use fastrender_ipc::{BrowserToRenderer, FrameBuffer, FrameId, IpcTransport, RendererToBrowser};
use std::collections::HashMap;

const DEFAULT_VIEWPORT: (u32, u32) = (800, 600);
const DEFAULT_DPR: f32 = 1.0;

// Keep allocations bounded even if the browser sends a pathological `Resize`.
// 256 MiB per frame is plenty for now: 4096*4096*4 = 64 MiB.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FrameState {
  pub url: Option<String>,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
}

impl FrameState {
  pub fn new() -> Self {
    Self {
      url: None,
      viewport_css: DEFAULT_VIEWPORT,
      dpr: DEFAULT_DPR,
    }
  }

  pub fn render_placeholder(&self, frame_id: FrameId) -> Result<FrameBuffer, String> {
    let (width, height) = self.viewport_css;
    let len = (width as usize)
      .checked_mul(height as usize)
      .and_then(|px| px.checked_mul(4))
      .ok_or_else(|| "viewport size overflow".to_string())?;

    if len > MAX_FRAME_BYTES {
      return Err(format!(
        "requested frame buffer too large: {width}x{height} => {len} bytes"
      ));
    }

    // Deterministic per-frame fill color to help catch cross-talk in tests/debugging.
    let id = frame_id.0;
    let r = (id & 0xFF) as u8;
    let g = ((id >> 8) & 0xFF) as u8;
    let b = ((id >> 16) & 0xFF) as u8;
    let a = 0xFF;

    let mut rgba8 = vec![0u8; len];
    for px in rgba8.chunks_exact_mut(4) {
      px[0] = r;
      px[1] = g;
      px[2] = b;
      px[3] = a;
    }

    Ok(FrameBuffer {
      width,
      height,
      rgba8,
    })
  }
}

pub struct RendererMainLoop<T: IpcTransport> {
  transport: T,
  frames: HashMap<FrameId, FrameState>,
}

impl<T: IpcTransport> RendererMainLoop<T> {
  pub fn new(transport: T) -> Self {
    Self {
      transport,
      frames: HashMap::new(),
    }
  }

  pub fn run(mut self) -> Result<(), T::Error> {
    while let Some(msg) = self.transport.recv()? {
      match msg {
        BrowserToRenderer::CreateFrame { frame_id } => {
          self.frames.insert(frame_id, FrameState::new());
        }
        BrowserToRenderer::DestroyFrame { frame_id } => {
          self.frames.remove(&frame_id);
        }
        BrowserToRenderer::Navigate { frame_id, url } => {
          if let Some(frame) = self.frames.get_mut(&frame_id) {
            frame.url = Some(url);
          } else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "Navigate for unknown frame".to_string(),
            });
          }
        }
        BrowserToRenderer::Resize {
          frame_id,
          width,
          height,
          dpr,
        } => {
          if let Some(frame) = self.frames.get_mut(&frame_id) {
            frame.viewport_css = (width, height);
            frame.dpr = dpr;
          } else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "Resize for unknown frame".to_string(),
            });
          }
        }
        BrowserToRenderer::RequestRepaint { frame_id } => {
          let Some(frame) = self.frames.get(&frame_id) else {
            let _ = self.transport.send(RendererToBrowser::Error {
              frame_id: Some(frame_id),
              message: "RequestRepaint for unknown frame".to_string(),
            });
            continue;
          };

          match frame.render_placeholder(frame_id) {
            Ok(buffer) => {
              self.transport
                .send(RendererToBrowser::FrameReady { frame_id, buffer })?;
            }
            Err(message) => {
              let _ = self.transport.send(RendererToBrowser::Error {
                frame_id: Some(frame_id),
                message,
              });
            }
          }
        }
        BrowserToRenderer::Shutdown => break,
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;
  use std::time::Duration;

  struct ChannelTransport {
    rx: mpsc::Receiver<BrowserToRenderer>,
    tx: mpsc::Sender<RendererToBrowser>,
  }

  impl IpcTransport for ChannelTransport {
    type Error = ();

    fn recv(&mut self) -> Result<Option<BrowserToRenderer>, Self::Error> {
      match self.rx.recv() {
        Ok(msg) => Ok(Some(msg)),
        Err(_) => Ok(None),
      }
    }

    fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error> {
      self.tx.send(msg).map_err(|_| ())
    }
  }

  #[test]
  fn multiplex_two_frames_in_one_process() {
    let (to_renderer_tx, to_renderer_rx) = mpsc::channel();
    let (to_browser_tx, to_browser_rx) = mpsc::channel();

    let join = std::thread::spawn(move || {
      let transport = ChannelTransport {
        rx: to_renderer_rx,
        tx: to_browser_tx,
      };
      RendererMainLoop::new(transport).run().unwrap();
    });

    let frame_a = FrameId(1);
    let frame_b = FrameId(2);

    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame_a })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::CreateFrame { frame_id: frame_b })
      .unwrap();

    // Use different sizes to catch accidental cross-talk.
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame_a,
        width: 2,
        height: 2,
        dpr: 1.0,
      })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::Resize {
        frame_id: frame_b,
        width: 3,
        height: 1,
        dpr: 1.0,
      })
      .unwrap();

    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame_a })
      .unwrap();
    to_renderer_tx
      .send(BrowserToRenderer::RequestRepaint { frame_id: frame_b })
      .unwrap();

    let mut ready = vec![];
    for _ in 0..2 {
      let msg = to_browser_rx.recv_timeout(Duration::from_secs(1)).unwrap();
      match msg {
        RendererToBrowser::FrameReady { frame_id, buffer } => ready.push((frame_id, buffer)),
        other => panic!("unexpected message: {other:?}"),
      }
    }

    ready.sort_by_key(|(id, _)| id.0);
    assert_eq!(ready[0].0, frame_a);
    assert_eq!(ready[0].1.width, 2);
    assert_eq!(ready[0].1.height, 2);
    assert_eq!(ready[0].1.rgba8.len(), 2 * 2 * 4);
    assert_eq!(ready[1].0, frame_b);
    assert_eq!(ready[1].1.width, 3);
    assert_eq!(ready[1].1.height, 1);
    assert_eq!(ready[1].1.rgba8.len(), 3 * 1 * 4);

    // Shut down and join the renderer loop.
    to_renderer_tx.send(BrowserToRenderer::Shutdown).unwrap();
    join.join().unwrap();
  }
}

