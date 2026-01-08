use crate::layout::axis::PhysicalAxis;
use crate::layout::formatting_context::FormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::layout::formatting_context::LayoutError;
use crate::tree::box_tree::BoxNode;

#[inline]
fn physical_axis_is_inline_axis(
  writing_mode: crate::style::types::WritingMode,
  axis: PhysicalAxis,
) -> bool {
  let inline_is_horizontal = crate::style::inline_axis_is_horizontal(writing_mode);
  match axis {
    PhysicalAxis::X => inline_is_horizontal,
    PhysicalAxis::Y => !inline_is_horizontal,
  }
}

/// Returns the intrinsic *border-box* size for `box_node` along `physical_axis`.
///
/// The `FormattingContext` intrinsic APIs are expressed in the box’s logical axes (inline vs block),
/// so this helper maps those to the requested physical axis using `writing-mode`.
pub(crate) fn physical_axis_intrinsic_border_box_size<FC: FormattingContext + ?Sized>(
  fc: &FC,
  box_node: &BoxNode,
  physical_axis: PhysicalAxis,
  mode: IntrinsicSizingMode,
) -> Result<f32, LayoutError> {
  if physical_axis_is_inline_axis(box_node.style.writing_mode, physical_axis) {
    fc.compute_intrinsic_inline_size(box_node, mode)
  } else {
    fc.compute_intrinsic_block_size(box_node, mode)
  }
}

/// Returns the intrinsic *border-box* min- and max-content sizes for `box_node` along `physical_axis`.
///
/// When the physical axis maps to the inline axis, this calls
/// [`FormattingContext::compute_intrinsic_inline_sizes`] so implementations can share work between
/// the two measurements.
pub(crate) fn physical_axis_intrinsic_border_box_sizes<FC: FormattingContext + ?Sized>(
  fc: &FC,
  box_node: &BoxNode,
  physical_axis: PhysicalAxis,
) -> Result<(f32, f32), LayoutError> {
  if physical_axis_is_inline_axis(box_node.style.writing_mode, physical_axis) {
    return fc.compute_intrinsic_inline_sizes(box_node);
  }

  let min = fc.compute_intrinsic_block_size(box_node, IntrinsicSizingMode::MinContent)?;
  let max = match fc.compute_intrinsic_block_size(box_node, IntrinsicSizingMode::MaxContent) {
    Ok(value) => value,
    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
    Err(_) => min,
  };
  Ok((min, max))
}

/// Resolves a `fit-content` sizing keyword/function to a concrete border-box size.
///
/// This implements the common clamp used by:
/// - `fit-content` keyword (uses the available size, when definite)
/// - `fit-content(<length-percentage>)` function (uses the provided preferred size)
///
/// The returned value is:
///
/// ```text
/// min(max, max(min, target))
/// where target = preferred.or(available).unwrap_or(max)
/// ```
pub(crate) fn resolve_fit_content_border_box(
  available: Option<f32>,
  preferred: Option<f32>,
  min: f32,
  max: f32,
) -> f32 {
  let target = preferred.or(available).unwrap_or(max);
  crate::layout::utils::clamp_with_order(target, min, max)
}

/// Runs `f` with a style override installed that clears the authored size on `physical_axis`.
///
/// This can be used by intrinsic sizing probes to avoid self-recursion when the element’s own
/// authored `width`/`height` is an intrinsic sizing keyword (min-content/max-content/fit-content).
pub(crate) fn with_size_axis_cleared_style_override<R>(
  box_id: usize,
  style: &std::sync::Arc<crate::style::ComputedStyle>,
  physical_axis: PhysicalAxis,
  f: impl FnOnce() -> R,
) -> R {
  // `ComputedStyle` is large; keep the modified override behind an `Arc` to avoid large by-value
  // clones on the stack in intrinsic sizing paths.
  let mut override_style = style.clone();
  {
    let s = std::sync::Arc::make_mut(&mut override_style);
    match physical_axis {
      PhysicalAxis::X => {
        s.width = None;
        s.width_keyword = None;
      }
      PhysicalAxis::Y => {
        s.height = None;
        s.height_keyword = None;
      }
    }
  }
  crate::layout::style_override::with_style_override(box_id, override_style, f)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::layout::constraints::LayoutConstraints;
  use crate::style::display::FormattingContextType;
  use crate::style::types::IntrinsicSizeKeyword;
  use crate::style::types::WritingMode;
  use crate::tree::fragment_tree::FragmentNode;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;

  struct StubFormattingContext {
    inline_size_calls: AtomicUsize,
    inline_sizes_calls: AtomicUsize,
    block_size_calls: AtomicUsize,
  }

  impl StubFormattingContext {
    fn new() -> Self {
      Self {
        inline_size_calls: AtomicUsize::new(0),
        inline_sizes_calls: AtomicUsize::new(0),
        block_size_calls: AtomicUsize::new(0),
      }
    }

    fn inline_min(&self) -> f32 {
      10.0
    }
    fn inline_max(&self) -> f32 {
      20.0
    }
    fn block_min(&self) -> f32 {
      30.0
    }
    fn block_max(&self) -> f32 {
      40.0
    }
  }

  impl FormattingContext for StubFormattingContext {
    fn layout(
      &self,
      _box_node: &BoxNode,
      _constraints: &LayoutConstraints,
    ) -> Result<FragmentNode, LayoutError> {
      unreachable!("layout should not be called by intrinsic sizing keyword tests");
    }

    fn compute_intrinsic_inline_size(
      &self,
      _box_node: &BoxNode,
      mode: IntrinsicSizingMode,
    ) -> Result<f32, LayoutError> {
      self.inline_size_calls.fetch_add(1, Ordering::Relaxed);
      Ok(match mode {
        IntrinsicSizingMode::MinContent => self.inline_min(),
        IntrinsicSizingMode::MaxContent => self.inline_max(),
      })
    }

    fn compute_intrinsic_inline_sizes(
      &self,
      _box_node: &BoxNode,
    ) -> Result<(f32, f32), LayoutError> {
      self.inline_sizes_calls.fetch_add(1, Ordering::Relaxed);
      Ok((self.inline_min(), self.inline_max()))
    }

    fn compute_intrinsic_block_size(
      &self,
      _box_node: &BoxNode,
      mode: IntrinsicSizingMode,
    ) -> Result<f32, LayoutError> {
      self.block_size_calls.fetch_add(1, Ordering::Relaxed);
      Ok(match mode {
        IntrinsicSizingMode::MinContent => self.block_min(),
        IntrinsicSizingMode::MaxContent => self.block_max(),
      })
    }
  }

  fn box_node_with_writing_mode(writing_mode: WritingMode) -> BoxNode {
    let mut style = crate::style::ComputedStyle::default();
    style.writing_mode = writing_mode;
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, Vec::new())
  }

  #[test]
  fn maps_physical_axes_in_horizontal_writing_mode() {
    let fc = StubFormattingContext::new();
    let node = box_node_with_writing_mode(WritingMode::HorizontalTb);

    assert_eq!(
      physical_axis_intrinsic_border_box_size(
        &fc,
        &node,
        PhysicalAxis::X,
        IntrinsicSizingMode::MinContent
      )
      .unwrap(),
      fc.inline_min()
    );
    assert_eq!(
      physical_axis_intrinsic_border_box_size(
        &fc,
        &node,
        PhysicalAxis::Y,
        IntrinsicSizingMode::MaxContent
      )
      .unwrap(),
      fc.block_max()
    );

    let inline_size_calls_before = fc.inline_size_calls.load(Ordering::Relaxed);
    let inline_sizes_calls_before = fc.inline_sizes_calls.load(Ordering::Relaxed);
    let block_size_calls_before = fc.block_size_calls.load(Ordering::Relaxed);
    let sizes =
      physical_axis_intrinsic_border_box_sizes(&fc, &node, PhysicalAxis::X).expect("sizes");
    assert_eq!(sizes, (fc.inline_min(), fc.inline_max()));
    assert_eq!(
      fc.inline_sizes_calls.load(Ordering::Relaxed),
      inline_sizes_calls_before + 1
    );
    assert_eq!(
      fc.inline_size_calls.load(Ordering::Relaxed),
      inline_size_calls_before
    );
    assert_eq!(
      fc.block_size_calls.load(Ordering::Relaxed),
      block_size_calls_before
    );

    let block_size_calls_before = fc.block_size_calls.load(Ordering::Relaxed);
    let sizes =
      physical_axis_intrinsic_border_box_sizes(&fc, &node, PhysicalAxis::Y).expect("sizes");
    assert_eq!(sizes, (fc.block_min(), fc.block_max()));
    assert_eq!(
      fc.block_size_calls.load(Ordering::Relaxed),
      block_size_calls_before + 2
    );
  }

  #[test]
  fn maps_physical_axes_in_vertical_writing_mode() {
    let fc = StubFormattingContext::new();
    let node = box_node_with_writing_mode(WritingMode::VerticalRl);

    assert_eq!(
      physical_axis_intrinsic_border_box_size(
        &fc,
        &node,
        PhysicalAxis::X,
        IntrinsicSizingMode::MinContent
      )
      .unwrap(),
      fc.block_min()
    );
    assert_eq!(
      physical_axis_intrinsic_border_box_size(
        &fc,
        &node,
        PhysicalAxis::Y,
        IntrinsicSizingMode::MaxContent
      )
      .unwrap(),
      fc.inline_max()
    );

    let block_size_calls_before = fc.block_size_calls.load(Ordering::Relaxed);
    let sizes =
      physical_axis_intrinsic_border_box_sizes(&fc, &node, PhysicalAxis::X).expect("sizes");
    assert_eq!(sizes, (fc.block_min(), fc.block_max()));
    assert_eq!(
      fc.block_size_calls.load(Ordering::Relaxed),
      block_size_calls_before + 2
    );

    let inline_sizes_calls_before = fc.inline_sizes_calls.load(Ordering::Relaxed);
    let sizes =
      physical_axis_intrinsic_border_box_sizes(&fc, &node, PhysicalAxis::Y).expect("sizes");
    assert_eq!(sizes, (fc.inline_min(), fc.inline_max()));
    assert_eq!(
      fc.inline_sizes_calls.load(Ordering::Relaxed),
      inline_sizes_calls_before + 1
    );
  }

  #[test]
  fn fit_content_clamps() {
    assert_eq!(
      resolve_fit_content_border_box(Some(100.0), None, 50.0, 200.0),
      100.0
    );
    assert_eq!(
      resolve_fit_content_border_box(Some(10.0), None, 50.0, 200.0),
      50.0
    );
    assert_eq!(
      resolve_fit_content_border_box(Some(300.0), None, 50.0, 200.0),
      200.0
    );
    assert_eq!(
      resolve_fit_content_border_box(None, Some(120.0), 50.0, 200.0),
      120.0
    );
    assert_eq!(
      resolve_fit_content_border_box(Some(100.0), Some(120.0), 50.0, 200.0),
      120.0
    );
    assert_eq!(
      resolve_fit_content_border_box(None, None, 50.0, 200.0),
      200.0
    );
  }

  #[test]
  fn clears_size_axis_via_style_override() {
    let fc = StubFormattingContext::new();
    let mut style = crate::style::ComputedStyle::default();
    style.width = Some(crate::style::values::Length::px(10.0));
    style.height = Some(crate::style::values::Length::px(20.0));
    style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let style = Arc::new(style);

    let node = BoxNode::new_block(style.clone(), FormattingContextType::Block, Vec::new());

    // Clearing width should not affect which intrinsic API is called, but should install a style override
    // so callers can observe `width == None` if needed.
    let result = with_size_axis_cleared_style_override(node.id(), &style, PhysicalAxis::X, || {
      crate::layout::style_override::style_override_for(node.id())
        .map(|s| s.width.is_none() && s.width_keyword.is_none())
    });
    assert_eq!(result, Some(true));

    // Ensure the helper itself is a no-op for intrinsic sizing calls.
    assert_eq!(
      physical_axis_intrinsic_border_box_size(
        &fc,
        &node,
        PhysicalAxis::X,
        IntrinsicSizingMode::MinContent
      )
      .unwrap(),
      fc.inline_min()
    );
  }

  // Silence unused import warnings for `Rect` in this module (kept for parity with other FC tests).
  #[allow(dead_code)]
  fn _dummy_rect() -> Rect {
    Rect::from_xywh(0.0, 0.0, 0.0, 0.0)
  }
}
