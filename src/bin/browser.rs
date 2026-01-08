#[cfg(not(feature = "browser_ui"))]
fn main() {
  eprintln!(
    "The `browser` binary requires the `browser_ui` feature.\n\
Run:\n\
  cargo run --features browser_ui --bin browser"
  );
}

#[cfg(feature = "browser_ui")]
fn main() {
  if let Err(err) = run() {
    eprintln!("browser exited with error: {err}");
  }
}

#[cfg(feature = "browser_ui")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
  use winit::event::Event;
  use winit::event::WindowEvent;
  use winit::event_loop::ControlFlow;
  use winit::event_loop::EventLoop;
  use winit::window::WindowBuilder;

  let event_loop = EventLoop::new();
  let window = WindowBuilder::new()
    .with_title("FastRender (browser_ui)")
    .build(&event_loop)?;

  let mut app = pollster::block_on(App::new(window, &event_loop))?;

  event_loop.run(move |event, _, control_flow| {
    *control_flow = ControlFlow::Poll;

    match event {
      Event::WindowEvent { window_id, event } if window_id == app.window.id() => {
        let _ = app.egui_state.on_event(&app.egui_ctx, &event);

        match event {
          WindowEvent::CloseRequested => {
            *control_flow = ControlFlow::Exit;
          }
          WindowEvent::Resized(new_size) => {
            app.resize(new_size);
          }
          WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
          } => {
            app.set_pixels_per_point(scale_factor as f32);
            app.resize(*new_inner_size);
          }
          _ => {}
        }
      }
      Event::RedrawRequested(window_id) if window_id == app.window.id() => {
        app.render_frame(control_flow);
      }
      Event::MainEventsCleared => {
        app.window.request_redraw();
      }
      _ => {}
    }
  });
}

#[cfg(feature = "browser_ui")]
struct App {
  window: winit::window::Window,

  surface: wgpu::Surface,
  device: wgpu::Device,
  queue: wgpu::Queue,
  surface_config: wgpu::SurfaceConfiguration,

  egui_ctx: egui::Context,
  egui_state: egui_winit::State,
  egui_renderer: egui_wgpu::Renderer,
  pixels_per_point: f32,

  address_bar: String,

  page_texture: Option<fastrender::ui::WgpuPixmapTexture>,
  page_pixmap: Option<tiny_skia::Pixmap>,
  page_size_px: [u32; 2],
}

#[cfg(feature = "browser_ui")]
impl App {
  async fn new(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<()>,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    let pixels_per_point = window.scale_factor() as f32;

    let egui_ctx = egui::Context::default();
    egui_ctx.set_pixels_per_point(pixels_per_point);
    let egui_state = egui_winit::State::new(event_loop);

    let instance = wgpu::Instance::default();
    let surface = unsafe { instance.create_surface(&window) }?;
    let adapter = instance
      .request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
      })
      .await
      .ok_or("no suitable GPU adapters found on the system")?;

    let (device, queue) = adapter
      .request_device(
        &wgpu::DeviceDescriptor {
          label: Some("device"),
          features: wgpu::Features::empty(),
          limits: wgpu::Limits::default(),
        },
        None,
      )
      .await?;

    let surface_caps = surface.get_capabilities(&adapter);
    let surface_format = surface_caps
      .formats
      .iter()
      .copied()
      .find(wgpu::TextureFormat::is_srgb)
      .unwrap_or(surface_caps.formats[0]);

    let present_mode = surface_caps
      .present_modes
      .iter()
      .copied()
      .find(|mode| *mode == wgpu::PresentMode::Fifo)
      .unwrap_or(surface_caps.present_modes[0]);

    let alpha_mode = surface_caps.alpha_modes[0];

    let size = window.inner_size();
    let surface_config = wgpu::SurfaceConfiguration {
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
      format: surface_format,
      width: size.width.max(1),
      height: size.height.max(1),
      present_mode,
      alpha_mode,
      view_formats: vec![],
    };
    surface.configure(&device, &surface_config);

    let egui_renderer = egui_wgpu::Renderer::new(&device, surface_format, None, 1);

    Ok(Self {
      window,
      surface,
      device,
      queue,
      surface_config,
      egui_ctx,
      egui_state,
      egui_renderer,
      pixels_per_point,
      address_bar: "https://example.com/".to_owned(),
      page_texture: None,
      page_pixmap: None,
      page_size_px: [0, 0],
    })
  }

  fn set_pixels_per_point(&mut self, ppp: f32) {
    self.pixels_per_point = ppp;
    self.egui_ctx.set_pixels_per_point(ppp);
    self.page_size_px = [0, 0];
  }

  fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
    if new_size.width == 0 || new_size.height == 0 {
      return;
    }

    self.surface_config.width = new_size.width;
    self.surface_config.height = new_size.height;
    self.surface.configure(&self.device, &self.surface_config);

    self.page_size_px = [0, 0];
  }

  fn ensure_dummy_page_pixmap(&mut self, logical_size_points: egui::Vec2) {
    let width_px = ((logical_size_points.x.max(0.0) * self.pixels_per_point) as u32).max(1);
    let height_px = ((logical_size_points.y.max(0.0) * self.pixels_per_point) as u32).max(1);

    if self.page_size_px == [width_px, height_px] && self.page_texture.is_some() {
      return;
    }

    let Some(mut pixmap) = tiny_skia::Pixmap::new(width_px, height_px) else {
      eprintln!("failed to allocate pixmap of size {width_px}x{height_px}");
      self.page_pixmap = None;
      if let Some(tex) = self.page_texture.take() {
        let id = tex.id();
        self.egui_renderer.free_texture(&id);
      }
      self.page_size_px = [0, 0];
      return;
    };

    const CELL: u32 = 16;
    let data = pixmap.data_mut();
    for y in 0..height_px {
      for x in 0..width_px {
        let idx = ((y * width_px + x) * 4) as usize;
        let is_light = ((x / CELL) + (y / CELL)) % 2 == 0;
        let (r, g, b) = if is_light {
          (0xDD, 0xDD, 0xDD)
        } else {
          (0x99, 0x99, 0x99)
        };

        data[idx] = r;
        data[idx + 1] = g;
        data[idx + 2] = b;
        data[idx + 3] = 0xFF;
      }
    }

    self.page_size_px = [width_px, height_px];
    self.page_pixmap = Some(pixmap);

    let Some(pixmap) = self.page_pixmap.as_ref() else {
      return;
    };

    match self.page_texture.as_mut() {
      Some(tex) => {
        tex.update(&self.device, &self.queue, &mut self.egui_renderer, pixmap);
      }
      None => {
        let mut tex = fastrender::ui::WgpuPixmapTexture::new(
          &self.device,
          &mut self.egui_renderer,
          pixmap,
        );
        tex.update(&self.device, &self.queue, &mut self.egui_renderer, pixmap);
        self.page_texture = Some(tex);
      }
    }
  }

  fn render_frame(&mut self, control_flow: &mut winit::event_loop::ControlFlow) {
    let raw_input = {
      let mut raw = self.egui_state.take_egui_input(&self.window);
      raw.pixels_per_point = Some(self.pixels_per_point);
      raw
    };

    self.egui_ctx.begin_frame(raw_input);

    let ctx = self.egui_ctx.clone();

    egui::TopBottomPanel::top("chrome").show(&ctx, |ui| {
      ui.horizontal(|ui| {
        let _ = ui.button("←");
        let _ = ui.button("→");
        let _ = ui.button("⟳");
        ui.add(
          egui::TextEdit::singleline(&mut self.address_bar)
            .desired_width(f32::INFINITY)
            .hint_text("Enter URL…"),
        );
      });
    });

    egui::CentralPanel::default().show(&ctx, |ui| {
      let logical_viewport_points = ui.available_size();
      self.ensure_dummy_page_pixmap(logical_viewport_points);

      let Some(page_texture) = self.page_texture.as_ref() else {
        ui.label("No page texture available.");
        return;
      };

      let size_points = page_texture.size_points(self.pixels_per_point);
      let response =
        ui.add(egui::Image::new((page_texture.id(), size_points)).sense(egui::Sense::click()));

      if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
          let local = pos - response.rect.min;
          println!("page click (css px): x={:.1} y={:.1}", local.x, local.y);
        }
      }
    });

    let full_output = self.egui_ctx.end_frame();
    self.egui_state.handle_platform_output(
      &self.window,
      &self.egui_ctx,
      full_output.platform_output,
    );

    let paint_jobs = self.egui_ctx.tessellate(full_output.shapes);

    let screen_descriptor = egui_wgpu::renderer::ScreenDescriptor {
      size_in_pixels: [self.surface_config.width, self.surface_config.height],
      pixels_per_point: self.pixels_per_point,
    };

    for (id, image_delta) in &full_output.textures_delta.set {
      self.egui_renderer
        .update_texture(&self.device, &self.queue, *id, image_delta);
    }

    let surface_texture = match self.surface.get_current_texture() {
      Ok(frame) => frame,
      Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
        self.surface.configure(&self.device, &self.surface_config);
        return;
      }
      Err(wgpu::SurfaceError::Timeout) => {
        return;
      }
      Err(wgpu::SurfaceError::OutOfMemory) => {
        eprintln!("wgpu surface out of memory; exiting");
        *control_flow = winit::event_loop::ControlFlow::Exit;
        return;
      }
    };

    let view = surface_texture
      .texture
      .create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = self
      .device
      .create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("egui_encoder"),
      });

    self.egui_renderer.update_buffers(
      &self.device,
      &self.queue,
      &mut encoder,
      &paint_jobs,
      &screen_descriptor,
    );

    {
      let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("render_pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
          view: &view,
          resolve_target: None,
          ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color {
              r: 0.08,
              g: 0.08,
              b: 0.08,
              a: 1.0,
            }),
            store: true,
          },
        })],
        depth_stencil_attachment: None,
      });

      self
        .egui_renderer
        .render(&mut rpass, &paint_jobs, &screen_descriptor);
    }

    self.queue.submit(Some(encoder.finish()));
    surface_texture.present();

    for id in &full_output.textures_delta.free {
      self.egui_renderer.free_texture(id);
    }
  }
}

