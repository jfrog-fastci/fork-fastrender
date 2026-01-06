use crate::style::types::IntrinsicSizeKeyword;
use crate::style::values::{CalcLength, Length};
use crate::style::ComputedStyle;
use rustc_hash::FxHasher;
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

struct StyleOverrideEntry {
  node_id: usize,
  style: Arc<ComputedStyle>,
  fingerprint: Cell<Option<u64>>,
}

thread_local! {
  /// Stack of temporary `ComputedStyle` overrides keyed by `BoxNode` id.
  ///
  /// This is used in hot flex/grid measurement paths where we need to measure the same box with
  /// different effective sizing hints (e.g. treating percentage widths as `auto` during
  /// min-/max-content probes, or clearing an authored `width` when computing content-based
  /// minimum sizes for flexbox).
  ///
  /// Storing overrides in TLS avoids allocating/cloning entire `BoxNode` subtrees just to swap a
  /// style pointer, while still keeping the overrides scoped to the current thread and call stack.
  static STYLE_OVERRIDES: RefCell<Vec<StyleOverrideEntry>> = RefCell::new(Vec::new());
}

pub(crate) struct StyleOverrideGuard {
  node_id: usize,
}

impl Drop for StyleOverrideGuard {
  fn drop(&mut self) {
    STYLE_OVERRIDES.with(|stack| {
      let mut stack = stack.borrow_mut();
      let popped = stack.pop();
      debug_assert!(
        popped
          .as_ref()
          .is_some_and(|entry| entry.node_id == self.node_id),
        "style override stack corrupted (expected node_id={})",
        self.node_id
      );
    });
  }
}

/// Installs a temporary style override for the duration of the returned guard.
pub(crate) fn push_style_override(node_id: usize, style: Arc<ComputedStyle>) -> StyleOverrideGuard {
  STYLE_OVERRIDES.with(|stack| {
    stack.borrow_mut().push(StyleOverrideEntry {
      node_id,
      style,
      fingerprint: Cell::new(None),
    })
  });
  StyleOverrideGuard { node_id }
}

/// Runs `f` with a temporary style override installed.
pub(crate) fn with_style_override<R>(
  node_id: usize,
  style: Arc<ComputedStyle>,
  f: impl FnOnce() -> R,
) -> R {
  let _guard = push_style_override(node_id, style);
  f()
}

/// Returns the currently active override style for `node_id`, if any.
pub(crate) fn style_override_for(node_id: usize) -> Option<Arc<ComputedStyle>> {
  STYLE_OVERRIDES.with(|stack| {
    stack
      .borrow()
      .iter()
      .rev()
      .find_map(|entry| (entry.node_id == node_id).then(|| entry.style.clone()))
  })
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

fn hash_enum_discriminant<T>(value: &T, hasher: &mut FxHasher) {
  std::mem::discriminant(value).hash(hasher);
}

fn hash_calc_length(calc: &CalcLength, hasher: &mut FxHasher) {
  let terms = calc.terms();
  (terms.len() as u8).hash(hasher);
  for term in terms {
    term.unit.hash(hasher);
    f32_to_canonical_bits(term.value).hash(hasher);
  }
}

fn hash_length(len: &Length, hasher: &mut FxHasher) {
  len.unit.hash(hasher);
  f32_to_canonical_bits(len.value).hash(hasher);
  match &len.calc {
    Some(calc) => {
      1u8.hash(hasher);
      hash_calc_length(calc, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_option_length(len: &Option<Length>, hasher: &mut FxHasher) {
  match len {
    Some(len) => {
      1u8.hash(hasher);
      hash_length(len, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_intrinsic_size_keyword(keyword: &IntrinsicSizeKeyword, hasher: &mut FxHasher) {
  hash_enum_discriminant(keyword, hasher);
  if let IntrinsicSizeKeyword::FitContent { limit } = keyword {
    hash_option_length(limit, hasher);
  }
}

fn hash_option_intrinsic_size_keyword(opt: &Option<IntrinsicSizeKeyword>, hasher: &mut FxHasher) {
  match opt {
    Some(keyword) => {
      1u8.hash(hasher);
      hash_intrinsic_size_keyword(keyword, hasher);
    }
    None => 0u8.hash(hasher),
  }
}

fn hash_sizing_property(
  length: &Option<Length>,
  keyword: &Option<IntrinsicSizeKeyword>,
  hasher: &mut FxHasher,
) {
  hash_option_length(length, hasher);
  hash_option_intrinsic_size_keyword(keyword, hasher);
}

fn style_override_fingerprint(style: &ComputedStyle) -> u64 {
  let mut h = FxHasher::default();
  hash_enum_discriminant(&style.display, &mut h);
  hash_enum_discriminant(&style.position, &mut h);
  hash_option_length(&style.top, &mut h);
  hash_option_length(&style.right, &mut h);
  hash_option_length(&style.bottom, &mut h);
  hash_option_length(&style.left, &mut h);
  hash_enum_discriminant(&style.float, &mut h);
  hash_enum_discriminant(&style.clear, &mut h);
  hash_enum_discriminant(&style.box_sizing, &mut h);
  hash_enum_discriminant(&style.writing_mode, &mut h);
  hash_enum_discriminant(&style.direction, &mut h);
  hash_sizing_property(&style.width, &style.width_keyword, &mut h);
  hash_sizing_property(&style.height, &style.height_keyword, &mut h);
  hash_sizing_property(&style.min_width, &style.min_width_keyword, &mut h);
  hash_sizing_property(&style.max_width, &style.max_width_keyword, &mut h);
  hash_sizing_property(&style.min_height, &style.min_height_keyword, &mut h);
  hash_sizing_property(&style.max_height, &style.max_height_keyword, &mut h);
  hash_option_length(&style.margin_top, &mut h);
  hash_option_length(&style.margin_right, &mut h);
  hash_option_length(&style.margin_bottom, &mut h);
  hash_option_length(&style.margin_left, &mut h);
  hash_length(&style.padding_top, &mut h);
  hash_length(&style.padding_right, &mut h);
  hash_length(&style.padding_bottom, &mut h);
  hash_length(&style.padding_left, &mut h);
  hash_length(&style.used_border_top_width(), &mut h);
  hash_length(&style.used_border_right_width(), &mut h);
  hash_length(&style.used_border_bottom_width(), &mut h);
  hash_length(&style.used_border_left_width(), &mut h);
  hash_enum_discriminant(&style.overflow_x, &mut h);
  hash_enum_discriminant(&style.overflow_y, &mut h);
  hash_enum_discriminant(&style.scrollbar_width, &mut h);
  hash_enum_discriminant(&style.line_height, &mut h);
  style.font_size.to_bits().hash(&mut h);
  style.root_font_size.to_bits().hash(&mut h);
  h.finish()
}

/// Returns the fingerprint for the currently active style override for `node_id`, if any.
///
/// Fingerprints are computed lazily to avoid doing any hashing when the override is only used to
/// adjust layout (and not intrinsic sizing). Intrinsic sizing caches can use the fingerprint as an
/// override-aware cache key component without re-hashing the full style on every lookup.
pub(crate) fn style_override_fingerprint_for(node_id: usize) -> Option<u64> {
  STYLE_OVERRIDES.with(|stack| {
    stack.borrow().iter().rev().find_map(|entry| {
      if entry.node_id != node_id {
        return None;
      }
      let fingerprint = entry.fingerprint.get().unwrap_or_else(|| {
        let fingerprint = style_override_fingerprint(entry.style.as_ref());
        entry.fingerprint.set(Some(fingerprint));
        fingerprint
      });
      Some(fingerprint)
    })
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn style_override_fingerprint_includes_intrinsic_size_keywords() {
    let base = ComputedStyle::default();

    let mut max_content = base.clone();
    max_content.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    assert_ne!(
      style_override_fingerprint(&base),
      style_override_fingerprint(&max_content)
    );

    let mut fit_content = base.clone();
    fit_content.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let mut fit_content_fn = base.clone();
    fit_content_fn.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::percent(50.0)),
    });
    assert_ne!(
      style_override_fingerprint(&fit_content),
      style_override_fingerprint(&fit_content_fn)
    );

    let mut max_width_none = base.clone();
    max_width_none.max_width = None;
    max_width_none.max_width_keyword = None;
    let mut max_width_max_content = base.clone();
    max_width_max_content.max_width = None;
    max_width_max_content.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    assert_ne!(
      style_override_fingerprint(&max_width_none),
      style_override_fingerprint(&max_width_max_content)
    );
  }

  #[test]
  fn style_override_fingerprint_includes_calc_lengths() {
    use crate::style::values::LengthUnit;

    let base = ComputedStyle::default();

    let percent = CalcLength::single(LengthUnit::Percent, 10.0);
    let calc_a = CalcLength::single(LengthUnit::Px, 10.0)
      .add_scaled(&percent, 1.0)
      .expect("calc terms");
    let calc_b = CalcLength::single(LengthUnit::Px, 20.0)
      .add_scaled(&percent, 1.0)
      .expect("calc terms");

    let mut style_a = base.clone();
    style_a.width = Some(Length::calc(calc_a));
    let mut style_b = base.clone();
    style_b.width = Some(Length::calc(calc_b));
    assert_ne!(
      style_override_fingerprint(&style_a),
      style_override_fingerprint(&style_b)
    );

    let mut fit_content_a = base.clone();
    fit_content_a.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::calc(calc_a)),
    });
    let mut fit_content_b = base;
    fit_content_b.width_keyword = Some(IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::calc(calc_b)),
    });
    assert_ne!(
      style_override_fingerprint(&fit_content_a),
      style_override_fingerprint(&fit_content_b)
    );
  }
}
