//! Contains GridItem used to represent a single grid item during layout
use super::GridTrack;
use crate::compute::grid::OriginZeroLine;
use crate::geometry::AbstractAxis;
use crate::geometry::{Line, Point, Rect, Size};
use crate::style::{
  AlignItems, AlignSelf, AvailableSpace, Dimension, LengthPercentageAuto, Overflow,
};
use crate::tree::{LayoutPartialTree, LayoutPartialTreeExt, NodeId, SizingMode};
use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};
use crate::{BoxSizing, GridItemStyle, LengthPercentage};
use core::ops::Range;

/// Represents a single grid item
#[derive(Debug)]
pub(in super::super) struct GridItem {
  /// The id of the node that this item represents
  pub node: NodeId,

  /// Whether this item is a virtual contribution (e.g., a descendant of a subgrid)
  pub is_virtual: bool,

  /// The order of the item in the children array
  ///
  /// We sort the list of grid items during track sizing. This field allows us to sort back the original order
  /// for final positioning
  pub source_order: u16,

  /// The item's definite row-start and row-end, as resolved by the placement algorithm
  /// (in origin-zero coordinates)
  pub row: Line<OriginZeroLine>,
  /// The items definite column-start and column-end, as resolved by the placement algorithm
  /// (in origin-zero coordinates)
  pub column: Line<OriginZeroLine>,

  /// Is it a compressible replaced element?
  /// https://drafts.csswg.org/css-sizing-3/#min-content-zero
  pub is_compressible_replaced: bool,
  /// The item's overflow style
  pub overflow: Point<Overflow>,
  /// The item's box_sizing style
  pub box_sizing: BoxSizing,
  /// The item's size style
  pub size: Size<Dimension>,
  /// The item's min_size style
  pub min_size: Size<Dimension>,
  /// The item's max_size style
  pub max_size: Size<Dimension>,
  /// The item's aspect_ratio style
  pub aspect_ratio: Option<f32>,
  /// The item's padding style
  pub padding: Rect<LengthPercentage>,
  /// The item's border style
  pub border: Rect<LengthPercentage>,
  /// The item's margin style
  pub margin: Rect<LengthPercentageAuto>,
  /// Extra margin applied by subgrid algorithms (for example, the CSS Grid Level 2 "subgrid gaps"
  /// half-difference adjustment).
  pub extra_margin: Rect<f32>,
  /// The item's align_self property, or the parent's align_items property is not set
  pub align_self: AlignSelf,
  /// The item's justify_self property, or the parent's justify_items property is not set
  pub justify_self: AlignSelf,
  /// The items first baseline (horizontal)
  pub baseline: Option<f32>,
  /// Shim for baseline alignment.
  ///
  /// `baseline_shim.y` models baseline alignment in the physical vertical axis and acts like an
  /// extra top margin.
  ///
  /// `baseline_shim.x` models baseline alignment in the physical horizontal axis and acts like an
  /// extra left margin.
  pub baseline_shim: Point<f32>,

  /// The item's definite row-start and row-end (same as `row` field, except in a different coordinate system)
  /// (as indexes into the Vec<GridTrack> stored in a grid's AbstractAxisTracks)
  pub row_indexes: Line<u16>,
  /// The items definite column-start and column-end (same as `column` field, except in a different coordinate system)
  /// (as indexes into the Vec<GridTrack> stored in a grid's AbstractAxisTracks)
  pub column_indexes: Line<u16>,

  /// Whether the item crosses a flexible row
  pub crosses_flexible_row: bool,
  /// Whether the item crosses a flexible column
  pub crosses_flexible_column: bool,
  /// Whether the item crosses an intrinsic row track based purely on the track sizing functions.
  ///
  /// This excludes the CSS Grid "percentage tracks behave as intrinsic when the container size is indefinite"
  /// rule, which is applied dynamically during track sizing passes.
  pub crosses_intrinsic_row_base: bool,
  /// Whether the item crosses an intrinsic column track based purely on the track sizing functions.
  ///
  /// This excludes the CSS Grid "percentage tracks behave as intrinsic when the container size is indefinite"
  /// rule, which is applied dynamically during track sizing passes.
  pub crosses_intrinsic_column_base: bool,
  /// Whether the item crosses any percentage column *track*.
  ///
  /// This must ignore gutters (gap tracks).
  pub crosses_percentage_column: bool,
  /// Whether the item crosses any percentage row *track*.
  ///
  /// This must ignore gutters (gap tracks).
  pub crosses_percentage_row: bool,
  /// Whether the item crosses a intrinsic row
  pub crosses_intrinsic_row: bool,
  /// Whether the item crosses a intrinsic column
  pub crosses_intrinsic_column: bool,

  // Caches for intrinsic size computation. These caches are only valid for a single run of the track-sizing algorithm.
  /// Cache for the known_dimensions input to intrinsic sizing computation
  pub available_space_cache: Option<Size<Option<f32>>>,
  /// Cache for the min-content size
  pub min_content_contribution_cache: Size<Option<f32>>,
  /// Cache for the minimum contribution
  pub minimum_contribution_cache: Size<Option<f32>>,
  /// Cache for the max-content size
  pub max_content_contribution_cache: Size<Option<f32>>,
  /// Cache for resolved margin axis sums excluding baseline shims.
  ///
  /// The cached value includes `extra_margin` but *not* `baseline_shim` because baseline shims can
  /// change across passes. The cache key is the `inner_node_width` basis used to resolve vertical
  /// percentage margins (stored as canonicalized `f32::to_bits()` where `0.0` and `-0.0` map to the
  /// same key).
  ///
  /// Cached values:
  /// - `Option<Size<f32>>`: axis sums including `extra_margin`, excluding baseline shims. This is
  ///   `None` when we've only cached the start margin(s) (for example from baseline shimming) and
  ///   have not yet computed full axis sums.
  /// - `Point<Option<f32>>`: resolved *start* margins (`left`, `top`) excluding `extra_margin` and
  ///   baseline shims. Used by baseline shimming, which sanitizes non-finite values per-side before
  ///   adding `extra_margin`.
  pub resolved_margin_axis_sums_cache: Option<(Option<u32>, Option<Size<f32>>, Point<Option<f32>>)>,

  /// Final y position. Used to compute baseline alignment for the container.
  pub y_position: f32,
  /// Final height. Used to compute baseline alignment for the container.
  pub height: f32,
}

impl GridItem {
  /// Create a new item given a concrete placement in both axes
  pub fn new_with_placement_style_and_order<S: GridItemStyle>(
    node: NodeId,
    col_span: Line<OriginZeroLine>,
    row_span: Line<OriginZeroLine>,
    style: S,
    parent_align_items: AlignItems,
    parent_justify_items: AlignItems,
    source_order: u16,
  ) -> Self {
    GridItem {
      node,
      is_virtual: false,
      source_order,
      row: row_span,
      column: col_span,
      is_compressible_replaced: style.is_compressible_replaced(),
      overflow: style.overflow(),
      box_sizing: style.box_sizing(),
      size: style.size(),
      min_size: style.min_size(),
      max_size: style.max_size(),
      aspect_ratio: style.aspect_ratio(),
      padding: style.padding(),
      border: style.border(),
      margin: style.margin(),
      extra_margin: Rect::default(),
      align_self: style.align_self().unwrap_or(parent_align_items),
      justify_self: style.justify_self().unwrap_or(parent_justify_items),
      baseline: None,
      baseline_shim: Point::ZERO,
      row_indexes: Line { start: 0, end: 0 }, // Properly initialised later
      column_indexes: Line { start: 0, end: 0 }, // Properly initialised later
      crosses_flexible_row: false,            // Properly initialised later
      crosses_flexible_column: false,         // Properly initialised later
      crosses_intrinsic_row_base: false,      // Properly initialised later
      crosses_intrinsic_column_base: false,   // Properly initialised later
      crosses_percentage_column: false,       // Properly initialised later
      crosses_percentage_row: false,          // Properly initialised later
      crosses_intrinsic_row: false,           // Properly initialised later
      crosses_intrinsic_column: false,        // Properly initialised later
      available_space_cache: None,
      min_content_contribution_cache: Size::NONE,
      max_content_contribution_cache: Size::NONE,
      minimum_contribution_cache: Size::NONE,
      resolved_margin_axis_sums_cache: None,
      y_position: 0.0,
      height: 0.0,
    }
  }

  /// This item's placement in the specified axis in OriginZero coordinates
  pub fn placement(&self, axis: AbstractAxis) -> Line<OriginZeroLine> {
    match axis {
      AbstractAxis::Block => self.row,
      AbstractAxis::Inline => self.column,
    }
  }

  /// This item's placement in the specified axis as GridTrackVec indices
  pub fn placement_indexes(&self, axis: AbstractAxis) -> Line<u16> {
    match axis {
      AbstractAxis::Block => self.row_indexes,
      AbstractAxis::Inline => self.column_indexes,
    }
  }

  /// Returns a range which can be used as an index into the GridTrackVec in the specified axis
  /// which will produce a sub-slice of covering all the tracks and lines that this item spans
  /// excluding the lines that bound it.
  pub fn track_range_excluding_lines(&self, axis: AbstractAxis) -> Range<usize> {
    let indexes = self.placement_indexes(axis);
    (indexes.start as usize + 1)..(indexes.end as usize)
  }

  /// Returns the number of tracks that this item spans in the specified axis
  pub fn span(&self, axis: AbstractAxis) -> u16 {
    match axis {
      AbstractAxis::Block => self.row.span(),
      AbstractAxis::Inline => self.column.span(),
    }
  }

  /// Returns the pre-computed value indicating whether the grid item crosses a flexible track in
  /// the specified axis
  pub fn crosses_flexible_track(&self, axis: AbstractAxis) -> bool {
    match axis {
      AbstractAxis::Inline => self.crosses_flexible_column,
      AbstractAxis::Block => self.crosses_flexible_row,
    }
  }

  /// Returns the pre-computed value indicating whether the grid item crosses an intrinsic track in
  /// the specified axis
  pub fn crosses_intrinsic_track(&self, axis: AbstractAxis) -> bool {
    match axis {
      AbstractAxis::Inline => self.crosses_intrinsic_column,
      AbstractAxis::Block => self.crosses_intrinsic_row,
    }
  }

  /// Similar to the spanned_track_limit, but excludes FitContent arguments from the limit.
  /// Used to clamp the automatic minimum contributions of an item
  pub fn spanned_fixed_track_limit(
    &mut self,
    axis: AbstractAxis,
    axis_tracks: &[GridTrack],
    axis_parent_size: Option<f32>,
    resolve_calc_value: &dyn Fn(*const (), f32) -> f32,
  ) -> Option<f32> {
    let spanned_tracks = &axis_tracks[self.track_range_excluding_lines(axis)];
    let tracks_all_fixed = spanned_tracks.iter().all(|track| {
      track
        .max_track_sizing_function
        .definite_value(axis_parent_size, resolve_calc_value)
        .is_some()
    });
    if tracks_all_fixed {
      let limit: f32 = spanned_tracks
        .iter()
        .map(|track| {
          track
            .max_track_sizing_function
            .definite_value(axis_parent_size, resolve_calc_value)
            .unwrap()
        })
        .sum();
      Some(limit)
    } else {
      None
    }
  }

  /// Compute the known_dimensions to be passed to the child sizing functions
  /// The key thing that is being done here is applying stretch alignment, which is necessary to
  /// allow percentage sizes further down the tree to resolve properly in some cases
  fn known_dimensions(
    &mut self,
    tree: &mut impl LayoutPartialTree,
    inner_node_size: Size<Option<f32>>,
    grid_area_size: Size<Option<f32>>,
  ) -> Size<Option<f32>> {
    let margins = self.margins_axis_sums_with_baseline_shims_cached(inner_node_size.width, tree);

    let aspect_ratio = self.aspect_ratio;
    let padding = self
      .padding
      .resolve_or_zero(grid_area_size.width, |val, basis| tree.calc(val, basis));
    let border = self
      .border
      .resolve_or_zero(grid_area_size.width, |val, basis| tree.calc(val, basis));
    let padding_border_size = (padding + border).sum_axes();
    let box_sizing_adjustment = if self.box_sizing == BoxSizing::ContentBox {
      padding_border_size
    } else {
      Size::ZERO
    };
    let inherent_size = self
      .size
      .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
      .maybe_apply_aspect_ratio(aspect_ratio)
      .maybe_add(box_sizing_adjustment);
    let min_size = self
      .min_size
      .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
      .maybe_apply_aspect_ratio(aspect_ratio)
      .maybe_add(box_sizing_adjustment);
    let max_size = self
      .max_size
      .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
      .maybe_apply_aspect_ratio(aspect_ratio)
      .maybe_add(box_sizing_adjustment);

    let grid_area_minus_item_margins_size = grid_area_size.maybe_sub(margins);

    // If node is absolutely positioned and width is not set explicitly, then deduce it
    // from left, right and container_content_box if both are set.
    let width = inherent_size.width.or_else(|| {
      // Apply width based on stretch alignment if:
      //  - Alignment style is "stretch"
      //  - The node is not absolutely positioned
      //  - The node does not have auto margins in this axis.
      if !self.margin.left.is_auto()
        && !self.margin.right.is_auto()
        && self.justify_self == AlignSelf::Stretch
      {
        return grid_area_minus_item_margins_size.width;
      }

      None
    });
    // Reapply aspect ratio after stretch and absolute position width adjustments
    let Size { width, height } = Size {
      width,
      height: inherent_size.height,
    }
    .maybe_apply_aspect_ratio(aspect_ratio);

    let height = height.or_else(|| {
      // Apply height based on stretch alignment if:
      //  - Alignment style is "stretch"
      //  - The node is not absolutely positioned
      //  - The node does not have auto margins in this axis.
      if !self.margin.top.is_auto()
        && !self.margin.bottom.is_auto()
        && self.align_self == AlignSelf::Stretch
      {
        return grid_area_minus_item_margins_size.height;
      }

      None
    });
    // Reapply aspect ratio after stretch and absolute position height adjustments
    let Size { width, height } = Size { width, height }.maybe_apply_aspect_ratio(aspect_ratio);

    // Clamp size by min and max width/height
    let Size { width, height } = Size { width, height }.maybe_clamp(min_size, max_size);

    Size { width, height }
  }

  /// Compute the available_space to be passed to the child sizing functions
  /// These are estimates based on either the max track sizing function or the provisional base size in the opposite
  /// axis to the one currently being sized.
  /// https://www.w3.org/TR/css-grid-1/#algo-overview
  pub fn available_space(
    &self,
    axis: AbstractAxis,
    other_axis_tracks: &[GridTrack],
    other_axis_available_space: Option<f32>,
    get_track_size_estimate: impl Fn(&GridTrack, Option<f32>) -> Option<f32>,
  ) -> Size<Option<f32>> {
    let item_other_axis_size: Option<f32> = {
      other_axis_tracks[self.track_range_excluding_lines(axis.other())]
        .iter()
        .map(|track| {
          get_track_size_estimate(track, other_axis_available_space)
            .map(|size| size + track.content_alignment_adjustment)
        })
        .sum::<Option<f32>>()
    };

    let mut size = Size::NONE;
    size.set(axis.other(), item_other_axis_size);
    size
  }

  /// Retrieve the available_space from the cache or compute them using the passed parameters
  pub fn available_space_cached(
    &mut self,
    axis: AbstractAxis,
    other_axis_tracks: &[GridTrack],
    other_axis_available_space: Option<f32>,
    get_track_size_estimate: impl Fn(&GridTrack, Option<f32>) -> Option<f32>,
  ) -> Size<Option<f32>> {
    self.available_space_cache.unwrap_or_else(|| {
      let available_spaces = self.available_space(
        axis,
        other_axis_tracks,
        other_axis_available_space,
        get_track_size_estimate,
      );
      self.available_space_cache = Some(available_spaces);
      available_spaces
    })
  }

  /// Compute the item's resolved margins for size contributions. Horizontal percentage margins always resolve
  /// to zero if the container size is indefinite as otherwise this would introduce a cyclic dependency.
  #[inline(always)]
  #[allow(dead_code)]
  pub fn margins_axis_sums_with_baseline_shims(
    &self,
    inner_node_width: Option<f32>,
    tree: &impl LayoutPartialTree,
  ) -> Size<f32> {
    (Rect {
      left: self
        .margin
        .left
        .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis))
        + self.baseline_shim.x,
      right: self
        .margin
        .right
        .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis)),
      top: self
        .margin
        .top
        .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis))
        + self.baseline_shim.y,
      bottom: self
        .margin
        .bottom
        .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis)),
    } + self.extra_margin)
      .sum_axes()
  }

  /// Retrieve the item's resolved margins for size contributions from an ephemeral cache.
  ///
  /// This cache is intended for grid track sizing, where intrinsic contribution queries may be
  /// served from cache but still need to account for margins. Margin resolution can be
  /// surprisingly expensive (percent + calc), so we cache the resolved sums per `inner_node_width`
  /// basis.
  ///
  /// The cached sums exclude baseline shims because baseline shims can change across passes. The
  /// returned value always includes the *current* baseline shims.
  #[inline(always)]
  pub fn margins_axis_sums_with_baseline_shims_cached(
    &mut self,
    inner_node_width: Option<f32>,
    tree: &impl LayoutPartialTree,
  ) -> Size<f32> {
    #[inline(always)]
    fn canonicalize_inner_node_width(width: Option<f32>) -> Option<u32> {
      width.map(|w| {
        // Canonicalize 0.0 and -0.0 so we don't miss cache hits due to sign-bit noise.
        let w = if w == 0.0 { 0.0 } else { w };
        w.to_bits()
      })
    }

    let key = canonicalize_inner_node_width(inner_node_width);
    let base_sums = match self.resolved_margin_axis_sums_cache {
      Some((cached_key, Some(cached_sums), _)) if cached_key == key => cached_sums,
      _ => {
        let cached_start_margins = match self.resolved_margin_axis_sums_cache {
          Some((cached_key, _, cached_start)) if cached_key == key => cached_start,
          _ => Point::NONE,
        };

        let left = cached_start_margins.x.unwrap_or_else(|| {
          self
            .margin
            .left
            .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis))
        });
        let top = cached_start_margins.y.unwrap_or_else(|| {
          self
            .margin
            .top
            .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis))
        });
        let right = self
          .margin
          .right
          .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis));
        let bottom = self
          .margin
          .bottom
          .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis));

        let resolved_sums = (Rect {
          left,
          right,
          top,
          bottom,
        } + self.extra_margin)
          .sum_axes();

        self.resolved_margin_axis_sums_cache = Some((
          key,
          Some(resolved_sums),
          Point {
            x: Some(left),
            y: Some(top),
          },
        ));
        resolved_sums
      }
    };

    Size {
      width: base_sums.width + self.baseline_shim.x,
      height: base_sums.height + self.baseline_shim.y,
    }
  }

  /// Resolve the start margin in the axis relevant for baseline shimming, using the same cache
  /// as [`Self::margins_axis_sums_with_baseline_shims_cached`].
  ///
  /// Returns the resolved margin **excluding** `extra_margin` and baseline shims.
  #[inline(always)]
  pub fn baseline_shim_margin_start_cached(
    &mut self,
    axis: AbstractAxis,
    inner_node_width: Option<f32>,
    tree: &impl LayoutPartialTree,
  ) -> f32 {
    #[inline(always)]
    fn canonicalize_inner_node_width(width: Option<f32>) -> Option<u32> {
      width.map(|w| {
        let w = if w == 0.0 { 0.0 } else { w };
        w.to_bits()
      })
    }

    let key = canonicalize_inner_node_width(inner_node_width);
    if let Some((cached_key, cached_sums, cached_start)) = self.resolved_margin_axis_sums_cache {
      if cached_key == key {
        let cached_value = match axis {
          AbstractAxis::Inline => cached_start.y,
          AbstractAxis::Block => cached_start.x,
        };
        if let Some(val) = cached_value {
          return val;
        }

        // Need to compute the missing start margin, but we can preserve any existing cached values
        // for this key.
        let computed = match axis {
          AbstractAxis::Inline => self
            .margin
            .top
            .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis)),
          AbstractAxis::Block => self
            .margin
            .left
            .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis)),
        };
        let mut new_start = cached_start;
        match axis {
          AbstractAxis::Inline => new_start.y = Some(computed),
          AbstractAxis::Block => new_start.x = Some(computed),
        }
        self.resolved_margin_axis_sums_cache = Some((key, cached_sums, new_start));
        return computed;
      }
    }

    // No cache for this key yet: compute only the requested start margin and store it (without
    // forcing full axis-sum resolution).
    let computed = match axis {
      AbstractAxis::Inline => self
        .margin
        .top
        .resolve_or_zero(inner_node_width, |val, basis| tree.calc(val, basis)),
      AbstractAxis::Block => self
        .margin
        .left
        .resolve_or_zero(Some(0.0), |val, basis| tree.calc(val, basis)),
    };
    let start = match axis {
      AbstractAxis::Inline => Point {
        x: None,
        y: Some(computed),
      },
      AbstractAxis::Block => Point {
        x: Some(computed),
        y: None,
      },
    };
    self.resolved_margin_axis_sums_cache = Some((key, None, start));
    computed
  }

  /// Compute the item's min content contribution from the provided parameters
  pub fn min_content_contribution(
    &mut self,
    axis: AbstractAxis,
    tree: &mut impl LayoutPartialTree,
    available_space: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    let known_dimensions = self.known_dimensions(tree, inner_node_size, available_space);
    tree.measure_child_size(
      self.node,
      known_dimensions,
      inner_node_size,
      available_space.map(|opt| match opt {
        Some(size) => AvailableSpace::Definite(size),
        None => AvailableSpace::MinContent,
      }),
      SizingMode::InherentSize,
      axis.as_abs_naive(),
      Line::FALSE,
    )
  }

  /// Retrieve the item's min content contribution from the cache or compute it using the provided parameters
  #[inline(always)]
  pub fn min_content_contribution_cached(
    &mut self,
    axis: AbstractAxis,
    tree: &mut impl LayoutPartialTree,
    available_space: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    self
      .min_content_contribution_cache
      .get(axis)
      .unwrap_or_else(|| {
        let size = self.min_content_contribution(axis, tree, available_space, inner_node_size);
        self.min_content_contribution_cache.set(axis, Some(size));
        size
      })
  }

  /// Compute the item's max content contribution from the provided parameters
  pub fn max_content_contribution(
    &mut self,
    axis: AbstractAxis,
    tree: &mut impl LayoutPartialTree,
    available_space: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    let known_dimensions = self.known_dimensions(tree, inner_node_size, available_space);
    tree.measure_child_size(
      self.node,
      known_dimensions,
      inner_node_size,
      available_space.map(|opt| match opt {
        Some(size) => AvailableSpace::Definite(size),
        None => AvailableSpace::MaxContent,
      }),
      SizingMode::InherentSize,
      axis.as_abs_naive(),
      Line::FALSE,
    )
  }

  /// Retrieve the item's max content contribution from the cache or compute it using the provided parameters
  #[inline(always)]
  pub fn max_content_contribution_cached(
    &mut self,
    axis: AbstractAxis,
    tree: &mut impl LayoutPartialTree,
    available_space: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    self
      .max_content_contribution_cache
      .get(axis)
      .unwrap_or_else(|| {
        let size = self.max_content_contribution(axis, tree, available_space, inner_node_size);
        self.max_content_contribution_cache.set(axis, Some(size));
        size
      })
  }

  /// The minimum contribution of an item is the smallest outer size it can have.
  /// Specifically:
  ///   - If the item’s computed preferred size behaves as auto or depends on the size of its containing block in the relevant axis:
  ///     Its minimum contribution is the outer size that would result from assuming the item’s used minimum size as its preferred size;
  ///   - Else the item’s minimum contribution is its min-content contribution.
  ///
  /// Because the minimum contribution often depends on the size of the item’s content, it is considered a type of intrinsic size contribution.
  /// See: https://www.w3.org/TR/css-grid-1/#min-size-auto
  pub fn minimum_contribution(
    &mut self,
    tree: &mut impl LayoutPartialTree,
    axis: AbstractAxis,
    axis_tracks: &[GridTrack],
    other_axis_tracks: &[GridTrack],
    known_dimensions: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    let padding = self
      .padding
      .resolve_or_zero(inner_node_size, |val, basis| tree.calc(val, basis));
    let border = self
      .border
      .resolve_or_zero(inner_node_size, |val, basis| tree.calc(val, basis));
    let padding_border_size = (padding + border).sum_axes();
    let box_sizing_adjustment = if self.box_sizing == BoxSizing::ContentBox {
      padding_border_size
    } else {
      Size::ZERO
    };
    // CSS Grid treats percentage/calc preferred sizes as depending on the containing block size.
    // During intrinsic track sizing, those sizes must *not* be treated as definite, otherwise
    // items like `width: 100%` can force their tracks to expand to the full grid container width
    // (a cyclic dependency).
    //
    // https://www.w3.org/TR/css-grid-1/#min-size-auto (minimum contribution definition)
    let mut size_style = self.size;
    if size_style.get(axis).0.uses_percentage() {
      size_style.set(axis, Dimension::auto());
    }

    let size = size_style
      .maybe_resolve(inner_node_size, |val, basis| tree.calc(val, basis))
      .maybe_add(box_sizing_adjustment)
      .get(axis)
      .or_else(|| {
        self
          .min_size
          .maybe_resolve(inner_node_size, |val, basis| tree.calc(val, basis))
          .maybe_add(box_sizing_adjustment)
          .get(axis)
      })
      .or_else(|| self.overflow.get(axis).maybe_into_automatic_min_size())
      .unwrap_or_else(|| {
        // Automatic minimum size. See https://www.w3.org/TR/css-grid-1/#min-size-auto

        // To provide a more reasonable default minimum size for grid items, the used value of its automatic minimum size
        // in a given axis is the content-based minimum size if all of the following are true:
        // https://www.w3.org/TR/css-grid-1/#min-size-auto
        let item_axis_tracks = &axis_tracks[self.track_range_excluding_lines(axis)];

        // 1) it spans at least one track in that axis whose min track sizing function is auto
        let spans_auto_min_track = item_axis_tracks
          .iter()
          // TODO: should this be 'behaves as auto' rather than just literal auto?
          .any(|track| track.min_track_sizing_function.is_auto());

        // 2) if it spans more than one track in that axis, none of those tracks are flexible
        let only_span_one_track = item_axis_tracks.len() == 1;
        let spans_a_flexible_track = item_axis_tracks
          .iter()
          .any(|track| track.max_track_sizing_function.is_fr());

        let use_content_based_minimum =
          spans_auto_min_track && (only_span_one_track || !spans_a_flexible_track);

        // Otherwise, the automatic minimum size is zero, as usual.
        if !use_content_based_minimum {
          return 0.0;
        }

        // --- Content-based minimum size -----------------------------------
        //
        // See "Automatic Minimum Size of Grid Items":
        // https://www.w3.org/TR/css-grid-1/#min-size-auto
        //
        // The content-based minimum size in an axis is:
        //   specified size suggestion -> transferred size suggestion -> content size suggestion
        // with additional clamping rules.

        // If the item spans only fixed max tracks then the specified/content size suggestions
        // (and the input from this dimension to the transferred size suggestion in the opposite dimension)
        // are clamped to the grid area's maximum size in that dimension.
        let axis_fixed_track_limit = self.spanned_fixed_track_limit(
          axis,
          axis_tracks,
          inner_node_size.get(axis),
          &|val, basis| tree.resolve_calc_value(val, basis),
        );
        let other_axis_fixed_track_limit = self.spanned_fixed_track_limit(
          axis.other(),
          other_axis_tracks,
          inner_node_size.get(axis.other()),
          &|val, basis| tree.resolve_calc_value(val, basis),
        );

        fn resolve_dimension<Tree: LayoutPartialTree>(
          tree: &Tree,
          dimension: Dimension,
          context: Option<f32>,
        ) -> Option<f32> {
          dimension.maybe_resolve(context, |val, basis| tree.calc(val, basis))
        }

        fn resolve_dimension_with_box_sizing<Tree: LayoutPartialTree>(
          tree: &Tree,
          dimension: Dimension,
          context: Option<f32>,
          axis: AbstractAxis,
          box_sizing_adjustment: Size<f32>,
        ) -> Option<f32> {
          resolve_dimension(tree, dimension, context)
            .map(|val| val + box_sizing_adjustment.get(axis))
        }

        // Specified size suggestion: if preferred size in the axis is definite, use it.
        let specified_size_suggestion = resolve_dimension_with_box_sizing(
          tree,
          self.size.get(axis),
          known_dimensions.get(axis),
          axis,
          box_sizing_adjustment,
        )
        .maybe_min(axis_fixed_track_limit);

        // Transferred size suggestion (for preferred aspect ratios).
        let transferred_size_suggestion = self.aspect_ratio.and_then(|ratio| {
          if ratio <= 0.0 {
            return None;
          }

          // Preferred size in the opposite axis must be definite.
          let opposite_preferred = resolve_dimension_with_box_sizing(
            tree,
            self.size.get(axis.other()),
            known_dimensions.get(axis.other()),
            axis.other(),
            box_sizing_adjustment,
          )?;

          // Clamp opposite preferred size by definite opposite min/max.
          let opposite_min = resolve_dimension_with_box_sizing(
            tree,
            self.min_size.get(axis.other()),
            known_dimensions.get(axis.other()),
            axis.other(),
            box_sizing_adjustment,
          );
          let opposite_max = resolve_dimension_with_box_sizing(
            tree,
            self.max_size.get(axis.other()),
            known_dimensions.get(axis.other()),
            axis.other(),
            box_sizing_adjustment,
          );
          let opposite_preferred = Some(opposite_preferred)
            .maybe_clamp(opposite_min, opposite_max)
            .maybe_min(other_axis_fixed_track_limit)?;

          // Convert through aspect ratio.
          let mut transferred = match axis {
            AbstractAxis::Inline => opposite_preferred * ratio,
            AbstractAxis::Block => opposite_preferred / ratio,
          };

          // If the item has a definite preferred size or maximum size in the relevant axis, cap.
          // For this purpose, any indefinite percentages are resolved against zero (and considered definite).
          let preferred_cap = resolve_dimension_with_box_sizing(
            tree,
            self.size.get(axis),
            Some(0.0),
            axis,
            box_sizing_adjustment,
          );
          let max_cap = resolve_dimension_with_box_sizing(
            tree,
            self.max_size.get(axis),
            Some(0.0),
            axis,
            box_sizing_adjustment,
          );
          transferred = transferred.maybe_min(preferred_cap).maybe_min(max_cap);

          Some(transferred)
        });

        // Content size suggestion: min-content size in the axis, clamped by opposite min/max through aspect ratio.
        let content_size_suggestion = {
          let mut min_content =
            self.min_content_contribution_cached(axis, tree, known_dimensions, inner_node_size);

          if let Some(ratio) = self.aspect_ratio {
            if ratio > 0.0 {
              let opposite_min = resolve_dimension_with_box_sizing(
                tree,
                self.min_size.get(axis.other()),
                known_dimensions.get(axis.other()),
                axis.other(),
                box_sizing_adjustment,
              );
              let opposite_max = resolve_dimension_with_box_sizing(
                tree,
                self.max_size.get(axis.other()),
                known_dimensions.get(axis.other()),
                axis.other(),
                box_sizing_adjustment,
              );

              let converted_min = opposite_min.map(|val| match axis {
                AbstractAxis::Inline => val * ratio,
                AbstractAxis::Block => val / ratio,
              });
              let converted_max = opposite_max.map(|val| match axis {
                AbstractAxis::Inline => val * ratio,
                AbstractAxis::Block => val / ratio,
              });

              min_content = min_content.maybe_clamp(converted_min, converted_max);
            }
          }

          // Clamp by the grid area's max size if spanning only fixed max tracks.
          min_content.maybe_min(axis_fixed_track_limit)
        };

        // Choose the content-based minimum size.
        let mut suggestion = if let Some(specified) = specified_size_suggestion {
          specified
        } else if let Some(transferred) = transferred_size_suggestion {
          // Note: The Grid 1 spec only uses the transferred size suggestion for replaced elements.
          // FastRender uses `aspect-ratio` on non-replaced boxes; align with that behavior.
          transferred
        } else {
          content_size_suggestion
        };

        // In all cases, clamp by the definite max-size in this axis.
        let definite_max_size = resolve_dimension_with_box_sizing(
          tree,
          self.max_size.get(axis),
          known_dimensions.get(axis),
          axis,
          box_sizing_adjustment,
        );
        suggestion = suggestion.maybe_min(definite_max_size);

        // Compressible replaced element caps.
        // Indefinite percentages are resolved against zero (and considered definite) for this purpose.
        if self.is_compressible_replaced {
          let preferred_cap = resolve_dimension_with_box_sizing(
            tree,
            self.size.get(axis),
            Some(0.0),
            axis,
            box_sizing_adjustment,
          );
          let max_cap = resolve_dimension_with_box_sizing(
            tree,
            self.max_size.get(axis),
            Some(0.0),
            axis,
            box_sizing_adjustment,
          );
          suggestion = suggestion.maybe_min(preferred_cap).maybe_min(max_cap);
        }

        suggestion
      });
    size
  }

  /// Retrieve the item's minimum contribution from the cache or compute it using the provided parameters
  #[inline(always)]
  pub fn minimum_contribution_cached(
    &mut self,
    tree: &mut impl LayoutPartialTree,
    axis: AbstractAxis,
    axis_tracks: &[GridTrack],
    other_axis_tracks: &[GridTrack],
    known_dimensions: Size<Option<f32>>,
    inner_node_size: Size<Option<f32>>,
  ) -> f32 {
    self
      .minimum_contribution_cache
      .get(axis)
      .unwrap_or_else(|| {
        let size = self.minimum_contribution(
          tree,
          axis,
          axis_tracks,
          other_axis_tracks,
          known_dimensions,
          inner_node_size,
        );
        self.minimum_contribution_cache.set(axis, Some(size));
        size
      })
  }
}

#[cfg(all(test, feature = "taffy_tree"))]
mod tests {
  use super::*;
  use crate::style::Style;
  use crate::sys::DefaultCheapStr;
  use crate::tree::{Layout, LayoutInput, LayoutOutput};
  use core::iter;

  struct DummyTree {
    style: Style<DefaultCheapStr>,
  }

  impl crate::tree::TraversePartialTree for DummyTree {
    type ChildIter<'a>
      = iter::Empty<NodeId>
    where
      Self: 'a;

    fn child_ids(&self, _parent_node_id: NodeId) -> Self::ChildIter<'_> {
      iter::empty()
    }

    fn child_count(&self, _parent_node_id: NodeId) -> usize {
      0
    }

    fn get_child_id(&self, _parent_node_id: NodeId, _child_index: usize) -> NodeId {
      NodeId::new(0)
    }
  }

  impl LayoutPartialTree for DummyTree {
    type CoreContainerStyle<'a>
      = &'a Style<DefaultCheapStr>
    where
      Self: 'a;

    type CustomIdent = DefaultCheapStr;

    fn get_core_container_style(&self, _node_id: NodeId) -> Self::CoreContainerStyle<'_> {
      &self.style
    }

    fn set_unrounded_layout(&mut self, _node_id: NodeId, _layout: &Layout) {}

    fn compute_child_layout(&mut self, _node_id: NodeId, _inputs: LayoutInput) -> LayoutOutput {
      LayoutOutput::from_outer_size(Size::ZERO)
    }
  }

  #[test]
  fn margins_axis_sums_cache_respects_inner_node_width_and_baseline_shims() {
    let tree = DummyTree {
      style: Style::default(),
    };

    let style = Style::<DefaultCheapStr> {
      // Top/bottom percent margins resolve against `inner_node_width`.
      margin: Rect {
        left: LengthPercentageAuto::percent(0.5),
        right: LengthPercentageAuto::length(5.0),
        top: LengthPercentageAuto::percent(0.1),
        bottom: LengthPercentageAuto::percent(0.2),
      },
      ..Default::default()
    };

    let mut item = GridItem::new_with_placement_style_and_order(
      NodeId::new(1),
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      style,
      AlignItems::Stretch,
      AlignItems::Stretch,
      0,
    );
    item.baseline_shim = Point { x: 6.0, y: 7.0 };
    item.extra_margin = Rect {
      left: 1.0,
      right: 2.0,
      top: 3.0,
      bottom: 4.0,
    };

    // First call populates the cache.
    let first = item.margins_axis_sums_with_baseline_shims_cached(Some(100.0), &tree);
    assert_eq!(first, Size { width: 14.0, height: 44.0 });
    let cache_after_first = item.resolved_margin_axis_sums_cache;
    assert_eq!(
      cache_after_first,
      Some((
        Some(100.0f32.to_bits()),
        Some(Size { width: 8.0, height: 37.0 }),
        Point {
          x: Some(0.0),
          y: Some(10.0)
        }
      ))
    );

    // Second call with the same key should return the same results and leave the cache unchanged.
    let second = item.margins_axis_sums_with_baseline_shims_cached(Some(100.0), &tree);
    assert_eq!(second, first);
    assert_eq!(item.resolved_margin_axis_sums_cache, cache_after_first);

    // A different inner_node_width should update the resolved margins.
    let third = item.margins_axis_sums_with_baseline_shims_cached(Some(200.0), &tree);
    assert_eq!(third, Size { width: 14.0, height: 74.0 });
    assert_eq!(
      item.resolved_margin_axis_sums_cache,
      Some((
        Some(200.0f32.to_bits()),
        Some(Size { width: 8.0, height: 67.0 }),
        Point {
          x: Some(0.0),
          y: Some(20.0)
        }
      ))
    );
  }

  #[test]
  fn grid_item_percentage_padding_border_resolve_against_width() {
    let mut tree = DummyTree {
      style: Style::default(),
    };

    let style = Style::<DefaultCheapStr> {
      box_sizing: BoxSizing::ContentBox,
      size: Size {
        width: Dimension::auto(),
        height: Dimension::length(10.0),
      },
      padding: Rect {
        left: LengthPercentage::length(0.0),
        right: LengthPercentage::length(0.0),
        top: LengthPercentage::percent(0.1),
        bottom: LengthPercentage::percent(0.1),
      },
      border: Rect {
        left: LengthPercentage::length(0.0),
        right: LengthPercentage::length(0.0),
        top: LengthPercentage::percent(0.05),
        bottom: LengthPercentage::percent(0.05),
      },
      ..Default::default()
    };

    let mut item = GridItem::new_with_placement_style_and_order(
      NodeId::new(1),
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      style,
      AlignItems::Stretch,
      AlignItems::Stretch,
      0,
    );

    // Width and height are deliberately different so that incorrectly resolving vertical
    // percentage padding/border against height will change the result.
    let grid_area_size = Size {
      width: Some(200.0),
      height: Some(100.0),
    };
    let inner_node_size = Size {
      width: Some(200.0),
      height: Some(100.0),
    };

    let known = item.known_dimensions(&mut tree, inner_node_size, grid_area_size);
    // top/bottom padding = 10% of width (200) => 20 each => 40 total
    // top/bottom border = 5% of width (200) => 10 each => 20 total
    // content-box height = 10 => border-box height = 10 + 40 + 20 = 70
    assert_eq!(known.height, Some(70.0));
  }
}
