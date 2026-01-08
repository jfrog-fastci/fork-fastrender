//! Block Formatting Context Layout
//!
//! This module implements the Block Formatting Context (BFC) layout algorithm
//! as specified in CSS 2.1 Section 9.4.1.
//!
//! # Block Formatting Context
//!
//! A BFC is a layout mode where block boxes are laid out vertically, one after
//! another, starting at the top of the containing block. The vertical distance
//! between boxes is determined by margins (which may collapse).
//!
//! # Key Features
//!
//! - **Vertical stacking**: Block boxes stack vertically
//! - **Full width**: By default, blocks stretch to fill containing block width
//! - **Margin collapsing**: Adjacent vertical margins collapse into one
//! - **Independent context**: Contents don't affect outside layout
//!
//! # Module Structure
//!
//! - `margin_collapse` - Margin collapsing algorithm (CSS 2.1 Section 8.3.1)
//! - `width` - Block width computation (CSS 2.1 Section 10.3.3)
//!
//! Reference: <https://www.w3.org/TR/CSS21/visuren.html#block-formatting>

pub mod margin_collapse;
pub mod width;

use crate::error::{RenderError, RenderStage};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::layout::axis::FragmentAxes;
use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::width::MarginValue;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::contexts::inline::InlineFormattingContext;
use crate::layout::contexts::positioned::ContainingBlock;
use crate::layout::contexts::positioned::PositionedLayout;
use crate::layout::engine::LayoutParallelism;
use crate::layout::float_context::FloatContext;
use crate::layout::float_context::FloatSide;
use crate::layout::float_shape::build_float_shape;
use crate::layout::formatting_context::count_block_intrinsic_call;
use crate::layout::formatting_context::intrinsic_cache_lookup;
use crate::layout::formatting_context::intrinsic_cache_store;
use crate::layout::formatting_context::layout_cache_lookup;
use crate::layout::formatting_context::layout_cache_store;
use crate::layout::formatting_context::remembered_size_cache_lookup;
use crate::layout::formatting_context::remembered_size_cache_store;
use crate::layout::formatting_context::FormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::layout::formatting_context::LayoutError;
use crate::layout::fragmentation::{
  clip_node, normalize_fragment_margins, propagate_fragment_metadata, FragmentAxis,
  FragmentationAnalyzer, FragmentationContext,
};
use crate::layout::profile::layout_timer;
use crate::layout::profile::LayoutKind;
use crate::layout::utils::border_size_from_box_sizing;
use crate::layout::utils::compute_replaced_size;
use crate::layout::utils::content_size_from_box_sizing;
use crate::layout::utils::resolve_length_with_percentage;
use crate::layout::utils::resolve_length_with_percentage_metrics;
use crate::layout::utils::resolve_scrollbar_width;
use crate::render_control::{
  active_deadline, active_stage, check_active, check_active_periodic, with_deadline, StageGuard,
};
use crate::style::block_axis_is_horizontal;
use crate::style::block_axis_positive;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::float::Clear;
use crate::style::float::Float;
use crate::style::inline_axis_is_horizontal;
use crate::style::inline_axis_positive;
use crate::style::position::Position;
use crate::style::types::BorderStyle;
use crate::style::types::ColumnFill;
use crate::style::types::ColumnSpan;
use crate::style::types::Direction;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::Overflow;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::style::PhysicalSide;
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::ReplacedBox;
use crate::tree::fragment_tree::BlockFragmentMetadata;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentationInfo;
use margin_collapse::establishes_bfc;
use margin_collapse::is_margin_collapsible_through;
use margin_collapse::should_collapse_with_first_child;
use margin_collapse::should_collapse_with_last_child;
use margin_collapse::CollapsibleMargin;
use margin_collapse::MarginCollapseContext;
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use width::compute_block_width;

#[derive(Clone)]
struct PositionedCandidate {
  node: BoxNode,
  source: ContainingBlockSource,
  static_position: Option<Point>,
  query_parent_id: usize,
}

#[derive(Clone)]
enum ContainingBlockSource {
  ParentPadding,
  Explicit(ContainingBlock),
}

#[derive(Clone, Copy, Debug, Default)]
struct CollapsedBlockMargins {
  top: CollapsibleMargin,
  bottom: CollapsibleMargin,
  /// True when the box is empty for margin collapsing and its own block-start/block-end margins
  /// collapse together (CSS 2.1 §8.3.1).
  collapsible_through: bool,
}

fn axis_sides(horizontal: bool, positive: bool) -> (PhysicalSide, PhysicalSide) {
  match (horizontal, positive) {
    (true, true) => (PhysicalSide::Left, PhysicalSide::Right),
    (true, false) => (PhysicalSide::Right, PhysicalSide::Left),
    (false, true) => (PhysicalSide::Top, PhysicalSide::Bottom),
    (false, false) => (PhysicalSide::Bottom, PhysicalSide::Top),
  }
}

fn inline_axis_sides(style: &ComputedStyle) -> (PhysicalSide, PhysicalSide) {
  if inline_axis_is_horizontal(style.writing_mode) {
    (PhysicalSide::Left, PhysicalSide::Right)
  } else {
    (PhysicalSide::Top, PhysicalSide::Bottom)
  }
}

fn block_axis_sides(style: &ComputedStyle) -> (PhysicalSide, PhysicalSide) {
  axis_sides(
    block_axis_is_horizontal(style.writing_mode),
    block_axis_positive(style.writing_mode),
  )
}

fn paint_viewport_for(
  writing_mode: WritingMode,
  _direction: Direction,
  viewport_size: Size,
) -> Rect {
  let inline_size = if inline_axis_is_horizontal(writing_mode) {
    viewport_size.width
  } else {
    viewport_size.height
  };
  let block_size = if inline_axis_is_horizontal(writing_mode) {
    viewport_size.height
  } else {
    viewport_size.width
  };
  Rect::from_xywh(0.0, 0.0, inline_size, block_size)
}

/// Block Formatting Context implementation
///
/// Implements the FormattingContext trait for block-level layout.
/// Handles vertical stacking, margin collapsing, and width computation.
#[derive(Clone)]
pub struct BlockFormattingContext {
  /// Shared factory used to create child formatting contexts without losing shared caches.
  factory: FormattingContextFactory,
  /// Shared inline formatting context used for intrinsic sizing (and for inline child layout when
  /// the nearest positioned containing block matches this block context's).
  ///
  /// This avoids rebuilding inline contexts (and their hyphenator/pipeline wiring) in hot loops.
  intrinsic_inline_fc: Arc<InlineFormattingContext>,
  font_context: FontContext,
  viewport_size: crate::geometry::Size,
  viewport_scroll: Point,
  nearest_positioned_cb: ContainingBlock,
  nearest_fixed_cb: ContainingBlock,
  /// When true, treat the root box as a flex item for width resolution (auto margins resolve to
  /// 0 and specified margins stay fixed instead of being rebalanced to satisfy the block width
  /// equation). This is only meant for the flex-item root; descendants revert to normal block
  /// behavior.
  flex_item_mode: bool,
  parallelism: LayoutParallelism,
}

impl BlockFormattingContext {
  /// Creates a new BlockFormattingContext
  pub fn new() -> Self {
    let viewport = crate::geometry::Size::new(800.0, 600.0);
    Self::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    )
  }

  /// Creates a BlockFormattingContext backed by a specific font context so text
  /// measurement shares caches with the caller.
  pub fn with_font_context(font_context: FontContext) -> Self {
    let viewport = crate::geometry::Size::new(800.0, 600.0);
    Self::with_font_context_viewport_and_cb(
      font_context,
      viewport,
      ContainingBlock::viewport(viewport),
    )
  }

  pub fn with_font_context_and_viewport(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
  ) -> Self {
    let cb = ContainingBlock::viewport(viewport_size);
    Self::with_font_context_viewport_and_cb(font_context, viewport_size, cb)
  }

  pub fn with_font_context_viewport_and_cb(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: ContainingBlock,
  ) -> Self {
    let factory =
      FormattingContextFactory::with_font_context_and_viewport(font_context, viewport_size)
        .with_positioned_cb(nearest_positioned_cb);
    Self::with_factory(factory)
  }

  pub fn with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let viewport_scroll = factory.viewport_scroll();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(factory.clone()));
    Self {
      factory,
      intrinsic_inline_fc,
      font_context,
      viewport_size,
      viewport_scroll,
      nearest_positioned_cb,
      nearest_fixed_cb,
      flex_item_mode: false,
      parallelism,
    }
  }

  /// Creates a BlockFormattingContext configured for laying out a flex item root. Margin
  /// resolution follows the flexbox hypothetical size rules (auto margins → 0; specified margins
  /// remain as authored).
  pub fn for_flex_item_with_font_context_viewport_and_cb(
    font_context: FontContext,
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: ContainingBlock,
  ) -> Self {
    let factory =
      FormattingContextFactory::with_font_context_and_viewport(font_context, viewport_size)
        .with_positioned_cb(nearest_positioned_cb);
    Self::for_flex_item_with_factory(factory)
  }

  pub fn for_flex_item_with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let viewport_scroll = factory.viewport_scroll();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let intrinsic_inline_fc = Arc::new(InlineFormattingContext::with_factory(factory.clone()));
    Self {
      factory,
      intrinsic_inline_fc,
      font_context,
      viewport_size,
      viewport_scroll,
      nearest_positioned_cb,
      nearest_fixed_cb,
      flex_item_mode: true,
      parallelism,
    }
  }

  pub fn with_parallelism(mut self, parallelism: LayoutParallelism) -> Self {
    self.parallelism = parallelism;
    self.factory = self.factory.clone().with_parallelism(parallelism);
    self.intrinsic_inline_fc =
      Arc::new(InlineFormattingContext::with_factory(self.factory.clone()));
    self
  }

  fn child_factory(&self) -> FormattingContextFactory {
    self.factory.clone()
  }

  fn child_factory_for_cb(&self, cb: ContainingBlock) -> FormattingContextFactory {
    if cb == self.nearest_positioned_cb {
      self.child_factory()
    } else {
      self.factory.with_positioned_cb(cb)
    }
  }

  fn intrinsic_inline_content_sizes_for_sizing_keywords(
    &self,
    node: &BoxNode,
    fc_type: FormattingContextType,
    factory: &FormattingContextFactory,
  ) -> Result<(f32, f32), LayoutError> {
    let style_override = crate::layout::style_override::style_override_for(node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| node.style.as_ref());
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
    let intrinsic_edges = if inline_is_horizontal {
      horizontal_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context)
    } else {
      vertical_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context)
    };
    let compute = || {
      let mut override_style = style.clone();
      override_style.width = None;
      override_style.width_keyword = None;
      override_style.min_width = None;
      override_style.min_width_keyword = None;
      override_style.max_width = None;
      override_style.max_width_keyword = None;
      let override_style = Arc::new(override_style);

      if node.id != 0 {
        crate::layout::style_override::with_style_override(node.id, override_style, || {
          if fc_type == FormattingContextType::Block {
            self.compute_intrinsic_inline_sizes(node)
          } else {
            factory.get(fc_type).compute_intrinsic_inline_sizes(node)
          }
        })
      } else {
        let mut cloned = node.clone();
        cloned.style = override_style;
        if fc_type == FormattingContextType::Block {
          self.compute_intrinsic_inline_sizes(&cloned)
        } else {
          factory.get(fc_type).compute_intrinsic_inline_sizes(&cloned)
        }
      }
    };
    let (min_border, max_border) = compute()?;
    Ok((
      (min_border - intrinsic_edges).max(0.0),
      (max_border - intrinsic_edges).max(0.0),
    ))
  }

  fn resolve_intrinsic_size_keyword_to_content_width(
    &self,
    keyword: IntrinsicSizeKeyword,
    min_content: f32,
    max_content: f32,
    available_content: f32,
    containing_width: f32,
    style: &ComputedStyle,
    inline_edges: f32,
  ) -> f32 {
    // The intrinsic sizing keywords (`min-content`, `max-content`, `fit-content(...)`) are defined
    // in terms of the element's intrinsic *border-box* sizes. Internally we carry content-box
    // dimensions in most of the block formatting context, so convert to border-box for the clamp
    // and then back to content-box at the end. This also naturally rebases percentage padding
    // because `min_content`/`max_content` are computed with a 0px percentage base.
    let min_border = min_content + inline_edges;
    let max_border = max_content + inline_edges;
    let available_border = available_content + inline_edges;

    let used_border = match keyword {
      IntrinsicSizeKeyword::MinContent => min_border,
      IntrinsicSizeKeyword::MaxContent => max_border,
      IntrinsicSizeKeyword::FillAvailable => available_border,
      IntrinsicSizeKeyword::FitContent { limit } => match limit {
        None => max_border.min(available_border.max(min_border)),
        Some(limit) => {
          let limit_border = resolve_length_for_width(
            limit,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
          );
          let limit_border =
            border_size_from_box_sizing(limit_border, inline_edges, style.box_sizing);
          max_border.min(limit_border.max(min_border))
        }
      },
    };

    (used_border - inline_edges).max(0.0)
  }

  fn child_factory_for_cbs(
    &self,
    positioned_cb: ContainingBlock,
    fixed_cb: ContainingBlock,
  ) -> FormattingContextFactory {
    let factory = self.child_factory_for_cb(positioned_cb);
    if fixed_cb == factory.nearest_fixed_cb() {
      factory
    } else {
      factory.with_fixed_cb(fixed_cb)
    }
  }

  fn maybe_attach_footnote_anchor(
    &self,
    child: &BoxNode,
    containing_width: f32,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    fragment: &mut FragmentNode,
  ) -> Result<(), LayoutError> {
    let Some(body) = child.footnote_body.as_deref() else {
      return Ok(());
    };

    let snapshot_node = body.clone();
    let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
    let fc_type = snapshot_node.formatting_context().unwrap_or_else(|| {
      if snapshot_node.is_block_level() {
        FormattingContextType::Block
      } else {
        FormattingContextType::Inline
      }
    });
    let fc = factory.get(fc_type);
    let snapshot_constraints = LayoutConstraints::new(
      AvailableSpace::Definite(containing_width.max(0.0)),
      AvailableSpace::Indefinite,
    );
    let snapshot_fragment = fc.layout(&snapshot_node, &snapshot_constraints)?;
    let anchor_bounds = Rect::from_xywh(0.0, 0.0, 0.0, 0.01);
    let mut anchor = FragmentNode::new_footnote_anchor(anchor_bounds, snapshot_fragment);
    anchor.style = Some(child.style.clone());
    fragment.children_mut().push(anchor);
    Ok(())
  }

  /// Lays out a single block-level child and returns its fragment
  #[allow(clippy::cognitive_complexity)]
  fn layout_block_child(
    &self,
    parent: &BoxNode,
    child: &BoxNode,
    containing_width: f32,
    constraints: &LayoutConstraints,
    box_y: f32,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    external_float_ctx: Option<&mut FloatContext>,
    external_float_base_y: f32,
    paint_viewport: Rect,
  ) -> Result<FragmentNode, LayoutError> {
    let toggles = crate::debug::runtime::runtime_toggles();
    let dump_child_y = toggles.truthy("FASTR_DUMP_CELL_CHILD_Y");
    let log_wide_flex = toggles.truthy("FASTR_LOG_WIDE_FLEX");
    if let BoxType::Replaced(replaced_box) = &child.box_type {
      let mut fragment = self.layout_replaced_child(
        child,
        replaced_box,
        containing_width,
        constraints,
        box_y,
        nearest_positioned_cb,
      )?;
      self.maybe_attach_footnote_anchor(
        child,
        containing_width,
        nearest_positioned_cb,
        nearest_fixed_cb,
        &mut fragment,
      )?;
      return Ok(fragment);
    }

    let style = &child.style;
    let font_size = style.font_size; // Get font-size for resolving em units
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
    // Map physical width/height inputs to the logical inline/block axes used by the block
    // formatting context.
    let (inline_length, inline_keyword, min_inline_length, min_inline_keyword, max_inline_length, max_inline_keyword) =
      if inline_is_horizontal {
        (
          style.width,
          style.width_keyword,
          style.min_width,
          style.min_width_keyword,
          style.max_width,
          style.max_width_keyword,
        )
      } else {
        (
          style.height,
          style.height_keyword,
          style.min_height,
          style.min_height_keyword,
          style.max_height,
          style.max_height_keyword,
        )
      };
    let (block_length, block_keyword, min_block_keyword, max_block_keyword) = if inline_is_horizontal {
      (
        style.height,
        style.height_keyword,
        style.min_height_keyword,
        style.max_height_keyword,
      )
    } else {
      (
        style.width,
        style.width_keyword,
        style.min_width_keyword,
        style.max_width_keyword,
      )
    };
    let containing_height = if inline_is_horizontal {
      constraints.height()
    } else {
      constraints.width()
    };

    // Handle block-axis margins (resolve em/rem units with font-size)
    let block_sides = block_axis_sides(style);
    let margin_top = resolve_margin_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let margin_bottom = resolve_margin_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    if dump_child_y && matches!(child.style.display, Display::Table) {
      eprintln!(
                "block child margins: display={:?} margin_top={:.2} box_y={:.2}",
                child.style.display, margin_top, box_y
            );
    }

    // Pre-resolve vertical edges so box-sizing and used-size overrides can convert border-box sizes
    // into content sizes before laying out descendants.
    let border_top = resolve_border_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let border_bottom = resolve_border_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let mut padding_top = resolve_padding_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let mut padding_bottom = resolve_padding_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let reserve_horizontal_gutter = matches!(style.overflow_x, Overflow::Scroll)
      || (style.scrollbar_gutter.stable
        && matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll));
    if reserve_horizontal_gutter {
      let gutter = resolve_scrollbar_width(style);
      if style.scrollbar_gutter.both_edges {
        padding_top += gutter;
      }
      padding_bottom += gutter;
    }
    let vertical_edges = border_top + padding_top + padding_bottom + border_bottom;

    // Create constraints for child layout.
    let height_auto = block_length.is_none() && block_keyword.is_none();
    let available_block_border_box = containing_height
      .map(|h| (h - margin_top - margin_bottom).max(0.0))
      .unwrap_or(f32::INFINITY);

    let intrinsic_block_sizes = if block_keyword.is_some()
      || min_block_keyword.is_some()
      || max_block_keyword.is_some()
    {
      let factory = self.child_factory_for_cb(*nearest_positioned_cb);
      let fc_type = child
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let (min_base0, max_base0) = if fc_type == FormattingContextType::Block {
        compute_intrinsic_block_sizes_without_block_size_constraints(self, child)?
      } else {
        let fc = factory.get(fc_type);
        compute_intrinsic_block_sizes_without_block_size_constraints(fc.as_ref(), child)?
      };

      let border_top_base0 = resolve_border_side(
        style,
        block_sides.0,
        0.0,
        &self.font_context,
        self.viewport_size,
      );
      let border_bottom_base0 = resolve_border_side(
        style,
        block_sides.1,
        0.0,
        &self.font_context,
        self.viewport_size,
      );
      let mut padding_top_base0 = resolve_padding_side(
        style,
        block_sides.0,
        0.0,
        &self.font_context,
        self.viewport_size,
      );
      let mut padding_bottom_base0 = resolve_padding_side(
        style,
        block_sides.1,
        0.0,
        &self.font_context,
        self.viewport_size,
      );
      if reserve_horizontal_gutter {
        let gutter = resolve_scrollbar_width(style);
        if style.scrollbar_gutter.both_edges {
          padding_top_base0 += gutter;
        }
        padding_bottom_base0 += gutter;
      }
      let vertical_edges_base0 =
        border_top_base0 + padding_top_base0 + padding_bottom_base0 + border_bottom_base0;
      Some((
        rebase_intrinsic_border_box_size(min_base0, vertical_edges_base0, vertical_edges),
        rebase_intrinsic_border_box_size(max_base0, vertical_edges_base0, vertical_edges),
      ))
    } else {
      None
    };

    let mut specified_height = block_length.and_then(|h| {
      resolve_length_with_percentage_metrics(
        h,
        containing_height,
        self.viewport_size,
        font_size,
        style.root_font_size,
        Some(style),
        Some(&self.font_context),
      )
    });
    specified_height =
      specified_height.map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing));
    if let Some(height_keyword) = block_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let used_border_box = match height_keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      specified_height = Some((used_border_box - vertical_edges).max(0.0));
    }
    if specified_height.is_none() && height_auto {
      let used_border_box = if inline_is_horizontal {
        constraints.used_border_box_height
      } else {
        constraints.used_border_box_width
      };
      if let Some(used_border_box) = used_border_box {
        specified_height = Some((used_border_box - vertical_edges).max(0.0));
      }
    }
    let child_height_space = specified_height
      .map(AvailableSpace::Definite)
      .unwrap_or(AvailableSpace::Indefinite);

    // Compute inline size using CSS 2.1 Section 10.3.3 algorithm
    let inline_sides = inline_axis_sides(style);
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    let mut computed_width = compute_block_width(
      style,
      containing_width,
      self.viewport_size,
      inline_sides,
      inline_positive,
    );
    let width_auto = inline_length.is_none() && inline_keyword.is_none();
    let inline_edges_for_fit = computed_width.border_left
      + computed_width.padding_left
      + computed_width.padding_right
      + computed_width.border_right;
    let available_inline_border_box = (containing_width
      - resolve_margin_side(
        style,
        inline_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
      )
      - resolve_margin_side(
        style,
        inline_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
      ))
    .max(0.0);
    let available_content_for_fit = (available_inline_border_box - inline_edges_for_fit).max(0.0);
    let mut intrinsic_content_sizes = None;
    if inline_length.is_none() && inline_keyword.is_some() {
      let keyword = inline_keyword.unwrap();
      let factory = self.child_factory_for_cb(*nearest_positioned_cb);
      let fc_type = child
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let (min_content, max_content) =
        self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?;
      intrinsic_content_sizes = Some((min_content, max_content));
      let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style,
        inline_edges_for_fit,
      );
      let specified_width = match style.box_sizing {
        crate::style::types::BoxSizing::ContentBox => keyword_content,
        crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges_for_fit,
      };
      let mut width_style = (*style).as_ref().clone();
      if inline_is_horizontal {
        width_style.width = Some(Length::px(specified_width));
        width_style.width_keyword = None;
      } else {
        width_style.height = Some(Length::px(specified_width));
        width_style.height_keyword = None;
      }
      computed_width = compute_block_width(
        &width_style,
        containing_width,
        self.viewport_size,
        inline_sides,
        inline_positive,
      );
    }
    if toggles.truthy("FASTR_LOG_BLOCK_WIDE")
      && computed_width.total_width() > containing_width + 0.5
    {
      let selector = child
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<child>".to_string());
      eprintln!(
                "[block-wide] id={} selector={} containing_w={:.1} content_w={:.1} total_w={:.1} width_decl={:?} min_w={:?} max_w={:?} margins=({:.1},{:.1})",
                child.id,
                selector,
                containing_width,
                computed_width.content_width,
                computed_width.total_width(),
                style.width,
                style.min_width,
                style.max_width,
                computed_width.margin_left,
                computed_width.margin_right,
            );
    }
    if width_auto {
      if let (Some(ratio), Some(h)) = (
        match style.aspect_ratio {
          crate::style::types::AspectRatio::Ratio(ratio)
          | crate::style::types::AspectRatio::AutoRatio(ratio) => Some(ratio),
          crate::style::types::AspectRatio::Auto => None,
        },
        specified_height,
      ) {
        if ratio > 0.0 {
          computed_width.content_width = if inline_is_horizontal {
            h * ratio
          } else {
            h / ratio
          };
        }
      }
    }

    // Tables use a shrink-to-fit inline size when `width` is `auto` (CSS 2.1 §17.5.2).
    // Without this, the block constraint equation would force auto-width tables to span the
    // containing block, which then makes `table-layout: fixed` distribute slack into authored
    // columns (CSS 2.1 §17.5.2.1) and breaks expected fixed-width column behavior.
    let shrink_to_fit = style.shrink_to_fit_inline_size || matches!(style.display, Display::Table);
    if shrink_to_fit && width_auto {
      let inline_edges = computed_width.border_left
        + computed_width.padding_left
        + computed_width.padding_right
        + computed_width.border_right;
      let margin_left = resolve_margin_side(
        style,
        inline_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
      );
      let margin_right = resolve_margin_side(
        style,
        inline_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
      );

      let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
      let fc_type = child
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let fc = factory.get(fc_type);
      let (preferred_min_content, preferred_content) = fc.compute_intrinsic_inline_sizes(child)?;

      let edges_base0 =
        inline_axis_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context);
      let preferred_min =
        rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, inline_edges);
      let preferred =
        rebase_intrinsic_border_box_size(preferred_content, edges_base0, inline_edges);
      let available = (containing_width - margin_left - margin_right).max(0.0);
      let shrink_border_box = preferred.min(available.max(preferred_min));
      let shrink_content = (shrink_border_box - inline_edges).max(0.0);
      let (margin_left, margin_right) = recompute_margins_for_width(
        style,
        containing_width,
        shrink_content,
        computed_width.border_left,
        computed_width.padding_left,
        computed_width.padding_right,
        computed_width.border_right,
        self.viewport_size,
        &self.font_context,
      );
      computed_width.content_width = shrink_content;
      computed_width.margin_left = margin_left;
      computed_width.margin_right = margin_right;
    }

    // CSS 2.1 §10.4: apply min/max inline-size constraints after computing the
    // tentative used width/margins. When clamping changes the used width, we
    // need to re-resolve auto margins so centering works (e.g. max-width +
    // margin: 0 auto).
    //
    // In-flow block children are laid out via `layout_block_child` (not via a recursive
    // BlockFormattingContext::layout call), so we must apply min/max sizing here to keep wrappers
    // like `max-width: 920px; margin: 0 auto` from inflating to the full containing block width.
    let horizontal_edges = computed_width.border_left
      + computed_width.padding_left
      + computed_width.padding_right
      + computed_width.border_right;
    let min_width = if let Some(keyword) = min_inline_keyword {
      if intrinsic_content_sizes.is_none() {
        let factory = self.child_factory_for_cb(*nearest_positioned_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        intrinsic_content_sizes =
          Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?);
      }
      let (min_content, max_content) = intrinsic_content_sizes.unwrap();
      self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style,
        horizontal_edges,
      )
    } else {
      min_inline_length
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
          )
        })
        .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
        .unwrap_or(0.0)
    };

    let max_width = if let Some(keyword) = max_inline_keyword {
      if intrinsic_content_sizes.is_none() {
        let factory = self.child_factory_for_cb(*nearest_positioned_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        intrinsic_content_sizes =
          Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?);
      }
      let (min_content, max_content) = intrinsic_content_sizes.unwrap();
      self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style,
        horizontal_edges,
      )
    } else {
      max_inline_length
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
          )
        })
        .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    };
    let max_width = if max_width.is_finite() && max_width < min_width {
      min_width
    } else {
      max_width
    };
    let clamped_content_width =
      crate::layout::utils::clamp_with_order(computed_width.content_width, min_width, max_width);
    if clamped_content_width != computed_width.content_width {
      let (margin_left, margin_right) = recompute_margins_for_width(
        style,
        containing_width,
        clamped_content_width,
        computed_width.border_left,
        computed_width.padding_left,
        computed_width.padding_right,
        computed_width.border_right,
        self.viewport_size,
        &self.font_context,
      );
      computed_width.content_width = clamped_content_width;
      computed_width.margin_left = margin_left;
      computed_width.margin_right = margin_right;
    }

    if width_auto {
      let used_border_box = if inline_is_horizontal {
        constraints.used_border_box_width
      } else {
        constraints.used_border_box_height
      };
      if let Some(used_border_box) = used_border_box {
        let horizontal_edges = computed_width.border_left
          + computed_width.padding_left
          + computed_width.padding_right
          + computed_width.border_right;
        let used_content = (used_border_box - horizontal_edges).max(0.0);
        let (margin_left, margin_right) = recompute_margins_for_width(
          style,
          containing_width,
          used_content,
          computed_width.border_left,
          computed_width.padding_left,
          computed_width.padding_right,
          computed_width.border_right,
          self.viewport_size,
          &self.font_context,
        );
        computed_width.content_width = used_content;
        computed_width.margin_left = margin_left;
        computed_width.margin_right = margin_right;
      }
    }
    let child_constraints = if inline_is_horizontal {
      LayoutConstraints::new(
        AvailableSpace::Definite(computed_width.content_width),
        child_height_space,
      )
    } else {
      LayoutConstraints::new(
        child_height_space,
        AvailableSpace::Definite(computed_width.content_width),
      )
    }
    .with_inline_percentage_base(Some(computed_width.content_width));

    // Check if this child establishes a different formatting context
    let fc_type = child.formatting_context();
    let log_flex_child = toggles.truthy("FASTR_LOG_FLEX_CHILD");
    let log_flex_child_ids = toggles
      .usize_list("FASTR_LOG_FLEX_CHILD_IDS")
      .unwrap_or_default();

    if matches!(
      fc_type,
      Some(FormattingContextType::Flex | FormattingContextType::Grid)
    ) {
      if log_flex_child || log_flex_child_ids.contains(&child.id) {
        let child_selector = child
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<child>".to_string());
        eprintln!(
                    "[flex-child-constraint] parent_id={} child_id={} child_sel={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1} style_margins=({:?},{:?}) parent_style_width={:?} parent_min_w={:?} parent_max_w={:?}",
                    parent.id,
                    child.id,
                    child_selector,
                    containing_width,
                    computed_width.content_width,
                    computed_width.total_width(),
                    child_constraints.width(),
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                    child.style.margin_left,
                    child.style.margin_right,
                    parent.style.width,
                    parent.style.min_width,
                    parent.style.max_width,
                );
      }
      if log_wide_flex {
        let content_w = computed_width.content_width;
        let total_w = computed_width.total_width();
        let constraint_w = child_constraints.width();
        if content_w > self.viewport_size.width + 0.5
          || total_w > self.viewport_size.width + 0.5
          || constraint_w
            .map(|w| w > self.viewport_size.width + 0.5)
            .unwrap_or(false)
          || content_w > containing_width + 0.5
          || total_w > containing_width + 0.5
        {
          let selector = child
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anonymous>".to_string());
          eprintln!(
                        "[flex-constraint-wide] parent_id={} child_id={:?} selector={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1}",
                        parent.id,
                        child.id,
                        selector,
                        containing_width,
                        content_w,
                        total_w,
                    constraint_w,
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                );
        }
      }
      if toggles.truthy("FASTR_LOG_NARROW_FLEX") && computed_width.content_width < 150.0 {
        // Compute how much auto margins and percentage padding/borders left for content.
        let horiz_edges = computed_width.border_left
          + computed_width.padding_left
          + computed_width.padding_right
          + computed_width.border_right;
        let selector = child
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anonymous>".to_string());
        eprintln!(
                    "[flex-constraint-narrow] child_id={:?} selector={} containing={:.1} content_w={:.1} total_w={:.1} constraint_w={:?} margins=({:.1},{:.1}) width={:?} min_w={:?} max_w={:?} viewport_w={:.1} edges={:.1} auto_width={:?}",
                    child.id,
                    selector,
                    containing_width,
                    computed_width.content_width,
                    computed_width.total_width(),
                    child_constraints.width(),
                    computed_width.margin_left,
                    computed_width.margin_right,
                    child.style.width,
                    child.style.min_width,
                    child.style.max_width,
                    self.viewport_size.width,
                    horiz_edges,
                    child.style.width.is_none(),
                );
      }
    }

    // If this block establishes a new containing block for absolute/fixed descendants (via
    // positioning, transforms, filters, containment, etc.), propagate that updated containing
    // block into the descendant layout call. Otherwise absolutely-positioned descendants inside
    // inline content can incorrectly resolve percentages against an ancestor CB (e.g. the
    // viewport).
    let establishes_positioned_cb = style.establishes_abs_containing_block();
    let establishes_fixed_cb = style.establishes_fixed_containing_block();
    let content_origin = Point::new(
      computed_width.border_left + computed_width.padding_left,
      border_top + padding_top,
    );
    let padding_origin = Point::new(computed_width.border_left, border_top);
    let content_height_base = specified_height.unwrap_or(0.0).max(0.0);
    let padding_size = Size::new(
      computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
      content_height_base + padding_top + padding_bottom,
    );
    let cb_block_base = specified_height.map(|h| h.max(0.0) + padding_top + padding_bottom);
    let descendant_nearest_positioned_cb = if establishes_positioned_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(padding_origin, padding_size),
        self.viewport_size,
        Some(padding_size.width),
        cb_block_base,
      )
    } else {
      *nearest_positioned_cb
    };
    let descendant_nearest_fixed_cb = if establishes_fixed_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(padding_origin, padding_size),
        self.viewport_size,
        Some(padding_size.width),
        cb_block_base,
      )
    } else {
      *nearest_fixed_cb
    };

    let box_width = computed_width.border_box_width();
    let box_width = if box_width.is_finite() {
      box_width.max(0.0)
    } else {
      0.0
    };
    let child_border_origin = Point::new(computed_width.margin_left, box_y);
    let child_content_origin = Point::new(
      child_border_origin.x + content_origin.x,
      child_border_origin.y + content_origin.y,
    );
    let child_viewport =
      paint_viewport.translate(Point::new(-child_content_origin.x, -child_content_origin.y));

    let skip_contents = match style.content_visibility {
      crate::style::types::ContentVisibility::Hidden => true,
      crate::style::types::ContentVisibility::Auto => {
        // A deterministic heuristic aligned with Chrome: if the element's border box does not
        // intersect the paint viewport, treat it as skipped content and size the box using
        // `contain-intrinsic-size` fallback rules.
        let activation_margin = toggles
          .f64("FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX")
          .unwrap_or(0.0)
          .max(0.0) as f32;
        let viewport = if activation_margin > 0.0 {
          paint_viewport.inflate(activation_margin)
        } else {
          paint_viewport
        };

        let estimated_border_box_block_size = specified_height
          .filter(|h| h.is_finite())
          .map(|h| h.max(0.0))
          .or_else(|| {
            let axis_is_width = block_axis_is_horizontal(style.writing_mode);
            let axis = if axis_is_width {
              style.contain_intrinsic_width
            } else {
              style.contain_intrinsic_height
            };
            axis
              .auto
              .then(|| {
                remembered_size_cache_lookup(child).map(|size| {
                  if axis_is_width {
                    size.width
                  } else {
                    size.height
                  }
                })
              })
              .flatten()
              .filter(|v| v.is_finite())
              .map(|v| v.max(0.0))
              .or_else(|| {
                axis
                  .length
                  .and_then(|l| {
                    resolve_length_with_percentage(
                      l,
                      containing_height,
                      self.viewport_size,
                      style.font_size,
                      style.root_font_size,
                    )
                  })
                  .map(|v| v.max(0.0))
              })
          })
          .and_then(|content_estimate| {
            let border_box = content_estimate + vertical_edges;
            border_box.is_finite().then_some(border_box.max(0.0))
          });

        if let Some(block_size) = estimated_border_box_block_size {
          let border_box =
            Rect::from_xywh(computed_width.margin_left, box_y, box_width, block_size);
          !viewport.intersects(border_box)
        } else {
          // Without a definite placeholder block-size (explicit height or a resolved
          // `contain-intrinsic-*` length), skipping layout would collapse the element to 0px (the
          // initial `contain-intrinsic-size: auto` has no fallback length) and pull later siblings
          // upward. In that case, keep laying out to determine sizing; paint skipping still
          // applies.
          false
        }
      }
      crate::style::types::ContentVisibility::Visible => false,
    };

    let use_columns = Self::is_multicol_container(style);

    // Child establishes a non-block formatting context (flex/grid/table). Delegate layout to the
    // appropriate formatting context and return its fragment directly.
    //
    // The block formatting context still owns margin collapsing and used width resolution for
    // block-level boxes. Provide the resolved border-box size via `used_border_box_*` so the child
    // formatting context doesn't re-run block wrapper logic (which would double-apply
    // padding/borders and can generate duplicate fragments for the same box).
    if !skip_contents {
      if let Some(fc_type) = fc_type {
        if fc_type != FormattingContextType::Block {
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          // Layout skipping (`content-visibility:auto`) inside the child formatting context is
          // viewport-relative, so translate the factory viewport scroll offset into the child’s
          // local coordinate space before invoking layout. Without this, nested flex/grid contexts
          // interpret the viewport as starting at (0, 0) and may incorrectly unskip offscreen
          // descendants.
          let parent_scroll = factory.viewport_scroll();
          let parent_scroll = if parent_scroll.x.is_finite() && parent_scroll.y.is_finite() {
            parent_scroll
          } else {
            Point::ZERO
          };
          let child_scroll = Point::new(
            parent_scroll.x - child_border_origin.x,
            parent_scroll.y - child_border_origin.y,
          );
          let factory = factory.with_viewport_scroll(child_scroll);
          let fc = factory.get(fc_type);

          let used_border_box_width = computed_width.border_box_width();
          let used_border_box_height =
            specified_height.map(|h| (h.max(0.0) + vertical_edges).max(0.0));
          let fc_constraints = LayoutConstraints::new(
            AvailableSpace::Definite(containing_width),
            constraints.available_height,
          )
          .with_used_border_box_size(Some(used_border_box_width), used_border_box_height);

          let mut fragment = fc.layout(child, &fc_constraints)?;
          let desired_origin = child_border_origin;
          let offset = Point::new(
            desired_origin.x - fragment.bounds.x(),
            desired_origin.y - fragment.bounds.y(),
          );
          if offset != Point::ZERO {
            fragment.translate_root_in_place(offset);
          }
          let remembered_block = (fragment.bounds.height() - vertical_edges).max(0.0);
          let remembered_inline = computed_width.content_width;
          let remembered = if block_axis_is_horizontal(style.writing_mode) {
            Size::new(remembered_block, remembered_inline)
          } else {
            Size::new(remembered_inline, remembered_block)
          };
          remembered_size_cache_store(child, remembered);
          fragment.block_metadata = Some(BlockFragmentMetadata {
            margin_top,
            margin_bottom,
            ..BlockFragmentMetadata::default()
          });
          self.maybe_attach_footnote_anchor(
            child,
            containing_width,
            nearest_positioned_cb,
            nearest_fixed_cb,
            &mut fragment,
          )?;

          return Ok(fragment);
        }
      }
    }

    let (mut child_fragments, mut content_height, positioned_children, column_info) =
      if skip_contents {
        (Vec::new(), 0.0, Vec::new(), None)
      } else if use_columns {
        let (frags, height, positioned, info) = self.layout_multicolumn(
          child,
          &child_constraints,
          &descendant_nearest_positioned_cb,
          &descendant_nearest_fixed_cb,
          computed_width.content_width,
          child_viewport,
        )?;
        (frags, height, positioned, info)
      } else {
        let (frags, height, positioned) = self.layout_children_with_external_floats(
          child,
          &child_constraints,
          &descendant_nearest_positioned_cb,
          &descendant_nearest_fixed_cb,
          child_viewport,
          external_float_ctx,
          external_float_base_y + child_content_origin.y,
        )?;
        (frags, height, positioned, None)
      };

    if skip_contents || style.containment.size {
      let axis_is_width = block_axis_is_horizontal(style.writing_mode);
      let axis = if axis_is_width {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let remembered = axis
        .auto
        .then(|| {
          remembered_size_cache_lookup(child).map(|size| {
            if axis_is_width {
              size.width
            } else {
              size.height
            }
          })
        })
        .flatten();
      content_height = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        remembered,
        containing_height,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
    }

    // Child fragments are produced in the block's content coordinate space (0,0 at the content
    // box). Translate them into the fragment's local coordinate space (border box) so padding and
    // borders correctly offset in-flow content.
    if content_origin.x != 0.0 || content_origin.y != 0.0 {
      for fragment in child_fragments.iter_mut() {
        fragment.translate_root_in_place(content_origin);
      }
    }

    // Height computation (CSS 2.1 Section 10.6.3) with aspect-ratio adjustment (CSS Sizing L4)
    let mut height = specified_height.unwrap_or(content_height);
    if specified_height.is_none() {
      if let crate::style::types::AspectRatio::Ratio(ratio)
      | crate::style::types::AspectRatio::AutoRatio(ratio) = style.aspect_ratio
      {
        if ratio > 0.0 && computed_width.content_width.is_finite() {
          let ratio_height = if inline_is_horizontal {
            computed_width.content_width / ratio
          } else {
            computed_width.content_width * ratio
          };
          // Do not shrink below content-based height
          height = height.max(ratio_height);
        }
      }
    }

    // Apply min/max height constraints
    let min_height_keyword = if inline_is_horizontal {
      style.min_height_keyword
    } else {
      style.min_width_keyword
    };
    let min_height = if let Some(keyword) = min_height_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let min_border = match keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      (min_border - vertical_edges).max(0.0)
    } else {
      (if inline_is_horizontal {
        style.min_height
      } else {
        style.min_width
      })
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics(
            *l,
            containing_height,
            self.viewport_size,
            font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(0.0)
    };
    let max_height_keyword = if inline_is_horizontal {
      style.max_height_keyword
    } else {
      style.max_width_keyword
    };
    let max_height = if let Some(keyword) = max_height_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let max_border = match keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      (max_border - vertical_edges).max(0.0)
    } else {
      (if inline_is_horizontal {
        style.max_height
      } else {
        style.max_width
      })
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics(
            *l,
            containing_height,
            self.viewport_size,
            font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    };
    let max_height = if max_height.is_finite() && max_height < min_height {
      min_height
    } else {
      max_height
    };
    let height = crate::layout::utils::clamp_with_order(height, min_height, max_height);

    // Create the fragment
    let box_height = border_top + padding_top + height + padding_bottom + border_bottom;
    let box_width = computed_width.border_box_width();

    // Layout out-of-flow positioned children against this block's padding box.
    if !positioned_children.is_empty() {
      let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
        self.font_context.clone(),
      );
      let mut anchor_index =
        crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
          child_fragments.as_slice(),
          child.id,
          &style.anchor_scope,
          self.viewport_size,
        );
      // Allow descendants to anchor against the containing block element itself.
      anchor_index.insert_names_for_box(
        child.id,
        &style.anchor_names,
        crate::layout::anchor_positioning::AnchorBox {
          rect: Rect::from_xywh(0.0, 0.0, box_width, box_height),
          writing_mode: style.writing_mode,
          direction: style.direction,
        },
      );
      let padding_origin = Point::new(computed_width.border_left, border_top);
      let padding_size = Size::new(
        computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
        height + padding_top + padding_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);
      let parent_padding_cb = ContainingBlock::with_viewport_and_bases(
        padding_rect,
        self.viewport_size,
        Some(padding_size.width),
        Some(padding_size.height),
      );
      let base_factory = self.factory.clone();
      let viewport_cb = ContainingBlock::viewport(self.viewport_size);
      let abs_factory = if parent_padding_cb == base_factory.nearest_positioned_cb() {
        base_factory.clone()
      } else {
        base_factory.with_positioned_cb(parent_padding_cb)
      };
      let fixed_factory = if viewport_cb == parent_padding_cb {
        abs_factory.clone()
      } else if viewport_cb == base_factory.nearest_positioned_cb() {
        base_factory.clone()
      } else {
        base_factory.with_positioned_cb(viewport_cb)
      };
      let factory_for_cb = |cb: ContainingBlock| -> &FormattingContextFactory {
        if cb == parent_padding_cb {
          &abs_factory
        } else if cb == viewport_cb {
          &fixed_factory
        } else {
          &base_factory
        }
      };

      let trace_positioned = trace_positioned_ids();
      for PositionedCandidate {
        node: pos_child,
        source,
        static_position,
        query_parent_id,
      } in positioned_children
      {
        let original_style = pos_child.style.clone();
        if trace_positioned.contains(&pos_child.id) {
          eprintln!(
                        "[block-positioned-layout] parent_id={} child_id={} padding_rect=({:.1},{:.1},{:.1},{:.1})",
                        parent.id,
                        pos_child.id,
                        padding_rect.x(),
                        padding_rect.y(),
                        padding_rect.width(),
                        padding_rect.height()
                    );
        }
        let cb = match source {
          ContainingBlockSource::ParentPadding => parent_padding_cb,
          ContainingBlockSource::Explicit(cb) => cb,
        };
        let factory = factory_for_cb(cb);
        // Layout the child as if it were in normal flow to obtain its intrinsic size.
        let mut static_style = (*pos_child.style).clone();
        static_style.position = Position::Relative;
        static_style.top = crate::style::types::InsetValue::Auto;
        static_style.right = crate::style::types::InsetValue::Auto;
        static_style.bottom = crate::style::types::InsetValue::Auto;
        static_style.left = crate::style::types::InsetValue::Auto;
        let static_style = Arc::new(static_style);

        let fc_type = pos_child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = factory.get(fc_type);
        let height_available = cb.block_percentage_base();
        let child_height_space = height_available
          .map(AvailableSpace::Definite)
          .unwrap_or(AvailableSpace::Indefinite);
        let child_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(padding_size.width),
          child_height_space,
        );

        // Resolve positioned style against the containing block.
        let anchors_for_cb = Some(&anchor_index);
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
          &pos_child.style,
          &cb,
          self.viewport_size,
          &self.font_context,
          anchors_for_cb,
          Some(query_parent_id),
        );

        let mut static_pos = static_position.unwrap_or(Point::ZERO);
        if cb == parent_padding_cb {
          static_pos = Point::new(
            static_pos.x + computed_width.padding_left,
            static_pos.y + padding_top,
          );
        }
        let is_replaced = pos_child.is_replaced();
        let needs_inline_intrinsics = (positioned_style.width.is_auto()
          && (positioned_style.left.is_auto() || positioned_style.right.is_auto() || is_replaced))
          || original_style.width_keyword.is_some()
          || original_style.min_width_keyword.is_some()
          || original_style.max_width_keyword.is_some();
        let needs_block_intrinsics = (positioned_style.height.is_auto()
          && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()))
          || original_style.height_keyword.is_some()
          || original_style.min_height_keyword.is_some()
          || original_style.max_height_keyword.is_some();
        let (
          mut child_fragment,
          preferred_min_inline,
          preferred_inline,
          preferred_min_block,
          preferred_block,
        ) = if pos_child.id != 0 {
          crate::layout::style_override::with_style_override(
            pos_child.id,
            static_style.clone(),
            || {
              let child_fragment = fc.layout(&pos_child, &child_constraints)?;
              let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                match fc.compute_intrinsic_inline_sizes(&pos_child) {
                  Ok((min, max)) => (Some(min), Some(max)),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {
                    let min = match fc
                      .compute_intrinsic_inline_size(&pos_child, IntrinsicSizingMode::MinContent)
                    {
                      Ok(value) => Some(value),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    let max = match fc
                      .compute_intrinsic_inline_size(&pos_child, IntrinsicSizingMode::MaxContent)
                    {
                      Ok(value) => Some(value),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    (min, max)
                  }
                }
              } else {
                (None, None)
              };
              let preferred_min_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(&pos_child, IntrinsicSizingMode::MinContent) {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              let preferred_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(&pos_child, IntrinsicSizingMode::MaxContent) {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              Ok((
                child_fragment,
                preferred_min_inline,
                preferred_inline,
                preferred_min_block,
                preferred_block,
              ))
            },
          )?
        } else {
          let mut layout_child = pos_child.clone();
          layout_child.style = static_style.clone();
          let child_fragment = fc.layout(&layout_child, &child_constraints)?;
          let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
            match fc.compute_intrinsic_inline_sizes(&layout_child) {
              Ok((min, max)) => (Some(min), Some(max)),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {
                let min = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MinContent)
                {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                let max = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                (min, max)
              }
            }
          } else {
            (None, None)
          };
          let preferred_min_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent) {
              Ok(value) => Some(value),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          let preferred_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent) {
              Ok(value) => Some(value),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          (
            child_fragment,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          )
        };

        let actual_horizontal = positioned_style.padding.left
          + positioned_style.padding.right
          + positioned_style.border_width.left
          + positioned_style.border_width.right;
        let actual_vertical = positioned_style.padding.top
          + positioned_style.padding.bottom
          + positioned_style.border_width.top
          + positioned_style.border_width.bottom;
        let content_offset = Point::new(
          positioned_style.border_width.left + positioned_style.padding.left,
          positioned_style.border_width.top + positioned_style.padding.top,
        );
        let (intrinsic_horizontal, intrinsic_vertical) =
          crate::layout::absolute_positioning::intrinsic_edge_sizes(
            &original_style,
            self.viewport_size,
            &self.font_context,
          );
        let preferred_min_inline =
          preferred_min_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_inline = preferred_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_min_block = preferred_min_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let preferred_block = preferred_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let intrinsic_size = Size::new(
          (child_fragment.bounds.size.width - actual_horizontal).max(0.0),
          (child_fragment.bounds.size.height - actual_vertical).max(0.0),
        );

        let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
          positioned_style,
          intrinsic_size,
          static_pos,
        );
        input.is_replaced = is_replaced;
        input.preferred_min_inline_size = preferred_min_inline;
        input.preferred_inline_size = preferred_inline;
        input.preferred_min_block_size = preferred_min_block;
        input.preferred_block_size = preferred_block;
        input.style.width_keyword = original_style.width_keyword;
        input.style.min_width_keyword = original_style.min_width_keyword;
        input.style.max_width_keyword = original_style.max_width_keyword;
        input.style.height_keyword = original_style.height_keyword;
        input.style.min_height_keyword = original_style.min_height_keyword;
        input.style.max_height_keyword = original_style.max_height_keyword;

        let result = abs.layout_absolute(&input, &cb)?;
        let border_size = Size::new(
          result.size.width + actual_horizontal,
          result.size.height + actual_vertical,
        );
        let border_origin = Point::new(
          result.position.x - content_offset.x,
          result.position.y - content_offset.y,
        );
        let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          || (border_size.height - child_fragment.bounds.height()).abs() > 0.01;
        if needs_relayout {
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
          );
          let relayout_constraints = child_constraints
            .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
          if pos_child.id != 0 {
            if supports_used_border_box {
              child_fragment = crate::layout::style_override::with_style_override(
                pos_child.id,
                static_style.clone(),
                || fc.layout(&pos_child, &relayout_constraints),
              )?;
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(crate::style::values::Length::px(border_size.width));
              relayout_style.height = Some(crate::style::values::Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              relayout_style.min_width_keyword = None;
              relayout_style.max_width_keyword = None;
              relayout_style.min_height_keyword = None;
              relayout_style.max_height_keyword = None;
              child_fragment = crate::layout::style_override::with_style_override(
                pos_child.id,
                Arc::new(relayout_style),
                || fc.layout(&pos_child, &relayout_constraints),
              )?;
            }
          } else {
            let mut relayout_child = pos_child.clone();
            if supports_used_border_box {
              relayout_child.style = static_style.clone();
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(crate::style::values::Length::px(border_size.width));
              relayout_style.height = Some(crate::style::values::Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              relayout_style.min_width_keyword = None;
              relayout_style.max_width_keyword = None;
              relayout_style.min_height_keyword = None;
              relayout_style.max_height_keyword = None;
              relayout_child.style = Arc::new(relayout_style);
            }
            child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
          }
        }
        child_fragment.bounds = Rect::new(border_origin, border_size);
        child_fragment.style = Some(original_style);
        if trace_positioned.contains(&pos_child.id) {
          let (text_count, total) = count_text_fragments(&child_fragment);
          let mut snippets = Vec::new();
          collect_first_texts(&child_fragment, &mut snippets, 3);
          eprintln!(
                        "[block-positioned-placed] child_id={} pos=({:.1},{:.1}) size=({:.1},{:.1}) texts={}/{} first_texts={:?}",
                        pos_child.id,
                        border_origin.x,
                        border_origin.y,
                        border_size.width,
                        border_size.height,
                        text_count,
                        total,
                        snippets
                    );
        }
        child_fragments.push(child_fragment);
      }
    }

    let bounds = Rect::from_xywh(computed_width.margin_left, box_y, box_width, box_height);

    let mut fragment = FragmentNode::new_with_style(
      bounds,
      crate::tree::fragment_tree::FragmentContent::Block {
        box_id: Some(child.id),
      },
      child_fragments,
      child.style.clone(),
    );
    fragment.block_metadata = Some(BlockFragmentMetadata {
      margin_top,
      margin_bottom,
      ..BlockFragmentMetadata::default()
    });
    if let Some(info) = column_info {
      fragment.fragmentation = Some(info.clone());
      // Keep logical bounds aligned with the physical multi-column fragment geometry so
      // pagination uses the clipped height rather than the unfragmented flow height.
      fragment.logical_override = Some(fragment.bounds);
    }

    if !skip_contents {
      // Remember the laid out content-box size so skipped-content placeholder sizing can reuse it
      // in subsequent layout passes (`contain-intrinsic-size: auto`).
      let remembered = if block_axis_is_horizontal(style.writing_mode) {
        Size::new(height, computed_width.content_width)
      } else {
        Size::new(computed_width.content_width, height)
      };
      remembered_size_cache_store(child, remembered);
    }

    self.maybe_attach_footnote_anchor(
      child,
      containing_width,
      nearest_positioned_cb,
      nearest_fixed_cb,
      &mut fragment,
    )?;

    Ok(fragment)
  }

  fn can_parallelize_block_children(parent: &BoxNode) -> bool {
    !parent.children.is_empty()
      && parent.children.iter().all(|child| {
        child.is_block_level()
          && !child.style.float.is_floating()
          && child.style.clear == Clear::None
          && child.style.running_position.is_none()
          && matches!(
            child.style.content_visibility,
            crate::style::types::ContentVisibility::Visible
          )
          && matches!(child.style.position, Position::Static | Position::Relative)
      })
  }

  fn translate_fragment_tree(fragment: &mut FragmentNode, delta: Point) {
    if delta.x == 0.0 && delta.y == 0.0 {
      return;
    }
    // `FragmentNode` positions are stored in the coordinate space of their parent
    // fragment. Child bounds (and `scroll_overflow`) are expressed in the fragment's
    // local coordinate space, so adjusting a block's placement within its parent
    // should only translate the fragment root, not its descendants.
    fragment.bounds = fragment.bounds.translate(delta);
    fragment.logical_override = fragment
      .logical_override
      .map(|logical| logical.translate(delta));
  }

  #[allow(clippy::too_many_arguments)]
  fn try_parallel_block_children(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    margin_ctx: MarginCollapseContext,
    relative_cb: &ContainingBlock,
    containing_width: f32,
    float_ctx_empty: bool,
    paint_viewport: Rect,
  ) -> Option<Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError>> {
    if !self.parallelism.should_parallelize(parent.children.len())
      || !float_ctx_empty
      || !Self::can_parallelize_block_children(parent)
      || Self::is_multicol_container(&parent.style)
    {
      return None;
    }

    let deadline = active_deadline();
    let stage = active_stage();
    let parallel_results = parent
      .children
      .par_iter()
      .enumerate()
      .map(|(idx, child)| {
        with_deadline(deadline.as_ref(), || {
          let _stage_guard = StageGuard::install(stage);
          crate::layout::engine::debug_record_parallel_work();
          let fragment = self.layout_block_child(
            parent,
            child,
            containing_width,
            constraints,
            0.0,
            nearest_positioned_cb,
            nearest_fixed_cb,
            None,
            0.0,
            paint_viewport,
          )?;
          let meta = fragment.block_metadata.clone().ok_or_else(|| {
            LayoutError::MissingContext(
              "Block fragment missing metadata for parallel layout".into(),
            )
          })?;
          Ok((idx, fragment, meta))
        })
      })
      .collect::<Result<Vec<_>, LayoutError>>();

    let mut parallel_results = match parallel_results {
      Ok(results) => results,
      Err(err) => return Some(Err(err)),
    };
    // `par_iter().enumerate()` should produce results in order, but `collect` through `Result`
    // doesn't strictly guarantee it. Avoid paying an unconditional stable `sort_by_key` (which
    // allocates and caches keys) unless the collected results are actually out of order.
    let mut ordered = true;
    let mut prev_idx: Option<usize> = None;
    for (idx, _, _) in &parallel_results {
      if let Some(prev) = prev_idx {
        if *idx <= prev {
          ordered = false;
          break;
        }
      }
      prev_idx = Some(*idx);
    }
    if !ordered {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Some(Err(LayoutError::Timeout { elapsed }));
      }
      parallel_results.sort_unstable_by_key(|(idx, _, _)| *idx);
    }

    let mut fragments = Vec::with_capacity(parallel_results.len());
    let mut content_height: f32 = 0.0;
    let mut current_y = 0.0;
    let mut margin_ctx = margin_ctx;

    for (idx, mut fragment, _meta) in parallel_results {
      let child = &parent.children[idx];
      let child_margins = self.collapsed_block_margins(child, containing_width, false);

      let box_y = if margin_ctx.is_at_start() && !child_margins.collapsible_through {
        // Parent/first-child margin collapsing is represented by the parent’s own collapsed
        // margins. Discard any leading collapsible-through margins and place the first
        // non-empty child at the block start.
        margin_ctx.consume_pending();
        margin_ctx.push_collapsible_margin(child_margins.bottom);
        current_y
      } else {
        let (offset, _) = margin_ctx.process_child_margins(
          child_margins.top,
          child_margins.bottom,
          child_margins.collapsible_through,
        );
        current_y + offset
      };

      let delta = box_y - fragment.bounds.y();
      Self::translate_fragment_tree(&mut fragment, Point::new(0.0, delta));
      content_height = content_height.max(fragment.bounds.max_y());
      current_y = box_y + fragment.bounds.height();

      if matches!(child.style.position, Position::Relative) {
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
          &child.style,
          relative_cb,
          self.viewport_size,
          &self.font_context,
        );
        fragment = match PositionedLayout::with_font_context(self.font_context.clone())
          .apply_relative_positioning(&fragment, &positioned_style, relative_cb)
        {
          Ok(f) => f,
          Err(err) => return Some(Err(err)),
        };
      }

      fragments.push(fragment);
    }

    let trailing_margin = margin_ctx.pending_margin();
    let allow_collapse_last = parent.id != 1 && should_collapse_with_last_child(&parent.style);
    let parent_has_bottom_separation = resolve_length_for_width(
      parent.style.used_border_bottom_width(),
      containing_width,
      &parent.style,
      &self.font_context,
      self.viewport_size,
    ) > 0.0
      || resolve_length_for_width(
        parent.style.padding_bottom,
        containing_width,
        &parent.style,
        &self.font_context,
        self.viewport_size,
      ) > 0.0;

    if !allow_collapse_last || parent_has_bottom_separation {
      // Trailing margins extend the BFC height only relative to the in-flow cursor. If floats (or
      // overlapping negative margins) already extend `content_height` past the in-flow end, don't
      // double-count the margin after the float bottom.
      content_height = content_height.max(current_y + trailing_margin.max(0.0));
    }

    Some(Ok((fragments, content_height, Vec::new())))
  }

  fn layout_replaced_child(
    &self,
    child: &BoxNode,
    replaced_box: &ReplacedBox,
    containing_width: f32,
    constraints: &LayoutConstraints,
    box_y: f32,
    _nearest_positioned_cb: &ContainingBlock,
  ) -> Result<FragmentNode, LayoutError> {
    let style = &child.style;
    let toggles = crate::debug::runtime::runtime_toggles();
    let log_wide_flex = toggles.truthy("FASTR_LOG_WIDE_FLEX");

    // Percentages on replaced elements resolve against the containing block size (width/height
    // when available). Even if the block height is indefinite, we still have a valid width
    // percentage base, which allows max-width: 100% (UA default) to clamp oversized images.
    let percentage_base = Some(crate::geometry::Size::new(
      containing_width,
      constraints.height().unwrap_or(f32::NAN),
    ));
    let used_size = compute_replaced_size(style, replaced_box, percentage_base, self.viewport_size);
    if log_wide_flex && used_size.width > containing_width + 0.5 {
      let resolved_max_w = style.max_width.as_ref().map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      });
      let resolved_min_w = style.min_width.as_ref().map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      });
      let selector = child
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anonymous>".to_string());
      eprintln!(
                "[replaced-wide] child_id={:?} selector={} used_w={:.1} used_h={:.1} containing_w={:.1} max_w={:?} min_w={:?}",
                child.id,
                selector,
                used_size.width,
                used_size.height,
                containing_width,
                resolved_max_w,
                resolved_min_w
            );
    }

    // Vertical margins collapse as normal blocks
    let margin_top = style
      .margin_top
      .as_ref()
      .map(|len| {
        resolve_length_for_width(
          *len,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      })
      .unwrap_or(0.0);
    let margin_bottom = style
      .margin_bottom
      .as_ref()
      .map(|len| {
        resolve_length_for_width(
          *len,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      })
      .unwrap_or(0.0);

    // Resolve padding and borders
    let mut padding_top = resolve_length_for_width(
      style.padding_top,
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let mut padding_bottom = resolve_length_for_width(
      style.padding_bottom,
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let reserve_horizontal_gutter = matches!(style.overflow_x, Overflow::Scroll)
      || (style.scrollbar_gutter.stable
        && matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll));
    if reserve_horizontal_gutter {
      let gutter = resolve_scrollbar_width(style);
      if style.scrollbar_gutter.both_edges {
        padding_top += gutter;
      }
      padding_bottom += gutter;
    }

    let border_top = resolve_length_for_width(
      style.used_border_top_width(),
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let border_bottom = resolve_length_for_width(
      style.used_border_bottom_width(),
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );

    // Use the resolved replaced width when computing horizontal metrics
    let mut width_style = (*style).as_ref().clone();
    width_style.width = Some(Length::px(used_size.width));
    width_style.width_keyword = None;
    width_style.box_sizing = crate::style::types::BoxSizing::ContentBox;
    let inline_sides = inline_axis_sides(style);
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    let computed_width = compute_block_width(
      &width_style,
      containing_width,
      self.viewport_size,
      inline_sides,
      inline_positive,
    );

    let box_width = computed_width.border_box_width();
    let box_height = border_top + padding_top + used_size.height + padding_bottom + border_bottom;
    let bounds = Rect::from_xywh(computed_width.margin_left, box_y, box_width, box_height);

    let mut fragment = FragmentNode::new_with_style(
      bounds,
      FragmentContent::Replaced {
        replaced_type: replaced_box.replaced_type.clone(),
        box_id: Some(child.id),
      },
      vec![],
      child.style.clone(),
    );
    fragment.block_metadata = Some(BlockFragmentMetadata {
      margin_top,
      margin_bottom,
      ..BlockFragmentMetadata::default()
    });

    Ok(fragment)
  }

  fn collapsed_block_margins(
    &self,
    node: &BoxNode,
    containing_width: f32,
    is_root: bool,
  ) -> CollapsedBlockMargins {
    let style = &node.style;
    let block_sides = block_axis_sides(style);
    let margin_top = resolve_margin_side(
      style,
      block_sides.0,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let margin_bottom = resolve_margin_side(
      style,
      block_sides.1,
      containing_width,
      &self.font_context,
      self.viewport_size,
    );
    let mut top = CollapsibleMargin::from_margin(margin_top);
    let mut bottom = CollapsibleMargin::from_margin(margin_bottom);

    // Root element margins never collapse with children (CSS 2.1 §8.3.1).
    let mut collapse_first = !is_root && should_collapse_with_first_child(style);
    let mut collapse_last = !is_root && should_collapse_with_last_child(style);

    let is_ignorable_whitespace = |child: &BoxNode| -> bool {
      matches!(&child.box_type, BoxType::Text(text_box)
        if text_box.text.trim().is_empty()
          && !matches!(
            child.style.white_space,
            crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
          ))
    };

    let is_out_of_flow_or_float = |child: &BoxNode| -> bool {
      child.style.running_position.is_some()
        || matches!(child.style.position, Position::Absolute | Position::Fixed)
        || child.style.float.is_floating()
    };

    let is_in_flow_block = |child: &BoxNode| -> bool {
      if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
        return false;
      }
      child.is_block_level()
        || matches!(child.box_type, BoxType::Replaced(_) if !child.style.display.is_inline_level())
    };

    // If the first/last in-flow content is not a block-level box (i.e., line boxes would be
    // generated first/last), parent/child margin collapsing cannot occur.
    if collapse_first {
      if let Some(first) = node
        .children
        .iter()
        .find(|c| !is_out_of_flow_or_float(c) && !is_ignorable_whitespace(c))
      {
        if !is_in_flow_block(first) {
          collapse_first = false;
        }
      }
    }
    if collapse_last {
      if let Some(last) = node
        .children
        .iter()
        .rev()
        .find(|c| !is_out_of_flow_or_float(c) && !is_ignorable_whitespace(c))
      {
        if !is_in_flow_block(last) {
          collapse_last = false;
        }
      }
    }

    if collapse_first {
      let mut chain = CollapsibleMargin::ZERO;
      for child in &node.children {
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if !is_in_flow_block(child) {
          break;
        }
        let child_margins = self.collapsed_block_margins(child, containing_width, false);
        if child_margins.collapsible_through {
          chain = chain.collapse_with(child_margins.top.collapse_with(child_margins.bottom));
          continue;
        }
        chain = chain.collapse_with(child_margins.top);
        break;
      }
      top = top.collapse_with(chain);
    }

    if collapse_last {
      let mut chain = CollapsibleMargin::ZERO;
      for child in node.children.iter().rev() {
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if !is_in_flow_block(child) {
          break;
        }
        let child_margins = self.collapsed_block_margins(child, containing_width, false);
        if child_margins.collapsible_through {
          chain = chain.collapse_with(child_margins.top.collapse_with(child_margins.bottom));
          continue;
        }
        chain = chain.collapse_with(child_margins.bottom);
        break;
      }
      bottom = bottom.collapse_with(chain);
    }

    // Collapsing through an empty block (CSS 2.1 §8.3.1).
    let mut has_in_flow_content = matches!(node.box_type, BoxType::Replaced(_));
    if !has_in_flow_content {
      for child in &node.children {
        if is_out_of_flow_or_float(child) || is_ignorable_whitespace(child) {
          continue;
        }
        if is_in_flow_block(child) {
          let child_margins = self.collapsed_block_margins(child, containing_width, false);
          if !child_margins.collapsible_through {
            has_in_flow_content = true;
            break;
          }
          continue;
        }
        // Inline-level in-flow content generates line boxes and prevents collapsing-through.
        has_in_flow_content = true;
        break;
      }
    }

    let collapsible_through = !has_in_flow_content && is_margin_collapsible_through(style);
    if collapsible_through {
      let combined = top.collapse_with(bottom);
      top = combined;
      bottom = combined;
    }

    CollapsedBlockMargins {
      top,
      bottom,
      collapsible_through,
    }
  }

  /// Lays out all children of a box
  #[allow(clippy::cognitive_complexity)]
  fn layout_children(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError> {
    self.layout_children_with_external_floats(
      parent,
      constraints,
      nearest_positioned_cb,
      nearest_fixed_cb,
      paint_viewport,
      None,
      0.0,
    )
  }

  #[allow(clippy::cognitive_complexity)]
  fn layout_children_with_external_floats(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
    mut external_float_ctx: Option<&mut FloatContext>,
    external_float_base_y: f32,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>), LayoutError> {
    let mut deadline_counter = 0usize;
    let toggles = crate::debug::runtime::runtime_toggles();
    let inline_is_horizontal = inline_axis_is_horizontal(parent.style.writing_mode);
    let inline_space = if inline_is_horizontal {
      constraints.available_width
    } else {
      constraints.available_height
    };
    let block_space = if inline_is_horizontal {
      constraints.available_height
    } else {
      constraints.available_width
    };
    let inline_percentage_base = match inline_space {
      AvailableSpace::Definite(_) => {
        // Child percentage sizes resolve against the parent’s used inline size (its content box),
        // not the parent’s containing block (which can differ for flex/grid items where we stash
        // the containing block inline size in `inline_percentage_base`).
        let base = if inline_is_horizontal {
          constraints.width()
        } else {
          constraints.height()
        };
        let viewport_inline = if inline_is_horizontal {
          self.viewport_size.width
        } else {
          self.viewport_size.height
        };
        base.unwrap_or(viewport_inline)
      }
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite => {
        constraints.inline_percentage_base.unwrap_or(0.0)
      }
    };
    let dump_cell_child_y = toggles.truthy("FASTR_DUMP_CELL_CHILD_Y");
    let mut fragments = Vec::new();
    let mut current_y: f32 = 0.0;
    // Floats are positioned using the margin-edge cursor, but must not consume the pending
    // collapsed margin chain between in-flow siblings (CSS 2.1 §8.3.1). Keep a separate cursor
    // for float placement so floats can be laid out at the correct Y without advancing the
    // in-flow stacking position.
    let mut float_cursor_y: f32 = 0.0;
    let mut content_height: f32 = 0.0;
    let mut margin_ctx = MarginCollapseContext::new();
    let mut inline_buffer: Vec<BoxNode> = Vec::new();
    let mut positioned_children: Vec<PositionedCandidate> = Vec::new();
    // Root element margins never collapse with their children (CSS 2.1 §8.3.1).
    let parent_is_root = parent.id == 1;
    let mut collapse_with_parent_top =
      !parent_is_root && should_collapse_with_first_child(&parent.style);
    let establishes_absolute_cb = parent.style.establishes_abs_containing_block();
    let establishes_fixed_cb = parent.style.establishes_fixed_containing_block();
    if !collapse_with_parent_top {
      margin_ctx.mark_content_encountered();
    }
    static TRACE_ENV_RAW_LOGGED: OnceLock<bool> = OnceLock::new();
    if let Some(val) = toggles.get("FASTR_TRACE_BOXES") {
      TRACE_ENV_RAW_LOGGED.get_or_init(|| {
        eprintln!("[trace-box-env-raw] {}", val);
        true
      });
    }
    let trace_boxes = toggles.usize_list("FASTR_TRACE_BOXES").unwrap_or_default();
    static TRACE_BOXES_LOGGED: OnceLock<bool> = OnceLock::new();
    if !trace_boxes.is_empty() {
      TRACE_BOXES_LOGGED.get_or_init(|| {
        eprintln!("[trace-box-env] ids={:?}", trace_boxes);
        true
      });
    }
    let progress_ms = toggles
      .usize("FASTR_LOG_BLOCK_PROGRESS_MS")
      .map(|v| v as u128)
      .unwrap_or(0);
    let progress_ids = toggles.usize_list("FASTR_LOG_BLOCK_PROGRESS_IDS");
    let progress_match = toggles.string_list("FASTR_LOG_BLOCK_PROGRESS_MATCH");
    let filters_set = progress_ids.is_some() || progress_match.is_some();
    let passes_filters = |node: &BoxNode| -> bool {
      let id_ok = progress_ids
        .as_ref()
        .map(|ids| ids.contains(&node.id))
        .unwrap_or(false);
      let match_ok = progress_match
        .as_ref()
        .map(|subs| {
          subs.iter().any(|sub| {
            node
              .debug_info
              .as_ref()
              .map(|d| d.to_selector().contains(sub))
              .unwrap_or(false)
          })
        })
        .unwrap_or(false);
      if !filters_set {
        true
      } else {
        id_ok || match_ok
      }
    };
    let should_log_progress = progress_ms > 0 && passes_filters(parent);
    let progress_ms = if should_log_progress { progress_ms } else { 0 };
    let progress_max = if progress_ms > 0 {
      toggles.usize("FASTR_LOG_BLOCK_PROGRESS_MAX").unwrap_or(10) as u32
    } else {
      0
    };
    static TOTAL_COUNT: OnceLock<std::sync::atomic::AtomicU32> = OnceLock::new();
    let total_cap = if should_log_progress {
      toggles
        .usize("FASTR_LOG_BLOCK_PROGRESS_TOTAL_MAX")
        .map(|v| v as u32)
        .or(Some(50))
    } else {
      None
    };
    let total_counter = TOTAL_COUNT.get_or_init(|| std::sync::atomic::AtomicU32::new(0));

    let within_total_cap = total_cap
      .map(|cap| total_counter.load(std::sync::atomic::Ordering::Relaxed) < cap)
      .unwrap_or(true);

    if progress_ms > 0 && within_total_cap {
      eprintln!(
        "[block-progress-start] parent_id={} children={} threshold_ms={}",
        parent.id,
        parent.children.len(),
        progress_ms
      );
      if total_cap.is_some() {
        total_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      }
    }
    let parent_selector = if progress_ms > 0 {
      parent
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anon>".to_string())
    } else {
      String::new()
    };
    let progress_start = Instant::now();
    let mut progress_last = if progress_ms > 0 {
      let clamped_ms = progress_ms.min(u128::from(u64::MAX)) as u64;
      progress_start
        .checked_sub(std::time::Duration::from_millis(clamped_ms))
        .unwrap_or(progress_start)
    } else {
      progress_start
    };
    let mut progress_count: u32 = 0;
    let mut progress_capped = false;

    // Get containing width from constraints, but guard against collapsed/indefinite widths that
    // would zero out percentage sizing for descendants. Mirror the root-width fallback used in
    // `layout` so children still see a usable containing block when the parent was laid out with
    // a near-zero available width (common when flex measurement feeds 0px constraints). When the
    // available inline size is intrinsic/indefinite (min-/max-content probes), avoid inflating
    // the base to the viewport — leave it at 0 unless the caller provided a definite percentage
    // base.
    let intrinsic_width = matches!(
      inline_space,
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite
    );
    let inline_viewport = if inline_is_horizontal {
      self.viewport_size.width
    } else {
      self.viewport_size.height
    };
    let mut containing_width = inline_percentage_base;
    if !intrinsic_width && containing_width <= 1.0 {
      let width_is_absolute = parent
        .style
        .width
        .as_ref()
        .map(|l| l.unit.is_absolute())
        .unwrap_or(false);
      if !width_is_absolute {
        containing_width = inline_viewport;
      }
    }
    let has_external_float_ctx = external_float_ctx.is_some();
    let owns_float_ctx = !has_external_float_ctx || establishes_bfc(&parent.style);
    let mut local_float_ctx = FloatContext::new(containing_width);
    let float_base_y = if owns_float_ctx {
      0.0
    } else {
      external_float_base_y
    };
    let float_ctx: &mut FloatContext = if owns_float_ctx {
      &mut local_float_ctx
    } else {
      external_float_ctx
        .as_deref_mut()
        .unwrap_or(&mut local_float_ctx)
    };
    let available_height = block_space;
    let relative_cb = ContainingBlock::with_viewport(
      Rect::new(
        Point::ZERO,
        Size::new(containing_width, block_space.to_option().unwrap_or(0.0)),
      ),
      self.viewport_size,
    );
    // Check for border/padding that prevents margin collapse with first child
    let parent_has_top_separation = resolve_length_for_width(
      parent.style.used_border_top_width(),
      containing_width,
      &parent.style,
      &self.font_context,
      self.viewport_size,
    ) > 0.0
      || resolve_length_for_width(
        parent.style.padding_top,
        containing_width,
        &parent.style,
        &self.font_context,
        self.viewport_size,
      ) > 0.0;

    if parent_has_top_separation {
      collapse_with_parent_top = false;
      margin_ctx.mark_content_encountered();
    }

    let trace_positioned = trace_positioned_ids();
    let trace_block_text = trace_block_text_ids();
    if !has_external_float_ctx {
      if let Some(result) = self.try_parallel_block_children(
        parent,
        constraints,
        nearest_positioned_cb,
        nearest_fixed_cb,
        margin_ctx.clone(),
        &relative_cb,
        containing_width,
        float_ctx.is_empty(),
        paint_viewport,
      ) {
        return result;
      }
    }

    let inline_fc_owned =
      if *nearest_positioned_cb == self.nearest_positioned_cb && *nearest_fixed_cb == self.nearest_fixed_cb
      {
        None
      } else {
        Some(InlineFormattingContext::with_factory(
          self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb),
        ))
      };
    let inline_fc = inline_fc_owned
      .as_ref()
      .unwrap_or_else(|| self.intrinsic_inline_fc.as_ref());

    let layout_in_flow_block_child =
      |child: &BoxNode,
       margin_ctx: &mut MarginCollapseContext,
       current_y: f32,
       float_ctx_ref: &mut FloatContext|
       -> Result<(FragmentNode, f32), LayoutError> {
        let child_margins = self.collapsed_block_margins(child, containing_width, false);
        let pending_margin = margin_ctx.pending_collapsible_margin();
        let margin_edge_y = current_y + pending_margin.resolve();
        let cleared_margin_edge_y =
          float_ctx_ref.compute_clearance(float_base_y + margin_edge_y, child.style.clear)
            - float_base_y;
        let clearance = (cleared_margin_edge_y - margin_edge_y).max(0.0);

        let box_y = if clearance > 0.0 {
          // Clearance is added above the top margin edge and breaks margin adjoining.
          margin_ctx.mark_content_encountered();
          let (offset, _) = margin_ctx.process_child_with_clearance(
            clearance,
            child_margins.top,
            child_margins.bottom,
            child_margins.collapsible_through,
          );
          current_y + offset
        } else if collapse_with_parent_top && margin_ctx.is_at_start() && !child_margins.collapsible_through
        {
          // Parent/first-child margin collapsing is represented by the parent's own collapsed
          // margins. Discard any leading collapsible-through margins and place the first
          // non-empty child at the block start.
          margin_ctx.consume_pending();
          margin_ctx.push_collapsible_margin(child_margins.bottom);
          current_y
        } else {
          let (offset, _) = margin_ctx.process_child_margins(
            child_margins.top,
            child_margins.bottom,
            child_margins.collapsible_through,
          );
          current_y + offset
        };

        let fragment = self.layout_block_child(
          parent,
          child,
          containing_width,
          constraints,
          box_y,
          nearest_positioned_cb,
          nearest_fixed_cb,
          Some(&mut *float_ctx_ref),
          float_base_y,
          paint_viewport,
        )?;
        let next_y = box_y + fragment.bounds.height();
        Ok((fragment, next_y))
      };

    let flush_inline_buffer = |buffer: &mut Vec<BoxNode>,
                               fragments: &mut Vec<FragmentNode>,
                               current_y: &mut f32,
                               content_height: &mut f32,
                               margin_ctx: &mut MarginCollapseContext,
                               float_ctx_ref: &mut FloatContext,
                               deadline_counter: &mut usize|
     -> Result<(), LayoutError> {
      if buffer.is_empty() {
        return Ok(());
      }

      // If the buffer contains any block-level boxes (or only collapsible whitespace),
      // lay each out separately to avoid creating an inline formatting context that spans
      // mixed block content or empty lines.
      let has_block = buffer.iter().any(|b| b.is_block_level());
      let all_whitespace = buffer.iter().all(|b| match &b.box_type {
        BoxType::Text(text) => text.text.trim().is_empty(),
        _ => false,
      });
      if has_block || all_whitespace {
        for child in buffer.drain(..) {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(deadline_counter, 16, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }
          let treated_as_block = child.is_block_level()
            || matches!(
              child.box_type,
              BoxType::Replaced(_) if !child.style.display.is_inline_level()
            );

          let (fragment, next_y) = if treated_as_block {
            layout_in_flow_block_child(&child, margin_ctx, *current_y, float_ctx_ref)?
          } else {
            let pending_margin = margin_ctx.consume_pending();
            *current_y += pending_margin;
            let box_y = *current_y;
            let fragment = self.layout_block_child(
              parent,
              &child,
              containing_width,
              constraints,
              box_y,
              nearest_positioned_cb,
              nearest_fixed_cb,
              Some(&mut *float_ctx_ref),
              float_base_y,
              paint_viewport,
            )?;
            let next_y = box_y + fragment.bounds.height();
            (fragment, next_y)
          };
          *content_height = content_height.max(fragment.bounds.max_y());
          *current_y = next_y;
          let mut fragment = fragment;
          if child.style.position.is_relative() {
            let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
              &child.style,
              &relative_cb,
              self.viewport_size,
              &self.font_context,
            );
            fragment = PositionedLayout::with_font_context(self.font_context.clone())
              .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
          }
          fragments.push(fragment);
        }
        return Ok(());
      }

      // Apply any pending collapsed margin before inline content
      let pending_margin = margin_ctx.consume_pending();
      *current_y += pending_margin;

      let mut inline_container = BoxNode::new_inline(parent.style.clone(), std::mem::take(buffer));
      // If the inline container would start below the current cursor because of pending
      // margins, advance to that baseline first.
      let inline_y = *current_y;
      let inline_constraints = if inline_is_horizontal {
        LayoutConstraints::new(AvailableSpace::Definite(containing_width), available_height)
      } else {
        LayoutConstraints::new(available_height, AvailableSpace::Definite(containing_width))
      };
      let mut inline_fragment = match inline_fc.layout_with_floats(
        &inline_container,
        &inline_constraints,
        Some(&mut *float_ctx_ref),
        float_base_y + inline_y,
      ) {
        Ok(fragment) => fragment,
        Err(err) => {
          *buffer = std::mem::take(&mut inline_container.children);
          return Err(err);
        }
      };

      inline_fragment.bounds = Rect::from_xywh(
        0.0,
        inline_y,
        inline_fragment.bounds.width(),
        inline_fragment.bounds.height(),
      );

      *content_height = content_height.max(inline_fragment.bounds.max_y());
      *current_y += inline_fragment.bounds.height();
      fragments.push(inline_fragment);
      *buffer = std::mem::take(&mut inline_container.children);
      buffer.clear();
      Ok(())
    };

    for (child_idx, child) in parent.children.iter().enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      if progress_ms > 0 {
        if let Some(cap) = total_cap {
          let current = total_counter.load(std::sync::atomic::Ordering::Relaxed);
          if current >= cap {
            continue;
          }
        }
        if progress_count < progress_max || progress_max == 0 {
          let now = Instant::now();
          if now.duration_since(progress_last).as_millis() >= progress_ms {
            let child_selector = child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".to_string());
            eprintln!(
                            "[block-progress] parent_id={} child={}/{} elapsed_ms={} selector={} child_selector={}",
                            parent.id,
                            child_idx,
                            parent.children.len(),
                            now.duration_since(progress_start).as_millis(),
                            parent_selector,
                            child_selector
                        );
            progress_last = now;
            if progress_max > 0 {
              progress_count += 1;
            }
            if total_cap.is_some() {
              total_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
          }
        } else if !progress_capped {
          eprintln!(
            "[block-progress-cap] parent_id={} selector={} max_logs={}",
            parent.id, parent_selector, progress_max
          );
          progress_capped = true;
        }
      }
      // Skip collapsible whitespace text in block formatting contexts (CSS 2.1 §16.6).
      if let BoxType::Text(text_box) = &child.box_type {
        if text_box.text.trim().is_empty()
          && !matches!(
            child.style.white_space,
            crate::style::types::WhiteSpace::Pre | crate::style::types::WhiteSpace::PreWrap
          )
        {
          if trace_positioned.contains(&child.id) || trace_block_text.contains(&child.id) {
            eprintln!(
              "[block-text-skip] id={} selector={:?} raw={:?}",
              child.id,
              child.debug_info.as_ref().map(|d| d.to_selector()),
              text_box.text
            );
          }
          continue;
        }
        if trace_block_text.contains(&child.id) {
          eprintln!(
            "[block-text] id={} selector={:?} text={:?} white_space={:?}",
            child.id,
            child.debug_info.as_ref().map(|d| d.to_selector()),
            text_box.text,
            child.style.white_space
          );
        }
      }

      let running_name = if matches!(
        child.box_type,
        BoxType::Block(_) | BoxType::Inline(_) | BoxType::Replaced(_)
      ) {
        child.style.running_position.as_ref()
      } else {
        None
      };

      if let Some(running_name) = running_name {
        let pending_margin = margin_ctx.pending_margin();
        // Running elements are positioned based on the hypothetical in-flow position. Make sure we
        // resolve intrinsic sizing keywords (`min-content`, `max-content`, `fit-content(...)`) the
        // same way as normal-flow blocks so the anchor point lines up with the rendered box.
        let inline_sides = inline_axis_sides(&child.style);
        let inline_positive = inline_axis_positive(child.style.writing_mode, child.style.direction);
        let mut hypo_width = compute_block_width(
          &child.style,
          containing_width,
          self.viewport_size,
          inline_sides,
          inline_positive,
        );
        let width_auto = child.style.width.is_none() && child.style.width_keyword.is_none();
        let inline_edges_for_fit = hypo_width.border_left
          + hypo_width.padding_left
          + hypo_width.padding_right
          + hypo_width.border_right;
        let available_inline_border_box = (containing_width
          - resolve_margin_side(
            &child.style,
            inline_sides.0,
            containing_width,
            &self.font_context,
            self.viewport_size,
          )
          - resolve_margin_side(
            &child.style,
            inline_sides.1,
            containing_width,
            &self.font_context,
            self.viewport_size,
          ))
        .max(0.0);
        let available_content_for_fit =
          (available_inline_border_box - inline_edges_for_fit).max(0.0);
        let mut intrinsic_content_sizes = None;
        if child.style.width.is_none() && child.style.width_keyword.is_some() {
          let keyword = child.style.width_keyword.unwrap();
          let factory = self.child_factory_for_cb(*nearest_positioned_cb);
          let fc_type = child.formatting_context().unwrap_or_else(|| {
            if child.is_block_level() {
              FormattingContextType::Block
            } else {
              FormattingContextType::Inline
            }
          });
          let (min_content, max_content) =
            self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?;
          intrinsic_content_sizes = Some((min_content, max_content));
          let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            inline_edges_for_fit,
          );
          let specified_width = match child.style.box_sizing {
            crate::style::types::BoxSizing::ContentBox => keyword_content,
            crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges_for_fit,
          };
          let mut width_style = child.style.as_ref().clone();
          width_style.width = Some(Length::px(specified_width));
          width_style.width_keyword = None;
          hypo_width = compute_block_width(
            &width_style,
            containing_width,
            self.viewport_size,
            inline_sides,
            inline_positive,
          );
        }

        // Apply min/max sizing constraints after resolving intrinsic keywords (CSS 2.1 §10.4),
        // mirroring `layout_block_child` so auto margins stay consistent.
        let horizontal_edges = hypo_width.border_left
          + hypo_width.padding_left
          + hypo_width.padding_right
          + hypo_width.border_right;
        let min_width = if let Some(keyword) = child.style.min_width_keyword {
          if intrinsic_content_sizes.is_none() {
            let factory = self.child_factory_for_cb(*nearest_positioned_cb);
            let fc_type = child.formatting_context().unwrap_or_else(|| {
              if child.is_block_level() {
                FormattingContextType::Block
              } else {
                FormattingContextType::Inline
              }
            });
            intrinsic_content_sizes = Some(
              self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
            );
          }
          let (min_content, max_content) = intrinsic_content_sizes.unwrap();
          self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            horizontal_edges,
          )
        } else {
          child
            .style
            .min_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                containing_width,
                &child.style,
                &self.font_context,
                self.viewport_size,
              )
            })
            .map(|w| content_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(0.0)
        };
        let max_width = if let Some(keyword) = child.style.max_width_keyword {
          if intrinsic_content_sizes.is_none() {
            let factory = self.child_factory_for_cb(*nearest_positioned_cb);
            let fc_type = child.formatting_context().unwrap_or_else(|| {
              if child.is_block_level() {
                FormattingContextType::Block
              } else {
                FormattingContextType::Inline
              }
            });
            intrinsic_content_sizes = Some(
              self.intrinsic_inline_content_sizes_for_sizing_keywords(child, fc_type, &factory)?,
            );
          }
          let (min_content, max_content) = intrinsic_content_sizes.unwrap();
          self.resolve_intrinsic_size_keyword_to_content_width(
            keyword,
            min_content,
            max_content,
            available_content_for_fit,
            containing_width,
            &child.style,
            horizontal_edges,
          )
        } else {
          child
            .style
            .max_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                containing_width,
                &child.style,
                &self.font_context,
                self.viewport_size,
              )
            })
            .map(|w| content_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(f32::INFINITY)
        };
        let max_width = if max_width.is_finite() && max_width < min_width {
          min_width
        } else {
          max_width
        };
        let clamped_content_width =
          crate::layout::utils::clamp_with_order(hypo_width.content_width, min_width, max_width);
        if clamped_content_width != hypo_width.content_width {
          let (margin_left, margin_right) = recompute_margins_for_width(
            &child.style,
            containing_width,
            clamped_content_width,
            hypo_width.border_left,
            hypo_width.padding_left,
            hypo_width.padding_right,
            hypo_width.border_right,
            self.viewport_size,
            &self.font_context,
          );
          hypo_width.content_width = clamped_content_width;
          hypo_width.margin_left = margin_left;
          hypo_width.margin_right = margin_right;
        }

        // If we're being asked to use a border-box width override, treat the inline size as auto
        // so the constraint equation can resolve the new width + margins.
        if width_auto {
          if let Some(used_border_box) = constraints.used_border_box_width {
            let used_content = (used_border_box - horizontal_edges).max(0.0);
            let (margin_left, margin_right) = recompute_margins_for_width(
              &child.style,
              containing_width,
              used_content,
              hypo_width.border_left,
              hypo_width.padding_left,
              hypo_width.padding_right,
              hypo_width.border_right,
              self.viewport_size,
              &self.font_context,
            );
            hypo_width.content_width = used_content;
            hypo_width.margin_left = margin_left;
            hypo_width.margin_right = margin_right;
          }
        }
        let static_x = hypo_width.margin_left;
        let static_y = current_y + pending_margin;

        let mut snapshot_node = child.clone();
        let mut snapshot_style = snapshot_node.style.as_ref().clone();
        snapshot_style.running_position = None;
        snapshot_style.position = Position::Static;
        snapshot_node.style = Arc::new(snapshot_style);
        crate::layout::running_elements::clear_running_position_in_box_tree(&mut snapshot_node);

        let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
        let fc_type = snapshot_node.formatting_context().unwrap_or_else(|| {
          if snapshot_node.is_block_level() {
            FormattingContextType::Block
          } else {
            FormattingContextType::Inline
          }
        });
        let fc = factory.get(fc_type);
        let snapshot_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(containing_width),
          AvailableSpace::Indefinite,
        );
        let snapshot_fragment = fc.layout(&snapshot_node, &snapshot_constraints)?;
        let anchor_bounds = Rect::from_xywh(static_x, static_y, 0.0, 0.01);
        let mut anchor =
          FragmentNode::new_running_anchor(anchor_bounds, running_name.clone(), snapshot_fragment);
        anchor.style = Some(child.style.clone());
        fragments.push(anchor);
        continue;
      }

      // Skip out-of-flow positioned boxes (absolute/fixed)
      if is_out_of_flow(child) {
        if trace_positioned.contains(&child.id) {
          eprintln!(
            "[block-positioned] parent_id={} child_id={} selector={} pos={:?}",
            parent.id,
            child.id,
            child
              .debug_info
              .as_ref()
              .map(|d| d.to_selector())
              .unwrap_or_else(|| "<anon>".into()),
            child.style.position
          );
        }
        // Static position is defined in terms of the hypothetical in-flow margin edge, which for
        // block-level siblings must respect vertical margin collapsing. Because absolutely/fixed
        // positioned boxes do not participate in margin collapse with surrounding flow content, we
        // must compute the collapsed block-start margin without mutating the `MarginCollapseContext`.
        let pending_margin = margin_ctx.pending_collapsible_margin();
        let block_sides = block_axis_sides(&child.style);
        let margin_top = resolve_margin_side(
          &child.style,
          block_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
        );
        let collapsed_margin = pending_margin
          .collapse_with(CollapsibleMargin::from_margin(margin_top))
          .resolve();
        // Static position is based on the hypothetical in-flow margin edge. For normal blocks, the
        // margin edge is aligned to the containing block start, so the inline coordinate is 0 and
        // the absolute positioning constraint equation will apply the actual margin.
        let static_x = 0.0;
        // `AbsoluteLayout` applies the element's margin-top as part of the constraint equation, so
        // the static position must be recorded at the (collapsed) margin edge rather than the
        // border edge.
        let static_y = current_y + collapsed_margin - margin_top;
        let static_position = Some(Point::new(static_x, static_y));
        let source = match child.style.position {
          Position::Fixed => {
            if establishes_fixed_cb {
              ContainingBlockSource::ParentPadding
            } else {
              ContainingBlockSource::Explicit(*nearest_fixed_cb)
            }
          }
          Position::Absolute => {
            if establishes_absolute_cb {
              ContainingBlockSource::ParentPadding
            } else {
              ContainingBlockSource::Explicit(*nearest_positioned_cb)
            }
          }
          _ => ContainingBlockSource::Explicit(*nearest_positioned_cb),
        };
        positioned_children.push(PositionedCandidate {
          node: child.clone(),
          source,
          static_position,
          query_parent_id: parent.id,
        });
        continue;
      }

      // Floats are taken out of flow but still participate in this BFC's float context
      if child.style.float.is_floating()
        && !matches!(child.style.position, Position::Absolute | Position::Fixed)
      {
        flush_inline_buffer(
          &mut inline_buffer,
          &mut fragments,
          &mut current_y,
          &mut content_height,
          &mut margin_ctx,
          float_ctx,
          &mut deadline_counter,
        )?;

        // Floats are out-of-flow: their own margins never collapse, but they also must not break
        // the sibling margin collapsing chain between in-flow blocks. Position floats relative to
        // the current in-flow cursor plus the pending collapsed margin without consuming it.
        let pending_margin = margin_ctx.pending_margin();
        let float_base_y_local = current_y + pending_margin;
        float_cursor_y = float_cursor_y.max(float_base_y_local);

        // Honor clearance against existing floats for this float's placement only.
        float_cursor_y = float_ctx
          .compute_clearance(float_base_y + float_cursor_y, child.style.clear)
          - float_base_y;

        let percentage_base = containing_width;
        let margin_left = child
          .style
          .margin_left
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
            )
          })
          .unwrap_or(0.0);
        let margin_right = child
          .style
          .margin_right
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
            )
          })
          .unwrap_or(0.0);
        let horizontal_edges = horizontal_padding_and_borders(
          &child.style,
          percentage_base,
          self.viewport_size,
          &self.font_context,
        );

        // CSS 2.1 shrink-to-fit formula for floats
        let factory = self.child_factory_for_cbs(*nearest_positioned_cb, *nearest_fixed_cb);
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = factory.get(fc_type);
        let (preferred_min_content, preferred_content) =
          match fc.compute_intrinsic_inline_sizes(child) {
            Ok(values) => values,
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {
              // Preserve legacy semantics for non-timeout intrinsic sizing failures: treat
              // the min-content width as 0 but still attempt the max-content measurement.
              let preferred_content =
                match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent) {
                  Ok(value) => value,
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => 0.0,
                };
              (0.0, preferred_content)
            }
          };

        let edges_base0 =
          horizontal_padding_and_borders(&child.style, 0.0, self.viewport_size, &self.font_context);
        let intrinsic_min =
          rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, horizontal_edges);
        let intrinsic_max =
          rebase_intrinsic_border_box_size(preferred_content, edges_base0, horizontal_edges);

        let (_, float_available_width) =
          float_ctx.available_width_at_y(float_base_y + float_cursor_y);
        let available = (float_available_width - margin_left - margin_right).max(0.0);

        let specified_width = child
          .style
          .width
          .as_ref()
          .map(|l| {
            resolve_length_for_width(
              *l,
              percentage_base,
              &child.style,
              &self.font_context,
              self.viewport_size,
            )
          })
          .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
          .or_else(|| {
            child.style.width_keyword.map(|keyword| match keyword {
              crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
              crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
              crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
              crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
                if let Some(limit) = limit {
                  let resolved = resolve_length_for_width(
                    limit,
                    percentage_base,
                    &child.style,
                    &self.font_context,
                    self.viewport_size,
                  );
                  let resolved_border =
                    border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                  intrinsic_max.min(intrinsic_min.max(resolved_border))
                } else {
                  intrinsic_max.min(available.max(intrinsic_min))
                }
              }
            })
          });

        let min_width = if let Some(keyword) = child.style.min_width_keyword {
          match keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              if let Some(limit) = limit {
                let resolved = resolve_length_for_width(
                  limit,
                  percentage_base,
                  &child.style,
                  &self.font_context,
                  self.viewport_size,
                );
                let resolved_border =
                  border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                intrinsic_max.min(intrinsic_min.max(resolved_border))
              } else {
                intrinsic_max.min(available.max(intrinsic_min))
              }
            }
          }
        } else {
          child
            .style
            .min_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                percentage_base,
                &child.style,
                &self.font_context,
                self.viewport_size,
              )
            })
            .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(0.0)
        };
        let max_width = if let Some(keyword) = child.style.max_width_keyword {
          match keyword {
            crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
            crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
            crate::style::types::IntrinsicSizeKeyword::FillAvailable => available,
            crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
              if let Some(limit) = limit {
                let resolved = resolve_length_for_width(
                  limit,
                  percentage_base,
                  &child.style,
                  &self.font_context,
                  self.viewport_size,
                );
                let resolved_border =
                  border_size_from_box_sizing(resolved, horizontal_edges, child.style.box_sizing);
                intrinsic_max.min(intrinsic_min.max(resolved_border))
              } else {
                intrinsic_max.min(available.max(intrinsic_min))
              }
            }
          }
        } else {
          child
            .style
            .max_width
            .as_ref()
            .map(|l| {
              resolve_length_for_width(
                *l,
                percentage_base,
                &child.style,
                &self.font_context,
                self.viewport_size,
              )
            })
            .map(|w| border_size_from_box_sizing(w, horizontal_edges, child.style.box_sizing))
            .unwrap_or(f32::INFINITY)
        };
        let max_width = if max_width.is_finite() && max_width < min_width {
          min_width
        } else {
          max_width
        };

        let used_border_box = if let Some(specified) = specified_width {
          crate::layout::utils::clamp_with_order(specified, min_width, max_width)
        } else {
          let shrink = intrinsic_max.min(available.max(intrinsic_min));
          crate::layout::utils::clamp_with_order(shrink, min_width, max_width)
        };

        // Layout the float's contents using the *containing block* width as the percentage base
        // (CSS 2.1 §8.3), while forcing the used border-box width we computed above for `width:auto`
        // shrink-to-fit (CSS 2.1 §10.3.5). Passing the used content width as the constraint would
        // incorrectly resolve percentage padding/borders against the float's own content box.
        let width_auto = specified_width.is_none();
        let child_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(containing_width),
          AvailableSpace::Indefinite,
        )
        .with_used_border_box_size(width_auto.then_some(used_border_box), None);
        let child_bfc = BlockFormattingContext::with_factory(factory.clone());
        let mut fragment = child_bfc.layout(child, &child_constraints)?;

        let block_sides = block_axis_sides(&child.style);
        let margin_top = resolve_margin_side(
          &child.style,
          block_sides.0,
          containing_width,
          &self.font_context,
          self.viewport_size,
        );
        let margin_bottom = resolve_margin_side(
          &child.style,
          block_sides.1,
          containing_width,
          &self.font_context,
          self.viewport_size,
        );
        let box_width = used_border_box;
        let float_height = margin_top + fragment.bounds.height() + margin_bottom;

        let side = match child.style.float {
          Float::Left => FloatSide::Left,
          Float::Right => FloatSide::Right,
          Float::None | Float::Footnote => unreachable!(),
        };

        let (fx, fy) = float_ctx.compute_float_position(
          side,
          margin_left + box_width + margin_right,
          float_height,
          float_base_y + float_cursor_y,
        );

        fragment.bounds = Rect::from_xywh(
          fx + margin_left,
          fy - float_base_y + margin_top,
          box_width,
          fragment.bounds.height(),
        );
        let border_box_height = fragment.bounds.height();
        let margin_box = Rect::from_xywh(
          fx,
          fy,
          margin_left + box_width + margin_right,
          margin_top + border_box_height + margin_bottom,
        );
        let border_box = Rect::from_xywh(
          fx + margin_left,
          fy + margin_top,
          box_width,
          border_box_height,
        );
        let containing_block_size =
          Size::new(containing_width, block_space.to_option().unwrap_or(0.0));
        let shape = build_float_shape(
          &child.style,
          margin_box,
          border_box,
          containing_block_size,
          self.viewport_size,
          &self.font_context,
          factory.image_cache(),
        )?;
        float_ctx.add_float_with_shape(
          side,
          fx,
          fy,
          margin_left + box_width + margin_right,
          float_height,
          shape,
        );
        if owns_float_ctx {
          content_height = content_height.max(fy + float_height - float_base_y);
        }
        if child.style.position.is_relative() {
          let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
            &child.style,
            &relative_cb,
            self.viewport_size,
            &self.font_context,
          );
          fragment = PositionedLayout::with_font_context(self.font_context.clone())
            .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
        }
        fragments.push(fragment);
        continue;
      }

      // Layout in-flow children
      let treated_as_block = child.is_block_level()
        || matches!(child.box_type, BoxType::Replaced(_) if !child.style.display.is_inline_level());

      if treated_as_block {
        flush_inline_buffer(
          &mut inline_buffer,
          &mut fragments,
          &mut current_y,
          &mut content_height,
          &mut margin_ctx,
          float_ctx,
          &mut deadline_counter,
        )?;

        if dump_cell_child_y && matches!(parent.style.display, Display::TableCell) {
          eprintln!(
            "cell child layout: parent_id={} child_idx={} child_display={:?} current_y={:.2}",
            parent.id, child_idx, child.style.display, current_y
          );
        }
        if !trace_boxes.is_empty() && trace_boxes.contains(&child.id) {
          eprintln!(
                    "[trace-box-pre] id={} display={:?} width={:?} min=({:?},{:?}) max=({:?},{:?}) margin=({:?},{:?})",
                    child.id,
                    child.style.display,
                    child.style.width,
                    child.style.min_width,
                    child.style.min_height,
                    child.style.max_width,
                    child.style.max_height,
                    child.style.margin_left,
                     child.style.margin_right,
          );
        }

        let (fragment, next_y) =
          layout_in_flow_block_child(child, &mut margin_ctx, current_y, float_ctx)?;

        if dump_cell_child_y && matches!(parent.style.display, Display::TableCell) {
          let b = fragment.bounds;
          eprintln!(
                        "cell child placed: parent_id={} child_id={} display={:?} current_y={:.2} frag=({:.2},{:.2},{:.2},{:.2}) next_y={:.2}",
                        parent.id,
                        child.id,
                        child.style.display,
                        current_y,
                        b.x(),
                        b.y(),
                        b.width(),
                        b.height(),
                        next_y
                    );
        }
        if !trace_boxes.is_empty() && trace_boxes.contains(&child.id) {
          eprintln!(
                        "[trace-box] id={} display={:?} width={:?} height={:?} min=({:?},{:?}) max=({:?},{:?}) at y={:.2} -> next_y={:.2}",
                        child.id,
                        child.style.display,
                        child.style.width,
                        child.style.height,
                        child.style.min_width,
                        child.style.min_height,
                        child.style.max_width,
                        child.style.max_height,
                        current_y,
                        next_y
                    );
        }

        content_height = content_height.max(fragment.bounds.max_y());
        current_y = next_y;
        let mut fragment = fragment;
        if child.style.position.is_relative() {
          let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
            &child.style,
            &relative_cb,
            self.viewport_size,
            &self.font_context,
          );
          fragment = PositionedLayout::with_font_context(self.font_context.clone())
            .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
        }
        fragments.push(fragment);
      } else {
        // Inline-level non-replaced elements should still respect block/inline splits:
        // if this inline itself establishes a block formatting context (e.g., display:block
        // on an inline ancestor), flush the buffer and lay it out as a block.
        if child.is_block_level() {
          flush_inline_buffer(
            &mut inline_buffer,
            &mut fragments,
            &mut current_y,
            &mut content_height,
            &mut margin_ctx,
            float_ctx,
            &mut deadline_counter,
          )?;
          let (fragment, next_y) =
            layout_in_flow_block_child(child, &mut margin_ctx, current_y, float_ctx)?;
          content_height = content_height.max(fragment.bounds.max_y());
          current_y = next_y;
          let mut fragment = fragment;
          if child.style.position.is_relative() {
            let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
              &child.style,
              &relative_cb,
              self.viewport_size,
              &self.font_context,
            );
            fragment = PositionedLayout::with_font_context(self.font_context.clone())
              .apply_relative_positioning(&fragment, &positioned_style, &relative_cb)?;
          }
          fragments.push(fragment);
        } else {
          inline_buffer.push(child.clone());
        }
      }
    }

    flush_inline_buffer(
      &mut inline_buffer,
      &mut fragments,
      &mut current_y,
      &mut content_height,
      &mut margin_ctx,
      float_ctx,
      &mut deadline_counter,
    )?;

    // Resolve any trailing margins
    let trailing_margin = margin_ctx.pending_margin();
    let allow_collapse_last = !parent_is_root && should_collapse_with_last_child(&parent.style);

    // Check for bottom separation
    let parent_has_bottom_separation = resolve_length_for_width(
      parent.style.used_border_bottom_width(),
      containing_width,
      &parent.style,
      &self.font_context,
      self.viewport_size,
    ) > 0.0
      || resolve_length_for_width(
        parent.style.padding_bottom,
        containing_width,
        &parent.style,
        &self.font_context,
        self.viewport_size,
      ) > 0.0;

    if !allow_collapse_last || parent_has_bottom_separation {
      // Trailing margins apply after the last in-flow cursor; avoid over-counting when earlier
      // siblings extend the maximum height (e.g. due to overlaps/negative margins).
      content_height = content_height.max(current_y + trailing_margin.max(0.0));
    }

    // Float boxes extend the formatting context height for BFC roots.
    let float_bottom = if owns_float_ctx {
      float_ctx
        .left_floats()
        .iter()
        .chain(float_ctx.right_floats())
        .map(|f| f.bottom())
        .fold(content_height, f32::max)
    } else {
      content_height
    };

    if let Some(err) = float_ctx.take_timeout_error() {
      return Err(err);
    }

    Ok((fragments, float_bottom, positioned_children))
  }

  fn is_multicol_container(style: &ComputedStyle) -> bool {
    style.column_count.unwrap_or(1) > 1 || style.column_width.is_some()
  }

  fn compute_column_geometry(
    &self,
    style: &ComputedStyle,
    available_inline: f32,
  ) -> (usize, f32, f32) {
    let available_inline = available_inline.max(0.0);
    let gap = resolve_length_for_width(
      style.column_gap,
      available_inline,
      style,
      &self.font_context,
      self.viewport_size,
    )
    .max(0.0);

    let specified_width = style.column_width.as_ref().and_then(|l| {
      let resolved = resolve_length_for_width(
        *l,
        available_inline,
        style,
        &self.font_context,
        self.viewport_size,
      );
      (resolved.is_finite() && resolved > 0.0).then_some(resolved)
    });
    let specified_count = style.column_count.unwrap_or(0) as usize;

    let compute_width = |count: usize| {
      let count = count.max(1) as f32;
      ((available_inline - gap * (count - 1.0)) / count).max(0.0)
    };

    if specified_count > 0 {
      if let Some(spec_width) = specified_width {
        let denom = spec_width + gap;
        let max_fit = if denom > 0.0 {
          ((available_inline + gap) / denom).floor() as usize
        } else {
          1
        };
        let count = specified_count.min(max_fit.max(1)).max(1);
        return (count, compute_width(count), gap);
      }

      let count = specified_count.max(1);
      return (count, compute_width(count), gap);
    }

    if let Some(spec_width) = specified_width {
      let denom = spec_width + gap;
      let count = if denom > 0.0 {
        ((available_inline + gap) / denom).floor().max(1.0) as usize
      } else {
        1
      };
      return (count, compute_width(count), gap);
    }

    (1, available_inline, gap)
  }

  fn set_logical_from_bounds(fragment: &mut FragmentNode) {
    fragment.logical_override = Some(fragment.bounds);
    for child in fragment.children_mut() {
      Self::set_logical_from_bounds(child);
    }
  }

  fn clone_with_children(parent: &BoxNode, children: Vec<BoxNode>) -> BoxNode {
    BoxNode {
      style: parent.style.clone(),
      starting_style: parent.starting_style.clone(),
      box_type: parent.box_type.clone(),
      children,
      footnote_body: parent.footnote_body.clone(),
      id: parent.id,
      generated_pseudo: parent.generated_pseudo,
      debug_info: parent.debug_info.clone(),
      styled_node_id: parent.styled_node_id,
      table_cell_span: parent.table_cell_span,
      table_column_span: parent.table_column_span,
      first_line_style: parent.first_line_style.clone(),
      first_letter_style: parent.first_letter_style.clone(),
    }
  }

  fn translate_with_logical(
    fragment: &mut FragmentNode,
    dx: f32,
    physical_dy: f32,
    logical_dy: f32,
  ) {
    fragment.bounds = Rect::from_xywh(
      fragment.bounds.x() + dx,
      fragment.bounds.y() + physical_dy,
      fragment.bounds.width(),
      fragment.bounds.height(),
    );
    if let Some(logical) = fragment.logical_override {
      fragment.logical_override = Some(Rect::from_xywh(
        logical.x() + dx,
        logical.y() + logical_dy,
        logical.width(),
        logical.height(),
      ));
    }
    for child in fragment.children_mut() {
      Self::translate_with_logical(child, dx, physical_dy, logical_dy);
    }
  }

  fn layout_column_segment(
    &self,
    parent: &BoxNode,
    children: &[BoxNode],
    column_count: usize,
    column_width: f32,
    column_gap: f32,
    available_height: AvailableSpace,
    column_fill: ColumnFill,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    paint_viewport: Rect,
  ) -> Result<(Vec<FragmentNode>, f32, Vec<PositionedCandidate>, f32), LayoutError> {
    let mut deadline_counter = 0usize;
    if children.is_empty() {
      return Ok((Vec::new(), 0.0, Vec::new(), 0.0));
    }

    if column_count <= 1 {
      let parent_clone = Self::clone_with_children(parent, children.to_vec());
      let (frags, height, positioned) = self.layout_children(
        &parent_clone,
        &LayoutConstraints::new(AvailableSpace::Definite(column_width), available_height),
        nearest_positioned_cb,
        nearest_fixed_cb,
        paint_viewport,
      )?;
      return Ok((frags, height, positioned, height));
    }

    let parent_clone = Self::clone_with_children(parent, children.to_vec());
    let column_constraints = LayoutConstraints::new(AvailableSpace::Definite(column_width), available_height);
    let (flow_fragments, flow_height, flow_positioned) = self.layout_children(
      &parent_clone,
      &column_constraints,
      nearest_positioned_cb,
      nearest_fixed_cb,
      paint_viewport,
    )?;
    let flow_fragments: Vec<FragmentNode> = flow_fragments
      .into_iter()
      .map(|mut frag| {
        Self::set_logical_from_bounds(&mut frag);
        frag
      })
      .collect();

    let mut flow_root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, column_width, flow_height),
      flow_fragments,
    );
    flow_root.style = Some(parent.style.clone());
    let flow_height = flow_height.max(flow_root.logical_bounding_box().height());
    if flow_height.is_finite() && flow_height > 0.0 {
      flow_root.bounds = Rect::from_xywh(0.0, 0.0, column_width, flow_height);
    }

    let (writing_mode, direction) = flow_root
      .style
      .as_ref()
      .map(|s| (s.writing_mode, s.direction))
      .unwrap_or((WritingMode::HorizontalTb, Direction::Ltr));
    // Multi-column layout happens while fragments are still in logical coordinate space
    // (inline axis = x, block axis = y). The final writing-mode transform happens later via
    // `convert_fragment_axes`, so fragmentation/clipping must use logical axes here.
    let axis = FragmentAxis {
      block_is_horizontal: false,
      block_positive: true,
    };
    let axes = FragmentAxes::default();
    let inline_is_horizontal = inline_axis_is_horizontal(writing_mode);
    let inline_positive = inline_axis_positive(writing_mode, direction);
    let mut analyzer =
      FragmentationAnalyzer::new(&flow_root, FragmentationContext::Column, axes, None);
    let flow_extent = analyzer.content_extent();

    let balanced_height = if column_count > 0 {
      flow_extent / column_count as f32
    } else {
      flow_extent
    };
    let fragmentainer_hint = crate::layout::formatting_context::fragmentainer_block_size_hint()
      .filter(|h| h.is_finite() && *h > 0.0);
    let fragmented_context = fragmentainer_hint.is_some();
    let mut column_height = match column_fill {
      ColumnFill::Auto => match available_height {
        AvailableSpace::Definite(h) => h,
        _ => balanced_height,
      },
      ColumnFill::Balance | ColumnFill::BalanceAll => balanced_height,
    };
    if matches!(available_height, AvailableSpace::Indefinite) {
      if let Some(hint) = fragmentainer_hint {
        column_height = hint;
      }
    }
    if matches!(
      column_fill,
      ColumnFill::Balance | ColumnFill::BalanceAll
    )
      && fragmentainer_hint.is_none()
      && column_height.is_finite()
      && column_height > 0.0
      && flow_extent.is_finite()
      && flow_extent > 0.0
      && column_count > 1
    {
      let max_height = match available_height {
        AvailableSpace::Definite(h) if h.is_finite() && h > 0.0 => h,
        _ => flow_extent,
      };
      let max_height = max_height.max(0.0);
      let min_height = column_height.min(max_height);
      if max_height > 0.0 && min_height > 0.0 {
        let mut fragment_count_for = |height: f32| -> Result<usize, LayoutError> {
          Ok(
            analyzer
              .boundaries(height, flow_extent.max(height))?
              .len()
              .saturating_sub(1),
          )
        };

        let count_at_max = fragment_count_for(max_height)?;
        if count_at_max > column_count {
          column_height = max_height;
        } else {
          let count_at_min = fragment_count_for(min_height)?;
          if count_at_min > column_count {
            let mut low = min_height;
            let mut high = max_height;
            for _ in 0..16 {
              let mid = (low + high) / 2.0;
              let count_at_mid = fragment_count_for(mid)?;
              if count_at_mid <= column_count {
                high = mid;
              } else {
                low = mid;
              }
            }
            column_height = high;
            if fragment_count_for(column_height)? > column_count {
              column_height = max_height;
            }
          } else {
            column_height = min_height;
          }
        }
      }
    }
    if let AvailableSpace::Definite(h) = available_height {
      if h.is_finite() && h > 0.0 {
        column_height = column_height.min(h);
      }
    }
    if !column_height.is_finite() || column_height <= 0.0 {
      column_height = flow_extent.max(0.0);
    }
    if column_height <= 0.0 {
      return Ok((
        flow_root.children.to_vec(),
        flow_height,
        flow_positioned,
        flow_height,
      ));
    }

    let root_block_size = axis.block_size(&flow_root.bounds);
    let total_extent = flow_extent.max(column_height);
    let mut boundaries = analyzer.boundaries(column_height, total_extent)?;
    let mut fragment_count = boundaries.len().saturating_sub(1);
    if fragmentainer_hint.is_none()
      && matches!(
        column_fill,
        ColumnFill::Balance | ColumnFill::BalanceAll
      )
      && fragment_count == column_count
    {
      let balanced = analyzer.balanced_boundaries(column_count, column_height, total_extent)?;
      let balanced_count = balanced.len().saturating_sub(1);
      if balanced_count == column_count {
        boundaries = balanced;
      }
    }

    // In paged/fragmented contexts the fragmentainer block-size hint pins the physical column
    // height to a fixed value (e.g. the page height). `column-fill: balance` and `balance-all`
    // still need to distribute content evenly across the columns inside each fragmentainer,
    // leaving whitespace at the bottom of shorter columns rather than filling sequentially.
    if fragmentainer_hint.is_some()
      && matches!(available_height, AvailableSpace::Indefinite)
      && matches!(
        column_fill,
        ColumnFill::Balance | ColumnFill::BalanceAll
      )
      && column_count > 1
    {
      let base_boundaries = boundaries;
      let base_fragment_count = base_boundaries.len().saturating_sub(1);
      let set_count = (base_fragment_count + column_count - 1) / column_count;
      if base_fragment_count > 0 && set_count > 0 {
        let last_set = set_count.saturating_sub(1);
        let mut balanced_boundaries =
          Vec::with_capacity(set_count.saturating_mul(column_count) + 1);
        balanced_boundaries.push(*base_boundaries.first().unwrap_or(&0.0));

        for set in 0..set_count {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }

          let start_idx = set * column_count;
          let end_idx = ((set + 1) * column_count).min(base_fragment_count);
          let set_start = base_boundaries.get(start_idx).copied().unwrap_or(0.0);
          let set_end = base_boundaries.get(end_idx).copied().unwrap_or(set_start);
          let should_balance = match column_fill {
            ColumnFill::BalanceAll => true,
            ColumnFill::Balance => set == last_set,
            _ => false,
          };

          if !should_balance {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            // Ensure subsequent sets map to the correct column indices even if the analyzer
            // produces fewer fragments (empty columns are represented as zero-length fragments).
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let set_total_extent = (set_end - set_start).max(0.0);
          if set_total_extent <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          // Only analyze the actual content in the set. Some callers extend the boundary list to
          // include trailing empty space (e.g. when the content is shorter than the fragmentainer).
          // Fragmenting that trailing empty region can create spurious extra "columns".
          let set_content_end = flow_extent.min(set_end).max(set_start);
          let set_content_total = (set_content_end - set_start).max(0.0);
          if set_content_total <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let clipped_content = clip_node(
            &flow_root,
            &axis,
            set_start,
            set_content_end,
            0.0,
            set_start,
            set_content_end,
            root_block_size,
            0,
            1,
            FragmentationContext::Column,
            column_height,
            axes,
          )?;
          let Some(mut clipped_content) = clipped_content else {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          };
          // `clip_node` preserves logical overrides from the original flow tree. Those overrides
          // reflect the pre-clipped coordinates and would cause the analyzer to think the clipped
          // subtree is much taller than it is (because `FragmentationAnalyzer` bases
          // `content_extent` on logical bounds). Reset logical overrides so fragmentation decisions
          // operate on the clipped geometry.
          Self::set_logical_from_bounds(&mut clipped_content);

          let mut set_analyzer =
            FragmentationAnalyzer::new(&clipped_content, FragmentationContext::Column, axes, None);
          let content_extent = set_analyzer.content_extent().max(0.0).min(set_content_total);
          if content_extent <= 0.0 {
            for _ in 0..column_count {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let min_height = (content_extent / column_count as f32).max(0.0);
          let max_height = column_height.min(content_extent).max(0.0);
          if min_height > column_height || max_height <= 0.0 {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let mut fragment_count_for = |height: f32| -> Result<usize, LayoutError> {
            Ok(
              set_analyzer
                .boundaries(height, content_extent)?
                .len()
                .saturating_sub(1),
            )
          };
          let count_at_max = fragment_count_for(max_height)?;
          if count_at_max > column_count {
            if start_idx + 1 <= end_idx && end_idx < base_boundaries.len() {
              balanced_boundaries.extend_from_slice(&base_boundaries[start_idx + 1..=end_idx]);
            }
            while balanced_boundaries.len() < (set + 1) * column_count + 1 {
              balanced_boundaries.push(set_end);
            }
            continue;
          }

          let used_height = {
            let count_at_min = fragment_count_for(min_height)?;
            if count_at_min > column_count {
              let mut low = min_height;
              let mut high = max_height;
              for _ in 0..16 {
                let mid = (low + high) / 2.0;
                let count_at_mid = fragment_count_for(mid)?;
                if count_at_mid <= column_count {
                  high = mid;
                } else {
                  low = mid;
                }
              }
              let mut height = high;
              if fragment_count_for(height)? > column_count {
                height = max_height;
              }
              height
            } else {
              min_height
            }
          };

          let mut set_boundaries = set_analyzer.boundaries(used_height, content_extent)?;
          if let Some(last) = set_boundaries.last_mut() {
            *last = set_total_extent;
          }
          while set_boundaries.len() < column_count + 1 {
            set_boundaries.push(set_total_extent);
          }
          for boundary in set_boundaries.iter().skip(1) {
            balanced_boundaries.push(set_start + *boundary);
          }
        }

        boundaries = balanced_boundaries;
      } else {
        boundaries = base_boundaries;
      }
    }
    fragment_count = boundaries.len().saturating_sub(1);
    if fragment_count == 0 {
      return Ok((
        flow_root.children.to_vec(),
        flow_height,
        flow_positioned,
        flow_height,
      ));
    }

    let inline_sign = if inline_positive { 1.0 } else { -1.0 };
    let stride = column_width + column_gap;
    let mut fragments = Vec::new();
    let mut fragment_heights = vec![0.0f32; fragment_count];
    let mut fragment_has_content = vec![false; fragment_count];
    for (index, window) in boundaries.windows(2).enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let start = window[0];
      let end = window[1];
      if end <= start {
        continue;
      }

      if let Some(mut clipped) = clip_node(
        &flow_root,
        &axis,
        start,
        end,
        0.0,
        start,
        end,
        root_block_size,
        index,
        fragment_count,
        FragmentationContext::Column,
        column_height,
        axes,
      )? {
        let has_content = !clipped.children.is_empty();
        fragment_has_content[index] = has_content;
        normalize_fragment_margins(&mut clipped, index == 0, index + 1 >= fragment_count, &axis);
        // `clip_node` preserves existing logical overrides from the unclipped flow tree.
        // For multi-column layout we translate fragments into column coordinates, so ensure
        // logical bounds stay in sync with the clipped fragment geometry.
        Self::set_logical_from_bounds(&mut clipped);
        propagate_fragment_metadata(&mut clipped, index, fragment_count);
        let col = if fragmented_context {
          index % column_count
        } else {
          index
        };
        let set = if fragmented_context { index / column_count } else { 0 };
        let column_offset = col as f32 * stride * inline_sign;
        let row_offset = set as f32 * column_height;
        let offset = Point::new(column_offset, row_offset);
        // `flow_root` and its descendants are still stored in "logical" coordinates where each
        // fragment's x/y represent its own inline/block axes. When descendants override
        // `writing-mode`, their inline axis no longer maps to the container's inline axis. Convert
        // the column/set placement delta through physical space so we translate each fragment in
        // the correct axis for its own writing mode.
        let physical_offset = if block_axis_is_horizontal(writing_mode) {
          let block_positive = block_axis_positive(writing_mode);
          let inline_positive = inline_axis_positive(writing_mode, direction);
          let dx = if block_positive { offset.y } else { -offset.y };
          let dy = if inline_positive { offset.x } else { -offset.x };
          Point::new(dx, dy)
        } else {
          offset
        };
        fragment_heights[index] = axis.block_size(&clipped.logical_bounding_box());
        let mut children: Vec<_> = clipped.children.into_iter().collect();
        for child in &mut children {
          let (child_wm, child_dir) = child
            .style
            .as_ref()
            .map(|s| (s.writing_mode, s.direction))
            .unwrap_or((writing_mode, direction));
          let child_offset = if block_axis_is_horizontal(child_wm) {
            let block_positive = block_axis_positive(child_wm);
            let inline_positive = inline_axis_positive(child_wm, child_dir);
            let dx = if inline_positive { physical_offset.y } else { -physical_offset.y };
            let dy = if block_positive { physical_offset.x } else { -physical_offset.x };
            Point::new(dx, dy)
          } else {
            physical_offset
          };
          child.bounds = child.bounds.translate(child_offset);
          if let Some(logical) = child.logical_override {
            child.logical_override = Some(logical.translate(child_offset));
          }
        }
        fragments.extend(children);
      }
    }

    let mut positioned_children = Vec::new();
    for mut positioned in flow_positioned {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      if let Some(pos) = positioned.static_position {
        if fragment_count > 0 && !boundaries.is_empty() {
          let flow_coord = pos.y;
          let frag_index = boundaries
            .windows(2)
            .position(|w| flow_coord >= w[0] - 0.01 && flow_coord < w[1] + 0.01)
            .unwrap_or(fragment_count - 1);
          let start = boundaries[frag_index];
          let col = if fragmented_context {
            frag_index % column_count
          } else {
            frag_index
          };
          let set = if fragmented_context { frag_index / column_count } else { 0 };
          let mut translated = pos;
          let column_delta = col as f32 * stride * inline_sign;
          let block_delta = -start + set as f32 * column_height;
          translated.x += column_delta;
          translated.y += block_delta;
          positioned.static_position = Some(translated);
        }
      }
      positioned_children.push(positioned);
    }

    let set_count = if fragment_count == 0 {
      0
    } else if fragmented_context {
      (fragment_count + column_count - 1) / column_count
    } else {
      1
    };
    let mut set_heights = vec![0.0f32; set_count];
    for (idx, height) in fragment_heights.iter().copied().enumerate() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let set = if fragmented_context { idx / column_count } else { 0 };
      if set < set_heights.len() {
        set_heights[set] = set_heights[set].max(height);
      }
    }

    let mut segment_height = if fragmented_context {
      let last_set_bottom = if set_count > 0 {
        let last_set = set_count - 1;
        let mut bottom = 0.0f32;
        for (idx, height) in fragment_heights.iter().copied().enumerate() {
          if let Err(RenderError::Timeout { elapsed, .. }) =
            check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
          {
            return Err(LayoutError::Timeout { elapsed });
          }
          if idx / column_count == last_set {
            bottom = bottom.max(last_set as f32 * column_height + height);
          }
        }
        bottom
      } else {
        0.0
      };
      (set_count as f32 * column_height).max(last_set_bottom)
    } else {
      let mut height = fragment_heights.iter().copied().fold(0.0, f32::max);
      if matches!(column_fill, ColumnFill::Auto) {
        if let AvailableSpace::Definite(h) = available_height {
          if h.is_finite() && h > 0.0 {
            height = height.max(h);
          }
        }
      }
      height
    };
    if segment_height == 0.0 {
      segment_height = flow_height;
    }

    if column_count > 1
      && column_gap > 0.0
      && !matches!(
        parent.style.column_rule_style,
        BorderStyle::None | BorderStyle::Hidden
      )
    {
      let mut rule_width = resolve_length_for_width(
        parent.style.column_rule_width,
        column_width,
        &parent.style,
        &self.font_context,
        self.viewport_size,
      )
      .min(column_gap)
      .max(0.0);
      if rule_width > 0.0 {
        let color = parent.style.column_rule_color.unwrap_or(parent.style.color);
        let mut rule_style = ComputedStyle::default();
        rule_style.writing_mode = writing_mode;
        rule_style.direction = direction;
        if rule_width > column_gap {
          rule_width = column_gap;
        }
        rule_style.display = Display::Block;
        rule_style.writing_mode = writing_mode;
        rule_style.direction = direction;
        if inline_is_horizontal {
          rule_style.border_left_width = Length::px(rule_width);
          rule_style.border_left_style = parent.style.column_rule_style;
          rule_style.border_left_color = color;
        } else {
          rule_style.border_top_width = Length::px(rule_width);
          rule_style.border_top_style = parent.style.column_rule_style;
          rule_style.border_top_color = color;
        }
        let rule_style = Arc::new(rule_style);
        for set in 0..set_count {
          let cols_in_set = if fragmented_context {
            let remaining = fragment_count.saturating_sub(set * column_count);
            remaining.min(column_count)
          } else if set == 0 {
            fragment_count
          } else {
            0
          };
          if cols_in_set < 2 {
            continue;
          }

          let rule_extent = if fragmented_context {
            column_height.max(set_heights.get(set).copied().unwrap_or(0.0))
          } else {
            segment_height
          };
          for i in 1..cols_in_set {
            let left_idx = if fragmented_context {
              set * column_count + (i - 1)
            } else {
              i - 1
            };
            let right_idx = left_idx + 1;
            if !fragment_has_content[left_idx] || !fragment_has_content[right_idx] {
              continue;
            }

            let prev_origin = (i - 1) as f32 * stride * inline_sign;
            let curr_origin = i as f32 * stride * inline_sign;
            let left_origin = prev_origin.min(curr_origin);
            let right_origin = prev_origin.max(curr_origin);
            let gap_start = left_origin + column_width;
            let gap = (right_origin - gap_start).max(0.0);
            let x = gap_start + (gap - rule_width).max(0.0) * 0.5;
            let y = if fragmented_context {
              set as f32 * column_height
            } else {
              0.0
            };
            let bounds = Rect::from_xywh(x, y, rule_width, rule_extent);
            let mut rule_fragment =
              FragmentNode::new_block_styled(bounds, Vec::new(), rule_style.clone());
            Self::set_logical_from_bounds(&mut rule_fragment);
            fragments.push(rule_fragment);
          }
        }
      }
    }

    Ok((fragments, segment_height, positioned_children, flow_height))
  }

  fn layout_multicolumn(
    &self,
    parent: &BoxNode,
    constraints: &LayoutConstraints,
    nearest_positioned_cb: &ContainingBlock,
    nearest_fixed_cb: &ContainingBlock,
    available_inline: f32,
    paint_viewport: Rect,
  ) -> Result<
    (
      Vec<FragmentNode>,
      f32,
      Vec<PositionedCandidate>,
      Option<FragmentationInfo>,
    ),
    LayoutError,
  > {
    let (column_count, column_width, column_gap) =
      self.compute_column_geometry(&parent.style, available_inline);
    if column_count <= 1 {
      let (frags, height, positioned) = self.layout_children(
        parent,
        constraints,
        nearest_positioned_cb,
        nearest_fixed_cb,
        paint_viewport,
      )?;
      let info = FragmentationInfo {
        column_count,
        column_gap,
        column_width,
        flow_height: height,
      };
      return Ok((frags, height, positioned, Some(info)));
    }

    let mut fragments = Vec::new();
    let mut positioned_children = Vec::new();
    let mut physical_offset = 0.0;
    let mut logical_offset = 0.0;
    let mut idx = 0;
    let mut deadline_counter = 0usize;

    while idx < parent.children.len() {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 16, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      let next_span = parent.children[idx..]
        .iter()
        .position(|c| c.style.column_span == ColumnSpan::All)
        .map(|p| p + idx);
      let end = next_span.unwrap_or(parent.children.len());

      if end > idx {
        let segment_viewport = paint_viewport.translate(Point::new(0.0, -physical_offset));
        let segment_column_fill = if next_span.is_some() {
          ColumnFill::Balance
        } else {
          parent.style.column_fill
        };
        let (mut seg_fragments, seg_height, mut seg_positioned, seg_flow_height) = self
          .layout_column_segment(
            parent,
            &parent.children[idx..end],
            column_count,
            column_width,
            column_gap,
            constraints.available_height,
            segment_column_fill,
            nearest_positioned_cb,
            nearest_fixed_cb,
            segment_viewport,
          )?;
        for frag in &mut seg_fragments {
          Self::translate_with_logical(frag, 0.0, physical_offset, logical_offset);
        }
        for positioned in &mut seg_positioned {
          if let Some(pos) = positioned.static_position {
            positioned.static_position = Some(Point::new(pos.x, pos.y + physical_offset));
          }
        }
        fragments.extend(seg_fragments);
        positioned_children.extend(seg_positioned);
        physical_offset += seg_height;
        logical_offset += seg_flow_height;
      }

      if let Some(span_idx) = next_span {
        let span_parent =
          Self::clone_with_children(parent, vec![parent.children[span_idx].clone()]);
        let span_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(available_inline),
          constraints.available_height,
        );
        let (mut span_fragments, span_height, mut span_positioned) = self.layout_children(
          &span_parent,
          &span_constraints,
          nearest_positioned_cb,
          nearest_fixed_cb,
          paint_viewport.translate(Point::new(0.0, -physical_offset)),
        )?;
        for frag in &mut span_fragments {
          Self::set_logical_from_bounds(frag);
          Self::translate_with_logical(frag, 0.0, physical_offset, logical_offset);
        }
        for positioned in &mut span_positioned {
          if let Some(pos) = positioned.static_position {
            positioned.static_position = Some(Point::new(pos.x, pos.y + physical_offset));
          }
        }
        fragments.extend(span_fragments);
        positioned_children.extend(span_positioned);
        physical_offset += span_height;
        logical_offset += span_height;
        idx = span_idx + 1;
      } else {
        break;
      }
    }

    let info = FragmentationInfo {
      column_count,
      column_gap,
      column_width,
      flow_height: logical_offset,
    };

    Ok((fragments, physical_offset, positioned_children, Some(info)))
  }
}

impl Default for BlockFormattingContext {
  fn default() -> Self {
    Self::new()
  }
}

impl std::fmt::Debug for BlockFormattingContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("BlockFormattingContext")
  }
}

impl FormattingContext for BlockFormattingContext {
  #[allow(clippy::cognitive_complexity)]
  fn layout(
    &self,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
  ) -> Result<FragmentNode, LayoutError> {
    let _profile = layout_timer(LayoutKind::Block);
    if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
      return Err(LayoutError::Timeout { elapsed });
    }
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    if let Some(cached) = layout_cache_lookup(
      box_node,
      FormattingContextType::Block,
      constraints,
      self.viewport_scroll,
      self.viewport_size,
    ) {
      return Ok(cached);
    }
    let style = style_override.as_ref().unwrap_or(&box_node.style);
    let base_paint_viewport =
      paint_viewport_for(style.writing_mode, style.direction, self.viewport_size);
    let toggles = crate::debug::runtime::runtime_toggles();
    let inline_is_horizontal = inline_axis_is_horizontal(style.writing_mode);
    let _inline_positive = inline_axis_positive(style.writing_mode, style.direction);
    let _block_positive = block_axis_positive(style.writing_mode);
    let inline_space = if inline_is_horizontal {
      constraints.available_width
    } else {
      constraints.available_height
    };
    let inline_viewport = if inline_is_horizontal {
      self.viewport_size.width
    } else {
      self.viewport_size.height
    };
    let log_skinny = toggles.truthy("FASTR_LOG_SKINNY_FLEX");
    let inline_percentage_base = match inline_space {
      AvailableSpace::Definite(_) => {
        let base = if inline_is_horizontal {
          constraints
            .inline_percentage_base
            .or_else(|| constraints.width())
        } else {
          constraints.height()
        };
        let viewport_inline = if inline_is_horizontal {
          self.viewport_size.width
        } else {
          self.viewport_size.height
        };
        base.unwrap_or(viewport_inline)
      }
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite => {
        constraints.inline_percentage_base.unwrap_or(0.0)
      }
    };
    // When the containing block inline size is intrinsic/indefinite (min-/max-content probes),
    // percentage widths behave as `auto` per CSS sizing. Strip percentage width/min/max hints
    // so intrinsic sizing does not resolve them against an unrelated base (e.g., viewport).
    let use_percent_as_auto = matches!(
      inline_space,
      AvailableSpace::MinContent | AvailableSpace::MaxContent | AvailableSpace::Indefinite
    );
    let _style_for_width_owned: Option<ComputedStyle>;
    let style_for_width: &ComputedStyle = if use_percent_as_auto {
      let mut s: ComputedStyle = (**style).clone();
      if matches!(s.width, Some(len) if len.unit.is_percentage())
        || s
          .width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.width = None;
        s.width_keyword = None;
      }
      if matches!(s.min_width, Some(len) if len.unit.is_percentage())
        || s
          .min_width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.min_width = None;
        s.min_width_keyword = None;
      }
      if matches!(s.max_width, Some(len) if len.unit.is_percentage())
        || s
          .max_width_keyword
          .is_some_and(|keyword| keyword.has_percentage())
      {
        s.max_width = None;
        s.max_width_keyword = None;
      }
      _style_for_width_owned = Some(s);
      _style_for_width_owned.as_ref().unwrap()
    } else {
      _style_for_width_owned = None;
      style
    };

    // When available width is indefinite/max-content, try to derive a reasonable containing
    // width from the element's own sizing hints (max-width/width/min-width) before falling
    // back to the viewport. The base for percentages must be the parent’s containing width
    // (the constraint) rather than the viewport; otherwise centered/narrow wrappers (e.g.,
    // 400px max-width zones) inflate to 1200px during intrinsic probes.
    let preferred_containing_width = |percentage_base: f32| {
      let resolve = |len: &Length| {
        resolve_length_for_width(
          *len,
          percentage_base,
          style,
          &self.font_context,
          self.viewport_size,
        )
      };
      style
        .max_width
        .as_ref()
        .map(resolve)
        .or_else(|| style.width.as_ref().map(resolve))
        .or_else(|| style.min_width.as_ref().map(resolve))
    };

    // Replaced elements laid out as standalone formatting contexts: compute their used size
    // directly instead of running the block width algorithm (which would treat the specified
    // width as the used content width without honoring max-width).
    if let BoxType::Replaced(replaced_box) = &box_node.box_type {
      let mut containing_width = inline_percentage_base;
      if containing_width <= 1.0 {
        let width_is_absolute = style
          .width
          .as_ref()
          .map(|l| l.unit.is_absolute())
          .unwrap_or(false);
        if !width_is_absolute {
          containing_width = self.viewport_size.width;
        }
      }
      let containing_height = if inline_is_horizontal {
        constraints.height()
      } else {
        constraints.width()
      };
      let percentage_base = Some(crate::geometry::Size::new(
        containing_width,
        containing_height.unwrap_or(f32::NAN),
      ));
      // `compute_replaced_size` returns the used content-box size and already accounts for min/max
      // constraints while interpreting `box-sizing`. Avoid reapplying min/max clamps here.
      let used_size = compute_replaced_size(style, replaced_box, percentage_base, self.viewport_size);
      if log_skinny && containing_width <= 1.0 {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        eprintln!(
                    "[skinny-block-constraint] id={} selector={} replaced containing_w={:.2} used_w={:.2} min_w={:?} max_w={:?}",
                    box_node.id, selector, containing_width, used_size.width, style.min_width, style.max_width
                );
      }
      // `compute_replaced_size` returns a content-box size. Fragment bounds are border-box sized,
      // so include padding and border edges here; absolute positioning and container query sizing
      // both expect border-box fragment geometry.
      let inline_edges = if inline_is_horizontal {
        horizontal_padding_and_borders(
          style,
          containing_width,
          self.viewport_size,
          &self.font_context,
        )
      } else {
        vertical_padding_and_borders(
          style,
          containing_width,
          self.viewport_size,
          &self.font_context,
        )
      };
      let block_edges = if inline_is_horizontal {
        vertical_padding_and_borders(
          style,
          containing_width,
          self.viewport_size,
          &self.font_context,
        )
      } else {
        horizontal_padding_and_borders(
          style,
          containing_width,
          self.viewport_size,
          &self.font_context,
        )
      };

      let bounds = Rect::new(
        Point::new(0.0, 0.0),
        Size::new(
          (used_size.width + inline_edges).max(0.0),
          (used_size.height + block_edges).max(0.0),
        ),
      );
      let fragment = FragmentNode::new_with_style(
        bounds,
        crate::tree::fragment_tree::FragmentContent::Replaced {
          replaced_type: replaced_box.replaced_type.clone(),
          box_id: Some(box_node.id),
        },
        vec![],
        box_node.style.clone(),
      );
      let converted = convert_fragment_axes(
        fragment,
        bounds.width(),
        bounds.height(),
        style.writing_mode,
        style.direction,
      );
      return Ok(converted);
    }

    let intrinsic_width_mode = matches!(
      inline_space,
      AvailableSpace::MaxContent | AvailableSpace::MinContent | AvailableSpace::Indefinite
    );
    let mut containing_width = match inline_space {
      AvailableSpace::Definite(w) => w,
      // In-flow blocks use the containing block’s inline size; shrink-to-fit contexts should
      // feed a definite width in constraints. When the available width is indefinite/max/min
      // content, prefer the element’s own sizing hints (resolved against the parent
      // containing width when known) before falling back to the viewport.
      AvailableSpace::MaxContent | AvailableSpace::MinContent | AvailableSpace::Indefinite => {
        preferred_containing_width(inline_percentage_base).unwrap_or(inline_percentage_base)
      }
    };
    if containing_width <= 1.0 && !intrinsic_width_mode {
      let width_is_absolute = style
        .width
        .as_ref()
        .map(|l| l.unit.is_absolute())
        .unwrap_or(false);
      if !width_is_absolute {
        containing_width = inline_viewport;
      }
    }
    if toggles.truthy("FASTR_LOG_SMALL_BLOCK") && containing_width < 150.0 {
      let selector = box_node
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anonymous>".to_string());
      eprintln!(
                "[block-small] id={} selector={} containing_w={:.1} avail_w={:?} width_decl={:?} min_w={:?} max_w={:?}",
                box_node.id,
                selector,
                containing_width,
                constraints.available_width,
                style.width,
                style.min_width,
                style.max_width,
            );
    }
    let containing_height = if inline_is_horizontal {
      constraints.height()
    } else {
      constraints.width()
    };
    // For flex items, prefer the max-content contribution instead of filling the available
    // width when width is auto (CSS Flexbox §4.5: auto main size uses the max-content size).
    // This avoids the block constraint equation forcing auto margins/auto widths to span the
    // containing block during flex item hypothetical sizing.
    let flex_pref_border = if self.flex_item_mode
      && style_for_width.width.is_none()
      && style_for_width.width_keyword.is_none()
    {
      let intrinsic_mode = match constraints.available_width {
        AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
        _ => IntrinsicSizingMode::MaxContent,
      };
      Some(self.compute_intrinsic_inline_size(box_node, intrinsic_mode)?)
    } else {
      None
    };

    let inline_sides = inline_axis_sides(style);
    let inline_positive = inline_axis_positive(style.writing_mode, style.direction);

    let mut computed_width = compute_block_width(
      style_for_width,
      containing_width,
      self.viewport_size,
      inline_sides,
      inline_positive,
    );
    let width_auto = style_for_width.width.is_none() && style_for_width.width_keyword.is_none();
    let inline_edges = computed_width.border_left
      + computed_width.padding_left
      + computed_width.padding_right
      + computed_width.border_right;
    let available_inline_border_box = (containing_width
      - resolve_margin_side(
        style,
        inline_sides.0,
        containing_width,
        &self.font_context,
        self.viewport_size,
      )
      - resolve_margin_side(
        style,
        inline_sides.1,
        containing_width,
        &self.font_context,
        self.viewport_size,
      ))
    .max(0.0);

    let available_content_for_fit = (available_inline_border_box - inline_edges).max(0.0);
    let mut intrinsic_content_sizes = None;
    if style_for_width.width.is_none() && style_for_width.width_keyword.is_some() {
      let keyword = style_for_width.width_keyword.unwrap();
      let fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let (min_content, max_content) = self.intrinsic_inline_content_sizes_for_sizing_keywords(
        box_node,
        fc_type,
        &self.factory,
      )?;
      intrinsic_content_sizes = Some((min_content, max_content));
      let keyword_content = self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style_for_width,
        inline_edges,
      );
      if self.flex_item_mode {
        computed_width.content_width = keyword_content;
      } else {
        let specified_width = match style_for_width.box_sizing {
          crate::style::types::BoxSizing::ContentBox => keyword_content,
          crate::style::types::BoxSizing::BorderBox => keyword_content + inline_edges,
        };
        let mut width_style = style_for_width.clone();
        width_style.width = Some(Length::px(specified_width));
        width_style.width_keyword = None;
        computed_width = compute_block_width(
          &width_style,
          containing_width,
          self.viewport_size,
          inline_sides,
          inline_positive,
        );
      }
    }

    if style.shrink_to_fit_inline_size && width_auto {
      let log_shrink = toggles.truthy("FASTR_LOG_SHRINK_TO_FIT");

      let fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let (preferred_min_content, preferred_content) = if fc_type == FormattingContextType::Block {
        self.compute_intrinsic_inline_sizes(box_node)?
      } else {
        let fc = self.factory.get(fc_type);
        fc.compute_intrinsic_inline_sizes(box_node)?
      };

      let edges_base0 =
        inline_axis_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context);
      let preferred_min =
        rebase_intrinsic_border_box_size(preferred_min_content, edges_base0, inline_edges);
      let preferred =
        rebase_intrinsic_border_box_size(preferred_content, edges_base0, inline_edges);
      let shrink_border_box = preferred.min(available_inline_border_box.max(preferred_min));
      let shrink_content = (shrink_border_box - inline_edges).max(0.0);
      if log_shrink {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        eprintln!(
                    "[shrink-to-fit] id={} selector={} preferred_min={:.1} preferred={:.1} available={:.1} content={:.1} edges={:.1}",
                    box_node.id, selector, preferred_min, preferred, available_inline_border_box, shrink_content, inline_edges
                );
      }
      let (margin_left, margin_right) = recompute_margins_for_width(
        style,
        containing_width,
        shrink_content,
        computed_width.border_left,
        computed_width.padding_left,
        computed_width.padding_right,
        computed_width.border_right,
        self.viewport_size,
        &self.font_context,
      );
      computed_width.content_width = shrink_content;
      computed_width.margin_left = margin_left;
      computed_width.margin_right = margin_right;
    }
    // When asked for intrinsic max-/min-content sizes, override the constraint equation with
    // the corresponding intrinsic inline size so flex/inline shrink-to-fit measurements don't
    // default to the full containing block width.
    if matches!(
      constraints.available_width,
      AvailableSpace::MinContent | AvailableSpace::MaxContent
    ) {
      let intrinsic_mode = match constraints.available_width {
        AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
        _ => IntrinsicSizingMode::MaxContent,
      };
      match self.compute_intrinsic_inline_size(box_node, intrinsic_mode) {
        Ok(intrinsic_border) => {
          let edges_base0 =
            inline_axis_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context);
          let intrinsic_border =
            rebase_intrinsic_border_box_size(intrinsic_border, edges_base0, inline_edges);
          let intrinsic_content = (intrinsic_border - inline_edges).max(0.0);
          computed_width.content_width = intrinsic_content;
        }
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => {}
      }
    }
    let horizontal_edges = computed_width.border_left
      + computed_width.padding_left
      + computed_width.padding_right
      + computed_width.border_right;
    if let Some(pref_border) = flex_pref_border {
      let edges_base0 =
        inline_axis_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context);
      let pref_border =
        rebase_intrinsic_border_box_size(pref_border, edges_base0, horizontal_edges);
      let pref_content = (pref_border - horizontal_edges).max(0.0);
      computed_width.content_width = pref_content;
    }
    let min_width = if let Some(keyword) = style_for_width.min_width_keyword {
      if intrinsic_content_sizes.is_none() {
        let fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        intrinsic_content_sizes = Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(
          box_node,
          fc_type,
          &self.factory,
        )?);
      }
      let (min_content, max_content) = intrinsic_content_sizes.unwrap();
      self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style_for_width,
        horizontal_edges,
      )
    } else {
      style_for_width
        .min_width
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style_for_width,
            &self.font_context,
            self.viewport_size,
          )
        })
        .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
        .unwrap_or(0.0)
    };

    let max_width = if let Some(keyword) = style_for_width.max_width_keyword {
      if intrinsic_content_sizes.is_none() {
        let fc_type = box_node
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        intrinsic_content_sizes = Some(self.intrinsic_inline_content_sizes_for_sizing_keywords(
          box_node,
          fc_type,
          &self.factory,
        )?);
      }
      let (min_content, max_content) = intrinsic_content_sizes.unwrap();
      self.resolve_intrinsic_size_keyword_to_content_width(
        keyword,
        min_content,
        max_content,
        available_content_for_fit,
        containing_width,
        style_for_width,
        horizontal_edges,
      )
    } else {
      style_for_width
        .max_width
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style_for_width,
            &self.font_context,
            self.viewport_size,
          )
        })
        .map(|w| content_size_from_box_sizing(w, horizontal_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    };

    // CSS 2.1 §10.4: if the computed min-width exceeds max-width, max-width is set to min-width.
    let max_width = if max_width.is_finite() && max_width < min_width {
      min_width
    } else {
      max_width
    };

    let clamped_content_width =
      crate::layout::utils::clamp_with_order(computed_width.content_width, min_width, max_width);
    let log_wide_block = toggles.truthy("FASTR_LOG_WIDE_FLEX");
    if log_wide_block && computed_width.content_width > self.viewport_size.width + 0.5 {
      let selector = box_node
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anonymous>".to_string());
      eprintln!(
                "[block-wide] box_id={:?} selector={} display={:?} containing={:.1} content_w={:.1} total_w={:.1} width={:?} min_w={:?} max_w={:?} viewport_w={:.1} avail_w={:?} margins=({:.1},{:.1})",
                box_node.id,
                selector,
                style.display,
                containing_width,
                computed_width.content_width,
                computed_width.total_width(),
                style.width,
                style.min_width,
                style.max_width,
                self.viewport_size.width,
                constraints.available_width,
                computed_width.margin_left,
                computed_width.margin_right,
            );
    }
    if self.flex_item_mode {
      // Flex items use their specified margins when computing hypothetical sizes; auto
      // margins resolve to 0 instead of being rebalanced to satisfy the block constraint
      // equation. Keep the clamped content width but avoid recomputing margins.
      computed_width.content_width = clamped_content_width;
      let resolved_ml = style
        .margin_left
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
          )
        })
        .unwrap_or(0.0);
      let resolved_mr = style
        .margin_right
        .as_ref()
        .map(|l| {
          resolve_length_for_width(
            *l,
            containing_width,
            style,
            &self.font_context,
            self.viewport_size,
          )
        })
        .unwrap_or(0.0);
      computed_width.margin_left = resolved_ml;
      computed_width.margin_right = resolved_mr;
    } else {
      if clamped_content_width != computed_width.content_width {
        let (margin_left, margin_right) = recompute_margins_for_width(
          style,
          containing_width,
          clamped_content_width,
          computed_width.border_left,
          computed_width.padding_left,
          computed_width.padding_right,
          computed_width.border_right,
          self.viewport_size,
          &self.font_context,
        );
        computed_width.content_width = clamped_content_width;
        computed_width.margin_left = margin_left;
        computed_width.margin_right = margin_right;
      }
    }

    if width_auto {
      if let Some(used_border_box) = constraints.used_border_box_width {
        let used_content = (used_border_box - horizontal_edges).max(0.0);
        if self.flex_item_mode {
          computed_width.content_width = used_content;
        } else {
          let (margin_left, margin_right) = recompute_margins_for_width(
            style,
            containing_width,
            used_content,
            computed_width.border_left,
            computed_width.padding_left,
            computed_width.padding_right,
            computed_width.border_right,
            self.viewport_size,
            &self.font_context,
          );
          computed_width.content_width = used_content;
          computed_width.margin_left = margin_left;
          computed_width.margin_right = margin_right;
        }
      }
    }

    let border_top = resolve_length_for_width(
      style.used_border_top_width(),
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let border_bottom = resolve_length_for_width(
      style.used_border_bottom_width(),
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let mut padding_top = resolve_length_for_width(
      style.padding_top,
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    let mut padding_bottom = resolve_length_for_width(
      style.padding_bottom,
      containing_width,
      style,
      &self.font_context,
      self.viewport_size,
    );
    // Reserve space for a horizontal scrollbar when requested by overflow or scrollbar-gutter stability.
    let reserve_horizontal_gutter = matches!(style.overflow_x, Overflow::Scroll)
      || (style.scrollbar_gutter.stable
        && matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll));
    if reserve_horizontal_gutter {
      let gutter = resolve_scrollbar_width(style);
      if style.scrollbar_gutter.both_edges {
        padding_top += gutter;
      }
      padding_bottom += gutter;
    }
    let vertical_edges = border_top + padding_top + padding_bottom + border_bottom;

    let block_length = if inline_is_horizontal { style.height } else { style.width };
    let block_keyword = if inline_is_horizontal {
      style.height_keyword
    } else {
      style.width_keyword
    };
    let min_block_keyword = if inline_is_horizontal {
      style.min_height_keyword
    } else {
      style.min_width_keyword
    };
    let max_block_keyword = if inline_is_horizontal {
      style.max_height_keyword
    } else {
      style.max_width_keyword
    };
    let height_auto = block_length.is_none() && block_keyword.is_none();
    let margin_top = style
      .margin_top
      .as_ref()
      .map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      })
      .unwrap_or(0.0);
    let margin_bottom = style
      .margin_bottom
      .as_ref()
      .map(|l| {
        resolve_length_for_width(
          *l,
          containing_width,
          style,
          &self.font_context,
          self.viewport_size,
        )
      })
      .unwrap_or(0.0);
    let available_block_border_box = containing_height
      .map(|h| (h - margin_top - margin_bottom).max(0.0))
      .unwrap_or(f32::INFINITY);

    let intrinsic_block_sizes = if block_keyword.is_some()
      || min_block_keyword.is_some()
      || max_block_keyword.is_some()
    {
      let fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      let (min_base0, max_base0) = if fc_type == FormattingContextType::Block {
        compute_intrinsic_block_sizes_without_block_size_constraints(self, box_node)?
      } else {
        let fc = self.factory.get(fc_type);
        compute_intrinsic_block_sizes_without_block_size_constraints(fc.as_ref(), box_node)?
      };

      let border_top_base0 = resolve_length_for_width(
        style.used_border_top_width(),
        0.0,
        style,
        &self.font_context,
        self.viewport_size,
      );
      let border_bottom_base0 = resolve_length_for_width(
        style.used_border_bottom_width(),
        0.0,
        style,
        &self.font_context,
        self.viewport_size,
      );
      let mut padding_top_base0 = resolve_length_for_width(
        style.padding_top,
        0.0,
        style,
        &self.font_context,
        self.viewport_size,
      );
      let mut padding_bottom_base0 = resolve_length_for_width(
        style.padding_bottom,
        0.0,
        style,
        &self.font_context,
        self.viewport_size,
      );
      if reserve_horizontal_gutter {
        let gutter = resolve_scrollbar_width(style);
        if style.scrollbar_gutter.both_edges {
          padding_top_base0 += gutter;
        }
        padding_bottom_base0 += gutter;
      }
      let vertical_edges_base0 =
        border_top_base0 + padding_top_base0 + padding_bottom_base0 + border_bottom_base0;
      Some((
        rebase_intrinsic_border_box_size(min_base0, vertical_edges_base0, vertical_edges),
        rebase_intrinsic_border_box_size(max_base0, vertical_edges_base0, vertical_edges),
      ))
    } else {
      None
    };

    let mut resolved_height = block_length
      .and_then(|h| {
        resolve_length_with_percentage_metrics(
          h,
          containing_height,
          self.viewport_size,
          style.font_size,
          style.root_font_size,
          Some(style),
          Some(&self.font_context),
        )
      })
      .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing));
    if let Some(height_keyword) = block_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let used_border_box = match height_keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      resolved_height = Some((used_border_box - vertical_edges).max(0.0));
    }
    if resolved_height.is_none() && height_auto {
      let used_border_box = if inline_is_horizontal {
        constraints.used_border_box_height
      } else {
        constraints.used_border_box_width
      };
      if let Some(used_border_box) = used_border_box {
        resolved_height = Some((used_border_box - vertical_edges).max(0.0));
      }
    }
    let child_height_space = resolved_height
      .map(|h| AvailableSpace::Definite(h.max(0.0)))
      .unwrap_or(AvailableSpace::Indefinite);

    let child_constraints = if inline_is_horizontal {
      LayoutConstraints::new(
        AvailableSpace::Definite(computed_width.content_width),
        child_height_space,
      )
    } else {
      LayoutConstraints::new(
        child_height_space,
        AvailableSpace::Definite(computed_width.content_width),
      )
    }
    .with_inline_percentage_base(Some(computed_width.content_width));

    let content_origin = Point::new(
      computed_width.border_left + computed_width.padding_left,
      border_top + padding_top,
    );
    let padding_origin = Point::new(computed_width.border_left, border_top);
    let content_height_base = resolved_height.unwrap_or(0.0).max(0.0);
    let padding_size = Size::new(
      computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
      content_height_base + padding_top + padding_bottom,
    );
    let cb_block_base = resolved_height.map(|h| h.max(0.0) + padding_top + padding_bottom);
    let establishes_positioned_cb = style.establishes_abs_containing_block();
    let establishes_fixed_cb = style.establishes_fixed_containing_block();
    let nearest_cb = if establishes_positioned_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(padding_origin, padding_size),
        self.viewport_size,
        Some(padding_size.width),
        cb_block_base,
      )
    } else {
      self.nearest_positioned_cb
    };
    let nearest_fixed_cb = if establishes_fixed_cb {
      ContainingBlock::with_viewport_and_bases(
        Rect::new(padding_origin, padding_size),
        self.viewport_size,
        Some(padding_size.width),
        cb_block_base,
      )
    } else {
      self.nearest_fixed_cb
    };

    let mut child_ctx = self.clone();
    child_ctx.flex_item_mode = false;
    child_ctx.nearest_positioned_cb = nearest_cb;
    child_ctx.nearest_fixed_cb = nearest_fixed_cb;
    if nearest_cb != self.nearest_positioned_cb || nearest_fixed_cb != self.nearest_fixed_cb {
      if nearest_cb != self.nearest_positioned_cb {
        child_ctx.factory = child_ctx.factory.with_positioned_cb(nearest_cb);
      }
      if nearest_fixed_cb != self.nearest_fixed_cb {
        child_ctx.factory = child_ctx.factory.with_fixed_cb(nearest_fixed_cb);
      }
      child_ctx.intrinsic_inline_fc =
        Arc::new(InlineFormattingContext::with_factory(child_ctx.factory.clone()));
    }
    let mut paint_viewport = base_paint_viewport;
    // The viewport rectangle is expressed in the formatting context's coordinate space. When this
    // block formatting context is nested inside another formatting context, the caller translates
    // the factory's `viewport_scroll` so it already accounts for the nested origin.
    let scroll = self.viewport_scroll;
    if scroll.x.is_finite() && scroll.y.is_finite() {
      let (scroll_inline, scroll_block) = if inline_is_horizontal {
        (scroll.x, scroll.y)
      } else {
        (scroll.y, scroll.x)
      };
      paint_viewport = paint_viewport.translate(Point::new(scroll_inline, scroll_block));
    }
    // Layout uses the block's content box coordinate space; translate the viewport into that
    // coordinate system so culling decisions stay relative to `box_y`/`margin_left` placement.
    let viewport_content_origin = Point::new(
      computed_width.margin_left + content_origin.x,
      content_origin.y,
    );
    paint_viewport = paint_viewport.translate(Point::new(
      -viewport_content_origin.x,
      -viewport_content_origin.y,
    ));
    let use_columns = Self::is_multicol_container(style);
    let skip_contents = match style.content_visibility {
      crate::style::types::ContentVisibility::Hidden => true,
      crate::style::types::ContentVisibility::Auto => {
        let activation_margin = toggles
          .f64("FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX")
          .unwrap_or(0.0)
          .max(0.0) as f32;
        let viewport = if activation_margin > 0.0 {
          paint_viewport.inflate(activation_margin)
        } else {
          paint_viewport
        };

        let box_width = computed_width.border_box_width();
        let box_width = if box_width.is_finite() {
          box_width.max(0.0)
        } else {
          0.0
        };

        let estimated_border_box_block_size = resolved_height
          .filter(|h| h.is_finite())
          .map(|h| h.max(0.0))
          .or_else(|| {
            let axis_is_width = block_axis_is_horizontal(style.writing_mode);
            let axis = if axis_is_width {
              style.contain_intrinsic_width
            } else {
              style.contain_intrinsic_height
            };
            axis
              .auto
              .then(|| {
                remembered_size_cache_lookup(box_node).map(|size| {
                  if axis_is_width {
                    size.width
                  } else {
                    size.height
                  }
                })
              })
              .flatten()
              .filter(|v| v.is_finite())
              .map(|v| v.max(0.0))
              .or_else(|| {
                axis
                  .length
                  .and_then(|l| {
                    resolve_length_with_percentage_metrics(
                      l,
                      containing_height,
                      self.viewport_size,
                      style.font_size,
                      style.root_font_size,
                      Some(style),
                      Some(&self.font_context),
                    )
                  })
                  .map(|v| v.max(0.0))
              })
          })
          .and_then(|content_estimate| {
            let border_box = content_estimate + vertical_edges;
            border_box.is_finite().then_some(border_box.max(0.0))
          });

        if let Some(block_size) = estimated_border_box_block_size {
          let border_box =
            Rect::from_xywh(-content_origin.x, -content_origin.y, box_width, block_size);
          !viewport.intersects(border_box)
        } else {
          false
        }
      }
      crate::style::types::ContentVisibility::Visible => false,
    };
    let (mut child_fragments, mut content_height, positioned_children, column_info) =
      if skip_contents {
        (Vec::new(), 0.0, Vec::new(), None)
      } else if use_columns {
        let (frags, height, positioned, info) = child_ctx.layout_multicolumn(
          box_node,
          &child_constraints,
          &nearest_cb,
          &nearest_fixed_cb,
          computed_width.content_width,
          paint_viewport,
        )?;
        (frags, height, positioned, info)
      } else {
        let (frags, height, positioned) = child_ctx.layout_children(
          box_node,
          &child_constraints,
          &nearest_cb,
          &nearest_fixed_cb,
          paint_viewport,
        )?;
        (frags, height, positioned, None)
      };
    if skip_contents || style.containment.size {
      let axis_is_width = block_axis_is_horizontal(style.writing_mode);
      let axis = if axis_is_width {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let remembered = axis
        .auto
        .then(|| {
          remembered_size_cache_lookup(box_node).map(|size| {
            if axis_is_width {
              size.width
            } else {
              size.height
            }
          })
        })
        .flatten();
      let resolved = if axis.auto {
        remembered.or_else(|| {
          axis.length.and_then(|l| {
            resolve_length_with_percentage_metrics(
              l,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
          })
        })
      } else {
        axis.length.and_then(|l| {
          resolve_length_with_percentage_metrics(
            l,
            containing_height,
            self.viewport_size,
            style.font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
          )
        })
      };
      let mut value = resolved.unwrap_or(0.0);
      if !value.is_finite() {
        value = 0.0;
      }
      content_height = value.max(0.0);
    }

    // Child fragments are produced in the block's content coordinate space (0,0 at the content
    // box). Translate them into the fragment's local coordinate space (border box) so padding and
    // borders correctly offset in-flow content.
    if content_origin.x != 0.0 || content_origin.y != 0.0 {
      for fragment in child_fragments.iter_mut() {
        fragment.translate_root_in_place(content_origin);
      }
    }

    let min_height = if let Some(keyword) = style.min_height_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let min_border = match keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      (min_border - vertical_edges).max(0.0)
    } else {
      style
        .min_height
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics(
            *l,
            containing_height,
            self.viewport_size,
            style.font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(0.0)
    };
    let max_height = if let Some(keyword) = style.max_height_keyword {
      let (intrinsic_min, intrinsic_max) = intrinsic_block_sizes.unwrap_or((0.0, 0.0));
      let max_border = match keyword {
        crate::style::types::IntrinsicSizeKeyword::MinContent => intrinsic_min,
        crate::style::types::IntrinsicSizeKeyword::MaxContent => intrinsic_max,
        crate::style::types::IntrinsicSizeKeyword::FillAvailable => {
          if available_block_border_box.is_finite() {
            available_block_border_box
          } else {
            intrinsic_max
          }
        }
        crate::style::types::IntrinsicSizeKeyword::FitContent { limit } => {
          let basis_border = match limit {
            Some(limit) => resolve_length_with_percentage_metrics(
              limit,
              containing_height,
              self.viewport_size,
              style.font_size,
              style.root_font_size,
              Some(style),
              Some(&self.font_context),
            )
            .map(|resolved| border_size_from_box_sizing(resolved, vertical_edges, style.box_sizing))
            .unwrap_or(f32::INFINITY),
            None => available_block_border_box,
          };
          crate::layout::utils::clamp_with_order(basis_border, intrinsic_min, intrinsic_max)
        }
      };
      (max_border - vertical_edges).max(0.0)
    } else {
      style
        .max_height
        .as_ref()
        .and_then(|l| {
          resolve_length_with_percentage_metrics(
            *l,
            containing_height,
            self.viewport_size,
            style.font_size,
            style.root_font_size,
            Some(style),
            Some(&self.font_context),
          )
        })
        .map(|h| content_size_from_box_sizing(h, vertical_edges, style.box_sizing))
        .unwrap_or(f32::INFINITY)
    };

    let max_height = if max_height.is_finite() && max_height < min_height {
      min_height
    } else {
      max_height
    };
    let height = crate::layout::utils::clamp_with_order(
      resolved_height.unwrap_or(content_height),
      min_height,
      max_height,
    );

    if !skip_contents {
      let remembered = if block_axis_is_horizontal(style.writing_mode) {
        Size::new(height, computed_width.content_width)
      } else {
        Size::new(computed_width.content_width, height)
      };
      remembered_size_cache_store(box_node, remembered);
    }

    let box_height = border_top + padding_top + height + padding_bottom + border_bottom;
    // For root/layout entry points, keep fragment bounds scoped to the border box so margins
    // don’t inflate measured sizes (e.g., when flex items are measured via a block FC). The
    // margin space stays outside the fragment’s local coordinates, matching the child layout
    // path in `layout_block_child`.
    let box_width = computed_width.border_box_width();

    // Layout out-of-flow positioned children against this block's padding box.
    let padding_origin = Point::new(computed_width.border_left, border_top);
    let padding_size = Size::new(
      computed_width.content_width + computed_width.padding_left + computed_width.padding_right,
      height + padding_top + padding_bottom,
    );
    let padding_rect = Rect::new(padding_origin, padding_size);

    if !positioned_children.is_empty() {
      let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
        self.font_context.clone(),
      );
      let mut anchor_index =
        crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
          child_fragments.as_slice(),
          box_node.id,
          &style.anchor_scope,
          self.viewport_size,
        );
      // Allow descendants to anchor against the containing block element itself.
      anchor_index.insert_names_for_box(
        box_node.id,
        &style.anchor_names,
        crate::layout::anchor_positioning::AnchorBox {
          rect: Rect::from_xywh(0.0, 0.0, box_width, box_height),
          writing_mode: style.writing_mode,
          direction: style.direction,
        },
      );
      let parent_padding_cb = ContainingBlock::with_viewport_and_bases(
        padding_rect,
        self.viewport_size,
        Some(padding_size.width),
        Some(padding_size.height),
      );
      let base_factory = self.factory.clone();
      let viewport_cb = ContainingBlock::viewport(self.viewport_size);
      let abs_factory = if parent_padding_cb == base_factory.nearest_positioned_cb() {
        base_factory.clone()
      } else {
        base_factory.with_positioned_cb(parent_padding_cb)
      };
      let fixed_factory = if viewport_cb == parent_padding_cb {
        abs_factory.clone()
      } else if viewport_cb == base_factory.nearest_positioned_cb() {
        base_factory.clone()
      } else {
        base_factory.with_positioned_cb(viewport_cb)
      };
      let factory_for_cb = |cb: ContainingBlock| -> &FormattingContextFactory {
        if cb == parent_padding_cb {
          &abs_factory
        } else if cb == viewport_cb {
          &fixed_factory
        } else {
          &base_factory
        }
      };

      let trace_positioned = trace_positioned_ids();
      for PositionedCandidate {
        node: child,
        source,
        static_position,
        query_parent_id,
      } in positioned_children
      {
        let original_style = child.style.clone();
        let cb = match source {
          ContainingBlockSource::ParentPadding => parent_padding_cb,
          ContainingBlockSource::Explicit(cb) => cb,
        };
        let factory = factory_for_cb(cb);
        // Layout the child as if it were in normal flow to obtain its intrinsic size.
        let mut static_style = (*child.style).clone();
        static_style.position = Position::Relative;
        static_style.top = crate::style::types::InsetValue::Auto;
        static_style.right = crate::style::types::InsetValue::Auto;
        static_style.bottom = crate::style::types::InsetValue::Auto;
        static_style.left = crate::style::types::InsetValue::Auto;
        let static_style = Arc::new(static_style);

        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        let fc = factory.get(fc_type);
        let child_height_space = cb
          .block_percentage_base()
          .map(AvailableSpace::Definite)
          .unwrap_or(AvailableSpace::Indefinite);
        let child_constraints = LayoutConstraints::new(
          AvailableSpace::Definite(padding_size.width),
          child_height_space,
        );

        // Resolve positioned style against the containing block.
        let anchors_for_cb = Some(&anchor_index);
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
          &original_style,
          &cb,
          self.viewport_size,
          &self.font_context,
          anchors_for_cb,
          Some(query_parent_id),
        );

        let mut static_pos = static_position.unwrap_or(Point::ZERO);
        if cb == parent_padding_cb {
          static_pos = Point::new(
            static_pos.x + computed_width.padding_left,
            static_pos.y + padding_top,
          );
        }
        let is_replaced = child.is_replaced();
        let needs_inline_intrinsics = (positioned_style.width.is_auto()
          && (positioned_style.left.is_auto() || positioned_style.right.is_auto() || is_replaced))
          || original_style.width_keyword.is_some()
          || original_style.min_width_keyword.is_some()
          || original_style.max_width_keyword.is_some();
        let needs_block_intrinsics = (positioned_style.height.is_auto()
          && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()))
          || original_style.height_keyword.is_some()
          || original_style.min_height_keyword.is_some()
          || original_style.max_height_keyword.is_some();
        let (
          mut child_fragment,
          preferred_min_inline,
          preferred_inline,
          preferred_min_block,
          preferred_block,
        ) = if child.id != 0 {
          crate::layout::style_override::with_style_override(
            child.id,
            static_style.clone(),
            || {
              let child_fragment = fc.layout(&child, &child_constraints)?;
              let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                match fc.compute_intrinsic_inline_sizes(&child) {
                  Ok((min, max)) => (Some(min), Some(max)),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => {
                    let min = match fc
                      .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MinContent)
                    {
                      Ok(value) => Some(value),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    let max = match fc
                      .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MaxContent)
                    {
                      Ok(value) => Some(value),
                      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                      Err(_) => None,
                    };
                    (min, max)
                  }
                }
              } else {
                (None, None)
              };
              let preferred_min_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(&child, IntrinsicSizingMode::MinContent) {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              let preferred_block = if needs_block_intrinsics {
                match fc.compute_intrinsic_block_size(&child, IntrinsicSizingMode::MaxContent) {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                }
              } else {
                None
              };
              Ok((
                child_fragment,
                preferred_min_inline,
                preferred_inline,
                preferred_min_block,
                preferred_block,
              ))
            },
          )?
        } else {
          let mut layout_child = child.clone();
          layout_child.style = static_style.clone();
          let child_fragment = fc.layout(&layout_child, &child_constraints)?;
          let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
            match fc.compute_intrinsic_inline_sizes(&layout_child) {
              Ok((min, max)) => (Some(min), Some(max)),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {
                let min = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MinContent)
                {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                let max = match fc
                  .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                {
                  Ok(value) => Some(value),
                  Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                  Err(_) => None,
                };
                (min, max)
              }
            }
          } else {
            (None, None)
          };
          let preferred_min_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent) {
              Ok(value) => Some(value),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          let preferred_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent) {
              Ok(value) => Some(value),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          (
            child_fragment,
            preferred_min_inline,
            preferred_inline,
            preferred_min_block,
            preferred_block,
          )
        };

        let actual_horizontal = positioned_style.padding.left
          + positioned_style.padding.right
          + positioned_style.border_width.left
          + positioned_style.border_width.right;
        let actual_vertical = positioned_style.padding.top
          + positioned_style.padding.bottom
          + positioned_style.border_width.top
          + positioned_style.border_width.bottom;
        let content_offset = Point::new(
          positioned_style.border_width.left + positioned_style.padding.left,
          positioned_style.border_width.top + positioned_style.padding.top,
        );
        let (intrinsic_horizontal, intrinsic_vertical) =
          crate::layout::absolute_positioning::intrinsic_edge_sizes(
            &original_style,
            self.viewport_size,
            &self.font_context,
          );
        let preferred_min_inline =
          preferred_min_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_inline = preferred_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
        let preferred_min_block = preferred_min_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let preferred_block = preferred_block.map(|v| (v - intrinsic_vertical).max(0.0));
        let intrinsic_size = Size::new(
          (child_fragment.bounds.size.width - actual_horizontal).max(0.0),
          (child_fragment.bounds.size.height - actual_vertical).max(0.0),
        );

        let mut input = crate::layout::absolute_positioning::AbsoluteLayoutInput::new(
          positioned_style,
          intrinsic_size,
          static_pos,
        );
        input.is_replaced = is_replaced;
        input.preferred_min_inline_size = preferred_min_inline;
        input.preferred_inline_size = preferred_inline;
        input.preferred_min_block_size = preferred_min_block;
        input.preferred_block_size = preferred_block;
        input.style.width_keyword = original_style.width_keyword;
        input.style.min_width_keyword = original_style.min_width_keyword;
        input.style.max_width_keyword = original_style.max_width_keyword;
        input.style.height_keyword = original_style.height_keyword;
        input.style.min_height_keyword = original_style.min_height_keyword;
        input.style.max_height_keyword = original_style.max_height_keyword;

        let result = abs.layout_absolute(&input, &cb)?;
        let border_size = Size::new(
          result.size.width + actual_horizontal,
          result.size.height + actual_vertical,
        );
        let border_origin = Point::new(
          result.position.x - content_offset.x,
          result.position.y - content_offset.y,
        );
        let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          || (border_size.height - child_fragment.bounds.height()).abs() > 0.01;
        if needs_relayout {
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
          );
          let relayout_constraints = child_constraints
            .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
          if child.id != 0 {
            if supports_used_border_box {
              child_fragment = crate::layout::style_override::with_style_override(
                child.id,
                static_style.clone(),
                || fc.layout(&child, &relayout_constraints),
              )?;
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(crate::style::values::Length::px(border_size.width));
              relayout_style.height = Some(crate::style::values::Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              relayout_style.min_width_keyword = None;
              relayout_style.max_width_keyword = None;
              relayout_style.min_height_keyword = None;
              relayout_style.max_height_keyword = None;
              child_fragment = crate::layout::style_override::with_style_override(
                child.id,
                Arc::new(relayout_style),
                || fc.layout(&child, &relayout_constraints),
              )?;
            }
          } else {
            let mut relayout_child = child.clone();
            if supports_used_border_box {
              relayout_child.style = static_style.clone();
            } else {
              let mut relayout_style = (*static_style).clone();
              relayout_style.width = Some(crate::style::values::Length::px(border_size.width));
              relayout_style.height = Some(crate::style::values::Length::px(border_size.height));
              relayout_style.width_keyword = None;
              relayout_style.height_keyword = None;
              relayout_style.min_width_keyword = None;
              relayout_style.max_width_keyword = None;
              relayout_style.min_height_keyword = None;
              relayout_style.max_height_keyword = None;
              relayout_child.style = Arc::new(relayout_style);
            }
            child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
          }
        }
        child_fragment.bounds = Rect::new(border_origin, border_size);
        child_fragment.style = Some(original_style);
        if trace_positioned.contains(&child.id) {
          let (text_count, total) = count_text_fragments(&child_fragment);
          eprintln!(
                        "[block-positioned-placed] child_id={} pos=({:.1},{:.1}) size=({:.1},{:.1}) texts={}/{}",
                        child.id,
                        border_origin.x,
                        border_origin.y,
                        border_size.width,
                        border_size.height,
                        text_count,
                        total
                    );
        }
        child_fragments.push(child_fragment);
      }
    }

    let bounds = Rect::from_xywh(computed_width.margin_left, 0.0, box_width, box_height);

    let mut fragment = FragmentNode::new_with_style(
      bounds,
      crate::tree::fragment_tree::FragmentContent::Block {
        box_id: Some(box_node.id),
      },
      child_fragments,
      box_node.style.clone(),
    );
    if let Some(info) = column_info {
      fragment.fragmentation = Some(info.clone());
      // Keep logical bounds aligned with the physical multi-column fragment geometry so
      // pagination uses the clipped height rather than the unfragmented flow height.
      fragment.logical_override = Some(fragment.bounds);
    }

    // Apply relative positioning after normal flow layout (CSS 2.1 §9.4.3).
    if style.position.is_relative() {
      let block_base = containing_height;
      let containing_block = ContainingBlock::with_viewport_and_bases(
        Rect::new(
          Point::ZERO,
          Size::new(containing_width, containing_height.unwrap_or(0.0)),
        ),
        self.viewport_size,
        Some(containing_width),
        block_base,
      );
      let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
        style,
        &containing_block,
        self.viewport_size,
        &self.font_context,
      );
      fragment = PositionedLayout::with_font_context(self.font_context.clone())
        .apply_relative_positioning(&fragment, &positioned_style, &containing_block)?;
    }
    let converted = convert_fragment_axes(
      fragment,
      box_width,
      box_height,
      style.writing_mode,
      style.direction,
    );

    layout_cache_store(
      box_node,
      FormattingContextType::Block,
      constraints,
      &converted,
      self.viewport_scroll,
      self.viewport_size,
    );

    Ok(converted)
  }

  fn compute_intrinsic_inline_sizes(&self, box_node: &BoxNode) -> Result<(f32, f32), LayoutError> {
    count_block_intrinsic_call();
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style = style_override.as_ref().unwrap_or(&box_node.style);
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);

    // Intrinsic inline sizes are normally memoized since they can require expensive inline layout.
    // However, when inline-size containment is enabled and `contain-intrinsic-*: auto` is in effect,
    // the returned size depends on the element's remembered size, which can change within a cache
    // epoch as elements are laid out (e.g., as `content-visibility:auto` boxes transition from
    // skipped → laid out). In that case, bypass the intrinsic cache so callers always observe the
    // latest remembered size.
    if !style.containment.isolates_inline_size() {
      let min_cached = intrinsic_cache_lookup(box_node, IntrinsicSizingMode::MinContent);
      let max_cached = intrinsic_cache_lookup(box_node, IntrinsicSizingMode::MaxContent);
      if let (Some(min), Some(max)) = (min_cached, max_cached) {
        return Ok((min, max));
      }
    }

    let edges = if inline_is_horizontal {
      horizontal_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context)
    } else {
      vertical_padding_and_borders(style, 0.0, self.viewport_size, &self.font_context)
    };
    // Honor specified widths that resolve without a containing block.
    if let Some(specified) = style.width.as_ref() {
      let resolved = resolve_length_for_width(
        *specified,
        0.0,
        style,
        &self.font_context,
        self.viewport_size,
      );
      // Ignore auto/relative cases that resolve to 0.0.
      if resolved > 0.0 {
        let result = border_size_from_box_sizing(resolved, edges, style.box_sizing);
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, result);
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, result);
        return Ok((result, result));
      }
    }

    if style.containment.isolates_inline_size() {
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let remembered = axis
        .auto
        .then(|| {
          remembered_size_cache_lookup(box_node).map(|size| {
            if inline_is_horizontal {
              size.width
            } else {
              size.height
            }
          })
        })
        .flatten();
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        remembered,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let result = (edges + fallback).max(0.0);
      if !axis.auto {
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, result);
        intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, result);
      }
      return Ok((result, result));
    }

    // Replaced elements fall back to their intrinsic content size plus padding/borders.
    if let BoxType::Replaced(replaced_box) = &box_node.box_type {
      let size = compute_replaced_size(style, replaced_box, None, self.viewport_size);
      let edges =
        horizontal_padding_and_borders(style, size.width, self.viewport_size, &self.font_context);
      let result = size.width + edges;
      intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, result);
      intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, result);
      return Ok((result, result));
    }

    let factory = &self.factory;
    let inline_fc = self.intrinsic_inline_fc.as_ref();

    // Inline formatting context contribution (text and inline-level children).
    // Block-level children split inline runs into separate formatting contexts.
    let log_ids = crate::debug::runtime::runtime_toggles()
      .usize_list("FASTR_LOG_INTRINSIC_IDS")
      .unwrap_or_default();
    let log_children = !log_ids.is_empty() && log_ids.contains(&box_node.id);

    let mut inline_min_width = 0.0f32;
    let mut inline_max_width = 0.0f32;
    let mut block_min_width = 0.0f32;
    let mut block_max_width = 0.0f32;
    let mut inline_run: Vec<&BoxNode> = Vec::new();
    let flush_inline_run = |run: &mut Vec<&BoxNode>,
                            widest_min: &mut f32,
                            widest_max: &mut f32|
     -> Result<(), LayoutError> {
      if run.is_empty() {
        return Ok(());
      }

      let (min_width, max_width) =
        inline_fc.intrinsic_widths_for_children(style, run.as_slice())?;
      if log_children {
        let ids: Vec<usize> = run.iter().map(|c| c.id()).collect();
        eprintln!(
          "[intrinsic-inline-run] parent_id={} ids={:?} min={:.2} max={:.2}",
          box_node.id, ids, min_width, max_width
        );
      }

      *widest_min = widest_min.max(min_width);
      *widest_max = widest_max.max(max_width);
      run.clear();
      Ok(())
    };

    let mut inline_child_debug: Vec<(usize, Display)> = Vec::new();
    let mut deadline_counter = 0usize;
    for child in &box_node.children {
      if let Err(RenderError::Timeout { elapsed, .. }) =
        check_active_periodic(&mut deadline_counter, 64, RenderStage::Layout)
      {
        return Err(LayoutError::Timeout { elapsed });
      }
      if is_out_of_flow(child) {
        continue;
      }

      // Floats are out-of-flow for intrinsic sizing; they shouldn't contribute to the
      // parent’s min/max-content inline size.
      if child.style.float.is_floating() {
        continue;
      }

      let treated_as_block = match child.box_type {
        BoxType::Replaced(_) if child.style.display.is_inline_level() => false,
        _ => child.is_block_level(),
      };

      if treated_as_block {
        flush_inline_run(
          &mut inline_run,
          &mut inline_min_width,
          &mut inline_max_width,
        )?;

        // Block-level in-flow children contribute their own intrinsic widths.
        let fc_type = child
          .formatting_context()
          .unwrap_or(FormattingContextType::Block);
        // Avoid going through `factory.get(FormattingContextType::Block)` for block children:
        // `FormattingContextFactory::block_context` constructs a BlockFormattingContext backed by a
        // `detached()` factory clone (to avoid factory↔cached-FC Arc cycles). If we used `get(Block)`
        // recursively we'd create a new detached factory per block depth during intrinsic sizing,
        // which is exactly the kind of allocation churn tables can amplify.
        let (child_min, child_max) = if fc_type == FormattingContextType::Block {
          self.compute_intrinsic_inline_sizes(child)?
        } else {
          factory.get(fc_type).compute_intrinsic_inline_sizes(child)?
        };
        block_min_width = block_min_width.max(child_min);
        block_max_width = block_max_width.max(child_max);
        if log_children {
          let sel = child
            .debug_info
            .as_ref()
            .map(|d| d.to_selector())
            .unwrap_or_else(|| "<anon>".to_string());
          let disp = child.style.display;
          eprintln!(
            "[intrinsic-child] parent_id={} child_id={} selector={} display={:?} min={:.2} max={:.2}",
            box_node.id, child.id, sel, disp, child_min, child_max
          );
        }
      } else {
        if log_children {
          inline_child_debug.push((child.id, child.style.display));
        }
        inline_run.push(child);
      }
    }
    flush_inline_run(
      &mut inline_run,
      &mut inline_min_width,
      &mut inline_max_width,
    )?;

    let min_content_width = inline_min_width.max(block_min_width);
    let max_content_width = inline_max_width.max(block_max_width);

    // Add this box's own padding and borders.
    let mut min_width = min_content_width + edges;
    let mut max_width = max_content_width + edges;

    // Apply min/max constraints to the border box.
    let min_constraint = style
      .min_width
      .map(|l| resolve_length_for_width(l, 0.0, style, &self.font_context, self.viewport_size))
      .map(|w| border_size_from_box_sizing(w, edges, style.box_sizing))
      .unwrap_or(0.0);
    let max_constraint = style
      .max_width
      .map(|l| resolve_length_for_width(l, 0.0, style, &self.font_context, self.viewport_size))
      .map(|w| border_size_from_box_sizing(w, edges, style.box_sizing))
      .unwrap_or(f32::INFINITY);
    let (min_constraint, max_constraint) = if max_constraint < min_constraint {
      (min_constraint, min_constraint)
    } else {
      (min_constraint, max_constraint)
    };
    min_width = crate::layout::utils::clamp_with_order(min_width, min_constraint, max_constraint);
    max_width = crate::layout::utils::clamp_with_order(max_width, min_constraint, max_constraint);

    let clamped_min = min_width.max(0.0);
    let clamped_max = max_width.max(0.0);

    // Optional tracing for over-large intrinsic widths.
    if !log_ids.is_empty() && log_ids.contains(&box_node.id) {
      let selector = box_node
        .debug_info
        .as_ref()
        .map(|d| d.to_selector())
        .unwrap_or_else(|| "<anon>".to_string());
      if !inline_child_debug.is_empty() {
        eprintln!(
          "[intrinsic-inline-children] parent_id={} ids={:?}",
          box_node.id, inline_child_debug
        );
      }
      eprintln!(
        "[intrinsic-widths] id={} selector={} inline_min={:.2} inline_max={:.2} block_min={:.2} block_max={:.2} edges={:.2} min={:.2} max={:.2} result_min={:.2} result_max={:.2}",
        box_node.id,
        selector,
        inline_min_width,
        inline_max_width,
        block_min_width,
        block_max_width,
        edges,
        min_constraint,
        max_constraint,
        clamped_min,
        clamped_max
      );
    }

    intrinsic_cache_store(box_node, IntrinsicSizingMode::MinContent, clamped_min);
    intrinsic_cache_store(box_node, IntrinsicSizingMode::MaxContent, clamped_max);
    Ok((clamped_min, clamped_max))
  }

  fn compute_intrinsic_inline_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style = style_override.as_ref().unwrap_or(&box_node.style);
    if style.containment.isolates_inline_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      if axis.auto {
        let (min, max) = self.compute_intrinsic_inline_sizes(box_node)?;
        return Ok(match mode {
          IntrinsicSizingMode::MinContent => min,
          IntrinsicSizingMode::MaxContent => max,
        });
      }
    }

    if let Some(cached) = intrinsic_cache_lookup(box_node, mode) {
      count_block_intrinsic_call();
      return Ok(cached);
    }
    // For blocks, computing min/max-content widths shares most of the work (inline item
    // collection, shaping, descendant traversal). When we're missing a single intrinsic mode,
    // compute and cache both to avoid an immediate second pass from grid/flex track sizing.
    let (min, max) = self.compute_intrinsic_inline_sizes(box_node)?;
    Ok(match mode {
      IntrinsicSizingMode::MinContent => min,
      IntrinsicSizingMode::MaxContent => max,
    })
  }
}

fn convert_fragment_axes(
  mut fragment: FragmentNode,
  parent_inline_size: f32,
  parent_block_size: f32,
  parent_writing_mode: WritingMode,
  parent_direction: crate::style::types::Direction,
) -> FragmentNode {
  if fragment
    .style
    .as_ref()
    .is_some_and(|style| matches!(style.display, Display::TableCell))
  {
    return fragment;
  }

  let style_wm = fragment
    .style
    .as_ref()
    .map(|s| s.writing_mode)
    .unwrap_or(parent_writing_mode);
  let dir = fragment
    .style
    .as_ref()
    .map(|s| s.direction)
    .unwrap_or(parent_direction);
  let _inline_is_horizontal = inline_axis_is_horizontal(style_wm);
  let block_is_horizontal = block_axis_is_horizontal(style_wm);
  let inline_positive = inline_axis_positive(style_wm, dir);
  let block_positive = block_axis_positive(style_wm);

  let logical_inline_start = fragment.bounds.x();
  let logical_block_start = fragment.bounds.y();
  let inline_size = fragment.bounds.width();
  let block_size = fragment.bounds.height();

  if block_is_horizontal {
    // Swap axes: logical block → physical x, logical inline → physical y.
    let phys_x = if block_positive {
      logical_block_start
    } else {
      parent_block_size - logical_block_start - block_size
    };
    let phys_y = if inline_positive {
      logical_inline_start
    } else {
      parent_inline_size - logical_inline_start - inline_size
    };
    fragment.bounds = Rect::from_xywh(phys_x, phys_y, block_size, inline_size);
    let child_inline = inline_size;
    let child_block = block_size;
    let mapped_children: Vec<_> = fragment
      .children
      .into_iter()
      .map(|c| convert_fragment_axes(c, child_inline, child_block, style_wm, dir))
      .collect();
    fragment.children = mapped_children.into();
    fragment
  } else {
    // Keep axes; only recurse.
    let child_inline = inline_size;
    let child_block = block_size;
    let mapped_children: Vec<_> = fragment
      .children
      .into_iter()
      .map(|c| convert_fragment_axes(c, child_inline, child_block, style_wm, dir))
      .collect();
    fragment.children = mapped_children.into();
    fragment
  }
}

/// Checks if a box is out of normal flow (absolute/fixed positioned or float)
fn is_out_of_flow(box_node: &BoxNode) -> bool {
  let position = box_node.style.position;
  box_node.style.running_position.is_some()
    || matches!(position, Position::Absolute | Position::Fixed)
}

fn count_text_fragments(fragment: &FragmentNode) -> (usize, usize) {
  fn walk(node: &FragmentNode, text: &mut usize, total: &mut usize) {
    *total += 1;
    if matches!(node.content, FragmentContent::Text { .. }) {
      *text += 1;
    }
    for child in node.children.iter() {
      walk(child, text, total);
    }
  }

  let mut text = 0;
  let mut total = 0;
  walk(fragment, &mut text, &mut total);
  (text, total)
}

fn collect_first_texts(fragment: &FragmentNode, out: &mut Vec<String>, limit: usize) {
  fn walk(node: &FragmentNode, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
      return;
    }
    if let FragmentContent::Text { text, .. } = &node.content {
      out.push(text.to_string());
      if out.len() >= limit {
        return;
      }
    }
    for child in node.children.iter() {
      walk(child, out, limit);
      if out.len() >= limit {
        return;
      }
    }
  }

  walk(fragment, out, limit);
}

fn trace_positioned_ids() -> Vec<usize> {
  crate::debug::runtime::runtime_toggles()
    .usize_list("FASTR_TRACE_POSITIONED")
    .unwrap_or_default()
}

fn trace_block_text_ids() -> Vec<usize> {
  crate::debug::runtime::runtime_toggles()
    .usize_list("FASTR_TRACE_BLOCK_TEXT")
    .unwrap_or_default()
}

fn resolve_length_for_width(
  length: Length,
  percentage_base: f32,
  style: &ComputedStyle,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
) -> f32 {
  let base = if percentage_base.is_finite() {
    Some(percentage_base)
  } else {
    None
  };
  resolve_length_with_percentage_metrics(
    length,
    base,
    viewport,
    style.font_size,
    style.root_font_size,
    Some(style),
    Some(font_context),
  )
  .unwrap_or(0.0)
}

fn horizontal_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
) -> f32 {
  let mut total = resolve_length_for_width(
    style.padding_left,
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.padding_right,
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.used_border_left_width(),
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.used_border_right_width(),
    percentage_base,
    style,
    font_context,
    viewport,
  );

  let reserve_vertical_gutter = matches!(style.overflow_y, Overflow::Scroll)
    || (style.scrollbar_gutter.stable
      && matches!(style.overflow_y, Overflow::Auto | Overflow::Scroll));
  if reserve_vertical_gutter {
    let gutter = resolve_scrollbar_width(style);
    if gutter > 0.0 {
      total += gutter;
      if style.scrollbar_gutter.both_edges {
        total += gutter;
      }
    }
  }

  total
}

fn vertical_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
) -> f32 {
  let mut total = resolve_length_for_width(
    style.padding_top,
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.padding_bottom,
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.used_border_top_width(),
    percentage_base,
    style,
    font_context,
    viewport,
  ) + resolve_length_for_width(
    style.used_border_bottom_width(),
    percentage_base,
    style,
    font_context,
    viewport,
  );

  let reserve_horizontal_gutter = matches!(style.overflow_x, Overflow::Scroll)
    || (style.scrollbar_gutter.stable
      && matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll));
  if reserve_horizontal_gutter {
    let gutter = resolve_scrollbar_width(style);
    if gutter > 0.0 {
      total += gutter;
      if style.scrollbar_gutter.both_edges {
        total += gutter;
      }
    }
  }

  total
}

fn inline_axis_padding_and_borders(
  style: &ComputedStyle,
  percentage_base: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
) -> f32 {
  if inline_axis_is_horizontal(style.writing_mode) {
    horizontal_padding_and_borders(style, percentage_base, viewport, font_context)
  } else {
    vertical_padding_and_borders(style, percentage_base, viewport, font_context)
  }
}

fn rebase_intrinsic_border_box_size(base: f32, edges_base: f32, edges_actual: f32) -> f32 {
  (base - edges_base + edges_actual).max(0.0)
}

fn compute_intrinsic_block_sizes_without_block_size_constraints(
  fc: &dyn FormattingContext,
  box_node: &BoxNode,
) -> Result<(f32, f32), LayoutError> {
  let style_override = crate::layout::style_override::style_override_for(box_node.id);
  let style = style_override.as_ref().unwrap_or(&box_node.style);
  let mut probe_style: ComputedStyle = (**style).clone();
  probe_style.height = None;
  probe_style.height_keyword = None;
  probe_style.min_height = None;
  probe_style.min_height_keyword = None;
  probe_style.max_height = None;
  probe_style.max_height_keyword = None;

  let compute = |node: &BoxNode| -> Result<(f32, f32), LayoutError> {
    let min = match fc.compute_intrinsic_block_size(node, IntrinsicSizingMode::MinContent) {
      Ok(value) => value,
      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
      Err(_) => 0.0,
    };
    let max = match fc.compute_intrinsic_block_size(node, IntrinsicSizingMode::MaxContent) {
      Ok(value) => value,
      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
      Err(_) => min,
    };
    Ok((min, max))
  };

  if box_node.id != 0 {
    crate::layout::style_override::with_style_override(box_node.id, Arc::new(probe_style), || {
      compute(box_node)
    })
  } else {
    let mut cloned = box_node.clone();
    cloned.style = Arc::new(probe_style);
    compute(&cloned)
  }
}

fn recompute_margins_for_width(
  style: &ComputedStyle,
  containing_width: f32,
  content_width: f32,
  border_left: f32,
  padding_left: f32,
  padding_right: f32,
  border_right: f32,
  viewport: crate::geometry::Size,
  font_context: &FontContext,
) -> (f32, f32) {
  let margin_left = match &style.margin_left {
    Some(len) => MarginValue::Length(resolve_length_for_width(
      *len,
      containing_width,
      style,
      font_context,
      viewport,
    )),
    None => MarginValue::Auto,
  };
  let margin_right = match &style.margin_right {
    Some(len) => MarginValue::Length(resolve_length_for_width(
      *len,
      containing_width,
      style,
      font_context,
      viewport,
    )),
    None => MarginValue::Auto,
  };

  let borders_and_padding = border_left + padding_left + padding_right + border_right;

  match (margin_left, margin_right) {
    (MarginValue::Auto, MarginValue::Auto) => {
      let remaining = containing_width - borders_and_padding - content_width;
      let margin = (remaining / 2.0).max(0.0);
      (margin, margin)
    }
    (MarginValue::Auto, MarginValue::Length(mr)) => {
      let ml = containing_width - borders_and_padding - content_width - mr;
      (ml, mr)
    }
    (MarginValue::Length(ml), MarginValue::Auto) => {
      let mr = containing_width - borders_and_padding - content_width - ml;
      (ml, mr)
    }
    (MarginValue::Length(ml), MarginValue::Length(_mr)) => {
      let mr = containing_width - borders_and_padding - content_width - ml;
      (ml, mr)
    }
  }
}
fn resolve_margin_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.margin_top,
    PhysicalSide::Right => style.margin_right,
    PhysicalSide::Bottom => style.margin_bottom,
    PhysicalSide::Left => style.margin_left,
  };
  length
    .map(|l| resolve_length_for_width(l, percentage_base, style, font_context, viewport))
    .unwrap_or(0.0)
}

fn resolve_padding_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.padding_top,
    PhysicalSide::Right => style.padding_right,
    PhysicalSide::Bottom => style.padding_bottom,
    PhysicalSide::Left => style.padding_left,
  };
  resolve_length_for_width(length, percentage_base, style, font_context, viewport)
}

fn resolve_border_side(
  style: &ComputedStyle,
  side: PhysicalSide,
  percentage_base: f32,
  font_context: &FontContext,
  viewport: crate::geometry::Size,
) -> f32 {
  let length = match side {
    PhysicalSide::Top => style.used_border_top_width(),
    PhysicalSide::Right => style.used_border_right_width(),
    PhysicalSide::Bottom => style.used_border_bottom_width(),
    PhysicalSide::Left => style.used_border_left_width(),
  };
  resolve_length_for_width(length, percentage_base, style, font_context, viewport)
}
#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Transform;
  use crate::debug::runtime;
  use crate::layout::contexts::inline::InlineFormattingContext;
  use crate::layout::formatting_context::IntrinsicSizingMode;
  use crate::style::display::Display;
  use crate::style::display::FormattingContextType;
  use crate::style::position::Position;
  use crate::style::types::BorderStyle;
  use crate::style::types::ContentVisibility;
  use crate::style::types::IntrinsicSizeKeyword;
  use crate::style::types::ListStylePosition;
  use crate::style::types::ListStyleType;
  use crate::style::types::Overflow;
  use crate::style::types::ScrollbarWidth;
  use crate::style::types::WritingMode;
  use crate::style::values::Length;
  use crate::style::ComputedStyle;
  use crate::text::font_loader::FontContext;
  use crate::tree::box_generation_demo::BoxGenerator;
  use crate::tree::box_generation_demo::DOMNode;
  use crate::tree::box_tree::BoxTree;
  use crate::tree::fragment_tree::FragmentContent;
  use std::collections::HashMap;
  use std::sync::Arc;

  fn default_style() -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    Arc::new(style)
  }

  fn block_style_with_height(height: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.height = Some(Length::px(height));
    style.height_keyword = None;
    Arc::new(style)
  }

  fn content_visibility_test_guard() -> runtime::ThreadRuntimeTogglesGuard {
    // Keep content-visibility:auto tests deterministic even when developers have FASTR_* env vars
    // set locally (e.g. activation margin experiments).
    runtime::set_thread_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(HashMap::from(
      [(
        "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
        "0".to_string(),
      )],
    ))))
  }

  fn inline_canvas(id: usize, width: f32, height: f32) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;
    let mut node = BoxNode::new_replaced(
      Arc::new(style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(width, height)),
      None,
    );
    node.id = id;
    node
  }

  #[test]
  fn width_max_content_rebases_percent_padding_and_borders() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width_keyword = Some(crate::style::types::IntrinsicSizeKeyword::MaxContent);
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let intrinsic_max_base0 = fc
      .compute_intrinsic_inline_size(&child, IntrinsicSizingMode::MaxContent)
      .unwrap();
    let edges_base0 = inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context);
    let expected_border_box =
      rebase_intrinsic_border_box_size(intrinsic_max_base0, edges_base0, edges_actual);

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
  }

  #[test]
  fn width_fit_content_function_clamps_between_min_and_max_content() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width_keyword = Some(crate::style::types::IntrinsicSizeKeyword::FitContent {
      limit: Some(Length::px(50.0)),
    });
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "word ".repeat(20));
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (min_base0, max_base0) = fc.compute_intrinsic_inline_sizes(&child).unwrap();
    let edges_base0 = inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context);
    let intrinsic_min = rebase_intrinsic_border_box_size(min_base0, edges_base0, edges_actual);
    let intrinsic_max = rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);
    let limit_border = border_size_from_box_sizing(50.0, edges_actual, child_style.box_sizing);
    let expected_border_box = intrinsic_max.min(intrinsic_min.max(limit_border));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected fit-content border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
  }

  #[test]
  fn max_width_fit_content_caps_auto_width_blocks() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.max_width_keyword =
      Some(crate::style::types::IntrinsicSizeKeyword::FitContent { limit: None });
    child_style.padding_left = Length::percent(10.0);
    child_style.padding_right = Length::px(5.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.margin_left = Some(Length::px(0.0));
    child_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "short".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (min_base0, max_base0) = fc.compute_intrinsic_inline_sizes(&child).unwrap();
    let edges_base0 = inline_axis_padding_and_borders(&child_style, 0.0, viewport, &font_context);
    let edges_actual =
      inline_axis_padding_and_borders(&child_style, 300.0, viewport, &font_context);
    let intrinsic_min = rebase_intrinsic_border_box_size(min_base0, edges_base0, edges_actual);
    let intrinsic_max = rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);
    let expected_border_box = intrinsic_max.min(300.0_f32.max(intrinsic_min));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - expected_border_box).abs() < 0.5,
      "expected capped border-box width {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.width()
    );
    assert!(
      child_frag.bounds.width() < 299.0,
      "expected max-width:fit-content to shrink below the containing width; got {:.2}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn max_width_fit_content_clamps_explicit_width_against_available_space() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut base_style = ComputedStyle::default();
    base_style.display = Display::Block;
    base_style.margin_left = Some(Length::px(0.0));
    base_style.margin_right = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "hello world goodbye".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);

    let intrinsic_child = BoxNode::new_block(
      Arc::new(base_style.clone()),
      FormattingContextType::Block,
      vec![inline.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let (min_border, max_border) = fc.compute_intrinsic_inline_sizes(&intrinsic_child).unwrap();
    assert!(
      min_border + 0.5 < 80.0 && max_border > 80.0 + 0.5,
      "expected intrinsic widths to straddle 80px (min={min_border:.2}, max={max_border:.2})",
    );

    let mut child_style = base_style;
    child_style.width = Some(Length::px(500.0));
    child_style.width_keyword = None;
    child_style.max_width = None;
    child_style.max_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

    let child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite_width(80.0))
      .unwrap();
    assert!(
      (fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected parent width to be 80px, got {:.2}",
      fragment.bounds.width()
    );
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-width:fit-content to clamp explicit width to 80px, got {:.2}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn height_max_content_rebases_percent_padding_and_borders() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height_keyword = Some(crate::style::types::IntrinsicSizeKeyword::MaxContent);
    // Percentage vertical padding uses the containing block inline size as its base (CSS2.1 10.5),
    // which is also the key edge case for intrinsic size rebasing.
    child_style.padding_top = Length::percent(10.0);
    child_style.padding_bottom = Length::px(5.0);
    child_style.border_top_width = Length::px(2.0);
    child_style.border_bottom_width = Length::px(2.0);
    child_style.margin_top = Some(Length::px(0.0));
    child_style.margin_bottom = Some(Length::px(0.0));

    let text = BoxNode::new_text(default_style(), "word ".repeat(40));
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let child = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child.clone()],
    );

    let viewport = Size::new(300.0, 200.0);
    let font_context = FontContext::new();
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      font_context.clone(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(300.0);

    let (_min_base0, max_base0) =
      compute_intrinsic_block_sizes_without_block_size_constraints(&fc, &child).unwrap();
    let edges_base0 = vertical_padding_and_borders(&child_style, 0.0, viewport, &font_context);
    let edges_actual = vertical_padding_and_borders(&child_style, 300.0, viewport, &font_context);
    let expected_border_box =
      rebase_intrinsic_border_box_size(max_base0, edges_base0, edges_actual);

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.height() - expected_border_box).abs() < 0.5,
      "expected border-box height {:.2}, got {:.2}",
      expected_border_box,
      child_frag.bounds.height()
    );
  }

  #[test]
  fn max_width_clamps_and_centers_in_flow_blocks() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(20.0));
    child_style.box_sizing = crate::style::types::BoxSizing::BorderBox;
    child_style.padding_left = Length::px(10.0);
    child_style.padding_right = Length::px(10.0);
    child_style.border_left_style = BorderStyle::Solid;
    child_style.border_right_style = BorderStyle::Solid;
    child_style.border_left_width = Length::px(2.0);
    child_style.border_right_width = Length::px(2.0);
    child_style.max_width = Some(Length::px(200.0));
    child_style.margin_left = None;
    child_style.margin_right = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let viewport = Size::new(500.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite_width(500.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);

    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 200.0).abs() < 0.5,
      "expected border-box width 200, got {}",
      child_frag.bounds.width()
    );
    assert!(
      (child_frag.bounds.x() - 150.0).abs() < 0.5,
      "expected centered x=150, got {}",
      child_frag.bounds.x()
    );
  }

  #[test]
  fn block_width_max_content_keyword_uses_intrinsic_inline_size() {
    let child_canvas = inline_canvas(9101, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = None;
    child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9100;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-content width 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn root_width_max_content_keyword_uses_intrinsic_inline_size() {
    let child_canvas = inline_canvas(9401, 80.0, 20.0);

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = None;
    root_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();
    assert!(
      (fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected root max-content width 80, got {}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn block_max_width_max_content_keyword_clamps_width() {
    let child_canvas = inline_canvas(9201, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(150.0));
    child_style.width_keyword = None;
    child_style.max_width = None;
    child_style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9200;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected max-width:max-content to clamp to 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn block_min_width_max_content_keyword_clamps_width() {
    let child_canvas = inline_canvas(9301, 80.0, 20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.width = Some(Length::px(50.0));
    child_style.width_keyword = None;
    child_style.min_width = None;
    child_style.min_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let mut child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![child_canvas],
    );
    child.id = 9300;

    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert!(
      (child_frag.bounds.width() - 80.0).abs() < 0.5,
      "expected min-width:max-content to expand to 80, got {}",
      child_frag.bounds.width()
    );
  }

  #[test]
  fn percent_heights_resolve_with_used_height_override() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_top = Length::px(10.0);
    parent_style.padding_bottom = Length::px(10.0);
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_bottom_style = BorderStyle::Solid;
    parent_style.border_top_width = Length::px(5.0);
    parent_style.border_bottom_width = Length::px(5.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let viewport = Size::new(300.0, 300.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Indefinite)
        .with_used_border_box_size(None, Some(200.0));

    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert!(
      (fragment.bounds.height() - 200.0).abs() < 0.5,
      "expected parent border-box height 200, got {}",
      fragment.bounds.height()
    );
    assert_eq!(fragment.children.len(), 1);

    // Parent vertical edges = 5 + 10 + 10 + 5 = 30px, so the used content height is 170px.
    // The child has height:50%, so it should resolve to 85px.
    assert!(
      (fragment.children[0].bounds.height() - 85.0).abs() < 0.5,
      "expected child height ~85, got {}",
      fragment.children[0].bounds.height()
    );
  }

  #[test]
  fn block_children_are_offset_by_parent_padding_and_border() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);
    parent_style.border_left_style = BorderStyle::Solid;
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_left_width = Length::px(5.0);
    parent_style.border_top_width = Length::px(2.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(10.0));

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child = &fragment.children[0];
    assert!(
      (child.bounds.x() - 15.0).abs() < 0.01,
      "expected child x≈15px, got {}",
      child.bounds.x()
    );
    assert!(
      (child.bounds.y() - 22.0).abs() < 0.01,
      "expected child y≈22px, got {}",
      child.bounds.y()
    );
  }

  #[test]
  fn horizontal_scrollbar_reserves_gutter_height() {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.overflow_x = Overflow::Scroll;
    style.scrollbar_width = ScrollbarWidth::Thin;

    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      Size::new(200.0, 200.0),
      ContainingBlock::viewport(Size::new(200.0, 200.0)),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&node, &constraints).unwrap();

    assert!((fragment.bounds.height() - 8.0).abs() < 0.01);
  }

  #[test]
  fn padding_offsets_in_flow_children() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(30.0));

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_fragment = &fragment.children[0];
    assert!(
      (child_fragment.bounds.x() - 10.0).abs() < 0.01,
      "expected child x≈10, got {}",
      child_fragment.bounds.x()
    );
    assert!(
      (child_fragment.bounds.y() - 20.0).abs() < 0.01,
      "expected child y≈20, got {}",
      child_fragment.bounds.y()
    );
  }

  #[test]
  fn padding_offsets_children_of_in_flow_blocks() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(20.0);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(30.0));

    let grandchild =
      BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![grandchild],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&root, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    assert_eq!(fragment.children[0].children.len(), 1);
    let grandchild_fragment = &fragment.children[0].children[0];
    assert!(
      (grandchild_fragment.bounds.x() - 10.0).abs() < 0.01,
      "expected grandchild x≈10, got {}",
      grandchild_fragment.bounds.x()
    );
    assert!(
      (grandchild_fragment.bounds.y() - 20.0).abs() < 0.01,
      "expected grandchild y≈20, got {}",
      grandchild_fragment.bounds.y()
    );
  }

  #[test]
  fn content_visibility_auto_skips_after_remembered_size() {
    let _toggles_guard = content_visibility_test_guard();
    let _cache_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::remembered_size_cache_clear();

    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let spacer_style = block_style_with_height(300.0);
    let spacer = BoxNode::new_block(spacer_style, FormattingContextType::Block, vec![]);

    let mut content_child_style = ComputedStyle::default();
    content_child_style.display = Display::Block;
    content_child_style.height = Some(Length::px(50.0));
    let content_child = BoxNode::new_block(
      Arc::new(content_child_style),
      FormattingContextType::Block,
      vec![],
    );

    let mut cv_style = ComputedStyle::default();
    cv_style.display = Display::Block;
    cv_style.content_visibility = ContentVisibility::Auto;
    let cv_node = BoxNode::new_block(
      Arc::new(cv_style),
      FormattingContextType::Block,
      vec![content_child],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, cv_node],
    );
    let tree = BoxTree::new(root);

    // Pass #1: offscreen heuristic is satisfied, but there is no definite placeholder size yet, so
    // we must NOT skip descendant layout.
    let frag1 = fc.layout(&tree.root, &constraints).expect("layout pass #1");
    assert_eq!(frag1.children.len(), 2);
    let cv_frag1 = &frag1.children[1];
    assert_eq!(
      cv_frag1.children.len(),
      1,
      "first pass should lay out descendants to establish remembered size"
    );
    assert!(
      (cv_frag1.bounds.height() - 50.0).abs() < 0.1,
      "expected cv:auto placeholder height to match laid-out content on pass #1"
    );

    // Pass #2: the element now has a remembered size, so it can skip layout and use that size as a
    // definite placeholder.
    let frag2 = fc.layout(&tree.root, &constraints).expect("layout pass #2");
    assert_eq!(frag2.children.len(), 2);
    let cv_frag2 = &frag2.children[1];
    assert_eq!(
      cv_frag2.children.len(),
      0,
      "second pass should skip descendant layout using remembered size"
    );
    assert!(
      (cv_frag2.bounds.height() - 50.0).abs() < 0.1,
      "expected cv:auto placeholder height to come from remembered size on pass #2"
    );

    crate::layout::formatting_context::remembered_size_cache_clear();
  }

  fn block_style_with_margin(margin: f32) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.margin_top = Some(Length::px(margin));
    style.margin_bottom = Some(Length::px(margin));
    style.margin_left = Some(Length::px(margin));
    style.margin_right = Some(Length::px(margin));
    Arc::new(style)
  }

  #[test]
  fn vertical_writing_blocks_stack_horizontally() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.writing_mode = WritingMode::VerticalRl;
    root_style.width = Some(Length::px(200.0));
    root_style.width_keyword = None;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.writing_mode = WritingMode::VerticalRl;
    child_style.height = Some(Length::px(40.0));
    child_style.height_keyword = None;

    let child1 = BoxNode::new_block(
      Arc::new(child_style.clone()),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child1, child2],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    let root_w = fragment.bounds.width();
    let root_h = fragment.bounds.height();
    // Block axis is horizontal; total block extent should be sum of block sizes (approx 80).
    assert!(
      (root_w - 80.0).abs() < 0.5,
      "expected ~80 width, got {}x{}",
      root_w,
      root_h
    );
    assert!(
      (root_h - 200.0).abs() < 0.5,
      "expected height 200, got {}x{}",
      root_w,
      root_h
    );
    assert_eq!(fragment.children.len(), 2);

    let first = &fragment.children[0];
    let second = &fragment.children[1];

    // Children are transposed: physical width = block size (40), height = inline size (200).
    assert!((first.bounds.width() - 40.0).abs() < 0.5);
    assert!((first.bounds.height() - 200.0).abs() < 0.5);
    assert!((second.bounds.width() - 40.0).abs() < 0.5);
    assert!((second.bounds.height() - 200.0).abs() < 0.5);

    // vertical-rl stacks from right to left (block-axis negative).
    assert!(first.bounds.x() > second.bounds.x());
    assert!((first.bounds.x() - 40.0).abs() < 0.5);
    assert!((second.bounds.x()).abs() < 0.5);
    assert!((first.bounds.y()).abs() < 0.5);
    assert!((second.bounds.y()).abs() < 0.5);
  }

  #[test]
  fn test_bfc_new() {
    let _bfc = BlockFormattingContext::new();
  }

  #[test]
  fn test_layout_empty_block() {
    let bfc = BlockFormattingContext::new();
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 800.0);
    assert_eq!(fragment.bounds.height(), 0.0);
  }

  #[test]
  fn test_layout_block_with_explicit_height() {
    let bfc = BlockFormattingContext::new();
    let root = BoxNode::new_block(
      block_style_with_height(200.0),
      FormattingContextType::Block,
      vec![],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 800.0);
    assert_eq!(fragment.bounds.height(), 200.0);
  }

  #[test]
  fn multicol_column_span_all_fragment_positions_are_stable() {
    fn block_with_id(id: usize, style: Arc<ComputedStyle>) -> BoxNode {
      let mut node = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
      node.id = id;
      node
    }

    fn fragments_with_id<'a>(root: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
      fn walk<'a>(node: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
        if let FragmentContent::Block {
          box_id: Some(box_id),
        } = &node.content
        {
          if *box_id == id {
            out.push(node);
          }
        }
        for child in node.children.iter() {
          walk(child, id, out);
        }
      }

      let mut out = Vec::new();
      walk(root, id, &mut out);
      out
    }

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(2);
    multicol_style.column_gap = Length::px(20.0);
    let multicol_style = Arc::new(multicol_style);

    let child_style = block_style_with_height(20.0);
    let span_style = {
      let mut style = (*block_style_with_height(10.0)).clone();
      style.column_span = ColumnSpan::All;
      Arc::new(style)
    };

    let mut multicol = BoxNode::new_block(
      multicol_style,
      FormattingContextType::Block,
      vec![
        block_with_id(5001, child_style.clone()),
        block_with_id(5002, child_style.clone()),
        block_with_id(5003, child_style.clone()),
        block_with_id(5004, child_style.clone()),
        block_with_id(5005, span_style),
        block_with_id(5006, child_style.clone()),
        block_with_id(5007, child_style.clone()),
        block_with_id(5008, child_style.clone()),
        block_with_id(5009, child_style),
      ],
    );
    multicol.id = 5000;

    let fc = BlockFormattingContext::new();
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let fragment = fc.layout(&multicol, &constraints).expect("multicol layout");

    let col_width = (200.0 - 20.0) / 2.0;
    let col2_x = col_width + 20.0;
    let segment_height = 40.0;
    let span_height = 10.0;
    let after_y = segment_height + span_height;

    let expected = [
      (5001, 0.0, 0.0, col_width, 20.0),
      (5002, 0.0, 20.0, col_width, 20.0),
      (5003, col2_x, 0.0, col_width, 20.0),
      (5004, col2_x, 20.0, col_width, 20.0),
      (5005, 0.0, segment_height, 200.0, span_height),
      (5006, 0.0, after_y, col_width, 20.0),
      (5007, 0.0, after_y + 20.0, col_width, 20.0),
      (5008, col2_x, after_y, col_width, 20.0),
      (5009, col2_x, after_y + 20.0, col_width, 20.0),
    ];

    for (id, x, y, w, h) in expected {
      let hits = fragments_with_id(&fragment, id);
      assert_eq!(
        hits.len(),
        1,
        "expected exactly one fragment for box_id={}",
        id
      );
      let frag = hits[0];
      assert!(
        (frag.bounds.x() - x).abs() < 0.1,
        "box_id={} expected x≈{}, got {}",
        id,
        x,
        frag.bounds.x()
      );
      assert!(
        (frag.bounds.y() - y).abs() < 0.1,
        "box_id={} expected y≈{}, got {}",
        id,
        y,
        frag.bounds.y()
      );
      assert!(
        (frag.bounds.width() - w).abs() < 0.1,
        "box_id={} expected width≈{}, got {}",
        id,
        w,
        frag.bounds.width()
      );
      assert!(
        (frag.bounds.height() - h).abs() < 0.1,
        "box_id={} expected height≈{}, got {}",
        id,
        h,
        frag.bounds.height()
      );
    }
  }

  #[test]
  fn multicol_segment_offset_skips_offscreen_auto() {
    let _guard = content_visibility_test_guard();

    fn block_with_id(id: usize, style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
      let mut node = BoxNode::new_block(style, FormattingContextType::Block, children);
      node.id = id;
      node
    }

    fn fragments_with_id<'a>(root: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
      fn walk<'a>(node: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
        if let FragmentContent::Block {
          box_id: Some(box_id),
        } = &node.content
        {
          if *box_id == id {
            out.push(node);
          }
        }
        for child in node.children.iter() {
          walk(child, id, out);
        }
      }

      let mut out = Vec::new();
      walk(root, id, &mut out);
      out
    }

    let viewport = Size::new(200.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(2);
    let multicol_style = Arc::new(multicol_style);

    let seg1 = block_with_id(6001, block_style_with_height(200.0), vec![]);
    let span_all = block_with_id(
      6002,
      {
        let mut style = (*block_style_with_height(10.0)).clone();
        style.column_span = ColumnSpan::All;
        Arc::new(style)
      },
      vec![],
    );

    let inner_child = block_with_id(6004, block_style_with_height(60.0), vec![]);
    let auto = block_with_id(
      6003,
      {
        let mut style = (*block_style_with_height(60.0)).clone();
        style.content_visibility = ContentVisibility::Auto;
        Arc::new(style)
      },
      vec![inner_child],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![BoxNode::new_block(
        multicol_style,
        FormattingContextType::Block,
        vec![seg1, span_all, auto],
      )],
    );

    let fragment = fc.layout(&root, &constraints).unwrap();

    let auto_fragments = fragments_with_id(&fragment, 6003);
    assert!(
      !auto_fragments.is_empty(),
      "expected to find fragment(s) for box_id=6003"
    );
    assert!(
      auto_fragments.iter().all(|frag| frag.children.is_empty()),
      "expected content-visibility:auto descendants to be skipped when the segment is offscreen"
    );
  }

  #[test]
  fn multicol_column_count_width_resolves_used_count() {
    let fc = BlockFormattingContext::new();

    let mut multicol_style = ComputedStyle::default();
    multicol_style.display = Display::Block;
    multicol_style.column_count = Some(3);
    multicol_style.column_width = Some(Length::px(250.0));
    multicol_style.column_gap = Length::px(40.0);

    let (count, width, gap) = fc.compute_column_geometry(&multicol_style, 600.0);
    assert_eq!(count, 2);
    assert!((gap - 40.0).abs() < 0.1);
    assert!((width - 280.0).abs() < 0.1);
  }

  #[test]
  fn legend_auto_width_shrinks_to_content() {
    let mut legend_style = ComputedStyle::default();
    legend_style.display = Display::Block;
    legend_style.shrink_to_fit_inline_size = true;

    let mut legend_child_style = ComputedStyle::default();
    legend_child_style.display = Display::Block;
    legend_child_style.width = Some(Length::px(80.0));
    legend_child_style.height = Some(Length::px(10.0));
    legend_child_style.width_keyword = None;
    legend_child_style.height_keyword = None;
    let legend_child = BoxNode::new_block(
      Arc::new(legend_child_style),
      FormattingContextType::Block,
      vec![],
    );

    let legend = BoxNode::new_block(
      Arc::new(legend_style),
      FormattingContextType::Block,
      vec![legend_child],
    );

    let mut sibling_style = ComputedStyle::default();
    sibling_style.display = Display::Block;
    let sibling = BoxNode::new_block(
      Arc::new(sibling_style),
      FormattingContextType::Block,
      vec![],
    );

    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      Size::new(200.0, 200.0),
      ContainingBlock::viewport(Size::new(200.0, 200.0)),
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
    let solo = fc.layout(&legend, &constraints).expect("legend layout");
    assert!(
      (solo.bounds.width() - 80.0).abs() < 0.1,
      "legend root should shrink to its contents; got width {}",
      solo.bounds.width()
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![legend, sibling],
    );
    let fragment = fc.layout(&root, &constraints).expect("block layout");

    assert_eq!(
      fragment.children.len(),
      2,
      "root should produce two children"
    );
    let legend_fragment = &fragment.children[0];
    assert!(
      legend_fragment
        .style
        .as_ref()
        .map(|s| s.shrink_to_fit_inline_size)
        .unwrap_or(false),
      "legend fragment should carry shrink-to-fit flag"
    );
    assert!(
      (legend_fragment.bounds.width() - 80.0).abs() < 0.1,
      "legend should shrink to its contents; got width {}",
      legend_fragment.bounds.width()
    );
    assert!(
      legend_fragment.bounds.x().abs() < 0.01,
      "legend should start at the origin"
    );

    let sibling_fragment = &fragment.children[1];
    assert!(
      (sibling_fragment.bounds.width() - 200.0).abs() < 0.1,
      "normal block should span the containing width; got width {}",
      sibling_fragment.bounds.width()
    );
    assert!(
      sibling_fragment.bounds.x().abs() < 0.01,
      "sibling should start at the container origin; got {}",
      sibling_fragment.bounds.x()
    );
  }

  #[test]
  fn test_layout_nested_blocks() {
    let bfc = BlockFormattingContext::new();

    let child1 = BoxNode::new_block(
      block_style_with_height(100.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      block_style_with_height(150.0),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![child1, child2],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
    assert!(fragment.bounds.height() >= 250.0);
  }

  #[test]
  fn relative_block_offsets_fragment_without_affecting_flow_size() {
    let mut relative_style = ComputedStyle::default();
    relative_style.display = Display::Block;
    relative_style.position = Position::Relative;
    relative_style.left = crate::style::types::InsetValue::Length(Length::px(30.0));
    relative_style.top = crate::style::types::InsetValue::Length(Length::px(20.0));
    relative_style.width = Some(Length::px(100.0));
    relative_style.height = Some(Length::px(40.0));
    relative_style.width_keyword = None;
    relative_style.height_keyword = None;

    let child = BoxNode::new_block(
      Arc::new(relative_style),
      FormattingContextType::Block,
      vec![],
    );
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let constraints = LayoutConstraints::definite(300.0, 200.0);

    let fragment = BlockFormattingContext::new()
      .layout(&root, &constraints)
      .unwrap();

    assert_eq!(fragment.bounds.height(), 40.0);
    let child_fragment = fragment.children.first().expect("child");
    assert_eq!(child_fragment.bounds.width(), 100.0);
    assert_eq!(child_fragment.bounds.height(), 40.0);
    assert_eq!(child_fragment.bounds.x(), 30.0);
    assert_eq!(child_fragment.bounds.y(), 20.0);
  }

  #[test]
  fn relative_block_percentage_offsets_use_containing_block() {
    let mut relative_style = ComputedStyle::default();
    relative_style.display = Display::Block;
    relative_style.position = Position::Relative;
    relative_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0)); // 50% of 200 = 100
    relative_style.top = crate::style::types::InsetValue::Length(Length::percent(25.0)); // 25% of 120 = 30
    relative_style.width = Some(Length::px(40.0));
    relative_style.height = Some(Length::px(10.0));
    relative_style.width_keyword = None;
    relative_style.height_keyword = None;

    let child = BoxNode::new_block(
      Arc::new(relative_style),
      FormattingContextType::Block,
      vec![],
    );
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.height = Some(Length::px(120.0));
    root_style.height_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = BlockFormattingContext::new()
      .layout(&root, &constraints)
      .unwrap();

    let child_fragment = fragment.children.first().expect("child");
    assert_eq!(child_fragment.bounds.x(), 100.0);
    assert_eq!(child_fragment.bounds.y(), 30.0);
  }

  #[test]
  fn percentage_height_uses_definite_containing_block() {
    let bfc = BlockFormattingContext::new();

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.height = Some(Length::px(300.0));
    root_style.height_keyword = None;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert!((child_fragment.bounds.height() - 150.0).abs() < 0.1);
  }

  #[test]
  fn aspect_ratio_sets_auto_height() {
    let bfc = BlockFormattingContext::new();

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.aspect_ratio = crate::style::types::AspectRatio::Ratio(2.0);
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![child],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert_eq!(child_fragment.bounds.height(), 100.0);
  }

  #[test]
  fn percentage_height_without_base_falls_back_to_auto() {
    let bfc = BlockFormattingContext::new();
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::percent(60.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);
    let constraints = LayoutConstraints::definite_width(200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let child_fragment = fragment.children.first().expect("child fragment");
    assert_eq!(child_fragment.bounds.height(), 0.0);
  }

  #[test]
  fn test_sibling_margin_collapse() {
    let bfc = BlockFormattingContext::new();

    let child1 = BoxNode::new_block(
      block_style_with_margin(20.0),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      block_style_with_margin(30.0),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![child1, child2],
    );
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 2);
  }

  #[test]
  fn test_fc_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BlockFormattingContext>();
  }

  #[test]
  fn floats_extend_height_and_clear_moves_following_block() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(60.0));
    float_style.height = Some(Length::px(50.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let mut cleared_style = ComputedStyle::default();
    cleared_style.display = Display::Block;
    cleared_style.clear = crate::style::float::Clear::Left;
    cleared_style.height = Some(Length::px(10.0));
    cleared_style.height_keyword = None;
    let cleared_node = BoxNode::new_block(
      Arc::new(cleared_style),
      FormattingContextType::Block,
      vec![],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, cleared_node],
    );
    let constraints = LayoutConstraints::definite(200.0, 400.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    assert!(
      fragment.bounds.height() >= 60.0,
      "BFC height should include float and cleared block; got {}",
      fragment.bounds.height()
    );

    let mut float_y = None;
    let mut clear_y = None;
    for child in fragment.children.iter() {
      if let Some(style) = &child.style {
        if style.float.is_floating() {
          float_y = Some(child.bounds.y());
        }
        if matches!(
          style.clear,
          crate::style::float::Clear::Left | crate::style::float::Clear::Both
        ) {
          clear_y = Some(child.bounds.y());
        }
      }
    }

    let float_y = float_y.expect("float fragment");
    let clear_y = clear_y.expect("cleared fragment");
    assert!(float_y.abs() < 0.01);
    assert!(
      clear_y >= 50.0,
      "cleared block should be pushed below float; got clear_y={clear_y}"
    );
  }

  #[test]
  fn inline_lines_shorten_next_to_float() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(80.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, BoxNode::new_inline(default_style(), vec![text])],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      line.bounds.width() <= 120.0,
      "line width should be shortened by float; got {}",
      line.bounds.width()
    );
    assert!(
      line.bounds.x() >= 79.9,
      "line should start after the float; got x={}",
      line.bounds.x()
    );
  }

  #[test]
  fn inline_lines_inside_following_block_boxes_consult_parent_float_context() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(80.0));
    float_style.height = Some(Length::px(20.0));
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let paragraph = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, paragraph],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      line.bounds.width() <= 120.0,
      "line width should be shortened by float; got {}",
      line.bounds.width()
    );
    assert!(
      line.bounds.x() >= 79.9,
      "line should start after the float; got x={}",
      line.bounds.x()
    );
  }

  #[test]
  fn float_negative_margin_reduces_blocked_width() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(60.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    float_style.margin_left = Some(Length::px(-20.0));
    let float_node =
      BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let text = BoxNode::new_text(default_style(), "text".to_string());
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node, BoxNode::new_inline(default_style(), vec![text])],
    );
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .expect("float fragment");
    assert!(
      float_fragment.bounds.x() < 0.0,
      "negative margin should shift float left; got {}",
      float_fragment.bounds.x()
    );

    let line = find_line(&fragment).expect("line fragment");
    assert!(
      (line.bounds.x() - 40.0).abs() < 1.0,
      "line should start after the reduced margin box; got {}",
      line.bounds.x()
    );
  }

  #[test]
  fn float_auto_width_shrinks_to_available_space_next_to_float() {
    let bfc = BlockFormattingContext::new();

    let mut wide_style = ComputedStyle::default();
    wide_style.display = Display::Block;
    wide_style.float = Float::Left;
    wide_style.width = Some(Length::px(120.0));
    wide_style.height = Some(Length::px(20.0));
    wide_style.width_keyword = None;
    wide_style.height_keyword = None;
    let wide_float = BoxNode::new_block(Arc::new(wide_style), FormattingContextType::Block, vec![]);

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.float = Float::Left;
    let text = BoxNode::new_text(default_style(), "word ".repeat(20));
    let auto_float = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![BoxNode::new_inline(default_style(), vec![text])],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![wide_float, auto_float],
    );
    let constraints =
      LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);

    let fragment = bfc.layout(&root, &constraints).unwrap();

    let floats: Vec<_> = fragment
      .children
      .iter()
      .filter(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .collect();

    assert_eq!(floats.len(), 2);

    let mut wide = None;
    let mut auto = None;
    for float in floats {
      if (float.bounds.width() - 120.0).abs() < 0.5 {
        wide = Some(float);
      } else {
        auto = Some(float);
      }
    }

    let wide = wide.expect("wide float fragment");
    let auto = auto.expect("auto float fragment");

    assert!(
      auto.bounds.y() < 0.01,
      "auto float should stay alongside the existing float; got y={}",
      auto.bounds.y()
    );
    assert!(
      (auto.bounds.x() - wide.bounds.width()).abs() < 0.5,
      "auto float should start after the first float; got x={}",
      auto.bounds.x()
    );
    assert!(
      auto.bounds.width() <= 90.0,
      "auto float should shrink to the available 80px space; got {}",
      auto.bounds.width()
    );
  }

  #[test]
  fn float_percent_padding_resolves_against_containing_block_width() {
    let bfc = BlockFormattingContext::new();

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(100.0));
    float_style.width_keyword = None;
    float_style.padding_left = Length::percent(10.0);
    float_style.padding_right = Length::px(0.0);
    float_style.border_left_width = Length::px(0.0);
    float_style.border_right_width = Length::px(0.0);

    let text = BoxNode::new_text(default_style(), "hello".into());
    let inline = BoxNode::new_inline(default_style(), vec![text]);
    let float_node = BoxNode::new_block(
      Arc::new(float_style.clone()),
      FormattingContextType::Block,
      vec![inline],
    );

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![float_node.clone()],
    );
    let constraints = LayoutConstraints::definite_width(200.0);

    let fragment = bfc.layout(&root, &constraints).unwrap();
    let float_fragment = fragment
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.float.is_floating())
          .unwrap_or(false)
      })
      .expect("float fragment");

    fn find_line_offset_x(fragment: &FragmentNode, offset: f32) -> Option<f32> {
      let offset = offset + fragment.bounds.x();
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(offset);
      }
      for child in fragment.children.iter() {
        if let Some(found) = find_line_offset_x(child, offset) {
          return Some(found);
        }
      }
      None
    }

    let line_x = float_fragment
      .children
      .iter()
      .find_map(|child| find_line_offset_x(child, 0.0))
      .expect("line fragment");

    let inline_sides = inline_axis_sides(&float_style);
    let inline_positive = inline_axis_positive(float_style.writing_mode, float_style.direction);
    let computed_width = compute_block_width(
      &float_style,
      200.0,
      bfc.viewport_size,
      inline_sides,
      inline_positive,
    );
    let expected_offset = computed_width.border_left + computed_width.padding_left;

    assert!(
      (line_x - expected_offset).abs() < 0.5,
      "expected line to start at x={:.2} inside the float (border+padding); got x={:.2}",
      expected_offset,
      line_x
    );
  }

  #[test]
  fn list_marker_outside_positions_marker_left_of_text() {
    let generator = BoxGenerator::new();

    let mut li_style = ComputedStyle::default();
    li_style.display = Display::ListItem;
    li_style.list_style_position = ListStylePosition::Outside;
    let li_style = Arc::new(li_style);

    let mut ul_style = ComputedStyle::default();
    ul_style.display = Display::Block;
    let ul_style = Arc::new(ul_style);

    let li = DOMNode::new_element(
      "li",
      li_style.clone(),
      vec![DOMNode::new_text("Item", li_style.clone())],
    );
    let ul = DOMNode::new_element("ul", ul_style, vec![li]);
    let box_tree = generator.generate(&ul).unwrap();

    let bfc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = bfc.layout(&box_tree.root, &constraints).unwrap();

    let li_fragment = fragment.children.first().expect("li fragment");
    fn find_line(fragment: &FragmentNode) -> Option<&FragmentNode> {
      if matches!(fragment.content, FragmentContent::Line { .. }) {
        return Some(fragment);
      }
      for child in fragment.children.iter() {
        if let Some(line) = find_line(child) {
          return Some(line);
        }
      }
      None
    }

    let line = find_line(li_fragment).expect("line fragment");

    let marker = line
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.list_style_type == ListStyleType::None)
          .unwrap_or(false)
      })
      .expect("marker fragment");
    let text = line
      .children
      .iter()
      .find(|child| {
        child
          .style
          .as_ref()
          .map(|s| s.list_style_type != ListStyleType::None)
          .unwrap_or(false)
      })
      .expect("text fragment");

    assert!(marker.bounds.x() < 0.0);
    assert!(text.bounds.x() >= 0.0);
  }

  #[test]
  fn intrinsic_inline_size_splits_runs_around_block_children() {
    let bfc = BlockFormattingContext::new();
    let ifc = InlineFormattingContext::new();

    let text_left = BoxNode::new_text(default_style(), "unbreakable".to_string());
    let text_right = BoxNode::new_text(default_style(), "unbreakable".to_string());
    let block_child = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let run_container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text_left.clone()],
    );
    let run_min = ifc
      .compute_intrinsic_inline_size(&run_container, IntrinsicSizingMode::MinContent)
      .unwrap();
    let run_max = ifc
      .compute_intrinsic_inline_size(&run_container, IntrinsicSizingMode::MaxContent)
      .unwrap();

    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text_left, block_child, text_right],
    );
    let min_width = bfc
      .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert!(
      min_width <= run_min * 1.1,
      "min-content width should follow the widest inline run, got {min_width} vs run {run_min}"
    );

    let max_width = bfc
      .compute_intrinsic_inline_size(&root, IntrinsicSizingMode::MaxContent)
      .unwrap();
    assert!(
            max_width <= run_max * 1.1,
            "max-content width should not concatenate inline runs across blocks, got {max_width} vs run {run_max}"
        );
  }

  #[test]
  fn intrinsic_inline_size_includes_inline_replaced_children() {
    let bfc = BlockFormattingContext::new();
    let mut replaced_style = ComputedStyle::default();
    replaced_style.display = Display::Inline;
    let replaced = BoxNode::new_replaced(
      Arc::new(replaced_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(120.0, 50.0)),
      None,
    );

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![replaced],
    );
    let min = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MinContent)
      .unwrap();
    let max = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(
      (min - 120.0).abs() < 0.5,
      "expected min-content width ~120, got {min}"
    );
    assert!(
      (max - 120.0).abs() < 0.5,
      "expected max-content width ~120, got {max}"
    );
  }

  #[test]
  fn size_containment_zeroes_intrinsic_inline_contribution() {
    let mut style = (*default_style()).clone();
    style.containment =
      crate::style::types::Containment::with_flags(true, false, false, false, false);
    style.padding_left = Length::px(4.0);
    style.padding_right = Length::px(4.0);
    style.border_left_style = BorderStyle::Solid;
    style.border_right_style = BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Block,
      vec![BoxNode::new_text(
        default_style(),
        "superlongword".to_string(),
      )],
    );

    let bfc = BlockFormattingContext::new();
    let max = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MaxContent)
      .unwrap();
    let min = bfc
      .compute_intrinsic_inline_size(&container, IntrinsicSizingMode::MinContent)
      .unwrap();

    assert!((max - 12.0).abs() < 0.001);
    assert!((min - 12.0).abs() < 0.001);
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_for_intrinsic_sizing_under_containment() {
    let _guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::intrinsic_cache_use_epoch(1, true);
    crate::layout::formatting_context::intrinsic_cache_clear();

    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.containment =
      crate::style::types::Containment::with_flags(false, true, false, false, false);

    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    node.id = 4242;

    // Pre-seed the intrinsic cache to ensure `contain-intrinsic-size: auto` bypasses stale cached
    // values when a remembered size is available.
    crate::layout::formatting_context::intrinsic_cache_store(
      &node,
      IntrinsicSizingMode::MinContent,
      0.0,
    );
    crate::layout::formatting_context::intrinsic_cache_store(
      &node,
      IntrinsicSizingMode::MaxContent,
      0.0,
    );
    crate::layout::formatting_context::remembered_size_cache_store(&node, Size::new(123.0, 456.0));

    let bfc = BlockFormattingContext::new();
    let (min, max) = bfc.compute_intrinsic_inline_sizes(&node).unwrap();
    assert!((min - 123.0).abs() < 0.001, "expected remembered min {min}");
    assert!((max - 123.0).abs() < 0.001, "expected remembered max {max}");
    assert!(
      (bfc
        .compute_intrinsic_inline_size(&node, IntrinsicSizingMode::MinContent)
        .unwrap()
        - 123.0)
        .abs()
        < 0.001
    );
    assert!(
      (bfc
        .compute_intrinsic_inline_size(&node, IntrinsicSizingMode::MaxContent)
        .unwrap()
        - 123.0)
        .abs()
        < 0.001
    );

    crate::layout::formatting_context::intrinsic_cache_use_epoch(1, true);
    crate::layout::formatting_context::intrinsic_cache_clear();
  }

  #[test]
  fn absolutely_positioned_child_uses_padding_containing_block_when_parent_positioned() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_top = Length::px(10.0);
    parent_style.border_left_style = BorderStyle::Solid;
    parent_style.border_top_style = BorderStyle::Solid;
    parent_style.border_left_width = Length::px(2.0);
    parent_style.border_top_width = Length::px(2.0);
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.position = Position::Absolute;
    child_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    child_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    child_style.width = Some(Length::px(50.0));
    child_style.height = Some(Length::px(20.0));
    child_style.width_keyword = None;
    child_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];
    assert_eq!(child_frag.bounds.x(), 7.0);
    assert_eq!(child_frag.bounds.y(), 9.0);
    assert_eq!(child_frag.bounds.width(), 50.0);
    assert_eq!(child_frag.bounds.height(), 20.0);
  }

  #[test]
  fn absolutely_positioned_child_percent_top_resolves_against_auto_height_padding_box() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;
    parent_style.padding_top = Length::px(10.0);
    parent_style.padding_bottom = Length::px(10.0);

    let flow_child = BoxNode::new_block(
      block_style_with_height(100.0),
      FormattingContextType::Block,
      vec![],
    );

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_id = 4242;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = abs_id;

    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![flow_child, abs_child],
    );

    fn find_fragment<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment(child, id) {
          return Some(found);
        }
      }
      None
    }

    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite(300.0, 300.0))
      .unwrap();
    let abs_frag = find_fragment(&fragment, abs_id).expect("absolute fragment");

    assert!(
      (abs_frag.bounds.y() - 60.0).abs() < 0.01,
      "expected top:50% to resolve against 100px content height + 20px padding (y=60); got {}",
      abs_frag.bounds.y()
    );
  }

  #[test]
  fn absolutely_positioned_child_width_max_content_centers_with_insets() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.position = Position::Absolute;
    child_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.right = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    child_style.margin_left = None;
    child_style.margin_right = None;
    child_style.width = None;
    child_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let text = BoxNode::new_text(default_style(), "x".to_string());
    let child = BoxNode::new_block(
      Arc::new(child_style),
      FormattingContextType::Block,
      vec![text],
    );
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![child],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&parent, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_frag = &fragment.children[0];

    assert!(
      child_frag.bounds.width() < 199.5,
      "expected max-content width smaller than containing block; got {}",
      child_frag.bounds.width()
    );
    let expected_x = (200.0 - child_frag.bounds.width()) / 2.0;
    assert!(
      (child_frag.bounds.x() - expected_x).abs() < 0.5,
      "expected centered x≈{}, got {} (width={})",
      expected_x,
      child_frag.bounds.x(),
      child_frag.bounds.width()
    );
  }

  #[test]
  fn absolute_children_inside_block_descendants_are_laid_out() {
    // Regression: positioned children collected during block child layout were dropped.
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(400.0));
    root_style.width_keyword = None;

    let mut middle_style = ComputedStyle::default();
    middle_style.display = Display::Block;
    middle_style.width = Some(Length::px(200.0));
    middle_style.width_keyword = None;
    middle_style.padding_left = Length::px(10.0);
    middle_style.padding_top = Length::px(10.0);
    middle_style.height = Some(Length::px(80.0));
    middle_style.height_keyword = None;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(30.0));
    abs_style.height = Some(Length::px(12.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let middle = BoxNode::new_block(
      Arc::new(middle_style),
      FormattingContextType::Block,
      vec![abs_child],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![middle],
    );

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(500.0, 500.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let middle_frag = &fragment.children[0];
    assert_eq!(
      middle_frag.children.len(),
      1,
      "positioned child should be laid out"
    );
    let abs_frag = &middle_frag.children[0];
    // Positioned child should still be included; coordinates are resolved relative to the
    // containing block origin (padding box in our implementation).
    assert_eq!(abs_frag.bounds.x(), 5.0);
    assert_eq!(abs_frag.bounds.y(), 7.0);
    assert_eq!(abs_frag.bounds.width(), 30.0);
    assert_eq!(abs_frag.bounds.height(), 12.0);
  }

  #[test]
  fn absolute_children_inside_inline_descendants_use_updated_positioned_containing_block() {
    // Regression: BlockFormattingContext reuses a cached InlineFormattingContext when the nearest
    // positioned containing block matches. When cloning a block context for a positioned element,
    // we must rebuild that cached inline context so percentage offsets for absolutely positioned
    // descendants resolve against the new containing block (not the previous one).

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.width = Some(Length::px(800.0));
    root_style.width_keyword = None;

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.position = Position::Relative;
    parent_style.width = Some(Length::px(200.0));
    parent_style.width_keyword = None;

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(Arc::new(inline_style), vec![abs_child]);
    let parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![inline],
    );
    let root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![parent],
    );

    fn find_abs_fragment(node: &FragmentNode) -> Option<&FragmentNode> {
      if node
        .style
        .as_ref()
        .map(|s| s.position == Position::Absolute)
        .unwrap_or(false)
      {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_abs_fragment(child) {
          return Some(found);
        }
      }
      None
    }

    let fc = BlockFormattingContext::new();
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&root, &constraints).unwrap();

    let abs_fragment =
      find_abs_fragment(&fragment).expect("expected absolute-positioned descendant");
    assert_eq!(
      abs_fragment.bounds.x(),
      100.0,
      "left:50% should resolve against the positioned 200px-wide containing block"
    );
  }

  #[test]
  fn inline_absolute_children_use_updated_nearest_positioned_cb() {
    fn find_fragment_by_box_id<'a>(node: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(id) {
        return Some(node);
      }
      for child in node.children.iter() {
        if let Some(found) = find_fragment_by_box_id(child, id) {
          return Some(found);
        }
      }
      None
    }

    let viewport = Size::new(300.0, 300.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut container_style = ComputedStyle::default();
    container_style.display = Display::Block;
    container_style.width = Some(Length::px(200.0));
    container_style.width_keyword = None;
    // Non-empty transforms establish a new positioned containing block. The block formatting
    // context clones itself when entering such a subtree; ensure the shared inline formatting
    // context is rebuilt with the updated `nearest_positioned_cb`.
    container_style.transform = vec![Transform::TranslateX(Length::px(0.0))];

    let mut wrapper_style = ComputedStyle::default();
    wrapper_style.display = Display::Inline;

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = Display::Block;
    abs_style.position = Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::percent(50.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(20.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_id = 9001;
    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = abs_id;

    let inline_wrapper = BoxNode::new_inline(
      Arc::new(wrapper_style),
      vec![
        BoxNode::new_text(Arc::new(text_style), "hi".to_string()),
        abs_child,
      ],
    );
    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Block,
      vec![inline_wrapper],
    );
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![container],
    );

    let fragment = fc
      .layout(&root, &LayoutConstraints::definite(300.0, 300.0))
      .unwrap();

    let abs_fragment = find_fragment_by_box_id(&fragment, abs_id).expect("positioned fragment");
    assert!(
      (abs_fragment.bounds.x() - 100.0).abs() < 0.01,
      "left:50% should resolve against the transformed 200px-wide containing block, got {}",
      abs_fragment.bounds.x()
    );
  }

  #[test]
  fn table_cell_intrinsic_width_uses_inline_children_path() {
    let mut cell_style = ComputedStyle::default();
    cell_style.display = Display::TableCell;
    cell_style.font_size = 16.0;
    let cell_style = Arc::new(cell_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let text1 = BoxNode::new_text(text_style.clone(), "hello ".to_string());
    let text2 = BoxNode::new_text(text_style, "world".to_string());
    let cell = BoxNode::new_block(
      cell_style.clone(),
      FormattingContextType::Block,
      vec![text1, text2],
    );

    let fc = BlockFormattingContext::new();
    let inline_fc = InlineFormattingContext::with_factory(fc.child_factory());
    let inline_container = BoxNode::new_inline(cell_style, cell.children.clone());

    let expected_min = inline_fc
      .compute_intrinsic_inline_size(&inline_container, IntrinsicSizingMode::MinContent)
      .expect("inline min");
    let expected_max = inline_fc
      .compute_intrinsic_inline_size(&inline_container, IntrinsicSizingMode::MaxContent)
      .expect("inline max");

    let actual_min = fc
      .compute_intrinsic_inline_size(&cell, IntrinsicSizingMode::MinContent)
      .expect("block min");
    let actual_max = fc
      .compute_intrinsic_inline_size(&cell, IntrinsicSizingMode::MaxContent)
      .expect("block max");

    assert!(
      (actual_min - expected_min).abs() < 0.01,
      "min-content: expected {}, got {}",
      expected_min,
      actual_min
    );
    assert!(
      (actual_max - expected_max).abs() < 0.01,
      "max-content: expected {}, got {}",
      expected_max,
      actual_max
    );
  }

  fn find_block_fragment<'a>(
    fragment: &'a FragmentNode,
    box_id: usize,
  ) -> Option<&'a FragmentNode> {
    if let FragmentContent::Block {
      box_id: Some(found),
    } = &fragment.content
    {
      if *found == box_id {
        return Some(fragment);
      }
    }
    for child in fragment.children.iter() {
      if let Some(found) = find_block_fragment(child, box_id) {
        return Some(found);
      }
    }
    None
  }

  #[test]
  fn content_visibility_auto_translates_paint_viewport_through_nested_offsets() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(400.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(50.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder block-size so `content-visibility:auto` can skip layout
    // when offscreen. Without this, the default `contain-intrinsic-size:auto` has no fallback
    // length and we keep laying out descendants to avoid collapsing the element to 0px.
    auto_style.contain_intrinsic_height.length = Some(Length::px(50.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut wrapper = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    wrapper.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, wrapper],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when translated viewport is offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_without_placeholder_does_not_skip_layout() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(400.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(50.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 4;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 3).expect("auto fragment");
    assert!(
      auto_fragment.bounds.y() > viewport.height,
      "expected auto subtree to start below the viewport (y={} height={})",
      auto_fragment.bounds.y(),
      viewport.height
    );
    assert!(
      !auto_fragment.children.is_empty(),
      "expected descendants to be laid out when no placeholder is available"
    );
  }

  #[test]
  fn content_visibility_auto_skips_when_fully_outside_inline_axis() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 12;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.margin_left = Some(Length::px(500.0));
    auto_style.width = Some(Length::px(100.0));
    auto_style.width_keyword = None;
    // Ensure auto skipping uses a non-content placeholder block-size.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 11;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 10;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 11).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the border box is outside the inline axis"
    );
  }

  #[test]
  fn content_visibility_auto_respects_vertical_writing_mode() {
    let _toggles = content_visibility_test_guard();
    // Choose a viewport where physical height > width so vertical writing mode mapping matters:
    // the logical block size should come from the physical width (50), not height (100).
    let viewport = Size::new(50.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Block;
    parent_style.writing_mode = WritingMode::VerticalRl;
    let parent = BoxNode::new_block(Arc::new(parent_style), FormattingContextType::Block, vec![]);

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 22;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.writing_mode = WritingMode::VerticalRl;
    auto_style.content_visibility = ContentVisibility::Auto;
    // For vertical writing modes, the logical block axis maps to the physical inline axis, so the
    // placeholder must come from the corresponding contain-intrinsic axis.
    auto_style.contain_intrinsic_width.length = Some(Length::px(10.0));
    let auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );

    let containing_width = viewport.height;
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(containing_width),
      AvailableSpace::Indefinite,
    );
    let paint_viewport = paint_viewport_for(WritingMode::VerticalRl, Direction::Ltr, viewport);
    let current_y = viewport.width + 10.0;
    let nearest_cb = ContainingBlock::viewport(viewport);

    let fragment = fc
      .layout_block_child(
        &parent,
        &auto_box,
        containing_width,
        &constraints,
        current_y,
        &nearest_cb,
        &nearest_cb,
        None,
        0.0,
        paint_viewport,
      )
      .expect("layout_block_child");

    assert!(
      fragment.children.is_empty(),
      "expected descendants to be skipped in vertical writing modes when offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_with_fixed_height_through_nested_offsets() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(300.0),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.height = Some(Length::px(50.0));
    auto_style.height_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut wrapper = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    wrapper.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, wrapper],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");

    let wrapper_fragment = find_block_fragment(&fragment, 3).expect("wrapper fragment");
    assert!(
      wrapper_fragment.bounds.y() > viewport.height,
      "expected wrapper subtree to be positioned below the paint viewport"
    );

    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.bounds.y().abs() < 0.1,
      "expected auto fragment to have a small local block offset (y={})",
      auto_fragment.bounds.y()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when translated viewport is offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_with_fixed_height_outside_inline_axis() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::new(
      AvailableSpace::Definite(viewport.width),
      AvailableSpace::Indefinite,
    );

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 12;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.margin_left = Some(Length::px(viewport.width + 10.0));
    auto_style.width = Some(Length::px(50.0));
    auto_style.width_keyword = None;
    auto_style.height = Some(Length::px(10.0));
    auto_style.height_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 11;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 10;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 11).expect("auto fragment");
    assert!(
      auto_fragment.bounds.x() > viewport.width,
      "expected auto fragment to be positioned outside the inline axis (x={})",
      auto_fragment.bounds.x()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the border box is outside the inline axis"
    );
  }

  #[test]
  fn content_visibility_auto_skips_descendants_in_vertical_writing_mode_with_spacer_offset() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(50.0, 100.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

    let spacer_style = {
      let mut style = (*block_style_with_height(viewport.width + 10.0)).clone();
      style.writing_mode = WritingMode::VerticalLr;
      Arc::new(style)
    };
    let mut spacer = BoxNode::new_block(spacer_style, FormattingContextType::Block, vec![]);
    spacer.id = 2;

    let leaf_style = {
      let mut style = (*block_style_with_height(10.0)).clone();
      style.writing_mode = WritingMode::VerticalLr;
      Arc::new(style)
    };
    let mut leaf = BoxNode::new_block(leaf_style, FormattingContextType::Block, vec![]);
    leaf.id = 5;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.writing_mode = WritingMode::VerticalLr;
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.height = Some(Length::px(10.0));
    auto_style.height_keyword = None;
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 4;

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.writing_mode = WritingMode::VerticalLr;
    let mut root = BoxNode::new_block(
      Arc::new(root_style),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 4).expect("auto fragment");
    assert!(
      auto_fragment.bounds.x() > viewport.width,
      "expected auto fragment to be positioned beyond the viewport block axis (x={})",
      auto_fragment.bounds.x()
    );
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped in vertical writing mode when offscreen"
    );
  }

  #[test]
  fn content_visibility_auto_accounts_for_viewport_scroll() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(300.0, 200.0);
    let scroll = Point::new(0.0, 300.0);
    let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

    let fc_no_scroll = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );

    let mut spacer = BoxNode::new_block(
      block_style_with_height(scroll.y),
      FormattingContextType::Block,
      vec![],
    );
    spacer.id = 2;

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 4;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder so offscreen auto content can be skipped pre-scroll.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 3;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![spacer, auto_box],
    );
    root.id = 1;

    let fragment_no_scroll = fc_no_scroll.layout(&root, &constraints).expect("layout");
    let auto_fragment_no_scroll =
      find_block_fragment(&fragment_no_scroll, 3).expect("auto fragment");
    assert!(
      auto_fragment_no_scroll.children.is_empty(),
      "expected descendants to be skipped before scrolling"
    );

    let factory = crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    )
    .with_viewport_scroll(scroll);
    let fc = BlockFormattingContext::with_factory(factory);
    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 3).expect("auto fragment");
    assert!(
      !auto_fragment.children.is_empty(),
      "expected scrolled viewport to keep descendants active"
    );
  }

  #[test]
  fn content_visibility_auto_uses_viewport_scroll_when_constraints_are_not_viewport_sized() {
    let _toggles = content_visibility_test_guard();
    let viewport = Size::new(200.0, 200.0);
    // The scroll is expressed in the nested formatting context's coordinate space. A negative
    // scroll offset corresponds to the nested context being shifted positively relative to the
    // viewport.
    let scroll = Point::new(-300.0, 0.0);
    // Use constraints that do not match the viewport size to mirror nested layout calls (e.g., flex
    // items, table cells) that still need viewport-relative `content-visibility:auto` decisions.
    let constraints = LayoutConstraints::definite(100.0, viewport.height);

    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        FontContext::new(),
        viewport,
        ContainingBlock::viewport(viewport),
      )
      .with_viewport_scroll(scroll);
    let fc = BlockFormattingContext::with_factory(factory);

    let mut leaf = BoxNode::new_block(
      block_style_with_height(10.0),
      FormattingContextType::Block,
      vec![],
    );
    leaf.id = 3;

    let mut auto_style = ComputedStyle::default();
    auto_style.display = Display::Block;
    auto_style.content_visibility = ContentVisibility::Auto;
    // Provide a deterministic placeholder so offscreen auto content can be skipped.
    auto_style.contain_intrinsic_height.length = Some(Length::px(10.0));
    let mut auto_box = BoxNode::new_block(
      Arc::new(auto_style),
      FormattingContextType::Block,
      vec![leaf],
    );
    auto_box.id = 2;

    let mut root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![auto_box],
    );
    root.id = 1;

    let fragment = fc.layout(&root, &constraints).expect("layout");
    let auto_fragment = find_block_fragment(&fragment, 2).expect("auto fragment");
    assert!(
      auto_fragment.children.is_empty(),
      "expected descendants to be skipped when the viewport does not intersect the translated subtree",
    );
  }

  #[test]
  fn floats_protrude_out_of_non_bfc_blocks_and_affect_following_siblings() {
    let viewport = Size::new(200.0, 200.0);
    let fc = BlockFormattingContext::with_font_context_viewport_and_cb(
      FontContext::new(),
      viewport,
      ContainingBlock::viewport(viewport),
    );
    let nearest_cb = ContainingBlock::viewport(viewport);
    let constraints = LayoutConstraints::definite_width(viewport.width);

    let mut outer_style = ComputedStyle::default();
    outer_style.display = Display::Block;
    let outer_style = Arc::new(outer_style);

    let mut block_style = ComputedStyle::default();
    block_style.display = Display::Block;
    let block_style = Arc::new(block_style);

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Left;
    float_style.width = Some(Length::px(40.0));
    float_style.height = Some(Length::px(20.0));
    float_style.width_keyword = None;
    float_style.height_keyword = None;
    let float_box = BoxNode::new_replaced(
      Arc::new(float_style),
      crate::tree::box_tree::ReplacedType::Canvas,
      Some(Size::new(40.0, 20.0)),
      None,
    );
    let float_container = BoxNode::new_block(
      block_style.clone(),
      FormattingContextType::Block,
      vec![float_box],
    );

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    let text = BoxNode::new_text(Arc::new(text_style), "hi".to_string());
    let text_container = BoxNode::new_block(block_style, FormattingContextType::Block, vec![text]);

    let outer = BoxNode::new_block(
      outer_style,
      FormattingContextType::Block,
      vec![float_container, text_container],
    );

    let paint_viewport =
      paint_viewport_for(outer.style.writing_mode, outer.style.direction, viewport);
    let mut float_ctx = FloatContext::new(viewport.width);
    let (fragments, _height, _positioned) = fc
      .layout_children_with_external_floats(
        &outer,
        &constraints,
        &nearest_cb,
        &nearest_cb,
        paint_viewport,
        Some(&mut float_ctx),
        0.0,
      )
      .expect("layout children");

    assert_eq!(fragments.len(), 2);
    assert!(
      fragments[0].bounds.height().abs() < 0.1,
      "expected float container height to ignore floats, got {:.2}",
      fragments[0].bounds.height()
    );
    assert!(
      fragments[1].bounds.y().abs() < 0.1,
      "expected following sibling to start at y=0, got {:.2}",
      fragments[1].bounds.y()
    );

    let (left_edge, available_width) = float_ctx.available_width_at_y(0.0);
    assert!(
      (left_edge - 40.0).abs() < 0.5,
      "expected left edge to be pushed past float (≈40px), got {:.2}",
      left_edge
    );
    assert!(
      (available_width - (viewport.width - 40.0)).abs() < 0.5,
      "expected available width to shrink (≈{:.2}px), got {:.2}",
      viewport.width - 40.0,
      available_width
    );
  }

  #[test]
  fn block_layout_reuses_shaping_pipeline_across_children() {
    // Regression: block layout used to instantiate a new shaping pipeline for each block box that
    // buffered inline children, preventing font fallback caches from being reused across blocks.
    crate::text::pipeline::ShapingPipeline::debug_reset_new_call_count();

    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.font_size = 16.0;
    let root_style = Arc::new(root_style);

    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.font_size = 16.0;
    let child_style = Arc::new(child_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let children = (0..64usize)
      .map(|idx| {
        let text = BoxNode::new_text(text_style.clone(), format!("hello {idx}"));
        BoxNode::new_block(
          child_style.clone(),
          FormattingContextType::Block,
          vec![text],
        )
      })
      .collect();
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);

    let factory = FormattingContextFactory::new().with_parallelism(LayoutParallelism::disabled());
    let fc = factory.create(FormattingContextType::Block);
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let _fragment = fc.layout(&root, &constraints).expect("layout");

    assert_eq!(
      crate::text::pipeline::ShapingPipeline::debug_new_call_count(),
      1
    );
  }
}
