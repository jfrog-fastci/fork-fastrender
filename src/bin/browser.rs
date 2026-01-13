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

#[cfg(feature = "browser_ui")]
const ENV_BROWSER_SHOW_MENU_BAR: &str = "FASTR_BROWSER_SHOW_MENU_BAR";

// Experimental renderer-chrome toggle (render browser chrome via FastRender HTML/CSS).
//
// Manual smoke test (macOS VoiceOver / Windows Narrator / Linux Orca):
// 1) Build + run the browser UI:
//      bash scripts/run_limited.sh --as 64G -- \
//        bash scripts/cargo_agent.sh run --features browser_ui --bin browser
// 2) Enable FastRender chrome (once implemented) + accessibility adapter:
//      FASTR_BROWSER_RENDERER_CHROME=1 .../browser
// 3) With a screen reader running, focus the browser window and verify it exposes an accessibility
//    tree (at minimum: AccessKit updates should be produced without panics).
//
// Note: Today the windowed browser chrome is still egui-based by default; the FastRender chrome
// implementation is a work-in-progress under `instructions/renderer_chrome.md`.
#[cfg(feature = "browser_ui")]
const ENV_BROWSER_RENDERER_CHROME: &str = "FASTR_BROWSER_RENDERER_CHROME";

#[cfg(feature = "browser_ui")]
const ENV_BROWSER_LOG_SURFACE_CONFIGURE: &str = "FASTR_BROWSER_LOG_SURFACE_CONFIGURE";

#[cfg(feature = "browser_ui")]
const ENV_BROWSER_TRACE_OUT: &str = "FASTR_BROWSER_TRACE_OUT";

#[cfg(feature = "browser_ui")]
const ENV_BROWSER_MAX_PENDING_FRAME_BYTES: &str = "FASTR_BROWSER_MAX_PENDING_FRAME_BYTES";

#[cfg(feature = "browser_ui")]
const DEFAULT_BROWSER_MAX_PENDING_FRAME_BYTES: u64 = 512 * 1024 * 1024;

// Opt-in perf logging for UI-level performance investigations (e.g. allocation churn).
#[cfg(feature = "browser_ui")]
const ENV_PERF_LOG: &str = "FASTR_PERF_LOG";

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
fn parse_resize_dpr_scale_env(raw: Option<&str>) -> Result<f32, String> {
  // Defaults tuned for interactive resize. Lowering DPR reduces the pixmap + upload size so the UI
  // can stay responsive during a resize drag.
  const DEFAULT: f32 = 0.5;
  const MIN: f32 = 0.25;
  const MAX: f32 = 1.0;

  let Some(raw) = raw else {
    return Ok(DEFAULT);
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(DEFAULT);
  }

  let parsed = raw
    .parse::<f32>()
    .map_err(|_| format!("invalid value {raw:?}; expected a finite f32"))?;
  if !parsed.is_finite() {
    return Err(format!("invalid value {raw:?}; expected a finite f32"));
  }

  Ok(parsed.clamp(MIN, MAX))
}

#[cfg(any(test, feature = "browser_ui"))]
fn should_show_debug_log_ui(debug_build: bool, env_value: Option<&str>) -> bool {
  debug_build || parse_env_bool(env_value)
}

#[cfg(feature = "browser_ui")]
fn renderer_chrome_enabled_from_env() -> bool {
  parse_env_bool(std::env::var(ENV_BROWSER_RENDERER_CHROME).ok().as_deref())
}

#[cfg(any(test, feature = "browser_ui"))]
fn date_picker_day_a11y_label(year: i32, month: u32, day: u32, selected: bool) -> String {
  let date = format!("{:04}-{:02}-{:02}", year, month, day);
  if selected {
    format!("Selected date: {date}")
  } else {
    format!("Select date: {date}")
  }
}

#[cfg(any(test, feature = "browser_ui"))]
fn should_restore_page_focus_for_state(
  address_bar_has_focus: bool,
  bookmarks_panel_open: bool,
  history_panel_open: bool,
  downloads_panel_open: bool,
  clear_browsing_data_dialog_open: bool,
  tab_search_open: bool,
  find_in_page_open: bool,
) -> bool {
  !address_bar_has_focus
    && !bookmarks_panel_open
    && !history_panel_open
    && !downloads_panel_open
    && !clear_browsing_data_dialog_open
    && !tab_search_open
    && !find_in_page_open
}

#[cfg(any(test, feature = "browser_ui"))]
fn should_open_page_context_menu_from_keyboard(
  page_has_focus: bool,
  chrome_has_text_focus: bool,
  page_loading_overlay_blocks_input: bool,
) -> bool {
  page_has_focus && !chrome_has_text_focus && !page_loading_overlay_blocks_input
}

#[cfg(any(test, feature = "browser_ui"))]
fn should_forward_paste_events_to_page(
  page_has_focus: bool,
  chrome_has_text_focus: bool,
  suppress_paste_events: bool,
) -> bool {
  page_has_focus && !chrome_has_text_focus && !suppress_paste_events
}

#[cfg(any(test, feature = "browser_ui"))]
const RESIZE_BURST_TTL: std::time::Duration = std::time::Duration::from_millis(120);

#[cfg(any(test, feature = "browser_ui"))]
fn is_resize_burst_active(
  last_resize_event_at: Option<std::time::Instant>,
  now: std::time::Instant,
) -> bool {
  last_resize_event_at.is_some_and(|t| {
    now
      .checked_duration_since(t)
      .is_some_and(|dt| dt < RESIZE_BURST_TTL)
  })
}

#[cfg(any(test, feature = "browser_ui"))]
fn resize_burst_deadline(
  last_resize_event_at: Option<std::time::Instant>,
) -> Option<std::time::Instant> {
  last_resize_event_at.and_then(|t| t.checked_add(RESIZE_BURST_TTL))
}

#[cfg(test)]
mod input_focus_helpers_tests {
  use super::{should_forward_paste_events_to_page, should_open_page_context_menu_from_keyboard};

  #[test]
  fn keyboard_context_menu_requires_page_focus_and_no_chrome_text_focus() {
    assert!(!should_open_page_context_menu_from_keyboard(false, false, false));
    assert!(!should_open_page_context_menu_from_keyboard(true, true, false));
    assert!(!should_open_page_context_menu_from_keyboard(true, false, true));
    assert!(should_open_page_context_menu_from_keyboard(true, false, false));
  }

  #[test]
  fn paste_events_forwarding_respects_focus_and_suppression() {
    assert!(!should_forward_paste_events_to_page(false, false, false));
    assert!(!should_forward_paste_events_to_page(true, true, false));
    assert!(!should_forward_paste_events_to_page(true, false, true));
    assert!(should_forward_paste_events_to_page(true, false, false));
  }
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy)]
struct DrainBudget {
  max_messages: usize,
  max_duration: std::time::Duration,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainStopReason {
  Empty,
  Disconnected,
  Budget,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy)]
struct DrainOutcome {
  drained: usize,
  stop_reason: DrainStopReason,
}

#[cfg(any(test, feature = "browser_ui"))]
fn drain_mpsc_receiver_with_budget<T>(
  rx: &std::sync::mpsc::Receiver<T>,
  budget: DrainBudget,
  mut handle: impl FnMut(T),
) -> DrainOutcome {
  use std::sync::mpsc::TryRecvError;
  let start = std::time::Instant::now();
  let mut drained = 0usize;

  loop {
    if drained >= budget.max_messages || start.elapsed() >= budget.max_duration {
      return DrainOutcome {
        drained,
        stop_reason: DrainStopReason::Budget,
      };
    }

    match rx.try_recv() {
      Ok(msg) => {
        drained = drained.saturating_add(1);
        handle(msg);
      }
      Err(TryRecvError::Empty) => {
        return DrainOutcome {
          drained,
          stop_reason: DrainStopReason::Empty,
        };
      }
      Err(TryRecvError::Disconnected) => {
        return DrainOutcome {
          drained,
          stop_reason: DrainStopReason::Disconnected,
        };
      }
    }
  }
}

/// Coalesced scroll delta produced by an active overlay-scrollbar thumb drag.
///
/// CursorMoved events can arrive at very high frequency (hundreds/thousands/sec). During a thumb
/// drag we coalesce these deltas and forward at most one `UiToWorker::Scroll` per rendered frame.
#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy)]
struct PendingScrollDrag {
  tab_id: fastrender::ui::TabId,
  delta_css: (f32, f32),
}

#[cfg(any(test, feature = "browser_ui"))]
impl PendingScrollDrag {
  fn push(pending: &mut Option<Self>, tab_id: fastrender::ui::TabId, delta_css: (f32, f32)) {
    if delta_css.0 == 0.0 && delta_css.1 == 0.0 {
      return;
    }
    match pending {
      Some(existing) if existing.tab_id == tab_id => {
        existing.delta_css.0 += delta_css.0;
        existing.delta_css.1 += delta_css.1;
      }
      _ => {
        *pending = Some(Self { tab_id, delta_css });
      }
    }
  }

  fn into_worker_msg(self) -> fastrender::ui::UiToWorker {
    fastrender::ui::UiToWorker::Scroll {
      tab_id: self.tab_id,
      delta_css: self.delta_css,
      pointer_css: None,
    }
  }
}

// Keep URL resolution helpers available to both the windowed browser (feature = `browser_ui`) and
// unit tests (which often run without the full winit/wgpu/egui stack).
#[cfg(any(test, feature = "browser_ui"))]
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Resolve/normalize/validate a user-supplied address bar input using the same semantics as
/// `BrowserTabState::navigate_typed`.
#[cfg(any(test, feature = "browser_ui"))]
fn resolve_typed_navigation_url(raw: &str, current_url: Option<&str>) -> Result<String, String> {
  use url::Url;

  let raw_trimmed = trim_ascii_whitespace(raw);

  let normalized = if raw_trimmed.starts_with('#') {
    let current = current_url
      .ok_or_else(|| "cannot navigate to a fragment without an active document".to_string())?;
    let current = Url::parse(current).map_err(|err| err.to_string())?;
    current
      .join(raw_trimmed)
      .map_err(|err| err.to_string())?
      .to_string()
  } else {
    match fastrender::ui::resolve_omnibox_input(raw_trimmed)? {
      fastrender::ui::OmniboxInputResolution::Url { url } => url,
      fastrender::ui::OmniboxInputResolution::Search { url, .. } => url,
    }
  };

  fastrender::ui::validate_user_navigation_url_scheme(&normalized)?;
  Ok(normalized)
}

#[cfg(any(test, feature = "browser_ui"))]
fn open_url_in_new_tab_state(
  browser_state: &mut fastrender::ui::BrowserAppState,
  tab_cancel: &mut std::collections::HashMap<
    fastrender::ui::TabId,
    fastrender::ui::cancel::CancelGens,
  >,
  url: String,
  reason: fastrender::ui::NavigationReason,
) -> (fastrender::ui::TabId, Vec<fastrender::ui::UiToWorker>) {
  use fastrender::ui::{BrowserTabState, RepaintReason, TabId, UiToWorker};

  let new_tab_id = TabId::new();
  let mut tab_state = BrowserTabState::new(new_tab_id, url.clone());
  tab_state.loading = true;
  tab_state.unresponsive = false;
  tab_state.last_worker_msg_at = std::time::SystemTime::now();
  let cancel = tab_state.cancel.clone();
  tab_cancel.insert(new_tab_id, cancel.clone());
  browser_state.push_tab(tab_state, true);

  let msgs = vec![
    UiToWorker::CreateTab {
      tab_id: new_tab_id,
      initial_url: None,
      cancel,
    },
    UiToWorker::SetActiveTab { tab_id: new_tab_id },
    UiToWorker::Navigate {
      tab_id: new_tab_id,
      url,
      reason,
    },
    UiToWorker::RequestRepaint {
      tab_id: new_tab_id,
      reason: RepaintReason::Explicit,
    },
  ];

  (new_tab_id, msgs)
}

#[cfg(any(test, feature = "browser_ui"))]
fn open_typed_in_new_tab_state(
  browser_state: &mut fastrender::ui::BrowserAppState,
  tab_cancel: &mut std::collections::HashMap<
    fastrender::ui::TabId,
    fastrender::ui::cancel::CancelGens,
  >,
  raw: &str,
) -> Result<(fastrender::ui::TabId, Vec<fastrender::ui::UiToWorker>), String> {
  let current_url = browser_state
    .active_tab()
    .and_then(|tab| tab.current_url.as_deref())
    .map(str::to_string);
  let url = resolve_typed_navigation_url(raw, current_url.as_deref())?;
  Ok(open_url_in_new_tab_state(
    browser_state,
    tab_cancel,
    url,
    fastrender::ui::NavigationReason::TypedUrl,
  ))
}

#[cfg(any(test, feature = "browser_ui"))]
fn truncate_utf8_str_to_bytes(value: &str, max_bytes: usize) -> String {
  if max_bytes == 0 {
    return String::new();
  }
  if value.len() <= max_bytes {
    return value.to_string();
  }
  let mut end = max_bytes.min(value.len());
  while end > 0 && !value.is_char_boundary(end) {
    end -= 1;
  }
  value.get(..end).unwrap_or("").to_string()
}

#[cfg(any(test, feature = "browser_ui"))]
fn truncate_utf8_string_in_place(value: &mut String, max_bytes: usize) {
  if value.len() <= max_bytes {
    return;
  }
  if max_bytes == 0 {
    value.clear();
    return;
  }
  let mut end = max_bytes.min(value.len());
  while end > 0 && !value.is_char_boundary(end) {
    end -= 1;
  }
  value.truncate(end);
}

#[cfg(any(test, feature = "browser_ui"))]
fn sanitize_anchor_css_rect(rect: fastrender::geometry::Rect) -> fastrender::geometry::Rect {
  const MAX_ABS_POS: f32 = 1_000_000.0;
  const MAX_SIZE: f32 = 1_000_000.0;

  fn clamp_finite(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
      value.clamp(min, max)
    } else {
      fallback
    }
  }

  let x = clamp_finite(rect.origin.x, -MAX_ABS_POS, MAX_ABS_POS, 0.0);
  let y = clamp_finite(rect.origin.y, -MAX_ABS_POS, MAX_ABS_POS, 0.0);
  let w = clamp_finite(rect.size.width, 0.0, MAX_SIZE, 0.0);
  let h = clamp_finite(rect.size.height, 0.0, MAX_SIZE, 0.0);
  fastrender::geometry::Rect::from_xywh(x, y, w, h)
}

#[cfg(any(test, feature = "browser_ui"))]
fn sanitize_select_control_for_windowed_ui(
  mut control: fastrender::tree::box_tree::SelectControl,
) -> Option<fastrender::tree::box_tree::SelectControl> {
  use fastrender::tree::box_tree::SelectItem;
  use fastrender::ui::protocol_limits::{MAX_SELECT_ITEMS, MAX_SELECT_LABEL_BYTES};

  let items_len = control.items.len();
  if items_len > MAX_SELECT_ITEMS {
    return None;
  }

  // Ensure `selected` cannot contain out-of-range indices (which can cause overflow in keyboard
  // selection code).
  control.selected.retain(|idx| *idx < items_len);

  let needs_truncation = control.items.iter().any(|item| match item {
    SelectItem::OptGroupLabel { label, .. } => label.len() > MAX_SELECT_LABEL_BYTES,
    SelectItem::Option { label, value, .. } => {
      label.len() > MAX_SELECT_LABEL_BYTES || value.len() > MAX_SELECT_LABEL_BYTES
    }
  });

  if !needs_truncation {
    return Some(control);
  }

  let mut items = Vec::with_capacity(items_len);
  for item in control.items.iter() {
    match item {
      SelectItem::OptGroupLabel { label, disabled } => {
        let label = if label.len() <= MAX_SELECT_LABEL_BYTES {
          label.clone()
        } else {
          truncate_utf8_str_to_bytes(label, MAX_SELECT_LABEL_BYTES)
        };
        items.push(SelectItem::OptGroupLabel {
          label,
          disabled: *disabled,
        });
      }
      SelectItem::Option {
        node_id,
        label,
        value,
        selected,
        disabled,
        in_optgroup,
      } => {
        let label = if label.len() <= MAX_SELECT_LABEL_BYTES {
          label.clone()
        } else {
          truncate_utf8_str_to_bytes(label, MAX_SELECT_LABEL_BYTES)
        };
        let value = if value.len() <= MAX_SELECT_LABEL_BYTES {
          value.clone()
        } else {
          truncate_utf8_str_to_bytes(value, MAX_SELECT_LABEL_BYTES)
        };
        items.push(SelectItem::Option {
          node_id: *node_id,
          label,
          value,
          selected: *selected,
          disabled: *disabled,
          in_optgroup: *in_optgroup,
        });
      }
    }
  }

  control.items = std::sync::Arc::new(items);
  Some(control)
}

/// Sanitize untrusted renderer payloads before the windowed browser UI uses them.
///
/// This protects the egui overlay state from retaining enormous vectors/strings or propagating NaN
/// coordinates.
#[cfg(any(test, feature = "browser_ui"))]
fn sanitize_worker_to_ui_for_windowed_browser(
  msg: fastrender::ui::WorkerToUi,
) -> Option<fastrender::ui::WorkerToUi> {
  use fastrender::ui::protocol_limits::{MAX_ACCEPT_ATTR_BYTES, MAX_INPUT_VALUE_BYTES};
  use fastrender::ui::WorkerToUi;

  const ABSURD_INPUT_VALUE_BYTES_MULTIPLIER: usize = 64;
  let absurd_input_limit =
    MAX_INPUT_VALUE_BYTES.saturating_mul(ABSURD_INPUT_VALUE_BYTES_MULTIPLIER);

  match msg {
    WorkerToUi::OpenSelectDropdown {
      tab_id,
      select_node_id,
      control,
    } => {
      let control = sanitize_select_control_for_windowed_ui(control)?;
      Some(WorkerToUi::OpenSelectDropdown {
        tab_id,
        select_node_id,
        control,
      })
    }
    WorkerToUi::SelectDropdownOpened {
      tab_id,
      select_node_id,
      control,
      anchor_css,
    } => {
      let control = sanitize_select_control_for_windowed_ui(control)?;
      let anchor_css = sanitize_anchor_css_rect(anchor_css);
      Some(WorkerToUi::SelectDropdownOpened {
        tab_id,
        select_node_id,
        control,
        anchor_css,
      })
    }
    WorkerToUi::DateTimePickerOpened {
      tab_id,
      input_node_id,
      kind,
      mut value,
      anchor_css,
    } => {
      // Date/time picker values are expected to be tiny; treat extremely large payloads as invalid so
      // we never clone/parse them.
      if value.len() > absurd_input_limit {
        return None;
      }
      truncate_utf8_string_in_place(&mut value, MAX_INPUT_VALUE_BYTES);
      let anchor_css = sanitize_anchor_css_rect(anchor_css);
      Some(WorkerToUi::DateTimePickerOpened {
        tab_id,
        input_node_id,
        kind,
        value,
        anchor_css,
      })
    }
    WorkerToUi::FilePickerOpened {
      tab_id,
      input_node_id,
      multiple,
      mut accept,
      anchor_css,
    } => {
      if let Some(accept) = accept.as_mut() {
        truncate_utf8_string_in_place(accept, MAX_ACCEPT_ATTR_BYTES);
      }
      let anchor_css = sanitize_anchor_css_rect(anchor_css);
      Some(WorkerToUi::FilePickerOpened {
        tab_id,
        input_node_id,
        multiple,
        accept,
        anchor_css,
      })
    }
    other => Some(other),
  }
}

#[cfg(feature = "browser_ui")]
fn main() {
  if let Err(err) = run() {
    eprintln!("browser exited with error: {err}");
    std::process::exit(1);
  }
}

const ENV_BROWSER_HUD: &str = "FASTR_BROWSER_HUD";
const ENV_BROWSER_RENDERER_WATCHDOG: &str = "FASTR_BROWSER_RENDERER_WATCHDOG";
const ENV_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS: &str = "FASTR_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS";
const ENV_BROWSER_ALLOW_CRASH_URLS: &str = "FASTR_BROWSER_ALLOW_CRASH_URLS";

// Default renderer watchdog timeout for the browser's headless smoke harness.
//
// `run_headless_smoke_mode` historically used a hard-coded 60s timeout while waiting for the first
// frame. Keep the same default so existing CI runs stay stable unless explicitly overridden.
#[cfg(feature = "browser_ui")]
const DEFAULT_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS: u64 = 60_000;

#[cfg(feature = "browser_ui")]
const ENV_BROWSER_RESIZE_DPR_SCALE: &str = "FASTR_BROWSER_RESIZE_DPR_SCALE";

#[cfg(feature = "browser_ui")]
const RENDERER_UNRESPONSIVE_TIMEOUT: std::time::Duration = if cfg!(debug_assertions) {
  std::time::Duration::from_secs(5)
} else {
  std::time::Duration::from_secs(15)
};

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

fn parse_env_bool_override(key: &str, raw: Option<&str>) -> Result<Option<bool>, String> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(None);
  }

  if raw == "1"
    || raw.eq_ignore_ascii_case("true")
    || raw.eq_ignore_ascii_case("yes")
    || raw.eq_ignore_ascii_case("on")
  {
    return Ok(Some(true));
  }

  if raw == "0"
    || raw.eq_ignore_ascii_case("false")
    || raw.eq_ignore_ascii_case("no")
    || raw.eq_ignore_ascii_case("off")
  {
    return Ok(Some(false));
  }

  Err(format!("{key}: invalid value {raw:?}; expected 0|1|true|false"))
}

fn parse_env_u64_override(key: &str, raw: Option<&str>) -> Result<Option<u64>, String> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let raw_trimmed = raw.trim();
  if raw_trimmed.is_empty() {
    return Ok(None);
  }

  // Accept underscore separators for convenience (e.g. 60_000).
  let cleaned = raw_trimmed.replace('_', "");
  let parsed = cleaned
    .parse::<u64>()
    .map_err(|_| format!("{key}: invalid value {raw_trimmed:?}; expected u64"))?;
  Ok(Some(parsed))
}

fn parse_renderer_watchdog_enabled_env(raw: Option<&str>) -> Result<Option<bool>, String> {
  parse_env_bool_override(ENV_BROWSER_RENDERER_WATCHDOG, raw)
}

fn parse_renderer_watchdog_timeout_ms_env(raw: Option<&str>) -> Result<Option<u64>, String> {
  parse_env_u64_override(ENV_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS, raw)
}

fn parse_allow_crash_urls_env(raw: Option<&str>) -> Result<bool, String> {
  Ok(
    parse_env_bool_override(ENV_BROWSER_ALLOW_CRASH_URLS, raw)?
      .unwrap_or(false),
  )
}

#[cfg(feature = "browser_ui")]
fn parse_browser_show_menu_bar_env(raw: Option<&str>) -> Result<Option<bool>, String> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(None);
  }

  if raw == "1"
    || raw.eq_ignore_ascii_case("true")
    || raw.eq_ignore_ascii_case("yes")
    || raw.eq_ignore_ascii_case("on")
  {
    return Ok(Some(true));
  }

  if raw == "0"
    || raw.eq_ignore_ascii_case("false")
    || raw.eq_ignore_ascii_case("no")
    || raw.eq_ignore_ascii_case("off")
  {
    return Ok(Some(false));
  }

  Err(format!(
    "{ENV_BROWSER_SHOW_MENU_BAR}: invalid value {raw:?}; expected 0|1|true|false"
  ))
}

#[cfg(feature = "browser_ui")]
fn show_menu_bar_override_from_env() -> Option<bool> {
  let raw = match std::env::var(ENV_BROWSER_SHOW_MENU_BAR) {
    Ok(raw) => raw,
    Err(_) => return None,
  };

  match parse_browser_show_menu_bar_env(Some(&raw)) {
    Ok(value) => value,
    Err(err) => {
      eprintln!("{err}");
      None
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

#[cfg(test)]
mod browser_renderer_watchdog_env_tests {
  use super::*;

  #[test]
  fn parse_renderer_watchdog_enabled_env_values() {
    assert_eq!(parse_renderer_watchdog_enabled_env(None), Ok(None));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("")), Ok(None));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("   ")), Ok(None));

    assert_eq!(parse_renderer_watchdog_enabled_env(Some("1")), Ok(Some(true)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("true")), Ok(Some(true)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("TrUe")), Ok(Some(true)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("yes")), Ok(Some(true)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("on")), Ok(Some(true)));

    assert_eq!(parse_renderer_watchdog_enabled_env(Some("0")), Ok(Some(false)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("false")), Ok(Some(false)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("no")), Ok(Some(false)));
    assert_eq!(parse_renderer_watchdog_enabled_env(Some("off")), Ok(Some(false)));

    assert!(parse_renderer_watchdog_enabled_env(Some("maybe")).is_err());
  }

  #[test]
  fn parse_renderer_watchdog_timeout_ms_env_values() {
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(None), Ok(None));
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(Some("")), Ok(None));
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(Some("   ")), Ok(None));
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(Some("0")), Ok(Some(0)));
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(Some("1000")), Ok(Some(1000)));
    assert_eq!(parse_renderer_watchdog_timeout_ms_env(Some("1_000")), Ok(Some(1000)));
    assert!(parse_renderer_watchdog_timeout_ms_env(Some("wat")).is_err());
  }

  #[test]
  fn parse_allow_crash_urls_env_values() {
    assert_eq!(parse_allow_crash_urls_env(None), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("")), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("   ")), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("0")), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("1")), Ok(true));
    assert_eq!(parse_allow_crash_urls_env(Some("true")), Ok(true));
    assert_eq!(parse_allow_crash_urls_env(Some("yes")), Ok(true));
    assert_eq!(parse_allow_crash_urls_env(Some("on")), Ok(true));
    assert_eq!(parse_allow_crash_urls_env(Some("false")), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("no")), Ok(false));
    assert_eq!(parse_allow_crash_urls_env(Some("off")), Ok(false));
    assert!(parse_allow_crash_urls_env(Some("maybe")).is_err());
  }
}

#[cfg(test)]
mod resize_dpr_scale_env_tests {
  use super::parse_resize_dpr_scale_env;

  #[test]
  fn parse_resize_dpr_scale_defaults_and_clamps() {
    assert!((parse_resize_dpr_scale_env(None).unwrap() - 0.5).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("")).unwrap() - 0.5).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("   ")).unwrap() - 0.5).abs() < f32::EPSILON);

    assert!((parse_resize_dpr_scale_env(Some("1.0")).unwrap() - 1.0).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("2.0")).unwrap() - 1.0).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("0.25")).unwrap() - 0.25).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("0.1")).unwrap() - 0.25).abs() < f32::EPSILON);
    assert!((parse_resize_dpr_scale_env(Some("0.7")).unwrap() - 0.7).abs() < f32::EPSILON);

    assert!(parse_resize_dpr_scale_env(Some("nope")).is_err());
    assert!(parse_resize_dpr_scale_env(Some("NaN")).is_err());
    assert!(parse_resize_dpr_scale_env(Some("inf")).is_err());
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod browser_show_menu_bar_env_tests {
  use super::*;

  #[test]
  fn parse_browser_show_menu_bar_env_values() {
    assert_eq!(parse_browser_show_menu_bar_env(None), Ok(None));
    assert_eq!(parse_browser_show_menu_bar_env(Some("")), Ok(None));
    assert_eq!(parse_browser_show_menu_bar_env(Some("   ")), Ok(None));

    assert_eq!(parse_browser_show_menu_bar_env(Some("1")), Ok(Some(true)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("true")), Ok(Some(true)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("TrUe")), Ok(Some(true)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("yes")), Ok(Some(true)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("on")), Ok(Some(true)));

    assert_eq!(parse_browser_show_menu_bar_env(Some("0")), Ok(Some(false)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("false")), Ok(Some(false)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("no")), Ok(Some(false)));
    assert_eq!(parse_browser_show_menu_bar_env(Some("off")), Ok(Some(false)));

    assert!(parse_browser_show_menu_bar_env(Some("maybe")).is_err());
  }
}

#[cfg(test)]
mod open_typed_in_new_tab_tests {
  use super::open_typed_in_new_tab_state;
  use fastrender::ui::{BrowserAppState, BrowserTabState, NavigationReason, TabId, UiToWorker};
  use std::collections::HashMap;

  #[test]
  fn typed_open_in_new_tab_creates_and_activates_tab() {
    let mut browser_state = BrowserAppState::new();
    let tab_a = TabId::new();
    browser_state.push_tab(
      BrowserTabState::new(tab_a, "https://example.org/".to_string()),
      true,
    );

    let mut cancels = HashMap::new();
    let (new_tab_id, msgs) =
      open_typed_in_new_tab_state(&mut browser_state, &mut cancels, "example.com")
        .expect("typed URL should resolve");

    assert_eq!(browser_state.tabs.len(), 2);
    assert_eq!(browser_state.active_tab_id(), Some(new_tab_id));

    let new_tab = browser_state
      .tab(new_tab_id)
      .expect("new tab state should exist");
    assert_eq!(new_tab.current_url.as_deref(), Some("https://example.com/"));

    assert!(cancels.contains_key(&new_tab_id));

    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        UiToWorker::CreateTab { tab_id, initial_url: None, .. } if *tab_id == new_tab_id
      )),
      "expected CreateTab for {new_tab_id:?}, got {msgs:?}"
    );
    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        UiToWorker::SetActiveTab { tab_id } if *tab_id == new_tab_id
      )),
      "expected SetActiveTab for {new_tab_id:?}, got {msgs:?}"
    );
    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        UiToWorker::Navigate { tab_id, url, reason } if *tab_id == new_tab_id && url == "https://example.com/" && *reason == NavigationReason::TypedUrl
      )),
      "expected Navigate(TypedUrl) for {new_tab_id:?}, got {msgs:?}"
    );
  }

  #[test]
  fn typed_fragment_open_in_new_tab_joins_against_active_url() {
    let mut browser_state = BrowserAppState::new();
    let tab_a = TabId::new();
    browser_state.push_tab(
      BrowserTabState::new(tab_a, "https://example.org/path?x=1".to_string()),
      true,
    );

    let mut cancels = HashMap::new();
    let (new_tab_id, msgs) = open_typed_in_new_tab_state(&mut browser_state, &mut cancels, "#frag")
      .expect("fragment URL should resolve");

    assert_eq!(browser_state.tabs.len(), 2);
    assert_eq!(browser_state.active_tab_id(), Some(new_tab_id));

    let new_tab = browser_state
      .tab(new_tab_id)
      .expect("new tab state should exist");
    assert_eq!(
      new_tab.current_url.as_deref(),
      Some("https://example.org/path?x=1#frag")
    );

    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        UiToWorker::Navigate { tab_id, url, reason }
          if *tab_id == new_tab_id
            && url == "https://example.org/path?x=1#frag"
            && *reason == NavigationReason::TypedUrl
      )),
      "expected Navigate(TypedUrl) for fragment URL {new_tab_id:?}, got {msgs:?}"
    );
  }

  #[test]
  fn typed_open_in_new_tab_rejects_disallowed_scheme() {
    let mut browser_state = BrowserAppState::new();
    let tab_a = TabId::new();
    browser_state.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );

    let mut cancels = HashMap::new();
    let result =
      open_typed_in_new_tab_state(&mut browser_state, &mut cancels, "javascript:alert(1)");

    assert!(result.is_err(), "expected disallowed scheme to error");
    assert_eq!(
      browser_state.tabs.len(),
      1,
      "expected no tab to be created on error"
    );
    assert!(
      cancels.is_empty(),
      "expected no cancel token to be created on error"
    );
  }

  #[test]
  fn typed_search_query_open_in_new_tab_resolves_to_search_url() {
    let mut browser_state = BrowserAppState::new();
    let tab_a = TabId::new();
    browser_state.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );

    let mut cancels = HashMap::new();
    let (new_tab_id, msgs) = open_typed_in_new_tab_state(&mut browser_state, &mut cancels, "cats")
      .expect("search query should resolve");

    let new_tab = browser_state
      .tab(new_tab_id)
      .expect("new tab state should exist");
    assert_eq!(
      new_tab.current_url.as_deref(),
      Some("https://duckduckgo.com/?q=cats")
    );

    assert!(
      msgs.iter().any(|msg| matches!(
        msg,
        UiToWorker::Navigate { tab_id, url, reason }
          if *tab_id == new_tab_id
            && url == "https://duckduckgo.com/?q=cats"
            && *reason == NavigationReason::TypedUrl
      )),
      "expected Navigate(TypedUrl) to search URL for {new_tab_id:?}, got {msgs:?}"
    );
  }

  #[test]
  fn typed_open_in_new_tab_rejects_empty_input() {
    let mut browser_state = BrowserAppState::new();
    let tab_a = TabId::new();
    browser_state.push_tab(
      BrowserTabState::new(tab_a, "about:newtab".to_string()),
      true,
    );

    let mut cancels = HashMap::new();
    let result = open_typed_in_new_tab_state(&mut browser_state, &mut cancels, "   ");
    assert!(result.is_err(), "expected empty input to error");
    assert_eq!(browser_state.tabs.len(), 1);
    assert!(cancels.is_empty());
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod browser_accesskit_adapter_compile_tests {
  #[test]
  fn browser_links_accesskit_winit_adapter_when_browser_ui_enabled() {
    let _ = std::any::TypeId::of::<accesskit_winit::Adapter>();
  }
}

#[cfg(test)]
mod drain_mpsc_receiver_with_budget_tests {
  use super::*;

  #[test]
  fn stops_after_max_messages_and_allows_resuming() {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    for value in 0..10usize {
      tx.send(value).unwrap();
    }

    let mut seen = Vec::new();
    let budget = DrainBudget {
      max_messages: 4,
      max_duration: std::time::Duration::from_secs(10),
    };

    let outcome1 = drain_mpsc_receiver_with_budget(&rx, budget, |v| seen.push(v));
    assert_eq!(outcome1.drained, 4);
    assert_eq!(outcome1.stop_reason, DrainStopReason::Budget);

    let outcome2 = drain_mpsc_receiver_with_budget(&rx, budget, |v| seen.push(v));
    assert_eq!(outcome2.drained, 4);
    assert_eq!(outcome2.stop_reason, DrainStopReason::Budget);

    let outcome3 = drain_mpsc_receiver_with_budget(&rx, budget, |v| seen.push(v));
    assert_eq!(outcome3.drained, 2);
    assert_eq!(outcome3.stop_reason, DrainStopReason::Empty);

    assert_eq!(seen, (0..10).collect::<Vec<_>>());
  }

  #[test]
  fn returns_empty_when_queue_drained_but_channel_still_open() {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    tx.send(1).unwrap();
    tx.send(2).unwrap();

    let mut count = 0usize;
    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 10,
        max_duration: std::time::Duration::from_secs(10),
      },
      |_| count += 1,
    );

    assert_eq!(count, 2);
    assert_eq!(outcome.drained, 2);
    assert_eq!(outcome.stop_reason, DrainStopReason::Empty);

    // Keep `tx` alive; receiver should still report empty.
    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 10,
        max_duration: std::time::Duration::from_secs(10),
      },
      |_| unreachable!(),
    );
    assert_eq!(outcome.drained, 0);
    assert_eq!(outcome.stop_reason, DrainStopReason::Empty);
  }

  #[test]
  fn returns_disconnected_when_sender_dropped_and_queue_drained() {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    drop(tx);

    let mut collected = Vec::new();
    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 10,
        max_duration: std::time::Duration::from_secs(10),
      },
      |v| collected.push(v),
    );

    assert_eq!(collected, vec![1, 2]);
    assert_eq!(outcome.drained, 2);
    assert_eq!(outcome.stop_reason, DrainStopReason::Disconnected);
  }

  #[test]
  fn respects_zero_budgets() {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    for value in 0..3usize {
      tx.send(value).unwrap();
    }

    let mut drained = Vec::new();
    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 0,
        max_duration: std::time::Duration::from_secs(10),
      },
      |v| drained.push(v),
    );
    assert_eq!(drained, Vec::<usize>::new());
    assert_eq!(outcome.drained, 0);
    assert_eq!(outcome.stop_reason, DrainStopReason::Budget);

    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 10,
        max_duration: std::time::Duration::from_nanos(0),
      },
      |v| drained.push(v),
    );
    assert_eq!(outcome.drained, 0);
    assert_eq!(outcome.stop_reason, DrainStopReason::Budget);

    let outcome = drain_mpsc_receiver_with_budget(
      &rx,
      DrainBudget {
        max_messages: 10,
        max_duration: std::time::Duration::from_secs(10),
      },
      |v| drained.push(v),
    );
    assert_eq!(outcome.drained, 3);
    assert_eq!(outcome.stop_reason, DrainStopReason::Empty);
    assert_eq!(drained, vec![0, 1, 2]);
  }
}

#[cfg(test)]
mod protocol_payload_sanitization_tests {
  use super::sanitize_worker_to_ui_for_windowed_browser;
  use fastrender::geometry::Rect;
  use fastrender::tree::box_tree::{SelectControl, SelectItem};
  use fastrender::ui::messages::DateTimeInputKind;
  use fastrender::ui::protocol_limits::{
    MAX_ACCEPT_ATTR_BYTES, MAX_INPUT_VALUE_BYTES, MAX_SELECT_ITEMS, MAX_SELECT_LABEL_BYTES,
  };
  use fastrender::ui::{TabId, WorkerToUi};
  use std::sync::Arc;

  fn euro_string(bytes_over: usize) -> String {
    // "€" is 3 bytes in UTF-8; this helps ensure truncation respects char boundaries.
    let base_chars = (bytes_over / 3) + 16;
    "€".repeat(base_chars)
  }

  #[test]
  fn select_dropdown_open_with_too_many_items_is_dropped() {
    let items = (0..(MAX_SELECT_ITEMS + 1))
      .map(|idx| SelectItem::Option {
        node_id: idx + 1,
        label: format!("Item {idx}"),
        value: format!("v{idx}"),
        selected: false,
        disabled: false,
        in_optgroup: false,
      })
      .collect::<Vec<_>>();
    let control = SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(items),
      selected: Vec::new(),
    };

    let msg = WorkerToUi::SelectDropdownOpened {
      tab_id: TabId(1),
      select_node_id: 42,
      control,
      anchor_css: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    };

    assert!(
      sanitize_worker_to_ui_for_windowed_browser(msg).is_none(),
      "expected oversized SelectDropdownOpened to be dropped"
    );
  }

  #[test]
  fn select_dropdown_item_labels_and_values_are_truncated() {
    let label = euro_string(MAX_SELECT_LABEL_BYTES + 8);
    let value = euro_string(MAX_SELECT_LABEL_BYTES + 32);
    let items = vec![SelectItem::Option {
      node_id: 1,
      label,
      value,
      selected: false,
      disabled: false,
      in_optgroup: false,
    }];
    let control = SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(items),
      selected: vec![0],
    };

    let msg = WorkerToUi::OpenSelectDropdown {
      tab_id: TabId(1),
      select_node_id: 1,
      control,
    };

    let msg = sanitize_worker_to_ui_for_windowed_browser(msg).expect("message should be kept");
    let WorkerToUi::OpenSelectDropdown { control, .. } = msg else {
      panic!("expected OpenSelectDropdown after sanitization");
    };
    assert_eq!(control.items.len(), 1);
    let SelectItem::Option { label, value, .. } = &control.items[0] else {
      panic!("expected option item");
    };
    assert!(
      label.as_bytes().len() <= MAX_SELECT_LABEL_BYTES,
      "label not truncated: {} bytes",
      label.len()
    );
    assert!(
      value.as_bytes().len() <= MAX_SELECT_LABEL_BYTES,
      "value not truncated: {} bytes",
      value.len()
    );
    // Ensure truncation didn't split the 3-byte "€" character.
    assert_eq!(label.as_bytes().len() % 3, 0);
    assert_eq!(value.as_bytes().len() % 3, 0);
  }

  #[test]
  fn date_time_picker_value_is_truncated() {
    let value = euro_string(MAX_INPUT_VALUE_BYTES + 64);
    let msg = WorkerToUi::DateTimePickerOpened {
      tab_id: TabId(1),
      input_node_id: 99,
      kind: DateTimeInputKind::DateTimeLocal,
      value,
      anchor_css: Rect::from_xywh(1.0, 2.0, 3.0, 4.0),
    };

    let msg = sanitize_worker_to_ui_for_windowed_browser(msg).expect("message should be kept");
    let WorkerToUi::DateTimePickerOpened { value, .. } = msg else {
      panic!("expected DateTimePickerOpened after sanitization");
    };
    assert!(value.as_bytes().len() <= MAX_INPUT_VALUE_BYTES);
    assert_eq!(value.as_bytes().len() % 3, 0);
  }

  #[test]
  fn file_picker_accept_is_truncated_and_anchor_css_is_sanitized() {
    let accept = Some(euro_string(MAX_ACCEPT_ATTR_BYTES + 64));
    let msg = WorkerToUi::FilePickerOpened {
      tab_id: TabId(1),
      input_node_id: 5,
      multiple: true,
      accept,
      anchor_css: Rect::from_xywh(f32::NAN, f32::INFINITY, -f32::INFINITY, f32::NAN),
    };

    let msg = sanitize_worker_to_ui_for_windowed_browser(msg).expect("message should be kept");
    let WorkerToUi::FilePickerOpened {
      accept, anchor_css, ..
    } = msg
    else {
      panic!("expected FilePickerOpened after sanitization");
    };
    let accept = accept.expect("accept should remain Some");
    assert!(accept.as_bytes().len() <= MAX_ACCEPT_ATTR_BYTES);
    assert_eq!(accept.as_bytes().len() % 3, 0);

    assert!(anchor_css.origin.x.is_finite());
    assert!(anchor_css.origin.y.is_finite());
    assert!(anchor_css.size.width.is_finite());
    assert!(anchor_css.size.height.is_finite());
    assert!(anchor_css.size.width >= 0.0);
    assert!(anchor_css.size.height >= 0.0);
    assert!(anchor_css.origin.x.abs() <= 1_000_000.0);
    assert!(anchor_css.origin.y.abs() <= 1_000_000.0);
    assert!(anchor_css.size.width <= 1_000_000.0);
    assert!(anchor_css.size.height <= 1_000_000.0);
  }
}

#[cfg(feature = "browser_ui")]
use clap::Parser;
#[cfg(feature = "browser_ui")]
use fastrender::ui::os_clipboard;

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone)]
enum UserEvent {
  WorkerWake(winit::window::WindowId),
  SearchSuggestWake(winit::window::WindowId),
  RequestNewWindow(winit::window::WindowId),
  RequestNewWindowWithSession {
    from_id: winit::window::WindowId,
    window: fastrender::ui::BrowserSessionWindow,
  },
  /// An accessibility action request (e.g. "activate" or "focus") dispatched by the OS via AccessKit.
  AccessKitAction {
    window_id: winit::window::WindowId,
    request: accesskit::ActionRequest,
  },
}

#[cfg(feature = "browser_ui")]
impl From<accesskit_winit::ActionRequestEvent> for UserEvent {
  fn from(event: accesskit_winit::ActionRequestEvent) -> Self {
    UserEvent::AccessKitAction {
      window_id: event.window_id,
      request: event.request,
    }
  }
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

  /// Directory to save downloaded files
  ///
  /// When unset, defaults to `FASTR_BROWSER_DOWNLOAD_DIR`, then to the OS downloads directory, then
  /// to the current working directory.
  #[arg(long = "download-dir", value_name = "PATH")]
  download_dir: Option<std::path::PathBuf>,

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

  /// Enable the renderer watchdog (unresponsive/crash-smoke timeout).
  ///
  /// When unset, defaults to `FASTR_BROWSER_RENDERER_WATCHDOG`.
  #[arg(
    long = "renderer-watchdog",
    action = clap::ArgAction::SetTrue,
    overrides_with = "no_renderer_watchdog"
  )]
  renderer_watchdog: bool,

  /// Disable the renderer watchdog.
  ///
  /// When unset, defaults to `FASTR_BROWSER_RENDERER_WATCHDOG`.
  #[arg(
    long = "no-renderer-watchdog",
    action = clap::ArgAction::SetTrue,
    overrides_with = "renderer_watchdog"
  )]
  no_renderer_watchdog: bool,

  /// Renderer watchdog timeout in milliseconds.
  ///
  /// When unset, defaults to `FASTR_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS`.
  #[arg(long = "renderer-watchdog-timeout-ms", value_name = "MS", value_parser = parse_u64_ms)]
  renderer_watchdog_timeout_ms: Option<u64>,

  /// Allow navigating to `crash://` URLs (testing hook).
  ///
  /// Equivalent env: `FASTR_BROWSER_ALLOW_CRASH_URLS=1`.
  #[arg(long = "allow-crash-urls", action = clap::ArgAction::SetTrue)]
  allow_crash_urls: bool,

  /// Run a minimal headless startup smoke test (no window / wgpu init)
  #[arg(long = "headless-smoke", action = clap::ArgAction::SetTrue)]
  headless_smoke: bool,

  /// Run a minimal headless crash-isolation smoke test (no window / wgpu init)
  ///
  /// This mode spawns a UI worker, opens a tab, navigates to `crash://panic`, and asserts the
  /// worker terminates while the browser process stays alive.
  #[arg(long = "headless-crash-smoke", action = clap::ArgAction::SetTrue)]
  headless_crash_smoke: bool,

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
fn parse_u64_ms(raw: &str) -> Result<u64, String> {
  parse_u64_mb(raw)
}

#[cfg(feature = "browser_ui")]
fn max_pending_frame_bytes_from_env() -> u64 {
  let raw = match std::env::var(ENV_BROWSER_MAX_PENDING_FRAME_BYTES) {
    Ok(raw) => raw,
    Err(_) => return DEFAULT_BROWSER_MAX_PENDING_FRAME_BYTES,
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return DEFAULT_BROWSER_MAX_PENDING_FRAME_BYTES;
  }
  match trimmed.replace('_', "").parse::<u64>() {
    Ok(value) => value,
    Err(_) => {
      eprintln!(
        "{ENV_BROWSER_MAX_PENDING_FRAME_BYTES}: invalid value {trimmed:?}; expected an integer byte count"
      );
      DEFAULT_BROWSER_MAX_PENDING_FRAME_BYTES
    }
  }
}

#[cfg(feature = "browser_ui")]
fn resolve_download_directory(cli_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
  if let Some(path) = cli_path.filter(|p| !p.as_os_str().is_empty()) {
    return path.clone();
  }

  if let Some(raw) = std::env::var_os(fastrender::ui::browser_cli::ENV_DOWNLOAD_DIR) {
    if !raw.is_empty() {
      return std::path::PathBuf::from(raw);
    }
  }

  if let Some(user_dirs) = directories::UserDirs::new() {
    if let Some(downloads) = user_dirs.download_dir() {
      return downloads.to_path_buf();
    }
  }

  std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
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
  let image =
    image::load_from_memory(png_bytes).map_err(|err| format!("image decode error: {err}"))?;
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

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileShortcutAction {
  ToggleBookmarkForActiveTab,
  ToggleHistoryPanel,
  ToggleBookmarksManager,
  ClearHistory,
}

#[cfg(feature = "browser_ui")]
fn profile_shortcut_action(
  modifiers: winit::event::ModifiersState,
  key: winit::event::VirtualKeyCode,
) -> Option<ProfileShortcutAction> {
  // On macOS, prefer Cmd as the "command" modifier. Elsewhere, prefer Ctrl.
  let cmd = if cfg!(target_os = "macos") {
    (modifiers.logo() || modifiers.ctrl()) && !modifiers.alt()
  } else {
    modifiers.ctrl() && !modifiers.alt()
  };

  if cmd && !modifiers.shift() && matches!(key, winit::event::VirtualKeyCode::D) {
    return Some(ProfileShortcutAction::ToggleBookmarkForActiveTab);
  }

  if cmd
    && !modifiers.shift()
    && ((cfg!(target_os = "macos") && matches!(key, winit::event::VirtualKeyCode::Y))
      || (!cfg!(target_os = "macos") && matches!(key, winit::event::VirtualKeyCode::H)))
  {
    return Some(ProfileShortcutAction::ToggleHistoryPanel);
  }

  // Firefox-style history shortcut on macOS.
  if cmd
    && modifiers.shift()
    && cfg!(target_os = "macos")
    && matches!(key, winit::event::VirtualKeyCode::H)
  {
    return Some(ProfileShortcutAction::ToggleHistoryPanel);
  }

  if cmd && modifiers.shift() && matches!(key, winit::event::VirtualKeyCode::O) {
    return Some(ProfileShortcutAction::ToggleBookmarksManager);
  }

  if cmd
    && modifiers.shift()
    && matches!(
      key,
      winit::event::VirtualKeyCode::Delete | winit::event::VirtualKeyCode::Back
    )
  {
    return Some(ProfileShortcutAction::ClearHistory);
  }

  None
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

  #[test]
  fn date_picker_day_a11y_label_formats_full_date() {
    assert_eq!(
      date_picker_day_a11y_label(2026, 1, 13, false),
      "Select date: 2026-01-13"
    );
    assert_eq!(
      date_picker_day_a11y_label(2026, 1, 13, true),
      "Selected date: 2026-01-13"
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod profile_shortcut_tests {
  use super::{profile_shortcut_action, ProfileShortcutAction};

  use winit::event::{ModifiersState, VirtualKeyCode};

  fn mods(ctrl: bool, shift: bool, alt: bool, logo: bool) -> ModifiersState {
    let mut out = ModifiersState::empty();
    if ctrl {
      out.insert(ModifiersState::CTRL);
    }
    if shift {
      out.insert(ModifiersState::SHIFT);
    }
    if alt {
      out.insert(ModifiersState::ALT);
    }
    if logo {
      out.insert(ModifiersState::LOGO);
    }
    out
  }

  fn cmd_mods() -> ModifiersState {
    if cfg!(target_os = "macos") {
      mods(false, false, false, true)
    } else {
      mods(true, false, false, false)
    }
  }

  #[test]
  fn cmd_d_toggles_bookmark() {
    assert_eq!(
      profile_shortcut_action(cmd_mods(), VirtualKeyCode::D),
      Some(ProfileShortcutAction::ToggleBookmarkForActiveTab)
    );
  }

  #[test]
  fn altgr_d_does_not_toggle_bookmark() {
    // Guard against AltGr being encoded as Ctrl+Alt.
    let mut modifiers = cmd_mods();
    modifiers.insert(ModifiersState::ALT);
    assert_eq!(profile_shortcut_action(modifiers, VirtualKeyCode::D), None);
  }

  #[test]
  fn cmd_shift_o_opens_bookmarks_manager() {
    let mut modifiers = cmd_mods();
    modifiers.insert(ModifiersState::SHIFT);
    assert_eq!(
      profile_shortcut_action(modifiers, VirtualKeyCode::O),
      Some(ProfileShortcutAction::ToggleBookmarksManager)
    );
  }

  #[test]
  fn cmd_shift_delete_clears_history() {
    let mut modifiers = cmd_mods();
    modifiers.insert(ModifiersState::SHIFT);
    assert_eq!(
      profile_shortcut_action(modifiers, VirtualKeyCode::Delete),
      Some(ProfileShortcutAction::ClearHistory)
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn cmd_y_opens_history_panel_on_macos() {
    assert_eq!(
      profile_shortcut_action(cmd_mods(), VirtualKeyCode::Y),
      Some(ProfileShortcutAction::ToggleHistoryPanel)
    );
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn ctrl_h_opens_history_panel_on_other_platforms() {
    assert_eq!(
      profile_shortcut_action(cmd_mods(), VirtualKeyCode::H),
      Some(ProfileShortcutAction::ToggleHistoryPanel)
    );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn cmd_shift_h_opens_history_panel_on_macos() {
    let mut modifiers = cmd_mods();
    modifiers.insert(ModifiersState::SHIFT);
    assert_eq!(
      profile_shortcut_action(modifiers, VirtualKeyCode::H),
      Some(ProfileShortcutAction::ToggleHistoryPanel)
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod date_time_picker_a11y_tests {
  use super::App;

  fn begin_frame(ctx: &egui::Context) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::Pos2::new(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    ctx.begin_frame(raw);
  }

  #[test]
  fn date_time_picker_value_text_edit_has_accessible_label_and_requests_focus_on_open() {
    let ctx = egui::Context::default();
    begin_frame(&ctx);

    let mut value = "2026-01-13".to_string();
    let mut value_id: Option<egui::Id> = None;
    let tab_id = fastrender::ui::TabId(1);
    let input_node_id: usize = 42;

    egui::CentralPanel::default().show(&ctx, |ui| {
      let resp =
        App::render_date_time_picker_value_input(ui, tab_id, input_node_id, &mut value, true);
      value_id = Some(resp.id);
    });

    let _output = ctx.end_frame();

    let value_id = value_id.expect("expected value response id");
    assert!(
      ctx.memory(|mem| mem.has_focus(value_id)),
      "expected picker TextEdit to be focused when the picker opens"
    );

    let label = ctx.data(|d| d.get_temp::<String>(egui::Id::new("test_dt_picker_value_label")));
    assert!(
      label.as_deref().is_some_and(|label| !label.trim().is_empty()),
      "expected picker TextEdit to have a non-empty accessible label"
    );
    assert_eq!(label.as_deref(), Some("Value"));
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod page_context_menu_keyboard_shortcut_tests {
  use super::{
    is_page_context_menu_keyboard_gesture, keyboard_page_context_menu_request,
    page_context_menu_anchor_points_for_keyboard_open, KeyboardPageContextMenuOpenPlan,
  };

  use egui::{Pos2, Rect, Vec2};
  use fastrender::ui::{InputMapping, TabId, UiToWorker};
  use winit::event::{ModifiersState, VirtualKeyCode};

  fn shift_mods() -> ModifiersState {
    let mut modifiers = ModifiersState::empty();
    modifiers.insert(ModifiersState::SHIFT);
    modifiers
  }

  #[test]
  fn shift_f10_opens_page_context_menu_at_cursor_position() {
    let tab_id = TabId(1);
    let page_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(page_rect, (800, 600));
    let cursor = Pos2::new(110.0, 70.0);

    let plan = keyboard_page_context_menu_request(
      VirtualKeyCode::F10,
      shift_mods(),
      true,
      false,
      false,
      tab_id,
      Some(page_rect),
      None,
      Some(mapping),
      Some(cursor),
    )
    .expect("expected Shift+F10 to open page context menu");

    let (pending, msg) = match plan {
      KeyboardPageContextMenuOpenPlan::WorkerRequest { pending, msg } => (pending, msg),
      other => panic!("expected worker request plan, got: {other:?}"),
    };

    assert_eq!(pending.tab_id, tab_id);
    assert_eq!(pending.anchor_points, cursor);
    assert!((pending.pos_css.0 - 100.0).abs() < 1e-4);
    assert!((pending.pos_css.1 - 50.0).abs() < 1e-4);

    match msg {
      UiToWorker::ContextMenuRequest {
        tab_id: msg_tab_id,
        pos_css,
      } => {
        assert_eq!(msg_tab_id, tab_id);
        assert_eq!(pos_css, pending.pos_css);
      }
      other => panic!("unexpected worker message: {other:?}"),
    }
  }

  #[test]
  fn apps_key_anchors_menu_to_center_when_cursor_outside_page() {
    let tab_id = TabId(1);
    let page_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(page_rect, (800, 600));

    let cursor_outside = Pos2::new(0.0, 0.0);
    let expected_anchor = page_rect.center();
    let expected_pos_css = mapping
      .pos_points_to_pos_css_clamped(expected_anchor)
      .expect("mapping should convert center point");

    let plan = keyboard_page_context_menu_request(
      VirtualKeyCode::Apps,
      ModifiersState::empty(),
      true,
      false,
      false,
      tab_id,
      Some(page_rect),
      None,
      Some(mapping),
      Some(cursor_outside),
    )
    .expect("expected Apps key to open page context menu");

    let (pending, msg) = match plan {
      KeyboardPageContextMenuOpenPlan::WorkerRequest { pending, msg } => (pending, msg),
      other => panic!("expected worker request plan, got: {other:?}"),
    };

    assert_eq!(pending.anchor_points, expected_anchor);
    assert_eq!(pending.pos_css, expected_pos_css);

    match msg {
      UiToWorker::ContextMenuRequest {
        tab_id: msg_tab_id, ..
      } => {
        assert_eq!(msg_tab_id, tab_id);
      }
      other => panic!("unexpected worker message: {other:?}"),
    }
  }

  #[test]
  fn opens_ui_only_menu_when_mapping_unavailable() {
    let tab_id = TabId(1);
    let page_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let cursor = Pos2::new(110.0, 70.0);

    let plan = keyboard_page_context_menu_request(
      VirtualKeyCode::F10,
      shift_mods(),
      true,
      false,
      false,
      tab_id,
      Some(page_rect),
      None,
      None,
      Some(cursor),
    )
    .expect("expected Shift+F10 to open page context menu");

    let menu = match plan {
      KeyboardPageContextMenuOpenPlan::OpenUiOnly(menu) => menu,
      other => panic!("expected ui-only plan, got: {other:?}"),
    };

    assert_eq!(menu.tab_id, tab_id);
    assert_eq!(menu.anchor_points, cursor);
    assert_eq!(menu.pos_css, (-1.0, -1.0));
    assert!(menu.link_url.is_none());
    assert!(menu.image_url.is_none());
    assert!(!menu.can_copy);
    assert!(!menu.can_cut);
    assert!(!menu.can_paste);
    assert!(!menu.can_select_all);
  }

  #[test]
  fn ctrl_shift_f10_does_not_open() {
    let tab_id = TabId(1);
    let page_rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 600.0));
    let mapping = InputMapping::new(page_rect, (800, 600));

    let mut modifiers = shift_mods();
    modifiers.insert(ModifiersState::CTRL);

    assert!(!is_page_context_menu_keyboard_gesture(VirtualKeyCode::F10, modifiers));
    assert!(keyboard_page_context_menu_request(
      VirtualKeyCode::F10,
      modifiers,
      true,
      false,
      false,
      tab_id,
      Some(page_rect),
      None,
      Some(mapping),
      Some(Pos2::new(20.0, 30.0)),
    )
    .is_none());
  }

  #[test]
  fn anchor_falls_back_to_content_center_when_page_rect_unknown() {
    let content_rect =
      Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
    let cursor = Pos2::new(20.0, 30.0);
    let anchor = page_context_menu_anchor_points_for_keyboard_open(None, Some(content_rect), Some(cursor));
    assert_eq!(anchor, content_rect.center());
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

    assert_eq!(
      throttle.push_desired(t0 + Duration::from_millis(5), (120, 90), 2.0),
      None
    );

    assert_eq!(throttle.poll(t0 + Duration::from_millis(54)), None);
    let second = throttle
      .poll(t0 + Duration::from_millis(55))
      .expect("trailing update should flush after debounce");
    assert_eq!(second.viewport_css, (120, 90));
    assert!((second.dpr() - 2.0).abs() < f32::EPSILON);
  }
}

#[cfg(test)]
mod resize_burst_detector_tests {
  use super::*;
  use std::time::{Duration, Instant};

  #[test]
  fn resize_burst_active_respects_ttl() {
    let t0 = Instant::now();

    assert!(!is_resize_burst_active(None, t0));
    assert!(is_resize_burst_active(Some(t0), t0));

    let near_end = t0 + RESIZE_BURST_TTL - Duration::from_millis(1);
    assert!(
      is_resize_burst_active(Some(t0), near_end),
      "expected burst to remain active within ttl"
    );

    let at_end = t0 + RESIZE_BURST_TTL;
    assert!(
      !is_resize_burst_active(Some(t0), at_end),
      "expected burst to end at ttl boundary"
    );

    let after_end = t0 + RESIZE_BURST_TTL + Duration::from_millis(1);
    assert!(
      !is_resize_burst_active(Some(t0), after_end),
      "expected burst to end after ttl"
    );
  }

  #[test]
  fn resize_burst_inactive_when_last_event_is_in_the_future() {
    let t0 = Instant::now();
    let future = t0 + Duration::from_millis(10);
    assert!(
      !is_resize_burst_active(Some(future), t0),
      "expected future timestamps to be treated as inactive"
    );
  }

  #[test]
  fn resize_burst_deadline_is_last_event_plus_ttl() {
    let t0 = Instant::now();
    assert_eq!(resize_burst_deadline(None), None);
    assert_eq!(resize_burst_deadline(Some(t0)), Some(t0 + RESIZE_BURST_TTL));
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

  let mut loaded_session = match fastrender::ui::session::load_session(session_path) {
    Ok(session) => session,
    Err(err) => {
      eprintln!(
        "failed to load session from {}: {err}",
        session_path.display()
      );
      None
    }
  };

  if wants_restore {
    if let Some(session) = loaded_session.take() {
      if !session.did_exit_cleanly {
        eprintln!("previous session ended unexpectedly; restoring");
      }
      return (session, StartupSessionSource::Restored);
    }
  }

  // Preserve user/session configuration even when we don't restore tabs (e.g. `browser <url>`,
  // `--no-restore`).
  let home_url = loaded_session
    .as_ref()
    .map(|s| s.home_url.clone())
    .unwrap_or_else(|| fastrender::ui::about_pages::ABOUT_NEWTAB.to_string());

  if let Some(url) = cli_url {
    let mut session = fastrender::ui::BrowserSession::single(url);
    session.home_url = home_url;
    return (session.sanitized(), StartupSessionSource::CliUrl);
  }

  let mut session =
    fastrender::ui::BrowserSession::single(fastrender::ui::about_pages::ABOUT_NEWTAB.to_string());
  session.home_url = home_url;
  (session.sanitized(), StartupSessionSource::DefaultNewTab)
}

#[cfg(feature = "browser_ui")]
fn update_renderer_media_prefs_runtime_toggles(
  resolved_theme: fastrender::ui::renderer_media_prefs::ResolvedTheme,
  high_contrast: bool,
  reduced_motion: bool,
) {
  use std::collections::HashMap;
  use std::sync::Arc;

  // Capture a fresh snapshot of the process environment so we preserve unrelated `FASTR_*` flags
  // (profiling toggles, resource limits, etc). Then fill in missing media-preference knobs based on
  // the browser chrome appearance.
  //
  // We intentionally do **not** mutate the process environment itself because the browser process
  // is long-lived and tests may reuse it.
  let mut raw = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<_, _>>();

  for (k, v) in fastrender::ui::renderer_media_prefs::prefers_env_vars_for_appearance(
    resolved_theme,
    high_contrast,
    reduced_motion,
  ) {
    // Respect explicit renderer overrides (`FASTR_PREFERS_*` env vars). The browser UI only supplies
    // defaults when they are unset/empty.
    let has_explicit_override = raw
      .get(k)
      .is_some_and(|existing| !existing.trim().is_empty());
    if !has_explicit_override {
      raw.insert(k.to_string(), v.to_string());
    }
  }

  let toggles = Arc::new(fastrender::debug::runtime::RuntimeToggles::from_map(raw));
  fastrender::debug::runtime::update_runtime_toggles(toggles);
}

#[cfg(feature = "browser_ui")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
  let cli = BrowserCliArgs::parse();

  // ---------------------------------------------------------------------------
  // Crash/unresponsive testing knobs (CLI/env)
  // ---------------------------------------------------------------------------

  let allow_crash_urls = if cli.allow_crash_urls {
    true
  } else {
    let raw = std::env::var(ENV_BROWSER_ALLOW_CRASH_URLS).ok();
    match parse_allow_crash_urls_env(raw.as_deref()) {
      Ok(enabled) => enabled,
      Err(err) => {
        eprintln!("warning: {err}; ignoring");
        false
      }
    }
  };
  // Enable/disable crash:// scheme validation across the process (worker + browser thread).
  fastrender::ui::url::set_allow_crash_urls(allow_crash_urls);

  let renderer_watchdog_enabled = if cli.renderer_watchdog {
    true
  } else if cli.no_renderer_watchdog {
    false
  } else {
    let raw = std::env::var(ENV_BROWSER_RENDERER_WATCHDOG).ok();
    match parse_renderer_watchdog_enabled_env(raw.as_deref()) {
      Ok(Some(enabled)) => enabled,
      Ok(None) => true,
      Err(err) => {
        eprintln!("warning: {err}; ignoring");
        true
      }
    }
  };

  let renderer_watchdog_timeout_ms = if let Some(ms) = cli.renderer_watchdog_timeout_ms {
    Some(ms)
  } else {
    let raw = std::env::var(ENV_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS).ok();
    match parse_renderer_watchdog_timeout_ms_env(raw.as_deref()) {
      Ok(ms) => ms,
      Err(err) => {
        eprintln!("warning: {err}; ignoring");
        None
      }
    }
  };

  let renderer_watchdog_timeout = if renderer_watchdog_enabled {
    Some(std::time::Duration::from_millis(
      renderer_watchdog_timeout_ms.unwrap_or(DEFAULT_BROWSER_RENDERER_WATCHDOG_TIMEOUT_MS),
    ))
  } else {
    None
  };

  let download_dir = resolve_download_directory(cli.download_dir.as_ref());

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
  //
  // Crash-isolation smoke test (worker crash should not kill the browser process):
  //   bash scripts/run_limited.sh --as 64G -- \
  //     bash scripts/cargo_agent.sh run --features browser_ui --bin browser -- --headless-crash-smoke
  //
  // Or:
  //   FASTR_TEST_BROWSER_HEADLESS_CRASH_SMOKE=1 bash scripts/run_limited.sh --as 64G -- \
  //     bash scripts/cargo_agent.sh run --features browser_ui --bin browser
  if cli.headless_crash_smoke
    || std::env::var_os("FASTR_TEST_BROWSER_HEADLESS_CRASH_SMOKE").is_some()
  {
    return run_headless_crash_smoke_mode(download_dir);
  }
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

    return run_headless_smoke_mode(
      startup_session,
      source,
      session_path,
      download_dir,
      renderer_watchdog_timeout,
    );
  }

  if cli.js_enabled {
    eprintln!(
      "warning: --js is currently supported only with --headless-smoke (windowed UI script execution is not wired yet)"
    );
  }

  let (startup_session, _source) = determine_startup_session(cli_url, restore, &session_path);
  let startup_session = startup_session.sanitized();
  let bookmarks_path = fastrender::ui::bookmarks_path();
  let history_path = fastrender::ui::history_path();
  let bookmarks = match fastrender::ui::load_bookmarks(&bookmarks_path) {
    Ok(outcome) => outcome.value,
    Err(err) => {
      eprintln!(
        "failed to load bookmarks from {}: {err}",
        bookmarks_path.display()
      );
      fastrender::ui::BookmarkStore::default()
    }
  };
  let history = match fastrender::ui::load_history(&history_path) {
    Ok(outcome) => outcome.value,
    Err(err) => {
      eprintln!(
        "failed to load history from {}: {err}",
        history_path.display()
      );
      fastrender::ui::GlobalHistoryStore::default()
    }
  };

  // Seed the process-global about-page snapshot so `about:newtab` can render bookmarks + history
  // immediately (including persisted state) before any new navigation commits happen.
  fastrender::ui::about_pages::set_about_snapshot_from_stores(&bookmarks, &history);

  use std::collections::HashMap;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::Arc;
  use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
  let mut session_autosave =
    fastrender::ui::session_autosave::SessionAutosave::new(session_path.clone());
  use winit::event::Event;
  use winit::event::StartCause;
  use winit::event::WindowEvent;
  use winit::event_loop::ControlFlow;
  use winit::event_loop::EventLoopBuilder;
  use winit::window::Theme;
  use winit::window::WindowBuilder;
  use winit::window::WindowId;

  struct BrowserWindow {
    app: App,
    ui_rx: std::sync::mpsc::Receiver<fastrender::ui::WorkerToUi>,
    worker_wake_pending: Arc<AtomicBool>,
    bridge_join: Option<std::thread::JoinHandle<()>>,
  }

  impl BrowserWindow {
    fn shutdown(mut self) {
      self.app.shutdown();

      if let Some(join) = self.bridge_join.take() {
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
    }
  }

  let appearance_env = fastrender::ui::appearance::AppearanceEnvOverrides::from_env();
  let startup_appearance = startup_session.appearance.clone();
  let applied_appearance = startup_appearance
    .clone()
    .with_env_overrides(appearance_env);
  let theme_accent = applied_appearance
    .accent_color
    .as_deref()
    .and_then(fastrender::ui::theme_parsing::parse_hex_color)
    .map(|c| c.to_color32());
  let window_theme_override = match applied_appearance.theme {
    fastrender::ui::theme_parsing::BrowserTheme::Light => Some(Theme::Light),
    fastrender::ui::theme_parsing::BrowserTheme::Dark => Some(Theme::Dark),
    _ => None,
  };

  let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
  let event_loop_proxy = event_loop.create_proxy();
  let window_icon = load_window_icon();

  let browser_trace_out = match std::env::var(ENV_BROWSER_TRACE_OUT) {
    Ok(raw) => {
      let trimmed = raw.trim();
      if trimmed.is_empty() {
        None
      } else {
        Some(std::path::PathBuf::from(trimmed))
      }
    }
    Err(_) => None,
  };
  let browser_trace = if browser_trace_out.is_some() {
    fastrender::debug::trace::TraceHandle::enabled()
  } else {
    fastrender::debug::trace::TraceHandle::disabled()
  };

  // Keep a single profile autosave worker (bookmarks/history) across all windows.
  let (profile_autosave_tx, mut profile_autosave) =
    match fastrender::ui::ProfileAutosaveHandle::spawn(bookmarks_path.clone(), history_path.clone())
    {
      Ok(handle) => (Some(handle.sender()), Some(handle)),
      Err(err) => {
        eprintln!("failed to start profile autosave: {err}");
        (None, None)
      }
    };

  let mut global_bookmarks = bookmarks;
  let mut global_history = history;

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
  let home_url = startup_session.home_url.clone();
  let startup_active_window_index = startup_session.active_window_index;
  let startup_windows = startup_session.windows;
  let window_count = startup_windows.len();
  let active_idx = startup_active_window_index.min(window_count.saturating_sub(1));

  let build_window = move |target: &winit::event_loop::EventLoopWindowTarget<UserEvent>,
                           window_state: Option<fastrender::ui::BrowserWindowState>,
                           inherit_size: Option<PhysicalSize<u32>>,
                           inherit_pos: Option<PhysicalPosition<i32>>|
        -> Result<winit::window::Window, Box<dyn std::error::Error>> {
    let mut window_builder = WindowBuilder::new()
      .with_title("FastRender")
      .with_inner_size(LogicalSize::new(1200.0, 800.0))
      .with_min_inner_size(LogicalSize::new(480.0, 320.0))
      .with_window_icon(window_icon.clone())
      // Match native window chrome to the browser theme override when one is set; otherwise follow
      // the system theme.
      .with_theme(window_theme_override);

    if let Some(size) = inherit_size {
      window_builder = window_builder.with_inner_size(size);
    }
    if let Some(pos) = inherit_pos {
      window_builder = window_builder.with_position(pos);
    }

    if let Some(state) = window_state.as_ref() {
      if let (Some(width), Some(height)) = (state.width, state.height) {
        window_builder = window_builder.with_inner_size(PhysicalSize::new(
          width.clamp(1, i64::from(u32::MAX)) as u32,
          height.clamp(1, i64::from(u32::MAX)) as u32,
        ));
      }
      if let (Some(x), Some(y)) = (state.x, state.y) {
        window_builder = window_builder.with_position(PhysicalPosition::new(
          x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
          y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        ));
      }
      if state.maximized {
        window_builder = window_builder.with_maximized(true);
      }
    }

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

    let window = window_builder.build(target)?;

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

    Ok(window)
  };

  if window_count == 0 {
    return Err("no windows to create".into());
  }

  // Create all native windows first (so adapter selection can use a real surface), then wire up one
  // `App` + render worker per window.
  let mut create_order: Vec<usize> = (0..window_count).collect();
  if window_count > 1 {
    create_order.retain(|idx| *idx != active_idx);
    create_order.push(active_idx);
  }

  let mut winit_windows: Vec<Option<winit::window::Window>> =
    (0..window_count).map(|_| None).collect();
  for idx in create_order {
    let window_state = startup_windows
      .get(idx)
      .and_then(|w| w.window_state.clone());
    winit_windows[idx] = Some(build_window(&event_loop, window_state, None, None)?);
  }

  let first_window = winit_windows
    .iter()
    .find_map(|w| w.as_ref())
    .ok_or("failed to create any window")?;

  // Propagate browser chrome appearance (theme/high contrast/reduced motion) into the renderer's
  // user-preference media queries so `@media (prefers-color-scheme: dark)` etc. match by default.
  //
  // Explicit renderer env overrides (`FASTR_PREFERS_*`) continue to win.
  let resolved_theme = match applied_appearance.theme {
    fastrender::ui::theme_parsing::BrowserTheme::Dark => {
      fastrender::ui::renderer_media_prefs::ResolvedTheme::Dark
    }
    fastrender::ui::theme_parsing::BrowserTheme::Light => {
      fastrender::ui::renderer_media_prefs::ResolvedTheme::Light
    }
    fastrender::ui::theme_parsing::BrowserTheme::System => match first_window.theme() {
      Some(Theme::Dark) => fastrender::ui::renderer_media_prefs::ResolvedTheme::Dark,
      _ => fastrender::ui::renderer_media_prefs::ResolvedTheme::Light,
    },
  };
  update_renderer_media_prefs_runtime_toggles(
    resolved_theme,
    applied_appearance.high_contrast,
    applied_appearance.reduced_motion,
  );

  let gpu = pollster::block_on(GpuContext::new(first_window, wgpu_init))?;

  let mut windows: HashMap<WindowId, BrowserWindow> = HashMap::new();
  let mut window_ids_by_index: Vec<Option<WindowId>> = vec![None; window_count];

  for (idx, session_window) in startup_windows.into_iter().enumerate() {
    let Some(window) = winit_windows.get_mut(idx).and_then(Option::take) else {
      continue;
    };

    let worker_name = format!("fastr-browser-ui-worker-{idx}");
    let (ui_to_worker_tx, worker_to_ui_rx, worker_join) =
      fastrender::ui::spawn_browser_ui_worker(&worker_name)?;

    // Set the download directory once during startup, before any `CreateTab { initial_url: Some(..) }`
    // messages trigger a navigation.
    ui_to_worker_tx.send(fastrender::ui::UiToWorker::SetDownloadDirectory {
      path: download_dir.clone(),
    })?;

    let mut app = App::new(
      window,
      &event_loop,
      event_loop_proxy.clone(),
      ui_to_worker_tx,
      worker_join,
      &gpu,
      browser_trace.clone(),
      appearance_env,
      applied_appearance.clone(),
      theme_accent,
      bookmarks_path.clone(),
      history_path.clone(),
      global_bookmarks.clone(),
      global_history.clone(),
    )?;
    app.profile_autosave_tx = profile_autosave_tx.clone();
    app.home_url = home_url.clone();
    app.browser_state.appearance = startup_appearance.clone();
    app.startup(session_window);

    let window_id = app.window.id();
    window_ids_by_index[idx] = Some(window_id);

    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<fastrender::ui::WorkerToUi>();
    let worker_wake_pending = Arc::new(AtomicBool::new(false));
    let worker_wake_counters = app
      .hud
      .as_ref()
      .map(|hud| Arc::clone(&hud.worker_wake_counters));

    // Worker → UI messages are forwarded through a small bridge thread so that we can keep the winit
    // event loop in `ControlFlow::Wait` (no busy polling), while still waking immediately when a new
    // frame/message arrives.
    let bridge_join = std::thread::Builder::new()
      .name(format!("browser_worker_bridge_{window_id:?}"))
      .spawn({
        let event_loop_proxy = event_loop_proxy.clone();
        let worker_wake_pending = Arc::clone(&worker_wake_pending);
        let worker_wake_counters = worker_wake_counters.clone();
        move || {
          while let Ok(msg) = worker_to_ui_rx.recv() {
            if ui_tx.send(msg).is_err() {
              break;
            }
            // Coalesce wakes: the UI thread drains messages in batches so we only need one pending
            // wake event per window.
            let send_wake = !worker_wake_pending.swap(true, Ordering::AcqRel);
            if let Some(counters) = worker_wake_counters.as_ref() {
              if send_wake {
                counters.wake_events_sent.fetch_add(1, Ordering::Relaxed);
              } else {
                counters
                  .wake_events_coalesced
                  .fetch_add(1, Ordering::Relaxed);
              }
            }
            if send_wake {
              // Ignore failures during shutdown (event loop already dropped).
              let _ = event_loop_proxy.send_event(UserEvent::WorkerWake(window_id));
            }
          }
        }
      })?;

    // Kick the first frame so the window shows chrome immediately even before the worker responds.
    app.window.request_redraw();

    windows.insert(
      window_id,
      BrowserWindow {
        app,
        ui_rx,
        worker_wake_pending,
        bridge_join: Some(bridge_join),
      },
    );
  }

  let mut window_order: Vec<WindowId> = window_ids_by_index.into_iter().flatten().collect();
  if window_order.is_empty() {
    return Err("no windows created".into());
  }

  let mut active_window_id: Option<WindowId> = window_order.get(active_idx).copied();
  let mut next_window_index: usize = window_order.len();

  event_loop.run(move |event, event_loop_target, control_flow| {
    // Keep the session lock alive for the duration of the winit event loop.
    let _ = &session_lock;
    // Keep the event loop idle when there is no work to do.
    *control_flow = ControlFlow::Wait;

    // `EventLoop::run` never returns, so do shutdown hygiene (dropping channels and joining
    // threads) explicitly when the loop is torn down.
    if matches!(event, Event::LoopDestroyed) {
      let active_window_index = active_window_id
        .and_then(|id| window_order.iter().position(|other| *other == id))
        .unwrap_or(0)
        .min(window_order.len().saturating_sub(1));

      let mut session_windows: Vec<fastrender::ui::BrowserSessionWindow> =
        Vec::with_capacity(window_order.len());
      for id in &window_order {
        if let Some(win) = windows.get(id) {
          let mut session_window =
            fastrender::ui::BrowserSessionWindow::from_app_state(&win.app.browser_state);
          session_window.window_state = capture_window_state(&win.app.window);
          session_windows.push(session_window);
        }
      }

      let mut appearance = startup_appearance.clone();
      let mut home_url = home_url.clone();
      if let Some(active_id) = active_window_id.and_then(|id| windows.get(&id).map(|_| id)) {
        if let Some(active) = windows.get(&active_id) {
          appearance = active.app.browser_state.appearance.clone();
          home_url = active.app.home_url.clone();
        }
      }

      let mut session = fastrender::ui::BrowserSession::from_windows(
        session_windows,
        active_window_index,
        appearance,
      );
      session.home_url = home_url;
      session.did_exit_cleanly = true;

      if let Some(autosave) = profile_autosave.take() {
        autosave.shutdown_with_timeout(std::time::Duration::from_millis(500));
      } else {
        // Best-effort fallback: if profile autosave isn't running, persist synchronously on shutdown.
        if let Err(err) = fastrender::ui::save_bookmarks_atomic(&bookmarks_path, &global_bookmarks)
        {
          eprintln!(
            "failed to save bookmarks to {}: {err}",
            bookmarks_path.display()
          );
        }
        if let Err(err) = fastrender::ui::save_history_atomic(&history_path, &global_history) {
          eprintln!(
            "failed to save history to {}: {err}",
            history_path.display()
          );
        }
      }

      for (_, win) in windows.drain() {
        win.shutdown();
      }

      // Mark the session as clean on shutdown (best-effort).
      session_autosave.request_save(session.clone());
      if let Err(err) = session_autosave.shutdown(std::time::Duration::from_millis(500)) {
        eprintln!("session autosave shutdown failed: {err}");
        if let Err(err) = fastrender::ui::session::save_session_atomic(&session_path, &session) {
          eprintln!(
            "failed to save session to {}: {err}",
            session_path.display()
          );
        }
      }

      if let Some(path) = browser_trace_out.as_ref() {
        if let Err(err) = browser_trace.write_chrome_trace(path) {
          eprintln!("failed to write browser trace to {}: {err}", path.display());
        }
      }
      return;
    }

    let request_autosave = |windows: &HashMap<WindowId, BrowserWindow>,
                            window_order: &[WindowId],
                            active_window_id: Option<WindowId>| {
      let active_window_index = active_window_id
        .and_then(|id| window_order.iter().position(|other| *other == id))
        .unwrap_or(0)
        .min(window_order.len().saturating_sub(1));

      let mut session_windows: Vec<fastrender::ui::BrowserSessionWindow> =
        Vec::with_capacity(window_order.len());
      for id in window_order {
        if let Some(win) = windows.get(id) {
          let mut session_window =
            fastrender::ui::BrowserSessionWindow::from_app_state(&win.app.browser_state);
          session_window.window_state = capture_window_state(&win.app.window);
          session_windows.push(session_window);
        }
      }

      let mut appearance = startup_appearance.clone();
      let mut home_url = home_url.clone();
      if let Some(active_id) = active_window_id.and_then(|id| windows.get(&id).map(|_| id)) {
        if let Some(active) = windows.get(&active_id) {
          appearance = active.app.browser_state.appearance.clone();
          home_url = active.app.home_url.clone();
        }
      }

      let mut session = fastrender::ui::BrowserSession::from_windows(
        session_windows,
        active_window_index,
        appearance,
      );
      session.home_url = home_url;
      session.did_exit_cleanly = false;
      session_autosave.request_save(session);
    };

    match event {
      Event::WindowEvent { window_id, event } => {
        let mut session_dirty = matches!(
          event,
          WindowEvent::Focused(_)
            | WindowEvent::Moved(_)
            | WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. }
        );

        // Window close is handled specially so we can drop its worker + textures immediately.
        if matches!(event, WindowEvent::CloseRequested) {
          if windows.len() <= 1 {
            *control_flow = ControlFlow::Exit;
          } else if let Some(win) = windows.remove(&window_id) {
            window_order.retain(|id| *id != window_id);
            if active_window_id == Some(window_id) {
              active_window_id = window_order.last().copied();
            }
            win.shutdown();
            session_dirty = true;
          }
        } else if let Some(win) = windows.get_mut(&window_id) {
          let response = win.app.egui_state.on_event(&win.app.egui_ctx, &event);
          win.app.handle_winit_input_event(&event);

          // Always redraw on keyboard events so chrome shortcuts (handled inside the egui frame via
          // `ui::chrome_ui`) are evaluated even when egui doesn't request a repaint.
          let is_resize_event = matches!(
            event,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
          );
          if (response.repaint && !is_resize_event)
            || matches!(
              event,
              WindowEvent::KeyboardInput { .. } | WindowEvent::MouseWheel { .. }
            )
          {
            win.app.window.request_redraw();
          }

          match event {
            WindowEvent::Focused(true) => {
              active_window_id = Some(window_id);
            }
            WindowEvent::Resized(new_size) => {
              win.app.window_minimized = new_size.width == 0 || new_size.height == 0;
              win.app.resize(new_size);
              win.app.schedule_resize_redraw();
            }
            WindowEvent::ScaleFactorChanged {
              scale_factor,
              new_inner_size,
            } => {
              win.app.window_minimized = new_inner_size.width == 0 || new_inner_size.height == 0;
              win.app.set_system_pixels_per_point(scale_factor as f32);
              win.app.resize(*new_inner_size);
              win.app.schedule_resize_redraw();
            }
            WindowEvent::ThemeChanged(theme) => {
              if win.app.refresh_theme_from_system_theme(Some(theme)) {
                win.app.sync_renderer_media_prefs_to_runtime_toggles();
                win.app.window.request_redraw();
              }
            }
            _ => {}
          }
        }

        if session_dirty {
          request_autosave(&windows, &window_order, active_window_id);
        }
      }
      Event::UserEvent(UserEvent::WorkerWake(window_id)) => {
        // Drain pending worker messages. This is time/message-budgeted so that under pathological
        // message floods (e.g. debug-log spam) we don't monopolize the UI thread and starve input /
        // resize / OS events.
        let mut request_redraw = false;
        let mut history_changed = false;
        let mut session_dirty = false;
        let mut needs_follow_up_wake = false;
        if let Some(win) = windows.get_mut(&window_id) {
          let mut span = win.app.trace.span("worker_drain", "ui.worker");
          let hud_enabled = win.app.hud.is_some();
          // Clear the pending wake flag up-front so any messages that arrive while we're draining
          // can schedule another wake (avoids getting stuck with messages in the queue but no wake
          // event queued).
          win.worker_wake_pending.store(false, Ordering::Release);

          let outcome = {
            let ui_rx = &win.ui_rx;
            let app = &mut win.app;
            drain_mpsc_receiver_with_budget(
              ui_rx,
              DrainBudget {
                max_messages: 512,
                max_duration: std::time::Duration::from_millis(2),
              },
              |msg| {
                session_dirty |= matches!(
                  &msg,
                  fastrender::ui::WorkerToUi::NavigationCommitted { .. }
                    | fastrender::ui::WorkerToUi::RequestOpenInNewTab { .. }
                    | fastrender::ui::WorkerToUi::RequestOpenInNewTabRequest { .. }
                );

                let result = app.handle_worker_message(msg);
                request_redraw |= result.request_redraw;
                history_changed |= result.history_changed;
              },
            )
          };
          needs_follow_up_wake = outcome.stop_reason == DrainStopReason::Budget;

          if hud_enabled {
            if let Some(hud) = win.app.hud.as_mut() {
              hud.record_worker_wake(outcome.drained as u64);
            }
          }
          span.arg_u64("messages", outcome.drained as u64);

          if request_redraw {
            win.app.window.request_redraw();
          }

          if needs_follow_up_wake {
            // We ran out of budget; queue another wake event so the remaining messages are drained
            // soon, but only after returning control to the event loop.
            if !win.worker_wake_pending.swap(true, Ordering::AcqRel) {
              let _ = event_loop_proxy.send_event(UserEvent::WorkerWake(window_id));
            }
          }
        }

        if history_changed {
          if let Some(source) = windows.get(&window_id) {
            global_history = source.app.browser_state.history.clone();
          }
          fastrender::ui::about_pages::sync_about_page_snapshot_history_from_global_history_store(
            &global_history,
          );
          if let Some(tx) = profile_autosave_tx.as_ref() {
            let _ = tx.send(fastrender::ui::AutosaveMsg::UpdateHistory(
              global_history.clone(),
            ));
          }
          for win in windows.values_mut() {
            win.app.browser_state.history = global_history.clone();
            win.app.browser_state.visited.clear();
            win.app.browser_state.seed_visited_from_history();
            win.app.browser_state.chrome.omnibox.reset();
            win.app.window.request_redraw();
          }
        }
        if session_dirty {
          request_autosave(&windows, &window_order, active_window_id);
        }
      }
      Event::UserEvent(UserEvent::SearchSuggestWake(window_id)) => {
        let mut request_redraw = false;
        if let Some(win) = windows.get_mut(&window_id) {
          while let Some(update) = win.app.search_suggest.try_recv() {
            win.app.browser_state.chrome.remote_search_cache.query = update.query;
            win.app.browser_state.chrome.remote_search_cache.suggestions = update.suggestions;
            win.app.browser_state.chrome.remote_search_cache.fetched_at = update.fetched_at;
            request_redraw = true;
          }

          if request_redraw {
            win.app.window.request_redraw();
          }
        }
      }
      Event::UserEvent(UserEvent::AccessKitAction { window_id, request }) => {
        if let Some(win) = windows.get_mut(&window_id) {
          win.app.handle_accesskit_action_request(request);
          win.app.window.request_redraw();
        }
      }
      Event::UserEvent(UserEvent::RequestNewWindow(from_id)) => {
        let inherit_size = windows.get(&from_id).map(|win| win.app.window.inner_size());
        let inherit_pos = windows.get(&from_id).and_then(|win| {
          win
            .app
            .window
            .outer_position()
            .ok()
            .map(|pos| PhysicalPosition::new(pos.x.saturating_add(32), pos.y.saturating_add(32)))
        });
        let inherit_show_menu_bar = windows
          .get(&from_id)
          .map(|win| win.app.browser_state.chrome.show_menu_bar);

        let window = match build_window(event_loop_target, None, inherit_size, inherit_pos) {
          Ok(window) => window,
          Err(err) => {
            eprintln!("failed to create new window: {err}");
            return;
          }
        };

        let worker_name = format!("fastr-browser-ui-worker-{next_window_index}");
        next_window_index = next_window_index.saturating_add(1);

        let (ui_to_worker_tx, worker_to_ui_rx, worker_join) =
          match fastrender::ui::spawn_browser_ui_worker(&worker_name) {
            Ok(v) => v,
            Err(err) => {
              eprintln!("failed to spawn browser worker for new window: {err}");
              return;
            }
          };

        if let Err(err) = ui_to_worker_tx.send(fastrender::ui::UiToWorker::SetDownloadDirectory {
          path: download_dir.clone(),
        }) {
          eprintln!("failed to send download dir to new window worker: {err}");
        }

        let mut app = match App::new(
          window,
          event_loop_target,
          event_loop_proxy.clone(),
          ui_to_worker_tx,
          worker_join,
          &gpu,
          browser_trace.clone(),
          appearance_env,
          applied_appearance.clone(),
          theme_accent,
          bookmarks_path.clone(),
          history_path.clone(),
          global_bookmarks.clone(),
          global_history.clone(),
        ) {
          Ok(app) => app,
          Err(err) => {
            eprintln!("failed to create new window app: {err}");
            return;
          }
        };
        app.profile_autosave_tx = profile_autosave_tx.clone();
        app.home_url = home_url.clone();
        app.browser_state.appearance = startup_appearance.clone();

        app.startup(fastrender::ui::BrowserSessionWindow {
          tabs: vec![fastrender::ui::BrowserSessionTab {
            url: fastrender::ui::about_pages::ABOUT_NEWTAB.to_string(),
            zoom: None,
            scroll_css: None,
            pinned: false,
            group: None,
          }],
          tab_groups: Vec::new(),
          active_tab_index: 0,
          show_menu_bar: inherit_show_menu_bar.unwrap_or(!cfg!(target_os = "macos")),
          window_state: None,
        });

        let window_id = app.window.id();
        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<fastrender::ui::WorkerToUi>();
        let worker_wake_pending = Arc::new(AtomicBool::new(false));
        let worker_wake_counters = app
          .hud
          .as_ref()
          .map(|hud| Arc::clone(&hud.worker_wake_counters));
        let bridge_join = match std::thread::Builder::new()
          .name(format!("browser_worker_bridge_{window_id:?}"))
          .spawn({
            let event_loop_proxy = event_loop_proxy.clone();
            let worker_wake_pending = Arc::clone(&worker_wake_pending);
            let worker_wake_counters = worker_wake_counters.clone();
            move || {
              while let Ok(msg) = worker_to_ui_rx.recv() {
                if ui_tx.send(msg).is_err() {
                  break;
                }
                let send_wake = !worker_wake_pending.swap(true, Ordering::AcqRel);
                if let Some(counters) = worker_wake_counters.as_ref() {
                  if send_wake {
                    counters.wake_events_sent.fetch_add(1, Ordering::Relaxed);
                  } else {
                    counters
                      .wake_events_coalesced
                      .fetch_add(1, Ordering::Relaxed);
                  }
                }
                if send_wake {
                  let _ = event_loop_proxy.send_event(UserEvent::WorkerWake(window_id));
                }
              }
            }
          }) {
          Ok(join) => join,
          Err(err) => {
            eprintln!("failed to spawn new window bridge thread: {err}");
            return;
          }
        };

        app.window.request_redraw();
        windows.insert(
          window_id,
          BrowserWindow {
            app,
            ui_rx,
            worker_wake_pending,
            bridge_join: Some(bridge_join),
          },
        );
        window_order.push(window_id);
        active_window_id = Some(window_id);
        request_autosave(&windows, &window_order, active_window_id);
      }
      Event::UserEvent(UserEvent::RequestNewWindowWithSession {
        from_id,
        window: session_window,
      }) => {
        let inherit_size = windows.get(&from_id).map(|win| win.app.window.inner_size());
        let inherit_pos = windows.get(&from_id).and_then(|win| {
          win
            .app
            .window
            .outer_position()
            .ok()
            .map(|pos| PhysicalPosition::new(pos.x.saturating_add(32), pos.y.saturating_add(32)))
        });
        let window_state = session_window.window_state.clone();

        let window = match build_window(event_loop_target, window_state, inherit_size, inherit_pos)
        {
          Ok(window) => window,
          Err(err) => {
            eprintln!("failed to create new window: {err}");
            return;
          }
        };

        let worker_name = format!("fastr-browser-ui-worker-{next_window_index}");
        next_window_index = next_window_index.saturating_add(1);

        let (ui_to_worker_tx, worker_to_ui_rx, worker_join) =
          match fastrender::ui::spawn_browser_ui_worker(&worker_name) {
            Ok(v) => v,
            Err(err) => {
              eprintln!("failed to spawn browser worker for new window: {err}");
              return;
            }
          };

        if let Err(err) = ui_to_worker_tx.send(fastrender::ui::UiToWorker::SetDownloadDirectory {
          path: download_dir.clone(),
        }) {
          eprintln!("failed to send download dir to new window worker: {err}");
        }

        let mut app = match App::new(
          window,
          event_loop_target,
          event_loop_proxy.clone(),
          ui_to_worker_tx,
          worker_join,
          &gpu,
          browser_trace.clone(),
          appearance_env,
          applied_appearance.clone(),
          theme_accent,
          bookmarks_path.clone(),
          history_path.clone(),
          global_bookmarks.clone(),
          global_history.clone(),
        ) {
          Ok(app) => app,
          Err(err) => {
            eprintln!("failed to create new window app: {err}");
            return;
          }
        };
        app.profile_autosave_tx = profile_autosave_tx.clone();
        app.home_url = home_url.clone();
        app.browser_state.appearance = startup_appearance.clone();

        app.startup(session_window);
        // For a detached-tab window, match typical browser UX by keeping keyboard focus in the page
        // rather than stealing it for the address bar (unlike a fresh `about:newtab` window).
        app.page_has_focus = true;
        app.browser_state.chrome.request_focus_address_bar = false;
        app.browser_state.chrome.request_select_all_address_bar = false;
        app.browser_state.chrome.address_bar_editing = false;
        app.browser_state.chrome.address_bar_has_focus = false;

        let window_id = app.window.id();
        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<fastrender::ui::WorkerToUi>();
        let worker_wake_pending = Arc::new(AtomicBool::new(false));
        let worker_wake_counters = app
          .hud
          .as_ref()
          .map(|hud| Arc::clone(&hud.worker_wake_counters));
        let bridge_join = match std::thread::Builder::new()
          .name(format!("browser_worker_bridge_{window_id:?}"))
          .spawn({
            let event_loop_proxy = event_loop_proxy.clone();
            let worker_wake_pending = Arc::clone(&worker_wake_pending);
            let worker_wake_counters = worker_wake_counters.clone();
            move || {
              while let Ok(msg) = worker_to_ui_rx.recv() {
                if ui_tx.send(msg).is_err() {
                  break;
                }
                let send_wake = !worker_wake_pending.swap(true, Ordering::AcqRel);
                if let Some(counters) = worker_wake_counters.as_ref() {
                  if send_wake {
                    counters.wake_events_sent.fetch_add(1, Ordering::Relaxed);
                  } else {
                    counters
                      .wake_events_coalesced
                      .fetch_add(1, Ordering::Relaxed);
                  }
                }
                if send_wake {
                  let _ = event_loop_proxy.send_event(UserEvent::WorkerWake(window_id));
                }
              }
            }
          }) {
          Ok(join) => join,
          Err(err) => {
            eprintln!("failed to spawn new window bridge thread: {err}");
            return;
          }
        };

        app.window.request_redraw();
        windows.insert(
          window_id,
          BrowserWindow {
            app,
            ui_rx,
            worker_wake_pending,
            bridge_join: Some(bridge_join),
          },
        );
        window_order.push(window_id);
        active_window_id = Some(window_id);
        request_autosave(&windows, &window_order, active_window_id);
      }
      Event::RedrawRequested(window_id) => {
        let mut session_dirty = false;
        if let Some(win) = windows.get_mut(&window_id) {
          session_dirty = win.app.render_frame(control_flow);
        }
        if session_dirty {
          request_autosave(&windows, &window_order, active_window_id);
        }
      }
      Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
        // Used for UI animations that need to progress even when there is no input/worker activity.
        // Today this is used for overlay scrollbar fade-in/out.
        for win in windows.values_mut() {
          win.app.maybe_request_redraw_for_ui_timers();
        }
      }
      Event::NewEvents(StartCause::Init) => {
        // Mark the session as "running" as soon as the restored session state is in memory.
        //
        // This is the crash marker: if the process is terminated unexpectedly, `did_exit_cleanly`
        // remains false on disk and the next launch can restore + log.
        request_autosave(&windows, &window_order, active_window_id);

        // Ensure we draw at least one frame on startup.
        for win in windows.values() {
          win.app.window.request_redraw();
        }
      }
      Event::MainEventsCleared => {
        // Coalesce OS file drops: winit delivers one `DroppedFile` event per path.
        for win in windows.values_mut() {
          win.app.flush_pending_dropped_files();
        }
      }
      _ => {}
    }

    // Synchronize profile state (bookmarks/history) across all windows.
    let mut bookmarks_update: Option<(fastrender::ui::BookmarkStore, bool)> = None;
    let mut history_update: Option<(fastrender::ui::GlobalHistoryStore, bool)> = None;
    for win in windows.values_mut() {
      if win.app.profile_bookmarks_dirty {
        win.app.profile_bookmarks_dirty = false;
        let flush = win.app.profile_bookmarks_flush_requested;
        win.app.profile_bookmarks_flush_requested = false;
        bookmarks_update = Some((win.app.bookmarks.clone(), flush));
      }
      if win.app.profile_history_dirty {
        win.app.profile_history_dirty = false;
        let flush = win.app.profile_history_flush_requested;
        win.app.profile_history_flush_requested = false;
        history_update = Some((win.app.browser_state.history.clone(), flush));
      }
    }

    if let Some((new_bookmarks, flush)) = bookmarks_update {
      global_bookmarks = new_bookmarks;
      fastrender::ui::about_pages::sync_about_page_snapshot_bookmarks_from_bookmark_store(
        &global_bookmarks,
      );
      if let Some(tx) = profile_autosave_tx.as_ref() {
        let _ = tx.send(fastrender::ui::AutosaveMsg::UpdateBookmarks(
          global_bookmarks.clone(),
        ));
        if flush {
          let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
          let _ = tx.send(fastrender::ui::AutosaveMsg::Flush(done_tx));
          let _ = done_rx.recv_timeout(std::time::Duration::from_millis(200));
        }
      }
      for win in windows.values_mut() {
        win.app.bookmarks = global_bookmarks.clone();
        win.app.window.request_redraw();
      }
    }

    if let Some((new_history, flush)) = history_update {
      global_history = new_history;
      fastrender::ui::about_pages::sync_about_page_snapshot_history_from_global_history_store(
        &global_history,
      );
      if let Some(tx) = profile_autosave_tx.as_ref() {
        let _ = tx.send(fastrender::ui::AutosaveMsg::UpdateHistory(
          global_history.clone(),
        ));
        if flush {
          let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
          let _ = tx.send(fastrender::ui::AutosaveMsg::Flush(done_tx));
          let _ = done_rx.recv_timeout(std::time::Duration::from_millis(200));
        }
      }
      for win in windows.values_mut() {
        win.app.browser_state.history = global_history.clone();
        win.app.browser_state.visited.clear();
        win.app.browser_state.seed_visited_from_history();
        win.app.browser_state.chrome.omnibox.reset();
        win.app.window.request_redraw();
      }
    }

    if matches!(*control_flow, ControlFlow::Exit) {
      return;
    }

    // Drive periodic worker ticks for animated documents and keep the event loop armed for the next
    // pending deadline (worker ticks, viewport throttling, egui repaint scheduling).
    for win in windows.values_mut() {
      win
        .app
        .drive_periodic_tasks_and_update_control_flow(control_flow);
    }
  });
}

#[cfg(feature = "browser_ui")]
fn run_headless_vmjs_smoke_mode() -> Result<(), Box<dyn std::error::Error>> {
  use std::time::Duration;

  // Keep the smoke test cheap and deterministic. See `run_headless_smoke_mode` for rationale.
  const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";
  if !std::env::var_os(RAYON_NUM_THREADS_ENV).is_some_and(|value| !value.is_empty()) {
    // Avoid mutating process environment variables (the test harness may reuse this process for
    // other work). Instead, eagerly initialize Rayon's global pool with the desired thread count.
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build_global();
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
  download_dir: std::path::PathBuf,
  renderer_watchdog_timeout: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
  use fastrender::ui::cancel::CancelGens;
  use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
  use std::sync::mpsc::RecvTimeoutError;
  use std::time::Instant;

  let mut session = session.sanitized();
  let source_label = match source {
    StartupSessionSource::Restored => "restored",
    StartupSessionSource::CliUrl => "cli",
    StartupSessionSource::DefaultNewTab => "default",
    StartupSessionSource::HeadlessOverride => "override",
  };
  let session_json = serde_json::to_string(&session).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_SESSION source={source_label} {session_json}");

  let bookmarks_path = fastrender::ui::bookmarks_path();
  let history_path = fastrender::ui::history_path();

  const BOOKMARKS_OVERRIDE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE_BOOKMARKS_JSON";
  let (bookmarks_source, bookmarks_store) = match std::env::var(BOOKMARKS_OVERRIDE_ENV) {
    Ok(raw) if !raw.trim().is_empty() => {
      let store = fastrender::ui::parse_bookmarks_json(&raw)
        .map_err(|err| format!("{BOOKMARKS_OVERRIDE_ENV}: invalid JSON: {err}"))?;
      ("override", store)
    }
    _ => match fastrender::ui::load_bookmarks(&bookmarks_path) {
      Ok(outcome) => match outcome.source {
        fastrender::ui::LoadSource::Disk => ("disk", outcome.value),
        fastrender::ui::LoadSource::Empty => ("empty", outcome.value),
      },
      Err(err) => {
        eprintln!(
          "failed to load bookmarks from {}: {err}",
          bookmarks_path.display()
        );
        ("empty", fastrender::ui::BookmarkStore::default())
      }
    },
  };
  let bookmarks_json =
    serde_json::to_string(&bookmarks_store).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_BOOKMARKS source={bookmarks_source} {bookmarks_json}");

  const HISTORY_OVERRIDE_ENV: &str = "FASTR_TEST_BROWSER_HEADLESS_SMOKE_HISTORY_JSON";
  let (history_source, history_store) = match std::env::var(HISTORY_OVERRIDE_ENV) {
    Ok(raw) if !raw.trim().is_empty() => {
      let store = fastrender::ui::parse_history_json(&raw)
        .map_err(|err| format!("{HISTORY_OVERRIDE_ENV}: invalid JSON: {err}"))?;
      ("override", store)
    }
    _ => match fastrender::ui::load_history(&history_path) {
      Ok(outcome) => match outcome.source {
        fastrender::ui::LoadSource::Disk => ("disk", outcome.value),
        fastrender::ui::LoadSource::Empty => ("empty", outcome.value),
      },
      Err(err) => {
        eprintln!(
          "failed to load history from {}: {err}",
          history_path.display()
        );
        ("empty", fastrender::ui::GlobalHistoryStore::default())
      }
    },
  };
  let persisted_history = fastrender::ui::PersistedGlobalHistoryStore::from_store(&history_store);
  let history_json =
    serde_json::to_string(&persisted_history).unwrap_or_else(|_| "<invalid>".to_string());
  println!("HEADLESS_HISTORY source={history_source} {history_json}");

  // Keep the smoke test cheap and deterministic: when Rayon is allowed to auto-initialize its
  // global pool it may attempt to spawn one worker per detected CPU (which can be very large on
  // CI hosts). Explicitly pin the pool to a single thread unless the caller has overridden it.
  //
  // Note: this also avoids a rare `rayon-core` panic when multiple subsystems race to initialize
  // the global pool with different settings.
  const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";
  if !std::env::var_os(RAYON_NUM_THREADS_ENV).is_some_and(|value| !value.is_empty()) {
    // Avoid mutating process environment variables (the test harness may reuse this process for
    // other work). Instead, eagerly initialize Rayon's global pool with the desired thread count.
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build_global();
  }

  const VIEWPORT_CSS: (u32, u32) = (200, 120);
  // Use a DPR != 1.0 so the smoke test validates viewport↔device-pixel scaling.
  const DPR: f32 = 2.0;

  let expected_pixmap_w = ((VIEWPORT_CSS.0 as f32) * DPR).round().max(1.0) as u32;
  let expected_pixmap_h = ((VIEWPORT_CSS.1 as f32) * DPR).round().max(1.0) as u32;

  let active_window_idx = session.active_window_index;
  let active_window = &session.windows[active_window_idx];

  let (ui_to_worker_tx, worker_to_ui_rx, join) =
    fastrender::ui::spawn_browser_ui_worker("fastr-browser-headless-smoke-worker")?;

  ui_to_worker_tx.send(UiToWorker::SetDownloadDirectory { path: download_dir })?;

  let mut tab_ids = Vec::with_capacity(active_window.tabs.len());
  for _tab in &active_window.tabs {
    let tab_id = TabId::new();
    tab_ids.push(tab_id);
    ui_to_worker_tx.send(UiToWorker::CreateTab {
      tab_id,
      // Do not start navigation until after the headless harness has applied viewport/DPR. This
      // avoids a race where the worker begins rendering with its default (800x600, DPR=1) and only
      // later receives the `ViewportChanged` message, which can make the smoke test slow/flaky on
      // debug builds / constrained CI.
      initial_url: None,
      cancel: CancelGens::new(),
    })?;
  }

  let active_idx = active_window
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
  if let Some(tab) = active_window.tabs.get(active_idx) {
    ui_to_worker_tx.send(UiToWorker::Navigate {
      tab_id: active_tab_id,
      url: tab.url.clone(),
      reason: NavigationReason::TypedUrl,
    })?;
  }

  // Close the channel so the worker thread exits after completing the above messages.
  drop(ui_to_worker_tx);

  let mut smoke_summary: Option<(u32, u32, (u32, u32), f32)> = None;
  let mut last_frame_meta: Option<(u32, u32, (u32, u32), f32)> = None;
  let mut frames_seen: u32 = 0;

  match renderer_watchdog_timeout {
    Some(timeout) => {
      let deadline = Instant::now() + timeout;
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
    }
    None => loop {
      match worker_to_ui_rx.recv() {
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
        Err(_) => {
          return Err("headless smoke worker disconnected before FrameReady".into());
        }
      }
    },
  }

  let Some((pixmap_w, pixmap_h, viewport_css, dpr)) = smoke_summary else {
    let hint = match last_frame_meta {
      Some((w, h, viewport, dpr)) => format!(
        " (saw {frames_seen} FrameReady; last was viewport_css={viewport:?} dpr={dpr} pixmap_px={w}x{h})"
      ),
      None => " (saw no FrameReady messages)".to_string(),
    };
    return Err(
      match renderer_watchdog_timeout {
        Some(timeout) => format!(
          "timed out after {timeout:?} waiting for WorkerToUi::FrameReady matching viewport_css={VIEWPORT_CSS:?} dpr={DPR} pixmap_px={expected_pixmap_w}x{expected_pixmap_h}{hint}"
        ),
        None => format!(
          "headless smoke worker exited before producing a matching WorkerToUi::FrameReady (watchdog disabled){hint}"
        ),
      }
      .into(),
    );
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

  session.did_exit_cleanly = true;
  if let Err(err) = fastrender::ui::session::save_session_atomic(&session_path, &session) {
    eprintln!(
      "failed to save session to {}: {err}",
      session_path.display()
    );
  }

  if let Err(err) = fastrender::ui::save_bookmarks_atomic(&bookmarks_path, &bookmarks_store) {
    eprintln!(
      "failed to save bookmarks to {}: {err}",
      bookmarks_path.display()
    );
  }

  if let Err(err) = fastrender::ui::save_history_atomic(&history_path, &history_store) {
    eprintln!(
      "failed to save history to {}: {err}",
      history_path.display()
    );
  }

  let active_url = active_window
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
fn run_headless_crash_smoke_mode(
  download_dir: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
  use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
  use fastrender::debug::runtime::RuntimeToggles;
  use fastrender::text::font_db::FontConfig;
  use fastrender::ui::cancel::CancelGens;
  use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
  use std::sync::mpsc::RecvTimeoutError;
  use std::time::{Duration, Instant};
  use std::collections::HashMap;

  // Keep the smoke test cheap and deterministic. See `run_headless_smoke_mode` for rationale.
  const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";
  if !std::env::var_os(RAYON_NUM_THREADS_ENV).is_some_and(|value| !value.is_empty()) {
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(1)
      .build_global();
  }

  // Bounded but generous: under debug builds / CI contention the worker may take a while to spin up
  // before hitting the crash URL.
  const TIMEOUT: Duration = Duration::from_secs(60);
  const VIEWPORT_CSS: (u32, u32) = (64, 64);
  const DPR: f32 = 1.0;

  let crash_url = "crash://panic".to_string();

  // Enable crash URLs explicitly for this smoke-test mode.
  let factory = {
    let mut raw = std::env::vars()
      .filter(|(k, _)| k.starts_with("FASTR_"))
      .collect::<HashMap<_, _>>();
    raw.insert("FASTR_ENABLE_CRASH_URLS".to_string(), "1".to_string());
    let toggles = RuntimeToggles::from_map(raw);

    let renderer_config = FastRenderConfig::default()
      .with_font_sources(FontConfig::bundled_only())
      .with_runtime_toggles(toggles);
    FastRenderFactory::with_config(
      FastRenderPoolConfig::new().with_renderer_config(renderer_config),
    )?
  };

  let (ui_to_worker_tx, worker_to_ui_rx, join) =
    fastrender::ui::spawn_ui_worker_with_factory("fastr-browser-headless-crash-smoke-worker", factory)?
      .split();

  // Not strictly needed for the crash smoke test, but keeps parity with other headless harnesses.
  ui_to_worker_tx.send(UiToWorker::SetDownloadDirectory { path: download_dir })?;

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
  ui_to_worker_tx.send(UiToWorker::SetActiveTab { tab_id })?;
  ui_to_worker_tx.send(UiToWorker::Navigate {
    tab_id,
    url: crash_url.clone(),
    reason: NavigationReason::TypedUrl,
  })?;

  let deadline = Instant::now() + TIMEOUT;
  let mut last_msg_debug: Option<String> = None;

  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker_to_ui_rx.recv_timeout(remaining) {
      Ok(msg) => {
        // If the worker stays alive long enough to report a navigation failure, that's a strong
        // signal that the crash URL isn't wired up (or the crash did not trigger).
        if let WorkerToUi::NavigationFailed { url, error, .. } = &msg {
          if url == &crash_url {
            return Err(
              format!(
                "headless crash smoke: worker reported NavigationFailed for {crash_url:?}: {error}"
              )
              .into(),
            );
          }
        }
        last_msg_debug = Some(format!("{msg:?}"));
      }
      Err(RecvTimeoutError::Disconnected) => break,
      Err(RecvTimeoutError::Timeout) => {
        let hint = match last_msg_debug.as_deref() {
          Some(msg) => format!(" (last WorkerToUi message: {msg})"),
          None => " (received no WorkerToUi messages)".to_string(),
        };
        return Err(format!(
          "timed out after {TIMEOUT:?} waiting for UI worker to crash/disconnect after navigating to {crash_url:?}{hint}"
        )
        .into());
      }
    }
  }

  // Best-effort: join the worker thread so the smoke test doesn't race process teardown.
  let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
  let _ = std::thread::spawn(move || {
    let _ = done_tx.send(join.join());
  });
  match done_rx.recv_timeout(Duration::from_secs(10)) {
    Ok(Ok(())) => {
      return Err("expected crash smoke worker thread to panic, but it exited cleanly".into());
    }
    Ok(Err(_)) => {}
    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
      return Err("timed out waiting for crash smoke worker thread to terminate".into());
    }
    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
      return Err("crash smoke worker join helper thread disconnected".into());
    }
  }

  println!("HEADLESS_CRASH_SMOKE_OK");
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
fn resize_dpr_scale_from_env() -> f32 {
  match parse_resize_dpr_scale_env(std::env::var(ENV_BROWSER_RESIZE_DPR_SCALE).ok().as_deref()) {
    Ok(value) => value,
    Err(err) => {
      eprintln!("{ENV_BROWSER_RESIZE_DPR_SCALE}: {err}");
      parse_resize_dpr_scale_env(None).unwrap_or(0.5)
    }
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone)]
struct OpenSelectDropdown {
  tab_id: fastrender::ui::TabId,
  select_node_id: usize,
  control: fastrender::tree::box_tree::SelectControl,
  /// egui widget id that held focus before the dropdown was opened.
  ///
  /// Used to restore focus when the popup closes (or clear focus when none was set) so we don't
  /// leave egui focused on widgets that have been removed.
  focus_before_open: Option<egui::Id>,
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
struct OpenDateTimePicker {
  tab_id: fastrender::ui::TabId,
  input_node_id: usize,
  kind: fastrender::ui::messages::DateTimeInputKind,
  /// egui widget id that held focus before the picker was opened.
  ///
  /// Used to restore focus when the popup closes (or clear focus when none was set) so we don't
  /// leave egui focused on widgets that have been removed.
  focus_before_open: Option<egui::Id>,
  /// Bounding box of the `<input>` control in viewport CSS coordinates.
  anchor_css: fastrender::geometry::Rect,
  /// Fallback anchor position in egui points (cursor position).
  anchor_points: egui::Pos2,
  /// Draft value string shown in the popup.
  value: String,
}

#[cfg(feature = "browser_ui")]
fn egui_focused_widget_id(ctx: &egui::Context) -> Option<egui::Id> {
  ctx.memory(|mem| mem.focus())
}

#[cfg(feature = "browser_ui")]
fn restore_or_clear_egui_focus(ctx: &egui::Context, previous_focus: Option<egui::Id>) {
  ctx.memory_mut(|mem| {
    if let Some(prev) = previous_focus {
      mem.request_focus(prev);
      return;
    }
    let Some(focused) = mem.focus() else {
      return;
    };
    mem.surrender_focus(focused);
  });
}

#[cfg(feature = "browser_ui")]
fn close_select_dropdown_popup(
  ctx: &egui::Context,
  open_select_dropdown: &mut Option<OpenSelectDropdown>,
  open_select_dropdown_rect: &mut Option<egui::Rect>,
) {
  let Some(dropdown) = open_select_dropdown.take() else {
    *open_select_dropdown_rect = None;
    return;
  };
  *open_select_dropdown_rect = None;
  restore_or_clear_egui_focus(ctx, dropdown.focus_before_open);
}

#[cfg(feature = "browser_ui")]
fn close_date_time_picker_popup(
  ctx: &egui::Context,
  open_date_time_picker: &mut Option<OpenDateTimePicker>,
  open_date_time_picker_rect: &mut Option<egui::Rect>,
) {
  let Some(picker) = open_date_time_picker.take() else {
    *open_date_time_picker_rect = None;
    return;
  };
  *open_date_time_picker_rect = None;
  restore_or_clear_egui_focus(ctx, picker.focus_before_open);
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone)]
struct OpenFilePicker {
  tab_id: fastrender::ui::TabId,
  input_node_id: usize,
  multiple: bool,
  accept: Option<String>,
  /// Bounding box of the `<input>` control in viewport CSS coordinates.
  anchor_css: fastrender::geometry::Rect,
  /// Fallback anchor position in egui points (cursor position).
  anchor_points: egui::Pos2,
  /// Draft path input shown in the popup.
  draft: String,
}

#[cfg(feature = "browser_ui")]
fn rfd_extensions_from_html_accept(accept: Option<&str>) -> Vec<String> {
  let Some(accept) = accept.map(str::trim).filter(|s| !s.is_empty()) else {
    return Vec::new();
  };

  let mut exts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

  let mut push_all = |values: &[&str]| {
    for ext in values {
      let ext = ext.trim().trim_start_matches('.');
      if ext.is_empty() {
        continue;
      }
      exts.insert(ext.to_ascii_lowercase());
    }
  };

  for raw_token in accept.split(',') {
    let token = raw_token.trim();
    if token.is_empty() {
      continue;
    }

    let token = token.to_ascii_lowercase();
    if token == "*/*" {
      // Explicitly accept anything; no filter.
      continue;
    }

    if let Some(ext) = token.strip_prefix('.') {
      let ext = ext.trim();
      if ext.is_empty() {
        continue;
      }
      exts.insert(ext.to_string());
      continue;
    }

    match token.as_str() {
      // Wildcard MIME groups.
      "image/*" => push_all(&["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg"]),
      "text/*" => push_all(&["txt", "md", "markdown", "csv", "json", "xml", "html", "htm"]),
      "audio/*" => push_all(&["mp3", "wav", "ogg", "flac", "m4a", "aac"]),
      "video/*" => push_all(&["mp4", "mov", "webm", "mkv", "avi", "mpeg", "mpg"]),

      // Common specific MIME types.
      "image/png" => push_all(&["png"]),
      "image/jpeg" | "image/jpg" => push_all(&["jpg", "jpeg"]),
      "image/gif" => push_all(&["gif"]),
      "image/webp" => push_all(&["webp"]),
      "image/bmp" => push_all(&["bmp"]),
      "image/svg+xml" => push_all(&["svg"]),
      "text/plain" => push_all(&["txt"]),
      "text/markdown" => push_all(&["md", "markdown"]),
      "text/csv" => push_all(&["csv"]),
      "text/html" => push_all(&["html", "htm"]),
      "application/json" => push_all(&["json"]),
      "application/pdf" => push_all(&["pdf"]),
      _ => {}
    }
  }

  exts.into_iter().collect()
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
  image_url: Option<String>,
  can_copy: bool,
  can_cut: bool,
  can_paste: bool,
  can_select_all: bool,
  selected_idx: usize,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug)]
enum KeyboardPageContextMenuOpenPlan {
  WorkerRequest {
    pending: PendingContextMenuRequest,
    msg: fastrender::ui::UiToWorker,
  },
  /// Fallback when the point->CSS mapping isn't available (or fails due to a degenerate render
  /// rect). In this mode we still open the context menu, but only with page-level entries (no
  /// hit-tested link/image/selection actions).
  OpenUiOnly(OpenContextMenu),
}

#[cfg(feature = "browser_ui")]
fn is_page_context_menu_keyboard_gesture(
  key: winit::event::VirtualKeyCode,
  modifiers: winit::event::ModifiersState,
) -> bool {
  use winit::event::VirtualKeyCode;

  // Standard "open context menu" gestures:
  // - Shift+F10
  // - Menu/Apps key
  //
  // Only handle these when they are not combined with browser/chrome command modifiers.
  let has_command_modifiers = modifiers.ctrl() || modifiers.alt() || modifiers.logo();
  match key {
    VirtualKeyCode::F10 => modifiers.shift() && !has_command_modifiers,
    VirtualKeyCode::Apps => !has_command_modifiers,
    _ => false,
  }
}

#[cfg(feature = "browser_ui")]
fn page_context_menu_anchor_points_for_keyboard_open(
  page_rect_points: Option<egui::Rect>,
  content_rect_points: Option<egui::Rect>,
  last_cursor_pos_points: Option<egui::Pos2>,
) -> egui::Pos2 {
  // Prefer opening at the last known cursor position when it is within the page bounds.
  if let (Some(page_rect), Some(cursor)) = (page_rect_points, last_cursor_pos_points) {
    if page_rect.contains(cursor) {
      return cursor;
    }
  }

  // Deterministic fallback: center of the rendered page region.
  if let Some(page_rect) = page_rect_points {
    return page_rect.center();
  }

  // If we don't yet know the page rect, fall back to something stable in the window.
  if let Some(content_rect) = content_rect_points {
    return content_rect.center();
  }

  last_cursor_pos_points.unwrap_or_else(|| egui::pos2(0.0, 0.0))
}

#[cfg(feature = "browser_ui")]
fn keyboard_page_context_menu_request(
  key: winit::event::VirtualKeyCode,
  modifiers: winit::event::ModifiersState,
  page_has_focus: bool,
  chrome_has_text_focus: bool,
  page_loading_overlay_blocks_input: bool,
  tab_id: fastrender::ui::TabId,
  page_rect_points: Option<egui::Rect>,
  content_rect_points: Option<egui::Rect>,
  mapping: Option<fastrender::ui::InputMapping>,
  last_cursor_pos_points: Option<egui::Pos2>,
) -> Option<KeyboardPageContextMenuOpenPlan> {
  if !is_page_context_menu_keyboard_gesture(key, modifiers) {
    return None;
  }

  if !should_open_page_context_menu_from_keyboard(
    page_has_focus,
    chrome_has_text_focus,
    page_loading_overlay_blocks_input,
  ) {
    return None;
  }

  let anchor_points = page_context_menu_anchor_points_for_keyboard_open(
    page_rect_points,
    content_rect_points,
    last_cursor_pos_points,
  );

  if let Some(pos_css) = mapping.and_then(|mapping| mapping.pos_points_to_pos_css_clamped(anchor_points))
  {
    let pending = PendingContextMenuRequest {
      tab_id,
      pos_css,
      anchor_points,
    };
    let msg = fastrender::ui::UiToWorker::ContextMenuRequest { tab_id, pos_css };
    return Some(KeyboardPageContextMenuOpenPlan::WorkerRequest { pending, msg });
  }

  Some(KeyboardPageContextMenuOpenPlan::OpenUiOnly(OpenContextMenu {
    tab_id,
    pos_css: (-1.0, -1.0),
    anchor_points,
    link_url: None,
    image_url: None,
    can_copy: false,
    can_cut: false,
    can_paste: false,
    can_select_all: false,
    selected_idx: 0,
  }))
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
struct PointerClickSequence {
  last_pos_points: egui::Pos2,
  last_instant: std::time::Instant,
  click_count: u8,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone, Copy, PartialEq)]
enum TouchGestureAction {
  /// A long-press gesture was recognised and should open a page context menu.
  ContextMenu { pos_points: (f32, f32) },
  /// A tap gesture was recognised and should be treated as a primary click.
  Tap { pos_points: (f32, f32) },
}

/// Minimal touch gesture recogniser used to emulate mouse interactions on touch devices.
///
/// This is intentionally self-contained (no winit/egui types) so it can be unit tested without the
/// heavy `browser_ui` feature.
#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone)]
struct TouchGestureRecognizer {
  active: Option<ActiveTouchGesture>,
  long_press_threshold: std::time::Duration,
  slop_radius_points: f32,
}

#[cfg(any(test, feature = "browser_ui"))]
#[derive(Debug, Clone)]
struct ActiveTouchGesture {
  id: u64,
  start_pos_points: (f32, f32),
  last_pos_points: (f32, f32),
  start_instant: std::time::Instant,
  long_press_deadline: std::time::Instant,
  moved_too_far: bool,
  long_press_triggered: bool,
}

#[cfg(any(test, feature = "browser_ui"))]
impl TouchGestureRecognizer {
  const DEFAULT_LONG_PRESS_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(500);
  const DEFAULT_SLOP_RADIUS_POINTS: f32 = 8.0;

  fn new() -> Self {
    Self::new_with_config(Self::DEFAULT_LONG_PRESS_THRESHOLD, Self::DEFAULT_SLOP_RADIUS_POINTS)
  }

  fn new_with_config(long_press_threshold: std::time::Duration, slop_radius_points: f32) -> Self {
    Self {
      active: None,
      long_press_threshold,
      slop_radius_points: slop_radius_points.max(0.0),
    }
  }

  fn reset(&mut self) {
    self.active = None;
  }

  fn next_deadline(&self) -> Option<std::time::Instant> {
    self.active.as_ref().and_then(|gesture| {
      if gesture.moved_too_far || gesture.long_press_triggered {
        None
      } else {
        Some(gesture.long_press_deadline)
      }
    })
  }

  fn touch_start(&mut self, id: u64, now: std::time::Instant, pos_points: (f32, f32)) {
    // Multi-touch is treated as a cancellation of the primary-touch gesture so we don't trigger
    // context menus during pinch/zoom interactions.
    if self.active.is_some() {
      self.active = None;
    }
    self.active = Some(ActiveTouchGesture {
      id,
      start_pos_points: pos_points,
      last_pos_points: pos_points,
      start_instant: now,
      long_press_deadline: now + self.long_press_threshold,
      moved_too_far: false,
      long_press_triggered: false,
    });
  }

  fn touch_move(&mut self, id: u64, pos_points: (f32, f32)) {
    let Some(active) = self.active.as_mut() else {
      return;
    };
    if active.id != id {
      return;
    }

    active.last_pos_points = pos_points;
    if active.moved_too_far || active.long_press_triggered {
      return;
    }

    let dx = pos_points.0 - active.start_pos_points.0;
    let dy = pos_points.1 - active.start_pos_points.1;
    let dist2 = dx * dx + dy * dy;
    let max_dist2 = self.slop_radius_points * self.slop_radius_points;
    if dist2 > max_dist2 {
      active.moved_too_far = true;
    }
  }

  fn touch_end(
    &mut self,
    id: u64,
    now: std::time::Instant,
    pos_points: (f32, f32),
  ) -> Option<TouchGestureAction> {
    let Some(active) = self.active.take() else {
      return None;
    };
    if active.id != id {
      // Different touch ended; keep the original gesture (best-effort) by restoring it.
      self.active = Some(active);
      return None;
    }

    if active.moved_too_far {
      return None;
    }

    if active.long_press_triggered {
      // Long-press was already emitted via `tick`; do not re-emit on release.
      return None;
    }

    if now >= active.long_press_deadline {
      // Treat a release after the deadline as a long-press even if the periodic tick didn't fire
      // (e.g. event-loop scheduling delays).
      return Some(TouchGestureAction::ContextMenu { pos_points });
    }

    Some(TouchGestureAction::Tap { pos_points })
  }

  fn touch_cancel(&mut self, id: u64) {
    if self.active.as_ref().is_some_and(|active| active.id == id) {
      self.active = None;
    }
  }

  fn tick(&mut self, now: std::time::Instant) -> Option<TouchGestureAction> {
    let Some(active) = self.active.as_mut() else {
      return None;
    };
    if active.moved_too_far || active.long_press_triggered {
      return None;
    }
    if now < active.long_press_deadline {
      return None;
    }

    active.long_press_triggered = true;
    Some(TouchGestureAction::ContextMenu {
      pos_points: active.last_pos_points,
    })
  }
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
struct WorkerWakeHudCounters {
  wake_events_sent: std::sync::atomic::AtomicU64,
  wake_events_coalesced: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "browser_ui")]
impl Default for WorkerWakeHudCounters {
  fn default() -> Self {
    Self {
      wake_events_sent: std::sync::atomic::AtomicU64::new(0),
      wake_events_coalesced: std::sync::atomic::AtomicU64::new(0),
    }
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug)]
struct BrowserHud {
  last_frame_start: Option<std::time::Instant>,
  last_frame_cpu_ms: Option<f32>,
  fps: Option<f32>,
  worker_wake_counters: std::sync::Arc<WorkerWakeHudCounters>,
  worker_wake_coalesced_prev: u64,
  worker_wake_sent_prev: u64,
  worker_stats_sample_start: std::time::Instant,
  worker_msgs_since_sample: u64,
  worker_wakes_since_sample: u64,
  worker_empty_wakes_since_sample: u64,
  worker_max_drain_since_sample: u64,
  worker_drained_total: u64,
  worker_forwarded_msgs_per_sec: Option<f32>,
  worker_msgs_per_sec: Option<f32>,
  worker_wakes_per_sec: Option<f32>,
  worker_wake_sent_per_sec: Option<f32>,
  worker_empty_wakes_per_sec: Option<f32>,
  worker_coalesced_wakes_per_sec: Option<f32>,
  worker_pending_msgs_estimate: Option<u64>,
  worker_msgs_per_nonempty_wake: Option<f32>,
  worker_last_drain: u64,
  worker_max_drain_recent: u64,
  surface_configure_calls_total: u64,
  page_texture_recreates_total: u64,
  last_upload_ms: f32,
  last_upload_bytes: u64,
  uploads_this_frame: u32,
  text_buf: String,
}

#[cfg(feature = "browser_ui")]
impl BrowserHud {
  fn new() -> Self {
    Self {
      last_frame_start: None,
      last_frame_cpu_ms: None,
      fps: None,
      worker_wake_counters: std::sync::Arc::new(WorkerWakeHudCounters::default()),
      worker_wake_coalesced_prev: 0,
      worker_wake_sent_prev: 0,
      worker_stats_sample_start: std::time::Instant::now(),
      worker_msgs_since_sample: 0,
      worker_wakes_since_sample: 0,
      worker_empty_wakes_since_sample: 0,
      worker_max_drain_since_sample: 0,
      worker_drained_total: 0,
      worker_forwarded_msgs_per_sec: None,
      worker_msgs_per_sec: None,
      worker_wakes_per_sec: None,
      worker_wake_sent_per_sec: None,
      worker_empty_wakes_per_sec: None,
      worker_coalesced_wakes_per_sec: None,
      worker_pending_msgs_estimate: None,
      worker_msgs_per_nonempty_wake: None,
      worker_last_drain: 0,
      worker_max_drain_recent: 0,
      surface_configure_calls_total: 0,
      page_texture_recreates_total: 0,
      last_upload_ms: 0.0,
      last_upload_bytes: 0,
      uploads_this_frame: 0,
      text_buf: String::with_capacity(384),
    }
  }

  fn record_worker_wake(&mut self, drained_msgs: u64) {
    self.worker_wakes_since_sample = self.worker_wakes_since_sample.saturating_add(1);
    self.worker_msgs_since_sample = self.worker_msgs_since_sample.saturating_add(drained_msgs);
    self.worker_drained_total = self.worker_drained_total.saturating_add(drained_msgs);
    self.worker_last_drain = drained_msgs;
    self.worker_max_drain_since_sample = self.worker_max_drain_since_sample.max(drained_msgs);
    if drained_msgs == 0 {
      self.worker_empty_wakes_since_sample = self.worker_empty_wakes_since_sample.saturating_add(1);
    }
  }

  fn maybe_sample_worker_rates(&mut self, now: std::time::Instant) {
    let coalesced_total = self
      .worker_wake_counters
      .wake_events_coalesced
      .load(std::sync::atomic::Ordering::Relaxed);
    let sent_total = self
      .worker_wake_counters
      .wake_events_sent
      .load(std::sync::atomic::Ordering::Relaxed);
    let forwarded_total = coalesced_total.saturating_add(sent_total);
    self.worker_pending_msgs_estimate =
      Some(forwarded_total.saturating_sub(self.worker_drained_total));

    const MIN_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
    let dt = now.saturating_duration_since(self.worker_stats_sample_start);
    if dt < MIN_SAMPLE_INTERVAL {
      return;
    }
    let secs = dt.as_secs_f32();
    if !secs.is_finite() || secs <= 0.0 {
      return;
    }

    let msgs = self.worker_msgs_since_sample;
    let wakes = self.worker_wakes_since_sample;
    let empty = self.worker_empty_wakes_since_sample;
    let nonempty_wakes = wakes.saturating_sub(empty);
    let coalesced_delta = coalesced_total.saturating_sub(self.worker_wake_coalesced_prev);
    let sent_delta = sent_total.saturating_sub(self.worker_wake_sent_prev);
    let forwarded_delta = coalesced_delta.saturating_add(sent_delta);

    self.worker_forwarded_msgs_per_sec = Some((forwarded_delta as f32 / secs).min(10_000_000.0));
    self.worker_msgs_per_sec = Some((msgs as f32 / secs).min(10_000_000.0));
    self.worker_wakes_per_sec = Some((wakes as f32 / secs).min(10_000_000.0));
    self.worker_wake_sent_per_sec = Some((sent_delta as f32 / secs).min(10_000_000.0));
    self.worker_empty_wakes_per_sec = Some((empty as f32 / secs).min(10_000_000.0));
    self.worker_coalesced_wakes_per_sec = Some((coalesced_delta as f32 / secs).min(10_000_000.0));
    self.worker_msgs_per_nonempty_wake = if nonempty_wakes > 0 {
      Some((msgs as f32 / nonempty_wakes as f32).min(10_000_000.0))
    } else {
      None
    };
    self.worker_max_drain_recent = self.worker_max_drain_since_sample;
    self.worker_wake_coalesced_prev = coalesced_total;
    self.worker_wake_sent_prev = sent_total;

    self.worker_stats_sample_start = now;
    self.worker_msgs_since_sample = 0;
    self.worker_wakes_since_sample = 0;
    self.worker_empty_wakes_since_sample = 0;
    self.worker_max_drain_since_sample = 0;
  }
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Default)]
struct TabNotificationUiState {
  warning_toast: fastrender::ui::WarningToastState,
  warning_toast_expanded: bool,
  last_warning_toast: Option<String>,
  last_error: Option<String>,
  error_details_open: bool,
}

#[cfg(feature = "browser_ui")]
impl TabNotificationUiState {
  fn sync_error(&mut self, error: Option<&str>) {
    let error = error.map(str::trim).filter(|s| !s.is_empty());
    if let Some(error) = error {
      let error = error.to_string();
      if Some(&error) != self.last_error.as_ref() {
        self.last_error = Some(error);
        self.error_details_open = false;
      }
    } else {
      // Error no longer present: collapse details, but keep the last message so the infobar can
      // fade out gracefully.
      self.error_details_open = false;
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
          wgpu_init.backends, wgpu_init.power_preference, wgpu_init.force_fallback_adapter,
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
struct ClosingTabFavicon {
  tab_id: fastrender::ui::TabId,
  expires_at: std::time::Instant,
  texture: fastrender::ui::WgpuPixmapTexture,
}

#[cfg(feature = "browser_ui")]
struct ChromeAccessKit {
  adapter: accesskit_winit::Adapter,
}

#[cfg(feature = "browser_ui")]
impl ChromeAccessKit {
  fn new(
    window: &winit::window::Window,
    event_loop_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
  ) -> Self {
    let adapter = accesskit_winit::Adapter::new(
      window,
      || {
        use std::num::NonZeroU128;

        // Initial tree shown when accessibility is first activated. The chrome tree is updated
        // continuously during rendering via `Adapter::update_if_active`.
        let mut classes = accesskit::NodeClassSet::new();
        let root_id = accesskit::NodeId(NonZeroU128::new(1).unwrap());
        let mut root = accesskit::NodeBuilder::new(accesskit::Role::Window);
        root.set_name("FastRender".to_string());
        accesskit::TreeUpdate {
          nodes: vec![(root_id, root.build(&mut classes))],
          tree: Some(accesskit::Tree::new(root_id)),
          focus: Some(root_id),
        }
      },
      event_loop_proxy,
    );

    Self {
      adapter,
    }
  }

  fn process_window_event(
    &mut self,
    window: &winit::window::Window,
    event: &winit::event::WindowEvent<'_>,
  ) {
    let _ = self.adapter.on_event(window, event);
  }

  fn is_active(&self) -> bool {
    self.adapter.is_active()
  }

  fn update_if_active(&self, updater: impl FnOnce() -> accesskit::TreeUpdate) {
    if !self.is_active() {
      return;
    }
    self.adapter.update_if_active(updater);
  }
}

#[cfg(feature = "browser_ui")]
enum ChromeA11yBackend {
  /// Delegate accessibility to egui/egui-winit's AccessKit integration (current default).
  Egui,
  /// Provide an AccessKit tree driven by a FastRender-rendered HTML/CSS chrome document.
  FastRender(ChromeAccessKit),
}

#[cfg(feature = "browser_ui")]
struct App {
  window: winit::window::Window,
  window_title_cache: String,
  event_loop_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
  chrome_a11y: ChromeA11yBackend,
  trace: fastrender::debug::trace::TraceHandle,

  surface: wgpu::Surface,
  device: std::sync::Arc<wgpu::Device>,
  queue: std::sync::Arc<wgpu::Queue>,
  surface_config: wgpu::SurfaceConfiguration,
  surface_needs_configure: bool,
  surface_configure_count: u64,
  log_surface_configure: bool,

  egui_ctx: egui::Context,
  egui_state: egui_winit::State,
  egui_renderer: egui_wgpu::Renderer,
  /// Window/system scale factor (physical pixels per egui point) reported by winit.
  ///
  /// This is *not* affected by the user-configurable UI scale multiplier.
  system_pixels_per_point: f32,
  /// User-configurable UI scale multiplier (separate from per-tab page zoom).
  ui_scale: f32,
  /// Effective egui pixels-per-point used for chrome rendering: `system_pixels_per_point * ui_scale`.
  pixels_per_point: f32,
  browser_limits: fastrender::ui::browser_limits::BrowserLimits,
  page_texture_filter_policy: PageTextureFilterPolicy,
  appearance_env_overrides: fastrender::ui::appearance::AppearanceEnvOverrides,
  applied_appearance: fastrender::ui::appearance::AppearanceSettings,
  theme_override: Option<fastrender::ui::theme::ThemeMode>,
  theme_accent: Option<egui::Color32>,
  theme: fastrender::ui::theme::BrowserTheme,
  clear_color: wgpu::Color,

  ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
  worker_join: Option<std::thread::JoinHandle<()>>,
  browser_state: fastrender::ui::BrowserAppState,
  /// Configured home page URL (default: `about:newtab`).
  home_url: String,
  search_suggest: fastrender::ui::SearchSuggestService,

  bookmarks_path: std::path::PathBuf,
  history_path: std::path::PathBuf,
  bookmarks: fastrender::ui::BookmarkStore,
  profile_autosave_tx: Option<std::sync::mpsc::Sender<fastrender::ui::AutosaveMsg>>,
  profile_bookmarks_dirty: bool,
  profile_bookmarks_flush_requested: bool,
  profile_history_dirty: bool,
  profile_history_flush_requested: bool,
  history_panel_open: bool,
  history_panel_request_focus_search: bool,
  bookmarks_panel_open: bool,
  downloads_panel_open: bool,
  downloads_panel_request_focus: bool,
  clear_browsing_data_dialog_open: bool,
  clear_browsing_data_dialog_request_focus: bool,
  bookmarks_manager: fastrender::ui::bookmarks_manager::BookmarksManagerState,
  clear_browsing_data_range: fastrender::ui::ClearBrowsingDataRange,

  tab_textures: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,
  tab_favicons: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::WgpuPixmapTexture>,
  /// Recently closed tab favicons that are kept alive briefly so tab-close animations can render a
  /// "ghost" closing tab with its original favicon.
  closing_tab_favicons: std::collections::VecDeque<ClosingTabFavicon>,
  tab_cancel: std::collections::HashMap<fastrender::ui::TabId, fastrender::ui::cancel::CancelGens>,
  /// Pending session scroll restores keyed by tab id.
  ///
  /// When restoring a session we only know the persisted scroll offset; applying it must wait until
  /// the tab has a known viewport (after the first `ViewportChanged` for this window/tab).
  pending_scroll_restores: std::collections::HashMap<fastrender::ui::TabId, (f32, f32)>,
  /// Pending `FrameReady` pixmaps coalesced until the next window redraw.
  ///
  /// Uploading a pixmap into a wgpu texture is expensive; the UI worker can produce multiple frames
  /// before the windowed UI draws again. We store at most one pending frame per tab and only upload
  /// for the active tab.
  pending_frame_uploads: fastrender::ui::FrameUploadCoalescer,
  /// Upper bound (in bytes) on `pending_frame_uploads` pixmaps that may be retained in RAM.
  max_pending_frame_bytes: u64,
  /// Timestamp of the most recent page pixmap upload to the GPU.
  ///
  /// Used to rate-limit `queue.write_texture` during interactive resize; when we skip an upload we
  /// keep the latest pixmap pending and rely on scaling the already-uploaded texture.
  last_page_upload_at: Option<std::time::Instant>,
  /// When to request the next redraw to retry a rate-limited page upload.
  next_page_upload_redraw: Option<std::time::Instant>,

  /// Rect of the central content panel (in egui points) from the last painted frame.
  ///
  /// Used to position transient overlays (toasts/infobars) without affecting layout.
  content_rect_points: Option<egui::Rect>,
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
  /// Screen-space rect for the active tab's "page unresponsive" overlay, if visible.
  page_unresponsive_overlay_rect: Option<egui::Rect>,
  overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars,
  overlay_scrollbar_visibility: fastrender::ui::scrollbars::OverlayScrollbarVisibilityState,
  scrollbar_drag: Option<ScrollbarDrag>,
  /// Pending scroll delta produced by a scrollbar thumb drag.
  ///
  /// CursorMoved events can arrive at very high frequency; we coalesce them so the UI worker sees
  /// at most one `UiToWorker::Scroll` per rendered frame.
  pending_scroll_drag: Option<PendingScrollDrag>,
  viewport_cache_tab: Option<fastrender::ui::TabId>,
  viewport_cache_css: (u32, u32),
  viewport_cache_dpr: f32,
  viewport_throttle: fastrender::ui::ViewportThrottle,
  viewport_throttle_resizing: fastrender::ui::ViewportThrottle,
  viewport_throttle_tab: Option<fastrender::ui::TabId>,
  last_resize_event_at: Option<std::time::Instant>,
  resize_burst_active: bool,
  resize_dpr_scale: f32,
  modifiers: winit::event::ModifiersState,
  /// Whether the current frame should ignore `egui::Event::Paste` events.
  ///
  /// We handle Ctrl/Cmd+V ourselves (reading the OS clipboard and sending `UiToWorker::Paste`) when
  /// the rendered page has focus. On some platforms/egui versions, egui-winit may still emit a
  /// `Paste` event for the same keypress; this flag avoids double-pasting.
  suppress_paste_events: bool,
  /// Scratch buffer for extracting `egui::Event::MouseWheel` payloads from `RawInput`.
  ///
  /// PERF: `RawInput::events` is scanned every frame; reusing this buffer avoids per-frame `Vec`
  /// allocations for the common "no wheel events" case.
  wheel_events_buf: Vec<(egui::MouseWheelUnit, egui::Vec2)>,
  /// Scratch buffer for extracting `egui::Event::Paste` payloads from `RawInput`.
  ///
  /// PERF: Reused across frames so that frames without paste input don't allocate.
  paste_events_buf: Vec<String>,
  /// Enable lightweight perf logging controlled by `FASTR_PERF_LOG=1`.
  perf_log_enabled: bool,

  window_focused: bool,
  window_occluded: bool,
  window_minimized: bool,
  window_fullscreen: bool,
 
  page_has_focus: bool,
  /// Whether a browser-chrome (non-page) text input currently owns keyboard input.
  ///
  /// This is intentionally backend-agnostic: the egui backend updates it from widget focus state,
  /// while a compositor backend can set it from chrome DOM focus without needing an `egui::Context`.
  chrome_has_text_focus: bool,
  pointer_captured: bool,
  captured_button: fastrender::ui::PointerButton,
  primary_click_sequence: Option<PointerClickSequence>,
  touch_gesture: TouchGestureRecognizer,
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

  /// OS file-drop paths received from winit (`WindowEvent::DroppedFile`) that have not yet been
  /// forwarded to the render worker.
  ///
  /// Winit reports one `DroppedFile` event per dropped path. We coalesce them and forward a single
  /// `UiToWorker::DropFiles` message once per event-loop cycle (see `Event::MainEventsCleared`).
  pending_dropped_files: Vec<std::path::PathBuf>,
  pending_dropped_files_tab: Option<fastrender::ui::TabId>,
  pending_dropped_files_pos_css: Option<(f32, f32)>,

  pending_context_menu_request: Option<PendingContextMenuRequest>,
  open_context_menu: Option<OpenContextMenu>,
  open_context_menu_rect: Option<egui::Rect>,

  open_select_dropdown: Option<OpenSelectDropdown>,
  open_select_dropdown_rect: Option<egui::Rect>,
  open_date_time_picker: Option<OpenDateTimePicker>,
  open_date_time_picker_rect: Option<egui::Rect>,
  open_file_picker: Option<OpenFilePicker>,
  open_file_picker_rect: Option<egui::Rect>,
  debug_log: std::collections::VecDeque<String>,
  debug_log_ui_enabled: bool,
  debug_log_ui_open: bool,
  debug_log_filter: String,
  debug_log_overlay_rect: Option<egui::Rect>,
  debug_log_overlay_pointer_capture: bool,
  hud: Option<BrowserHud>,

  tab_notifications: std::collections::HashMap<fastrender::ui::TabId, TabNotificationUiState>,
  warning_toast_rect: Option<egui::Rect>,
  error_infobar_rect: Option<egui::Rect>,
  chrome_toast: fastrender::ui::ToastState,
  chrome_toast_rect: Option<egui::Rect>,

  /// Deadline for the next egui-driven repaint (derived from `egui::FullOutput::repaint_after`).
  ///
  /// Stored as an `Instant` so the winit event loop can sleep in `ControlFlow::WaitUntil` even when
  /// no OS events are arriving (e.g. focus changes, spinners).
  next_egui_repaint: Option<std::time::Instant>,
  /// Last time we requested a redraw due to egui's repaint scheduling.
  ///
  /// Used to rate-limit "immediate" (`Duration::ZERO`) repaint requests so we don't busy loop.
  last_egui_redraw_request: Option<std::time::Instant>,
  /// Last time we requested a redraw due to a resize event (`WindowEvent::Resized` / `ScaleFactorChanged`).
  ///
  /// This is separate from `last_egui_redraw_request` so resize-driven redraw throttling does not
  /// interfere with other repaint sources (input events, worker messages, egui repaint scheduling).
  last_resize_redraw_request: Option<std::time::Instant>,
  /// Deadline for the next resize-driven redraw when resize events arrive faster than the frame cadence.
  next_resize_redraw: Option<std::time::Instant>,

  /// Periodic tick driver state for animated documents.
  ///
  /// The render worker only advances CSS animation/transition sampling time when the UI sends
  /// [`fastrender::ui::UiToWorker::Tick`]. We keep a small scheduler here so the windowed browser
  /// can display multi-frame animations without busy-polling the event loop.
  animation_tick_tab: Option<fastrender::ui::TabId>,
  next_animation_tick: Option<std::time::Instant>,

  /// Worker-requested "tickless" wakeups for media/A-V timing (e.g. video frame presentation).
  ///
  /// Stored per tab so multiple tabs can request independent wakeups (e.g. background audio, future
  /// features).
  next_media_wakeup: std::collections::HashMap<fastrender::ui::TabId, std::time::Instant>,
  /// Last time we sent a worker-driven tick due to `next_media_wakeup` (rate limiting).
  last_media_wakeup_tick: std::collections::HashMap<fastrender::ui::TabId, std::time::Instant>,
}

#[cfg(feature = "browser_ui")]
#[derive(Debug, Clone, Copy, Default)]
struct WorkerMessageResult {
  request_redraw: bool,
  history_changed: bool,
}

#[cfg(feature = "browser_ui")]
impl App {
  const DEBUG_LOG_MAX_LINES: usize = 200;
  const ANIMATION_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
  const PAGE_UPLOAD_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
  const RESIZE_PAGE_UPLOAD_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);
  /// Minimum interval between resize-driven redraw requests.
  ///
  /// Resize events can arrive at very high frequency on some platforms/window managers; throttling
  /// them avoids spamming `window.request_redraw()` faster than a typical display refresh.
  const MIN_RESIZE_REDRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
  const CLOSE_ANIM_FAVICON_TTL: std::time::Duration = std::time::Duration::from_millis(250);
  const MAX_CLOSING_TAB_FAVICONS: usize = 64;
  const RESIZE_VIEWPORT_THROTTLE_CONFIG: fastrender::ui::ViewportThrottleConfig =
    fastrender::ui::ViewportThrottleConfig {
      max_hz: 12,
      debounce: std::time::Duration::from_millis(140),
    };
  const SELECT_DROPDOWN_EDGE_PADDING_POINTS: f32 = 8.0;
  const SELECT_DROPDOWN_MIN_WIDTH_POINTS: f32 = 180.0;
  const SELECT_DROPDOWN_MAX_WIDTH_POINTS: f32 = 600.0;
  const SELECT_DROPDOWN_MAX_HEIGHT_POINTS: f32 = 320.0;
  const SELECT_DROPDOWN_PAGE_STEP: isize = 10;
  const SELECT_DROPDOWN_TYPEAHEAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1000);
  const MULTI_CLICK_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
  const MULTI_CLICK_MAX_DIST_POINTS: f32 = 4.0;

  fn sync_media_preferences_to_worker(&mut self, system_theme: Option<winit::window::Theme>) {
    use fastrender::ui::UiToWorker;

    let prefs = fastrender::ui::media_prefs::media_prefs_from_appearance(
      &self.applied_appearance,
      fastrender::ui::media_prefs::SystemTheme::from_winit(system_theme),
    );
    self.send_worker_msg(UiToWorker::SetMediaPreferences { prefs });
  }

  fn refresh_theme_from_system_theme(
    &mut self,
    system_theme: Option<winit::window::Theme>,
  ) -> bool {
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

    let high_contrast = self.applied_appearance.high_contrast;
    let accent = self.theme_accent;
    let desired_theme = match resolved_mode {
      ThemeMode::Dark => {
        if high_contrast {
          fastrender::ui::theme::BrowserTheme::dark_high_contrast(accent)
        } else {
          fastrender::ui::theme::BrowserTheme::dark(accent)
        }
      }
      _ => {
        if high_contrast {
          fastrender::ui::theme::BrowserTheme::light_high_contrast(accent)
        } else {
          fastrender::ui::theme::BrowserTheme::light(accent)
        }
      }
    };

    if resolved_mode == self.theme.mode
      && high_contrast == self.theme.high_contrast
      && desired_theme.colors.accent == self.theme.colors.accent
    {
      return false;
    }

    self.theme = desired_theme;
    // UI scale is applied via `egui_ctx.set_pixels_per_point(system_pixels_per_point * ui_scale)`.
    // Avoid also applying it through the theme system, otherwise text would scale quadratically.
    fastrender::ui::theme::apply_browser_theme(&self.egui_ctx, &self.theme);
    fastrender::ui::about_pages::sync_about_page_snapshot_chrome_accent(Some(
      fastrender::ui::theme_parsing::RgbaColor::from(self.theme.colors.accent),
    ));

    let bg = self.theme.colors.bg;
    self.clear_color = wgpu::Color {
      r: bg.r() as f64 / 255.0,
      g: bg.g() as f64 / 255.0,
      b: bg.b() as f64 / 255.0,
      a: bg.a() as f64 / 255.0,
    };

    self.sync_media_preferences_to_worker(system_theme);
    true
  }

  fn sync_renderer_media_prefs_to_runtime_toggles(&self) {
    use fastrender::ui::renderer_media_prefs::ResolvedTheme;

    let resolved_theme = match self.theme.mode {
      fastrender::ui::theme::ThemeMode::Dark => ResolvedTheme::Dark,
      _ => ResolvedTheme::Light,
    };

    update_renderer_media_prefs_runtime_toggles(
      resolved_theme,
      self.applied_appearance.high_contrast,
      self.applied_appearance.reduced_motion,
    );
  }

  fn sync_appearance_settings(&mut self) -> bool {
    let desired = self
      .browser_state
      .appearance
      .clone()
      .with_env_overrides(self.appearance_env_overrides);

    if desired == self.applied_appearance {
      return false;
    }

    let prev = self.applied_appearance.clone();
    self.applied_appearance = desired.clone();

    let mut needs_redraw = false;
    let mut media_prefs_changed = false;

    if desired.reduced_motion != prev.reduced_motion {
      needs_redraw = true;
      media_prefs_changed = true;
    }

    if (desired.ui_scale - prev.ui_scale).abs() > 1e-6 {
      self.ui_scale = desired.ui_scale;
      self.update_effective_pixels_per_point();
      // Point-space popups/hit boxes become stale when UI scaling changes; close them.
      self.close_select_dropdown();
      self.close_context_menu();
      if self.scrollbar_drag.is_some() {
        self.cancel_scrollbar_drag();
      }
      if self.pointer_captured {
        self.cancel_pointer_capture();
      }
      needs_redraw = true;
    }

    let theme_changed = desired.theme != prev.theme;
    let high_contrast_changed = desired.high_contrast != prev.high_contrast;
    let accent_changed = desired.accent_color != prev.accent_color;

    if accent_changed {
      self.theme_accent = desired
        .accent_color
        .as_deref()
        .and_then(fastrender::ui::theme_parsing::parse_hex_color)
        .map(|c| c.to_color32());
    }

    if theme_changed || high_contrast_changed {
      media_prefs_changed = true;
    }

    if theme_changed {
      self.theme_override = match desired.theme {
        fastrender::ui::theme_parsing::BrowserTheme::Light => {
          Some(fastrender::ui::theme::ThemeMode::Light)
        }
        fastrender::ui::theme_parsing::BrowserTheme::Dark => {
          Some(fastrender::ui::theme::ThemeMode::Dark)
        }
        fastrender::ui::theme_parsing::BrowserTheme::System => None,
      };

      let window_theme_override = match desired.theme {
        fastrender::ui::theme_parsing::BrowserTheme::Light => Some(winit::window::Theme::Light),
        fastrender::ui::theme_parsing::BrowserTheme::Dark => Some(winit::window::Theme::Dark),
        fastrender::ui::theme_parsing::BrowserTheme::System => None,
      };
      self.window.set_theme(window_theme_override);
    }

    if theme_changed || high_contrast_changed || accent_changed {
      needs_redraw |= self.refresh_theme_from_system_theme(self.window.theme());
      // Even if the resolved egui theme is unchanged (e.g. switching System→Light while the system
      // is already light), still treat this as a redraw-worthy UI change.
      needs_redraw = true;
    }

    if desired.theme != prev.theme
      || desired.high_contrast != prev.high_contrast
      || desired.reduced_motion != prev.reduced_motion
    {
      self.sync_renderer_media_prefs_to_runtime_toggles();
    }

    if media_prefs_changed {
      self.sync_media_preferences_to_worker(self.window.theme());
    }

    needs_redraw
  }

  fn cursor_over_egui_overlay(&self, pos_points: egui::Pos2) -> bool {
    self
      .open_select_dropdown_rect
      .is_some_and(|rect| rect.contains(pos_points))
      || self
        .open_date_time_picker_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .open_file_picker_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .open_context_menu_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .debug_log_overlay_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .warning_toast_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .error_infobar_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .page_unresponsive_overlay_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self
        .chrome_toast_rect
        .is_some_and(|rect| rect.contains(pos_points))
      || self.debug_log_overlay_pointer_capture
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
    self.overlay_scrollbars.vertical.is_some_and(|sb| {
      sb.track_rect_points
        .inflate(HOVER_INFLATE_POINTS)
        .contains_point(pos)
    }) || self.overlay_scrollbars.horizontal.is_some_and(|sb| {
      sb.track_rect_points
        .inflate(HOVER_INFLATE_POINTS)
        .contains_point(pos)
    })
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

  fn click_count_for_pointer_down(
    &mut self,
    button: fastrender::ui::PointerButton,
    pos_points: egui::Pos2,
  ) -> u8 {
    if !matches!(button, fastrender::ui::PointerButton::Primary) {
      self.primary_click_sequence = None;
      return 1;
    }

    let now = std::time::Instant::now();
    let mut count = 1u8;
    if let Some(prev) = self.primary_click_sequence {
      let dt = now.saturating_duration_since(prev.last_instant);
      let dx = pos_points.x - prev.last_pos_points.x;
      let dy = pos_points.y - prev.last_pos_points.y;
      let dist2 = dx * dx + dy * dy;
      let max_dist2 = Self::MULTI_CLICK_MAX_DIST_POINTS * Self::MULTI_CLICK_MAX_DIST_POINTS;
      if dt <= Self::MULTI_CLICK_MAX_DELAY && dist2 <= max_dist2 {
        count = prev.click_count.saturating_add(1).min(3);
      }
    }

    self.primary_click_sequence = Some(PointerClickSequence {
      last_pos_points: pos_points,
      last_instant: now,
      click_count: count,
    });
    count
  }

  fn new(
    window: winit::window::Window,
    event_loop: &winit::event_loop::EventLoopWindowTarget<UserEvent>,
    event_loop_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    ui_to_worker_tx: std::sync::mpsc::Sender<fastrender::ui::UiToWorker>,
    worker_join: std::thread::JoinHandle<()>,
    gpu: &GpuContext,
    trace: fastrender::debug::trace::TraceHandle,
    appearance_env_overrides: fastrender::ui::appearance::AppearanceEnvOverrides,
    applied_appearance: fastrender::ui::appearance::AppearanceSettings,
    theme_accent: Option<egui::Color32>,
    bookmarks_path: std::path::PathBuf,
    history_path: std::path::PathBuf,
    bookmarks: fastrender::ui::BookmarkStore,
    history: fastrender::ui::GlobalHistoryStore,
  ) -> Result<Self, Box<dyn std::error::Error>> {
    // Enable OS IME integration (WindowEvent::Ime) so the page can handle non-Latin input methods.
    // Egui manages IME for chrome text fields; we forward IME events to the page when appropriate.
    window.set_ime_allowed(true);

    // Accessibility backend selection:
    // - Default: egui-winit drives AccessKit for the egui-based chrome UI.
    // - Renderer-chrome: FastRender drives its own AccessKit adapter (separate tree, separate action routing).
    let chrome_a11y = if renderer_chrome_enabled_from_env() {
      ChromeA11yBackend::FastRender(ChromeAccessKit::new(&window, event_loop_proxy.clone()))
    } else {
      ChromeA11yBackend::Egui
    };

    let system_pixels_per_point = window.scale_factor() as f32;
    let ui_scale = applied_appearance.ui_scale;
    let pixels_per_point = system_pixels_per_point * ui_scale;

    let egui_ctx = egui::Context::default();
    egui_ctx.set_pixels_per_point(pixels_per_point);
    let egui_state = egui_winit::State::new(event_loop);

    let theme_override = match applied_appearance.theme {
      fastrender::ui::theme_parsing::BrowserTheme::Light => {
        Some(fastrender::ui::theme::ThemeMode::Light)
      }
      fastrender::ui::theme_parsing::BrowserTheme::Dark => {
        Some(fastrender::ui::theme::ThemeMode::Dark)
      }
      fastrender::ui::theme_parsing::BrowserTheme::System => None,
    };

    let theme_mode = fastrender::ui::theme::resolve_theme_mode(&window, theme_override);
    let high_contrast = applied_appearance.high_contrast;
    let theme = match theme_mode {
      fastrender::ui::theme::ThemeMode::Dark => {
        if high_contrast {
          fastrender::ui::theme::BrowserTheme::dark_high_contrast(theme_accent)
        } else {
          fastrender::ui::theme::BrowserTheme::dark(theme_accent)
        }
      }
      _ => {
        if high_contrast {
          fastrender::ui::theme::BrowserTheme::light_high_contrast(theme_accent)
        } else {
          fastrender::ui::theme::BrowserTheme::light(theme_accent)
        }
      }
    };
    // UI scale is applied via `egui_ctx.set_pixels_per_point(system_pixels_per_point * ui_scale)`.
    // Avoid also applying it through the theme system, otherwise text would scale quadratically.
    fastrender::ui::theme::apply_browser_theme(&egui_ctx, &theme);
    fastrender::ui::about_pages::sync_about_page_snapshot_chrome_accent(Some(
      fastrender::ui::theme_parsing::RgbaColor::from(theme.colors.accent),
    ));
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
    let window_fullscreen = window.fullscreen().is_some();
    let surface_config = wgpu::SurfaceConfiguration {
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
      format: surface_format,
      width: size.width.max(1),
      height: size.height.max(1),
      present_mode,
      alpha_mode,
      view_formats: vec![],
    };

    let mut hud = if browser_hud_enabled_from_env() {
      Some(BrowserHud::new())
    } else {
      None
    };
    surface.configure(&device, &surface_config);
    if let Some(hud) = hud.as_mut() {
      hud.surface_configure_calls_total = hud.surface_configure_calls_total.saturating_add(1);
    }

    let egui_renderer = egui_wgpu::Renderer::new(&device, surface_format, None, 1);
    let debug_log_ui_enabled = debug_log_ui_enabled();
    let log_surface_configure =
      parse_env_bool(std::env::var(ENV_BROWSER_LOG_SURFACE_CONFIGURE).ok().as_deref());
    let surface_configure_count = 1u64;
    if log_surface_configure {
      eprintln!(
        "[window:{:?}] surface.configure #{surface_configure_count} ({}x{}) [startup]",
        window.id(),
        surface_config.width,
        surface_config.height
      );
    }
    let window_id = window.id();
    let perf_log_enabled = parse_env_bool(std::env::var(ENV_PERF_LOG).ok().as_deref());

    let mut browser_state = fastrender::ui::BrowserAppState::new();
    browser_state.history = history;
    browser_state.seed_visited_from_history();

    let search_suggest_wake: std::sync::Arc<dyn Fn() + Send + Sync> = {
      // `SearchSuggestService` is winit-agnostic, so bridge back into the event loop via a callback.
      // Use a `Mutex` so the callback is `Sync` even if `EventLoopProxy` is `Send`-only.
      let proxy = std::sync::Mutex::new(event_loop_proxy.clone());
      std::sync::Arc::new(move || {
        if let Ok(proxy) = proxy.lock() {
          // Ignore failures during shutdown (event loop already dropped).
          let _ = proxy.send_event(UserEvent::SearchSuggestWake(window_id));
        }
      })
    };

    Ok(Self {
      window,
      window_title_cache: String::new(),
      event_loop_proxy,
      chrome_a11y,
      trace,
      surface,
      device,
      queue,
      surface_config,
      surface_needs_configure: false,
      surface_configure_count,
      log_surface_configure,
      egui_ctx,
      egui_state,
      egui_renderer,
      system_pixels_per_point,
      ui_scale,
      pixels_per_point,
      browser_limits: fastrender::ui::browser_limits::BrowserLimits::from_env(),
      page_texture_filter_policy: PageTextureFilterPolicy::from_env(),
      appearance_env_overrides,
      applied_appearance,
      theme_override,
      theme_accent,
      theme,
      clear_color,
      ui_to_worker_tx,
      worker_join: Some(worker_join),
      browser_state,
      home_url: fastrender::ui::about_pages::ABOUT_NEWTAB.to_string(),
      search_suggest: fastrender::ui::SearchSuggestService::new_with_wake(
        fastrender::ui::SearchSuggestConfig::default(),
        Some(search_suggest_wake),
      ),
      bookmarks_path,
      history_path,
      bookmarks,
      profile_autosave_tx: None,
      profile_bookmarks_dirty: false,
      profile_bookmarks_flush_requested: false,
      profile_history_dirty: false,
      profile_history_flush_requested: false,
      history_panel_open: false,
      history_panel_request_focus_search: false,
      bookmarks_panel_open: false,
      downloads_panel_open: false,
      downloads_panel_request_focus: false,
      clear_browsing_data_dialog_open: false,
      clear_browsing_data_dialog_request_focus: false,
      clear_browsing_data_range: fastrender::ui::ClearBrowsingDataRange::default(),
      tab_textures: std::collections::HashMap::new(),
      tab_favicons: std::collections::HashMap::new(),
      closing_tab_favicons: std::collections::VecDeque::new(),
      tab_cancel: std::collections::HashMap::new(),
      pending_scroll_restores: std::collections::HashMap::new(),
      pending_frame_uploads: fastrender::ui::FrameUploadCoalescer::new(),
      max_pending_frame_bytes: max_pending_frame_bytes_from_env(),
      last_page_upload_at: None,
      next_page_upload_redraw: None,
      content_rect_points: None,
      page_rect_points: None,
      page_viewport_css: None,
      page_input_tab: None,
      page_input_mapping: None,
      page_loading_overlay_blocks_input: false,
      page_unresponsive_overlay_rect: None,
      overlay_scrollbars: fastrender::ui::scrollbars::OverlayScrollbars::default(),
      overlay_scrollbar_visibility:
        fastrender::ui::scrollbars::OverlayScrollbarVisibilityState::default(),
      scrollbar_drag: None,
      pending_scroll_drag: None,
      viewport_cache_tab: None,
      viewport_cache_css: (0, 0),
      viewport_cache_dpr: 0.0,
      viewport_throttle: fastrender::ui::ViewportThrottle::new(),
      viewport_throttle_resizing: fastrender::ui::ViewportThrottle::with_config(
        Self::RESIZE_VIEWPORT_THROTTLE_CONFIG,
      ),
      viewport_throttle_tab: None,
      last_resize_event_at: None,
      resize_burst_active: false,
      resize_dpr_scale: resize_dpr_scale_from_env(),
      modifiers: winit::event::ModifiersState::default(),
      suppress_paste_events: false,
      wheel_events_buf: Vec::with_capacity(8),
      paste_events_buf: Vec::with_capacity(2),
      perf_log_enabled,
      window_focused: true,
      window_occluded: false,
      window_minimized: size.width == 0 || size.height == 0,
      window_fullscreen,
      page_has_focus: false,
      chrome_has_text_focus: false,
      pointer_captured: false,
      captured_button: fastrender::ui::PointerButton::None,
      primary_click_sequence: None,
      touch_gesture: TouchGestureRecognizer::new(),
      last_cursor_pos_points: None,
      cursor_in_page: false,
      page_cursor_override: None,
      pending_pointer_move: None,
      hover_sync_pending: false,
      pending_dropped_files: Vec::new(),
      pending_dropped_files_tab: None,
      pending_dropped_files_pos_css: None,
      pending_context_menu_request: None,
      open_context_menu: None,
      open_context_menu_rect: None,
      open_select_dropdown: None,
      open_select_dropdown_rect: None,
      open_date_time_picker: None,
      open_date_time_picker_rect: None,
      open_file_picker: None,
      open_file_picker_rect: None,
      bookmarks_manager: fastrender::ui::bookmarks_manager::BookmarksManagerState::default(),
      debug_log: std::collections::VecDeque::new(),
      debug_log_ui_enabled,
      debug_log_ui_open: debug_log_ui_enabled,
      debug_log_filter: String::new(),
      debug_log_overlay_rect: None,
      debug_log_overlay_pointer_capture: false,
      hud,
      tab_notifications: std::collections::HashMap::new(),
      warning_toast_rect: None,
      error_infobar_rect: None,
      chrome_toast: fastrender::ui::ToastState::default(),
      chrome_toast_rect: None,
      next_egui_repaint: None,
      last_egui_redraw_request: None,
      last_resize_redraw_request: None,
      next_resize_redraw: None,
      animation_tick_tab: None,
      next_animation_tick: None,
      next_media_wakeup: std::collections::HashMap::new(),
      last_media_wakeup_tick: std::collections::HashMap::new(),
    })
  }

  fn startup(&mut self, window: fastrender::ui::BrowserSessionWindow) {
    use fastrender::ui::UiToWorker;

    let fastrender::ui::BrowserSessionWindow {
      tabs,
      tab_groups,
      active_tab_index,
      show_menu_bar,
      ..
    } = window.sanitized();

    // Allow CI/scripts to force showing/hiding the menu bar regardless of persisted state.
    self.browser_state.chrome.show_menu_bar =
      show_menu_bar_override_from_env().unwrap_or(show_menu_bar);

    // Recreate runtime tab group ids (session indices are stable; runtime ids are process-unique).
    let mut runtime_groups: Vec<fastrender::ui::TabGroupId> = Vec::with_capacity(tab_groups.len());
    for group in tab_groups {
      let id = fastrender::ui::TabGroupId::new();
      runtime_groups.push(id);
      self.browser_state.tab_groups.insert(
        id,
        fastrender::ui::TabGroupState {
          id,
          title: group.title,
          color: group.color,
          collapsed: group.collapsed,
        },
      );
    }

    // Ensure pages see the current theme/accessibility preferences via `prefers-*` media queries
    // from the start (before any initial navigations).
    self.sync_media_preferences_to_worker(self.window.theme());

    // Ensure pinned tabs are always contiguous at the start (mirrors `BrowserAppState` invariant).
    // The session format stores the full tab ordering, but we defensively re-partition here in case
    // the on-disk session was hand-edited or produced by an older build.
    let mut pinned_tabs: Vec<(usize, fastrender::ui::BrowserSessionTab)> = Vec::new();
    let mut unpinned_tabs: Vec<(usize, fastrender::ui::BrowserSessionTab)> = Vec::new();
    for (idx, tab) in tabs.into_iter().enumerate() {
      if tab.pinned {
        pinned_tabs.push((idx, tab));
      } else {
        unpinned_tabs.push((idx, tab));
      }
    }

    let mut tab_ids = Vec::with_capacity(pinned_tabs.len() + unpinned_tabs.len());
    let mut active_tab_id: Option<fastrender::ui::TabId> = None;

    for (old_idx, tab) in pinned_tabs.into_iter().chain(unpinned_tabs.into_iter()) {
      let tab_id = fastrender::ui::TabId::new();
      tab_ids.push(tab_id);
      if old_idx == active_tab_index {
        active_tab_id = Some(tab_id);
      }

      if let Some(scroll_css) = tab.scroll_css {
        self.pending_scroll_restores.insert(tab_id, scroll_css);
      }

      let mut tab_state = fastrender::ui::BrowserTabState::new(tab_id, tab.url.clone());
      if let Some(zoom) = tab.zoom {
        tab_state.zoom = zoom;
      }
      tab_state.loading = true;
      tab_state.unresponsive = false;
      tab_state.last_worker_msg_at = std::time::SystemTime::now();
      tab_state.pinned = tab.pinned;
      tab_state.group = if tab.pinned {
        None
      } else {
        tab.group.and_then(|idx| runtime_groups.get(idx).copied())
      };
      let cancel = tab_state.cancel.clone();
      self.tab_cancel.insert(tab_id, cancel.clone());
      self.browser_state.push_tab(tab_state, false);

      self.send_worker_msg(UiToWorker::CreateTab {
        tab_id,
        initial_url: Some(tab.url),
        cancel,
      });
    }

    let active_tab_id = active_tab_id
      .or_else(|| tab_ids.first().copied())
      .expect("sanitized session should always provide at least one tab");
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
    // During interactive window resizing, pause animation ticks to keep CPU available for the UI
    // thread. The page will still repaint on user input / new frames, but we avoid continuously
    // advancing CSS animation time while the user is dragging the window edge.
    if self.window_occluded
      || self.window_minimized
      || !self.window_focused
      || self.resize_burst_active
    {
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

  fn update_resize_burst_state(&mut self, now: std::time::Instant) {
    let is_resizing = is_resize_burst_active(self.last_resize_event_at, now);
    if is_resizing == self.resize_burst_active {
      if !is_resizing {
        // Once the TTL expires, clear the timestamp so we stop scheduling wakeups.
        self.last_resize_event_at = None;
      }
      return;
    }

    let was_resizing = self.resize_burst_active;
    self.resize_burst_active = is_resizing;

    // When entering/leaving resize mode, drop the page-upload rate limiter so the next frame can
    // upload immediately (either the first low-quality resize frame or the restored full-quality
    // frame).
    self.last_page_upload_at = None;
    self.next_page_upload_redraw = None;

    // When a resize burst ends, flush any pending low-quality viewport update immediately before
    // returning to the normal throttle/DPR.
    if was_resizing && !is_resizing {
      self.force_send_viewport_now();
      self.last_resize_event_at = None;
    }

    std::mem::swap(&mut self.viewport_throttle, &mut self.viewport_throttle_resizing);
    self.viewport_throttle.reset();

    if was_resizing && !is_resizing {
      // Force a redraw so `render_frame` computes a full-DPR viewport and we repaint promptly.
      self.window.request_redraw();
    }
  }

  fn drive_periodic_tasks_and_update_control_flow(
    &mut self,
    control_flow: &mut winit::event_loop::ControlFlow,
  ) {
    // In a multi-window event loop, this should be called for every live window/app after handling
    // the current event so that:
    // - animation ticks are driven even when a given window is otherwise idle, and
    // - the global `ControlFlow::WaitUntil` deadline accounts for the earliest wakeup across all windows.
    self.update_resize_burst_state(std::time::Instant::now());
    self.drain_expired_closing_tab_favicons();
    self.drive_animation_tick();
    self.drive_media_wakeups();
    self.drive_viewport_throttle();
    self.drive_egui_repaint();
    self.drive_page_upload_redraw();
    self.drive_touch_long_press();
    self.drive_resize_redraw();
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

  fn drive_media_wakeups(&mut self) {
    // Keep behaviour consistent with the existing tick driver: don't run worker ticks when the
    // window is not in a state where we expect animations to be active (avoids surprising "background"
    // ticking when a window is unfocused/occluded).
    if self.window_occluded || self.window_minimized || !self.window_focused {
      return;
    }

    if self.next_media_wakeup.is_empty() {
      return;
    }

    let now = std::time::Instant::now();

    let mut due: Vec<fastrender::ui::TabId> = Vec::new();
    for (tab_id, deadline) in &self.next_media_wakeup {
      if now >= *deadline {
        due.push(*tab_id);
      }
    }

    if due.is_empty() {
      return;
    }

    for tab_id in due {
      self.next_media_wakeup.remove(&tab_id);
      // Ignore stale wakeups for tabs that have already been closed.
      if self.browser_state.tab(tab_id).is_none() {
        continue;
      }
      self.send_worker_msg(fastrender::ui::UiToWorker::Tick { tab_id });
      self.last_media_wakeup_tick.insert(tab_id, now);
    }
  }

  fn schedule_media_wakeup(&mut self, tab_id: fastrender::ui::TabId, after: std::time::Duration) {
    if self.browser_state.tab(tab_id).is_none() {
      // Ignore stale requests for tabs that have already been closed.
      return;
    }
    let now = std::time::Instant::now();
    let last = self.last_media_wakeup_tick.get(&tab_id).copied();
    let plan = fastrender::ui::repaint_scheduler::plan_worker_wake_after(now, after, last);

    if plan.wake_now {
      // Store as an immediate deadline so `drive_media_wakeups` can deliver the tick in a single
      // place (and apply window-state gating consistently).
      self.next_media_wakeup.insert(tab_id, now);
    } else if let Some(deadline) = plan.next_deadline {
      self.next_media_wakeup.insert(tab_id, deadline);
    } else {
      // `Duration::MAX` (or overflow) is treated as "cancel wakeup".
      self.next_media_wakeup.remove(&tab_id);
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

  fn drive_page_upload_redraw(&mut self) {
    if self.window_occluded || self.window_minimized {
      self.next_page_upload_redraw = None;
      return;
    }

    let Some(deadline) = self.next_page_upload_redraw else {
      return;
    };

    // Drop the scheduled redraw if there is no longer anything to upload for the active tab (e.g.
    // the tab was closed or a different tab became active).
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      self.next_page_upload_redraw = None;
      return;
    };
    if !self.pending_frame_uploads.has_pending_for_tab(tab_id) {
      self.next_page_upload_redraw = None;
      return;
    }

    let now = std::time::Instant::now();
    if now >= deadline {
      self.next_page_upload_redraw = None;
      self.window.request_redraw();
    }
  }

  fn schedule_resize_redraw(&mut self) {
    if self.window_occluded || self.window_minimized {
      self.next_resize_redraw = None;
      return;
    }

    let now = std::time::Instant::now();
    let Some(last) = self.last_resize_redraw_request else {
      self.last_resize_redraw_request = Some(now);
      self.next_resize_redraw = None;
      self.window.request_redraw();
      return;
    };

    if now.saturating_duration_since(last) >= Self::MIN_RESIZE_REDRAW_INTERVAL {
      self.last_resize_redraw_request = Some(now);
      self.next_resize_redraw = None;
      self.window.request_redraw();
      return;
    }

    let Some(deadline) = last.checked_add(Self::MIN_RESIZE_REDRAW_INTERVAL) else {
      self.last_resize_redraw_request = Some(now);
      self.next_resize_redraw = None;
      self.window.request_redraw();
      return;
    };

    self.next_resize_redraw = Some(match self.next_resize_redraw {
      Some(existing) => existing.min(deadline),
      None => deadline,
    });
  }

  fn drive_resize_redraw(&mut self) {
    if self.window_occluded || self.window_minimized {
      self.next_resize_redraw = None;
      return;
    }

    let Some(deadline) = self.next_resize_redraw else {
      return;
    };

    let now = std::time::Instant::now();
    if now >= deadline {
      self.next_resize_redraw = None;
      self.last_resize_redraw_request = Some(now);
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

  fn next_wakeup_deadline(&self, now: std::time::Instant) -> Option<std::time::Instant> {
    let mut deadline: Option<std::time::Instant> = None;

    // Worker-driven animation ticks (CSS animations/transitions).
    if let Some(tab_id) = self.desired_animation_tick_tab() {
      // `drive_animation_tick` is responsible for keeping `next_animation_tick` in sync, but be
      // defensive: if the schedule isn't primed yet, fall back to a "first tick" interval.
      let tick_deadline = if self.animation_tick_tab == Some(tab_id) {
        self
          .next_animation_tick
          .unwrap_or(now + Self::ANIMATION_TICK_INTERVAL)
      } else {
        now + Self::ANIMATION_TICK_INTERVAL
      };
      deadline = Some(match deadline {
        Some(existing) => existing.min(tick_deadline),
        None => tick_deadline,
      });
    }

    // Worker-requested media wakeups (tickless scheduling).
    if !(self.window_occluded || self.window_minimized || !self.window_focused) {
      if let Some(media_deadline) = self.next_media_wakeup.values().copied().min() {
        deadline = Some(match deadline {
          Some(existing) => existing.min(media_deadline),
          None => media_deadline,
        });
      }
    }

    // Egui repaint scheduling (focus changes, animated widgets like spinners, etc).
    if let Some(egui_deadline) = self.next_egui_repaint {
      deadline = Some(match deadline {
        Some(existing) => existing.min(egui_deadline),
        None => egui_deadline,
      });
    }

    // Rate-limited page pixmap uploads (retry uploads after `PAGE_UPLOAD_MIN_INTERVAL`).
    if let Some(upload_deadline) = self.next_page_upload_redraw {
      deadline = Some(match deadline {
        Some(existing) => existing.min(upload_deadline),
        None => upload_deadline,
      });
    }

    // Resize-driven redraw throttling (interactive window resize / DPI changes).
    if let Some(resize_deadline) = self.next_resize_redraw {
      deadline = Some(match deadline {
        Some(existing) => existing.min(resize_deadline),
        None => resize_deadline,
      });
    }

    // Delayed-destroy favicons for closing-tab animations.
    if let Some(favicon_deadline) = self.next_closing_tab_favicon_deadline() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(favicon_deadline),
        None => favicon_deadline,
      });
    }

    // UI-only timers (e.g. overlay scrollbar fade).
    let cfg = fastrender::ui::scrollbars::OverlayScrollbarVisibilityConfig::default();
    let force_visible = self.overlay_scrollbars_force_visible();
    if let Some(sb_deadline) =
      self
        .overlay_scrollbar_visibility
        .next_wakeup(now, cfg, force_visible)
    {
      deadline = Some(match deadline {
        Some(existing) => existing.min(sb_deadline),
        None => sb_deadline,
      });
    }

    // Touch long-press gesture deadline (context menu).
    if let Some(touch_deadline) = self.touch_gesture.next_deadline() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(touch_deadline),
        None => touch_deadline,
      });
    }

    // Warning toast expiry (auto-dismiss).
    if let Some(toast_deadline) = self.next_warning_toast_deadline_for_active_tab() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(toast_deadline),
        None => toast_deadline,
      });
    }

    // Chrome toast expiry (auto-dismiss).
    if let Some(toast_deadline) = self.chrome_toast.next_deadline() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(toast_deadline),
        None => toast_deadline,
      });
    }

    // Viewport debounce wakeups (resize settling).
    if let Some(vp_deadline) = self.viewport_throttle.next_deadline() {
      deadline = Some(match deadline {
        Some(existing) => existing.min(vp_deadline),
        None => vp_deadline,
      });
    }

    // Resize-burst wakeup (window drag-resize detection).
    if let Some(resize_deadline) = resize_burst_deadline(self.last_resize_event_at) {
      deadline = Some(match deadline {
        Some(existing) => existing.min(resize_deadline),
        None => resize_deadline,
      });
    }

    deadline
  }

  fn drive_touch_long_press(&mut self) {
    let now = std::time::Instant::now();
    let Some(action) = self.touch_gesture.tick(now) else {
      return;
    };
    if let TouchGestureAction::ContextMenu { pos_points } = action {
      let pos_points = egui::pos2(pos_points.0, pos_points.1);

      // Do not open context menus while stale page content is covered by the loading overlay.
      if self.page_loading_overlay_blocks_input
        && self
          .page_rect_points
          .is_some_and(|page_rect| page_rect.contains(pos_points))
      {
        return;
      }

      let Some(rect) = self.page_rect_points else {
        return;
      };
      if !rect.contains(pos_points) {
        return;
      }
      if self.cursor_over_egui_overlay(pos_points) || self.cursor_over_overlay_scrollbars(pos_points) {
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

      self.primary_click_sequence = None;
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
    }
  }

  fn maybe_request_redraw_for_ui_timers(&mut self) {
    let now = std::time::Instant::now();
    let mut needs_redraw = false;

    let cfg = fastrender::ui::scrollbars::OverlayScrollbarVisibilityConfig::default();
    let force_visible = self.overlay_scrollbars_force_visible();
    if self
      .overlay_scrollbar_visibility
      .needs_repaint(now, cfg, force_visible)
    {
      needs_redraw = true;
    }

    if self
      .next_warning_toast_deadline_for_active_tab()
      .is_some_and(|deadline| now >= deadline)
    {
      needs_redraw = true;
    }

    if self
      .chrome_toast
      .next_deadline()
      .is_some_and(|deadline| now >= deadline)
    {
      needs_redraw = true;
    }

    if needs_redraw {
      self.window.request_redraw();
    }
  }

  fn send_worker_msg(&mut self, msg: fastrender::ui::UiToWorker) {
    use fastrender::ui::UiToWorker;

    // If the active tab changes while a scrollbar thumb drag is in progress, drop any unflushed
    // delta and cancel the drag. This avoids "leaking" drag scroll into the wrong tab.
    if let UiToWorker::SetActiveTab { tab_id } = &msg {
      if self
        .scrollbar_drag
        .is_some_and(|drag| drag.tab_id != *tab_id)
      {
        self.pending_scroll_drag = None;
        self.cancel_scrollbar_drag();
      }
    }
    if let UiToWorker::CloseTab { tab_id } = &msg {
      if self.scrollbar_drag.is_some_and(|drag| drag.tab_id == *tab_id) {
        self.pending_scroll_drag = None;
        self.cancel_scrollbar_drag();
      }
    }

    // Keep overlay scrollbars visible when a scroll is initiated via any input path (wheel, track
    // click, thumb drag, keyboard shortcuts that synthesize `ScrollTo`, etc).
    if matches!(
      &msg,
      UiToWorker::Scroll { .. } | UiToWorker::ScrollTo { .. }
    ) {
      self
        .overlay_scrollbar_visibility
        .register_interaction(std::time::Instant::now());
    }

    let tab_id = match &msg {
      UiToWorker::SetMediaPreferences { .. } => None,
      UiToWorker::SetDownloadDirectory { .. } => None,
      UiToWorker::CreateTab { tab_id, .. }
      | UiToWorker::NewTab { tab_id, .. }
      | UiToWorker::CloseTab { tab_id }
      | UiToWorker::SetActiveTab { tab_id }
      | UiToWorker::Navigate { tab_id, .. }
      | UiToWorker::NavigateRequest { tab_id, .. }
      | UiToWorker::GoBack { tab_id }
      | UiToWorker::GoForward { tab_id }
      | UiToWorker::Reload { tab_id }
      | UiToWorker::StopLoading { tab_id }
      | UiToWorker::Tick { tab_id }
      | UiToWorker::ViewportChanged { tab_id, .. }
      | UiToWorker::Scroll { tab_id, .. }
      | UiToWorker::ScrollTo { tab_id, .. }
      | UiToWorker::PointerMove { tab_id, .. }
      | UiToWorker::PointerDown { tab_id, .. }
      | UiToWorker::PointerUp { tab_id, .. }
      | UiToWorker::DropFiles { tab_id, .. }
      | UiToWorker::ContextMenuRequest { tab_id, .. }
      | UiToWorker::SelectDropdownChoose { tab_id, .. }
      | UiToWorker::SelectDropdownCancel { tab_id }
      | UiToWorker::SelectDropdownPick { tab_id, .. }
      | UiToWorker::DateTimePickerChoose { tab_id, .. }
      | UiToWorker::DateTimePickerCancel { tab_id }
      | UiToWorker::ColorPickerChoose { tab_id, .. }
      | UiToWorker::ColorPickerCancel { tab_id }
      | UiToWorker::FilePickerChoose { tab_id, .. }
      | UiToWorker::FilePickerCancel { tab_id }
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
      | UiToWorker::RequestRepaint { tab_id, .. }
      | UiToWorker::StartDownload { tab_id, .. }
      | UiToWorker::CancelDownload { tab_id, .. } => Some(*tab_id),
    };

    if let Some(tab_id) = tab_id {
      if let Some(cancel) = self.tab_cancel.get(&tab_id) {
        match &msg {
          // Navigations should cancel any in-flight navigation + paint work.
          UiToWorker::Navigate { .. }
          | UiToWorker::NavigateRequest { .. }
          | UiToWorker::GoBack { .. }
          | UiToWorker::GoForward { .. }
          | UiToWorker::Reload { .. }
          | UiToWorker::StopLoading { .. } => cancel.bump_nav(),
          // Repaint-driving events should cancel in-flight paints so we don't waste time rendering
          // intermediate frames (e.g. rapid scroll/resize/typing).
           UiToWorker::Tick { .. }
           | UiToWorker::ViewportChanged { .. }
           | UiToWorker::Scroll { .. }
           | UiToWorker::ScrollTo { .. }
           | UiToWorker::PointerMove { .. }
           | UiToWorker::PointerDown { .. }
           | UiToWorker::PointerUp { .. }
          | UiToWorker::DropFiles { .. }
          | UiToWorker::SelectDropdownChoose { .. }
          | UiToWorker::SelectDropdownCancel { .. }
          | UiToWorker::SelectDropdownPick { .. }
          | UiToWorker::DateTimePickerChoose { .. }
          | UiToWorker::DateTimePickerCancel { .. }
          | UiToWorker::ColorPickerChoose { .. }
          | UiToWorker::ColorPickerCancel { .. }
          | UiToWorker::FilePickerChoose { .. }
          | UiToWorker::FilePickerCancel { .. }
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
           // Tab-management messages should not force cancellation.
           UiToWorker::SetMediaPreferences { .. }
           | UiToWorker::ContextMenuRequest { .. }
           | UiToWorker::CreateTab { .. }
           | UiToWorker::NewTab { .. }
           | UiToWorker::CloseTab { .. }
           | UiToWorker::SetActiveTab { .. }
          | UiToWorker::SetDownloadDirectory { .. }
          | UiToWorker::Copy { .. }
          | UiToWorker::SelectAll { .. }
          | UiToWorker::StartDownload { .. }
          | UiToWorker::CancelDownload { .. } => {}
        }
      }
    }

    let _ = self.ui_to_worker_tx.send(msg);
  }

  fn update_effective_pixels_per_point(&mut self) {
    let effective = self.system_pixels_per_point * self.ui_scale;
    // Protect against bogus values so we don't feed NaNs into egui input mapping.
    let effective = if effective.is_finite() && effective > 0.0 {
      effective
    } else {
      1.0
    };

    if (effective - self.pixels_per_point).abs() <= 1e-6 {
      return;
    }

    self.pixels_per_point = effective;
    self.egui_ctx.set_pixels_per_point(effective);
    // Invalidate cached point-space state; next frame will recompute layout/viewport.
    self.last_cursor_pos_points = None;
    self.viewport_cache_tab = None;
  }

  fn set_system_pixels_per_point(&mut self, system_ppp: f32) {
    self.system_pixels_per_point = system_ppp;
    self.update_effective_pixels_per_point();
  }

  fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
    self.last_resize_event_at = Some(std::time::Instant::now());
    if new_size.width == 0 || new_size.height == 0 {
      // Window minimization can transiently report a 0×0 size; clear any pending configure request
      // so we don't reconfigure the surface at a stale size while minimized.
      self.surface_needs_configure = false;
      return;
    }

    if new_size.width == self.surface_config.width && new_size.height == self.surface_config.height {
      return;
    }

    self.surface_config.width = new_size.width;
    self.surface_config.height = new_size.height;
    self.surface_needs_configure = true;
    // Invalidate the cached viewport so the worker sees the new dimensions on the next frame.
    self.viewport_cache_tab = None;
  }

  fn configure_surface(&mut self, reason: &'static str) {
    self.surface.configure(&self.device, &self.surface_config);
    if let Some(hud) = self.hud.as_mut() {
      hud.surface_configure_calls_total = hud.surface_configure_calls_total.saturating_add(1);
    }
    self.surface_needs_configure = false;
    self.surface_configure_count = self.surface_configure_count.saturating_add(1);
    if self.log_surface_configure {
      eprintln!(
        "[window:{:?}] surface.configure #{} ({}x{}) [{reason}]",
        self.window.id(),
        self.surface_configure_count,
        self.surface_config.width,
        self.surface_config.height
      );
    }
  }

  fn destroy_all_textures(&mut self) {
    for (_, tex) in std::mem::take(&mut self.tab_textures) {
      tex.destroy(&mut self.egui_renderer);
    }
    for (_, tex) in std::mem::take(&mut self.tab_favicons) {
      tex.destroy(&mut self.egui_renderer);
    }
    for entry in std::mem::take(&mut self.closing_tab_favicons) {
      entry.texture.destroy(&mut self.egui_renderer);
    }
  }

  fn close_anim_favicon_ttl(&self) -> std::time::Duration {
    let motion =
      fastrender::ui::motion::UiMotion::from_settings(self.browser_state.appearance.reduced_motion);
    if motion.enabled {
      Self::CLOSE_ANIM_FAVICON_TTL
    } else {
      std::time::Duration::ZERO
    }
  }

  fn move_tab_favicon_into_delayed_destroy(&mut self, tab_id: fastrender::ui::TabId) {
    let Some(tex) = self.tab_favicons.remove(&tab_id) else {
      return;
    };

    let ttl = self.close_anim_favicon_ttl();
    if ttl.is_zero() {
      tex.destroy(&mut self.egui_renderer);
      return;
    }

    let expires_at = std::time::Instant::now() + ttl;
    self.closing_tab_favicons.push_back(ClosingTabFavicon {
      tab_id,
      expires_at,
      texture: tex,
    });

    while self.closing_tab_favicons.len() > Self::MAX_CLOSING_TAB_FAVICONS {
      if let Some(entry) = self.closing_tab_favicons.pop_front() {
        entry.texture.destroy(&mut self.egui_renderer);
      }
    }
  }

  fn drain_expired_closing_tab_favicons(&mut self) {
    let now = std::time::Instant::now();
    if self.closing_tab_favicons.is_empty() {
      return;
    }

    let mut remaining: std::collections::VecDeque<ClosingTabFavicon> =
      std::collections::VecDeque::with_capacity(self.closing_tab_favicons.len());
    for entry in std::mem::take(&mut self.closing_tab_favicons) {
      if now >= entry.expires_at {
        entry.texture.destroy(&mut self.egui_renderer);
      } else {
        remaining.push_back(entry);
      }
    }
    self.closing_tab_favicons = remaining;
  }

  fn next_closing_tab_favicon_deadline(&self) -> Option<std::time::Instant> {
    self
      .closing_tab_favicons
      .iter()
      .map(|entry| entry.expires_at)
      .min()
  }

  fn close_context_menu(&mut self) {
    self.pending_context_menu_request = None;
    self.open_context_menu = None;
    self.open_context_menu_rect = None;
  }

  fn close_select_dropdown(&mut self) {
    close_select_dropdown_popup(
      &self.egui_ctx,
      &mut self.open_select_dropdown,
      &mut self.open_select_dropdown_rect,
    );
  }

  fn cancel_select_dropdown(&mut self) {
    if let Some(dropdown) = self.open_select_dropdown.as_ref() {
      self.send_worker_msg(fastrender::ui::UiToWorker::select_dropdown_cancel(
        dropdown.tab_id,
      ));
    }
    self.close_select_dropdown();
  }

  fn close_date_time_picker(&mut self) {
    close_date_time_picker_popup(
      &self.egui_ctx,
      &mut self.open_date_time_picker,
      &mut self.open_date_time_picker_rect,
    );
  }

  fn cancel_date_time_picker(&mut self) {
    if let Some(picker) = self.open_date_time_picker.as_ref() {
      self.send_worker_msg(fastrender::ui::UiToWorker::date_time_picker_cancel(
        picker.tab_id,
      ));
    }
    self.close_date_time_picker();
  }

  fn close_file_picker(&mut self) {
    self.open_file_picker = None;
    self.open_file_picker_rect = None;
  }

  fn cancel_file_picker(&mut self) {
    if let Some(picker) = self.open_file_picker.as_ref() {
      self.send_worker_msg(fastrender::ui::UiToWorker::FilePickerCancel { tab_id: picker.tab_id });
    }
    self.close_file_picker();
  }

  fn launch_native_file_picker_dialog(
    &self,
    tab_id: fastrender::ui::TabId,
    input_node_id: usize,
    multiple: bool,
    accept: Option<String>,
  ) {
    let tx = self.ui_to_worker_tx.clone();
    let cancel: Option<fastrender::ui::cancel::CancelGens> = self.tab_cancel.get(&tab_id).cloned();

    let spawn_result = std::thread::Builder::new()
      .name(format!("fastr-file-picker-{}", tab_id.0))
      .spawn(move || {
        let extensions = rfd_extensions_from_html_accept(accept.as_deref());

        let dialog = if extensions.is_empty() {
          rfd::FileDialog::new()
        } else {
          let ext_refs: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
          rfd::FileDialog::new().add_filter("Allowed files", &ext_refs)
        };

        let chosen: Option<Vec<std::path::PathBuf>> = if multiple {
          dialog.pick_files()
        } else {
          dialog.pick_file().map(|path| vec![path])
        };

        let msg = match chosen {
          Some(paths) if !paths.is_empty() => fastrender::ui::UiToWorker::FilePickerChoose {
            tab_id,
            input_node_id,
            paths,
          },
          _ => fastrender::ui::UiToWorker::FilePickerCancel { tab_id },
        };

        if let Some(cancel) = cancel {
          // Mirror `App::send_worker_msg` cancellation semantics for repaint-driving UI events.
          cancel.bump_paint();
        }
        // Best-effort: the UI may have already shut down.
        let _ = tx.send(msg);
      });

    if let Err(err) = spawn_result {
      eprintln!("failed to spawn native file picker dialog thread: {err}");
    }
  }

  fn flush_pending_scroll_drag(&mut self) {
    let Some(pending) = self.pending_scroll_drag.take() else {
      return;
    };
    self.send_worker_msg(pending.into_worker_msg());
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
      if !self.pointer_captured && self.cursor_over_egui_overlay(pos) {
        // Avoid updating page hover state while the pointer is interacting with a popup.
        return;
      }
    }
    self.send_worker_msg(msg);
  }

  fn page_pos_points_to_pos_css_for_drop(&self, pos_points: egui::Pos2) -> Option<(f32, f32)> {
    if self.page_loading_overlay_blocks_input {
      return None;
    }
    if !self
      .page_rect_points
      .is_some_and(|page_rect| page_rect.contains(pos_points))
    {
      return None;
    }
    if !self.pointer_captured && self.cursor_over_egui_overlay(pos_points) {
      return None;
    }
    if !self.pointer_captured && self.cursor_over_overlay_scrollbars(pos_points) {
      return None;
    }
    let mapping = self.page_input_mapping.as_ref()?;
    mapping.pos_points_to_pos_css_clamped(pos_points)
  }

  fn flush_pending_dropped_files(&mut self) {
    if self.pending_dropped_files.is_empty() {
      return;
    }
    let Some(tab_id) = self
      .pending_dropped_files_tab
      .take()
      .or(self.page_input_tab)
      .or(self.browser_state.active_tab_id())
    else {
      self.pending_dropped_files.clear();
      self.pending_dropped_files_pos_css = None;
      return;
    };

    let pos_css = self
      .pending_dropped_files_pos_css
      .take()
      .or_else(|| {
        self
          .last_cursor_pos_points
          .and_then(|pos| self.page_pos_points_to_pos_css_for_drop(pos))
      });
    let Some(pos_css) = pos_css else {
      self.pending_dropped_files.clear();
      return;
    };

    let paths = std::mem::take(&mut self.pending_dropped_files);
    // Ensure any coalesced hover update is delivered before the drop message.
    self.flush_pending_pointer_move();
    self.send_worker_msg(fastrender::ui::UiToWorker::DropFiles { tab_id, pos_css, paths });
  }

  fn apply_page_cursor_icon(&mut self) {
    use fastrender::ui::CursorKind;
    use winit::window::CursorIcon;

    let overlay_intercepts = !self.pointer_captured
      && self
        .last_cursor_pos_points
        .is_some_and(|pos| self.cursor_over_egui_overlay(pos));

    if !self.cursor_in_page || overlay_intercepts {
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

  fn apply_open_url_in_new_tab_messages(&mut self, msgs: Vec<fastrender::ui::UiToWorker>) {
    use fastrender::ui::PointerButton;

    // Reset per-tab cached state; mimic `ChromeAction::NewTab`/`ActivateTab` behaviour.
    self.page_has_focus = true;
    self.viewport_cache_tab = None;
    self.pointer_captured = false;
    self.captured_button = PointerButton::None;
    self.cursor_in_page = false;
    self.hover_sync_pending = true;
    self.pending_pointer_move = None;

    for msg in msgs {
      self.send_worker_msg(msg);
    }
    self.window.request_redraw();
  }

  /// Open `url` in a new tab and focus the page (matching the expected "open in new tab" UX).
  fn open_url_in_new_tab(&mut self, url: String) -> bool {
    self.open_url_in_new_tab_with_reason(url, fastrender::ui::NavigationReason::LinkClick)
  }

  fn open_url_in_new_tab_with_reason(
    &mut self,
    url: String,
    reason: fastrender::ui::NavigationReason,
  ) -> bool {
    let url = match fastrender::ui::untrusted::validate_untrusted_navigation_url(&url) {
      Ok(url) => url,
      Err(_) => {
        self.show_chrome_toast("Blocked attempt to open an invalid URL");
        return false;
      }
    };

    // Close any transient UI state before switching tabs.
    if self.open_select_dropdown.is_some() {
      self.cancel_select_dropdown();
    }
    if self.open_date_time_picker.is_some() {
      self.cancel_date_time_picker();
    }
    if self.open_file_picker.is_some() {
      self.cancel_file_picker();
    }
    if self.pointer_captured {
      self.cancel_pointer_capture();
    }

    let (_tab_id, msgs) =
      open_url_in_new_tab_state(&mut self.browser_state, &mut self.tab_cancel, url, reason);
    self.apply_open_url_in_new_tab_messages(msgs);
    true
  }

  /// Open a navigation request in a new tab and focus the page.
  ///
  /// This is used for form submissions with `target=_blank` that carry an explicit request (notably
  /// POST submissions).
  fn open_request_in_new_tab(&mut self, request: fastrender::interaction::FormSubmission) -> bool {
    use fastrender::ui::cancel::CancelGens;
    use fastrender::ui::messages::{NavigationReason, RepaintReason, UiToWorker};
    use fastrender::ui::{BrowserTabState, PointerButton, TabId};

    let mut request = request;
    let safe_url = match fastrender::ui::untrusted::validate_untrusted_navigation_url(&request.url) {
      Ok(url) => url,
      Err(_) => {
        self.show_chrome_toast("Blocked attempt to open an invalid URL");
        return false;
      }
    };
    request.url = safe_url;

    // Close any transient UI state before switching tabs.
    if self.open_select_dropdown.is_some() {
      self.cancel_select_dropdown();
    }
    if self.open_date_time_picker.is_some() {
      self.cancel_date_time_picker();
    }
    if self.open_file_picker.is_some() {
      self.cancel_file_picker();
    }
    if self.pointer_captured {
      self.cancel_pointer_capture();
    }

    let url = request.url.clone();
    let new_tab_id = TabId::new();
    let mut tab_state = BrowserTabState::new(new_tab_id, url.clone());
    tab_state.loading = true;
    tab_state.unresponsive = false;
    tab_state.last_worker_msg_at = std::time::SystemTime::now();
    let cancel: CancelGens = tab_state.cancel.clone();
    self.tab_cancel.insert(new_tab_id, cancel.clone());
    self.browser_state.push_tab(tab_state, true);

    // Reset per-tab cached state; mimic `ChromeAction::NewTab`/`ActivateTab` behaviour.
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
    self.send_worker_msg(UiToWorker::NavigateRequest {
      tab_id: new_tab_id,
      request,
      reason: NavigationReason::LinkClick,
    });
    self.send_worker_msg(UiToWorker::RequestRepaint {
      tab_id: new_tab_id,
      reason: RepaintReason::Explicit,
    });
    self.window.request_redraw();

    true
  }

  fn handle_worker_message(&mut self, msg: fastrender::ui::WorkerToUi) -> WorkerMessageResult {
    // Worker-initiated tab creation/navigation.
    let msg = match msg {
      fastrender::ui::WorkerToUi::RequestOpenInNewTab { tab_id, url } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.last_worker_msg_at = std::time::SystemTime::now();
          tab.unresponsive = false;
        }
        self.open_url_in_new_tab(url);
        return WorkerMessageResult {
          request_redraw: true,
          history_changed: false,
        };
      }
      fastrender::ui::WorkerToUi::RequestOpenInNewTabRequest { tab_id, request } => {
        if let Some(tab) = self.browser_state.tab_mut(tab_id) {
          tab.last_worker_msg_at = std::time::SystemTime::now();
          tab.unresponsive = false;
        }
        self.open_request_in_new_tab(request);
        return WorkerMessageResult {
          request_redraw: true,
          history_changed: false,
        };
      }
      msg => msg,
    };

    let (msg, clipboard_update) =
      fastrender::ui::protocol_limits::sanitize_worker_to_ui_clipboard_message(msg);

    // The render worker can send large/hostile payloads for picker/dropdown overlays. Sanitize them
    // before any UI code (including our own pre-reducer side effects) clones/uses the data.
    let Some(msg) = sanitize_worker_to_ui_for_windowed_browser(msg) else {
      return WorkerMessageResult {
        request_redraw: false,
        history_changed: false,
      };
    };

    // UI-only side effects that depend on the raw message before the shared reducer consumes it.
    match &msg {
      fastrender::ui::WorkerToUi::NavigationStarted { tab_id, .. }
      | fastrender::ui::WorkerToUi::NavigationCommitted { tab_id, .. }
      | fastrender::ui::WorkerToUi::NavigationFailed { tab_id, .. }
      | fastrender::ui::WorkerToUi::SelectDropdownClosed { tab_id }
      | fastrender::ui::WorkerToUi::DateTimePickerClosed { tab_id }
      | fastrender::ui::WorkerToUi::FilePickerClosed { tab_id } => {
        if self
          .open_select_dropdown
          .as_ref()
          .is_some_and(|d| d.tab_id == *tab_id)
        {
          self.close_select_dropdown();
        }
        if self
          .open_date_time_picker
          .as_ref()
          .is_some_and(|picker| picker.tab_id == *tab_id)
        {
          self.close_date_time_picker();
        }
        if self
          .open_file_picker
          .as_ref()
          .is_some_and(|picker| picker.tab_id == *tab_id)
        {
          self.close_file_picker();
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

    if let fastrender::ui::WorkerToUi::RequestWakeAfter { tab_id, after, .. } = &msg {
      self.schedule_media_wakeup(*tab_id, *after);
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
    let mut history_changed = false;

    if let fastrender::ui::WorkerToUi::DebugLog { tab_id, line } = &msg {
      let safe = fastrender::ui::untrusted::sanitize_untrusted_text(
        line,
        fastrender::ui::protocol_limits::MAX_DEBUG_LOG_BYTES,
      );
      if !safe.is_empty() {
        eprintln!("[worker:{tab_id:?}] {safe}");
      }
      if self.debug_log_ui_enabled {
        if !safe.is_empty() {
          if self.debug_log.len() >= Self::DEBUG_LOG_MAX_LINES {
            self.debug_log.pop_front();
          }
          self
            .debug_log
            .push_back(format!("[tab {}] {}", tab_id.0, safe));
          request_redraw = true;
        }
      }
    }

    if let Some(clipboard_update) = clipboard_update {
      let limit = fastrender::ui::protocol_limits::MAX_CLIPBOARD_TEXT_BYTES;
      if clipboard_update.truncated {
        use fastrender::ui::{ToastKind, TOAST_DEFAULT_TTL};

        // Only show a toast when the originating tab is active to avoid surprising the user when a
        // background tab triggers copy/cut.
        if self.browser_state.active_tab_id() == Some(clipboard_update.tab_id) {
          let toast_text = format!(
            "Clipboard text too large; truncated to {} KiB",
            (limit / 1024).max(1)
          );
          self
            .chrome_toast
            .show(ToastKind::Warning, toast_text, std::time::Instant::now(), TOAST_DEFAULT_TTL);
        }

        let debug_line = format!(
          "[clipboard] truncated oversized clipboard payload from tab {} ({} bytes > {} bytes)",
          clipboard_update.tab_id.0, clipboard_update.original_bytes, limit
        );
        eprintln!("{debug_line}");
        if self.debug_log_ui_enabled {
          if self.debug_log.len() >= Self::DEBUG_LOG_MAX_LINES {
            self.debug_log.pop_front();
          }
          self.debug_log.push_back(debug_line);
        }
      }

      // Best-effort write to the OS clipboard, bounded to `MAX_CLIPBOARD_TEXT_BYTES`.
      os_clipboard::write_text(&clipboard_update.text);
      request_redraw = true;
    }

    if let fastrender::ui::WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      default_prevented,
      link_url,
      image_url,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
    } = &msg
    {
      // Always clear the matching pending request so the UI doesn't get stuck in a
      // "context menu pending" state when page JS suppresses the default menu via
      // `preventDefault()`.
      let matches_pending = self
        .pending_context_menu_request
        .as_ref()
        .is_some_and(|pending| pending.tab_id == *tab_id && pending.pos_css == *pos_css);

      if matches_pending {
        let pending = self.pending_context_menu_request.take();

          // Only open the egui menu when the default `contextmenu` was not suppressed and the tab
          // is still active.
          if !*default_prevented && self.browser_state.active_tab_id() == Some(*tab_id) {
            if let Some(pending) = pending {
              let link_url = link_url
                .as_deref()
                .and_then(fastrender::ui::url::sanitize_worker_url_for_ui);
              let image_url = image_url
                .as_deref()
                .and_then(fastrender::ui::url::sanitize_worker_url_for_ui);
              self.open_context_menu = Some(OpenContextMenu {
                tab_id: *tab_id,
                pos_css: *pos_css,
                anchor_points: pending.anchor_points,
              link_url,
              image_url,
              can_copy: *can_copy,
              can_cut: *can_cut,
              can_paste: *can_paste,
              can_select_all: *can_select_all,
              selected_idx: 0,
            });
            self.open_context_menu_rect = None;
            request_redraw = true;
          }
        }
      }
    }

    if let fastrender::ui::WorkerToUi::DateTimePickerOpened {
      tab_id,
      input_node_id,
      kind,
      value,
      anchor_css,
    } = &msg
    {
      if self.browser_state.active_tab_id() == Some(*tab_id) {
        let safe_value = fastrender::ui::untrusted::sanitize_untrusted_text(
          value,
          fastrender::ui::protocol_limits::MAX_INPUT_VALUE_BYTES,
        );
        let focus_before_open = self
          .open_date_time_picker
          .as_ref()
          .map(|existing| existing.focus_before_open)
          .unwrap_or_else(|| egui_focused_widget_id(&self.egui_ctx));

        let mut anchor_points = self
          .last_cursor_pos_points
          .or_else(|| self.page_rect_points.map(|rect| rect.center()))
          .unwrap_or_else(|| egui::pos2(0.0, 0.0));
        if self.page_input_tab == Some(*tab_id) {
          if let Some(mapping) = self.page_input_mapping {
            if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(*anchor_css) {
              anchor_points = egui::pos2(rect_points.min.x, rect_points.max.y);
            }
          }
        }
        self.open_date_time_picker = Some(OpenDateTimePicker {
          tab_id: *tab_id,
          input_node_id: *input_node_id,
          kind: *kind,
          focus_before_open,
          anchor_css: *anchor_css,
          anchor_points,
          value: safe_value,
        });
        self.open_date_time_picker_rect = None;
        request_redraw = true;
      }
    }

    if let fastrender::ui::WorkerToUi::FilePickerOpened {
      tab_id,
      input_node_id,
      multiple,
      accept,
      anchor_css: _,
    } = &msg
    {
      if self.browser_state.active_tab_id() == Some(*tab_id) {
        let accept = accept
          .as_deref()
          .map(|raw| {
            fastrender::ui::untrusted::sanitize_untrusted_text(
              raw,
              fastrender::ui::protocol_limits::MAX_ACCEPT_ATTR_BYTES,
            )
          })
          .filter(|s| !s.is_empty());

        // `rfd` dialogs are blocking; launch them on a helper thread so the egui frame loop stays
        // responsive.
        self.launch_native_file_picker_dialog(*tab_id, *input_node_id, *multiple, accept);
        request_redraw = true;
      }
    }

    let update = self.browser_state.apply_worker_msg(msg);
    history_changed |= update.history_changed;

    if let Some(frame_ready) = update.frame_ready {
      // Ignore stale frames for tabs that have already been closed.
      if self.browser_state.tab(frame_ready.tab_id).is_some() {
        // Coalesce uploads until the next `render_frame`: uploading each intermediate pixmap is
        // expensive.
        self.pending_frame_uploads.push(frame_ready);
        self.pending_frame_uploads.evict_to_budget(
          self.max_pending_frame_bytes,
          self.browser_state.active_tab_id(),
        );
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

          let focus_before_open = self
            .open_select_dropdown
            .as_ref()
            .map(|existing| existing.focus_before_open)
            .unwrap_or_else(|| egui_focused_widget_id(&self.egui_ctx));

          self.open_select_dropdown = Some(OpenSelectDropdown {
            tab_id: dropdown.tab_id,
            select_node_id: dropdown.select_node_id,
            control: dropdown.control,
            focus_before_open,
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
    WorkerMessageResult {
      request_redraw,
      history_changed,
    }
  }

  fn flush_pending_frame_uploads(&mut self) {
    let _span = self.trace.span("flush_pending_frame_uploads", "ui.upload");
    // Reset per-frame upload stats so the HUD remains stable when idle (or when uploads are
    // rate-limited).
    if let Some(hud) = self.hud.as_mut() {
      hud.uploads_this_frame = 0;
      hud.last_upload_bytes = 0;
      hud.last_upload_ms = 0.0;
    }

    let Some(tab_id) = self.browser_state.active_tab_id() else {
      self.next_page_upload_redraw = None;
      return;
    };

    // Only upload page pixmaps for the active tab. Background tabs keep their latest pixmap pending
    // in `pending_frame_uploads` so tab switches can upload immediately without burning GPU upload
    // time on every redraw (especially during resize).
    if !self.pending_frame_uploads.has_pending_for_tab(tab_id) {
      self.next_page_upload_redraw = None;
      return;
    }

    let now = std::time::Instant::now();

    // If we already have a texture for this tab, we can rate-limit uploads and let egui scale the
    // existing texture during interactive resize.
    if self.tab_textures.contains_key(&tab_id) {
      let min_interval = if self.resize_burst_active {
        Self::RESIZE_PAGE_UPLOAD_MIN_INTERVAL
      } else {
        Self::PAGE_UPLOAD_MIN_INTERVAL
      };
      if let Some(last) = self.last_page_upload_at {
        if let Some(earliest) = last.checked_add(min_interval) {
          if now < earliest {
            self.next_page_upload_redraw = Some(
              self
                .next_page_upload_redraw
                .map(|existing| existing.min(earliest))
                .unwrap_or(earliest),
            );
            return;
          }
        } else {
          // Overflow is vanishingly unlikely, but treat it as "not rate limited" so we don't wedge
          // the uploader.
          self.last_page_upload_at = None;
        }
      }
    }

    self.next_page_upload_redraw = None;

    let upload_start = self.hud.is_some().then(std::time::Instant::now);
    let Some(frame_ready) = self.pending_frame_uploads.take(tab_id) else {
      return;
    };

    // Ignore stale frames for tabs that have already been closed.
    if self.browser_state.tab(frame_ready.tab_id).is_none() {
      return;
    }

    let pixmap = frame_ready.pixmap;
    let uploaded_bytes = u64::from(pixmap.width())
      .saturating_mul(u64::from(pixmap.height()))
      .saturating_mul(4);
    if let Some(tex) = self.tab_textures.get_mut(&tab_id) {
      let recreated = tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
      if recreated {
        if let Some(hud) = self.hud.as_mut() {
          hud.page_texture_recreates_total = hud.page_texture_recreates_total.saturating_add(1);
        }
      }
    } else {
      let mut tex =
        fastrender::ui::WgpuPixmapTexture::new_page(&self.device, &mut self.egui_renderer, &pixmap);
      let recreated = tex.update(&self.device, &self.queue, &mut self.egui_renderer, &pixmap);
      if recreated {
        if let Some(hud) = self.hud.as_mut() {
          hud.page_texture_recreates_total = hud.page_texture_recreates_total.saturating_add(1);
        }
      }
      self.tab_textures.insert(tab_id, tex);
    }

    self.last_page_upload_at = Some(now);
    if let Some(hud) = self.hud.as_mut() {
      hud.uploads_this_frame = 1;
      hud.last_upload_bytes = uploaded_bytes;
      hud.last_upload_ms = upload_start
        .map(|t| t.elapsed().as_secs_f32() * 1000.0)
        .filter(|ms| ms.is_finite())
        .unwrap_or(0.0);
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
      // Even when the viewport is unchanged, session restore may still have a pending scroll
      // offset to apply once the tab's viewport has been established.
      self.maybe_restore_session_scroll(tab_id);
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

    self.maybe_restore_session_scroll(tab_id);
  }

  fn maybe_restore_session_scroll(&mut self, tab_id: fastrender::ui::TabId) {
    use fastrender::ui::{RepaintReason, UiToWorker};

    let Some(pos_css) = self.pending_scroll_restores.remove(&tab_id) else {
      return;
    };

    self.send_worker_msg(UiToWorker::ScrollTo { tab_id, pos_css });
    // Ensure the restored scroll becomes visible even if it lands after the initial paint.
    self.send_worker_msg(UiToWorker::RequestRepaint {
      tab_id,
      reason: RepaintReason::Scroll,
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

    // During interactive window drags we intentionally downscale DPR so the render worker has less
    // pixmap/GPU upload work to do (keeps the UI responsive).
    let dpr = if self.resize_burst_active {
      dpr * self.resize_dpr_scale
    } else {
      dpr
    };

    // Clamp *before* sending to the worker so we never request an absurd RGBA pixmap allocation.
    let clamp = self
      .browser_limits
      .clamp_viewport_and_dpr(viewport_css, dpr);
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

  fn sync_tab_notifications(&mut self, now: std::time::Instant) {
    use fastrender::ui::WARNING_TOAST_DEFAULT_TTL;
    use std::collections::HashSet;

    let live_tabs: HashSet<fastrender::ui::TabId> =
      self.browser_state.tabs.iter().map(|t| t.id).collect();
    self
      .tab_notifications
      .retain(|tab_id, _| live_tabs.contains(tab_id));

    for tab in &self.browser_state.tabs {
      let state = self.tab_notifications.entry(tab.id).or_default();
      let shown =
        state
          .warning_toast
          .update(tab.warning.as_deref(), now, WARNING_TOAST_DEFAULT_TTL);
      if shown {
        state.last_warning_toast = state.warning_toast.toast().map(|toast| toast.text.clone());
        state.warning_toast_expanded = false;
      }
      if state.warning_toast.toast().is_none() {
        state.warning_toast_expanded = false;
      }

      state.sync_error(tab.error.as_deref());
    }
  }

  fn next_warning_toast_deadline_for_active_tab(&self) -> Option<std::time::Instant> {
    let tab_id = self.browser_state.active_tab_id()?;
    let state = self.tab_notifications.get(&tab_id)?;
    state.warning_toast.next_deadline()
  }

  fn show_chrome_toast(&mut self, text: &str) {
    use fastrender::ui::{ToastKind, TOAST_DEFAULT_TTL};
    let now = std::time::Instant::now();
    self
      .chrome_toast
      .show(ToastKind::Info, text.to_string(), now, TOAST_DEFAULT_TTL);
  }

  fn render_chrome_toast(&mut self, ctx: &egui::Context) {
    use fastrender::ui::ToastKind;

    let Some(toast) = self.chrome_toast.toast() else {
      self.chrome_toast_rect = None;
      return;
    };

    let toast_text = toast.text.clone();
    let toast_kind = toast.kind;

    let theme_colors = self.theme.colors.clone();
    let theme_sizing = self.theme.sizing.clone();

    // Position relative to the central content area so the toast doesn't cover the status bar.
    let screen_rect = ctx.screen_rect();
    let content_rect = self.content_rect_points.unwrap_or(screen_rect);
    let bottom_inset = (screen_rect.max.y - content_rect.max.y).max(0.0);
    let right_inset = (screen_rect.max.x - content_rect.max.x).max(0.0);
    let margin = theme_sizing.padding.max(8.0) + 4.0;
    // Nudge upward so it doesn't overlap the warning toast when both are visible.
    let anchor_offset = egui::vec2(-margin - right_inset, -margin - bottom_inset - 56.0);

    let (stroke_color, icon) = match toast_kind {
      ToastKind::Info => (theme_colors.accent, fastrender::ui::BrowserIcon::Info),
      ToastKind::Warning => (
        theme_colors.warn,
        fastrender::ui::BrowserIcon::WarningInsecure,
      ),
      ToastKind::Error => (theme_colors.danger, fastrender::ui::BrowserIcon::Error),
    };

    let toast_id = egui::Id::new("fastr_chrome_toast");
    let popup = egui::Area::new(toast_id)
      .order(egui::Order::Foreground)
      .anchor(egui::Align2::RIGHT_BOTTOM, anchor_offset)
      .interactable(true)
      .show(ctx, |ui| {
        let mut dismiss = false;
        let fill = egui::Color32::from_rgba_unmultiplied(
          theme_colors.raised.r(),
          theme_colors.raised.g(),
          theme_colors.raised.b(),
          240,
        );
        let stroke = egui::Stroke::new(theme_sizing.stroke_width, stroke_color);
        let frame = egui::Frame::none()
          .fill(fill)
          .stroke(stroke)
          .rounding(egui::Rounding::same(theme_sizing.corner_radius))
          .inner_margin(egui::Margin::symmetric(
            theme_sizing.padding * 1.25,
            theme_sizing.padding,
          ));
        frame.show(ui, |ui| {
          ui.set_min_width(320.0);
          ui.horizontal(|ui| {
            let icon_side = ui.spacing().icon_width;
            let icon_resp = fastrender::ui::icon_tinted(ui, icon, icon_side, stroke_color);
            let title = Self::toast_title_from_text(&toast_text);
            icon_resp.widget_info({
              let label = format!("{title}");
              move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
            });

            let msg = ui.add(
              egui::Label::new(
                egui::RichText::new(toast_text.clone()).color(theme_colors.text_primary),
              )
              .wrap(true)
              .sense(egui::Sense::hover()),
            );
            msg.widget_info({
              let label = toast_text.clone();
              move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
              let close_resp = fastrender::ui::icon_button(
                ui,
                fastrender::ui::BrowserIcon::Close,
                "Dismiss",
                true,
              );
              close_resp
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Dismiss"));
              if close_resp.clicked() {
                dismiss = true;
              }
            });
          });
        });
        dismiss
      });

    self.chrome_toast_rect = Some(popup.response.rect);
    if popup.inner {
      self.chrome_toast.dismiss();
      self.window.request_redraw();
    }
  }

  fn render_warning_toast(&mut self, ctx: &egui::Context) {
    let Some(tab_id) = self.browser_state.active_tab_id() else {
      self.warning_toast_rect = None;
      return;
    };

    let motion = fastrender::ui::motion::UiMotion::from_ctx(ctx);
    let (toast_text, expanded_initial, toast_is_open) = {
      let Some(state) = self.tab_notifications.get(&tab_id) else {
        self.warning_toast_rect = None;
        return;
      };
      let live_toast_text = state.warning_toast.toast().map(|toast| toast.text.as_str());
      let toast_text = live_toast_text
        .or_else(|| state.last_warning_toast.as_deref())
        .unwrap_or("")
        .to_string();
      (
        toast_text,
        state.warning_toast_expanded,
        live_toast_text.is_some(),
      )
    };

    let toast_id = egui::Id::new(("fastr_warning_toast", tab_id.0));
    let open_t = motion.animate_bool(
      ctx,
      toast_id.with("open"),
      toast_is_open,
      motion.durations.popup_open,
    );
    let open_opacity = open_t.clamp(0.0, 1.0);
    if open_opacity <= 0.0 {
      self.warning_toast_rect = None;
      return;
    }

    let theme_colors = self.theme.colors.clone();
    let theme_sizing = self.theme.sizing.clone();

    // Position relative to the central content area so the toast stays within the page viewport
    // (and avoids any side panels / chrome insets).
    let screen_rect = ctx.screen_rect();
    let content_rect = self.content_rect_points.unwrap_or(screen_rect);
    let bottom_inset = (screen_rect.max.y - content_rect.max.y).max(0.0);
    let right_inset = (screen_rect.max.x - content_rect.max.x).max(0.0);
    let margin = theme_sizing.padding.max(8.0) + 4.0;
    let anchor_offset = egui::vec2(-margin - right_inset, -margin - bottom_inset);

    let mut expanded = expanded_initial && toast_is_open;
    let popup = egui::Area::new(toast_id)
      .order(egui::Order::Foreground)
      .anchor(egui::Align2::RIGHT_BOTTOM, anchor_offset)
      .interactable(toast_is_open)
      .show(ctx, |ui| {
        ui.set_enabled(toast_is_open);
        ui.visuals_mut().override_text_color =
          Some(Self::with_alpha(ui.visuals().text_color(), open_opacity));
        let mut dismiss = false;

        let fill = egui::Color32::from_rgba_unmultiplied(
          theme_colors.raised.r(),
          theme_colors.raised.g(),
          theme_colors.raised.b(),
          240,
        );
        let fill = Self::with_alpha(fill, open_opacity);
        let stroke = egui::Stroke::new(
          theme_sizing.stroke_width,
          Self::with_alpha(theme_colors.warn, open_opacity),
        );
        let frame = egui::Frame::none()
          .fill(fill)
          .stroke(stroke)
          .rounding(egui::Rounding::same(theme_sizing.corner_radius))
          .inner_margin(egui::Margin::symmetric(
            theme_sizing.padding * 1.25,
            theme_sizing.padding,
          ));

        let presentation = fastrender::ui::classify_warning_toast(Some(&toast_text))
          .unwrap_or_else(|| fastrender::ui::WarningToastPresentation {
            title: fastrender::ui::notifications::derive_warning_toast_title(&toast_text),
            summary: None,
            icon: fastrender::ui::WarningToastIcon::WarningInsecure,
          });
        let title_text = presentation.title.clone();
        let summary_text = presentation.summary.clone();
        let icon = match presentation.icon {
          fastrender::ui::WarningToastIcon::Info => fastrender::ui::BrowserIcon::Info,
          fastrender::ui::WarningToastIcon::ViewportClamp => fastrender::ui::BrowserIcon::ZoomOut,
          fastrender::ui::WarningToastIcon::WarningInsecure => {
            fastrender::ui::BrowserIcon::WarningInsecure
          }
        };

        frame.show(ui, |ui| {
          ui.set_min_width(340.0);

          let title_color = Self::with_alpha(theme_colors.text_primary, open_opacity);
          let summary_color = Self::with_alpha(theme_colors.text_secondary, open_opacity);
          let accent_color = Self::with_alpha(theme_colors.warn, open_opacity);
          let a11y_context = summary_text
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|summary| format!("{title_text}: {summary}"))
            .unwrap_or_else(|| title_text.clone());

          ui.vertical(|ui| {
            ui.horizontal(|ui| {
              let icon_side = ui.spacing().icon_width;
              let icon_resp = fastrender::ui::icon_tinted(ui, icon, icon_side, accent_color);
              let icon_a11y_label = a11y_context.clone();
              icon_resp.widget_info({
                let label = icon_a11y_label.clone();
                move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
              });

              // Make the title an explicit, focusable control so it is discoverable by both
              // keyboard navigation and screen readers (AccessKit).
              let title_resp = ui
                .push_id(toast_id.with("toggle_details"), |ui| {
                  ui.add(
                    egui::Button::new(egui::RichText::new(&title_text).strong().color(title_color))
                      .frame(false),
                  )
                })
                .inner
                .on_hover_text(&toast_text);

              // AccessKit may request explicit expand/collapse actions when the node exposes an
              // expanded state.
              let expand_requested = ui.input(|i| {
                i.has_accesskit_action_request(title_resp.id, accesskit::Action::Expand)
              });
              let collapse_requested = ui.input(|i| {
                i.has_accesskit_action_request(title_resp.id, accesskit::Action::Collapse)
              });

              let mut toggle_requested = title_resp.clicked();
              if title_resp.has_focus() {
                toggle_requested |= ui.input_mut(|i| {
                  i.consume_key(Default::default(), egui::Key::Enter)
                    || i.consume_key(Default::default(), egui::Key::Space)
                });
              }
              if expand_requested || collapse_requested {
                let desired = expand_requested && !collapse_requested;
                if expanded != desired {
                  expanded = desired;
                  title_resp.request_focus();
                }
              } else if toggle_requested {
                expanded = !expanded;
                title_resp.request_focus();
              }

              // Expose the title as an expand/collapse control to assistive tech (AccessKit) so
              // screen readers can announce the expanded/collapsed state.
              let title_a11y_label = if expanded {
                format!("Hide warning details: {a11y_context}")
              } else {
                format!("Show warning details: {a11y_context}")
              };
              title_resp.widget_info({
                let label = title_a11y_label.clone();
                move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
              });
              let _ = title_resp
                .ctx
                .accesskit_node_builder(title_resp.id, |builder| {
                  builder.set_expanded(expanded);
                  if expanded {
                    builder.add_action(accesskit::Action::Collapse);
                    builder.remove_action(accesskit::Action::Expand);
                  } else {
                    builder.add_action(accesskit::Action::Expand);
                    builder.remove_action(accesskit::Action::Collapse);
                  }
                });

              if title_resp.has_focus() {
                let focus_stroke = ui.visuals().selection.stroke;
                let focus_stroke = egui::Stroke::new(
                  focus_stroke.width,
                  Self::with_alpha(focus_stroke.color, open_opacity),
                );
                let rounding =
                  egui::Rounding::same((theme_sizing.corner_radius * 0.5).clamp(2.0, 6.0));
                ui.painter()
                  .rect_stroke(title_resp.rect.expand(2.0), rounding, focus_stroke);
              }

              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let close_resp = fastrender::ui::icon_button(
                  ui,
                  fastrender::ui::BrowserIcon::Close,
                  "Dismiss",
                  true,
                );
                close_resp
                  .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Dismiss"));
                if close_resp.clicked() {
                  dismiss = true;
                }
              });
            });

            if let Some(summary) = summary_text.as_deref().filter(|s| !s.trim().is_empty()) {
              ui.add_space(2.0);
              ui.add(
                egui::Label::new(egui::RichText::new(summary).color(summary_color)).wrap(true),
              );
            }

            if expanded {
              ui.add_space(6.0);
              ui.separator();
              ui.add_space(6.0);
              ui.add(
                egui::Label::new(egui::RichText::new(&toast_text).small().color(title_color))
                  .wrap(true),
              );
            }
          });
        });

        dismiss
      });

    self.warning_toast_rect = Some(popup.response.rect);

    if let Some(state) = self.tab_notifications.get_mut(&tab_id) {
      if popup.inner {
        state.warning_toast.dismiss();
        state.warning_toast_expanded = false;
        self.window.request_redraw();
      } else {
        state.warning_toast_expanded = expanded;
      }
    }
  }

  fn render_error_infobar(&mut self, ctx: &egui::Context) {
    use fastrender::ui::ChromeAction;

    let Some(tab_id) = self.browser_state.active_tab_id() else {
      self.error_infobar_rect = None;
      return;
    };
    let Some(tab) = self.browser_state.tab(tab_id) else {
      self.error_infobar_rect = None;
      return;
    };
    let error_now = tab
      .error
      .as_deref()
      .map(str::trim)
      .filter(|s| !s.is_empty());
    let error_is_open = error_now.is_some();

    let (last_error, details_open_initial) = self
      .tab_notifications
      .get(&tab_id)
      .map(|state| (state.last_error.clone(), state.error_details_open))
      .unwrap_or((None, false));

    let motion = fastrender::ui::motion::UiMotion::from_ctx(ctx);
    let infobar_id = egui::Id::new(("fastr_error_infobar", tab_id.0));
    let open_t = motion.animate_bool(
      ctx,
      infobar_id.with("open"),
      error_is_open,
      motion.durations.popup_open,
    );
    let open_opacity = open_t.clamp(0.0, 1.0);
    if open_opacity <= 0.0 {
      self.error_infobar_rect = None;
      return;
    }

    let theme_colors = self.theme.colors.clone();
    let theme_sizing = self.theme.sizing.clone();

    let screen_rect = ctx.screen_rect();
    let content_rect = self.content_rect_points.unwrap_or(screen_rect);
    let margin = theme_sizing.padding.max(8.0) + 4.0;
    let pos = egui::pos2(content_rect.min.x + margin, content_rect.min.y + margin);
    let available_width = (content_rect.width() - margin * 2.0).max(240.0);

    let error = error_now
      .map(str::to_string)
      .or(last_error)
      .unwrap_or_else(String::new);
    let first_line = error.lines().next().unwrap_or(&error).trim();
    let short_error = if first_line.chars().count() > 160 {
      format!("{}…", first_line.chars().take(160).collect::<String>())
    } else {
      first_line.to_string()
    };
    let retry_a11y_label = tab
      .current_url()
      .or(tab.committed_url.as_deref())
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(|url| format!("Retry navigation to {url}"))
      .unwrap_or_else(|| "Retry navigation".to_string());
    let mut details_open = details_open_initial && error_is_open;

    let popup = egui::Area::new(infobar_id)
      .order(egui::Order::Foreground)
      .fixed_pos(pos)
      .interactable(error_is_open)
      .show(ctx, |ui| {
        ui.set_enabled(error_is_open);
        ui.visuals_mut().override_text_color =
          Some(Self::with_alpha(ui.visuals().text_color(), open_opacity));
        let mut retry = false;

        let fill = egui::Color32::from_rgba_unmultiplied(
          theme_colors.raised.r(),
          theme_colors.raised.g(),
          theme_colors.raised.b(),
          245,
        );
        let fill = Self::with_alpha(fill, open_opacity);
        let stroke = egui::Stroke::new(
          theme_sizing.stroke_width,
          Self::with_alpha(theme_colors.danger, open_opacity),
        );
        let frame = egui::Frame::none()
          .fill(fill)
          .stroke(stroke)
          .rounding(egui::Rounding::same(theme_sizing.corner_radius))
          .inner_margin(egui::Margin::symmetric(
            theme_sizing.padding * 1.25,
            theme_sizing.padding,
          ));

        frame.show(ui, |ui| {
          ui.set_min_width(available_width);

          ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
              let icon_side = ui.spacing().icon_width;
              let icon_resp = fastrender::ui::icon_tinted(
                ui,
                fastrender::ui::BrowserIcon::Error,
                icon_side,
                Self::with_alpha(theme_colors.danger, open_opacity),
              );
              icon_resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Label, "Navigation failed")
              });

              ui.label(
                egui::RichText::new("Navigation failed")
                  .strong()
                  .color(Self::with_alpha(theme_colors.text_primary, open_opacity)),
              );
              ui.label(
                egui::RichText::new(&short_error)
                  .color(Self::with_alpha(theme_colors.text_primary, open_opacity)),
              );

              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let retry_resp = ui
                  .push_id(infobar_id.with("retry"), |ui| ui.button("Retry"))
                  .inner;
                retry_resp.widget_info({
                  let label = retry_a11y_label.clone();
                  move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
                });
                if retry_resp.clicked() {
                  retry = true;
                }

                let details_label = if details_open {
                  "Hide details"
                } else {
                  "Details"
                };
                let details_resp = ui
                  .push_id(infobar_id.with("toggle_details"), |ui| {
                    ui.button(details_label)
                  })
                  .inner;

                // AccessKit may request explicit expand/collapse actions when the node exposes an
                // expanded state.
                let expand_requested = ui.input(|i| {
                  i.has_accesskit_action_request(details_resp.id, accesskit::Action::Expand)
                });
                let collapse_requested = ui.input(|i| {
                  i.has_accesskit_action_request(details_resp.id, accesskit::Action::Collapse)
                });

                let mut toggle_requested = details_resp.clicked();
                if details_resp.has_focus() {
                  toggle_requested |= ui.input_mut(|i| {
                    i.consume_key(Default::default(), egui::Key::Enter)
                      || i.consume_key(Default::default(), egui::Key::Space)
                  });
                }

                if expand_requested || collapse_requested {
                  let desired = expand_requested && !collapse_requested;
                  if details_open != desired {
                    details_open = desired;
                    details_resp.request_focus();
                  }
                } else if toggle_requested {
                  details_open = !details_open;
                  details_resp.request_focus();
                }

                let details_a11y_label = if details_open {
                  "Hide error details"
                } else {
                  "Show error details"
                };
                details_resp.widget_info(|| {
                  egui::WidgetInfo::labeled(egui::WidgetType::Button, details_a11y_label)
                });
                let _ = details_resp
                  .ctx
                  .accesskit_node_builder(details_resp.id, |builder| {
                    builder.set_expanded(details_open);
                    if details_open {
                      builder.add_action(accesskit::Action::Collapse);
                      builder.remove_action(accesskit::Action::Expand);
                    } else {
                      builder.add_action(accesskit::Action::Expand);
                      builder.remove_action(accesskit::Action::Collapse);
                    }
                  });
              });
            });

            if details_open {
              ui.add_space(6.0);
              ui.separator();
              ui.add_space(6.0);
              ui.label(
                egui::RichText::new(&error)
                  .small()
                  .color(Self::with_alpha(theme_colors.text_primary, open_opacity)),
              );
            }
          });
        });

        retry
      });

    self.error_infobar_rect = Some(popup.response.rect);

    if let Some(state) = self.tab_notifications.get_mut(&tab_id) {
      state.error_details_open = details_open;
    }

    if popup.inner {
      self.handle_chrome_actions(vec![ChromeAction::Reload]);
      self.window.request_redraw();
    }
  }

  fn render_hud(&mut self, ctx: &egui::Context) {
    let Some(hud) = self.hud.as_mut() else {
      return;
    };
    hud.maybe_sample_worker_rates(std::time::Instant::now());

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
    let last_stage = tab.and_then(|t| t.stage).map(|s| s.as_str()).unwrap_or("-");

    let warning = tab.and_then(|t| t.warning.as_deref());
    let viewport_clamped = warning.is_some_and(|w| w.starts_with("Viewport clamped:"));

    let (viewport_css, dpr) = if self.viewport_cache_tab == self.browser_state.active_tab_id()
      && self.viewport_cache_dpr > 0.0
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

    match (
      hud.worker_msgs_per_sec,
      hud.worker_forwarded_msgs_per_sec,
      hud.worker_wakes_per_sec,
      hud.worker_wake_sent_per_sec,
      hud.worker_empty_wakes_per_sec,
      hud.worker_coalesced_wakes_per_sec,
      hud.worker_pending_msgs_estimate,
    ) {
      (Some(msgs), Some(in_msgs), Some(wakes), Some(sent), Some(empty), Some(coalesced), Some(pending))
        if msgs.is_finite()
          && in_msgs.is_finite()
          && wakes.is_finite()
          && sent.is_finite()
          && empty.is_finite()
          && coalesced.is_finite() =>
      {
        let _ = writeln!(
          &mut hud.text_buf,
          "worker: {msgs:.0} msg/s  in {in_msgs:.0}/s  {wakes:.0} wake/s  sent {sent:.0}/s  {empty:.0} empty/s  {coalesced:.0} coal/s  pend {pending}"
        );
      }
      _ => {
        let _ = writeln!(
          &mut hud.text_buf,
          "worker: - msg/s  in -/s  - wake/s  sent -/s  - empty/s  - coal/s  pend -"
        );
      }
    }

    match (hud.worker_msgs_per_nonempty_wake, hud.worker_max_drain_recent) {
      (Some(msg_per_wake), max_drain) if msg_per_wake.is_finite() => {
        let _ = writeln!(
          &mut hud.text_buf,
          "drain: last {}  max {}  msg/nonempty_wake {msg_per_wake:.2}",
          hud.worker_last_drain,
          max_drain
        );
      }
      _ => {
        let _ = writeln!(
          &mut hud.text_buf,
          "drain: last {}  max {}  msg/nonempty_wake -",
          hud.worker_last_drain,
          hud.worker_max_drain_recent
        );
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
      "wgpu: surface.configure={}  page_tex_recreate={}",
      hud.surface_configure_calls_total,
      hud.page_texture_recreates_total
    );
    if hud.last_upload_ms.is_finite() {
      let _ = writeln!(
        &mut hud.text_buf,
        "upload: {}  {:.2}ms  {} bytes",
        hud.uploads_this_frame,
        hud.last_upload_ms,
        hud.last_upload_bytes
      );
    } else {
      let _ = writeln!(
        &mut hud.text_buf,
        "upload: {}  - ms  {} bytes",
        hud.uploads_this_frame,
        hud.last_upload_bytes
      );
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

    if self.resize_burst_active {
      let ttl_left_ms = resize_burst_deadline(self.last_resize_event_at)
        .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()).as_millis())
        .unwrap_or(0);
      let _ = writeln!(
        &mut hud.text_buf,
        "resize_burst: yes (ttl {}ms, dpr×{:.2})",
        ttl_left_ms,
        self.resize_dpr_scale
      );
    } else {
      let _ = writeln!(&mut hud.text_buf, "resize_burst: no");
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
          .inner_margin(egui::Margin::symmetric(
            self.theme.sizing.padding,
            self.theme.sizing.padding * 0.75,
          ))
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
    let prev_rect = self.debug_log_overlay_rect;

    if !self.debug_log_ui_enabled {
      self.debug_log_overlay_rect = None;
      self.debug_log_overlay_pointer_capture = false;
      return;
    }
    let margin = self.theme.sizing.padding;

    if !self.debug_log_ui_open {
      let popup = egui::Area::new(egui::Id::new("fastr_debug_log_reopen"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-margin, -margin))
        .show(ctx, |ui| {
          let label = if self.debug_log.is_empty() {
            "Debug log".to_string()
          } else {
            format!("Debug log ({})", self.debug_log.len())
          };
          let button = egui::Button::new(egui::RichText::new(label).small());
          let resp = ui.add(button);
          let a11y_label = if self.debug_log.is_empty() {
            "Open debug log".to_string()
          } else {
            format!("Open debug log ({} lines)", self.debug_log.len())
          };
          resp.widget_info({
            let label = a11y_label;
            move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
          });
          if resp.clicked() {
            self.debug_log_ui_open = true;
          }
        });
      self.debug_log_overlay_rect = Some(popup.response.rect);
      self.update_page_hover_for_debug_log_overlay(ctx, prev_rect);
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
    let popup = egui::Window::new("Debug log")
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
          let clear_resp = ui.button("Clear");
          clear_resp
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear debug log"));
          if clear_resp.clicked() {
            self.debug_log.clear();
          }

          let copy_resp = ui.button("Copy all");
          copy_resp.widget_info(|| {
            egui::WidgetInfo::labeled(
              egui::WidgetType::Button,
              "Copy all debug log lines to clipboard",
            )
          });
          wants_copy = copy_resp.clicked();

          ui.separator();

          ui.label(egui::RichText::new("Filter:").small());
          let filter_resp = ui.add(
            egui::TextEdit::singleline(&mut self.debug_log_filter)
              .hint_text("text")
              .desired_width(160.0),
          );
          filter_resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, "Debug log filter")
          });
          if !self.debug_log_filter.is_empty() {
            let clear_filter = fastrender::ui::icon_button(
              ui,
              fastrender::ui::BrowserIcon::Close,
              "Clear filter",
              true,
            );
            clear_filter
              .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear filter"));
            if clear_filter.clicked() {
              self.debug_log_filter.clear();
            }
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
              ui.label(
                egui::RichText::new("No debug log lines yet.")
                  .italics()
                  .small(),
              );
              return;
            }

            for line in self.debug_log.iter().filter(|line| matches_filter(line)) {
              ui.label(egui::RichText::new(line).monospace().small());
            }
          });
      });

    self.debug_log_overlay_rect = popup.as_ref().map(|popup| popup.response.rect);
    self.debug_log_ui_open = open;

    self.update_page_hover_for_debug_log_overlay(ctx, prev_rect);
  }

  fn update_page_hover_for_debug_log_overlay(
    &mut self,
    ctx: &egui::Context,
    prev_rect: Option<egui::Rect>,
  ) {
    use fastrender::ui::CursorKind;
    use fastrender::ui::PointerButton;
    use fastrender::ui::UiToWorker;

    let cursor_pos_points = self
      .last_cursor_pos_points
      .or_else(|| ctx.input(|i| i.pointer.hover_pos()));
    let prev_contains_cursor =
      cursor_pos_points.is_some_and(|pos| prev_rect.is_some_and(|rect| rect.contains(pos)));
    let now_contains_cursor = cursor_pos_points.is_some_and(|pos| {
      self
        .debug_log_overlay_rect
        .is_some_and(|rect| rect.contains(pos))
    });

    if !self.pointer_captured && now_contains_cursor {
      // The debug log window/button is positioned as an overlay. If it appears under a stationary
      // cursor, clear any active page hover state so hovered-link status and cursor overrides do
      // not remain "stuck" beneath the overlay.
      let active_tab_id = self.browser_state.active_tab_id();
      let should_clear = self.cursor_in_page
        || active_tab_id.is_some_and(|tab_id| {
          self.browser_state.tab(tab_id).is_some_and(|tab| {
            tab.hovered_url.is_some() || !matches!(tab.cursor, CursorKind::Default)
          })
        });

      if should_clear {
        self.pending_pointer_move = None;
        self.cursor_in_page = false;

        if let Some(tab) = active_tab_id.and_then(|tab_id| self.browser_state.tab_mut(tab_id)) {
          tab.hovered_url = None;
          tab.cursor = CursorKind::Default;
        }

        if let Some(tab_id) = self.page_input_tab.or(active_tab_id) {
          self.send_worker_msg(UiToWorker::PointerMove {
            tab_id,
            pos_css: (-1.0, -1.0),
            button: PointerButton::None,
            modifiers: map_modifiers(self.modifiers),
          });
        }
      }
    }

    if !self.pointer_captured
      && !self.debug_log_overlay_pointer_capture
      && prev_contains_cursor
      && !now_contains_cursor
    {
      // The overlay moved/closed while the cursor stayed still. If the cursor is now inside the
      // page rect, request a hover re-sync so the hovered URL + cursor update immediately.
      if cursor_pos_points.is_some_and(|pos| {
        self
          .page_rect_points
          .is_some_and(|page_rect| page_rect.contains(pos))
      }) {
        self.hover_sync_pending = true;
      }
    }
  }

  fn with_alpha(color: egui::Color32, alpha: f32) -> egui::Color32 {
    let [r, g, b, a] = color.to_array();
    let a = ((a as f32) * alpha).round().clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
  }

  fn toast_title_from_text(text: &str) -> String {
    let first_line = text
      .lines()
      .map(str::trim)
      .find(|line| !line.is_empty())
      .unwrap_or("");
    let title = first_line
      .split_once(':')
      .map(|(head, _)| head.trim())
      .filter(|head| !head.is_empty())
      .unwrap_or(first_line);
    if title.is_empty() {
      return "Notification".to_string();
    }
    // Keep the title compact so it fits in the toast header.
    const MAX_CHARS: usize = 48;
    if title.chars().count() > MAX_CHARS {
      format!("{}…", title.chars().take(MAX_CHARS).collect::<String>())
    } else {
      title.to_string()
    }
  }

  fn scaled_clip_rect(rect: egui::Rect, pivot: egui::Align2, scale: f32) -> egui::Rect {
    if !scale.is_finite() {
      return rect;
    }
    let scale = scale.clamp(0.0, 1.0);
    let size = rect.size() * scale;
    match pivot {
      egui::Align2::LEFT_TOP => egui::Rect::from_min_size(rect.min, size),
      egui::Align2::LEFT_BOTTOM => {
        egui::Rect::from_min_size(egui::pos2(rect.min.x, rect.max.y - size.y), size)
      }
      _ => egui::Rect::from_center_size(rect.center(), size),
    }
  }

  fn render_context_menu(&mut self, ctx: &egui::Context) -> bool {
    use fastrender::ui::context_menu::{
      apply_page_context_menu_action, build_page_context_menu_entries, PageContextMenuAction,
      PageContextMenuBuildInput, PageContextMenuEntry,
    };
    use fastrender::ui::motion::UiMotion;
    use fastrender::ui::BrowserIcon;
    use fastrender::ui::ChromeAction;
    use fastrender::ui::UiToWorker;

    let motion = UiMotion::from_ctx(ctx);
    let open_t = motion.animate_bool(
      ctx,
      egui::Id::new("fastr_page_context_menu_open"),
      self.open_context_menu.is_some(),
      motion.durations.popup_open,
    );

    let mut session_dirty = false;
    let (
      tab_id,
      pos_css,
      anchor_points,
      link_url,
      image_url,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
      selected_idx,
    ) = match self.open_context_menu.as_ref() {
      Some(menu) => (
        menu.tab_id,
        menu.pos_css,
        menu.anchor_points,
        menu.link_url.clone(),
        menu.image_url.clone(),
        menu.can_copy,
        menu.can_cut,
        menu.can_paste,
        menu.can_select_all,
        menu.selected_idx,
      ),
      None => {
        self.open_context_menu_rect = None;
        return session_dirty;
      }
    };

    if self.browser_state.active_tab_id() != Some(tab_id) {
      self.close_context_menu();
      self.window.request_redraw();
      return session_dirty;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.close_context_menu();
      self.window.request_redraw();
      return session_dirty;
    }

    let page_url = self
      .browser_state
      .active_tab()
      .and_then(|tab| tab.committed_url.as_deref().or(tab.current_url.as_deref()));

    let entries = build_page_context_menu_entries(PageContextMenuBuildInput {
      link_url: link_url.as_deref(),
      image_url: image_url.as_deref(),
      page_url,
      bookmarks: &self.bookmarks,
      history_panel_open: self.history_panel_open,
      bookmarks_panel_open: self.bookmarks_panel_open,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
    });

    #[derive(Clone)]
    struct MenuItem {
      icon: BrowserIcon,
      label: String,
      action: PageContextMenuAction,
      checked: bool,
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
            (PageContextMenuAction::CopySelection, _) => BrowserIcon::Copy,
            (PageContextMenuAction::Cut, _) => BrowserIcon::Copy,
            (PageContextMenuAction::Paste, _) => BrowserIcon::Copy,
            (PageContextMenuAction::SelectAll, _) => BrowserIcon::Copy,
            (PageContextMenuAction::OpenImageInNewTab(_), _) => BrowserIcon::OpenInNewTab,
            (PageContextMenuAction::DownloadImage(_), _) => BrowserIcon::ArrowDown,
            (PageContextMenuAction::CopyImageAddress(_), _) => BrowserIcon::Copy,
            (PageContextMenuAction::OpenLinkInNewTab(_), _) => BrowserIcon::OpenInNewTab,
            (PageContextMenuAction::DownloadLink(_), _) => BrowserIcon::ArrowDown,
            (PageContextMenuAction::CopyLinkAddress(_), _) => BrowserIcon::Copy,
            (PageContextMenuAction::BookmarkLink(_), true)
            | (PageContextMenuAction::BookmarkPage(_), true) => BrowserIcon::BookmarkFilled,
            (PageContextMenuAction::BookmarkLink(_), false)
            | (PageContextMenuAction::BookmarkPage(_), false) => BrowserIcon::BookmarkOutline,
            (PageContextMenuAction::ToggleHistoryPanel, true) => BrowserIcon::Check,
            (PageContextMenuAction::ToggleHistoryPanel, false) => BrowserIcon::History,
            (PageContextMenuAction::ToggleBookmarksPanel, true) => BrowserIcon::Check,
            (PageContextMenuAction::ToggleBookmarksPanel, false) => BrowserIcon::BookmarkOutline,
            (PageContextMenuAction::Reload, _) => BrowserIcon::Reload,
          };
          menu_entries.push(MenuEntry::Item(MenuItem {
            icon,
            label: item.label.to_string(),
            action: item.action,
            checked: item.checked,
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
      return session_dirty;
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

    let open_opacity = open_t.clamp(0.0, 1.0);
    let open_scale = motion.popup_open_scale(open_t);
    let popup_rect_target = egui::Rect::from_min_size(
      egui::pos2(menu_origin.x, menu_origin.y),
      egui::vec2(menu_size.width, menu_size.height),
    );
    let clip_rect = Self::scaled_clip_rect(popup_rect_target, egui::Align2::LEFT_TOP, open_scale);

    let popup = egui::Area::new(popup_id)
      .order(egui::Order::Foreground)
      .fixed_pos(egui::pos2(menu_origin.x, menu_origin.y))
      .show(ctx, |ui| {
        ui.set_clip_rect(clip_rect);
        ui.visuals_mut().override_text_color =
          Some(Self::with_alpha(ui.visuals().text_color(), open_opacity));

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
        frame.fill = Self::with_alpha(frame.fill, open_opacity);
        frame.stroke.color = Self::with_alpha(frame.stroke.color, open_opacity);
        frame.shadow.color = Self::with_alpha(frame.shadow.color, open_opacity);

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
            response.widget_info({
              let label = fastrender::ui::context_menu::format_page_context_menu_a11y_label(
                &item.label,
                &item.action,
                item.checked,
              );
              move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
            });

            let item_id = popup_id.with(("item", idx));
            let hover_t = motion.animate_bool(
              ui.ctx(),
              item_id.with("hover"),
              response.hovered(),
              motion.durations.hover_fade,
            );
            let is_selected = idx == selected_idx;
            let selected_t = motion.animate_bool(
              ui.ctx(),
              item_id.with("selected"),
              is_selected,
              motion.durations.hover_fade,
            );

            let bg_rect = rect.shrink(1.0);
            if hover_t > 0.0 {
              ui.painter().rect_filled(
                bg_rect,
                item_rounding,
                Self::with_alpha(hover_fill, hover_t * open_opacity),
              );
            }
            if selected_t > 0.0 {
              ui.painter().rect_filled(
                bg_rect,
                item_rounding,
                Self::with_alpha(selected_fill, selected_t * open_opacity),
              );
            }

            ui.allocate_ui_at_rect(rect, |ui| {
              ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(10.0);
                let (_id, icon_rect) = ui.allocate_space(egui::vec2(16.0, MENU_ITEM_HEIGHT));
                fastrender::ui::paint_icon_in_rect(
                  ui,
                  icon_rect,
                  item.icon,
                  16.0,
                  ui.visuals().text_color(),
                );
                ui.add_space(8.0);
                ui.label(&item.label);
              });
            });

            if idx == selected_idx {
              response.request_focus();
            }

            let has_focus = response.has_focus() || is_selected;
            if has_focus {
              let focus_stroke = ui.visuals().selection.stroke;
              let focus_rect = rect.shrink(1.0);
              ui.painter()
                .rect_stroke(focus_rect, item_rounding, focus_stroke);
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
                let separator_color = Self::with_alpha(
                  ui.visuals().widgets.noninteractive.bg_stroke.color,
                  open_opacity,
                );
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
      return session_dirty;
    };

    match action {
      PageContextMenuAction::CopySelection => {
        self.send_worker_msg(UiToWorker::Copy { tab_id });
      }
      PageContextMenuAction::Cut => {
        self.send_worker_msg(UiToWorker::Cut { tab_id });
      }
      PageContextMenuAction::Paste => {
        if let Some(text) = os_clipboard::read_text() {
          self.send_worker_msg(UiToWorker::Paste { tab_id, text });
        }
      }
      PageContextMenuAction::SelectAll => {
        self.send_worker_msg(UiToWorker::SelectAll { tab_id });
      }
      PageContextMenuAction::CopyImageAddress(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          ctx.output_mut(|o| o.copied_text = url);
        }
      }
      PageContextMenuAction::DownloadImage(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          self.send_worker_msg(UiToWorker::StartDownload {
            tab_id,
            url,
            filename_hint: None,
          });
          // Downloads are shown in the right-side panel, so close other panels that share that space.
          self.history_panel_open = false;
          self.bookmarks_panel_open = false;
          if !self.downloads_panel_open {
            self.downloads_panel_request_focus = true;
          }
          self.downloads_panel_open = true;
        }
      }
      PageContextMenuAction::OpenImageInNewTab(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          session_dirty |= self.open_url_in_new_tab(url);
        }
      }
      PageContextMenuAction::CopyLinkAddress(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          ctx.output_mut(|o| o.copied_text = url);
        }
      }
      PageContextMenuAction::Reload => {
        self.handle_chrome_actions(vec![ChromeAction::Reload]);
      }
      PageContextMenuAction::DownloadLink(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          self.send_worker_msg(UiToWorker::StartDownload {
            tab_id,
            url,
            filename_hint: None,
          });
          // Downloads are shown in the right-side panel, so close other panels that share that space.
          self.history_panel_open = false;
          self.bookmarks_panel_open = false;
          self.history_panel_request_focus_search = false;
          self.bookmarks_manager.clear_transient();
          if !self.downloads_panel_open {
            self.downloads_panel_request_focus = true;
          }
          self.downloads_panel_open = true;
          self.page_has_focus = false;
        }
      }
      PageContextMenuAction::OpenLinkInNewTab(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          session_dirty |= self.open_url_in_new_tab(url);
        }
      }
      PageContextMenuAction::BookmarkLink(url) => {
        if let Some(url) = fastrender::ui::url::sanitize_worker_url_for_ui(&url) {
          let action = PageContextMenuAction::BookmarkLink(url);
          let result = apply_page_context_menu_action(
            &mut self.bookmarks,
            &mut self.history_panel_open,
            &mut self.bookmarks_panel_open,
            &action,
          );
          if result.bookmarks_changed {
            self.autosave_bookmarks();
            self.sync_about_newtab_bookmarks_snapshot();
          }
        }
      }
      PageContextMenuAction::BookmarkPage(url) => {
        // `Bookmark Page` uses the current page URL (also ultimately renderer-supplied); apply the
        // same scheme allowlist and length limits as other renderer-driven UI URL actions.
        let action = PageContextMenuAction::BookmarkPage(
          fastrender::ui::url::sanitize_worker_url_for_ui(&url).unwrap_or_default(),
        );
        let result = apply_page_context_menu_action(
          &mut self.bookmarks,
          &mut self.history_panel_open,
          &mut self.bookmarks_panel_open,
          &action,
        );
        if result.bookmarks_changed {
          self.autosave_bookmarks();
          self.sync_about_newtab_bookmarks_snapshot();
        }
      }
      PageContextMenuAction::ToggleHistoryPanel => {
        self.downloads_panel_open = false;
        let action = PageContextMenuAction::ToggleHistoryPanel;
        let result = apply_page_context_menu_action(
          &mut self.bookmarks,
          &mut self.history_panel_open,
          &mut self.bookmarks_panel_open,
          &action,
        );
        if result.bookmarks_changed {
          self.autosave_bookmarks();
          self.sync_about_newtab_bookmarks_snapshot();
        }
      }
      PageContextMenuAction::ToggleBookmarksPanel => {
        self.downloads_panel_open = false;
        let action = PageContextMenuAction::ToggleBookmarksPanel;
        let result = apply_page_context_menu_action(
          &mut self.bookmarks,
          &mut self.history_panel_open,
          &mut self.bookmarks_panel_open,
          &action,
        );
        if result.bookmarks_changed {
          self.autosave_bookmarks();
          self.sync_about_newtab_bookmarks_snapshot();
        }
      }
    }

    self.close_context_menu();
    self.window.request_redraw();

    session_dirty
  }

  fn render_downloads_panel(&mut self, ctx: &egui::Context) {
    use fastrender::ui::UiToWorker;

    let request_initial_focus = self.downloads_panel_request_focus && !ctx.wants_keyboard_input();
    self.downloads_panel_request_focus = false;

    let output = fastrender::ui::panels::downloads_panel_ui(
      ctx,
      fastrender::ui::panels::DownloadsPanelInput {
        downloads: &self.browser_state.downloads.downloads,
        theme: &self.theme,
        request_initial_focus,
      },
    );

    if output.close_requested {
      self.downloads_panel_open = false;
      self.downloads_panel_request_focus = false;
      self.page_has_focus = self.should_restore_page_focus();
    }

    for (tab_id, download_id) in output.cancel_requests {
      self.send_worker_msg(UiToWorker::CancelDownload {
        tab_id,
        download_id,
      });
    }
    for (tab_id, url) in output.retry_requests {
      self.send_worker_msg(UiToWorker::StartDownload {
        tab_id,
        url,
        filename_hint: None,
      });
    }

    for path in output.open_requests {
      open_file_with_os_default(&path);
    }
    for path in output.reveal_requests {
      reveal_file_in_os_file_manager(&path);
    }
  }

  fn select_dropdown_option_a11y_label(base: &str) -> String {
    let base = base.trim();
    if base.is_empty() {
      "Option".to_string()
    } else {
      base.to_string()
    }
  }

  fn select_dropdown_optgroup_a11y_label(label: &str, disabled: bool) -> String {
    let label = label.trim();
    let mut out = if label.is_empty() {
      "Group".to_string()
    } else {
      label.to_string()
    };
    if disabled {
      out.push_str(" (disabled)");
    }
    out
  }

  fn select_dropdown_focus_target_item_index(
    control: &fastrender::tree::box_tree::SelectControl,
  ) -> Option<usize> {
    use fastrender::tree::box_tree::SelectItem;

    let mut first_enabled: Option<usize> = None;
    for (idx, item) in control.items.iter().enumerate() {
      match item {
        SelectItem::OptGroupLabel { .. } => {}
        SelectItem::Option {
          selected, disabled, ..
        } => {
          if first_enabled.is_none() && !*disabled {
            first_enabled = Some(idx);
          }
          if *selected && !*disabled {
            return Some(idx);
          }
        }
      }
    }
    first_enabled
  }

  fn render_select_dropdown(&mut self, ctx: &egui::Context) {
    use fastrender::tree::box_tree::SelectItem;
    use fastrender::ui::motion::UiMotion;
    use fastrender::ui::UiToWorker;

    let motion = UiMotion::from_ctx(ctx);
    let open_t = motion.animate_bool(
      ctx,
      egui::Id::new("fastr_select_dropdown_open"),
      self.open_select_dropdown.is_some(),
      motion.durations.popup_open,
    );

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

    let open_opacity = open_t.clamp(0.0, 1.0);
    let open_scale = motion.popup_open_scale(open_t);
    // Used for the scale/clip open animation (the actual popup height may be smaller than the
    // placement max height if there are few items).
    let content_height = 26.0 * (control.items.len() as f32);
    let popup_height =
      content_height.min(inner_max_height) + popup_margin.top + popup_margin.bottom;
    let popup_width = placement.rect.width();
    let popup_rect_target = match placement.direction {
      fastrender::select_dropdown::SelectDropdownPopupDirection::Down => {
        egui::Rect::from_min_size(popup_pos, egui::vec2(popup_width, popup_height))
      }
      fastrender::select_dropdown::SelectDropdownPopupDirection::Up => egui::Rect::from_min_size(
        egui::pos2(popup_pos.x, popup_pos.y - popup_height),
        egui::vec2(popup_width, popup_height),
      ),
    };
    let clip_rect = Self::scaled_clip_rect(popup_rect_target, popup_pivot, open_scale);

    let popup = egui::Area::new(egui::Id::new((
      "fastr_select_dropdown_popup",
      tab_id.0,
      select_node_id,
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(popup_pos)
    .pivot(popup_pivot)
    .show(ctx, |ui| {
      ui.set_clip_rect(clip_rect);
      ui.visuals_mut().override_text_color =
        Some(Self::with_alpha(ui.visuals().text_color(), open_opacity));

      let mut frame = egui::Frame::popup(ui.style());
      frame.inner_margin = popup_margin;
      frame.rounding = egui::Rounding::same(theme_corner_radius);
      frame.shadow.extrusion = frame.shadow.extrusion.max(12.0);
      frame.fill = Self::with_alpha(frame.fill, open_opacity);
      frame.stroke.color = Self::with_alpha(frame.stroke.color, open_opacity);
      frame.shadow.color = Self::with_alpha(frame.shadow.color, open_opacity);

      let frame = frame.show(ui, |ui| {
        ui.set_min_width(inner_width);

        let mut clicked_item_idx: Option<usize> = None;
        let mut scroll_to_selected = scroll_to_selected;

        let visuals = ui.visuals().clone();
        let hover_bg = {
          // Use a subtle text-colored scrim so hover remains visible even when the theme's hovered
          // widget fill matches the popup background.
          let base = visuals.text_color();
          let alpha = if visuals.dark_mode { 24 } else { 14 };
          egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha)
        };

        // Respect the theme's selection fill (including alpha) so high-contrast themes remain
        // readable and don't get washed out by hard-coded opacity tweaks.
        let selection_fill = visuals.selection.bg_fill;

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
            let mut focus_requested = false;
            let focus_target_idx = Self::select_dropdown_focus_target_item_index(&control);

            for (idx, item) in control.items.iter().enumerate() {
              match item {
                SelectItem::OptGroupLabel { label, disabled } => {
                  ui.add_space(6.0);
                  let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 18.0),
                    egui::Sense::hover(),
                  );
                  response.widget_info({
                    let label = Self::select_dropdown_optgroup_a11y_label(label, *disabled);
                    move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
                  });
                  let label_color = if *disabled {
                    visuals.weak_text_color()
                  } else {
                    visuals.text_color()
                  };
                  ui.painter().text(
                    egui::pos2(rect.min.x + base_padding_x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    small_font.clone(),
                    Self::with_alpha(label_color, open_opacity),
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
                  let row_width = ui.available_width();
                  let (rect, response) = ui
                    .add_enabled_ui(!*disabled, |ui| {
                      ui.allocate_exact_size(
                        egui::vec2(row_width, row_height),
                        egui::Sense::click(),
                      )
                    })
                    .inner;

                  response.widget_info({
                    let label = Self::select_dropdown_option_a11y_label(base);
                    let selected = *selected;
                    move || {
                      egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        selected,
                        label.clone(),
                      )
                    }
                  });

                  if response.clicked() && !*disabled {
                    clicked_item_idx = Some(idx);
                  }

                  if *selected && scroll_to_selected {
                    response.scroll_to_me(Some(egui::Align::Center));
                    scroll_to_selected = false;
                  }

                  if !*disabled
                    && !focus_requested
                    && focus_target_idx.is_some_and(|target| target == idx)
                  {
                    response.request_focus();
                    focus_requested = true;
                  }

                  let hovered = response.hovered();
                  let row_id =
                    egui::Id::new(("fastr_select_dropdown_row", tab_id.0, select_node_id, idx));
                  let hover_t = motion.animate_bool(
                    ui.ctx(),
                    row_id.with("hover"),
                    hovered && !*disabled,
                    motion.durations.hover_fade,
                  );
                  let selected_t = motion.animate_bool(
                    ui.ctx(),
                    row_id.with("selected"),
                    *selected,
                    motion.durations.hover_fade,
                  );

                  let painter = ui.painter();
                  if hover_t > 0.0 || selected_t > 0.0 {
                    let bg_rect = rect.shrink2(egui::vec2(row_bg_inset_x, 0.0));
                    if hover_t > 0.0 {
                      painter.rect_filled(
                        bg_rect,
                        row_rounding,
                        Self::with_alpha(hover_bg, hover_t * open_opacity),
                      );
                    }
                    if selected_t > 0.0 {
                      painter.rect_filled(
                        bg_rect,
                        row_rounding,
                        Self::with_alpha(selection_fill, selected_t * open_opacity),
                      );
                    }
                  }

                  let has_focus = response.has_focus() || (*selected && !*disabled);
                  if has_focus {
                    let focus_stroke = ui.visuals().selection.stroke;
                    let focus_rect = rect.shrink2(egui::vec2(row_bg_inset_x, 0.0));
                    painter.rect_stroke(focus_rect, row_rounding, focus_stroke);
                  }

                  let text_color = if *disabled {
                    visuals.weak_text_color()
                  } else {
                    visuals.text_color()
                  };
                  let text_color = Self::with_alpha(text_color, open_opacity);

                  let mut x = rect.min.x + base_padding_x;
                  if selected_t > 0.0 {
                    let check_rect = egui::Rect::from_min_max(
                      egui::pos2(x, rect.top()),
                      egui::pos2(x + check_col_width, rect.bottom()),
                    );
                    fastrender::ui::paint_icon_in_rect(
                      ui,
                      check_rect,
                      fastrender::ui::BrowserIcon::Check,
                      14.0,
                      Self::with_alpha(visuals.text_color(), selected_t * open_opacity),
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

  fn render_date_time_picker_value_input(
    ui: &mut egui::Ui,
    tab_id: fastrender::ui::TabId,
    input_node_id: usize,
    value: &mut String,
    request_initial_focus: bool,
  ) -> egui::Response {
    const VALUE_LABEL: &str = "Value";

    ui.label(VALUE_LABEL);
    let text_id = ui.make_persistent_id(("dt_picker_value", tab_id.0, input_node_id));
    let resp = ui.add(
      egui::TextEdit::singleline(value)
        .id(text_id)
        .desired_width(220.0),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, VALUE_LABEL));
    #[cfg(test)]
    ui.ctx().data_mut(|d| {
      d.insert_temp(
        egui::Id::new("test_dt_picker_value_label"),
        VALUE_LABEL.to_string(),
      );
    });
    if request_initial_focus {
      resp.request_focus();
    }
    resp
  }

  fn render_date_time_picker(&mut self, ctx: &egui::Context) {
    use fastrender::ui::messages::DateTimeInputKind;
    use fastrender::ui::UiToWorker;

    let (tab_id, input_node_id, kind, anchor_css, fallback_anchor_points) =
      match self.open_date_time_picker.as_ref() {
        Some(picker) => (
          picker.tab_id,
          picker.input_node_id,
          picker.kind,
          picker.anchor_css,
          picker.anchor_points,
        ),
        None => {
          self.open_date_time_picker_rect = None;
          return;
        }
      };

    // When the popup first opens, force keyboard focus into it so keyboard-only workflows (and
    // screen reader focus) work without requiring an extra click.
    let request_initial_focus = self.open_date_time_picker_rect.is_none();

    let mut anchor_pos_points = fallback_anchor_points;
    if let Some(mapping) = self.page_input_mapping {
      if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
        anchor_pos_points = egui::pos2(rect_points.min.x, rect_points.max.y);
      }
    }

    if self.browser_state.active_tab_id() != Some(tab_id) {
      self.cancel_date_time_picker();
      self.window.request_redraw();
      return;
    }

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.cancel_date_time_picker();
      self.window.request_redraw();
      return;
    }

    enum Action {
      Choose(String),
      Cancel,
    }

    fn kind_label(kind: DateTimeInputKind) -> &'static str {
      match kind {
        DateTimeInputKind::Date => "Date",
        DateTimeInputKind::Time => "Time",
        DateTimeInputKind::DateTimeLocal => "DateTimeLocal",
        DateTimeInputKind::Month => "Month",
        DateTimeInputKind::Week => "Week",
      }
    }

    let popup = egui::Area::new(egui::Id::new((
      "fastr_date_time_picker_popup",
      tab_id.0,
      input_node_id,
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(anchor_pos_points)
    .show(ctx, |ui| {
      let frame = egui::Frame::popup(ui.style()).show(ui, |ui| {
        let Some(picker) = self.open_date_time_picker.as_mut() else {
          return None;
        };

        ui.label(egui::RichText::new(kind_label(kind)).strong());
        Self::render_date_time_picker_value_input(
          ui,
          tab_id,
          input_node_id,
          &mut picker.value,
          request_initial_focus,
        );

        let mut action: Option<Action> = None;
        ui.horizontal(|ui| {
          if ui.button("Apply").clicked() {
            action = Some(Action::Choose(picker.value.clone()));
          }
          if ui.button("Cancel").clicked() {
            action = Some(Action::Cancel);
          }
        });

        action
      });

      (frame.response.rect, frame.inner)
    });

    let (popup_rect, action) = popup.inner;
    self.open_date_time_picker_rect = Some(popup_rect);

    match action {
      Some(Action::Choose(value)) => {
        self.send_worker_msg(UiToWorker::date_time_picker_choose(
          tab_id,
          input_node_id,
          value,
        ));
        self.close_date_time_picker();
        self.window.request_redraw();
      }
      Some(Action::Cancel) => {
        self.cancel_date_time_picker();
        self.window.request_redraw();
      }
      None => {}
    }
  }

  fn render_file_picker(&mut self, ctx: &egui::Context) {
    use fastrender::ui::UiToWorker;
    use std::path::PathBuf;
 
    let (tab_id, input_node_id, multiple, anchor_css, fallback_anchor_points) =
      match self.open_file_picker.as_ref() {
        Some(picker) => (
          picker.tab_id,
          picker.input_node_id,
          picker.multiple,
          picker.anchor_css,
          picker.anchor_points,
        ),
        None => {
          self.open_file_picker_rect = None;
          return;
        }
      };
 
    // When the popup first opens, force keyboard focus into it so keyboard-only workflows work
    // without requiring an extra click.
    let request_initial_focus = self.open_file_picker_rect.is_none();
 
    let mut anchor_pos_points = fallback_anchor_points;
    if let Some(mapping) = self.page_input_mapping {
      if let Some(rect_points) = mapping.rect_css_to_rect_points_clamped(anchor_css) {
        anchor_pos_points = egui::pos2(rect_points.min.x, rect_points.max.y);
      }
    }
 
    if self.browser_state.active_tab_id() != Some(tab_id) {
      self.cancel_file_picker();
      self.window.request_redraw();
      return;
    }
 
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
      self.cancel_file_picker();
      self.window.request_redraw();
      return;
    }
 
    enum Action {
      Choose(String),
      Cancel,
    }
 
    let popup = egui::Area::new(egui::Id::new((
      "fastr_file_picker_popup",
      tab_id.0,
      input_node_id,
    )))
    .order(egui::Order::Foreground)
    .fixed_pos(anchor_pos_points)
    .show(ctx, |ui| {
      let frame = egui::Frame::popup(ui.style()).show(ui, |ui| {
        let Some(picker) = self.open_file_picker.as_mut() else {
          return None;
        };
 
        if picker.multiple {
          ui.label(egui::RichText::new("Multiple selection (separate paths with ';')").small());
        }
        if let Some(accept) = picker.accept.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
          ui.label(egui::RichText::new(format!("Accept: {accept}")).small());
        }
 
        let hint = if picker.multiple { "Path;path;..." } else { "Path" };
        let resp = ui.add(
          egui::TextEdit::singleline(&mut picker.draft)
            .desired_width(360.0)
            .hint_text(hint),
        );
        if request_initial_focus {
          resp.request_focus();
        }
 
        let mut action: Option<Action> = None;
        ui.horizontal(|ui| {
          if ui.button("OK").clicked() {
            action = Some(Action::Choose(picker.draft.clone()));
          }
          if ui.button("Cancel").clicked() {
            action = Some(Action::Cancel);
          }
        });
 
        action
      });
 
      (frame.response.rect, frame.inner)
    });
 
    let (popup_rect, action) = popup.inner;
    self.open_file_picker_rect = Some(popup_rect);
 
    match action {
      Some(Action::Choose(draft)) => {
        let mut paths: Vec<PathBuf> = Vec::new();
        let trimmed = draft.trim();
        if !trimmed.is_empty() {
          if multiple {
            for part in trimmed.split(';') {
              let part = part.trim();
              if part.is_empty() {
                continue;
              }
              paths.push(PathBuf::from(part));
            }
          } else {
            paths.push(PathBuf::from(trimmed));
          }
        }
 
        self.send_worker_msg(UiToWorker::FilePickerChoose {
          tab_id,
          input_node_id,
          paths,
        });
        self.close_file_picker();
        self.window.request_redraw();
      }
      Some(Action::Cancel) => {
        self.cancel_file_picker();
        self.window.request_redraw();
      }
      None => {}
    }
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

    // Avoid updating page hover state while the pointer is interacting with egui-owned overlays.
    if self.cursor_over_egui_overlay(pos_points) {
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

  fn should_restore_page_focus(&self) -> bool {
    should_restore_page_focus_for_state(
      self.browser_state.chrome.address_bar_has_focus,
      self.bookmarks_panel_open,
      self.history_panel_open,
      self.downloads_panel_open,
      self.clear_browsing_data_dialog_open,
      self.browser_state.chrome.tab_search.open,
      self
        .browser_state
        .active_tab()
        .is_some_and(|tab| tab.find.open),
    )
  }

  fn refresh_chrome_text_focus_from_egui(&mut self, ctx: &egui::Context) {
    // In the egui backend, `Context::wants_keyboard_input` is the most reliable way to detect
    // whether any egui `TextEdit` currently owns keyboard input. We cache it into an egui-free flag
    // so the winit input routing path can make the same decisions without depending on egui.
    //
    // Note: `address_bar_has_focus` is included defensively since some chrome states keep the
    // address bar "focused" even while transitioning between display/edit widgets.
    self.chrome_has_text_focus = ctx.wants_keyboard_input() || self.browser_state.chrome.address_bar_has_focus;
  }

  fn handle_profile_shortcuts(&mut self, key: winit::event::VirtualKeyCode) -> bool {
    use fastrender::ui::ChromeAction;

    match profile_shortcut_action(self.modifiers, key) {
      // Most chrome/profile shortcuts are handled inside the egui frame (`ui::chrome_ui`) so we can
      // apply egui focus rules consistently. Avoid handling those shortcuts here to prevent double
      // execution (winit handler + egui handler).
      Some(
        ProfileShortcutAction::ToggleBookmarkForActiveTab
        | ProfileShortcutAction::ToggleHistoryPanel
        | ProfileShortcutAction::ToggleBookmarksManager,
      ) => false,
      Some(ProfileShortcutAction::ClearHistory) => {
        // Match canonical browser UX: Ctrl/Cmd+Shift+Delete opens a "Clear browsing data" dialog
        // rather than immediately wiping data.
        self.clear_browsing_data_range = fastrender::ui::ClearBrowsingDataRange::default();
        self.handle_chrome_actions(vec![ChromeAction::OpenClearBrowsingDataDialog]);
        true
      }
      None => false,
    }
  }

  fn autosave_bookmarks(&mut self) {
    self.profile_bookmarks_dirty = true;
  }

  fn sync_about_newtab_bookmarks_snapshot(&self) {
    fastrender::ui::about_pages::sync_about_page_snapshot_bookmarks_from_bookmark_store(
      &self.bookmarks,
    );
  }

  fn toggle_bookmark_for_active_tab(&mut self) {
    let Some((url, title)) = self.browser_state.active_tab().and_then(|tab| {
      let url = tab
        .committed_url
        .as_deref()
        .or(tab.current_url.as_deref())?;
      let title = tab
        .committed_title
        .as_deref()
        .or(tab.title.as_deref())
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string);
      Some((url.to_string(), title))
    }) else {
      return;
    };

    self.bookmarks.toggle(&url, title.as_deref());
    self.autosave_bookmarks();
    self.sync_about_newtab_bookmarks_snapshot();
  }

  fn sync_history_after_mutation(&mut self, flush: bool) {
    // Keep omnibox history suggestions consistent with the canonical global store.
    self.browser_state.visited.clear();
    self.browser_state.seed_visited_from_history();
    self.browser_state.chrome.omnibox.reset();

    fastrender::ui::about_pages::sync_about_page_snapshot_history_from_global_history_store(
      &self.browser_state.history,
    );
    self.window.request_redraw();
    self.profile_history_dirty = true;
    if flush {
      self.profile_history_flush_requested = true;
    }
  }

  fn delete_history_entry_at(&mut self, index: usize) {
    if self.browser_state.history.remove_at(index).is_some() {
      self.sync_history_after_mutation(false);
    }
  }

  fn clear_browsing_data(&mut self, range: fastrender::ui::ClearBrowsingDataRange) {
    self.browser_state.history.clear_browsing_data_range(range);
    self.sync_history_after_mutation(true);
  }

  fn clear_history(&mut self) {
    self.clear_browsing_data(fastrender::ui::ClearBrowsingDataRange::AllTime);
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
    if let Some(tab) = self.browser_state.tab_mut(tab_id) {
      tab.hovered_url = None;
      tab.cursor = fastrender::ui::CursorKind::Default;
    }
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

    let _span = self
      .trace
      .span("handle_winit_input_event", "ui.input");

    if let ChromeA11yBackend::FastRender(a11y) = &mut self.chrome_a11y {
      a11y.process_window_event(&self.window, event);
    }

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
        self.debug_log_overlay_pointer_capture = false;
        self.touch_gesture.reset();
        if self.open_select_dropdown.is_some() {
          self.cancel_select_dropdown();
          self.window.request_redraw();
        }
        if self.open_date_time_picker.is_some() {
          self.cancel_date_time_picker();
          self.window.request_redraw();
        }
        if self.open_file_picker.is_some() {
          self.cancel_file_picker();
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
        if self.browser_state.chrome.dragging_tab_id.is_some() {
          self.browser_state.chrome.clear_tab_drag();
          self.window.request_redraw();
        }
      }
      WindowEvent::CursorLeft { .. } => {
        let had_pointer_capture = self.pointer_captured;
        let had_scrollbar_drag = self.scrollbar_drag.is_some();
        let had_cursor_in_page = self.cursor_in_page;
        let had_debug_log_pointer_capture = self.debug_log_overlay_pointer_capture;
        let had_cursor_near_scrollbars = self
          .last_cursor_pos_points
          .is_some_and(|pos| self.cursor_near_overlay_scrollbars(pos));
        let had_context_menu =
          self.open_context_menu.is_some() || self.pending_context_menu_request.is_some();

        // Winit does not provide cursor coordinates when leaving the window. Clear our cached
        // position so hover updates are not suppressed by stale dropdown rect checks.
        self.last_cursor_pos_points = None;
        self.debug_log_overlay_pointer_capture = false;
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
          || had_debug_log_pointer_capture
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
            PendingScrollDrag::push(&mut self.pending_scroll_drag, tab_id, delta_css);
          }
          self.window.request_redraw();
          return;
        }

        if self.clear_browsing_data_dialog_open {
          return;
        }

        if !self.pointer_captured
          && self
            .debug_log_overlay_rect
            .is_some_and(|rect| rect.contains(pos_points))
        {
          // Treat the debug log UI as a true overlay. If the cursor moves from the page into the
          // debug log window/button, ensure the worker receives a hover-clear sentinel so the
          // hovered URL/cursor state doesn't remain "stuck" under the overlay.
          //
          // Also drop any coalesced pointer move from earlier CursorMoved events in this frame so
          // we don't forward a stale hover update when the cursor leaves the overlay before the
          // next redraw flush.
          let active_tab_id = self.browser_state.active_tab_id();
          let should_clear = self.cursor_in_page
            || active_tab_id.is_some_and(|tab_id| {
              self.browser_state.tab(tab_id).is_some_and(|tab| {
                tab.hovered_url.is_some()
                  || !matches!(tab.cursor, fastrender::ui::CursorKind::Default)
              })
            });

          self.pending_pointer_move = None;
          self.cursor_in_page = false;

          if should_clear {
            if let Some(tab) = active_tab_id.and_then(|tab_id| self.browser_state.tab_mut(tab_id)) {
              tab.hovered_url = None;
              tab.cursor = fastrender::ui::CursorKind::Default;
            }

            if let Some(tab_id) = self.page_input_tab.or(active_tab_id) {
              self.send_worker_msg(fastrender::ui::UiToWorker::PointerMove {
                tab_id,
                pos_css: (-1.0, -1.0),
                button: fastrender::ui::PointerButton::None,
                modifiers: map_modifiers(self.modifiers),
              });
            }

            // Ensure hovered URL and cursor updates paint immediately, even if the worker doesn't
            // send an updated hover state until the next frame.
            self.window.request_redraw();
          }
          return;
        }

        if !self.pointer_captured && self.cursor_over_egui_overlay(pos_points) {
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
      WindowEvent::MouseWheel { delta, .. } => {
        // Handle page scrolling from native wheel/trackpad events.
        //
        // Historically the egui backend forwarded wheel deltas by inspecting egui input events
        // inside `render_frame`. The compositor/non-egui backend does not have access to that path,
        // so we forward directly from the winit event stream here.

        // Ctrl/Cmd+wheel is treated as a zoom gesture (handled elsewhere), so do not forward it as
        // scroll input.
        if map_modifiers(self.modifiers).command() {
          return;
        }

        // Treat the clear browsing data dialog as a modal: do not scroll the page beneath it.
        if self.clear_browsing_data_dialog_open {
          return;
        }

        let Some(pos_points) = self.last_cursor_pos_points else {
          return;
        };

        // Best-effort popup UX: if a wheel scroll happens outside an open picker/dropdown, close
        // it; if it happens inside the popup, treat the wheel as UI-only (handled by egui) and do
        // not forward it to the page worker.
        let mut wheel_blocked_by_popup = false;
        if self.open_select_dropdown.is_some() {
          if self
            .open_select_dropdown_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            wheel_blocked_by_popup = true;
          } else {
            self.cancel_select_dropdown();
            self.window.request_redraw();
          }
        }
        if self.open_date_time_picker.is_some() {
          if self
            .open_date_time_picker_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            wheel_blocked_by_popup = true;
          } else {
            self.cancel_date_time_picker();
            self.window.request_redraw();
          }
        }
        if self.open_file_picker.is_some() {
          if self
            .open_file_picker_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            wheel_blocked_by_popup = true;
          } else {
            self.cancel_file_picker();
            self.window.request_redraw();
          }
        }
        if wheel_blocked_by_popup {
          return;
        }

        // Do not forward wheel scrolls when the page is showing a stale frame under a loading
        // overlay. This matches the pointer-input blocking logic used for other event types.
        if self.page_loading_overlay_blocks_input {
          return;
        }

        // Treat egui overlay UI surfaces (context menus, debug log, etc.) as true overlays: wheel
        // scrolling over them should not scroll the underlying page.
        if self.cursor_over_egui_overlay(pos_points) {
          return;
        }

        // Only forward wheel events when the cursor is inside the drawn page region.
        if !self
          .page_rect_points
          .is_some_and(|rect| rect.contains(pos_points))
        {
          return;
        }

        let Some(tab_id) = self.page_input_tab.or(self.browser_state.active_tab_id()) else {
          return;
        };
        let Some(mapping) = self.page_input_mapping else {
          return;
        };

        let wheel_delta = fastrender::ui::WheelDelta::from_winit(*delta, self.pixels_per_point);
        let Some(delta_css) = mapping.wheel_delta_to_delta_css(wheel_delta) else {
          return;
        };
        if delta_css.0 == 0.0 && delta_css.1 == 0.0 {
          return;
        }

        // Treat wheel scrolling over overlay scrollbars as viewport scrolling (like browsers): do
        // not route the delta to nested element scrollers via hit-testing.
        let pointer_css = if self.cursor_over_overlay_scrollbars(pos_points) {
          None
        } else {
          mapping.pos_points_to_pos_css_clamped(pos_points)
        };

        self.send_worker_msg(fastrender::ui::UiToWorker::Scroll {
          tab_id,
          delta_css,
          pointer_css,
        });
        self.window.request_redraw();
      }
      WindowEvent::Touch(touch) => {
        use winit::event::TouchPhase;

        // While the tab search overlay is open, treat touch interactions as UI-only (handled by
        // egui) and do not forward them to the page worker.
        if self.browser_state.chrome.tab_search.open {
          return;
        }
        if self.clear_browsing_data_dialog_open {
          return;
        }
        let pos_points = egui::pos2(
          touch.location.x as f32 / self.pixels_per_point,
          touch.location.y as f32 / self.pixels_per_point,
        );
        // Keep `last_cursor_pos_points` updated so helper logic (e.g. file drops, keyboard context
        // menu anchoring) can reuse the most recent touch position.
        self.last_cursor_pos_points = Some(pos_points);
        let now = std::time::Instant::now();

        match touch.phase {
          TouchPhase::Started => {
            // Mirror the mouse-input behaviour: touching outside the page should clear page focus
            // immediately.
            if fastrender::ui::input_routing::should_clear_page_focus_on_pointer_press(
              self.page_rect_points,
              pos_points,
            ) {
              self.page_has_focus = false;
            }

            // If a popup is open, touches inside it are handled by egui. Any touch outside should
            // dismiss it before we start a new page gesture.
            if self.open_context_menu.is_some() {
              if self
                .open_context_menu_rect
                .is_some_and(|rect| rect.contains(pos_points))
              {
                return;
              }
              self.close_context_menu();
              self.window.request_redraw();
            }

            if self.open_select_dropdown.is_some() {
              if self
                .open_select_dropdown_rect
                .is_some_and(|rect| rect.contains(pos_points))
              {
                return;
              }
              let clicked_select_control =
                self.open_select_dropdown.as_ref().is_some_and(|dropdown| {
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

            if self.open_date_time_picker.is_some() {
              if self
                .open_date_time_picker_rect
                .is_some_and(|rect| rect.contains(pos_points))
              {
                return;
              }
              let clicked_input_control = self.open_date_time_picker.as_ref().is_some_and(|picker| {
                self
                  .page_input_mapping
                  .and_then(|mapping| mapping.rect_css_to_rect_points_clamped(picker.anchor_css))
                  .is_some_and(|rect_points| rect_points.contains(pos_points))
              });
              self.cancel_date_time_picker();
              self.window.request_redraw();
              if clicked_input_control {
                return;
              }
            }

            if self.open_file_picker.is_some() {
              if self
                .open_file_picker_rect
                .is_some_and(|rect| rect.contains(pos_points))
              {
                return;
              }
              let clicked_input_control = self.open_file_picker.as_ref().is_some_and(|picker| {
                self
                  .page_input_mapping
                  .and_then(|mapping| mapping.rect_css_to_rect_points_clamped(picker.anchor_css))
                  .is_some_and(|rect_points| rect_points.contains(pos_points))
              });
              self.cancel_file_picker();
              self.window.request_redraw();
              if clicked_input_control {
                return;
              }
            }

            if self.cursor_over_egui_overlay(pos_points) {
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

            // Only start touch gestures that began on the rendered page (i.e. not on browser
            // chrome/widgets).
            let Some(page_rect) = self.page_rect_points else {
              return;
            };
            if !page_rect.contains(pos_points) {
              return;
            }
            if self.cursor_over_overlay_scrollbars(pos_points) {
              return;
            }
            if self.page_input_tab.is_none() || self.page_input_mapping.is_none() {
              return;
            }

            self.touch_gesture.touch_start(touch.id, now, (pos_points.x, pos_points.y));
          }
          TouchPhase::Moved => {
            self
              .touch_gesture
              .touch_move(touch.id, (pos_points.x, pos_points.y));
          }
          TouchPhase::Ended => {
            let action =
              self
                .touch_gesture
                .touch_end(touch.id, now, (pos_points.x, pos_points.y));
            let Some(action) = action else {
              return;
            };

            match action {
              TouchGestureAction::Tap { pos_points } => {
                let pos_points = egui::pos2(pos_points.0, pos_points.1);
                let Some(rect) = self.page_rect_points else {
                  return;
                };
                if !rect.contains(pos_points) {
                  return;
                }
                if self.cursor_over_egui_overlay(pos_points)
                  || self.cursor_over_overlay_scrollbars(pos_points)
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

                self.page_has_focus = true;
                self.cursor_in_page = true;
                // Touch taps are always single-clicks.
                self.send_worker_msg(fastrender::ui::UiToWorker::PointerDown {
                  tab_id,
                  pos_css,
                  button: fastrender::ui::PointerButton::Primary,
                  modifiers: map_modifiers(self.modifiers),
                  click_count: 1,
                });
                self.send_worker_msg(fastrender::ui::UiToWorker::PointerUp {
                  tab_id,
                  pos_css,
                  button: fastrender::ui::PointerButton::Primary,
                  modifiers: map_modifiers(self.modifiers),
                });
                self.window.request_redraw();
              }
              TouchGestureAction::ContextMenu { pos_points } => {
                let pos_points = egui::pos2(pos_points.0, pos_points.1);
                if self.page_loading_overlay_blocks_input
                  && self
                    .page_rect_points
                    .is_some_and(|page_rect| page_rect.contains(pos_points))
                {
                  return;
                }
                let Some(rect) = self.page_rect_points else {
                  return;
                };
                if !rect.contains(pos_points) {
                  return;
                }
                if self.cursor_over_egui_overlay(pos_points)
                  || self.cursor_over_overlay_scrollbars(pos_points)
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

                self.primary_click_sequence = None;
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
              }
            }
          }
          TouchPhase::Cancelled => {
            self.touch_gesture.touch_cancel(touch.id);
          }
        }
      }
      WindowEvent::DroppedFile(path) => {
        // OS-level file drop.
        //
        // Winit reports one `DroppedFile` event per dropped path. Coalesce them into one
        // `UiToWorker::DropFiles` message (flushed from `Event::MainEventsCleared`) so multi-file
        // drops populate `<input type=file multiple>` correctly.
        //
        // Winit does not provide a drop position, so we use the last known cursor position tracked
        // by `CursorMoved` and map it into page-space CSS coordinates.
        let tab_id = self.page_input_tab.or(self.browser_state.active_tab_id());
        let pos_points = self.last_cursor_pos_points;
        let pos_css = pos_points.and_then(|pos| self.page_pos_points_to_pos_css_for_drop(pos));

        if self.pending_dropped_files.is_empty() {
          self.pending_dropped_files_tab = tab_id;
          self.pending_dropped_files_pos_css = pos_css;
        }
        if !self.pending_dropped_files.contains(path) {
          self.pending_dropped_files.push(path.clone());
        }
        self.window.request_redraw();
      }
      WindowEvent::MouseInput { state, button, .. } => {
        // While the tab search overlay is open, treat mouse interactions as UI-only (handled by
        // egui) and do not forward them to the page worker.
        if self.browser_state.chrome.tab_search.open {
          return;
        }

        let mapped_button = map_mouse_button(*button);
        if self.clear_browsing_data_dialog_open {
          return;
        }
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

        if self.debug_log_overlay_pointer_capture {
          if matches!(state, ElementState::Released)
            && matches!(mapped_button, fastrender::ui::PointerButton::Primary)
          {
            self.debug_log_overlay_pointer_capture = false;
            if let Some(pos_points) = self.last_cursor_pos_points {
              if self
                .page_rect_points
                .is_some_and(|page_rect| page_rect.contains(pos_points))
                && !self.cursor_over_egui_overlay(pos_points)
              {
                // Pointer capture ended with the cursor over the page. Re-sync hover state so the
                // worker immediately sees the current hover target without waiting for another
                // CursorMoved event.
                self.hover_sync_pending = true;
                self.window.request_redraw();
              }
            }
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

        let pos_points = self.last_cursor_pos_points.or_else(|| {
          // Winit's `MouseInput` events do not contain cursor coordinates. On some platforms winit
          // can deliver a click without a preceding `CursorMoved` (e.g. first click after focusing
          // the window), leaving `last_cursor_pos_points` unset. Fall back to egui's tracked hover
          // position, then to a direct window query.
          let egui_hover_pos = self.egui_ctx.input(|i| i.pointer.hover_pos());
          let window_cursor_pos = self.window.cursor_position().ok();
          let resolved = resolve_cursor_pos_points_for_mouse_input(
            egui_hover_pos,
            window_cursor_pos,
            self.pixels_per_point,
          );
          self.last_cursor_pos_points = resolved;
          resolved
        });

        let Some(pos_points) = pos_points else {
          // Cursor position is unknown. Conservatively treat this as a chrome/non-page click so we
          // don't misroute the next typed character to the page.
          clear_page_focus_for_unknown_cursor_pos(
            &mut self.page_has_focus,
            &mut self.cursor_in_page,
          );
          if self.scrollbar_drag.is_some() {
            self.cancel_scrollbar_drag();
          }
          if self.pointer_captured {
            self.cancel_pointer_capture();
          }
          self.window.request_redraw();
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

        if matches!(state, ElementState::Pressed) && self.open_date_time_picker.is_some() {
          // If the picker popup is open, clicks inside it are handled by egui.
          if self
            .open_date_time_picker_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            return;
          }

          // Close the picker before processing the click so we don't require a second click to
          // interact with the underlying page/chrome.
          //
          // Special-case: clicking the `<input>` control itself should typically just toggle the
          // popup closed (don't immediately reopen it by forwarding the click to the page).
          let clicked_input_control = self.open_date_time_picker.as_ref().is_some_and(|picker| {
            self
              .page_input_mapping
              .and_then(|mapping| mapping.rect_css_to_rect_points_clamped(picker.anchor_css))
              .is_some_and(|rect_points| rect_points.contains(pos_points))
          });

          self.cancel_date_time_picker();
          self.window.request_redraw();
          if clicked_input_control {
            return;
          }
        }

        if matches!(state, ElementState::Pressed) && self.open_file_picker.is_some() {
          // If the picker popup is open, clicks inside it are handled by egui.
          if self
            .open_file_picker_rect
            .is_some_and(|rect| rect.contains(pos_points))
          {
            return;
          }

          // Close the picker before processing the click so we don't require a second click to
          // interact with the underlying page/chrome.
          //
          // Special-case: clicking the `<input>` control itself should typically just toggle the
          // popup closed (don't immediately reopen it by forwarding the click to the page).
          let clicked_input_control = self.open_file_picker.as_ref().is_some_and(|picker| {
            self
              .page_input_mapping
              .and_then(|mapping| mapping.rect_css_to_rect_points_clamped(picker.anchor_css))
              .is_some_and(|rect_points| rect_points.contains(pos_points))
          });

          self.cancel_file_picker();
          self.window.request_redraw();
          if clicked_input_control {
            return;
          }
        }

        if !self.pointer_captured
          && self
            .debug_log_overlay_rect
            .is_some_and(|rect| rect.contains(pos_points))
        {
          // Treat the debug log UI as an egui overlay: don't forward clicks to the page and clear
          // focus so keyboard input can remain in egui (e.g. the filter text edit).
          self.page_has_focus = false;
          if matches!(state, ElementState::Pressed)
            && matches!(mapped_button, fastrender::ui::PointerButton::Primary)
          {
            // Track the press so the corresponding release is also suppressed even if the debug log
            // window moves/resizes under the cursor.
            self.debug_log_overlay_pointer_capture = true;
          }
          // Also clear any page hover state so hovered-link status does not remain active under the
          // overlay (this can happen if the overlay appears under a stationary cursor, e.g. when
          // toggled via menu/shortcut).
          if matches!(state, ElementState::Pressed) {
            let active_tab_id = self.browser_state.active_tab_id();
            let should_clear = self.cursor_in_page
              || active_tab_id.is_some_and(|tab_id| {
                self.browser_state.tab(tab_id).is_some_and(|tab| {
                  tab.hovered_url.is_some()
                    || !matches!(tab.cursor, fastrender::ui::CursorKind::Default)
                })
              });
            self.pending_pointer_move = None;
            self.cursor_in_page = false;

            if should_clear {
              if let Some(tab) = active_tab_id.and_then(|tab_id| self.browser_state.tab_mut(tab_id))
              {
                tab.hovered_url = None;
                tab.cursor = fastrender::ui::CursorKind::Default;
              }

              if let Some(tab_id) = self.page_input_tab.or(active_tab_id) {
                self.send_worker_msg(fastrender::ui::UiToWorker::PointerMove {
                  tab_id,
                  pos_css: (-1.0, -1.0),
                  button: fastrender::ui::PointerButton::None,
                  modifiers: map_modifiers(self.modifiers),
                });
              }

              self.window.request_redraw();
            }
          }
          return;
        }

        if !self.pointer_captured
          && (self
            .warning_toast_rect
            .is_some_and(|rect| rect.contains(pos_points))
            || self
              .error_infobar_rect
              .is_some_and(|rect| rect.contains(pos_points))
            || self
              .chrome_toast_rect
              .is_some_and(|rect| rect.contains(pos_points)))
        {
          return;
        }

        match state {
          ElementState::Pressed => {
            // Only track one "captured" pointer interaction at a time. When a primary-button drag
            // is in progress, ignore additional mouse button presses until the primary button is
            // released/cancelled.
            if self.pointer_captured {
              return;
            }

            // Clicking anywhere outside the rendered page should immediately clear page focus so
            // subsequent keyboard input is routed to egui/chrome even before the next redraw. This
            // prevents the first typed character after a chrome click (e.g. the address bar) from
            // being forwarded to the page when winit batches the click + keypress before the next
            // `RedrawRequested`.
            if fastrender::ui::input_routing::should_clear_page_focus_on_pointer_press(
              self.page_rect_points.map(|rect| {
                fastrender::Rect::from_points(
                  fastrender::Point::new(rect.min.x, rect.min.y),
                  fastrender::Point::new(rect.max.x, rect.max.y),
                )
              }),
              fastrender::Point::new(pos_points.x, pos_points.y),
            ) {
              self.page_has_focus = false;
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
            if !ensure_page_focus_cleared_for_chrome_click(
              self.page_rect_points,
              pos_points,
              &mut self.page_has_focus,
              &mut self.cursor_in_page,
            ) {
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
              self.primary_click_sequence = None;
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
            let click_count = self.click_count_for_pointer_down(mapped_button, pos_points);
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
              click_count,
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
        if self.clear_browsing_data_dialog_open {
          return;
        }
        if input.state != ElementState::Pressed {
          return;
        }
        let Some(key) = input.virtual_keycode else {
          return;
        };

        // Profile-level shortcuts that are *not* handled by `ui::chrome_ui` should never reach page
        // input (currently just "clear browsing data"). Most chrome shortcuts are handled in the
        // egui frame so we can respect egui focus rules.
        if self.handle_profile_shortcuts(key) {
          self.window.request_redraw();
          return;
        }

        // Keyboard-accessible page context menu (Shift+F10 / Menu key).
        //
        // This is handled at the winit layer so keyboard-only users can open the context menu even
        // when no pointer interaction has occurred.
        if is_page_context_menu_keyboard_gesture(key, self.modifiers) {
          // If a popup is already open, dismiss it rather than opening a new one.
          if self.open_context_menu.is_some() || self.pending_context_menu_request.is_some() {
            self.close_context_menu();
            self.window.request_redraw();
            return;
          }
          if self.open_select_dropdown.is_some() {
            self.cancel_select_dropdown();
            self.window.request_redraw();
            return;
          }
          if self.open_date_time_picker.is_some() {
            self.cancel_date_time_picker();
            self.window.request_redraw();
            return;
          }
          if self.open_file_picker.is_some() {
            self.cancel_file_picker();
            self.window.request_redraw();
            return;
          }

          if let Some(tab_id) = self.page_input_tab.or(self.browser_state.active_tab_id()) {
            if let Some(plan) = keyboard_page_context_menu_request(
              key,
              self.modifiers,
              self.page_has_focus,
              self.chrome_has_text_focus,
              self.page_loading_overlay_blocks_input,
              tab_id,
              self.page_rect_points,
              self.content_rect_points,
              self.page_input_mapping,
              self.last_cursor_pos_points,
            ) {
              // Ensure the menu opens fresh (selection starts at index 0 once the worker replies).
              self.close_context_menu();
              match plan {
                KeyboardPageContextMenuOpenPlan::WorkerRequest { pending, msg } => {
                  self.pending_context_menu_request = Some(pending);
                  self.send_worker_msg(msg);
                }
                KeyboardPageContextMenuOpenPlan::OpenUiOnly(menu) => {
                  self.open_context_menu = Some(menu);
                  self.open_context_menu_rect = None;
                }
              }
              self.window.request_redraw();
              return;
            }
          }
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

        if matches!(key, VirtualKeyCode::Escape) {
          // Escape should:
          // - dismiss popups (handled above for `<select>`; handled elsewhere for context menus),
          // - cancel address bar editing (handled inside the egui frame),
          // - close the find-in-page bar when it's open,
          // - otherwise act as "Stop loading" when a navigation is in-flight.
          //
          // Only trigger stop when egui is not actively editing text and when no popups are open,
          // matching typical browser UX.
          if let Some(tab_id) = self.browser_state.active_tab_id() {
            if self
              .browser_state
              .tab(tab_id)
              .is_some_and(|tab| tab.find.open)
            {
              if let Some(tab) = self.browser_state.tab_mut(tab_id) {
                tab.find = fastrender::ui::FindInPageState::default();
              }
              self.send_worker_msg(fastrender::ui::UiToWorker::FindStop { tab_id });
              self.page_has_focus = self.should_restore_page_focus();
              self.window.request_redraw();
              return;
            }
          }

          if self.open_context_menu.is_some() || self.pending_context_menu_request.is_some() {
            // Let the context menu consume Escape (close it), rather than interpreting it as stop.
            self.close_context_menu();
            self.window.request_redraw();
            return;
          }

          if !self.chrome_has_text_focus {
            if self
              .browser_state
              .active_tab()
              .is_some_and(|tab| tab.loading)
            {
              self.handle_chrome_actions(vec![fastrender::ui::ChromeAction::StopLoading]);
              self.window.request_redraw();
              return;
            }
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

            if fastrender::ui::shortcuts::shortcut_preempts_page_focus(action) {
              self.page_has_focus = false;
            }

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

              ShortcutAction::FocusAddressBar => {
                // Prevent text typed before the next egui frame from being forwarded to the page.
                self.focus_address_bar_select_all();
                self.window.request_redraw();
                return;
              }

              ShortcutAction::FindInPage => {
                // Prevent text typed before the next egui frame (when the find bar takes focus)
                // from being forwarded to the page.
                self.page_has_focus = false;
                self.window.request_redraw();
                return;
              }

              ShortcutAction::ToggleFullScreen => {
                let currently_fullscreen = self.window.fullscreen().is_some();
                if currently_fullscreen {
                  self.window.set_fullscreen(None);
                } else {
                  let monitor = self
                    .window
                    .current_monitor()
                    .or_else(|| self.window.primary_monitor());
                  self
                    .window
                    .set_fullscreen(Some(winit::window::Fullscreen::Borderless(monitor)));
                }
                self.window.request_redraw();
                return;
              }

              ShortcutAction::SavePage => {
                self.show_chrome_toast("Save not implemented yet");
                self.window.request_redraw();
                return;
              }

              ShortcutAction::PrintPage => {
                self.show_chrome_toast("Print not implemented yet");
                self.window.request_redraw();
                return;
              }

              // Page-level shortcuts only apply when the rendered page has focus and egui isn't
              // actively editing text (e.g. address bar).
              ShortcutAction::Copy
              | ShortcutAction::Cut
              | ShortcutAction::Paste
              | ShortcutAction::SelectAll => {
                // If egui is actively editing text (e.g. the address bar), don't handle page-level
                // key events.
                if self.chrome_has_text_focus {
                  return;
                }
                if !self.page_has_focus {
                  return;
                }
                let Some(tab_id) = self.browser_state.active_tab_id() else {
                  return;
                };

                match action {
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
                    if let Some(text) = os_clipboard::read_text() {
                      // egui-winit can also emit `egui::Event::Paste` from Ctrl/Cmd+V. Suppress it
                      // for this frame to avoid double pastes.
                      self.suppress_paste_events = true;
                      self.send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
                    }
                  }
                  _ => {}
                }
                return;
              }

              // Allow these keys to be forwarded to the page so focused text controls can handle
              // them for caret navigation and text entry.
              ShortcutAction::Space
              | ShortcutAction::Home
              | ShortcutAction::End
              | ShortcutAction::PageUp
              | ShortcutAction::PageDown => {}
              // Any other mapped shortcut should not be forwarded into the page. Most are handled
              // inside the egui chrome layer; swallowing here prevents pages from intercepting
              // browser shortcuts even if we haven't added a dedicated winit-layer handler.
              _ => {
                return;
              }
            }
          }
        }

        // If egui is actively editing text (e.g. the address bar), don't handle page-level key
        // events.
        if self.chrome_has_text_focus {
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

        let word_mod = if cfg!(target_os = "macos") {
          alt_only
        } else {
          self.modifiers.ctrl() && !self.modifiers.alt()
        };
        if word_mod && matches!(key, VirtualKeyCode::Left) {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: if self.modifiers.shift() {
              fastrender::interaction::KeyAction::WordSelectLeft
            } else {
              fastrender::interaction::KeyAction::WordLeft
            },
          });
          return;
        }
        if word_mod && matches!(key, VirtualKeyCode::Right) {
          self.send_worker_msg(fastrender::ui::UiToWorker::KeyAction {
            tab_id,
            key: if self.modifiers.shift() {
              fastrender::interaction::KeyAction::WordSelectRight
            } else {
              fastrender::interaction::KeyAction::WordRight
            },
          });
          return;
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
          VirtualKeyCode::PageUp => Some(fastrender::interaction::KeyAction::PageUp),
          VirtualKeyCode::PageDown => Some(fastrender::interaction::KeyAction::PageDown),
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
        if self.clear_browsing_data_dialog_open {
          return;
        }
        // If egui is actively editing text (e.g. the address bar), don't handle page-level IME
        // events.
        if !self.page_has_focus || self.chrome_has_text_focus {
          return;
        }

        let Some(tab_id) = self.browser_state.active_tab_id() else {
          return;
        };

        // `<select>` dropdown popups own keyboard interaction; dismiss them before IME editing.
        if self.open_select_dropdown.is_some() {
          self.cancel_select_dropdown();
        }
        if self.open_date_time_picker.is_some() {
          self.cancel_date_time_picker();
        }
        if self.open_file_picker.is_some() {
          self.cancel_file_picker();
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
        if self.clear_browsing_data_dialog_open {
          return;
        }
        if !self.page_has_focus || self.chrome_has_text_focus {
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
        if self.open_date_time_picker.is_some() {
          self.cancel_date_time_picker();
          self.window.request_redraw();
        }
        if self.open_file_picker.is_some() {
          self.cancel_file_picker();
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

  fn handle_chrome_actions(&mut self, actions: Vec<fastrender::ui::ChromeAction>) -> bool {
    use fastrender::ui::ChromeAction;
    use fastrender::ui::RepaintReason;
    use fastrender::ui::UiToWorker;

    let mut session_dirty = false;

    if !actions.is_empty() {
      self.cancel_select_dropdown();
      self.cancel_date_time_picker();
      self.cancel_file_picker();
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
        ChromeAction::NewWindow => {
          // The winit event loop owns native window creation; request a new window via a user event.
          let _ = self
            .event_loop_proxy
            .send_event(UserEvent::RequestNewWindow(self.window.id()));
        }
        ChromeAction::ToggleFullScreen => {
          use winit::window::Fullscreen;

          let currently_fullscreen = self.window.fullscreen().is_some();
          let next = !currently_fullscreen;
          self.window_fullscreen = next;
          if next {
            let monitor = self
              .window
              .current_monitor()
              .or_else(|| self.window.primary_monitor());
            self
              .window
              .set_fullscreen(Some(Fullscreen::Borderless(monitor)));
          } else {
            self.window.set_fullscreen(None);
          }
          self.window.request_redraw();
        }
        ChromeAction::OpenFindInPage => {
          // Treat the find bar as chrome text input: while it's opening/active, don't forward
          // keyboard events to the page.
          self.page_has_focus = false;
        }
        ChromeAction::SavePage => {
          self.show_chrome_toast("Save not implemented yet");
          self.window.request_redraw();
        }
        ChromeAction::PrintPage => {
          self.show_chrome_toast("Print not implemented yet");
          self.window.request_redraw();
        }
        ChromeAction::FindQuery {
          tab_id,
          query,
          case_sensitive,
        } => {
          self.send_worker_msg(UiToWorker::FindQuery {
            tab_id,
            query,
            case_sensitive,
          });
        }
        ChromeAction::FindNext(tab_id) => {
          self.send_worker_msg(UiToWorker::FindNext { tab_id });
        }
        ChromeAction::FindPrev(tab_id) => {
          self.send_worker_msg(UiToWorker::FindPrev { tab_id });
        }
        ChromeAction::CloseFindInPage(tab_id) => {
          self.send_worker_msg(UiToWorker::FindStop { tab_id });
          self.page_has_focus = self.should_restore_page_focus();
          self.window.request_redraw();
        }
        ChromeAction::OpenTabSearch => {
          // The tab search overlay owns keyboard focus while open; keep page focus disabled so the
          // rendered page doesn't steal egui focus from the overlay input.
          self.page_has_focus = false;
          self.window.request_redraw();
        }
        ChromeAction::CloseTabSearch => {
          // After dismissing the overlay, restore page focus so keyboard scrolling works without an
          // extra click.
          self.page_has_focus = self.should_restore_page_focus();
          self.window.request_redraw();
        }
        ChromeAction::ToggleDownloadsPanel => {
          let next = !self.downloads_panel_open;
          self.downloads_panel_open = next;
          if next {
            self.downloads_panel_request_focus = true;
            // Keep the right-side panel area exclusive: downloads share the same side panel space as
            // history/bookmarks.
            self.history_panel_open = false;
            self.bookmarks_panel_open = false;
            self.history_panel_request_focus_search = false;
            self.bookmarks_manager.clear_transient();
            self.page_has_focus = false;
          } else {
            self.downloads_panel_request_focus = false;
            self.page_has_focus = self.should_restore_page_focus();
          }
          self.window.request_redraw();
        }
        ChromeAction::AddressBarFocusChanged(_has_focus) => {
          // Treat address bar focus as the only "chrome text input" focus surface for now.
          //
          // When the address bar has focus, keyboard input should not be forwarded to the page.
          // When it loses focus (via Enter/Escape/clicking elsewhere), restore page focus so common
          // scrolling shortcuts work without requiring an extra click.
          self.page_has_focus = self.should_restore_page_focus();
        }
        ChromeAction::ToggleBookmarkForActiveTab => {
          self.toggle_bookmark_for_active_tab();
          self.window.request_redraw();
        }
        ChromeAction::ReorderBookmarksBar(order) => {
          if let Err(err) = self.bookmarks.reorder_root(&order) {
            eprintln!("failed to reorder bookmarks: {err:?}");
            continue;
          }
          self.autosave_bookmarks();
          self.sync_about_newtab_bookmarks_snapshot();
          self.window.request_redraw();
        }
        ChromeAction::ToggleHistoryPanel => {
          self.history_panel_open = !self.history_panel_open;
          if self.history_panel_open {
            self.bookmarks_panel_open = false;
            self.downloads_panel_open = false;
            self.bookmarks_manager.clear_transient();
            self.history_panel_request_focus_search = true;
            self.page_has_focus = false;
          } else {
            self.page_has_focus = self.should_restore_page_focus();
          }
          self.window.request_redraw();
        }
        ChromeAction::ToggleBookmarksManager => {
          self.bookmarks_panel_open = !self.bookmarks_panel_open;
          if self.bookmarks_panel_open {
            self.history_panel_open = false;
            self.downloads_panel_open = false;
            self.bookmarks_manager.request_focus_search();
            // While the manager is open, do not forward keyboard focus to the page. The manager
            // itself will request focus for its search box.
            self.page_has_focus = false;
          } else {
            self.bookmarks_manager.clear_transient();
            self.page_has_focus = self.should_restore_page_focus();
          }
          self.window.request_redraw();
        }
        ChromeAction::OpenClearBrowsingDataDialog => {
          self.clear_browsing_data_dialog_open = true;
          self.clear_browsing_data_dialog_request_focus = true;
          // Ensure chrome/page UI doesn't continue to capture keyboard focus/input while the modal
          // is open.
          self.browser_state.chrome.request_focus_address_bar = false;
          self.browser_state.chrome.request_select_all_address_bar = false;
          self.browser_state.chrome.address_bar_has_focus = false;
          self.browser_state.chrome.omnibox.reset();
          self.browser_state.chrome.appearance_popup_open = false;
          self.browser_state.chrome.tab_search.open = false;
          self.browser_state.chrome.open_tab_context_menu = None;
          self.browser_state.chrome.tab_context_menu_rect = None;
          self.browser_state.chrome.clear_tab_drag();
          self.browser_state.chrome.clear_link_drag();
          // Treat the dialog as a modal: while it's open, keyboard focus/input should not be
          // forwarded to the rendered page.
          self.page_has_focus = false;
          // End any in-flight page-scoped drags so the page doesn't keep scrolling underneath the
          // modal dialog.
          self.cancel_scrollbar_drag();
          // Default to a "safe" time range when the dialog is opened (including from shortcuts).
          self.clear_browsing_data_range = fastrender::ui::ClearBrowsingDataRange::default();
          self.window.request_redraw();
        }
        ChromeAction::NewTab => {
          session_dirty = true;
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

          session_dirty = true;
          let tab_id = fastrender::ui::TabId::new();
          let url = closed.url;
          let mut tab_state = fastrender::ui::BrowserTabState::new(tab_id, url.clone());
          tab_state.title = closed.title.clone();
          tab_state.committed_title = closed.title;
          tab_state.pinned = closed.pinned;
          tab_state.loading = true;
          tab_state.unresponsive = false;
          tab_state.last_worker_msg_at = std::time::SystemTime::now();

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
        ChromeAction::TogglePinTab(tab_id) => {
          if self.browser_state.toggle_pin_tab(tab_id) {
            session_dirty = true;
            // `chrome_ui` has already been built for this frame; request another redraw so the tab
            // strip reflects the new ordering immediately.
            self.window.request_redraw();
          }
        }
        ChromeAction::DetachTab(tab_id) => {
          if self.browser_state.tab(tab_id).is_none() {
            continue;
          }

          // If this is the last tab in the window, create a replacement tab first so we preserve
          // the invariant that each window always has at least one tab.
          if self.browser_state.tabs.len() <= 1 {
            session_dirty = true;
            let replacement_id = fastrender::ui::TabId::new();
            let initial_url = fastrender::ui::about_pages::ABOUT_NEWTAB.to_string();
            let mut tab_state =
              fastrender::ui::BrowserTabState::new(replacement_id, initial_url.clone());
            tab_state.loading = true;
            tab_state.unresponsive = false;
            tab_state.last_worker_msg_at = std::time::SystemTime::now();
            let cancel = tab_state.cancel.clone();
            self.tab_cancel.insert(replacement_id, cancel.clone());
            self.browser_state.push_tab(tab_state, true);
            self.browser_state.chrome.address_bar_text = initial_url.clone();

            self.viewport_cache_tab = None;
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.hover_sync_pending = true;
            self.pending_pointer_move = None;

            self.send_worker_msg(UiToWorker::CreateTab {
              tab_id: replacement_id,
              initial_url: Some(initial_url),
              cancel,
            });
            self.send_worker_msg(UiToWorker::SetActiveTab {
              tab_id: replacement_id,
            });
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id: replacement_id,
              reason: RepaintReason::Explicit,
            });
          }

          // Capture the detached tab's serializable state before we mutate the window.
          //
          // Note: Session persistence supports pinned tabs + tab groups, so we keep that state when
          // detaching into a new window.
          let (url, zoom, scroll_css, pinned, group_id) = {
            let tab = self
              .browser_state
              .tab(tab_id)
              .expect("tab exists (checked above)");
            let url = tab
              .committed_url
              .as_ref()
              .or(tab.current_url.as_ref())
              .cloned()
              .unwrap_or_else(|| fastrender::ui::about_pages::ABOUT_NEWTAB.to_string());
            let zoom = tab.zoom;
            let pinned = tab.pinned;
            let group_id = tab.group;
            let scroll_css = {
              let viewport = tab.scroll_state.viewport;
              let x = viewport.x;
              let y = viewport.y;
              if !x.is_finite() || !y.is_finite() {
                None
              } else {
                let x = x.max(0.0);
                let y = y.max(0.0);
                ((x, y) != (0.0, 0.0)).then_some((x, y))
              }
            };
            (url, zoom, scroll_css, pinned, group_id)
          };

          let (tab_groups, group) = group_id
            .filter(|_| !pinned)
            .and_then(|group_id| self.browser_state.tab_groups.get(&group_id))
            .map(|group_state| {
              (
                vec![fastrender::ui::BrowserSessionTabGroup {
                  title: group_state.title.clone(),
                  color: group_state.color,
                  collapsed: group_state.collapsed,
                }],
                Some(0),
              )
            })
            .unwrap_or_else(|| (Vec::new(), None));

          let session_window = fastrender::ui::BrowserSessionWindow {
            tabs: vec![fastrender::ui::BrowserSessionTab {
              url,
              zoom: Some(zoom),
              scroll_css,
              pinned,
              group,
            }],
            tab_groups,
            active_tab_index: 0,
            show_menu_bar: self.browser_state.chrome.show_menu_bar,
            window_state: None,
          };
          let _ = self
            .event_loop_proxy
            .send_event(UserEvent::RequestNewWindowWithSession {
              from_id: self.window.id(),
              window: session_window,
            });

          // Close the tab in the source window.
          session_dirty = true;
          self.pending_frame_uploads.remove_tab(tab_id);
          self.next_media_wakeup.remove(&tab_id);
          self.last_media_wakeup_tick.remove(&tab_id);
          if let Some(tex) = self.tab_textures.remove(&tab_id) {
            tex.destroy(&mut self.egui_renderer);
          }
          self.move_tab_favicon_into_delayed_destroy(tab_id);

          let was_active = self.browser_state.active_tab_id() == Some(tab_id);
          if let Some(cancel) = self.tab_cancel.remove(&tab_id) {
            cancel.bump_nav();
          }
          self.send_worker_msg(UiToWorker::CloseTab { tab_id });

          let close_result = self.browser_state.remove_tab(tab_id);

          if was_active {
            self.viewport_cache_tab = None;
            self.last_page_upload_at = None;
            self.next_page_upload_redraw = None;
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.pending_pointer_move = None;
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
            if self.pending_frame_uploads.has_pending_for_tab(new_active) {
              self.window.request_redraw();
            }
          }
        }
        ChromeAction::CloseTab(tab_id) => {
          if self.browser_state.tabs.len() <= 1 || self.browser_state.tab(tab_id).is_none() {
            // If the tab disappeared (or became the last remaining tab) while an animation was
            // pending, ensure we don't keep it in a permanently "closing" visual state.
            let had_closing = self.browser_state.chrome.closing_tabs.contains_key(&tab_id);
            self.browser_state.chrome.clear_tab_close(tab_id);
            if had_closing {
              self.window.request_redraw();
            }
            continue;
          }

          // `ChromeAction::CloseTab` is treated as "request close". When motion is enabled, the
          // first request starts the close animation and we return without closing. The tab strip
          // re-emits `CloseTab` once the animation finishes.
          let was_closing = self.browser_state.chrome.closing_tabs.contains_key(&tab_id);
          let should_close_now = {
            let motion = fastrender::ui::motion::UiMotion::from_settings(
              self.browser_state.appearance.reduced_motion,
            );
            let animations_enabled = self.egui_ctx.style().animation_time > 0.0;
            let now = self.egui_ctx.input(|i| i.time);
            self
              .browser_state
              .chrome
              .request_close_tab(tab_id, now, motion, animations_enabled)
          };
          if !should_close_now {
            if !was_closing {
              // Chrome UI has already been built for this frame; request another redraw so the tab
              // strip can render the closing state.
              self.window.request_redraw();
            }
            continue;
          }

           session_dirty = true;
           self.pending_frame_uploads.remove_tab(tab_id);
           self.next_media_wakeup.remove(&tab_id);
           self.last_media_wakeup_tick.remove(&tab_id);
           if let Some(tex) = self.tab_textures.remove(&tab_id) {
             tex.destroy(&mut self.egui_renderer);
           }
           self.move_tab_favicon_into_delayed_destroy(tab_id);

          let was_active = self.browser_state.active_tab_id() == Some(tab_id);
          if let Some(cancel) = self.tab_cancel.remove(&tab_id) {
            cancel.bump_nav();
          }
          self.send_worker_msg(UiToWorker::CloseTab { tab_id });

          let close_result = self.browser_state.remove_tab(tab_id);
          self.browser_state.chrome.clear_tab_close(tab_id);

          if was_active {
            self.viewport_cache_tab = None;
            self.last_page_upload_at = None;
            self.next_page_upload_redraw = None;
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.pending_pointer_move = None;
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

          // Chrome UI was already built for this frame. Request another redraw so the tab strip and
          // address bar reflect the updated tab list immediately after the close completes.
          self.window.request_redraw();
        }
        ChromeAction::CloseOtherTabs(tab_id) => {
          if self.browser_state.tabs.len() <= 1 || self.browser_state.tab(tab_id).is_none() {
            continue;
          }

          let prev_active = self.browser_state.active_tab_id();
          let closed = self.browser_state.close_other_tabs(tab_id);
          if closed.is_empty() {
            continue;
          }

          for closed_tab_id in closed {
            self.pending_frame_uploads.remove_tab(closed_tab_id);
            self.next_media_wakeup.remove(&closed_tab_id);
            self.last_media_wakeup_tick.remove(&closed_tab_id);
            if let Some(tex) = self.tab_textures.remove(&closed_tab_id) {
              tex.destroy(&mut self.egui_renderer);
            }
            self.move_tab_favicon_into_delayed_destroy(closed_tab_id);
            if let Some(cancel) = self.tab_cancel.remove(&closed_tab_id) {
              cancel.bump_nav();
            }
            self.send_worker_msg(UiToWorker::CloseTab {
              tab_id: closed_tab_id,
            });
          }

          let new_active = self.browser_state.active_tab_id();
          if new_active != prev_active {
            if let Some(new_active) = new_active {
              self.viewport_cache_tab = None;
              self.last_page_upload_at = None;
              self.next_page_upload_redraw = None;
              self.pointer_captured = false;
              self.captured_button = fastrender::ui::PointerButton::None;
              self.cursor_in_page = false;
              self.hover_sync_pending = true;
              self.pending_pointer_move = None;
              self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: new_active });
              self.send_worker_msg(UiToWorker::RequestRepaint {
                tab_id: new_active,
                reason: RepaintReason::Explicit,
              });
            }
          }

          // Chrome UI was already drawn this frame; request another redraw so tab strip reflects the
          // updated tab list immediately.
          self.window.request_redraw();
        }
        ChromeAction::CloseTabsToRight(tab_id) => {
          if self.browser_state.tabs.len() <= 1 || self.browser_state.tab(tab_id).is_none() {
            continue;
          }

          let prev_active = self.browser_state.active_tab_id();
          let closed = self.browser_state.close_tabs_to_right(tab_id);
          if closed.is_empty() {
            continue;
          }

          for closed_tab_id in closed {
            self.pending_frame_uploads.remove_tab(closed_tab_id);
            self.next_media_wakeup.remove(&closed_tab_id);
            self.last_media_wakeup_tick.remove(&closed_tab_id);
            if let Some(tex) = self.tab_textures.remove(&closed_tab_id) {
              tex.destroy(&mut self.egui_renderer);
            }
            self.move_tab_favicon_into_delayed_destroy(closed_tab_id);
            if let Some(cancel) = self.tab_cancel.remove(&closed_tab_id) {
              cancel.bump_nav();
            }
            self.send_worker_msg(UiToWorker::CloseTab {
              tab_id: closed_tab_id,
            });
          }

          let new_active = self.browser_state.active_tab_id();
          if new_active != prev_active {
            if let Some(new_active) = new_active {
              self.viewport_cache_tab = None;
              self.last_page_upload_at = None;
              self.next_page_upload_redraw = None;
              self.pointer_captured = false;
              self.captured_button = fastrender::ui::PointerButton::None;
              self.cursor_in_page = false;
              self.hover_sync_pending = true;
              self.pending_pointer_move = None;
              self.send_worker_msg(UiToWorker::SetActiveTab { tab_id: new_active });
              self.send_worker_msg(UiToWorker::RequestRepaint {
                tab_id: new_active,
                reason: RepaintReason::Explicit,
              });
            }
          }

          // Chrome UI was already drawn this frame; request another redraw so tab strip reflects the
          // updated tab list immediately.
          self.window.request_redraw();
        }
        ChromeAction::ReloadTab(tab_id) => {
          if self.browser_state.tab(tab_id).is_none() {
            continue;
          }
          if let Some(tab) = self.browser_state.tab_mut(tab_id) {
            tab.loading = true;
            tab.unresponsive = false;
            tab.last_worker_msg_at = std::time::SystemTime::now();
            tab.error = None;
            tab.stage = None;
            tab.title = None;
          }
          self.send_worker_msg(UiToWorker::Reload { tab_id });
        }
        ChromeAction::DuplicateTab(source_tab_id) => {
          use fastrender::ui::about_pages;

          let Some(source) = self.browser_state.tab(source_tab_id) else {
            continue;
          };
          let url = source
            .committed_url
            .clone()
            .or_else(|| source.current_url.clone())
            .unwrap_or_else(|| about_pages::ABOUT_NEWTAB.to_string());

          let tab_id = fastrender::ui::TabId::new();
          let mut tab_state = fastrender::ui::BrowserTabState::new(tab_id, url.clone());
          tab_state.title = source.title.clone();
          tab_state.committed_title = source.committed_title.clone();
          tab_state.loading = true;
          tab_state.unresponsive = false;
          tab_state.last_worker_msg_at = std::time::SystemTime::now();

          let cancel = tab_state.cancel.clone();
          self.tab_cancel.insert(tab_id, cancel.clone());
          self.browser_state.push_tab(tab_state, true);
          self.browser_state.chrome.address_bar_text = url.clone();

          // Match typical UX: duplicating a tab activates the new tab, but should not steal focus to
          // the address bar.
          self.viewport_cache_tab = None;
          self.last_page_upload_at = None;
          self.next_page_upload_redraw = None;
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

          // Chrome UI was already drawn this frame; request another redraw so the new tab appears in
          // the tab strip immediately.
          self.window.request_redraw();
        }
        ChromeAction::ActivateTab(tab_id) => {
          if self.browser_state.set_active_tab(tab_id) {
            session_dirty = true;
            self.viewport_cache_tab = None;
            self.last_page_upload_at = None;
            self.next_page_upload_redraw = None;
            self.pointer_captured = false;
            self.captured_button = fastrender::ui::PointerButton::None;
            self.cursor_in_page = false;
            self.hover_sync_pending = true;
            self.pending_pointer_move = None;
            self.send_worker_msg(UiToWorker::SetActiveTab { tab_id });
            self.send_worker_msg(UiToWorker::RequestRepaint {
              tab_id,
              reason: RepaintReason::Explicit,
            });
            // `flush_pending_frame_uploads` runs before chrome actions are processed each frame, so
            // ensure we draw a follow-up frame to upload any pending pixmap for the newly active
            // tab.
            if self.pending_frame_uploads.has_pending_for_tab(tab_id) {
              self.window.request_redraw();
            }
          }
        }
        ChromeAction::OpenUrlInNewTab(raw) => {
          // Resolve/normalize/validate the typed input (including fragment-only `#foo` joins)
          // consistently with `BrowserTabState::navigate_typed`.
          let result =
            open_typed_in_new_tab_state(&mut self.browser_state, &mut self.tab_cancel, &raw);
          match result {
            Ok((_tab_id, msgs)) => {
              session_dirty = true;
              self.apply_open_url_in_new_tab_messages(msgs);
            }
            Err(err) => {
              if let Some(tab) = self.browser_state.active_tab_mut() {
                tab.error = Some(err);
              }
            }
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
              Ok(msg) => Some(msg),
              Err(err) => {
                tab.error = Some(err);
                None
              }
            }
          };
          let Some(msg) = msg else {
            continue;
          };
          session_dirty = true;
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
            tab.unresponsive = false;
            tab.last_worker_msg_at = std::time::SystemTime::now();
            tab.error = None;
            tab.stage = None;
            tab.title = None;
          }

          self.send_worker_msg(UiToWorker::Reload { tab_id });
        }
        ChromeAction::StopLoading => {
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          if let Some(tab) = self.browser_state.tab_mut(tab_id) {
            tab.loading = false;
            tab.unresponsive = false;
            tab.stage = None;
            // Restore optimistic URL/title back to the last committed state.
            if let Some(committed_url) = tab.committed_url.clone() {
              tab.current_url = Some(committed_url);
            }
            tab.title = tab.committed_title.clone();
          }
          self.browser_state.sync_address_bar_to_active();

          self.send_worker_msg(UiToWorker::StopLoading { tab_id });
        }
        ChromeAction::Home => {
          // Ensure the worker has the latest viewport before starting a navigation-affecting
          // operation.
          self.force_send_viewport_now();
          let Some(tab_id) = self.browser_state.active_tab_id() else {
            continue;
          };
          let msg = {
            let Some(tab) = self.browser_state.tab_mut(tab_id) else {
              continue;
            };
            tab.stage = None;
            match tab.navigate_typed(&self.home_url) {
              Ok(msg) => Some(msg),
              Err(err) => {
                tab.error = Some(err);
                None
              }
            }
          };
          let Some(msg) = msg else {
            continue;
          };
          session_dirty = true;
          if let UiToWorker::Navigate { url, .. } = &msg {
            self.browser_state.chrome.address_bar_text = url.clone();
          }
          self.send_worker_msg(msg);
        }
        ChromeAction::ToggleBookmarksBar => {
          self.browser_state.chrome.bookmarks_bar_visible =
            !self.browser_state.chrome.bookmarks_bar_visible;
          self.window.request_redraw();
        }
        ChromeAction::SetShowMenuBar(show) => {
          if self.browser_state.chrome.show_menu_bar != show {
            self.browser_state.chrome.show_menu_bar = show;
            session_dirty = true;
          }
          self.window.request_redraw();
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
          tab.unresponsive = false;
          tab.last_worker_msg_at = std::time::SystemTime::now();
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
          tab.unresponsive = false;
          tab.last_worker_msg_at = std::time::SystemTime::now();
          tab.error = None;
          tab.stage = None;
          tab.title = None;
          self.send_worker_msg(UiToWorker::GoForward { tab_id });
        }
      }
    }

    session_dirty
  }

  fn handle_accesskit_action_request(&mut self, request: accesskit::ActionRequest) {
    // Renderer-chrome action routing lives outside egui. This is intentionally a no-op placeholder
    // until the FastRender chrome document is wired into the windowed browser.
    let _ = request;
  }

  fn build_chrome_accesskit_tree_update(&self) -> accesskit::TreeUpdate {
    use std::num::NonZeroU128;

    let mut classes = accesskit::NodeClassSet::new();

    // Placeholder tree until renderer-chrome's real tree builder is integrated.
    let root_id = accesskit::NodeId(NonZeroU128::new(1).unwrap());
    let doc_id = accesskit::NodeId(NonZeroU128::new(2).unwrap());

    let window_name = if self.window_title_cache.trim().is_empty() {
      "FastRender".to_string()
    } else {
      self.window_title_cache.clone()
    };

    let mut root = accesskit::NodeBuilder::new(accesskit::Role::Window);
    root.set_name(window_name);
    root.set_children(vec![doc_id]);

    let mut doc = accesskit::NodeBuilder::new(accesskit::Role::Document);
    doc.set_name("Browser chrome".to_string());
    doc.set_children(Vec::new());

    accesskit::TreeUpdate {
      nodes: vec![
        (root_id, root.build(&mut classes)),
        (doc_id, doc.build(&mut classes)),
      ],
      tree: Some(accesskit::Tree::new(root_id)),
      focus: Some(doc_id),
    }
  }

  fn update_chrome_accesskit_tree(&mut self) {
    if let ChromeA11yBackend::FastRender(a11y) = &self.chrome_a11y {
      a11y.update_if_active(|| self.build_chrome_accesskit_tree_update());
    }
  }

  fn render_frame(&mut self, control_flow: &mut winit::event_loop::ControlFlow) -> bool {
    let _frame_span = self.trace.span("ui.frame", "ui.frame");

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

    let session_revision_before = self.browser_state.session_revision();
    // Keep fullscreen tracking in sync with winit (e.g. when the user toggles fullscreen via
    // platform window controls).
    self.window_fullscreen = self.window.fullscreen().is_some();
    let mut session_dirty = false;

    {
      let _span = self.trace.span("egui.begin_frame", "ui.frame");
      let raw_input = {
        let mut raw = self.egui_state.take_egui_input(&self.window);
        raw.pixels_per_point = Some(self.pixels_per_point);

        // PERF: Avoid per-frame allocation churn by extracting wheel/paste events into reusable
        // buffers instead of collecting into new `Vec`s every frame. The common case (no wheel/paste)
        // should be allocation-free for this path.
        //
        // Validation:
        // - Run with `FASTR_PERF_LOG=1` and scroll/paste; a log line is printed only when either
        //   buffer grows (heap alloc).
        // - For full allocation profiling, see `docs/profiling-linux.md` ("heaptrack").
        let wheel_cap_before = self.wheel_events_buf.capacity();
        let paste_cap_before = self.paste_events_buf.capacity();
        self.wheel_events_buf.clear();
        self.paste_events_buf.clear();
        for event in raw.events.iter() {
          match event {
            egui::Event::MouseWheel {
              unit,
              delta,
              modifiers,
            } => {
              // Ctrl/Cmd+wheel is treated as zoom (handled in `ui::chrome_ui`), so do not forward it
              // to the page scroll pipeline.
              if !modifiers.command {
                self.wheel_events_buf.push((*unit, *delta));
              }
            }
            egui::Event::Paste(text) => {
              self.paste_events_buf.push(text.clone());
            }
            _ => {}
          }
        }
        if self.perf_log_enabled {
          let wheel_cap_after = self.wheel_events_buf.capacity();
          let paste_cap_after = self.paste_events_buf.capacity();
          if wheel_cap_after > wheel_cap_before || paste_cap_after > paste_cap_before {
            eprintln!(
              "[{ENV_PERF_LOG}] egui scratch buffers grew: wheel {wheel_cap_before}→{wheel_cap_after}, paste {paste_cap_before}→{paste_cap_after} (raw.events={}, wheel={}, paste={})",
              raw.events.len(),
              self.wheel_events_buf.len(),
              self.paste_events_buf.len(),
            );
          }
        }

        raw
      };

      self.egui_ctx.begin_frame(raw_input);
    }

    let ctx = self.egui_ctx.clone();
    fastrender::ui::motion::UiMotion::set_ctx_reduced_motion(
      &ctx,
      self.browser_state.appearance.reduced_motion,
    );

    // Treat the clear browsing data dialog as a modal: while it's open, the rendered page should
    // never take keyboard focus (e.g. via `response.request_focus()` on the central page image).
    if self.clear_browsing_data_dialog_open {
      self.page_has_focus = false;
    }

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
      style.spacing.window_margin.left = style
        .spacing
        .window_margin
        .left
        .max(TRAFFIC_LIGHTS_LEFT_INSET_POINTS);
      ctx.set_style(style);
    }
    let zoom_before = self.browser_state.active_tab().map(|t| t.zoom);
    // -----------------------------------------------------------------------------
    // Top menu bar (browser-style)
    // -----------------------------------------------------------------------------
    //
    // Render this before `ui::chrome_ui` so clipboard commands can inject egui events for the
    // address bar (TextEdit reads input during widget construction).
    if self.browser_state.chrome.show_menu_bar {
      let page_url = self
        .browser_state
        .active_tab()
        .and_then(|tab| tab.committed_url.as_deref().or(tab.current_url.as_deref()));
      let page_bookmarked = page_url
        .map(|url| self.bookmarks.contains_url(url))
        .unwrap_or(false);
      let menu_commands = fastrender::ui::menu_bar_ui(
        &ctx,
        &self.browser_state,
        fastrender::ui::MenuBarState {
          debug_log_open: self.debug_log_ui_enabled && self.debug_log_ui_open,
          history_panel_open: self.history_panel_open,
          bookmarks_panel_open: self.bookmarks_panel_open,
          page_bookmarked,
        },
      );
      if !menu_commands.is_empty() {
        let mut chrome_actions = Vec::new();
        for cmd in menu_commands {
          match cmd {
            fastrender::ui::MenuCommand::ToggleDebugLogPanel => {
              if !self.debug_log_ui_enabled {
                self.debug_log_ui_enabled = true;
                self.debug_log_ui_open = true;
              } else {
                self.debug_log_ui_open = !self.debug_log_ui_open;
              }
            }
            fastrender::ui::MenuCommand::ToggleHistoryPanel => {
              self.history_panel_open = !self.history_panel_open;
              if self.history_panel_open {
                self.bookmarks_panel_open = false;
                self.downloads_panel_open = false;
                self.downloads_panel_request_focus = false;
                self.bookmarks_manager.clear_transient();
                self.history_panel_request_focus_search = true;
                self.page_has_focus = false;
              } else {
                self.page_has_focus = self.should_restore_page_focus();
              }
            }
            fastrender::ui::MenuCommand::ToggleBookmarksPanel => {
              self.bookmarks_panel_open = !self.bookmarks_panel_open;
              if self.bookmarks_panel_open {
                self.history_panel_open = false;
                self.downloads_panel_open = false;
                self.downloads_panel_request_focus = false;
                self.history_panel_request_focus_search = false;
                self.bookmarks_manager.request_focus_search();
                self.page_has_focus = false;
              } else {
                self.bookmarks_manager.clear_transient();
                self.page_has_focus = self.should_restore_page_focus();
              }
            }
            fastrender::ui::MenuCommand::ToggleBookmarkThisPage => {
              self.toggle_bookmark_for_active_tab();
            }
            fastrender::ui::MenuCommand::Quit => {
              self.shutdown();
              *control_flow = winit::event_loop::ControlFlow::Exit;
              return session_dirty;
            }
            fastrender::ui::MenuCommand::Copy
            | fastrender::ui::MenuCommand::Cut
            | fastrender::ui::MenuCommand::Paste
            | fastrender::ui::MenuCommand::Undo
            | fastrender::ui::MenuCommand::Redo
            | fastrender::ui::MenuCommand::SelectAll
            | fastrender::ui::MenuCommand::FindInPage => {
              // Match our shortcut routing semantics:
              // - when egui has an active text field (address bar), prefer egui editing;
              // - otherwise, when the rendered page has focus, route to the worker.
              let egui_target =
                ctx.wants_keyboard_input() || self.browser_state.chrome.address_bar_has_focus;
              let should_route_to_egui_text_input = egui_target
                || self.browser_state.chrome.tab_search.open
                || self.bookmarks_panel_open
                || self.history_panel_open
                || self
                  .browser_state
                  .active_tab()
                  .is_some_and(|tab| tab.find.open);

              let inject_command_key = |key: egui::Key, shift: bool| {
                ctx.input_mut(|i| {
                  i.events.push(egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                      command: true,
                      shift,
                      ..Default::default()
                    },
                  });
                });
              };

              // Find is always chrome-level (even when the page is focused), so always route it
              // through egui shortcut handling.
              if matches!(cmd, fastrender::ui::MenuCommand::FindInPage) {
                inject_command_key(egui::Key::F, false);
                continue;
              }

              if should_route_to_egui_text_input {
                match cmd {
                  fastrender::ui::MenuCommand::Copy => {
                    ctx.input_mut(|i| i.events.push(egui::Event::Copy));
                  }
                  fastrender::ui::MenuCommand::Cut => {
                    ctx.input_mut(|i| i.events.push(egui::Event::Cut));
                  }
                  fastrender::ui::MenuCommand::Paste => {
                    if let Some(text) = os_clipboard::read_text() {
                      ctx.input_mut(|i| i.events.push(egui::Event::Paste(text)));
                    }
                  }
                  fastrender::ui::MenuCommand::Undo => {
                    inject_command_key(egui::Key::Z, false);
                  }
                  fastrender::ui::MenuCommand::Redo => {
                    inject_command_key(egui::Key::Z, true);
                  }
                  fastrender::ui::MenuCommand::SelectAll => {
                    inject_command_key(egui::Key::A, false);
                  }
                  _ => {}
                }
              } else if self.page_has_focus {
                let Some(tab_id) = self.browser_state.active_tab_id() else {
                  continue;
                };
                match cmd {
                  fastrender::ui::MenuCommand::Copy => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::Copy { tab_id });
                  }
                  fastrender::ui::MenuCommand::Cut => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::Cut { tab_id });
                  }
                  fastrender::ui::MenuCommand::Paste => {
                    if let Some(text) = os_clipboard::read_text() {
                      self.send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
                    }
                  }
                  fastrender::ui::MenuCommand::SelectAll => {
                    self.send_worker_msg(fastrender::ui::UiToWorker::SelectAll { tab_id });
                  }
                  _ => {}
                }
              }
            }
            other => {
              chrome_actions.extend(fastrender::ui::dispatch_menu_command(
                other,
                &mut self.browser_state,
              ));
            }
          }
        }
        if !chrome_actions.is_empty() {
          session_dirty |= self.handle_chrome_actions(chrome_actions);
        }
      }
    }

    let appearance_before = self.browser_state.appearance.clone();
    let chrome_actions = fastrender::ui::chrome_ui_with_bookmarks(
      &ctx,
      &mut self.browser_state,
      Some(&self.bookmarks),
      !self.clear_browsing_data_dialog_open,
      |tab_id| {
        if let Some(tex) = self.tab_favicons.get(&tab_id) {
          Some(tex.id())
        } else {
          self
            .closing_tab_favicons
            .iter()
            .find(|entry| entry.tab_id == tab_id)
            .map(|entry| entry.texture.id())
        }
      },
    );
    let zoom_after = self.browser_state.active_tab().map(|t| t.zoom);
    let appearance_after = self.browser_state.appearance.clone();

    #[cfg(target_os = "macos")]
    {
      ctx.set_style(original_style);
    }

    if self.browser_state.chrome.address_bar_has_focus
      && self.browser_state.chrome.address_bar_editing
    {
      if let Some(query) =
        fastrender::ui::resolve_omnibox_search_query(&self.browser_state.chrome.address_bar_text)
      {
        self.search_suggest.request(query);
      }
    }
    session_dirty |= zoom_before != zoom_after;
    session_dirty |= appearance_before != appearance_after;
    session_dirty |= self.handle_chrome_actions(chrome_actions);
    self.sync_window_title();

    // Cache egui text focus state into our backend-agnostic focus model before routing any keyboard
    // derived events (paste/menu commands/etc).
    self.refresh_chrome_text_focus_from_egui(&ctx);
  
    let suppress_paste_events = std::mem::take(&mut self.suppress_paste_events);
    if should_forward_paste_events_to_page(
      self.page_has_focus,
      self.chrome_has_text_focus,
      suppress_paste_events,
    ) && !self.paste_events_buf.is_empty()
    {
      if let Some(tab_id) = self.browser_state.active_tab_id() {
        // Move the buffer out so we can call `send_worker_msg` while iterating.
        let mut paste_events = std::mem::take(&mut self.paste_events_buf);
        for text in paste_events.drain(..) {
          self.send_worker_msg(fastrender::ui::UiToWorker::Paste { tab_id, text });
        }
        // Reuse the allocation next frame.
        self.paste_events_buf = paste_events;
      } else {
        self.paste_events_buf.clear();
      }
    } else {
      // Drop any collected paste copies eagerly; they'll be re-collected next frame if needed.
      self.paste_events_buf.clear();
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
    let mut history_open_in_new_tab: Option<String> = None;
    let mut history_delete_index: Option<usize> = None;

    if !self.clear_browsing_data_dialog_open
      && (self.bookmarks_panel_open || self.history_panel_open)
      && ctx.input(|i| i.key_pressed(egui::Key::Escape))
      && (!ctx.wants_keyboard_input()
        || (!self.browser_state.chrome.address_bar_has_focus
          && !self.browser_state.chrome.tab_search.open
          && !self
            .browser_state
            .active_tab()
            .is_some_and(|tab| tab.find.open)))
    {
      close_bookmarks_panel |= self.bookmarks_panel_open;
      close_history_panel |= self.history_panel_open;
    }

    if self.bookmarks_panel_open && !close_bookmarks_panel {
      let output = fastrender::ui::panels::bookmarks_manager_side_panel(
        &ctx,
        fastrender::ui::panels::BookmarksManagerInput {
          state: &mut self.bookmarks_manager,
          store: &mut self.bookmarks,
        },
      );
      if output.close_requested {
        close_bookmarks_panel = true;
      }
      if output.unfocus_page {
        self.page_has_focus = false;
      }
      if output.changed {
        self.autosave_bookmarks();
        self.sync_about_newtab_bookmarks_snapshot();

        if output.request_flush {
          self.profile_bookmarks_flush_requested = true;
        }
      }

      for action in output.actions {
        match action {
          fastrender::ui::bookmarks_manager::BookmarksManagerAction::Open(url) => {
            panel_actions.push(fastrender::ui::ChromeAction::NavigateTo(url));
          }
          fastrender::ui::bookmarks_manager::BookmarksManagerAction::OpenInNewTab(url) => {
            session_dirty |= self.open_url_in_new_tab(url);
          }
        }
      }
    } else if self.history_panel_open && !close_history_panel {
      let output = fastrender::ui::panels::history_panel_ui(
        &ctx,
        fastrender::ui::panels::HistoryPanelInput {
          history: &self.browser_state.history,
          search_text: &mut self.browser_state.chrome.history_search_text,
          request_focus_search: &mut self.history_panel_request_focus_search,
        },
      );
      if output.close_requested {
        close_history_panel = true;
      }
      if output.unfocus_page {
        self.page_has_focus = false;
      }
      if output.open_clear_browsing_data_dialog {
        panel_actions.push(fastrender::ui::ChromeAction::OpenClearBrowsingDataDialog);
      }
      if let Some(url) = output.open_url {
        panel_actions.push(fastrender::ui::ChromeAction::NavigateTo(url));
      }
      if let Some(url) = output.open_in_new_tab {
        history_open_in_new_tab = Some(url);
      }
      if let Some(idx) = output.delete_index {
        history_delete_index = Some(idx);
      }
    }

    if close_bookmarks_panel {
      self.bookmarks_panel_open = false;
      self.bookmarks_manager.clear_transient();
      self.page_has_focus = self.should_restore_page_focus();
    }
    if close_history_panel {
      self.history_panel_open = false;
      self.page_has_focus = self.should_restore_page_focus();
    }
    if !panel_actions.is_empty() {
      session_dirty |= self.handle_chrome_actions(panel_actions);
    }
    if let Some(url) = history_open_in_new_tab.take() {
      session_dirty |= self.open_url_in_new_tab(url);
    }
    if let Some(index) = history_delete_index.take() {
      self.delete_history_entry_at(index);
    }

    if self.clear_browsing_data_dialog_open {
      let was_open = self.clear_browsing_data_dialog_open;
      let output = fastrender::ui::panels::clear_browsing_data_dialog_ui(
        &ctx,
        fastrender::ui::panels::ClearBrowsingDataDialogInput {
          open: &mut self.clear_browsing_data_dialog_open,
          range: &mut self.clear_browsing_data_range,
          request_initial_focus: &mut self.clear_browsing_data_dialog_request_focus,
        },
      );
      if output.clear_now {
        self.clear_browsing_data(self.clear_browsing_data_range);
      }
      if was_open && !self.clear_browsing_data_dialog_open {
        self.page_has_focus = self.should_restore_page_focus();
        self.window.request_redraw();
      }
    }

    if !self.clear_browsing_data_dialog_open
      && self.downloads_panel_open
      && ctx.input(|i| i.key_pressed(egui::Key::Escape))
      && !ctx.wants_keyboard_input()
    {
      self.downloads_panel_open = false;
      self.downloads_panel_request_focus = false;
      self.page_has_focus = self.should_restore_page_focus();
    }
    if self.downloads_panel_open {
      self.render_downloads_panel(&ctx);
    }

    // -----------------------------------------------------------------------------
    // Renderer unresponsive watchdog
    // -----------------------------------------------------------------------------
    //
    // Mark tabs as unresponsive when they are loading but we haven't observed *any* worker→UI
    // messages for a while. This runs purely on the browser/UI side so we can surface hangs before
    // implementing thread/process killing.
    let watchdog_now = std::time::SystemTime::now();
    if self
      .browser_state
      .update_unresponsive_tabs(watchdog_now, RENDERER_UNRESPONSIVE_TIMEOUT)
    {
      // Ensure the overlay appears/disappears immediately even if no other UI state changed.
      ctx.request_repaint();
    }
    if let Some(next_check) = self
      .browser_state
      .next_unresponsive_check_in(watchdog_now, RENDERER_UNRESPONSIVE_TIMEOUT)
    {
      // Keep the event loop waking up to run the watchdog even when reduced motion disables
      // animated spinners (which would otherwise trigger continuous repaints during loading).
      ctx.request_repaint_after(next_check);
    }

    let central_response = egui::CentralPanel::default().show(&ctx, |ui| {
      let logical_viewport_points = ui.available_size();

      // Browser-like zoom: keep the drawn page size constant (in egui points) while scaling the
      // number of CSS pixels in the viewport by adjusting viewport_css + dpr.
      let zoom = self
        .browser_state
        .active_tab()
        .map(|t| t.zoom)
        .unwrap_or(fastrender::ui::DEFAULT_ZOOM);
      let (viewport_css, dpr) = fastrender::ui::viewport_css_and_dpr_for_zoom(
        // UI scale affects egui's points↔pixels mapping (chrome/widget scaling), but should not
        // change the page zoom level. Feed the render worker the *system* pixels-per-point and
        // counteract the UI-scale effect by scaling the available points.
        (
          logical_viewport_points.x * self.ui_scale,
          logical_viewport_points.y * self.ui_scale,
        ),
        self.system_pixels_per_point,
        zoom,
      );
      self.update_viewport_throttled(viewport_css, dpr);

      self.page_rect_points = None;
      self.page_viewport_css = None;
      self.page_input_tab = None;
      self.page_input_mapping = None;
      let prev_loading_overlay_blocks_input = self.page_loading_overlay_blocks_input;
      self.page_loading_overlay_blocks_input = false;
      self.page_unresponsive_overlay_rect = None;
      self.overlay_scrollbars = fastrender::ui::scrollbars::OverlayScrollbars::default();

      let Some(active_tab) = self.browser_state.active_tab_id() else {
        ui.label("No active tab.");
        return;
      };

      // Best-effort popup UX: when a native wheel scroll happens outside an open picker/dropdown,
      // close it (matching typical browser behaviour).
      let mut wheel_blocked_by_popup = false;
      if !self.wheel_events_buf.is_empty() && !self.clear_browsing_data_dialog_open {
        if let Some(pos_points) = ctx.input(|i| i.pointer.hover_pos()) {
          if self.open_select_dropdown.is_some() {
            if self
              .open_select_dropdown_rect
              .is_some_and(|rect| rect.contains(pos_points))
            {
              wheel_blocked_by_popup = true;
            } else {
              self.cancel_select_dropdown();
            }
          }
          if self.open_date_time_picker.is_some() {
            if self
              .open_date_time_picker_rect
              .is_some_and(|rect| rect.contains(pos_points))
            {
              wheel_blocked_by_popup = true;
            } else {
              self.cancel_date_time_picker();
            }
          }
          if self.open_file_picker.is_some() {
            if self
              .open_file_picker_rect
              .is_some_and(|rect| rect.contains(pos_points))
            {
              wheel_blocked_by_popup = true;
            } else {
              self.cancel_file_picker();
            }
          }
        }
      }

      let (tab_loading, tab_stage, tab_progress, tab_unresponsive) = self
        .browser_state
        .tab(active_tab)
        .map(|t| (t.loading, t.load_stage, t.chrome_loading_progress(), t.unresponsive))
        .unwrap_or((false, None, None, false));

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
            // Auto mode tries to keep text crisp at 1:1, but switching between samplers requires
            // `egui_wgpu::Renderer::free_texture/register_native_texture` and therefore a new
            // `TextureId`.
            //
            // During interactive resize the drawn scale hovers around 1:1 and can fluctuate (due to
            // both window-size rounding and pixmap updates arriving asynchronously). Use:
            // - resize-burst forcing: always use Linear while viewport changes are being throttled,
            // - hysteresis: avoid flipping filters when the scale sits near the threshold.
            let resizing = self.viewport_throttle.next_deadline().is_some();
            if resizing {
              wgpu::FilterMode::Linear
            } else {
              let current = tex.filter_mode();
              let (tex_w_px, tex_h_px) = tex.size_px();
              let drawn_px_w = size_points.x * self.pixels_per_point;
              let drawn_px_h = size_points.y * self.pixels_per_point;

              let max_scale_delta = if tex_w_px > 0
                && tex_h_px > 0
                && drawn_px_w.is_finite()
                && drawn_px_h.is_finite()
              {
                let scale_x = drawn_px_w / tex_w_px as f32;
                let scale_y = drawn_px_h / tex_h_px as f32;
                if scale_x.is_finite() && scale_y.is_finite() {
                  Some((scale_x - 1.0).abs().max((scale_y - 1.0).abs()))
                } else {
                  None
                }
              } else {
                None
              };

              // Hysteresis band:
              // - switch *to* Nearest only when very close to 1:1,
              // - switch *back* to Linear only when clearly away from 1:1.
              const SWITCH_TO_NEAREST: f32 = 0.005;
              const SWITCH_TO_LINEAR: f32 = 0.02;

              match (current, max_scale_delta) {
                (_, None) => current,
                (wgpu::FilterMode::Linear, Some(delta)) if delta < SWITCH_TO_NEAREST => {
                  wgpu::FilterMode::Nearest
                }
                (wgpu::FilterMode::Nearest, Some(delta)) if delta > SWITCH_TO_LINEAR => {
                  wgpu::FilterMode::Linear
                }
                _ => current,
              }
            }
          }
        };

        tex.set_filter_mode(&self.device, &mut self.egui_renderer, desired_filter);
        let response = ui.add(
          egui::Image::new((tex.id(), size_points))
            .uv(tex.uv_rect())
            .sense(egui::Sense::click()),
        );
        // The page is currently presented as a rendered image (no document accessibility yet). Give
        // it a stable label so screen readers can identify what this focusable region represents.
        response.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Label, "Web page content (rendered image)")
        });
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
            if self.open_date_time_picker.is_some() {
              self.cancel_date_time_picker();
            }
            if self.open_file_picker.is_some() {
              self.cancel_file_picker();
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
            if !self.wheel_events_buf.is_empty()
              && !wheel_blocked_by_popup
              && response.hovered()
              && !self.clear_browsing_data_dialog_open
            {
              let cursor_over_overlay = self.debug_log_overlay_pointer_capture
                || response
                  .hover_pos()
                  .is_some_and(|pos| self.cursor_over_egui_overlay(pos));
              if !cursor_over_overlay {
                let mut delta_css = (0.0, 0.0);
                for (unit, delta) in &self.wheel_events_buf {
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
              self
                .overlay_scrollbar_visibility
                .alpha(now, cfg, force_visible)
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

              let clamp_alpha = |base: u8| ((base as f32) * alpha).round().clamp(0.0, 255.0) as u8;

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
                let cross_inset = if dragging {
                  0.5
                } else if hovered {
                  1.0
                } else {
                  2.0
                };
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

                let track_alpha = if dragging {
                  60
                } else if hovered {
                  40
                } else {
                  0
                };
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

                let thumb_alpha = if dragging {
                  220
                } else if hovered {
                  180
                } else {
                  140
                };
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

        {
          let overlay_id = egui::Id::new(("fastr_page_loading_overlay", active_tab.0));
          let motion = fastrender::ui::motion::UiMotion::from_ctx(&ctx);
          let overlay_t = motion.animate_bool(
            &ctx,
            overlay_id.with("visible"),
            matches!(
              loading_ui.kind,
              fastrender::ui::loading_overlay::PageLoadingUiKind::Overlay
            ),
            motion.durations.progress_fade,
          );
          let overlay_opacity = overlay_t.clamp(0.0, 1.0);

          if overlay_opacity > 0.0 {
            let painter = ui.painter();
            let scrim = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 44);
            painter.rect_filled(
              response.rect,
              egui::Rounding::same(0.0),
              Self::with_alpha(scrim, overlay_opacity),
            );

            if let Some(progress) = tab_progress {
              let bar_h = 2.0;
              let progress = if progress.is_finite() {
                progress.clamp(0.0, 1.0).max(0.02)
              } else {
                0.02
              };
              let x1 = response.rect.left() + response.rect.width() * progress;
              let bar_rect = egui::Rect::from_min_max(
                egui::pos2(response.rect.left(), response.rect.top()),
                egui::pos2(x1, response.rect.top() + bar_h),
              );
              if bar_rect.width() > 0.0 {
                let accent = ui.visuals().selection.stroke.color;
                let accent =
                  egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 220);
                painter.rect_filled(
                  bar_rect,
                  egui::Rounding::same(1.0),
                  Self::with_alpha(accent, overlay_opacity),
                );
              }
            }

            let center = response.rect.center();
            let overlay_padding = self.theme.sizing.padding;
            let spinner_size = 18.0;
            let overlay_size = spinner_size + overlay_padding * 2.0;
            egui::Area::new(overlay_id)
              .order(egui::Order::Foreground)
              .fixed_pos(egui::pos2(
                center.x - overlay_size * 0.5,
                center.y - overlay_size * 0.5,
              ))
              .show(&ctx, |ui| {
                let fill = ui.visuals().window_fill;
                let fill = egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 220);
                egui::Frame::none()
                  .fill(Self::with_alpha(fill, overlay_opacity))
                  .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
                  .inner_margin(egui::Margin::same(overlay_padding))
                  .show(ui, |ui| {
                    let _ = fastrender::ui::spinner(ui, spinner_size);
                  });
              });
          }
        }

        // "Page unresponsive" watchdog overlay.
        if tab_unresponsive {
          let overlay_id = egui::Id::new(("fastr_page_unresponsive_overlay", active_tab.0));
          let center = response.rect.center();
          let width = response.rect.width().min(360.0).max(220.0);
          let pos = egui::pos2(center.x - width * 0.5, center.y - 70.0);
          let overlay = egui::Area::new(overlay_id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(&ctx, |ui| {
              let fill = ui.visuals().window_fill;
              let fill = egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 240);
              egui::Frame::none()
                .fill(fill)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
                .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                  ui.set_min_width(width);
                  ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    ui.label(egui::RichText::new("Page unresponsive").strong().size(16.0));
                    ui.horizontal(|ui| {
                      let wait = ui.button("Wait");
                      let reload = ui.button("Reload");
                      if wait.clicked() {
                        let _ = self
                          .browser_state
                          .dismiss_tab_unresponsive(active_tab, std::time::SystemTime::now());
                        ctx.request_repaint();
                      }
                      if reload.clicked() {
                        let now = std::time::SystemTime::now();
                        let _ = self.browser_state.dismiss_tab_unresponsive(active_tab, now);
                        if let Some(tab) = self.browser_state.tab_mut(active_tab) {
                          tab.loading = true;
                          tab.error = None;
                          tab.stage = None;
                          tab.title = None;
                          tab.last_worker_msg_at = now;
                          tab.unresponsive = false;
                        }
                        self.force_send_viewport_now();
                        self.send_worker_msg(fastrender::ui::UiToWorker::Reload { tab_id: active_tab });
                        ctx.request_repaint();
                      }
                    });
                  });
                });
            });
          self.page_unresponsive_overlay_rect = Some(overlay.response.rect);
        }

        // Note: page scrolling from wheel/trackpad input is forwarded from the winit event stream
        // inside `handle_winit_input_event(WindowEvent::MouseWheel { .. })` so both the egui and
        // compositor backends share the same behaviour.
      } else {
        let loading_ui =
          fastrender::ui::loading_overlay::decide_page_loading_ui(false, tab_loading, tab_stage);

        let size_points = logical_viewport_points.max(egui::Vec2::ZERO);
        let (rect, _) = ui.allocate_exact_size(size_points, egui::Sense::hover());

        let painter = ui.painter();
        painter.rect_filled(rect, egui::Rounding::same(0.0), ui.visuals().panel_fill);

        if let Some(progress) = tab_progress {
          let bar_h = 2.0;
          let progress = if progress.is_finite() {
            progress.clamp(0.0, 1.0).max(0.02)
          } else {
            0.02
          };
          let x1 = rect.left() + rect.width() * progress;
          let bar_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(x1, rect.top() + bar_h),
          );
          if bar_rect.width() > 0.0 {
            let accent = ui.visuals().selection.stroke.color;
            let accent =
              egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 220);
            painter.rect_filled(bar_rect, egui::Rounding::same(1.0), accent);
          }
        }

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
              let _ = fastrender::ui::spinner(ui, 32.0);
              if let Some(headline) = loading_ui.headline {
                ui.label(egui::RichText::new(headline).strong().size(16.0));
              }
              if let Some(detail) = loading_ui.detail {
                ui.label(egui::RichText::new(detail).small());
              }
            },
          );
        });

        // "Page unresponsive" watchdog overlay (shown even when no frame exists yet).
        if tab_unresponsive {
          let overlay_id = egui::Id::new(("fastr_page_unresponsive_overlay", active_tab.0));
          let center = rect.center();
          let width = rect.width().min(360.0).max(220.0);
          let pos = egui::pos2(center.x - width * 0.5, center.y - 70.0);
          let overlay = egui::Area::new(overlay_id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(&ctx, |ui| {
              let fill = ui.visuals().window_fill;
              let fill = egui::Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 240);
              egui::Frame::none()
                .fill(fill)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .rounding(egui::Rounding::same(self.theme.sizing.corner_radius))
                .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                  ui.set_min_width(width);
                  ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    ui.label(egui::RichText::new("Page unresponsive").strong().size(16.0));
                    ui.horizontal(|ui| {
                      let wait = ui.button("Wait");
                      let reload = ui.button("Reload");
                      if wait.clicked() {
                        let _ = self
                          .browser_state
                          .dismiss_tab_unresponsive(active_tab, std::time::SystemTime::now());
                        ctx.request_repaint();
                      }
                      if reload.clicked() {
                        let now = std::time::SystemTime::now();
                        let _ =
                          self.browser_state.dismiss_tab_unresponsive(active_tab, now);
                        if let Some(tab) = self.browser_state.tab_mut(active_tab) {
                          tab.loading = true;
                          tab.error = None;
                          tab.stage = None;
                          tab.title = None;
                          tab.last_worker_msg_at = now;
                          tab.unresponsive = false;
                        }
                        self.force_send_viewport_now();
                        self.send_worker_msg(fastrender::ui::UiToWorker::Reload { tab_id: active_tab });
                        ctx.request_repaint();
                      }
                    });
                  });
                });
            });
          self.page_unresponsive_overlay_rect = Some(overlay.response.rect);
        }
      }
    });

    self.content_rect_points = Some(central_response.response.rect);

    let now = std::time::Instant::now();
    self.sync_tab_notifications(now);
    self.chrome_toast.expire(now);
    self.render_error_infobar(&ctx);
    self.render_warning_toast(&ctx);
    self.render_chrome_toast(&ctx);

    self.render_hud(&ctx);
    self.render_debug_log_overlay(&ctx);
    fastrender::ui::chrome::hover_status_overlay_ui(
      &ctx,
      &self.browser_state,
      central_response.response.rect,
    );
    self.render_select_dropdown(&ctx);
    self.render_date_time_picker(&ctx);
    self.render_file_picker(&ctx);
    session_dirty |= self.render_context_menu(&ctx);
    self.sync_hover_after_tab_change(&ctx);
    // Coalesce scrollbar thumb drag CursorMoved bursts to at most one scroll message per frame.
    self.flush_pending_scroll_drag();
    // Coalesce pointer-move bursts to at most one message per rendered frame.
    self.flush_pending_pointer_move();

    // Update the cached focus model for use by the winit input handler after this frame.
    self.refresh_chrome_text_focus_from_egui(&ctx);

    let mut full_output = {
      let _span = self.trace.span("egui.end_frame", "ui.frame");
      self.egui_ctx.end_frame()
    };
    let repaint_after = full_output.repaint_after;
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

    let paint_jobs = {
      let _span = self.trace.span("egui.tessellate", "ui.frame");
      self.egui_ctx.tessellate(full_output.shapes)
    };

    let screen_descriptor = egui_wgpu::renderer::ScreenDescriptor {
      size_in_pixels: [self.surface_config.width, self.surface_config.height],
      pixels_per_point: self.pixels_per_point,
    };

    {
      let _span = self.trace.span("egui.update_textures", "ui.upload");
      for (id, image_delta) in &full_output.textures_delta.set {
        self
          .egui_renderer
          .update_texture(&self.device, &self.queue, *id, image_delta);
      }
    }

    let surface_texture = {
      let _span = self.trace.span("wgpu.acquire", "ui.frame");
      let mut surface_configured_this_frame = false;
      if self.surface_needs_configure {
        self.configure_surface("resize");
        surface_configured_this_frame = true;
      }

      let mut surface_texture_result = self.surface.get_current_texture();
      if matches!(
        surface_texture_result,
        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated)
      ) {
        if self.window_minimized {
          return session_dirty;
        }
        if !surface_configured_this_frame {
          self.configure_surface("lost/outdated");
          surface_configured_this_frame = true;
        }
        surface_texture_result = self.surface.get_current_texture();
      }

      match surface_texture_result {
        Ok(frame) => frame,
        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
          return session_dirty;
        }
        Err(wgpu::SurfaceError::Timeout) => {
          return session_dirty;
        }
        Err(wgpu::SurfaceError::OutOfMemory) => {
          eprintln!("wgpu surface out of memory; exiting");
          self.shutdown();
          *control_flow = winit::event_loop::ControlFlow::Exit;
          return session_dirty;
        }
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

    {
      let _span = self.trace.span("wgpu.update_buffers", "ui.frame");
      self.egui_renderer.update_buffers(
        &self.device,
        &self.queue,
        &mut encoder,
        &paint_jobs,
        &screen_descriptor,
      );
    }

    {
      let _span = self.trace.span("wgpu.render_pass", "ui.frame");
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

    {
      let _span = self.trace.span("wgpu.submit", "ui.frame");
      self.queue.submit(Some(encoder.finish()));
    }
    {
      let _span = self.trace.span("wgpu.present", "ui.frame");
      surface_texture.present();
    }

    for id in &full_output.textures_delta.free {
      self.egui_renderer.free_texture(id);
    }

    // Apply any appearance changes (theme/high-contrast/UI scale) for the *next* frame. Doing this
    // after presenting avoids mutating egui state while we're still rendering the current frame's
    // output.
    if self.sync_appearance_settings() {
      self.window.request_redraw();
    }

    if let (Some(hud), Some(frame_start)) = (self.hud.as_mut(), frame_start) {
      let elapsed_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
      if elapsed_ms.is_finite() {
        hud.last_frame_cpu_ms = Some(elapsed_ms);
      }
    }

    // Keep the platform accessibility tree for renderer-chrome in sync with the latest rendered
    // chrome document state. This is a no-op when the window is using egui's AccessKit integration.
    self.update_chrome_accesskit_tree();

    session_dirty |= self.browser_state.session_revision() != session_revision_before;
    session_dirty
  }
}

#[cfg(feature = "browser_ui")]
fn capture_window_state(
  window: &winit::window::Window,
) -> Option<fastrender::ui::BrowserWindowState> {
  let maximized = window.is_maximized();
  let size = window.inner_size();
  let pos = window.outer_position().ok();

  Some(fastrender::ui::BrowserWindowState {
    x: pos.map(|p| p.x as i64),
    y: pos.map(|p| p.y as i64),
    width: Some(size.width as i64),
    height: Some(size.height as i64),
    maximized,
  })
}

#[cfg(feature = "browser_ui")]
fn map_winit_key_to_shortcuts_key(
  key: winit::event::VirtualKeyCode,
) -> Option<fastrender::ui::shortcuts::Key> {
  use fastrender::ui::shortcuts::Key as ShortcutKey;
  use winit::event::VirtualKeyCode;

  Some(match key {
    VirtualKeyCode::A => ShortcutKey::A,
    VirtualKeyCode::B => ShortcutKey::B,
    VirtualKeyCode::C => ShortcutKey::C,
    VirtualKeyCode::D => ShortcutKey::D,
    VirtualKeyCode::F => ShortcutKey::F,
    VirtualKeyCode::H => ShortcutKey::H,
    VirtualKeyCode::J => ShortcutKey::J,
    VirtualKeyCode::K => ShortcutKey::K,
    VirtualKeyCode::L => ShortcutKey::L,
    VirtualKeyCode::N => ShortcutKey::N,
    VirtualKeyCode::O => ShortcutKey::O,
    VirtualKeyCode::P => ShortcutKey::P,
    VirtualKeyCode::LBracket => ShortcutKey::OpenBracket,
    VirtualKeyCode::RBracket => ShortcutKey::CloseBracket,
    VirtualKeyCode::R => ShortcutKey::R,
    VirtualKeyCode::S => ShortcutKey::S,
    VirtualKeyCode::T => ShortcutKey::T,
    VirtualKeyCode::V => ShortcutKey::V,
    VirtualKeyCode::W => ShortcutKey::W,
    VirtualKeyCode::X => ShortcutKey::X,
    VirtualKeyCode::Y => ShortcutKey::Y,
    VirtualKeyCode::Insert => ShortcutKey::Insert,
    VirtualKeyCode::Delete => ShortcutKey::Delete,
    VirtualKeyCode::Tab => ShortcutKey::Tab,
    VirtualKeyCode::Left => ShortcutKey::Left,
    VirtualKeyCode::Right => ShortcutKey::Right,
    VirtualKeyCode::F4 => ShortcutKey::F4,
    VirtualKeyCode::F5 => ShortcutKey::F5,
    VirtualKeyCode::F6 => ShortcutKey::F6,
    VirtualKeyCode::F11 => ShortcutKey::F11,
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

#[cfg(feature = "browser_ui")]
fn open_file_with_os_default(path: &std::path::Path) {
  use std::process::Command;

  let result = if cfg!(target_os = "macos") {
    Command::new("open").arg(path).spawn()
  } else if cfg!(target_os = "windows") {
    // `start` is a shell builtin. The empty string is the window title.
    Command::new("cmd")
      .args(["/C", "start", ""])
      .arg(path)
      .spawn()
  } else {
    Command::new("xdg-open").arg(path).spawn()
  };

  if let Err(err) = result {
    eprintln!("failed to open file {}: {err}", path.display());
  }
}

#[cfg(feature = "browser_ui")]
fn reveal_file_in_os_file_manager(path: &std::path::Path) {
  use std::process::Command;

  let result = if cfg!(target_os = "macos") {
    Command::new("open").arg("-R").arg(path).spawn()
  } else if cfg!(target_os = "windows") {
    Command::new("explorer")
      .arg(format!("/select,{}", path.display()))
      .spawn()
  } else {
    // Linux doesn't have a standard "reveal" command; best effort by opening the parent directory.
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    Command::new("xdg-open").arg(parent).spawn()
  };

  if let Err(err) = result {
    eprintln!(
      "failed to reveal file {} in file manager: {err}",
      path.display()
    );
  }
}

#[cfg(feature = "browser_ui")]
fn resolve_cursor_pos_points_for_mouse_input(
  egui_hover_pos_points: Option<egui::Pos2>,
  window_cursor_pos_physical: Option<winit::dpi::PhysicalPosition<f64>>,
  pixels_per_point: f32,
) -> Option<egui::Pos2> {
  if let Some(pos_points) = egui_hover_pos_points {
    return Some(pos_points);
  }

  let physical = window_cursor_pos_physical?;
  if pixels_per_point <= 0.0 {
    return None;
  }
  Some(egui::pos2(
    physical.x as f32 / pixels_per_point,
    physical.y as f32 / pixels_per_point,
  ))
}

#[cfg(feature = "browser_ui")]
fn clear_page_focus_for_unknown_cursor_pos(page_has_focus: &mut bool, cursor_in_page: &mut bool) {
  *page_has_focus = false;
  *cursor_in_page = false;
}

#[cfg(all(test, feature = "browser_ui"))]
mod mouse_input_cursor_pos_resolution_tests {
  use super::{clear_page_focus_for_unknown_cursor_pos, resolve_cursor_pos_points_for_mouse_input};
  use winit::dpi::PhysicalPosition;

  #[test]
  fn none_inputs_returns_none_and_clears_focus() {
    let resolved = resolve_cursor_pos_points_for_mouse_input(None, None, 1.0);
    assert!(resolved.is_none());

    let mut page_has_focus = true;
    let mut cursor_in_page = true;
    if resolved.is_none() {
      clear_page_focus_for_unknown_cursor_pos(&mut page_has_focus, &mut cursor_in_page);
    }
    assert!(!page_has_focus);
    assert!(!cursor_in_page);
  }

  #[test]
  fn prefers_egui_hover_position() {
    let hover = egui::pos2(12.5, 99.0);
    let resolved = resolve_cursor_pos_points_for_mouse_input(
      Some(hover),
      Some(PhysicalPosition::new(5.0, 6.0)),
      2.0,
    );
    assert_eq!(resolved, Some(hover));
  }

  #[test]
  fn converts_physical_cursor_position_to_points() {
    let resolved = resolve_cursor_pos_points_for_mouse_input(
      None,
      Some(PhysicalPosition::new(200.0, 100.0)),
      2.0,
    );
    assert_eq!(resolved, Some(egui::pos2(100.0, 50.0)));
  }
}

#[cfg(feature = "browser_ui")]
fn ensure_page_focus_cleared_for_chrome_click(
  page_rect_points: Option<egui::Rect>,
  pos_points: egui::Pos2,
  page_has_focus: &mut bool,
  cursor_in_page: &mut bool,
) -> bool {
  let in_page = page_rect_points.is_some_and(|rect| rect.contains(pos_points));
  if in_page {
    return true;
  }
  *page_has_focus = false;
  *cursor_in_page = false;
  false
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

#[cfg(test)]
mod page_focus_restore_tests {
  use super::should_restore_page_focus_for_state;

  #[test]
  fn restore_focus_requires_no_other_focus_surfaces() {
    assert!(should_restore_page_focus_for_state(
      false, false, false, false, false, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      true, false, false, false, false, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, true, false, false, false, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, false, true, false, false, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, false, false, true, false, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, false, false, false, true, false, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, false, false, false, false, true, false
    ));
    assert!(!should_restore_page_focus_for_state(
      false, false, false, false, false, false, true
    ));
  }
}

#[cfg(test)]
mod pending_scroll_drag_tests {
  use super::PendingScrollDrag;

  #[test]
  fn coalesces_multiple_deltas_into_one_scroll_message() {
    let tab_id = fastrender::ui::TabId(1);
    let mut pending: Option<PendingScrollDrag> = None;

    PendingScrollDrag::push(&mut pending, tab_id, (0.0, 1.0));
    PendingScrollDrag::push(&mut pending, tab_id, (0.0, 2.0));

    let msg = pending
      .take()
      .expect("pending scroll drag should exist")
      .into_worker_msg();

    match msg {
      fastrender::ui::UiToWorker::Scroll {
        tab_id: msg_tab,
        delta_css,
        pointer_css,
      } => {
        assert_eq!(msg_tab, tab_id);
        assert_eq!(pointer_css, None);
        assert_eq!(delta_css, (0.0, 3.0));
      }
      other => panic!("expected UiToWorker::Scroll, got {other:?}"),
    }
  }
}

#[cfg(test)]
mod touch_long_press_tests {
  use super::{TouchGestureAction, TouchGestureRecognizer};
  use std::time::{Duration, Instant};

  #[test]
  fn held_past_threshold_triggers_context_menu() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (10.0, 20.0));
    assert_eq!(recognizer.tick(t0 + Duration::from_millis(499)), None);
    assert_eq!(
      recognizer.tick(t0 + Duration::from_millis(500)),
      Some(TouchGestureAction::ContextMenu {
        pos_points: (10.0, 20.0)
      })
    );
  }

  #[test]
  fn movement_beyond_slop_cancels_long_press() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (0.0, 0.0));
    recognizer.touch_move(1, (10.0, 0.0));
    assert_eq!(recognizer.tick(t0 + Duration::from_millis(600)), None);
  }

  #[test]
  fn long_press_suppresses_tap_on_release() {
    let mut recognizer = TouchGestureRecognizer::new_with_config(Duration::from_millis(500), 4.0);
    let t0 = Instant::now();
    recognizer.touch_start(1, t0, (5.0, 5.0));
    assert!(matches!(
      recognizer.tick(t0 + Duration::from_millis(600)),
      Some(TouchGestureAction::ContextMenu { .. })
    ));
    assert_eq!(
      recognizer.touch_end(1, t0 + Duration::from_millis(650), (5.0, 5.0)),
      None
    );
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
    assert_eq!(
      map_mouse_button(MouseButton::Other(5)),
      PointerButton::Forward
    );
    assert_eq!(map_mouse_button(MouseButton::Other(8)), PointerButton::Back);
    assert_eq!(
      map_mouse_button(MouseButton::Other(9)),
      PointerButton::Forward
    );
    assert_eq!(
      map_mouse_button(MouseButton::Other(10)),
      PointerButton::Other(10)
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod page_focus_tests {
  use super::ensure_page_focus_cleared_for_chrome_click;

  #[test]
  fn chrome_click_clears_page_focus_when_outside_page_rect() {
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 80.0));
    let mut page_has_focus = true;
    let mut cursor_in_page = true;
    let ok = ensure_page_focus_cleared_for_chrome_click(
      Some(rect),
      egui::pos2(0.0, 0.0),
      &mut page_has_focus,
      &mut cursor_in_page,
    );
    assert!(!ok);
    assert!(!page_has_focus);
    assert!(!cursor_in_page);
  }

  #[test]
  fn page_click_does_not_clear_page_focus_when_inside_page_rect() {
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 80.0));
    let mut page_has_focus = true;
    let mut cursor_in_page = true;
    let ok = ensure_page_focus_cleared_for_chrome_click(
      Some(rect),
      egui::pos2(15.0, 25.0),
      &mut page_has_focus,
      &mut cursor_in_page,
    );
    assert!(ok);
    assert!(page_has_focus);
    assert!(cursor_in_page);
  }

  #[test]
  fn click_clears_page_focus_when_page_rect_unknown() {
    let mut page_has_focus = true;
    let mut cursor_in_page = true;
    let ok = ensure_page_focus_cleared_for_chrome_click(
      None,
      egui::pos2(0.0, 0.0),
      &mut page_has_focus,
      &mut cursor_in_page,
    );
    assert!(!ok);
    assert!(!page_has_focus);
    assert!(!cursor_in_page);
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod warning_toast_a11y_tests {
  fn key_press_release(key: egui::Key) -> Vec<egui::Event> {
    let modifiers = egui::Modifiers::default();
    vec![
      egui::Event::Key {
        key,
        pressed: true,
        repeat: false,
        modifiers,
      },
      egui::Event::Key {
        key,
        pressed: false,
        repeat: false,
        modifiers,
      },
    ]
  }

  fn warning_toast_toggle_ui(ui: &mut egui::Ui, expanded: &mut bool) {
    let title_resp =
      ui.add(egui::Button::new(egui::RichText::new("Viewport clamped").strong()).frame(false));

    let mut toggle_requested = title_resp.clicked();
    if title_resp.has_focus() {
      toggle_requested |= ui.input_mut(|i| {
        i.consume_key(Default::default(), egui::Key::Enter)
          || i.consume_key(Default::default(), egui::Key::Space)
      });
    }

    if toggle_requested {
      *expanded = !*expanded;
      title_resp.request_focus();
    }

    let a11y_label = if *expanded {
      "Viewport clamped: hide details"
    } else {
      "Viewport clamped: show details"
    };
    title_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, a11y_label));
  }

  fn run_toast_frame(ctx: &egui::Context, expanded: &mut bool, events: Vec<egui::Event>) {
    let mut raw = egui::RawInput::default();
    raw.focused = true;
    raw.pixels_per_point = Some(1.0);
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(600.0, 240.0),
    ));
    raw.events = events;

    ctx.begin_frame(raw);
    egui::CentralPanel::default().show(ctx, |ui| {
      warning_toast_toggle_ui(ui, expanded);
    });
    let _ = ctx.end_frame();
  }

  #[test]
  fn viewport_clamped_warning_toast_toggle_works_with_space() {
    let ctx = egui::Context::default();
    let mut expanded = false;

    run_toast_frame(&ctx, &mut expanded, Vec::new());
    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Space));
    assert!(
      !expanded,
      "expected Space to do nothing when the toggle is not focused"
    );

    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Tab));
    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Space));
    assert!(
      expanded,
      "expected Space to toggle expanded when focused via Tab"
    );
  }

  #[test]
  fn viewport_clamped_warning_toast_toggle_works_with_enter() {
    let ctx = egui::Context::default();
    let mut expanded = false;

    run_toast_frame(&ctx, &mut expanded, Vec::new());
    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Enter));
    assert!(
      !expanded,
      "expected Enter to do nothing when the toggle is not focused"
    );

    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Tab));
    run_toast_frame(&ctx, &mut expanded, key_press_release(egui::Key::Enter));
    assert!(
      expanded,
      "expected Enter to toggle expanded when focused via Tab"
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod error_infobar_a11y_tests {
  #[derive(Clone, Copy)]
  struct FrameInfo {
    id: egui::Id,
    has_focus: bool,
  }

  fn accesskit_update_from_platform_output(output: &egui::PlatformOutput) -> &accesskit::TreeUpdate {
    output.accesskit_update.as_ref().expect(
      "egui did not emit an AccessKit update. Ensure `ctx.enable_accesskit()` was called for the frame under test.",
    )
  }

  fn accesskit_names_from_full_output(output: &egui::FullOutput) -> Vec<String> {
    let update = accesskit_update_from_platform_output(&output.platform_output);
    let mut names: Vec<String> = update
      .nodes
      .iter()
      .map(|(_id, node)| node.name().unwrap_or("").trim().to_string())
      .filter(|name| !name.is_empty())
      .collect();
    names.sort();
    names.dedup();
    names
  }

  fn key_press_release(key: egui::Key) -> Vec<egui::Event> {
    let modifiers = egui::Modifiers::default();
    vec![
      egui::Event::Key {
        key,
        pressed: true,
        repeat: false,
        modifiers,
      },
      egui::Event::Key {
        key,
        pressed: false,
        repeat: false,
        modifiers,
      },
    ]
  }

  fn error_infobar_details_toggle_ui(ui: &mut egui::Ui, details_open: &mut bool) -> FrameInfo {
    let infobar_id = egui::Id::new("test_error_infobar");
    let details_label = if *details_open {
      "Hide details"
    } else {
      "Details"
    };
    let details_resp = ui
      .push_id(infobar_id.with("toggle_details"), |ui| {
        ui.button(details_label)
      })
      .inner;

    let mut toggle_requested = details_resp.clicked();
    if details_resp.has_focus() {
      toggle_requested |= ui.input_mut(|i| {
        i.consume_key(Default::default(), egui::Key::Enter)
          || i.consume_key(Default::default(), egui::Key::Space)
      });
    }

    if toggle_requested {
      *details_open = !*details_open;
      details_resp.request_focus();
    }

    FrameInfo {
      id: details_resp.id,
      has_focus: details_resp.has_focus(),
    }
  }

  fn run_infobar_frame(
    ctx: &egui::Context,
    details_open: &mut bool,
    events: Vec<egui::Event>,
  ) -> FrameInfo {
    let mut raw = egui::RawInput::default();
    raw.focused = true;
    raw.pixels_per_point = Some(1.0);
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(600.0, 240.0),
    ));
    raw.events = events;

    ctx.begin_frame(raw);
    let mut info = None;
    egui::CentralPanel::default().show(ctx, |ui| {
      info = Some(error_infobar_details_toggle_ui(ui, details_open));
    });
    let _ = ctx.end_frame();
    info.unwrap()
  }

  fn error_infobar_accesskit_ui(ui: &mut egui::Ui, details_open: &mut bool) {
    let infobar_id = egui::Id::new("test_error_infobar_accesskit");

    let retry_resp = ui
      .push_id(infobar_id.with("retry"), |ui| ui.button("Retry"))
      .inner;
    retry_resp.widget_info(|| {
      egui::WidgetInfo::labeled(egui::WidgetType::Button, "Retry navigation")
    });

    let details_label = if *details_open { "Hide details" } else { "Details" };
    let details_resp = ui
      .push_id(infobar_id.with("toggle_details"), |ui| ui.button(details_label))
      .inner;

    let details_a11y_label = if *details_open {
      "Hide error details"
    } else {
      "Show error details"
    };
    details_resp.widget_info(|| {
      egui::WidgetInfo::labeled(egui::WidgetType::Button, details_a11y_label)
    });
    let _ = details_resp.ctx.accesskit_node_builder(details_resp.id, |builder| {
      builder.set_expanded(*details_open);
      if *details_open {
        builder.add_action(accesskit::Action::Collapse);
        builder.remove_action(accesskit::Action::Expand);
      } else {
        builder.add_action(accesskit::Action::Expand);
        builder.remove_action(accesskit::Action::Collapse);
      }
    });
  }

  fn run_infobar_accesskit_frame(ctx: &egui::Context, details_open: &mut bool) -> egui::FullOutput {
    let mut raw = egui::RawInput::default();
    raw.focused = true;
    raw.time = Some(0.0);
    raw.pixels_per_point = Some(1.0);
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(600.0, 240.0),
    ));

    ctx.begin_frame(raw);
    egui::CentralPanel::default().show(ctx, |ui| {
      error_infobar_accesskit_ui(ui, details_open);
    });
    ctx.end_frame()
  }

  #[test]
  fn error_infobar_details_toggle_works_with_space() {
    let ctx = egui::Context::default();
    let mut details_open = false;

    run_infobar_frame(&ctx, &mut details_open, Vec::new());
    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Space));
    assert!(
      !details_open,
      "expected Space to do nothing when the toggle is not focused"
    );

    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Tab));
    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Space));
    assert!(
      details_open,
      "expected Space to toggle details when focused via Tab"
    );
  }

  #[test]
  fn error_infobar_details_toggle_works_with_enter() {
    let ctx = egui::Context::default();
    let mut details_open = false;

    run_infobar_frame(&ctx, &mut details_open, Vec::new());
    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Enter));
    assert!(
      !details_open,
      "expected Enter to do nothing when the toggle is not focused"
    );

    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Tab));
    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Enter));
    assert!(
      details_open,
      "expected Enter to toggle details when focused via Tab"
    );
  }

  #[test]
  fn error_infobar_details_toggle_keeps_focus_and_id_when_label_changes() {
    let ctx = egui::Context::default();
    let mut details_open = false;

    let initial = run_infobar_frame(&ctx, &mut details_open, Vec::new());

    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Tab));
    let focused_closed = run_infobar_frame(&ctx, &mut details_open, Vec::new());
    assert!(
      focused_closed.has_focus,
      "expected details toggle to have focus after Tab"
    );
    assert_eq!(
      focused_closed.id, initial.id,
      "expected details toggle egui id to be stable while closed"
    );

    run_infobar_frame(&ctx, &mut details_open, key_press_release(egui::Key::Space));
    assert!(details_open, "expected Space to open details when focused");

    let focused_open = run_infobar_frame(&ctx, &mut details_open, Vec::new());
    assert!(
      focused_open.has_focus,
      "expected details toggle to retain focus after changing label"
    );
    assert_eq!(
      focused_open.id, initial.id,
      "expected details toggle egui id to be stable across label changes"
    );
  }

  #[test]
  fn error_infobar_emits_accesskit_names_for_retry_and_details_toggle() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut details_open = false;
    let output = run_infobar_accesskit_frame(&ctx, &mut details_open);
    let names = accesskit_names_from_full_output(&output);
    for expected in ["Retry navigation", "Show error details"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in error infobar output.\n\nnames: {names:#?}"
      );
    }

    details_open = true;
    let output = run_infobar_accesskit_frame(&ctx, &mut details_open);
    let names = accesskit_names_from_full_output(&output);
    for expected in ["Retry navigation", "Hide error details"] {
      assert!(
        names.iter().any(|n| n == expected),
        "expected AccessKit name {expected:?} in error infobar output.\n\nnames: {names:#?}"
      );
    }
  }

  fn details_node_from_output<'a>(
    output: &'a egui::FullOutput,
    expected_name: &str,
  ) -> &'a accesskit::Node {
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected AccessKit update to be emitted");
    update
      .nodes
      .iter()
      .find_map(|(_id, node)| {
        let name = node.name().unwrap_or("").trim();
        (name == expected_name).then_some(node)
      })
      .unwrap_or_else(|| {
        panic!(
          "expected to find AccessKit node with name {expected_name:?}.\n\nnames: {:#?}",
          accesskit_names_from_full_output(output)
        )
      })
  }

  #[test]
  fn error_infobar_details_toggle_exposes_expanded_state_and_expand_collapse_actions() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut details_open = false;
    let output = run_infobar_accesskit_frame(&ctx, &mut details_open);
    let node = details_node_from_output(&output, "Show error details");
    assert_eq!(
      node.is_expanded(),
      Some(false),
      "expected details toggle to be collapsed when closed"
    );
    assert!(
      node.supports_action(accesskit::Action::Expand),
      "expected collapsed details toggle to expose Expand action"
    );
    assert!(
      !node.supports_action(accesskit::Action::Collapse),
      "expected collapsed details toggle to not expose Collapse action"
    );

    details_open = true;
    let output = run_infobar_accesskit_frame(&ctx, &mut details_open);
    let node = details_node_from_output(&output, "Hide error details");
    assert_eq!(
      node.is_expanded(),
      Some(true),
      "expected details toggle to be expanded when open"
    );
    assert!(
      node.supports_action(accesskit::Action::Collapse),
      "expected expanded details toggle to expose Collapse action"
    );
    assert!(
      !node.supports_action(accesskit::Action::Expand),
      "expected expanded details toggle to not expose Expand action"
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod select_dropdown_a11y_tests {
  use super::App;
  use fastrender::tree::box_tree::{SelectControl, SelectItem};
  use std::sync::Arc;

  fn make_control() -> SelectControl {
    let items = vec![
      SelectItem::OptGroupLabel {
        label: "Group A".to_string(),
        disabled: false,
      },
      SelectItem::Option {
        node_id: 1,
        label: "Enabled option".to_string(),
        value: "enabled".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: true,
      },
      SelectItem::Option {
        node_id: 2,
        label: "Disabled option".to_string(),
        value: "disabled".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: true,
      },
      SelectItem::Option {
        node_id: 3,
        label: "Selected option".to_string(),
        value: "selected".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
    ];

    SelectControl {
      multiple: false,
      size: 1,
      items: Arc::new(items),
      selected: vec![3],
    }
  }

  #[test]
  fn select_dropdown_row_a11y_labels_are_non_empty() {
    let control = make_control();

    let mut optgroup_labels = Vec::new();
    let mut option_labels = Vec::new();
    for item in control.items.iter() {
      match item {
        SelectItem::OptGroupLabel { label, disabled } => {
          optgroup_labels.push(App::select_dropdown_optgroup_a11y_label(label, *disabled));
        }
        SelectItem::Option { label, value, .. } => {
          let base = if label.trim().is_empty() { value } else { label };
          option_labels.push(App::select_dropdown_option_a11y_label(base));
        }
      }
    }

    assert_eq!(optgroup_labels, vec!["Group A".to_string()]);
    assert_eq!(
      option_labels,
      vec![
        "Enabled option".to_string(),
        "Disabled option".to_string(),
        "Selected option".to_string(),
      ]
    );
    assert!(option_labels.iter().all(|label| !label.trim().is_empty()));
  }

  #[test]
  fn select_dropdown_focus_target_prefers_selected_enabled_else_first_enabled() {
    let control = make_control();
    assert_eq!(
      App::select_dropdown_focus_target_item_index(&control),
      Some(3)
    );

    // If the selected option is disabled, focus should fall back to the first enabled option.
    let mut items = (*control.items).clone();
    match &mut items[3] {
      SelectItem::Option { disabled, .. } => *disabled = true,
      _ => panic!("expected option"),
    }
    let control = SelectControl {
      items: Arc::new(items),
      ..control
    };
    assert_eq!(
      App::select_dropdown_focus_target_item_index(&control),
      Some(1)
    );
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod page_form_popup_focus_tests {
  use super::{
    close_date_time_picker_popup, egui_focused_widget_id, OpenDateTimePicker,
  };
  use fastrender::geometry::Rect;
  use fastrender::ui::messages::DateTimeInputKind;
  use fastrender::ui::TabId;

  fn begin_frame(ctx: &egui::Context) {
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    // Keep unit tests deterministic: avoid egui falling back to OS time for animations.
    raw.time = Some(0.0);
    raw.focused = true;
    ctx.begin_frame(raw);
  }

  #[test]
  fn date_time_picker_text_edit_focus_is_cleared_on_close() {
    let ctx = egui::Context::default();
    let mut draft = String::new();
    let picker_text_id = egui::Id::new("test_date_time_picker_text_edit");

    // Frame 1: "open" the picker and focus the text edit.
    begin_frame(&ctx);
    egui::CentralPanel::default().show(&ctx, |ui| {
      // Mirror the browser UI behaviour: focus requests are applied before constructing the widget.
      ui.memory_mut(|mem| mem.request_focus(picker_text_id));
      ui.add(egui::TextEdit::singleline(&mut draft).id(picker_text_id));
    });
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(picker_text_id)),
      "expected the picker text edit to hold egui focus"
    );
    assert!(
      ctx.wants_keyboard_input(),
      "expected the picker text edit to claim keyboard focus"
    );
    // Simulate the browser closing the picker with no previous focused id.
    let mut open_picker = Some(OpenDateTimePicker {
      tab_id: TabId(1),
      input_node_id: 1,
      kind: DateTimeInputKind::DateTimeLocal,
      focus_before_open: None,
      anchor_css: Rect::ZERO,
      anchor_points: egui::pos2(0.0, 0.0),
      value: String::new(),
    });
    let mut picker_rect = Some(egui::Rect::NOTHING);
    close_date_time_picker_popup(&ctx, &mut open_picker, &mut picker_rect);
    assert_eq!(
      egui_focused_widget_id(&ctx),
      None,
      "expected closing the picker to clear focus immediately"
    );

    begin_frame(&ctx);
    egui::CentralPanel::default().show(&ctx, |_ui| {});
    let _ = ctx.end_frame();

    assert!(
      !ctx.memory(|mem| mem.has_focus(picker_text_id)),
      "expected closing the picker to clear focus from the removed picker text edit"
    );
    assert!(
      !ctx.wants_keyboard_input(),
      "expected closing the picker to clear egui keyboard focus"
    );
  }

  #[test]
  fn date_time_picker_close_restores_previous_focus() {
    let ctx = egui::Context::default();
    let prev_id = egui::Id::new("test_prev_text_edit");
    let picker_text_id = egui::Id::new("test_picker_text_edit");
    let mut prev_text = String::new();
    let mut picker_text = String::new();

    // Frame 1: focus a stable widget (simulating chrome focus before opening the picker).
    begin_frame(&ctx);
    egui::CentralPanel::default().show(&ctx, |ui| {
      ui.memory_mut(|mem| mem.request_focus(prev_id));
      ui.add(egui::TextEdit::singleline(&mut prev_text).id(prev_id));
    });
    let _ = ctx.end_frame();
    assert!(ctx.memory(|mem| mem.has_focus(prev_id)));

    // Frame 2: picker opens and takes focus.
    begin_frame(&ctx);
    egui::CentralPanel::default().show(&ctx, |ui| {
      ui.add(egui::TextEdit::singleline(&mut prev_text).id(prev_id));
      ui.memory_mut(|mem| mem.request_focus(picker_text_id));
      ui.add(egui::TextEdit::singleline(&mut picker_text).id(picker_text_id));
    });
    let _ = ctx.end_frame();
    assert!(ctx.memory(|mem| mem.has_focus(picker_text_id)));

    // Close picker: focus should be restored to the previous widget.
    let mut open_picker = Some(OpenDateTimePicker {
      tab_id: TabId(1),
      input_node_id: 1,
      kind: DateTimeInputKind::DateTimeLocal,
      focus_before_open: Some(prev_id),
      anchor_css: Rect::ZERO,
      anchor_points: egui::pos2(0.0, 0.0),
      value: String::new(),
    });
    let mut picker_rect = Some(egui::Rect::NOTHING);
    close_date_time_picker_popup(&ctx, &mut open_picker, &mut picker_rect);

    begin_frame(&ctx);
    egui::CentralPanel::default().show(&ctx, |ui| {
      ui.add(egui::TextEdit::singleline(&mut prev_text).id(prev_id));
    });
    let _ = ctx.end_frame();

    assert!(
      ctx.memory(|mem| mem.has_focus(prev_id)),
      "expected closing the picker to restore focus to the previously focused widget"
    );
    assert!(
      ctx.wants_keyboard_input(),
      "expected focus restore to keep keyboard input routed to the previous TextEdit"
    );
  }
}
