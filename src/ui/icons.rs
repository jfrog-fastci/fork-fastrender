#![cfg(feature = "browser_ui")]

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
  Reload,
  CloseTab,
  NewTab,
  LockSecure,
  WarningInsecure,
  Error,
  Spinner,
}

impl BrowserIcon {
  fn name(self) -> &'static str {
    match self {
      Self::Back => "back",
      Self::Forward => "forward",
      Self::Reload => "reload",
      Self::CloseTab => "close_tab",
      Self::NewTab => "new_tab",
      Self::LockSecure => "lock_secure",
      Self::WarningInsecure => "warning_insecure",
      Self::Error => "error",
      Self::Spinner => "spinner",
    }
  }

  fn svg_bytes(self) -> &'static [u8] {
    match self {
      Self::Back => include_bytes!("../../assets/browser_icons/back.svg"),
      Self::Forward => include_bytes!("../../assets/browser_icons/forward.svg"),
      Self::Reload => include_bytes!("../../assets/browser_icons/reload.svg"),
      Self::CloseTab => include_bytes!("../../assets/browser_icons/close_tab.svg"),
      Self::NewTab => include_bytes!("../../assets/browser_icons/new_tab.svg"),
      Self::LockSecure => include_bytes!("../../assets/browser_icons/lock_secure.svg"),
      Self::WarningInsecure => include_bytes!("../../assets/browser_icons/warning_insecure.svg"),
      Self::Error => include_bytes!("../../assets/browser_icons/error.svg"),
      Self::Spinner => include_bytes!("../../assets/browser_icons/spinner.svg"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
  icon: BrowserIcon,
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
        NonZeroUsize::new(ICON_CACHE_CAPACITY).expect("ICON_CACHE_CAPACITY must be non-zero"),
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
    icon,
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
  let (rect, response) = ui.allocate_exact_size(egui::vec2(side_points, side_points), egui::Sense::hover());
  paint_icon(ui, rect, icon, side_points, tint);
  response
}

struct IconButton {
  icon: BrowserIcon,
  tooltip: egui::WidgetText,
}

impl IconButton {
  fn new(icon: BrowserIcon, tooltip: egui::WidgetText) -> Self {
    Self { icon, tooltip }
  }
}

impl egui::Widget for IconButton {
  fn ui(self, ui: &mut egui::Ui) -> egui::Response {
    let side_points = ui.spacing().interact_size.y;
    let (rect, mut response) = ui.allocate_exact_size(
      egui::vec2(side_points, side_points),
      egui::Sense::click(),
    );

    let visuals = ui.style().interact(&response);
    let rect = rect.expand(visuals.expansion);

    if ui.is_rect_visible(rect) {
      ui.painter().rect(
        rect,
        visuals.rounding,
        visuals.bg_fill,
        visuals.bg_stroke,
      );

      // Use egui's canonical icon size and center it within the button rect.
      let icon_side = ui.spacing().icon_width;
      paint_icon(ui, rect, self.icon, icon_side, visuals.fg_stroke.color);
    }

    response = response.on_hover_text(self.tooltip);
    response
  }
}

/// A toolbar-friendly icon button.
///
/// The icon is tinted using `ui.style().interact(&response).fg_stroke.color`, so it automatically
/// tracks the active theme (including hovered/disabled states).
pub fn icon_button(
  ui: &mut egui::Ui,
  icon: BrowserIcon,
  tooltip: impl Into<egui::WidgetText>,
  enabled: bool,
) -> egui::Response {
  ui.add_enabled(enabled, IconButton::new(icon, tooltip.into()))
}

/// A lightweight loading spinner.
///
/// Today this uses `egui::Spinner` directly (vector drawing, theme-aware). The SVG icon is still
/// included in `assets/browser_icons/` for completeness and potential future use.
pub fn spinner(ui: &mut egui::Ui, side_points: f32) -> egui::Response {
  ui.add(egui::Spinner::new().size(side_points))
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn all_icons_rasterize_to_non_empty_alpha_mask() {
    // Guard against accidentally shipping empty/malformed SVG assets.
    let icons = [
      BrowserIcon::Back,
      BrowserIcon::Forward,
      BrowserIcon::Reload,
      BrowserIcon::CloseTab,
      BrowserIcon::NewTab,
      BrowserIcon::LockSecure,
      BrowserIcon::WarningInsecure,
      BrowserIcon::Error,
      BrowserIcon::Spinner,
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
