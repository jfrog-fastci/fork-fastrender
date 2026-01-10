#[cfg(not(feature = "browser_ui"))]
fn main() {
  eprintln!(
    "The `browser` binary requires the `browser_ui` feature.\n\
Run:\n\
  bash scripts/run_limited.sh --as 64G -- \\\n\
    bash scripts/cargo_agent.sh run --features browser_ui --bin browser"
  );
  std::process::exit(2);
}

#[cfg(feature = "browser_ui")]
fn main() {
  if let Err(err) = run() {
    eprintln!("browser exited with error: {err}");
    std::process::exit(1);
  }
}

#[cfg(feature = "browser_ui")]
use clap::Parser;
#[cfg(feature = "browser_ui")]
use arboard::Clipboard;

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy)]
enum UserEvent {
  WorkerWake,
}

#[cfg(feature = "browser_ui")]
#[derive(clap::Parser, Debug)]
#[command(
  name = "browser",
  about = "FastRender browser UI (experimental)",
  disable_version_flag = true,
  disable_help_subcommand = true,
  color = clap::ColorChoice::Never,
  term_width = 90,
  after_help = "If URL is omitted, the browser opens `about:newtab`.\nThe URL value is normalized like the address bar (e.g. `example.com` → https).\nSupported schemes: http, https, file, about."
)]
struct BrowserCliArgs {
  /// Start URL (default: about:newtab)
  #[arg(value_name = "URL")]
  url: Option<String>,

  /// Restore the previous session (if supported)
  #[arg(long, action = clap::ArgAction::SetTrue, overrides_with = "no_restore")]
  restore: bool,

  /// Do not restore the previous session
  #[arg(long = "no-restore", action = clap::ArgAction::SetTrue, overrides_with = "restore")]
  no_restore: bool,

  /// Override the address-space memory limit in MiB (0 disables)
  ///
  /// When unset, defaults to the `FASTR_BROWSER_MEM_LIMIT_MB` environment variable.
  #[arg(long = "mem-limit-mb", value_name = "MB", value_parser = parse_u64_mb)]
  mem_limit_mb: Option<u64>,

  /// wgpu adapter power preference when selecting a GPU
  ///
  /// - `high`: prefer a discrete/high-performance GPU (default)
  /// - `low`: prefer an integrated/low-power GPU
  /// - `none`: no preference (wgpu default behaviour)
  #[arg(
    long = "power-preference",
    value_enum,
    default_value_t = CliPowerPreference::High,
    value_name = "PREF"
  )]
  power_preference: CliPowerPreference,

  /// Force a fallback adapter (e.g. software rasterizer) during wgpu adapter selection.
  ///
  /// Equivalent env: `FASTR_BROWSER_WGPU_FALLBACK=1`.
  #[arg(
    long = "force-fallback-adapter",
    alias = "wgpu-fallback",
    action = clap::ArgAction::SetTrue
  )]
  force_fallback_adapter: bool,

  /// Restrict the wgpu backend set used for instance/adapter creation (comma-separated)
  ///
  /// Examples:
  ///   --wgpu-backends all
  ///   --wgpu-backends vulkan
  ///   --wgpu-backends vulkan,gl
  ///
  /// Equivalent env: `FASTR_BROWSER_WGPU_BACKENDS=...`.
  #[arg(
    long = "wgpu-backends",
    alias = "wgpu-backend",
    value_delimiter = ',',
    value_enum,
    value_name = "BACKEND"
  )]
  wgpu_backends: Option<Vec<CliWgpuBackend>>,

  /// Run a minimal headless startup smoke test (no window / wgpu init)
  #[arg(long = "headless-smoke", action = clap::ArgAction::SetTrue)]
  headless_smoke: bool,

  /// Enable JavaScript execution (experimental)
  ///
  /// Note: the windowed browser UI worker does not execute author scripts yet. Today this flag is
  /// supported only for `--headless-smoke --js` (a vm-js `BrowserTab` smoke test).
  #[arg(long = "js", action = clap::ArgAction::SetTrue)]
  js_enabled: bool,

  /// Exit after parsing CLI + applying mem limits, without creating a window
  #[arg(long = "exit-immediately", action = clap::ArgAction::SetTrue)]
  exit_immediately: bool,
}

#[cfg(feature = "browser_ui")]
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum CliPowerPreference {
  High,
  Low,
  None,
}

#[cfg(feature = "browser_ui")]
impl CliPowerPreference {
  fn to_wgpu(self) -> wgpu::PowerPreference {
    match self {
      Self::High => wgpu::PowerPreference::HighPerformance,
      Self::Low => wgpu::PowerPreference::LowPower,
      Self::None => wgpu::PowerPreference::None,
    }
  }
}

#[cfg(feature = "browser_ui")]
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum CliWgpuBackend {
  /// Enable all supported wgpu backends (useful for overriding `FASTR_BROWSER_WGPU_BACKENDS`).
  #[value(name = "all")]
  All,
  Vulkan,
  Metal,
  Dx12,
  Dx11,
  #[value(alias = "opengl")]
  Gl,
  #[value(name = "browser-webgpu", alias = "webgpu")]
  BrowserWebGpu,
}

#[cfg(feature = "browser_ui")]
impl CliWgpuBackend {
  fn to_wgpu(self) -> wgpu::Backends {
    match self {
      Self::All => wgpu::Backends::all(),
      Self::Vulkan => wgpu::Backends::VULKAN,
      Self::Metal => wgpu::Backends::METAL,
      Self::Dx12 => wgpu::Backends::DX12,
      Self::Dx11 => wgpu::Backends::DX11,
      Self::Gl => wgpu::Backends::GL,
      Self::BrowserWebGpu => wgpu::Backends::BROWSER_WEBGPU,
    }
  }
}

#[cfg(feature = "browser_ui")]
fn parse_u64_mb(raw: &str) -> Result<u64, String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err("expected an integer".to_string());
  }
  trimmed
    .replace('_', "")
    .parse::<u64>()
    .map_err(|_| format!("invalid integer: {raw:?}"))
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreMode {
  /// Default behaviour:
  /// - When `<url>` is omitted, try to restore the previous session.
  /// - When `<url>` is provided, open that single URL (do not restore).
  Auto,
  /// Restore the previous session even when `<url>` is provided.
  Force,
  /// Never restore a previous session.
  Disable,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupSessionSource {
  Restored,
  CliUrl,
  DefaultNewTab,
  HeadlessOverride,
}

#[cfg(feature = "browser_ui")]
fn determine_startup_session(
  cli_url: Option<String>,
  restore: RestoreMode,
  session_path: &std::path::Path,
) -> (fastrender::ui::BrowserSession, StartupSessionSource) {
  let wants_restore = match restore {
    RestoreMode::Disable => false,
    RestoreMode::Auto => cli_url.is_none(),
    RestoreMode::Force => true,
  };

  if wants_restore {
    match fastrender::ui::session::load_session(session_path) {
      Ok(Some(session)) => return (session, StartupSessionSource::Restored),
      Ok(None) => {}
      Err(err) => {
        eprintln!("failed to load session from {}: {err}", session_path.display());
      }
    }
  }

  if let Some(url) = cli_url {
    return (fastrender::ui::BrowserSession::single(url), StartupSessionSource::CliUrl);
  }

  (
    fastrender::ui::BrowserSession::single(fastrender::ui::about_pages::ABOUT_NEWTAB.to_string()),
    StartupSessionSource::DefaultNewTab,
  )
}

#[cfg(feature = "browser_ui")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
  let cli = BrowserCliArgs::parse();

  // When the user provides `<url>`, normalize + apply an allowlist (same as the address bar).
  // This is *not* applied to session restore entries: those are expected to already be normalized.
  let cli_url = cli.url.as_deref().map(|raw_url| {
    match fastrender::ui::normalize_user_url(raw_url).and_then(|url| {
      fastrender::ui::validate_user_navigation_url_scheme(&url)?;
      Ok(url)
    }) {
      Ok(url) => url,
      Err(err) => {
        eprintln!(
          "invalid start URL {raw_url:?}: {err}; falling back to {}",
          fastrender::ui::about_pages::ABOUT_NEWTAB
        );
        fastrender::ui::about_pages::ABOUT_NEWTAB.to_string()
      }
    }
  });

  let restore = if cli.restore {
    RestoreMode::Force
  } else if cli.no_restore {
    RestoreMode::Disable
  } else {
    RestoreMode::Auto
  };

  apply_address_space_limit_from_cli_or_env(cli.mem_limit_mb);

  // Test/CI hook: allow integration tests to exercise startup behaviour (including mem-limit
  // parsing) without opening a window or initialising wgpu.
  if cli.exit_immediately || std::env::var_os("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY").is_some() {
    return Ok(());
  }

  let session_path = fastrender::ui::session::session_path();

  // Test/CI hook: run a minimal end-to-end wiring smoke test without creating a window or
  // initialising winit/wgpu.
  //
  // This exists so CI environments without an X11 display / GPU can still exercise the real
  // `src/bin/browser.rs` entrypoint and UI↔worker messaging.
  //
  // Usage:
  //   bash scripts/run_limited.sh --as 64G -- \
  //     bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-smoke
  //
  // Or (legacy):
  //   FASTR_TEST_BROWSER_HEADLESS_SMOKE=1 bash scripts/run_limited.sh --as 64G -- \
  //     bash scripts/cargo_agent.sh run --features browser_ui --bin browser
  if cli.headless_smoke || std::env::var_os("FASTR_TEST_BROWSER_HEADLESS_SMOKE").is_some() {
    if cli.js_enabled {
      return run_headless_vmjs_smoke_mode();
    }

    const OVERRIDE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON";
    let (startup_session, source) = match std::env::var(OVERRIDE_ENV) {
      Ok(raw) if !raw.trim().is_empty() => {
        let session: fastrender::ui::BrowserSession = serde_json::from_str(&raw)
          .map_err(|err| format!("{OVERRIDE_ENV}: invalid JSON: {err}"))?;
        (session, StartupSessionSource::HeadlessOverride)
      }
      _ => determine_startup_session(cli_url, restore, &session_path),
    };

    return run_headless_smoke_mode(startup_session, source, session_path);
  }

  if cli.js_enabled {
    eprintln!(
      "warning: --js is currently supported only with --headless-smoke (windowed UI script execution is not wired yet)"
    );
  }

  let (startup_session, _source) = determine_startup_session(cli_url, restore, &session_path);

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

  let (ui_to_worker_tx, worker_to_ui_rx, worker_join) =
    fastrender::ui::spawn_browser_ui_worker("fastr-browser-ui-worker")?;

  let wgpu_init = {
    let cli_backends = cli.wgpu_backends.as_deref().map(|backends| {
      let mut out = wgpu::Backends::empty();
      for backend in backends {
        out |= backend.to_wgpu();
      }
      out
    });

    let env_fallback = std::env::var(fastrender::ui::browser_cli::ENV_WGPU_FALLBACK).ok();
    let env_backends = std::env::var(fastrender::ui::browser_cli::ENV_WGPU_BACKENDS).ok();
    let wgpu_options = fastrender::ui::browser_cli::resolve_wgpu_options(
      cli.force_fallback_adapter,
      cli_backends,
      env_fallback.as_deref(),
      env_backends.as_deref(),
    )?;
    let mut backends = wgpu_options.backends;
    if backends.is_empty() {
      // Defensive fallback: never attempt to create a wgpu instance with no backends.
      backends = wgpu::Backends::all();
    }

    WgpuInitOptions {
      backends,
      power_preference: cli.power_preference.to_wgpu(),
      force_fallback_adapter: wgpu_options.force_fallback_adapter,
    }
  };
  let mut app = pollster::block_on(App::new(
    window,
    &event_loop,
    ui_to_worker_tx,
    worker_join,
    wgpu_init,
  ))?;
  app.startup(startup_session);

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
        let session = fastrender::ui::BrowserSession::from_app_state(&app.browser_state);
        if let Err(err) = fastrender::ui::session::save_session_atomic(&session_path, &session) {
          eprintln!(
            "failed to save session to {}: {err}",
            session_path.display()
          );
        }
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
            app.window_minimized = new_size.width == 0 || new_size.height == 0;
            app.resize(new_size);
            app.window.request_redraw();
          }
          WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
          } => {
            app.window_minimized = new_inner_size.width == 0 || new_inner_size.height == 0;
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
        let mut request_redraw = false;
        while let Ok(msg) = ui_rx.try_recv() {
          request_redraw |= app.handle_worker_message(msg);
        }
        if request_redraw {
          app.window.request_redraw();
        }
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

    if matches!(*control_flow, ControlFlow::Exit) {
      return;
    }

    // Drive periodic worker ticks for animated documents and keep the event loop armed for the next
    // tick deadline when needed.
    app.drive_animation_tick();
  app.update_control_flow_for_animation_ticks(control_flow);
  });
}

#[cfg(feature = "browser_ui")]
fn run_headless_vmjs_smoke_mode() -> Result<(), Box<dyn std::error::Error>> {
  use std::time::Duration;

  // Keep the smoke test cheap and deterministic. See `run_headless_smoke_mode` for rationale.
  const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";
  if !std::env::var_os(RAYON_NUM_THREADS_ENV).is_some_and(|value| !value.is_empty()) {
    std::env::set_var(RAYON_NUM_THREADS_ENV, "1");
  }

  // Prefer deterministic bundled fonts for this smoke path unless explicitly opted out.
  if std::env::var_os("FASTR_USE_BUNDLED_FONTS").is_none() {
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  }

  const VIEWPORT_CSS: (u32, u32) = (200, 120);
  const DPR: f32 = 2.0;
  let expected_pixmap_w = ((VIEWPORT_CSS.0 as f32) * DPR).round().max(1.0) as u32;
  let expected_pixmap_h = ((VIEWPORT_CSS.1 as f32) * DPR).round().max(1.0) as u32;

  let html = r#"<!doctype html>
    <html>
      <body>
        <script>document.body.setAttribute("data-ok", "1")</script>
      </body>
    </html>"#;

  let mut tab = fastrender::BrowserTab::from_html_with_vmjs(
    html,
    fastrender::RenderOptions::new()
      .with_viewport(VIEWPORT_CSS.0, VIEWPORT_CSS.1)
      .with_device_pixel_ratio(DPR),
  )?;

  let run_limits = fastrender::js::RunLimits {
    max_tasks: 128,
    max_microtasks: 1024,
    max_wall_time: Some(Duration::from_millis(500)),
  };
  let outcome = tab.run_event_loop_until_idle(run_limits)?;
  if outcome != fastrender::js::RunUntilIdleOutcome::Idle {
    return Err(fastrender::Error::Other(format!(
      "expected vmjs event loop to reach idle, got {outcome:?}"
    ))
    .into());
  }

  let dom: &fastrender::dom2::Document = tab.dom();
  let body = dom
    .body()
    .ok_or_else(|| fastrender::Error::Other("expected document.body to exist".to_string()))?;
  let value = dom
    .get_attribute(body, "data-ok")
    .map_err(|err| fastrender::Error::Other(format!("failed to read body[data-ok]: {err}")))?;
  if value != Some("1") {
    return Err(fastrender::Error::Other(format!(
      "expected body[data-ok]=\"1\", got {value:?}"
    ))
    .into());
  }

  let pixmap = tab.render_frame()?;
  let pixmap_px = (pixmap.width(), pixmap.height());
  if pixmap_px != (expected_pixmap_w, expected_pixmap_h) {
    return Err(fastrender::Error::Other(format!(
      "unexpected pixmap size: got {}x{}, expected {}x{}",
      pixmap_px.0, pixmap_px.1, expected_pixmap_w, expected_pixmap_h
    )
    )
    .into());
  }

  println!(
    "HEADLESS_VMJS_SMOKE_OK viewport_css={}x{} dpr={:.1} pixmap_px={}x{}",
    VIEWPORT_CSS.0, VIEWPORT_CSS.1, DPR, pixmap_px.0, pixmap_px.1
  );
  Ok(())
}

#[cfg(feature = "browser_ui")]
fn run_headless_smoke_mode(
  session: fastrender::ui::BrowserSession,
  source: StartupSessionSource,
  session_path: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
  use fastrender::ui::cancel::CancelGens;
  use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
  use std::sync::mpsc::RecvTimeoutError;
  use std::time::{Duration, Instant};

  let session = session.sanitized();
  let source_label = match source {
    StartupSessionSource::Restored => "restored",
    StartupSessionSource::CliUrl => "cli",
    StartupSessionSource::DefaultNewTab => "default",
    StartupSessionSource::HeadlessOverride => "override",
  };
  let session_json = serde_json::to_string(&session).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_SESSION source={source_label} {session_json}");

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

  const VIEWPORT_CSS: (u32, u32) = (200, 120);
  // Use a DPR != 1.0 so the smoke test validates viewport↔device-pixel scaling.
  const DPR: f32 = 2.0;
  const TIMEOUT: Duration = Duration::from_secs(20);

  let expected_pixmap_w = ((VIEWPORT_CSS.0 as f32) * DPR).round().max(1.0) as u32;
  let expected_pixmap_h = ((VIEWPORT_CSS.1 as f32) * DPR).round().max(1.0) as u32;

  let (ui_to_worker_tx, worker_to_ui_rx, join) =
    fastrender::ui::spawn_browser_ui_worker("fastr-browser-headless-smoke-worker")?;

  let mut tab_ids = Vec::with_capacity(session.tabs.len());
  for tab in &session.tabs {
    let tab_id = TabId::new();
    tab_ids.push(tab_id);
    ui_to_worker_tx.send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(tab.url.clone()),
      cancel: CancelGens::new(),
    })?;
  }

  let active_idx = session.active_tab_index.min(tab_ids.len().saturating_sub(1));
  let active_tab_id = tab_ids[active_idx];
  ui_to_worker_tx.send(UiToWorker::ViewportChanged {
    tab_id: active_tab_id,
    viewport_css: VIEWPORT_CSS,
    dpr: DPR,
  })?;
  ui_to_worker_tx.send(UiToWorker::SetActiveTab {
    tab_id: active_tab_id,
  })?;

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
      }) if msg_tab == active_tab_id => {
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

  if let Err(err) = fastrender::ui::session::save_session_atomic(&session_path, &session) {
    eprintln!("failed to save session to {}: {err}", session_path.display());
  }

  let active_url = session
    .tabs
    .get(active_idx)
    .map(|t| t.url.as_str())
    .unwrap_or(fastrender::ui::about_pages::ABOUT_NEWTAB);
  println!(
    "HEADLESS_SMOKE_OK source={source_label} active_url={active_url} viewport_css={}x{} dpr={:.1} pixmap_px={}x{}",
    viewport_css.0, viewport_css.1, dpr, pixmap_w, pixmap_h
  );

  Ok(())
}

#[cfg(feature = "browser_ui")]
fn apply_address_space_limit_from_cli_or_env(mem_limit_mb: Option<u64>) {
  if let Some(limit_mb) = mem_limit_mb {
    apply_address_space_limit_mb("--mem-limit-mb", limit_mb);
  } else {
    apply_address_space_limit_from_env();
  }
}

#[cfg(feature = "browser_ui")]
fn apply_address_space_limit_mb(label: &str, limit_mb: u64) {
  if limit_mb == 0 {
    eprintln!("{label}: Disabled");
    return;
  }

  match fastrender::process_limits::apply_address_space_limit_mb(limit_mb) {
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Applied) => {
      eprintln!("{label}: Applied ({limit_mb} MiB)");
    }
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Disabled) => {
      eprintln!("{label}: Disabled");
    }
    Ok(fastrender::process_limits::AddressSpaceLimitStatus::Unsupported) => {
      eprintln!("{label}: Unsupported (requested {limit_mb} MiB)");
    }
    // This is a best-effort safety valve. If we fail to apply the limit (e.g. under sandboxing),
    // keep running rather than preventing the UI from starting.
    Err(err) => {
      eprintln!("{label}: Disabled (failed to apply {limit_mb} MiB: {err})");
    }
  }
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

  apply_address_space_limit_mb(KEY, limit_mb);
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
#[derive(Debug, Clone)]
struct PendingContextMenuRequest {
  tab_id: fastrender::ui::TabId,
  pos_css: (f32, f32),
  anchor_points: egui::Pos2,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone)]
struct OpenContextMenu {
  tab_id: fastrender::ui::TabId,
  pos_css: (f32, f32),
  anchor_points: egui::Pos2,
  link_url: Option<String>,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
  tab_id: fastrender::ui::TabId,
  axis: fastrender::ui::scrollbars::ScrollbarAxis,
  last_cursor_points: egui::Pos2,
  scrollbar: fastrender::ui::scrollbars::OverlayScrollbar,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy)]
struct WgpuInitOptions {
  backends: wgpu::Backends,
  power_preference: wgpu::PowerPreference,
  force_fallback_adapter: bool,
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
  browser_limits: fastrender::ui::browser_limits::BrowserLimits,

  ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
  worker_join: Option<std::thread::JoinHandle<()>>,
  browser_state: fastrender::ui::BrowserAppState,

  tab_textures: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,
  tab_favicons: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,
  tab_cancel: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::cancel::CancelGens>,
  /// Pending `FrameReady` pixmaps coalesced until the next window redraw.
  ///
  /// Uploading a pixmap into a wgpu texture is expensive; the UI worker can produce multiple frames
  /// before the windowed UI draws again. We store at most one pending frame per tab and only upload
  /// for the active tab.
  pending_frame_uploads: fastrender::ui::FrameUploadCoalescer,

  page_rect_points: Option<egui::Rect>,
  page_viewport_css: Option<(u32, u32)>,
  page_input_tab: Option<fastrender::ui::TabId>,
  page_input_mapping: Option<fastrender::ui::InputMapping>,
  overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars,
  scrollbar_drag: Option<ScrollbarDrag>,
  viewport_cache_tab: Option<fastrender::ui::TabId>,
  viewport_cache_css: (u32, u32),
  viewport_cache_dpr: f32,
  modifiers: winit::event::ModifiersState,
  /// Clipboard text received from the worker that should be forwarded to the OS clipboard on the
  /// next egui frame.
  pending_clipboard_text: Option<String>,
  /// Whether the current frame should ignore `egui::Event::Paste` events.
  ///
  /// We handle Ctrl/Cmd+V ourselves (reading the OS clipboard and sending `UiToWorker::Paste`) when
  /// the rendered page has focus. On some platforms/egui versions, egui-winit may still emit a
  /// `Paste` event for the same keypress; this flag avoids double-pasting.
  suppress_paste_events: bool,

  window_focused: bool,
  window_occluded: bool,
  window_minimized: bool,

  page_has_focus: bool,
  pointer_captured: bool,
  captured_button: fastrender::ui::PointerButton,
  last_cursor_pos_points: Option<egui::Pos2>,
  cursor_in_page: bool,
  page_cursor_override: Option<fastrender::ui::CursorKind>,
  /// Latest pending pointer-move message.
  ///
  /// Pointer move events can arrive at very high frequency. We coalesce them so the UI worker sees
  /// at most one `UiToWorker::PointerMove` per rendered frame (and before pointer up/down when
  /// needed).
  pending_pointer_move: Option<fastrender::ui::UiToWorker>,
  /// Whether the next `render_frame` should send a synthetic `PointerMove` to the active tab based
  /// on the current cursor position.
  ///
  /// Switching tabs does not necessarily produce a `CursorMoved` event, so without this the newly
  /// active tab might not receive a `PointerMove` until the user moves the mouse (hover state would
  /// appear "stuck" or missing).
  hover_sync_pending: bool,

  pending_context_menu_request: Option<PendingContextMenuRequest>,
  open_context_menu: Option<OpenContextMenu>,
  open_context_menu_rect: Option<egui::Rect>,

  open_select_dropdown: Option<OpenSelectDropdown>,
  open_select_dropdown_rect: Option<egui::Rect>,
  debug_log: std::collections::VecDeque<String>,

  /// Periodic tick driver state for animated documents.
  ///
  /// The render worker only advances CSS animation/transition sampling time when the UI sends
  /// [`fastrender::ui::UiToWorker::Tick`]. We keep a small scheduler here so the windowed browser
  /// can display multi-frame animations without busy-polling the event loop.
  animation_tick_tab: Option<fastrender::ui::TabId>,
  next_animation_tick: Option<std::time::Instant>,
}

#[cfg(feature = "browser_ui")]
impl App {
  const DEBUG_LOG_MAX_LINES: usize = 200;
  const ANIMATION_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

  fn cursor_over_overlay_scrollbars(&self, pos_points: egui::Pos2) -> bool {
    let pos = fastrender::Point::new(pos_points.x, pos_points.y);
    self
      .overlay_scrollbars
      .vertical
      .is_some_and(|sb| sb.track_rect_points.contains_point(pos))
      || self
        .overlay_scrollbars
        .horizontal
        .is_some_and(|sb| sb.track_rect_points.contains_point(pos))
  }

  async fn new<T: 'static>(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<T>,
    ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
    worker_join: std::thread::JoinHandle<()>,
    wgpu_init: WgpuInitOptions,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    // Enable OS IME integration (WindowEvent::Ime) so the page can handle non-Latin input methods.
    // Egui manages IME for chrome text fields; we forward IME events to the page when appropriate.
    window.set_ime_allowed(true);

    let pixels_per_point = window.scale_factor() as f32;

    let egui_ctx = egui::Context::default();
    egui_ctx.set_pixels_per_point(pixels_per_point);
    let egui_state = egui_winit::State::new(event_loop);

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
      backends: wgpu_init.backends,
      ..Default::default()
    });
    let surface = unsafe { instance.create_surface(&window) }?;

    let adapter_options = wgpu::RequestAdapterOptions {
      power_preference: wgpu_init.power_preference,
      compatible_surface: Some(&surface),
      force_fallback_adapter: wgpu_init.force_fallback_adapter,
    };
    let adapter = instance
      .request_adapter(&adapter_options)
      .await
      .ok_or_else(|| {
        let mut available = Vec::new();
        for adapter in instance.enumerate_adapters(wgpu_init.backends) {
          let info = adapter.get_info();
          available.push(format!("{} ({:?})", info.name, info.backend));
        }
        let available = if available.is_empty() {
          "none".to_string()
        } else {
          available.join(", ")
        };

        let mut msg = format!(
          "wgpu adapter selection failed.\n\
requested: backends={:?} power_preference={:?} force_fallback_adapter={}\n\
available adapters (instance.enumerate_adapters): {available}",
          wgpu_init.backends,
          wgpu_init.power_preference,
          wgpu_init.force_fallback_adapter,
        );

        if !wgpu_init.force_fallback_adapter {
          msg.push_str(&format!(
            "\nHint: Try enabling the software adapter with `--wgpu-fallback` or `{}`=1.",
            fastrender::ui::browser_cli::ENV_WGPU_FALLBACK
          ));
        }

        msg.push_str(&format!(
          "\nHint: Try forcing a backend set with `--wgpu-backends gl` or `{}`=gl.",
          fastrender::ui::browser_cli::ENV_WGPU_BACKENDS
        ));

        msg.push_str(
          "\nHint: If you're running in a headless environment, try `browser --headless-smoke` (skips window + wgpu).",
        );

        std::io::Error::new(std::io::ErrorKind::Other, msg)
      })?;

    let adapter_info = adapter.get_info();
    // Populate `about:gpu` with the adapter selected by the windowed front-end.
    fastrender::ui::about_pages::set_gpu_info(
      adapter_info.name.clone(),
      format!("{:?}", adapter_info.backend),
      format!("{:?}", wgpu_init.power_preference),
      wgpu_init.force_fallback_adapter,
      format!("{:?}", wgpu_init.backends),
    );

    let (device, queue) = adapter
      .request_device(
        &wgpu::DeviceDescriptor {
          label: Some("device"),
          features: wgpu::Features::empty(),
          limits: wgpu::Limits::default(),
        },
        None,
      )
      .await
      .map_err(|err| {
        let mut msg = format!(
          "wgpu device request failed for adapter {adapter_info:?}.\n\
requested: backends={:?} power_preference={:?} force_fallback_adapter={}\n\
error: {err}",
          wgpu_init.backends,
          wgpu_init.power_preference,
          wgpu_init.force_fallback_adapter,
        );
        if !wgpu_init.force_fallback_adapter {
          msg.push_str(&format!(
            "\nHint: Try enabling the software adapter with `--wgpu-fallback` or `{}`=1.",
            fastrender::ui::browser_cli::ENV_WGPU_FALLBACK
          ));
        }
        msg.push_str(&format!(
          "\nHint: Try forcing a different backend set with `--wgpu-backends gl` or `{}`=gl.",
          fastrender::ui::browser_cli::ENV_WGPU_BACKENDS
        ));
        std::io::Error::new(std::io::ErrorKind::Other, msg)
      })?;

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
      browser_limits: fastrender::ui::browser_limits::BrowserLimits::from_env(),
      ui_to_worker_tx,
      worker_join: Some(worker_join),
      browser_state: fastrender::ui::BrowserAppState::new(),
      tab_textures: std::collections::HashMap::new(),
      tab_favicons: std::collections::HashMap::new(),
      tab_cancel: std::collections::HashMap::new(),
      pending_frame_uploads: fastrender::ui::FrameUploadCoalescer::new(),
      page_rect_points: None,
      page_viewport_css: None,
      page_input_tab: None,
      page_input_mapping: None,
      overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars::default(),
      scrollbar_drag: None,
      viewport_cache_tab: None,
      viewport_cache_css: (0, 0),
      viewport_cache_dpr: 0.0,
      modifiers: winit::event::ModifiersState::default(),
      pending_clipboard_text: None,
      suppress_paste_events: false,
      window_focused: true,
      window_occluded: false,
      window_minimized: size.width == 0 || size.height == 0,
      page_has_focus: false,
      pointer_captured: false,
      captured_button: fastrender::ui::PointerButton::None,
      last_cursor_pos_points: None,
      cursor_in_page: false,
      page_cursor_override: None,
      pending_pointer_move: None,
      hover_sync_pending: false,
      pending_context_menu_request: None,
      open_context_menu: None,
      open_context_menu_rect: None,
      open_select_dropdown: None,
      open_select_dropdown_rect: None,
      debug_log: std::collections::VecDeque::new(),
      animation_tick_tab: None,
      next_animation_tick: None,
    })
  }

  fn startup(&mut self, session: fastrender::ui::BrowserSession) {
    use fastrender::ui::UiToWorker;

    let session = session.sanitized();
    let mut tab_ids = Vec::with_capacity(session.tabs.len());

    for tab in session.tabs {
      let tab_id = fastrender::ui::TabId::new();
      tab_ids.push(tab_id);

      let mut tab_state = fastrender::ui::BrowserTabState::new(tab_id, tab.url.clone());
      if let Some(zoom) = tab.zoom {
        tab_state.zoom = zoom;
      }
      let cancel = tab_state.cancel.clone();
      self.tab_cancel.insert(tab_id, cancel.clone());
      self.browser_state.push_tab(tab_state, false);

      self.send_worker_msg(UiToWorker::CreateTab {
        tab_id,
        initial_url: Some(tab.url),
        cancel,
      });
    }

    let active_idx = session.active_tab_index.min(tab_ids.len().saturating_sub(1));
    let active_tab_id = tab_ids[active_idx];
    self.browser_state.set_active_tab(active_tab_id);
    self.send_worker_msg(UiToWorker::SetActiveTab {
      tab_id: active_tab_id,
    });

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

  fn desired_animation_tick_tab(&self) -> Option<fastrender::ui::TabId> {
    if self.window_occluded || self.window_minimized || !self.window_focused {
      return None;
    }
    let tab_id = self.browser_state.active_tab_id()?;
    let wants_ticks = self
      .browser_state
      .tab(tab_id)
      .and_then(|tab| tab.latest_frame_meta.as_ref())
      .is_some_and(|meta| meta.wants_ticks);
    wants_ticks.then_some(tab_id)
  }

  fn drive_animation_tick(&mut self) {
    let Some(tab_id) = self.desired_animation_tick_tab() else {
      self.animation_tick_tab = None;
      self.next_animation_tick = None;
      return;
    };

    // If the active tab changed (or ticking just became enabled), start a fresh schedule.
    if self.animation_tick_tab != Some(tab_id) {
      self.animation_tick_tab = Some(tab_id);
      self.next_animation_tick = Some(std::time::Instant::now() + Self::ANIMATION_TICK_INTERVAL);
      return;
    }

    let now = std::time::Instant::now();
    let deadline = self.next_animation_tick.unwrap_or(now);
    if now >= deadline {
      self.send_worker_msg(fastrender::ui::UiToWorker::Tick { tab_id });
      self.next_animation_tick = Some(now + Self::ANIMATION_TICK_INTERVAL);
    }
  }

  fn update_control_flow_for_animation_ticks(
    &mut self,
    control_flow: &mut winit::event_loop::ControlFlow,
  ) {
    let Some(tab_id) = self.desired_animation_tick_tab() else {
      self.animation_tick_tab = None;
      self.next_animation_tick = None;
      return;
    };

    if self.animation_tick_tab != Some(tab_id) || self.next_animation_tick.is_none() {
      self.animation_tick_tab = Some(tab_id);
      self.next_animation_tick = Some(std::time::Instant::now() + Self::ANIMATION_TICK_INTERVAL);
    }

    if let Some(deadline) = self.next_animation_tick {
      *control_flow = winit::event_loop::ControlFlow::WaitUntil(deadline);
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
      | UiToWorker::ScrollTo { tab_id, .. }
      | UiToWorker::PointerMove { tab_id, .. }
      | UiToWorker::PointerDown { tab_id, .. }
      | UiToWorker::PointerUp { tab_id, .. }
      | UiToWorker::ContextMenuRequest { tab_id, .. }
      | UiToWorker::SelectDropdownChoose { tab_id, .. }
      | UiToWorker::SelectDropdownCancel { tab_id }
      | UiToWorker::SelectDropdownPick { tab_id, .. }
      | UiToWorker::TextInput { tab_id, .. }
      | UiToWorker::ImePreedit { tab_id, .. }
      | UiToWorker::ImeCommit { tab_id, .. }
      | UiToWorker::ImeCancel { tab_id }
      | UiToWorker::Paste { tab_id, .. }
      | UiToWorker::Copy { tab_id }
      | UiToWorker::Cut { tab_id }
      | UiToWorker::SelectAll { tab_id }
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
        | UiToWorker::ScrollTo { .. }
        | UiToWorker::PointerMove { .. }
        | UiToWorker::PointerDown { .. }
        | UiToWorker::PointerUp { .. }
        | UiToWorker::SelectDropdownChoose { .. }
        | UiToWorker::SelectDropdownCancel { .. }
        | UiToWorker::SelectDropdownPick { .. }
        | UiToWorker::TextInput { .. }
        | UiToWorker::ImePreedit { .. }
        | UiToWorker::ImeCommit { .. }
        | UiToWorker::ImeCancel { .. }
        | UiToWorker::Paste { .. }
        | UiToWorker::Cut { .. }
        | UiToWorker::KeyAction { .. }
        | UiToWorker::RequestRepaint { .. } => cancel.bump_paint(),
        // `Tick` and tab-management messages should not force cancellation.
        UiToWorker::Tick { .. }
        | UiToWorker::ContextMenuRequest { .. }
        | UiToWorker::CreateTab { .. }
        | UiToWorker::NewTab { .. }
        | UiToWorker::CloseTab { .. }
        | UiToWorker::SetActiveTab { .. }
        | UiToWorker::Copy { .. }
        | UiToWorker::SelectAll { .. } => {}
      }
    }

    let _ = self.ui_to_worker_tx.send(msg);
  }

  fn set_pixels_per_point(&mut self, ppp: f32) {
    self.pixels_per_point = ppp;
    self.egui_ctx.set_pixels_per_point(ppp);
    // Invalidate the cached viewport so the worker receives the new DPR: changing the DPI scale
    // factor affects the effective device pixel ratio used for rendering.
    self.viewport_cache_tab = None;
  }

  fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
    if new_size.width == 0 || new_size.height == 0 {
      return;
    }

    self.surface_config.width = new_size.width;
    self.surface_config.height = new_size.height;
    self.surface.configure(&self.device, &self.surface_config);
    // Invalidate the cached viewport so the worker sees the new dimensions on the next frame.
    self.viewport_cache_tab = None;
  }

  fn destroy_all_textures(&mut self) {
    for (_, tex) in std::mem::take(&mut self.tab_textures) {
      tex.destroy(&mut self.egui_renderer);
    }
    for (_, tex) in std::mem::take(&mut self.tab_favicons) {
      tex.destroy(&mut self.egui_renderer);
    }
  }

  fn close_context_menu(&mut self) {
    self.pending_context_menu_request = None;
    self.open_context_menu = None;
    self.open_context_menu_rect = None;
  }

  fn close_select_dropdown(&mut self) {
    self.open_select_dropdown = None;
    self.open_select_dropdown_rect = None;
  }

  fn cancel_select_dropdown(&mut self) {
    if let Some(dropdown) = self.open_select_dropdown.as_ref() {
      self.send_worker_msg(fastrender::ui::UiToWorker::select_dropdown_cancel(
        dropdown.tab_id,
      ));
    }
    self.close_select_dropdown();
  }

  fn flush_pending_pointer_move(&mut self) {
    let Some(msg) = self.pending_pointer_move.take() else {
      return;
    };
    if let Some(pos) = self.last_cursor_pos_points {
      if self
        .open_select_dropdown_rect
        .is_some_and(|rect| rect.contains(pos))
        || self.open_context_menu_rect.is_some_and(|rect| rect.contains(pos))
      {
        // Avoid updating page hover state while the pointer is interacting with a popup.
        return;
      }
    }
    self.send_worker_msg(msg);
  }

  fn apply_page_cursor_icon(&mut self) {
    use fastrender::ui::CursorKind;
    use winit::window::CursorIcon;

    let overlay_intercepts = self.last_cursor_pos_points.is_some_and(|pos| {
      self
        .open_select_dropdown_rect
        .is_some_and(|rect| rect.contains(pos))
        || self.open_context_menu_rect.is_some_and(|rect| rect.contains(pos))
    });

    if !self.cursor_in_page || overlay_intercepts {
      self.page_cursor_override = None;
      return;
    }

    let kind = self
      .browser_state
      .active_tab()
      .map(|tab| tab.cursor)
      .unwrap_or(CursorKind::Default);
    if self.page_cursor_override == Some(kind) {
      return;
    }
    self.page_cursor_override = Some(kind);

    let icon = match kind {
      CursorKind::Default => CursorIcon::Default,
      CursorKind::Pointer => CursorIcon::Hand,
      CursorKind::Text => CursorIcon::Text,
      CursorKind::Crosshair => CursorIcon::Crosshair,
      CursorKind::NotAllowed => CursorIcon::NotAllowed,
      CursorKind::Grab => CursorIcon::Grab,
      CursorKind::Grabbing => CursorIcon::Grabbing,
    };
    self.window.set_cursor_icon(icon);
  }

  fn update_open_select_dropdown_selection_for_key(
    &mut self,
    key: fastrender::interaction::KeyAction,
  ) {
    use fastrender::tree::box_tree::SelectItem;

    let Some(dropdown) = self.open_select_dropdown.as_mut() else {
      return;
    };

    let Some(selected_item_idx) =
      fastrender::select_dropdown::next_enabled_option_item_index(&dropdown.control, key)
    else {
      return;
    };

    // Update the local `SelectControl` snapshot so the popup highlights the same option that the
    // worker will select after handling the corresponding `UiToWorker::KeyAction`.
    //
    // This keeps the dropdown open while navigating with arrow keys, without requiring additional
    // worker→UI protocol messages.
    let mut items = (*dropdown.control.items).clone();
    let mut selected = Vec::new();
    for (idx, item) in items.iter_mut().enumerate() {
      match item {
        SelectItem::Option {
          selected: is_selected,
          disabled,
          ..
        } => {
          if idx == selected_item_idx && !*disabled {
            *is_selected = true;
            selected.push(idx);
          } else {
            *is_selected = false;
          }
        }
        SelectItem::OptGroupLabel { .. } => {}
      }
    }

    if selected.is_empty() {
      return;
    }

    dropdown.control.items = std::sync::Arc::new(items);
    dropdown.control.selected = selected;
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

  fn handle_worker_message(&mut self, msg: fastrender::ui::WorkerToUi) -> bool {
    // Worker-initiated tab creation/navigation.
    if let fastrender::ui::WorkerToUi::RequestOpenInNewTab { tab_id: _, url } = msg {
      use fastrender::ui::cancel::CancelGens;
      use fastrender::ui::messages::{NavigationReason, RepaintReason, UiToWorker};
      use fastrender::ui::{BrowserTabState, PointerButton, TabId};

      // Close any transient UI state before switching tabs.
      if self.open_select_dropdown.is_some() {
        self.cancel_select_dropdown();
      }
      if self.pointer_captured {
        self.cancel_pointer_capture();
      }

      let new_tab_id = TabId::new();
      let mut tab_state = BrowserTabState::new(new_tab_id, url.clone());
      tab_state.loading = true;
      let cancel: CancelGens = tab_state.cancel.clone();
      self.tab_cancel.insert(new_tab_id, cancel.clone());
      self.browser_state.push_tab(tab_state, true);

      // Reset per-tab cached state; mimic `ChromeAction::NewTab`/`ActivateTab` behaviour.
      self.page_has_focus = false;
      self.viewport_cache_tab = None;
      self.pointer_captured = false;
      self.captured_button = PointerButton::None;
      self.cursor_in_page = false;
      self.hover_sync_pending = true;
      self.pending_pointer_move = None;

      self.send_worker_msg(UiToWorker::CreateTab {
        tab_id: new_tab_id,
        initial_url: None,
        cancel,
      });
      self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: new_tab_id });
      self.send_worker_msg(UiToWorker::Navigate {
        tab_id: new_tab_id,
        url,
        reason: NavigationReason::LinkClick,
      });
      self.send_worker_msg(UiToWorker::RequestRepaint {
        tab_id: new_tab_id,
        reason: RepaintReason::Explicit,
      });

      return true;
    }

    // UI-only side effects that depend on the raw message before the shared reducer consumes it.
    match &msg {
      fastrender::ui::WorkerToUi::NavigationStarted { tab_id, .. }
      | fastrender::ui::WorkerToUi::NavigationCommitted { tab_id, .. }
      | fastrender::ui::WorkerToUi::NavigationFailed { tab_id, .. }
      | fastrender::ui::WorkerToUi::SelectDropdownClosed { tab_id } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|d| d.tab_id == *tab_id)
        {
          self.close_select_dropdown();
        }
        if self
          .open_context_menu
          .as_ref()
          .is_some_and(|menu| menu.tab_id == *tab_id)
        {
          self.close_context_menu();
        }
      }
      _ => {}
    }

    // Navigations reset a tab's favicon; drop any cached favicon textures eagerly so GPU resources
    // don't accumulate when switching between many pages.
    match &msg {
      fastrender::ui::WorkerToUi::NavigationStarted { tab_id, .. }
      | fastrender::ui::WorkerToUi::NavigationFailed { tab_id, .. } => {
        if let Some(tex) = self.tab_favicons.remove(tab_id) {
          tex.destroy(&mut self.egui_renderer);
        }
      }
      _ => {}
    }

    let mut request_redraw = false;

    if let fastrender::ui::WorkerToUi::DebugLog { tab_id, line } = &msg {
      eprintln!("[worker:{tab_id:?}] {line}");
      let line = line.trim_end();
      if !line.is_empty() {
        if self.debug_log.len() >= Self::DEBUG_LOG_MAX_LINES {
          self.debug_log.pop_front();
        }
        self
          .debug_log
          .push_back(format!("[tab {}] {}", tab_id.0, line));
        // Debug log is rendered via a bottom panel regardless of active tab.
        request_redraw = true;
      }
    }

    if let fastrender::ui::WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      link_url,
    } = &msg
    {
      if self.browser_state.active_tab_id() == Some(*tab_id) {
        if self
          .pending_context_menu_request
          .as_ref()
          .is_some_and(|pending| pending.tab_id == *tab_id && pending.pos_css == *pos_css)
        {
          let pending = self
            .pending_context_menu_request
            .take()
            .expect("checked is_some above");
          self.open_context_menu = Some(OpenContextMenu {
            tab_id: *tab_id,
            pos_css: *pos_css,
            anchor_points: pending.anchor_points,
            link_url: link_url.clone(),
          });
          self.open_context_menu_rect = None;
          request_redraw = true;
        }
      }
    }

    if let fastrender::ui::WorkerToUi::SetClipboardText { text, .. } = &msg {
      // Defer OS clipboard writes to the next egui frame so we can use egui-winit's platform output
      // plumbing.
      self.pending_clipboard_text = Some(text.clone());
      request_redraw = true;
    }

    let update = self.browser_state.apply_worker_msg(msg);

    if let Some(frame_ready) = update.frame_ready {
      // Ignore stale frames for tabs that have already been closed.
      if self.browser_state.tab(frame_ready.tab_id).is_some() {
        // Coalesce uploads until the next `render_frame`: uploading each intermediate pixmap is
        // expensive, and we do not upload frames for inactive/background tabs.
        self
          .pending_frame_uploads
          .push_for_active_tab(self.browser_state.active_tab_id(), frame_ready);
      }
    }

    if let Some(favicon_ready) = update.favicon_ready {
      // Ignore stale favicons for tabs that have already been closed.
      if self.browser_state.tab(favicon_ready.tab_id).is_some() {
        let size = tiny_skia::IntSize::from_wh(favicon_ready.width, favicon_ready.height);
        let pixmap = size.and_then(|size| tiny_skia::Pixmap::from_vec(favicon_ready.rgba, size));
        if let Some(pixmap) = pixmap {
          if let Some(tex) = self.tab_favicons.get_mut(&favicon_ready.tab_id) {
            tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
          } else {
            let mut tex = fastrender::ui::WgpuPixmapTexture::new_with_filter(
              &self.device,
              &mut self.egui_renderer,
              &pixmap,
              wgpu::FilterMode::Linear,
            );
            tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
            self.tab_favicons.insert(favicon_ready.tab_id, tex);
          }
        }
      }
    }

    if let Some(dropdown) = update.open_select_dropdown {
      if self.browser_state.active_tab_id() == Some(dropdown.tab_id) {
        // Legacy cursor-anchored dropdown message (kept for backwards compatibility in the core
        // protocol). Prefer `SelectDropdownOpened` (anchored to the `<select>` control); if the
        // control-anchored dropdown is already open for the same `<select>`, ignore the legacy
        // message so it doesn't override the better anchor.
        let control_anchor = dropdown
          .anchor_css
          .filter(|rect| *rect != fastrender::geometry::Rect::ZERO);
        let legacy_anchor = control_anchor.is_none();
        if legacy_anchor
          && self.open_select_dropdown.as_ref().is_some_and(|existing| {
            existing.tab_id == dropdown.tab_id
              && existing.select_node_id == dropdown.select_node_id
              && existing.anchored_to_control
          })
        {
          // Ignore.
        } else {
          let mut anchor_points = self
            .last_cursor_pos_points
            .or_else(|| self.page_rect_points.map(|rect| rect.center()))
            .unwrap_or_else(|| egui::pos2(0.0, 0.0));
          let mut anchor_width_points = None;
          if let Some(anchor_css) = control_anchor {
            if self.page_input_tab == Some(dropdown.tab_id) {
              if let Some(mapping) = self.page_input_mapping {
                if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
                  anchor_points = egui::pos2(rect_points.min.x, rect_points.max.y);
                  anchor_width_points = Some(rect_points.width());
                }
              }
            }
          }

          self.open_select_dropdown = Some(OpenSelectDropdown {
            tab_id: dropdown.tab_id,
            select_node_id: dropdown.select_node_id,
            control: dropdown.control,
            anchor_css: control_anchor,
            anchor_points,
            anchor_width_points,
            anchored_to_control: control_anchor.is_some(),
          });
          self.open_select_dropdown_rect = None;
          request_redraw = true;
        }
      }
    }

    request_redraw |= update.request_redraw;
    request_redraw
  }

  fn flush_pending_frame_uploads(&mut self) {
    let active_tab = self.browser_state.active_tab_id();
    for frame_ready in self.pending_frame_uploads.drain() {
      // If the active tab changed after we queued this frame (e.g. the user clicked another tab
      // before the next redraw), do not upload it.
      if Some(frame_ready.tab_id) != active_tab {
        continue;
      }

      // Ignore stale frames for tabs that have already been closed.
      if self.browser_state.tab(frame_ready.tab_id).is_none() {
        continue;
      }

      let tab_id = frame_ready.tab_id;
      let pixmap = frame_ready.pixmap;
      if let Some(tex) = self.tab_textures.get_mut(&tab_id) {
        tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
      } else {
        let mut tex =
          fastrender::ui::WgpuPixmapTexture::new(&self.device, &mut self.egui_renderer, &pixmap);
        tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
        self.tab_textures.insert(tab_id, tex);
      }
    }
  }

  fn send_viewport_changed_if_needed(&mut self, viewport_css: (u32, u32), dpr: f32) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      return;
    };

    // Clamp *before* sending to the worker so we never request an absurd RGBA pixmap allocation.
    let clamp = self.browser_limits.clamp_viewport_and_dpr(viewport_css, dpr);
    let viewport_css = clamp.viewport_css;
    let dpr = clamp.dpr;

    if let Some(tab) = self.browser_state.tab_mut(tab_id) {
      tab.warning = clamp.warning_text(&self.browser_limits);
    }

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

  fn render_hover_status(&mut self, ctx: &egui::Context) {
    let Some(url) = self
      .browser_state
      .active_tab()
      .and_then(|tab| tab.hovered_url.as_deref())
    else {
      return;
    };
    let Some(page_rect) = self.page_rect_points else {
      return;
    };

    let overlay_intercepts = self.last_cursor_pos_points.is_some_and(|pos| {
      self
        .open_select_dropdown_rect
        .is_some_and(|rect| rect.contains(pos))
        || self.open_context_menu_rect.is_some_and(|rect| rect.contains(pos))
    });

    if !self.cursor_in_page || overlay_intercepts {
      return;
    }

    // Position the status overlay at the bottom-left of the rendered page rect (in egui points).
    let padding = 4.0;
    let approx_height = 22.0;
    let pos = egui::pos2(
      page_rect.min.x + padding,
      (page_rect.max.y - padding - approx_height).max(page_rect.min.y + padding),
    );

    egui::Area::new(egui::Id::new("fastr_hover_status"))
      .order(egui::Order::Foreground)
      .fixed_pos(pos)
      .show(ctx, |ui| {
        egui::Frame::none()
          .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 200))
          .rounding(egui::Rounding::same(2.0))
          .inner_margin(egui::Margin::symmetric(6.0, 2.0))
          .show(ui, |ui| {
            ui.label(egui::RichText::new(url).small().color(egui::Color32::WHITE));
          });
      });
  }

  fn render_context_menu(&mut self, ctx: &egui::Context) {
    use fastrender::ui::ChromeAction;

    let (tab_id, pos_css, anchor_points, link_url) = match self.open_context_menu.as_ref() {
      Some(menu) => (
        menu.tab_id,
        menu.pos_css,
        menu.anchor_points,
        menu.link_url.clone(),
      ),
      None => {
        self.open_context_menu_rect = None;
        return;
      }
    };

    if self.browser_state.active_tab_id() != Some(tab_id) {
      self.close_context_menu();
      self.window.request_redraw();
      return;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.close_context_menu();
      self.window.request_redraw();
      return;
    }

    enum Action {
      OpenInNewTab(String),
      CopyLink(String),
      Reload,
    }

    let popup = egui::Area::new(egui::Id::new((
      "fastr_page_context_menu",
      tab_id.0,
      pos_css.0.to_bits(),
      pos_css.1.to_bits(),
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(anchor_points)
    .show(ctx, |ui| {
      let frame = egui::Frame::popup(ui.style()).show(ui, |ui| {
        let mut action: Option<Action> = None;

        if let Some(url) = link_url.as_deref() {
          if ui.button("Open Link in New Tab").clicked() {
            action = Some(Action::OpenInNewTab(url.to_string()));
          }
          if ui.button("Copy Link Address").clicked() {
            action = Some(Action::CopyLink(url.to_string()));
          }
          ui.separator();
        }

        if ui.button("Reload").clicked() {
          action = Some(Action::Reload);
        }

        action
      });

      (frame.response.rect, frame.inner)
    });

    let (popup_rect, action) = popup.inner;
    self.open_context_menu_rect = Some(popup_rect);

    let Some(action) = action else {
      return;
    };

    match action {
      Action::CopyLink(url) => {
        ctx.output_mut(|o| o.copied_text = url);
      }
      Action::Reload => {
        self.handle_chrome_actions(vec![ChromeAction::Reload]);
      }
      Action::OpenInNewTab(url) => {
        use fastrender::ui::RepaintReason;
        use fastrender::ui::UiToWorker;

        let tab_id = fastrender::ui::TabId::new();
        let tab_state = fastrender::ui::BrowserTabState::new(tab_id, url.clone());
        let cancel = tab_state.cancel.clone();
        self.tab_cancel.insert(tab_id, cancel.clone());

        self.browser_state.push_tab(tab_state, true);
        self.browser_state.chrome.address_bar_text = url.clone();
        self.page_has_focus = false;
        self.viewport_cache_tab = None;
        self.pointer_captured = false;
        self.captured_button = fastrender::ui::PointerButton::None;
        self.cursor_in_page = false;
        self.hover_sync_pending = true;
        self.pending_pointer_move = None;

        self.send_worker_msg(UiToWorker::CreateTab {
          tab_id,
          initial_url: Some(url),
          cancel,
        });
        self.send_worker_msg(UiToWorker::SetActiveTab { tab_id });
        self.send_worker_msg(UiToWorker::RequestRepaint {
          tab_id,
          reason: RepaintReason::Explicit,
        });
      }
    }

    self.close_context_menu();
    self.window.request_redraw();
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
      self.cancel_select_dropdown();
      self.window.request_redraw();
      return;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.cancel_select_dropdown();
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
                  let base = if label.trim().is_empty() {
                    value
                  } else {
                    label
                  };
                  let text = if *in_optgroup {
                    format!("  {base}")
                  } else {
                    base.to_string()
                  };

                  let response =
                    ui.add_enabled(!*disabled, egui::SelectableLabel::new(*selected, text));
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
      node_id: option_dom_id,
      disabled,
      ..
    }) = control.items.get(clicked_item_idx)
    else {
      self.cancel_select_dropdown();
      self.window.request_redraw();
      return;
    };
    if *disabled {
      self.cancel_select_dropdown();
      self.window.request_redraw();
      return;
    }

    // Apply selection directly rather than synthesizing key events.
    self.send_worker_msg(UiToWorker::select_dropdown_choose(
      tab_id,
      select_node_id,
      *option_dom_id,
    ));

    self.close_select_dropdown();
    self.window.request_redraw();
  }

  fn sync_hover_after_tab_change(&mut self, ctx: &egui::Context) {
    use fastrender::ui::PointerButton;
    use fastrender::ui::UiToWorker;

    if !self.hover_sync_pending {
      return;
    }

    let Some(tab_id) = self.page_input_tab else {
      // We don't yet know where the page image is drawn (e.g. no frame uploaded). Retry on the
      // next frame.
      return;
    };
    let Some(mapping) = self.page_input_mapping else {
      return;
    };

    let pos_points = self
      .last_cursor_pos_points
      .or_else(|| ctx.input(|i| i.pointer.hover_pos()));
    let Some(pos_points) = pos_points else {
      // Cursor position is unknown (outside window, or never moved). Bail rather than retrying
      // indefinitely.
      self.hover_sync_pending = false;
      self.cursor_in_page = false;
      return;
    };

    // Avoid updating page hover state while the pointer is interacting with a dropdown popup.
    if self
      .open_select_dropdown_rect
      .is_some_and(|rect| rect.contains(pos_points))
      || self.open_context_menu_rect.is_some_and(|rect| rect.contains(pos_points))
    {
      self.hover_sync_pending = false;
      self.cursor_in_page = false;
      return;
    }

    // Overlay scrollbars behave like UI chrome (not page content). If the cursor is currently over
    // a scrollbar track, send a sentinel pointer-move so the worker clears hover/cursor state
    // instead of hit-testing against the rendered page.
    if self.cursor_over_overlay_scrollbars(pos_points) {
      self.cursor_in_page = false;
      self.pending_pointer_move = Some(UiToWorker::PointerMove {
        tab_id,
        pos_css: (-1.0, -1.0),
        button: PointerButton::None,
        modifiers: map_modifiers(self.modifiers),
      });
      self.hover_sync_pending = false;
      return;
    }

    if let Some(pos_css) = mapping.pos_points_to_pos_css_if_inside(pos_points) {
      self.cursor_in_page = true;
      self.pending_pointer_move = Some(UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button: PointerButton::None,
        modifiers: map_modifiers(self.modifiers),
      });
    } else {
      self.cursor_in_page = false;
    }

    self.hover_sync_pending = false;
  }

  fn focus_address_bar_select_all(&mut self) {
    self.page_has_focus = false;
    self.browser_state.chrome.request_focus_address_bar = true;
    self.browser_state.chrome.request_select_all_address_bar = true;
  }

  fn cancel_pointer_capture(&mut self) {
    if !self.pointer_captured {
      return;
    }
    self.flush_pending_pointer_move();
    self.pointer_captured = false;

    let button = self.captured_button;
    self.captured_button = fastrender::ui::PointerButton::None;

    // Best-effort: when we lose pointer capture (e.g. cursor leaves the window), synthesize a
    // PointerUp so the worker can clear `:active` state and end in-progress drags.
    if let Some(tab_id) = self.page_input_tab.or(self.browser_state.active_tab_id()) {
      self.send_worker_msg(fastrender::ui::UiToWorker::PointerUp {
        tab_id,
        pos_css: (-1.0, -1.0),
        button,
        modifiers: fastrender::ui::PointerModifiers::NONE,
      });
    }
  }

  fn cancel_scrollbar_drag(&mut self) {
    if self.scrollbar_drag.is_none() {
      return;
    }
    self.scrollbar_drag = None;

    let cursor_inside_page = self.last_cursor_pos_points.is_some_and(|pos| {
      self
        .page_rect_points
        .is_some_and(|page_rect| page_rect.contains(pos))
    });

    if cursor_inside_page {
      self.hover_sync_pending = true;
    } else {
      // When ending a scrollbar drag with the cursor outside the page rect, ensure the worker's
      // hover state is cleared.
      self.cursor_in_page = false;
      self.clear_page_hover();
    }
  }

  fn clear_page_hover(&mut self) {
    let Some(tab_id) = self.page_input_tab.or(self.browser_state.active_tab_id()) else {
      return;
    };
    self.pending_pointer_move = Some(fastrender::ui::UiToWorker::PointerMove {
      tab_id,
      pos_css: (-1.0, -1.0),
      button: fastrender::ui::PointerButton::None,
      modifiers: fastrender::ui::PointerModifiers::NONE,
    });
    self.flush_pending_pointer_move();
    self.cursor_in_page = false;
  }

  fn handle_winit_input_event(&mut self, event: &winit::event::WindowEvent<'_>) {
    use winit::event::ElementState;
    use winit::event::Ime;
    use winit::event::VirtualKeyCode;
    use winit::event::WindowEvent;

    match event {
      WindowEvent::Occluded(occluded) => {
        self.window_occluded = *occluded;
      }
      WindowEvent::Focused(focused) => {
        self.window_focused = *focused;
        if *focused {
          return;
        }
        // Losing window focus should cancel temporary UI state such as `<select>` popups and active
        // pointer drags.
        if self.open_select_dropdown.is_some() {
          self.cancel_select_dropdown();
          self.window.request_redraw();
        }
        if self.open_context_menu.is_some() || self.pending_context_menu_request.is_some() {
          self.close_context_menu();
          self.window.request_redraw();
        }
        if self.scrollbar_drag.is_some() {
          self.cancel_scrollbar_drag();
          self.window.request_redraw();
        }
        if self.pointer_captured {
          self.cancel_pointer_capture();
          self.window.request_redraw();
        }
      }
      WindowEvent::CursorLeft { .. } => {
        let had_pointer_capture = self.pointer_captured;
        let had_scrollbar_drag = self.scrollbar_drag.is_some();
        let had_cursor_in_page = self.cursor_in_page;
        let had_context_menu = self.open_context_menu.is_some() || self.pending_context_menu_request.is_some();

        // Winit does not provide cursor coordinates when leaving the window. Clear our cached
        // position so hover updates are not suppressed by stale dropdown rect checks.
        self.last_cursor_pos_points = None;

        if had_context_menu {
          self.close_context_menu();
        }

        if had_pointer_capture {
          self.cancel_pointer_capture();
        }
        if had_scrollbar_drag {
          self.cancel_scrollbar_drag();
        }

        if had_cursor_in_page || had_pointer_capture {
          self.clear_page_hover();
        }
        if had_cursor_in_page || had_pointer_capture || had_scrollbar_drag || had_context_menu {
          self.window.request_redraw();
        }
      }
      WindowEvent::CursorMoved { position, .. } => {
        let pos_points = egui::pos2(
          position.x as f32 / self.pixels_per_point,
          position.y as f32 / self.pixels_per_point,
        );
        self.last_cursor_pos_points = Some(pos_points);

        if self.scrollbar_drag.is_some() {
          let (tab_id, axis, scrollbar, axis_delta_points) = {
            let drag = self.scrollbar_drag.as_mut().unwrap();
            let delta_points = pos_points - drag.last_cursor_points;
            drag.last_cursor_points = pos_points;
            let axis_delta_points = match drag.axis {
              fastrender::ui::scrollbars::ScrollbarAxis::Vertical => delta_points.y,
              fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => delta_points.x,
            };
            (drag.tab_id, drag.axis, drag.scrollbar, axis_delta_points)
          };

          let axis_delta_css = scrollbar.scroll_delta_css_for_thumb_drag_points(axis_delta_points);
          if axis_delta_css != 0.0 {
            let delta_css = match axis {
              fastrender::ui::scrollbars::ScrollbarAxis::Vertical => (0.0, axis_delta_css),
              fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => (axis_delta_css, 0.0),
            };
            self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
              tab_id,
              delta_css,
              pointer_css: None,
            });
          }
          self.window.request_redraw();
          return;
        }

        if self
          .open_select_dropdown_rect
          .is_some_and(|rect| rect.contains(pos_points))
          || self.open_context_menu_rect.is_some_and(|rect| rect.contains(pos_points))
        {
          return;
        }

        let Some(rect) = self.page_rect_points else {
          self.cursor_in_page = false;
          return;
        };
        let mut now_in_page = rect.contains(pos_points);
        if now_in_page
          && !self.pointer_captured
          && self.cursor_over_overlay_scrollbars(pos_points)
        {
          now_in_page = false;
        }

        // `page_input_mapping`/`page_input_tab` are populated during the most recent paint. When
        // they are missing, we cannot reliably map points→CSS, so we just track whether the cursor
        // is inside the page rect.
        let Some(tab_id) = self.page_input_tab else {
          self.cursor_in_page = now_in_page;
          return;
        };
        let Some(mapping) = self.page_input_mapping else {
          self.cursor_in_page = now_in_page;
          return;
        };

        // Send pointer moves for:
        // - hover updates while inside the page rect,
        // - a single sentinel move when leaving the page to clear hover,
        // - all moves while a button is held down (captured), even outside the rect.
        let should_send = self.pointer_captured || now_in_page || self.cursor_in_page;
        if !should_send {
          self.cursor_in_page = false;
          return;
        }

        let pos_css = if now_in_page {
          let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(pos_points) else {
            return;
          };
          pos_css
        } else {
          (-1.0, -1.0)
        };

        let button = if self.pointer_captured {
          self.captured_button
        } else {
          fastrender::ui::PointerButton::None
        };
        self.pending_pointer_move = Some(fastrender::ui::UiToWorker::PointerMove {
          tab_id,
          pos_css,
          button,
          modifiers: map_modifiers(self.modifiers),
        });
        // `egui_winit` may not request a repaint for pointer moves inside a single widget. We need
        // a redraw so `render_frame` can flush the coalesced PointerMove to the worker.
        self.window.request_redraw();
        self.cursor_in_page = now_in_page;
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

        if self.scrollbar_drag.is_some() {
          if matches!(state, ElementState::Released)
            && matches!(mapped_button, fastrender::ui::PointerButton::Primary)
          {
            self.cancel_scrollbar_drag();
            self.window.request_redraw();
          }
          return;
        }

        let Some(pos_points) = self.last_cursor_pos_points else {
          return;
        };

        if matches!(state, ElementState::Pressed) && self.open_context_menu.is_some() {
          // While the context menu is open, clicks inside it are handled by egui. Any click outside
          // should dismiss it before we forward the interaction to the page/chrome.
          if self
            .open_context_menu_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            return;
          }
          self.close_context_menu();
          self.window.request_redraw();
        }

        if matches!(state, ElementState::Pressed) && self.open_select_dropdown.is_some() {
          // If the dropdown popup is open, clicks inside it are handled by egui (option selection).
          if self
            .open_select_dropdown_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            return;
          }

          // Close the dropdown before processing the click so we don't require a second click to
          // interact with the underlying page/chrome.
          //
          // Special-case: clicking the `<select>` control itself should typically just toggle the
          // popup closed (don't immediately reopen it by forwarding the click to the page).
          let clicked_select_control = self.open_select_dropdown.as_ref().is_some_and(|dropdown| {
            dropdown.anchor_css.is_some_and(|anchor_css| {
              self
                .page_input_mapping
                .and_then(|mapping| mapping.rect_css_to_rect_points_clamped(anchor_css))
                .is_some_and(|rect_points| rect_points.contains(pos_points))
            })
          });

          self.cancel_select_dropdown();
          self.window.request_redraw();
          if clicked_select_control {
            return;
          }
        }

        match state {
          ElementState::Pressed => {
            // Only track one "captured" pointer interaction at a time. When a primary-button drag
            // is in progress, ignore additional mouse button presses until the primary button is
            // released/cancelled.
            if self.pointer_captured {
              return;
            }

            // Scrollbar track/thumb interactions should not be forwarded to the page worker.
            if matches!(mapped_button, fastrender::ui::PointerButton::Primary) {
              let Some(tab_id) = self.page_input_tab else {
                return;
              };
              let pos = fastrender::Point::new(pos_points.x, pos_points.y);

              if let Some(scrollbar) = self
                .overlay_scrollbars
                .vertical
                .filter(|sb| sb.thumb_rect_points.contains_point(pos))
                .or_else(|| {
                  self
                    .overlay_scrollbars
                    .horizontal
                    .filter(|sb| sb.thumb_rect_points.contains_point(pos))
                })
              {
                self.scrollbar_drag = Some(ScrollbarDrag {
                  tab_id,
                  axis: scrollbar.axis,
                  last_cursor_points: pos_points,
                  scrollbar,
                });
                self.window.request_redraw();
                return;
              }

              if let Some(delta_y) = self
                .overlay_scrollbars
                .vertical
                .and_then(|sb| sb.page_delta_css_for_track_click(pos))
              {
                self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
                  tab_id,
                  delta_css: (0.0, delta_y),
                  pointer_css: None,
                });
                self.window.request_redraw();
                return;
              }
              if let Some(delta_x) = self
                .overlay_scrollbars
                .horizontal
                .and_then(|sb| sb.page_delta_css_for_track_click(pos))
              {
                self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
                  tab_id,
                  delta_css: (delta_x, 0.0),
                  pointer_css: None,
                });
                self.window.request_redraw();
                return;
              }
            }

            // Ensure any pending hover update is applied before we start a new pointer interaction.
            self.flush_pending_pointer_move();
            let Some(rect) = self.page_rect_points else {
              return;
            };
            if !rect.contains(pos_points) {
              return;
            }
            let pos = fastrender::Point::new(pos_points.x, pos_points.y);
            if self
              .overlay_scrollbars
              .vertical
              .is_some_and(|sb| sb.track_rect_points.contains_point(pos))
              || self
                .overlay_scrollbars
                .horizontal
                .is_some_and(|sb| sb.track_rect_points.contains_point(pos))
            {
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

            if matches!(mapped_button, fastrender::ui::PointerButton::Secondary) {
              // Right-click: request worker hit-test and open an egui context menu once the worker
              // responds with link information.
              self.page_has_focus = true;
              self.cursor_in_page = true;
              self.close_context_menu();
              self.pending_context_menu_request = Some(PendingContextMenuRequest {
                tab_id,
                pos_css,
                anchor_points: pos_points,
              });
              self.send_worker_msg(fastrender::ui::UiToWorker::ContextMenuRequest { tab_id, pos_css });
              self.window.request_redraw();
              return;
            }

            self.page_has_focus = true;
            if matches!(mapped_button, fastrender::ui::PointerButton::Primary) {
              self.pointer_captured = true;
              self.captured_button = mapped_button;
            }
            self.cursor_in_page = true;
            self.send_worker_msg(fastrender::ui::UiToWorker::PointerDown {
              tab_id,
              pos_css,
              button: mapped_button,
              modifiers: map_modifiers(self.modifiers),
            });
          }
          ElementState::Released => {
            if self.pointer_captured {
              if !matches!(mapped_button, fastrender::ui::PointerButton::Primary)
                || !matches!(self.captured_button, fastrender::ui::PointerButton::Primary)
              {
                return;
              }
              // Flush any coalesced pointer moves so interactions (e.g. range drags) see the latest
              // pointer position before the release.
              self.flush_pending_pointer_move();
              self.pointer_captured = false;
              self.captured_button = fastrender::ui::PointerButton::None;
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
            let in_page = rect.contains(pos_points);
            let pos_css = if in_page {
              let Some(pos_css) = mapping.pos_points_to_pos_css_clamped(pos_points) else {
                return;
              };
              pos_css
            } else {
              (-1.0, -1.0)
            };
            self.send_worker_msg(fastrender::ui::UiToWorker::PointerUp {
              tab_id,
              pos_css,
              button: mapped_button,
              modifiers: map_modifiers(self.modifiers),
            });
            self.cursor_in_page = in_page;
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
            self.cancel_select_dropdown();
            self.window.request_redraw();
            return;
          }

          if matches!(
            key,
            VirtualKeyCode::Return | VirtualKeyCode::NumpadEnter | VirtualKeyCode::Space
          ) {
            let choice = self.open_select_dropdown.as_ref().and_then(|dropdown| {
              fastrender::select_dropdown::selected_choice(
                dropdown.select_node_id,
                &dropdown.control,
              )
              .map(|choice| {
                (
                  dropdown.tab_id,
                  choice.select_node_id,
                  choice.option_node_id,
                )
              })
            });

            if let Some((tab_id, select_node_id, option_node_id)) = choice {
              self.send_worker_msg(fastrender::ui::UiToWorker::select_dropdown_choose(
                tab_id,
                select_node_id,
                option_node_id,
              ));
            }

            self.close_select_dropdown();
            self.window.request_redraw();
            return;
          }
 
           let dropdown_nav_key = match key {
             VirtualKeyCode::Up => Some(fastrender::interaction::KeyAction::ArrowUp),
             VirtualKeyCode::Down => Some(fastrender::interaction::KeyAction::ArrowDown),
             VirtualKeyCode::Home => Some(fastrender::interaction::KeyAction::Home),
             VirtualKeyCode::End => Some(fastrender::interaction::KeyAction::End),
             _ => None,
           };
           if let Some(nav_key) = dropdown_nav_key {
             self.update_open_select_dropdown_selection_for_key(nav_key);
             self.window.request_redraw();
             return;
           } else {
             self.cancel_select_dropdown();
             self.window.request_redraw();
           }
         }

        // Centralised shortcut handling: interpret as a browser shortcut first, and only forward
        // to the page when it isn't reserved.
        if let Some(shortcut_key) = map_winit_key_to_shortcuts_key(key) {
          let shortcut_modifiers = winit_modifiers_to_shortcuts_modifiers(self.modifiers);
          let shortcut_action = fastrender::ui::shortcuts::map_shortcut(
            fastrender::ui::shortcuts::KeyEvent::new(shortcut_key, shortcut_modifiers),
          );
          if let Some(action) = shortcut_action {
            use fastrender::ui::shortcuts::ShortcutAction;

            match action {
              // Chrome-level shortcuts are evaluated inside the egui frame (`ui::chrome_ui`) so we
              // can respect its editing focus rules. Ensure they never reach page input.
              ShortcutAction::FocusAddressBar
              | ShortcutAction::NewTab
              | ShortcutAction::CloseTab
              | ShortcutAction::ReopenClosedTab
              | ShortcutAction::NextTab
              | ShortcutAction::PrevTab
              | ShortcutAction::Back
              | ShortcutAction::Forward
              | ShortcutAction::Reload
              | ShortcutAction::ActivateTabNumber(_)
              | ShortcutAction::ZoomIn
              | ShortcutAction::ZoomOut
              | ShortcutAction::ZoomReset => {
                return;
              }

              // Page-level shortcuts only apply when the rendered page has focus and egui isn't
              // actively editing text (e.g. address bar).
              ShortcutAction::Copy
              | ShortcutAction::Cut
              | ShortcutAction::Paste
              | ShortcutAction::SelectAll
              | ShortcutAction::PageUp
              | ShortcutAction::PageDown => {
                // If egui is actively editing text (e.g. the address bar), don't handle page-level
                // key events.
                if self.egui_ctx.wants_keyboard_input() {
                  return;
                }
                if !self.page_has_focus {
                  return;
                }
                let Some(tab_id) = self.browser_state.active_tab_id() else {
                  return;
                };
 
                  match action {
                    ShortcutAction::PageUp | ShortcutAction::PageDown => {
                      let viewport_css = self
                        .page_viewport_css
                        .or_else(|| {
                          self
                           .browser_state
                           .tab(tab_id)
                           .and_then(|tab| tab.latest_frame_meta.as_ref())
                           .map(|meta| meta.viewport_css)
                       })
                       .unwrap_or((0, 0));
                      let h = viewport_css.1.max(1) as f32;
                      let mut dy = (h * 0.9).max(1.0);
                      if matches!(action, ShortcutAction::PageUp) {
                        dy = -dy;
                      }
                      self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
                        tab_id,
                       delta_css: (0.0, dy),
                       pointer_css: None,
                      });
                    }
                     ShortcutAction::Copy => self.send_worker_msg(fastrender::ui::UiToWorker::Copy { tab_id }),
                     ShortcutAction::Cut => self.send_worker_msg(fastrender::ui::UiToWorker::Cut { tab_id }),
                     ShortcutAction::SelectAll => {
                       self.send_worker_msg(fastrender::ui::UiToWorker::SelectAll { tab_id })
                    }
                    ShortcutAction::Paste => {
                      if let Ok(mut clipboard) = Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() {
                          // egui-winit can also emit `egui::Event::Paste` from Ctrl/Cmd+V. Suppress
                          // it for this frame to avoid double pastes.
                          self.suppress_paste_events = true;
                          self
                            .send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
                        }
                      }
                    }
                    _ => {}
                  }
                 return;
                }
              // Allow these keys to be forwarded to the page so focused text controls can handle
              // them for caret navigation and text entry.
              ShortcutAction::Space | ShortcutAction::Home | ShortcutAction::End => {}
            }
          }
        }

        // If egui is actively editing text (e.g. the address bar), don't handle page-level key
        // events.
        if self.egui_ctx.wants_keyboard_input() {
          return;
        }

        // Ctrl/Cmd+Tab is reserved for chrome tab switching; don't forward it to the page as a Tab
        // key press.
        if (self.modifiers.ctrl() || self.modifiers.logo()) && matches!(key, VirtualKeyCode::Tab) {
          return;
        }

        // Alt+Left/Right are reserved for chrome back/forward navigation (handled in the egui
        // chrome layer). Don't forward them to the page as caret movement.
        //
        // Guard against AltGr (often encoded as Ctrl+Alt).
        let alt_only = self.modifiers.alt() && !(self.modifiers.ctrl() || self.modifiers.logo());
        if alt_only && matches!(key, VirtualKeyCode::Left | VirtualKeyCode::Right) {
          return;
        }
        if !self.page_has_focus {
          return;
        }
        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };

        // Ctrl/Cmd+A selects all in the focused text control.
        //
        // Guard against AltGr (often encoded as Ctrl+Alt).
        if (self.modifiers.ctrl() || self.modifiers.logo())
          && !self.modifiers.alt()
          && matches!(key, VirtualKeyCode::A)
        {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: fastrender::interaction::KeyAction::SelectAll,
          });
          return;
        }

        let key_action = match key {
          VirtualKeyCode::Back => Some(fastrender::interaction::KeyAction::Backspace),
          VirtualKeyCode::Delete => Some(fastrender::interaction::KeyAction::Delete),
          VirtualKeyCode::Return => Some(fastrender::interaction::KeyAction::Enter),
          VirtualKeyCode::NumpadEnter => Some(fastrender::interaction::KeyAction::Enter),
          VirtualKeyCode::Space => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftSpace
          } else {
            fastrender::interaction::KeyAction::Space
          }),
          VirtualKeyCode::Tab => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftTab
          } else {
            fastrender::interaction::KeyAction::Tab
          }),
          VirtualKeyCode::Left => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftArrowLeft
          } else {
            fastrender::interaction::KeyAction::ArrowLeft
          }),
          VirtualKeyCode::Right => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftArrowRight
          } else {
            fastrender::interaction::KeyAction::ArrowRight
          }),
          VirtualKeyCode::Up => Some(fastrender::interaction::KeyAction::ArrowUp),
          VirtualKeyCode::Down => Some(fastrender::interaction::KeyAction::ArrowDown),
          VirtualKeyCode::Home => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftHome
          } else {
            fastrender::interaction::KeyAction::Home
          }),
          VirtualKeyCode::End => Some(if self.modifiers.shift() {
            fastrender::interaction::KeyAction::ShiftEnd
          } else {
            fastrender::interaction::KeyAction::End
          }),
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
      WindowEvent::Ime(ime) => {
        // If egui is actively editing text (e.g. the address bar), don't handle page-level IME
        // events.
        if !self.page_has_focus || self.egui_ctx.wants_keyboard_input() {
          return;
        }

        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };

        // `<select>` dropdown popups own keyboard interaction; dismiss them before IME editing.
        if self.open_select_dropdown.is_some() {
          self.cancel_select_dropdown();
        }

        match ime {
          Ime::Preedit(text, cursor_range) => {
            if text.is_empty() {
              self.send_worker_msg(fastrender::ui::UiToWorker::ImeCancel { tab_id });
            } else {
              let cursor = cursor_range.as_ref().copied();
              self.send_worker_msg(fastrender::ui::UiToWorker::ImePreedit {
                tab_id,
                text: text.clone(),
                cursor,
              });
            }
            self.window.request_redraw();
          }
          Ime::Commit(text) => {
            if text.is_empty() {
              self.send_worker_msg(fastrender::ui::UiToWorker::ImeCancel { tab_id });
            } else {
              self.send_worker_msg(fastrender::ui::UiToWorker::ImeCommit {
                tab_id,
                text: text.clone(),
              });
            }
            self.window.request_redraw();
          }
          Ime::Disabled => {
            self.send_worker_msg(fastrender::ui::UiToWorker::ImeCancel { tab_id });
            self.window.request_redraw();
          }
          Ime::Enabled => {}
        }
      }
      WindowEvent::ReceivedCharacter(ch) => {
        if !self.page_has_focus || self.egui_ctx.wants_keyboard_input() {
          return;
        }
        // Avoid forwarding browser-chrome shortcuts (e.g. Ctrl/Cmd+L) as text input to the page.
        //
        // We intentionally still forward Ctrl+Alt combinations to avoid breaking AltGr-based text
        // entry on some keyboard layouts.
        if self.modifiers.logo() || (self.modifiers.ctrl() && !self.modifiers.alt()) {
          return;
        }
        if ch.is_control() {
          return;
        }
        if self.open_select_dropdown.is_some() {
          self.cancel_select_dropdown();
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
      self.cancel_select_dropdown();
      self.cancel_pointer_capture();
      self.close_context_menu();
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
          self.pointer_captured = false;
          self.captured_button = fastrender::ui::PointerButton::None;
          self.cursor_in_page = false;
          self.hover_sync_pending = true;
          self.pending_pointer_move = None;
          self.pending_frame_uploads.clear();

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
        ChromeAction::ReopenClosedTab => {
          let Some(closed) = self.browser_state.pop_closed_tab() else {
            continue;
          };

          let tab_id = fastrender::ui::TabId::new();
          let url = closed.url;
          let mut tab_state = fastrender::ui::BrowserTabState::new(tab_id, url.clone());
          tab_state.title = closed.title.clone();
          tab_state.committed_title = closed.title;
          tab_state.loading = true;

          let cancel = tab_state.cancel.clone();
          self.tab_cancel.insert(tab_id, cancel.clone());
          self.browser_state.push_tab(tab_state, true);
          self.browser_state.chrome.address_bar_text = url.clone();
          self.page_has_focus = false;
          self.viewport_cache_tab = None;
          self.pointer_captured = false;
          self.captured_button = fastrender::ui::PointerButton::None;
          self.cursor_in_page = false;
          self.hover_sync_pending = true;
          self.pending_pointer_move = None;

          self.send_worker_msg(UiToWorker::CreateTab {
            tab_id,
            initial_url: Some(url),
            cancel,
          });
          self.send_worker_msg(UiToWorker::SetActiveTab { tab_id });
          self.send_worker_msg(UiToWorker::RequestRepaint {
            tab_id,
            reason: RepaintReason::Explicit,
          });

          // Request a second frame so chrome UI reflects the newly created tab immediately.
          self.window.request_redraw();
        }
        ChromeAction::CloseTab(tab_id) => {
          if self.browser_state.tabs.len() <= 1 || self.browser_state.tab(tab_id).is_none() {
            continue;
          }

          self.pending_frame_uploads.remove_tab(tab_id);
          if let Some(tex) = self.tab_textures.remove(&tab_id) {
            tex.destroy(&mut self.egui_renderer);
          }
          if let Some(tex) = self.tab_favicons.remove(&tab_id) {
            tex.destroy(&mut self.egui_renderer);
          }

          let was_active = self.browser_state.active_tab_id() == Some(tab_id);
          if let Some(cancel) = self.tab_cancel.remove(&tab_id) {
            cancel.bump_nav();
          }
          self.send_worker_msg(UiToWorker::CloseTab { tab_id });

          let close_result = self.browser_state.remove_tab(tab_id);

          if was_active {
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.pending_pointer_move = None;
            self.pending_frame_uploads.clear();
          }

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
            self.send_worker_msg(UiToWorker::SetActiveTab {
              tab_id: created_tab,
            });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            self.hover_sync_pending = true;
            self.pending_pointer_move = None;
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id: created_tab,
              reason: RepaintReason::Explicit,
            });

            self.focus_address_bar_select_all();
            self.window.request_redraw();
          } else if let Some(new_active) = close_result.new_active {
            self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: new_active });
            self.viewport_cache_tab = None;
            self.page_has_focus = false;
            self.hover_sync_pending = true;
            self.pending_pointer_move = None;
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
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.hover_sync_pending = true;
            self.pending_pointer_move = None;
            self.pending_frame_uploads.clear();
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
    // Upload any newly received page pixmaps now (coalesced). We do this right before drawing so
    // multiple `FrameReady` messages received between redraws result in a single GPU upload.
    self.flush_pending_frame_uploads();

    let (raw_input, wheel_events, paste_events) = {
      let mut raw = self.egui_state.take_egui_input(&self.window);
      raw.pixels_per_point = Some(self.pixels_per_point);
      let wheel_events = raw
        .events
        .iter()
        .filter_map(|event| match event {
          egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
          } => {
            // Ctrl/Cmd+wheel is treated as zoom (handled in `ui::chrome_ui`), so do not forward it
            // to the page scroll pipeline.
            if modifiers.command {
              None
            } else {
              Some((*unit, *delta))
            }
          }
          _ => None,
        })
        .collect::<Vec<_>>();
      let paste_events = raw
        .events
        .iter()
        .filter_map(|event| match event {
          egui::Event::Paste(text) => Some(text.clone()),
          _ => None,
        })
        .collect::<Vec<_>>();
      (raw, wheel_events, paste_events)
    };

    self.egui_ctx.begin_frame(raw_input);

    let ctx = self.egui_ctx.clone();

    let chrome_actions = fastrender::ui::chrome_ui(&ctx, &mut self.browser_state, |tab_id| {
      self.tab_favicons.get(&tab_id).map(|tex| tex.id())
    });
    self.handle_chrome_actions(chrome_actions);
    self.sync_window_title();

    let suppress_paste_events = std::mem::take(&mut self.suppress_paste_events);
    if !paste_events.is_empty()
      && self.page_has_focus
      && !self.egui_ctx.wants_keyboard_input()
      && !suppress_paste_events
    {
      if let Some(tab_id) = self.browser_state.active_tab_id() {
        for text in paste_events {
          self.send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
        }
      }
    }

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

      // Browser-like zoom: keep the drawn page size constant (in egui points) while scaling the
      // number of CSS pixels in the viewport by adjusting viewport_css + dpr.
      let zoom = self
        .browser_state
        .active_tab()
        .map(|t| t.zoom)
        .unwrap_or(fastrender::ui::DEFAULT_ZOOM);
      let (viewport_css, dpr) = fastrender::ui::viewport_css_and_dpr_for_zoom(
        (logical_viewport_points.x, logical_viewport_points.y),
        self.pixels_per_point,
        zoom,
      );
      self.send_viewport_changed_if_needed(viewport_css, dpr);

      self.page_rect_points = None;
      self.page_viewport_css = None;
      self.page_input_tab = None;
      self.page_input_mapping = None;
      self.overlay_scrollbars = fastrender::ui::scrollbars::OverlayScrollbars::default();

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
            self.cancel_select_dropdown();
          }
        }
      }

      if let Some(tex) = self.tab_textures.get(&active_tab) {
        let viewport_css_for_mapping = self
          .browser_state
          .tab(active_tab)
          .and_then(|tab| tab.latest_frame_meta.as_ref().map(|m| m.viewport_css))
          .or_else(|| {
            (self.viewport_cache_tab == Some(active_tab)).then_some(self.viewport_cache_css)
          })
          .unwrap_or(viewport_css);
        // Draw the page image to fill the available panel size (egui points), even when:
        // - the worker clamps DPR/viewport for safety (pixmap may be smaller than panel in points),
        // - the per-tab zoom mapping changes `viewport_css` (we keep physical size constant).
        //
        // The input mapping (points→CSS) uses `viewport_css_for_mapping`, so scaling here stays
        // coherent for hit-testing.
        let size_points = logical_viewport_points.max(egui::Vec2::ZERO);
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

        // Overlay scrollbars (visual only; interactions are handled by the winit event path so we
        // can reliably suppress forwarding pointer events to the page worker).
        if let Some(tab) = self.browser_state.tab(active_tab) {
          if let Some(metrics) = tab.scroll_metrics {
            let page_rect_points = fastrender::Rect::from_xywh(
              response.rect.min.x,
              response.rect.min.y,
              response.rect.width(),
              response.rect.height(),
            );
            self.overlay_scrollbars = fastrender::ui::scrollbars::overlay_scrollbars_for_viewport(
              page_rect_points,
              viewport_css_for_mapping,
              &tab.scroll_state,
              metrics.bounds_css,
            );

            let painter = ui.painter();
            let to_egui_rect = |rect: fastrender::Rect| {
              egui::Rect::from_min_max(
                egui::pos2(rect.min_x(), rect.min_y()),
                egui::pos2(rect.max_x(), rect.max_y()),
              )
            };

            let dark = ui.visuals().dark_mode;
            let (r, g, b) = if dark { (255, 255, 255) } else { (0, 0, 0) };

            let draw_scrollbar =
              |scrollbar: fastrender::ui::scrollbars::OverlayScrollbar, dragging: bool| {
                let thickness_points = match scrollbar.axis {
                  fastrender::ui::scrollbars::ScrollbarAxis::Vertical => scrollbar.track_rect_points.width(),
                  fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => scrollbar.track_rect_points.height(),
                };
                let rounding = egui::Rounding::same((thickness_points * 0.5).max(0.0));
                let track = to_egui_rect(scrollbar.track_rect_points);
                let thumb = to_egui_rect(scrollbar.thumb_rect_points);

                let track_color = egui::Color32::from_rgba_unmultiplied(r, g, b, 32);
                let thumb_alpha = if dragging { 196 } else { 128 };
                let thumb_color = egui::Color32::from_rgba_unmultiplied(r, g, b, thumb_alpha);

                painter.rect_filled(track, rounding, track_color);
                painter.rect_filled(thumb, rounding, thumb_color);
              };

            if let Some(v) = self.overlay_scrollbars.vertical {
              let dragging = self
                .scrollbar_drag
                .as_ref()
                .is_some_and(|d| d.axis == v.axis && d.tab_id == active_tab);
              draw_scrollbar(v, dragging);
            }
            if let Some(h) = self.overlay_scrollbars.horizontal {
              let dragging = self
                .scrollbar_drag
                .as_ref()
                .is_some_and(|d| d.axis == h.axis && d.tab_id == active_tab);
              draw_scrollbar(h, dragging);
            }
          }
        }

        if !wheel_events.is_empty() && !wheel_blocked_by_dropdown && response.hovered() {
          let Some(hover_pos) = response.hover_pos() else {
            return;
          };

          let mut delta_css = (0.0, 0.0);
          for (unit, delta) in &wheel_events {
            let Some((dx, dy)) = mapping
              .wheel_delta_to_delta_css(fastrender::ui::WheelDelta::from_egui(*unit, *delta))
            else {
              continue;
            };
            delta_css.0 += dx;
            delta_css.1 += dy;
          }
          if delta_css.0 != 0.0 || delta_css.1 != 0.0 {
            // Treat wheel scrolling over overlay scrollbars as viewport scrolling (like browsers):
            // do not route the scroll delta to underlying element scrollers via hit-testing.
            let pointer_css = if self.cursor_over_overlay_scrollbars(hover_pos) {
              None
            } else {
              mapping.pos_points_to_pos_css_clamped(hover_pos)
            };
            self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
              tab_id: active_tab,
              delta_css,
              pointer_css,
            });
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

    self.render_hover_status(&ctx);
    self.render_select_dropdown(&ctx);
    self.render_context_menu(&ctx);
    self.sync_hover_after_tab_change(&ctx);
    // Coalesce pointer-move bursts to at most one message per rendered frame.
    self.flush_pending_pointer_move();

    let mut full_output = self.egui_ctx.end_frame();
    if let Some(text) = self.pending_clipboard_text.take() {
      full_output.platform_output.copied_text = text;
    }
    self.egui_state.handle_platform_output(
      &self.window,
      &self.egui_ctx,
      full_output.platform_output,
    );
    // Egui sets cursor icons as part of platform output. Override it for page content hover
    // semantics (links, text inputs) when the cursor is inside the rendered page image.
    self.apply_page_cursor_icon();

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
fn map_winit_key_to_shortcuts_key(
  key: winit::event::VirtualKeyCode,
) -> Option<fastrender::ui::shortcuts::Key> {
  use fastrender::ui::shortcuts::Key as ShortcutKey;
  use winit::event::VirtualKeyCode;

  Some(match key {
    VirtualKeyCode::A => ShortcutKey::A,
    VirtualKeyCode::C => ShortcutKey::C,
    VirtualKeyCode::K => ShortcutKey::K,
    VirtualKeyCode::L => ShortcutKey::L,
    VirtualKeyCode::R => ShortcutKey::R,
    VirtualKeyCode::T => ShortcutKey::T,
    VirtualKeyCode::V => ShortcutKey::V,
    VirtualKeyCode::W => ShortcutKey::W,
    VirtualKeyCode::X => ShortcutKey::X,
    VirtualKeyCode::Insert => ShortcutKey::Insert,
    VirtualKeyCode::Delete => ShortcutKey::Delete,
    VirtualKeyCode::Tab => ShortcutKey::Tab,
    VirtualKeyCode::Left => ShortcutKey::Left,
    VirtualKeyCode::Right => ShortcutKey::Right,
    VirtualKeyCode::F4 => ShortcutKey::F4,
    VirtualKeyCode::F5 => ShortcutKey::F5,
    VirtualKeyCode::Key0 | VirtualKeyCode::Numpad0 => ShortcutKey::Num0,
    VirtualKeyCode::Key1 | VirtualKeyCode::Numpad1 => ShortcutKey::Num1,
    VirtualKeyCode::Key2 | VirtualKeyCode::Numpad2 => ShortcutKey::Num2,
    VirtualKeyCode::Key3 | VirtualKeyCode::Numpad3 => ShortcutKey::Num3,
    VirtualKeyCode::Key4 | VirtualKeyCode::Numpad4 => ShortcutKey::Num4,
    VirtualKeyCode::Key5 | VirtualKeyCode::Numpad5 => ShortcutKey::Num5,
    VirtualKeyCode::Key6 | VirtualKeyCode::Numpad6 => ShortcutKey::Num6,
    VirtualKeyCode::Key7 | VirtualKeyCode::Numpad7 => ShortcutKey::Num7,
    VirtualKeyCode::Key8 | VirtualKeyCode::Numpad8 => ShortcutKey::Num8,
    VirtualKeyCode::Key9 | VirtualKeyCode::Numpad9 => ShortcutKey::Num9,
    VirtualKeyCode::Equals | VirtualKeyCode::NumpadEquals => ShortcutKey::Equals,
    VirtualKeyCode::Minus | VirtualKeyCode::NumpadSubtract => ShortcutKey::Minus,
    VirtualKeyCode::NumpadAdd => ShortcutKey::Plus,
    VirtualKeyCode::PageUp => ShortcutKey::PageUp,
    VirtualKeyCode::PageDown => ShortcutKey::PageDown,
    VirtualKeyCode::Space => ShortcutKey::Space,
    VirtualKeyCode::Home => ShortcutKey::Home,
    VirtualKeyCode::End => ShortcutKey::End,
    _ => return None,
  })
}

#[cfg(feature = "browser_ui")]
fn winit_modifiers_to_shortcuts_modifiers(
  modifiers: winit::event::ModifiersState,
) -> fastrender::ui::shortcuts::Modifiers {
  fastrender::ui::shortcuts::Modifiers {
    ctrl: modifiers.ctrl(),
    shift: modifiers.shift(),
    alt: modifiers.alt(),
    meta: modifiers.logo(),
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

#[cfg(feature = "browser_ui")]
fn map_modifiers(modifiers: winit::event::ModifiersState) -> fastrender::ui::PointerModifiers {
  use fastrender::ui::PointerModifiers;

  let mut out = PointerModifiers::NONE;
  if modifiers.ctrl() {
    out |= PointerModifiers::CTRL;
  }
  if modifiers.shift() {
    out |= PointerModifiers::SHIFT;
  }
  if modifiers.alt() {
    out |= PointerModifiers::ALT;
  }
  if modifiers.logo() {
    out |= PointerModifiers::META;
  }
  out
}
