//! Scroll-driven animation utilities.
//!
//! This module provides lightweight timeline evaluation for scroll and view
//! timelines along with keyframe sampling helpers. It is intentionally small
//! and self contained so it can be reused by layout/paint and tests without
//! wiring a full animation engine.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::css::types::{
  BoxShadow, Keyframe, KeyframesRule, PropertyValue, RotateValue, ScaleValue, TextShadow,
  TranslateValue,
};
use crate::debug::runtime;
use crate::geometry::{Point, Rect, Size};
use crate::paint::display_list::{Transform2D, Transform3D};
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
use crate::style::types::FillRule;
use crate::style::types::FilterColor;
use crate::style::types::FilterFunction;
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

/// Resolved animated property value used by interpolation/apply steps.
#[derive(Debug, Clone)]
pub enum AnimatedValue {
  Opacity(f32),
  Visibility(Visibility),
  Color(Rgba),
  Length(Length),
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

  fn length_components(
    len: &Length,
    style: &ComputedStyle,
    ctx: &AnimationResolveContext,
  ) -> (f32, f32) {
    // The computed value type for `<length-percentage>` can be expressed as a linear combination
    // of percentage + absolute length. Convert any non-percent units into px so interpolation can
    // produce a canonical value string that `parse_length()` can reparse later.
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
      let (a_pct, a_px) = length_components(a, from_style, ctx);
      let (b_pct, b_px) = length_components(b, to_style, ctx);
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

fn resolve_shape_radius(
  radius: &ShapeRadius,
  axis: f32,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<f32> {
  match radius {
    ShapeRadius::Length(l) => Some(resolve_length_px(l, Some(axis), style, ctx)),
    // Keywords require reference box distances; fall back to discrete handling for now.
    ShapeRadius::ClosestSide | ShapeRadius::FarthestSide => None,
  }
}

fn resolve_clip_path(
  path: &ClipPath,
  style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> Option<ResolvedClipPath> {
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
      } => {
        let width = ctx.element_size.width;
        let height = ctx.element_size.height;
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
              x: Length::px(resolve_length_px(
                &r.bottom_right.x,
                Some(width),
                style,
                ctx,
              )),
              y: Length::px(resolve_length_px(
                &r.bottom_right.y,
                Some(height),
                style,
                ctx,
              )),
            },
            BorderCornerRadius {
              x: Length::px(resolve_length_px(&r.bottom_left.x, Some(width), style, ctx)),
              y: Length::px(resolve_length_px(
                &r.bottom_left.y,
                Some(height),
                style,
                ctx,
              )),
            },
          ])
        });
        Some(ResolvedClipPath::Inset {
          top: resolve_length_px(top, Some(height), style, ctx),
          right: resolve_length_px(right, Some(width), style, ctx),
          bottom: resolve_length_px(bottom, Some(height), style, ctx),
          left: resolve_length_px(left, Some(width), style, ctx),
          radii,
          reference: *reference,
        })
      }
      BasicShape::Circle { radius, position } => {
        let width = ctx.element_size.width;
        let height = ctx.element_size.height;
        let radius_px = resolve_shape_radius(radius, width.min(height), style, ctx)?;
        let resolved_pos = resolve_background_positions(&[*position], style, ctx).pop()?;
        Some(ResolvedClipPath::Circle {
          radius: radius_px,
          position: resolved_pos,
          reference: *reference,
        })
      }
      BasicShape::Ellipse {
        radius_x,
        radius_y,
        position,
      } => {
        let width = ctx.element_size.width;
        let height = ctx.element_size.height;
        let rx = resolve_shape_radius(radius_x, width, style, ctx)?;
        let ry = resolve_shape_radius(radius_y, height, style, ctx)?;
        let resolved_pos = resolve_background_positions(&[*position], style, ctx).pop()?;
        Some(ResolvedClipPath::Ellipse {
          radius_x: rx,
          radius_y: ry,
          position: resolved_pos,
          reference: *reference,
        })
      }
      BasicShape::Polygon { fill, points } => {
        let width = ctx.element_size.width;
        let height = ctx.element_size.height;
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
          reference: *reference,
        })
      }
      // Path and unsupported shapes fall back to discrete animation.
      BasicShape::Path { .. } => None,
    },
  }
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
    ) if refa == refb => {
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
    ) if refa == refb => {
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
    ) if refa == refb => {
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
    ) if fa == fb && refa == refb && pa.len() == pb.len() => {
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
    _ => None,
  }
}

fn resolved_clip_to_clip_path(resolved: &ResolvedClipPath) -> ClipPath {
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
        *reference,
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
      *reference,
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
      *reference,
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
      *reference,
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
        reference: *reference,
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
            reference: *reference,
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
          reference: *reference,
        }),
        _ => None,
      },
      BasicShape::Polygon { fill, points } => Some(ResolvedClipPath::Polygon {
        fill: *fill,
        points: points.iter().map(|(x, y)| (x.to_px(), y.to_px())).collect(),
        reference: *reference,
      }),
      BasicShape::Path { .. } => None,
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
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
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
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
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
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
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
  let width = ctx.element_size.width;
  let height = ctx.element_size.height;
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

fn resolve_progress_offset(
  offset: &RangeOffset,
  base_start: f32,
  base_end: f32,
  view_size: f32,
  phases: Option<(f32, f32, f32, f32)>,
) -> f32 {
  match offset {
    RangeOffset::Progress(p) => base_start + (base_end - base_start) * *p,
    RangeOffset::View(phase, adj) => {
      let Some((entry, contain, cover, exit)) = phases else {
        return base_start;
      };
      let base = match phase {
        ViewTimelinePhase::Entry => entry,
        ViewTimelinePhase::Contain => contain,
        ViewTimelinePhase::Cover => cover,
        ViewTimelinePhase::Exit => exit,
      };
      let adjustment = adj
        .resolve_against(view_size)
        .unwrap_or_else(|| adj.to_px());
      base + adjustment
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
  let start = resolve_progress_offset(range.start(), start_base, end_base, viewport_size, None);
  let end = resolve_progress_offset(range.end(), start_base, end_base, viewport_size, None);
  Some(raw_progress(scroll_position, start, end))
}

/// Computes view timeline progress using the target position relative to the
/// containing scroll port.
pub fn view_timeline_progress(
  timeline: &ViewTimeline,
  target_start: f32,
  target_end: f32,
  view_size: f32,
  scroll_offset: f32,
  range: &AnimationRange,
) -> Option<f32> {
  // Degenerate geometries produce an inactive timeline.
  if !view_size.is_finite()
    || view_size <= f32::EPSILON
    || !target_start.is_finite()
    || !target_end.is_finite()
    || !scroll_offset.is_finite()
    || (target_end - target_start).abs() <= f32::EPSILON
  {
    return None;
  }

  let view_size = view_size.max(0.0);
  let inset = timeline.inset.unwrap_or_default();
  let inset_start_len = inset.start.unwrap_or(Length::px(0.0));
  let inset_end_len = inset.end.unwrap_or(Length::px(0.0));
  let inset_start = inset_start_len
    .resolve_against(view_size)
    .unwrap_or_else(|| inset_start_len.to_px())
    .clamp(0.0, view_size);
  let inset_end = inset_end_len
    .resolve_against(view_size)
    .unwrap_or_else(|| inset_end_len.to_px())
    .clamp(0.0, view_size);

  let entry_edge = target_start - view_size + inset_end;
  let contain_edge = target_end - view_size + inset_end;
  let cover_edge = target_start - inset_start;
  let exit_edge = target_end - inset_start;
  let exit_phase_start = contain_edge.max(cover_edge);
  let start_base = entry_edge;
  let end_base = exit_edge;
  let phases = Some((entry_edge, contain_edge, cover_edge, exit_phase_start));
  let start = resolve_progress_offset(range.start(), start_base, end_base, view_size, phases);
  let end = resolve_progress_offset(range.end(), start_base, end_base, view_size, phases);
  Some(raw_progress(scroll_offset, start, end))
}

/// Determines the scroll position and range along the requested axis given
/// container and content sizes. The returned tuple is `(position, range, size)`.
pub fn axis_scroll_state(
  axis: TimelineAxis,
  writing_mode: WritingMode,
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

  let horizontal = axis_is_horizontal(axis, writing_mode);
  if horizontal {
    let view_width = sanitize(view_width);
    let content_width = sanitize(content_width);
    let range = (content_width - view_width).max(0.0);
    let scroll_x = sanitize(scroll_x);
    (scroll_x.min(range), range, view_width)
  } else {
    let view_height = sanitize(view_height);
    let content_height = sanitize(content_height);
    let range = (content_height - view_height).max(0.0);
    let scroll_y = sanitize(scroll_y);
    (scroll_y.min(range), range, view_height)
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

fn sample_keyframes_with_default_timing(
  rule: &KeyframesRule,
  progress: f32,
  base_style: &ComputedStyle,
  viewport: Size,
  element_size: Size,
  default_timing_function: &TransitionTimingFunction,
) -> SampledKeyframes {
  if rule.keyframes.is_empty() {
    return SampledKeyframes::default();
  }
  let mut frames: Vec<&Keyframe> = rule.keyframes.iter().collect();
  frames.sort_by(|a, b| {
    a.offset
      .partial_cmp(&b.offset)
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  let progress = clamp_progress(progress);
  let defaults = ComputedStyle::default();
  let mut groups: Vec<(f32, Vec<&Keyframe>)> = Vec::new();
  for frame in frames.iter().copied() {
    match groups.last_mut() {
      Some((offset, list)) if (*offset - frame.offset).abs() <= f32::EPSILON => list.push(frame),
      _ => groups.push((frame.offset, vec![frame])),
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
  let mut properties: FxHashSet<&str> = FxHashSet::default();
  for frame in &frames {
    for decl in &frame.declarations {
      properties.insert(decl.property.as_str());
    }
  }

  let mut result = HashMap::new();
  let mut custom_properties = Vec::new();
  for prop in properties {
    let group_has_property = |group: &[&Keyframe]| {
      group.iter().any(|frame| {
        frame
          .declarations
          .iter()
          .any(|d| d.property.as_str() == prop)
      })
    };

    let mut prev_idx = None;
    for (idx, (offset, group_frames)) in groups.iter().enumerate() {
      if *offset <= progress + f32::EPSILON {
        if group_has_property(group_frames) {
          prev_idx = Some(idx);
        }
      } else {
        break;
      }
    }

    let mut next_idx = None;
    for (idx, (offset, group_frames)) in groups.iter().enumerate() {
      if !group_has_property(group_frames) {
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

      let interpolated = match (from, to) {
        (Some(from), Some(to)) => {
          interpolate_custom_property(from, to, eased_t, from_style, to_style, &ctx)
        }
        _ => None,
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
      true
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
      true
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
      true
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
      true
    }
    ("opacity", AnimatedValue::Opacity(effect)) => {
      style.opacity = clamp_progress(style.opacity + effect);
      true
    }
    _ => false,
  }
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
) -> ScrollContainerContext {
  ScrollContainerContext {
    scroll: scroll_state.viewport,
    viewport: Size::new(root_viewport.width(), root_viewport.height()),
    content: Size::new(root_content.width(), root_content.height()),
    origin: Point::ZERO,
    writing_mode,
    scroll_padding_top: Length::px(0.0),
    scroll_padding_right: Length::px(0.0),
    scroll_padding_bottom: Length::px(0.0),
    scroll_padding_left: Length::px(0.0),
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
  horizontal: bool,
) -> ViewTimelineInset {
  let (auto_start, auto_end) = if horizontal {
    (
      scroll_container.scroll_padding_left,
      scroll_container.scroll_padding_right,
    )
  } else {
    (
      scroll_container.scroll_padding_top,
      scroll_container.scroll_padding_bottom,
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
    let mut resolved_timeline = tl.clone();
    resolved_timeline.inset = Some(resolved_view_timeline_inset(
      tl.inset,
      view_timeline_context,
      horizontal,
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

fn scroll_driven_effect_progress(style: &ComputedStyle, idx: usize, overall: f32) -> f32 {
  scroll_driven_effect_state(style, idx, overall).progress
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
) -> Option<f32> {
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

  let timeline = ViewTimeline {
    name: None,
    axis: func.axis,
    inset: Some(resolved_view_timeline_inset(
      func.inset,
      scroll_container,
      horizontal,
    )),
  };

  view_timeline_progress(
    &timeline,
    target_start,
    target_end,
    view_size,
    scroll_offset,
    range,
  )
}

fn apply_animations_to_node_scoped(
  node: &mut FragmentNode,
  origin: Point,
  viewport: Rect,
  parent_styles: Option<&ComputedStyle>,
  root_context: ScrollContainerContext,
  scroll_state: &ScrollState,
  keyframes: &HashMap<String, KeyframesRule>,
  animation_time_ms: Option<f32>,
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
        let mut resolved_timeline = tl.clone();
        resolved_timeline.inset = Some(resolved_view_timeline_inset(
          tl.inset,
          view_timeline_context,
          horizontal,
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
      let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
      let mut changed = false;
      let mut custom_properties_changed = false;
      let viewport_size = Size::new(viewport.width(), viewport.height());
      let element_size = Size::new(node.bounds.width(), node.bounds.height());
      let resolve_ctx = AnimationResolveContext::new(viewport_size, element_size);
      let mut applied_value_sets: Vec<(AnimationComposition, HashMap<String, AnimatedValue>)> =
        Vec::new();

      for (idx, name) in names.iter().enumerate() {
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

        let progress = match timeline_ref {
          AnimationTimeline::Auto => match animation_time_ms {
            Some(time_ms) => time_based_animation_state_impl(&*style_arc, idx, time_ms, true),
            None => settled_time_based_animation_state(&*style_arc, idx),
          },
          AnimationTimeline::None => None,
          AnimationTimeline::Named(ref timeline_name) => {
            if matches!(play_state, AnimationPlayState::Paused) {
              timeline_scope_resolve(scope, timeline_name).and_then(|state| {
                let active = match state {
                  TimelineState::Scroll { scroll_range, .. } => scroll_range.abs() >= f32::EPSILON,
                  TimelineState::View {
                    target_start,
                    target_end,
                    view_size,
                    scroll_offset,
                    ..
                  } => {
                    view_size.is_finite()
                      && *view_size > f32::EPSILON
                      && target_start.is_finite()
                      && target_end.is_finite()
                      && scroll_offset.is_finite()
                      && (target_end - target_start).abs() > f32::EPSILON
                  }
                  TimelineState::Inactive => false,
                };
                active.then_some(scroll_driven_effect_state(&*style_arc, idx, 0.0))
              })
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
                  } => view_timeline_progress(
                    timeline,
                    *target_start,
                    *target_end,
                    *view_size,
                    *scroll_offset,
                    &range,
                  ),
                  TimelineState::Inactive => None,
                });
              let fill = pick(
                &style_arc.animation_fill_modes,
                idx,
                AnimationFillMode::default(),
              );
              raw
                .and_then(|raw| scroll_driven_fill_progress(raw, fill))
                .map(|overall| scroll_driven_effect_state(&*style_arc, idx, overall))
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
                  0.0,
                  0.0,
                  scroll_container.viewport.width,
                  scroll_container.viewport.height,
                  scroll_container.content.width,
                  scroll_container.content.height,
                );
                (scroll_range.abs() >= f32::EPSILON).then_some(scroll_driven_effect_state(
                  &*style_arc,
                  idx,
                  0.0,
                ))
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
                .map(|overall| scroll_driven_effect_state(&*style_arc, idx, overall))
            }
          }
          AnimationTimeline::View(ref func) => {
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

                let active = view_size.is_finite()
                  && view_size > f32::EPSILON
                  && target_start.is_finite()
                  && target_end.is_finite()
                  && (target_end - target_start).abs() > f32::EPSILON;
                active.then_some(scroll_driven_effect_state(&*style_arc, idx, 0.0))
              })
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
              raw
                .and_then(|raw| scroll_driven_fill_progress(raw, fill))
                .map(|overall| scroll_driven_effect_state(&*style_arc, idx, overall))
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
            );
            let end = sample_keyframes_with_default_timing(
              rule,
              end_progress,
              &animated,
              viewport_size,
              element_size,
              &timing,
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
      child_style.recompute_var_dependent_properties(parent_for_children, viewport_size);
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
      animation_time_ms,
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
      snapshot_style.recompute_var_dependent_properties(parent_for_children, viewport_size);
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
      animation_time_ms,
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
  let root_context =
    root_scroll_container_context(scroll_state, viewport, content, root_writing_mode);

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
      animation_time_ms,
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
      animation_time_ms,
      &mut scope,
      &mut scroll_containers,
      plan,
    );
  }
}

/// Applies scroll/view timeline-driven animations (and settles time-based animations) to a fragment
/// tree using the provided scroll state.
pub fn apply_scroll_driven_animations(tree: &mut FragmentTree, scroll_state: &ScrollState) {
  apply_animations(tree, scroll_state, None);
}

fn interpolated_transition_names() -> impl Iterator<Item = &'static str> {
  property_interpolators().iter().map(|p| p.name)
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
  let duration = pick(durations, idx, *durations.last().unwrap_or(&0.0));
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
  let duration = pick(durations, idx, *durations.last().unwrap_or(&0.0));
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

  let sampled = interpolate_custom_property(&from_val, &to_val, progress, start_style, style, ctx)
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
  // the built-in interpolated properties. Custom property iteration order is not stable, so sort
  // the candidate list to keep transition sampling deterministic.
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
        for name in interpolated_transition_names() {
          insert(name);
        }
        for name in &all_custom_properties {
          insert(name);
        }
      }
      TransitionProperty::Name(name) => insert(name.as_str()),
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
    // Still traverse for running anchors/children.
    for child in fragment.children_mut() {
      apply_transitions_to_fragment(child, time_ms, viewport, log_enabled, parent_styles);
    }
    if let FragmentContent::RunningAnchor { snapshot, .. } = &mut fragment.content {
      apply_transitions_to_fragment(
        Arc::make_mut(snapshot),
        time_ms,
        viewport,
        log_enabled,
        parent_styles,
      );
    }
    return;
  };
  if let Some(start_arc) = fragment.starting_style.clone() {
    if let Some(pairs) = transition_pairs(&style_arc.transition_properties, &start_arc, &style_arc)
    {
      let ctx = AnimationResolveContext::new(
        viewport,
        Size::new(fragment.bounds.width(), fragment.bounds.height()),
      );
      let mut updates: HashMap<String, AnimatedValue> = HashMap::new();
      let mut custom_updates: Vec<(Arc<str>, CustomPropertyValue)> = Vec::new();
      for (name, idx) in pairs {
        let name_str = name;
        let behavior = pick(
          &style_arc.transition_behaviors,
          idx,
          TransitionBehavior::Normal,
        );
        let allow_discrete = matches!(behavior, TransitionBehavior::AllowDiscrete);
        if name_str.starts_with("--") {
          let value = transition_value_for_custom_property(
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

        let value = transition_value_for_property(
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
        );
        if let Some((animated, progress, delay, duration)) = value {
          updates.insert(name_str.to_string(), animated);
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
        let mut updated_style = (*style_arc).clone();
        apply_animated_properties(&mut updated_style, &updates);
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

        if custom_properties_changed {
          let parent_styles = parent_styles.unwrap_or_else(|| default_parent_style());
          updated_style.recompute_var_dependent_properties(parent_styles, viewport);
          apply_animated_properties(&mut updated_style, &updates);
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
  apply_transitions_to_fragment(&mut tree.root, time_ms, viewport, log_enabled, None);
  for root in &mut tree.additional_fragments {
    apply_transitions_to_fragment(root, time_ms, viewport, log_enabled, None);
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
  use crate::style::media::MediaContext;
  use crate::text::font_db::FontConfig;
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
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
    style.animation_durations = vec![1000.0].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    let progress = time_based_animation_progress(&style, 0, 500.0).expect("active");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 0.0).abs() < 1e-6);
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
    style.animation_names = vec!["k".to_string()];
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
    style.animation_names = vec!["tri".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("filled");
    assert!((progress - 1.0).abs() < 1e-6, "progress={progress}");
    assert!((sampled_opacity(&rule, progress) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn settled_time_based_animation_progress_paused_returns_initial_progress() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
    style.animation_durations = vec![1000.0].into();
    style.animation_delays = vec![10_000.0].into();
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_paused_infinite_iterations_is_deterministically_start() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec!["fade".to_string()];
    style.animation_durations = vec![1000.0].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Infinite].into();
    style.animation_play_states = vec![AnimationPlayState::Paused].into();

    let progress = settled_time_based_animation_progress(&style, 0).expect("active");
    assert!((progress - 0.0).abs() < 1e-6, "progress={progress}");
  }

  #[test]
  fn settled_time_based_animation_progress_skips_non_filled_animations() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec!["fade".to_string()];
    style.animation_fill_modes = vec![AnimationFillMode::None].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_skips_infinite_iterations() {
    let mut style = ComputedStyle::default();
    style.animation_names = vec!["fade".to_string()];
    style.animation_fill_modes = vec![AnimationFillMode::Forwards].into();
    style.animation_iteration_counts = vec![AnimationIterationCount::Infinite].into();

    assert_eq!(settled_time_based_animation_progress(&style, 0), None);
  }

  #[test]
  fn settled_time_based_animation_progress_respects_direction_and_iterations() {
    let rule = fade_rule();
    let mut style = ComputedStyle::default();
    style.animation_names = vec!["fade".to_string()];
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
    style.animation_names = vec!["fade".to_string()];
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

    assert_eq!(&*style.animation_names, &["fade".to_string()]);
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
}
