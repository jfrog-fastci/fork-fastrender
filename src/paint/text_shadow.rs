use crate::style::color::Rgba;
use crate::style::values::Length;
use crate::style::ComputedStyle;

#[derive(Debug, Clone)]
pub struct ResolvedTextShadow {
  pub offset_x: f32,
  pub offset_y: f32,
  pub blur_radius: f32,
  pub color: Rgba,
}

#[derive(Debug, Clone, Copy)]
pub struct PathBounds {
  pub min_x: f32,
  pub min_y: f32,
  pub max_x: f32,
  pub max_y: f32,
}

impl PathBounds {
  pub fn new() -> Self {
    Self {
      min_x: f32::INFINITY,
      min_y: f32::INFINITY,
      max_x: f32::NEG_INFINITY,
      max_y: f32::NEG_INFINITY,
    }
  }

  pub fn include(&mut self, rect: &tiny_skia::Rect) {
    self.min_x = self.min_x.min(rect.left());
    self.min_y = self.min_y.min(rect.top());
    self.max_x = self.max_x.max(rect.right());
    self.max_y = self.max_y.max(rect.bottom());
  }

  pub fn is_valid(&self) -> bool {
    self.min_x.is_finite()
      && self.min_y.is_finite()
      && self.max_x.is_finite()
      && self.max_y.is_finite()
  }
}

pub fn resolve_text_shadows(style: &ComputedStyle) -> Vec<ResolvedTextShadow> {
  resolve_text_shadows_with_viewport(style, None)
}

pub fn resolve_text_shadows_with_viewport(
  style: &ComputedStyle,
  viewport: Option<(f32, f32)>,
) -> Vec<ResolvedTextShadow> {
  style
    .text_shadow
    .iter()
    .map(|shadow| ResolvedTextShadow {
      offset_x: resolve_shadow_length(
        &shadow.offset_x,
        style.font_size,
        style.root_font_size,
        viewport,
      ),
      offset_y: resolve_shadow_length(
        &shadow.offset_y,
        style.font_size,
        style.root_font_size,
        viewport,
      ),
      blur_radius: resolve_shadow_length(
        &shadow.blur_radius,
        style.font_size,
        style.root_font_size,
        viewport,
      )
      .max(0.0),
      color: shadow.color.unwrap_or(style.color),
    })
    .collect()
}

fn resolve_shadow_length(
  len: &Length,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
) -> f32 {
  crate::paint::paint_bounds::resolve_length_for_paint(
    len,
    font_size,
    root_font_size,
    font_size,
    viewport,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::TextShadow;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;
  use std::sync::Arc;

  #[test]
  fn text_shadow_rem_uses_root_font_size() {
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    style.root_font_size = 20.0;
    style.color = Rgba::BLACK;
    style.text_shadow = Arc::from(vec![TextShadow {
      offset_x: Length::rem(1.0),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: None,
    }]);

    let shadows = resolve_text_shadows_with_viewport(&style, None);
    assert_eq!(shadows.len(), 1);
    assert!((shadows[0].offset_x - 20.0).abs() < 0.01);
    assert!((shadows[0].offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn text_shadow_vw_resolves_against_viewport() {
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    style.root_font_size = 20.0;
    style.color = Rgba::BLACK;
    style.text_shadow = Arc::from(vec![TextShadow {
      offset_x: Length::new(10.0, LengthUnit::Vw),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: None,
    }]);

    let shadows = resolve_text_shadows_with_viewport(&style, Some((200.0, 100.0)));
    assert_eq!(shadows.len(), 1);
    assert!((shadows[0].offset_x - 20.0).abs() < 0.01);
  }

  #[test]
  fn text_shadow_vw_requires_viewport() {
    let mut style = ComputedStyle::default();
    style.font_size = 10.0;
    style.root_font_size = 20.0;
    style.color = Rgba::BLACK;
    style.text_shadow = Arc::from(vec![TextShadow {
      offset_x: Length::new(10.0, LengthUnit::Vw),
      offset_y: Length::px(0.0),
      blur_radius: Length::px(0.0),
      color: None,
    }]);

    let shadows = resolve_text_shadows_with_viewport(&style, None);
    assert_eq!(shadows.len(), 1);
    assert_eq!(shadows[0].offset_x, 0.0);
  }
}
