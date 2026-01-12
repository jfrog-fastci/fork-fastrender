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

// The debug log UI (a developer-focused overlay) should be opt-in for normal browsing sessions.
// Keep the env parsing outside the `browser_ui` feature so unit tests can run without pulling in
// the full winit/wgpu/egui stack.
#[cfg(feature = "browser_ui")]
const ENV_BROWSER_DEBUG_LOG: &str = "FASTR_BROWSER_DEBUG_LOG";

#[cfg(any(test, feature = "browser_ui"))]
fn parse_env_bool(raw: Option<&str>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  match trimmed.to_ascii_lowercase().as_str() {
    "0" | "false" | "no" | "off" => false,
    _ => true,
  }
}

#[cfg(any(test, feature = "browser_ui"))]
fn should_show_debug_log_ui(debug_build: bool, env_value: Option<&str>) -> bool {
  debug_build || parse_env_bool(env_value)
}

#[cfg(feature = "browser_ui")]
fn main() {
  if let Err(err) = run() {
    eprintln!("browser exited with error: {err}");
    std::process::exit(1);
  }
}

const ENV_BROWSER_HUD: &str = "FASTR_BROWSER_HUD";

fn parse_browser_hud_env(raw: Option<&str>) -> Result<bool, String> {
  let Some(raw) = raw else {
    return Ok(false);
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(false);
  }

  if raw == "1"
    || raw.eq_ignore_ascii_case("true")
    || raw.eq_ignore_ascii_case("yes")
    || raw.eq_ignore_ascii_case("on")
  {
    return Ok(true);
  }

  if raw == "0"
    || raw.eq_ignore_ascii_case("false")
    || raw.eq_ignore_ascii_case("no")
    || raw.eq_ignore_ascii_case("off")
  {
    return Ok(false);
  }

  Err(format!(
    "{ENV_BROWSER_HUD}: invalid value {raw:?}; expected 0|1|true|false"
  ))
}

fn browser_hud_enabled_from_env() -> bool {
  let raw = match std::env::var(ENV_BROWSER_HUD) {
    Ok(raw) => raw,
    Err(_) => return false,
  };

  match parse_browser_hud_env(Some(&raw)) {
    Ok(enabled) => enabled,
    Err(err) => {
      eprintln!("{err}");
      false
    }
  }
}

#[cfg(test)]
mod browser_hud_env_tests {
  use super::*;

  #[test]
  fn parse_browser_hud_env_values() {
    assert_eq!(parse_browser_hud_env(None), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("")), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("   ")), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("0")), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("1")), Ok(true));
    assert_eq!(parse_browser_hud_env(Some("true")), Ok(true));
    assert_eq!(parse_browser_hud_env(Some("TrUe")), Ok(true));
    assert_eq!(parse_browser_hud_env(Some("yes")), Ok(true));
    assert_eq!(parse_browser_hud_env(Some("on")), Ok(true));
    assert_eq!(parse_browser_hud_env(Some("false")), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("no")), Ok(false));
    assert_eq!(parse_browser_hud_env(Some("off")), Ok(false));
    assert!(parse_browser_hud_env(Some("maybe")).is_err());
  }
}
#[cfg(feature = "browser_ui")]
use arboard::Clipboard;
#[cfg(feature = "browser_ui")]
use clap::Parser;

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
  after_help = "If URL is omitted, the browser attempts to restore the previous session; if none exists it opens `about:newtab`.\nUse `--restore` to try restoring even when a URL is provided, or `--no-restore` to disable session restore.\nThe URL value is resolved like the address bar: URL-like inputs are normalized (e.g. `example.com` → https), otherwise treated as a DuckDuckGo search (e.g. `cats` → https://duckduckgo.com/?q=cats).\nSupported schemes: http, https, file, about."
)]
struct BrowserCliArgs {
  /// Start URL (omit to restore previous session when available; otherwise `about:newtab`)
  #[arg(value_name = "URL")]
  url: Option<String>,

  /// Try to restore the previous session (even when a URL is provided)
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

#[cfg(any(test, feature = "browser_ui"))]
const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/app_icon/fastrender.png");

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone)]
struct DecodedRgbaIcon {
  width: u32,
  height: u32,
  rgba: Vec<u8>,
}

#[cfg(any(test, feature = "browser_ui"))]
fn decode_rgba_icon(png_bytes: &[u8]) -> Result<DecodedRgbaIcon, String> {
  let image = image::load_from_memory(png_bytes).map_err(|err| format!("image decode error: {err}"))?;
  let rgba = image.to_rgba8();
  let (width, height) = rgba.dimensions();
  let rgba = rgba.into_raw();
  let expected_len = (width as usize)
    .checked_mul(height as usize)
    .and_then(|v| v.checked_mul(4))
    .ok_or_else(|| "icon dimensions overflowed".to_string())?;
  if rgba.len() != expected_len {
    return Err(format!(
      "unexpected RGBA length: got {}, expected {} for {}x{}",
      rgba.len(),
      expected_len,
      width,
      height
    ));
  }
  Ok(DecodedRgbaIcon {
    width,
    height,
    rgba,
  })
}

#[cfg(feature = "browser_ui")]
fn load_window_icon() -> Option<winit::window::Icon> {
  let decoded = match decode_rgba_icon(APP_ICON_PNG) {
    Ok(icon) => icon,
    Err(err) => {
      eprintln!("failed to decode app icon: {err}");
      return None;
    }
  };

  match winit::window::Icon::from_rgba(decoded.rgba, decoded.width, decoded.height) {
    Ok(icon) => Some(icon),
    Err(err) => {
      eprintln!("failed to create winit window icon: {err:?}");
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn app_icon_decodes_into_rgba() {
    let icon = decode_rgba_icon(APP_ICON_PNG).expect("app icon should decode");
    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(icon.rgba.len(), (256 * 256 * 4) as usize);
  }
}

#[cfg(test)]
mod viewport_throttle_integration_tests {
  use std::time::{Duration, Instant};

  #[test]
  fn viewport_throttle_emits_leading_and_trailing_updates() {
    let cfg = fastrender::ui::ViewportThrottleConfig {
      max_hz: 60,
      debounce: Duration::from_millis(50),
    };
    let mut throttle = fastrender::ui::ViewportThrottle::with_config(cfg);

    let t0 = Instant::now();
    let first = throttle
      .push_desired(t0, (100, 80), 2.0)
      .expect("leading update should emit immediately");
    assert_eq!(first.viewport_css, (100, 80));
    assert!((first.dpr() - 2.0).abs() < f32::EPSILON);

    assert_eq!(throttle.push_desired(t0 + Duration::from_millis(5), (120, 90), 2.0), None);

    assert_eq!(throttle.poll(t0 + Duration::from_millis(54)), None);
    let second = throttle
      .poll(t0 + Duration::from_millis(55))
      .expect("trailing update should flush after debounce");
    assert_eq!(second.viewport_css, (120, 90));
    assert!((second.dpr() - 2.0).abs() < f32::EPSILON);
  }
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
        eprintln!(
          "failed to load session from {}: {err}",
          session_path.display()
        );
      }
    }
  }

  if let Some(url) = cli_url {
    return (
      fastrender::ui::BrowserSession::single(url),
      StartupSessionSource::CliUrl,
    );
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
    match fastrender::ui::resolve_omnibox_input(raw_url).and_then(|resolved| {
      let url = resolved.url();
      fastrender::ui::validate_user_navigation_url_scheme(url)?;
      Ok(url.to_string())
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
  let session_lock = match fastrender::ui::session::acquire_session_lock(&session_path) {
    Ok(lock) => lock,
    Err(fastrender::ui::session::SessionLockError::AlreadyLocked { lock_path }) => {
      return Err(
        format!(
          "refusing to start: session file {} is already in use by another `browser` process (lock file: {})",
          session_path.display(),
          lock_path.display()
        )
        .into(),
      );
    }
    Err(fastrender::ui::session::SessionLockError::Io { lock_path, error }) => {
      return Err(
        format!(
          "failed to acquire session lock file {} (session {}): {error}",
          lock_path.display(),
          session_path.display(),
        )
        .into(),
      );
    }
  };

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
        let session = fastrender::ui::session::parse_session_json(&raw)
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
  let bookmarks_path = fastrender::ui::bookmarks_path();
  let history_path = fastrender::ui::history_path();
  let bookmarks = match fastrender::ui::load_bookmarks(&bookmarks_path) {
    Ok(Some(store)) => store,
    Ok(None) => fastrender::ui::BookmarkStore::default(),
    Err(err) => {
      eprintln!(
        "failed to load bookmarks from {}: {err}",
        bookmarks_path.display()
      );
      fastrender::ui::BookmarkStore::default()
    }
  };
  let history = match fastrender::ui::load_history(&history_path) {
    Ok(Some(store)) => store,
    Ok(None) => fastrender::ui::GlobalHistoryStore::default(),
    Err(err) => {
      eprintln!(
        "failed to load history from {}: {err}",
        history_path.display()
      );
      fastrender::ui::GlobalHistoryStore::default()
    }
  };

  use winit::dpi::LogicalSize;
  use winit::event::Event;
  use winit::event::StartCause;
  use winit::event::WindowEvent;
  use winit::event_loop::ControlFlow;
  use winit::event_loop::EventLoopBuilder;
  use winit::window::Theme;
  use winit::window::WindowBuilder;

  let theme_override = fastrender::ui::theme::theme_mode_override_from_env();
  let theme_accent = fastrender::ui::theme::accent_color_override_from_env();
  let window_theme_override = match theme_override {
    Some(fastrender::ui::theme::ThemeMode::Light) => Some(Theme::Light),
    Some(fastrender::ui::theme::ThemeMode::Dark) => Some(Theme::Dark),
    _ => None,
  };

  let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
  let event_loop_proxy = event_loop.create_proxy();
  let window_icon = load_window_icon();
  let window_builder = WindowBuilder::new()
    .with_title("FastRender")
    .with_inner_size(LogicalSize::new(1200.0, 800.0))
    .with_min_inner_size(LogicalSize::new(480.0, 320.0))
    .with_window_icon(window_icon)
    // Match native window chrome to the browser theme override when one is set; otherwise follow
    // the system theme.
    .with_theme(window_theme_override);

  // Platform-native titlebar integration.
  //
  // On macOS, render the chrome into the titlebar area (unified toolbar look).
  #[cfg(target_os = "macos")]
  let window_builder = {
    use winit::platform::macos::WindowBuilderExtMacOS;

    window_builder
      .with_title_hidden(true)
      .with_titlebar_transparent(true)
      .with_fullsize_content_view(true)
  };

  let window = window_builder.build(&event_loop)?;

  #[cfg(target_os = "macos")]
  {
    use winit::platform::macos::WindowExtMacOS;

    // Ensure the titlebar settings are applied on the native window as well.
    //
    // NOTE: These are best-effort; on older macOS versions some settings may be ignored.
    window.set_title_hidden(true);
    window.set_titlebar_transparent(true);
    window.set_fullsize_content_view(true);
  }

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
  let gpu = pollster::block_on(GpuContext::new(&window, wgpu_init))?;
  let mut app = App::new(
    window,
    &event_loop,
    ui_to_worker_tx,
    worker_join,
    &gpu,
    theme_override,
    theme_accent,
    bookmarks_path,
    history_path,
    bookmarks,
    history,
  )?;
  app.startup(startup_session);
  app.profile_autosave = Some(fastrender::ui::ProfileAutosaveHandle::spawn(
    app.bookmarks_path.clone(),
    app.history_path.clone(),
  ));

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
    // Keep the session lock alive for the duration of the winit event loop.
    let _ = &session_lock;
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
          if let Err(err) = fastrender::ui::save_bookmarks_atomic(&app.bookmarks_path, &app.bookmarks)
          {
            eprintln!(
              "failed to save bookmarks to {}: {err}",
              app.bookmarks_path.display()
            );
          }
          if let Err(err) = fastrender::ui::save_history_atomic(&app.history_path, &app.history) {
            eprintln!(
              "failed to save history to {}: {err}",
              app.history_path.display()
            );
          }

          if let Some(autosave) = app.profile_autosave.take() {
            autosave.shutdown_with_timeout(std::time::Duration::from_millis(500));
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
          WindowEvent::ThemeChanged(theme) => {
            if app.refresh_theme_from_system_theme(Some(theme)) {
              app.window.request_redraw();
            }
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
      Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
        // Used for UI animations that need to progress even when there is no input/worker activity.
        // Today this is used for overlay scrollbar fade-in/out.
        app.maybe_request_redraw_for_ui_timers();
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
    // pending deadline (worker ticks, viewport throttling, egui repaint scheduling).
    app.drive_periodic_tasks_and_update_control_flow(control_flow);
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
    return Err(
      fastrender::Error::Other(format!(
        "expected vmjs event loop to reach idle, got {outcome:?}"
      ))
      .into(),
    );
  }

  let dom: &fastrender::dom2::Document = tab.dom();
  let body = dom
    .body()
    .ok_or_else(|| fastrender::Error::Other("expected document.body to exist".to_string()))?;
  let value = dom
    .get_attribute(body, "data-ok")
    .map_err(|err| fastrender::Error::Other(format!("failed to read body[data-ok]: {err}")))?;
  if value != Some("1") {
    return Err(
      fastrender::Error::Other(format!("expected body[data-ok]=\"1\", got {value:?}")).into(),
    );
  }

  let pixmap = tab.render_frame()?;
  let pixmap_px = (pixmap.width(), pixmap.height());
  if pixmap_px != (expected_pixmap_w, expected_pixmap_h) {
    return Err(
      fastrender::Error::Other(format!(
        "unexpected pixmap size: got {}x{}, expected {}x{}",
        pixmap_px.0, pixmap_px.1, expected_pixmap_w, expected_pixmap_h
      ))
      .into(),
    );
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

  let bookmarks_path = fastrender::ui::bookmarks_persistence::bookmarks_path();
  let history_path = fastrender::ui::history_persistence::history_path();

  const BOOKMARKS_OVERRIDE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE_BOOKMARKS_JSON";
  let (bookmarks_source, bookmarks_snapshot) = match std::env::var(BOOKMARKS_OVERRIDE_ENV) {
    Ok(raw) if !raw.trim().is_empty() => {
      let snapshot: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|err| format!("{BOOKMARKS_OVERRIDE_ENV}: invalid JSON: {err}"))?;
      ("override", snapshot)
    }
    _ => match fastrender::ui::bookmarks_persistence::load_bookmarks(&bookmarks_path) {
      Ok(Some(snapshot)) => ("disk", snapshot),
      Ok(None) => ("empty", serde_json::Value::Array(Vec::new())),
      Err(err) => {
        eprintln!(
          "failed to load bookmarks from {}: {err}",
          bookmarks_path.display()
        );
        ("empty", serde_json::Value::Array(Vec::new()))
      }
    },
  };
  let bookmarks_json =
    serde_json::to_string(&bookmarks_snapshot).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_BOOKMARKS source={bookmarks_source} {bookmarks_json}");

  const HISTORY_OVERRIDE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE_HISTORY_JSON";
  let (history_source, history_snapshot) = match std::env::var(HISTORY_OVERRIDE_ENV) {
    Ok(raw) if !raw.trim().is_empty() => {
      let snapshot: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|err| format!("{HISTORY_OVERRIDE_ENV}: invalid JSON: {err}"))?;
      ("override", snapshot)
    }
    _ => match fastrender::ui::history_persistence::load_history(&history_path) {
      Ok(Some(snapshot)) => ("disk", snapshot),
      Ok(None) => ("empty", serde_json::Value::Array(Vec::new())),
      Err(err) => {
        eprintln!(
          "failed to load history from {}: {err}",
          history_path.display()
        );
        ("empty", serde_json::Value::Array(Vec::new()))
      }
    },
  };
  let history_json =
    serde_json::to_string(&history_snapshot).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_HISTORY source={history_source} {history_json}");

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

  let window = &session.windows[session.active_window_index];
  let mut tab_ids = Vec::with_capacity(window.tabs.len());
  for tab in &window.tabs {
    let tab_id = TabId::new();
    tab_ids.push(tab_id);
    ui_to_worker_tx.send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(tab.url.clone()),
      cancel: CancelGens::new(),
    })?;
  }

  let active_idx = window
    .active_tab_index
    .min(tab_ids.len().saturating_sub(1));
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
    eprintln!(
      "failed to save session to {}: {err}",
      session_path.display()
    );
  }

  if let Err(err) = fastrender::ui::bookmarks_persistence::save_bookmarks_atomic(
    &bookmarks_path,
    &bookmarks_snapshot,
  ) {
    eprintln!(
      "failed to save bookmarks to {}: {err}",
      bookmarks_path.display()
    );
  }

  if let Err(err) =
    fastrender::ui::history_persistence::save_history_atomic(&history_path, &history_snapshot)
  {
    eprintln!(
      "failed to save history to {}: {err}",
      history_path.display()
    );
  }

  let active_url = window
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
fn debug_log_ui_enabled() -> bool {
  should_show_debug_log_ui(
    cfg!(debug_assertions),
    std::env::var(ENV_BROWSER_DEBUG_LOG).ok().as_deref(),
  )
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
  /// When true, the next dropdown render should scroll the currently-selected option into view.
  scroll_to_selected: bool,
  /// Accumulated typeahead query (lowercased) for keyboard selection.
  typeahead_query: String,
  typeahead_last_input: Option<std::time::Instant>,
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
  selected_idx: usize,
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
#[derive(Debug)]
struct BrowserHud {
  last_frame_start: Option<std::time::Instant>,
  last_frame_cpu_ms: Option<f32>,
  fps: Option<f32>,
  text_buf: String,
}

#[cfg(feature = "browser_ui")]
impl BrowserHud {
  fn new() -> Self {
    Self {
      last_frame_start: None,
      last_frame_cpu_ms: None,
      fps: None,
      text_buf: String::with_capacity(256),
    }
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageTextureFilterPolicy {
  /// Use nearest-neighbour filtering when the page image is drawn at ~1:1, otherwise use linear.
  Auto,
  /// Always use nearest-neighbour filtering for page textures.
  Nearest,
  /// Always use linear filtering for page textures.
  Linear,
}

#[cfg(feature = "browser_ui")]
impl PageTextureFilterPolicy {
  const ENV_KEY: &'static str = "FASTR_BROWSER_PAGE_FILTER";

  fn from_env() -> Self {
    let raw = std::env::var(Self::ENV_KEY).ok();
    let Some(raw) = raw else {
      return Self::Auto;
    };
    let raw = raw.trim();
    if raw.is_empty() {
      return Self::Auto;
    }
    match raw.to_ascii_lowercase().as_str() {
      "auto" => Self::Auto,
      "nearest" => Self::Nearest,
      "linear" => Self::Linear,
      other => {
        eprintln!(
          "{}: invalid value {other:?} (expected nearest|linear|auto); defaulting to auto",
          Self::ENV_KEY
        );
        Self::Auto
      }
    }
  }
}

#[cfg(feature = "browser_ui")]
struct GpuContext {
  instance: wgpu::Instance,
  adapter: wgpu::Adapter,
  device: std::sync::Arc<wgpu::Device>,
  queue: std::sync::Arc<wgpu::Queue>,
}

#[cfg(feature = "browser_ui")]
impl GpuContext {
  async fn new(
    window: &winit::window::Window,
    wgpu_init: WgpuInitOptions,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
      backends: wgpu_init.backends,
      ..Default::default()
    });
    let surface = unsafe { instance.create_surface(window) }?;

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

    Ok(Self {
      instance,
      adapter,
      device: std::sync::Arc::new(device),
      queue: std::sync::Arc::new(queue),
    })
  }
}

#[cfg(feature = "browser_ui")]
struct App {
  window: winit::window::Window,
  window_title_cache: String,

  surface: wgpu::Surface,
  device: std::sync::Arc<wgpu::Device>,
  queue: std::sync::Arc<wgpu::Queue>,
  surface_config: wgpu::SurfaceConfiguration,

  egui_ctx: egui::Context,
  egui_state: egui_winit::State,
  egui_renderer: egui_wgpu::Renderer,
  pixels_per_point: f32,
  browser_limits: fastrender::ui::browser_limits::BrowserLimits,
  page_texture_filter_policy: PageTextureFilterPolicy,
  theme_override: Option<fastrender::ui::theme::ThemeMode>,
  theme_accent: Option<egui::Color32>,
  theme: fastrender::ui::theme::BrowserTheme,
  clear_color: wgpu::Color,

  ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
  worker_join: Option<std::thread::JoinHandle<()>>,
  browser_state: fastrender::ui::BrowserAppState,

  bookmarks_path: std::path::PathBuf,
  history_path: std::path::PathBuf,
  bookmarks: fastrender::ui::BookmarkStore,
  history: fastrender::ui::GlobalHistoryStore,
  profile_autosave: Option<fastrender::ui::ProfileAutosaveHandle>,
  history_panel_open: bool,
  bookmarks_panel_open: bool,

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
  /// Whether the page-area loading overlay is currently blocking pointer events from reaching the
  /// page worker.
  ///
  /// We block pointer interactions while a tab is loading a navigation but still showing the last
  /// rendered frame to avoid interacting with stale content.
  page_loading_overlay_blocks_input: bool,
  overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars,
  overlay_scrollbar_visibility: fastrender::ui::scrollbars::OverlayScrollbarVisibilityState,
  scrollbar_drag: Option<ScrollbarDrag>,
  viewport_cache_tab: Option<fastrender::ui::TabId>,
  viewport_cache_css: (u32, u32),
  viewport_cache_dpr: f32,
  viewport_throttle: fastrender::ui::ViewportThrottle,
  viewport_throttle_tab: Option<fastrender::ui::TabId>,
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
  debug_log_ui_enabled: bool,
  debug_log_ui_open: bool,
  debug_log_filter: String,
  hud: Option<BrowserHud>,

  /// Deadline for the next egui-driven repaint (derived from `egui::FullOutput::repaint_after`).
  ///
  /// Stored as an `Instant` so the winit event loop can sleep in `ControlFlow::WaitUntil` even when
  /// no OS events are arriving (e.g. focus changes, spinners).
  next_egui_repaint: Option<std::time::Instant>,
  /// Last time we requested a redraw due to egui's repaint scheduling.
  ///
  /// Used to rate-limit "immediate" (`Duration::ZERO`) repaint requests so we don't busy loop.
  last_egui_redraw_request: Option<std::time::Instant>,

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
  const SELECT_DROPDOWN_EDGE_PADDING_POINTS: f32 = 8.0;
  const SELECT_DROPDOWN_MIN_WIDTH_POINTS: f32 = 180.0;
  const SELECT_DROPDOWN_MAX_WIDTH_POINTS: f32 = 600.0;
  const SELECT_DROPDOWN_MAX_HEIGHT_POINTS: f32 = 320.0;
  const SELECT_DROPDOWN_PAGE_STEP: isize = 10;
  const SELECT_DROPDOWN_TYPEAHEAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1000);

  fn refresh_theme_from_system_theme(&mut self, system_theme: Option<winit::window::Theme>) -> bool {
    use fastrender::ui::theme::ThemeMode;

    let resolved_mode = match self.theme_override.unwrap_or(ThemeMode::System) {
      ThemeMode::Light => ThemeMode::Light,
      ThemeMode::Dark => ThemeMode::Dark,
      ThemeMode::System => match system_theme {
        Some(winit::window::Theme::Dark) => ThemeMode::Dark,
        Some(winit::window::Theme::Light) => ThemeMode::Light,
        None => ThemeMode::Light,
      },
    };

    if resolved_mode == self.theme.mode {
      return false;
    }

    self.theme = match resolved_mode {
      fastrender::ui::theme::ThemeMode::Dark => {
        fastrender::ui::theme::BrowserTheme::dark(self.theme_accent)
      }
      _ => fastrender::ui::theme::BrowserTheme::light(self.theme_accent),
    };
    fastrender::ui::theme::apply_browser_theme(&self.egui_ctx, &self.theme);

    let bg = self.theme.colors.bg;
    self.clear_color = wgpu::Color {
      r: bg.r() as f64 / 255.0,
      g: bg.g() as f64 / 255.0,
      b: bg.b() as f64 / 255.0,
      a: bg.a() as f64 / 255.0,
    };

    true
  }

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

  fn cursor_near_overlay_scrollbars(&self, pos_points: egui::Pos2) -> bool {
    const HOVER_INFLATE_POINTS: f32 = 10.0;
    let pos = fastrender::Point::new(pos_points.x, pos_points.y);
    self
      .overlay_scrollbars
      .vertical
      .is_some_and(|sb| sb.track_rect_points.inflate(HOVER_INFLATE_POINTS).contains_point(pos))
      || self
        .overlay_scrollbars
        .horizontal
        .is_some_and(|sb| sb.track_rect_points.inflate(HOVER_INFLATE_POINTS).contains_point(pos))
  }

  fn overlay_scrollbars_force_visible(&self) -> bool {
    let active_tab = self.browser_state.active_tab_id();
    let dragging = self
      .scrollbar_drag
      .as_ref()
      .is_some_and(|drag| Some(drag.tab_id) == active_tab);
    let hovering = self
      .last_cursor_pos_points
      .is_some_and(|pos| self.cursor_near_overlay_scrollbars(pos));
    dragging || hovering
  }

  fn new<T: 'static>(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<T>,
    ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
    worker_join: std::thread::JoinHandle<()>,
    gpu: &GpuContext,
    theme_override: Option<fastrender::ui::theme::ThemeMode>,
    theme_accent: Option<egui::Color32>,
    bookmarks_path: std::path::PathBuf,
    history_path: std::path::PathBuf,
    bookmarks: fastrender::ui::BookmarkStore,
    history: fastrender::ui::GlobalHistoryStore,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    // Enable OS IME integration (WindowEvent::Ime) so the page can handle non-Latin input methods.
    // Egui manages IME for chrome text fields; we forward IME events to the page when appropriate.
    window.set_ime_allowed(true);

    let pixels_per_point = window.scale_factor() as f32;

    let egui_ctx = egui::Context::default();
    egui_ctx.set_pixels_per_point(pixels_per_point);
    let egui_state = egui_winit::State::new(event_loop);

    let theme_mode = fastrender::ui::theme::resolve_theme_mode(&window, theme_override);
    let theme = match theme_mode {
      fastrender::ui::theme::ThemeMode::Dark => {
        fastrender::ui::theme::BrowserTheme::dark(theme_accent)
      }
      _ => fastrender::ui::theme::BrowserTheme::light(theme_accent),
    };
    fastrender::ui::theme::apply_browser_theme(&egui_ctx, &theme);
    let clear_color = {
      let bg = theme.colors.bg;
      wgpu::Color {
        r: bg.r() as f64 / 255.0,
        g: bg.g() as f64 / 255.0,
        b: bg.b() as f64 / 255.0,
        a: bg.a() as f64 / 255.0,
      }
    };

    let surface = unsafe { gpu.instance.create_surface(&window) }?;
    let device = gpu.device.clone();
    let queue = gpu.queue.clone();

    let surface_caps = surface.get_capabilities(&gpu.adapter);
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
    let debug_log_ui_enabled = debug_log_ui_enabled();

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
      page_texture_filter_policy: PageTextureFilterPolicy::from_env(),
      theme_override,
      theme_accent,
      theme,
      clear_color,
      ui_to_worker_tx,
      worker_join: Some(worker_join),
      browser_state: fastrender::ui::BrowserAppState::new(),
      bookmarks_path,
      history_path,
      bookmarks,
      history,
      profile_autosave: None,
      history_panel_open: false,
      bookmarks_panel_open: false,
      tab_textures: std::collections::HashMap::new(),
      tab_favicons: std::collections::HashMap::new(),
      tab_cancel: std::collections::HashMap::new(),
      pending_frame_uploads: fastrender::ui::FrameUploadCoalescer::new(),
      page_rect_points: None,
      page_viewport_css: None,
      page_input_tab: None,
      page_input_mapping: None,
      page_loading_overlay_blocks_input: false,
      overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars::default(),
      overlay_scrollbar_visibility: fastrender::ui::scrollbars::OverlayScrollbarVisibilityState::default(),
      scrollbar_drag: None,
      viewport_cache_tab: None,
      viewport_cache_css: (0, 0),
      viewport_cache_dpr: 0.0,
      viewport_throttle: fastrender::ui::ViewportThrottle::new(),
      viewport_throttle_tab: None,
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
      debug_log_ui_enabled,
      debug_log_ui_open: debug_log_ui_enabled,
      debug_log_filter: String::new(),
      hud: if browser_hud_enabled_from_env() {
        Some(BrowserHud::new())
      } else {
        None
      },
      next_egui_repaint: None,
      last_egui_redraw_request: None,
      animation_tick_tab: None,
      next_animation_tick: None,
    })
  }

  fn startup(&mut self, session: fastrender::ui::BrowserSession) {
    use fastrender::ui::UiToWorker;

    let session = session.sanitized();
    let window = session
      .windows
      .into_iter()
      .nth(session.active_window_index)
      .unwrap_or_else(|| fastrender::ui::BrowserSessionWindow {
        tabs: vec![fastrender::ui::BrowserSessionTab {
          url: fastrender::ui::about_pages::ABOUT_NEWTAB.to_string(),
          zoom: None,
        }],
        active_tab_index: 0,
        window_state: None,
      });
    let mut tab_ids = Vec::with_capacity(window.tabs.len());

    for tab in window.tabs {
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

    let active_idx = window
      .active_tab_index
      .min(tab_ids.len().saturating_sub(1));
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

  fn drive_periodic_tasks_and_update_control_flow(
    &mut self,
    control_flow: &mut winit::event_loop::ControlFlow,
  ) {
    // In a multi-window event loop, this should be called for every live window/app after handling
    // the current event so that:
    // - animation ticks are driven even when a given window is otherwise idle, and
    // - the global `ControlFlow::WaitUntil` deadline accounts for the earliest wakeup across all windows.
    self.drive_animation_tick();
    self.drive_viewport_throttle();
    self.drive_egui_repaint();
    self.update_control_flow_for_animation_ticks(control_flow);
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

  fn schedule_egui_repaint(&mut self, repaint_after: std::time::Duration) {
    if self.window_occluded || self.window_minimized {
      self.next_egui_repaint = None;
      return;
    }

    let now = std::time::Instant::now();
    let plan = fastrender::ui::repaint_scheduler::plan_egui_repaint(
      now,
      repaint_after,
      self.last_egui_redraw_request,
    );

    if plan.request_redraw_now {
      self.next_egui_repaint = None;
      self.last_egui_redraw_request = Some(now);
      self.window.request_redraw();
    } else {
      self.next_egui_repaint = plan.next_deadline;
    }
  }

  fn drive_egui_repaint(&mut self) {
    if self.window_occluded || self.window_minimized {
      self.next_egui_repaint = None;
      return;
    }

    let Some(deadline) = self.next_egui_repaint else {
      return;
    };

    let now = std::time::Instant::now();
    if now >= deadline {
      self.next_egui_repaint = None;
      self.last_egui_redraw_request = Some(now);
      self.window.request_redraw();
    }
  }

  fn update_control_flow_for_animation_ticks(
    &mut self,
    control_flow: &mut winit::event_loop::ControlFlow,
  ) {
    let now = std::time::Instant::now();
    let Some(deadline) = self.next_wakeup_deadline(now) else {
      return;
    };

    // In a multi-window event loop, each window `App` can contribute its own desired wakeup
    // deadline. Avoid "last window wins" behaviour by merging with any existing wait deadline.
    *control_flow = match *control_flow {
      winit::event_loop::ControlFlow::Wait => winit::event_loop::ControlFlow::WaitUntil(deadline),
      winit::event_loop::ControlFlow::WaitUntil(existing) => {
        winit::event_loop::ControlFlow::WaitUntil(existing.min(deadline))
      }
      other => other,
    };
  }

  fn next_wakeup_deadline(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
    let mut deadline: Option<std::time::Instant> = None;

    // Worker-driven animation ticks (CSS animations/transitions).
    if let Some(tab_id) = self.desired_animation_tick_tab() {
      if self.animation_tick_tab != Some(tab_id) || self.next_animation_tick.is_none() {
        self.animation_tick_tab = Some(tab_id);
        self.next_animation_tick = Some(now + Self::ANIMATION_TICK_INTERVAL);
      }
      if let Some(tick_deadline) = self.next_animation_tick {
        deadline = Some(match deadline {
          Some(existing) => existing.min(tick_deadline),
          None => tick_deadline,
        });
      }
    } else {
      self.animation_tick_tab = None;
      self.next_animation_tick = None;
    }

    // Egui repaint scheduling (focus changes, animated widgets like spinners, etc).
    if let Some(egui_deadline) = self.next_egui_repaint {
      deadline = Some(match deadline {
        Some(existing) => existing.min(egui_deadline),
        None => egui_deadline,
      });
    }

    // UI-only timers (e.g. overlay scrollbar fade).
    let cfg = fastrender::ui::scrollbars::OverlayScrollbarVisibilityConfig::default();
    let force_visible = self.overlay_scrollbars_force_visible();
    if let Some(sb_deadline) = self
      .overlay_scrollbar_visibility
      .next_wakeup(now, cfg, force_visible)
    {
      deadline = Some(match deadline {
        Some(existing) => existing.min(sb_deadline),
        None => sb_deadline,
      });
    }

    // Viewport debounce wakeups (resize settling).
    if let Some(vp_deadline) = self.viewport_throttle.next_deadline() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(vp_deadline),
        None => vp_deadline,
      });
    }

    deadline
  }

  fn maybe_request_redraw_for_ui_timers(&mut self) {
    let cfg = fastrender::ui::scrollbars::OverlayScrollbarVisibilityConfig::default();
    let force_visible = self.overlay_scrollbars_force_visible();
    if self.overlay_scrollbar_visibility.needs_repaint(
      std::time::Instant::now(),
      cfg,
      force_visible,
    ) {
      self.window.request_redraw();
    }
  }

  fn send_worker_msg(&mut self, msg: fastrender::ui::UiToWorker) {
    use fastrender::ui::UiToWorker;

    // Keep overlay scrollbars visible when a scroll is initiated via any input path (wheel, track
    // click, thumb drag, keyboard shortcuts that synthesize `ScrollTo`, etc).
    if matches!(&msg, UiToWorker::Scroll { .. } | UiToWorker::ScrollTo { .. }) {
      self
        .overlay_scrollbar_visibility
        .register_interaction(std::time::Instant::now());
    }

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
      | UiToWorker::FindQuery { tab_id, .. }
      | UiToWorker::FindNext { tab_id }
      | UiToWorker::FindPrev { tab_id }
      | UiToWorker::FindStop { tab_id }
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
        | UiToWorker::FindQuery { .. }
        | UiToWorker::FindNext { .. }
        | UiToWorker::FindPrev { .. }
        | UiToWorker::FindStop { .. }
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
    if self.page_loading_overlay_blocks_input {
      // While the page-area loading overlay is active we intentionally suppress pointer-move
      // updates so the render worker doesn't update hover state/cursor based on stale content.
      return;
    }
    if let Some(pos) = self.last_cursor_pos_points {
      if self
        .open_select_dropdown_rect
        .is_some_and(|rect| rect.contains(pos))
        || self
          .open_context_menu_rect
          .is_some_and(|rect| rect.contains(pos))
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

    let popup_intercepts = self.last_cursor_pos_points.is_some_and(|pos| {
      self
        .open_select_dropdown_rect
        .is_some_and(|rect| rect.contains(pos))
        || self
          .open_context_menu_rect
          .is_some_and(|rect| rect.contains(pos))
    });

    if !self.cursor_in_page || popup_intercepts {
      self.page_cursor_override = None;
      return;
    }

    let kind = if self.page_loading_overlay_blocks_input {
      CursorKind::Default
    } else {
      self
        .browser_state
        .active_tab()
        .map(|tab| tab.cursor)
        .unwrap_or(CursorKind::Default)
    };
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
    let selected_item_idx = self.open_select_dropdown.as_ref().and_then(|dropdown| {
      fastrender::select_dropdown::next_enabled_option_item_index(&dropdown.control, key)
    });
    let Some(selected_item_idx) = selected_item_idx else {
      return;
    };
    self.update_open_select_dropdown_selection_for_item_index(selected_item_idx, true);
  }

  fn update_open_select_dropdown_selection_for_item_index(
    &mut self,
    selected_item_idx: usize,
    clear_typeahead: bool,
  ) {
    use fastrender::tree::box_tree::SelectItem;

    let Some(dropdown) = self.open_select_dropdown.as_mut() else {
      return;
    };

    // Update the local `SelectControl` snapshot so the popup highlights the same option that the
    // worker will select after handling the corresponding `UiToWorker::KeyAction`.
    //
    // This keeps the dropdown open while navigating with keyboard input, without requiring
    // additional worker→UI protocol messages.
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
    dropdown.scroll_to_selected = true;

    if clear_typeahead {
      dropdown.typeahead_query.clear();
      dropdown.typeahead_last_input = None;
    }
  }

  fn update_open_select_dropdown_selection_by_enabled_delta(&mut self, delta: isize) {
    let selected_item_idx = self.open_select_dropdown.as_ref().and_then(|dropdown| {
      fastrender::select_dropdown::offset_enabled_option_item_index(&dropdown.control, delta)
    });
    let Some(selected_item_idx) = selected_item_idx else {
      return;
    };
    self.update_open_select_dropdown_selection_for_item_index(selected_item_idx, true);
  }

  fn handle_select_dropdown_typeahead(&mut self, ch: char) {
    use fastrender::tree::box_tree::SelectItem;

    if ch.is_control() || !ch.is_alphanumeric() {
      return;
    }

    let matched_item_idx = {
      let Some(dropdown) = self.open_select_dropdown.as_mut() else {
        return;
      };

      let now = std::time::Instant::now();
      let timed_out = dropdown
        .typeahead_last_input
        .is_none_or(|last| now.duration_since(last) > Self::SELECT_DROPDOWN_TYPEAHEAD_TIMEOUT);
      if timed_out {
        dropdown.typeahead_query.clear();
      }
      dropdown.typeahead_last_input = Some(now);

      for lower in ch.to_lowercase() {
        dropdown.typeahead_query.push(lower);
      }

      let query = dropdown.typeahead_query.as_str();
      let total = dropdown.control.items.len();
      if total == 0 || query.is_empty() {
        return;
      }

      let start_item_idx = dropdown
        .control
        .selected
        .last()
        .copied()
        .unwrap_or_else(|| total.saturating_sub(1));

      let mut matched_item_idx = None;
      for offset in 1..=total {
        let idx = (start_item_idx + offset) % total;
        let SelectItem::Option {
          label,
          value,
          disabled,
          ..
        } = &dropdown.control.items[idx]
        else {
          continue;
        };
        if *disabled {
          continue;
        }
        let base = if label.trim().is_empty() {
          value
        } else {
          label
        };
        if base.trim_start().to_lowercase().starts_with(query) {
          matched_item_idx = Some(idx);
          break;
        }
      }

      matched_item_idx
    };

    if let Some(item_idx) = matched_item_idx {
      self.update_open_select_dropdown_selection_for_item_index(item_idx, false);
    }
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
    // Keep the `about:newtab` snapshot updated with global history so opening a new tab surfaces
    // recently visited pages even when the primary UI lives in egui panels.
    if let fastrender::ui::WorkerToUi::NavigationCommitted { url, title, .. } = &msg {
      if !fastrender::ui::about_pages::is_about_url(url) {
        fastrender::ui::about_pages::record_global_history_visit(url, title.as_deref());
      }
    }

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
      // Opening a link in a new tab should behave like a regular tab activation: keyboard input
      // should target the page unless the chrome has explicit focus.
      self.page_has_focus = true;
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

    if let fastrender::ui::WorkerToUi::NavigationCommitted { url, title, .. } = &msg {
      self.history.record(url.clone(), title.clone());
      if let Some(autosave) = self.profile_autosave.as_ref() {
        let _ = autosave.send(fastrender::ui::AutosaveMsg::UpdateHistory(self.history.clone()));
      }
    }

    let mut request_redraw = false;

    if let fastrender::ui::WorkerToUi::DebugLog { tab_id, line } = &msg {
      eprintln!("[worker:{tab_id:?}] {line}");
      if self.debug_log_ui_enabled {
        let line = line.trim_end();
        if !line.is_empty() {
          if self.debug_log.len() >= Self::DEBUG_LOG_MAX_LINES {
            self.debug_log.pop_front();
          }
          self
            .debug_log
            .push_back(format!("[tab {}] {}", tab_id.0, line));
          request_redraw = true;
        }
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
          if let Some(pending) = self.pending_context_menu_request.take() {
            self.open_context_menu = Some(OpenContextMenu {
              tab_id: *tab_id,
              pos_css: *pos_css,
              anchor_points: pending.anchor_points,
              link_url: link_url.clone(),
              selected_idx: 0,
            });
            self.open_context_menu_rect = None;
            request_redraw = true;
          }
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
            scroll_to_selected: true,
            typeahead_query: String::new(),
            typeahead_last_input: None,
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

  fn send_viewport_changed_clamped_if_needed(
    &mut self,
    tab_id: fastrender::ui::TabId,
    viewport_css: (u32, u32),
    dpr: f32,
  ) {
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

  fn update_viewport_throttled(&mut self, viewport_css: (u32, u32), dpr: f32) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      self.viewport_throttle_tab = None;
      self.viewport_throttle.reset();
      return;
    };

    // Keep the throttle state scoped to the active tab so tab switches don't inherit the previous
    // tab's rate-limit window.
    if self.viewport_throttle_tab != Some(tab_id) {
      self.viewport_throttle_tab = Some(tab_id);
      self.viewport_throttle.reset();
    }

    // Clamp *before* sending to the worker so we never request an absurd RGBA pixmap allocation.
    let clamp = self.browser_limits.clamp_viewport_and_dpr(viewport_css, dpr);
    let viewport_css = clamp.viewport_css;
    let dpr = clamp.dpr;

    if let Some(tab) = self.browser_state.tab_mut(tab_id) {
      tab.warning = clamp.warning_text(&self.browser_limits);
    }

    let now = std::time::Instant::now();
    if let Some(update) = self.viewport_throttle.push_desired(now, viewport_css, dpr) {
      self.send_viewport_changed_clamped_if_needed(tab_id, update.viewport_css, update.dpr());
    }
  }

  fn drive_viewport_throttle(&mut self) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      return;
    };

    if self.viewport_throttle_tab != Some(tab_id) {
      // The active tab changed without giving us a chance to reset state via `update_viewport_throttled`.
      // Drop any pending viewport update so we don't emit it against the wrong tab.
      self.viewport_throttle_tab = Some(tab_id);
      self.viewport_throttle.reset();
      return;
    }

    let now = std::time::Instant::now();
    if let Some(update) = self.viewport_throttle.poll(now) {
      self.send_viewport_changed_clamped_if_needed(tab_id, update.viewport_css, update.dpr());
    }
  }

  fn force_send_viewport_now(&mut self) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      return;
    };

    if self.viewport_throttle_tab != Some(tab_id) {
      self.viewport_throttle_tab = Some(tab_id);
      self.viewport_throttle.reset();
    }

    let now = std::time::Instant::now();
    if let Some(update) = self.viewport_throttle.force_send_now(now) {
      self.send_viewport_changed_clamped_if_needed(tab_id, update.viewport_css, update.dpr());
    }
  }

  fn render_hud(&mut self, ctx: &egui::Context) {
    let Some(hud) = self.hud.as_mut() else {
      return;
    };

    let pos = if let Some(page_rect) = self.page_rect_points {
      egui::pos2(page_rect.min.x + 6.0, page_rect.min.y + 6.0)
    } else {
      egui::pos2(6.0, 6.0)
    };

    let tab = self.browser_state.active_tab();
    let loading = tab.map(|t| t.loading).unwrap_or(false);
    // Use the monotonic stage/progress for user-facing display; the raw last-received stage can
    // regress if heartbeats arrive out-of-order (keep it as a debug signal).
    let stage = tab
      .and_then(|t| t.load_stage)
      .map(|s| s.as_str())
      .unwrap_or("-");
    let last_stage = tab
      .and_then(|t| t.stage)
      .map(|s| s.as_str())
      .unwrap_or("-");

    let warning = tab.and_then(|t| t.warning.as_deref());
    let viewport_clamped = warning.is_some_and(|w| w.starts_with("Viewport clamped:"));

    let (viewport_css, dpr) =
      if self.viewport_cache_tab == self.browser_state.active_tab_id() && self.viewport_cache_dpr > 0.0
      {
        (Some(self.viewport_cache_css), Some(self.viewport_cache_dpr))
      } else {
        (None, None)
      };

    use std::fmt::Write;
    hud.text_buf.clear();

    match (hud.last_frame_cpu_ms, hud.fps) {
      (Some(ms), Some(fps)) if ms.is_finite() && fps.is_finite() => {
        let _ = writeln!(&mut hud.text_buf, "ui: {ms:.1}ms  {fps:.0} fps");
      }
      (Some(ms), _) if ms.is_finite() => {
        let _ = writeln!(&mut hud.text_buf, "ui: {ms:.1}ms  - fps");
      }
      _ => {
        let _ = writeln!(&mut hud.text_buf, "ui: - ms  - fps");
      }
    }

    let _ = writeln!(
      &mut hud.text_buf,
      "tab: loading={} stage={} last={}",
      if loading { "yes" } else { "no" },
      stage,
      last_stage
    );

    if let (Some((w, h)), Some(dpr)) = (viewport_css, dpr) {
      let _ = writeln!(&mut hud.text_buf, "viewport_css: {w}x{h}  dpr: {dpr:.2}");
    } else {
      let _ = writeln!(&mut hud.text_buf, "viewport_css: -  dpr: -");
    }

    let _ = writeln!(
      &mut hud.text_buf,
      "clamped: {}",
      if viewport_clamped { "yes" } else { "no" }
    );

    if let Some(deadline) = self.viewport_throttle.next_deadline() {
      let due_in = deadline.saturating_duration_since(std::time::Instant::now());
      let _ = writeln!(
        &mut hud.text_buf,
        "viewport_throttle: pending (due in {}ms)",
        due_in.as_millis()
      );
    } else {
      let _ = writeln!(&mut hud.text_buf, "viewport_throttle: idle");
    }

    egui::Area::new(egui::Id::new("fastr_browser_hud"))
      .order(egui::Order::Foreground)
      .interactable(false)
      .fixed_pos(pos)
      .show(ctx, |ui| {
        let fill = egui::Color32::from_rgba_unmultiplied(
          self.theme.colors.raised.r(),
          self.theme.colors.raised.g(),
          self.theme.colors.raised.b(),
          230,
        );
        egui::Frame::none()
          .fill(fill)
          .stroke(egui::Stroke::new(
            self.theme.sizing.stroke_width,
            self.theme.colors.border,
          ))
          .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
          .inner_margin(egui::Margin::symmetric(self.theme.sizing.padding, self.theme.sizing.padding * 0.75))
          .show(ui, |ui| {
            ui.label(
              egui::RichText::new(hud.text_buf.as_str())
                .monospace()
                .small()
                .color(self.theme.colors.text_primary),
            );
          });
      });
  }
  fn render_debug_log_overlay(&mut self, ctx: &egui::Context) {
    if !self.debug_log_ui_enabled {
      return;
    }
    let margin = self.theme.sizing.padding;

    if !self.debug_log_ui_open {
      egui::Area::new(egui::Id::new("fastr_debug_log_reopen"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-margin, -margin))
        .show(ctx, |ui| {
          let label = if self.debug_log.is_empty() {
            "Debug log".to_string()
          } else {
            format!("Debug log ({})", self.debug_log.len())
          };
          let button = egui::Button::new(egui::RichText::new(label).small());
          if ui.add(button).clicked() {
            self.debug_log_ui_open = true;
          }
        });
      return;
    }

    let fill = egui::Color32::from_rgba_unmultiplied(
      self.theme.colors.raised.r(),
      self.theme.colors.raised.g(),
      self.theme.colors.raised.b(),
      230,
    );
    let frame = egui::Frame::none()
      .fill(fill)
      .stroke(egui::Stroke::new(
        self.theme.sizing.stroke_width,
        self.theme.colors.border,
      ))
      .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
      .inner_margin(egui::Margin::symmetric(
        self.theme.sizing.padding,
        self.theme.sizing.padding * 0.75,
      ));

    let mut open = self.debug_log_ui_open;
    egui::Window::new("Debug log")
      .open(&mut open)
      .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-margin, -margin))
      .collapsible(true)
      .resizable(true)
      .min_width(260.0)
      .min_height(120.0)
      .default_width(560.0)
      .default_height(200.0)
      .frame(frame)
      .show(ctx, |ui| {
        let mut wants_copy = false;
        ui.horizontal(|ui| {
          if ui.button("Clear").clicked() {
            self.debug_log.clear();
          }

          wants_copy = ui.button("Copy all").clicked();

          ui.separator();

          ui.label(egui::RichText::new("Filter:").small());
          ui.add(
            egui::TextEdit::singleline(&mut self.debug_log_filter)
              .hint_text("text")
              .desired_width(160.0),
          );
          if !self.debug_log_filter.is_empty() && ui.small_button("×").clicked() {
            self.debug_log_filter.clear();
          }
        });

        let filter_raw = self.debug_log_filter.trim();
        let filter = if filter_raw.is_empty() {
          None
        } else {
          Some(filter_raw.to_ascii_lowercase())
        };

        let matches_filter = |line: &str| {
          let Some(filter) = filter.as_deref() else {
            return true;
          };
          line.to_ascii_lowercase().contains(filter)
        };

        if wants_copy {
          let mut out = String::new();
          for (idx, line) in self
            .debug_log
            .iter()
            .filter(|line| matches_filter(line))
            .enumerate()
          {
            if idx > 0 {
              out.push('\n');
            }
            out.push_str(line);
          }
          ctx.output_mut(|o| o.copied_text = out);
        }

        ui.separator();

        let total_lines = self.debug_log.len();
        egui::ScrollArea::vertical()
          .auto_shrink([false, false])
          .stick_to_bottom(true)
          .show(ui, |ui| {
            if total_lines == 0 {
              ui.label(egui::RichText::new("No debug log lines yet.").italics().small());
              return;
            }

            for line in self.debug_log.iter().filter(|line| matches_filter(line)) {
              ui.label(egui::RichText::new(line).monospace().small());
            }
          });
      });

    self.debug_log_ui_open = open;
  }

  fn render_context_menu(&mut self, ctx: &egui::Context) {
    use fastrender::ui::ChromeAction;
    use fastrender::ui::context_menu::{
      apply_page_context_menu_action, build_page_context_menu_entries, PageContextMenuAction,
      PageContextMenuBuildInput, PageContextMenuEntry,
    };

    let (tab_id, pos_css, anchor_points, link_url, selected_idx) = match self.open_context_menu.as_ref() {
      Some(menu) => (
        menu.tab_id,
        menu.pos_css,
        menu.anchor_points,
        menu.link_url.clone(),
        menu.selected_idx,
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

    let page_url = self
      .browser_state
      .active_tab()
      .and_then(|tab| tab.committed_url.as_deref().or(tab.current_url.as_deref()));

    let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
      link_url: link_url.as_deref(),
      page_url,
      bookmarks: &self.bookmarks,
      history_panel_open: self.history_panel_open,
      bookmarks_panel_open: self.bookmarks_panel_open,
    });

    #[derive(Clone)]
    struct MenuItem {
      icon: &'static str,
      label: String,
      action: PageContextMenuAction,
    }

    #[derive(Clone)]
    enum MenuEntry {
      Item(MenuItem),
      Separator,
    }

    const MENU_CONTENT_WIDTH: f32 = 220.0;
    const MENU_ITEM_HEIGHT: f32 = 28.0;
    const MENU_SEPARATOR_HEIGHT: f32 = 9.0;
    const MENU_EDGE_MARGIN: f32 = 4.0;

    let menu_inner_margin = self.theme.sizing.padding * 0.5;

    let mut menu_entries = Vec::with_capacity(entries.len());
    for entry in entries {
      match entry {
        PageContextMenuEntry::Separator => {
          menu_entries.push(MenuEntry::Separator);
        }
        PageContextMenuEntry::Action(item) => {
          let icon = match (&item.action, item.checked) {
            (PageContextMenuAction::OpenLinkInNewTab(_), _) => "↗",
            (PageContextMenuAction::CopyLinkAddress(_), _) => "⧉",
            (PageContextMenuAction::BookmarkLink(_), true)
            | (PageContextMenuAction::BookmarkPage(_), true) => "★",
            (PageContextMenuAction::BookmarkLink(_), false)
            | (PageContextMenuAction::BookmarkPage(_), false) => "☆",
            (PageContextMenuAction::ToggleHistoryPanel, true) => "✓",
            (PageContextMenuAction::ToggleHistoryPanel, false) => "⌛",
            (PageContextMenuAction::ToggleBookmarksPanel, true) => "✓",
            (PageContextMenuAction::ToggleBookmarksPanel, false) => "☆",
            (PageContextMenuAction::Reload, _) => "⟳",
          };
          menu_entries.push(MenuEntry::Item(MenuItem {
            icon,
            label: item.label.to_string(),
            action: item.action,
          }));
        }
      }
    }

    let items: Vec<&MenuItem> = menu_entries
      .iter()
      .filter_map(|entry| match entry {
        MenuEntry::Item(item) => Some(item),
        MenuEntry::Separator => None,
      })
      .collect();

    if items.is_empty() {
      self.close_context_menu();
      self.window.request_redraw();
      return;
    }

    let mut selected_idx = selected_idx.min(items.len().saturating_sub(1));

    // Keyboard navigation / activation.
    let (nav_delta, activate_selected, jump_char) = ctx.input(|i| {
      let mut nav_delta: isize = 0;
      let mut activate_selected = false;
      let mut jump_char: Option<char> = None;

      for event in &i.events {
        match event {
          egui::Event::Key {
            key,
            pressed: true,
            repeat: _,
            modifiers,
          } => {
            // Don't steal browser/chrome shortcuts while the menu is open.
            if modifiers.alt || modifiers.command || modifiers.ctrl || modifiers.mac_cmd {
              continue;
            }

            match key {
              egui::Key::ArrowUp => nav_delta = nav_delta.saturating_sub(1),
              egui::Key::ArrowDown => nav_delta = nav_delta.saturating_add(1),
              egui::Key::Enter | egui::Key::Space => {
                activate_selected = true;
              }
              egui::Key::Home => nav_delta = isize::MIN,
              egui::Key::End => nav_delta = isize::MAX,
              _ => {}
            }
          }
          egui::Event::Text(text) => {
            if let Some(ch) = text.chars().next().filter(|ch| ch.is_alphanumeric()) {
              jump_char = Some(ch.to_ascii_lowercase());
            }
          }
          _ => {}
        }
      }

      (nav_delta, activate_selected, jump_char)
    });

    if nav_delta == isize::MIN {
      selected_idx = 0;
    } else if nav_delta == isize::MAX {
      selected_idx = items.len().saturating_sub(1);
    } else if nav_delta > 0 {
      selected_idx = (selected_idx + nav_delta as usize).min(items.len() - 1);
    } else if nav_delta < 0 {
      selected_idx = selected_idx.saturating_sub((-nav_delta) as usize);
    }

    if let Some(ch) = jump_char {
      // Search forward from the current selection (wrapping) for the first matching item.
      for offset in 1..=items.len() {
        let idx = (selected_idx + offset) % items.len();
        let first = items[idx]
          .label
          .chars()
          .find(|ch| !ch.is_whitespace())
          .map(|ch| ch.to_ascii_lowercase());
        if first == Some(ch) {
          selected_idx = idx;
          break;
        }
      }
    }

    let keyboard_action = activate_selected.then(|| items[selected_idx].action.clone());

    let screen_rect = ctx.input(|i| i.screen_rect());
    let bounds = fastrender::Rect::from_xywh(
      screen_rect.min.x,
      screen_rect.min.y,
      screen_rect.width(),
      screen_rect.height(),
    );

    let content_height = menu_entries
      .iter()
      .map(|entry| match entry {
        MenuEntry::Item(_) => MENU_ITEM_HEIGHT,
        MenuEntry::Separator => MENU_SEPARATOR_HEIGHT,
      })
      .sum::<f32>();
    let menu_size = fastrender::Size::new(
      MENU_CONTENT_WIDTH + menu_inner_margin * 2.0,
      content_height + menu_inner_margin * 2.0,
    );
    let menu_origin = fastrender::ui::context_menu::place_menu(
      fastrender::Point::new(anchor_points.x, anchor_points.y),
      menu_size,
      bounds,
      MENU_EDGE_MARGIN,
    );

    let popup_id = egui::Id::new((
      "fastr_page_context_menu",
      tab_id.0,
      pos_css.0.to_bits(),
      pos_css.1.to_bits(),
    ));

    let popup = egui::Area::new(popup_id)
      .order(egui::Order::Foreground)
      .fixed_pos(egui::pos2(menu_origin.x, menu_origin.y))
      .show(ctx, |ui| {
        let selection_bg_fill = ui.visuals().selection.bg_fill;
        let hovered_bg_fill = {
          // Use a subtle text-colored scrim so hover remains visible even when the theme's hovered
          // widget fill matches the popup background.
          let base = ui.visuals().text_color();
          let alpha = if ui.visuals().dark_mode { 24 } else { 14 };
          egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha)
        };

        let mut frame = egui::Frame::popup(ui.style());
        frame.inner_margin = egui::Margin::same(menu_inner_margin);
        // Ensure the context menu matches the theme's rounded + shadowed popups.
        frame.rounding = ui.visuals().menu_rounding;
        frame.shadow.extrusion = frame.shadow.extrusion.max(12.0);

        let frame = frame.show(ui, |ui| {
          ui.set_min_width(MENU_CONTENT_WIDTH);
          ui.set_max_width(MENU_CONTENT_WIDTH);
          ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

          let mut action: Option<PageContextMenuAction> = None;
          let mut selected_idx = selected_idx;

          let item_rounding = egui::Rounding::same(self.theme.sizing.corner_radius * 0.6);
          let selected_fill = selection_bg_fill;
          let hover_fill = hovered_bg_fill;

          let mut draw_item = |ui: &mut egui::Ui, idx: usize, item: &MenuItem| {
            let (rect, response) = ui.allocate_exact_size(
              egui::vec2(MENU_CONTENT_WIDTH, MENU_ITEM_HEIGHT),
              egui::Sense::click(),
            );

            let is_selected = idx == selected_idx;
            let bg = if is_selected {
              selected_fill
            } else if response.hovered() {
              hover_fill
            } else {
              egui::Color32::TRANSPARENT
            };

            if bg != egui::Color32::TRANSPARENT {
              ui.painter()
                .rect_filled(rect.shrink(1.0), item_rounding, bg);
            }

            ui.allocate_ui_at_rect(rect, |ui| {
              ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(10.0);
                ui.add_sized(
                  egui::vec2(16.0, MENU_ITEM_HEIGHT),
                  egui::Label::new(egui::RichText::new(item.icon)),
                );
                ui.add_space(8.0);
                ui.label(&item.label);
              });
            });

            if is_selected {
              response.request_focus();
            }

            if response.hovered() {
              selected_idx = idx;
            }
            if response.clicked() {
              action = Some(item.action.clone());
            }
          };

          let mut item_idx: usize = 0;
          for entry in &menu_entries {
            match entry {
              MenuEntry::Separator => {
                let (sep_rect, _) = ui.allocate_exact_size(
                  egui::vec2(MENU_CONTENT_WIDTH, MENU_SEPARATOR_HEIGHT),
                  egui::Sense::hover(),
                );
                let y = sep_rect.center().y;
                let inset = 8.0;
                let separator_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
                ui.painter().line_segment(
                  [
                    egui::pos2(sep_rect.min.x + inset, y),
                    egui::pos2(sep_rect.max.x - inset, y),
                  ],
                  egui::Stroke::new(1.0, separator_color),
                );
              }
              MenuEntry::Item(item) => {
                draw_item(ui, item_idx, item);
                item_idx += 1;
              }
            }
          }

          (action.or(keyboard_action), selected_idx)
        });

        (frame.response.rect, frame.inner)
      });

    let (popup_rect, (action, new_selected_idx)) = popup.inner;
    self.open_context_menu_rect = Some(popup_rect);

    if let Some(menu) = self.open_context_menu.as_mut() {
      menu.selected_idx = new_selected_idx;
    }

    let Some(action) = action else {
      return;
    };

    match action {
      PageContextMenuAction::CopyLinkAddress(url) => {
        ctx.output_mut(|o| o.copied_text = url);
      }
      PageContextMenuAction::Reload => {
        self.handle_chrome_actions(vec![ChromeAction::Reload]);
      }
      PageContextMenuAction::OpenLinkInNewTab(url) => {
        use fastrender::ui::RepaintReason;
        use fastrender::ui::UiToWorker;

        let tab_id = fastrender::ui::TabId::new();
        let tab_state = fastrender::ui::BrowserTabState::new(tab_id, url.clone());
        let cancel = tab_state.cancel.clone();
        self.tab_cancel.insert(tab_id, cancel.clone());

        self.browser_state.push_tab(tab_state, true);
        self.browser_state.chrome.address_bar_text = url.clone();
        // Opening a context-menu link in a new tab should focus the page (similar to clicking it).
        self.page_has_focus = true;
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
      action @ (PageContextMenuAction::BookmarkLink(_)
      | PageContextMenuAction::BookmarkPage(_)
      | PageContextMenuAction::ToggleHistoryPanel
      | PageContextMenuAction::ToggleBookmarksPanel) => {
        let result = apply_page_context_menu_action(
          &mut self.bookmarks,
          &mut self.history_panel_open,
          &mut self.bookmarks_panel_open,
          &action,
        );
        if result.bookmarks_changed {
          self.autosave_bookmarks();
        }
      }
    }

    self.close_context_menu();
    self.window.request_redraw();
  }

  fn render_select_dropdown(&mut self, ctx: &egui::Context) {
    use fastrender::tree::box_tree::SelectItem;
    use fastrender::ui::UiToWorker;

    let (
      tab_id,
      select_node_id,
      control,
      anchor_css,
      fallback_anchor_points,
      anchor_width_points,
      scroll_to_selected,
    ) = match self.open_select_dropdown.as_mut() {
      Some(dropdown) => (
        dropdown.tab_id,
        dropdown.select_node_id,
        dropdown.control.clone(),
        dropdown.anchor_css,
        dropdown.anchor_points,
        dropdown.anchor_width_points,
        std::mem::take(&mut dropdown.scroll_to_selected),
      ),
      None => {
        self.open_select_dropdown_rect = None;
        return;
      }
    };

    let mut anchor_rect_points: Option<egui::Rect> = None;
    let mut fallback_anchor_pos_points = fallback_anchor_points;
    let mut preferred_width_points = anchor_width_points
      .filter(|w| w.is_finite() && *w > 0.0)
      .unwrap_or(Self::SELECT_DROPDOWN_MIN_WIDTH_POINTS);

    if let Some(anchor_css) = anchor_css {
      if let Some(mapping) = self.page_input_mapping {
        if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
          anchor_rect_points = Some(rect_points);
          fallback_anchor_pos_points = egui::pos2(rect_points.min.x, rect_points.max.y);
          preferred_width_points = rect_points.width().max(preferred_width_points);
        }
      }
    }
    preferred_width_points = preferred_width_points.max(Self::SELECT_DROPDOWN_MIN_WIDTH_POINTS);

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

    let screen_rect_points = ctx.screen_rect();
    let screen_rect = fastrender::Rect::from_xywh(
      screen_rect_points.min.x,
      screen_rect_points.min.y,
      screen_rect_points.width(),
      screen_rect_points.height(),
    );
    let anchor_rect = anchor_rect_points
      .map(|rect| fastrender::Rect::from_xywh(rect.min.x, rect.min.y, rect.width(), rect.height()));

    let placement = fastrender::select_dropdown::select_dropdown_popup_placement(
      screen_rect,
      anchor_rect,
      fastrender::Point::new(fallback_anchor_pos_points.x, fallback_anchor_pos_points.y),
      preferred_width_points,
      Self::SELECT_DROPDOWN_MIN_WIDTH_POINTS,
      Self::SELECT_DROPDOWN_MAX_WIDTH_POINTS,
      Self::SELECT_DROPDOWN_MAX_HEIGHT_POINTS,
      Self::SELECT_DROPDOWN_EDGE_PADDING_POINTS,
    );

    let (popup_pos, popup_pivot) = match placement.direction {
      fastrender::select_dropdown::SelectDropdownPopupDirection::Down => (
        egui::pos2(placement.rect.min_x(), placement.rect.min_y()),
        egui::Align2::LEFT_TOP,
      ),
      fastrender::select_dropdown::SelectDropdownPopupDirection::Up => (
        egui::pos2(placement.rect.min_x(), placement.rect.max_y()),
        egui::Align2::LEFT_BOTTOM,
      ),
    };

    let theme_padding = self.theme.sizing.padding;
    let theme_corner_radius = self.theme.sizing.corner_radius;

    // Popup frame margin (used when translating from outer max size → inner scroll area max size).
    let popup_margin = egui::Margin::same(theme_padding * 0.5);
    let inner_width = (placement.rect.width() - popup_margin.left - popup_margin.right).max(0.0);
    let inner_max_height =
      (placement.rect.height() - popup_margin.top - popup_margin.bottom).max(0.0);

    let popup = egui::Area::new(egui::Id::new((
      "fastr_select_dropdown_popup",
      tab_id.0,
      select_node_id,
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(popup_pos)
    .pivot(popup_pivot)
    .show(ctx, |ui| {
      let mut frame = egui::Frame::popup(ui.style());
      frame.inner_margin = popup_margin;
      frame.rounding = egui::Rounding::same(theme_corner_radius);
      frame.shadow.extrusion = frame.shadow.extrusion.max(12.0);

      let frame = frame.show(ui, |ui| {
        ui.set_min_width(inner_width);

        let mut clicked_item_idx: Option<usize> = None;
        let mut scroll_to_selected = scroll_to_selected;

        let visuals = ui.visuals().clone();
        let hover_bg = if visuals.dark_mode {
          egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
        } else {
          egui::Color32::from_rgba_unmultiplied(0, 0, 0, 14)
        };

        let selection_base = visuals.selection.bg_fill;
        let [sr, sg, sb, _] = selection_base.to_array();
        let selection_bg = if visuals.dark_mode {
          egui::Color32::from_rgba_unmultiplied(sr, sg, sb, 140)
        } else {
          egui::Color32::from_rgba_unmultiplied(sr, sg, sb, 72)
        };
        let selection_hover_bg = if visuals.dark_mode {
          egui::Color32::from_rgba_unmultiplied(sr, sg, sb, 180)
        } else {
          egui::Color32::from_rgba_unmultiplied(sr, sg, sb, 104)
        };

        let body_font = ui
          .style()
          .text_styles
          .get(&egui::TextStyle::Body)
          .cloned()
          .unwrap_or_else(|| egui::FontId::proportional(14.0));
        let small_font = ui
          .style()
          .text_styles
          .get(&egui::TextStyle::Small)
          .cloned()
          .unwrap_or_else(|| egui::FontId::proportional(12.0));

        egui::ScrollArea::vertical()
          .max_height(inner_max_height)
          .auto_shrink([false, false])
          .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

            let row_height = 26.0;
            let row_rounding = egui::Rounding::same(theme_corner_radius * 0.7);
            let row_bg_inset_x = 4.0;
            let check_col_width = 18.0;
            let base_padding_x = 10.0;

            for (idx, item) in control.items.iter().enumerate() {
              match item {
                SelectItem::OptGroupLabel { label, disabled } => {
                  ui.add_space(6.0);
                  let (rect, _response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 18.0),
                    egui::Sense::hover(),
                  );
                  ui.painter().text(
                    egui::pos2(rect.min.x + base_padding_x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    small_font.clone(),
                    if *disabled {
                      visuals.weak_text_color()
                    } else {
                      visuals.text_color()
                    },
                  );
                  ui.add_space(2.0);
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
                  let row_sense = if *disabled {
                    egui::Sense::hover()
                  } else {
                    egui::Sense::click()
                  };
                  let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), row_sense);

                  if response.clicked() && !*disabled {
                    clicked_item_idx = Some(idx);
                  }

                  if *selected && scroll_to_selected {
                    response.scroll_to_me(Some(egui::Align::Center));
                    scroll_to_selected = false;
                  }

                  let hovered = response.hovered();
                  let mut bg = egui::Color32::TRANSPARENT;
                  if *selected {
                    bg = if hovered {
                      selection_hover_bg
                    } else {
                      selection_bg
                    };
                  } else if hovered && !*disabled {
                    bg = hover_bg;
                  }

                  let painter = ui.painter();
                  if bg != egui::Color32::TRANSPARENT {
                    let bg_rect = rect.shrink2(egui::vec2(row_bg_inset_x, 0.0));
                    painter.rect_filled(bg_rect, row_rounding, bg);
                  }

                  let text_color = if *disabled {
                    visuals.weak_text_color()
                  } else {
                    visuals.text_color()
                  };

                  let mut x = rect.min.x + base_padding_x;
                  if *selected {
                    painter.text(
                      egui::pos2(x, rect.center().y),
                      egui::Align2::LEFT_CENTER,
                      "✓",
                      body_font.clone(),
                      selection_base,
                    );
                  }
                  x += check_col_width;
                  if *in_optgroup {
                    x += 12.0;
                  }

                  painter.text(
                    egui::pos2(x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    base,
                    body_font.clone(),
                    text_color,
                  );
                }
              }
            }
          });

        clicked_item_idx
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
    if self.page_loading_overlay_blocks_input {
      // Defer hover sync until loading finishes; we don't want to hit-test against stale content.
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
      || self
        .open_context_menu_rect
        .is_some_and(|rect| rect.contains(pos_points))
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

  fn handle_profile_shortcuts(&mut self, key: winit::event::VirtualKeyCode) -> bool {
    // On macOS, prefer Cmd as the "command" modifier. Elsewhere, prefer Ctrl.
    let cmd = if cfg!(target_os = "macos") {
      (self.modifiers.logo() || self.modifiers.ctrl()) && !self.modifiers.alt()
    } else {
      self.modifiers.ctrl() && !self.modifiers.alt()
    };

    if cmd && !self.modifiers.shift() && matches!(key, winit::event::VirtualKeyCode::D) {
      self.toggle_bookmark_for_active_tab();
      return true;
    }

    if cmd
      && self.modifiers.shift()
      && matches!(
        key,
        winit::event::VirtualKeyCode::Delete | winit::event::VirtualKeyCode::Back
      )
    {
      self.clear_history();
      return true;
    }

    false
  }

  fn autosave_bookmarks(&self) {
    if let Some(autosave) = self.profile_autosave.as_ref() {
      let _ = autosave.send(fastrender::ui::AutosaveMsg::UpdateBookmarks(
        self.bookmarks.clone(),
      ));
    }
  }

  fn toggle_bookmark_for_active_tab(&mut self) {
    let Some(url) = self
      .browser_state
      .active_tab()
      .and_then(|tab| tab.committed_url.as_deref().or(tab.current_url.as_deref()))
      .map(str::to_string)
    else {
      return;
    };

    self.bookmarks.toggle_url(&url);
    self.autosave_bookmarks();
  }

  fn clear_history(&mut self) {
    self.history.clear();

    let Some(autosave) = self.profile_autosave.as_ref() else {
      return;
    };

    let _ = autosave.send(fastrender::ui::AutosaveMsg::UpdateHistory(self.history.clone()));

    // Force an immediate write of the cleared state, but don't block the UI thread waiting for the
    // ack.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    drop(done_rx);
    let _ = autosave.send(fastrender::ui::AutosaveMsg::Flush(done_tx));
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
    self
      .overlay_scrollbar_visibility
      .register_interaction(std::time::Instant::now());

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
        let had_cursor_near_scrollbars = self
          .last_cursor_pos_points
          .is_some_and(|pos| self.cursor_near_overlay_scrollbars(pos));
        let had_context_menu =
          self.open_context_menu.is_some() || self.pending_context_menu_request.is_some();

        // Winit does not provide cursor coordinates when leaving the window. Clear our cached
        // position so hover updates are not suppressed by stale dropdown rect checks.
        self.last_cursor_pos_points = None;
        if had_cursor_near_scrollbars {
          self
            .overlay_scrollbar_visibility
            .register_interaction(std::time::Instant::now());
        }

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
        if had_cursor_in_page
          || had_pointer_capture
          || had_scrollbar_drag
          || had_context_menu
          || had_cursor_near_scrollbars
        {
          self.window.request_redraw();
        }
      }
      WindowEvent::CursorMoved { position, .. } => {
        let was_cursor_near_scrollbars = self
          .last_cursor_pos_points
          .is_some_and(|pos| self.cursor_near_overlay_scrollbars(pos));
        let pos_points = egui::pos2(
          position.x as f32 / self.pixels_per_point,
          position.y as f32 / self.pixels_per_point,
        );
        self.last_cursor_pos_points = Some(pos_points);
        let now_cursor_near_scrollbars = self.cursor_near_overlay_scrollbars(pos_points);
        if was_cursor_near_scrollbars != now_cursor_near_scrollbars {
          self
            .overlay_scrollbar_visibility
            .register_interaction(std::time::Instant::now());
          // Egui doesn't necessarily request a repaint for pointer motion over the rendered page
          // image. We need explicit redraws so overlay scrollbars can fade in/out when the cursor
          // approaches the scrollbar area.
          self.window.request_redraw();
        }

        let drag_update = if let Some(drag) = self.scrollbar_drag.as_mut() {
          let delta_points = pos_points - drag.last_cursor_points;
          drag.last_cursor_points = pos_points;
          let axis_delta_points = match drag.axis {
            fastrender::ui::scrollbars::ScrollbarAxis::Vertical => delta_points.y,
            fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => delta_points.x,
          };
          Some((drag.tab_id, drag.axis, drag.scrollbar, axis_delta_points))
        } else {
          None
        };

        if let Some((tab_id, axis, scrollbar, axis_delta_points)) = drag_update {

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
          || self
            .open_context_menu_rect
            .is_some_and(|rect| rect.contains(pos_points))
        {
          return;
        }

        let Some(rect) = self.page_rect_points else {
          self.cursor_in_page = false;
          return;
        };
        let mut now_in_page = rect.contains(pos_points);
        if now_in_page && !self.pointer_captured && self.cursor_over_overlay_scrollbars(pos_points)
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

        if self.page_loading_overlay_blocks_input && !self.pointer_captured {
          // The rendered image is stale while a navigation is loading. Track whether the cursor is
          // inside the page rect (for cursor overrides), but do not forward hover updates.
          self.cursor_in_page = now_in_page;
          return;
        }

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
            if self.page_loading_overlay_blocks_input
              && self
                .page_rect_points
                .is_some_and(|page_rect| page_rect.contains(pos_points))
            {
              // While a navigation is loading we show a scrim/spinner over the last frame. Ignore
              // new pointer interactions so users can't interact with stale content.
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
                self
                  .overlay_scrollbar_visibility
                  .register_interaction(std::time::Instant::now());
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
              self.send_worker_msg(fastrender::ui::UiToWorker::ContextMenuRequest {
                tab_id,
                pos_css,
              });
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

        // Profile-level shortcuts (bookmarks/history) should never reach page input.
        if self.handle_profile_shortcuts(key) {
          self.window.request_redraw();
          return;
        }

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
          }

          if matches!(key, VirtualKeyCode::PageUp) {
            self.update_open_select_dropdown_selection_by_enabled_delta(
              -Self::SELECT_DROPDOWN_PAGE_STEP,
            );
            self.window.request_redraw();
            return;
          }
          if matches!(key, VirtualKeyCode::PageDown) {
            self.update_open_select_dropdown_selection_by_enabled_delta(
              Self::SELECT_DROPDOWN_PAGE_STEP,
            );
            self.window.request_redraw();
            return;
          }

          // Typeahead uses `WindowEvent::ReceivedCharacter` (so we get the actual typed character
          // for the current keyboard layout). Do not dismiss the dropdown for plain alphanumeric key
          // presses without modifiers.
          let has_command_modifiers =
            self.modifiers.ctrl() || self.modifiers.logo() || self.modifiers.alt();
          if !has_command_modifiers
            && matches!(
              key,
              VirtualKeyCode::Key0
                | VirtualKeyCode::Key1
                | VirtualKeyCode::Key2
                | VirtualKeyCode::Key3
                | VirtualKeyCode::Key4
                | VirtualKeyCode::Key5
                | VirtualKeyCode::Key6
                | VirtualKeyCode::Key7
                | VirtualKeyCode::Key8
                | VirtualKeyCode::Key9
                | VirtualKeyCode::A
                | VirtualKeyCode::B
                | VirtualKeyCode::C
                | VirtualKeyCode::D
                | VirtualKeyCode::E
                | VirtualKeyCode::F
                | VirtualKeyCode::G
                | VirtualKeyCode::H
                | VirtualKeyCode::I
                | VirtualKeyCode::J
                | VirtualKeyCode::K
                | VirtualKeyCode::L
                | VirtualKeyCode::M
                | VirtualKeyCode::N
                | VirtualKeyCode::O
                | VirtualKeyCode::P
                | VirtualKeyCode::Q
                | VirtualKeyCode::R
                | VirtualKeyCode::S
                | VirtualKeyCode::T
                | VirtualKeyCode::U
                | VirtualKeyCode::V
                | VirtualKeyCode::W
                | VirtualKeyCode::X
                | VirtualKeyCode::Y
                | VirtualKeyCode::Z
            )
          {
            return;
          }

          // For all other keys, close the dropdown so the key press can act on the page/chrome
          // (e.g. Tab focus navigation, browser shortcuts).
          self.cancel_select_dropdown();
          self.window.request_redraw();
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
              ShortcutAction::Back | ShortcutAction::Forward => {
                // On macOS, egui does not expose bracket keys as `egui::Key` variants, so chrome
                // cannot observe Cmd+[ / Cmd+] via `ui::chrome_ui`. Handle these shortcuts at the
                // winit layer instead.
                if cfg!(target_os = "macos")
                  && matches!(key, VirtualKeyCode::LBracket | VirtualKeyCode::RBracket)
                {
                  use fastrender::ui::ChromeAction;
                  self.handle_chrome_actions(vec![if matches!(action, ShortcutAction::Back) {
                    ChromeAction::Back
                  } else {
                    ChromeAction::Forward
                  }]);
                }
                return;
              }

              // Chrome-level shortcuts are evaluated inside the egui frame (`ui::chrome_ui`) so we
              // can respect its editing focus rules. Ensure they never reach page input.
              ShortcutAction::FocusAddressBar
              | ShortcutAction::NewWindow
              | ShortcutAction::NewTab
              | ShortcutAction::CloseTab
              | ShortcutAction::ReopenClosedTab
              | ShortcutAction::NextTab
              | ShortcutAction::PrevTab
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
                  ShortcutAction::Copy => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::Copy { tab_id })
                  }
                  ShortcutAction::Cut => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::Cut { tab_id })
                  }
                  ShortcutAction::SelectAll => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::SelectAll { tab_id })
                  }
                  ShortcutAction::Paste => {
                    if let Ok(mut clipboard) = Clipboard::new() {
                      if let Ok(text) = clipboard.get_text() {
                        // egui-winit can also emit `egui::Event::Paste` from Ctrl/Cmd+V. Suppress
                        // it for this frame to avoid double pastes.
                        self.suppress_paste_events = true;
                        self.send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
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
          // On macOS, the Option/Alt key is commonly used for word-wise text navigation. Since we
          // prefer Cmd+[ / Cmd+] for history navigation on mac, allow Alt+Left/Right through to the
          // page so focused form controls can handle it.
          if !cfg!(target_os = "macos") {
            return;
          }
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

        let command = if cfg!(target_os = "macos") {
          (self.modifiers.logo() || self.modifiers.ctrl()) && !self.modifiers.alt()
        } else {
          self.modifiers.ctrl() && !self.modifiers.alt()
        };

        if command && matches!(key, VirtualKeyCode::Z) {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: if self.modifiers.shift() {
              fastrender::interaction::KeyAction::Redo
            } else {
              fastrender::interaction::KeyAction::Undo
            },
          });
          return;
        }
        if !cfg!(target_os = "macos")
          && self.modifiers.ctrl()
          && !self.modifiers.alt()
          && !self.modifiers.shift()
          && matches!(key, VirtualKeyCode::Y)
        {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: fastrender::interaction::KeyAction::Redo,
          });
          return;
        }

        if !self.modifiers.shift() {
          let word_mod = if cfg!(target_os = "macos") {
            alt_only
          } else {
            self.modifiers.ctrl() && !self.modifiers.alt()
          };
          if word_mod && matches!(key, VirtualKeyCode::Left) {
            self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
              tab_id,
              key: fastrender::interaction::KeyAction::WordLeft,
            });
            return;
          }
          if word_mod && matches!(key, VirtualKeyCode::Right) {
            self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
              tab_id,
              key: fastrender::interaction::KeyAction::WordRight,
            });
            return;
          }
        }

        let word_delete_mod = if cfg!(target_os = "macos") {
          alt_only
        } else {
          self.modifiers.ctrl() && !self.modifiers.alt()
        };
        if word_delete_mod && matches!(key, VirtualKeyCode::Back) {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: fastrender::interaction::KeyAction::WordBackspace,
          });
          return;
        }
        if word_delete_mod && matches!(key, VirtualKeyCode::Delete) {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: fastrender::interaction::KeyAction::WordDelete,
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
          self.handle_select_dropdown_typeahead(*ch);
          self.window.request_redraw();
          return;
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
          // Treat address bar focus as the only "chrome text input" focus surface for now.
          //
          // When the address bar has focus, keyboard input should not be forwarded to the page.
          // When it loses focus (via Enter/Escape/clicking elsewhere), restore page focus so common
          // scrolling shortcuts work without requiring an extra click.
          self.page_has_focus = !has_focus;
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
          // If we're in the middle of a debounced resize burst, flush the final viewport now so
          // the navigation lays out at the correct size.
          self.force_send_viewport_now();
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
          self.send_worker_msg(msg);
        }
        ChromeAction::Reload => {
          // Ensure the worker has the latest viewport before starting a navigation-affecting
          // operation.
          self.force_send_viewport_now();
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          if let Some(tab) = self.browser_state.tab_mut(tab_id) {
            tab.loading = true;
            tab.error = None;
            tab.stage = None;
            tab.title = None;
          }

          self.send_worker_msg(UiToWorker::Reload { tab_id });
        }
        ChromeAction::Back => {
          self.force_send_viewport_now();
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
          self.send_worker_msg(UiToWorker::GoBack { tab_id });
        }
        ChromeAction::Forward => {
          self.force_send_viewport_now();
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
          self.send_worker_msg(UiToWorker::GoForward { tab_id });
        }
      }
    }
  }

  fn render_frame(&mut self, control_flow: &mut winit::event_loop::ControlFlow) {
    let frame_start = if let Some(hud) = self.hud.as_mut() {
      let now = std::time::Instant::now();
      if let Some(prev) = hud.last_frame_start {
        let dt = now.saturating_duration_since(prev);
        let secs = dt.as_secs_f32();
        if secs.is_finite() && secs > 0.0 {
          hud.fps = Some((1.0 / secs).min(10_000.0));
        }
      }
      hud.last_frame_start = Some(now);
      Some(now)
    } else {
      None
    };

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

    // When using a full-size content view on macOS (transparent titlebar / unified toolbar),
    // the top chrome is drawn into the titlebar area. Reserve a left inset so the system traffic
    // lights remain visible and clickable.
    #[cfg(target_os = "macos")]
    let original_style = (*ctx.style()).clone();
    #[cfg(target_os = "macos")]
    {
      // Rough sizing (in egui points) for the traffic-light region: 3 × 12px buttons + padding.
      // This doesn't need to be pixel-perfect; it just needs to ensure we never place tab widgets
      // directly under the buttons.
      const TRAFFIC_LIGHTS_LEFT_INSET_POINTS: f32 = 72.0;
      let mut style = original_style.clone();
      style.spacing.window_margin.left =
        style.spacing.window_margin.left.max(TRAFFIC_LIGHTS_LEFT_INSET_POINTS);
      ctx.set_style(style);
    }

    let chrome_actions = fastrender::ui::chrome_ui(&ctx, &mut self.browser_state, |tab_id| {
      self.tab_favicons.get(&tab_id).map(|tex| tex.id())
    });

    #[cfg(target_os = "macos")]
    {
      ctx.set_style(original_style);
    }

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

    // ---------------------------------------------------------------------------
    // Bookmarks / History panels (simple browser-ui-only views)
    // ---------------------------------------------------------------------------
    //
    // These are toggled by the page context menu. They live in the windowed UI so they can reuse
    // the in-memory stores + autosave plumbing without needing renderer-level `about:` pages.
    let mut panel_actions: Vec<fastrender::ui::ChromeAction> = Vec::new();
    let mut close_bookmarks_panel = false;
    let mut close_history_panel = false;
    let mut clear_history = false;

    if self.bookmarks_panel_open {
      egui::SidePanel::right("fastr_bookmarks_panel")
        .resizable(true)
        .default_width(320.0)
        .show(&ctx, |ui| {
          ui.horizontal(|ui| {
            ui.heading("Bookmarks");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
              if ui.button("✕").clicked() {
                close_bookmarks_panel = true;
              }
            });
          });
          ui.separator();

          if self.bookmarks.urls.is_empty() {
            ui.label("No bookmarks.");
            return;
          }

          egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
              for url in &self.bookmarks.urls {
                if ui.button(url).clicked() {
                  panel_actions.push(fastrender::ui::ChromeAction::NavigateTo(url.clone()));
                }
              }
            });
        });
    } else if self.history_panel_open {
      egui::SidePanel::right("fastr_history_panel")
        .resizable(true)
        .default_width(360.0)
        .show(&ctx, |ui| {
          ui.horizontal(|ui| {
            ui.heading("History");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
              if ui.button("✕").clicked() {
                close_history_panel = true;
              }
            });
          });

          ui.horizontal(|ui| {
            if ui.button("Clear").clicked() {
              clear_history = true;
            }
          });

          ui.separator();

          if self.history.entries.is_empty() {
            ui.label("No history.");
            return;
          }

          egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
              for entry in self.history.entries.iter().rev().take(200) {
                let label = entry
                  .title
                  .as_deref()
                  .filter(|t| !t.trim().is_empty())
                  .unwrap_or(entry.url.as_str());
                if ui.button(label).clicked() {
                  panel_actions.push(fastrender::ui::ChromeAction::NavigateTo(entry.url.clone()));
                }
              }
            });
        });
    }

    if close_bookmarks_panel {
      self.bookmarks_panel_open = false;
    }
    if close_history_panel {
      self.history_panel_open = false;
    }
    if clear_history {
      self.clear_history();
    }
    if !panel_actions.is_empty() {
      self.handle_chrome_actions(panel_actions);
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
      self.update_viewport_throttled(viewport_css, dpr);

      self.page_rect_points = None;
      self.page_viewport_css = None;
      self.page_input_tab = None;
      self.page_input_mapping = None;
      let prev_loading_overlay_blocks_input = self.page_loading_overlay_blocks_input;
      self.page_loading_overlay_blocks_input = false;
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

      let (tab_loading, tab_stage) = self
        .browser_state
        .tab(active_tab)
        .map(|t| (t.loading, t.stage))
        .unwrap_or((false, None));

      if let Some(tex) = self.tab_textures.get_mut(&active_tab) {
        let loading_ui =
          fastrender::ui::loading_overlay::decide_page_loading_ui(true, tab_loading, tab_stage);
        self.page_loading_overlay_blocks_input = loading_ui.intercept_pointer_events;

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

        let desired_filter = match self.page_texture_filter_policy {
          PageTextureFilterPolicy::Nearest => wgpu::FilterMode::Nearest,
          PageTextureFilterPolicy::Linear => wgpu::FilterMode::Linear,
          PageTextureFilterPolicy::Auto => {
            let (tex_w_px, tex_h_px) = tex.size_px();
            let drawn_px_w = size_points.x * self.pixels_per_point;
            let drawn_px_h = size_points.y * self.pixels_per_point;

            let one_to_one = if tex_w_px > 0
              && tex_h_px > 0
              && drawn_px_w.is_finite()
              && drawn_px_h.is_finite()
            {
              let scale_x = drawn_px_w / tex_w_px as f32;
              let scale_y = drawn_px_h / tex_h_px as f32;
              const EPSILON: f32 = 0.01;
              (scale_x - 1.0).abs() < EPSILON && (scale_y - 1.0).abs() < EPSILON
            } else {
              true
            };

            if one_to_one {
              wgpu::FilterMode::Nearest
            } else {
              wgpu::FilterMode::Linear
            }
          }
        };

        tex.set_filter_mode(&self.device, &mut self.egui_renderer, desired_filter);
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

        if prev_loading_overlay_blocks_input != self.page_loading_overlay_blocks_input {
          if self.page_loading_overlay_blocks_input {
            // Loading overlay is now active: close any page-scoped UI (popups) and stop in-flight
            // pointer drags so stale content can't be interacted with.
            if self.open_select_dropdown.is_some() {
              self.cancel_select_dropdown();
            }
            if self.open_context_menu.is_some() || self.pending_context_menu_request.is_some() {
              self.close_context_menu();
            }
            if self.scrollbar_drag.is_some() {
              self.cancel_scrollbar_drag();
            }
            if self.pointer_captured {
              self.cancel_pointer_capture();
            }
            self.pending_pointer_move = None;
            if let Some(tab) = self.browser_state.tab_mut(active_tab) {
              tab.hovered_url = None;
              tab.cursor = fastrender::ui::CursorKind::Default;
            }
          } else {
            // Loading overlay is gone: re-sync hover state so cursor + hovered URL update
            // immediately.
            self.hover_sync_pending = true;
          }
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
            // If a wheel scroll is happening this frame, register it before drawing so scrollbars
            // become visible immediately (even if this is a single-tick wheel scroll).
            if !wheel_events.is_empty() && !wheel_blocked_by_dropdown && response.hovered() {
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
                self
                  .overlay_scrollbar_visibility
                  .register_interaction(std::time::Instant::now());
              }
            }

            let any_scrollbar_visible = self.overlay_scrollbars.vertical.is_some()
              || self.overlay_scrollbars.horizontal.is_some();
            if !any_scrollbar_visible {
              self.overlay_scrollbar_visibility =
                fastrender::ui::scrollbars::OverlayScrollbarVisibilityState::default();
            } else if self.overlay_scrollbars_force_visible()
              && self.overlay_scrollbar_visibility.visible_since.is_none()
            {
              // If the cursor is already near the scrollbar area (e.g. the page just became
              // scrollable), ensure the scrollbars become visible without requiring another cursor
              // move event.
              self
                .overlay_scrollbar_visibility
                .register_interaction(std::time::Instant::now());
            }

            let cfg = fastrender::ui::scrollbars::OverlayScrollbarVisibilityConfig::default();
            let force_visible = self.overlay_scrollbars_force_visible();
            let now = std::time::Instant::now();
            let dragging_any = self
              .scrollbar_drag
              .as_ref()
              .is_some_and(|drag| drag.tab_id == active_tab);
            let alpha = if dragging_any {
              1.0
            } else {
              self.overlay_scrollbar_visibility.alpha(now, cfg, force_visible)
            };
            if alpha > 0.0 {
              let painter = ui.painter();

              let to_egui_rect = |rect: fastrender::Rect| {
                egui::Rect::from_min_max(
                  egui::pos2(rect.min_x(), rect.min_y()),
                  egui::pos2(rect.max_x(), rect.max_y()),
                )
              };

              let shrink_rect = |rect: egui::Rect, dx: f32, dy: f32| {
                let min = rect.min + egui::vec2(dx, dy);
                let max = rect.max - egui::vec2(dx, dy);
                if min.x >= max.x || min.y >= max.y {
                  rect
                } else {
                  egui::Rect::from_min_max(min, max)
                }
              };

              let clamp_alpha = |base: u8| {
                ((base as f32) * alpha)
                  .round()
                  .clamp(0.0, 255.0) as u8
              };

              let visuals = ui.visuals();
              // Use theme-aware colors (dark mode uses light thumbs, light mode uses dark thumbs)
              // so overlay scrollbars remain visible against both light and dark content.
              let fill_color = visuals.text_color();
              let stroke_color = if visuals.dark_mode {
                visuals.panel_fill
              } else {
                visuals.window_fill
              };

              let cursor_pos = self.last_cursor_pos_points;
              let cursor = cursor_pos.map(|pos| fastrender::Point::new(pos.x, pos.y));
              const HOVER_INFLATE_POINTS: f32 = 10.0;

              let draw_scrollbar = |scrollbar: fastrender::ui::scrollbars::OverlayScrollbar,
                                    hovered: bool,
                                    dragging: bool| {
                let track = to_egui_rect(scrollbar.track_rect_points);
                let mut thumb = to_egui_rect(scrollbar.thumb_rect_points);

                // Modern overlay scrollbars are typically narrower by default, widening on hover.
                let cross_inset = if dragging { 0.5 } else if hovered { 1.0 } else { 2.0 };
                let length_inset = 1.0;
                thumb = match scrollbar.axis {
                  fastrender::ui::scrollbars::ScrollbarAxis::Vertical => {
                    shrink_rect(thumb, cross_inset, length_inset)
                  }
                  fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => {
                    shrink_rect(thumb, length_inset, cross_inset)
                  }
                };

                let thickness = match scrollbar.axis {
                  fastrender::ui::scrollbars::ScrollbarAxis::Vertical => thumb.width(),
                  fastrender::ui::scrollbars::ScrollbarAxis::Horizontal => thumb.height(),
                };
                let rounding = egui::Rounding::same((thickness * 0.5).max(0.0));

                let track_alpha = if dragging { 60 } else if hovered { 40 } else { 0 };
                if track_alpha > 0 {
                  painter.rect_filled(
                    track,
                    egui::Rounding::same((thickness * 0.5).max(0.0)),
                    egui::Color32::from_rgba_unmultiplied(
                      fill_color.r(),
                      fill_color.g(),
                      fill_color.b(),
                      clamp_alpha(track_alpha),
                    ),
                  );
                }

                let thumb_alpha = if dragging { 220 } else if hovered { 180 } else { 140 };
                painter.rect_filled(
                  thumb,
                  rounding,
                  egui::Color32::from_rgba_unmultiplied(
                    fill_color.r(),
                    fill_color.g(),
                    fill_color.b(),
                    clamp_alpha(thumb_alpha),
                  ),
                );
                painter.rect_stroke(
                  thumb,
                  rounding,
                  egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(
                      stroke_color.r(),
                      stroke_color.g(),
                      stroke_color.b(),
                      clamp_alpha(36),
                    ),
                  ),
                );
              };

              if let Some(v) = self.overlay_scrollbars.vertical {
                let dragging = self
                  .scrollbar_drag
                  .as_ref()
                  .is_some_and(|d| d.axis == v.axis && d.tab_id == active_tab);
                let hovered = cursor.is_some_and(|pos| {
                  v.track_rect_points
                    .inflate(HOVER_INFLATE_POINTS)
                    .contains_point(pos)
                });
                draw_scrollbar(v, hovered, dragging);
              }
              if let Some(h) = self.overlay_scrollbars.horizontal {
                let dragging = self
                  .scrollbar_drag
                  .as_ref()
                  .is_some_and(|d| d.axis == h.axis && d.tab_id == active_tab);
                let hovered = cursor.is_some_and(|pos| {
                  h.track_rect_points
                    .inflate(HOVER_INFLATE_POINTS)
                    .contains_point(pos)
                });
                draw_scrollbar(h, hovered, dragging);
              }
            } else if !force_visible {
              // Once fully hidden, clear state so the next interaction fades in again.
              self.overlay_scrollbar_visibility =
                fastrender::ui::scrollbars::OverlayScrollbarVisibilityState::default();
            }
          }
        }

        if matches!(loading_ui.kind, fastrender::ui::loading_overlay::PageLoadingUiKind::Overlay) {
          let painter = ui.painter();
          let scrim = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 44);
          painter.rect_filled(response.rect, egui::Rounding::same(0.0), scrim);

          let center = response.rect.center();
          let overlay_padding = self.theme.sizing.padding;
          let spinner_size = 18.0;
          let overlay_size = spinner_size + overlay_padding * 2.0;
          egui::Area::new(egui::Id::new(("fastr_page_loading_overlay", active_tab.0)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
              center.x - overlay_size * 0.5,
              center.y - overlay_size * 0.5,
            ))
            .show(&ctx, |ui| {
              let fill = ui.visuals().window_fill;
              let fill = egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 220);
              egui::Frame::none()
                .fill(fill)
                .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
                .inner_margin(egui::Margin::same(overlay_padding))
                .show(ui, |ui| {
                  ui.add(egui::Spinner::new().size(spinner_size));
                });
            });
        }

        if !wheel_events.is_empty()
          && !wheel_blocked_by_dropdown
          && response.hovered()
          && !self.page_loading_overlay_blocks_input
        {
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
        let loading_ui =
          fastrender::ui::loading_overlay::decide_page_loading_ui(false, tab_loading, tab_stage);

        let size_points = logical_viewport_points.max(egui::Vec2::ZERO);
        let (rect, _) = ui.allocate_exact_size(size_points, egui::Sense::hover());

        let painter = ui.painter();
        painter.rect_filled(rect, egui::Rounding::same(0.0), ui.visuals().panel_fill);

        if loading_ui.show_skeleton {
          let dark = ui.visuals().dark_mode;
          let skeleton = if dark {
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24)
          } else {
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 24)
          };
          let margin_x = 32.0;
          let line_h = 14.0;
          let gap_y = 10.0;
          let rounding = egui::Rounding::same(5.0);

          let x = rect.left() + margin_x;
          let max_w = (rect.width() - margin_x * 2.0).max(0.0);
          let mut y = rect.top() + 48.0;
          for frac in [0.55, 0.9, 0.8, 0.95, 0.6, 0.85] {
            let w = (max_w * frac).max(0.0);
            let line = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, line_h));
            painter.rect_filled(line, rounding, skeleton);
            y += line_h + gap_y;
          }
        }

        ui.allocate_ui_at_rect(rect, |ui| {
          ui.with_layout(
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
            |ui| {
              ui.spacing_mut().item_spacing.y = 10.0;
              ui.add(egui::Spinner::new().size(32.0));
              if let Some(headline) = loading_ui.headline {
                ui.label(egui::RichText::new(headline).strong().size(16.0));
              }
              if let Some(detail) = loading_ui.detail {
                ui.label(egui::RichText::new(detail).small());
              }
            },
          );
        });
      }
    });

    self.render_hud(&ctx);
    self.render_debug_log_overlay(&ctx);
    // Hovered-link URLs are rendered by `ui::chrome_ui` in the bottom status bar.
    self.render_select_dropdown(&ctx);
    self.render_context_menu(&ctx);
    self.sync_hover_after_tab_change(&ctx);
    // Coalesce pointer-move bursts to at most one message per rendered frame.
    self.flush_pending_pointer_move();

    let mut full_output = self.egui_ctx.end_frame();
    let repaint_after = full_output.repaint_after;
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

    // Honor egui's repaint scheduling. Focus changes and animated widgets often require follow-up
    // frames even when no OS events are incoming.
    self.schedule_egui_repaint(repaint_after);

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
            load: wgpu::LoadOp::Clear(self.clear_color),
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

    if let (Some(hud), Some(frame_start)) = (self.hud.as_mut(), frame_start) {
      let elapsed_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
      if elapsed_ms.is_finite() {
        hud.last_frame_cpu_ms = Some(elapsed_ms);
      }
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
    VirtualKeyCode::D => ShortcutKey::D,
    VirtualKeyCode::K => ShortcutKey::K,
    VirtualKeyCode::L => ShortcutKey::L,
    VirtualKeyCode::N => ShortcutKey::N,
    VirtualKeyCode::LBracket => ShortcutKey::OpenBracket,
    VirtualKeyCode::RBracket => ShortcutKey::CloseBracket,
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
    VirtualKeyCode::F6 => ShortcutKey::F6,
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
      // Common mouse back/forward button indices:
      // - Windows/macOS typically report 4/5.
      // - X11 typically reports 8/9.
      4 | 8 => fastrender::ui::PointerButton::Back,
      5 | 9 => fastrender::ui::PointerButton::Forward,
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

#[cfg(test)]
mod debug_log_env_tests {
  use super::{parse_env_bool, should_show_debug_log_ui};

  #[test]
  fn env_bool_parsing() {
    assert!(!parse_env_bool(None));
    assert!(!parse_env_bool(Some("")));
    assert!(!parse_env_bool(Some("   ")));
    assert!(!parse_env_bool(Some("0")));
    assert!(!parse_env_bool(Some("false")));
    assert!(!parse_env_bool(Some("FALSE")));
    assert!(!parse_env_bool(Some("no")));
    assert!(!parse_env_bool(Some("off")));

    assert!(parse_env_bool(Some("1")));
    assert!(parse_env_bool(Some("true")));
    assert!(parse_env_bool(Some("TRUE")));
    assert!(parse_env_bool(Some("yes")));
    assert!(parse_env_bool(Some("on")));
    assert!(parse_env_bool(Some("anything")));
  }

  #[test]
  fn debug_log_enablement_logic() {
    assert!(!should_show_debug_log_ui(false, None));
    assert!(!should_show_debug_log_ui(false, Some("0")));
    assert!(should_show_debug_log_ui(false, Some("1")));
    assert!(should_show_debug_log_ui(true, None));
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod mouse_button_mapping_tests {
  use super::map_mouse_button;
  use fastrender::ui::PointerButton;
  use winit::event::MouseButton;

  #[test]
  fn map_mouse_button_back_forward_other() {
    assert_eq!(map_mouse_button(MouseButton::Other(4)), PointerButton::Back);
    assert_eq!(map_mouse_button(MouseButton::Other(5)), PointerButton::Forward);
    assert_eq!(map_mouse_button(MouseButton::Other(8)), PointerButton::Back);
    assert_eq!(map_mouse_button(MouseButton::Other(9)), PointerButton::Forward);
    assert_eq!(
      map_mouse_button(MouseButton::Other(10)),
      PointerButton::Other(10)
    );
  }
}
