//! Root element font metrics for root-relative CSS units.
//!
//! CSS Values & Units 4 defines the root-relative font units (`rex`, `rch`, `rcap`, `ric`, `rlh`)
//! in terms of the *root element's* font metrics and used line-height. Layout computes these once
//! per document render and caches them on [`crate::text::font_loader::FontContext`] so that length
//! resolution does not need to repeatedly query font tables.
//!
//! All values are expressed in **CSS pixels**.
//!
//! Reference: <https://www.w3.org/TR/css-values-4/#font-relative-lengths>
//! (`rex`, `rch`, `rcap`, `ric`, `rlh`).

/// Root element font metrics and used line height in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RootFontMetrics {
  /// Root element computed font size in CSS px.
  pub root_font_size_px: f32,
  /// Root element x-height in CSS px.
  pub root_x_height_px: f32,
  /// Root element cap height in CSS px.
  pub root_cap_height_px: f32,
  /// Root element inline-axis advance of U+0030 ('0') in CSS px.
  pub root_ch_advance_px: f32,
  /// Root element inline-axis advance of a representative ideograph in CSS px.
  pub root_ic_advance_px: f32,
  /// Root element used line height in CSS px.
  pub root_used_line_height_px: f32,
}

