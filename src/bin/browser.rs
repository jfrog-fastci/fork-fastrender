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
    std::process::exit(1);
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy)]
enum UserEvent {
  WorkerWake,
}

#[cfg(feature = "browser_ui")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
  apply_address_space_limit_from_env();

  // Test/CI hook: allow integration tests to exercise startup behaviour (including mem-limit
  // parsing) without opening a window or initialising wgpu.
  if std::env::var_os("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY").is_some() {
    return Ok(());
  }

  // Test/CI hook: run a minimal end-to-end wiring smoke test without creating a window or
  // initialising winit/wgpu.
  //
  // This exists so CI environments without an X11 display / GPU can still exercise the real
  // `src/bin/browser.rs` entrypoint and UI↔worker messaging.
  //
  // Usage:
  //   FASTR_TEST_BROWSER_HEADLESS_SMOKE=1 cargo run --features browser_ui --bin browser
  if std::env::var_os("FASTR_TEST_BROWSER_HEADLESS_SMOKE").is_some() {
    return run_headless_smoke_mode();
  }

  use winit::event::Event;
  use winit::event::StartCause;
  use winit::event::WindowEvent;
  use winit::event_loop::ControlFlow;
  use winit::event_loop::EventLoopBuilder;
  use winit::window::WindowBuilder;

  let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
  let event_loop_proxy = event_loop.create_proxy();
  let window = WindowBuilder::new()
    .with_title("FastRender")
    .build(&event_loop)?;

  let worker = fastrender::ui::spawn_browser_worker()?;
  let ui_to_worker_tx = worker.tx.clone();
  let raw_rx = worker.rx;
  let _worker_join = worker.join;

  let mut app = pollster::block_on(App::new(window, &event_loop, ui_to_worker_tx))?;
  app.startup();

  let (ui_tx, ui_rx) = std::sync::mpsc::channel::<fastrender::ui::WorkerToUi>();

  // Worker → UI messages are forwarded through a small bridge thread so that we can keep the winit
  // event loop in `ControlFlow::Wait` (no busy polling), while still waking immediately when a new
  // frame/message arrives.
  std::thread::spawn({
    let event_loop_proxy = event_loop_proxy.clone();
    move || {
      while let Ok(msg) = raw_rx.recv() {
        if ui_tx.send(msg).is_err() {
          break;
        }
        // Ignore failures during shutdown (event loop already dropped).
        let _ = event_loop_proxy.send_event(UserEvent::WorkerWake);
      }
    }
  });

  // Kick the first frame so the window shows chrome immediately even before the worker responds.
  app.window.request_redraw();

  event_loop.run(move |event, _, control_flow| {
    *control_flow = ControlFlow::Wait;

    match event {
      Event::WindowEvent { window_id, event } if window_id == app.window.id() => {
        let response = app.egui_state.on_event(&app.egui_ctx, &event);
        app.handle_winit_input_event(&event);
        if response.repaint {
          app.window.request_redraw();
        }

        match event {
          WindowEvent::CloseRequested => {
            app.destroy_all_textures();
            *control_flow = ControlFlow::Exit;
          }
          WindowEvent::Resized(new_size) => {
            app.resize(new_size);
            app.window.request_redraw();
          }
          WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
          } => {
            app.set_pixels_per_point(scale_factor as f32);
            app.resize(*new_inner_size);
            app.window.request_redraw();
          }
          _ => {}
        }
      }
      Event::UserEvent(UserEvent::WorkerWake) => {
        // Drain all pending worker messages. The bridge thread emits one wake event per message but
        // draining here ensures we coalesce renders if multiple arrive in quick succession.
        while let Ok(msg) = ui_rx.try_recv() {
          app.handle_worker_message(msg);
        }
        app.window.request_redraw();
      }
      Event::RedrawRequested(window_id) if window_id == app.window.id() => {
        app.render_frame(control_flow);
      }
      Event::NewEvents(StartCause::Init) => {
        // Ensure we draw at least one frame on startup.
        app.window.request_redraw();
      }
      _ => {}
    }
  });
}

#[cfg(feature = "browser_ui")]
fn run_headless_smoke_mode() -> Result<(), Box<dyn std::error::Error>> {
  use fastrender::ui::cancel::CancelGens;
  use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
  use std::collections::HashMap;
  use std::sync::mpsc;
  use std::time::{Duration, Instant};

  // Keep the smoke test cheap and deterministic: when Rayon is allowed to auto-initialize its
  // global pool it may attempt to spawn one worker per detected CPU (which can be very large on
  // CI hosts). Explicitly pin the pool to a single thread unless the caller has overridden it.
  //
  // Note: this also avoids a rare `rayon-core` panic when multiple subsystems race to initialize
  // the global pool with different settings.
  const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";
  if !std::env::var_os(RAYON_NUM_THREADS_ENV).is_some_and(|value| !value.is_empty()) {
    std::env::set_var(RAYON_NUM_THREADS_ENV, "1");
  }

  const URL: &str = "about:newtab";
  const VIEWPORT_CSS: (u32, u32) = (200, 120);
  const DPR: f32 = 1.0;
  const TIMEOUT: Duration = Duration::from_secs(20);

  #[derive(Debug, Clone, Copy)]
  struct TabState {
    viewport_css: (u32, u32),
    dpr: f32,
  }

  let (ui_to_worker_tx, ui_to_worker_rx) = mpsc::channel::<UiToWorker>();
  let (worker_to_ui_tx, worker_to_ui_rx) = mpsc::channel::<WorkerToUi>();

  // Run the render pipeline on a dedicated thread, mirroring the real UI architecture.
  let handle = std::thread::Builder::new()
    .name("fastr-browser-headless-smoke-worker".to_string())
    .stack_size(fastrender::system::DEFAULT_RENDER_STACK_SIZE)
    .spawn(move || -> Result<(), String> {
      let renderer = fastrender::FastRender::new().map_err(|e| e.to_string())?;
      let mut worker =
        fastrender::ui::browser_worker::BrowserWorker::new(renderer, worker_to_ui_tx);
      let mut tabs: HashMap<TabId, TabState> = HashMap::new();

      for msg in ui_to_worker_rx {
        match msg {
          UiToWorker::CreateTab {
            tab_id,
            initial_url,
            ..
          }
          | UiToWorker::NewTab {
            tab_id,
            initial_url,
          } => {
            tabs.insert(
              tab_id,
              TabState {
                viewport_css: VIEWPORT_CSS,
                dpr: DPR,
              },
            );
            if let Some(url) = initial_url {
              let state = tabs.get(&tab_id).copied().unwrap_or(TabState {
                viewport_css: VIEWPORT_CSS,
                dpr: DPR,
              });
              let options = fastrender::RenderOptions::new()
                .with_viewport(state.viewport_css.0, state.viewport_css.1)
                .with_device_pixel_ratio(state.dpr)
                .with_fit_canvas_to_content(false);
              worker
                .navigate(tab_id, &url, options)
                .map_err(|e| e.to_string())?;
            }
          }
          UiToWorker::ViewportChanged {
            tab_id,
            viewport_css,
            dpr,
          } => {
            tabs
              .entry(tab_id)
              .and_modify(|state| {
                state.viewport_css = viewport_css;
                state.dpr = dpr;
              })
              .or_insert(TabState { viewport_css, dpr });
          }
          UiToWorker::Navigate { tab_id, url, .. } => {
            let state = tabs.get(&tab_id).copied().unwrap_or(TabState {
              viewport_css: VIEWPORT_CSS,
              dpr: DPR,
            });
            let options = fastrender::RenderOptions::new()
              .with_viewport(state.viewport_css.0, state.viewport_css.1)
              .with_device_pixel_ratio(state.dpr)
              .with_fit_canvas_to_content(false);
            worker
              .navigate(tab_id, &url, options)
              .map_err(|e| e.to_string())?;
          }
          UiToWorker::CloseTab { tab_id } => {
            tabs.remove(&tab_id);
          }
          UiToWorker::SetActiveTab { .. }
          | UiToWorker::GoBack { .. }
          | UiToWorker::GoForward { .. }
          | UiToWorker::Reload { .. }
          | UiToWorker::Scroll { .. }
          | UiToWorker::PointerMove { .. }
          | UiToWorker::PointerDown { .. }
          | UiToWorker::PointerUp { .. }
          | UiToWorker::TextInput { .. }
          | UiToWorker::KeyAction { .. }
          | UiToWorker::RequestRepaint { .. } => {
            // Not needed for the smoke test.
          }
        }
      }

      Ok(())
    })?;

  let tab_id = TabId::new();
  ui_to_worker_tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: CancelGens::new(),
  })?;
  ui_to_worker_tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: VIEWPORT_CSS,
    dpr: DPR,
  })?;
  ui_to_worker_tx.send(UiToWorker::Navigate {
    tab_id,
    url: URL.to_string(),
    reason: NavigationReason::TypedUrl,
  })?;

  // Close the channel so the worker thread exits after completing the above messages.
  drop(ui_to_worker_tx);

  let deadline = Instant::now() + TIMEOUT;
  let mut smoke_summary: Option<(u32, u32, (u32, u32), f32)> = None;

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker_to_ui_rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        let pixmap_px = (frame.pixmap.width(), frame.pixmap.height());
        smoke_summary = Some((pixmap_px.0, pixmap_px.1, frame.viewport_css, frame.dpr));
        break;
      }
      Ok(_) => {}
      Err(mpsc::RecvTimeoutError::Timeout) => break,
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        return Err("headless smoke worker disconnected before FrameReady".into());
      }
    }
  }

  let Some((pixmap_w, pixmap_h, viewport_css, dpr)) = smoke_summary else {
    return Err(format!("timed out after {TIMEOUT:?} waiting for WorkerToUi::FrameReady").into());
  };

  if viewport_css != VIEWPORT_CSS {
    return Err(
      format!(
        "unexpected viewport_css from FrameReady: got {:?}, expected {:?}",
        viewport_css, VIEWPORT_CSS
      )
      .into(),
    );
  }
  if pixmap_w != VIEWPORT_CSS.0 || pixmap_h != VIEWPORT_CSS.1 {
    return Err(
      format!(
        "unexpected pixmap size from FrameReady: got {}x{}, expected {}x{}",
        pixmap_w, pixmap_h, VIEWPORT_CSS.0, VIEWPORT_CSS.1
      )
      .into(),
    );
  }
  if (dpr - DPR).abs() > 0.01 {
    return Err(format!("unexpected dpr from FrameReady: got {dpr}, expected {DPR}").into());
  }

  match handle.join() {
    Ok(Ok(())) => {}
    Ok(Err(err)) => {
      return Err(format!("headless smoke worker failed: {err}").into());
    }
    Err(_) => {
      return Err("headless smoke worker panicked".into());
    }
  }

  println!(
    "HEADLESS_SMOKE_OK url={URL} viewport_css={}x{} dpr={:.1} pixmap_px={}x{}",
    viewport_css.0, viewport_css.1, dpr, pixmap_w, pixmap_h
  );

  Ok(())
}

#[cfg(feature = "browser_ui")]
fn apply_address_space_limit_from_env() {
  const KEY: &str = "FASTR_BROWSER_MEM_LIMIT_MB";
  let raw = std::env::var(KEY).ok();
  let Some(raw) = raw else {
    eprintln!("{KEY}: Disabled");
    return;
  };

  let raw_trimmed = raw.trim();
  if raw_trimmed.is_empty() {
    eprintln!("{KEY}: Disabled");
    return;
  }

  // Accept underscore separators (e.g. 1_024) for convenience.
  let limit_mb = match raw_trimmed.replace('_', "").parse::<u64>() {
    Ok(limit) => limit,
    Err(_) => {
      eprintln!("{KEY}: Disabled (invalid value: {raw_trimmed:?}; expected u64 MiB)");
      return;
    }
  };

  if limit_mb == 0 {
    eprintln!("{KEY}: Disabled");
    return;
  }

  match fastrender::process_limits::apply_address_space_limit_mb(limit_mb) {
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Applied) => {
      eprintln!("{KEY}: Applied ({limit_mb} MiB)");
    }
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Disabled) => {
      eprintln!("{KEY}: Disabled");
    }
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Unsupported) => {
      eprintln!("{KEY}: Unsupported (requested {limit_mb} MiB)");
    }
    // This is a best-effort safety valve. If we fail to apply the limit (e.g. under sandboxing),
    // keep running rather than preventing the UI from starting.
    Err(err) => {
      eprintln!("{KEY}: Disabled (failed to apply {limit_mb} MiB: {err})");
    }
  }
}

#[cfg(feature = "browser_ui")]
struct App {
  window: winit::window::Window,
  window_title_cache: String,

  surface: wgpu::Surface,
  device: wgpu::Device,
  queue: wgpu::Queue,
  surface_config: wgpu::SurfaceConfiguration,

  egui_ctx: egui::Context,
  egui_state: egui_winit::State,
  egui_renderer: egui_wgpu::Renderer,
  pixels_per_point: f32,

  ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
  browser_state: fastrender::ui::BrowserAppState,

  tab_textures: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,

  page_rect_points: Option<egui::Rect>,
  page_input_tab: Option<fastrender::ui::TabId>,
  page_input_mapping: Option<fastrender::ui::InputMapping>,
  viewport_cache_tab: Option<fastrender::ui::TabId>,
  viewport_cache_css: (u32, u32),
  viewport_cache_dpr: f32,

  page_has_focus: bool,
  pointer_captured: bool,
  captured_button: fastrender::ui::PointerButton,
  last_cursor_pos_points: Option<egui::Pos2>,

  address_bar_id: Option<egui::Id>,
  address_bar_select_all_pending: bool,
}

#[cfg(feature = "browser_ui")]
impl App {
  async fn new<T: 'static>(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<T>,
    ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
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
      window_title_cache: String::new(),
      surface,
      device,
      queue,
      surface_config,
      egui_ctx,
      egui_state,
      egui_renderer,
      pixels_per_point,
      ui_to_worker_tx,
      browser_state: fastrender::ui::BrowserAppState::new(),
      tab_textures: std::collections::HashMap::new(),
      page_rect_points: None,
      page_input_tab: None,
      page_input_mapping: None,
      viewport_cache_tab: None,
      viewport_cache_css: (0, 0),
      viewport_cache_dpr: 0.0,
      page_has_focus: false,
      pointer_captured: false,
      captured_button: fastrender::ui::PointerButton::None,
      last_cursor_pos_points: None,
      address_bar_id: None,
      address_bar_select_all_pending: false,
    })
  }

  fn startup(&mut self) {
    let tab_id = fastrender::ui::TabId::new();
    let initial_url = "about:newtab".to_string();

    self.browser_state.push_tab(
      fastrender::ui::BrowserTabState::new(tab_id, initial_url.clone()),
      true,
    );
    self.browser_state.chrome.address_bar_text = initial_url.clone();

    let _ = self
      .ui_to_worker_tx
      .send(fastrender::ui::UiToWorker::CreateTab {
        tab_id,
        initial_url: Some(initial_url),
        cancel: fastrender::ui::cancel::CancelGens::new(),
      });
    let _ = self
      .ui_to_worker_tx
      .send(fastrender::ui::UiToWorker::SetActiveTab { tab_id });

    self.sync_window_title();
  }

  fn sync_window_title(&mut self) {
    let title = match self.browser_state.active_tab() {
      Some(tab) => format!("{} — FastRender", tab.display_title()),
      None => "FastRender".to_string(),
    };
    if title != self.window_title_cache {
      self.window.set_title(&title);
      self.window_title_cache = title;
    }
  }

  fn set_pixels_per_point(&mut self, ppp: f32) {
    self.pixels_per_point = ppp;
    self.egui_ctx.set_pixels_per_point(ppp);
    // Force a `ViewportChanged` message on the next frame: changing the DPI scale factor affects
    // the effective device pixel ratio used for rendering.
    self.viewport_cache_tab = None;
  }

  fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
    if new_size.width == 0 || new_size.height == 0 {
      return;
    }

    self.surface_config.width = new_size.width;
    self.surface_config.height = new_size.height;
    self.surface.configure(&self.device, &self.surface_config);
  }

  fn destroy_all_textures(&mut self) {
    for (_, tex) in std::mem::take(&mut self.tab_textures) {
      tex.destroy(&mut self.egui_renderer);
    }
  }

  fn handle_worker_message(&mut self, msg: fastrender::ui::WorkerToUi) {
    match msg {
      fastrender::ui::WorkerToUi::FrameReady { tab_id, frame } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.scroll_state = frame.scroll_state.clone();
          tab.latest_frame_meta = Some(fastrender::ui::LatestFrameMeta {
            pixmap_px: (frame.pixmap.width(), frame.pixmap.height()),
            viewport_css: frame.viewport_css,
            dpr: frame.dpr,
          });
        }

        let pixmap = frame.pixmap;
        if let Some(tex) = self.tab_textures.get_mut(&tab_id) {
          tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
        } else {
          let mut tex =
            fastrender::ui::WgpuPixmapTexture::new(&self.device, &mut self.egui_renderer, &pixmap);
          tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
          self.tab_textures.insert(tab_id, tex);
        }
      }
      fastrender::ui::WorkerToUi::OpenSelectDropdown { .. } => {}
      fastrender::ui::WorkerToUi::Stage { tab_id, stage } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.stage = Some(stage);
        }
      }
      fastrender::ui::WorkerToUi::NavigationStarted { tab_id, url } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = Some(url.clone());
          tab.title = None;
        }
        if self.browser_state.active_tab_id() == Some(tab_id) {
          if !self.browser_state.chrome.address_bar_editing {
            self.browser_state.chrome.address_bar_text = url;
          }
        }
      }
      fastrender::ui::WorkerToUi::NavigationCommitted {
        tab_id,
        url,
        title,
        can_go_back,
        can_go_forward,
      } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.title = title;
          tab.loading = false;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = None;
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
        }
        if self.browser_state.active_tab_id() == Some(tab_id) {
          if !self.browser_state.chrome.address_bar_editing {
            self.browser_state.chrome.address_bar_text = url;
          }
        }
      }
      fastrender::ui::WorkerToUi::NavigationFailed { tab_id, error, .. } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.loading = false;
          tab.error = Some(error);
          tab.stage = None;
          tab.pending_nav_url = None;
        }
      }
      fastrender::ui::WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.scroll_state = scroll.clone();
        }
      }
      fastrender::ui::WorkerToUi::LoadingState { tab_id, loading } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.loading = loading;
        }
      }
      fastrender::ui::WorkerToUi::DebugLog { tab_id, line } => {
        eprintln!("[worker:{tab_id:?}] {line}");
      }
    }
  }

  fn send_viewport_changed_if_needed(&mut self, viewport_css: (u32, u32), dpr: f32) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      return;
    };

    if self.viewport_cache_tab == Some(tab_id)
      && self.viewport_cache_css == viewport_css
      && (self.viewport_cache_dpr - dpr).abs() < f32::EPSILON
    {
      return;
    }

    self.viewport_cache_tab = Some(tab_id);
    self.viewport_cache_css = viewport_css;
    self.viewport_cache_dpr = dpr;

    let _ = self
      .ui_to_worker_tx
      .send(fastrender::ui::UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      });
  }

  fn render_chrome_ui(&mut self, ctx: &egui::Context) -> Vec<fastrender::ui::ChromeAction> {
    use fastrender::ui::ChromeAction;

    let mut actions = Vec::new();

    egui::TopBottomPanel::top("chrome").show(ctx, |ui| {
      // Tabs row.
      ui.horizontal_wrapped(|ui| {
        for tab in &self.browser_state.tabs {
          let is_active = self.browser_state.active_tab_id() == Some(tab.id);
          let title = tab.display_title();

          if ui.selectable_label(is_active, title).clicked() {
            actions.push(ChromeAction::ActivateTab(tab.id));
          }

          if ui.button("×").clicked() {
            actions.push(ChromeAction::CloseTab(tab.id));
          }

          ui.separator();
        }

        if ui.button("+").clicked() {
          actions.push(ChromeAction::NewTab);
        }
      });

      ui.separator();

      // Navigation + address bar row.
      ui.horizontal(|ui| {
        let active = self.browser_state.active_tab();
        let (can_back, can_forward, loading) = active
          .map(|t| (t.can_go_back, t.can_go_forward, t.loading))
          .unwrap_or((false, false, false));

        if ui.add_enabled(can_back, egui::Button::new("←")).clicked() {
          actions.push(ChromeAction::Back);
        }
        if ui
          .add_enabled(can_forward, egui::Button::new("→"))
          .clicked()
        {
          actions.push(ChromeAction::Forward);
        }
        if ui.button("⟳").clicked() {
          actions.push(ChromeAction::Reload);
        }

        if self.address_bar_select_all_pending {
          // Ctrl/Cmd+L should select all text in the address bar. egui does not expose a stable
          // "select all" API on `TextEditState` in 0.23, so we inject a synthetic Ctrl/Cmd+A key
          // event for the focused text edit.
          //
          // We only do this once we have seen a valid `address_bar_id` from a previous frame.
          if let Some(address_bar_id) = self.address_bar_id {
            ctx.memory_mut(|mem| mem.request_focus(address_bar_id));
            ctx.input_mut(|i| {
              let mut modifiers = egui::Modifiers::default();
              modifiers.command = true;
              i.events.push(egui::Event::Key {
                key: egui::Key::A,
                pressed: true,
                modifiers,
                repeat: false,
              });
            });
            self.address_bar_select_all_pending = false;
          }
        }

        let response = ui.add(
          egui::TextEdit::singleline(&mut self.browser_state.chrome.address_bar_text)
            .desired_width(f32::INFINITY)
            .hint_text("Enter URL…"),
        );

        self.address_bar_id = Some(response.id);

        let has_focus = response.has_focus();
        if has_focus != self.browser_state.chrome.address_bar_has_focus {
          self.browser_state.chrome.address_bar_has_focus = has_focus;
          actions.push(ChromeAction::AddressBarFocusChanged(has_focus));
        }

        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
          actions.push(ChromeAction::NavigateTo(
            self.browser_state.chrome.address_bar_text.clone(),
          ));
        }

        if loading {
          ui.label("Loading…");
        }
      });

      if let Some(active) = self.browser_state.active_tab() {
        if let Some(err) = active.error.as_ref().filter(|s| !s.trim().is_empty()) {
          ui.separator();
          ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
      }
    });

    actions
  }

  fn focus_address_bar_select_all(&mut self) {
    self.page_has_focus = false;
    self.address_bar_select_all_pending = true;
    if let Some(address_bar_id) = self.address_bar_id {
      self
        .egui_ctx
        .memory_mut(|mem| mem.request_focus(address_bar_id));
    }
  }

  fn handle_winit_input_event(&mut self, event: &winit::event::WindowEvent<'_>) {
    use winit::event::ElementState;
    use winit::event::VirtualKeyCode;
    use winit::event::WindowEvent;

    match event {
      WindowEvent::CursorMoved { position, .. } => {
        let pos_points = egui::pos2(
          position.x as f32 / self.pixels_per_point,
          position.y as f32 / self.pixels_per_point,
        );
        self.last_cursor_pos_points = Some(pos_points);

        let Some(rect) = self.page_rect_points else {
          return;
        };
        let Some(tab_id) = self.page_input_tab else {
          return;
        };
        let Some(mapping) = self.page_input_mapping else {
          return;
        };

        if !self.pointer_captured && !rect.contains(pos_points) {
          return;
        }

        let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(pos_points) else {
          return;
        };

        let button = if self.pointer_captured {
          self.captured_button
        } else {
          fastrender::ui::PointerButton::None
        };
        let _ = self
          .ui_to_worker_tx
          .send(fastrender::ui::UiToWorker::PointerMove {
            tab_id,
            pos_css,
            button,
          });
      }
      WindowEvent::MouseInput { state, button, .. } => {
        let mapped_button = map_mouse_button(*button);
        if matches!(
          mapped_button,
          fastrender::ui::PointerButton::Back | fastrender::ui::PointerButton::Forward
        ) {
          // Treat mouse back/forward buttons as browser chrome actions rather than page input.
          if matches!(state, ElementState::Pressed) {
            let action = match mapped_button {
              fastrender::ui::PointerButton::Back => fastrender::ui::ChromeAction::Back,
              fastrender::ui::PointerButton::Forward => fastrender::ui::ChromeAction::Forward,
              _ => return,
            };
            self.handle_chrome_actions(vec![action]);
            self.window.request_redraw();
          }
          return;
        }

        let Some(pos_points) = self.last_cursor_pos_points else {
          return;
        };

        match state {
          ElementState::Pressed => {
            let Some(rect) = self.page_rect_points else {
              return;
            };
            if !rect.contains(pos_points) {
              return;
            }
            let Some(tab_id) = self.page_input_tab else {
              return;
            };
            let Some(mapping) = self.page_input_mapping else {
              return;
            };
            let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(pos_points) else {
              return;
            };
            self.page_has_focus = true;
            self.pointer_captured = true;
            self.captured_button = mapped_button;
            let _ = self
              .ui_to_worker_tx
              .send(fastrender::ui::UiToWorker::PointerDown {
                tab_id,
                pos_css,
                button: mapped_button,
              });
          }
          ElementState::Released => {
            if !self.pointer_captured {
              return;
            }
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;

            let Some(_rect) = self.page_rect_points else {
              return;
            };
            let Some(tab_id) = self.page_input_tab else {
              return;
            };
            let Some(mapping) = self.page_input_mapping else {
              return;
            };
            let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(pos_points) else {
              return;
            };
            let _ = self
              .ui_to_worker_tx
              .send(fastrender::ui::UiToWorker::PointerUp {
                tab_id,
                pos_css,
                button: mapped_button,
              });
          }
        }
      }
      WindowEvent::MouseWheel { delta, .. } => {
        let Some(pos_points) = self.last_cursor_pos_points else {
          return;
        };
        let Some(rect) = self.page_rect_points else {
          return;
        };
        let Some(tab_id) = self.page_input_tab else {
          return;
        };
        let Some(mapping) = self.page_input_mapping else {
          return;
        };
        if !rect.contains(pos_points) {
          return;
        }

        let wheel_delta = fastrender::ui::WheelDelta::from_winit(*delta, self.pixels_per_point);
        let Some(delta_css) = mapping.wheel_delta_to_delta_css(wheel_delta) else {
          return;
        };
        let pointer_css = mapping.pos_points_to_pos_css_clamped(pos_points);

        let _ = self
          .ui_to_worker_tx
          .send(fastrender::ui::UiToWorker::Scroll {
            tab_id,
            delta_css,
            pointer_css,
          });
      }
      WindowEvent::KeyboardInput { input, .. } => {
        if input.state != ElementState::Pressed {
          return;
        }
        let Some(key) = input.virtual_keycode else {
          return;
        };

        let mods = input.modifiers;
        let primary = mods.ctrl() || mods.logo();

        // Ctrl/Cmd+L: focus address bar and select all text (always allowed).
        if primary && matches!(key, VirtualKeyCode::L) {
          self.focus_address_bar_select_all();
          self.window.request_redraw();
          return;
        }

        // If egui is actively editing text (e.g. the address bar), don't handle browser-level
        // shortcuts other than Ctrl+L (above).
        if self.egui_ctx.wants_keyboard_input() {
          return;
        }

        // Browser-level shortcuts (match Chrome behaviour).
        if primary && matches!(key, VirtualKeyCode::T) {
          self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::NewTab]);
          self.window.request_redraw();
          return;
        }

        if primary && matches!(key, VirtualKeyCode::W) {
          // Only close if more than one tab exists; closing the last tab is a no-op.
          if self.browser_state.tabs.len() > 1 {
            if let Some(tab_id) = self.browser_state.active_tab_id() {
              self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::CloseTab(tab_id)]);
              self.window.request_redraw();
            }
          }
          return;
        }

        if primary && matches!(key, VirtualKeyCode::Tab) {
          let tab_count = self.browser_state.tabs.len();
          if tab_count > 1 {
            if let Some(active) = self.browser_state.active_tab_id() {
              if let Some(active_idx) = self.browser_state.tabs.iter().position(|t| t.id == active)
              {
                let next_idx = if mods.shift() {
                  (active_idx + tab_count - 1) % tab_count
                } else {
                  (active_idx + 1) % tab_count
                };
                if let Some(next_id) = self.browser_state.tabs.get(next_idx).map(|t| t.id) {
                  self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::ActivateTab(
                    next_id,
                  )]);
                  self.window.request_redraw();
                }
              }
            }
          }
          return;
        }

        if mods.alt() && !primary && matches!(key, VirtualKeyCode::Left) {
          self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::Back]);
          self.window.request_redraw();
          return;
        }
        if mods.alt() && !primary && matches!(key, VirtualKeyCode::Right) {
          self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::Forward]);
          self.window.request_redraw();
          return;
        }

        if matches!(key, VirtualKeyCode::F5) || (primary && matches!(key, VirtualKeyCode::R)) {
          self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::Reload]);
          self.window.request_redraw();
          return;
        }

        if !self.page_has_focus {
          return;
        }
        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };

        let key_action = match key {
          VirtualKeyCode::Back => Some(fastrender::interaction::KeyAction::Backspace),
          VirtualKeyCode::Return => Some(fastrender::interaction::KeyAction::Enter),
          VirtualKeyCode::Space => Some(fastrender::interaction::KeyAction::Space),
          VirtualKeyCode::Tab => Some(fastrender::interaction::KeyAction::Tab),
          VirtualKeyCode::Up => Some(fastrender::interaction::KeyAction::ArrowUp),
          VirtualKeyCode::Down => Some(fastrender::interaction::KeyAction::ArrowDown),
          _ => None,
        };
        let Some(key_action) = key_action else {
          return;
        };

        let _ = self
          .ui_to_worker_tx
          .send(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: key_action,
          });
      }
      WindowEvent::ReceivedCharacter(ch) => {
        if !self.page_has_focus || self.egui_ctx.wants_keyboard_input() {
          return;
        }
        if ch.is_control() {
          return;
        }
        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };
        let _ = self
          .ui_to_worker_tx
          .send(fastrender::ui::UiToWorker::TextInput {
            tab_id,
            text: ch.to_string(),
          });
      }
      _ => {}
    }
  }

  fn handle_chrome_actions(&mut self, actions: Vec<fastrender::ui::ChromeAction>) {
    use fastrender::ui::ChromeAction;
    use fastrender::ui::RepaintReason;
    use fastrender::ui::UiToWorker;

    for action in actions {
      match action {
        ChromeAction::AddressBarFocusChanged(has_focus) => {
          if has_focus {
            self.page_has_focus = false;
          }
        }
        ChromeAction::NewTab => {
          let tab_id = fastrender::ui::TabId::new();
          let initial_url = "about:newtab".to_string();
          self.browser_state.push_tab(
            fastrender::ui::BrowserTabState::new(tab_id, initial_url.clone()),
            true,
          );
          self.browser_state.chrome.address_bar_text = initial_url.clone();
          self.page_has_focus = false;
          self.viewport_cache_tab = None;

          let _ = self.ui_to_worker_tx.send(UiToWorker::CreateTab {
            tab_id,
            initial_url: Some(initial_url),
            cancel: fastrender::ui::cancel::CancelGens::new(),
          });
          let _ = self
            .ui_to_worker_tx
            .send(UiToWorker::SetActiveTab { tab_id });
          let _ = self.ui_to_worker_tx.send(UiToWorker::RequestRepaint {
            tab_id,
            reason: RepaintReason::Explicit,
          });
        }
        ChromeAction::CloseTab(tab_id) => {
          if let Some(tex) = self.tab_textures.remove(&tab_id) {
            tex.destroy(&mut self.egui_renderer);
          }

          let close_result = self.browser_state.remove_tab(tab_id);
          let _ = self.ui_to_worker_tx.send(UiToWorker::CloseTab { tab_id });

          if let Some(created_tab) = close_result.created_tab {
            let initial_url = "about:newtab".to_string();
            let _ = self.ui_to_worker_tx.send(UiToWorker::CreateTab {
              tab_id: created_tab,
              initial_url: Some(initial_url),
              cancel: fastrender::ui::cancel::CancelGens::new(),
            });
            let _ = self.ui_to_worker_tx.send(UiToWorker::SetActiveTab {
              tab_id: created_tab,
            });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            let _ = self.ui_to_worker_tx.send(UiToWorker::RequestRepaint {
              tab_id: created_tab,
              reason: RepaintReason::Explicit,
            });
          } else if let Some(new_active) = close_result.new_active {
            let _ = self
              .ui_to_worker_tx
              .send(UiToWorker::SetActiveTab { tab_id: new_active });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            let _ = self.ui_to_worker_tx.send(UiToWorker::RequestRepaint {
              tab_id: new_active,
              reason: RepaintReason::Explicit,
            });
          }
        }
        ChromeAction::ActivateTab(tab_id) => {
          if self.browser_state.set_active_tab(tab_id) {
            self.page_has_focus = false;
            self.viewport_cache_tab = None;
            let _ = self
              .ui_to_worker_tx
              .send(UiToWorker::SetActiveTab { tab_id });
            let _ = self.ui_to_worker_tx.send(UiToWorker::RequestRepaint {
              tab_id,
              reason: RepaintReason::Explicit,
            });
          }
        }
        ChromeAction::NavigateTo(raw) => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          let Some(tab) = self.browser_state.tab_mut(tab_id) else {
            continue;
          };
          tab.stage = None;
          let msg = match tab.navigate_typed(&raw) {
            Ok(msg) => msg,
            Err(err) => {
              tab.error = Some(err);
              continue;
            }
          };

          tab.stage = None;
          if let UiToWorker::Navigate { url, .. } = &msg {
            self.browser_state.chrome.address_bar_text = url.clone();
          }
          self.page_has_focus = false;

          let _ = self.ui_to_worker_tx.send(msg);
        }
        ChromeAction::Reload => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };

          if let Some(tab) = self.browser_state.tab_mut(tab_id) {
            tab.loading = true;
            tab.error = None;
            tab.stage = None;
            tab.pending_nav_url = tab.current_url.clone();
          }

          self.page_has_focus = false;

          let _ = self.ui_to_worker_tx.send(UiToWorker::Reload { tab_id });
        }
        ChromeAction::Back => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          let Some(tab) = self.browser_state.tab_mut(tab_id) else {
            continue;
          };
          if !tab.can_go_back {
            continue;
          }
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = None;
          self.page_has_focus = false;

          let _ = self.ui_to_worker_tx.send(UiToWorker::GoBack { tab_id });
        }
        ChromeAction::Forward => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          let Some(tab) = self.browser_state.tab_mut(tab_id) else {
            continue;
          };
          if !tab.can_go_forward {
            continue;
          }
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
          tab.pending_nav_url = None;
          self.page_has_focus = false;

          let _ = self.ui_to_worker_tx.send(UiToWorker::GoForward { tab_id });
        }
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

    let chrome_actions = self.render_chrome_ui(&ctx);
    self.handle_chrome_actions(chrome_actions);
    self.sync_window_title();

    egui::CentralPanel::default().show(&ctx, |ui| {
      let logical_viewport_points = ui.available_size();

      let viewport_css = (
        (logical_viewport_points.x.max(0.0).floor() as u32).max(1),
        (logical_viewport_points.y.max(0.0).floor() as u32).max(1),
      );
      let dpr = self.pixels_per_point;
      self.send_viewport_changed_if_needed(viewport_css, dpr);

      self.page_rect_points = None;
      self.page_input_tab = None;
      self.page_input_mapping = None;

      let Some(active_tab) = self.browser_state.active_tab_id() else {
        ui.label("No active tab.");
        return;
      };

      if let Some(tex) = self.tab_textures.get(&active_tab) {
        let viewport_css_for_mapping = self
          .browser_state
          .tab(active_tab)
          .and_then(|tab| tab.latest_frame_meta.as_ref().map(|m| m.viewport_css))
          .or_else(|| (self.viewport_cache_tab == Some(active_tab)).then_some(self.viewport_cache_css))
          .unwrap_or(viewport_css);
        let size_points = tex.size_points(self.pixels_per_point);
        let response =
          ui.add(egui::Image::new((tex.id(), size_points)).sense(egui::Sense::click()));
        self.page_rect_points = Some(response.rect);
        self.page_input_tab = Some(active_tab);
        self.page_input_mapping =
          Some(fastrender::ui::InputMapping::new(response.rect, viewport_css_for_mapping));
      } else {
        let loading = self
          .browser_state
          .tab(active_tab)
          .map(|t| t.loading)
          .unwrap_or(false);
        ui.label(if loading {
          "Loading…"
        } else {
          "Waiting for first frame…"
        });
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
      self
        .egui_renderer
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
        self.destroy_all_textures();
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

#[cfg(feature = "browser_ui")]
fn map_mouse_button(button: winit::event::MouseButton) -> fastrender::ui::PointerButton {
  match button {
    winit::event::MouseButton::Left => fastrender::ui::PointerButton::Primary,
    winit::event::MouseButton::Right => fastrender::ui::PointerButton::Secondary,
    winit::event::MouseButton::Middle => fastrender::ui::PointerButton::Middle,
    winit::event::MouseButton::Other(v) => match v {
      // X11 typically uses buttons 8/9 for mouse back/forward.
      8 => fastrender::ui::PointerButton::Back,
      9 => fastrender::ui::PointerButton::Forward,
      _ => fastrender::ui::PointerButton::Other(v),
    },
  }
}
