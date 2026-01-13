//! Page rule resolution and page box sizing utilities.

use std::collections::BTreeMap;

use crate::css::types::PropertyValue;
use crate::css::types::{CollectedPageRule, PageMarginArea, PagePseudoClass, PageSelector};
use crate::geometry::{Point, Size};
use crate::style::cascade::inherit_styles;
use crate::style::display::Display;
use crate::style::position::Position;
use crate::style::properties::{
  apply_container_type_implied_containment, apply_content_visibility_implied_containment,
  apply_declaration_with_base, resolve_pending_logical_properties,
};
use crate::style::types::TextAlign;
use crate::style::values::{Length, LengthUnit};
use crate::style::ComputedStyle;

/// Logical side for page selectors (:left/:right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSide {
  Left,
  Right,
}

/// CSS Page selector specificity, expressed as a 3-component tuple (f, g, h).
///
/// See: <https://drafts.csswg.org/css-page-3/#typedef-page-selector>
///
/// - `f`: page type selector (named page)
/// - `g`: `:first` and `:blank` pseudo-class count
/// - `h`: `:left` and `:right` pseudo-class count
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PageSelectorSpecificity {
  f: u8,
  g: u8,
  h: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageOrientation {
  Portrait,
  Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageNamedSize {
  A5,
  A4,
  A3,
  B5,
  B4,
  JisB5,
  JisB4,
  Letter,
  Legal,
  Ledger,
}

#[derive(Debug, Clone, Default)]
struct PageSizeSpec {
  width: Option<Length>,
  height: Option<Length>,
  named: Option<PageNamedSize>,
  orientation: Option<PageOrientation>,
}

/// Computed value for the CSS Paged Media `marks` property.
///
/// This controls which printer marks (crop/cross) are rendered in the page bleed area.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageMarks {
  pub crop: bool,
  pub cross: bool,
}

impl PageMarks {
  pub fn is_none(&self) -> bool {
    !self.crop && !self.cross
  }
}

#[derive(Debug, Clone, Default)]
struct PageProperties {
  size: Option<PageSizeSpec>,
  margin_top: Option<Length>,
  margin_right: Option<Length>,
  margin_bottom: Option<Length>,
  margin_left: Option<Length>,
  bleed: Option<PageBleedValue>,
  trim: Option<Length>,
  marks: Option<PageMarks>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PageBleedValue {
  Auto,
  Length(Length),
}

/// Resolved page box metrics after applying @page rules.
#[derive(Debug, Clone)]
pub struct ResolvedPageStyle {
  pub page_size: Size,
  pub total_size: Size,
  pub content_size: Size,
  pub content_origin: Point,
  pub margin_top: f32,
  pub margin_right: f32,
  pub margin_bottom: f32,
  pub margin_left: f32,
  pub bleed: f32,
  pub trim: f32,
  pub marks: PageMarks,
  pub margin_boxes: BTreeMap<PageMarginArea, ComputedStyle>,
  pub footnote_style: ComputedStyle,
  pub page_style: ComputedStyle,
}

/// Resolve page styles for a specific page index/name/side.
pub fn resolve_page_style(
  rules: &[CollectedPageRule<'_>],
  page_index: usize,
  page_name: Option<&str>,
  side: PageSide,
  is_blank: bool,
  fallback_size: Size,
  root_font_size: f32,
  base_style: Option<&ComputedStyle>,
) -> ResolvedPageStyle {
  let defaults = ComputedStyle::default();
  let inherited_base = base_style.unwrap_or(&defaults);
  let parent_font_size = base_style.map_or(root_font_size, |s| s.font_size);
  let root_font_size = base_style
    .map(|s| s.root_font_size)
    .unwrap_or(root_font_size);
  let mut props = PageProperties::default();
  let mut margin_styles: BTreeMap<PageMarginArea, ComputedStyle> = BTreeMap::new();
  let mut page_style = default_page_style(root_font_size);
  let mut page_context_style = default_page_context_style(base_style, root_font_size);

  let mut matching: Vec<(&CollectedPageRule<'_>, PageSelectorSpecificity)> = Vec::new();
  for rule in rules {
    let mut matched_spec: Option<PageSelectorSpecificity> = None;
    for selector in &rule.rule.selectors {
      if selector_matches(selector, page_index, page_name, side, is_blank) {
        let spec = selector_specificity(selector);
        matched_spec = Some(matched_spec.map_or(spec, |s| s.max(spec)));
      }
    }

    if let Some(spec) = matched_spec {
      matching.push((rule, spec));
    }
  }

  // CSS cascade for @page rules:
  // 1) Apply normal (non-!important) declarations in normal layer order (later layers win).
  // 2) Apply !important declarations with cascade layer order reversed (earlier layers win).
  //
  // Unlayered rules use a sentinel layer order of u32::MAX and therefore:
  // - win over layered rules in the normal cascade (applied last),
  // - lose to layered rules in the important cascade (applied first in reversed order).
  let mut normal_matching = matching.clone();
  normal_matching.sort_by(|a, b| {
    a.0
      .layer_order
      .as_ref()
      .cmp(b.0.layer_order.as_ref())
      .then(a.1.cmp(&b.1))
      .then(a.0.order.cmp(&b.0.order))
  });
  let mut important_matching = matching;
  important_matching.sort_by(|a, b| {
    b.0
      .layer_order
      .as_ref()
      .cmp(a.0.layer_order.as_ref())
      .then(a.1.cmp(&b.1))
      .then(a.0.order.cmp(&b.0.order))
  });

  let mut apply_page_rule_declarations = |rule: &CollectedPageRule<'_>, important: bool| {
    for decl in &rule.rule.declarations {
      if decl.important != important {
        continue;
      }
      if apply_page_declaration(&mut props, decl) {
        continue;
      }
      apply_declaration_with_base(
        &mut page_context_style,
        decl,
        inherited_base,
        &defaults,
        None,
        parent_font_size,
        root_font_size,
        fallback_size,
        false,
      );
      apply_page_box_declaration(
        &mut page_style,
        decl,
        &defaults,
        root_font_size,
        fallback_size,
      );
    }
  };

  for (rule, _) in &normal_matching {
    apply_page_rule_declarations(rule, false);
  }
  for (rule, _) in &important_matching {
    apply_page_rule_declarations(rule, true);
  }

  let mut footnote_style = default_footnote_style(&page_context_style);
  let mut apply_footnote_rule_declarations = |rule: &CollectedPageRule<'_>, important: bool| {
    for footnote_rule in &rule.rule.footnote_rules {
      for decl in &footnote_rule.declarations {
        if decl.important != important {
          continue;
        }
        if !footnote_area_property_allowed(decl.property.as_str()) {
          continue;
        }
        apply_declaration_with_base(
          &mut footnote_style,
          decl,
          &page_context_style,
          &defaults,
          None,
          page_context_style.font_size,
          root_font_size,
          fallback_size,
          false,
        );
      }
    }
  };

  for (rule, _) in &normal_matching {
    apply_footnote_rule_declarations(rule, false);
  }
  for (rule, _) in &important_matching {
    apply_footnote_rule_declarations(rule, true);
  }

  let mut apply_margin_rule_declarations = |rule: &CollectedPageRule<'_>, important: bool| {
    for margin_rule in &rule.rule.margin_rules {
      let style = margin_styles.entry(margin_rule.area).or_insert_with(|| {
        let mut style = default_margin_style(&page_context_style);
        style.text_align = default_margin_text_align(margin_rule.area);
        style
      });
      for decl in &margin_rule.declarations {
        if decl.important != important {
          continue;
        }
        if !margin_box_property_allowed(decl.property.as_str()) {
          continue;
        }
        apply_declaration_with_base(
          style,
          decl,
          &page_context_style,
          &defaults,
          None,
          page_context_style.font_size,
          root_font_size,
          fallback_size,
          false,
        );
      }
    }
  };

  for (rule, _) in &normal_matching {
    apply_margin_rule_declarations(rule, false);
  }
  for (rule, _) in &important_matching {
    apply_margin_rule_declarations(rule, true);
  }

  for style in margin_styles.values_mut() {
    resolve_pending_logical_properties(style);
    apply_container_type_implied_containment(style);
    apply_content_visibility_implied_containment(style);
    // CSS Page 3: `display` and `position` do not apply to page-margin boxes.
    style.display = Display::Block;
    style.position = Position::Static;
  }
  resolve_pending_logical_properties(&mut footnote_style);
  apply_container_type_implied_containment(&mut footnote_style);
  apply_content_visibility_implied_containment(&mut footnote_style);
  // The footnote area should behave like a block container. Leave positioning to pagination.
  footnote_style.display = Display::Block;
  footnote_style.position = Position::Static;
  resolve_pending_logical_properties(&mut page_style);
  apply_container_type_implied_containment(&mut page_style);
  apply_content_visibility_implied_containment(&mut page_style);
  if matches!(page_style.display, Display::Inline) {
    page_style.display = Display::Block;
  }
  page_style.root_font_size = root_font_size;
  page_style.font_size = root_font_size;

  let (page_width, page_height) = resolve_page_size(&props, fallback_size, root_font_size);
  let marks = props.marks.unwrap_or_default();
  let bleed = match props.bleed.unwrap_or(PageBleedValue::Auto) {
    PageBleedValue::Length(l) => resolve_length_on_axis(
      &l,
      page_width.max(page_height),
      fallback_size,
      root_font_size,
    )
    .unwrap_or(0.0),
    // CSS Page 3: https://drafts.csswg.org/css-page-3/#bleed
    // "auto" computes to:
    // - 6pt when crop marks are requested
    // - otherwise 0
    PageBleedValue::Auto => {
      if marks.crop {
        resolve_length_on_axis(
          &Length::pt(6.0),
          page_width.max(page_height),
          fallback_size,
          root_font_size,
        )
        .unwrap_or(0.0)
      } else {
        0.0
      }
    }
  };
  let bleed = bleed.is_finite().then_some(bleed).unwrap_or(0.0);
  let trim = props
    .trim
    .and_then(|l| {
      resolve_length_on_axis(
        &l,
        page_width.max(page_height),
        fallback_size,
        root_font_size,
      )
    })
    .unwrap_or(0.0)
    .max(0.0);

  let margin_top = resolve_length_on_axis(
    &props.margin_top.unwrap_or_else(|| Length::px(0.0)),
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let margin_bottom = resolve_length_on_axis(
    &props.margin_bottom.unwrap_or_else(|| Length::px(0.0)),
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let margin_left = resolve_length_on_axis(
    &props.margin_left.unwrap_or_else(|| Length::px(0.0)),
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let margin_right = resolve_length_on_axis(
    &props.margin_right.unwrap_or_else(|| Length::px(0.0)),
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);

  let page_box_width = (page_width - 2.0 * trim - margin_left - margin_right).max(0.0);
  let page_box_height = (page_height - 2.0 * trim - margin_top - margin_bottom).max(0.0);

  // The page box's border/padding further reduce the available size for laying out the document
  // contents. These properties participate in the page box model the same way as for regular
  // block boxes: the document is laid out inside the page box's content box (inside padding).
  let border_left = resolve_length_on_axis(
    &page_style.used_border_left_width(),
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let border_right = resolve_length_on_axis(
    &page_style.used_border_right_width(),
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let border_top = resolve_length_on_axis(
    &page_style.used_border_top_width(),
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let border_bottom = resolve_length_on_axis(
    &page_style.used_border_bottom_width(),
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);

  let padding_left = resolve_length_on_axis(
    &page_style.padding_left,
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let padding_right = resolve_length_on_axis(
    &page_style.padding_right,
    page_width,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let padding_top = resolve_length_on_axis(
    &page_style.padding_top,
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);
  let padding_bottom = resolve_length_on_axis(
    &page_style.padding_bottom,
    page_height,
    fallback_size,
    root_font_size,
  )
  .unwrap_or(0.0)
  .max(0.0);

  let content_width =
    (page_box_width - border_left - border_right - padding_left - padding_right).max(0.0);
  let content_height =
    (page_box_height - border_top - border_bottom - padding_top - padding_bottom).max(0.0);

  let content_origin = Point::new(
    bleed + trim + margin_left + border_left + padding_left,
    bleed + trim + margin_top + border_top + padding_top,
  );
  let total_size = Size::new(
    (page_width + 2.0 * bleed).max(0.0),
    (page_height + 2.0 * bleed).max(0.0),
  );

  ResolvedPageStyle {
    page_size: Size::new(page_width, page_height),
    total_size,
    content_size: Size::new(content_width, content_height),
    content_origin,
    margin_top,
    margin_right,
    margin_bottom,
    margin_left,
    bleed,
    trim,
    marks,
    margin_boxes: margin_styles,
    footnote_style,
    page_style,
  }
}

fn selector_matches(
  selector: &PageSelector,
  page_index: usize,
  page_name: Option<&str>,
  side: PageSide,
  is_blank: bool,
) -> bool {
  if let Some(name) = &selector.name {
    if let Some(actual) = page_name {
      if actual != name {
        return false;
      }
    } else {
      return false;
    }
  }

  for pseudo in &selector.pseudos {
    match pseudo {
      PagePseudoClass::First => {
        if page_index != 0 {
          return false;
        }
      }
      PagePseudoClass::Left => {
        if side != PageSide::Left {
          return false;
        }
      }
      PagePseudoClass::Right => {
        if side != PageSide::Right {
          return false;
        }
      }
      PagePseudoClass::Blank => {
        if !is_blank {
          return false;
        }
      }
    }
  }

  true
}

fn selector_specificity(selector: &crate::css::types::PageSelector) -> PageSelectorSpecificity {
  let mut g = 0u8;
  let mut h = 0u8;
  for pseudo in &selector.pseudos {
    match pseudo {
      PagePseudoClass::First | PagePseudoClass::Blank => {
        g = g.saturating_add(1);
      }
      PagePseudoClass::Left | PagePseudoClass::Right => {
        h = h.saturating_add(1);
      }
    }
  }

  PageSelectorSpecificity {
    f: u8::from(selector.name.is_some()),
    g,
    h,
  }
}

fn apply_page_declaration(
  props: &mut PageProperties,
  decl: &crate::css::types::Declaration,
) -> bool {
  match decl.property.as_str() {
    "size" => {
      if let Some(size) = parse_page_size_value(&decl.value) {
        props.size = Some(size);
      }
      true
    }
    "margin" => {
      if let Some(values) = parse_margin_shorthand(&decl.value) {
        props.margin_top = Some(values[0]);
        props.margin_right = Some(values[1]);
        props.margin_bottom = Some(values[2]);
        props.margin_left = Some(values[3]);
      }
      true
    }
    "margin-top" => {
      if let Some(len) = length_from_value(&decl.value) {
        props.margin_top = Some(len);
      }
      true
    }
    "margin-right" => {
      if let Some(len) = length_from_value(&decl.value) {
        props.margin_right = Some(len);
      }
      true
    }
    "margin-bottom" => {
      if let Some(len) = length_from_value(&decl.value) {
        props.margin_bottom = Some(len);
      }
      true
    }
    "margin-left" => {
      if let Some(len) = length_from_value(&decl.value) {
        props.margin_left = Some(len);
      }
      true
    }
    "bleed" => {
      if let Some(bleed) = parse_page_bleed_value(&decl.value) {
        props.bleed = Some(bleed);
      }
      true
    }
    "trim" => {
      if let Some(len) = length_from_value(&decl.value) {
        props.trim = Some(len);
      }
      true
    }
    "marks" => {
      if let Some(marks) = parse_page_marks_value(&decl.value) {
        props.marks = Some(marks);
      }
      true
    }
    _ => false,
  }
}

fn parse_page_marks_value(value: &PropertyValue) -> Option<PageMarks> {
  fn keyword_to_mark(value: &str, marks: &mut PageMarks) -> Result<(), ()> {
    match value {
      "crop" => marks.crop = true,
      "cross" => marks.cross = true,
      _ => return Err(()),
    }
    Ok(())
  }

  match value {
    PropertyValue::Keyword(kw) => {
      let lower = kw.to_ascii_lowercase();
      match lower.as_str() {
        "none" => Some(PageMarks::default()),
        other => {
          let mut marks = PageMarks::default();
          keyword_to_mark(other, &mut marks).ok()?;
          Some(marks)
        }
      }
    }
    PropertyValue::Multiple(values) => {
      if values.is_empty() {
        return None;
      }

      let mut marks = PageMarks::default();
      let mut has_none = false;

      for part in values {
        let PropertyValue::Keyword(kw) = part else {
          return None;
        };
        let lower = kw.to_ascii_lowercase();
        if lower == "none" {
          has_none = true;
          continue;
        }
        keyword_to_mark(lower.as_str(), &mut marks).ok()?;
      }

      if has_none {
        // `none` is only valid as a single keyword.
        return (values.len() == 1).then_some(PageMarks::default());
      }

      (!marks.is_none()).then_some(marks)
    }
    _ => None,
  }
}

fn parse_page_size_value(value: &PropertyValue) -> Option<PageSizeSpec> {
  let mut spec = PageSizeSpec::default();
  match value {
    PropertyValue::Keyword(kw) => {
      let lower = kw.to_ascii_lowercase();
      if lower == "auto" {
        return None;
      }
      if let Some(named) = named_size(&lower) {
        spec.named = Some(named);
      } else if let Some(orientation) = orientation_from_keyword(&lower) {
        spec.orientation = Some(orientation);
      }
    }
    PropertyValue::Length(len) => {
      spec.width = Some(*len);
    }
    PropertyValue::Multiple(values) => {
      for part in values {
        match part {
          PropertyValue::Length(len) => {
            if spec.width.is_none() {
              spec.width = Some(*len);
            } else if spec.height.is_none() {
              spec.height = Some(*len);
            }
          }
          PropertyValue::Keyword(kw) => {
            let lower = kw.to_ascii_lowercase();
            if let Some(orientation) = orientation_from_keyword(&lower) {
              spec.orientation = Some(orientation);
              continue;
            }
            if let Some(named) = named_size(&lower) {
              spec.named = Some(named);
            }
          }
          _ => {}
        }
      }
    }
    _ => {}
  }

  if spec.width.is_some()
    || spec.height.is_some()
    || spec.named.is_some()
    || spec.orientation.is_some()
  {
    Some(spec)
  } else {
    None
  }
}

fn orientation_from_keyword(value: &str) -> Option<PageOrientation> {
  match value {
    "landscape" => Some(PageOrientation::Landscape),
    "portrait" => Some(PageOrientation::Portrait),
    _ => None,
  }
}

fn named_size(value: &str) -> Option<PageNamedSize> {
  match value {
    "a5" => Some(PageNamedSize::A5),
    "a4" => Some(PageNamedSize::A4),
    "a3" => Some(PageNamedSize::A3),
    "b5" => Some(PageNamedSize::B5),
    "b4" => Some(PageNamedSize::B4),
    "jis-b5" => Some(PageNamedSize::JisB5),
    "jis-b4" => Some(PageNamedSize::JisB4),
    "letter" => Some(PageNamedSize::Letter),
    "legal" => Some(PageNamedSize::Legal),
    // CSS Paged Media 3 defines `ledger` as 11in×17in. Accept `tabloid` as a common alias
    // used by author stylesheets and tooling.
    "ledger" | "tabloid" => Some(PageNamedSize::Ledger),
    _ => None,
  }
}

fn length_from_value(value: &PropertyValue) -> Option<Length> {
  match value {
    PropertyValue::Length(l) => Some(*l),
    PropertyValue::Number(n) => Some(Length::px(*n)),
    PropertyValue::Percentage(p) => Some(Length::percent(*p)),
    PropertyValue::Keyword(k) if k.eq_ignore_ascii_case("auto") => Some(Length::px(0.0)),
    _ => None,
  }
}

fn parse_page_bleed_value(value: &PropertyValue) -> Option<PageBleedValue> {
  match value {
    PropertyValue::Keyword(k) if k.eq_ignore_ascii_case("auto") => Some(PageBleedValue::Auto),
    other => length_from_value(other).map(PageBleedValue::Length),
  }
}

fn parse_margin_shorthand(value: &PropertyValue) -> Option<[Length; 4]> {
  let mut values = Vec::new();
  match value {
    PropertyValue::Multiple(list) => {
      for part in list {
        if let Some(len) = length_from_value(part) {
          values.push(len);
        }
      }
    }
    _ => {
      if let Some(len) = length_from_value(value) {
        values.push(len);
      }
    }
  }

  if values.is_empty() {
    return None;
  }

  let resolved = match values.len() {
    1 => [values[0], values[0], values[0], values[0]],
    2 => [values[0], values[1], values[0], values[1]],
    3 => [values[0], values[1], values[2], values[1]],
    _ => [values[0], values[1], values[2], values[3]],
  };
  Some(resolved)
}

fn resolve_page_size(props: &PageProperties, fallback: Size, root_font_size: f32) -> (f32, f32) {
  let mut width = fallback.width;
  let mut height = fallback.height;

  if let Some(spec) = &props.size {
    if let Some(named) = spec.named {
      let dims = named.dimensions();
      width = dims.width;
      height = dims.height;
    }

    if let Some(w) = spec
      .width
      .and_then(|l| resolve_length_on_axis(&l, fallback.width, fallback, root_font_size))
    {
      width = w;
    }
    if let Some(h) = spec
      .height
      .and_then(|l| resolve_length_on_axis(&l, fallback.height, fallback, root_font_size))
    {
      height = h;
    }

    if let Some(orientation) = spec.orientation {
      match orientation {
        PageOrientation::Landscape if height > width => {
          std::mem::swap(&mut width, &mut height);
        }
        PageOrientation::Portrait if width > height => {
          std::mem::swap(&mut width, &mut height);
        }
        _ => {}
      }
    }
  }

  (width.max(0.0), height.max(0.0))
}

fn resolve_length_on_axis(
  length: &Length,
  axis: f32,
  viewport: Size,
  root_font_size: f32,
) -> Option<f32> {
  let percent_base = if length.unit == LengthUnit::Percent {
    Some(axis)
  } else {
    None
  };
  length.resolve_with_context(
    percent_base,
    viewport.width,
    viewport.height,
    root_font_size,
    root_font_size,
  )
}

fn default_page_context_style(
  base_style: Option<&ComputedStyle>,
  root_font_size: f32,
) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  if let Some(base) = base_style {
    inherit_styles(&mut style, base);
    style.root_font_size = base.root_font_size;
  } else {
    style.root_font_size = root_font_size;
    style.font_size = root_font_size;
  }
  style
}

fn default_margin_style(page_context_style: &ComputedStyle) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  inherit_styles(&mut style, page_context_style);
  style.display = Display::Block;
  style
}

fn default_footnote_style(page_context_style: &ComputedStyle) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  inherit_styles(&mut style, page_context_style);
  style.display = Display::Block;
  style
}

fn footnote_area_property_allowed(property: &str) -> bool {
  // CSS GCPM's `@page { @footnote { ... } }` defines the styling/geometry of the footnote area.
  //
  // FastRender currently restricts this to a small, page-layout relevant subset:
  // - Sizing: `max-height`
  // - Box model: border/padding
  // - Paint: background/color
  // - Typography: `font-*` and `font`
  //
  // Other properties are ignored for now; pagination is responsible for positioning the footnote
  // area and stacking its children.
  property == "max-height"
    || property == "color"
    || property == "background"
    || property.starts_with("background-")
    || property == "border"
    || property.starts_with("border-")
    || property == "padding"
    || property.starts_with("padding-")
    || property == "font"
    || property.starts_with("font-")
}

fn margin_box_property_allowed(property: &str) -> bool {
  // CSS Page 3: `display` and `position` do not apply to page-margin boxes.
  !matches!(property, "display" | "position")
}

fn default_margin_text_align(area: PageMarginArea) -> TextAlign {
  match area {
    PageMarginArea::TopLeftCorner
    | PageMarginArea::TopLeft
    | PageMarginArea::BottomLeft
    | PageMarginArea::BottomLeftCorner
    | PageMarginArea::LeftTop
    | PageMarginArea::LeftMiddle
    | PageMarginArea::LeftBottom => TextAlign::Left,
    PageMarginArea::TopCenter | PageMarginArea::BottomCenter => TextAlign::Center,
    PageMarginArea::TopRightCorner
    | PageMarginArea::TopRight
    | PageMarginArea::BottomRight
    | PageMarginArea::BottomRightCorner
    | PageMarginArea::RightTop
    | PageMarginArea::RightMiddle
    | PageMarginArea::RightBottom => TextAlign::Right,
  }
}

fn default_page_style(root_font_size: f32) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.root_font_size = root_font_size;
  style.font_size = root_font_size;
  style
}

fn apply_page_box_declaration(
  style: &mut ComputedStyle,
  decl: &crate::css::types::Declaration,
  defaults: &ComputedStyle,
  root_font_size: f32,
  viewport: Size,
) -> bool {
  if !page_box_property_allowed(decl.property.as_str()) {
    return false;
  }

  apply_declaration_with_base(
    style,
    decl,
    defaults,
    defaults,
    None,
    root_font_size,
    root_font_size,
    viewport,
    false,
  );
  true
}

fn page_box_property_allowed(property: &str) -> bool {
  matches!(
    property,
    "background"
      | "background-color"
      | "background-image"
      | "background-size"
      | "background-repeat"
      | "background-position"
      | "background-position-x"
      | "background-position-y"
      | "background-attachment"
      | "background-origin"
      | "background-clip"
      | "background-blend-mode"
      | "border"
      | "border-color"
      | "border-style"
      | "border-width"
      | "border-top"
      | "border-right"
      | "border-bottom"
      | "border-left"
      | "border-top-color"
      | "border-right-color"
      | "border-bottom-color"
      | "border-left-color"
      | "border-top-style"
      | "border-right-style"
      | "border-bottom-style"
      | "border-left-style"
      | "border-top-width"
      | "border-right-width"
      | "border-bottom-width"
      | "border-left-width"
      | "border-radius"
      | "border-top-left-radius"
      | "border-top-right-radius"
      | "border-bottom-left-radius"
      | "border-bottom-right-radius"
      | "box-shadow"
      | "color"
  )
}

impl PageNamedSize {
  fn dimensions(self) -> Size {
    match self {
      PageNamedSize::A5 => Size::new(mm_to_px(148.0), mm_to_px(210.0)),
      PageNamedSize::A4 => Size::new(mm_to_px(210.0), mm_to_px(297.0)),
      PageNamedSize::A3 => Size::new(mm_to_px(297.0), mm_to_px(420.0)),
      PageNamedSize::B5 => Size::new(mm_to_px(176.0), mm_to_px(250.0)),
      PageNamedSize::B4 => Size::new(mm_to_px(250.0), mm_to_px(353.0)),
      PageNamedSize::JisB5 => Size::new(mm_to_px(182.0), mm_to_px(257.0)),
      PageNamedSize::JisB4 => Size::new(mm_to_px(257.0), mm_to_px(364.0)),
      PageNamedSize::Letter => Size::new(in_to_px(8.5), in_to_px(11.0)),
      PageNamedSize::Legal => Size::new(in_to_px(8.5), in_to_px(14.0)),
      PageNamedSize::Ledger => Size::new(in_to_px(11.0), in_to_px(17.0)),
    }
  }
}

fn mm_to_px(mm: f32) -> f32 {
  mm / 25.4 * 96.0
}

fn in_to_px(inches: f32) -> f32 {
  inches * 96.0
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::parser::parse_stylesheet;
  use crate::style::media::MediaContext;
  use std::collections::BTreeMap;

  #[test]
  fn margin_box_styles_iterate_in_enum_order() {
    let canonical = vec![
      PageMarginArea::TopLeftCorner,
      PageMarginArea::TopLeft,
      PageMarginArea::TopCenter,
      PageMarginArea::TopRight,
      PageMarginArea::TopRightCorner,
      PageMarginArea::RightTop,
      PageMarginArea::RightMiddle,
      PageMarginArea::RightBottom,
      PageMarginArea::BottomRightCorner,
      PageMarginArea::BottomRight,
      PageMarginArea::BottomCenter,
      PageMarginArea::BottomLeft,
      PageMarginArea::BottomLeftCorner,
      PageMarginArea::LeftBottom,
      PageMarginArea::LeftMiddle,
      PageMarginArea::LeftTop,
    ];

    // Insert in reverse order to ensure the iteration order comes from the map's key ordering.
    let mut margin_boxes: BTreeMap<PageMarginArea, ComputedStyle> = BTreeMap::new();
    for area in canonical.iter().rev() {
      margin_boxes.insert(*area, ComputedStyle::default());
    }

    let style = ResolvedPageStyle {
      page_size: Size::new(0.0, 0.0),
      total_size: Size::new(0.0, 0.0),
      content_size: Size::new(0.0, 0.0),
      content_origin: Point::new(0.0, 0.0),
      margin_top: 0.0,
      margin_right: 0.0,
      margin_bottom: 0.0,
      margin_left: 0.0,
      bleed: 0.0,
      trim: 0.0,
      marks: PageMarks::default(),
      margin_boxes,
      footnote_style: ComputedStyle::default(),
      page_style: ComputedStyle::default(),
    };

    let iterated: Vec<PageMarginArea> = style.margin_boxes.keys().copied().collect();
    assert_eq!(iterated, canonical);
  }

  #[test]
  fn page_size_parses_named_sizes_from_css_page_3() {
    let cases: &[(&str, PageNamedSize, Size)] = &[
      (
        "A5",
        PageNamedSize::A5,
        Size::new(mm_to_px(148.0), mm_to_px(210.0)),
      ),
      (
        "A4",
        PageNamedSize::A4,
        Size::new(mm_to_px(210.0), mm_to_px(297.0)),
      ),
      (
        "A3",
        PageNamedSize::A3,
        Size::new(mm_to_px(297.0), mm_to_px(420.0)),
      ),
      (
        "B5",
        PageNamedSize::B5,
        Size::new(mm_to_px(176.0), mm_to_px(250.0)),
      ),
      (
        "B4",
        PageNamedSize::B4,
        Size::new(mm_to_px(250.0), mm_to_px(353.0)),
      ),
      (
        "JIS-B5",
        PageNamedSize::JisB5,
        Size::new(mm_to_px(182.0), mm_to_px(257.0)),
      ),
      (
        "JIS-B4",
        PageNamedSize::JisB4,
        Size::new(mm_to_px(257.0), mm_to_px(364.0)),
      ),
      (
        "letter",
        PageNamedSize::Letter,
        Size::new(in_to_px(8.5), in_to_px(11.0)),
      ),
      (
        "legal",
        PageNamedSize::Legal,
        Size::new(in_to_px(8.5), in_to_px(14.0)),
      ),
      (
        "ledger",
        PageNamedSize::Ledger,
        Size::new(in_to_px(11.0), in_to_px(17.0)),
      ),
      // `tabloid` is a widely-used alias for `ledger` and should parse to the same dimensions.
      (
        "tabloid",
        PageNamedSize::Ledger,
        Size::new(in_to_px(11.0), in_to_px(17.0)),
      ),
    ];

    for (keyword, expected_named, expected_size) in cases {
      let spec = parse_page_size_value(&PropertyValue::Keyword((*keyword).to_string()))
        .unwrap_or_else(|| panic!("expected parse_page_size_value({keyword}) to succeed"));
      assert_eq!(
        spec.named,
        Some(*expected_named),
        "expected {keyword} to map to {:?}, got {:?}",
        expected_named,
        spec.named
      );

      let actual_size = expected_named.dimensions();
      assert!(
        (actual_size.width - expected_size.width).abs() < 0.01
          && (actual_size.height - expected_size.height).abs() < 0.01,
        "expected {keyword} to resolve to {:?}, got {:?}",
        expected_size,
        actual_size
      );
    }
  }

  #[test]
  fn page_size_orientation_applies_to_named_sizes() {
    let spec = parse_page_size_value(&PropertyValue::Multiple(vec![
      PropertyValue::Keyword("B5".into()),
      PropertyValue::Keyword("landscape".into()),
    ]))
    .expect("parse size spec");

    let props = PageProperties {
      size: Some(spec),
      ..PageProperties::default()
    };

    let fallback = Size::new(800.0, 600.0);
    let (width, height) = resolve_page_size(&props, fallback, 16.0);
    let dims = PageNamedSize::B5.dimensions();
    assert!(
      (width - dims.height).abs() < 0.01 && (height - dims.width).abs() < 0.01,
      "expected B5 landscape to swap axes: got {}x{}, expected {}x{}",
      width,
      height,
      dims.height,
      dims.width
    );
  }

  #[test]
  fn page_bleed_auto_resolves_to_6pt_when_crop_marks_requested() {
    let css = "@page { marks: crop; }";
    let sheet = parse_stylesheet(css).expect("parse stylesheet");
    let media = MediaContext::print(800.0, 600.0);
    let collected = sheet.collect_page_rules(&media);

    let resolved = resolve_page_style(
      &collected,
      0,
      None,
      PageSide::Right,
      false,
      Size::new(800.0, 600.0),
      16.0,
      None,
    );

    let expected = resolve_length_on_axis(&Length::pt(6.0), 800.0, Size::new(800.0, 600.0), 16.0)
      .expect("resolve 6pt");
    assert!(
      (resolved.bleed - expected).abs() < 0.01,
      "expected bleed to resolve to 6pt ({}px), got {}",
      expected,
      resolved.bleed
    );
    assert!(
      (resolved.total_size.width - (resolved.page_size.width + 2.0 * resolved.bleed)).abs() < 0.01
        && (resolved.total_size.height - (resolved.page_size.height + 2.0 * resolved.bleed)).abs()
          < 0.01,
      "expected total_size to expand by 2*bleed, got {:?} for page {:?} bleed {}",
      resolved.total_size,
      resolved.page_size,
      resolved.bleed
    );
  }

  #[test]
  fn page_bleed_auto_resolves_to_zero_without_crop_marks() {
    let css = "@page { marks: none; }";
    let sheet = parse_stylesheet(css).expect("parse stylesheet");
    let media = MediaContext::print(800.0, 600.0);
    let collected = sheet.collect_page_rules(&media);

    let resolved = resolve_page_style(
      &collected,
      0,
      None,
      PageSide::Right,
      false,
      Size::new(800.0, 600.0),
      16.0,
      None,
    );

    assert_eq!(resolved.bleed, 0.0);
  }

  #[test]
  fn page_bleed_allows_negative_lengths() {
    let fallback = Size::new(800.0, 600.0);
    let base = resolve_page_style(&[], 0, None, PageSide::Right, false, fallback, 16.0, None);

    let css = "@page { bleed: -5px; }";
    let sheet = parse_stylesheet(css).expect("parse stylesheet");
    let media = MediaContext::print(800.0, 600.0);
    let collected = sheet.collect_page_rules(&media);
    let resolved = resolve_page_style(
      &collected,
      0,
      None,
      PageSide::Right,
      false,
      fallback,
      16.0,
      None,
    );

    assert!(
      (resolved.bleed + 5.0).abs() < 0.01,
      "expected negative bleed (-5px), got {}",
      resolved.bleed
    );
    assert!(
      (resolved.total_size.width - (base.total_size.width - 10.0)).abs() < 0.01
        && (resolved.total_size.height - (base.total_size.height - 10.0)).abs() < 0.01,
      "expected total_size to shrink by 10px, base {:?}, got {:?}",
      base.total_size,
      resolved.total_size
    );
    assert!(
      (resolved.content_origin.x - (base.content_origin.x - 5.0)).abs() < 0.01
        && (resolved.content_origin.y - (base.content_origin.y - 5.0)).abs() < 0.01,
      "expected content_origin to shift by bleed (-5px), base {:?}, got {:?}",
      base.content_origin,
      resolved.content_origin
    );
  }
}
