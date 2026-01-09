use chrono::{Datelike, NaiveDate, Weekday};
use regex::Regex;
use url::Url;

use super::DomNode;
use super::DomNodeType;
use super::ElementRef;
use std::ptr;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ValidityState {
  pub value_missing: bool,
  pub type_mismatch: bool,
  pub pattern_mismatch: bool,
  pub too_long: bool,
  pub too_short: bool,
  pub range_underflow: bool,
  pub range_overflow: bool,
  pub step_mismatch: bool,
  pub bad_input: bool,
  pub custom_error: bool,
  pub valid: bool,
}

impl ValidityState {
  fn compute_validity(&mut self) {
    self.valid = !(self.value_missing
      || self.type_mismatch
      || self.pattern_mismatch
      || self.too_long
      || self.too_short
      || self.range_underflow
      || self.range_overflow
      || self.step_mismatch
      || self.bad_input
      || self.custom_error);
  }

  fn valid() -> Self {
    Self {
      valid: true,
      ..Self::default()
    }
  }
}

fn utf16_len(s: &str) -> usize {
  s.encode_utf16().count()
}

fn parse_non_negative_integer(value: &str) -> Option<usize> {
  let trimmed = super::trim_ascii_whitespace_html(value);
  if trimmed.is_empty() {
    return None;
  }
  let parsed = trimmed.parse::<i64>().ok()?;
  if parsed < 0 {
    return None;
  }
  usize::try_from(parsed).ok()
}

fn value_is_empty(value: &str) -> bool {
  value.is_empty()
}

fn parse_step_attribute(value: &str) -> Step {
  let trimmed = super::trim_ascii_whitespace_html(value);
  if trimmed.eq_ignore_ascii_case("any") {
    return Step::Any;
  }
  let Some(step) = super::parse_finite_number(trimmed).filter(|v| *v > 0.0) else {
    return Step::Default;
  };
  Step::Explicit(step)
}

#[derive(Debug, Clone, Copy)]
enum Step {
  Any,
  Default,
  Explicit(f64),
}

fn step_value_number(step: Step, default: f64) -> Option<f64> {
  match step {
    Step::Any => None,
    Step::Default => Some(default),
    Step::Explicit(v) => Some(v),
  }
}

fn is_step_mismatch(value: f64, step: f64, base: f64) -> bool {
  // The HTML spec defines step mismatch in terms of exact rational arithmetic. We use floating point
  // but include a tight epsilon to avoid false mismatches for common decimal values.
  let q = (value - base) / step;
  let nearest = q.round();
  let diff = (q - nearest).abs();
  diff > 1e-9
}

fn parse_email_address(value: &str) -> bool {
  if value.is_empty() {
    return false;
  }
  if value.chars().any(super::is_ascii_whitespace_html) {
    return false;
  }
  let mut parts = value.split('@');
  let local = parts.next().unwrap_or_default();
  let domain = parts.next();
  if parts.next().is_some() {
    return false;
  }
  let Some(domain) = domain else {
    return false;
  };
  !local.is_empty() && !domain.is_empty()
}

fn email_type_mismatch(value: &str, multiple: bool) -> bool {
  if value.is_empty() {
    return false;
  }
  if multiple {
    // HTML's multiple email parsing trims ASCII whitespace around comma-separated entries.
    for entry in value.split(',') {
      let entry = super::trim_ascii_whitespace_html(entry);
      if entry.is_empty() || !parse_email_address(entry) {
        return true;
      }
    }
    false
  } else {
    !parse_email_address(value)
  }
}

fn url_type_mismatch(value: &str) -> bool {
  if value.is_empty() {
    return false;
  }
  Url::parse(value).is_err()
}

fn pattern_mismatch(pattern: &str, value: &str) -> bool {
  if value.is_empty() {
    return false;
  }

  // The HTML pattern attribute is implicitly anchored to the entire value.
  let wrapped = format!("^(?:{pattern})$");
  let Ok(re) = Regex::new(&wrapped) else {
    return false;
  };
  !re.is_match(value)
}

fn apply_length_constraints(state: &mut ValidityState, value: &str, node: &DomNode) {
  let len = utf16_len(value);

  if let Some(max) = node.get_attribute_ref("maxlength").and_then(parse_non_negative_integer) {
    if len > max {
      state.too_long = true;
    }
  }

  if !value.is_empty() {
    if let Some(min) = node.get_attribute_ref("minlength").and_then(parse_non_negative_integer) {
      if len < min {
        state.too_short = true;
      }
    }
  }
}

fn parse_time_value(value: &str) -> Option<i64> {
  // https://html.spec.whatwg.org/#time-state-(type=time)
  // Supported formats:
  //   HH:MM
  //   HH:MM:SS
  //   HH:MM:SS.sss (fractional seconds)
  let (h, rest) = value.split_once(':')?;
  let hour: u32 = h.parse().ok()?;
  if hour > 23 {
    return None;
  }

  let (m, rest) = match rest.split_once(':') {
    Some((m, rest)) => (m, Some(rest)),
    None => (rest, None),
  };
  let minute: u32 = m.parse().ok()?;
  if minute > 59 {
    return None;
  }

  let mut second: u32 = 0;
  let mut millis: u32 = 0;

  if let Some(rest) = rest {
    let (s, frac) = match rest.split_once('.') {
      Some((s, frac)) => (s, Some(frac)),
      None => (rest, None),
    };
    second = s.parse().ok()?;
    if second > 59 {
      return None;
    }

    if let Some(frac) = frac {
      if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
      }
      millis = frac.parse().ok()?;
      // Normalize 1-2 digit fractional seconds to milliseconds.
      if frac.len() == 1 {
        millis *= 100;
      } else if frac.len() == 2 {
        millis *= 10;
      }
    }
  }

  let total_ms = (hour as i64) * 3_600_000 + (minute as i64) * 60_000 + (second as i64) * 1_000 + millis as i64;
  Some(total_ms)
}

fn parse_date_value(value: &str) -> Option<i32> {
  let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
  Some(date.num_days_from_ce())
}

fn parse_datetime_local_value(value: &str) -> Option<i64> {
  let (date, time) = value.split_once('T')?;
  let date_days = parse_date_value(date)? as i64;
  let time_ms = parse_time_value(time)?;
  Some(date_days * 86_400_000 + time_ms)
}

fn parse_month_value(value: &str) -> Option<i32> {
  let (year, month) = value.split_once('-')?;
  let year: i32 = year.parse().ok()?;
  let month: u32 = month.parse().ok()?;
  if !(1..=12).contains(&month) {
    return None;
  }
  year.checked_mul(12)?.checked_add((month as i32) - 1)
}

fn parse_week_value(value: &str) -> Option<i32> {
  let (year, week) = value.split_once("-W")?;
  let year: i32 = year.parse().ok()?;
  let week: u32 = week.parse().ok()?;
  // Use Monday of the given ISO week for comparisons.
  let date = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)?;
  Some(date.num_days_from_ce())
}

fn range_flags_numeric(state: &mut ValidityState, value: f64, node: &DomNode) -> Option<(Option<f64>, Option<f64>)> {
  let min = node.get_attribute_ref("min").and_then(super::parse_finite_number);
  let max = node.get_attribute_ref("max").and_then(super::parse_finite_number);
  if let Some(min) = min {
    if value < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max {
    if value > max {
      state.range_overflow = true;
    }
  }
  Some((min, max))
}

fn range_flags_integer(
  state: &mut ValidityState,
  value: i32,
  node: &DomNode,
  parser: fn(&str) -> Option<i32>,
) -> Option<(Option<i32>, Option<i32>)> {
  let min = node.get_attribute_ref("min").and_then(|v| {
    let trimmed = super::trim_ascii_whitespace_html(v);
    if trimmed.is_empty() {
      None
    } else {
      parser(trimmed)
    }
  });
  let max = node.get_attribute_ref("max").and_then(|v| {
    let trimmed = super::trim_ascii_whitespace_html(v);
    if trimmed.is_empty() {
      None
    } else {
      parser(trimmed)
    }
  });

  if let Some(min) = min {
    if value < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max {
    if value > max {
      state.range_overflow = true;
    }
  }
  Some((min, max))
}

fn numeric_value_missing(value: &str) -> bool {
  super::trim_ascii_whitespace_html(value).is_empty()
}

pub(crate) fn validity_state(element: &ElementRef) -> Option<ValidityState> {
  validity_state_with_disabled(element, element.is_disabled())
}

pub(crate) fn validity_state_with_disabled(element: &ElementRef, disabled: bool) -> Option<ValidityState> {
  if !element.supports_validation() {
    return None;
  }

  if disabled {
    return Some(ValidityState::valid());
  }

  let tag = element.node.tag_name()?;
  if tag.eq_ignore_ascii_case("select") {
    return Some(validity_for_select(element));
  }
  if tag.eq_ignore_ascii_case("textarea") {
    return Some(validity_for_textarea(element));
  }
  if tag.eq_ignore_ascii_case("input") {
    return Some(validity_for_input(element));
  }

  Some(ValidityState::valid())
}

fn validity_for_textarea(element: &ElementRef) -> ValidityState {
  let mut state = ValidityState::default();
  let required = element.is_required();
  let value = element.control_value().unwrap_or_default();

  if required && value_is_empty(&value) {
    state.value_missing = true;
    state.compute_validity();
    return state;
  }

  apply_length_constraints(&mut state, &value, element.node);
  state.compute_validity();
  state
}

fn validity_for_select(element: &ElementRef) -> ValidityState {
  let mut state = ValidityState::default();
  let required = element.is_required();
  if !required {
    state.compute_validity();
    return state;
  }

  let multiple = element.node.get_attribute_ref("multiple").is_some();
  let size = super::select_display_size(element.node);

  if multiple || size != 1 {
    if !super::select_has_non_disabled_selected_option(element.node) {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(selected) = super::single_select_selected_option(element.node) else {
    state.value_missing = true;
    state.compute_validity();
    return state;
  };

  if let Some(placeholder) = super::select_placeholder_label_option(element.node) {
    if std::ptr::eq(placeholder, selected) {
      state.value_missing = true;
    }
  }

  state.compute_validity();
  state
}

fn validity_for_input(element: &ElementRef) -> ValidityState {
  let mut state = ValidityState::default();

  let input_type = element.node.get_attribute_ref("type").unwrap_or("text");
  let required = element.is_required();

  if input_type.eq_ignore_ascii_case("checkbox") {
    if required && element.node.get_attribute_ref("checked").is_none() {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  if input_type.eq_ignore_ascii_case("radio") {
    if radio_group_is_missing(element) {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  if input_type.eq_ignore_ascii_case("file") {
    let value = element.node.get_attribute_ref("value").unwrap_or_default();
    if required && value.is_empty() {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  if input_type.eq_ignore_ascii_case("number") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if numeric_value_missing(raw) {
      if required {
        state.value_missing = true;
      }
      state.compute_validity();
      return state;
    }

    let trimmed = super::trim_ascii_whitespace_html(raw);
    let parsed = trimmed
      .parse::<f64>()
      .ok()
      .filter(|v| v.is_finite());
    let Some(value) = parsed else {
      state.bad_input = true;
      state.compute_validity();
      return state;
    };

    let (min, _max) = range_flags_numeric(&mut state, value, element.node).unwrap_or((None, None));

    if let Some(step_attr) = element.node.get_attribute_ref("step") {
      let step = parse_step_attribute(step_attr);
      if let Some(step) = step_value_number(step, 1.0) {
        let base = min.unwrap_or(0.0);
        if is_step_mismatch(value, step, base) {
          state.step_mismatch = true;
        }
      }
    } else {
      // Default step=1.
      let base = min.unwrap_or(0.0);
      if is_step_mismatch(value, 1.0, base) {
        state.step_mismatch = true;
      }
    }

    state.compute_validity();
    return state;
  }

  if input_type.eq_ignore_ascii_case("range") {
    // Range inputs are value-sanitized; they are never invalid for requiredness, range, or step.
    state.compute_validity();
    return state;
  }

  if input_type.eq_ignore_ascii_case("color") {
    // Color inputs have a default value and their value is sanitized to a valid simple color. They
    // are never invalid for requiredness or parsing errors in browsers.
    state.compute_validity();
    return state;
  }

  // Date/time-like inputs.
  if input_type.eq_ignore_ascii_case("date") {
    return validity_for_date_like(element, required, parse_date_value, 1.0, |v| v as i64 * 86_400_000);
  }
  if input_type.eq_ignore_ascii_case("month") {
    return validity_for_month(element, required);
  }
  if input_type.eq_ignore_ascii_case("week") {
    return validity_for_week(element, required);
  }
  if input_type.eq_ignore_ascii_case("time") {
    return validity_for_time_like(element, required);
  }
  if input_type.eq_ignore_ascii_case("datetime-local") {
    return validity_for_datetime_local(element, required);
  }

  // Text-like inputs (default to text for unknown types).
  let value = element.node.get_attribute_ref("value").unwrap_or_default();
  if required && value_is_empty(value) {
    state.value_missing = true;
    state.compute_validity();
    return state;
  }

  apply_length_constraints(&mut state, value, element.node);

  if let Some(pattern) = element.node.get_attribute_ref("pattern") {
    if pattern_mismatch(pattern, value) {
      state.pattern_mismatch = true;
    }
  }

  if input_type.eq_ignore_ascii_case("email") {
    let multiple = element.node.get_attribute_ref("multiple").is_some();
    if email_type_mismatch(value, multiple) {
      state.type_mismatch = true;
    }
  } else if input_type.eq_ignore_ascii_case("url") {
    if url_type_mismatch(value) {
      state.type_mismatch = true;
    }
  }

  state.compute_validity();
  state
}

fn validity_for_date_like(
  element: &ElementRef,
  required: bool,
  parser: fn(&str) -> Option<i32>,
  default_step: f64,
  to_ms: fn(i32) -> i64,
) -> ValidityState {
  let mut state = ValidityState::default();
  let raw = element.node.get_attribute_ref("value").unwrap_or_default();
  let trimmed = super::trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    if required {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(value) = parser(trimmed) else {
    state.bad_input = true;
    state.compute_validity();
    return state;
  };

  let (min, _max) = range_flags_integer(&mut state, value, element.node, parser).unwrap_or((None, None));

  let step_attr = element.node.get_attribute_ref("step");
  let step = step_attr.map(parse_step_attribute).unwrap_or(Step::Default);
  if let Some(step_days) = step_value_number(step, default_step) {
    let base_days = min.unwrap_or_else(|| parser("1970-01-01").unwrap_or(0));
    let diff_ms = (to_ms(value) - to_ms(base_days)) as f64;
    let step_ms = step_days * 86_400_000.0;
    if step_ms > 0.0 {
      let q = diff_ms / step_ms;
      if (q - q.round()).abs() > 1e-9 {
        state.step_mismatch = true;
      }
    }
  }

  state.compute_validity();
  state
}

fn validity_for_month(element: &ElementRef, required: bool) -> ValidityState {
  let mut state = ValidityState::default();
  let raw = element.node.get_attribute_ref("value").unwrap_or_default();
  let trimmed = super::trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    if required {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(value) = parse_month_value(trimmed) else {
    state.bad_input = true;
    state.compute_validity();
    return state;
  };

  let min = element
    .node
    .get_attribute_ref("min")
    .and_then(|v| parse_month_value(super::trim_ascii_whitespace_html(v)));
  let max = element
    .node
    .get_attribute_ref("max")
    .and_then(|v| parse_month_value(super::trim_ascii_whitespace_html(v)));

  if let Some(min) = min {
    if value < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max {
    if value > max {
      state.range_overflow = true;
    }
  }

  // Step is in months; default 1.
  let step_attr = element.node.get_attribute_ref("step");
  let step = step_attr.map(parse_step_attribute).unwrap_or(Step::Default);
  if let Some(step_months) = step_value_number(step, 1.0) {
    let base = min.unwrap_or(1970 * 12);
    let diff = (value - base) as f64;
    if step_months > 0.0 {
      let q = diff / step_months;
      if (q - q.round()).abs() > 1e-9 {
        state.step_mismatch = true;
      }
    }
  }

  state.compute_validity();
  state
}

fn validity_for_week(element: &ElementRef, required: bool) -> ValidityState {
  let mut state = ValidityState::default();
  let raw = element.node.get_attribute_ref("value").unwrap_or_default();
  let trimmed = super::trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    if required {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(value_days) = parse_week_value(trimmed) else {
    state.bad_input = true;
    state.compute_validity();
    return state;
  };

  let min_days = element
    .node
    .get_attribute_ref("min")
    .and_then(|v| parse_week_value(super::trim_ascii_whitespace_html(v)));
  let max_days = element
    .node
    .get_attribute_ref("max")
    .and_then(|v| parse_week_value(super::trim_ascii_whitespace_html(v)));

  if let Some(min) = min_days {
    if value_days < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max_days {
    if value_days > max {
      state.range_overflow = true;
    }
  }

  // Step is in weeks; default 1.
  let step_attr = element.node.get_attribute_ref("step");
  let step = step_attr.map(parse_step_attribute).unwrap_or(Step::Default);
  if let Some(step_weeks) = step_value_number(step, 1.0) {
    let base = min_days.unwrap_or_else(|| parse_week_value("1970-W01").unwrap_or(0));
    let diff_weeks = ((value_days - base) as f64) / 7.0;
    if step_weeks > 0.0 {
      let q = diff_weeks / step_weeks;
      if (q - q.round()).abs() > 1e-9 {
        state.step_mismatch = true;
      }
    }
  }

  state.compute_validity();
  state
}

fn validity_for_time_like(element: &ElementRef, required: bool) -> ValidityState {
  let mut state = ValidityState::default();
  let raw = element.node.get_attribute_ref("value").unwrap_or_default();
  let trimmed = super::trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    if required {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(value_ms) = parse_time_value(trimmed) else {
    state.bad_input = true;
    state.compute_validity();
    return state;
  };

  let min = element
    .node
    .get_attribute_ref("min")
    .and_then(|v| parse_time_value(super::trim_ascii_whitespace_html(v)));
  let max = element
    .node
    .get_attribute_ref("max")
    .and_then(|v| parse_time_value(super::trim_ascii_whitespace_html(v)));

  if let Some(min) = min {
    if value_ms < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max {
    if value_ms > max {
      state.range_overflow = true;
    }
  }

  // Step is in seconds; default 60.
  let step_attr = element.node.get_attribute_ref("step");
  let step = step_attr.map(parse_step_attribute).unwrap_or(Step::Default);
  if let Some(step_seconds) = step_value_number(step, 60.0) {
    let base = min.unwrap_or(0);
    let diff_ms = (value_ms - base) as f64;
    let step_ms = step_seconds * 1000.0;
    if step_ms > 0.0 {
      let q = diff_ms / step_ms;
      if (q - q.round()).abs() > 1e-9 {
        state.step_mismatch = true;
      }
    }
  }

  state.compute_validity();
  state
}

fn validity_for_datetime_local(element: &ElementRef, required: bool) -> ValidityState {
  let mut state = ValidityState::default();
  let raw = element.node.get_attribute_ref("value").unwrap_or_default();
  let trimmed = super::trim_ascii_whitespace_html(raw);
  if trimmed.is_empty() {
    if required {
      state.value_missing = true;
    }
    state.compute_validity();
    return state;
  }

  let Some(value_ms) = parse_datetime_local_value(trimmed) else {
    state.bad_input = true;
    state.compute_validity();
    return state;
  };

  let min = element
    .node
    .get_attribute_ref("min")
    .and_then(|v| parse_datetime_local_value(super::trim_ascii_whitespace_html(v)));
  let max = element
    .node
    .get_attribute_ref("max")
    .and_then(|v| parse_datetime_local_value(super::trim_ascii_whitespace_html(v)));

  if let Some(min) = min {
    if value_ms < min {
      state.range_underflow = true;
    }
  }
  if let Some(max) = max {
    if value_ms > max {
      state.range_overflow = true;
    }
  }

  // Step is in seconds; default 60.
  let step_attr = element.node.get_attribute_ref("step");
  let step = step_attr.map(parse_step_attribute).unwrap_or(Step::Default);
  if let Some(step_seconds) = step_value_number(step, 60.0) {
    // Values are compared in milliseconds; our internal representation uses days-from-CE, so align
    // the implicit step base with 1970-01-01T00:00.
    let base = min.unwrap_or_else(|| parse_datetime_local_value("1970-01-01T00:00").unwrap_or(0));
    let diff_ms = (value_ms - base) as f64;
    let step_ms = step_seconds * 1000.0;
    if step_ms > 0.0 {
      let q = diff_ms / step_ms;
      if (q - q.round()).abs() > 1e-9 {
        state.step_mismatch = true;
      }
    }
  }

  state.compute_validity();
  state
}

pub(crate) fn range_state(element: &ElementRef) -> Option<bool> {
  let tag = element.node.tag_name()?;
  if !tag.eq_ignore_ascii_case("input") {
    return None;
  }
  let input_type = element.node.get_attribute_ref("type").unwrap_or("text");

  if input_type.eq_ignore_ascii_case("range") {
    let (min, max) = super::input_range_bounds(element.node)?;
    let value = super::input_range_value(element.node)?;
    return Some(value >= min && value <= max);
  }

  if input_type.eq_ignore_ascii_case("number") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if numeric_value_missing(raw) {
      return None;
    }
    let trimmed = super::trim_ascii_whitespace_html(raw);
    let parsed = trimmed
      .parse::<f64>()
      .ok()
      .filter(|v| v.is_finite())?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(super::parse_finite_number);
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(super::parse_finite_number);
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if parsed < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if parsed > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  if input_type.eq_ignore_ascii_case("date") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if super::trim_ascii_whitespace_html(raw).is_empty() {
      return None;
    }
    let value = parse_date_value(super::trim_ascii_whitespace_html(raw))?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(|v| parse_date_value(super::trim_ascii_whitespace_html(v)));
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(|v| parse_date_value(super::trim_ascii_whitespace_html(v)));
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  if input_type.eq_ignore_ascii_case("datetime-local") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if super::trim_ascii_whitespace_html(raw).is_empty() {
      return None;
    }
    let value = parse_datetime_local_value(super::trim_ascii_whitespace_html(raw))?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(|v| parse_datetime_local_value(super::trim_ascii_whitespace_html(v)));
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(|v| parse_datetime_local_value(super::trim_ascii_whitespace_html(v)));
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  if input_type.eq_ignore_ascii_case("month") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if super::trim_ascii_whitespace_html(raw).is_empty() {
      return None;
    }
    let value = parse_month_value(super::trim_ascii_whitespace_html(raw))?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(|v| parse_month_value(super::trim_ascii_whitespace_html(v)));
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(|v| parse_month_value(super::trim_ascii_whitespace_html(v)));
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  if input_type.eq_ignore_ascii_case("week") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if super::trim_ascii_whitespace_html(raw).is_empty() {
      return None;
    }
    let value = parse_week_value(super::trim_ascii_whitespace_html(raw))?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(|v| parse_week_value(super::trim_ascii_whitespace_html(v)));
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(|v| parse_week_value(super::trim_ascii_whitespace_html(v)));
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  if input_type.eq_ignore_ascii_case("time") {
    let raw = element.node.get_attribute_ref("value").unwrap_or_default();
    if super::trim_ascii_whitespace_html(raw).is_empty() {
      return None;
    }
    let value = parse_time_value(super::trim_ascii_whitespace_html(raw))?;
    let min = element
      .node
      .get_attribute_ref("min")
      .and_then(|v| parse_time_value(super::trim_ascii_whitespace_html(v)));
    let max = element
      .node
      .get_attribute_ref("max")
      .and_then(|v| parse_time_value(super::trim_ascii_whitespace_html(v)));
    if min.is_none() && max.is_none() {
      return None;
    }
    if let Some(min) = min {
      if value < min {
        return Some(false);
      }
    }
    if let Some(max) = max {
      if value > max {
        return Some(false);
      }
    }
    return Some(true);
  }

  None
}

pub(crate) fn radio_group_is_missing(element: &ElementRef) -> bool {
  let name = element.node.get_attribute_ref("name").unwrap_or("");
  if name.is_empty() {
    return element.node.get_attribute_ref("required").is_some()
      && element.node.get_attribute_ref("checked").is_none();
  }

  let (tree_root, ancestors_in_tree) = element.tree_root_info();
  let forms_by_id = ElementRef::collect_forms_by_id(tree_root);

  let self_nearest_form = ancestors_in_tree.iter().rev().copied().find(|node| {
    node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
  });
  let self_form_owner =
    ElementRef::resolve_form_owner_for_node(element.node, self_nearest_form, &forms_by_id)
      .map(|form| form as *const DomNode);

  let mut group_required = false;
  let mut stack: Vec<(&DomNode, Option<&DomNode>)> = vec![(tree_root, None)];

  while let Some((node, nearest_form)) = stack.pop() {
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) && !ptr::eq(node, tree_root) {
      continue;
    }

    let mut nearest_form = nearest_form;
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
    {
      nearest_form = Some(node);
    }

    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      && node
        .get_attribute_ref("type")
        .unwrap_or("text")
        .eq_ignore_ascii_case("radio")
      && node.get_attribute_ref("name") == Some(name)
    {
      let owner =
        ElementRef::resolve_form_owner_for_node(node, nearest_form, &forms_by_id).map(|form| {
          form as *const DomNode
        });
      if owner == self_form_owner {
        if node.get_attribute_ref("checked").is_some() {
          return false;
        }
        if node.get_attribute_ref("required").is_some() {
          group_required = true;
        }
      }
    }

    for child in node.traversal_children().iter().rev() {
      stack.push((child, nearest_form));
    }
  }

  group_required
}
