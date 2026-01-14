#![cfg(feature = "browser_ui")]

//! Reusable widgets for browser side panels and simple dialogs.
//!
//! The windowed browser UI has several surfaces with similar structure (History, Downloads,
//! Bookmarks, Clear browsing data). This module provides small, theme-aware building blocks so
//! those UIs can converge visually without duplicating egui boilerplate everywhere.
//!
//! Design goals:
//! - Respect the active egui theme (`ui.visuals()`).
//! - Use `UiMotion::from_ctx` for micro-interactions with reduced-motion support.
//! - Prefer output structs (responses + flags) so callers can integrate with their own state.

use crate::ui::motion::UiMotion;
use crate::ui::{icon, icon_button, icon_tinted, BrowserIcon};

/// Output for [`panel_header`].
#[derive(Debug)]
pub struct PanelHeaderOutput {
  pub close_response: egui::Response,
}

fn close_response_or_placeholder(
  ui: &mut egui::Ui,
  close_response: Option<egui::Response>,
) -> egui::Response {
  close_response.unwrap_or_else(|| ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover()))
}

/// Standard side-panel header: leading icon + title, with a trailing close button.
///
/// Callers own the close behavior via `on_close` so the widget can be reused by multiple panels.
pub fn panel_header(
  ui: &mut egui::Ui,
  icon_glyph: BrowserIcon,
  title: &str,
  on_close: impl FnOnce(),
) -> PanelHeaderOutput {
  panel_header_with_actions(ui, icon_glyph, title, |_| {}, on_close)
}

/// Standard side-panel header: leading icon + title, with trailing actions and a close button.
///
/// `trailing_actions` are placed to the left of the close button (Chrome-like).
pub fn panel_header_with_actions(
  ui: &mut egui::Ui,
  icon_glyph: BrowserIcon,
  title: &str,
  trailing_actions: impl FnOnce(&mut egui::Ui),
  on_close: impl FnOnce(),
) -> PanelHeaderOutput {
  let mut close_response: Option<egui::Response> = None;
  ui.horizontal(|ui| {
    let icon_side = ui.spacing().icon_width;
    icon(ui, icon_glyph, icon_side);
    ui.add_space(4.0);
    ui.heading(title);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let resp = icon_button(ui, BrowserIcon::Close, "Close", true);
      let mut label = String::with_capacity("Close ".len() + title.len());
      label.push_str("Close ");
      label.push_str(title);
      resp.widget_info(move || {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone())
      });
      if resp.clicked() {
        on_close();
      }
      close_response = Some(resp);
      trailing_actions(ui);
    });
  });

  PanelHeaderOutput {
    close_response: close_response_or_placeholder(ui, close_response),
  }
}

#[cfg(test)]
mod close_response_tests {
  use super::*;

  #[test]
  fn close_response_placeholder_is_zero_sized_and_non_clickable() {
    let ctx = egui::Context::default();
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
      egui::CentralPanel::default().show(ctx, |ui| {
        let resp = close_response_or_placeholder(ui, None);
        assert_eq!(resp.rect.size(), egui::Vec2::ZERO);
        assert!(!resp.clicked());
      });
    });
  }
}

/// Output for [`panel_search_field`].
#[derive(Debug)]
pub struct SearchFieldOutput {
  /// Response for the underlying `TextEdit`.
  pub response: egui::Response,
  /// Response for the clear button, when present.
  pub clear_response: Option<egui::Response>,
  /// True when the search text was cleared this frame (via the clear button or Escape key).
  pub cleared: bool,
  /// True when Escape was pressed while the search field had focus and the query was already empty.
  ///
  /// Panels can use this to implement standard browser UX:
  /// - Escape clears a non-empty query.
  /// - Escape again (with an empty query) closes the panel.
  pub request_close: bool,
  /// True when this helper consumed the `request_focus` flag and called `Response::request_focus`.
  pub focus_requested: bool,
}

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

fn with_alpha(color: egui::Color32, alpha: u8) -> egui::Color32 {
  egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Theme-aware search field suitable for side panels.
///
/// This uses a shared `Frame` so the leading search icon, text edit, and optional clear button read
/// as one control.
pub fn panel_search_field(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  text: &mut String,
  hint: &str,
  request_focus: &mut bool,
  a11y_label: &str,
) -> SearchFieldOutput {
  let visuals = ui.visuals().clone();
  let id = ui.make_persistent_id(id_source);
  let motion = UiMotion::from_ctx(ui.ctx());

  let desired_height = ui.spacing().interact_size.y;
  let inner_margin = egui::Margin::symmetric(ui.spacing().button_padding.x, ui.spacing().button_padding.y);

  // Initialize the output with a zero-sized placeholder response; we'll overwrite it once the
  // `TextEdit` is built.
  let (_dummy_id, dummy_rect) = ui.allocate_space(egui::Vec2::ZERO);
  let dummy_response = ui.interact(dummy_rect, id.with("dummy"), egui::Sense::hover());
  let mut output = SearchFieldOutput {
    response: dummy_response,
    clear_response: None,
    cleared: false,
    request_close: false,
    focus_requested: false,
  };

  let frame = egui::Frame::none()
    .fill(visuals.widgets.inactive.bg_fill)
    .stroke(egui::Stroke::NONE)
    .rounding(visuals.widgets.inactive.rounding)
    .inner_margin(inner_margin);

  let inner = frame.show(ui, |ui| {
    ui.set_min_height(desired_height);
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
      // Leading search glyph.
      let tint = ui.visuals().weak_text_color();
      let icon_side = ui.spacing().icon_width;
      icon_tinted(ui, BrowserIcon::Search, icon_side, tint);

      ui.add_space(4.0);

      let resp = ui.add_sized(
        egui::vec2(ui.available_width(), desired_height),
        egui::TextEdit::singleline(text)
          .id(id.with("input"))
          .hint_text(hint)
          .desired_width(f32::INFINITY)
          .frame(false),
      );
      let label = a11y_label.to_string();
      resp.widget_info(move || {
        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, label.clone())
      });

      output.response = resp;

      if !text.is_empty() {
        ui.add_space(2.0);
        let clear_resp = icon_button(ui, BrowserIcon::Close, "Clear", true);
        clear_resp.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Clear search")
        });
        output.clear_response = Some(clear_resp);
      }
    });
  });

  // Paint a hover/focus stroke on top of the frame. We draw this ourselves (instead of relying on
  // `Frame::stroke`) so we can animate it with reduced-motion support.
  let hover_t = motion.animate_bool(
    ui.ctx(),
    id.with("hover"),
    ui.is_enabled() && (inner.response.hovered() || output.response.has_focus()),
    motion.durations.hover_fade,
  );
  if ui.is_rect_visible(inner.response.rect) {
    let base = visuals.widgets.inactive.bg_stroke;
    let hover = visuals.widgets.hovered.bg_stroke;
    let stroke = egui::Stroke::new(lerp(base.width, hover.width, hover_t), lerp_color(base.color, hover.color, hover_t));
    ui.painter().rect_stroke(
      inner.response.rect,
      visuals.widgets.inactive.rounding,
      stroke,
    );
  }

  // Support standard browser Escape semantics while focused:
  // - If the query is non-empty, Escape clears it (consuming the key so outer surfaces don't also
  //   interpret it as "close the panel").
  // - If the query is already empty, Escape requests that the surrounding panel close.
  if output.response.has_focus() {
    if !text.is_empty() {
      if ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)) {
        text.clear();
        output.cleared = true;
        // Keep focus on the input so keyboard users can continue typing.
        output.response.request_focus();
      }
    } else if ui.input_mut(|i| i.consume_key(Default::default(), egui::Key::Escape)) {
      output.request_close = true;
    }
  }

  if let Some(clear_resp) = output.clear_response.as_ref() {
    if clear_resp.clicked() {
      text.clear();
      output.cleared = true;
      output.response.request_focus();
    }
  }

  if *request_focus {
    output.response.request_focus();
    *request_focus = false;
    output.focus_requested = true;
  }

  output
}

/// Output for [`panel_list_row`].
#[derive(Debug)]
pub struct PanelListRowResponse {
  /// Response for the row hit-target. This is clickable and focusable.
  pub response: egui::Response,
}

/// A standard list row for side panels (History/Downloads/etc).
///
/// - Large hit target (`interact_size` + padding).
/// - Subtle hover highlight (with reduced-motion support).
/// - Supports optional leading icon and trailing action widgets.
pub fn panel_list_row(
  ui: &mut egui::Ui,
  id_source: impl std::hash::Hash,
  primary: impl Into<egui::WidgetText>,
  secondary: Option<egui::WidgetText>,
  tertiary: Option<egui::WidgetText>,
  leading_icon: Option<BrowserIcon>,
  trailing_actions: impl FnOnce(&mut egui::Ui),
) -> PanelListRowResponse {
  // Scope all nested widgets under a stable row id so egui auto-ids (and therefore AccessKit node
  // IDs) don't shift when the list ordering changes between frames.
  ui
    .push_id(id_source, |ui| {
      let id = ui.make_persistent_id("panel_list_row");
      let motion = UiMotion::from_ctx(ui.ctx());
      let visuals = ui.visuals().clone();

      let padding = ui.spacing().button_padding;
      let text_h = ui.text_style_height(&egui::TextStyle::Body);
      let small_h = ui.text_style_height(&egui::TextStyle::Small);
      let mut content_h = text_h;
      if secondary.is_some() {
        content_h += small_h;
      }
      if tertiary.is_some() {
        content_h += small_h;
      }
      let row_h = (content_h + padding.y * 2.0).max(ui.spacing().interact_size.y.max(30.0));

      let (_row_id, rect) = ui.allocate_space(egui::vec2(ui.available_width(), row_h));
      let response = ui.interact(rect, id.with("row"), egui::Sense::click());

      let hover_t = motion.animate_bool(
        ui.ctx(),
        id.with("hover"),
        ui.is_enabled() && (response.hovered() || response.has_focus()),
        motion.durations.hover_fade,
      );

      // Hover highlight (fade in/out, but stays static when reduced motion is enabled).
      if ui.is_rect_visible(rect) {
        let hovered = visuals.widgets.hovered;
        let mut fill = hovered.bg_fill;
        let alpha = (fill.a() as f32 * hover_t).round().clamp(0.0, 255.0) as u8;
        fill = with_alpha(fill, alpha);

        let rounding = visuals.widgets.inactive.rounding;
        ui.painter().rect(rect, rounding, fill, egui::Stroke::NONE);

        if response.has_focus() {
          let focus_stroke = visuals.selection.stroke;
          let expand = 1.0 + focus_stroke.width * 0.5;
          let focus_rect = rect.expand(expand);
          let focus_rounding = egui::Rounding::same(rounding.nw + expand);
          ui
            .painter()
            .rect_stroke(focus_rect, focus_rounding, focus_stroke);
        }
      }

      // Layout: reserve a small trailing area for action buttons so the primary text can truncate in
      // narrow panels without colliding with trailing controls.
      let mut inner_rect = rect.shrink2(padding);
      if inner_rect.width() < 1.0 || inner_rect.height() < 1.0 {
        return PanelListRowResponse { response };
      }

      let action_button_side = ui.spacing().interact_size.y;
      let reserved_actions_w = (action_button_side * 2.5).min(inner_rect.width());
      let actions_rect = egui::Rect::from_min_max(
        egui::pos2(inner_rect.max.x - reserved_actions_w, inner_rect.min.y),
        inner_rect.max,
      );
      inner_rect.max.x = actions_rect.min.x;

      ui.allocate_ui_at_rect(actions_rect, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing_actions);
      });

      ui.allocate_ui_at_rect(inner_rect, |ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
          if let Some(icon) = leading_icon {
            let side = ui.spacing().icon_width;
            icon_tinted(ui, icon, side, ui.visuals().text_color());
            ui.add_space(6.0);
          }

          ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            let primary = primary.into();
            ui.add(egui::Label::new(primary).truncate(true));

            let weak = ui.visuals().weak_text_color();
            if let Some(secondary) = secondary {
              ui.scope(|ui| {
                ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
                ui.visuals_mut().override_text_color = Some(weak);
                ui.add(egui::Label::new(secondary).truncate(true));
              });
            }
            if let Some(tertiary) = tertiary {
              ui.scope(|ui| {
                ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
                ui.visuals_mut().override_text_color = Some(weak);
                ui.add(egui::Label::new(tertiary).truncate(true));
              });
            }
          });
        });
      });

      PanelListRowResponse { response }
    })
    .inner
}

/// Output for [`panel_empty_state`].
#[derive(Debug, Default)]
pub struct PanelEmptyStateOutput {
  /// Response for the optional action button.
  pub action_response: Option<egui::Response>,
}

/// Side-panel empty state: icon + headline + optional detail + optional action button.
///
/// This returns the action button response (when present) so callers can decide what to do when it
/// is clicked.
pub fn panel_empty_state(
  ui: &mut egui::Ui,
  icon_glyph: BrowserIcon,
  headline: &str,
  detail: Option<&str>,
  action_label: Option<&str>,
) -> PanelEmptyStateOutput {
  let available = ui.available_size();
  let mut out = PanelEmptyStateOutput::default();

  ui.allocate_ui_with_layout(
    available,
    egui::Layout::top_down(egui::Align::Center),
    |ui| {
      ui.add_space((ui.available_height() * 0.18).max(8.0));
      let icon_side = ui.spacing().icon_width * 2.5;
      icon_tinted(ui, icon_glyph, icon_side, ui.visuals().weak_text_color());
      ui.add_space(10.0);
      ui.heading(headline);
      if let Some(detail) = detail.filter(|s| !s.trim().is_empty()) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(detail).color(ui.visuals().weak_text_color()));
      }
      if let Some(label) = action_label.filter(|s| !s.trim().is_empty()) {
        ui.add_space(12.0);
        out.action_response = Some(ui.button(label));
      }
    },
  );

  out
}

fn text_contrast_color(bg: egui::Color32) -> egui::Color32 {
  // Simple sRGB luma heuristic. Good enough for picking a readable text color.
  let r = bg.r() as f32 / 255.0;
  let g = bg.g() as f32 / 255.0;
  let b = bg.b() as f32 / 255.0;
  let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  if luma > 0.5 {
    egui::Color32::BLACK
  } else {
    egui::Color32::WHITE
  }
}

/// Theme-aware destructive action button.
///
/// Uses `ui.visuals().error_fg_color` (theme danger color) and keeps the label readable even in
/// high-contrast mode by selecting an appropriate foreground text color.
pub fn danger_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
  let motion = UiMotion::from_ctx(ui.ctx());
  let danger = ui.visuals().error_fg_color;
  let text_color = text_contrast_color(danger);

  // Build a button-like widget with subtle hover fade. We draw it manually so we can blend between
  // idle/hover states without relying on egui's instantaneous widget visuals.
  let padding = ui.spacing().button_padding;
  let font_id = ui
    .style()
    .text_styles
    .get(&egui::TextStyle::Button)
    .cloned()
    .unwrap_or_else(|| egui::FontId::proportional(14.0));

  let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id.clone(), text_color));
  let desired_size = egui::vec2(galley.size().x + padding.x * 2.0, galley.size().y + padding.y * 2.0);

  let (_btn_id, rect) = ui.allocate_space(desired_size);
  let response = ui.interact(rect, ui.make_persistent_id(("danger_button", label)), egui::Sense::click());

  let hover_t = motion.animate_bool(
    ui.ctx(),
    response.id.with("hover"),
    ui.is_enabled() && (response.hovered() || response.has_focus()),
    motion.durations.hover_fade,
  );

  if ui.is_rect_visible(rect) {
    // Idle: slightly translucent fill with danger-colored stroke.
    let idle_fill = with_alpha(danger, if ui.visuals().dark_mode { 40 } else { 28 });
    let hover_fill = with_alpha(danger, if ui.visuals().dark_mode { 70 } else { 55 });
    let fill = if hover_t <= 0.0 {
      idle_fill
    } else if hover_t >= 1.0 {
      hover_fill
    } else {
      // Blend alpha only (cheap and theme-safe).
      let a = (idle_fill.a() as f32 + (hover_fill.a() as f32 - idle_fill.a() as f32) * hover_t)
        .round()
        .clamp(0.0, 255.0) as u8;
      with_alpha(danger, a)
    };

    let rounding = ui.visuals().widgets.inactive.rounding;
    let stroke = egui::Stroke::new(ui.visuals().widgets.inactive.bg_stroke.width, danger);
    ui.painter().rect(rect, rounding, fill, stroke);

    let text_pos = egui::pos2(
      rect.center().x - galley.size().x * 0.5,
      rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(text_pos, galley);

    if response.has_focus() {
      let focus_stroke = ui.visuals().selection.stroke;
      let expand = 1.0 + focus_stroke.width * 0.5;
      let focus_rect = rect.expand(expand);
      let focus_rounding = egui::Rounding::same(rounding.nw + expand);
      ui
        .painter()
        .rect_stroke(focus_rect, focus_rounding, focus_stroke);
    }
  }

  let label = label.to_string();
  response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label.clone()));
  response
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::panel_list_row;
  use crate::ui::{a11y_test_util, icon_button, BrowserIcon};

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

  fn render_rows(ctx: &egui::Context, include_extra_row: bool) -> egui::FullOutput {
    begin_frame(ctx);

    egui::CentralPanel::default().show(ctx, |ui| {
      if include_extra_row {
        panel_list_row(
          ui,
          "row_extra",
          "Extra row",
          None,
          None,
          None,
          |ui| {
            let resp = icon_button(ui, BrowserIcon::Trash, "Delete", true);
            resp.widget_info(|| {
              egui::WidgetInfo::labeled(egui::WidgetType::Button, "Extra row delete action")
            });
          },
        );
      }

      panel_list_row(ui, "row_a", "Row A", None, None, None, |ui| {
        let resp = icon_button(ui, BrowserIcon::Trash, "Delete", true);
        resp.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Row A delete action")
        });
      });

      panel_list_row(ui, "row_b", "Row B", None, None, None, |ui| {
        let resp = icon_button(ui, BrowserIcon::Trash, "Delete", true);
        resp.widget_info(|| {
          egui::WidgetInfo::labeled(egui::WidgetType::Button, "Row B delete action")
        });
      });
    });

    ctx.end_frame()
  }

  fn accesskit_id_for_name(output: &egui::FullOutput, name: &str) -> String {
    let snapshot = a11y_test_util::accesskit_snapshot_from_full_output(output);
    let pretty = a11y_test_util::accesskit_pretty_json_from_full_output(output);
    let matches: Vec<_> = snapshot.nodes.iter().filter(|n| n.name == name).collect();
    assert!(
      matches.len() == 1,
      "expected exactly one AccessKit node with name {name:?}, found {}.\n\nsnapshot:\n{pretty}",
      matches.len()
    );
    matches[0].id.clone()
  }

  #[test]
  fn panel_list_row_trailing_action_accesskit_ids_are_stable_across_row_insertion() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let output = render_rows(&ctx, false);
    let id_a = accesskit_id_for_name(&output, "Row A delete action");
    let id_b = accesskit_id_for_name(&output, "Row B delete action");
    assert_ne!(
      id_a, id_b,
      "expected identical trailing action buttons in different rows to have distinct AccessKit node IDs"
    );

    let output_with_extra = render_rows(&ctx, true);
    let id_b_after = accesskit_id_for_name(&output_with_extra, "Row B delete action");
    assert_eq!(
      id_b, id_b_after,
      "expected Row B trailing action AccessKit node ID to remain stable when a row is inserted before it"
    );
  }
}
