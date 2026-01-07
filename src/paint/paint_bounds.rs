use crate::geometry::Rect;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::FragmentNode;

pub(crate) fn fragment_paint_bounds(
  fragment: &FragmentNode,
  rect: Rect,
  style: Option<&ComputedStyle>,
  viewport: Option<(f32, f32)>,
) -> Rect {
  let mut bounds = rect;
  let Some(style) = style else {
    return bounds;
  };

  if let Some(outline) = outline_bounds(style, rect) {
    bounds = bounds.union(outline);
  }
  if let Some(shadows) = box_shadow_bounds(rect, style, false, viewport) {
    bounds = bounds.union(shadows);
  }
  if let Some(inset) = box_shadow_bounds(rect, style, true, viewport) {
    bounds = bounds.union(inset);
  }

  if !style.text_shadow.is_empty() {
    let mut min_x = rect.min_x();
    let mut min_y = rect.min_y();
    let mut max_x = rect.max_x();
    let mut max_y = rect.max_y();
    for shadow in style.text_shadow.iter() {
      let offset_x = resolve_length_for_paint(
        &shadow.offset_x,
        style.font_size,
        style.root_font_size,
        rect.width(),
        viewport,
      );
      let offset_y = resolve_length_for_paint(
        &shadow.offset_y,
        style.font_size,
        style.root_font_size,
        rect.width(),
        viewport,
      );
      let blur = resolve_length_for_paint(
        &shadow.blur_radius,
        style.font_size,
        style.root_font_size,
        rect.width(),
        viewport,
      )
      .max(0.0)
        * 3.0;
      min_x = min_x.min(rect.min_x() + offset_x - blur);
      min_y = min_y.min(rect.min_y() + offset_y - blur);
      max_x = max_x.max(rect.max_x() + offset_x + blur);
      max_y = max_y.max(rect.max_y() + offset_y + blur);
    }
    bounds = bounds.union(Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y));
  }

  if let Some(borders) = fragment.table_borders.as_ref() {
    bounds = bounds.union(borders.paint_bounds.translate(rect.origin));
  }

  bounds
}

pub(crate) fn resolve_length_for_paint(
  len: &Length,
  font_size: f32,
  root_font_size: f32,
  percentage_base: f32,
  viewport: Option<(f32, f32)>,
) -> f32 {
  if len.is_zero() {
    return 0.0;
  }

  if len.calc.is_some() {
    let needs_viewport = len.unit.is_viewport_relative()
      || len
        .calc
        .as_ref()
        .map(|c| c.has_viewport_relative())
        .unwrap_or(false);
    let (vw, vh) = match viewport {
      Some(vp) => vp,
      None if needs_viewport => (f32::NAN, f32::NAN),
      None => (percentage_base, percentage_base),
    };
    let resolved = len
      .resolve_with_context(Some(percentage_base), vw, vh, font_size, root_font_size)
      .unwrap_or_else(|| {
        if len.unit.is_absolute() {
          len.to_px()
        } else {
          len.value * font_size
        }
      });
    return if resolved.is_finite() { resolved } else { 0.0 };
  }

  let resolved = if len.unit.is_absolute() {
    len.to_px()
  } else if len.unit.is_percentage() {
    if percentage_base.is_finite() {
      (len.value / 100.0) * percentage_base
    } else {
      len.value * font_size
    }
  } else if len.unit.is_viewport_relative() {
    if let Some((vw, vh)) = viewport {
      len
        .resolve_with_viewport(vw, vh)
        .unwrap_or(len.value * font_size)
    } else {
      len.value * font_size
    }
  } else if len.unit.is_font_relative() {
    let px = if matches!(len.unit, LengthUnit::Rem) {
      root_font_size
    } else {
      font_size
    };
    len
      .resolve_with_font_size(px)
      .unwrap_or(len.value * font_size)
  } else {
    len.value
  };

  if resolved.is_finite() { resolved } else { 0.0 }
}

fn outline_bounds(style: &ComputedStyle, rect: Rect) -> Option<Rect> {
  let width = style.outline_width.to_px();
  let outline_style = style.outline_style.to_border_style();
  if width <= 0.0
    || matches!(
      outline_style,
      crate::style::types::BorderStyle::None | crate::style::types::BorderStyle::Hidden
    )
  {
    return None;
  }
  let offset = style.outline_offset.to_px();
  let expand = width.abs() + offset.abs();
  Some(rect.inflate(expand))
}

fn box_shadow_bounds(
  rect: Rect,
  style: &ComputedStyle,
  inset: bool,
  viewport: Option<(f32, f32)>,
) -> Option<Rect> {
  if style.box_shadow.is_empty() {
    return None;
  }

  let base_rect = if inset {
    let font_size = style.font_size;
    let base = rect.width().max(0.0);

    let border_left = resolve_length_for_paint(
      &style.used_border_left_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_right = resolve_length_for_paint(
      &style.used_border_right_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_top = resolve_length_for_paint(
      &style.used_border_top_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );
    let border_bottom = resolve_length_for_paint(
      &style.used_border_bottom_width(),
      font_size,
      style.root_font_size,
      base,
      viewport,
    );

    inset_rect(rect, border_left, border_top, border_right, border_bottom)
  } else {
    rect
  };

  let mut min_x = base_rect.min_x();
  let mut min_y = base_rect.min_y();
  let mut max_x = base_rect.max_x();
  let mut max_y = base_rect.max_y();
  let mut changed = false;

  for shadow in &style.box_shadow {
    if shadow.inset != inset {
      continue;
    }
    let offset_x = resolve_length_for_paint(
      &shadow.offset_x,
      style.font_size,
      style.root_font_size,
      rect.width(),
      viewport,
    );
    let offset_y = resolve_length_for_paint(
      &shadow.offset_y,
      style.font_size,
      style.root_font_size,
      rect.width(),
      viewport,
    );
    let blur = resolve_length_for_paint(
      &shadow.blur_radius,
      style.font_size,
      style.root_font_size,
      rect.width(),
      viewport,
    )
    .max(0.0);
    let spread = resolve_length_for_paint(
      &shadow.spread_radius,
      style.font_size,
      style.root_font_size,
      rect.width(),
      viewport,
    )
    .max(-1e6);
    let blur_pad = blur * 3.0;
    let left = blur_pad + spread - offset_x.min(0.0);
    let right = blur_pad + spread + offset_x.max(0.0);
    let top = blur_pad + spread - offset_y.min(0.0);
    let bottom = blur_pad + spread + offset_y.max(0.0);

    min_x = min_x.min(base_rect.min_x() - left);
    min_y = min_y.min(base_rect.min_y() - top);
    max_x = max_x.max(base_rect.max_x() + right);
    max_y = max_y.max(base_rect.max_y() + bottom);
    changed = true;
  }

  if changed {
    Some(Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y))
  } else {
    None
  }
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  let new_x = rect.x() + left;
  let new_y = rect.y() + top;
  let new_w = (rect.width() - left - right).max(0.0);
  let new_h = (rect.height() - top - bottom).max(0.0);
  Rect::from_xywh(new_x, new_y, new_w, new_h)
}

