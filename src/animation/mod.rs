//! Scroll-driven animation utilities.
//!
//! This module provides lightweight timeline evaluation for scroll and view
//! timelines along with keyframe sampling helpers. It is intentionally small
//! and self contained so it can be reused by layout/paint and tests without
//! wiring a full animation engine.

pub mod timing;
mod state_store;

pub use state_store::AnimationStateStore;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use smallvec::SmallVec;
use crate::css::types::{
  BoxShadow, Keyframe, KeyframeSelector, KeyframesRule, PropertyValue, RotateValue, ScaleValue,
  TextShadow, TranslateValue,
};
use crate::debug::runtime;
use crate::error::RenderStage;
use crate::geometry::{Point, Rect, Size};
use crate::paint::display_list::{Transform2D, Transform3D};
use crate::render_control::check_active_periodic;
use crate::scroll::ScrollState;
use crate::style::inline_axis_is_horizontal;
use crate::style::properties::{
  apply_declaration_with_base, parse_transition_timing_function, split_top_level_commas,
  ANIMATION_DURATION_AUTO_SENTINEL_MS,
};
use crate::style::types::AnimationComposition;
use crate::style::types::AnimationDirection;
use crate::style::types::AnimationFillMode;
use crate::style::types::AnimationIterationCount;
use crate::style::types::AnimationPlayState;
use crate::style::types::AnimationRange;
use crate::style::types::AnimationTimeline;
use crate::style::types::BackgroundPosition;
use crate::style::types::BackgroundPositionComponent;
use crate::style::types::BackgroundSize;
use crate::style::types::BackgroundSizeComponent;
use crate::style::types::BackgroundSizeKeyword;
use crate::style::types::BasicShape;
use crate::style::types::BorderCornerRadius;
use crate::style::types::BorderStyle;
use crate::style::types::ClipComponent;
use crate::style::types::ClipPath;
use crate::style::types::ClipRadii;
use crate::style::types::ClipRect;
use crate::style::types::Direction;
use crate::style::types::FillRule;
use crate::style::types::FilterColor;
use crate::style::types::FilterFunction;
use crate::style::types::OffsetAnchor;
use crate::style::types::OffsetRotate;
use crate::style::types::OutlineColor;
use crate::style::types::OutlineStyle;
use crate::style::types::Overflow;
use crate::style::types::RangeOffset;
use crate::style::types::ReferenceBox;
use crate::style::types::ScrollFunctionTimeline;
use crate::style::types::ScrollTimeline;
use crate::style::types::ScrollTimelineScroller;
use crate::style::types::ShapeRadius;
use crate::style::types::TimelineAxis;
use crate::style::types::TimelineOffset;
use crate::style::types::TimelineScopeProperty;
use crate::style::types::TransformBox;
use crate::style::types::TransformOrigin;
use crate::style::types::TransitionBehavior;
use crate::style::types::TransitionProperty;
use crate::style::types::TransitionTimingFunction;
use crate::style::types::ViewFunctionTimeline;
use crate::style::types::ViewTimeline;
use crate::style::types::ViewTimelineInset;
use crate::style::types::ViewTimelinePhase;
use crate::style::types::WritingMode;
use crate::style::values::{
  CalcLength, CustomPropertyTypedValue, CustomPropertyValue, Length, LengthUnit,
};
use crate::style::var_resolution::{resolve_var_for_property, VarResolutionResult};
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use rustc_hash::FxHashSet;
use std::mem::discriminant;
use std::sync::{Arc, OnceLock};

use crate::style::color::Rgba;
use crate::style::computed::Visibility;

mod transitions;
pub use transitions::TransitionState;

/// Resolved animated property value used by interpolation/apply steps.
#[derive(Debug, Clone, PartialEq)]
pub enum AnimatedValue {
  Opacity(f32),
  Visibility(Visibility),
  Color(Rgba),
  Length(Length),
  OffsetDistance(Length),
  OffsetAnchor { x: f32, y: f32 },
  OffsetRotate(OffsetRotate),
  OutlineColor(OutlineColor),
  OutlineStyle(OutlineStyle),
  Outline(OutlineColor, OutlineStyle, Length),
  BorderStyle([BorderStyle; 4]),
  Transform(Vec<crate::css::types::Transform>),
  TransformOrigin(TransformOrigin),
  Translate(TranslateValue),
  Rotate(RotateValue),
  Scale(ScaleValue),
  Filter(Vec<FilterFunction>),
  BackdropFilter(Vec<FilterFunction>),
  ClipPath(ClipPath),
  ClipRect(Option<ClipRect>),
  BackgroundPosition(Vec<BackgroundPosition>),
  BackgroundSize(Vec<BackgroundSize>),
  BoxShadow(Vec<BoxShadow>),
  TextShadow(Vec<TextShadow>),
  Border([Length; 4], [BorderStyle; 4], [Rgba; 4]),
  BorderColor([Rgba; 4]),
  BorderWidth([Length; 4]),
  BorderRadius([BorderCornerRadius; 4]),
  CustomProperty(Option<CustomPropertyValue>),
}

#[derive(Clone, Copy)]
struct AnimationResolveContext {
  viewport: Size,
  element_size: Size,
}

impl AnimationResolveContext {
  fn new(viewport: Size, element_size: Size) -> Self {
    Self {
      viewport,
      element_size,
    }
  }
}

static DEFAULT_PARENT_STYLE: OnceLock<ComputedStyle> = OnceLock::new();

fn default_parent_style() -> &'static ComputedStyle {
  DEFAULT_PARENT_STYLE.get_or_init(ComputedStyle::default)
}

fn recompute_var_dependent_properties_preserving_animated_color(
  style: &mut ComputedStyle,
  parent_styles: &ComputedStyle,
  viewport: Size,
  color_is_animated: bool,
) {
  // `recompute_var_dependent_properties` reapplies all cached var/currentColor-dependent declarations,
  // including potentially the element's own `color` declaration when it contained `var()`.
  //
  // When `color` is being animated by transitions/animations, we must not override the animated
  // `style.color` while recomputing other currentColor-dependent properties (e.g.
  // `border-top-color: currentColor`). Temporarily filter `color` out of the var-dependent
  // declaration set so recomputation uses the animated `style.color`.
  if !color_is_animated {
    style.recompute_var_dependent_properties(parent_styles, viewport);
    return;
  }

  let original = Arc::clone(&style.var_dependent_declarations);
  if !original.contains_key("color") {
    style.recompute_var_dependent_properties(parent_styles, viewport);
    return;
  }

  let mut filtered = (*original).clone();
  filtered.remove("color");
  style.var_dependent_declarations = Arc::new(filtered);
  style.recompute_var_dependent_properties(parent_styles, viewport);
  style.var_dependent_declarations = original;
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

fn lerp_color(a: Rgba, b: Rgba, t: f32) -> Rgba {
  let lerp_chan =
    |ca: u8, cb: u8| -> u8 { lerp(ca as f32, cb as f32, t).round().clamp(0.0, 255.0) as u8 };
  Rgba::new(
    lerp_chan(a.r, b.r),
    lerp_chan(a.g, b.g),
    lerp_chan(a.b, b.b),
    lerp(a.a, b.a, t),
  )
}

fn add_color(a: Rgba, b: Rgba) -> Rgba {
  let add_chan = |ca: u8, cb: u8| -> u8 { ca.saturating_add(cb) };
  Rgba::new(
    add_chan(a.r, b.r),
    add_chan(a.g, b.g),
    add_chan(a.b, b.b),
    (a.a + b.a).clamp(0.0, 1.0),
  )
}

fn clamp_color_channel_i128(value: i128) -> u8 {
  value.clamp(0, 255) as u8
}

fn length_percentage_components(
  len: &Length,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> (f32, f32) {
  // The computed value type for `<length-percentage>` can be expressed as a linear combination of
  // percentage + absolute length. Convert any non-percent units into px so interpolation can
  // produce a canonical computed value.
  let mut pct = 0.0f32;
  let mut px = 0.0f32;

  if let Some(calc) = len.calc.as_ref() {
    for term in calc.terms() {
      if term.unit == LengthUnit::Percent {
        pct += term.value;
        continue;
      }
      let term_len = Length::new(term.value, term.unit);
      px += resolve_length_px(&term_len, None, style, ctx);
    }
    return (pct, px);
  }

  if len.unit == LengthUnit::Percent {
    pct += len.value;
  } else {
    px += resolve_length_px(len, None, style, ctx);
  }
  (pct, px)
}

fn length_percentage_components_no_context(len: &Length) -> (f32, f32) {
  let mut pct = 0.0f32;
  let mut px = 0.0f32;

  if let Some(calc) = len.calc.as_ref() {
    for term in calc.terms() {
      if term.unit == LengthUnit::Percent {
        pct += term.value;
      } else {
        px += Length::new(term.value, term.unit).to_px();
      }
    }
    return (pct, px);
  }

  if len.unit == LengthUnit::Percent {
    pct += len.value;
  } else {
    px += len.to_px();
  }
  (pct, px)
}

fn build_length_from_components(px: f32, pct: f32) -> Option<Length> {
  if !px.is_finite() || !pct.is_finite() {
    return None;
  }

  let px = if px.abs() <= 1e-6 { 0.0 } else { px };
  let pct = if pct.abs() <= 1e-6 { 0.0 } else { pct };

  if pct == 0.0 {
    return Some(Length::px(px));
  }
  if px == 0.0 {
    return Some(Length::percent(pct));
  }

  let calc = CalcLength::single(LengthUnit::Px, px)
    .add_scaled(&CalcLength::single(LengthUnit::Percent, pct), 1.0)?;
  if let Some(term) = calc.single_term() {
    return Some(Length::new(term.value, term.unit));
  }
  Some(Length::calc(calc))
}

fn interpolate_custom_property(
  from: &CustomPropertyValue,
  to: &CustomPropertyValue,
  t: f32,
  from_style: &ComputedStyle,
  to_style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<CustomPropertyValue> {
  let from_typed = from.typed.as_ref()?;
  let to_typed = to.typed.as_ref()?;

  let typed = match (from_typed, to_typed) {
    (CustomPropertyTypedValue::Number(a), CustomPropertyTypedValue::Number(b)) => {
      CustomPropertyTypedValue::Number(lerp(*a, *b, t))
    }
    (CustomPropertyTypedValue::Percentage(a), CustomPropertyTypedValue::Percentage(b)) => {
      CustomPropertyTypedValue::Percentage(lerp(*a, *b, t))
    }
    (CustomPropertyTypedValue::Angle(a), CustomPropertyTypedValue::Angle(b)) => {
      CustomPropertyTypedValue::Angle(lerp(*a, *b, t))
    }
    (CustomPropertyTypedValue::Color(a), CustomPropertyTypedValue::Color(b)) => {
      let from_rgba = a.to_rgba_with_scheme(from_style.color, from_style.used_dark_color_scheme);
      let to_rgba = b.to_rgba_with_scheme(to_style.color, to_style.used_dark_color_scheme);
      let rgba = lerp_color(from_rgba, to_rgba, t);
      CustomPropertyTypedValue::Color(crate::style::color::Color::Rgba(rgba))
    }
    (CustomPropertyTypedValue::Length(a), CustomPropertyTypedValue::Length(b)) => {
      let (a_pct, a_px) = length_percentage_components(a, from_style, ctx);
      let (b_pct, b_px) = length_percentage_components(b, to_style, ctx);
      let pct = lerp(a_pct, b_pct, t);
      let px = lerp(a_px, b_px, t);
      let len = build_length_from_components(px, pct)?;
      CustomPropertyTypedValue::Length(len)
    }
    _ => return None,
  };

  Some(CustomPropertyValue::new(typed.to_css(), Some(typed)))
}

fn resolve_length_px(
  len: &Length,
  percent_base: Option<f32>,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> f32 {
  len
    .resolve_with_context(
      percent_base,
      ctx.viewport.width,
      ctx.viewport.height,
      style.font_size,
      style.root_font_size,
    )
    .unwrap_or_else(|| len.to_px())
}

fn transform_reference_size(style: &ComputedStyle, ctx: &AnimationResolveContext) -> (f32, f32) {
  let border_box_width = if ctx.element_size.width.is_finite() {
    ctx.element_size.width.max(0.0)
  } else {
    0.0
  };
  let border_box_height = if ctx.element_size.height.is_finite() {
    ctx.element_size.height.max(0.0)
  } else {
    0.0
  };

  match style.transform_box {
    TransformBox::ContentBox => {
      // Mirror `paint::transform_resolver::background_rects`: padding percentages are resolved
      // against the border box width per CSS2.1.
      let base = Some(border_box_width);

      let border_left = resolve_length_px(&style.used_border_left_width(), base, style, ctx);
      let border_right = resolve_length_px(&style.used_border_right_width(), base, style, ctx);
      let border_top = resolve_length_px(&style.used_border_top_width(), base, style, ctx);
      let border_bottom = resolve_length_px(&style.used_border_bottom_width(), base, style, ctx);

      let padding_left = resolve_length_px(&style.padding_left, base, style, ctx);
      let padding_right = resolve_length_px(&style.padding_right, base, style, ctx);
      let padding_top = resolve_length_px(&style.padding_top, base, style, ctx);
      let padding_bottom = resolve_length_px(&style.padding_bottom, base, style, ctx);

      let content_width =
        (border_box_width - border_left - border_right - padding_left - padding_right).max(0.0);
      let content_height =
        (border_box_height - border_top - border_bottom - padding_top - padding_bottom).max(0.0);

      (content_width, content_height)
    }
    TransformBox::BorderBox
    | TransformBox::FillBox
    | TransformBox::StrokeBox
    | TransformBox::ViewBox => (border_box_width, border_box_height),
  }
}

#[derive(Debug, Clone)]
struct ResolvedShadow {
  offset_x: f32,
  offset_y: f32,
  blur: f32,
  spread: f32,
  color: Rgba,
}

#[derive(Debug, Clone)]
struct ResolvedPositionComponent {
  alignment: f32,
  offset: f32,
}

#[derive(Debug, Clone)]
struct ResolvedBackgroundPosition {
  x: ResolvedPositionComponent,
  y: ResolvedPositionComponent,
}

#[derive(Debug, Clone)]
enum ResolvedSizeComponent {
  Auto,
  Length(f32),
}

#[derive(Debug, Clone)]
enum ResolvedBackgroundSize {
  Keyword(BackgroundSizeKeyword),
  Explicit(ResolvedSizeComponent, ResolvedSizeComponent),
}

#[derive(Debug, Clone)]
enum ResolvedClipPath {
  None,
  Box(ReferenceBox),
  Inset {
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
    radii: Option<[BorderCornerRadius; 4]>,
    reference: Option<ReferenceBox>,
  },
  Circle {
    radius: f32,
    position: ResolvedBackgroundPosition,
    reference: Option<ReferenceBox>,
  },
  Ellipse {
    radius_x: f32,
    radius_y: f32,
    position: ResolvedBackgroundPosition,
    reference: Option<ReferenceBox>,
  },
  Polygon {
    fill: FillRule,
    points: Vec<(f32, f32)>,
    reference: Option<ReferenceBox>,
  },
  Path {
    fill: FillRule,
    reference: Option<ReferenceBox>,
    data: Arc<str>,
  },
}

#[derive(Debug, Clone)]
enum ResolvedFilter {
  Blur(f32),
  Brightness(f32),
  Contrast(f32),
  Grayscale(f32),
  Sepia(f32),
  Saturate(f32),
  HueRotate(f32),
  Invert(f32),
  Opacity(f32),
  DropShadow(ResolvedShadow),
  Url(String),
}

fn resolve_filter_color(color: &FilterColor, current_color: Rgba) -> Rgba {
  match color {
    FilterColor::CurrentColor => current_color,
    FilterColor::Color(c) => *c,
  }
}

fn resolve_filter_list(
  filters: &[FilterFunction],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<ResolvedFilter> {
  filters
    .iter()
    .map(|f| match f {
      FilterFunction::Blur(len) => ResolvedFilter::Blur(resolve_length_px(len, None, style, ctx)),
      FilterFunction::Brightness(v) => ResolvedFilter::Brightness(*v),
      FilterFunction::Contrast(v) => ResolvedFilter::Contrast(*v),
      FilterFunction::Grayscale(v) => ResolvedFilter::Grayscale(*v),
      FilterFunction::Sepia(v) => ResolvedFilter::Sepia(*v),
      FilterFunction::Saturate(v) => ResolvedFilter::Saturate(*v),
      FilterFunction::HueRotate(v) => ResolvedFilter::HueRotate(*v),
      FilterFunction::Invert(v) => ResolvedFilter::Invert(*v),
      FilterFunction::Opacity(v) => ResolvedFilter::Opacity(*v),
      FilterFunction::DropShadow(shadow) => ResolvedFilter::DropShadow(ResolvedShadow {
        offset_x: resolve_length_px(&shadow.offset_x, None, style, ctx),
        offset_y: resolve_length_px(&shadow.offset_y, None, style, ctx),
        blur: resolve_length_px(&shadow.blur_radius, None, style, ctx),
        spread: resolve_length_px(&shadow.spread, None, style, ctx),
        color: resolve_filter_color(&shadow.color, style.color),
      }),
      FilterFunction::Url(u) => ResolvedFilter::Url(u.clone()),
    })
    .collect()
}

fn interpolate_filters(
  a: &[ResolvedFilter],
  b: &[ResolvedFilter],
  t: f32,
) -> Option<Vec<ResolvedFilter>> {
  if t <= f32::EPSILON {
    return Some(a.to_vec());
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(b.to_vec());
  }

  let identity = |filter: &ResolvedFilter| -> Option<ResolvedFilter> {
    Some(match filter {
      ResolvedFilter::Blur(_) => ResolvedFilter::Blur(0.0),
      ResolvedFilter::Brightness(_) => ResolvedFilter::Brightness(1.0),
      ResolvedFilter::Contrast(_) => ResolvedFilter::Contrast(1.0),
      ResolvedFilter::Grayscale(_) => ResolvedFilter::Grayscale(0.0),
      ResolvedFilter::Sepia(_) => ResolvedFilter::Sepia(0.0),
      ResolvedFilter::Saturate(_) => ResolvedFilter::Saturate(1.0),
      ResolvedFilter::HueRotate(_) => ResolvedFilter::HueRotate(0.0),
      ResolvedFilter::Invert(_) => ResolvedFilter::Invert(0.0),
      ResolvedFilter::Opacity(_) => ResolvedFilter::Opacity(1.0),
      ResolvedFilter::DropShadow(shadow) => ResolvedFilter::DropShadow(ResolvedShadow {
        offset_x: 0.0,
        offset_y: 0.0,
        blur: 0.0,
        spread: 0.0,
        color: Rgba::new(shadow.color.r, shadow.color.g, shadow.color.b, 0.0),
      }),
      // `url()` filters are discrete and don't have a meaningful identity value. For list
      // interpolation we treat a missing `url()` entry as the existing value so animations like
      // `url(#f)` -> `none` keep the filter applied until the end keyframe.
      ResolvedFilter::Url(url) => ResolvedFilter::Url(url.clone()),
    })
  };

  let interpolate_pair = |fa: &ResolvedFilter, fb: &ResolvedFilter| -> Option<ResolvedFilter> {
    match (fa, fb) {
      (ResolvedFilter::Blur(la), ResolvedFilter::Blur(lb)) => {
        Some(ResolvedFilter::Blur(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Brightness(la), ResolvedFilter::Brightness(lb)) => {
        Some(ResolvedFilter::Brightness(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Contrast(la), ResolvedFilter::Contrast(lb)) => {
        Some(ResolvedFilter::Contrast(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Grayscale(la), ResolvedFilter::Grayscale(lb)) => {
        Some(ResolvedFilter::Grayscale(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Sepia(la), ResolvedFilter::Sepia(lb)) => {
        Some(ResolvedFilter::Sepia(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Saturate(la), ResolvedFilter::Saturate(lb)) => {
        Some(ResolvedFilter::Saturate(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::HueRotate(la), ResolvedFilter::HueRotate(lb)) => {
        Some(ResolvedFilter::HueRotate(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Invert(la), ResolvedFilter::Invert(lb)) => {
        Some(ResolvedFilter::Invert(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::Opacity(la), ResolvedFilter::Opacity(lb)) => {
        Some(ResolvedFilter::Opacity(lerp(*la, *lb, t)))
      }
      (ResolvedFilter::DropShadow(sa), ResolvedFilter::DropShadow(sb)) => {
        Some(ResolvedFilter::DropShadow(ResolvedShadow {
          offset_x: lerp(sa.offset_x, sb.offset_x, t),
          offset_y: lerp(sa.offset_y, sb.offset_y, t),
          blur: lerp(sa.blur, sb.blur, t),
          spread: lerp(sa.spread, sb.spread, t),
          color: lerp_color(sa.color, sb.color, t),
        }))
      }
      (ResolvedFilter::Url(a), ResolvedFilter::Url(b)) if a == b => {
        Some(ResolvedFilter::Url(a.clone()))
      }
      _ => None,
    }
  };

  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    let next = match (a.get(idx), b.get(idx)) {
      (Some(fa), Some(fb)) => interpolate_pair(fa, fb)?,
      (Some(fa), None) => {
        let fb = identity(fa)?;
        interpolate_pair(fa, &fb)?
      }
      (None, Some(fb)) => {
        let fa = identity(fb)?;
        interpolate_pair(&fa, fb)?
      }
      (None, None) => continue,
    };
    out.push(next);
  }

  Some(out)
}

fn resolved_filters_to_functions(filters: &[ResolvedFilter]) -> Vec<FilterFunction> {
  filters
    .iter()
    .map(|f| match f {
      ResolvedFilter::Blur(v) => FilterFunction::Blur(Length::px(*v)),
      ResolvedFilter::Brightness(v) => FilterFunction::Brightness(*v),
      ResolvedFilter::Contrast(v) => FilterFunction::Contrast(*v),
      ResolvedFilter::Grayscale(v) => FilterFunction::Grayscale(*v),
      ResolvedFilter::Sepia(v) => FilterFunction::Sepia(*v),
      ResolvedFilter::Saturate(v) => FilterFunction::Saturate(*v),
      ResolvedFilter::HueRotate(v) => FilterFunction::HueRotate(*v),
      ResolvedFilter::Invert(v) => FilterFunction::Invert(*v),
      ResolvedFilter::Opacity(v) => FilterFunction::Opacity(*v),
      ResolvedFilter::DropShadow(s) => {
        FilterFunction::DropShadow(Box::new(crate::style::types::FilterShadow {
          offset_x: Length::px(s.offset_x),
          offset_y: Length::px(s.offset_y),
          blur_radius: Length::px(s.blur),
          spread: Length::px(s.spread),
          color: FilterColor::Color(s.color),
        }))
      }
      ResolvedFilter::Url(u) => FilterFunction::Url(u.clone()),
    })
    .collect()
}

fn resolved_filters_from_functions(filters: &[FilterFunction]) -> Vec<ResolvedFilter> {
  filters
    .iter()
    .map(|f| match f {
      FilterFunction::Blur(len) => ResolvedFilter::Blur(len.to_px()),
      FilterFunction::Brightness(v) => ResolvedFilter::Brightness(*v),
      FilterFunction::Contrast(v) => ResolvedFilter::Contrast(*v),
      FilterFunction::Grayscale(v) => ResolvedFilter::Grayscale(*v),
      FilterFunction::Sepia(v) => ResolvedFilter::Sepia(*v),
      FilterFunction::Saturate(v) => ResolvedFilter::Saturate(*v),
      FilterFunction::HueRotate(v) => ResolvedFilter::HueRotate(*v),
      FilterFunction::Invert(v) => ResolvedFilter::Invert(*v),
      FilterFunction::Opacity(v) => ResolvedFilter::Opacity(*v),
      FilterFunction::DropShadow(shadow) => ResolvedFilter::DropShadow(ResolvedShadow {
        offset_x: shadow.offset_x.to_px(),
        offset_y: shadow.offset_y.to_px(),
        blur: shadow.blur_radius.to_px(),
        spread: shadow.spread.to_px(),
        color: resolve_filter_color(&shadow.color, Rgba::BLACK),
      }),
      FilterFunction::Url(u) => ResolvedFilter::Url(u.clone()),
    })
    .collect()
}

fn filter_identity_like(filter: &ResolvedFilter) -> Option<ResolvedFilter> {
  Some(match filter {
    ResolvedFilter::Blur(_) => ResolvedFilter::Blur(0.0),
    ResolvedFilter::DropShadow(shadow) => ResolvedFilter::DropShadow(ResolvedShadow {
      offset_x: 0.0,
      offset_y: 0.0,
      blur: 0.0,
      spread: 0.0,
      color: Rgba::new(shadow.color.r, shadow.color.g, shadow.color.b, 0.0),
    }),
    _ => return None,
  })
}

fn add_resolved_filter_list(a: &[ResolvedFilter], b: &[ResolvedFilter]) -> Option<Vec<ResolvedFilter>> {
  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    match (a.get(idx), b.get(idx)) {
      (Some(fa), Some(fb)) => {
        let next = match (fa, fb) {
          (ResolvedFilter::Blur(a), ResolvedFilter::Blur(b)) => {
            if !a.is_finite() || !b.is_finite() {
              return None;
            }
            ResolvedFilter::Blur((a + b).max(0.0))
          }
          (ResolvedFilter::DropShadow(a), ResolvedFilter::DropShadow(b)) => {
            if !a.offset_x.is_finite()
              || !a.offset_y.is_finite()
              || !a.blur.is_finite()
              || !a.spread.is_finite()
              || !b.offset_x.is_finite()
              || !b.offset_y.is_finite()
              || !b.blur.is_finite()
              || !b.spread.is_finite()
              || !a.color.a.is_finite()
              || !b.color.a.is_finite()
            {
              return None;
            }
            ResolvedFilter::DropShadow(ResolvedShadow {
              offset_x: a.offset_x + b.offset_x,
              offset_y: a.offset_y + b.offset_y,
              blur: (a.blur + b.blur).max(0.0),
              spread: a.spread + b.spread,
              color: add_color(a.color, b.color),
            })
          }
          _ => return None,
        };
        out.push(next);
      }
      (Some(fa), None) => out.push(fa.clone()),
      (None, Some(fb)) => out.push(fb.clone()),
      (None, None) => {}
    }
  }
  Some(out)
}

fn add_filter_list(a: &[FilterFunction], b: &[FilterFunction]) -> Option<Vec<FilterFunction>> {
  let ra = resolved_filters_from_functions(a);
  let rb = resolved_filters_from_functions(b);
  let combined = add_resolved_filter_list(&ra, &rb)?;
  Some(resolved_filters_to_functions(&combined))
}

fn accumulate_resolved_filter_list(
  current: &[ResolvedFilter],
  start: &[ResolvedFilter],
  end: &[ResolvedFilter],
  iteration: u64,
) -> Option<Vec<ResolvedFilter>> {
  let max_len = current.len().max(start.len()).max(end.len());
  let iter = iteration as f32;
  let iter_i = iteration as i128;
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    let cur_entry = if let Some(f) = current.get(idx) {
      f.clone()
    } else if let Some(f) = start.get(idx) {
      filter_identity_like(f)?
    } else if let Some(f) = end.get(idx) {
      filter_identity_like(f)?
    } else {
      continue;
    };
    let start_entry = if let Some(f) = start.get(idx) {
      f.clone()
    } else if let Some(f) = end.get(idx) {
      filter_identity_like(f)?
    } else {
      continue;
    };
    let end_entry = if let Some(f) = end.get(idx) {
      f.clone()
    } else if let Some(f) = start.get(idx) {
      filter_identity_like(f)?
    } else {
      continue;
    };

    let next = match (&cur_entry, &start_entry, &end_entry) {
      (ResolvedFilter::Blur(cur), ResolvedFilter::Blur(start), ResolvedFilter::Blur(end)) => {
        if !cur.is_finite() || !start.is_finite() || !end.is_finite() {
          return None;
        }
        let delta = end - start;
        ResolvedFilter::Blur((cur + iter * delta).max(0.0))
      }
      (
        ResolvedFilter::DropShadow(cur),
        ResolvedFilter::DropShadow(start),
        ResolvedFilter::DropShadow(end),
      ) => {
        if !cur.offset_x.is_finite()
          || !cur.offset_y.is_finite()
          || !cur.blur.is_finite()
          || !cur.spread.is_finite()
          || !start.offset_x.is_finite()
          || !start.offset_y.is_finite()
          || !start.blur.is_finite()
          || !start.spread.is_finite()
          || !end.offset_x.is_finite()
          || !end.offset_y.is_finite()
          || !end.blur.is_finite()
          || !end.spread.is_finite()
          || !cur.color.a.is_finite()
          || !start.color.a.is_finite()
          || !end.color.a.is_finite()
        {
          return None;
        }

        let r = clamp_color_channel_i128(
          cur.color.r as i128 + iter_i * (end.color.r as i128 - start.color.r as i128),
        );
        let g = clamp_color_channel_i128(
          cur.color.g as i128 + iter_i * (end.color.g as i128 - start.color.g as i128),
        );
        let b = clamp_color_channel_i128(
          cur.color.b as i128 + iter_i * (end.color.b as i128 - start.color.b as i128),
        );
        let alpha = (cur.color.a + iter * (end.color.a - start.color.a)).clamp(0.0, 1.0);
        if !alpha.is_finite() {
          return None;
        }

        ResolvedFilter::DropShadow(ResolvedShadow {
          offset_x: cur.offset_x + iter * (end.offset_x - start.offset_x),
          offset_y: cur.offset_y + iter * (end.offset_y - start.offset_y),
          blur: (cur.blur + iter * (end.blur - start.blur)).max(0.0),
          spread: cur.spread + iter * (end.spread - start.spread),
          color: Rgba::new(r, g, b, alpha),
        })
      }
      _ => return None,
    };
    out.push(next);
  }
  Some(out)
}

fn resolve_background_position_component(
  comp: &BackgroundPositionComponent,
  axis_base: f32,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> ResolvedPositionComponent {
  ResolvedPositionComponent {
    alignment: comp.alignment,
    offset: resolve_length_px(&comp.offset, Some(axis_base), style, ctx),
  }
}

fn resolve_background_positions(
  list: &[BackgroundPosition],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<ResolvedBackgroundPosition> {
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
  list
    .iter()
    .map(|pos| match pos {
      BackgroundPosition::Position { x, y } => ResolvedBackgroundPosition {
        x: resolve_background_position_component(x, width, style, ctx),
        y: resolve_background_position_component(y, height, style, ctx),
      },
    })
    .collect()
}

fn interpolate_background_positions(
  a: &[ResolvedBackgroundPosition],
  b: &[ResolvedBackgroundPosition],
  t: f32,
) -> Option<Vec<ResolvedBackgroundPosition>> {
  if a.len() != b.len() {
    return None;
  }
  let mut out = Vec::with_capacity(a.len());
  for (pa, pb) in a.iter().zip(b.iter()) {
    out.push(ResolvedBackgroundPosition {
      x: ResolvedPositionComponent {
        alignment: lerp(pa.x.alignment, pb.x.alignment, t),
        offset: lerp(pa.x.offset, pb.x.offset, t),
      },
      y: ResolvedPositionComponent {
        alignment: lerp(pa.y.alignment, pb.y.alignment, t),
        offset: lerp(pa.y.offset, pb.y.offset, t),
      },
    });
  }
  Some(out)
}

fn resolved_positions_to_background(
  list: &[ResolvedBackgroundPosition],
) -> Vec<BackgroundPosition> {
  list
    .iter()
    .map(|p| BackgroundPosition::Position {
      x: BackgroundPositionComponent {
        alignment: p.x.alignment,
        offset: Length::px(p.x.offset),
      },
      y: BackgroundPositionComponent {
        alignment: p.y.alignment,
        offset: Length::px(p.y.offset),
      },
    })
    .collect()
}

fn background_positions_to_resolved(
  list: &[BackgroundPosition],
) -> Vec<ResolvedBackgroundPosition> {
  list
    .iter()
    .map(|p| match p {
      BackgroundPosition::Position { x, y } => ResolvedBackgroundPosition {
        x: ResolvedPositionComponent {
          alignment: x.alignment,
          offset: x.offset.to_px(),
        },
        y: ResolvedPositionComponent {
          alignment: y.alignment,
          offset: y.offset.to_px(),
        },
      },
    })
    .collect()
}

fn resolve_background_sizes(
  list: &[BackgroundSize],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<ResolvedBackgroundSize> {
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
  list
    .iter()
    .map(|size| match size {
      BackgroundSize::Keyword(k) => ResolvedBackgroundSize::Keyword(*k),
      BackgroundSize::Explicit(x, y) => ResolvedBackgroundSize::Explicit(
        match x {
          BackgroundSizeComponent::Auto => ResolvedSizeComponent::Auto,
          BackgroundSizeComponent::Length(l) => {
            ResolvedSizeComponent::Length(resolve_length_px(l, Some(width), style, ctx))
          }
        },
        match y {
          BackgroundSizeComponent::Auto => ResolvedSizeComponent::Auto,
          BackgroundSizeComponent::Length(l) => {
            ResolvedSizeComponent::Length(resolve_length_px(l, Some(height), style, ctx))
          }
        },
      ),
    })
    .collect()
}

fn interpolate_background_sizes(
  a: &[ResolvedBackgroundSize],
  b: &[ResolvedBackgroundSize],
  t: f32,
) -> Option<Vec<ResolvedBackgroundSize>> {
  if a.len() != b.len() {
    return None;
  }

  let mut out = Vec::with_capacity(a.len());
  for (sa, sb) in a.iter().zip(b.iter()) {
    let next = match (sa, sb) {
      (ResolvedBackgroundSize::Keyword(ka), ResolvedBackgroundSize::Keyword(kb)) => {
        if ka == kb {
          ResolvedBackgroundSize::Keyword(*ka)
        } else {
          return None;
        }
      }
      (ResolvedBackgroundSize::Explicit(xa, ya), ResolvedBackgroundSize::Explicit(xb, yb)) => {
        let interp_component = |ca: &ResolvedSizeComponent,
                                cb: &ResolvedSizeComponent|
         -> Option<ResolvedSizeComponent> {
          match (ca, cb) {
            (ResolvedSizeComponent::Auto, ResolvedSizeComponent::Auto) => {
              Some(ResolvedSizeComponent::Auto)
            }
            (ResolvedSizeComponent::Length(la), ResolvedSizeComponent::Length(lb)) => {
              Some(ResolvedSizeComponent::Length(lerp(*la, *lb, t)))
            }
            _ => None,
          }
        };
        let x = interp_component(xa, xb)?;
        let y = interp_component(ya, yb)?;
        ResolvedBackgroundSize::Explicit(x, y)
      }
      _ => return None,
    };
    out.push(next);
  }
  Some(out)
}

fn resolved_sizes_to_background(list: &[ResolvedBackgroundSize]) -> Vec<BackgroundSize> {
  list
    .iter()
    .map(|size| match size {
      ResolvedBackgroundSize::Keyword(k) => BackgroundSize::Keyword(*k),
      ResolvedBackgroundSize::Explicit(x, y) => {
        let to_comp = |c: &ResolvedSizeComponent| match c {
          ResolvedSizeComponent::Auto => BackgroundSizeComponent::Auto,
          ResolvedSizeComponent::Length(l) => BackgroundSizeComponent::Length(Length::px(*l)),
        };
        BackgroundSize::Explicit(to_comp(x), to_comp(y))
      }
    })
    .collect()
}

fn background_sizes_to_resolved(list: &[BackgroundSize]) -> Vec<ResolvedBackgroundSize> {
  list
    .iter()
    .map(|size| match size {
      BackgroundSize::Keyword(k) => ResolvedBackgroundSize::Keyword(*k),
      BackgroundSize::Explicit(x, y) => ResolvedBackgroundSize::Explicit(
        match x {
          BackgroundSizeComponent::Auto => ResolvedSizeComponent::Auto,
          BackgroundSizeComponent::Length(l) => ResolvedSizeComponent::Length(l.to_px()),
        },
        match y {
          BackgroundSizeComponent::Auto => ResolvedSizeComponent::Auto,
          BackgroundSizeComponent::Length(l) => ResolvedSizeComponent::Length(l.to_px()),
        },
      ),
    })
    .collect()
}

fn resolve_corner_radius(
  radius: &BorderCornerRadius,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> BorderCornerRadius {
  BorderCornerRadius {
    x: Length::px(resolve_length_px(
      &radius.x,
      Some(ctx.element_size.width),
      style,
      ctx,
    )),
    y: Length::px(resolve_length_px(
      &radius.y,
      Some(ctx.element_size.height),
      style,
      ctx,
    )),
  }
}

fn resolve_border_radii(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> [BorderCornerRadius; 4] {
  [
    resolve_corner_radius(&style.border_top_left_radius, style, ctx),
    resolve_corner_radius(&style.border_top_right_radius, style, ctx),
    resolve_corner_radius(&style.border_bottom_right_radius, style, ctx),
    resolve_corner_radius(&style.border_bottom_left_radius, style, ctx),
  ]
}

fn interpolate_radii(
  a: [BorderCornerRadius; 4],
  b: [BorderCornerRadius; 4],
  t: f32,
) -> [BorderCornerRadius; 4] {
  let mut out = [BorderCornerRadius::default(); 4];
  for i in 0..4 {
    out[i] = BorderCornerRadius {
      x: Length::px(lerp(a[i].x.to_px(), b[i].x.to_px(), t)),
      y: Length::px(lerp(a[i].y.to_px(), b[i].y.to_px(), t)),
    };
  }
  out
}

fn resolve_circle_radius(
  radius: &ShapeRadius,
  center_x: f32,
  center_y: f32,
  width: f32,
  height: f32,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<f32> {
  let resolved = match radius {
    ShapeRadius::Length(l) => resolve_length_px(l, Some(width.min(height)), style, ctx),
    ShapeRadius::ClosestSide => {
      let dx = center_x.abs().min((width - center_x).abs());
      let dy = center_y.abs().min((height - center_y).abs());
      dx.min(dy)
    }
    ShapeRadius::FarthestSide => {
      let dx = center_x.abs().max((width - center_x).abs());
      let dy = center_y.abs().max((height - center_y).abs());
      dx.max(dy)
    }
  };
  resolved.is_finite().then_some(resolved)
}

fn resolve_ellipse_radius(
  radius: &ShapeRadius,
  horizontal: bool,
  center_x: f32,
  center_y: f32,
  width: f32,
  height: f32,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<f32> {
  let base = if horizontal { width } else { height };
  let (center, axis_size) = if horizontal {
    (center_x, width)
  } else {
    (center_y, height)
  };
  let resolved = match radius {
    ShapeRadius::Length(l) => resolve_length_px(l, Some(base), style, ctx),
    ShapeRadius::ClosestSide => center.abs().min((axis_size - center).abs()),
    ShapeRadius::FarthestSide => center.abs().max((axis_size - center).abs()),
  };
  resolved.is_finite().then_some(resolved)
}

#[derive(Debug, Clone, Copy)]
struct ClipPathReferenceBoxSizes {
  border_box: Size,
  padding_box: Size,
  content_box: Size,
  margin_box: Size,
}

impl ClipPathReferenceBoxSizes {
  fn select(&self, reference: ReferenceBox) -> Size {
    match reference {
      ReferenceBox::BorderBox => self.border_box,
      ReferenceBox::PaddingBox => self.padding_box,
      ReferenceBox::ContentBox => self.content_box,
      ReferenceBox::MarginBox => self.margin_box,
      // SVG reference boxes need geometry bounds that aren't available to the animation sampler yet.
      // Fall back to the border box to keep transition output deterministic.
      ReferenceBox::FillBox | ReferenceBox::StrokeBox | ReferenceBox::ViewBox => self.border_box,
    }
  }
}

fn clip_path_reference_box_sizes(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> ClipPathReferenceBoxSizes {
  let border_box = ctx.element_size;

  let used_border_width = |width: &Length, line_style: BorderStyle| -> f32 {
    if matches!(line_style, BorderStyle::None | BorderStyle::Hidden) {
      0.0
    } else {
      resolve_length_px(width, None, style, ctx)
    }
  };

  let border_top = used_border_width(&style.border_top_width, style.border_top_style);
  let border_right = used_border_width(&style.border_right_width, style.border_right_style);
  let border_bottom = used_border_width(&style.border_bottom_width, style.border_bottom_style);
  let border_left = used_border_width(&style.border_left_width, style.border_left_style);

  let padding_percent_base = border_box.width;
  let padding_top = resolve_length_px(&style.padding_top, Some(padding_percent_base), style, ctx);
  let padding_right =
    resolve_length_px(&style.padding_right, Some(padding_percent_base), style, ctx);
  let padding_bottom =
    resolve_length_px(&style.padding_bottom, Some(padding_percent_base), style, ctx);
  let padding_left =
    resolve_length_px(&style.padding_left, Some(padding_percent_base), style, ctx);

  let margin_percent_base = border_box.width;
  let resolve_margin = |value: &Option<Length>| -> f32 {
    value
      .as_ref()
      .map(|len| resolve_length_px(len, Some(margin_percent_base), style, ctx))
      .unwrap_or(0.0)
  };
  let margin_top = resolve_margin(&style.margin_top);
  let margin_right = resolve_margin(&style.margin_right);
  let margin_bottom = resolve_margin(&style.margin_bottom);
  let margin_left = resolve_margin(&style.margin_left);

  let padding_box = Size::new(
    (border_box.width - border_left - border_right).max(0.0),
    (border_box.height - border_top - border_bottom).max(0.0),
  );

  let content_box = Size::new(
    (padding_box.width - padding_left - padding_right).max(0.0),
    (padding_box.height - padding_top - padding_bottom).max(0.0),
  );

  let margin_box = Size::new(
    (border_box.width + margin_left + margin_right).max(0.0),
    (border_box.height + margin_top + margin_bottom).max(0.0),
  );

  ClipPathReferenceBoxSizes {
    border_box,
    padding_box,
    content_box,
    margin_box,
  }
}

fn resolve_background_positions_for_size(
  list: &[BackgroundPosition],
  size: Size,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<ResolvedBackgroundPosition> {
  list
    .iter()
    .map(|pos| match pos {
      BackgroundPosition::Position { x, y } => ResolvedBackgroundPosition {
        x: resolve_background_position_component(x, size.width, style, ctx),
        y: resolve_background_position_component(y, size.height, style, ctx),
      },
    })
    .collect()
}

fn resolve_clip_path(
  path: &ClipPath,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<ResolvedClipPath> {
  match path {
    ClipPath::None => Some(ResolvedClipPath::None),
    ClipPath::Box(b) => Some(ResolvedClipPath::Box(*b)),
    ClipPath::BasicShape(shape, reference_override) => {
      let boxes = clip_path_reference_box_sizes(style, ctx);
      let canonical_reference = reference_override.unwrap_or(ReferenceBox::BorderBox);
      let reference_size = boxes.select(canonical_reference);
      // Canonicalize the default reference so `circle(...)` and `circle(...) border-box` interpolate.
      let reference = Some(canonical_reference);

      match shape.as_ref() {
        BasicShape::Inset {
          top,
          right,
          bottom,
          left,
          border_radius,
        } => {
          let width = reference_size.width;
          let height = reference_size.height;
          let radii = border_radius.as_ref().and_then(|r| {
            Some([
              BorderCornerRadius {
                x: Length::px(resolve_length_px(&r.top_left.x, Some(width), style, ctx)),
                y: Length::px(resolve_length_px(&r.top_left.y, Some(height), style, ctx)),
              },
              BorderCornerRadius {
                x: Length::px(resolve_length_px(&r.top_right.x, Some(width), style, ctx)),
                y: Length::px(resolve_length_px(&r.top_right.y, Some(height), style, ctx)),
              },
              BorderCornerRadius {
                x: Length::px(resolve_length_px(&r.bottom_right.x, Some(width), style, ctx)),
                y: Length::px(resolve_length_px(&r.bottom_right.y, Some(height), style, ctx)),
              },
              BorderCornerRadius {
                x: Length::px(resolve_length_px(&r.bottom_left.x, Some(width), style, ctx)),
                y: Length::px(resolve_length_px(&r.bottom_left.y, Some(height), style, ctx)),
              },
            ])
          });
          Some(ResolvedClipPath::Inset {
            top: resolve_length_px(top, Some(height), style, ctx),
            right: resolve_length_px(right, Some(width), style, ctx),
            bottom: resolve_length_px(bottom, Some(height), style, ctx),
            left: resolve_length_px(left, Some(width), style, ctx),
            radii,
            reference,
          })
        }
        BasicShape::Circle { radius, position } => {
          let width = reference_size.width;
          let height = reference_size.height;
          let resolved_pos = resolve_background_positions_for_size(&[*position], reference_size, style, ctx)
            .into_iter()
            .next()?;
          let cx = resolved_pos.x.alignment * width + resolved_pos.x.offset;
          let cy = resolved_pos.y.alignment * height + resolved_pos.y.offset;
          if !(cx.is_finite() && cy.is_finite()) {
            return None;
          }
          let radius_px = resolve_circle_radius(radius, cx, cy, width, height, style, ctx)?;
          Some(ResolvedClipPath::Circle {
            radius: radius_px,
            position: resolved_pos,
            reference,
          })
        }
        BasicShape::Ellipse {
          radius_x,
          radius_y,
          position,
        } => {
          let width = reference_size.width;
          let height = reference_size.height;
          let resolved_pos = resolve_background_positions_for_size(&[*position], reference_size, style, ctx)
            .into_iter()
            .next()?;
          let cx = resolved_pos.x.alignment * width + resolved_pos.x.offset;
          let cy = resolved_pos.y.alignment * height + resolved_pos.y.offset;
          if !(cx.is_finite() && cy.is_finite()) {
            return None;
          }
          let rx = resolve_ellipse_radius(radius_x, true, cx, cy, width, height, style, ctx)?;
          let ry = resolve_ellipse_radius(radius_y, false, cx, cy, width, height, style, ctx)?;
          Some(ResolvedClipPath::Ellipse {
            radius_x: rx,
            radius_y: ry,
            position: resolved_pos,
            reference,
          })
        }
        BasicShape::Polygon { fill, points } => {
          let width = reference_size.width;
          let height = reference_size.height;
          let resolved_points = points
            .iter()
            .map(|(x, y)| {
              (
                resolve_length_px(x, Some(width), style, ctx),
                resolve_length_px(y, Some(height), style, ctx),
              )
            })
            .collect();
          Some(ResolvedClipPath::Polygon {
            fill: *fill,
            points: resolved_points,
            reference,
          })
        }
        BasicShape::Path { fill, data } => Some(ResolvedClipPath::Path {
          fill: *fill,
          reference,
          data: Arc::clone(data),
        }),
      }
    }
  }
}

const CLIP_PATH_PATH_DEADLINE_STRIDE: usize = 256;

#[derive(Debug, Clone)]
enum CanonicalSvgPathSegment {
  MoveTo { x: f32, y: f32 },
  LineTo { x: f32, y: f32 },
  CurveTo {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    x: f32,
    y: f32,
  },
  Quadratic { x1: f32, y1: f32, x: f32, y: f32 },
  ClosePath,
}

fn canonicalize_svg_path_data(data: &str) -> Option<Vec<CanonicalSvgPathSegment>> {
  use svgtypes::PathParser;
  use svgtypes::PathSegment;

  let mut segments = Vec::new();
  let mut current = (0.0f32, 0.0f32);
  let mut subpath_start = (0.0f32, 0.0f32);
  let mut last_cubic_ctrl: Option<(f32, f32)> = None;
  let mut last_quad_ctrl: Option<(f32, f32)> = None;

  let mut deadline_counter = 0usize;
  let stage = crate::render_control::active_stage().unwrap_or(RenderStage::Paint);

  let to_f32 = |v: f64| -> Option<f32> {
    let out = v as f32;
    out.is_finite().then_some(out)
  };

  for segment in PathParser::from(data) {
    check_active_periodic(&mut deadline_counter, CLIP_PATH_PATH_DEADLINE_STRIDE, stage).ok()?;

    let seg = segment.ok()?;
    match seg {
      PathSegment::MoveTo { abs, x, y } => {
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(nx.is_finite() && ny.is_finite()) {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::MoveTo { x: nx, y: ny });
        current = (nx, ny);
        subpath_start = current;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::LineTo { abs, x, y } => {
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(nx.is_finite() && ny.is_finite()) {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::LineTo { x: nx, y: ny });
        current = (nx, ny);
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::HorizontalLineTo { abs, x } => {
        let x = to_f32(x)?;
        let nx = if abs { x } else { current.0 + x };
        if !nx.is_finite() {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::LineTo {
          x: nx,
          y: current.1,
        });
        current.0 = nx;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::VerticalLineTo { abs, y } => {
        let y = to_f32(y)?;
        let ny = if abs { y } else { current.1 + y };
        if !ny.is_finite() {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::LineTo {
          x: current.0,
          y: ny,
        });
        current.1 = ny;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::CurveTo {
        abs,
        x1,
        y1,
        x2,
        y2,
        x,
        y,
      } => {
        let x1 = to_f32(x1)?;
        let y1 = to_f32(y1)?;
        let x2 = to_f32(x2)?;
        let y2 = to_f32(y2)?;
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (cx1, cy1) = if abs {
          (x1, y1)
        } else {
          (current.0 + x1, current.1 + y1)
        };
        let (cx2, cy2) = if abs {
          (x2, y2)
        } else {
          (current.0 + x2, current.1 + y2)
        };
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(cx1.is_finite()
          && cy1.is_finite()
          && cx2.is_finite()
          && cy2.is_finite()
          && nx.is_finite()
          && ny.is_finite())
        {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::CurveTo {
          x1: cx1,
          y1: cy1,
          x2: cx2,
          y2: cy2,
          x: nx,
          y: ny,
        });
        current = (nx, ny);
        last_cubic_ctrl = Some((cx2, cy2));
        last_quad_ctrl = None;
      }
      PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
        let x2 = to_f32(x2)?;
        let y2 = to_f32(y2)?;
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (cx1, cy1) = match last_cubic_ctrl {
          Some((px, py)) => (2.0 * current.0 - px, 2.0 * current.1 - py),
          None => current,
        };
        let (cx2, cy2) = if abs {
          (x2, y2)
        } else {
          (current.0 + x2, current.1 + y2)
        };
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(cx1.is_finite()
          && cy1.is_finite()
          && cx2.is_finite()
          && cy2.is_finite()
          && nx.is_finite()
          && ny.is_finite())
        {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::CurveTo {
          x1: cx1,
          y1: cy1,
          x2: cx2,
          y2: cy2,
          x: nx,
          y: ny,
        });
        current = (nx, ny);
        last_cubic_ctrl = Some((cx2, cy2));
        last_quad_ctrl = None;
      }
      PathSegment::Quadratic { abs, x1, y1, x, y } => {
        let x1 = to_f32(x1)?;
        let y1 = to_f32(y1)?;
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (cx1, cy1) = if abs {
          (x1, y1)
        } else {
          (current.0 + x1, current.1 + y1)
        };
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(cx1.is_finite() && cy1.is_finite() && nx.is_finite() && ny.is_finite()) {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::Quadratic {
          x1: cx1,
          y1: cy1,
          x: nx,
          y: ny,
        });
        current = (nx, ny);
        last_quad_ctrl = Some((cx1, cy1));
        last_cubic_ctrl = None;
      }
      PathSegment::SmoothQuadratic { abs, x, y } => {
        let x = to_f32(x)?;
        let y = to_f32(y)?;
        let (cx1, cy1) = match last_quad_ctrl {
          Some((px, py)) => (2.0 * current.0 - px, 2.0 * current.1 - py),
          None => current,
        };
        let (nx, ny) = if abs {
          (x, y)
        } else {
          (current.0 + x, current.1 + y)
        };
        if !(cx1.is_finite() && cy1.is_finite() && nx.is_finite() && ny.is_finite()) {
          return None;
        }
        segments.push(CanonicalSvgPathSegment::Quadratic {
          x1: cx1,
          y1: cy1,
          x: nx,
          y: ny,
        });
        current = (nx, ny);
        last_quad_ctrl = Some((cx1, cy1));
        last_cubic_ctrl = None;
      }
      // We currently treat arcs as non-interpolatable. (`svg_path.rs` converts arcs to cubics for
      // rendering; if we want to interpolate arcs in the future we'd need a similar canonicalisation
      // pass here.)
      PathSegment::EllipticalArc { .. } => return None,
      PathSegment::ClosePath { .. } => {
        segments.push(CanonicalSvgPathSegment::ClosePath);
        current = subpath_start;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
    }
  }

  Some(segments)
}

fn push_svg_path_number(out: &mut String, value: f32) -> Option<()> {
  if !value.is_finite() {
    return None;
  }
  let value = if value == 0.0 { 0.0 } else { value };
  out.push_str(&value.to_string());
  Some(())
}

fn serialize_svg_path_data(segments: &[CanonicalSvgPathSegment]) -> Option<Arc<str>> {
  let mut out = String::new();
  for (idx, seg) in segments.iter().enumerate() {
    if idx > 0 {
      out.push(' ');
    }
    match seg {
      CanonicalSvgPathSegment::MoveTo { x, y } => {
        out.push('M');
        out.push(' ');
        push_svg_path_number(&mut out, *x)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y)?;
      }
      CanonicalSvgPathSegment::LineTo { x, y } => {
        out.push('L');
        out.push(' ');
        push_svg_path_number(&mut out, *x)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y)?;
      }
      CanonicalSvgPathSegment::CurveTo {
        x1,
        y1,
        x2,
        y2,
        x,
        y,
      } => {
        out.push('C');
        out.push(' ');
        push_svg_path_number(&mut out, *x1)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y1)?;
        out.push(' ');
        push_svg_path_number(&mut out, *x2)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y2)?;
        out.push(' ');
        push_svg_path_number(&mut out, *x)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y)?;
      }
      CanonicalSvgPathSegment::Quadratic { x1, y1, x, y } => {
        out.push('Q');
        out.push(' ');
        push_svg_path_number(&mut out, *x1)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y1)?;
        out.push(' ');
        push_svg_path_number(&mut out, *x)?;
        out.push(' ');
        push_svg_path_number(&mut out, *y)?;
      }
      CanonicalSvgPathSegment::ClosePath => {
        out.push('Z');
      }
    }
  }

  Some(Arc::from(out))
}

fn interpolate_svg_path_data(a: &str, b: &str, t: f32) -> Option<Arc<str>> {
  if !t.is_finite() {
    return None;
  }

  let a_segments = canonicalize_svg_path_data(a)?;
  let b_segments = canonicalize_svg_path_data(b)?;
  if a_segments.len() != b_segments.len() {
    return None;
  }
  for (sa, sb) in a_segments.iter().zip(b_segments.iter()) {
    if discriminant(sa) != discriminant(sb) {
      return None;
    }
  }

  let mut out = Vec::with_capacity(a_segments.len());
  for (sa, sb) in a_segments.iter().zip(b_segments.iter()) {
    let seg = match (sa, sb) {
      (CanonicalSvgPathSegment::MoveTo { x: ax, y: ay }, CanonicalSvgPathSegment::MoveTo { x: bx, y: by }) => {
        CanonicalSvgPathSegment::MoveTo {
          x: lerp(*ax, *bx, t),
          y: lerp(*ay, *by, t),
        }
      }
      (CanonicalSvgPathSegment::LineTo { x: ax, y: ay }, CanonicalSvgPathSegment::LineTo { x: bx, y: by }) => {
        CanonicalSvgPathSegment::LineTo {
          x: lerp(*ax, *bx, t),
          y: lerp(*ay, *by, t),
        }
      }
      (
        CanonicalSvgPathSegment::CurveTo {
          x1: ax1,
          y1: ay1,
          x2: ax2,
          y2: ay2,
          x: ax,
          y: ay,
        },
        CanonicalSvgPathSegment::CurveTo {
          x1: bx1,
          y1: by1,
          x2: bx2,
          y2: by2,
          x: bx,
          y: by,
        },
      ) => CanonicalSvgPathSegment::CurveTo {
        x1: lerp(*ax1, *bx1, t),
        y1: lerp(*ay1, *by1, t),
        x2: lerp(*ax2, *bx2, t),
        y2: lerp(*ay2, *by2, t),
        x: lerp(*ax, *bx, t),
        y: lerp(*ay, *by, t),
      },
      (
        CanonicalSvgPathSegment::Quadratic {
          x1: ax1,
          y1: ay1,
          x: ax,
          y: ay,
        },
        CanonicalSvgPathSegment::Quadratic {
          x1: bx1,
          y1: by1,
          x: bx,
          y: by,
        },
      ) => CanonicalSvgPathSegment::Quadratic {
        x1: lerp(*ax1, *bx1, t),
        y1: lerp(*ay1, *by1, t),
        x: lerp(*ax, *bx, t),
        y: lerp(*ay, *by, t),
      },
      (CanonicalSvgPathSegment::ClosePath, CanonicalSvgPathSegment::ClosePath) => {
        CanonicalSvgPathSegment::ClosePath
      }
      _ => return None,
    };

    // Ensure the interpolated numbers are finite so we don't emit invalid path data.
    match &seg {
      CanonicalSvgPathSegment::MoveTo { x, y } | CanonicalSvgPathSegment::LineTo { x, y } => {
        if !(x.is_finite() && y.is_finite()) {
          return None;
        }
      }
      CanonicalSvgPathSegment::CurveTo {
        x1,
        y1,
        x2,
        y2,
        x,
        y,
      } => {
        if !(x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() && x.is_finite() && y.is_finite()) {
          return None;
        }
      }
      CanonicalSvgPathSegment::Quadratic { x1, y1, x, y } => {
        if !(x1.is_finite() && y1.is_finite() && x.is_finite() && y.is_finite()) {
          return None;
        }
      }
      CanonicalSvgPathSegment::ClosePath => {}
    }

    out.push(seg);
  }

  serialize_svg_path_data(&out)
}

fn interpolate_clip_paths(
  a: &ResolvedClipPath,
  b: &ResolvedClipPath,
  t: f32,
) -> Option<ResolvedClipPath> {
  match (a, b) {
    (ResolvedClipPath::None, ResolvedClipPath::None) => Some(ResolvedClipPath::None),
    (ResolvedClipPath::Box(a), ResolvedClipPath::Box(b)) if a == b => {
      Some(ResolvedClipPath::Box(*a))
    }
    (
      ResolvedClipPath::Inset {
        top: ta,
        right: ra,
        bottom: ba,
        left: la,
        radii: raadii,
        reference: refa,
      },
      ResolvedClipPath::Inset {
        top: tb,
        right: rb,
        bottom: bb,
        left: lb,
        radii: rbra,
        reference: refb,
      },
    ) if refa.unwrap_or(ReferenceBox::BorderBox) == refb.unwrap_or(ReferenceBox::BorderBox) => {
      let radii = match (raadii, rbra) {
        (Some(a), Some(b)) => Some(interpolate_radii(*a, *b, t)),
        (None, None) => None,
        _ => return None,
      };
      Some(ResolvedClipPath::Inset {
        top: lerp(*ta, *tb, t),
        right: lerp(*ra, *rb, t),
        bottom: lerp(*ba, *bb, t),
        left: lerp(*la, *lb, t),
        radii,
        reference: *refa,
      })
    }
    (
      ResolvedClipPath::Circle {
        radius: ra,
        position: pa,
        reference: refa,
      },
      ResolvedClipPath::Circle {
        radius: rb,
        position: pb,
        reference: refb,
      },
    ) if refa.unwrap_or(ReferenceBox::BorderBox) == refb.unwrap_or(ReferenceBox::BorderBox) => {
      let pos = interpolate_background_positions(&[pa.clone()], &[pb.clone()], t)?;
      Some(ResolvedClipPath::Circle {
        radius: lerp(*ra, *rb, t),
        position: pos[0].clone(),
        reference: *refa,
      })
    }
    (
      ResolvedClipPath::Ellipse {
        radius_x: rxa,
        radius_y: rya,
        position: pa,
        reference: refa,
      },
      ResolvedClipPath::Ellipse {
        radius_x: rxb,
        radius_y: ryb,
        position: pb,
        reference: refb,
      },
    ) if refa.unwrap_or(ReferenceBox::BorderBox) == refb.unwrap_or(ReferenceBox::BorderBox) => {
      let pos = interpolate_background_positions(&[pa.clone()], &[pb.clone()], t)?;
      Some(ResolvedClipPath::Ellipse {
        radius_x: lerp(*rxa, *rxb, t),
        radius_y: lerp(*rya, *ryb, t),
        position: pos[0].clone(),
        reference: *refa,
      })
    }
    (
      ResolvedClipPath::Polygon {
        fill: fa,
        points: pa,
        reference: refa,
      },
      ResolvedClipPath::Polygon {
        fill: fb,
        points: pb,
        reference: refb,
      },
    ) if fa == fb
      && refa.unwrap_or(ReferenceBox::BorderBox) == refb.unwrap_or(ReferenceBox::BorderBox)
      && pa.len() == pb.len() =>
    {
      let mut points = Vec::with_capacity(pa.len());
      for ((ax, ay), (bx, by)) in pa.iter().zip(pb.iter()) {
        points.push((lerp(*ax, *bx, t), lerp(*ay, *by, t)));
      }
      Some(ResolvedClipPath::Polygon {
        fill: *fa,
        points,
        reference: *refa,
      })
    }
    (
      ResolvedClipPath::Path {
        fill: fa,
        reference: refa,
        data: da,
      },
      ResolvedClipPath::Path {
        fill: fb,
        reference: refb,
        data: db,
      },
    ) if fa == fb && refa == refb => {
      let data = interpolate_svg_path_data(da.as_ref(), db.as_ref(), t)?;
      Some(ResolvedClipPath::Path {
        fill: *fa,
        reference: *refa,
        data,
      })
    }
    _ => None,
  }
}

fn resolved_clip_to_clip_path(resolved: &ResolvedClipPath) -> ClipPath {
  let normalize_reference = |reference: Option<ReferenceBox>| match reference {
    // `border-box` is the computed default for clip-path reference boxes. Store it as `None` so we
    // preserve the canonical representation for "unspecified" in interpolated values.
    Some(ReferenceBox::BorderBox) => None,
    other => other,
  };

  match resolved {
    ResolvedClipPath::None => ClipPath::None,
    ResolvedClipPath::Box(b) => ClipPath::Box(*b),
    ResolvedClipPath::Inset {
      top,
      right,
      bottom,
      left,
      radii,
      reference,
    } => {
      let border_radius: Box<Option<ClipRadii>> = Box::new(radii.map(|r| ClipRadii {
        top_left: r[0],
        top_right: r[1],
        bottom_right: r[2],
        bottom_left: r[3],
      }));
      ClipPath::BasicShape(
        Box::new(BasicShape::Inset {
          top: Length::px(*top),
          right: Length::px(*right),
          bottom: Length::px(*bottom),
          left: Length::px(*left),
          border_radius,
        }),
        normalize_reference(*reference),
      )
    }
    ResolvedClipPath::Circle {
      radius,
      position,
      reference,
    } => ClipPath::BasicShape(
      Box::new(BasicShape::Circle {
        radius: ShapeRadius::Length(Length::px(*radius)),
        position: BackgroundPosition::Position {
          x: BackgroundPositionComponent {
            alignment: position.x.alignment,
            offset: Length::px(position.x.offset),
          },
          y: BackgroundPositionComponent {
            alignment: position.y.alignment,
            offset: Length::px(position.y.offset),
          },
        },
      }),
      normalize_reference(*reference),
    ),
    ResolvedClipPath::Ellipse {
      radius_x,
      radius_y,
      position,
      reference,
    } => ClipPath::BasicShape(
      Box::new(BasicShape::Ellipse {
        radius_x: ShapeRadius::Length(Length::px(*radius_x)),
        radius_y: ShapeRadius::Length(Length::px(*radius_y)),
        position: BackgroundPosition::Position {
          x: BackgroundPositionComponent {
            alignment: position.x.alignment,
            offset: Length::px(position.x.offset),
          },
          y: BackgroundPositionComponent {
            alignment: position.y.alignment,
            offset: Length::px(position.y.offset),
          },
        },
      }),
      normalize_reference(*reference),
    ),
    ResolvedClipPath::Polygon {
      fill,
      points,
      reference,
    } => ClipPath::BasicShape(
      Box::new(BasicShape::Polygon {
        fill: *fill,
        points: points
          .iter()
          .map(|(x, y)| (Length::px(*x), Length::px(*y)))
          .collect(),
      }),
      normalize_reference(*reference),
    ),
    ResolvedClipPath::Path {
      fill,
      reference,
      data,
    } => ClipPath::BasicShape(
      Box::new(BasicShape::Path {
        fill: *fill,
        data: Arc::clone(data),
      }),
      normalize_reference(*reference),
    ),
  }
}

fn clip_path_to_resolved(path: &ClipPath) -> Option<ResolvedClipPath> {
  match path {
    ClipPath::None => Some(ResolvedClipPath::None),
    ClipPath::Box(b) => Some(ResolvedClipPath::Box(*b)),
    ClipPath::BasicShape(shape, reference) => match shape.as_ref() {
      BasicShape::Inset {
        top,
        right,
        bottom,
        left,
        border_radius,
      } => Some(ResolvedClipPath::Inset {
        top: top.to_px(),
        right: right.to_px(),
        bottom: bottom.to_px(),
        left: left.to_px(),
        radii: border_radius.as_ref().map(|r| {
          [
            BorderCornerRadius {
              x: Length::px(r.top_left.x.to_px()),
              y: Length::px(r.top_left.y.to_px()),
            },
            BorderCornerRadius {
              x: Length::px(r.top_right.x.to_px()),
              y: Length::px(r.top_right.y.to_px()),
            },
            BorderCornerRadius {
              x: Length::px(r.bottom_right.x.to_px()),
              y: Length::px(r.bottom_right.y.to_px()),
            },
            BorderCornerRadius {
              x: Length::px(r.bottom_left.x.to_px()),
              y: Length::px(r.bottom_left.y.to_px()),
            },
          ]
        }),
        reference: Some(reference.unwrap_or(ReferenceBox::BorderBox)),
      }),
      BasicShape::Circle { radius, position } => match (radius, position) {
        (ShapeRadius::Length(len), BackgroundPosition::Position { x, y }) => {
          Some(ResolvedClipPath::Circle {
            radius: len.to_px(),
            position: ResolvedBackgroundPosition {
              x: ResolvedPositionComponent {
                alignment: x.alignment,
                offset: x.offset.to_px(),
              },
              y: ResolvedPositionComponent {
                alignment: y.alignment,
                offset: y.offset.to_px(),
              },
            },
            reference: Some(reference.unwrap_or(ReferenceBox::BorderBox)),
          })
        }
        _ => None,
      },
      BasicShape::Ellipse {
        radius_x,
        radius_y,
        position,
      } => match (radius_x, radius_y, position) {
        (
          ShapeRadius::Length(rx),
          ShapeRadius::Length(ry),
          BackgroundPosition::Position { x, y },
        ) => Some(ResolvedClipPath::Ellipse {
          radius_x: rx.to_px(),
          radius_y: ry.to_px(),
          position: ResolvedBackgroundPosition {
            x: ResolvedPositionComponent {
              alignment: x.alignment,
              offset: x.offset.to_px(),
            },
            y: ResolvedPositionComponent {
              alignment: y.alignment,
              offset: y.offset.to_px(),
            },
          },
          reference: Some(reference.unwrap_or(ReferenceBox::BorderBox)),
        }),
        _ => None,
      },
      BasicShape::Polygon { fill, points } => Some(ResolvedClipPath::Polygon {
        fill: *fill,
        points: points.iter().map(|(x, y)| (x.to_px(), y.to_px())).collect(),
        reference: Some(reference.unwrap_or(ReferenceBox::BorderBox)),
      }),
      BasicShape::Path { fill, data } => Some(ResolvedClipPath::Path {
        fill: *fill,
        reference: *reference,
        data: Arc::clone(data),
      }),
    },
  }
}

struct PropertyInterpolator {
  name: &'static str,
  extract: fn(&ComputedStyle, &AnimationResolveContext) -> Option<AnimatedValue>,
  interpolate: fn(&AnimatedValue, &AnimatedValue, f32) -> Option<AnimatedValue>,
  apply: fn(&mut ComputedStyle, &AnimatedValue),
}

fn extract_opacity(style: &ComputedStyle, _ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Opacity(style.opacity))
}

fn interpolate_opacity(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::Opacity(x), AnimatedValue::Opacity(y)) => {
      Some(AnimatedValue::Opacity(lerp(*x, *y, t)))
    }
    _ => None,
  }
}

fn apply_opacity(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Opacity(v) = value {
    style.opacity = clamp_progress(*v);
  }
}

fn extract_visibility(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Visibility(style.visibility))
}

fn interpolate_visibility_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::Visibility(va), AnimatedValue::Visibility(vb)) = (a, b) else {
    return None;
  };

  // `visibility` is a discrete animated property with special-case behaviour:
  // if either endpoint is `visible`, the intermediate values should also be `visible`
  // (so opacity fades don't keep content hidden for the whole segment).
  //
  // We preserve the exact endpoints (t=0 and t=1) deterministically using EPS
  // tolerances consistent with other sampling code in this module.
  if matches!(*va, Visibility::Visible) || matches!(*vb, Visibility::Visible) {
    if t <= f32::EPSILON {
      return Some(AnimatedValue::Visibility(*va));
    }
    if (1.0 - t).abs() <= f32::EPSILON {
      return Some(AnimatedValue::Visibility(*vb));
    }
    return Some(AnimatedValue::Visibility(Visibility::Visible));
  }

  // Base discrete behaviour: switch at the 50% midpoint.
  Some(AnimatedValue::Visibility(if t < 0.5 { *va } else { *vb }))
}

fn apply_visibility(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Visibility(v) = value {
    style.visibility = *v;
  }
}

fn extract_color(style: &ComputedStyle, _ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.color))
}

fn extract_background_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.background_color))
}

fn interpolate_color(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::Color(ca), AnimatedValue::Color(cb)) => {
      Some(AnimatedValue::Color(lerp_color(*ca, *cb, t)))
    }
    _ => None,
  }
}

fn interpolate_length_value(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::Length(la), AnimatedValue::Length(lb)) => Some(AnimatedValue::Length(
      Length::px(lerp(la.to_px(), lb.to_px(), t)),
    )),
    _ => None,
  }
}

fn apply_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.color = *c;
  }
}

fn apply_background_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.background_color = *c;
  }
}

fn extract_transform(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Transform(resolve_transform_list(
    &style.transform,
    style,
    ctx,
  )))
}

fn interpolate_transform_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::Transform(ta), AnimatedValue::Transform(tb)) => {
      // Preserve exact endpoints deterministically (and avoid turning `none` into an explicit
      // identity matrix which would incorrectly establish transform containing blocks).
      if t <= f32::EPSILON {
        return Some(AnimatedValue::Transform(ta.clone()));
      }
      if t >= 1.0 - f32::EPSILON {
        return Some(AnimatedValue::Transform(tb.clone()));
      }

      let interpolated = interpolate_transform_lists(ta, tb, t).unwrap_or_else(|| {
        let ma = compose_transform_list(ta);
        let mb = compose_transform_list(tb);
        vec![crate::css::types::Transform::Matrix3d(
          lerp_matrix(&ma, &mb, t).m,
        )]
      });
      Some(AnimatedValue::Transform(interpolated))
    }
    _ => None,
  }
}

fn apply_transform(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Transform(list) = value {
    style.transform = list.clone();
  }
}

fn extract_translate(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let (width, height) = transform_reference_size(style, ctx);
  let resolved = match style.translate {
    TranslateValue::None => TranslateValue::None,
    TranslateValue::Values { x, y, z } => TranslateValue::Values {
      x: Length::px(resolve_length_px(&x, Some(width), style, ctx)),
      y: Length::px(resolve_length_px(&y, Some(height), style, ctx)),
      // translate Z disallows percentages.
      z: Length::px(resolve_length_px(&z, None, style, ctx)),
    },
  };
  Some(AnimatedValue::Translate(resolved))
}

fn interpolate_translate_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::Translate(ta), AnimatedValue::Translate(tb)) = (a, b) else {
    return None;
  };
  // Preserve exact endpoints deterministically, especially when `none` is involved (to avoid
  // spuriously establishing transform containing blocks).
  if t <= f32::EPSILON {
    return Some(AnimatedValue::Translate(*ta));
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(AnimatedValue::Translate(*tb));
  }
  if matches!(ta, TranslateValue::None) && matches!(tb, TranslateValue::None) {
    return Some(AnimatedValue::Translate(TranslateValue::None));
  }

  let (ax, ay, az) = match ta {
    TranslateValue::None => (0.0, 0.0, 0.0),
    TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
  };
  let (bx, by, bz) = match tb {
    TranslateValue::None => (0.0, 0.0, 0.0),
    TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
  };
  Some(AnimatedValue::Translate(TranslateValue::Values {
    x: Length::px(lerp(ax, bx, t)),
    y: Length::px(lerp(ay, by, t)),
    z: Length::px(lerp(az, bz, t)),
  }))
}

fn apply_translate(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Translate(v) = value {
    style.translate = *v;
  }
}

fn extract_rotate(style: &ComputedStyle, _ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Rotate(style.rotate))
}

fn interpolate_rotate_value(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  let (AnimatedValue::Rotate(ra), AnimatedValue::Rotate(rb)) = (a, b) else {
    return None;
  };
  if matches!(ra, RotateValue::None) && matches!(rb, RotateValue::None) {
    return Some(AnimatedValue::Rotate(RotateValue::None));
  }

  // Preserve exact endpoints deterministically.
  if t <= f32::EPSILON {
    return Some(AnimatedValue::Rotate(*ra));
  }
  if (1.0 - t).abs() <= f32::EPSILON {
    return Some(AnimatedValue::Rotate(*rb));
  }

  fn z_axis_angle_degrees(v: RotateValue) -> Option<f32> {
    match v {
      RotateValue::None => Some(0.0),
      RotateValue::Angle(deg) => Some(deg),
      RotateValue::AxisAngle { x, y, z, angle } if x == 0.0 && y == 0.0 => Some(angle * z.signum()),
      _ => None,
    }
  }

  if let (Some(a_deg), Some(b_deg)) = (z_axis_angle_degrees(*ra), z_axis_angle_degrees(*rb)) {
    return Some(AnimatedValue::Rotate(RotateValue::Angle(lerp(
      a_deg, b_deg, t,
    ))));
  }

  match (ra, rb) {
    (
      RotateValue::AxisAngle {
        x: ax,
        y: ay,
        z: az,
        angle: a_deg,
      },
      RotateValue::AxisAngle {
        x: bx,
        y: by,
        z: bz,
        angle: b_deg,
      },
    ) if (*ax - *bx).abs() < 1e-6 && (*ay - *by).abs() < 1e-6 && (*az - *bz).abs() < 1e-6 => {
      Some(AnimatedValue::Rotate(RotateValue::AxisAngle {
        x: *ax,
        y: *ay,
        z: *az,
        angle: lerp(*a_deg, *b_deg, t),
      }))
    }
    (RotateValue::None, RotateValue::AxisAngle { x, y, z, angle }) => {
      Some(AnimatedValue::Rotate(RotateValue::AxisAngle {
        x: *x,
        y: *y,
        z: *z,
        angle: lerp(0.0, *angle, t),
      }))
    }
    (RotateValue::AxisAngle { x, y, z, angle }, RotateValue::None) => {
      Some(AnimatedValue::Rotate(RotateValue::AxisAngle {
        x: *x,
        y: *y,
        z: *z,
        angle: lerp(*angle, 0.0, t),
      }))
    }
    _ => None,
  }
}

fn apply_rotate(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Rotate(v) = value {
    style.rotate = *v;
  }
}

fn extract_scale(style: &ComputedStyle, _ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Scale(style.scale))
}

fn interpolate_scale_value(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  let (AnimatedValue::Scale(sa), AnimatedValue::Scale(sb)) = (a, b) else {
    return None;
  };
  // Preserve exact endpoints deterministically so `scale:none` remains `none` at keyframe
  // boundaries.
  if t <= f32::EPSILON {
    return Some(AnimatedValue::Scale(*sa));
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(AnimatedValue::Scale(*sb));
  }
  if matches!(sa, ScaleValue::None) && matches!(sb, ScaleValue::None) {
    return Some(AnimatedValue::Scale(ScaleValue::None));
  }

  let (ax, ay, az) = match sa {
    ScaleValue::None => (1.0, 1.0, 1.0),
    ScaleValue::Values { x, y, z } => (*x, *y, *z),
  };
  let (bx, by, bz) = match sb {
    ScaleValue::None => (1.0, 1.0, 1.0),
    ScaleValue::Values { x, y, z } => (*x, *y, *z),
  };
  Some(AnimatedValue::Scale(ScaleValue::Values {
    x: lerp(ax, bx, t),
    y: lerp(ay, by, t),
    z: lerp(az, bz, t),
  }))
}

fn apply_scale(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Scale(v) = value {
    style.scale = *v;
  }
}

fn extract_offset_distance(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let (pct, px) = length_percentage_components(&style.offset_distance, style, ctx);
  let len = build_length_from_components(px, pct)?;
  Some(AnimatedValue::OffsetDistance(len))
}

fn interpolate_offset_distance_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::OffsetDistance(la), AnimatedValue::OffsetDistance(lb)) = (a, b) else {
    return None;
  };
  let (a_pct, a_px) = length_percentage_components_no_context(la);
  let (b_pct, b_px) = length_percentage_components_no_context(lb);
  let pct = lerp(a_pct, b_pct, t);
  let px = lerp(a_px, b_px, t);
  let len = build_length_from_components(px, pct)?;
  Some(AnimatedValue::OffsetDistance(len))
}

fn apply_offset_distance(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::OffsetDistance(v) = value {
    style.offset_distance = *v;
  }
}

fn extract_offset_anchor(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
  let (x_len, y_len) = match style.offset_anchor {
    OffsetAnchor::Auto => (Length::percent(50.0), Length::percent(50.0)),
    OffsetAnchor::Position { x, y } => (x, y),
  };
  let x = resolve_length_px(&x_len, Some(width), style, ctx);
  let y = resolve_length_px(&y_len, Some(height), style, ctx);
  if !x.is_finite() || !y.is_finite() {
    return None;
  }
  Some(AnimatedValue::OffsetAnchor { x, y })
}

fn interpolate_offset_anchor_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::OffsetAnchor { x: ax, y: ay }, AnimatedValue::OffsetAnchor { x: bx, y: by }) =
    (a, b)
  else {
    return None;
  };
  Some(AnimatedValue::OffsetAnchor {
    x: lerp(*ax, *bx, t),
    y: lerp(*ay, *by, t),
  })
}

fn apply_offset_anchor(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::OffsetAnchor { x, y } = value {
    style.offset_anchor = OffsetAnchor::Position {
      x: Length::px(*x),
      y: Length::px(*y),
    };
  }
}

fn extract_offset_rotate(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::OffsetRotate(style.offset_rotate))
}

fn interpolate_offset_rotate_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::OffsetRotate(ra), AnimatedValue::OffsetRotate(rb)) = (a, b) else {
    return None;
  };
  if t <= f32::EPSILON {
    return Some(AnimatedValue::OffsetRotate(*ra));
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(AnimatedValue::OffsetRotate(*rb));
  }
  match (ra, rb) {
    (OffsetRotate::Angle(a), OffsetRotate::Angle(b)) => Some(AnimatedValue::OffsetRotate(
      OffsetRotate::Angle(lerp(*a, *b, t)),
    )),
    (
      OffsetRotate::Auto {
        angle: a,
        reverse: rev_a,
      },
      OffsetRotate::Auto {
        angle: b,
        reverse: rev_b,
      },
    ) if rev_a == rev_b => Some(AnimatedValue::OffsetRotate(OffsetRotate::Auto {
      angle: lerp(*a, *b, t),
      reverse: *rev_a,
    })),
    _ => None,
  }
}

fn apply_offset_rotate(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::OffsetRotate(v) = value {
    style.offset_rotate = *v;
  }
}

fn extract_filter(style: &ComputedStyle, ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  let resolved = resolve_filter_list(&style.filter, style, ctx);
  Some(AnimatedValue::Filter(resolved_filters_to_functions(
    &resolved,
  )))
}

fn extract_backdrop_filter(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let resolved = resolve_filter_list(&style.backdrop_filter, style, ctx);
  Some(AnimatedValue::BackdropFilter(
    resolved_filters_to_functions(&resolved),
  ))
}

fn interpolate_filters_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::Filter(fa), AnimatedValue::Filter(fb)) => {
      let ra = resolved_filters_from_functions(fa);
      let rb = resolved_filters_from_functions(fb);
      let interpolated = interpolate_filters(&ra, &rb, t)?;
      Some(AnimatedValue::Filter(resolved_filters_to_functions(
        &interpolated,
      )))
    }
    (AnimatedValue::BackdropFilter(fa), AnimatedValue::BackdropFilter(fb)) => {
      let ra = resolved_filters_from_functions(fa);
      let rb = resolved_filters_from_functions(fb);
      let interpolated = interpolate_filters(&ra, &rb, t)?;
      Some(AnimatedValue::BackdropFilter(
        resolved_filters_to_functions(&interpolated),
      ))
    }
    _ => None,
  }
}

fn apply_filter(style: &mut ComputedStyle, value: &AnimatedValue) {
  match value {
    AnimatedValue::Filter(filters) => style.filter = filters.clone(),
    AnimatedValue::BackdropFilter(filters) => style.backdrop_filter = filters.clone(),
    _ => {}
  }
}

fn extract_clip_path(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  resolve_clip_path(&style.clip_path, style, ctx)
    .map(|resolved| AnimatedValue::ClipPath(resolved_clip_to_clip_path(&resolved)))
}

fn interpolate_clip_path_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::ClipPath(pa), AnimatedValue::ClipPath(pb)) = (a, b) else {
    return None;
  };
  let ra = clip_path_to_resolved(pa)?;
  let rb = clip_path_to_resolved(pb)?;
  let interpolated = interpolate_clip_paths(&ra, &rb, t)?;
  Some(AnimatedValue::ClipPath(resolved_clip_to_clip_path(
    &interpolated,
  )))
}

fn apply_clip_path(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::ClipPath(path) = value {
    style.clip_path = path.clone();
  }
}

fn resolve_clip_component(
  component: &ClipComponent,
  percent_base: f32,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> ClipComponent {
  match component {
    ClipComponent::Auto => ClipComponent::Auto,
    ClipComponent::Length(len) => ClipComponent::Length(Length::px(resolve_length_px(
      len,
      Some(percent_base),
      style,
      ctx,
    ))),
  }
}

fn extract_clip_rect(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let rect = style.clip.as_ref().map(|rect| {
    let width = ctx.element_size.width;
    let height = ctx.element_size.height;
    ClipRect {
      top: resolve_clip_component(&rect.top, height, style, ctx),
      right: resolve_clip_component(&rect.right, width, style, ctx),
      bottom: resolve_clip_component(&rect.bottom, height, style, ctx),
      left: resolve_clip_component(&rect.left, width, style, ctx),
    }
  });
  Some(AnimatedValue::ClipRect(rect))
}

fn interpolate_clip_component(
  a: &ClipComponent,
  b: &ClipComponent,
  t: f32,
) -> Option<ClipComponent> {
  match (a, b) {
    (ClipComponent::Auto, ClipComponent::Auto) => Some(ClipComponent::Auto),
    (ClipComponent::Length(a), ClipComponent::Length(b)) => Some(ClipComponent::Length(
      Length::px(lerp(a.to_px(), b.to_px(), t)),
    )),
    _ => None,
  }
}

fn interpolate_clip_rect_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::ClipRect(a), AnimatedValue::ClipRect(b)) = (a, b) else {
    return None;
  };
  let rect = match (a.as_ref(), b.as_ref()) {
    (None, None) => None,
    (Some(a), Some(b)) => Some(ClipRect {
      top: interpolate_clip_component(&a.top, &b.top, t)?,
      right: interpolate_clip_component(&a.right, &b.right, t)?,
      bottom: interpolate_clip_component(&a.bottom, &b.bottom, t)?,
      left: interpolate_clip_component(&a.left, &b.left, t)?,
    }),
    _ => return None,
  };
  Some(AnimatedValue::ClipRect(rect))
}

fn apply_clip_rect(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::ClipRect(rect) = value {
    style.clip = rect.clone();
  }
}

fn extract_transform_origin(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let (width, height) = transform_reference_size(style, ctx);
  Some(AnimatedValue::TransformOrigin(TransformOrigin {
    x: Length::px(resolve_length_px(
      &style.transform_origin.x,
      Some(width),
      style,
      ctx,
    )),
    y: Length::px(resolve_length_px(
      &style.transform_origin.y,
      Some(height),
      style,
      ctx,
    )),
    z: Length::px(resolve_length_px(
      &style.transform_origin.z,
      None,
      style,
      ctx,
    )),
  }))
}

fn extract_perspective_origin(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let (width, height) = transform_reference_size(style, ctx);
  Some(AnimatedValue::TransformOrigin(TransformOrigin {
    x: Length::px(resolve_length_px(
      &style.perspective_origin.x,
      Some(width),
      style,
      ctx,
    )),
    y: Length::px(resolve_length_px(
      &style.perspective_origin.y,
      Some(height),
      style,
      ctx,
    )),
    z: Length::px(resolve_length_px(
      &style.perspective_origin.z,
      None,
      style,
      ctx,
    )),
  }))
}

fn interpolate_transform_origin_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::TransformOrigin(oa), AnimatedValue::TransformOrigin(ob)) = (a, b) else {
    return None;
  };
  Some(AnimatedValue::TransformOrigin(TransformOrigin {
    x: Length::px(lerp(oa.x.to_px(), ob.x.to_px(), t)),
    y: Length::px(lerp(oa.y.to_px(), ob.y.to_px(), t)),
    z: Length::px(lerp(oa.z.to_px(), ob.z.to_px(), t)),
  }))
}

fn apply_transform_origin(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::TransformOrigin(origin) = value {
    style.transform_origin = *origin;
  }
}

fn apply_perspective_origin(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::TransformOrigin(origin) = value {
    style.perspective_origin = *origin;
  }
}

fn extract_background_position(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let resolved = resolve_background_positions(&style.background_positions, style, ctx);
  Some(AnimatedValue::BackgroundPosition(
    resolved_positions_to_background(&resolved),
  ))
}

fn interpolate_background_position_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BackgroundPosition(pa), AnimatedValue::BackgroundPosition(pb)) = (a, b)
  else {
    return None;
  };
  let ra = background_positions_to_resolved(pa);
  let rb = background_positions_to_resolved(pb);
  let interpolated = interpolate_background_positions(&ra, &rb, t)?;
  Some(AnimatedValue::BackgroundPosition(
    resolved_positions_to_background(&interpolated),
  ))
}

fn apply_background_position(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BackgroundPosition(pos) = value {
    style.background_positions = pos.clone().into();
    style.rebuild_background_layers();
  }
}

fn extract_mask_position(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let resolved = resolve_background_positions(&style.mask_positions, style, ctx);
  Some(AnimatedValue::BackgroundPosition(
    resolved_positions_to_background(&resolved),
  ))
}

fn apply_mask_position(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BackgroundPosition(pos) = value {
    style.mask_positions = pos.clone().into();
    style.rebuild_mask_layers();
  }
}

fn extract_background_size(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let resolved = resolve_background_sizes(&style.background_sizes, style, ctx);
  Some(AnimatedValue::BackgroundSize(resolved_sizes_to_background(
    &resolved,
  )))
}

fn interpolate_background_size_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BackgroundSize(sa), AnimatedValue::BackgroundSize(sb)) = (a, b) else {
    return None;
  };
  let ra = background_sizes_to_resolved(sa);
  let rb = background_sizes_to_resolved(sb);
  let interpolated = interpolate_background_sizes(&ra, &rb, t)?;
  Some(AnimatedValue::BackgroundSize(resolved_sizes_to_background(
    &interpolated,
  )))
}

fn apply_background_size(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BackgroundSize(sizes) = value {
    style.background_sizes = sizes.clone().into();
    style.rebuild_background_layers();
  }
}

fn extract_mask_size(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let resolved = resolve_background_sizes(&style.mask_sizes, style, ctx);
  Some(AnimatedValue::BackgroundSize(resolved_sizes_to_background(
    &resolved,
  )))
}

fn apply_mask_size(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BackgroundSize(sizes) = value {
    style.mask_sizes = sizes.clone().into();
    style.rebuild_mask_layers();
  }
}

fn resolve_box_shadow_list(
  shadows: &[BoxShadow],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<BoxShadow> {
  shadows
    .iter()
    .map(|shadow| BoxShadow {
      offset_x: Length::px(resolve_length_px(&shadow.offset_x, None, style, ctx)),
      offset_y: Length::px(resolve_length_px(&shadow.offset_y, None, style, ctx)),
      blur_radius: Length::px(resolve_length_px(&shadow.blur_radius, None, style, ctx)),
      spread_radius: Length::px(resolve_length_px(&shadow.spread_radius, None, style, ctx)),
      color: shadow.color,
      inset: shadow.inset,
    })
    .collect()
}

fn extract_box_shadow(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::BoxShadow(resolve_box_shadow_list(
    &style.box_shadow,
    style,
    ctx,
  )))
}

fn transparent_box_shadow_like(shadow: &BoxShadow) -> BoxShadow {
  BoxShadow {
    offset_x: Length::px(0.0),
    offset_y: Length::px(0.0),
    blur_radius: Length::px(0.0),
    spread_radius: Length::px(0.0),
    color: Rgba::new(shadow.color.r, shadow.color.g, shadow.color.b, 0.0),
    inset: shadow.inset,
  }
}

fn add_box_shadow_list(a: &[BoxShadow], b: &[BoxShadow]) -> Option<Vec<BoxShadow>> {
  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    match (a.get(idx), b.get(idx)) {
      (Some(a), Some(b)) => {
        if a.inset != b.inset {
          return None;
        }
        if !a.color.a.is_finite() || !b.color.a.is_finite() {
          return None;
        }
        let ax = a.offset_x.to_px();
        let ay = a.offset_y.to_px();
        let ab = a.blur_radius.to_px();
        let aspread = a.spread_radius.to_px();
        let bx = b.offset_x.to_px();
        let by = b.offset_y.to_px();
        let bb = b.blur_radius.to_px();
        let bspread = b.spread_radius.to_px();
        if !ax.is_finite()
          || !ay.is_finite()
          || !ab.is_finite()
          || !aspread.is_finite()
          || !bx.is_finite()
          || !by.is_finite()
          || !bb.is_finite()
          || !bspread.is_finite()
        {
          return None;
        }
        out.push(BoxShadow {
          offset_x: Length::px(ax + bx),
          offset_y: Length::px(ay + by),
          blur_radius: Length::px((ab + bb).max(0.0)),
          spread_radius: Length::px(aspread + bspread),
          color: add_color(a.color, b.color),
          inset: a.inset,
        });
      }
      (Some(a), None) => out.push(a.clone()),
      (None, Some(b)) => out.push(b.clone()),
      (None, None) => {}
    }
  }
  Some(out)
}

fn interpolate_single_box_shadow(a: &BoxShadow, b: &BoxShadow, t: f32) -> Option<BoxShadow> {
  if a.inset != b.inset {
    return None;
  }
  Some(BoxShadow {
    offset_x: Length::px(lerp(a.offset_x.to_px(), b.offset_x.to_px(), t)),
    offset_y: Length::px(lerp(a.offset_y.to_px(), b.offset_y.to_px(), t)),
    blur_radius: Length::px(lerp(a.blur_radius.to_px(), b.blur_radius.to_px(), t)),
    spread_radius: Length::px(lerp(a.spread_radius.to_px(), b.spread_radius.to_px(), t)),
    color: lerp_color(a.color, b.color, t),
    inset: a.inset,
  })
}

fn interpolate_box_shadow_list(a: &[BoxShadow], b: &[BoxShadow], t: f32) -> Option<Vec<BoxShadow>> {
  if t <= f32::EPSILON {
    return Some(a.to_vec());
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(b.to_vec());
  }

  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    match (a.get(idx), b.get(idx)) {
      (Some(a_shadow), Some(b_shadow)) => {
        out.push(interpolate_single_box_shadow(a_shadow, b_shadow, t)?);
      }
      (Some(a_shadow), None) => {
        let transparent = transparent_box_shadow_like(a_shadow);
        out.push(interpolate_single_box_shadow(a_shadow, &transparent, t)?);
      }
      (None, Some(b_shadow)) => {
        let transparent = transparent_box_shadow_like(b_shadow);
        out.push(interpolate_single_box_shadow(&transparent, b_shadow, t)?);
      }
      (None, None) => {}
    }
  }

  Some(out)
}

fn interpolate_box_shadow_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BoxShadow(sa), AnimatedValue::BoxShadow(sb)) = (a, b) else {
    return None;
  };

  Some(AnimatedValue::BoxShadow(interpolate_box_shadow_list(
    sa, sb, t,
  )?))
}

fn apply_box_shadow(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BoxShadow(shadows) = value {
    style.box_shadow = shadows.clone();
  }
}

fn resolve_text_shadow_list(
  shadows: &[TextShadow],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<TextShadow> {
  shadows
    .iter()
    .map(|shadow| TextShadow {
      offset_x: Length::px(resolve_length_px(&shadow.offset_x, None, style, ctx)),
      offset_y: Length::px(resolve_length_px(&shadow.offset_y, None, style, ctx)),
      blur_radius: Length::px(resolve_length_px(&shadow.blur_radius, None, style, ctx)),
      color: Some(shadow.color.unwrap_or(style.color)),
    })
    .collect()
}

fn extract_text_shadow(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::TextShadow(resolve_text_shadow_list(
    style.text_shadow.as_ref(),
    style,
    ctx,
  )))
}

fn transparent_text_shadow_like(shadow: &TextShadow) -> TextShadow {
  let color = shadow.color.unwrap_or(Rgba::BLACK);
  TextShadow {
    offset_x: Length::px(0.0),
    offset_y: Length::px(0.0),
    blur_radius: Length::px(0.0),
    color: Some(Rgba::new(color.r, color.g, color.b, 0.0)),
  }
}

fn add_text_shadow_list(a: &[TextShadow], b: &[TextShadow]) -> Option<Vec<TextShadow>> {
  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    match (a.get(idx), b.get(idx)) {
      (Some(a), Some(b)) => {
        let ca = a.color.unwrap_or(Rgba::BLACK);
        let cb = b.color.unwrap_or(Rgba::BLACK);
        if !ca.a.is_finite() || !cb.a.is_finite() {
          return None;
        }
        let ax = a.offset_x.to_px();
        let ay = a.offset_y.to_px();
        let ab = a.blur_radius.to_px();
        let bx = b.offset_x.to_px();
        let by = b.offset_y.to_px();
        let bb = b.blur_radius.to_px();
        if !ax.is_finite()
          || !ay.is_finite()
          || !ab.is_finite()
          || !bx.is_finite()
          || !by.is_finite()
          || !bb.is_finite()
        {
          return None;
        }
        out.push(TextShadow {
          offset_x: Length::px(ax + bx),
          offset_y: Length::px(ay + by),
          blur_radius: Length::px((ab + bb).max(0.0)),
          color: Some(add_color(ca, cb)),
        });
      }
      (Some(a), None) => out.push(a.clone()),
      (None, Some(b)) => out.push(b.clone()),
      (None, None) => {}
    }
  }
  Some(out)
}

fn interpolate_single_text_shadow(a: &TextShadow, b: &TextShadow, t: f32) -> Option<TextShadow> {
  Some(TextShadow {
    offset_x: Length::px(lerp(a.offset_x.to_px(), b.offset_x.to_px(), t)),
    offset_y: Length::px(lerp(a.offset_y.to_px(), b.offset_y.to_px(), t)),
    blur_radius: Length::px(lerp(a.blur_radius.to_px(), b.blur_radius.to_px(), t)),
    color: Some(lerp_color(
      a.color.unwrap_or(Rgba::BLACK),
      b.color.unwrap_or(Rgba::BLACK),
      t,
    )),
  })
}

fn interpolate_text_shadow_list(
  a: &[TextShadow],
  b: &[TextShadow],
  t: f32,
) -> Option<Vec<TextShadow>> {
  if t <= f32::EPSILON {
    return Some(a.to_vec());
  }
  if t >= 1.0 - f32::EPSILON {
    return Some(b.to_vec());
  }

  let max_len = a.len().max(b.len());
  let mut out = Vec::with_capacity(max_len);
  for idx in 0..max_len {
    match (a.get(idx), b.get(idx)) {
      (Some(a_shadow), Some(b_shadow)) => {
        out.push(interpolate_single_text_shadow(a_shadow, b_shadow, t)?);
      }
      (Some(a_shadow), None) => {
        let transparent = transparent_text_shadow_like(a_shadow);
        out.push(interpolate_single_text_shadow(a_shadow, &transparent, t)?);
      }
      (None, Some(b_shadow)) => {
        let transparent = transparent_text_shadow_like(b_shadow);
        out.push(interpolate_single_text_shadow(&transparent, b_shadow, t)?);
      }
      (None, None) => {}
    }
  }

  Some(out)
}

fn interpolate_text_shadow_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::TextShadow(sa), AnimatedValue::TextShadow(sb)) = (a, b) else {
    return None;
  };

  Some(AnimatedValue::TextShadow(interpolate_text_shadow_list(
    sa, sb, t,
  )?))
}

fn apply_text_shadow(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::TextShadow(shadows) = value {
    style.text_shadow = shadows.clone().into();
  }
}

fn extract_border_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::BorderColor([
    style.border_top_color,
    style.border_right_color,
    style.border_bottom_color,
    style.border_left_color,
  ]))
}

fn interpolate_border_color_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BorderColor(ca), AnimatedValue::BorderColor(cb)) = (a, b) else {
    return None;
  };

  let mut out = [Rgba::BLACK; 4];
  for i in 0..4 {
    out[i] = lerp_color(ca[i], cb[i], t);
  }
  Some(AnimatedValue::BorderColor(out))
}

fn apply_border_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderColor(c) = value {
    style.border_top_color = c[0];
    style.border_right_color = c[1];
    style.border_bottom_color = c[2];
    style.border_left_color = c[3];
  }
}

fn extract_border_top_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.border_top_color))
}

fn extract_border_right_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.border_right_color))
}

fn extract_border_bottom_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.border_bottom_color))
}

fn extract_border_left_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Color(style.border_left_color))
}

fn apply_border_top_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.border_top_color = *c;
  }
}

fn apply_border_right_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.border_right_color = *c;
  }
}

fn apply_border_bottom_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.border_bottom_color = *c;
  }
}

fn apply_border_left_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Color(c) = value {
    style.border_left_color = *c;
  }
}

fn resolve_border_widths(style: &ComputedStyle, ctx: &AnimationResolveContext) -> [Length; 4] {
  [
    Length::px(resolve_length_px(&style.border_top_width, None, style, ctx)),
    Length::px(resolve_length_px(
      &style.border_right_width,
      None,
      style,
      ctx,
    )),
    Length::px(resolve_length_px(
      &style.border_bottom_width,
      None,
      style,
      ctx,
    )),
    Length::px(resolve_length_px(
      &style.border_left_width,
      None,
      style,
      ctx,
    )),
  ]
}

fn extract_border_width(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::BorderWidth(resolve_border_widths(
    style, ctx,
  )))
}

fn interpolate_border_width_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BorderWidth(wa), AnimatedValue::BorderWidth(wb)) = (a, b) else {
    return None;
  };

  let mut out = [Length::px(0.0); 4];
  for i in 0..4 {
    out[i] = Length::px(lerp(wa[i].to_px(), wb[i].to_px(), t).max(0.0));
  }
  Some(AnimatedValue::BorderWidth(out))
}

fn apply_border_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderWidth(w) = value {
    style.border_top_width = w[0];
    style.border_right_width = w[1];
    style.border_bottom_width = w[2];
    style.border_left_width = w[3];
  }
}

fn apply_border_top_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderWidth(w) = value {
    style.border_top_width = w[0];
  }
}

fn apply_border_right_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderWidth(w) = value {
    style.border_right_width = w[1];
  }
}

fn apply_border_bottom_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderWidth(w) = value {
    style.border_bottom_width = w[2];
  }
}

fn apply_border_left_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderWidth(w) = value {
    style.border_left_width = w[3];
  }
}

fn extract_border_style(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::BorderStyle([
    style.border_top_style,
    style.border_right_style,
    style.border_bottom_style,
    style.border_left_style,
  ]))
}

fn interpolate_border_style_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BorderStyle(sa), AnimatedValue::BorderStyle(sb)) = (a, b) else {
    return None;
  };
  let _ = t;
  let _ = sa;
  let _ = sb;
  None
}

fn apply_border_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderStyle(styles) = value {
    style.border_top_style = styles[0];
    style.border_right_style = styles[1];
    style.border_bottom_style = styles[2];
    style.border_left_style = styles[3];
  }
}

fn apply_border_style_side(style: &mut ComputedStyle, value: &AnimatedValue, side: usize) {
  if let AnimatedValue::BorderStyle(styles) = value {
    match side {
      0 => style.border_top_style = styles[0],
      1 => style.border_right_style = styles[1],
      2 => style.border_bottom_style = styles[2],
      3 => style.border_left_style = styles[3],
      _ => {}
    }
  }
}

fn apply_border_top_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_style_side(style, value, 0);
}

fn apply_border_right_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_style_side(style, value, 1);
}

fn apply_border_bottom_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_style_side(style, value, 2);
}

fn apply_border_left_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_style_side(style, value, 3);
}

fn extract_border(style: &ComputedStyle, ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Border(
    resolve_border_widths(style, ctx),
    [
      style.border_top_style,
      style.border_right_style,
      style.border_bottom_style,
      style.border_left_style,
    ],
    [
      style.border_top_color,
      style.border_right_color,
      style.border_bottom_color,
      style.border_left_color,
    ],
  ))
}

fn interpolate_border_value(a: &AnimatedValue, b: &AnimatedValue, t: f32) -> Option<AnimatedValue> {
  let (AnimatedValue::Border(wa, sa, ca), AnimatedValue::Border(wb, sb, cb)) = (a, b) else {
    return None;
  };

  let mut widths = [Length::px(0.0); 4];
  let mut styles = [BorderStyle::None; 4];
  let mut colors = [Rgba::BLACK; 4];
  for i in 0..4 {
    widths[i] = Length::px(lerp(wa[i].to_px(), wb[i].to_px(), t).max(0.0));
    styles[i] = if t < 0.5 { sa[i] } else { sb[i] };
    colors[i] = lerp_color(ca[i], cb[i], t);
  }

  Some(AnimatedValue::Border(widths, styles, colors))
}

fn apply_border(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Border(widths, styles, colors) = value {
    style.border_top_width = widths[0];
    style.border_right_width = widths[1];
    style.border_bottom_width = widths[2];
    style.border_left_width = widths[3];
    style.border_top_style = styles[0];
    style.border_right_style = styles[1];
    style.border_bottom_style = styles[2];
    style.border_left_style = styles[3];
    style.border_top_color = colors[0];
    style.border_right_color = colors[1];
    style.border_bottom_color = colors[2];
    style.border_left_color = colors[3];
  }
}

fn apply_border_side(style: &mut ComputedStyle, value: &AnimatedValue, side: usize) {
  if let AnimatedValue::Border(widths, styles, colors) = value {
    match side {
      0 => {
        style.border_top_width = widths[0];
        style.border_top_style = styles[0];
        style.border_top_color = colors[0];
      }
      1 => {
        style.border_right_width = widths[1];
        style.border_right_style = styles[1];
        style.border_right_color = colors[1];
      }
      2 => {
        style.border_bottom_width = widths[2];
        style.border_bottom_style = styles[2];
        style.border_bottom_color = colors[2];
      }
      3 => {
        style.border_left_width = widths[3];
        style.border_left_style = styles[3];
        style.border_left_color = colors[3];
      }
      _ => {}
    }
  }
}

fn apply_border_top(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_side(style, value, 0);
}

fn apply_border_right(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_side(style, value, 1);
}

fn apply_border_bottom(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_side(style, value, 2);
}

fn apply_border_left(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_side(style, value, 3);
}

fn resolve_outline_color(style: &ComputedStyle) -> OutlineColor {
  match style.outline_color {
    OutlineColor::CurrentColor => OutlineColor::Color(style.color),
    other => other,
  }
}

fn extract_outline_color(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::OutlineColor(resolve_outline_color(style)))
}

fn interpolate_outline_color_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  match (a, b) {
    (
      AnimatedValue::OutlineColor(OutlineColor::Color(ca)),
      AnimatedValue::OutlineColor(OutlineColor::Color(cb)),
    ) => Some(AnimatedValue::OutlineColor(OutlineColor::Color(
      lerp_color(*ca, *cb, t),
    ))),
    (
      AnimatedValue::OutlineColor(OutlineColor::Invert),
      AnimatedValue::OutlineColor(OutlineColor::Invert),
    ) => Some(AnimatedValue::OutlineColor(OutlineColor::Invert)),
    _ => None,
  }
}

fn apply_outline_color(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::OutlineColor(color) = value {
    style.outline_color = *color;
  }
}

fn extract_outline_style(
  style: &ComputedStyle,
  _ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::OutlineStyle(style.outline_style))
}

fn apply_outline_style(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::OutlineStyle(style_value) = value {
    style.outline_style = *style_value;
  }
}

fn interpolate_outline_style_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  match (a, b) {
    (AnimatedValue::OutlineStyle(sa), AnimatedValue::OutlineStyle(sb)) => {
      let _ = t;
      let _ = sa;
      let _ = sb;
      None
    }
    _ => None,
  }
}

fn extract_outline_width(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Length(Length::px(resolve_length_px(
    &style.outline_width,
    None,
    style,
    ctx,
  ))))
}

fn extract_outline_offset(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  Some(AnimatedValue::Length(Length::px(resolve_length_px(
    &style.outline_offset,
    None,
    style,
    ctx,
  ))))
}

fn apply_outline_width(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Length(len) = value {
    style.outline_width = Length::px(len.to_px().max(0.0));
  }
}

fn apply_outline_offset(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Length(len) = value {
    style.outline_offset = *len;
  }
}

fn extract_outline(style: &ComputedStyle, ctx: &AnimationResolveContext) -> Option<AnimatedValue> {
  Some(AnimatedValue::Outline(
    resolve_outline_color(style),
    style.outline_style,
    Length::px(resolve_length_px(&style.outline_width, None, style, ctx)),
  ))
}

fn interpolate_outline_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::Outline(ca, sa, wa), AnimatedValue::Outline(cb, sb, wb)) = (a, b) else {
    return None;
  };

  let width = Length::px(lerp(wa.to_px(), wb.to_px(), t).max(0.0));
  let style = if t < 0.5 { *sa } else { *sb };

  let color = match (ca, cb) {
    (OutlineColor::Color(ca), OutlineColor::Color(cb)) => {
      OutlineColor::Color(lerp_color(*ca, *cb, t))
    }
    _ => {
      if t < 0.5 {
        *ca
      } else {
        *cb
      }
    }
  };

  Some(AnimatedValue::Outline(color, style, width))
}

fn apply_outline(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::Outline(color, outline_style, width) = value {
    style.outline_color = *color;
    style.outline_style = *outline_style;
    style.outline_width = *width;
  }
}

fn extract_border_radius(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<AnimatedValue> {
  let radii = resolve_border_radii(style, ctx);
  Some(AnimatedValue::BorderRadius(radii))
}

fn extract_border_corner(
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
  _idx: usize,
) -> Option<AnimatedValue> {
  extract_border_radius(style, ctx)
}

fn interpolate_border_radius_value(
  a: &AnimatedValue,
  b: &AnimatedValue,
  t: f32,
) -> Option<AnimatedValue> {
  let (AnimatedValue::BorderRadius(ra), AnimatedValue::BorderRadius(rb)) = (a, b) else {
    return None;
  };
  let mut out = [BorderCornerRadius::default(); 4];
  for i in 0..4 {
    out[i] = BorderCornerRadius {
      x: Length::px(lerp(ra[i].x.to_px(), rb[i].x.to_px(), t)),
      y: Length::px(lerp(ra[i].y.to_px(), rb[i].y.to_px(), t)),
    };
  }
  Some(AnimatedValue::BorderRadius(out))
}

fn apply_border_radius(style: &mut ComputedStyle, value: &AnimatedValue) {
  if let AnimatedValue::BorderRadius(r) = value {
    style.border_top_left_radius = r[0];
    style.border_top_right_radius = r[1];
    style.border_bottom_right_radius = r[2];
    style.border_bottom_left_radius = r[3];
  }
}

fn apply_border_corner(style: &mut ComputedStyle, value: &AnimatedValue, idx: usize) {
  if let AnimatedValue::BorderRadius(r) = value {
    match idx {
      0 => style.border_top_left_radius = r[0],
      1 => style.border_top_right_radius = r[1],
      2 => style.border_bottom_left_radius = r[3],
      3 => style.border_bottom_right_radius = r[2],
      _ => {}
    }
  }
}

fn apply_border_top_left_radius(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_corner(style, value, 0);
}

fn apply_border_top_right_radius(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_corner(style, value, 1);
}

fn apply_border_bottom_left_radius(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_corner(style, value, 2);
}

fn apply_border_bottom_right_radius(style: &mut ComputedStyle, value: &AnimatedValue) {
  apply_border_corner(style, value, 3);
}

fn property_interpolators() -> &'static [PropertyInterpolator] {
  static PROPS: &[PropertyInterpolator] = &[
    PropertyInterpolator {
      name: "opacity",
      extract: extract_opacity,
      interpolate: interpolate_opacity,
      apply: apply_opacity,
    },
    PropertyInterpolator {
      name: "visibility",
      extract: extract_visibility,
      interpolate: interpolate_visibility_value,
      apply: apply_visibility,
    },
    PropertyInterpolator {
      name: "color",
      extract: extract_color,
      interpolate: interpolate_color,
      apply: apply_color,
    },
    PropertyInterpolator {
      name: "background-color",
      extract: extract_background_color,
      interpolate: interpolate_color,
      apply: apply_background_color,
    },
    PropertyInterpolator {
      name: "transform",
      extract: extract_transform,
      interpolate: interpolate_transform_value,
      apply: apply_transform,
    },
    PropertyInterpolator {
      name: "translate",
      extract: extract_translate,
      interpolate: interpolate_translate_value,
      apply: apply_translate,
    },
    PropertyInterpolator {
      name: "rotate",
      extract: extract_rotate,
      interpolate: interpolate_rotate_value,
      apply: apply_rotate,
    },
    PropertyInterpolator {
      name: "scale",
      extract: extract_scale,
      interpolate: interpolate_scale_value,
      apply: apply_scale,
    },
    PropertyInterpolator {
      name: "offset-distance",
      extract: extract_offset_distance,
      interpolate: interpolate_offset_distance_value,
      apply: apply_offset_distance,
    },
    PropertyInterpolator {
      name: "offset-anchor",
      extract: extract_offset_anchor,
      interpolate: interpolate_offset_anchor_value,
      apply: apply_offset_anchor,
    },
    PropertyInterpolator {
      name: "offset-rotate",
      extract: extract_offset_rotate,
      interpolate: interpolate_offset_rotate_value,
      apply: apply_offset_rotate,
    },
    PropertyInterpolator {
      name: "filter",
      extract: extract_filter,
      interpolate: interpolate_filters_value,
      apply: apply_filter,
    },
    PropertyInterpolator {
      name: "backdrop-filter",
      extract: extract_backdrop_filter,
      interpolate: interpolate_filters_value,
      apply: apply_filter,
    },
    PropertyInterpolator {
      name: "clip-path",
      extract: extract_clip_path,
      interpolate: interpolate_clip_path_value,
      apply: apply_clip_path,
    },
    PropertyInterpolator {
      name: "clip",
      extract: extract_clip_rect,
      interpolate: interpolate_clip_rect_value,
      apply: apply_clip_rect,
    },
    PropertyInterpolator {
      name: "transform-origin",
      extract: extract_transform_origin,
      interpolate: interpolate_transform_origin_value,
      apply: apply_transform_origin,
    },
    PropertyInterpolator {
      name: "perspective-origin",
      extract: extract_perspective_origin,
      interpolate: interpolate_transform_origin_value,
      apply: apply_perspective_origin,
    },
    PropertyInterpolator {
      name: "background-position",
      extract: extract_background_position,
      interpolate: interpolate_background_position_value,
      apply: apply_background_position,
    },
    PropertyInterpolator {
      name: "mask-position",
      extract: extract_mask_position,
      interpolate: interpolate_background_position_value,
      apply: apply_mask_position,
    },
    PropertyInterpolator {
      name: "background-size",
      extract: extract_background_size,
      interpolate: interpolate_background_size_value,
      apply: apply_background_size,
    },
    PropertyInterpolator {
      name: "mask-size",
      extract: extract_mask_size,
      interpolate: interpolate_background_size_value,
      apply: apply_mask_size,
    },
    PropertyInterpolator {
      name: "box-shadow",
      extract: extract_box_shadow,
      interpolate: interpolate_box_shadow_value,
      apply: apply_box_shadow,
    },
    PropertyInterpolator {
      name: "text-shadow",
      extract: extract_text_shadow,
      interpolate: interpolate_text_shadow_value,
      apply: apply_text_shadow,
    },
    PropertyInterpolator {
      name: "border-color",
      extract: extract_border_color,
      interpolate: interpolate_border_color_value,
      apply: apply_border_color,
    },
    PropertyInterpolator {
      name: "border-top-color",
      extract: extract_border_top_color,
      interpolate: interpolate_color,
      apply: apply_border_top_color,
    },
    PropertyInterpolator {
      name: "border-right-color",
      extract: extract_border_right_color,
      interpolate: interpolate_color,
      apply: apply_border_right_color,
    },
    PropertyInterpolator {
      name: "border-bottom-color",
      extract: extract_border_bottom_color,
      interpolate: interpolate_color,
      apply: apply_border_bottom_color,
    },
    PropertyInterpolator {
      name: "border-left-color",
      extract: extract_border_left_color,
      interpolate: interpolate_color,
      apply: apply_border_left_color,
    },
    PropertyInterpolator {
      name: "border-width",
      extract: extract_border_width,
      interpolate: interpolate_border_width_value,
      apply: apply_border_width,
    },
    PropertyInterpolator {
      name: "border-style",
      extract: extract_border_style,
      interpolate: interpolate_border_style_value,
      apply: apply_border_style,
    },
    PropertyInterpolator {
      name: "border-top-style",
      extract: extract_border_style,
      interpolate: interpolate_border_style_value,
      apply: apply_border_top_style,
    },
    PropertyInterpolator {
      name: "border-right-style",
      extract: extract_border_style,
      interpolate: interpolate_border_style_value,
      apply: apply_border_right_style,
    },
    PropertyInterpolator {
      name: "border-bottom-style",
      extract: extract_border_style,
      interpolate: interpolate_border_style_value,
      apply: apply_border_bottom_style,
    },
    PropertyInterpolator {
      name: "border-left-style",
      extract: extract_border_style,
      interpolate: interpolate_border_style_value,
      apply: apply_border_left_style,
    },
    PropertyInterpolator {
      name: "border",
      extract: extract_border,
      interpolate: interpolate_border_value,
      apply: apply_border,
    },
    PropertyInterpolator {
      name: "border-top",
      extract: extract_border,
      interpolate: interpolate_border_value,
      apply: apply_border_top,
    },
    PropertyInterpolator {
      name: "border-right",
      extract: extract_border,
      interpolate: interpolate_border_value,
      apply: apply_border_right,
    },
    PropertyInterpolator {
      name: "border-bottom",
      extract: extract_border,
      interpolate: interpolate_border_value,
      apply: apply_border_bottom,
    },
    PropertyInterpolator {
      name: "border-left",
      extract: extract_border,
      interpolate: interpolate_border_value,
      apply: apply_border_left,
    },
    PropertyInterpolator {
      name: "border-top-width",
      extract: extract_border_width,
      interpolate: interpolate_border_width_value,
      apply: apply_border_top_width,
    },
    PropertyInterpolator {
      name: "border-right-width",
      extract: extract_border_width,
      interpolate: interpolate_border_width_value,
      apply: apply_border_right_width,
    },
    PropertyInterpolator {
      name: "border-bottom-width",
      extract: extract_border_width,
      interpolate: interpolate_border_width_value,
      apply: apply_border_bottom_width,
    },
    PropertyInterpolator {
      name: "border-left-width",
      extract: extract_border_width,
      interpolate: interpolate_border_width_value,
      apply: apply_border_left_width,
    },
    PropertyInterpolator {
      name: "outline-color",
      extract: extract_outline_color,
      interpolate: interpolate_outline_color_value,
      apply: apply_outline_color,
    },
    PropertyInterpolator {
      name: "outline-style",
      extract: extract_outline_style,
      interpolate: interpolate_outline_style_value,
      apply: apply_outline_style,
    },
    PropertyInterpolator {
      name: "outline",
      extract: extract_outline,
      interpolate: interpolate_outline_value,
      apply: apply_outline,
    },
    PropertyInterpolator {
      name: "outline-width",
      extract: extract_outline_width,
      interpolate: interpolate_length_value,
      apply: apply_outline_width,
    },
    PropertyInterpolator {
      name: "outline-offset",
      extract: extract_outline_offset,
      interpolate: interpolate_length_value,
      apply: apply_outline_offset,
    },
    PropertyInterpolator {
      name: "border-radius",
      extract: extract_border_radius,
      interpolate: interpolate_border_radius_value,
      apply: apply_border_radius,
    },
    PropertyInterpolator {
      name: "border-top-left-radius",
      extract: |s, c| extract_border_corner(s, c, 0),
      interpolate: interpolate_border_radius_value,
      apply: apply_border_top_left_radius,
    },
    PropertyInterpolator {
      name: "border-top-right-radius",
      extract: |s, c| extract_border_corner(s, c, 1),
      interpolate: interpolate_border_radius_value,
      apply: apply_border_top_right_radius,
    },
    PropertyInterpolator {
      name: "border-bottom-left-radius",
      extract: |s, c| extract_border_corner(s, c, 2),
      interpolate: interpolate_border_radius_value,
      apply: apply_border_bottom_left_radius,
    },
    PropertyInterpolator {
      name: "border-bottom-right-radius",
      extract: |s, c| extract_border_corner(s, c, 3),
      interpolate: interpolate_border_radius_value,
      apply: apply_border_bottom_right_radius,
    },
  ];
  PROPS
}

fn interpolator_for(name: &str) -> Option<&'static PropertyInterpolator> {
  property_interpolators().iter().find(|p| p.name == name)
}

fn resolve_transform_length(
  len: &Length,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
  base: f32,
) -> f32 {
  let reference = if base.is_finite() && base.abs() > f32::EPSILON {
    Some(base)
  } else {
    None
  };
  resolve_length_px(len, reference, style, ctx)
}

fn resolve_transform_list(
  list: &[crate::css::types::Transform],
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Vec<crate::css::types::Transform> {
  let (width, height) = transform_reference_size(style, ctx);
  list
    .iter()
    .map(|t| match t {
      crate::css::types::Transform::Translate(x, y) => crate::css::types::Transform::Translate(
        Length::px(resolve_transform_length(x, style, ctx, width)),
        Length::px(resolve_transform_length(y, style, ctx, height)),
      ),
      crate::css::types::Transform::TranslateX(x) => crate::css::types::Transform::TranslateX(
        Length::px(resolve_transform_length(x, style, ctx, width)),
      ),
      crate::css::types::Transform::TranslateY(y) => crate::css::types::Transform::TranslateY(
        Length::px(resolve_transform_length(y, style, ctx, height)),
      ),
      crate::css::types::Transform::TranslateZ(z) => crate::css::types::Transform::TranslateZ(
        Length::px(resolve_transform_length(z, style, ctx, width)),
      ),
      crate::css::types::Transform::Translate3d(x, y, z) => {
        crate::css::types::Transform::Translate3d(
          Length::px(resolve_transform_length(x, style, ctx, width)),
          Length::px(resolve_transform_length(y, style, ctx, height)),
          Length::px(resolve_transform_length(z, style, ctx, width)),
        )
      }
      crate::css::types::Transform::Scale(sx, sy) => crate::css::types::Transform::Scale(*sx, *sy),
      crate::css::types::Transform::ScaleX(sx) => crate::css::types::Transform::ScaleX(*sx),
      crate::css::types::Transform::ScaleY(sy) => crate::css::types::Transform::ScaleY(*sy),
      crate::css::types::Transform::ScaleZ(sz) => crate::css::types::Transform::ScaleZ(*sz),
      crate::css::types::Transform::Scale3d(sx, sy, sz) => {
        crate::css::types::Transform::Scale3d(*sx, *sy, *sz)
      }
      crate::css::types::Transform::Rotate(deg) | crate::css::types::Transform::RotateZ(deg) => {
        crate::css::types::Transform::Rotate(*deg)
      }
      crate::css::types::Transform::RotateX(deg) => crate::css::types::Transform::RotateX(*deg),
      crate::css::types::Transform::RotateY(deg) => crate::css::types::Transform::RotateY(*deg),
      crate::css::types::Transform::Rotate3d(x, y, z, deg) => {
        crate::css::types::Transform::Rotate3d(*x, *y, *z, *deg)
      }
      crate::css::types::Transform::SkewX(deg) => crate::css::types::Transform::SkewX(*deg),
      crate::css::types::Transform::SkewY(deg) => crate::css::types::Transform::SkewY(*deg),
      crate::css::types::Transform::Skew(ax, ay) => crate::css::types::Transform::Skew(*ax, *ay),
      crate::css::types::Transform::Perspective(len) => crate::css::types::Transform::Perspective(
        Length::px(resolve_transform_length(len, style, ctx, width)),
      ),
      crate::css::types::Transform::Matrix(a, b, c, d, e, f) => {
        crate::css::types::Transform::Matrix(*a, *b, *c, *d, *e, *f)
      }
      crate::css::types::Transform::Matrix3d(values) => {
        crate::css::types::Transform::Matrix3d(*values)
      }
    })
    .collect()
}

fn compose_transform_list(list: &[crate::css::types::Transform]) -> Transform3D {
  let mut ts = Transform3D::identity();
  const EPS: f32 = 1e-6;
  for component in list {
    let next = match component {
      crate::css::types::Transform::Translate(x, y) => {
        Transform3D::translate(x.to_px(), y.to_px(), 0.0)
      }
      crate::css::types::Transform::TranslateX(x) => Transform3D::translate(x.to_px(), 0.0, 0.0),
      crate::css::types::Transform::TranslateY(y) => Transform3D::translate(0.0, y.to_px(), 0.0),
      crate::css::types::Transform::TranslateZ(z) => Transform3D::translate(0.0, 0.0, z.to_px()),
      crate::css::types::Transform::Translate3d(x, y, z) => {
        Transform3D::translate(x.to_px(), y.to_px(), z.to_px())
      }
      crate::css::types::Transform::Scale(sx, sy) => Transform3D::scale(*sx, *sy, 1.0),
      crate::css::types::Transform::ScaleX(sx) => Transform3D::scale(*sx, 1.0, 1.0),
      crate::css::types::Transform::ScaleY(sy) => Transform3D::scale(1.0, *sy, 1.0),
      crate::css::types::Transform::ScaleZ(sz) => Transform3D::scale(1.0, 1.0, *sz),
      crate::css::types::Transform::Scale3d(sx, sy, sz) => Transform3D::scale(*sx, *sy, *sz),
      crate::css::types::Transform::Rotate(deg) | crate::css::types::Transform::RotateZ(deg) => {
        Transform3D::rotate_z(deg.to_radians())
      }
      crate::css::types::Transform::RotateX(deg) => Transform3D::rotate_x(deg.to_radians()),
      crate::css::types::Transform::RotateY(deg) => Transform3D::rotate_y(deg.to_radians()),
      crate::css::types::Transform::Rotate3d(x, y, z, deg) => {
        let len = (x * x + y * y + z * z).sqrt();
        if len < EPS {
          Transform3D::identity()
        } else {
          let ax = *x / len;
          let ay = *y / len;
          let az = *z / len;
          let angle = deg.to_radians();
          let (s, c) = angle.sin_cos();
          let t = 1.0 - c;

          // Rodrigues rotation formula (right-handed, column vectors) to match `Transform3D::rotate_*`.
          let m00 = t * ax * ax + c;
          let m01 = t * ax * ay - s * az;
          let m02 = t * ax * az + s * ay;
          let m10 = t * ax * ay + s * az;
          let m11 = t * ay * ay + c;
          let m12 = t * ay * az - s * ax;
          let m20 = t * ax * az - s * ay;
          let m21 = t * ay * az + s * ax;
          let m22 = t * az * az + c;

          Transform3D {
            m: [
              m00, m10, m20, 0.0, // column 1
              m01, m11, m21, 0.0, // column 2
              m02, m12, m22, 0.0, // column 3
              0.0, 0.0, 0.0, 1.0, // column 4
            ],
          }
        }
      }
      crate::css::types::Transform::SkewX(deg) => Transform3D::skew(deg.to_radians(), 0.0),
      crate::css::types::Transform::SkewY(deg) => Transform3D::skew(0.0, deg.to_radians()),
      crate::css::types::Transform::Skew(ax, ay) => {
        Transform3D::skew(ax.to_radians(), ay.to_radians())
      }
      crate::css::types::Transform::Perspective(len) => Transform3D::perspective(len.to_px()),
      crate::css::types::Transform::Matrix(a, b, c, d, e, f) => {
        Transform3D::from_2d(&Transform2D {
          a: *a,
          b: *b,
          c: *c,
          d: *d,
          e: *e,
          f: *f,
        })
      }
      crate::css::types::Transform::Matrix3d(values) => Transform3D { m: *values },
    };
    ts = ts.multiply(&next);
  }
  ts
}

fn lerp_matrix(a: &Transform3D, b: &Transform3D, t: f32) -> Transform3D {
  let mut m = [0.0; 16];
  for i in 0..16 {
    m[i] = lerp(a.m[i], b.m[i], t);
  }
  Transform3D { m }
}

fn interpolate_transform_lists(
  a: &[crate::css::types::Transform],
  b: &[crate::css::types::Transform],
  t: f32,
) -> Option<Vec<crate::css::types::Transform>> {
  if a.len() != b.len() {
    return None;
  }
  let mut out = Vec::with_capacity(a.len());
  for (ta, tb) in a.iter().zip(b.iter()) {
    if discriminant(ta) != discriminant(tb) {
      return None;
    }

    let next = match (ta, tb) {
      (
        crate::css::types::Transform::Translate(ax, ay),
        crate::css::types::Transform::Translate(bx, by),
      ) => {
        let x = lerp(ax.to_px(), bx.to_px(), t);
        let y = lerp(ay.to_px(), by.to_px(), t);
        crate::css::types::Transform::Translate(Length::px(x), Length::px(y))
      }
      (
        crate::css::types::Transform::TranslateX(ax),
        crate::css::types::Transform::TranslateX(bx),
      ) => crate::css::types::Transform::TranslateX(Length::px(lerp(ax.to_px(), bx.to_px(), t))),
      (
        crate::css::types::Transform::TranslateY(ay),
        crate::css::types::Transform::TranslateY(by),
      ) => crate::css::types::Transform::TranslateY(Length::px(lerp(ay.to_px(), by.to_px(), t))),
      (
        crate::css::types::Transform::TranslateZ(az),
        crate::css::types::Transform::TranslateZ(bz),
      ) => crate::css::types::Transform::TranslateZ(Length::px(lerp(az.to_px(), bz.to_px(), t))),
      (
        crate::css::types::Transform::Translate3d(ax, ay, az),
        crate::css::types::Transform::Translate3d(bx, by, bz),
      ) => crate::css::types::Transform::Translate3d(
        Length::px(lerp(ax.to_px(), bx.to_px(), t)),
        Length::px(lerp(ay.to_px(), by.to_px(), t)),
        Length::px(lerp(az.to_px(), bz.to_px(), t)),
      ),
      (
        crate::css::types::Transform::Scale(ax, ay),
        crate::css::types::Transform::Scale(bx, by),
      ) => crate::css::types::Transform::Scale(lerp(*ax, *bx, t), lerp(*ay, *by, t)),
      (crate::css::types::Transform::ScaleX(ax), crate::css::types::Transform::ScaleX(bx)) => {
        crate::css::types::Transform::ScaleX(lerp(*ax, *bx, t))
      }
      (crate::css::types::Transform::ScaleY(ay), crate::css::types::Transform::ScaleY(by)) => {
        crate::css::types::Transform::ScaleY(lerp(*ay, *by, t))
      }
      (crate::css::types::Transform::ScaleZ(az), crate::css::types::Transform::ScaleZ(bz)) => {
        crate::css::types::Transform::ScaleZ(lerp(*az, *bz, t))
      }
      (
        crate::css::types::Transform::Scale3d(ax, ay, az),
        crate::css::types::Transform::Scale3d(bx, by, bz),
      ) => crate::css::types::Transform::Scale3d(
        lerp(*ax, *bx, t),
        lerp(*ay, *by, t),
        lerp(*az, *bz, t),
      ),
      (crate::css::types::Transform::Rotate(ax), crate::css::types::Transform::Rotate(bx))
      | (crate::css::types::Transform::RotateZ(ax), crate::css::types::Transform::RotateZ(bx)) => {
        crate::css::types::Transform::Rotate(lerp(*ax, *bx, t))
      }
      (crate::css::types::Transform::RotateX(ax), crate::css::types::Transform::RotateX(bx)) => {
        crate::css::types::Transform::RotateX(lerp(*ax, *bx, t))
      }
      (crate::css::types::Transform::RotateY(ay), crate::css::types::Transform::RotateY(by)) => {
        crate::css::types::Transform::RotateY(lerp(*ay, *by, t))
      }
      (
        crate::css::types::Transform::Rotate3d(ax, ay, az, aa),
        crate::css::types::Transform::Rotate3d(bx, by, bz, ba),
      ) => crate::css::types::Transform::Rotate3d(
        lerp(*ax, *bx, t),
        lerp(*ay, *by, t),
        lerp(*az, *bz, t),
        lerp(*aa, *ba, t),
      ),
      (crate::css::types::Transform::SkewX(ax), crate::css::types::Transform::SkewX(bx)) => {
        crate::css::types::Transform::SkewX(lerp(*ax, *bx, t))
      }
      (crate::css::types::Transform::SkewY(ay), crate::css::types::Transform::SkewY(by)) => {
        crate::css::types::Transform::SkewY(lerp(*ay, *by, t))
      }
      (crate::css::types::Transform::Skew(ax, ay), crate::css::types::Transform::Skew(bx, by)) => {
        crate::css::types::Transform::Skew(lerp(*ax, *bx, t), lerp(*ay, *by, t))
      }
      (
        crate::css::types::Transform::Perspective(pa),
        crate::css::types::Transform::Perspective(pb),
      ) => crate::css::types::Transform::Perspective(Length::px(lerp(pa.to_px(), pb.to_px(), t))),
      _ => return None,
    };

    out.push(next);
  }

  Some(out)
}

fn axis_is_horizontal(axis: TimelineAxis, writing_mode: WritingMode) -> bool {
  match axis {
    TimelineAxis::X => true,
    TimelineAxis::Y => false,
    TimelineAxis::Inline => inline_axis_is_horizontal(writing_mode),
    TimelineAxis::Block => crate::style::block_axis_is_horizontal(writing_mode),
  }
}

fn axis_is_positive(axis: TimelineAxis, writing_mode: WritingMode, direction: Direction) -> bool {
  let inline_horizontal = inline_axis_is_horizontal(writing_mode);
  match axis {
    TimelineAxis::Inline => crate::style::inline_axis_positive(writing_mode, direction),
    TimelineAxis::Block => crate::style::block_axis_positive(writing_mode),
    TimelineAxis::X => {
      if inline_horizontal {
        crate::style::inline_axis_positive(writing_mode, direction)
      } else {
        crate::style::block_axis_positive(writing_mode)
      }
    }
    TimelineAxis::Y => {
      if inline_horizontal {
        crate::style::block_axis_positive(writing_mode)
      } else {
        crate::style::inline_axis_positive(writing_mode, direction)
      }
    }
  }
}

fn resolve_offset_value(
  offset: &TimelineOffset,
  scroll_range: f32,
  viewport_size: f32,
  is_end: bool,
) -> f32 {
  match offset {
    TimelineOffset::Auto => {
      if is_end {
        scroll_range.max(0.0)
      } else {
        0.0
      }
    }
    TimelineOffset::Length(len) => len
      .resolve_against(scroll_range)
      .unwrap_or_else(|| len.to_px().max(0.0)),
    TimelineOffset::Percentage(pct) => (pct / 100.0) * scroll_range,
  }
  .clamp(0.0, scroll_range.max(0.0).max(viewport_size))
}

#[derive(Clone, Copy)]
struct ViewTimelineRangeEdges {
  entry: f32,
  contain: f32,
  cover: f32,
  exit: f32,
}

fn resolve_progress_offset(
  offset: &RangeOffset,
  base_start: f32,
  base_end: f32,
  view_ranges: Option<ViewTimelineRangeEdges>,
) -> f32 {
  match offset {
    RangeOffset::Progress(p) => base_start + (base_end - base_start) * *p,
    RangeOffset::Length(len) => {
      let range = base_end - base_start;
      let resolved = len.resolve_against(range).unwrap_or_else(|| len.to_px());
      base_start + resolved
    }
    RangeOffset::View(phase, adj) => {
      let Some(ranges) = view_ranges else {
        return base_start;
      };
      let contain_start = ranges.cover.min(ranges.contain);
      let contain_end = ranges.cover.max(ranges.contain);
      let (range_start, range_end) = match phase {
        ViewTimelinePhase::Cover => (ranges.entry, ranges.exit),
        ViewTimelinePhase::Contain => (contain_start, contain_end),
        ViewTimelinePhase::Entry => (ranges.entry, contain_start),
        ViewTimelinePhase::Exit => (contain_end, ranges.exit),
        ViewTimelinePhase::EntryCrossing => (ranges.entry, ranges.contain),
        ViewTimelinePhase::ExitCrossing => (ranges.cover, ranges.exit),
      };
      let range_len = range_end - range_start;
      let adjustment = adj
        .resolve_against(range_len)
        .unwrap_or_else(|| adj.to_px());
      range_start + adjustment
    }
  }
}

fn clamp_progress(value: f32) -> f32 {
  value.clamp(0.0, 1.0)
}

fn raw_progress(position: f32, start: f32, end: f32) -> f32 {
  if (end - start).abs() < f32::EPSILON {
    if position >= end {
      1.0
    } else {
      0.0
    }
  } else {
    (position - start) / (end - start)
  }
}

/// Computes progress for a scroll timeline.
///
/// `scroll_position` should be measured from the scroll origin along the selected axis. Use
/// [`axis_scroll_state`] to derive an origin-corrected scroll offset when working with physical
/// scroll offsets (`scrollLeft`/`scrollTop`).
pub fn scroll_timeline_progress(
  timeline: &ScrollTimeline,
  scroll_position: f32,
  scroll_range: f32,
  viewport_size: f32,
  range: &AnimationRange,
) -> Option<f32> {
  if scroll_range.abs() < f32::EPSILON {
    return None;
  }
  let start_base = resolve_offset_value(&timeline.start, scroll_range, viewport_size, false);
  let end_base = resolve_offset_value(&timeline.end, scroll_range, viewport_size, true);
  let start = resolve_progress_offset(range.start(), start_base, end_base, None);
  let end = resolve_progress_offset(range.end(), start_base, end_base, None);
  Some(raw_progress(scroll_position, start, end))
}

fn view_timeline_attachment_range(
  timeline: &ViewTimeline,
  target_start: f32,
  target_end: f32,
  view_size: f32,
  range: &AnimationRange,
) -> Option<(f32, f32, ViewTimelineRangeEdges)> {
  // Degenerate geometries produce an inactive timeline.
  if !view_size.is_finite()
    || view_size <= f32::EPSILON
    || !target_start.is_finite()
    || !target_end.is_finite()
    || (target_end - target_start).abs() <= f32::EPSILON
  {
    return None;
  }

  let view_size = view_size.max(0.0);
  let inset = timeline.inset.unwrap_or_default();
  let resolve_inset_value = |len: Length| -> Option<f32> {
    let resolved = len
      .resolve_against(view_size)
      .unwrap_or_else(|| len.to_px());
    resolved.is_finite().then_some(resolved)
  };
  let inset_start_len = inset.start.unwrap_or(Length::px(0.0));
  let inset_end_len = inset.end.unwrap_or(Length::px(0.0));
  let inset_start = resolve_inset_value(inset_start_len)?;
  let inset_end = resolve_inset_value(inset_end_len)?;

  let entry_edge = target_start - view_size + inset_end;
  let cover_edge = target_start - inset_start;
  let contain_edge = target_end - view_size + inset_end;
  let exit_edge = target_end - inset_start;
  let start_base = entry_edge;
  let end_base = exit_edge;
  let phases = ViewTimelineRangeEdges {
    entry: entry_edge,
    contain: contain_edge,
    cover: cover_edge,
    exit: exit_edge,
  };
  let start = resolve_progress_offset(range.start(), start_base, end_base, Some(phases));
  let end = resolve_progress_offset(range.end(), start_base, end_base, Some(phases));
  Some((start, end, phases))
}

/// Computes view timeline progress using the target position relative to the
/// containing scroll port.
///
/// Inputs are measured along the timeline axis in a coordinate system whose origin is the scroll
/// origin. For writing modes where the scroll origin is reversed, callers should flip the inputs
/// accordingly.
pub fn view_timeline_progress(
  timeline: &ViewTimeline,
  target_start: f32,
  target_end: f32,
  view_size: f32,
  scroll_offset: f32,
  range: &AnimationRange,
) -> Option<f32> {
  if !scroll_offset.is_finite() {
    return None;
  }
  let (start, end, _) =
    view_timeline_attachment_range(timeline, target_start, target_end, view_size, range)?;
  Some(raw_progress(scroll_offset, start, end))
}

/// Determines the scroll position and range along the requested axis given
/// container and content sizes.
///
/// The returned position is measured from the *scroll origin*, which can flip depending on
/// `writing-mode` and `direction` (even for physical axes like `x`/`y`).
///
/// The returned tuple is `(position_from_origin, range, viewport_size_along_axis)`.
pub fn axis_scroll_state(
  axis: TimelineAxis,
  writing_mode: WritingMode,
  direction: Direction,
  scroll_x: f32,
  scroll_y: f32,
  view_width: f32,
  view_height: f32,
  content_width: f32,
  content_height: f32,
) -> (f32, f32, f32) {
  let sanitize = |value: f32| -> f32 {
    if value.is_finite() {
      value.max(0.0)
    } else {
      0.0
    }
  };

  // Scroll timelines measure offsets from the scroll origin. The scroll origin can flip depending
  // on writing-mode/direction, even for physical axes (x/y). See the note in Scroll Animations 1
  // under the `scroll()` notation.
  let axis_positive = axis_is_positive(axis, writing_mode, direction);

  let horizontal = axis_is_horizontal(axis, writing_mode);
  if horizontal {
    let view_width = sanitize(view_width);
    let content_width = sanitize(content_width);
    let range = (content_width - view_width).max(0.0);
    let mut pos = sanitize(scroll_x).min(range);
    if !axis_positive {
      pos = range - pos;
    }
    (pos.clamp(0.0, range), range, view_width)
  } else {
    let view_height = sanitize(view_height);
    let content_height = sanitize(content_height);
    let range = (content_height - view_height).max(0.0);
    let mut pos = sanitize(scroll_y).min(range);
    if !axis_positive {
      pos = range - pos;
    }
    (pos.clamp(0.0, range), range, view_height)
  }
}

fn axis_view_state(
  axis: TimelineAxis,
  writing_mode: WritingMode,
  direction: Direction,
  target_start: f32,
  target_end: f32,
  scroll_pos: f32,
  view_size: f32,
  content_size: f32,
) -> (f32, f32, f32, f32) {
  let sanitize_non_negative = |value: f32| -> f32 {
    if value.is_finite() {
      value.max(0.0)
    } else {
      0.0
    }
  };
  let sanitize = |value: f32| -> f32 {
    if value.is_finite() {
      value
    } else {
      0.0
    }
  };

  // View timelines are also defined relative to the scroll origin along the selected axis.
  // Match the origin flipping rules used by scroll timelines so `view-timeline-axis: x/y`
  // is consistent with `inline/block` in RTL/vertical writing modes.
  let axis_positive = axis_is_positive(axis, writing_mode, direction);
  let target_start = sanitize(target_start);
  let target_end = sanitize(target_end);
  let view_size = sanitize_non_negative(view_size);
  let content_size = sanitize_non_negative(content_size);
  let scroll_pos = sanitize_non_negative(scroll_pos);
  let range = (content_size - view_size).max(0.0);

  if axis_positive {
    (target_start, target_end, view_size, scroll_pos)
  } else {
    let flipped_scroll = range - scroll_pos;
    let flipped_start = content_size - target_end;
    let flipped_end = content_size - target_start;
    (flipped_start, flipped_end, view_size, flipped_scroll)
  }
}

/// Samples a @keyframes rule at the given progress, returning a property map of
/// interpolated computed values.
pub fn sample_keyframes(
  rule: &KeyframesRule,
  progress: f32,
  base_style: &ComputedStyle,
  viewport: Size,
  element_size: Size,
) -> HashMap<String, AnimatedValue> {
  let default_timing = TransitionTimingFunction::Linear;
  sample_keyframes_with_default_timing(
    rule,
    progress,
    base_style,
    viewport,
    element_size,
    &default_timing,
    None,
  )
  .animated
}

fn parse_first_timing_function(value: &str) -> Option<TransitionTimingFunction> {
  let first = split_top_level_commas(value).into_iter().next()?;
  parse_transition_timing_function(&first)
}

#[derive(Default)]
struct SampledKeyframes {
  animated: HashMap<String, AnimatedValue>,
  custom_properties: Vec<(Arc<str>, Option<CustomPropertyValue>)>,
}

#[derive(Clone, Copy)]
struct ViewTimelineKeyframeResolver {
  attachment_start: f32,
  attachment_end: f32,
  view_ranges: ViewTimelineRangeEdges,
}

fn view_timeline_phase_for_named_range(name: &str) -> Option<ViewTimelinePhase> {
  if name.eq_ignore_ascii_case("cover") {
    Some(ViewTimelinePhase::Cover)
  } else if name.eq_ignore_ascii_case("contain") {
    Some(ViewTimelinePhase::Contain)
  } else if name.eq_ignore_ascii_case("entry") {
    Some(ViewTimelinePhase::Entry)
  } else if name.eq_ignore_ascii_case("exit") {
    Some(ViewTimelinePhase::Exit)
  } else if name.eq_ignore_ascii_case("entry-crossing") {
    Some(ViewTimelinePhase::EntryCrossing)
  } else if name.eq_ignore_ascii_case("exit-crossing") {
    Some(ViewTimelinePhase::ExitCrossing)
  } else {
    None
  }
}

fn resolve_view_timeline_keyframe_offset(
  resolver: ViewTimelineKeyframeResolver,
  range_name: &str,
  range_progress: f32,
) -> Option<f32> {
  let phase = view_timeline_phase_for_named_range(range_name)?;
  if !range_progress.is_finite() {
    return None;
  }
  let range_progress = range_progress.clamp(0.0, 1.0);
  let keyframe_position = resolve_progress_offset(
    &RangeOffset::View(phase, Length::percent(range_progress * 100.0)),
    resolver.view_ranges.entry,
    resolver.view_ranges.exit,
    Some(resolver.view_ranges),
  );
  let offset = raw_progress(
    keyframe_position,
    resolver.attachment_start,
    resolver.attachment_end,
  );
  offset.is_finite().then_some(offset)
}

fn expanded_properties_for_keyframe_sampling(property: &str) -> Option<&'static [&'static str]> {
  const BORDER_WIDTH: [&str; 4] = [
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
  ];
  const BORDER_STYLE: [&str; 4] = [
    "border-top-style",
    "border-right-style",
    "border-bottom-style",
    "border-left-style",
  ];
  const BORDER_COLOR: [&str; 4] = [
    "border-top-color",
    "border-right-color",
    "border-bottom-color",
    "border-left-color",
  ];
  const BORDER: [&str; 12] = [
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
    "border-top-style",
    "border-right-style",
    "border-bottom-style",
    "border-left-style",
    "border-top-color",
    "border-right-color",
    "border-bottom-color",
    "border-left-color",
  ];
  const BORDER_TOP: [&str; 3] = ["border-top-width", "border-top-style", "border-top-color"];
  const BORDER_RIGHT: [&str; 3] = ["border-right-width", "border-right-style", "border-right-color"];
  const BORDER_BOTTOM: [&str; 3] = [
    "border-bottom-width",
    "border-bottom-style",
    "border-bottom-color",
  ];
  const BORDER_LEFT: [&str; 3] = ["border-left-width", "border-left-style", "border-left-color"];
  const BORDER_RADIUS: [&str; 4] = [
    "border-top-left-radius",
    "border-top-right-radius",
    "border-bottom-right-radius",
    "border-bottom-left-radius",
  ];
  const OUTLINE: [&str; 3] = ["outline-color", "outline-style", "outline-width"];

  match property {
    "border" => Some(&BORDER),
    "border-top" => Some(&BORDER_TOP),
    "border-right" => Some(&BORDER_RIGHT),
    "border-bottom" => Some(&BORDER_BOTTOM),
    "border-left" => Some(&BORDER_LEFT),
    "border-color" => Some(&BORDER_COLOR),
    "border-width" => Some(&BORDER_WIDTH),
    "border-style" => Some(&BORDER_STYLE),
    "border-radius" => Some(&BORDER_RADIUS),
    "outline" => Some(&OUTLINE),
    _ => None,
  }
}

fn sample_keyframes_with_default_timing(
  rule: &KeyframesRule,
  progress: f32,
  base_style: &ComputedStyle,
  viewport: Size,
  element_size: Size,
  default_timing_function: &TransitionTimingFunction,
  view_timeline_keyframe_resolver: Option<ViewTimelineKeyframeResolver>,
) -> SampledKeyframes {
  if rule.keyframes.is_empty() {
    return SampledKeyframes::default();
  }
  let mut frames: Vec<(f32, &Keyframe)> = Vec::new();
  for frame in &rule.keyframes {
    let offset = match &frame.selector {
      KeyframeSelector::Offset(offset) => Some(*offset),
      KeyframeSelector::TimelineRange { name, progress } => view_timeline_keyframe_resolver
        .and_then(|resolver| resolve_view_timeline_keyframe_offset(resolver, name, *progress)),
    };
    if let Some(offset) = offset.filter(|offset| offset.is_finite()) {
      frames.push((offset, frame));
    }
  }
  if frames.is_empty() {
    return SampledKeyframes::default();
  }

  frames.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
  let progress = clamp_progress(progress);
  let defaults = ComputedStyle::default();
  let mut groups: Vec<(f32, Vec<&Keyframe>)> = Vec::new();
  for (offset, frame) in frames.iter().copied() {
    match groups.last_mut() {
      Some((group_offset, list)) if (*group_offset - offset).abs() <= f32::EPSILON => {
        list.push(frame)
      }
      _ => groups.push((offset, vec![frame])),
    }
  }

  let mut resolved_styles = Vec::with_capacity(groups.len());
  let mut timing_functions = Vec::with_capacity(groups.len());
  for (_, group_frames) in &groups {
    let mut style = base_style.clone();
    let is_dark_color_scheme = base_style.used_dark_color_scheme;

    for frame in group_frames {
      for decl in &frame.declarations {
        if !decl.property.is_custom() {
          continue;
        }
        apply_declaration_with_base(
          &mut style,
          decl,
          base_style,
          &defaults,
          None,
          base_style.font_size,
          base_style.root_font_size,
          viewport,
          is_dark_color_scheme,
        );
      }
    }

    let mut keyframe_timing_function: Option<TransitionTimingFunction> = None;
    for frame in group_frames {
      for decl in &frame.timing_functions {
        let PropertyValue::Keyword(raw_value) = &decl.value else {
          continue;
        };

        let resolved_css = if decl.contains_var {
          match resolve_var_for_property(
            &decl.value,
            &style.custom_properties,
            "animation-timing-function",
          ) {
            VarResolutionResult::Resolved { css_text, .. } => {
              if css_text.is_empty() {
                raw_value.clone()
              } else {
                css_text.into_owned()
              }
            }
            _ => continue,
          }
        } else {
          raw_value.clone()
        };

        if let Some(parsed) = parse_first_timing_function(&resolved_css) {
          keyframe_timing_function = Some(parsed);
        }
      }
    }

    for frame in group_frames {
      for decl in &frame.declarations {
        if decl.property.is_custom() {
          continue;
        }
        apply_declaration_with_base(
          &mut style,
          decl,
          base_style,
          &defaults,
          None,
          base_style.font_size,
          base_style.root_font_size,
          viewport,
          is_dark_color_scheme,
        );
      }
    }

    timing_functions
      .push(keyframe_timing_function.unwrap_or_else(|| default_timing_function.clone()));
    resolved_styles.push(style);
  }

  let ctx = AnimationResolveContext::new(viewport, element_size);
  let mut group_properties: Vec<FxHashSet<&str>> = Vec::with_capacity(groups.len());
  let mut properties: FxHashSet<&str> = FxHashSet::default();
  for (_, group_frames) in &groups {
    let mut group_set: FxHashSet<&str> = FxHashSet::default();
    for frame in group_frames {
      for decl in &frame.declarations {
        let prop = decl.property.as_str();
        if decl.property.is_custom() {
          group_set.insert(prop);
          properties.insert(prop);
          continue;
        }
        if let Some(expanded) = expanded_properties_for_keyframe_sampling(prop) {
          for &longhand in expanded {
            group_set.insert(longhand);
            properties.insert(longhand);
          }
          continue;
        }
        group_set.insert(prop);
        properties.insert(prop);
      }
    }
    group_properties.push(group_set);
  }

  let mut result = HashMap::new();
  let mut custom_properties = Vec::new();
  for prop in properties {
    let mut prev_idx = None;
    for (idx, (offset, _)) in groups.iter().enumerate() {
      if *offset <= progress + f32::EPSILON {
        if group_properties[idx].contains(prop) {
          prev_idx = Some(idx);
        }
      } else {
        break;
      }
    }

    let mut next_idx = None;
    for (idx, (offset, _)) in groups.iter().enumerate() {
      if !group_properties[idx].contains(prop) {
        continue;
      }
      if *offset + f32::EPSILON < progress {
        continue;
      }
      next_idx = Some(idx);
      break;
    }

    let start = prev_idx.map(|idx| groups[idx].0).unwrap_or(0.0);
    let end = next_idx.map(|idx| groups[idx].0).unwrap_or(1.0);
    let local_t = if end - start > f32::EPSILON {
      clamp_progress((progress - start) / (end - start))
    } else {
      0.0
    };
    let eased_t = match prev_idx {
      Some(idx) => timing_functions[idx].value_at(local_t),
      None => default_timing_function.value_at(local_t),
    };

    let from_style = prev_idx
      .map(|idx| &resolved_styles[idx])
      .unwrap_or(base_style);
    let to_style = next_idx
      .map(|idx| &resolved_styles[idx])
      .unwrap_or(base_style);

    if prop.starts_with("--") {
      let from = from_style.custom_properties.get(prop);
      let to = to_style.custom_properties.get(prop);

      let can_interpolate = match (
        from_style.custom_property_registry.get(prop),
        to_style.custom_property_registry.get(prop),
      ) {
        (Some(from_rule), Some(to_rule))
          if from_rule.syntax == to_rule.syntax
            && !from_rule.syntax.is_universal() =>
        {
          true
        }
        _ => false,
      };

      let interpolated = if can_interpolate {
        match (from, to) {
          (Some(from), Some(to)) => {
            interpolate_custom_property(from, to, eased_t, from_style, to_style, &ctx)
          }
          _ => None,
        }
      } else {
        None
      };
      let sampled = interpolated.or_else(|| {
        let chosen = if eased_t >= 0.5 { to } else { from };
        chosen.cloned()
      });
      custom_properties.push((Arc::from(prop), sampled));
      continue;
    }

    let Some(interpolator) = interpolator_for(prop) else {
      continue;
    };

    let Some(from_val) = (interpolator.extract)(from_style, &ctx) else {
      continue;
    };
    let Some(to_val) = (interpolator.extract)(to_style, &ctx) else {
      continue;
    };
    let value = (interpolator.interpolate)(&from_val, &to_val, eased_t).or_else(|| {
      if eased_t >= 0.5 {
        Some(to_val.clone())
      } else {
        Some(from_val.clone())
      }
    });
    if let Some(v) = value {
      result.insert(prop.to_string(), v);
    }
  }

  SampledKeyframes {
    animated: result,
    custom_properties,
  }
}

/// Applies animated property values to the computed style.
pub fn apply_animated_properties(
  style: &mut ComputedStyle,
  values: &HashMap<String, AnimatedValue>,
) {
  for (name, value) in values {
    if let Some(interpolator) = interpolator_for(name) {
      (interpolator.apply)(style, value);
    }
  }
}

fn apply_animated_properties_ordered(style: &mut ComputedStyle, values: &[(String, AnimatedValue)]) {
  for (name, value) in values {
    if let Some(interpolator) = interpolator_for(name) {
      (interpolator.apply)(style, value);
    }
  }
}

fn apply_animated_properties_with_composition(
  style: &mut ComputedStyle,
  values: &HashMap<String, AnimatedValue>,
  composition: AnimationComposition,
  ctx: &AnimationResolveContext,
) {
  let composition = match composition {
    // Per-iteration accumulation semantics are applied during keyframe sampling. At this stage,
    // `accumulate` behaves like additive composition against the underlying value.
    AnimationComposition::Accumulate => AnimationComposition::Add,
    other => other,
  };

  if matches!(composition, AnimationComposition::Replace) {
    apply_animated_properties(style, values);
    return;
  }

  for (name, value) in values {
    if apply_additive_animation_value(style, name, value, ctx) {
      continue;
    }
    if let Some(interpolator) = interpolator_for(name) {
      (interpolator.apply)(style, value);
    }
  }
}

fn apply_additive_animation_value(
  style: &mut ComputedStyle,
  property: &str,
  value: &AnimatedValue,
  ctx: &AnimationResolveContext,
) -> bool {
  match (property, value) {
    ("transform", AnimatedValue::Transform(effect_list)) => {
      // `transform: none` is represented as an empty list and should be treated as a no-op for
      // additive composition. Converting it into an explicit identity matrix would incorrectly
      // establish transform containing blocks/stacking contexts.
      if effect_list.is_empty() {
        return true;
      }
      let underlying_list = resolve_transform_list(&style.transform, style, ctx);
      let underlying = compose_transform_list(&underlying_list);
      let effect = compose_transform_list(effect_list);
      let combined = underlying.multiply(&effect);
      style.transform = vec![crate::css::types::Transform::Matrix3d(combined.m)];
      return true;
    }
    ("translate", AnimatedValue::Translate(effect)) => {
      // `translate: none` is an identity translation and should not force the property into the
      // value form (which would incorrectly establish transform containing blocks).
      if matches!(effect, TranslateValue::None) {
        return true;
      }
      let Some(AnimatedValue::Translate(underlying)) = extract_translate(style, ctx) else {
        return false;
      };

      let (ux, uy, uz) = match underlying {
        TranslateValue::None => (0.0, 0.0, 0.0),
        TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
      };
      let (ex, ey, ez) = match effect {
        TranslateValue::None => (0.0, 0.0, 0.0),
        TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
      };

      style.translate = TranslateValue::Values {
        x: Length::px(ux + ex),
        y: Length::px(uy + ey),
        z: Length::px(uz + ez),
      };
      return true;
    }
    ("rotate", AnimatedValue::Rotate(effect)) => {
      // `rotate: none` is an identity rotation and should not force the property into the angle
      // form (which would incorrectly establish transform containing blocks).
      if matches!(effect, RotateValue::None) {
        return true;
      }

      let underlying = style.rotate;
      if matches!(underlying, RotateValue::None) {
        style.rotate = *effect;
        return true;
      }

      fn axis_angle(value: RotateValue) -> Option<((f32, f32, f32), f32)> {
        let (x, y, z, angle) = match value {
          RotateValue::None => return None,
          RotateValue::Angle(angle) => (0.0, 0.0, 1.0, angle),
          RotateValue::AxisAngle { x, y, z, angle } => (x, y, z, angle),
        };
        if !x.is_finite() || !y.is_finite() || !z.is_finite() || !angle.is_finite() {
          return None;
        }
        let len = (x * x + y * y + z * z).sqrt();
        if !len.is_finite() || len < 1e-6 {
          return None;
        }
        Some(((x / len, y / len, z / len), angle))
      }

      let Some(((ax, ay, az), a_angle)) = axis_angle(underlying) else {
        return false;
      };
      let Some(((mut bx, mut by, mut bz), mut b_angle)) = axis_angle(*effect) else {
        return false;
      };

      let dot = ax * bx + ay * by + az * bz;
      // Axis-angle has a sign ambiguity: (a, θ) == (-a, -θ). Flip the effect axis so we can
      // compare and add angles in a canonical direction.
      if dot < 0.0 {
        bx = -bx;
        by = -by;
        bz = -bz;
        b_angle = -b_angle;
      }

      if (ax - bx).abs() > 1e-6 || (ay - by).abs() > 1e-6 || (az - bz).abs() > 1e-6 {
        return false;
      }

      let angle = a_angle + b_angle;
      if ax.abs() < 1e-6 && ay.abs() < 1e-6 {
        style.rotate = RotateValue::Angle(angle * az.signum());
      } else {
        style.rotate = RotateValue::AxisAngle {
          x: ax,
          y: ay,
          z: az,
          angle,
        };
      }
      return true;
    }
    ("scale", AnimatedValue::Scale(effect)) => {
      // `scale: none` is an identity scaling and should not force the property into the numeric
      // form (which would incorrectly establish transform containing blocks).
      if matches!(effect, ScaleValue::None) {
        return true;
      }

      let (ux, uy, uz) = match style.scale {
        ScaleValue::None => (1.0, 1.0, 1.0),
        ScaleValue::Values { x, y, z } => (x, y, z),
      };
      let (ex, ey, ez) = match *effect {
        ScaleValue::None => (1.0, 1.0, 1.0),
        ScaleValue::Values { x, y, z } => (x, y, z),
      };

      style.scale = ScaleValue::Values {
        x: ux * ex,
        y: uy * ey,
        z: uz * ez,
      };
      return true;
    }
    ("opacity", AnimatedValue::Opacity(effect)) => {
      style.opacity = clamp_progress(style.opacity + effect);
      return true;
    }
    _ => {}
  }

  let Some(interpolator) = interpolator_for(property) else {
    return false;
  };
  let Some(underlying) = (interpolator.extract)(style, ctx) else {
    return false;
  };

  let combined = match (&underlying, value) {
    (AnimatedValue::Color(under), AnimatedValue::Color(effect)) => {
      if !under.a.is_finite() || !effect.a.is_finite() {
        None
      } else {
        Some(AnimatedValue::Color(add_color(*under, *effect)))
      }
    }
    (AnimatedValue::Length(under), AnimatedValue::Length(effect)) => {
      let under_px = under.to_px();
      let effect_px = effect.to_px();
      if !under_px.is_finite() || !effect_px.is_finite() {
        None
      } else {
        let mut px = under_px + effect_px;
        // `outline-width` is clamped to be non-negative at computed-value time.
        if property == "outline-width" {
          px = px.max(0.0);
        }
        Some(AnimatedValue::Length(Length::px(px)))
      }
    }
    (
      AnimatedValue::OutlineColor(OutlineColor::Color(under)),
      AnimatedValue::OutlineColor(OutlineColor::Color(effect)),
    ) => {
      if !under.a.is_finite() || !effect.a.is_finite() {
        None
      } else {
        Some(AnimatedValue::OutlineColor(OutlineColor::Color(add_color(
          *under, *effect,
        ))))
      }
    }
    (AnimatedValue::BorderColor(under), AnimatedValue::BorderColor(effect)) => {
      if !under.iter().all(|c| c.a.is_finite()) || !effect.iter().all(|c| c.a.is_finite()) {
        None
      } else {
        let mut out = [Rgba::TRANSPARENT; 4];
        for i in 0..4 {
          out[i] = add_color(under[i], effect[i]);
        }
        Some(AnimatedValue::BorderColor(out))
      }
    }
    (AnimatedValue::BorderWidth(under), AnimatedValue::BorderWidth(effect)) => {
      let mut out = [Length::px(0.0); 4];
      for i in 0..4 {
        let under_px = under[i].to_px();
        let effect_px = effect[i].to_px();
        if !under_px.is_finite() || !effect_px.is_finite() {
          return false;
        }
        out[i] = Length::px((under_px + effect_px).max(0.0));
      }
      Some(AnimatedValue::BorderWidth(out))
    }
    (AnimatedValue::BorderRadius(under), AnimatedValue::BorderRadius(effect)) => {
      let mut out = [BorderCornerRadius::default(); 4];
      for i in 0..4 {
        let under_x = under[i].x.to_px();
        let under_y = under[i].y.to_px();
        let effect_x = effect[i].x.to_px();
        let effect_y = effect[i].y.to_px();
        if !under_x.is_finite()
          || !under_y.is_finite()
          || !effect_x.is_finite()
          || !effect_y.is_finite()
        {
          return false;
        }
        out[i] = BorderCornerRadius {
          x: Length::px((under_x + effect_x).max(0.0)),
          y: Length::px((under_y + effect_y).max(0.0)),
        };
      }
      Some(AnimatedValue::BorderRadius(out))
    }
    (AnimatedValue::BoxShadow(under), AnimatedValue::BoxShadow(effect)) => {
      add_box_shadow_list(under, effect).map(AnimatedValue::BoxShadow)
    }
    (AnimatedValue::TextShadow(under), AnimatedValue::TextShadow(effect)) => {
      add_text_shadow_list(under, effect).map(AnimatedValue::TextShadow)
    }
    (AnimatedValue::Filter(under), AnimatedValue::Filter(effect)) => {
      add_filter_list(under, effect).map(AnimatedValue::Filter)
    }
    (AnimatedValue::BackdropFilter(under), AnimatedValue::BackdropFilter(effect)) => {
      add_filter_list(under, effect).map(AnimatedValue::BackdropFilter)
    }
    _ => None,
  };

  if let Some(combined) = combined {
    (interpolator.apply)(style, &combined);
    return true;
  }
  false
}

fn apply_iteration_accumulation(
  values: &mut HashMap<String, AnimatedValue>,
  start: &HashMap<String, AnimatedValue>,
  end: &HashMap<String, AnimatedValue>,
  iteration: u64,
) {
  if iteration == 0 {
    return;
  }
  for (name, value) in values.iter_mut() {
    let (Some(start_val), Some(end_val)) = (start.get(name), end.get(name)) else {
      continue;
    };
    if let Some(accumulated) = accumulate_iteration_value(value, start_val, end_val, iteration) {
      *value = accumulated;
    }
  }
}

fn pow_transform3d(mut base: Transform3D, mut exp: u64) -> Transform3D {
  let mut result = Transform3D::identity();
  while exp > 0 {
    if exp & 1 == 1 {
      result = result.multiply(&base);
    }
    exp >>= 1;
    if exp > 0 {
      base = base.multiply(&base);
    }
  }
  result
}

fn invert_transform3d(matrix: &Transform3D) -> Option<Transform3D> {
  let m = matrix.m;
  if !m.iter().all(|v| v.is_finite()) {
    return None;
  }

  let mut inv = [0.0f32; 16];

  inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
    + m[9] * m[7] * m[14]
    + m[13] * m[6] * m[11]
    - m[13] * m[7] * m[10];

  inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
    - m[8] * m[7] * m[14]
    - m[12] * m[6] * m[11]
    + m[12] * m[7] * m[10];

  inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
    + m[8] * m[7] * m[13]
    + m[12] * m[5] * m[11]
    - m[12] * m[7] * m[9];

  inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
    - m[8] * m[6] * m[13]
    - m[12] * m[5] * m[10]
    + m[12] * m[6] * m[9];

  inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
    - m[9] * m[3] * m[14]
    - m[13] * m[2] * m[11]
    + m[13] * m[3] * m[10];

  inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
    + m[8] * m[3] * m[14]
    + m[12] * m[2] * m[11]
    - m[12] * m[3] * m[10];

  inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
    - m[8] * m[3] * m[13]
    - m[12] * m[1] * m[11]
    + m[12] * m[3] * m[9];

  inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
    + m[8] * m[2] * m[13]
    + m[12] * m[1] * m[10]
    - m[12] * m[2] * m[9];

  inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
    + m[5] * m[3] * m[14]
    + m[13] * m[2] * m[7]
    - m[13] * m[3] * m[6];

  inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
    - m[4] * m[3] * m[14]
    - m[12] * m[2] * m[7]
    + m[12] * m[3] * m[6];

  inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
    + m[4] * m[3] * m[13]
    + m[12] * m[1] * m[7]
    - m[12] * m[3] * m[5];

  inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
    - m[4] * m[2] * m[13]
    - m[12] * m[1] * m[6]
    + m[12] * m[2] * m[5];

  inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
    - m[5] * m[3] * m[10]
    - m[9] * m[2] * m[7]
    + m[9] * m[3] * m[6];

  inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
    + m[4] * m[3] * m[10]
    + m[8] * m[2] * m[7]
    - m[8] * m[3] * m[6];

  inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
    - m[4] * m[3] * m[9]
    - m[8] * m[1] * m[7]
    + m[8] * m[3] * m[5];

  inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
    + m[4] * m[2] * m[9]
    + m[8] * m[1] * m[6]
    - m[8] * m[2] * m[5];

  let det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
  if !det.is_finite() || det.abs() < 1e-6 {
    return None;
  }
  let inv_det = 1.0 / det;
  for v in inv.iter_mut() {
    *v *= inv_det;
  }
  if !inv.iter().all(|v| v.is_finite()) {
    return None;
  }
  Some(Transform3D { m: inv })
}

fn accumulate_iteration_value(
  current: &AnimatedValue,
  start: &AnimatedValue,
  end: &AnimatedValue,
  iteration: u64,
) -> Option<AnimatedValue> {
  match (current, start, end) {
    (AnimatedValue::Opacity(cur), AnimatedValue::Opacity(start), AnimatedValue::Opacity(end)) => {
      if !cur.is_finite() || !start.is_finite() || !end.is_finite() {
        return None;
      }
      let delta = end - start;
      Some(AnimatedValue::Opacity(cur + (iteration as f32) * delta))
    }
    (AnimatedValue::Color(cur), AnimatedValue::Color(start), AnimatedValue::Color(end)) => {
      if !cur.a.is_finite() || !start.a.is_finite() || !end.a.is_finite() {
        return None;
      }
      let iter = iteration as i128;
      let r = clamp_color_channel_i128(cur.r as i128 + iter * (end.r as i128 - start.r as i128));
      let g = clamp_color_channel_i128(cur.g as i128 + iter * (end.g as i128 - start.g as i128));
      let b = clamp_color_channel_i128(cur.b as i128 + iter * (end.b as i128 - start.b as i128));
      let alpha = (cur.a + (iteration as f32) * (end.a - start.a)).clamp(0.0, 1.0);
      if !alpha.is_finite() {
        return None;
      }
      Some(AnimatedValue::Color(Rgba::new(r, g, b, alpha)))
    }
    (AnimatedValue::Length(cur), AnimatedValue::Length(start), AnimatedValue::Length(end)) => {
      let cur_px = cur.to_px();
      let start_px = start.to_px();
      let end_px = end.to_px();
      if !cur_px.is_finite() || !start_px.is_finite() || !end_px.is_finite() {
        return None;
      }
      let delta = end_px - start_px;
      Some(AnimatedValue::Length(Length::px(
        cur_px + (iteration as f32) * delta,
      )))
    }
    (
      AnimatedValue::OutlineColor(OutlineColor::Color(cur)),
      AnimatedValue::OutlineColor(OutlineColor::Color(start)),
      AnimatedValue::OutlineColor(OutlineColor::Color(end)),
    ) => {
      if !cur.a.is_finite() || !start.a.is_finite() || !end.a.is_finite() {
        return None;
      }
      let iter = iteration as i128;
      let r = clamp_color_channel_i128(cur.r as i128 + iter * (end.r as i128 - start.r as i128));
      let g = clamp_color_channel_i128(cur.g as i128 + iter * (end.g as i128 - start.g as i128));
      let b = clamp_color_channel_i128(cur.b as i128 + iter * (end.b as i128 - start.b as i128));
      let alpha = (cur.a + (iteration as f32) * (end.a - start.a)).clamp(0.0, 1.0);
      if !alpha.is_finite() {
        return None;
      }
      Some(AnimatedValue::OutlineColor(OutlineColor::Color(Rgba::new(
        r, g, b, alpha,
      ))))
    }
    (
      AnimatedValue::BorderColor(cur),
      AnimatedValue::BorderColor(start),
      AnimatedValue::BorderColor(end),
    ) => {
      if !cur.iter().all(|c| c.a.is_finite())
        || !start.iter().all(|c| c.a.is_finite())
        || !end.iter().all(|c| c.a.is_finite())
      {
        return None;
      }
      let iter = iteration as i128;
      let mut out = [Rgba::TRANSPARENT; 4];
      for i in 0..4 {
        let r = clamp_color_channel_i128(
          cur[i].r as i128 + iter * (end[i].r as i128 - start[i].r as i128),
        );
        let g = clamp_color_channel_i128(
          cur[i].g as i128 + iter * (end[i].g as i128 - start[i].g as i128),
        );
        let b = clamp_color_channel_i128(
          cur[i].b as i128 + iter * (end[i].b as i128 - start[i].b as i128),
        );
        let alpha = (cur[i].a + (iteration as f32) * (end[i].a - start[i].a)).clamp(0.0, 1.0);
        if !alpha.is_finite() {
          return None;
        }
        out[i] = Rgba::new(r, g, b, alpha);
      }
      Some(AnimatedValue::BorderColor(out))
    }
    (
      AnimatedValue::BorderWidth(cur),
      AnimatedValue::BorderWidth(start),
      AnimatedValue::BorderWidth(end),
    ) => {
      let mut out = [Length::px(0.0); 4];
      for i in 0..4 {
        let cur_px = cur[i].to_px();
        let start_px = start[i].to_px();
        let end_px = end[i].to_px();
        if !cur_px.is_finite() || !start_px.is_finite() || !end_px.is_finite() {
          return None;
        }
        let delta = end_px - start_px;
        out[i] = Length::px((cur_px + (iteration as f32) * delta).max(0.0));
      }
      Some(AnimatedValue::BorderWidth(out))
    }
    (
      AnimatedValue::BorderRadius(cur),
      AnimatedValue::BorderRadius(start),
      AnimatedValue::BorderRadius(end),
    ) => {
      let mut out = [BorderCornerRadius::default(); 4];
      for i in 0..4 {
        let cx = cur[i].x.to_px();
        let cy = cur[i].y.to_px();
        let sx = start[i].x.to_px();
        let sy = start[i].y.to_px();
        let ex = end[i].x.to_px();
        let ey = end[i].y.to_px();
        if !cx.is_finite() || !cy.is_finite() || !sx.is_finite() || !sy.is_finite() {
          return None;
        }
        if !ex.is_finite() || !ey.is_finite() {
          return None;
        }
        out[i] = BorderCornerRadius {
          x: Length::px((cx + (iteration as f32) * (ex - sx)).max(0.0)),
          y: Length::px((cy + (iteration as f32) * (ey - sy)).max(0.0)),
        };
      }
      Some(AnimatedValue::BorderRadius(out))
    }
    (
      AnimatedValue::BoxShadow(cur),
      AnimatedValue::BoxShadow(start),
      AnimatedValue::BoxShadow(end),
    ) => {
      let max_len = cur.len().max(start.len()).max(end.len());
      let iter = iteration as f32;
      let iter_i = iteration as i128;
      let mut out = Vec::with_capacity(max_len);
      for idx in 0..max_len {
        let cur_shadow = if let Some(shadow) = cur.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = start.get(idx) {
          transparent_box_shadow_like(shadow)
        } else if let Some(shadow) = end.get(idx) {
          transparent_box_shadow_like(shadow)
        } else {
          continue;
        };
        let start_shadow = if let Some(shadow) = start.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = end.get(idx) {
          transparent_box_shadow_like(shadow)
        } else {
          continue;
        };
        let end_shadow = if let Some(shadow) = end.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = start.get(idx) {
          transparent_box_shadow_like(shadow)
        } else {
          continue;
        };

        if cur_shadow.inset != start_shadow.inset || cur_shadow.inset != end_shadow.inset {
          return None;
        }
        if !cur_shadow.color.a.is_finite()
          || !start_shadow.color.a.is_finite()
          || !end_shadow.color.a.is_finite()
        {
          return None;
        }

        let cx = cur_shadow.offset_x.to_px();
        let cy = cur_shadow.offset_y.to_px();
        let cb = cur_shadow.blur_radius.to_px();
        let cs = cur_shadow.spread_radius.to_px();
        let sx = start_shadow.offset_x.to_px();
        let sy = start_shadow.offset_y.to_px();
        let sb = start_shadow.blur_radius.to_px();
        let ss = start_shadow.spread_radius.to_px();
        let ex = end_shadow.offset_x.to_px();
        let ey = end_shadow.offset_y.to_px();
        let eb = end_shadow.blur_radius.to_px();
        let es = end_shadow.spread_radius.to_px();
        if !cx.is_finite()
          || !cy.is_finite()
          || !cb.is_finite()
          || !cs.is_finite()
          || !sx.is_finite()
          || !sy.is_finite()
          || !sb.is_finite()
          || !ss.is_finite()
          || !ex.is_finite()
          || !ey.is_finite()
          || !eb.is_finite()
          || !es.is_finite()
        {
          return None;
        }

        let r = clamp_color_channel_i128(
          cur_shadow.color.r as i128
            + iter_i * (end_shadow.color.r as i128 - start_shadow.color.r as i128),
        );
        let g = clamp_color_channel_i128(
          cur_shadow.color.g as i128
            + iter_i * (end_shadow.color.g as i128 - start_shadow.color.g as i128),
        );
        let b = clamp_color_channel_i128(
          cur_shadow.color.b as i128
            + iter_i * (end_shadow.color.b as i128 - start_shadow.color.b as i128),
        );
        let alpha =
          (cur_shadow.color.a + iter * (end_shadow.color.a - start_shadow.color.a)).clamp(0.0, 1.0);
        if !alpha.is_finite() {
          return None;
        }

        out.push(BoxShadow {
          offset_x: Length::px(cx + iter * (ex - sx)),
          offset_y: Length::px(cy + iter * (ey - sy)),
          blur_radius: Length::px((cb + iter * (eb - sb)).max(0.0)),
          spread_radius: Length::px(cs + iter * (es - ss)),
          color: Rgba::new(r, g, b, alpha),
          inset: cur_shadow.inset,
        });
      }
      Some(AnimatedValue::BoxShadow(out))
    }
    (
      AnimatedValue::TextShadow(cur),
      AnimatedValue::TextShadow(start),
      AnimatedValue::TextShadow(end),
    ) => {
      let max_len = cur.len().max(start.len()).max(end.len());
      let iter = iteration as f32;
      let iter_i = iteration as i128;
      let mut out = Vec::with_capacity(max_len);
      for idx in 0..max_len {
        let cur_shadow = if let Some(shadow) = cur.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = start.get(idx) {
          transparent_text_shadow_like(shadow)
        } else if let Some(shadow) = end.get(idx) {
          transparent_text_shadow_like(shadow)
        } else {
          continue;
        };
        let start_shadow = if let Some(shadow) = start.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = end.get(idx) {
          transparent_text_shadow_like(shadow)
        } else {
          continue;
        };
        let end_shadow = if let Some(shadow) = end.get(idx) {
          shadow.clone()
        } else if let Some(shadow) = start.get(idx) {
          transparent_text_shadow_like(shadow)
        } else {
          continue;
        };

        let cur_color = cur_shadow.color.unwrap_or(Rgba::BLACK);
        let start_color = start_shadow.color.unwrap_or(Rgba::BLACK);
        let end_color = end_shadow.color.unwrap_or(Rgba::BLACK);
        if !cur_color.a.is_finite() || !start_color.a.is_finite() || !end_color.a.is_finite() {
          return None;
        }

        let cx = cur_shadow.offset_x.to_px();
        let cy = cur_shadow.offset_y.to_px();
        let cb = cur_shadow.blur_radius.to_px();
        let sx = start_shadow.offset_x.to_px();
        let sy = start_shadow.offset_y.to_px();
        let sb = start_shadow.blur_radius.to_px();
        let ex = end_shadow.offset_x.to_px();
        let ey = end_shadow.offset_y.to_px();
        let eb = end_shadow.blur_radius.to_px();
        if !cx.is_finite()
          || !cy.is_finite()
          || !cb.is_finite()
          || !sx.is_finite()
          || !sy.is_finite()
          || !sb.is_finite()
          || !ex.is_finite()
          || !ey.is_finite()
          || !eb.is_finite()
        {
          return None;
        }

        let r = clamp_color_channel_i128(
          cur_color.r as i128 + iter_i * (end_color.r as i128 - start_color.r as i128),
        );
        let g = clamp_color_channel_i128(
          cur_color.g as i128 + iter_i * (end_color.g as i128 - start_color.g as i128),
        );
        let b = clamp_color_channel_i128(
          cur_color.b as i128 + iter_i * (end_color.b as i128 - start_color.b as i128),
        );
        let alpha = (cur_color.a + iter * (end_color.a - start_color.a)).clamp(0.0, 1.0);
        if !alpha.is_finite() {
          return None;
        }

        out.push(TextShadow {
          offset_x: Length::px(cx + iter * (ex - sx)),
          offset_y: Length::px(cy + iter * (ey - sy)),
          blur_radius: Length::px((cb + iter * (eb - sb)).max(0.0)),
          color: Some(Rgba::new(r, g, b, alpha)),
        });
      }
      Some(AnimatedValue::TextShadow(out))
    }
    (AnimatedValue::Filter(cur), AnimatedValue::Filter(start), AnimatedValue::Filter(end)) => {
      let rc = resolved_filters_from_functions(cur);
      let rs = resolved_filters_from_functions(start);
      let re = resolved_filters_from_functions(end);
      let accumulated = accumulate_resolved_filter_list(&rc, &rs, &re, iteration)?;
      Some(AnimatedValue::Filter(resolved_filters_to_functions(
        &accumulated,
      )))
    }
    (
      AnimatedValue::BackdropFilter(cur),
      AnimatedValue::BackdropFilter(start),
      AnimatedValue::BackdropFilter(end),
    ) => {
      let rc = resolved_filters_from_functions(cur);
      let rs = resolved_filters_from_functions(start);
      let re = resolved_filters_from_functions(end);
      let accumulated = accumulate_resolved_filter_list(&rc, &rs, &re, iteration)?;
      Some(AnimatedValue::BackdropFilter(resolved_filters_to_functions(
        &accumulated,
      )))
    }
    (
      AnimatedValue::Translate(cur),
      AnimatedValue::Translate(start),
      AnimatedValue::Translate(end),
    ) => {
      let wants_values = !matches!(cur, TranslateValue::None)
        || !matches!(start, TranslateValue::None)
        || !matches!(end, TranslateValue::None);
      if !wants_values {
        return None;
      }
      let (cx, cy, cz) = match cur {
        TranslateValue::None => (0.0, 0.0, 0.0),
        TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
      };
      let (sx, sy, sz) = match start {
        TranslateValue::None => (0.0, 0.0, 0.0),
        TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
      };
      let (ex, ey, ez) = match end {
        TranslateValue::None => (0.0, 0.0, 0.0),
        TranslateValue::Values { x, y, z } => (x.to_px(), y.to_px(), z.to_px()),
      };
      let delta_x = ex - sx;
      let delta_y = ey - sy;
      let delta_z = ez - sz;
      let iter = iteration as f32;
      Some(AnimatedValue::Translate(TranslateValue::Values {
        x: Length::px(cx + iter * delta_x),
        y: Length::px(cy + iter * delta_y),
        z: Length::px(cz + iter * delta_z),
      }))
    }
    (AnimatedValue::Rotate(cur), AnimatedValue::Rotate(start), AnimatedValue::Rotate(end)) => {
      if matches!(cur, RotateValue::None)
        && matches!(start, RotateValue::None)
        && matches!(end, RotateValue::None)
      {
        return None;
      }

      fn axis_angle(
        value: RotateValue,
        fallback_axis: (f32, f32, f32),
      ) -> Option<((f32, f32, f32), f32)> {
        let (x, y, z, angle) = match value {
          RotateValue::None => (fallback_axis.0, fallback_axis.1, fallback_axis.2, 0.0),
          RotateValue::Angle(angle) => (0.0, 0.0, 1.0, angle),
          RotateValue::AxisAngle { x, y, z, angle } => (x, y, z, angle),
        };
        if !x.is_finite() || !y.is_finite() || !z.is_finite() || !angle.is_finite() {
          return None;
        }
        let len = (x * x + y * y + z * z).sqrt();
        if !len.is_finite() || len < 1e-6 {
          return None;
        }
        Some(((x / len, y / len, z / len), angle))
      }

      fn axis_hint(value: RotateValue) -> Option<(f32, f32, f32)> {
        match value {
          RotateValue::None => None,
          RotateValue::Angle(_) => Some((0.0, 0.0, 1.0)),
          RotateValue::AxisAngle { x, y, z, angle: _ } => {
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
              return None;
            }
            let len = (x * x + y * y + z * z).sqrt();
            if !len.is_finite() || len < 1e-6 {
              return None;
            }
            Some((x / len, y / len, z / len))
          }
        }
      }

      let fallback_axis = axis_hint(*start)
        .or_else(|| axis_hint(*end))
        .or_else(|| axis_hint(*cur))
        .unwrap_or((0.0, 0.0, 1.0));

      let Some(((ax, ay, az), start_angle)) = axis_angle(*start, fallback_axis) else {
        return None;
      };
      let Some(((mut bx, mut by, mut bz), mut end_angle)) = axis_angle(*end, (ax, ay, az)) else {
        return None;
      };
      let Some(((mut cx, mut cy, mut cz), mut cur_angle)) = axis_angle(*cur, (ax, ay, az)) else {
        return None;
      };

      // Axis-angle has a sign ambiguity: (a, θ) == (-a, -θ). Canonicalize both values to the
      // start axis direction so we can subtract/add angles reliably.
      let end_dot = ax * bx + ay * by + az * bz;
      if end_dot < 0.0 {
        bx = -bx;
        by = -by;
        bz = -bz;
        end_angle = -end_angle;
      }
      let cur_dot = ax * cx + ay * cy + az * cz;
      if cur_dot < 0.0 {
        cx = -cx;
        cy = -cy;
        cz = -cz;
        cur_angle = -cur_angle;
      }

      if (ax - bx).abs() > 1e-6
        || (ay - by).abs() > 1e-6
        || (az - bz).abs() > 1e-6
        || (ax - cx).abs() > 1e-6
        || (ay - cy).abs() > 1e-6
        || (az - cz).abs() > 1e-6
      {
        return None;
      }

      let delta = end_angle - start_angle;
      let angle = cur_angle + (iteration as f32) * delta;
      if !angle.is_finite() {
        return None;
      }

      if ax.abs() < 1e-6 && ay.abs() < 1e-6 {
        Some(AnimatedValue::Rotate(RotateValue::Angle(
          angle * az.signum(),
        )))
      } else {
        Some(AnimatedValue::Rotate(RotateValue::AxisAngle {
          x: ax,
          y: ay,
          z: az,
          angle,
        }))
      }
    }
    (AnimatedValue::Scale(cur), AnimatedValue::Scale(start), AnimatedValue::Scale(end)) => {
      let wants_values = !matches!(cur, ScaleValue::None)
        || !matches!(start, ScaleValue::None)
        || !matches!(end, ScaleValue::None);
      if !wants_values {
        return None;
      }
      let exp = i32::try_from(iteration).ok()?;
      let (cx, cy, cz) = match cur {
        ScaleValue::None => (1.0, 1.0, 1.0),
        ScaleValue::Values { x, y, z } => (*x, *y, *z),
      };
      let (sx, sy, sz) = match start {
        ScaleValue::None => (1.0, 1.0, 1.0),
        ScaleValue::Values { x, y, z } => (*x, *y, *z),
      };
      let (ex, ey, ez) = match end {
        ScaleValue::None => (1.0, 1.0, 1.0),
        ScaleValue::Values { x, y, z } => (*x, *y, *z),
      };
      if sx.abs() < 1e-6 || sy.abs() < 1e-6 || sz.abs() < 1e-6 {
        return None;
      }
      let rx = ex / sx;
      let ry = ey / sy;
      let rz = ez / sz;
      if !rx.is_finite() || !ry.is_finite() || !rz.is_finite() {
        return None;
      }
      Some(AnimatedValue::Scale(ScaleValue::Values {
        x: cx * rx.powi(exp),
        y: cy * ry.powi(exp),
        z: cz * rz.powi(exp),
      }))
    }
    (
      AnimatedValue::Transform(cur_list),
      AnimatedValue::Transform(start_list),
      AnimatedValue::Transform(end_list),
    ) => {
      if cur_list.is_empty() && start_list.is_empty() && end_list.is_empty() {
        return None;
      }

      let start_matrix = compose_transform_list(start_list);
      let inv_start = invert_transform3d(&start_matrix)?;
      let end_matrix = compose_transform_list(end_list);
      let delta = end_matrix.multiply(&inv_start);
      let delta_pow = pow_transform3d(delta, iteration);
      let cur_matrix = if cur_list.is_empty() {
        Transform3D::identity()
      } else {
        compose_transform_list(cur_list)
      };
      let accumulated = delta_pow.multiply(&cur_matrix);
      Some(AnimatedValue::Transform(vec![
        crate::css::types::Transform::Matrix3d(accumulated.m),
      ]))
    }
    _ => None,
  }
}

#[derive(Debug, Clone)]
enum TimelineState {
  Inactive,
  Scroll {
    timeline: ScrollTimeline,
    scroll_pos: f32,
    scroll_range: f32,
    viewport_size: f32,
  },
  View {
    timeline: ViewTimeline,
    target_start: f32,
    target_end: f32,
    view_size: f32,
    scroll_offset: f32,
  },
}

fn pick<'a, T: Clone>(list: &'a [T], idx: usize, default: T) -> T {
  if list.is_empty() {
    return default;
  }
  list
    .get(idx)
    .cloned()
    .unwrap_or_else(|| list.last().cloned().unwrap_or(default))
}

fn fill_backwards(fill: AnimationFillMode) -> bool {
  matches!(fill, AnimationFillMode::Backwards | AnimationFillMode::Both)
}

fn fill_forwards(fill: AnimationFillMode) -> bool {
  matches!(fill, AnimationFillMode::Forwards | AnimationFillMode::Both)
}

fn iteration_reverses(direction: AnimationDirection, iteration: u64) -> bool {
  match direction {
    AnimationDirection::Normal => false,
    AnimationDirection::Reverse => true,
    AnimationDirection::Alternate => iteration % 2 == 1,
    AnimationDirection::AlternateReverse => iteration % 2 == 0,
  }
}

fn animation_end_progress(direction: AnimationDirection, iterations: f32) -> f32 {
  if !iterations.is_finite() {
    return 0.0;
  }
  let iterations = iterations.max(0.0);
  let reversed_start = iteration_reverses(direction, 0);
  if iterations <= 0.0 {
    return if reversed_start { 1.0 } else { 0.0 };
  }

  let whole = iterations.floor();
  let frac = (iterations - whole).clamp(0.0, 1.0);
  if frac <= f32::EPSILON {
    let last_iteration = (whole.max(1.0) as u64).saturating_sub(1);
    let reversed = iteration_reverses(direction, last_iteration);
    if reversed {
      0.0
    } else {
      1.0
    }
  } else {
    let iteration = whole as u64;
    let reversed = iteration_reverses(direction, iteration);
    if reversed {
      1.0 - frac
    } else {
      frac
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct AnimationProgress {
  progress: f32,
  iteration: u64,
}

fn animation_end_iteration(iterations: f32) -> u64 {
  if !iterations.is_finite() {
    return 0;
  }
  let iterations = iterations.max(0.0);
  if iterations <= 0.0 {
    return 0;
  }
  let whole = iterations.floor();
  let frac = (iterations - whole).clamp(0.0, 1.0);
  if frac <= f32::EPSILON {
    (whole.max(1.0) as u64).saturating_sub(1)
  } else {
    whole as u64
  }
}

fn time_based_animation_progress_impl(
  style: &ComputedStyle,
  idx: usize,
  time_ms: f32,
  respect_play_state: bool,
) -> Option<f32> {
  time_based_animation_state_impl(style, idx, time_ms, respect_play_state).map(|s| s.progress)
}

fn time_based_animation_state_impl(
  style: &ComputedStyle,
  idx: usize,
  time_ms: f32,
  respect_play_state: bool,
) -> Option<AnimationProgress> {
  if time_ms < 0.0 {
    return None;
  }

  let raw_duration = pick(&style.animation_durations, idx, 0.0);
  // CSS Animations Level 2 adds `animation-duration: auto`, primarily for scroll-driven
  // animations. This engine stores the keyword as a negative sentinel.
  //
  // For time-based timelines (`animation-timeline: auto`), there is no intrinsic duration, so we
  // treat `auto` like `0ms` to keep output deterministic and avoid dividing by a negative value.
  let duration = if raw_duration <= ANIMATION_DURATION_AUTO_SENTINEL_MS {
    0.0
  } else {
    raw_duration.max(0.0)
  };
  let delay = pick(&style.animation_delays, idx, 0.0);
  let iteration_count = pick(
    &style.animation_iteration_counts,
    idx,
    AnimationIterationCount::default(),
  );
  let direction = pick(
    &style.animation_directions,
    idx,
    AnimationDirection::default(),
  );
  let fill = pick(
    &style.animation_fill_modes,
    idx,
    AnimationFillMode::default(),
  );
  let play_state = pick(
    &style.animation_play_states,
    idx,
    AnimationPlayState::default(),
  );

  let effective_time_ms = if respect_play_state {
    match play_state {
      AnimationPlayState::Running => time_ms,
      AnimationPlayState::Paused => 0.0,
    }
  } else {
    time_ms
  };

  let local_time = effective_time_ms - delay;
  let iterations = iteration_count.as_f32();
  let active_duration = if duration <= 0.0 {
    0.0
  } else if iterations.is_infinite() {
    f32::INFINITY
  } else {
    duration * iterations.max(0.0)
  };

  let start_progress = if iteration_reverses(direction, 0) {
    1.0
  } else {
    0.0
  };
  let end_progress = animation_end_progress(direction, iterations);
  let end_iteration = animation_end_iteration(iterations);

  if local_time < 0.0 {
    return if fill_backwards(fill) {
      Some(AnimationProgress {
        progress: start_progress,
        iteration: 0,
      })
    } else {
      None
    };
  }

  if active_duration.is_finite() && local_time >= active_duration {
    return if fill_forwards(fill) {
      Some(AnimationProgress {
        progress: end_progress,
        iteration: end_iteration,
      })
    } else {
      None
    };
  }

  if duration <= 0.0 {
    return Some(AnimationProgress {
      progress: end_progress,
      iteration: end_iteration,
    });
  }

  let total = (local_time / duration).max(0.0);
  let iteration = total.floor() as u64;
  let iteration_progress = (total - iteration as f32).clamp(0.0, 1.0);
  let reversed = iteration_reverses(direction, iteration);
  let directed = if reversed {
    1.0 - iteration_progress
  } else {
    iteration_progress
  };
  Some(AnimationProgress {
    progress: directed,
    iteration,
  })
}

fn time_based_animation_progress(style: &ComputedStyle, idx: usize, time_ms: f32) -> Option<f32> {
  time_based_animation_progress_impl(style, idx, time_ms, true)
}

fn settled_time_based_animation_progress(style: &ComputedStyle, idx: usize) -> Option<f32> {
  settled_time_based_animation_state(style, idx).map(|s| s.progress)
}

fn settled_time_based_animation_state(
  style: &ComputedStyle,
  idx: usize,
) -> Option<AnimationProgress> {
  let play_state = pick(
    &style.animation_play_states,
    idx,
    AnimationPlayState::default(),
  );
  if matches!(play_state, AnimationPlayState::Paused) {
    return time_based_animation_state_impl(style, idx, 0.0, true);
  }

  // `animation-duration: auto` is represented by a negative sentinel. For time-based timelines we
  // treat it as `0ms`, so the end progress here depends solely on iterations and direction.
  let fill = pick(
    &style.animation_fill_modes,
    idx,
    AnimationFillMode::default(),
  );
  if !fill_forwards(fill) {
    return None;
  }

  let iteration_count = pick(
    &style.animation_iteration_counts,
    idx,
    AnimationIterationCount::default(),
  );
  let iterations = iteration_count.as_f32();
  // Infinite animations don't have a deterministic "final" keyframe state.
  if !iterations.is_finite() {
    return None;
  }

  let direction = pick(
    &style.animation_directions,
    idx,
    AnimationDirection::default(),
  );

  let end_progress = animation_end_progress(direction, iterations);
  Some(AnimationProgress {
    progress: end_progress,
    iteration: animation_end_iteration(iterations),
  })
}

#[derive(Debug, Clone, Copy)]
struct ScrollContainerContext {
  scroll: Point,
  viewport: Size,
  content: Size,
  origin: Point,
  writing_mode: WritingMode,
  direction: Direction,
  scroll_padding_top: Length,
  scroll_padding_right: Length,
  scroll_padding_bottom: Length,
  scroll_padding_left: Length,
}

fn is_scroll_container(node: &FragmentNode, scroll_state: &ScrollState) -> bool {
  let style = node.style.as_deref();
  let overflow_scroll = style
    .map(|s| {
      matches!(s.overflow_x, Overflow::Scroll | Overflow::Auto)
        || matches!(s.overflow_y, Overflow::Scroll | Overflow::Auto)
    })
    .unwrap_or(false);

  if overflow_scroll {
    return true;
  }
  node
    .box_id()
    .is_some_and(|id| scroll_state.elements.contains_key(&id))
}

fn root_scroll_container_context(
  scroll_state: &ScrollState,
  root_viewport: Rect,
  root_content: Rect,
  writing_mode: WritingMode,
  direction: Direction,
  scroll_padding_top: Length,
  scroll_padding_right: Length,
  scroll_padding_bottom: Length,
  scroll_padding_left: Length,
) -> ScrollContainerContext {
  ScrollContainerContext {
    scroll: scroll_state.viewport,
    viewport: Size::new(root_viewport.width(), root_viewport.height()),
    content: Size::new(root_content.width(), root_content.height()),
    origin: Point::ZERO,
    writing_mode,
    direction,
    scroll_padding_top,
    scroll_padding_right,
    scroll_padding_bottom,
    scroll_padding_left,
  }
}

fn scroll_container_context_for_node(
  node: &FragmentNode,
  origin: Point,
  scroll_state: &ScrollState,
) -> Option<ScrollContainerContext> {
  if !is_scroll_container(node, scroll_state) {
    return None;
  }

  let scroll = node
    .box_id()
    .map(|id| scroll_state.element_offset(id))
    .unwrap_or(Point::ZERO);
  let writing_mode = node
    .style
    .as_deref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  let direction = node
    .style
    .as_deref()
    .map(|s| s.direction)
    .unwrap_or(Direction::Ltr);
  let (scroll_padding_top, scroll_padding_right, scroll_padding_bottom, scroll_padding_left) = node
    .style
    .as_deref()
    .map(|s| {
      (
        s.scroll_padding_top,
        s.scroll_padding_right,
        s.scroll_padding_bottom,
        s.scroll_padding_left,
      )
    })
    .unwrap_or((
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
    ));
  Some(ScrollContainerContext {
    scroll,
    viewport: Size::new(node.bounds.width(), node.bounds.height()),
    content: Size::new(node.scroll_overflow.width(), node.scroll_overflow.height()),
    origin,
    writing_mode,
    direction,
    scroll_padding_top,
    scroll_padding_right,
    scroll_padding_bottom,
    scroll_padding_left,
  })
}

fn timeline_scroll_context(
  node: &FragmentNode,
  origin: Point,
  scroll_state: &ScrollState,
  root: ScrollContainerContext,
) -> ScrollContainerContext {
  scroll_container_context_for_node(node, origin, scroll_state).unwrap_or(root)
}

fn resolved_view_timeline_inset(
  inset: Option<ViewTimelineInset>,
  scroll_container: ScrollContainerContext,
  axis: TimelineAxis,
) -> ViewTimelineInset {
  let horizontal = axis_is_horizontal(axis, scroll_container.writing_mode);
  let positive = axis_is_positive(
    axis,
    scroll_container.writing_mode,
    scroll_container.direction,
  );
  let (auto_start, auto_end) = if horizontal {
    if positive {
      (
        scroll_container.scroll_padding_left,
        scroll_container.scroll_padding_right,
      )
    } else {
      (
        scroll_container.scroll_padding_right,
        scroll_container.scroll_padding_left,
      )
    }
  } else if positive {
    (
      scroll_container.scroll_padding_top,
      scroll_container.scroll_padding_bottom,
    )
  } else {
    (
      scroll_container.scroll_padding_bottom,
      scroll_container.scroll_padding_top,
    )
  };
  let inset = inset.unwrap_or_default();
  ViewTimelineInset {
    start: Some(inset.start.unwrap_or(auto_start)),
    end: Some(inset.end.unwrap_or(auto_end)),
  }
}

#[derive(Default)]
struct TimelineScopePlanNode {
  promotions: Vec<(String, TimelineState)>,
  children: Vec<TimelineScopePlanNode>,
  running_anchor_snapshot: Option<Box<TimelineScopePlanNode>>,
}

type TimelineCandidates = HashMap<String, Vec<TimelineState>>;

fn merge_timeline_candidates(into: &mut TimelineCandidates, mut other: TimelineCandidates) {
  for (name, mut candidates) in other.drain() {
    into.entry(name).or_default().append(&mut candidates);
  }
}

fn named_timeline_states_for_export(
  node: &FragmentNode,
  origin: Point,
  abs: Rect,
  root_context: ScrollContainerContext,
  ancestor_scroll_containers: &[ScrollContainerContext],
  scroll_state: &ScrollState,
) -> HashMap<String, TimelineState> {
  let Some(style) = node.style.as_deref() else {
    return HashMap::new();
  };

  // When a scroll progress timeline and view progress timeline share the same name on
  // an element, the scroll timeline should win (Scroll Animations §timeline-scoping).
  //
  // Store view timelines first so scroll timelines overwrite the entry for a shared name.
  let mut map = HashMap::new();

  let view_timeline_context = ancestor_scroll_containers
    .last()
    .copied()
    .unwrap_or(root_context);
  for tl in &style.view_timelines {
    let Some(name) = &tl.name else {
      continue;
    };
    let horizontal = axis_is_horizontal(tl.axis, view_timeline_context.writing_mode);
    let target_start = if horizontal {
      abs.x() - view_timeline_context.origin.x
    } else {
      abs.y() - view_timeline_context.origin.y
    };
    let target_end = if horizontal {
      target_start + abs.width()
    } else {
      target_start + abs.height()
    };
    let view_size = if horizontal {
      view_timeline_context.viewport.width
    } else {
      view_timeline_context.viewport.height
    };
    let scroll_offset = if horizontal {
      view_timeline_context.scroll.x
    } else {
      view_timeline_context.scroll.y
    };
    let content_size = if horizontal {
      view_timeline_context.content.width
    } else {
      view_timeline_context.content.height
    };
    let (target_start, target_end, view_size, scroll_offset) = axis_view_state(
      tl.axis,
      view_timeline_context.writing_mode,
      view_timeline_context.direction,
      target_start,
      target_end,
      scroll_offset,
      view_size,
      content_size,
    );
    let mut resolved_timeline = tl.clone();
    resolved_timeline.inset = Some(resolved_view_timeline_inset(
      tl.inset,
      view_timeline_context,
      tl.axis,
    ));
    map.insert(
      name.clone(),
      TimelineState::View {
        timeline: resolved_timeline,
        target_start,
        target_end,
        view_size,
        scroll_offset,
      },
    );
  }

  let scroll_timeline_context = timeline_scroll_context(node, origin, scroll_state, root_context);
  for tl in &style.scroll_timelines {
    let Some(name) = &tl.name else {
      continue;
    };
    let (scroll_pos, scroll_range, viewport_size) = axis_scroll_state(
      tl.axis,
      scroll_timeline_context.writing_mode,
      scroll_timeline_context.direction,
      scroll_timeline_context.scroll.x,
      scroll_timeline_context.scroll.y,
      scroll_timeline_context.viewport.width,
      scroll_timeline_context.viewport.height,
      scroll_timeline_context.content.width,
      scroll_timeline_context.content.height,
    );
    map.insert(
      name.clone(),
      TimelineState::Scroll {
        timeline: tl.clone(),
        scroll_pos,
        scroll_range,
        viewport_size,
      },
    );
  }

  map
}

fn build_timeline_scope_plan(
  node: &FragmentNode,
  origin: Point,
  root_context: ScrollContainerContext,
  scroll_state: &ScrollState,
) -> TimelineScopePlanNode {
  fn build_impl(
    node: &FragmentNode,
    origin: Point,
    root_context: ScrollContainerContext,
    scroll_state: &ScrollState,
    ancestor_scroll_containers: &mut Vec<ScrollContainerContext>,
  ) -> (TimelineScopePlanNode, TimelineCandidates) {
    let abs = Rect::from_xywh(
      origin.x,
      origin.y,
      node.bounds.width(),
      node.bounds.height(),
    );

    let pushed_scroll_container = scroll_container_context_for_node(node, origin, scroll_state);
    if let Some(ctx) = pushed_scroll_container {
      ancestor_scroll_containers.push(ctx);
    }

    let mut children_plans = Vec::with_capacity(node.children_ref().len());
    let mut descendant_candidates: TimelineCandidates = HashMap::new();
    for child in node.children() {
      let child_offset = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
      let (plan, exported) = build_impl(
        child,
        child_offset,
        root_context,
        scroll_state,
        ancestor_scroll_containers,
      );
      children_plans.push(plan);
      merge_timeline_candidates(&mut descendant_candidates, exported);
    }

    let mut running_anchor_snapshot = None;
    if let FragmentContent::RunningAnchor { snapshot, .. } = &node.content {
      let snapshot_node = snapshot.as_ref();
      let snapshot_offset = Point::new(
        origin.x + snapshot_node.bounds.x(),
        origin.y + snapshot_node.bounds.y(),
      );
      let (plan, exported) = build_impl(
        snapshot_node,
        snapshot_offset,
        root_context,
        scroll_state,
        ancestor_scroll_containers,
      );
      running_anchor_snapshot = Some(Box::new(plan));
      merge_timeline_candidates(&mut descendant_candidates, exported);
    }

    if pushed_scroll_container.is_some() {
      ancestor_scroll_containers.pop();
    }

    let scope_prop = node.style.as_deref().map(|s| &s.timeline_scope);
    let mut promotions: Vec<(String, TimelineState)> = Vec::new();

    match scope_prop {
      Some(TimelineScopeProperty::Names(names)) => {
        for name in names {
          let binding = match descendant_candidates.remove(name) {
            Some(mut candidates) if candidates.len() == 1 => {
              candidates.pop().unwrap_or(TimelineState::Inactive)
            }
            _ => TimelineState::Inactive,
          };
          promotions.push((name.clone(), binding));
        }
      }
      Some(TimelineScopeProperty::All) => {
        // When multiple timelines with the same name exist under `timeline-scope: all`,
        // this engine chooses a deterministic "inactive" binding rather than picking an
        // arbitrary candidate.
        let mut names: Vec<String> = descendant_candidates.keys().cloned().collect();
        names.sort();
        for name in names {
          let binding = match descendant_candidates.remove(&name) {
            Some(candidates) if candidates.len() == 1 => candidates
              .into_iter()
              .next()
              .unwrap_or(TimelineState::Inactive),
            _ => TimelineState::Inactive,
          };
          promotions.push((name, binding));
        }
      }
      _ => {}
    }

    let mut blocked_names: HashSet<String> = HashSet::new();
    match scope_prop {
      Some(TimelineScopeProperty::Names(names)) => {
        blocked_names.extend(names.iter().cloned());
      }
      Some(TimelineScopeProperty::All) => {
        blocked_names.extend(promotions.iter().map(|(name, _)| name.clone()));
      }
      _ => {}
    }

    let mut exported = descendant_candidates;
    let own = named_timeline_states_for_export(
      node,
      origin,
      abs,
      root_context,
      ancestor_scroll_containers,
      scroll_state,
    );
    for (name, state) in own {
      if blocked_names.contains(&name) {
        continue;
      }
      exported.entry(name).or_default().push(state);
    }

    (
      TimelineScopePlanNode {
        promotions,
        children: children_plans,
        running_anchor_snapshot,
      },
      exported,
    )
  }

  let mut ancestor_scroll_containers = Vec::new();
  let (plan, _) = build_impl(
    node,
    origin,
    root_context,
    scroll_state,
    &mut ancestor_scroll_containers,
  );
  plan
}

type TimelineScope = HashMap<String, Vec<TimelineState>>;

fn timeline_scope_push(scope: &mut TimelineScope, name: String, state: TimelineState) {
  scope.entry(name).or_default().push(state);
}

fn timeline_scope_pop(scope: &mut TimelineScope, name: &str) {
  let Some(stack) = scope.get_mut(name) else {
    return;
  };
  stack.pop();
  if stack.is_empty() {
    scope.remove(name);
  }
}

fn timeline_scope_resolve<'a>(scope: &'a TimelineScope, name: &str) -> Option<&'a TimelineState> {
  scope.get(name).and_then(|stack| stack.last())
}

fn scroll_driven_fill_progress(raw: f32, fill: AnimationFillMode) -> Option<f32> {
  if !raw.is_finite() {
    return None;
  }
  if raw < 0.0 {
    fill_backwards(fill).then_some(0.0)
  } else if raw > 1.0 {
    fill_forwards(fill).then_some(1.0)
  } else {
    Some(raw)
  }
}

fn progress_based_animation_state(
  style: &ComputedStyle,
  idx: usize,
  overall_progress: f32,
) -> Option<AnimationProgress> {
  if !overall_progress.is_finite() {
    return None;
  }
  let overall_progress = overall_progress.clamp(0.0, 1.0);

  let raw_duration = pick(&style.animation_durations, idx, 0.0);
  // `animation-duration: auto` is represented by a negative sentinel. When attached to a
  // progress-based timeline (scroll/view), Web Animations forbids mixing `auto` iteration duration
  // with time-based delays. Treat the delay as `0` and map scroll progress directly into
  // iteration progress (the legacy behaviour).
  //
  // This engine historically treated scroll-driven animations with the CSS default duration
  // (`0ms`) like `auto` so that simply attaching a scroll/view timeline yields a useful animation
  // without requiring authors/tests to opt into `auto` explicitly. Preserve that behaviour here by
  // treating non-positive durations like `auto`.
  if raw_duration <= ANIMATION_DURATION_AUTO_SENTINEL_MS || raw_duration <= 0.0 {
    return Some(scroll_driven_effect_state(style, idx, overall_progress));
  }

  let specified_duration_ms = raw_duration.max(0.0);
  let specified_delay_ms = pick(&style.animation_delays, idx, 0.0);
  let iteration_count = pick(
    &style.animation_iteration_counts,
    idx,
    AnimationIterationCount::default(),
  );
  let iterations = iteration_count.as_f32();
  // Scaling an infinite active duration into a finite [0,1] progress range is undefined here.
  // Keep the existing deterministic behaviour until dedicated infinite-iteration support is
  // implemented.
  if !iterations.is_finite() {
    return Some(scroll_driven_effect_state(style, idx, overall_progress));
  }

  let active_duration_ms = specified_duration_ms * iterations.max(0.0);
  // CSS Animations do not have an end delay, so the end time is simply start delay + active
  // duration (clamped to be non-negative for pathological negative delays).
  let end_time_ms = (specified_delay_ms + active_duration_ms).max(0.0);
  let time_ms = overall_progress * end_time_ms;
  time_based_animation_state_impl(style, idx, time_ms, false)
}

fn scroll_driven_effect_state(
  style: &ComputedStyle,
  idx: usize,
  overall: f32,
) -> AnimationProgress {
  let iteration_count = pick(
    &style.animation_iteration_counts,
    idx,
    AnimationIterationCount::default(),
  );
  let direction = pick(
    &style.animation_directions,
    idx,
    AnimationDirection::default(),
  );

  let iterations = iteration_count.as_f32();
  let start_progress = if iteration_reverses(direction, 0) {
    1.0
  } else {
    0.0
  };
  let end_progress = animation_end_progress(direction, iterations);
  let end_iteration = animation_end_iteration(iterations);

  // Scroll/view timelines are finite (progress-based) timelines. When converting an animation to
  // proportions for a finite timeline, the Web Animations Level 2 proportional timing algorithm
  // divides by the animation's end time. For `animation-iteration-count: infinite` the end time is
  // infinite, which yields 0 for all converted proportions. Scroll Animations Level 1 explicitly
  // calls this out for `animation-duration: auto` by defining the used iteration duration and
  // resulting active duration as 0.
  //
  // In practice, this means infinite-iteration scroll-driven animations should behave like a
  // 0-duration animation sampled at time 0 (i.e. the start of the first iteration) rather than
  // progressing across the scroll range.
  if !iterations.is_finite() {
    return AnimationProgress {
      progress: start_progress,
      iteration: 0,
    };
  }

  let overall = overall.clamp(0.0, 1.0);
  if overall <= f32::EPSILON {
    return AnimationProgress {
      progress: start_progress,
      iteration: 0,
    };
  }
  if overall >= 1.0 - f32::EPSILON {
    return AnimationProgress {
      progress: end_progress,
      iteration: end_iteration,
    };
  }

  let finite_iterations = if iterations.is_finite() {
    iterations.max(0.0)
  } else {
    1.0
  };
  let total = overall * finite_iterations;
  let iteration = total.floor() as u64;
  let iteration_progress = (total - iteration as f32).clamp(0.0, 1.0);
  let reversed = iteration_reverses(direction, iteration);
  let directed = if reversed {
    1.0 - iteration_progress
  } else {
    iteration_progress
  };
  AnimationProgress {
    progress: directed,
    iteration,
  }
}

fn scroll_progress_for_function(
  func: &ScrollFunctionTimeline,
  node: &FragmentNode,
  origin: Point,
  root: ScrollContainerContext,
  ancestor_scroll_containers: &[ScrollContainerContext],
  scroll_state: &ScrollState,
  range: &AnimationRange,
) -> Option<f32> {
  let scroll_container = match func.scroller {
    ScrollTimelineScroller::Root => root,
    ScrollTimelineScroller::Nearest => ancestor_scroll_containers.last().copied().unwrap_or(root),
    ScrollTimelineScroller::SelfElement => {
      scroll_container_context_for_node(node, origin, scroll_state)?
    }
  };

  let (scroll_pos, scroll_range, viewport_size) = axis_scroll_state(
    func.axis,
    scroll_container.writing_mode,
    scroll_container.direction,
    scroll_container.scroll.x,
    scroll_container.scroll.y,
    scroll_container.viewport.width,
    scroll_container.viewport.height,
    scroll_container.content.width,
    scroll_container.content.height,
  );

  let timeline = ScrollTimeline {
    axis: func.axis,
    ..ScrollTimeline::default()
  };

  scroll_timeline_progress(&timeline, scroll_pos, scroll_range, viewport_size, range)
}

fn view_progress_for_function(
  func: &ViewFunctionTimeline,
  node: &FragmentNode,
  origin: Point,
  root: ScrollContainerContext,
  ancestor_scroll_containers: &[ScrollContainerContext],
  scroll_state: &ScrollState,
  range: &AnimationRange,
) -> Option<(f32, ViewTimelineKeyframeResolver)> {
  let scroll_container = match func.scroller {
    ScrollTimelineScroller::Root => root,
    ScrollTimelineScroller::Nearest => ancestor_scroll_containers.last().copied().unwrap_or(root),
    ScrollTimelineScroller::SelfElement => {
      scroll_container_context_for_node(node, origin, scroll_state)?
    }
  };

  let abs = Rect::from_xywh(
    origin.x,
    origin.y,
    node.bounds.width(),
    node.bounds.height(),
  );
  let horizontal = axis_is_horizontal(func.axis, scroll_container.writing_mode);
  let target_start = if horizontal {
    abs.x() - scroll_container.origin.x
  } else {
    abs.y() - scroll_container.origin.y
  };
  let target_end = if horizontal {
    target_start + abs.width()
  } else {
    target_start + abs.height()
  };
  let view_size = if horizontal {
    scroll_container.viewport.width
  } else {
    scroll_container.viewport.height
  };
  let scroll_offset = if horizontal {
    scroll_container.scroll.x
  } else {
    scroll_container.scroll.y
  };
  let content_size = if horizontal {
    scroll_container.content.width
  } else {
    scroll_container.content.height
  };
  let (target_start, target_end, view_size, scroll_offset) = axis_view_state(
    func.axis,
    scroll_container.writing_mode,
    scroll_container.direction,
    target_start,
    target_end,
    scroll_offset,
    view_size,
    content_size,
  );

  let timeline = ViewTimeline {
    name: None,
    axis: func.axis,
    inset: Some(resolved_view_timeline_inset(
      func.inset,
      scroll_container,
      func.axis,
    )),
  };

  if !scroll_offset.is_finite() {
    return None;
  }
  let (start, end, view_ranges) =
    view_timeline_attachment_range(&timeline, target_start, target_end, view_size, range)?;
  let resolver = ViewTimelineKeyframeResolver {
    attachment_start: start,
    attachment_end: end,
    view_ranges,
  };
  Some((raw_progress(scroll_offset, start, end), resolver))
}

struct AnimationApplyContext<'a> {
  animation_time_ms: Option<f32>,
  state_store: Option<&'a mut AnimationStateStore>,
}

fn apply_animations_to_node_scoped(
  node: &mut FragmentNode,
  origin: Point,
  viewport: Rect,
  parent_styles: Option<&ComputedStyle>,
  root_context: ScrollContainerContext,
  scroll_state: &ScrollState,
  keyframes: &HashMap<String, KeyframesRule>,
  apply_ctx: &mut AnimationApplyContext<'_>,
  scope: &mut TimelineScope,
  ancestor_scroll_containers: &mut Vec<ScrollContainerContext>,
  plan: TimelineScopePlanNode,
) {
  let abs = Rect::from_xywh(
    origin.x,
    origin.y,
    node.bounds.width(),
    node.bounds.height(),
  );

  let TimelineScopePlanNode {
    promotions,
    children: child_plans,
    running_anchor_snapshot,
  } = plan;

  let mut pushed_names: Vec<String> = Vec::new();
  for (name, state) in promotions {
    timeline_scope_push(scope, name.clone(), state);
    pushed_names.push(name);
  }
  if let Some(style) = node.style.as_ref() {
    let view_timeline_context = ancestor_scroll_containers
      .last()
      .copied()
      .unwrap_or(root_context);
    for tl in &style.view_timelines {
      if let Some(name) = &tl.name {
        let horizontal = axis_is_horizontal(tl.axis, view_timeline_context.writing_mode);
        let target_start = if horizontal {
          abs.x() - view_timeline_context.origin.x
        } else {
          abs.y() - view_timeline_context.origin.y
        };
        let target_end = if horizontal {
          target_start + abs.width()
        } else {
          target_start + abs.height()
        };
        let view_size = if horizontal {
          view_timeline_context.viewport.width
        } else {
          view_timeline_context.viewport.height
        };
        let scroll_offset = if horizontal {
          view_timeline_context.scroll.x
        } else {
          view_timeline_context.scroll.y
        };
        let content_size = if horizontal {
          view_timeline_context.content.width
        } else {
          view_timeline_context.content.height
        };
        let (target_start, target_end, view_size, scroll_offset) = axis_view_state(
          tl.axis,
          view_timeline_context.writing_mode,
          view_timeline_context.direction,
          target_start,
          target_end,
          scroll_offset,
          view_size,
          content_size,
        );
        let mut resolved_timeline = tl.clone();
        resolved_timeline.inset = Some(resolved_view_timeline_inset(
          tl.inset,
          view_timeline_context,
          tl.axis,
        ));
        timeline_scope_push(
          scope,
          name.clone(),
          TimelineState::View {
            timeline: resolved_timeline,
            target_start,
            target_end,
            view_size,
            scroll_offset,
          },
        );
        pushed_names.push(name.clone());
      }
    }

    let scroll_timeline_context = timeline_scroll_context(node, origin, scroll_state, root_context);
    for tl in &style.scroll_timelines {
      if let Some(name) = &tl.name {
        let (scroll_pos, scroll_range, viewport_size) = axis_scroll_state(
          tl.axis,
          scroll_timeline_context.writing_mode,
          scroll_timeline_context.direction,
          scroll_timeline_context.scroll.x,
          scroll_timeline_context.scroll.y,
          scroll_timeline_context.viewport.width,
          scroll_timeline_context.viewport.height,
          scroll_timeline_context.content.width,
          scroll_timeline_context.content.height,
        );
        timeline_scope_push(
          scope,
          name.clone(),
          TimelineState::Scroll {
            timeline: tl.clone(),
            scroll_pos,
            scroll_range,
            viewport_size,
          },
        );
        pushed_names.push(name.clone());
      }
    }
  }

  if let Some(style_arc) = node.style.clone() {
    let names = &style_arc.animation_names;
    if !names.is_empty() {
      let timelines_list = &style_arc.animation_timelines;
      let ranges_list = &style_arc.animation_ranges;

      let mut animated = (*style_arc).clone();
      let original_color = style_arc.color;
      let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
      let mut changed = false;
      let mut custom_properties_changed = false;
      let viewport_size = Size::new(viewport.width(), viewport.height());
      let element_size = Size::new(node.bounds.width(), node.bounds.height());
      let resolve_ctx = AnimationResolveContext::new(viewport_size, element_size);
      let mut applied_value_sets: Vec<(AnimationComposition, HashMap<String, AnimatedValue>)> =
        Vec::new();

      for (idx, name) in names.iter().enumerate() {
        let Some(name) = name.as_ref() else { continue };
        let timeline_ref = pick(timelines_list, idx, AnimationTimeline::Auto);
        let range = pick(ranges_list, idx, AnimationRange::default());
        let play_state = pick(
          &style_arc.animation_play_states,
          idx,
          AnimationPlayState::Running,
        );
        let timing = pick(
          &style_arc.animation_timing_functions,
          idx,
          TransitionTimingFunction::Ease,
        );
        let direction = pick(
          &style_arc.animation_directions,
          idx,
          AnimationDirection::default(),
        );
        let composition = pick(
          &style_arc.animation_compositions,
          idx,
          AnimationComposition::default(),
        );

        let mut view_timeline_keyframe_resolver: Option<ViewTimelineKeyframeResolver> = None;
        let progress = match timeline_ref {
          AnimationTimeline::Auto => match apply_ctx.animation_time_ms {
            Some(timeline_time_ms) => {
              if let Some(store) = apply_ctx.state_store.as_deref_mut() {
                if let Some(box_id) = node.box_id() {
                  let current_time_ms = store.sample_time_based_animation(
                    box_id,
                    idx,
                    name,
                    timeline_time_ms,
                    play_state,
                  );
                  time_based_animation_state_impl(&*style_arc, idx, current_time_ms, false)
                } else {
                  time_based_animation_state_impl(&*style_arc, idx, timeline_time_ms, true)
                }
              } else {
                time_based_animation_state_impl(&*style_arc, idx, timeline_time_ms, true)
              }
            }
            None => settled_time_based_animation_state(&*style_arc, idx),
          },
          AnimationTimeline::None => None,
          AnimationTimeline::Named(ref timeline_name) => {
            if matches!(play_state, AnimationPlayState::Paused) {
              timeline_scope_resolve(scope, timeline_name)
                .and_then(|state| match state {
                  TimelineState::Scroll { scroll_range, .. } => {
                    (scroll_range.abs() >= f32::EPSILON).then_some(0.0)
                  }
                  TimelineState::View {
                    timeline,
                    target_start,
                    target_end,
                    view_size,
                    scroll_offset,
                  } => {
                    if !scroll_offset.is_finite() {
                      None
                    } else if let Some((start, end, view_ranges)) = view_timeline_attachment_range(
                      timeline,
                      *target_start,
                      *target_end,
                      *view_size,
                      &range,
                    ) {
                      view_timeline_keyframe_resolver = Some(ViewTimelineKeyframeResolver {
                        attachment_start: start,
                        attachment_end: end,
                        view_ranges,
                      });
                      Some(0.0)
                    } else {
                      None
                    }
                  }
                  TimelineState::Inactive => None,
                })
                .and_then(|overall| progress_based_animation_state(&*style_arc, idx, overall))
            } else {
              let raw =
                timeline_scope_resolve(scope, timeline_name).and_then(|state| match state {
                  TimelineState::Scroll {
                    timeline,
                    scroll_pos,
                    scroll_range,
                    viewport_size,
                  } => scroll_timeline_progress(
                    timeline,
                    *scroll_pos,
                    *scroll_range,
                    *viewport_size,
                    &range,
                  ),
                  TimelineState::View {
                    timeline,
                    target_start,
                    target_end,
                    view_size,
                    scroll_offset,
                  } => {
                    if !scroll_offset.is_finite() {
                      None
                    } else if let Some((start, end, view_ranges)) = view_timeline_attachment_range(
                      timeline,
                      *target_start,
                      *target_end,
                      *view_size,
                      &range,
                    ) {
                      view_timeline_keyframe_resolver = Some(ViewTimelineKeyframeResolver {
                        attachment_start: start,
                        attachment_end: end,
                        view_ranges,
                      });
                      Some(raw_progress(*scroll_offset, start, end))
                    } else {
                      None
                    }
                  }
                  TimelineState::Inactive => None,
                });
              let fill = pick(
                &style_arc.animation_fill_modes,
                idx,
                AnimationFillMode::default(),
              );
              raw
                .and_then(|raw| scroll_driven_fill_progress(raw, fill))
                .and_then(|overall| progress_based_animation_state(&*style_arc, idx, overall))
            }
          }
          AnimationTimeline::Scroll(ref func) => {
            if matches!(play_state, AnimationPlayState::Paused) {
              match func.scroller {
                ScrollTimelineScroller::Root => Some(root_context),
                ScrollTimelineScroller::Nearest => Some(
                  ancestor_scroll_containers
                    .last()
                    .copied()
                    .unwrap_or(root_context),
                ),
                ScrollTimelineScroller::SelfElement => {
                  scroll_container_context_for_node(node, origin, scroll_state)
                }
              }
              .and_then(|scroll_container| {
                let (_, scroll_range, _) = axis_scroll_state(
                  func.axis,
                  scroll_container.writing_mode,
                  scroll_container.direction,
                  0.0,
                  0.0,
                  scroll_container.viewport.width,
                  scroll_container.viewport.height,
                  scroll_container.content.width,
                  scroll_container.content.height,
                );
                (scroll_range.abs() >= f32::EPSILON)
                  .then_some(0.0)
                  .and_then(|overall| progress_based_animation_state(&*style_arc, idx, overall))
              })
            } else {
              let raw = scroll_progress_for_function(
                func,
                node,
                origin,
                root_context,
                ancestor_scroll_containers,
                scroll_state,
                &range,
              );
              let fill = pick(
                &style_arc.animation_fill_modes,
                idx,
                AnimationFillMode::default(),
              );
              raw
                .and_then(|raw| scroll_driven_fill_progress(raw, fill))
                .and_then(|overall| progress_based_animation_state(&*style_arc, idx, overall))
            }
          }
          AnimationTimeline::View(ref func) => {
            if matches!(play_state, AnimationPlayState::Paused) {
              match view_progress_for_function(
                func,
                node,
                origin,
                root_context,
                ancestor_scroll_containers,
                scroll_state,
                &range,
              ) {
                Some((_, resolver)) => {
                  view_timeline_keyframe_resolver = Some(resolver);
                  progress_based_animation_state(&*style_arc, idx, 0.0)
                }
                None => None,
              }
            } else {
              let raw = view_progress_for_function(
                func,
                node,
                origin,
                root_context,
                ancestor_scroll_containers,
                scroll_state,
                &range,
              );
              let fill = pick(
                &style_arc.animation_fill_modes,
                idx,
                AnimationFillMode::default(),
              );
              match raw {
                Some((raw, resolver)) => {
                  view_timeline_keyframe_resolver = Some(resolver);
                  scroll_driven_fill_progress(raw, fill)
                    .and_then(|overall| progress_based_animation_state(&*style_arc, idx, overall))
                }
                None => None,
              }
            }
          }
        };

        let Some(progress) = progress else { continue };

        if let Some(rule) = keyframes.get(name) {
          let mut sample = sample_keyframes_with_default_timing(
            rule,
            progress.progress,
            &animated,
            viewport_size,
            element_size,
            &timing,
            view_timeline_keyframe_resolver,
          );
          if matches!(composition, AnimationComposition::Accumulate)
            && progress.iteration > 0
            && !sample.animated.is_empty()
          {
            let (start_progress, end_progress) = if iteration_reverses(direction, 0) {
              (1.0, 0.0)
            } else {
              (0.0, 1.0)
            };
            let start = sample_keyframes_with_default_timing(
              rule,
              start_progress,
              &animated,
              viewport_size,
              element_size,
              &timing,
              view_timeline_keyframe_resolver,
            );
            let end = sample_keyframes_with_default_timing(
              rule,
              end_progress,
              &animated,
              viewport_size,
              element_size,
              &timing,
              view_timeline_keyframe_resolver,
            );
            apply_iteration_accumulation(
              &mut sample.animated,
              &start.animated,
              &end.animated,
              progress.iteration,
            );
          }

          for (name, maybe_value) in sample.custom_properties {
            match maybe_value {
              Some(value) => {
                let needs_update = animated
                  .custom_properties
                  .get(name.as_ref())
                  .map(|existing| existing != &value)
                  .unwrap_or(true);
                if needs_update {
                  animated.custom_properties.insert(name, value);
                  custom_properties_changed = true;
                  changed = true;
                }
              }
              None => {
                let key = name.as_ref();
                if animated.custom_properties.contains_key(key) {
                  animated.custom_properties.remove(key);
                  custom_properties_changed = true;
                  changed = true;
                }
              }
            }
          }

          if !sample.animated.is_empty() {
            apply_animated_properties_with_composition(
              &mut animated,
              &sample.animated,
              composition,
              &resolve_ctx,
            );
            applied_value_sets.push((composition, sample.animated));
            changed = true;
          }
        }
      }

      if custom_properties_changed {
        let mut recomputed = (*style_arc).clone();
        recomputed.custom_properties = animated.custom_properties.clone();
        recomputed.recompute_var_dependent_properties(parent_styles, viewport_size);
        animated = recomputed;
        for (composition, values) in &applied_value_sets {
          apply_animated_properties_with_composition(
            &mut animated,
            values,
            *composition,
            &resolve_ctx,
          );
        }
      }

      if animated.color != original_color {
        // Recompute `currentColor`-dependent cascade values against the new used color. This can
        // overwrite animated values (including via shorthands), so rebuild the animated style on
        // top of the recomputed base rather than applying the recomputation in-place (which could
        // also double-apply additive compositions).
        if !animated.current_color_dependent_declarations.is_empty() {
          let mut recomputed = (*style_arc).clone();
          if custom_properties_changed {
            recomputed.custom_properties = animated.custom_properties.clone();
            recomputed.recompute_var_dependent_properties(parent_styles, viewport_size);
          }
          recomputed.color = animated.color;
          recomputed.recompute_current_color_dependent_properties(parent_styles, viewport_size);
          animated = recomputed;
          for (composition, values) in &applied_value_sets {
            apply_animated_properties_with_composition(
              &mut animated,
              values,
              *composition,
              &resolve_ctx,
            );
          }
        } else {
          animated.recompute_current_color_dependent_properties(parent_styles, viewport_size);
        }
      }

      if changed {
        node.style = Some(Arc::new(animated));
      }
    }
  }

  let pushed_scroll_container = scroll_container_context_for_node(node, origin, scroll_state);
  if let Some(ctx) = pushed_scroll_container {
    ancestor_scroll_containers.push(ctx);
  }

  let parent_style = node.style.clone();
  let parent_for_children = parent_style
    .as_deref()
    .or(parent_styles)
    .unwrap_or_else(|| default_parent_style());
  let viewport_size = Size::new(viewport.width(), viewport.height());
  let mut child_plans = child_plans.into_iter();
  for child in node.children_mut() {
    let child_plan = child_plans.next().unwrap_or_default();
    if let Some(child_style_arc) = child.style.as_mut() {
      let child_style = Arc::make_mut(child_style_arc);
      child_style.recompute_inherited_custom_properties(parent_for_children);
      if child_style.color_is_inherited {
        child_style.color = parent_for_children.color;
      }
      child_style.recompute_var_dependent_properties(parent_for_children, viewport_size);
      if child_style.color_is_inherited {
        child_style.recompute_current_color_dependent_properties(parent_for_children, viewport_size);
      }
    }
    let child_offset = Point::new(origin.x + child.bounds.x(), origin.y + child.bounds.y());
      apply_animations_to_node_scoped(
        child,
        child_offset,
        viewport,
        Some(parent_for_children),
        root_context,
        scroll_state,
        keyframes,
        apply_ctx,
        scope,
        ancestor_scroll_containers,
        child_plan,
      );
  }
  debug_assert!(
    child_plans.next().is_none(),
    "timeline scope plan children must match fragment children"
  );
  if let FragmentContent::RunningAnchor { snapshot, .. } = &mut node.content {
    let snapshot_plan = running_anchor_snapshot
      .map(|plan| *plan)
      .unwrap_or_default();
    let snapshot_node = Arc::make_mut(snapshot);
    if let Some(snapshot_style_arc) = snapshot_node.style.as_mut() {
      let snapshot_style = Arc::make_mut(snapshot_style_arc);
      snapshot_style.recompute_inherited_custom_properties(parent_for_children);
      if snapshot_style.color_is_inherited {
        snapshot_style.color = parent_for_children.color;
      }
      snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport_size);
      if snapshot_style.color_is_inherited {
        snapshot_style.recompute_current_color_dependent_properties(
          parent_for_children,
          viewport_size,
        );
      }
    }
    let snapshot_offset = Point::new(
      origin.x + snapshot_node.bounds.x(),
      origin.y + snapshot_node.bounds.y(),
    );
    apply_animations_to_node_scoped(
      snapshot_node,
      snapshot_offset,
      viewport,
      Some(parent_for_children),
      root_context,
      scroll_state,
      keyframes,
      apply_ctx,
      scope,
      ancestor_scroll_containers,
      snapshot_plan,
    );
  }

  if pushed_scroll_container.is_some() {
    ancestor_scroll_containers.pop();
  }

  for name in pushed_names.iter().rev() {
    timeline_scope_pop(scope, name);
  }
}

/// Applies CSS animations to the fragment tree by sampling matching `@keyframes` rules and
/// applying animated properties (currently opacity).
///
/// Scroll- and view-timeline animations are always sampled using the provided scroll state.
///
/// Time-based animations (`animation-timeline: auto`) are sampled as follows:
/// - When `animation_time` is `Some`, animations are sampled at that timestamp, honoring per-
///   animation duration/delay/iteration-count/direction/fill-mode/play-state.
/// - When `animation_time` is `None`, time-based animations resolve to a deterministic settled
///   state: finite `animation-fill-mode: forwards|both` animations sample their end state, while
///   all other time-based animations have no effect (falling back to the underlying style).
pub fn apply_animations(
  tree: &mut FragmentTree,
  scroll_state: &ScrollState,
  animation_time: Option<Duration>,
) {
  if tree.keyframes.is_empty() {
    return;
  }

  let animation_time_ms = animation_time.map(|time| time.as_secs_f32() * 1000.0);
  let mut apply_ctx = AnimationApplyContext {
    animation_time_ms,
    state_store: None,
  };
  let viewport = Rect::from_xywh(
    0.0,
    0.0,
    tree.viewport_size().width,
    tree.viewport_size().height,
  );
  let content = tree.content_size();

  let root_writing_mode = tree
    .root
    .style
    .as_deref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  let root_direction = tree
    .root
    .style
    .as_deref()
    .map(|s| s.direction)
    .unwrap_or(Direction::Ltr);
  let (scroll_padding_top, scroll_padding_right, scroll_padding_bottom, scroll_padding_left) = tree
    .root
    .style
    .as_deref()
    .map(|s| {
      (
        s.scroll_padding_top,
        s.scroll_padding_right,
        s.scroll_padding_bottom,
        s.scroll_padding_left,
      )
    })
    .unwrap_or((
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
    ));
  let root_context = root_scroll_container_context(
    scroll_state,
    viewport,
    content,
    root_writing_mode,
    root_direction,
    scroll_padding_top,
    scroll_padding_right,
    scroll_padding_bottom,
    scroll_padding_left,
  );

  {
    let root_offset = Point::new(tree.root.bounds.x(), tree.root.bounds.y());
    let plan = build_timeline_scope_plan(&tree.root, root_offset, root_context, scroll_state);
    let mut scope = TimelineScope::new();
    let mut scroll_containers = Vec::new();
    apply_animations_to_node_scoped(
      &mut tree.root,
      root_offset,
      viewport,
      None,
      root_context,
      scroll_state,
      &tree.keyframes,
      &mut apply_ctx,
      &mut scope,
      &mut scroll_containers,
      plan,
    );
  }

  for frag in &mut tree.additional_fragments {
    let offset = Point::new(frag.bounds.x(), frag.bounds.y());
    let plan = build_timeline_scope_plan(&*frag, offset, root_context, scroll_state);
    let mut scope = TimelineScope::new();
    let mut scroll_containers = Vec::new();
    apply_animations_to_node_scoped(
      frag,
      offset,
      viewport,
      None,
      root_context,
      scroll_state,
      &tree.keyframes,
      &mut apply_ctx,
      &mut scope,
      &mut scroll_containers,
      plan,
    );
  }
}

/// Applies CSS animations to a fragment tree while persisting per-animation timing state across
/// frames.
///
/// This API is intended for multi-frame rendering pipelines that want time-based CSS animations
/// (`animation-timeline: auto`) to pause/resume correctly when `animation-play-state` changes. The
/// supplied `AnimationStateStore` should be kept and reused across frames.
pub fn apply_animations_with_state(
  tree: &mut FragmentTree,
  scroll_state: &ScrollState,
  animation_time: Duration,
  state: &mut AnimationStateStore,
) {
  state.begin_frame();
  if tree.keyframes.is_empty() {
    state.sweep();
    return;
  }

  let animation_time_ms = animation_time.as_secs_f32() * 1000.0;
  let viewport = Rect::from_xywh(
    0.0,
    0.0,
    tree.viewport_size().width,
    tree.viewport_size().height,
  );
  let content = tree.content_size();

  let root_writing_mode = tree
    .root
    .style
    .as_deref()
    .map(|s| s.writing_mode)
    .unwrap_or(WritingMode::HorizontalTb);
  let root_direction = tree
    .root
    .style
    .as_deref()
    .map(|s| s.direction)
    .unwrap_or(Direction::Ltr);
  let (scroll_padding_top, scroll_padding_right, scroll_padding_bottom, scroll_padding_left) = tree
    .root
    .style
    .as_deref()
    .map(|s| {
      (
        s.scroll_padding_top,
        s.scroll_padding_right,
        s.scroll_padding_bottom,
        s.scroll_padding_left,
      )
    })
    .unwrap_or((
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
      Length::px(0.0),
    ));
  let root_context = root_scroll_container_context(
    scroll_state,
    viewport,
    content,
    root_writing_mode,
    root_direction,
    scroll_padding_top,
    scroll_padding_right,
    scroll_padding_bottom,
    scroll_padding_left,
  );

  {
    let mut apply_ctx = AnimationApplyContext {
      animation_time_ms: Some(animation_time_ms),
      state_store: Some(state),
    };

    let root_offset = Point::new(tree.root.bounds.x(), tree.root.bounds.y());
    let plan = build_timeline_scope_plan(&tree.root, root_offset, root_context, scroll_state);
    let mut scope = TimelineScope::new();
    let mut scroll_containers = Vec::new();
    apply_animations_to_node_scoped(
      &mut tree.root,
      root_offset,
      viewport,
      None,
      root_context,
      scroll_state,
      &tree.keyframes,
      &mut apply_ctx,
      &mut scope,
      &mut scroll_containers,
      plan,
    );

    for frag in &mut tree.additional_fragments {
      let offset = Point::new(frag.bounds.x(), frag.bounds.y());
      let plan = build_timeline_scope_plan(&*frag, offset, root_context, scroll_state);
      let mut scope = TimelineScope::new();
      let mut scroll_containers = Vec::new();
      apply_animations_to_node_scoped(
        frag,
        offset,
        viewport,
        None,
        root_context,
        scroll_state,
        &tree.keyframes,
        &mut apply_ctx,
        &mut scope,
        &mut scroll_containers,
        plan,
      );
    }
  }

  state.sweep();
}

/// Applies scroll/view timeline-driven animations (and settles time-based animations) to a fragment
/// tree using the provided scroll state.
pub fn apply_scroll_driven_animations(tree: &mut FragmentTree, scroll_state: &ScrollState) {
  apply_animations(tree, scroll_state, None);
}

/// Returns the set of *longhand* property names that can participate in CSS transitions.
///
/// CSS Transitions Level 2 defines transitions in terms of an "expanded transition property name",
/// which is always a longhand property (or custom property). This means shorthands must be
/// expanded before generating transition effects.
fn transition_longhand_names() -> &'static [&'static str] {
  static NAMES: &[&str] = &[
    "opacity",
    "visibility",
    "color",
    "background-color",
    "transform",
    "translate",
    "rotate",
    "scale",
    "filter",
    "backdrop-filter",
    "clip-path",
    "clip",
    "transform-origin",
    "perspective-origin",
    "background-position",
    "mask-position",
    "background-size",
    "mask-size",
    "box-shadow",
    "text-shadow",
    "border-top-color",
    "border-right-color",
    "border-bottom-color",
    "border-left-color",
    "border-top-style",
    "border-right-style",
    "border-bottom-style",
    "border-left-style",
    "border-top-width",
    "border-right-width",
    "border-bottom-width",
    "border-left-width",
    "outline-color",
    "outline-style",
    "outline-width",
    "outline-offset",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
  ];
  NAMES
}

/// Expands a `transition-property` entry into the corresponding longhand names.
///
/// This is a minimal subset aligned to the properties supported by the animation system.
fn expand_transition_property_name<'a>(name: &'a str) -> SmallVec<[&'a str; 12]> {
  match name {
    "border" => smallvec::smallvec![
      "border-top-width",
      "border-right-width",
      "border-bottom-width",
      "border-left-width",
      "border-top-color",
      "border-right-color",
      "border-bottom-color",
      "border-left-color",
      "border-top-style",
      "border-right-style",
      "border-bottom-style",
      "border-left-style",
    ],
    "border-top" => smallvec::smallvec!["border-top-width", "border-top-color", "border-top-style"],
    "border-right" => smallvec::smallvec![
      "border-right-width",
      "border-right-color",
      "border-right-style",
    ],
    "border-bottom" => smallvec::smallvec![
      "border-bottom-width",
      "border-bottom-color",
      "border-bottom-style",
    ],
    "border-left" => smallvec::smallvec!["border-left-width", "border-left-color", "border-left-style"],
    "border-color" => smallvec::smallvec![
      "border-top-color",
      "border-right-color",
      "border-bottom-color",
      "border-left-color",
    ],
    "border-width" => smallvec::smallvec![
      "border-top-width",
      "border-right-width",
      "border-bottom-width",
      "border-left-width",
    ],
    "border-style" => smallvec::smallvec![
      "border-top-style",
      "border-right-style",
      "border-bottom-style",
      "border-left-style",
    ],
    "border-radius" => smallvec::smallvec![
      "border-top-left-radius",
      "border-top-right-radius",
      "border-bottom-right-radius",
      "border-bottom-left-radius",
    ],
    "outline" => smallvec::smallvec!["outline-color", "outline-style", "outline-width"],
    _ => smallvec::smallvec![name],
  }
}

fn transition_value_for_property(
  name: &str,
  idx: usize,
  allow_discrete: bool,
  style: &ComputedStyle,
  start_style: &ComputedStyle,
  durations: &[f32],
  delays: &[f32],
  timings: &[TransitionTimingFunction],
  time_ms: f32,
  ctx: &AnimationResolveContext,
) -> Option<(AnimatedValue, f32, f32, f32)> {
  transition_value_for_property_with_duration_override(
    name,
    idx,
    allow_discrete,
    style,
    start_style,
    durations,
    delays,
    timings,
    time_ms,
    ctx,
    None,
  )
}

fn transition_value_for_property_with_duration_override(
  name: &str,
  idx: usize,
  allow_discrete: bool,
  style: &ComputedStyle,
  start_style: &ComputedStyle,
  durations: &[f32],
  delays: &[f32],
  timings: &[TransitionTimingFunction],
  time_ms: f32,
  ctx: &AnimationResolveContext,
  duration_override_ms: Option<f32>,
) -> Option<(AnimatedValue, f32, f32, f32)> {
  let mut duration = pick(durations, idx, *durations.last().unwrap_or(&0.0));
  if let Some(override_ms) = duration_override_ms {
    duration = override_ms;
  }
  if duration <= 0.0 {
    return None;
  }
  let delay = pick(delays, idx, *delays.last().unwrap_or(&0.0));
  let elapsed = time_ms - delay;
  if elapsed >= duration {
    return None;
  }
  let raw_progress = if elapsed <= 0.0 {
    0.0
  } else {
    (elapsed / duration).clamp(0.0, 1.0)
  };
  let timing = pick(timings, idx, TransitionTimingFunction::Ease);
  let progress = timing.value_at(raw_progress);

  if !allow_discrete
    && matches!(
      name,
      "visibility"
        | "border-style"
        | "border-top-style"
        | "border-right-style"
        | "border-bottom-style"
        | "border-left-style"
        | "outline-style"
    )
  {
    // CSS Transitions Level 2: discrete transitions only run when explicitly enabled via
    // `transition-behavior: allow-discrete`.
    return None;
  }

  let Some(interpolator) = interpolator_for(name) else {
    return None;
  };
  let Some(from_val) = (interpolator.extract)(start_style, ctx) else {
    return None;
  };
  let Some(to_val) = (interpolator.extract)(style, ctx) else {
    return None;
  };

  let value = if allow_discrete {
    (interpolator.interpolate)(&from_val, &to_val, progress).or_else(|| {
      if progress >= 0.5 {
        Some(to_val.clone())
      } else {
        Some(from_val.clone())
      }
    })
  } else {
    let mut value = (interpolator.interpolate)(&from_val, &to_val, progress)?;

    // Suppress discrete sub-components for shorthands that include both interpolable and discrete
    // parts (e.g. `border` includes `border-*-style`).
    match (&mut value, &to_val) {
      (AnimatedValue::Border(_, styles, _), AnimatedValue::Border(_, to_styles, _))
        if matches!(
          name,
          "border" | "border-top" | "border-right" | "border-bottom" | "border-left"
        ) =>
      {
        *styles = *to_styles;
      }
      (
        AnimatedValue::Outline(color, outline_style, _),
        AnimatedValue::Outline(to_color, to_style, _),
      ) if name == "outline" => {
        *outline_style = *to_style;
        // Outline color interpolation is only continuous when both endpoints are explicit colors;
        // otherwise it is discrete and follows `transition-behavior`.
        if !matches!(
          (&from_val, &to_val),
          (
            AnimatedValue::Outline(OutlineColor::Color(_), _, _),
            AnimatedValue::Outline(OutlineColor::Color(_), _, _)
          )
        ) {
          *color = *to_color;
        }
      }
      _ => {}
    }

    Some(value)
  }?;

  Some((value, progress, delay, duration))
}

fn transition_value_for_custom_property(
  name: &str,
  idx: usize,
  allow_discrete: bool,
  style: &ComputedStyle,
  start_style: &ComputedStyle,
  durations: &[f32],
  delays: &[f32],
  timings: &[TransitionTimingFunction],
  time_ms: f32,
  ctx: &AnimationResolveContext,
) -> Option<(CustomPropertyValue, f32, f32, f32)> {
  transition_value_for_custom_property_with_duration_override(
    name,
    idx,
    allow_discrete,
    style,
    start_style,
    durations,
    delays,
    timings,
    time_ms,
    ctx,
    None,
  )
}

fn transition_value_for_custom_property_with_duration_override(
  name: &str,
  idx: usize,
  allow_discrete: bool,
  style: &ComputedStyle,
  start_style: &ComputedStyle,
  durations: &[f32],
  delays: &[f32],
  timings: &[TransitionTimingFunction],
  time_ms: f32,
  ctx: &AnimationResolveContext,
  duration_override_ms: Option<f32>,
) -> Option<(CustomPropertyValue, f32, f32, f32)> {
  let mut duration = pick(durations, idx, *durations.last().unwrap_or(&0.0));
  if let Some(override_ms) = duration_override_ms {
    duration = override_ms;
  }
  if duration <= 0.0 {
    return None;
  }
  let delay = pick(delays, idx, *delays.last().unwrap_or(&0.0));
  let elapsed = time_ms - delay;
  if elapsed >= duration {
    return None;
  }
  let raw_progress = if elapsed <= 0.0 {
    0.0
  } else {
    (elapsed / duration).clamp(0.0, 1.0)
  };
  let timing = pick(timings, idx, TransitionTimingFunction::Ease);
  let progress = timing.value_at(raw_progress);

  let from_val = start_style.custom_properties.get(name)?.clone();
  let to_val = style.custom_properties.get(name)?.clone();

  let can_interpolate = match (
    start_style.custom_property_registry.get(name),
    style.custom_property_registry.get(name),
  ) {
    (Some(from_rule), Some(to_rule))
      if from_rule.syntax == to_rule.syntax
        && !from_rule.syntax.is_universal() =>
    {
      true
    }
    _ => false,
  };

  let sampled = (can_interpolate
    .then(|| interpolate_custom_property(&from_val, &to_val, progress, start_style, style, ctx))
    .flatten())
  .or_else(|| {
    if allow_discrete {
      if progress >= 0.5 {
        Some(to_val.clone())
      } else {
        Some(from_val.clone())
      }
    } else {
      None
    }
  })?;

  Some((sampled, progress, delay, duration))
}

fn transition_pairs<'a>(
  properties: &'a [TransitionProperty],
  start_style: &'a ComputedStyle,
  style: &'a ComputedStyle,
) -> Option<Vec<(&'a str, usize)>> {
  let mut has_all = false;
  for prop in properties {
    match prop {
      TransitionProperty::None => return None,
      TransitionProperty::All => has_all = true,
      TransitionProperty::Name(_) => {}
    }
  }

  // When `transition-property` contains `all`, we need to include custom properties in addition to
  // the built-in longhand properties. Custom property iteration order is not stable, so sort the
  // candidate list to keep transition sampling deterministic.
  let mut all_custom_properties: Vec<&'a str> = Vec::new();
  if has_all {
    all_custom_properties = start_style
      .custom_properties
      .iter()
      .filter_map(|(name, _)| {
        let name = name.as_ref();
        if style.custom_properties.contains_key(name) {
          Some(name)
        } else {
          None
        }
      })
      .collect();
    all_custom_properties.sort_unstable();
  }

  // Deduplicate by property name so the last entry in `transition-property` wins, including the
  // duration/delay/timing-function indexed by that entry. This also avoids wasted interpolation
  // work when `all` and explicit names overlap.
  let mut order = 0usize;
  let mut map: HashMap<&'a str, (usize, usize)> = HashMap::new();
  for (idx, prop) in properties.iter().enumerate() {
    let mut insert = |name: &'a str| {
      map.insert(name, (idx, order));
      order = order.saturating_add(1);
    };

    match prop {
      TransitionProperty::All => {
        for name in transition_longhand_names() {
          insert(name);
        }
        for name in &all_custom_properties {
          insert(name);
        }
      }
      TransitionProperty::Name(name) => {
        let name = name.as_str();
        if name.starts_with("--") {
          insert(name);
        } else {
          let expanded = expand_transition_property_name(name);
          for name in expanded {
            insert(name);
          }
        }
      }
      TransitionProperty::None => {}
    }
  }

  let mut ordered: Vec<(&'a str, usize, usize)> = map
    .into_iter()
    .map(|(name, (idx, order))| (name, idx, order))
    .collect();
  ordered.sort_by_key(|(_, _, order)| *order);
  Some(
    ordered
      .into_iter()
      .map(|(name, idx, _)| (name, idx))
      .collect(),
  )
}

fn apply_transitions_to_fragment(
  fragment: &mut FragmentNode,
  time_ms: f32,
  viewport: Size,
  log_enabled: bool,
  parent_styles: Option<&ComputedStyle>,
) {
  let Some(style_arc) = fragment.style.clone() else {
    // Still traverse for running anchors/children. Even without a style on this node, descendants
    // may inherit from `parent_styles`, so mirror the inherited-property and var recomputation we
    // do for styled nodes.
    let parent_for_children = parent_styles.unwrap_or_else(|| default_parent_style());
    for child in fragment.children_mut() {
      if let Some(child_style_arc) = child.style.as_mut() {
        let child_style = Arc::make_mut(child_style_arc);
        if child_style.color_is_inherited && child_style.color != parent_for_children.color {
          child_style.color = parent_for_children.color;
        }
        if child_style.visibility_is_inherited
          && child_style.visibility != parent_for_children.visibility
        {
          child_style.visibility = parent_for_children.visibility;
        }
        child_style.recompute_inherited_custom_properties(parent_for_children);
        child_style.recompute_var_dependent_properties(parent_for_children, viewport);
      }
      apply_transitions_to_fragment(
        child,
        time_ms,
        viewport,
        log_enabled,
        Some(parent_for_children),
      );
    }
    if let FragmentContent::RunningAnchor { snapshot, .. } = &mut fragment.content {
      let snapshot_node = Arc::make_mut(snapshot);
      if let Some(snapshot_style_arc) = snapshot_node.style.as_mut() {
        let snapshot_style = Arc::make_mut(snapshot_style_arc);
        if snapshot_style.color_is_inherited && snapshot_style.color != parent_for_children.color {
          snapshot_style.color = parent_for_children.color;
        }
        if snapshot_style.visibility_is_inherited
          && snapshot_style.visibility != parent_for_children.visibility
        {
          snapshot_style.visibility = parent_for_children.visibility;
        }
        snapshot_style.recompute_inherited_custom_properties(parent_for_children);
        snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport);
      }
      apply_transitions_to_fragment(
        snapshot_node,
        time_ms,
        viewport,
        log_enabled,
        Some(parent_for_children),
      );
    }
    return;
  };

  let start_arc = fragment.starting_style.clone();
  let start_time_ms = 0.0;
  let duration_overrides_ms: Option<&HashMap<String, f32>> = None;

  if let Some(start_arc) = start_arc {
    let time_ms = (time_ms - start_time_ms).max(0.0);
    if let Some(pairs) = transition_pairs(&style_arc.transition_properties, &start_arc, &style_arc)
    {
      let ctx = AnimationResolveContext::new(
        viewport,
        Size::new(fragment.bounds.width(), fragment.bounds.height()),
      );
      let mut updates: Vec<(String, AnimatedValue)> = Vec::new();
      let mut custom_updates: Vec<(Arc<str>, CustomPropertyValue)> = Vec::new();
      for (name, idx) in pairs {
        let name_str = name;
        let behavior = pick(
          &style_arc.transition_behaviors,
          idx,
          TransitionBehavior::Normal,
        );
        let allow_discrete = matches!(behavior, TransitionBehavior::AllowDiscrete);
        let duration_override_ms =
          duration_overrides_ms.and_then(|map| map.get(name_str)).copied();

        if name_str.starts_with("--") {
          let value = transition_value_for_custom_property_with_duration_override(
            name_str,
            idx,
            allow_discrete,
            &style_arc,
            &start_arc,
            &style_arc.transition_durations,
            &style_arc.transition_delays,
            &style_arc.transition_timing_functions,
            time_ms,
            &ctx,
            duration_override_ms,
          );
          if let Some((sampled, progress, delay, duration)) = value {
            custom_updates.push((Arc::from(name_str), sampled));
            if log_enabled {
              let identifier = fragment
                .box_id()
                .map(|id| format!("box_id={id}"))
                .unwrap_or_else(|| "box_id=<none>".to_string());
              eprintln!(
                "[transition] {} property={} progress={:.3} delay_ms={:.1} duration_ms={:.1}",
                identifier, name_str, progress, delay, duration
              );
            }
          }
          continue;
        }

        let value = transition_value_for_property_with_duration_override(
          name_str,
          idx,
          allow_discrete,
          &style_arc,
          &start_arc,
          &style_arc.transition_durations,
          &style_arc.transition_delays,
          &style_arc.transition_timing_functions,
          time_ms,
          &ctx,
          duration_override_ms,
        );
        if let Some((animated, progress, delay, duration)) = value {
          updates.push((name_str.to_string(), animated));
          if log_enabled {
            let identifier = fragment
              .box_id()
              .map(|id| format!("box_id={id}"))
              .unwrap_or_else(|| "box_id=<none>".to_string());
            eprintln!(
              "[transition] {} property={} progress={:.3} delay_ms={:.1} duration_ms={:.1}",
              identifier, name_str, progress, delay, duration
            );
          }
        }
      }

      if !updates.is_empty() || !custom_updates.is_empty() {
        let color_changed = updates.iter().any(|(name, _)| name == "color");
        let original_color = style_arc.color;
        let mut updated_style = (*style_arc).clone();
        apply_animated_properties_ordered(&mut updated_style, &updates);
        let mut custom_properties_changed = false;
        for (name, value) in custom_updates {
          let needs_update = updated_style
            .custom_properties
            .get(name.as_ref())
            .map(|existing| existing != &value)
            .unwrap_or(true);
          if needs_update {
            updated_style.custom_properties.insert(name, value);
            custom_properties_changed = true;
          }
        }

        // Some computed values depend on `currentColor` (and are recorded in
        // `var_dependent_declarations`). When animating `color`, those dependent values must be
        // re-resolved so `currentColor` tracks the animated value.
        if custom_properties_changed || color_changed {
          let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
          recompute_var_dependent_properties_preserving_animated_color(
            &mut updated_style,
            parent_styles,
            viewport,
            color_changed,
          );
          apply_animated_properties_ordered(&mut updated_style, &updates);
        }

        if updated_style.color != original_color {
          let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
          updated_style.recompute_current_color_dependent_properties(parent_styles, viewport);
          // `recompute_current_color_dependent_properties` reapplies cached cascade declarations,
          // which can touch properties that are also being transitioned. Reapply transition updates
          // afterward so the transition layer continues to win over the underlying cascade.
          apply_animated_properties_ordered(&mut updated_style, &updates);
        }
        fragment.style = Some(Arc::new(updated_style));
      }
    }
  }

  let parent_style = fragment.style.clone();
  let parent_for_children = parent_style
    .as_deref()
    .or(parent_styles)
    .unwrap_or_else(|| default_parent_style());
  for child in fragment.children_mut() {
    if let Some(child_style_arc) = child.style.as_mut() {
      let child_style = Arc::make_mut(child_style_arc);
      if child_style.color_is_inherited && child_style.color != parent_for_children.color {
        child_style.color = parent_for_children.color;
      }
      if child_style.visibility_is_inherited
        && child_style.visibility != parent_for_children.visibility
      {
        child_style.visibility = parent_for_children.visibility;
      }
      child_style.recompute_inherited_custom_properties(parent_for_children);
      if child_style.color_is_inherited {
        child_style.color = parent_for_children.color;
      }
      child_style.recompute_var_dependent_properties(parent_for_children, viewport);
      if child_style.color_is_inherited {
        child_style.recompute_current_color_dependent_properties(parent_for_children, viewport);
      }
    }
    apply_transitions_to_fragment(
      child,
      time_ms,
      viewport,
      log_enabled,
      Some(parent_for_children),
    );
  }
  if let FragmentContent::RunningAnchor { snapshot, .. } = &mut fragment.content {
    let snapshot_node = Arc::make_mut(snapshot);
    if let Some(snapshot_style_arc) = snapshot_node.style.as_mut() {
      let snapshot_style = Arc::make_mut(snapshot_style_arc);
      if snapshot_style.color_is_inherited && snapshot_style.color != parent_for_children.color {
        snapshot_style.color = parent_for_children.color;
      }
      if snapshot_style.visibility_is_inherited
        && snapshot_style.visibility != parent_for_children.visibility
      {
        snapshot_style.visibility = parent_for_children.visibility;
      }
      snapshot_style.recompute_inherited_custom_properties(parent_for_children);
      if snapshot_style.color_is_inherited {
        snapshot_style.color = parent_for_children.color;
      }
      snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport);
      if snapshot_style.color_is_inherited {
        snapshot_style.recompute_current_color_dependent_properties(parent_for_children, viewport);
      }
    }
    apply_transitions_to_fragment(
      snapshot_node,
      time_ms,
      viewport,
      log_enabled,
      Some(parent_for_children),
    );
  }
}

fn apply_transition_state_to_fragment(
  fragment: &mut FragmentNode,
  transition_state: &TransitionState,
  time_ms: f32,
  viewport: Size,
  log_enabled: bool,
  parent_styles: Option<&ComputedStyle>,
) {
  let Some(style_arc) = fragment.style.clone() else {
    // Still traverse for running anchors/children. Even without a style on this node, descendants
    // may inherit from `parent_styles`, so mirror the inherited-property and var recomputation we
    // do for styled nodes.
    let parent_for_children = parent_styles.unwrap_or_else(|| default_parent_style());
    for child in fragment.children_mut() {
      if let Some(child_style_arc) = child.style.as_mut() {
        let child_style = Arc::make_mut(child_style_arc);
        if child_style.color_is_inherited && child_style.color != parent_for_children.color {
          child_style.color = parent_for_children.color;
        }
        if child_style.visibility_is_inherited
          && child_style.visibility != parent_for_children.visibility
        {
          child_style.visibility = parent_for_children.visibility;
        }
        child_style.recompute_inherited_custom_properties(parent_for_children);
        child_style.recompute_var_dependent_properties(parent_for_children, viewport);
      }
      apply_transition_state_to_fragment(
        child,
        transition_state,
        time_ms,
        viewport,
        log_enabled,
        Some(parent_for_children),
      );
    }
    if let FragmentContent::RunningAnchor { snapshot, .. } = &mut fragment.content {
      let snapshot_node = Arc::make_mut(snapshot);
      if let Some(snapshot_style_arc) = snapshot_node.style.as_mut() {
        let snapshot_style = Arc::make_mut(snapshot_style_arc);
        if snapshot_style.color_is_inherited && snapshot_style.color != parent_for_children.color {
          snapshot_style.color = parent_for_children.color;
        }
        if snapshot_style.visibility_is_inherited
          && snapshot_style.visibility != parent_for_children.visibility
        {
          snapshot_style.visibility = parent_for_children.visibility;
        }
        snapshot_style.recompute_inherited_custom_properties(parent_for_children);
        snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport);
      }
      apply_transition_state_to_fragment(
        snapshot_node,
        transition_state,
        time_ms,
        viewport,
        log_enabled,
        Some(parent_for_children),
      );
    }
    return;
  };

  if let Some(box_id) = fragment.box_id() {
    if let Some(key) = transition_state.box_to_element.get(&box_id) {
      if let Some(element) = transition_state.elements.get(key) {
        let ctx = AnimationResolveContext::new(
          viewport,
          Size::new(fragment.bounds.width(), fragment.bounds.height()),
        );
        // Apply sampled transition updates deterministically. Transition records are stored in
        // hash maps, but HashMap iteration order is nondeterministic and can lead to flaky results
        // when multiple properties overlap (e.g. borders/outlines) or when debug output is used for
        // comparisons.
        let mut ordered_running: Vec<(&Arc<str>, &transitions::TransitionRecord)> =
          element.running.iter().collect();
        ordered_running.sort_by(|(a, _), (b, _)| a.as_ref().cmp(b.as_ref()));

        let mut updates: Vec<(String, AnimatedValue)> = Vec::new();
        let mut custom_updates: Vec<(Arc<str>, CustomPropertyValue)> = Vec::new();
        for (name_arc, record) in ordered_running {
          let name = name_arc.as_ref();
          let Some(sample) = record.sample(time_ms, &ctx) else {
            continue;
          };
          match sample.value {
            transitions::TransitionValue::Builtin(animated) => {
              updates.push((name.to_string(), animated));
            }
            transitions::TransitionValue::Custom(value) => {
              custom_updates.push((name_arc.clone(), value));
            }
          }

          if log_enabled {
            let identifier = fragment
              .box_id()
              .map(|id| format!("box_id={id}"))
              .unwrap_or_else(|| "box_id=<none>".to_string());
            eprintln!(
              "[transition] {} property={} progress={:.3} delay_ms={:.1} duration_ms={:.1}",
              identifier,
              name,
              sample.progress,
              sample.delay_ms,
              sample.duration_ms
            );
          }
        }

        if !updates.is_empty() || !custom_updates.is_empty() {
          let color_changed = updates.iter().any(|(name, _)| name == "color");
          let mut updated_style = (*style_arc).clone();
          apply_animated_properties_ordered(&mut updated_style, &updates);
          let mut custom_properties_changed = false;
          for (name, value) in custom_updates {
            let needs_update = updated_style
              .custom_properties
              .get(name.as_ref())
              .map(|existing| existing != &value)
              .unwrap_or(true);
            if needs_update {
              updated_style.custom_properties.insert(name, value);
              custom_properties_changed = true;
            }
          }

          // Like `@starting-style` sampling, ensure `currentColor`-dependent declarations are
          // re-resolved when `color` animates via the persistent TransitionState engine.
          if custom_properties_changed || color_changed {
            let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
            recompute_var_dependent_properties_preserving_animated_color(
              &mut updated_style,
              parent_styles,
              viewport,
              color_changed,
            );
            apply_animated_properties_ordered(&mut updated_style, &updates);
          }
          fragment.style = Some(Arc::new(updated_style));
        }
      }
    }
  }

  let parent_style = fragment.style.clone();
  let parent_for_children = parent_style
    .as_deref()
    .or(parent_styles)
    .unwrap_or_else(|| default_parent_style());
  for child in fragment.children_mut() {
    if let Some(child_style_arc) = child.style.as_mut() {
      let child_style = Arc::make_mut(child_style_arc);
      if child_style.color_is_inherited && child_style.color != parent_for_children.color {
        child_style.color = parent_for_children.color;
      }
      if child_style.visibility_is_inherited
        && child_style.visibility != parent_for_children.visibility
      {
        child_style.visibility = parent_for_children.visibility;
      }
      child_style.recompute_inherited_custom_properties(parent_for_children);
      child_style.recompute_var_dependent_properties(parent_for_children, viewport);
    }
    apply_transition_state_to_fragment(
      child,
      transition_state,
      time_ms,
      viewport,
      log_enabled,
      Some(parent_for_children),
    );
  }
  if let FragmentContent::RunningAnchor { snapshot, .. } = &mut fragment.content {
    let snapshot_node = Arc::make_mut(snapshot);
    if let Some(snapshot_style_arc) = snapshot_node.style.as_mut() {
      let snapshot_style = Arc::make_mut(snapshot_style_arc);
      if snapshot_style.color_is_inherited && snapshot_style.color != parent_for_children.color {
        snapshot_style.color = parent_for_children.color;
      }
      if snapshot_style.visibility_is_inherited
        && snapshot_style.visibility != parent_for_children.visibility
      {
        snapshot_style.visibility = parent_for_children.visibility;
      }
      snapshot_style.recompute_inherited_custom_properties(parent_for_children);
      snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport);
    }
    apply_transition_state_to_fragment(
      snapshot_node,
      transition_state,
      time_ms,
      viewport,
      log_enabled,
      Some(parent_for_children),
    );
  }
}

/// Applies `@starting-style` transitions to a fragment tree for the given timestamp in milliseconds.
///
/// Transitions are sampled before scroll/view timeline animations so later animation
/// sampling can override any overlapping properties when both are present.
pub fn apply_transitions(tree: &mut FragmentTree, time_ms: f32, viewport: Size) {
  if time_ms < 0.0 {
    return;
  }
  let log_enabled = runtime::runtime_toggles().truthy("FASTR_LOG_TRANSITIONS");
  if let Some(state) = tree.transition_state.as_deref() {
    apply_transition_state_to_fragment(&mut tree.root, state, time_ms, viewport, log_enabled, None);
    for root in &mut tree.additional_fragments {
      apply_transition_state_to_fragment(root, state, time_ms, viewport, log_enabled, None);
    }
  } else {
    apply_transitions_to_fragment(&mut tree.root, time_ms, viewport, log_enabled, None);
    for root in &mut tree.additional_fragments {
      apply_transitions_to_fragment(root, time_ms, viewport, log_enabled, None);
    }
  }
}

trait AnimationRangeExt {
  fn start(&self) -> &RangeOffset;
  fn end(&self) -> &RangeOffset;
}

impl AnimationRangeExt for AnimationRange {
  fn start(&self) -> &RangeOffset {
    &self.start
  }

  fn end(&self) -> &RangeOffset {
    &self.end
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::parser::parse_stylesheet;
  use crate::css::types::CssRule;
  use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
  use crate::image_output::{encode_image, OutputFormat};
  use crate::style::cascade::apply_styles;
  use crate::style::display::FormattingContextType;
  use crate::style::media::MediaContext;
  use crate::text::font_db::FontConfig;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use crate::{FastRender, RenderOptions, ResourcePolicy};
  use image::ImageFormat;
  use std::fs;
  use std::path::{Path, PathBuf};
  use url::Url;

  fn fade_rule() -> KeyframesRule {
    let sheet =
      parse_stylesheet("@keyframes fade { 0% { opacity: 0; } 100% { opacity: 1; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    keyframes[0].clone()
  }

  fn sampled_opacity(rule: &KeyframesRule, progress: f32) -> f32 {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("opacity") {
      Some(AnimatedValue::Opacity(v)) => *v,
      other => panic!("expected opacity, got {other:?}"),
    }
  }

  fn sampled_opacity_with_timing(
    rule: &KeyframesRule,
    progress: f32,
    timing: &TransitionTimingFunction,
  ) -> f32 {
    let values = sample_keyframes_with_default_timing(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
      timing,
      None,
    )
    .animated;
    match values.get("opacity") {
      Some(AnimatedValue::Opacity(v)) => *v,
      other => panic!("expected opacity, got {other:?}"),
    }
  }

  fn sampled_opacity_with_base_opacity(
    rule: &KeyframesRule,
    progress: f32,
    base_opacity: f32,
  ) -> f32 {
    let mut base_style = ComputedStyle::default();
    base_style.opacity = base_opacity;
    let values = sample_keyframes(
      rule,
      progress,
      &base_style,
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("opacity") {
      Some(AnimatedValue::Opacity(v)) => *v,
      other => panic!("expected opacity, got {other:?}"),
    }
  }

  fn sampled_visibility(rule: &KeyframesRule, progress: f32) -> Visibility {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("visibility") {
      Some(AnimatedValue::Visibility(v)) => *v,
      other => panic!("expected visibility, got {other:?}"),
    }
  }

  fn sampled_translate(rule: &KeyframesRule, progress: f32, element_size: Size) -> TranslateValue {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      element_size,
    );
    match values.get("translate") {
      Some(AnimatedValue::Translate(v)) => *v,
      other => panic!("expected translate, got {other:?}"),
    }
  }

  fn sampled_rotate(rule: &KeyframesRule, progress: f32) -> RotateValue {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("rotate") {
      Some(AnimatedValue::Rotate(v)) => *v,
      other => panic!("expected rotate, got {other:?}"),
    }
  }

  fn sampled_scale(rule: &KeyframesRule, progress: f32) -> ScaleValue {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("scale") {
      Some(AnimatedValue::Scale(v)) => *v,
      other => panic!("expected scale, got {other:?}"),
    }
  }

  fn sampled_transform_translate_x(rule: &KeyframesRule, progress: f32) -> f32 {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("transform") {
      Some(AnimatedValue::Transform(list)) => compose_transform_list(list).m[12],
      other => panic!("expected transform, got {other:?}"),
    }
  }

  fn sampled_filter_len(rule: &KeyframesRule, progress: f32) -> usize {
    let values = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("filter") {
      Some(AnimatedValue::Filter(filters)) => filters.len(),
      other => panic!("expected filter, got {other:?}"),
    }
  }

  #[test]
  fn sample_keyframes_resolves_light_dark_using_base_used_color_scheme() {
    let sheet = parse_stylesheet(
      "@keyframes k {
        0% { color: light-dark(red, blue); }
        100% { color: light-dark(green, red); }
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let mut base_style = ComputedStyle::default();
    base_style.used_dark_color_scheme = true;

    let values = sample_keyframes(
      rule,
      0.0,
      &base_style,
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("color") {
      Some(AnimatedValue::Color(v)) => assert_eq!(*v, Rgba::BLUE),
      other => panic!("expected color, got {other:?}"),
    }

    let values = sample_keyframes(
      rule,
      1.0,
      &base_style,
      Size::new(800.0, 600.0),
      Size::new(100.0, 100.0),
    );
    match values.get("color") {
      Some(AnimatedValue::Color(v)) => assert_eq!(*v, Rgba::RED),
      other => panic!("expected color, got {other:?}"),
    }
  }

  #[test]
  fn time_based_animation_fill_forwards_applies_after_end() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_delays = vec![500.0].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();

    assert_eq!(time_based_animation_progress(&style, 0, 0.0), None);

    let progress = time_based_animation_progress(&style, 0, 2500.0).expect("filled");
    assert!((progress - 1.0).abs() < 1e-6);
    assert!((sampled_opacity(&rule, progress) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn time_based_animation_fill_backwards_respects_reverse_direction() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_delays = vec![500.0].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.animation_fill_modes = vec![AnimationFillMode::Backwards].into();
    style.animation_directions = vec![AnimationDirection::Reverse].into();

    let progress = time_based_animation_progress(&style, 0, 0.0).expect("filled");
    assert!((progress - 1.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn time_based_animation_alternate_ends_on_start_state() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Count(2.0)].into();
    style.animation_directions = vec![AnimationDirection::Alternate].into();
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();

    let progress = time_based_animation_progress(&style, 0, 2500.0).expect("filled");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn time_based_animation_paused_samples_at_start_time() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    let progress = time_based_animation_progress(&style, 0, 500.0).expect("active");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn time_based_animation_state_store_freezes_and_resumes_when_play_state_changes() {
    let rule = fade_rule();

    let mut store = AnimationStateStore::new();

    let tree_with_play_state = |play_state: AnimationPlayState| -> FragmentTree {
      let mut style = ComputedStyle::default();
      style.animation_names = vec![Some("fade".to_string())];
      style.animation_durations = vec![1000.0].into();
      style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
      style.animation_play_states = vec![play_state].into();

      let style = Arc::new(style);
      let mut animated =
        FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), 1, vec![]);
      animated.style = Some(style);

      let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![animated]);
      let mut tree = FragmentTree::new(root);
      tree.keyframes.insert(rule.name.clone(), rule.clone());
      tree
    };

    let sample = |store: &mut AnimationStateStore,
                  play_state: AnimationPlayState,
                  time_ms: u64|
     -> f32 {
      let mut tree = tree_with_play_state(play_state);
      apply_animations_with_state(
        &mut tree,
        &ScrollState::default(),
        Duration::from_millis(time_ms),
        store,
      );
      tree.root.children[0]
        .style
        .as_ref()
        .expect("animated style present")
        .opacity
    };

    let opacity_0 = sample(&mut store, AnimationPlayState::Running, 0);
    assert!((opacity_0 - 0.0).abs() < 1e-6, "opacity_0={opacity_0}");

    let opacity_500 = sample(&mut store, AnimationPlayState::Running, 500);
    assert!((opacity_500 - 0.5).abs() < 1e-3, "opacity_500={opacity_500}");

    let opacity_600 = sample(&mut store, AnimationPlayState::Paused, 600);
    assert!((opacity_600 - 0.6).abs() < 1e-3, "opacity_600={opacity_600}");

    let opacity_900 = sample(&mut store, AnimationPlayState::Paused, 900);
    assert!((opacity_900 - 0.6).abs() < 1e-3, "opacity_900={opacity_900}");

    let opacity_1000 = sample(&mut store, AnimationPlayState::Running, 1000);
    assert!((opacity_1000 - 0.6).abs() < 1e-3, "opacity_1000={opacity_1000}");

    let opacity_1100 = sample(&mut store, AnimationPlayState::Running, 1100);
    assert!((opacity_1100 - 0.7).abs() < 1e-3, "opacity_1100={opacity_1100}");
  }

  #[test]
  fn animation_name_none_keyword_does_not_match_quoted_keyframes_name() {
    let sheet =
      parse_stylesheet("@keyframes \"None\" { 0% { opacity: 0; } 100% { opacity: 0; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    assert_eq!(keyframes.len(), 1);
    let rule = keyframes[0].clone();
    assert_eq!(rule.name, "None");

    let defaults = ComputedStyle::default();
    let mut style = ComputedStyle::default();
    let decl_name = crate::css::types::Declaration {
      property: "animation-name".into(),
      value: PropertyValue::Keyword("None, missing".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    };
    apply_declaration_with_base(
      &mut style,
      &decl_name,
      &defaults,
      &defaults,
      None,
      defaults.font_size,
      defaults.root_font_size,
      Size::new(800.0, 600.0),
      false,
    );
    let decl_duration = crate::css::types::Declaration {
      property: "animation-duration".into(),
      value: PropertyValue::Keyword("1s".into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    };
    apply_declaration_with_base(
      &mut style,
      &decl_duration,
      &defaults,
      &defaults,
      None,
      defaults.font_size,
      defaults.root_font_size,
      Size::new(800.0, 600.0),
      false,
    );

    assert_eq!(
      style.animation_names,
      vec![None, Some("missing".to_string())]
    );

    let style = Arc::new(style);
    let mut animated = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![]);
    animated.style = Some(style);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![animated]);
    let mut tree = FragmentTree::new(root);
    tree.keyframes.insert(rule.name.clone(), rule);

    apply_animations(
      &mut tree,
      &ScrollState::default(),
      Some(Duration::from_millis(500)),
    );

    let opacity = tree.root.children[0]
      .style
      .as_ref()
      .expect("animated style present")
      .opacity;
    assert!(
      (opacity - 1.0).abs() < 1e-6,
      "keyword none should not match @keyframes \"None\", opacity={opacity}"
    );
  }

  #[test]
  fn time_based_animation_progress_does_not_warp_keyframe_boundaries() {
    let sheet = parse_stylesheet(
      "@keyframes k { 0% { opacity: 0; } 50% { opacity: 0.5; } 100% { opacity: 1; } }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let timing = TransitionTimingFunction::EaseIn;

    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("k".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_timing_functions = vec![timing.clone()].into();

    let progress = time_based_animation_progress(&style, 0, 500.0).expect("active");
    assert!((progress - 0.5).abs() < 1e-6, "progress={progress}");
    assert!(
      (sampled_opacity_with_timing(rule, progress, &timing) - 0.5).abs() < 1e-6,
      "opacity should match the 50% keyframe when sampling at 50% progress",
    );
  }

  #[test]
  fn sample_keyframes_applies_keyframe_timing_functions_per_interval() {
    let sheet = parse_stylesheet(
      "@keyframes k {
        0% { opacity: 0; animation-timing-function: steps(2, end); }
        40% { opacity: 0.4; }
        60% { opacity: 0.6; animation-timing-function: steps(5, end); }
        100% { opacity: 1; }
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let overall = TransitionTimingFunction::Linear;

    let opacity_early = sampled_opacity_with_timing(rule, 0.1, &overall);
    assert!(
      (opacity_early - 0.0).abs() < 1e-6,
      "opacity_early={opacity_early}"
    );

    let opacity_middle = sampled_opacity_with_timing(rule, 0.45, &overall);
    assert!(
      (opacity_middle - 0.45).abs() < 1e-6,
      "opacity_middle={opacity_middle}"
    );

    let opacity_late = sampled_opacity_with_timing(rule, 0.72, &overall);
    assert!(
      (opacity_late - 0.68).abs() < 1e-6,
      "opacity_late={opacity_late}"
    );
  }

  #[test]
  fn time_based_animations_apply_timing_function_within_keyframe_intervals() {
    let sheet = parse_stylesheet(
      "@keyframes tri { 0% { opacity: 0; } 50% { opacity: 1; } 100% { opacity: 0; } }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let timing = TransitionTimingFunction::CubicBezier(0.0, 1.0, 0.0, 1.0);

    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("tri".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_timing_functions = vec![timing.clone()].into();

    let progress_mid = time_based_animation_progress(&style, 0, 500.0).expect("active");
    assert!(
      (progress_mid - 0.5).abs() < 1e-6,
      "progress_mid={progress_mid}"
    );
    assert!((sampled_opacity_with_timing(rule, progress_mid, &timing) - 1.0).abs() < 1e-6);

    let progress_quarter = time_based_animation_progress(&style, 0, 250.0).expect("active");
    assert!(
      (progress_quarter - 0.25).abs() < 1e-6,
      "progress_quarter={progress_quarter}"
    );
    assert!(
      sampled_opacity_with_timing(rule, progress_quarter, &timing) > 0.7,
      "expected cubic-bezier timing to ease within the first interval"
    );
  }

  fn render_scroll_self_opacity(
    scroll_overflow_height: f32,
    scroll_offset_y: f32,
    range: AnimationRange,
    fill: AnimationFillMode,
  ) -> f32 {
    let mut style = ComputedStyle::default();
    style.overflow_y = Overflow::Scroll;
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_timelines = vec![AnimationTimeline::Scroll(ScrollFunctionTimeline {
      scroller: ScrollTimelineScroller::SelfElement,
      axis: TimelineAxis::Block,
    })];
    style.animation_ranges = vec![range];
    style.animation_fill_modes = vec![fill].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();

    let style = Arc::new(style);
    let mut animated =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), 1, vec![]);
    animated.style = Some(style);
    animated.scroll_overflow = Rect::from_xywh(0.0, 0.0, 100.0, scroll_overflow_height);

    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![animated]);
    let mut tree = FragmentTree::new(root);
    tree.keyframes.insert("fade".to_string(), fade_rule());

    let mut elements = HashMap::new();
    elements.insert(1, Point::new(0.0, scroll_offset_y));
    let scroll_state = ScrollState::from_parts(Point::ZERO, elements);
    apply_animations(&mut tree, &scroll_state, None);

    tree.root.children[0]
      .style
      .as_ref()
      .expect("child style")
      .opacity
  }

  #[test]
  fn scroll_self_timeline_inactive_when_scroll_range_is_zero() {
    let opacity = render_scroll_self_opacity(
      100.0,
      0.0,
      AnimationRange::default(),
      AnimationFillMode::Both,
    );
    assert!((opacity - 1.0).abs() < 1e-6, "opacity={opacity}");
  }

  #[test]
  fn axis_scroll_state_does_not_extend_range_for_out_of_bounds_scroll_offsets() {
    let (pos, range, view_size) = axis_scroll_state(
      TimelineAxis::Block,
      WritingMode::HorizontalTb,
      Direction::Ltr,
      0.0,
      50.0,
      100.0,
      100.0,
      100.0,
      100.0,
    );
    assert!((range - 0.0).abs() < 1e-6, "range={range}");
    assert!((pos - 0.0).abs() < 1e-6, "pos={pos}");
    assert!((view_size - 100.0).abs() < 1e-6, "view_size={view_size}");

    let timeline = ScrollTimeline::default();
    assert_eq!(
      scroll_timeline_progress(&timeline, pos, range, view_size, &AnimationRange::default()),
      None
    );
  }

  #[test]
  fn scroll_self_timeline_active_when_scroll_range_is_positive() {
    let opacity = render_scroll_self_opacity(
      200.0,
      0.0,
      AnimationRange::default(),
      AnimationFillMode::None,
    );
    assert!((opacity - 0.0).abs() < 1e-6, "opacity={opacity}");
  }

  #[test]
  fn view_timeline_entry_offsets_resolve_against_view_size() {
    let range = AnimationRange {
      start: RangeOffset::View(ViewTimelinePhase::Entry, Length::px(100.0)),
      end: RangeOffset::View(ViewTimelinePhase::Entry, Length::px(500.0)),
    };
    let timeline = ViewTimeline::default();
    let raw = view_timeline_progress(&timeline, 1000.0, 1100.0, 400.0, 900.0, &range).unwrap();
    assert!((raw - 0.5).abs() < 1e-6, "raw={raw}");
  }

  #[test]
  fn scroll_driven_fill_mode_controls_out_of_range_application() {
    let range = AnimationRange {
      start: RangeOffset::Progress(0.5),
      end: RangeOffset::Progress(1.0),
    };

    let opacity_none =
      render_scroll_self_opacity(200.0, 0.0, range.clone(), AnimationFillMode::None);
    assert!(
      (opacity_none - 1.0).abs() < 1e-6,
      "opacity_none={opacity_none}"
    );

    let opacity_backwards =
      render_scroll_self_opacity(200.0, 0.0, range, AnimationFillMode::Backwards);
    assert!(
      (opacity_backwards - 0.0).abs() < 1e-6,
      "opacity_backwards={opacity_backwards}"
    );
  }

  #[test]
  fn settled_time_based_animation_progress_samples_fill_forwards_end_state() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("filled");
    assert!((progress - 1.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn settled_time_based_animation_progress_paused_returns_initial_progress() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();
    style.animation_directions = vec![AnimationDirection::Normal].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Count(1.0)].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("active");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
  }

  #[test]
  fn settled_time_based_animation_progress_paused_with_positive_delay_is_none_without_backwards_fill(
  ) {
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_delays = vec![10_000.0].into();
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_paused_infinite_iterations_is_deterministically_start() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_durations = vec![1000.0].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Infinite].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("active");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
  }

  #[test]
  fn settled_time_based_animation_progress_skips_non_filled_animations() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_fill_modes = vec![AnimationFillMode::None].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_skips_infinite_iterations() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Infinite].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_respects_direction_and_iterations() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_directions = vec![AnimationDirection::Alternate].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Count(2.0)].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("filled");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn settled_time_based_animation_progress_supports_fractional_iterations() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec![Some("fade".to_string())];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Count(1.5)].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("filled");
    assert!((progress - 0.5).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 0.5).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_visibility_hidden_to_visible_is_visible_for_open_interval() {
    let sheet = parse_stylesheet(
      "@keyframes show { 0% { visibility: hidden; } 100% { visibility: visible; } }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert_eq!(sampled_visibility(rule, 0.0), Visibility::Hidden);
    assert_eq!(sampled_visibility(rule, 0.25), Visibility::Visible);
    assert_eq!(sampled_visibility(rule, 1.0), Visibility::Visible);
  }

  #[test]
  fn sample_keyframes_visibility_visible_to_hidden_is_visible_until_end() {
    let sheet = parse_stylesheet(
      "@keyframes hide { 0% { visibility: visible; } 100% { visibility: hidden; } }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert_eq!(sampled_visibility(rule, 0.0), Visibility::Visible);
    assert_eq!(sampled_visibility(rule, 0.75), Visibility::Visible);
    assert_eq!(sampled_visibility(rule, 1.0), Visibility::Hidden);
  }

  #[test]
  fn sample_keyframes_translate_interpolates_percentages() {
    let sheet =
      parse_stylesheet("@keyframes move { from { translate: 0 -100%; } to { translate: 0 0; } }")
        .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let translate = sampled_translate(rule, 0.5, Size::new(80.0, 80.0));
    match translate {
      TranslateValue::Values { x, y, z } => {
        assert!((x.to_px() - 0.0).abs() < 1e-6, "x={x:?}");
        assert!((y.to_px() - -40.0).abs() < 1e-6, "y={y:?}");
        assert!((z.to_px() - 0.0).abs() < 1e-6, "z={z:?}");
      }
      TranslateValue::None => panic!("expected translate values"),
    }
  }

  #[test]
  fn sample_keyframes_rotate_interpolates_angle() {
    let sheet =
      parse_stylesheet("@keyframes spin { from { rotate: 0deg; } to { rotate: 90deg; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let rotate = sampled_rotate(rule, 0.5);
    match rotate {
      RotateValue::Angle(deg) => assert!((deg - 45.0).abs() < 1e-6, "deg={deg}"),
      RotateValue::None => panic!("expected rotate angle"),
      RotateValue::AxisAngle { .. } => panic!("expected rotate angle"),
    }
  }

  #[test]
  fn sample_keyframes_rotate_interpolates_axis_angle() {
    let sheet =
      parse_stylesheet("@keyframes spin { from { rotate: x 0deg; } to { rotate: x 90deg; } }")
        .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let rotate = sampled_rotate(rule, 0.5);
    match rotate {
      RotateValue::AxisAngle { x, y, z, angle } => {
        assert!((x - 1.0).abs() < 1e-6, "x={x}");
        assert!(y.abs() < 1e-6, "y={y}");
        assert!(z.abs() < 1e-6, "z={z}");
        assert!((angle - 45.0).abs() < 1e-6, "angle={angle}");
      }
      other => panic!("expected rotate axis-angle, got {other:?}"),
    }
  }

  #[test]
  fn sample_keyframes_rotate_interpolates_axis_angle_from_none() {
    let sheet =
      parse_stylesheet("@keyframes spin { from { rotate: none; } to { rotate: x 90deg; } }")
        .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let rotate = sampled_rotate(rule, 0.5);
    match rotate {
      RotateValue::AxisAngle { x, y, z, angle } => {
        assert!((x - 1.0).abs() < 1e-6, "x={x}");
        assert!(y.abs() < 1e-6, "y={y}");
        assert!(z.abs() < 1e-6, "z={z}");
        assert!((angle - 45.0).abs() < 1e-6, "angle={angle}");
      }
      other => panic!("expected rotate axis-angle, got {other:?}"),
    }
  }

  #[test]
  fn sample_keyframes_scale_interpolates_numbers() {
    let sheet =
      parse_stylesheet("@keyframes zoom { from { scale: 1; } to { scale: 2 3; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let scale = sampled_scale(rule, 0.5);
    match scale {
      ScaleValue::Values { x, y, z } => {
        assert!((x - 1.5).abs() < 1e-6, "x={x}");
        assert!((y - 2.0).abs() < 1e-6, "y={y}");
        assert!((z - 1.0).abs() < 1e-6, "z={z}");
      }
      ScaleValue::None => panic!("expected scale values"),
    }
  }

  #[test]
  fn sample_keyframes_to_only_inserts_implicit_from() {
    let sheet = parse_stylesheet("@keyframes k { to { opacity: 1; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let opacity = sampled_opacity_with_base_opacity(rule, 0.5, 0.0);
    assert!((opacity - 0.5).abs() < 1e-6, "opacity={opacity}");
  }

  #[test]
  fn sample_keyframes_from_only_inserts_implicit_to() {
    let sheet = parse_stylesheet("@keyframes k { from { opacity: 0; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let opacity = sampled_opacity_with_base_opacity(rule, 0.5, 1.0);
    assert!((opacity - 0.5).abs() < 1e-6, "opacity={opacity}");
  }

  #[test]
  fn sample_keyframes_mid_only_inserts_both_endpoints() {
    let sheet = parse_stylesheet("@keyframes k { 50% { opacity: 0; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    let before_mid = sampled_opacity_with_base_opacity(rule, 0.25, 1.0);
    assert!((before_mid - 0.5).abs() < 1e-6, "before_mid={before_mid}");

    let after_mid = sampled_opacity_with_base_opacity(rule, 0.75, 1.0);
    assert!((after_mid - 0.5).abs() < 1e-6, "after_mid={after_mid}");
  }

  #[test]
  fn sample_keyframes_mixed_offsets_sample_per_property() {
    let sheet = parse_stylesheet(
      "@keyframes mix { 0% { opacity: 0; } 50% { transform: translateX(100px); } 100% { opacity: 1; } }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_opacity(rule, 0.5) - 0.5).abs() < 1e-6);

    assert!((sampled_opacity(rule, 0.25) - 0.25).abs() < 1e-6);
    assert!((sampled_transform_translate_x(rule, 0.25) - 50.0).abs() < 1e-6);

    assert!((sampled_opacity(rule, 0.75) - 0.75).abs() < 1e-6);
    assert!((sampled_transform_translate_x(rule, 0.75) - 50.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_var_resolution_sees_custom_properties_from_same_offset_keyframes() {
    let sheet = parse_stylesheet(
      "@keyframes vars {\
        0% { transform: translateX(var(--x)); }\
        0% { --x: 100px; }\
        100% { transform: translateX(0px); }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_transform_translate_x(rule, 0.0) - 100.0).abs() < 1e-6);
    assert!((sampled_transform_translate_x(rule, 0.5) - 50.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_var_resolution_is_order_independent_within_keyframe_block() {
    let sheet = parse_stylesheet(
      "@keyframes vars {\
        from { transform: translateX(var(--x)); --x: 100px; }\
        to { transform: translateX(0px); }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_transform_translate_x(rule, 0.0) - 100.0).abs() < 1e-6);
  }

  #[test]
  fn apply_animations_recomputes_var_dependent_properties_after_animating_custom_properties() {
    let sheet = parse_stylesheet(
      r#"
      @property --x {
        syntax: "<number>";
        inherits: false;
        initial-value: 0;
      }
      #el {
        width: calc(10px * var(--x));
        animation: varAnim 1s linear;
      }
      @keyframes varAnim {
        from { --x: 0; }
        to { --x: 1; }
      }
      "#,
    )
    .unwrap();

    let dom = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "el".to_string())],
      },
      children: vec![],
    };
    let styled = apply_styles(&dom, &sheet);
    assert_eq!(styled.styles.width, Some(Length::px(0.0)));

    let mut tree = FragmentTree::with_viewport(
      FragmentNode::new_block_styled(
        Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
        Vec::new(),
        styled.styles.clone(),
      ),
      Size::new(100.0, 100.0),
    );

    for rule in sheet.collect_keyframes(&MediaContext::screen(100.0, 100.0)) {
      tree.keyframes.insert(rule.name.clone(), rule);
    }

    apply_animations(
      &mut tree,
      &ScrollState::default(),
      Some(Duration::from_millis(500)),
    );
    let style = tree.root.style.as_deref().expect("animated style");
    assert_eq!(style.width, Some(Length::px(5.0)));
  }

  #[test]
  fn sample_keyframes_respects_keyframe_timing_function_for_following_interval() {
    let sheet = parse_stylesheet(
      "@keyframes step {\
        0% { opacity: 0; }\
        50% { opacity: 1; animation-timing-function: step-end; }\
        100% { opacity: 0; }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_opacity(rule, 0.25) - 0.5).abs() < 1e-6);
    assert!((sampled_opacity(rule, 0.75) - 1.0).abs() < 1e-6);
    assert!((sampled_opacity(rule, 1.0) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_parses_first_timing_function_in_keyframe_timing_function_lists() {
    let sheet = parse_stylesheet(
      "@keyframes step {\
        0% { opacity: 0; }\
        50% { opacity: 1; animation-timing-function: steps(1, end), linear; }\
        100% { opacity: 0; }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_opacity(rule, 0.25) - 0.5).abs() < 1e-6);
    assert!((sampled_opacity(rule, 0.75) - 1.0).abs() < 1e-6);
    assert!((sampled_opacity(rule, 1.0) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_respects_webkit_keyframe_timing_function() {
    let sheet = parse_stylesheet(
      "@keyframes step {\
        0% { opacity: 0; }\
        50% { opacity: 1; -webkit-animation-timing-function: step-end; }\
        100% { opacity: 0; }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_opacity(rule, 0.25) - 0.5).abs() < 1e-6);
    assert!((sampled_opacity(rule, 0.75) - 1.0).abs() < 1e-6);
    assert!((sampled_opacity(rule, 1.0) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_parse_keyframe_timing_function_ignores_commas_inside_comments() {
    let sheet = parse_stylesheet(
      "@keyframes step {\
        0% { opacity: 0; }\
        50% { opacity: 1; animation-timing-function: steps(1, end) /* comment, with comma */, linear; }\
        100% { opacity: 0; }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert!((sampled_opacity(rule, 0.25) - 0.5).abs() < 1e-6);
    assert!((sampled_opacity(rule, 0.75) - 1.0).abs() < 1e-6);
    assert!((sampled_opacity(rule, 1.0) - 0.0).abs() < 1e-6);
  }

  #[test]
  fn sample_keyframes_resolves_keyframe_timing_functions_with_vars() {
    let sheet = parse_stylesheet(
      "@keyframes step {\
        0% { --tf: step-end; opacity: 0; animation-timing-function: var(--tf); }\
        100% { opacity: 1; }\
      }",
    )
    .unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    // Halfway through the only interval, `step-end` should keep the start keyframe value.
    assert!(
      (sampled_opacity_with_timing(rule, 0.5, &TransitionTimingFunction::Linear) - 0.0).abs()
        < 1e-6
    );
  }

  #[test]
  fn sample_keyframes_interpolates_filter_against_none_as_identity() {
    let sheet =
      parse_stylesheet("@keyframes f { from { filter: url(#a); } to { filter: none; } }").unwrap();
    let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
    let rule = &keyframes[0];

    assert_eq!(sampled_filter_len(rule, 0.25), 1);
    assert_eq!(sampled_filter_len(rule, 0.5), 1);
    assert_eq!(sampled_filter_len(rule, 0.75), 1);
    assert_eq!(sampled_filter_len(rule, 1.0), 0);
  }

  fn decode_png(bytes: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(bytes, ImageFormat::Png)
      .expect("decode png")
      .to_rgba8()
  }

  fn assert_png_eq(actual: &[u8], expected: &[u8]) {
    let actual = decode_png(actual);
    let expected = decode_png(expected);
    assert_eq!(
      actual.dimensions(),
      expected.dimensions(),
      "png dimensions mismatch"
    );
    assert_eq!(actual.as_raw(), expected.as_raw(), "png pixels mismatch");
  }

  fn should_update_animation_sampling_goldens() -> bool {
    std::env::var("UPDATE_ANIMATION_TIME_SAMPLING_GOLDEN").is_ok()
  }

  fn render_animation_sampling_fixture(
    html: &str,
    base_url: String,
    time_ms: Option<f32>,
  ) -> Vec<u8> {
    let policy = ResourcePolicy::default()
      .allow_http(false)
      .allow_https(false)
      .allow_file(true)
      .allow_data(true);
    let mut renderer = FastRender::builder()
      .base_url(base_url)
      .resource_policy(policy)
      // Avoid system font discovery in tests so rendering stays deterministic and local `cargo test`
      // doesn't spend minutes crawling the host font directory.
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("renderer");
    let mut options = RenderOptions::new().with_viewport(200, 200);
    if let Some(time_ms) = time_ms {
      options = options.with_animation_time(time_ms);
    }
    let pixmap = renderer
      .render_html_with_options(html, options)
      .expect("render fixture");
    encode_image(&pixmap, OutputFormat::Png).expect("encode png")
  }

  #[test]
  fn animation_time_sampling_applies_visibility_keyframes() {
    let html = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(0, 0, 0); }
        #box {
          width: 100px;
          height: 100px;
          background: rgb(255, 0, 0);
          visibility: hidden;
          opacity: 0;
          animation: show 1s forwards;
        }
        @keyframes show {
          from { visibility: hidden; opacity: 0; }
          to { visibility: visible; opacity: 1; }
        }
      </style>
      <div id="box"></div>
    "#;

    let base_url = Url::parse("https://example.com/").unwrap().to_string();
    let rendered = render_animation_sampling_fixture(html, base_url, Some(1500.0));
    let image = decode_png(&rendered);
    let px = image.get_pixel(50, 50);
    assert_eq!(px.0, [255, 0, 0, 255]);
  }

  #[test]
  fn animation_time_sampling_does_not_warp_keyframe_boundaries_with_easing() {
    let html = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(255, 255, 255); }
        #box {
          width: 100px;
          height: 100px;
          background-color: rgb(255, 0, 0);
          animation: colors 1s ease-in forwards;
        }
        @keyframes colors {
          0% { background-color: rgb(255, 0, 0); }
          50% { background-color: rgb(0, 255, 0); }
          100% { background-color: rgb(0, 0, 255); }
        }
      </style>
      <div id="box"></div>
    "#;

    let base_url = Url::parse("https://example.com/").unwrap().to_string();
    let rendered = render_animation_sampling_fixture(html, base_url, Some(500.0));
    let image = decode_png(&rendered);
    // At 50% progress the eased animation must land exactly on the 50% keyframe, not an interpolated
    // value between the surrounding offsets.
    assert_eq!(image.get_pixel(50, 50).0, [0, 255, 0, 255]);
  }

  #[test]
  fn animation_time_sampling_applies_keyframe_timing_function_within_interval() {
    let html = r#"
      <!doctype html>
      <style>
        html, body { margin: 0; background: rgb(255, 255, 255); }
        #box {
          width: 100px;
          height: 100px;
          /* Underlying style must differ so the test fails if the animation is ignored. */
          background-color: rgb(0, 0, 255);
          animation: colors 1s linear forwards;
        }
        @keyframes colors {
          0% {
            background-color: rgb(255, 0, 0);
            animation-timing-function: step-end;
          }
          50% { background-color: rgb(0, 255, 0); }
          100% { background-color: rgb(0, 0, 255); }
        }
      </style>
      <div id="box"></div>
    "#;

    let base_url = Url::parse("https://example.com/").unwrap().to_string();
    // 250ms into a 1s animation is 25% overall progress, so we are half-way through the first
    // keyframe interval. With `step-end` on the 0% keyframe, the local eased progress should still
    // be 0 (holding the start keyframe value).
    let rendered = render_animation_sampling_fixture(html, base_url, Some(250.0));
    let image = decode_png(&rendered);
    assert_eq!(image.get_pixel(50, 50).0, [255, 0, 0, 255]);
  }

  #[test]
  fn animation_timeline_none_disables_time_based_animation() {
    let html = r#"<!DOCTYPE html>
<html>
  <head>
    <style>
      html, body { margin: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        margin: 50px;
        background: rgb(255, 0, 0);
        opacity: 0;
        animation: fade 1s forwards;
        animation-timeline: none;
      }
      @keyframes fade {
        from { opacity: 0; }
        to { opacity: 1; }
      }
    </style>
  </head>
  <body>
    <div id="box"></div>
  </body>
</html>
"#;
    let base_url = Url::parse("https://example.com/").unwrap().to_string();
    let rendered = render_animation_sampling_fixture(html, base_url, Some(1500.0));
    let image = decode_png(&rendered);
    assert_eq!(image.get_pixel(100, 100).0, [255, 255, 255, 255]);
  }

  #[test]
  fn animation_time_sampling_fixture_matches_golden() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = repo_root.join("tests/pages/fixtures/animation_time_sampling/index.html");
    let html = fs::read_to_string(&fixture_path).unwrap_or_else(|e| panic!("read fixture: {e}"));
    let base_url = Url::from_directory_path(
      fixture_path
        .parent()
        .expect("fixture directory should have parent"),
    )
    .expect("fixture base url")
    .to_string();

    let golden_dir = repo_root.join("tests/pages/golden");
    let outputs = [
      (0.0_f32, "animation_time_sampling_t0.png"),
      (1500.0_f32, "animation_time_sampling_t1500.png"),
    ];

    for (time_ms, golden_name) in outputs {
      let rendered = render_animation_sampling_fixture(&html, base_url.clone(), Some(time_ms));
      let golden_path = golden_dir.join(golden_name);
      if should_update_animation_sampling_goldens() {
        fs::create_dir_all(&golden_dir)
          .unwrap_or_else(|e| panic!("Failed to create golden dir {}: {e}", golden_dir.display()));
        fs::write(&golden_path, &rendered).unwrap_or_else(|e| {
          panic!(
            "Failed to write golden {} ({}): {e}",
            golden_name,
            golden_path.display()
          )
        });
        continue;
      }

      let golden = fs::read(&golden_path).unwrap_or_else(|e| {
        panic!(
          "Missing golden {} ({}): {e}",
          golden_name,
          golden_path.display()
        )
      });
      assert_png_eq(&rendered, &golden);

      // Sanity-check the golden itself is a PNG to make failure messages clearer when the file is
      // missing/corrupt.
      assert_eq!(
        Path::new(golden_name).extension().and_then(|s| s.to_str()),
        Some("png")
      );
    }
  }

  #[test]
  fn animation_time_sampling_fixture_settles_without_explicit_timestamp() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = repo_root.join("tests/pages/fixtures/animation_time_sampling/index.html");
    let html = fs::read_to_string(&fixture_path).unwrap_or_else(|e| panic!("read fixture: {e}"));
    let base_url = Url::from_directory_path(
      fixture_path
        .parent()
        .expect("fixture directory should have parent"),
    )
    .expect("fixture base url")
    .to_string();

    let golden_dir = repo_root.join("tests/pages/golden");
    let golden_name = "animation_time_sampling_t1500.png";
    let rendered = render_animation_sampling_fixture(&html, base_url, None);
    let golden_path = golden_dir.join(golden_name);

    if should_update_animation_sampling_goldens() {
      fs::create_dir_all(&golden_dir)
        .unwrap_or_else(|e| panic!("Failed to create golden dir {}: {e}", golden_dir.display()));
      fs::write(&golden_path, &rendered).unwrap_or_else(|e| {
        panic!(
          "Failed to write golden {} ({}): {e}",
          golden_name,
          golden_path.display()
        )
      });
      return;
    }

    let golden = fs::read(&golden_path).unwrap_or_else(|e| {
      panic!(
        "Missing golden {} ({}): {e}",
        golden_name,
        golden_path.display()
      )
    });
    assert_png_eq(&rendered, &golden);
  }

  #[test]
  fn animation_shorthand_parses_forwards_fill_mode_with_zero_delay() {
    let sheet =
      parse_stylesheet("#box { animation: fade 1000ms linear 0ms 1 normal forwards; }").unwrap();
    let CssRule::Style(rule) = &sheet.rules[0] else {
      panic!("expected style rule");
    };
    let decl = rule
      .declarations
      .iter()
      .find(|decl| decl.property.as_str() == "animation")
      .expect("expected animation declaration");

    let defaults = ComputedStyle::default();
    let mut style = ComputedStyle::default();
    apply_declaration_with_base(
      &mut style,
      decl,
      &defaults,
      &defaults,
      None,
      defaults.font_size,
      defaults.root_font_size,
      Size::new(800.0, 600.0),
      false,
    );

    assert_eq!(&*style.animation_names, &[Some("fade".to_string())]);
    assert_eq!(&*style.animation_durations, &[1000.0]);
    assert_eq!(&*style.animation_delays, &[0.0]);
    assert_eq!(
      &*style.animation_timing_functions,
      &[TransitionTimingFunction::Linear]
    );
    assert_eq!(
      &*style.animation_iteration_counts,
      &[AnimationIterationCount::Count(1.0)]
    );
    assert_eq!(&*style.animation_directions, &[AnimationDirection::Normal]);
    assert_eq!(&*style.animation_fill_modes, &[AnimationFillMode::Forwards]);

    let progress = time_based_animation_progress(&style, 0, 2000.0).expect("filled");
    assert!((progress - 1.0).abs() < 1e-6, "progress={progress}");
  }

  #[test]
  fn animation_time_none_samples_settled_forwards_animation() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_path =
      repo_root.join("tests/pages/fixtures/animation_time_settled_forwards/index.html");
    let html = fs::read_to_string(&fixture_path).unwrap_or_else(|e| panic!("read fixture: {e}"));
    let base_url = Url::from_directory_path(
      fixture_path
        .parent()
        .expect("fixture directory should have parent"),
    )
    .expect("fixture base url")
    .to_string();

    let count_non_white =
      |img: &image::RgbaImage| img.pixels().filter(|p| p.0 != [255, 255, 255, 255]).count();
    let start = render_animation_sampling_fixture(&html, base_url.clone(), Some(0.0));
    let start_image = decode_png(&start);
    let start_non_white = count_non_white(&start_image);
    assert_eq!(
      start_non_white, 0,
      "expected no painted pixels at t=0, got {start_non_white}"
    );

    let settled = render_animation_sampling_fixture(&html, base_url.clone(), None);
    let settled_image = decode_png(&settled);
    let settled_non_white = count_non_white(&settled_image);
    assert_eq!(settled_image.get_pixel(100, 100).0, [200, 0, 0, 255]);

    // Sanity-check the fixture animates when an explicit time is provided.
    let mid = render_animation_sampling_fixture(&html, base_url.clone(), Some(500.0));
    let mid_image = decode_png(&mid);
    let mid_non_white = count_non_white(&mid_image);
    assert!(
      mid_non_white > 0,
      "expected non-white pixels while the animation is active, got {mid_non_white}"
    );

    let end = render_animation_sampling_fixture(&html, base_url.clone(), Some(2000.0));
    let end_image = decode_png(&end);
    let end_non_white = count_non_white(&end_image);
    assert!(
      end_non_white > 0,
      "expected non-white pixels at the end of the animation, got {end_non_white}"
    );
    assert_eq!(end_image.get_pixel(100, 100).0, [200, 0, 0, 255]);
    assert!(
      settled_non_white > 0,
      "expected non-white pixels when animation_time is unset, got {settled_non_white}"
    );
  }

  #[test]
  fn expand_transition_property_name_expands_supported_shorthands() {
    assert_eq!(
      expand_transition_property_name("border").as_slice(),
      &[
        "border-top-width",
        "border-right-width",
        "border-bottom-width",
        "border-left-width",
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
        "border-top-style",
        "border-right-style",
        "border-bottom-style",
        "border-left-style",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-top").as_slice(),
      &["border-top-width", "border-top-color", "border-top-style"]
    );
    assert_eq!(
      expand_transition_property_name("border-right").as_slice(),
      &[
        "border-right-width",
        "border-right-color",
        "border-right-style",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-bottom").as_slice(),
      &[
        "border-bottom-width",
        "border-bottom-color",
        "border-bottom-style",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-left").as_slice(),
      &["border-left-width", "border-left-color", "border-left-style"]
    );
    assert_eq!(
      expand_transition_property_name("border-color").as_slice(),
      &[
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-width").as_slice(),
      &[
        "border-top-width",
        "border-right-width",
        "border-bottom-width",
        "border-left-width",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-style").as_slice(),
      &[
        "border-top-style",
        "border-right-style",
        "border-bottom-style",
        "border-left-style",
      ]
    );
    assert_eq!(
      expand_transition_property_name("border-radius").as_slice(),
      &[
        "border-top-left-radius",
        "border-top-right-radius",
        "border-bottom-right-radius",
        "border-bottom-left-radius",
      ]
    );
    assert_eq!(
      expand_transition_property_name("outline").as_slice(),
      &["outline-color", "outline-style", "outline-width"]
    );
    assert_eq!(expand_transition_property_name("opacity").as_slice(), &["opacity"]);
    assert_eq!(
      expand_transition_property_name("not-a-real-prop").as_slice(),
      &["not-a-real-prop"]
    );
  }

  #[test]
  fn transition_property_all_generates_longhand_transitions_only() {
    let mut start_style = ComputedStyle::default();
    start_style.border_top_color = Rgba::BLACK;
    start_style.border_right_color = Rgba::BLACK;

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![TransitionProperty::All].into();
    style.transition_durations = vec![1000.0].into();
    style.transition_delays = vec![0.0].into();
    style.transition_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.border_top_color = Rgba::RED;
    style.border_right_color = Rgba::GREEN;

    let pairs =
      transition_pairs(&style.transition_properties, &start_style, &style).expect("pairs");

    assert!(pairs.iter().any(|(name, _)| *name == "border-top-color"));
    assert!(pairs.iter().any(|(name, _)| *name == "border-right-color"));
    assert!(
      !pairs.iter().any(|(name, _)| {
        matches!(
          *name,
          "border"
            | "border-top"
            | "border-right"
            | "border-bottom"
            | "border-left"
            | "border-color"
            | "border-style"
            | "border-width"
            | "border-radius"
            | "outline"
        )
      }),
      "expected shorthands to be excluded from `transition-property: all`"
    );

    let idx_top = pairs
      .iter()
      .find(|(name, _)| *name == "border-top-color")
      .expect("top color idx")
      .1;
    let idx_right = pairs
      .iter()
      .find(|(name, _)| *name == "border-right-color")
      .expect("right color idx")
      .1;
    let ctx = AnimationResolveContext::new(Size::new(800.0, 600.0), Size::new(100.0, 100.0));
    let (top_value, _, _, _) = transition_value_for_property(
      "border-top-color",
      idx_top,
      false,
      &style,
      &start_style,
      &style.transition_durations,
      &style.transition_delays,
      &style.transition_timing_functions,
      500.0,
      &ctx,
    )
    .expect("sample top");
    let (right_value, _, _, _) = transition_value_for_property(
      "border-right-color",
      idx_right,
      false,
      &style,
      &start_style,
      &style.transition_durations,
      &style.transition_delays,
      &style.transition_timing_functions,
      500.0,
      &ctx,
    )
    .expect("sample right");

    let mut animated = style.clone();
    apply_animated_properties_ordered(
      &mut animated,
      &vec![
        ("border-top-color".to_string(), top_value),
        ("border-right-color".to_string(), right_value),
      ],
    );
    assert_eq!(animated.border_top_color, Rgba::rgb(128, 0, 0));
    assert_eq!(animated.border_right_color, Rgba::rgb(0, 128, 0));
  }

  #[test]
  fn transition_property_shorthand_border_radius_expands_to_corner_longhands() {
    let mut start_style = ComputedStyle::default();
    start_style.border_top_left_radius = BorderCornerRadius {
      x: Length::px(0.0),
      y: Length::px(0.0),
    };

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![TransitionProperty::Name("border-radius".to_string())].into();
    style.transition_durations = vec![1000.0].into();
    style.transition_delays = vec![0.0].into();
    style.transition_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.border_top_left_radius = BorderCornerRadius {
      x: Length::px(10.0),
      y: Length::px(10.0),
    };

    let pairs =
      transition_pairs(&style.transition_properties, &start_style, &style).expect("pairs");
    assert!(
      pairs
        .iter()
        .any(|(name, _)| *name == "border-top-left-radius"),
      "expected shorthand to expand into at least one corner longhand"
    );
    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-radius"),
      "expected shorthand name to be removed after expansion"
    );

    let idx = pairs
      .iter()
      .find(|(name, _)| *name == "border-top-left-radius")
      .expect("corner idx")
      .1;
    let ctx = AnimationResolveContext::new(Size::new(800.0, 600.0), Size::new(100.0, 100.0));
    let (value, _, _, _) = transition_value_for_property(
      "border-top-left-radius",
      idx,
      false,
      &style,
      &start_style,
      &style.transition_durations,
      &style.transition_delays,
      &style.transition_timing_functions,
      500.0,
      &ctx,
    )
    .expect("sample corner");

    let mut animated = style.clone();
    apply_animated_properties_ordered(
      &mut animated,
      &[("border-top-left-radius".to_string(), value)],
    );
    assert!((animated.border_top_left_radius.x.to_px() - 5.0).abs() < 1e-6);
    assert!((animated.border_top_left_radius.y.to_px() - 5.0).abs() < 1e-6);
  }

  #[test]
  fn transition_longhand_names_matches_supported_interpolator_longhands() {
    let shorthands = [
      "border",
      "border-top",
      "border-right",
      "border-bottom",
      "border-left",
      "border-color",
      "border-width",
      "border-style",
      "border-radius",
      "outline",
    ];

    let mut expected: Vec<&'static str> = property_interpolators()
      .iter()
      .map(|p| p.name)
      .filter(|name| !shorthands.contains(name))
      .collect();
    expected.sort_unstable();

    let mut actual: Vec<&'static str> = transition_longhand_names().iter().copied().collect();
    let actual_len = actual.len();
    actual.sort_unstable();
    actual.dedup();
    assert_eq!(
      actual.len(),
      actual_len,
      "transition_longhand_names() must not contain duplicates"
    );

    assert_eq!(actual, expected);
  }

  #[test]
  fn transition_pairs_expands_border_color_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-color".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-color"),
      "expected shorthands to be expanded away"
    );

    // The `border-color` entry is last, so it should own the expanded longhands.
    for side in ["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"]
    {
      let idx = pairs
        .iter()
        .find(|(name, _)| *name == side)
        .unwrap_or_else(|| panic!("missing {side}"))
        .1;
      assert_eq!(idx, 1, "{side} should use the border-color list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border"),
      "expected shorthands to be expanded away"
    );

    // The `border` entry is last, so it should own the expanded longhands.
    for name in [
      "border-top-width",
      "border-right-width",
      "border-bottom-width",
      "border-left-width",
      "border-top-color",
      "border-right-color",
      "border-bottom-color",
      "border-left-color",
      "border-top-style",
      "border-right-style",
      "border-bottom-style",
      "border-left-style",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border list index");
    }
  }

  #[test]
  fn transition_pairs_expands_outline_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("outline".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "outline"),
      "expected shorthands to be expanded away"
    );

    for name in ["outline-color", "outline-style", "outline-width"] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the outline list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_radius_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-radius".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-radius"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-top-left-radius",
      "border-top-right-radius",
      "border-bottom-right-radius",
      "border-bottom-left-radius",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-radius list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_width_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-width".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-width"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-top-width",
      "border-right-width",
      "border-bottom-width",
      "border-left-width",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-width list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_style_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-style".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-style"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-top-style",
      "border-right-style",
      "border-bottom-style",
      "border-left-style",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-style list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_top_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-top".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-top"),
      "expected shorthands to be expanded away"
    );

    for name in ["border-top-width", "border-top-color", "border-top-style"] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-top list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_right_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-right".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-right"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-right-width",
      "border-right-color",
      "border-right-style",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-right list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_bottom_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-bottom".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-bottom"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-bottom-width",
      "border-bottom-color",
      "border-bottom-style",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-bottom list index");
    }
  }

  #[test]
  fn transition_pairs_expands_border_left_against_all() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::All,
      TransitionProperty::Name("border-left".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-left"),
      "expected shorthands to be expanded away"
    );

    for name in ["border-left-width", "border-left-color", "border-left-style"] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the border-left list index");
    }
  }

  #[test]
  fn transition_pairs_all_overrides_border_color_shorthand() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::Name("border-color".to_string()),
      TransitionProperty::All,
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-color"),
      "expected shorthands to be expanded away"
    );

    for name in [
      "border-top-color",
      "border-right-color",
      "border-bottom-color",
      "border-left-color",
    ] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the all list index");
    }
  }

  #[test]
  fn transition_pairs_all_overrides_border_top_shorthand() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::Name("border-top".to_string()),
      TransitionProperty::All,
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    assert!(
      !pairs.iter().any(|(name, _)| *name == "border-top"),
      "expected shorthands to be expanded away"
    );

    for name in ["border-top-width", "border-top-color", "border-top-style"] {
      let idx = pairs
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .1;
      assert_eq!(idx, 1, "{name} should use the all list index");
    }
  }

  #[test]
  fn transition_pairs_shorthand_overlap_last_entry_wins_per_longhand() {
    let start_style = ComputedStyle::default();

    // `border-top` overlaps with `border` for the top side longhands only. Ensure the last entry
    // wins per expanded longhand name.
    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::Name("border".to_string()),
      TransitionProperty::Name("border-top".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    let top_width_idx = pairs
      .iter()
      .find(|(name, _)| *name == "border-top-width")
      .expect("border-top-width present")
      .1;
    let right_width_idx = pairs
      .iter()
      .find(|(name, _)| *name == "border-right-width")
      .expect("border-right-width present")
      .1;

    assert_eq!(top_width_idx, 1, "border-top-width should be owned by border-top");
    assert_eq!(
      right_width_idx, 0,
      "border-right-width should still be owned by border"
    );
  }

  #[test]
  fn transition_pairs_shorthand_overrides_earlier_longhand() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::Name("border-top-width".to_string()),
      TransitionProperty::Name("border".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    let idx = pairs
      .iter()
      .find(|(name, _)| *name == "border-top-width")
      .expect("border-top-width present")
      .1;
    assert_eq!(idx, 1, "border should override earlier border-top-width");
  }

  #[test]
  fn transition_pairs_shorthand_overrides_earlier_longhand_outline() {
    let start_style = ComputedStyle::default();

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![
      TransitionProperty::Name("outline-width".to_string()),
      TransitionProperty::Name("outline".to_string()),
    ]
    .into();

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");

    let idx = pairs
      .iter()
      .find(|(name, _)| *name == "outline-width")
      .expect("outline-width present")
      .1;
    assert_eq!(idx, 1, "outline should override earlier outline-width");
  }

  #[test]
  fn transition_pairs_all_sorts_custom_properties_deterministically() {
    let mut start_style = ComputedStyle::default();
    start_style.custom_properties.insert(
      Arc::from("--b"),
      CustomPropertyValue::new("1", None),
    );
    start_style.custom_properties.insert(
      Arc::from("--a"),
      CustomPropertyValue::new("1", None),
    );

    let mut style = ComputedStyle::default();
    style.transition_properties = vec![TransitionProperty::All].into();
    style.custom_properties.insert(
      Arc::from("--a"),
      CustomPropertyValue::new("2", None),
    );
    style.custom_properties.insert(
      Arc::from("--b"),
      CustomPropertyValue::new("2", None),
    );

    let pairs = transition_pairs(&style.transition_properties, &start_style, &style)
      .expect("transition pairs");
    let custom: Vec<&str> = pairs
      .into_iter()
      .map(|(name, _)| name)
      .filter(|name| name.starts_with("--"))
      .collect();
    assert_eq!(custom, vec!["--a", "--b"]);
  }

  #[test]
  fn transition_state_interruption_sampling_expands_border_radius_shorthand() {
    fn tree(style: ComputedStyle) -> BoxTree {
      let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
      node.styled_node_id = Some(1);
      BoxTree::new(node)
    }

    let mut start_style = ComputedStyle::default();
    start_style.border_top_left_radius = BorderCornerRadius {
      x: Length::px(0.0),
      y: Length::px(0.0),
    };

    let mut style_a = ComputedStyle::default();
    style_a.transition_properties =
      vec![TransitionProperty::Name("border-radius".to_string())].into();
    style_a.transition_durations = vec![1000.0].into();
    style_a.transition_delays = vec![0.0].into();
    style_a.transition_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style_a.border_top_left_radius = BorderCornerRadius {
      x: Length::px(10.0),
      y: Length::px(10.0),
    };

    let mut style_b = style_a.clone();
    style_b.border_top_left_radius = BorderCornerRadius {
      x: Length::px(20.0),
      y: Length::px(20.0),
    };

    let before_tree = tree(start_style);
    let tree_a = tree(style_a);
    let tree_b = tree(style_b);

    let state_a = TransitionState::update_for_style_change(None, Some(&before_tree), &tree_a, 0.0);
    let key = super::transitions::ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    assert!(
      state_a
        .elements
        .get(&key)
        .is_some_and(|el| el.running.contains_key("border-top-left-radius")),
      "expected border-radius shorthand to expand into a running corner longhand transition"
    );

    let state_b =
      TransitionState::update_for_style_change(Some(&state_a), Some(&tree_a), &tree_b, 500.0);
    let record = state_b
      .elements
      .get(&key)
      .and_then(|el| el.running.get("border-top-left-radius"))
      .expect("interrupted corner transition record");
    assert!((record.from_style.border_top_left_radius.x.to_px() - 5.0).abs() < 1e-6);
    assert!((record.from_style.border_top_left_radius.y.to_px() - 5.0).abs() < 1e-6);
  }

  #[test]
  fn transition_state_interruption_sampling_longhand_all_only() {
    fn tree(style: ComputedStyle) -> BoxTree {
      let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
      node.styled_node_id = Some(1);
      BoxTree::new(node)
    }

    let mut start_style = ComputedStyle::default();
    start_style.border_top_color = Rgba::BLACK;
    start_style.border_right_color = Rgba::BLACK;

    let mut style_a = ComputedStyle::default();
    style_a.transition_properties = vec![TransitionProperty::All].into();
    style_a.transition_durations = vec![1000.0].into();
    style_a.transition_delays = vec![0.0].into();
    style_a.transition_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style_a.border_top_color = Rgba::RED;
    style_a.border_right_color = Rgba::GREEN;

    let mut style_b = style_a.clone();
    style_b.border_top_color = Rgba::BLUE;
    style_b.border_right_color = Rgba::rgb(255, 255, 0);

    let before_tree = tree(start_style);
    let tree_a = tree(style_a);
    let tree_b = tree(style_b);

    let state_a = TransitionState::update_for_style_change(None, Some(&before_tree), &tree_a, 0.0);
    let key = super::transitions::ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    assert!(
      state_a.elements.get(&key).is_some_and(|el| {
        el.running.contains_key("border-top-color") && el.running.contains_key("border-right-color")
      }),
      "expected both border colors to be running transitions when transition-property: all"
    );

    let state_b =
      TransitionState::update_for_style_change(Some(&state_a), Some(&tree_a), &tree_b, 500.0);
    let element = state_b.elements.get(&key).expect("element state");
    let top = element
      .running
      .get("border-top-color")
      .expect("border-top-color record");
    let right = element
      .running
      .get("border-right-color")
      .expect("border-right-color record");
    assert_eq!(top.from_style.border_top_color, Rgba::rgb(128, 0, 0));
    assert_eq!(right.from_style.border_right_color, Rgba::rgb(0, 128, 0));
  }
}
