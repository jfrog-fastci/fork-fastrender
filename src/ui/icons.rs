#![cfg(feature = "browser_ui")]

use crate::ui::motion::UiMotion;
use lru::LruCache;
use std::num::NonZeroUsize;

/// Repo-owned browser chrome icons.
///
/// These are rasterized from embedded SVG at an integer pixel size (based on
/// `egui::Context::pixels_per_point`) and cached as egui textures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserIcon {
  Back,
  Forward,
  ArrowUp,
  ArrowDown,
  Reload,
  StopLoading,
  Home,
  Menu,
  Search,
  History,
  Download,
  Tab,
  Copy,
  Check,
  Close,
  CloseTab,
  NewTab,
  OpenInNewTab,
  Appearance,
  ZoomIn,
  ZoomOut,
  LockSecure,
  WarningInsecure,
  Error,
  Spinner,
  Info,
  Trash,
  Edit,
  Folder,
  Plus,
  BookmarkOutline,
  BookmarkFilled,
  // Media controls (windowed browser overlay / HTMLMediaElement UI).
  Play,
  Pause,
  Volume,
  Mute,
  Fullscreen,
  ExitFullscreen,
}

impl BrowserIcon {
  fn name(self) -> &'static str {
    match self {
      Self::Back => "back",
      Self::Forward => "forward",
      Self::ArrowUp => "arrow_up",
      Self::ArrowDown => "arrow_down",
      Self::Reload => "reload",
      Self::StopLoading => "stop_loading",
      Self::Home => "home",
      Self::Menu => "menu",
      Self::Search => "search",
      Self::History => "history",
      Self::Download => "download",
      Self::Tab => "tab",
      Self::Copy => "copy",
      Self::Check => "check",
      // `Close` and `CloseTab` intentionally share the same glyph/asset; keep distinct variants for
      // accessibility labels while deduplicating the underlying texture cache entry.
      Self::Close => "close_tab",
      Self::CloseTab => "close_tab",
      Self::NewTab => "new_tab",
      Self::OpenInNewTab => "open_in_new_tab",
      Self::Appearance => "appearance",
      Self::ZoomIn => "zoom_in",
      Self::ZoomOut => "zoom_out",
      Self::LockSecure => "lock_secure",
      Self::WarningInsecure => "warning_insecure",
      Self::Error => "error",
      Self::Spinner => "spinner",
      Self::Info => "info",
      Self::Trash => "trash",
      Self::Edit => "edit",
      Self::Folder => "folder",
      Self::Plus => "plus",
      Self::BookmarkOutline => "bookmark_outline",
      Self::BookmarkFilled => "bookmark_filled",
      Self::Play => "play",
      Self::Pause => "pause",
      Self::Volume => "volume",
      Self::Mute => "mute",
      Self::Fullscreen => "fullscreen",
      Self::ExitFullscreen => "fullscreen_exit",
    }
  }

  /// Accessible name for icon-only controls.
  ///
  /// This is used for egui/AccessKit `WidgetInfo` so screen readers announce a semantic label
  /// ("Back") instead of a generic "button" or raw icon description.
  pub const fn a11y_label(self) -> &'static str {
    match self {
      Self::Back => "Back",
      Self::Forward => "Forward",
      Self::ArrowUp => "Up",
      Self::ArrowDown => "Down",
      Self::Reload => "Reload",
      Self::StopLoading => "Stop loading",
      Self::Home => "Home",
      Self::Menu => "Menu",
      Self::Search => "Search",
      Self::History => "History",
      Self::Download => "Downloads",
      Self::Tab => "Tab",
      Self::Copy => "Copy",
      Self::Check => "Check",
      Self::Close => "Close",
      Self::CloseTab => "Close tab",
      Self::NewTab => "New tab",
      Self::OpenInNewTab => "Open in new tab",
      Self::Appearance => "Appearance",
      Self::ZoomIn => "Zoom in",
      Self::ZoomOut => "Zoom out",
      Self::LockSecure => "Secure connection",
      Self::WarningInsecure => "Not secure",
      Self::Error => "Error",
      Self::Spinner => "Loading",
      Self::Info => "Info",
      Self::Trash => "Delete",
      Self::Edit => "Edit",
      Self::Folder => "Folder",
      Self::Plus => "Add",
      Self::BookmarkOutline => "Bookmark",
      Self::BookmarkFilled => "Bookmark",
      Self::Play => "Play",
      Self::Pause => "Pause",
      Self::Volume => "Volume",
      Self::Mute => "Mute",
      Self::Fullscreen => "Fullscreen",
      Self::ExitFullscreen => "Exit fullscreen",
    }
  }

  fn svg_bytes(self) -> &'static [u8] {
    match self {
      Self::Back => include_bytes!("../../assets/browser_icons/back.svg"),
      Self::Forward => include_bytes!("../../assets/browser_icons/forward.svg"),
      Self::ArrowUp => include_bytes!("../../assets/browser_icons/arrow_up.svg"),
      Self::ArrowDown => include_bytes!("../../assets/browser_icons/arrow_down.svg"),
      Self::Reload => include_bytes!("../../assets/browser_icons/reload.svg"),
      Self::StopLoading => include_bytes!("../../assets/browser_icons/stop_loading.svg"),
      Self::Home => include_bytes!("../../assets/browser_icons/home.svg"),
      Self::Menu => include_bytes!("../../assets/browser_icons/menu.svg"),
      Self::Search => include_bytes!("../../assets/browser_icons/search.svg"),
      Self::History => include_bytes!("../../assets/browser_icons/history.svg"),
      Self::Download => include_bytes!("../../assets/browser_icons/download.svg"),
      Self::Tab => include_bytes!("../../assets/browser_icons/tab.svg"),
      Self::Copy => include_bytes!("../../assets/browser_icons/copy.svg"),
      Self::Check => include_bytes!("../../assets/browser_icons/check.svg"),
      Self::Close => include_bytes!("../../assets/browser_icons/close_tab.svg"),
      Self::CloseTab => include_bytes!("../../assets/browser_icons/close_tab.svg"),
      Self::NewTab => include_bytes!("../../assets/browser_icons/new_tab.svg"),
      Self::OpenInNewTab => include_bytes!("../../assets/browser_icons/open_in_new_tab.svg"),
      Self::Appearance => include_bytes!("../../assets/browser_icons/appearance.svg"),
      Self::ZoomIn => include_bytes!("../../assets/browser_icons/zoom_in.svg"),
      Self::ZoomOut => include_bytes!("../../assets/browser_icons/zoom_out.svg"),
      Self::LockSecure => include_bytes!("../../assets/browser_icons/lock_secure.svg"),
      Self::WarningInsecure => include_bytes!("../../assets/browser_icons/warning_insecure.svg"),
      Self::Error => include_bytes!("../../assets/browser_icons/error.svg"),
      Self::Spinner => include_bytes!("../../assets/browser_icons/spinner.svg"),
      Self::Info => include_bytes!("../../assets/browser_icons/info.svg"),
      Self::Trash => include_bytes!("../../assets/browser_icons/trash.svg"),
      Self::Edit => include_bytes!("../../assets/browser_icons/edit.svg"),
      Self::Folder => include_bytes!("../../assets/browser_icons/folder.svg"),
      Self::Plus => include_bytes!("../../assets/browser_icons/plus.svg"),
      Self::BookmarkOutline => include_bytes!("../../assets/browser_icons/bookmark_outline.svg"),
      Self::BookmarkFilled => include_bytes!("../../assets/browser_icons/bookmark_filled.svg"),
      Self::Play => include_bytes!("../../assets/browser_icons/play.svg"),
      Self::Pause => include_bytes!("../../assets/browser_icons/pause.svg"),
      Self::Volume => include_bytes!("../../assets/browser_icons/volume.svg"),
      Self::Mute => include_bytes!("../../assets/browser_icons/mute.svg"),
      Self::Fullscreen => include_bytes!("../../assets/browser_icons/fullscreen.svg"),
      Self::ExitFullscreen => include_bytes!("../../assets/browser_icons/fullscreen_exit.svg"),
    }
  }
}

/// Maximum icon side length we will rasterize (in physical pixels).
///
/// This keeps `ctx.load_texture` allocations bounded even if a caller accidentally requests an
/// absurd icon size.
const MAX_ICON_SIDE_PX: u32 = 256;

/// Upper bound for icon cache entries per egui context.
///
/// We only have a handful of icons and a small set of sizes (typically one per DPI scale), but
/// using an LRU keeps memory bounded if callers request many different sizes.
const ICON_CACHE_CAPACITY: usize = 128;

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
  lerp(a as f32, b as f32, t).round().clamp(0.0, 255.0) as u8
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
  let [ar, ag, ab, aa] = a.to_array();
  let [br, bg, bb, ba] = b.to_array();
  egui::Color32::from_rgba_unmultiplied(
    lerp_u8(ar, br, t),
    lerp_u8(ag, bg, t),
    lerp_u8(ab, bb, t),
    lerp_u8(aa, ba, t),
  )
}

fn lerp_stroke(a: egui::Stroke, b: egui::Stroke, t: f32) -> egui::Stroke {
  egui::Stroke::new(lerp(a.width, b.width, t), lerp_color(a.color, b.color, t))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
  asset: &'static str,
  side_px: u32,
  dark_mode: bool,
}

#[derive(Debug, Clone)]
struct IconCache {
  textures: LruCache<CacheKey, egui::TextureHandle>,
  rasterize_calls: u64,
}

impl Default for IconCache {
  fn default() -> Self {
    Self {
      textures: LruCache::new(
        NonZeroUsize::new(ICON_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
      ),
      rasterize_calls: 0,
    }
  }
}

fn cache_id() -> egui::Id {
  egui::Id::new("fastrender_browser_icon_cache")
}

fn points_to_pixels(points: f32, pixels_per_point: f32) -> u32 {
  let px = (points * pixels_per_point).round();
  if !px.is_finite() || px <= 0.0 {
    1
  } else if px > MAX_ICON_SIDE_PX as f32 {
    MAX_ICON_SIDE_PX
  } else {
    px as u32
  }
}

fn actual_points_for_pixels(side_px: u32, pixels_per_point: f32) -> f32 {
  if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
    // Fallback: egui expects points; if scaling is invalid, treat px as points.
    return side_px as f32;
  }
  side_px as f32 / pixels_per_point
}

fn lookup_cached_texture(ctx: &egui::Context, key: CacheKey) -> Option<egui::TextureHandle> {
  ctx.data_mut(|d| {
    // Avoid holding the data lock longer than necessary; cloning a `TextureHandle` is cheap.
    let cache = d.get_temp_mut_or_default::<IconCache>(cache_id());
    cache.textures.get(&key).cloned()
  })
}

fn insert_cached_texture(ctx: &egui::Context, key: CacheKey, texture: egui::TextureHandle) {
  ctx.data_mut(|d| {
    let cache = d.get_temp_mut_or_default::<IconCache>(cache_id());
    cache.textures.put(key, texture);
  });
}

fn record_rasterize(ctx: &egui::Context) {
  ctx.data_mut(|d| {
    let cache = d.get_temp_mut_or_default::<IconCache>(cache_id());
    cache.rasterize_calls = cache.rasterize_calls.saturating_add(1);
  });
}

fn panic_payload_to_reason(panic: &(dyn std::any::Any + Send)) -> String {
  if let Some(s) = panic.downcast_ref::<&'static str>() {
    return (*s).to_string();
  }
  if let Some(s) = panic.downcast_ref::<String>() {
    return s.clone();
  }
  "unknown panic payload".to_string()
}

fn rasterize_svg_icon(
  icon: BrowserIcon,
  side_px: u32,
  _dark_mode: bool,
) -> Result<tiny_skia::Pixmap, String> {
  if side_px == 0 || side_px > MAX_ICON_SIDE_PX {
    return Err(format!(
      "icon side_px {side_px} exceeds MAX_ICON_SIDE_PX {MAX_ICON_SIDE_PX}"
    ));
  }

  let options = resvg::usvg::Options::default();
  let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::usvg::Tree::from_data(icon.svg_bytes(), &options)
  })) {
    Ok(Ok(tree)) => tree,
    Ok(Err(err)) => return Err(format!("failed to parse SVG: {err}")),
    Err(panic) => {
      return Err(format!(
        "SVG parse panicked: {}",
        panic_payload_to_reason(&*panic)
      ));
    }
  };

  let size = tree.size();
  let source_width = size.width();
  let source_height = size.height();
  if source_width <= 0.0 || source_height <= 0.0 {
    return Err("SVG has empty/invalid size".to_string());
  }

  let mut pixmap =
    tiny_skia::Pixmap::new(side_px, side_px).ok_or_else(|| "failed to create pixmap".to_string())?;

  let scale_x = side_px as f32 / source_width;
  let scale_y = side_px as f32 / source_height;
  let transform = tiny_skia::Transform::from_scale(scale_x, scale_y);

  if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::render(&tree, transform, &mut pixmap.as_mut());
  })) {
    return Err(format!(
      "SVG render panicked: {}",
      panic_payload_to_reason(&*panic)
    ));
  }
  Ok(pixmap)
}

fn load_icon_texture(
  ctx: &egui::Context,
  icon: BrowserIcon,
  side_px: u32,
  dark_mode: bool,
) -> egui::TextureHandle {
  let image = match rasterize_svg_icon(icon, side_px, dark_mode) {
    Ok(pixmap) => {
      let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
      egui::ColorImage::from_rgba_premultiplied([w, h], pixmap.data())
    }
    Err(_err) => egui::ColorImage::new([1, 1], egui::Color32::TRANSPARENT),
  };
  ctx.load_texture(
    format!("browser_icon_{}_{}_{}", icon.name(), side_px, dark_mode),
    image,
    egui::TextureOptions::LINEAR,
  )
}

fn icon_texture(
  ctx: &egui::Context,
  icon: BrowserIcon,
  side_points: f32,
  dark_mode: bool,
) -> (egui::TextureId, f32) {
  let pixels_per_point = ctx.pixels_per_point();
  let side_px = points_to_pixels(side_points, pixels_per_point);
  let side_points = actual_points_for_pixels(side_px, pixels_per_point);
  let key = CacheKey {
    asset: icon.name(),
    side_px,
    dark_mode,
  };

  if let Some(handle) = lookup_cached_texture(ctx, key) {
    return (handle.id(), side_points);
  }

  record_rasterize(ctx);
  let handle = load_icon_texture(ctx, icon, side_px, dark_mode);
  let id = handle.id();
  insert_cached_texture(ctx, key, handle);
  (id, side_points)
}

fn paint_icon(
  ui: &egui::Ui,
  rect: egui::Rect,
  icon: BrowserIcon,
  side_points: f32,
  tint: egui::Color32,
) {
  if !ui.is_rect_visible(rect) {
    return;
  }

  let (tex_id, side_points) = icon_texture(ui.ctx(), icon, side_points, ui.visuals().dark_mode);
  let size = egui::vec2(side_points, side_points);
  let icon_rect = egui::Rect::from_center_size(rect.center(), size);
  let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
  ui.painter().image(tex_id, icon_rect, uv, tint);
}

pub fn paint_icon_in_rect(
  ui: &egui::Ui,
  rect: egui::Rect,
  icon: BrowserIcon,
  side_points: f32,
  tint: egui::Color32,
) {
  paint_icon(ui, rect, icon, side_points, tint);
}

/// Draw a non-interactive icon at the given point size, tinted with the current theme's
/// foreground/text color.
pub fn icon(ui: &mut egui::Ui, icon: BrowserIcon, side_points: f32) -> egui::Response {
  icon_tinted(ui, icon, side_points, ui.visuals().text_color())
}

/// Draw a non-interactive icon at the given point size with an explicit tint color.
pub fn icon_tinted(
  ui: &mut egui::Ui,
  icon: BrowserIcon,
  side_points: f32,
  tint: egui::Color32,
) -> egui::Response {
  let pixels_per_point = ui.ctx().pixels_per_point();
  let side_px = points_to_pixels(side_points, pixels_per_point);
  let side_points = actual_points_for_pixels(side_px, pixels_per_point);
  let (rect, response) =
    ui.allocate_exact_size(egui::vec2(side_points, side_points), egui::Sense::hover());
  paint_icon(ui, rect, icon, side_points, tint);
  // Expose a label for icon-only status indicators (e.g. security lock icon). Hover text alone is
  // not sufficient for screen readers.
  let label = icon.a11y_label();
  response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label));
  response
}

struct IconButton {
  id: Option<egui::Id>,
  icon: BrowserIcon,
  tooltip: egui::WidgetText,
}

impl IconButton {
  fn new(icon: BrowserIcon, tooltip: egui::WidgetText) -> Self {
    Self {
      id: None,
      icon,
      tooltip,
    }
  }

  fn with_id(mut self, id: egui::Id) -> Self {
    self.id = Some(id);
    self
  }
}

impl egui::Widget for IconButton {
  fn ui(self, ui: &mut egui::Ui) -> egui::Response {
    let icon = self.icon;
    let side_points = ui.spacing().interact_size.y;
    let tooltip = self.tooltip;
    let desired_size = egui::vec2(side_points, side_points);
    let (rect, mut response) = if let Some(id) = self.id {
      // `allocate_space` now returns `(Id, Rect)` (the auto-generated widget id plus the rect).
      // We provide our own explicit id for deterministic focus/shortcuts, so we only need the rect.
      let (_allocated_id, rect) = ui.allocate_space(desired_size);
      let response = ui.interact(rect, id, egui::Sense::click());
      (rect, response)
    } else {
      ui.allocate_exact_size(desired_size, egui::Sense::click())
    };

    // Micro-interaction: fade between inactive/hovered button visuals.
    //
    // Egui's default widget visuals snap immediately between states. For the browser chrome we use
    // subtle animations so the toolbar feels responsive and premium, with reduced-motion support.
    let motion = UiMotion::from_ctx(ui.ctx());
    let hover_t = motion.animate_bool(
      ui.ctx(),
      response.id.with("hover"),
      ui.is_enabled() && response.hovered(),
      motion.durations.hover_fade,
    );

    let widgets = &ui.visuals().widgets;
    let inactive = if ui.is_enabled() {
      &widgets.inactive
    } else {
      &widgets.noninteractive
    };
    let hovered = if ui.is_enabled() {
      &widgets.hovered
    } else {
      inactive
    };
    let active = &widgets.active;

    let mut bg_fill = lerp_color(inactive.bg_fill, hovered.bg_fill, hover_t);
    let mut bg_stroke = lerp_stroke(inactive.bg_stroke, hovered.bg_stroke, hover_t);
    let mut fg_color = lerp_color(inactive.fg_stroke.color, hovered.fg_stroke.color, hover_t);
    let mut expansion = lerp(inactive.expansion, hovered.expansion, hover_t);
    let rounding = inactive.rounding;

    if ui.is_enabled() && response.is_pointer_button_down_on() {
      bg_fill = active.bg_fill;
      bg_stroke = active.bg_stroke;
      fg_color = active.fg_stroke.color;
      expansion = active.expansion;
    }

    let rect = rect.expand(expansion);

    if ui.is_rect_visible(rect) {
      ui.painter().rect(
        rect,
        rounding,
        bg_fill,
        bg_stroke,
      );

      // Use egui's canonical icon size and center it within the button rect.
      let icon_side = ui.spacing().icon_width;
      paint_icon(ui, rect, self.icon, icon_side, fg_color);
    }

    // Ensure keyboard focus is visible (important for a11y and non-mouse workflows).
    if response.has_focus() {
      let focus_stroke = ui.visuals().selection.stroke;
      let expand = 1.0 + focus_stroke.width * 0.5;
      let focus_rect = rect.expand(expand);
      let focus_rounding = egui::Rounding::same(rounding.nw + expand);
      ui
        .painter()
        .rect_stroke(focus_rect, focus_rounding, focus_stroke);
    }

    response = response.on_hover_text(tooltip.clone());
    if response.has_focus() && !response.hovered() {
      // Egui tooltips only show on pointer hover. Mirror the hover tooltip while keyboard-focused
      // so icon-only controls remain discoverable for keyboard-only users.
      egui::show_tooltip_text(ui.ctx(), response.id.with("focus_tooltip"), tooltip);
    }
    let label = icon.a11y_label();
    response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response
  }
}

/// A toolbar-friendly icon button.
///
/// This uses egui widget visuals so it matches the active theme (including disabled states), while
/// animating the hover transition when motion is enabled.
pub fn icon_button(
  ui: &mut egui::Ui,
  icon: BrowserIcon,
  tooltip: impl Into<egui::WidgetText>,
  enabled: bool,
) -> egui::Response {
  ui.add_enabled(enabled, IconButton::new(icon, tooltip.into()))
}

/// A toolbar-friendly icon button with an explicit egui id.
///
/// Use this for important, long-lived chrome controls so their widget ids (and derived AccessKit
/// node ids) remain stable even if surrounding UI structure changes.
pub fn icon_button_with_id(
  ui: &mut egui::Ui,
  id: egui::Id,
  icon: BrowserIcon,
  tooltip: impl Into<egui::WidgetText>,
  enabled: bool,
) -> egui::Response {
  ui.add_enabled(enabled, IconButton::new(icon, tooltip.into()).with_id(id))
}

/// A lightweight loading spinner.
///
/// Today this uses `egui::Spinner` directly (vector drawing, theme-aware). The SVG icon is still
/// included in `assets/browser_icons/` for completeness and potential future use.
pub fn spinner(ui: &mut egui::Ui, side_points: f32) -> egui::Response {
  let motion = UiMotion::from_ctx(ui.ctx());
  let response = if motion.enabled {
    ui.add(egui::Spinner::new().size(side_points))
  } else {
    // Reduced motion: render a static loading glyph so we don't trigger continuous repaints.
    icon_tinted(ui, BrowserIcon::Spinner, side_points, ui.visuals().text_color())
  };

  // `egui::Spinner` is often used without accompanying text (e.g. in compact toolbars). Give it a
  // stable label so assistive tech can announce what it represents.
  let label = BrowserIcon::Spinner.a11y_label();
  response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label));
  response
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn icon_buttons_have_accessible_labels() {
    for icon in [
      BrowserIcon::Back,
      BrowserIcon::Forward,
      BrowserIcon::ArrowUp,
      BrowserIcon::ArrowDown,
      BrowserIcon::Reload,
      BrowserIcon::StopLoading,
      BrowserIcon::Home,
      BrowserIcon::Menu,
      BrowserIcon::Search,
      BrowserIcon::History,
      BrowserIcon::Download,
      BrowserIcon::Tab,
      BrowserIcon::Copy,
      BrowserIcon::Check,
      BrowserIcon::Close,
      BrowserIcon::NewTab,
      BrowserIcon::CloseTab,
      BrowserIcon::OpenInNewTab,
      BrowserIcon::Appearance,
      BrowserIcon::ZoomOut,
      BrowserIcon::ZoomIn,
      BrowserIcon::LockSecure,
      BrowserIcon::WarningInsecure,
      BrowserIcon::Error,
      BrowserIcon::Spinner,
      BrowserIcon::Info,
      BrowserIcon::Trash,
      BrowserIcon::Edit,
      BrowserIcon::Folder,
      BrowserIcon::Plus,
      BrowserIcon::BookmarkOutline,
      BrowserIcon::BookmarkFilled,
      BrowserIcon::Play,
      BrowserIcon::Pause,
      BrowserIcon::Volume,
      BrowserIcon::Mute,
      BrowserIcon::Fullscreen,
      BrowserIcon::ExitFullscreen,
    ] {
      let label = icon.a11y_label();
      assert!(
        !label.trim().is_empty(),
        "expected {:?} to have a non-empty a11y label",
        icon
      );
    }
  }

  #[test]
  fn rasterize_produces_expected_dimensions() {
    let pixmap = rasterize_svg_icon(BrowserIcon::Back, 32, false).expect("rasterize should succeed");
    assert_eq!(pixmap.width(), 32);
    assert_eq!(pixmap.height(), 32);
    assert_eq!(pixmap.data().len(), 32 * 32 * 4);
  }

  #[test]
  fn rasterize_rejects_oversized_icons() {
    let err = rasterize_svg_icon(BrowserIcon::Back, MAX_ICON_SIDE_PX + 1, false)
      .expect_err("expected oversize rasterize to fail");
    assert!(
      err.contains("MAX_ICON_SIDE_PX"),
      "unexpected error message: {err}"
    );
  }

  #[test]
  fn cache_hits_avoid_rasterization() {
    let ctx = egui::Context::default();

    let (id_a, _size_a) = icon_texture(&ctx, BrowserIcon::Reload, 16.0, false);
    let calls_after_first = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });
    assert_eq!(calls_after_first, 1);

    let (id_b, _size_b) = icon_texture(&ctx, BrowserIcon::Reload, 16.0, false);
    let calls_after_second = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });

    assert_eq!(id_a, id_b);
    assert_eq!(
      calls_after_second, 1,
      "expected cache hit to avoid extra rasterization"
    );
  }

  #[test]
  fn icon_texture_clamps_huge_sizes_to_max_side_px() {
    let ctx = egui::Context::default();

    // Request an absurdly large icon. This should clamp rather than allocating a giant image/texture.
    let (_id, side_points) = icon_texture(&ctx, BrowserIcon::Back, 100_000.0, false);
    assert!(
      side_points <= MAX_ICON_SIDE_PX as f32,
      "expected side_points to clamp to MAX_ICON_SIDE_PX, got {side_points}"
    );
  }

  #[test]
  fn cache_keys_include_dark_mode() {
    let ctx = egui::Context::default();

    let (id_light, _size_light) = icon_texture(&ctx, BrowserIcon::Reload, 16.0, false);
    let calls_after_light = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });

    let (id_dark, _size_dark) = icon_texture(&ctx, BrowserIcon::Reload, 16.0, true);
    let calls_after_dark = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });

    assert_ne!(id_light, id_dark, "expected dark-mode key to create a new texture");
    assert_eq!(
      calls_after_light + 1,
      calls_after_dark,
      "expected dark-mode request to trigger one additional rasterization"
    );
  }

  #[test]
  fn icons_sharing_svg_assets_share_cache_entries() {
    let ctx = egui::Context::default();

    let (id_close_tab, _size_close_tab) = icon_texture(&ctx, BrowserIcon::CloseTab, 16.0, false);
    let calls_after_first = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });
    assert_eq!(calls_after_first, 1);

    let (id_close, _size_close) = icon_texture(&ctx, BrowserIcon::Close, 16.0, false);
    let calls_after_close = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });
    assert_eq!(id_close_tab, id_close);
    assert_eq!(
      calls_after_close, 1,
      "expected Close to reuse the CloseTab SVG texture"
    );

    let (id_stop, _size_stop) = icon_texture(&ctx, BrowserIcon::StopLoading, 16.0, false);
    let calls_after_stop = ctx.data_mut(|d| {
      d.get_temp_mut_or_default::<IconCache>(cache_id()).rasterize_calls
    });
    assert_ne!(id_close_tab, id_stop);
    assert_eq!(
      calls_after_stop, 2,
      "expected StopLoading to rasterize its own SVG texture"
    );
  }

  #[test]
  fn all_icons_rasterize_to_non_empty_alpha_mask() {
    // Guard against accidentally shipping empty/malformed SVG assets.
    let icons = [
      BrowserIcon::Back,
      BrowserIcon::Forward,
      BrowserIcon::ArrowUp,
      BrowserIcon::ArrowDown,
      BrowserIcon::Reload,
      BrowserIcon::StopLoading,
      BrowserIcon::Home,
      BrowserIcon::Menu,
      BrowserIcon::Search,
      BrowserIcon::History,
      BrowserIcon::Download,
      BrowserIcon::Tab,
      BrowserIcon::Copy,
      BrowserIcon::Check,
      BrowserIcon::Close,
      BrowserIcon::CloseTab,
      BrowserIcon::NewTab,
      BrowserIcon::OpenInNewTab,
      BrowserIcon::Appearance,
      BrowserIcon::ZoomIn,
      BrowserIcon::ZoomOut,
      BrowserIcon::LockSecure,
      BrowserIcon::WarningInsecure,
      BrowserIcon::Error,
      BrowserIcon::Spinner,
      BrowserIcon::Info,
      BrowserIcon::Trash,
      BrowserIcon::Edit,
      BrowserIcon::Folder,
      BrowserIcon::Plus,
      BrowserIcon::BookmarkOutline,
      BrowserIcon::BookmarkFilled,
      BrowserIcon::Play,
      BrowserIcon::Pause,
      BrowserIcon::Volume,
      BrowserIcon::Mute,
      BrowserIcon::Fullscreen,
      BrowserIcon::ExitFullscreen,
    ];

    for icon in icons {
      let pixmap = rasterize_svg_icon(icon, 32, false).expect("rasterize should succeed");
      let mut any_alpha = false;
      for a in pixmap.data().iter().skip(3).step_by(4) {
        if *a != 0 {
          any_alpha = true;
          break;
        }
      }
      assert!(any_alpha, "icon {icon:?} rendered fully transparent");
    }
  }
}
