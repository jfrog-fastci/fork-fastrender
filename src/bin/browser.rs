//! Experimental desktop "browser" UI.
//!
//! This binary is feature-gated behind `browser_ui` so the core renderer can
//! compile without pulling in heavy UI dependencies (winit/wgpu/egui).
//!
//! For now this is just a minimal `winit` window/event loop smoke test. Future
//! milestones will integrate `wgpu` + `egui` and render page pixels.

use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::WindowBuilder;

fn main() {
  let event_loop = EventLoop::new();
  let window = WindowBuilder::new()
    .with_title("FastRender Browser (WIP)")
    .build(&event_loop)
    .expect("failed to create winit window");

  event_loop.run(move |event, _target, control_flow| {
    *control_flow = ControlFlow::Wait;
    match event {
      Event::WindowEvent {
        event: WindowEvent::CloseRequested,
        ..
      } => {
        *control_flow = ControlFlow::Exit;
      }
      Event::MainEventsCleared => {
        // Continuously redraw so we have a place to hook wgpu + egui later.
        window.request_redraw();
      }
      Event::RedrawRequested(_) => {
        // Future: integrate wgpu + egui and render a frame.
      }
      _ => {}
    }
  });
}
