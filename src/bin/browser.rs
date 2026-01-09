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
  let fastrender::ui::BrowserWorkerHandle {
    tx: ui_to_worker_tx,
    rx: worker_to_ui_rx,
    join: worker_join,
  } = worker;

  let mut app = pollster::block_on(App::new(
    window,
    &event_loop,
    ui_to_worker_tx,
    worker_join,
  ))?;
  app.startup();

  let (ui_tx, ui_rx) = std::sync::mpsc::channel::<fastrender::ui::WorkerToUi>();

  // Worker → UI messages are forwarded through a small bridge thread so that we can keep the winit
  // event loop in `ControlFlow::Wait` (no busy polling), while still waking immediately when a new
  // frame/message arrives.
  let bridge_join = std::thread::Builder::new()
    .name("browser_worker_bridge".to_string())
    .spawn({
      let event_loop_proxy = event_loop_proxy.clone();
      move || {
        while let Ok(msg) = worker_to_ui_rx.recv() {
          if ui_tx.send(msg).is_err() {
            break;
          }
          // Ignore failures during shutdown (event loop already dropped).
          let _ = event_loop_proxy.send_event(UserEvent::WorkerWake);
        }
      }
    })?;

  // Kick the first frame so the window shows chrome immediately even before the worker responds.
  app.window.request_redraw();

  let mut app = Some(app);
  let mut bridge_join = Some(bridge_join);

  event_loop.run(move |event, _, control_flow| {
    // Keep the event loop idle when there is no work to do.
    *control_flow = ControlFlow::Wait;

    // `EventLoop::run` never returns, so do shutdown hygiene (dropping channels and joining
    // threads) explicitly when the loop is torn down.
    if matches!(event, Event::LoopDestroyed) {
      if let Some(mut app) = app.take() {
        app.shutdown();
      }

      if let Some(join) = bridge_join.take() {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
        let _ = std::thread::spawn(move || {
          let _ = done_tx.send(join.join());
        });
        match done_rx.recv_timeout(std::time::Duration::from_millis(500)) {
          Ok(_) => {}
          Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("timed out waiting for browser worker bridge thread to exit");
          }
          Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("browser worker bridge join helper thread disconnected during shutdown");
          }
        }
      }
      return;
    }

    let Some(app) = app.as_mut() else {
      return;
    };

    match event {
      Event::WindowEvent { window_id, event } if window_id == app.window.id() => {
        let response = app.egui_state.on_event(&app.egui_ctx, &event);
        app.handle_winit_input_event(&event);
        // Always redraw on keyboard events so chrome shortcuts (handled inside the egui frame via
        // `ui::chrome_ui`) are evaluated even when egui doesn't request a repaint.
        if response.repaint
          || matches!(
            event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::MouseWheel { .. }
          )
        {
          app.window.request_redraw();
        }

        match event {
          WindowEvent::CloseRequested => {
            app.shutdown();
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
  use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
  use std::sync::mpsc::RecvTimeoutError;
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
  // Use a DPR != 1.0 so the smoke test validates viewport↔device-pixel scaling.
  const DPR: f32 = 2.0;
  const TIMEOUT: Duration = Duration::from_secs(20);

  let expected_pixmap_w = ((VIEWPORT_CSS.0 as f32) * DPR).round().max(1.0) as u32;
  let expected_pixmap_h = ((VIEWPORT_CSS.1 as f32) * DPR).round().max(1.0) as u32;

  let worker = fastrender::ui::spawn_browser_worker()?;
  let fastrender::ui::BrowserWorkerHandle {
    tx: ui_to_worker_tx,
    rx: worker_to_ui_rx,
    join,
  } = worker;

  let tab_id = TabId::new();
  ui_to_worker_tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: Some(URL.to_string()),
    cancel: CancelGens::new(),
  })?;
  ui_to_worker_tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: VIEWPORT_CSS,
    dpr: DPR,
  })?;
  ui_to_worker_tx.send(UiToWorker::SetActiveTab { tab_id })?;

  // Close the channel so the worker thread exits after completing the above messages.
  drop(ui_to_worker_tx);

  let deadline = Instant::now() + TIMEOUT;
  let mut smoke_summary: Option<(u32, u32, (u32, u32), f32)> = None;
  let mut last_frame_meta: Option<(u32, u32, (u32, u32), f32)> = None;
  let mut frames_seen: u32 = 0;

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker_to_ui_rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        let pixmap_px = (frame.pixmap.width(), frame.pixmap.height());
        frames_seen += 1;
        last_frame_meta = Some((pixmap_px.0, pixmap_px.1, frame.viewport_css, frame.dpr));
        if frame.viewport_css == VIEWPORT_CSS
          && (frame.dpr - DPR).abs() <= 0.01
          && pixmap_px == (expected_pixmap_w, expected_pixmap_h)
        {
          smoke_summary = last_frame_meta;
          break;
        }
      }
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => break,
      Err(RecvTimeoutError::Disconnected) => {
        return Err("headless smoke worker disconnected before FrameReady".into());
      }
    }
  }

  let Some((pixmap_w, pixmap_h, viewport_css, dpr)) = smoke_summary else {
    let hint = match last_frame_meta {
      Some((w, h, viewport, dpr)) => format!(
        " (saw {frames_seen} FrameReady; last was viewport_css={viewport:?} dpr={dpr} pixmap_px={w}x{h})"
      ),
      None => " (saw no FrameReady messages)".to_string(),
    };
    return Err(format!(
      "timed out after {TIMEOUT:?} waiting for WorkerToUi::FrameReady matching viewport_css={VIEWPORT_CSS:?} dpr={DPR} pixmap_px={expected_pixmap_w}x{expected_pixmap_h}{hint}"
    )
    .into());
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
  if pixmap_w != expected_pixmap_w || pixmap_h != expected_pixmap_h {
    return Err(
      format!(
        "unexpected pixmap size from FrameReady: got {}x{}, expected {}x{}",
        pixmap_w, pixmap_h, expected_pixmap_w, expected_pixmap_h
      )
      .into(),
    );
  }
  if (dpr - DPR).abs() > 0.01 {
    return Err(format!("unexpected dpr from FrameReady: got {dpr}, expected {DPR}").into());
  }

  match join.join() {
    Ok(()) => {}
    Err(_) => return Err("headless smoke worker panicked".into()),
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
#[derive(Debug, Clone)]
struct OpenSelectDropdown {
  tab_id: fastrender::ui::TabId,
  select_node_id: usize,
  control: fastrender::tree::box_tree::SelectControl,
  /// Optional viewport-local CSS-pixel rect for positioning the popup.
  ///
  /// When present, this should be the `<select>` control's bounds in **viewport-local CSS
  /// pixels** (0,0 at the top-left of the rendered viewport).
  anchor_css: Option<fastrender::geometry::Rect>,
  /// Fallback anchor position in egui points (cursor position).
  ///
  /// Used when `anchor_css` is unavailable or the page rect is not currently known.
  anchor_points: egui::Pos2,
  anchor_width_points: Option<f32>,
  /// True when this dropdown was opened with a control anchor rect
  /// (`WorkerToUi::SelectDropdownOpened`) rather than the legacy cursor-anchored
  /// `WorkerToUi::OpenSelectDropdown` message.
  ///
  /// When both messages are emitted, prefer the control-anchored variant.
  anchored_to_control: bool,
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
  worker_join: Option<std::thread::JoinHandle<()>>,
  browser_state: fastrender::ui::BrowserAppState,

  tab_textures: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,
  tab_cancel: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::cancel::CancelGens>,

  page_rect_points: Option<egui::Rect>,
  page_viewport_css: Option<(u32, u32)>,
  page_input_tab: Option<fastrender::ui::TabId>,
  page_input_mapping: Option<fastrender::ui::InputMapping>,
  viewport_cache_tab: Option<fastrender::ui::TabId>,
  viewport_cache_css: (u32, u32),
  viewport_cache_dpr: f32,
  modifiers: winit::event::ModifiersState,

  page_has_focus: bool,
  pointer_captured: bool,
  captured_button: fastrender::ui::PointerButton,
  last_cursor_pos_points: Option<egui::Pos2>,

  open_select_dropdown: Option<OpenSelectDropdown>,
  open_select_dropdown_rect: Option<egui::Rect>,

  debug_log: std::collections::VecDeque<String>,
}

#[cfg(feature = "browser_ui")]
impl App {
  const DEBUG_LOG_MAX_LINES: usize = 200;

  async fn new<T: 'static>(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<T>,
    ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
    worker_join: std::thread::JoinHandle<()>,
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
      .or_else(|| surface_caps.formats.first().copied())
      .ok_or("wgpu surface reports no supported texture formats")?;

    let present_mode = surface_caps
      .present_modes
      .iter()
      .copied()
      .find(|mode| *mode == wgpu::PresentMode::Fifo)
      .or_else(|| surface_caps.present_modes.first().copied())
      .ok_or("wgpu surface reports no present modes")?;

    let alpha_mode = surface_caps
      .alpha_modes
      .first()
      .copied()
      .ok_or("wgpu surface reports no alpha modes")?;

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
      worker_join: Some(worker_join),
      browser_state: fastrender::ui::BrowserAppState::new(),
      tab_textures: std::collections::HashMap::new(),
      tab_cancel: std::collections::HashMap::new(),
      page_rect_points: None,
      page_viewport_css: None,
      page_input_tab: None,
      page_input_mapping: None,
      viewport_cache_tab: None,
      viewport_cache_css: (0, 0),
      viewport_cache_dpr: 0.0,
      modifiers: winit::event::ModifiersState::default(),
      page_has_focus: false,
      pointer_captured: false,
      captured_button: fastrender::ui::PointerButton::None,
      last_cursor_pos_points: None,
      open_select_dropdown: None,
      open_select_dropdown_rect: None,
      debug_log: std::collections::VecDeque::new(),
    })
  }

  fn startup(&mut self) {
    let tab_id = fastrender::ui::TabId::new();
    let initial_url = "about:newtab".to_string();
    let tab_state = fastrender::ui::BrowserTabState::new(tab_id, initial_url.clone());
    let cancel = tab_state.cancel.clone();
    self.tab_cancel.insert(tab_id, cancel.clone());

    self.browser_state.push_tab(tab_state, true);
    self.browser_state.chrome.address_bar_text = initial_url.clone();

    self.send_worker_msg(fastrender::ui::UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(initial_url),
      cancel,
    });
    self.send_worker_msg(fastrender::ui::UiToWorker::SetActiveTab { tab_id });

    self.sync_window_title();

    // Initial UX: focus the address bar so typing immediately navigates.
    self.focus_address_bar_select_all();
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

  fn send_worker_msg(&mut self, msg: fastrender::ui::UiToWorker) {
    use fastrender::ui::UiToWorker;

    let tab_id = match &msg {
      UiToWorker::CreateTab { tab_id, .. }
      | UiToWorker::NewTab { tab_id, .. }
      | UiToWorker::CloseTab { tab_id }
      | UiToWorker::SetActiveTab { tab_id }
      | UiToWorker::Navigate { tab_id, .. }
      | UiToWorker::GoBack { tab_id }
      | UiToWorker::GoForward { tab_id }
      | UiToWorker::Reload { tab_id }
      | UiToWorker::Tick { tab_id }
      | UiToWorker::ViewportChanged { tab_id, .. }
      | UiToWorker::Scroll { tab_id, .. }
      | UiToWorker::PointerMove { tab_id, .. }
      | UiToWorker::PointerDown { tab_id, .. }
      | UiToWorker::PointerUp { tab_id, .. }
      | UiToWorker::SelectDropdownChoose { tab_id, .. }
      | UiToWorker::SelectDropdownPick { tab_id, .. }
      | UiToWorker::TextInput { tab_id, .. }
      | UiToWorker::KeyAction { tab_id, .. }
      | UiToWorker::RequestRepaint { tab_id, .. } => *tab_id,
    };

    if let Some(cancel) = self.tab_cancel.get(&tab_id) {
      match &msg {
        // Navigations should cancel any in-flight navigation + paint work.
        UiToWorker::Navigate { .. }
        | UiToWorker::GoBack { .. }
        | UiToWorker::GoForward { .. }
        | UiToWorker::Reload { .. } => cancel.bump_nav(),
        // Repaint-driving events should cancel in-flight paints so we don't waste time rendering
        // intermediate frames (e.g. rapid scroll/resize/typing).
        UiToWorker::ViewportChanged { .. }
        | UiToWorker::Scroll { .. }
        | UiToWorker::PointerMove { .. }
        | UiToWorker::PointerDown { .. }
        | UiToWorker::PointerUp { .. }
        | UiToWorker::SelectDropdownChoose { .. }
        | UiToWorker::SelectDropdownPick { .. }
        | UiToWorker::TextInput { .. }
        | UiToWorker::KeyAction { .. }
        | UiToWorker::RequestRepaint { .. } => cancel.bump_paint(),
        // `Tick` and tab-management messages should not force cancellation.
        UiToWorker::Tick { .. }
        | UiToWorker::CreateTab { .. }
        | UiToWorker::NewTab { .. }
        | UiToWorker::CloseTab { .. }
        | UiToWorker::SetActiveTab { .. } => {}
      }
    }

    let _ = self.ui_to_worker_tx.send(msg);
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

  fn close_select_dropdown(&mut self) {
    self.open_select_dropdown = None;
    self.open_select_dropdown_rect = None;
  }

  fn shutdown(&mut self) {
    // Close the UI→worker channel so the worker can observe it and exit.
    //
    // We can't `drop(self.ui_to_worker_tx)` directly because `App` continues to exist until the
    // winit loop exits; instead swap in a disconnected sender.
    let (dummy_tx, _dummy_rx) = std::sync::mpsc::channel::<fastrender::ui::UiToWorker>();
    drop(std::mem::replace(&mut self.ui_to_worker_tx, dummy_tx));

    if let Some(join) = self.worker_join.take() {
      // Best-effort join: don't risk hanging the UI thread forever if the worker is stuck in a
      // long render job.
      let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
      let _ = std::thread::spawn(move || {
        let _ = done_tx.send(join.join());
      });

      match done_rx.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
          eprintln!("browser worker thread panicked during shutdown");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
          eprintln!("timed out waiting for browser worker thread to exit; shutting down anyway");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
          eprintln!("browser worker join helper thread disconnected during shutdown");
        }
      }
    }

    self.destroy_all_textures();
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
      fastrender::ui::WorkerToUi::OpenSelectDropdown {
        tab_id,
        select_node_id,
        control,
      } => {
        if self.browser_state.active_tab_id() != Some(tab_id) {
          return;
        }

        // Legacy cursor-anchored dropdown message (kept for backwards compatibility in the core
        // protocol). Prefer `SelectDropdownOpened` (anchored to the `<select>` control); if the
        // control-anchored dropdown is already open for the same `<select>`, ignore the legacy
        // message so it doesn't override the better anchor.
        if self.open_select_dropdown.as_ref().is_some_and(|existing| {
          existing.tab_id == tab_id
            && existing.select_node_id == select_node_id
            && existing.anchored_to_control
        }) {
          return;
        }

        let anchor_points = self.last_cursor_pos_points.unwrap_or_else(|| egui::pos2(0.0, 0.0));

        self.open_select_dropdown = Some(OpenSelectDropdown {
          tab_id,
          select_node_id,
          control,
          anchor_css: None,
          anchor_points,
          anchor_width_points: None,
          anchored_to_control: false,
        });
        self.open_select_dropdown_rect = None;
      }
      fastrender::ui::WorkerToUi::SelectDropdownOpened {
        tab_id,
        select_node_id,
        control,
        anchor_css,
      } => {
        if self.browser_state.active_tab_id() != Some(tab_id) {
          return;
        }

        let mut anchor_points =
          self.last_cursor_pos_points.unwrap_or_else(|| egui::pos2(0.0, 0.0));
        let mut anchor_width_points = None;
        if let Some(mapping) = self.page_input_mapping {
          if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
            anchor_points = egui::pos2(rect_points.min.x, rect_points.max.y);
            anchor_width_points = Some(rect_points.width());
          }
        }

        self.open_select_dropdown = Some(OpenSelectDropdown {
          tab_id,
          select_node_id,
          control,
          anchor_css: Some(anchor_css),
          anchor_points,
          anchor_width_points,
          anchored_to_control: true,
        });
        self.open_select_dropdown_rect = None;
      }
      fastrender::ui::WorkerToUi::SelectDropdownClosed { tab_id } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|dropdown| dropdown.tab_id == tab_id)
        {
          self.close_select_dropdown();
          self.window.request_redraw();
        }
      }
      fastrender::ui::WorkerToUi::Stage { tab_id, stage } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.stage = Some(stage);
        }
      }
      fastrender::ui::WorkerToUi::NavigationStarted { tab_id, url } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|d| d.tab_id == tab_id)
        {
          self.close_select_dropdown();
        }
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.title = None;
          tab.current_url = Some(url.clone());
          tab.loading = true;
          tab.error = None;
          tab.stage = None;
        }
        if self.browser_state.active_tab_id() == Some(tab_id)
          && !self.browser_state.chrome.address_bar_editing
        {
          self.browser_state.chrome.address_bar_text = url;
        }
      }
      fastrender::ui::WorkerToUi::NavigationCommitted {
        tab_id,
        url,
        title,
        can_go_back,
        can_go_forward,
      } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|d| d.tab_id == tab_id)
        {
          self.close_select_dropdown();
        }
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.title = title;
          tab.loading = false;
          tab.error = None;
          tab.stage = None;
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
        }
        if self.browser_state.active_tab_id() == Some(tab_id)
          && !self.browser_state.chrome.address_bar_editing
        {
          self.browser_state.chrome.address_bar_text = url;
        }
      }
      fastrender::ui::WorkerToUi::NavigationFailed {
        tab_id,
        url,
        error,
        can_go_back,
        can_go_forward,
      } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|d| d.tab_id == tab_id)
        {
          self.close_select_dropdown();
        }
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.current_url = Some(url.clone());
          tab.loading = false;
          tab.error = Some(error);
          tab.stage = None;
          tab.can_go_back = can_go_back;
          tab.can_go_forward = can_go_forward;
          tab.title = None;
        }
        if self.browser_state.active_tab_id() == Some(tab_id)
          && !self.browser_state.chrome.address_bar_editing
        {
          self.browser_state.chrome.address_bar_text = url;
        }
      }
      fastrender::ui::WorkerToUi::ScrollStateUpdated { tab_id, scroll } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.scroll_state = scroll;
        }
      }
      fastrender::ui::WorkerToUi::LoadingState { tab_id, loading } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.loading = loading;
          if !loading {
            tab.stage = None;
          }
        }
      }
      fastrender::ui::WorkerToUi::DebugLog { tab_id, line } => {
        eprintln!("[worker:{tab_id:?}] {line}");
        let line = line.trim_end();
        if !line.is_empty() {
          if self.debug_log.len() >= Self::DEBUG_LOG_MAX_LINES {
            self.debug_log.pop_front();
          }
          self.debug_log.push_back(format!("[tab {}] {}", tab_id.0, line));
        }
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

    self.send_worker_msg(fastrender::ui::UiToWorker::ViewportChanged {
      tab_id,
      viewport_css,
      dpr,
    });
  }
  fn render_select_dropdown(&mut self, ctx: &egui::Context) {
    use fastrender::tree::box_tree::SelectItem;
    use fastrender::ui::UiToWorker;

    let (tab_id, select_node_id, control, anchor_css, fallback_anchor_points, anchor_width_points) =
      match self.open_select_dropdown.as_ref() {
        Some(dropdown) => (
          dropdown.tab_id,
          dropdown.select_node_id,
          dropdown.control.clone(),
          dropdown.anchor_css,
          dropdown.anchor_points,
          dropdown.anchor_width_points,
        ),
        None => {
          self.open_select_dropdown_rect = None;
          return;
        }
      };

    let mut anchor_pos_points = fallback_anchor_points;
    let mut min_width_points = anchor_width_points
      .filter(|w| w.is_finite() && *w > 0.0)
      .unwrap_or(200.0);

    if let Some(anchor_css) = anchor_css {
      if let Some(mapping) = self.page_input_mapping {
        if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
          anchor_pos_points = egui::pos2(rect_points.min.x, rect_points.max.y);
          min_width_points = rect_points.width().max(min_width_points);
        }
      }
    }

    if self.browser_state.active_tab_id() != Some(tab_id) {
      self.close_select_dropdown();
      self.window.request_redraw();
      return;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.close_select_dropdown();
      self.window.request_redraw();
      return;
    }

    let popup = egui::Area::new(egui::Id::new((
      "fastr_select_dropdown_popup",
      tab_id.0,
      select_node_id,
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(anchor_pos_points)
    .show(ctx, |ui| {
      let frame = egui::Frame::popup(ui.style()).show(ui, |ui| {
        ui.set_min_width(min_width_points);
        egui::ScrollArea::vertical()
          .max_height(240.0)
          .show(ui, |ui| {
            let mut clicked_item_idx: Option<usize> = None;

            for (idx, item) in control.items.iter().enumerate() {
              match item {
                SelectItem::OptGroupLabel { label, disabled } => {
                  let text = egui::RichText::new(label).strong();
                  if *disabled {
                    ui.add_enabled(false, egui::Label::new(text));
                  } else {
                    ui.label(text);
                  }
                }
                SelectItem::Option {
                  label,
                  value,
                  selected,
                  disabled,
                  in_optgroup,
                  ..
                } => {
                  let base = if label.trim().is_empty() { value } else { label };
                  let text = if *in_optgroup {
                    format!("  {base}")
                  } else {
                    base.to_string()
                  };

                  let response = ui.add_enabled(
                    !*disabled,
                    egui::SelectableLabel::new(*selected, text),
                  );
                  if response.clicked() {
                    clicked_item_idx = Some(idx);
                  }
                }
              }
            }
 
            clicked_item_idx
          })
          .inner
      });

      (frame.response.rect, frame.inner)
    });

    let (popup_rect, clicked_item_idx) = popup.inner;
    self.open_select_dropdown_rect = Some(popup_rect);

    let Some(clicked_item_idx) = clicked_item_idx else {
      return;
    };

    let Some(SelectItem::Option {
      node_id: option_node_id,
      disabled,
      ..
    }) = control.items.get(clicked_item_idx)
    else {
      self.close_select_dropdown();
      self.window.request_redraw();
      return;
    };
    if *disabled {
      self.close_select_dropdown();
      self.window.request_redraw();
      return;
    }

    // Apply selection directly rather than synthesizing key events.
    self.send_worker_msg(UiToWorker::SelectDropdownChoose {
      tab_id,
      select_node_id,
      option_node_id: *option_node_id,
    });

    self.close_select_dropdown();
    self.window.request_redraw();
  }
  fn focus_address_bar_select_all(&mut self) {
    self.page_has_focus = false;
    self.browser_state.chrome.request_focus_address_bar = true;
    self.browser_state.chrome.request_select_all_address_bar = true;
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
        if self
          .open_select_dropdown_rect
          .is_some_and(|rect| rect.contains(pos_points))
        {
          return;
        }

        let Some(rect) = self.page_rect_points else {
          return;
        };
        let Some(_viewport_css) = self.page_viewport_css else {
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
        self.send_worker_msg(fastrender::ui::UiToWorker::PointerMove {
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

        if matches!(state, ElementState::Pressed) && self.open_select_dropdown.is_some() {
          if self
            .open_select_dropdown_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            return;
          }
          self.close_select_dropdown();
          self.window.request_redraw();
          return;
        }

        match state {
          ElementState::Pressed => {
            let Some(rect) = self.page_rect_points else {
              return;
            };
            if !rect.contains(pos_points) {
              return;
            }
            let Some(_viewport_css) = self.page_viewport_css else {
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
            self.page_has_focus = true;
            self.pointer_captured = true;
            self.captured_button = mapped_button;
            self.send_worker_msg(fastrender::ui::UiToWorker::PointerDown {
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
            let Some(_viewport_css) = self.page_viewport_css else {
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
            self.send_worker_msg(fastrender::ui::UiToWorker::PointerUp {
              tab_id,
              pos_css,
              button: mapped_button,
            });
          }
        }
      }
      WindowEvent::ModifiersChanged(modifiers) => {
        self.modifiers = *modifiers;
      }
      WindowEvent::KeyboardInput { input, .. } => {
        if input.state != ElementState::Pressed {
          return;
        }
        let Some(key) = input.virtual_keycode else {
          return;
        };

        if self.open_select_dropdown.is_some() {
          if matches!(key, VirtualKeyCode::Escape) {
            self.close_select_dropdown();
            self.window.request_redraw();
            return;
          }
          self.close_select_dropdown();
          self.window.request_redraw();
        }

        // If egui is actively editing text (e.g. the address bar), don't handle page-level key
        // events.
        if self.egui_ctx.wants_keyboard_input() {
          return;
        }

        // Ctrl/Cmd+Tab is reserved for chrome tab switching; don't forward it to the page as a Tab
        // key press.
        if (self.modifiers.ctrl() || self.modifiers.logo()) && matches!(key, VirtualKeyCode::Tab)
        {
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
          VirtualKeyCode::Tab => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftTab
          } else {
            fastrender::interaction::KeyAction::Tab
          }),
          VirtualKeyCode::Up => Some(fastrender::interaction::KeyAction::ArrowUp),
          VirtualKeyCode::Down => Some(fastrender::interaction::KeyAction::ArrowDown),
          VirtualKeyCode::Home => Some(fastrender::interaction::KeyAction::Home),
          VirtualKeyCode::End => Some(fastrender::interaction::KeyAction::End),
          _ => None,
        };
        let Some(key_action) = key_action else {
          return;
        };

        self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
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
        if self.open_select_dropdown.is_some() {
          self.close_select_dropdown();
          self.window.request_redraw();
        }
        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };
        self.send_worker_msg(fastrender::ui::UiToWorker::TextInput {
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

    if !actions.is_empty() {
      self.close_select_dropdown();
    }

    for action in actions {
      match action {
        ChromeAction::FocusAddressBar => {
          self.focus_address_bar_select_all();
          // Request another redraw so egui can apply the focus/select-all request.
          self.window.request_redraw();
        }
        ChromeAction::AddressBarFocusChanged(has_focus) => {
          if has_focus {
            self.page_has_focus = false;
          }
        }
        ChromeAction::NewTab => {
          let tab_id = fastrender::ui::TabId::new();
          let initial_url = "about:newtab".to_string();
          let tab_state = fastrender::ui::BrowserTabState::new(tab_id, initial_url.clone());
          let cancel = tab_state.cancel.clone();
          self.tab_cancel.insert(tab_id, cancel.clone());
          self.browser_state.push_tab(tab_state, true);
          self.browser_state.chrome.address_bar_text = initial_url.clone();
          self.page_has_focus = false;
          self.viewport_cache_tab = None;

          self.send_worker_msg(UiToWorker::CreateTab {
            tab_id,
            initial_url: Some(initial_url),
            cancel,
          });
          self.send_worker_msg(UiToWorker::SetActiveTab { tab_id });
          self.send_worker_msg(UiToWorker::RequestRepaint {
            tab_id,
            reason: RepaintReason::Explicit,
          });

          // Match typical browser UX: after opening a new tab, focus the address bar so the user
          // can immediately type a URL.
          self.focus_address_bar_select_all();
          self.window.request_redraw();
        }
        ChromeAction::CloseTab(tab_id) => {
          if let Some(tex) = self.tab_textures.remove(&tab_id) {
            tex.destroy(&mut self.egui_renderer);
          }

          if let Some(cancel) = self.tab_cancel.remove(&tab_id) {
            cancel.bump_nav();
          }
          self.send_worker_msg(UiToWorker::CloseTab { tab_id });

          let close_result = self.browser_state.remove_tab(tab_id);

          if let Some(created_tab) = close_result.created_tab {
            let initial_url = "about:newtab".to_string();
            let cancel = self
              .browser_state
              .tab(created_tab)
              .map(|t| t.cancel.clone())
              .unwrap_or_else(fastrender::ui::cancel::CancelGens::new);
            self.tab_cancel.insert(created_tab, cancel.clone());
            self.send_worker_msg(UiToWorker::CreateTab {
              tab_id: created_tab,
              initial_url: Some(initial_url),
              cancel,
            });
            self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: created_tab });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id: created_tab,
              reason: RepaintReason::Explicit,
            });

            // Closing the last tab creates a fresh new-tab page; focus the address bar for quick
            // navigation.
            self.focus_address_bar_select_all();
            self.window.request_redraw();
          } else if let Some(new_active) = close_result.new_active {
            self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: new_active });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id: new_active,
              reason: RepaintReason::Explicit,
            });
          }
        }
        ChromeAction::ActivateTab(tab_id) => {
          if self.browser_state.set_active_tab(tab_id) {
            self.page_has_focus = false;
            self.viewport_cache_tab = None;
            self.send_worker_msg(UiToWorker::SetActiveTab { tab_id });
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id,
              reason: RepaintReason::Explicit,
            });
          }
        }
        ChromeAction::NavigateTo(raw) => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          let msg = {
            let Some(tab) = self.browser_state.tab_mut(tab_id) else {
              continue;
            };
            tab.stage = None;
            match tab.navigate_typed(&raw) {
              Ok(msg) => msg,
              Err(err) => {
                tab.error = Some(err);
                continue;
              }
            }
          };
          if let UiToWorker::Navigate { url, .. } = &msg {
            self.browser_state.chrome.address_bar_text = url.clone();
          }
          self.page_has_focus = false;
          self.send_worker_msg(msg);
        }
        ChromeAction::Reload => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          if let Some(tab) = self.browser_state.tab_mut(tab_id) {
            tab.loading = true;
            tab.error = None;
            tab.stage = None;
            tab.title = None;
          }

          self.page_has_focus = false;

          self.send_worker_msg(UiToWorker::Reload { tab_id });
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
          tab.title = None;
          self.page_has_focus = false;
          self.send_worker_msg(UiToWorker::GoBack { tab_id });
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
          tab.title = None;
          self.page_has_focus = false;
          self.send_worker_msg(UiToWorker::GoForward { tab_id });
        }
      }
    }
  }

  fn render_frame(&mut self, control_flow: &mut winit::event_loop::ControlFlow) {
    let (raw_input, wheel_events) = {
      let mut raw = self.egui_state.take_egui_input(&self.window);
      raw.pixels_per_point = Some(self.pixels_per_point);
      let wheel_events = raw
        .events
        .iter()
        .filter_map(|event| match event {
          egui::Event::MouseWheel { unit, delta, .. } => Some((*unit, *delta)),
          _ => None,
        })
        .collect::<Vec<_>>();
      (raw, wheel_events)
    };

    self.egui_ctx.begin_frame(raw_input);

    let ctx = self.egui_ctx.clone();

    let chrome_actions = fastrender::ui::chrome_ui(&ctx, &mut self.browser_state);
    self.handle_chrome_actions(chrome_actions);
    self.sync_window_title();

    if !self.debug_log.is_empty() {
      egui::TopBottomPanel::bottom("debug_log")
        .resizable(true)
        .default_height(140.0)
        .show(&ctx, |ui| {
          egui::CollapsingHeader::new("Debug log")
            .default_open(false)
            .show(ui, |ui| {
              egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                  for line in &self.debug_log {
                    ui.label(line);
                  }
                });
            });
        });
    }

    egui::CentralPanel::default().show(&ctx, |ui| {
      let logical_viewport_points = ui.available_size();

      let viewport_css = (
        (logical_viewport_points.x.max(0.0).floor() as u32).max(1),
        (logical_viewport_points.y.max(0.0).floor() as u32).max(1),
      );
      let dpr = self.pixels_per_point;
      self.send_viewport_changed_if_needed(viewport_css, dpr);

      self.page_rect_points = None;
      self.page_viewport_css = None;
      self.page_input_tab = None;
      self.page_input_mapping = None;

      let Some(active_tab) = self.browser_state.active_tab_id() else {
        ui.label("No active tab.");
        return;
      };

      // Best-effort dropdown UX: when a native wheel scroll happens outside an open `<select>`
      // dropdown, close it (matching typical browser behaviour).
      let mut wheel_blocked_by_dropdown = false;
      if !wheel_events.is_empty() && self.open_select_dropdown.is_some() {
        if let Some(pos_points) = ctx.input(|i| i.pointer.hover_pos()) {
          if self
            .open_select_dropdown_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            wheel_blocked_by_dropdown = true;
          } else {
            self.close_select_dropdown();
          }
        }
      }

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
        self.page_viewport_css = Some(viewport_css_for_mapping);
        let mapping = fastrender::ui::InputMapping::new(response.rect, viewport_css_for_mapping);
        self.page_input_tab = Some(active_tab);
        self.page_input_mapping = Some(mapping);
        if self.page_has_focus {
          response.request_focus();
        }

        if !wheel_events.is_empty() && !wheel_blocked_by_dropdown && response.hovered() {
          let Some(hover_pos) = response.hover_pos() else {
            return;
          };

          let mut delta_css = (0.0, 0.0);
          for (unit, delta) in &wheel_events {
            let Some((dx, dy)) =
              mapping.wheel_delta_to_delta_css(fastrender::ui::WheelDelta::from_egui(*unit, *delta))
            else {
              continue;
            };
            delta_css.0 += dx;
            delta_css.1 += dy;
          }
          if delta_css.0 != 0.0 || delta_css.1 != 0.0 {
            if let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(hover_pos) {
              self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
                tab_id: active_tab,
                delta_css,
                pointer_css: Some(pos_css),
              });
            }
          }
        }
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

    self.render_select_dropdown(&ctx);

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
        self.shutdown();
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
