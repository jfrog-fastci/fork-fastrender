//! Implements the track sizing algorithm
//! <https://www.w3.org/TR/css-grid-1/#layout-algorithm>
use super::types::{GridItem, GridTrack, GridTrackKind, TrackCounts};
use crate::geometry::{AbstractAxis, Line, Size};
use crate::style::{AlignContent, AlignSelf, AvailableSpace};
use crate::style_helpers::TaffyMinContent;
use crate::tree::{LayoutPartialTree, LayoutPartialTreeExt, SizingMode};
use crate::util::check_layout_abort;
use crate::util::sys::{f32_max, f32_min, Vec};
use crate::util::MaybeMath;
use crate::CompactLength;
use arrayvec::ArrayVec;
use core::cmp::Ordering;

#[derive(Clone, Copy)]
struct PrefixSumMaybe {
  sum: f32,
  none: u32,
}

#[inline(always)]
fn sum_range(prefix: &[PrefixSumMaybe], range: core::ops::Range<usize>) -> Option<f32> {
  let start = prefix[range.start];
  let end = prefix[range.end];
  if end.none - start.none > 0 {
    None
  } else {
    Some(end.sum - start.sum)
  }
}

/// Takes an axis, and a list of grid items sorted firstly by whether they cross a flex track
/// in the specified axis (items that don't cross a flex track first) and then by the number
/// of tracks they cross in specified axis (ascending order).
struct ItemBatcher {
  /// The axis in which the ItemBatcher is operating. Used when querying properties from items.
  axis: AbstractAxis,
  /// The starting index of the current batch
  index_offset: usize,
  /// The span of the items in the current batch
  current_span: u16,
  /// Whether the current batch of items cross a flexible track
  current_is_flex: bool,
}

impl ItemBatcher {
  /// Create a new ItemBatcher for the specified axis
  #[inline(always)]
  fn new(axis: AbstractAxis) -> Self {
    ItemBatcher {
      index_offset: 0,
      axis,
      current_span: 1,
      current_is_flex: false,
    }
  }

  /// This is basically a manual version of Iterator::next which passes `items`
  /// in as a parameter on each iteration to work around borrow checker rules
  #[inline]
  fn next<'items>(
    &mut self,
    items: &'items mut [GridItem],
  ) -> Option<(&'items mut [GridItem], bool)> {
    // `resolve_intrinsic_track_sizes` can run for a long time on large grids. Ensure we
    // periodically check for cooperative abort requests so render deadlines can surface as
    // structured timeouts rather than hard kills.
    check_layout_abort();

    if self.current_is_flex || self.index_offset >= items.len() {
      return None;
    }

    let item = &items[self.index_offset];
    self.current_span = item.span(self.axis);
    self.current_is_flex = item.crosses_flexible_track(self.axis);

    let next_index_offset = if self.current_is_flex {
      items.len()
    } else {
      // `ItemBatcher` advances in monotonically increasing `index_offset` order, so scanning from
      // the start of the slice would devolve into O(n^2) behaviour (re-checking already-processed
      // items for every batch). Restrict the scan to the tail slice to keep the overall batching
      // work O(n).
      let start = (self.index_offset + 1).min(items.len());
      items[start..]
        .iter()
        .position(|item: &GridItem| {
          item.crosses_flexible_track(self.axis) || item.span(self.axis) > self.current_span
        })
        .map(|pos| start + pos)
        .unwrap_or(items.len())
    };

    let batch_range = self.index_offset..next_index_offset;
    self.index_offset = next_index_offset;

    let batch = &mut items[batch_range];
    Some((batch, self.current_is_flex))
  }
}

/// This struct captures a bunch of variables which are used to compute the intrinsic sizes of children so that those variables
/// don't have to be passed around all over the place below. It then has methods that implement the intrinsic sizing computations
struct IntrisicSizeMeasurer<'tree, 'oat, Tree>
where
  Tree: LayoutPartialTree,
{
  /// The layout tree
  tree: &'tree mut Tree,
  /// The tracks in the opposite axis to the one we are currently sizing
  other_axis_tracks: &'oat [GridTrack],
  /// The axis we are currently sizing
  axis: AbstractAxis,
  /// The available grid space
  inner_node_size: Size<Option<f32>>,
}

impl<Tree> IntrisicSizeMeasurer<'_, '_, Tree>
where
  Tree: LayoutPartialTree,
{
  /// Compute the available_space to be passed to the child sizing functions
  /// These are estimates based on either the max track sizing function or the provisional base size in the opposite
  /// axis to the one currently being sized.
  /// https://www.w3.org/TR/css-grid-1/#algo-overview
  #[inline(always)]
  fn available_space(&self, item: &mut GridItem) -> Size<Option<f32>> {
    // `resolve_intrinsic_track_sizes` pre-fills this cache for the current sizing pass.
    item
      .available_space_cache
      .expect("available_space_cache should be set before intrinsic measurement")
  }

  /// Compute the item's resolved margins for size contributions. Horizontal percentage margins always resolve
  /// to zero if the container size is indefinite as otherwise this would introduce a cyclic dependency.
  #[inline(always)]
  fn margins_axis_sums_with_baseline_shims(&self, item: &mut GridItem) -> Size<f32> {
    item.margins_axis_sums_with_baseline_shims_cached(self.inner_node_size.width, self.tree)
  }

  /// Simple pass-through function to `LayoutPartialTreeExt::calc`
  #[inline(always)]
  fn calc(&self, val: *const (), basis: f32) -> f32 {
    self.tree.calc(val, basis)
  }

  /// Retrieve the item's min content contribution from the cache or compute it using the provided parameters
  #[inline(always)]
  fn min_content_contribution(&mut self, item: &mut GridItem) -> f32 {
    let available_space = self.available_space(item);
    let margin_axis_sums = self.margins_axis_sums_with_baseline_shims(item);
    let contribution = item.min_content_contribution_cached(
      self.axis,
      self.tree,
      available_space,
      self.inner_node_size,
    );
    contribution + margin_axis_sums.get(self.axis)
  }

  /// Retrieve the item's max content contribution from the cache or compute it using the provided parameters
  #[inline(always)]
  fn max_content_contribution(&mut self, item: &mut GridItem) -> f32 {
    let available_space = self.available_space(item);
    let margin_axis_sums = self.margins_axis_sums_with_baseline_shims(item);
    let contribution = item.max_content_contribution_cached(
      self.axis,
      self.tree,
      available_space,
      self.inner_node_size,
    );
    contribution + margin_axis_sums.get(self.axis)
  }

  /// The minimum contribution of an item is the smallest outer size it can have.
  /// Specifically:
  ///   - If the item’s computed preferred size behaves as auto or depends on the size of its containing block in the relevant axis:
  ///     Its minimum contribution is the outer size that would result from assuming the item’s used minimum size as its preferred size;
  ///   - Else the item’s minimum contribution is its min-content contribution.
  ///
  /// Because the minimum contribution often depends on the size of the item’s content, it is considered a type of intrinsic size contribution.
  #[inline(always)]
  fn minimum_contribution(&mut self, item: &mut GridItem, axis_tracks: &[GridTrack]) -> f32 {
    let available_space = self.available_space(item);
    let margin_axis_sums = self.margins_axis_sums_with_baseline_shims(item);
    let contribution = item.minimum_contribution_cached(
      self.tree,
      self.axis,
      axis_tracks,
      self.other_axis_tracks,
      available_space,
      self.inner_node_size,
    );
    contribution + margin_axis_sums.get(self.axis)
  }
}

/// To make track sizing efficient we want to order tracks
/// Here a placement is either a Line<i16> representing a row-start/row-end or a column-start/column-end
#[inline(always)]
pub(super) fn cmp_by_cross_flex_then_span_then_start(
  axis: AbstractAxis,
) -> impl FnMut(&GridItem, &GridItem) -> Ordering {
  move |item_a: &GridItem, item_b: &GridItem| -> Ordering {
    match (
      item_a.crosses_flexible_track(axis),
      item_b.crosses_flexible_track(axis),
    ) {
      (false, true) => Ordering::Less,
      (true, false) => Ordering::Greater,
      _ => {
        let placement_a = item_a.placement(axis);
        let placement_b = item_b.placement(axis);
        match placement_a.span().cmp(&placement_b.span()) {
          Ordering::Less => Ordering::Less,
          Ordering::Greater => Ordering::Greater,
          Ordering::Equal => placement_a.start.cmp(&placement_b.start),
        }
      }
    }
  }
}

/// When applying the track sizing algorithm and estimating the size in the other axis for content sizing items
/// we should take into account align-content/justify-content if both the grid container and all items in the
/// other axis have definite sizes. This function computes such a per-gutter additional size adjustment.
#[inline(always)]
pub(super) fn compute_alignment_gutter_adjustment(
  alignment: AlignContent,
  axis_inner_node_size: Option<f32>,
  get_track_size_estimate: impl Fn(&GridTrack, Option<f32>) -> Option<f32>,
  tracks: &[GridTrack],
) -> f32 {
  if tracks.len() <= 1 {
    return 0.0;
  }

  // As items never cross the outermost gutters in a grid, we can simplify our calculations by treating
  // AlignContent::Start and AlignContent::End the same
  let outer_gutter_weight = match alignment {
    AlignContent::Start => 1,
    AlignContent::FlexStart => 1,
    AlignContent::End => 1,
    AlignContent::FlexEnd => 1,
    AlignContent::Center => 1,
    AlignContent::Stretch => 0,
    AlignContent::SpaceBetween => 0,
    AlignContent::SpaceAround => 1,
    AlignContent::SpaceEvenly => 1,
  };

  let inner_gutter_weight = match alignment {
    AlignContent::FlexStart => 0,
    AlignContent::Start => 0,
    AlignContent::FlexEnd => 0,
    AlignContent::End => 0,
    AlignContent::Center => 0,
    AlignContent::Stretch => 0,
    AlignContent::SpaceBetween => 1,
    AlignContent::SpaceAround => 2,
    AlignContent::SpaceEvenly => 1,
  };

  if inner_gutter_weight == 0 {
    return 0.0;
  }

  if let Some(axis_inner_node_size) = axis_inner_node_size {
    let free_space = tracks
      .iter()
      .map(|track| get_track_size_estimate(track, Some(axis_inner_node_size)))
      .sum::<Option<f32>>()
      .map(|track_size_sum| f32_max(0.0, axis_inner_node_size - track_size_sum))
      .unwrap_or(0.0);

    let weighted_track_count = (((tracks.len() - 3) / 2) * inner_gutter_weight as usize)
      + (2 * outer_gutter_weight as usize);

    return (free_space / weighted_track_count as f32) * inner_gutter_weight as f32;
  }

  0.0
}

/// Convert origin-zero coordinates track placement in grid track vector indexes
#[inline(always)]
pub(super) fn resolve_item_track_indexes(
  items: &mut [GridItem],
  column_counts: TrackCounts,
  row_counts: TrackCounts,
) {
  for item in items {
    check_layout_abort();
    item.column_indexes = item
      .column
      .map(|line| line.into_track_vec_index(column_counts) as u16);
    item.row_indexes = item
      .row
      .map(|line| line.into_track_vec_index(row_counts) as u16);
  }
}

/// Determine (in each axis) whether the item crosses any flexible tracks
#[inline(always)]
pub(super) fn determine_if_item_crosses_flexible_or_intrinsic_tracks(
  items: &mut Vec<GridItem>,
  columns: &[GridTrack],
  rows: &[GridTrack],
) {
  #[derive(Clone, Copy)]
  struct TrackPrefixCounts {
    flexible: u16,
    intrinsic: u16,
    percentage: u16,
  }

  #[inline(always)]
  fn build_prefix_counts(tracks: &[GridTrack]) -> Vec<TrackPrefixCounts> {
    // Building each prefix array in separate passes would require scanning the full track list
    // 3x per axis. We compute all three in one pass to reduce CPU time for large grids, while also
    // storing them in a single allocation to reduce allocator churn.
    //
    // Use `u16` for the prefix values: the count of "real" tracks is bounded by `TrackCounts` (u16),
    // and gutters/lines are never flexible/intrinsic/percentage tracks.
    let mut prefix: Vec<TrackPrefixCounts> = Vec::with_capacity(tracks.len() + 1);

    let mut flexible_count = 0u16;
    let mut intrinsic_count = 0u16;
    let mut percentage_count = 0u16;
    prefix.push(TrackPrefixCounts {
      flexible: flexible_count,
      intrinsic: intrinsic_count,
      percentage: percentage_count,
    });
    for track in tracks {
      check_layout_abort();
      if track.is_flexible() {
        flexible_count += 1;
      }
      if track.has_intrinsic_sizing_function() {
        intrinsic_count += 1;
      }
      if track.kind == GridTrackKind::Track && track.uses_percentage() {
        percentage_count += 1;
      }

      prefix.push(TrackPrefixCounts {
        flexible: flexible_count,
        intrinsic: intrinsic_count,
        percentage: percentage_count,
      });
    }

    prefix
  }

  #[inline(always)]
  fn range_has_match(start: usize, end: usize, start_count: u16, end_count: u16) -> bool {
    // `GridItem::track_range_excluding_lines()` can return an empty range for degenerate spans.
    // This mirrors `Range::any()` which would also return `false` for empty ranges.
    start < end && end_count != start_count
  }

  let column_prefix = build_prefix_counts(columns);
  let row_prefix = build_prefix_counts(rows);

  for item in items {
    check_layout_abort();
    let col_range = item.track_range_excluding_lines(AbstractAxis::Inline);
    let col_start = column_prefix[col_range.start];
    let col_end = column_prefix[col_range.end];
    item.crosses_flexible_column =
      range_has_match(col_range.start, col_range.end, col_start.flexible, col_end.flexible);
    let crosses_intrinsic_column_base =
      range_has_match(col_range.start, col_range.end, col_start.intrinsic, col_end.intrinsic);
    item.crosses_intrinsic_column_base = crosses_intrinsic_column_base;
    item.crosses_percentage_column =
      range_has_match(col_range.start, col_range.end, col_start.percentage, col_end.percentage);
    item.crosses_intrinsic_column = crosses_intrinsic_column_base;

    let row_range = item.track_range_excluding_lines(AbstractAxis::Block);
    let row_start = row_prefix[row_range.start];
    let row_end = row_prefix[row_range.end];
    item.crosses_flexible_row =
      range_has_match(row_range.start, row_range.end, row_start.flexible, row_end.flexible);
    let crosses_intrinsic_row_base =
      range_has_match(row_range.start, row_range.end, row_start.intrinsic, row_end.intrinsic);
    item.crosses_intrinsic_row_base = crosses_intrinsic_row_base;
    item.crosses_percentage_row =
      range_has_match(row_range.start, row_range.end, row_start.percentage, row_end.percentage);
    item.crosses_intrinsic_row = crosses_intrinsic_row_base;
  }
}

#[cfg(test)]
thread_local! {
  static UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

/// Update the cached `GridItem::{crosses_intrinsic_column,crosses_intrinsic_row}` flags for the
/// specified axis.
///
/// CSS Grid treats percentage track sizing functions as `auto` (intrinsic) when the grid container
/// size in that axis is indefinite. Once the container size becomes definite, the same percentage
/// tracks behave as fixed sizing functions.
///
/// Taffy caches whether each item crosses an intrinsic track because spanning-item processing is
/// gated on that check. We recompute the flags per sizing pass so percentage tracks participate in
/// intrinsic sizing in the first pass but do not in reruns once the container size resolves.
#[inline(always)]
fn update_item_crosses_intrinsic_tracks_for_axis(
  axis: AbstractAxis,
  items: &mut [GridItem],
  axis_inner_node_size: Option<f32>,
) {
  #[cfg(test)]
  UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS.with(|c| c.set(c.get() + 1));

  for item in items.iter_mut() {
    let crosses_intrinsic_base = match axis {
      AbstractAxis::Inline => item.crosses_intrinsic_column_base,
      AbstractAxis::Block => item.crosses_intrinsic_row_base,
    };
    let crosses_percentage = match axis {
      AbstractAxis::Inline => item.crosses_percentage_column,
      AbstractAxis::Block => item.crosses_percentage_row,
    };
    let crosses_intrinsic =
      crosses_intrinsic_base || (axis_inner_node_size.is_none() && crosses_percentage);

    match axis {
      AbstractAxis::Inline => item.crosses_intrinsic_column = crosses_intrinsic,
      AbstractAxis::Block => item.crosses_intrinsic_row = crosses_intrinsic,
    }
  }
}

/// Track sizing algorithm
/// Note: Gutters are treated as empty fixed-size tracks for the purpose of the track sizing algorithm.
#[allow(clippy::too_many_arguments)]
pub(super) fn track_sizing_algorithm<Tree: LayoutPartialTree>(
  tree: &mut Tree,
  axis: AbstractAxis,
  axis_min_size: Option<f32>,
  axis_max_size: Option<f32>,
  axis_alignment: AlignContent,
  other_axis_alignment: AlignContent,
  available_grid_space: Size<AvailableSpace>,
  inner_node_size: Size<Option<f32>>,
  axis_tracks: &mut [GridTrack],
  other_axis_tracks: &mut [GridTrack],
  items: &mut [GridItem],
  get_track_size_estimate: fn(&GridTrack, Option<f32>, &Tree) -> Option<f32>,
  has_baseline_aligned_item: bool,
) {
  // 11.4 Initialise Track sizes
  // Initialize each track’s base size and growth limit.
  let axis_inner_node_size = inner_node_size.get(axis);
  let inline_inner_node_size = inner_node_size.get(AbstractAxis::Inline);
  initialize_track_sizes(
    tree,
    axis,
    axis_tracks,
    axis_inner_node_size,
    inline_inner_node_size,
  );

  // Percentage track sizing functions are treated as `auto` when the grid container's size in that
  // axis is indefinite. This affects which items participate in intrinsic track sizing (spanning
  // items are only processed when they cross an intrinsic track).
  //
  // Recompute `crosses_intrinsic_*` for this sizing pass so percentage tracks behave as intrinsic
  // in the first pass and as fixed tracks once the container size becomes definite (reruns).
  let has_percentage_track = axis_tracks
    .iter()
    .any(|t| t.kind == GridTrackKind::Track && t.uses_percentage());
  if has_percentage_track {
    update_item_crosses_intrinsic_tracks_for_axis(axis, items, axis_inner_node_size);
  }

  // 11.5.1 Shim item baselines
  if has_baseline_aligned_item {
    resolve_item_baselines(tree, axis, items, inner_node_size);
  }

  // If all tracks have base_size = growth_limit, then skip the rest of this function.
  // Note: this can only happen both track sizing function have the same fixed track sizing function
  if axis_tracks
    .iter()
    .all(|track| track.base_size == track.growth_limit)
  {
    return;
  }

  // Pre-computations for 11.5 Resolve Intrinsic Track Sizes

  // Compute an additional amount to add to each spanned gutter when computing item's estimated size in the
  // in the opposite axis based on the alignment, container size, and estimated track sizes in that axis
  let gutter_alignment_adjustment = compute_alignment_gutter_adjustment(
    other_axis_alignment,
    inner_node_size.get(axis.other()),
    |track, basis| get_track_size_estimate(track, basis, tree),
    other_axis_tracks,
  );
  if other_axis_tracks.len() > 3 {
    let len = other_axis_tracks.len();
    let inner_gutter_tracks = other_axis_tracks[2..len].iter_mut().step_by(2);
    for track in inner_gutter_tracks {
      track.content_alignment_adjustment = gutter_alignment_adjustment;
    }
  }

  // 11.5 Resolve Intrinsic Track Sizes
  resolve_intrinsic_track_sizes(
    tree,
    axis,
    axis_tracks,
    other_axis_tracks,
    items,
    available_grid_space.get(axis),
    inner_node_size,
    get_track_size_estimate,
  );

  // 11.6. Maximise Tracks
  // Distributes free space (if any) to tracks with FINITE growth limits, up to their limits.
  maximise_tracks(
    axis_tracks,
    inner_node_size.get(axis),
    available_grid_space.get(axis),
  );

  // For the purpose of the final two expansion steps ("Expand Flexible Tracks" and "Stretch auto Tracks"), we only want to expand
  // into space generated by the grid container's size (as defined by either it's preferred size style or by it's parent node through
  // something like stretch alignment), not just any available space. To do this we map definite available space to AvailableSpace::MaxContent
  // in the case that inner_node_size is None
  let axis_available_space_for_expansion = if let Some(available_space) = inner_node_size.get(axis)
  {
    AvailableSpace::Definite(available_space)
  } else {
    match available_grid_space.get(axis) {
      AvailableSpace::MinContent => AvailableSpace::MinContent,
      AvailableSpace::MaxContent | AvailableSpace::Definite(_) => AvailableSpace::MaxContent,
    }
  };

  // 11.7. Expand Flexible Tracks
  // This step sizes flexible tracks using the largest value it can assign to an fr without exceeding the available space.
  expand_flexible_tracks(
    tree,
    axis,
    axis_tracks,
    other_axis_tracks,
    items,
    axis_min_size,
    axis_max_size,
    axis_available_space_for_expansion,
    inner_node_size,
    get_track_size_estimate,
  );

  // 11.8. Stretch auto Tracks
  // This step expands tracks that have an auto max track sizing function by dividing any remaining positive, definite free space equally amongst them.
  if axis_alignment == AlignContent::Stretch {
    stretch_auto_tracks(
      axis_tracks,
      axis_min_size,
      axis_available_space_for_expansion,
    );
  }
}

/// Whether it is a minimum or maximum size's space being distributed
/// This controls behaviour of the space distribution algorithm when distributing beyond limits
/// See "distributing space beyond limits" at https://www.w3.org/TR/css-grid-1/#extra-space
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum IntrinsicContributionType {
  /// It's a minimum size's space being distributed
  Minimum,
  /// It's a maximum size's space being distributed
  Maximum,
}

/// Add any planned base size increases to the base size after a round of distributing space to base sizes
/// Reset the planed base size increase to zero ready for the next round.
#[inline(always)]
fn flush_planned_base_size_increases(tracks: &mut [GridTrack]) {
  for track in tracks {
    track.base_size += track.base_size_planned_increase;
    track.base_size_planned_increase = 0.0;
  }
}

/// Add any planned growth limit increases to the growth limit after a round of distributing space to growth limits
/// Reset the planed growth limit increase to zero ready for the next round.
#[inline(always)]
fn flush_planned_growth_limit_increases(tracks: &mut [GridTrack], set_infinitely_growable: bool) {
  for track in tracks {
    if track.growth_limit_planned_increase > 0.0 {
      track.growth_limit = if track.growth_limit == f32::INFINITY {
        track.base_size + track.growth_limit_planned_increase
      } else {
        track.growth_limit + track.growth_limit_planned_increase
      };
      track.infinitely_growable = set_infinitely_growable;
    } else {
      track.infinitely_growable = false;
    }
    track.growth_limit_planned_increase = 0.0
  }
}

/// 11.4 Initialise Track sizes
/// Initialize each track’s base size and growth limit.
#[inline(always)]
fn initialize_track_sizes(
  tree: &impl LayoutPartialTree,
  _axis: AbstractAxis,
  axis_tracks: &mut [GridTrack],
  axis_inner_node_size: Option<f32>,
  inline_inner_node_size: Option<f32>,
) {
  // Per CSS Box Alignment, percentage gaps resolve against the inline size of the container.
  // If the inline size is indefinite, the percentage part resolves to 0 to avoid cyclic sizing.
  //
  // For gutters we treat the percentage basis as always definite (0 when unknown) so they behave
  // like fixed-size tracks rather than becoming intrinsic/auto tracks.
  let gutter_percentage_basis = Some(inline_inner_node_size.unwrap_or(0.0));

  for track in axis_tracks.iter_mut() {
    let percentage_basis = if track.kind == GridTrackKind::Gutter {
      gutter_percentage_basis
    } else {
      axis_inner_node_size
    };

    // For each track, if the track’s min track sizing function is:
    // - A fixed sizing function
    //     Resolve to an absolute length and use that size as the track’s initial base size.
    //     Note: Indefinite lengths cannot occur, as they’re treated as auto.
    // - An intrinsic sizing function
    //     Use an initial base size of zero.
    track.base_size = track
      .min_track_sizing_function
      .definite_value(percentage_basis, |val, basis| tree.calc(val, basis))
      .unwrap_or(0.0);

    // For each track, if the track’s max track sizing function is:
    // - A fixed sizing function
    //     Resolve to an absolute length and use that size as the track’s initial growth limit.
    // - An intrinsic sizing function
    //     Use an initial growth limit of infinity.
    // - A flexible sizing function
    //     Use an initial growth limit of infinity.
    track.growth_limit = if track.kind == GridTrackKind::Gutter {
      track.base_size
    } else {
      track
        .max_track_sizing_function
        .definite_value(percentage_basis, |val, basis| tree.calc(val, basis))
        .unwrap_or(f32::INFINITY)
    };

    // In all cases, if the growth limit is less than the base size, increase the growth limit to match the base size.
    if track.growth_limit < track.base_size {
      track.growth_limit = track.base_size;
    }
  }
}

/// 11.5.1 Shim baseline-aligned items so their intrinsic size contributions reflect their baseline alignment.
pub(super) fn resolve_item_baselines(
  tree: &mut impl LayoutPartialTree,
  axis: AbstractAxis,
  items: &mut [GridItem],
  inner_node_size: Size<Option<f32>>,
) {
  if items.is_empty() {
    return;
  }

  let other_axis = axis.other();

  // Baseline alignment operates independently per other-axis track (row/column). We only need to
  // consider items that actually participate in baseline alignment, so we avoid sorting the entire
  // `items` slice by other-axis start and instead group participating items via maps.
  let is_baseline_aligned = |item: &GridItem| match axis {
    AbstractAxis::Inline => item.align_self == AlignSelf::Baseline,
    AbstractAxis::Block => item.justify_self == AlignSelf::Baseline,
  };

  // Determine:
  // - The first other-axis track containing items (needed for grid container baseline computation).
  // - How many items participate in baseline alignment per other-axis track.
  let mut first_group_key: Option<u16> = None;
  #[derive(Clone, Copy)]
  struct BaselineGroupStats {
    count: u32,
    max_baseline: f32,
  }

  // Track baseline group stats in a dense vec indexed by other-axis start. This avoids
  // per-group heap allocations (BTreeMap) / hash table rehashing (FxHashMap) in baseline-heavy
  // grids.
  let mut group_stats: Vec<BaselineGroupStats> = Vec::new();
  let mut has_shim_group = false;
  for item in items.iter_mut() {
    check_layout_abort();
    // Clear any stale shims for the axis we are computing. The track sizing algorithm can rerun, so
    // we must ensure we don't carry baseline shims forward when baseline participation changes.
    match axis {
      // Column sizing pass shims `align-self: baseline` items (physical vertical baseline alignment).
      AbstractAxis::Inline => item.baseline_shim.y = 0.0,
      // Row sizing pass shims `justify-self: baseline` items (physical horizontal baseline alignment).
      AbstractAxis::Block => item.baseline_shim.x = 0.0,
    }
    let key = match other_axis {
      AbstractAxis::Inline => item.column_indexes.start,
      AbstractAxis::Block => item.row_indexes.start,
    };
    first_group_key = Some(match first_group_key {
      Some(min_key) => min_key.min(key),
      None => key,
    });
    if is_baseline_aligned(item) {
      let key = key as usize;
      if key >= group_stats.len() {
        group_stats.resize(
          key + 1,
          BaselineGroupStats {
            count: 0,
            max_baseline: f32::NEG_INFINITY,
          },
        );
      }
      let entry = &mut group_stats[key];
      entry.count += 1;
      if entry.count == 2 {
        // Only groups with > 1 baseline-aligned item require actual shimming work.
        has_shim_group = true;
      }
    }
  }

  let Some(first_group_key) = first_group_key else {
    return;
  };
  if group_stats.is_empty() {
    return;
  }

  let mut measure_item_baseline_value = |item: &mut GridItem| -> f32 {
    let measured_size_and_baselines = tree.perform_child_layout(
      item.node,
      Size::NONE,
      inner_node_size,
      Size::MIN_CONTENT,
      SizingMode::InherentSize,
      Line::FALSE,
    );

    let (baseline, fallback_size, margin_start) = match axis {
      AbstractAxis::Inline => (
        measured_size_and_baselines.first_baselines.y,
        measured_size_and_baselines.size.height,
        item.baseline_shim_margin_start_cached(axis, inner_node_size.width, tree),
      ),
      AbstractAxis::Block => (
        measured_size_and_baselines.first_baselines.x,
        measured_size_and_baselines.size.width,
        item.baseline_shim_margin_start_cached(axis, inner_node_size.width, tree),
      ),
    };

    let extra_margin_start = match axis {
      AbstractAxis::Inline => item.extra_margin.top,
      AbstractAxis::Block => item.extra_margin.left,
    };

    let baseline = baseline.filter(|b| b.is_finite());
    let fallback_size = if fallback_size.is_finite() {
      fallback_size
    } else {
      0.0
    };
    let margin_start = if margin_start.is_finite() { margin_start } else { 0.0 };
    let extra_margin_start = if extra_margin_start.is_finite() {
      extra_margin_start
    } else {
      0.0
    };

    let value = baseline.unwrap_or(fallback_size) + margin_start + extra_margin_start;
    let value = if value.is_finite() { value } else { 0.0 };

    if axis == AbstractAxis::Inline {
      // Record the vertical baseline for computing the grid container baseline.
      item.baseline = Some(value);
    }

    value
  };

  // If there are no groups with multiple baseline-aligned items, there is no shimming work to do.
  // In the row sizing pass this means baseline processing is a complete no-op. In the column
  // sizing pass we may still need to measure the baseline-aligned item in the first row in order
  // to compute the grid container baseline.
  if !has_shim_group {
    if axis == AbstractAxis::Inline {
      let first_group_baseline_count = group_stats
        .get(first_group_key as usize)
        .map(|s| s.count)
        .unwrap_or(0);
      if first_group_baseline_count > 0 {
        for item in items.iter_mut() {
          check_layout_abort();
          if !is_baseline_aligned(item) {
            continue;
          }
          let key = match other_axis {
            AbstractAxis::Inline => item.column_indexes.start,
            AbstractAxis::Block => item.row_indexes.start,
          };
          if key == first_group_key {
            let _ = measure_item_baseline_value(item);
            break;
          }
        }
      }
    }
    return;
  }

  for item in items.iter_mut() {
    check_layout_abort();
    if !is_baseline_aligned(item) {
      continue;
    }

    let key = match other_axis {
      AbstractAxis::Inline => item.column_indexes.start,
      AbstractAxis::Block => item.row_indexes.start,
    };
    let baseline_item_count = group_stats.get(key as usize).map(|s| s.count).unwrap_or(0);

    // Baseline alignment is a no-op if <= 1 items in an other-axis group participate. In that
    // case we can skip the expensive baseline-measurement pass entirely, with one exception:
    // - In the column sizing pass (`axis == Inline`), the grid container baseline is derived from
    //   the first other-axis group's baseline-aligned item (if any), so we still need to measure
    //   that baseline value.
    if baseline_item_count <= 1 {
      match axis {
        // `justify-self: baseline` does not contribute to the grid container baseline, so we can
        // skip measuring entirely when there is no alignment work to do.
        AbstractAxis::Block => continue,
        // `align-self: baseline` is only needed for the grid container baseline if this is the
        // first other-axis group containing items.
        AbstractAxis::Inline => {
          if key != first_group_key {
            continue;
          }
          // Measure only the single baseline item in this group to compute the grid container
          // baseline later. No shim is needed.
          let _ = measure_item_baseline_value(item);
          continue;
        }
      }
    }

    let value = measure_item_baseline_value(item);
    match axis {
      AbstractAxis::Inline => item.baseline_shim.y = value,
      AbstractAxis::Block => item.baseline_shim.x = value,
    }
    if let Some(entry) = group_stats.get_mut(key as usize) {
      entry.max_baseline = f32_max(entry.max_baseline, value);
    }
  }

  for item in items.iter_mut() {
    check_layout_abort();
    if !is_baseline_aligned(item) {
      continue;
    }

    let key = match other_axis {
      AbstractAxis::Inline => item.column_indexes.start,
      AbstractAxis::Block => item.row_indexes.start,
    };
    let baseline_item_count = group_stats.get(key as usize).map(|s| s.count).unwrap_or(0);
    if baseline_item_count <= 1 {
      continue;
    }
    let Some(group_max) = group_stats.get(key as usize).map(|s| s.max_baseline) else {
      continue;
    };
    let value = match axis {
      AbstractAxis::Inline => item.baseline_shim.y,
      AbstractAxis::Block => item.baseline_shim.x,
    };
    let shim = group_max - value;
    let shim = if shim.is_finite() { shim } else { 0.0 };
    match axis {
      AbstractAxis::Inline => item.baseline_shim.y = shim,
      AbstractAxis::Block => item.baseline_shim.x = shim,
    }
  }
}
/// 11.5 Resolve Intrinsic Track Sizes
#[allow(clippy::too_many_arguments)]
fn resolve_intrinsic_track_sizes<Tree: LayoutPartialTree>(
  tree: &mut Tree,
  axis: AbstractAxis,
  axis_tracks: &mut [GridTrack],
  other_axis_tracks: &[GridTrack],
  items: &mut [GridItem],
  axis_available_grid_space: AvailableSpace,
  inner_node_size: Size<Option<f32>>,
  get_track_size_estimate: impl Fn(&GridTrack, Option<f32>, &Tree) -> Option<f32>,
) {
  // Step 1. Shim baseline-aligned items so their intrinsic size contributions reflect their baseline alignment.

  // Already done at this point. See resolve_item_baselines function.

  // Fast path: if there are no intrinsic track sizing functions in this axis for this sizing pass,
  // the intrinsic track sizing algorithm cannot change any track's base size or growth limit.
  //
  // We must still perform CSS Grid step 11.5 "Step 5" to ensure that flex tracks do not get
  // expanded by the "Maximise Tracks" step (which distributes free space to tracks with infinite
  // growth limits).
  let axis_inner_node_size = inner_node_size.get(axis);
  let should_treat_percentage_tracks_as_intrinsic = axis_inner_node_size.is_none();
  let has_intrinsic_tracks_for_this_pass = axis_tracks.iter().any(|track| {
    track.min_track_sizing_function.is_intrinsic()
      || track.max_track_sizing_function.is_intrinsic()
      || (should_treat_percentage_tracks_as_intrinsic
        && track.kind == GridTrackKind::Track
        && track.uses_percentage())
  });
  if !has_intrinsic_tracks_for_this_pass {
    // Step 5. If any track still has an infinite growth limit (because, for example, it had no items placed
    // in it or it is a flexible track), set its growth limit to its base size.
    // NOTE: this step is super-important to ensure that the "Maximise Tracks" step doesn't affect flexible tracks
    axis_tracks
      .iter_mut()
      .filter(|track| track.growth_limit == f32::INFINITY)
      .for_each(|track| track.growth_limit = track.base_size);
    return;
  }

  // Step 2.

  // The track sizing algorithm requires us to iterate through the items in ascendeding order of the number of
  // tracks they span (first items that span 1 track, then items that span 2 tracks, etc).
  // To avoid having to do multiple iterations of the items, we pre-sort them into this order.
  items.sort_unstable_by(cmp_by_cross_flex_then_span_then_start(axis));

  // Step 2, Step 3 and Step 4
  // 2 & 3. Iterate over items that don't cross a flex track. Items should have already been sorted in ascending order
  // of the number of tracks they span. Step 2 is the 1 track case and has an optimised implementation
  // 4. Next, repeat the previous step instead considering (together, rather than grouped by span size) all items
  // that do span a track with a flexible sizing function while

  // Compute item's intrinsic (content-based) sizes
  // Note: For items with a specified minimum size of auto (the initial value), the minimum contribution is usually equivalent
  // to the min-content contribution—but can differ in some cases, see §6.6 Automatic Minimum Size of Grid Items.
  // Also, minimum contribution <= min-content contribution <= max-content contribution.

  let gutter_percentage_basis = Some(inner_node_size.get(AbstractAxis::Inline).unwrap_or(0.0));
  let percentage_basis = |track: &GridTrack| {
    if track.kind == GridTrackKind::Gutter {
      gutter_percentage_basis
    } else {
      axis_inner_node_size
    }
  };
  let flex_factor_sum = axis_tracks
    .iter()
    .map(|track| track.flex_factor())
    .sum::<f32>();

  // Prefix sums of per-track definite max-size limits (used for clamping limited contributions).
  // This avoids O(span) scans for every item that needs a limited contribution clamp.
  //
  // These limits are only consulted when sizing under a min/max-content constraint, so avoid the
  // extra work when sizing under a definite constraint.
  let needs_max_limit_prefix =
    matches!(axis_available_grid_space, AvailableSpace::MinContent | AvailableSpace::MaxContent);
  let max_limit_prefix = if needs_max_limit_prefix {
    let mut prefix: Vec<PrefixSumMaybe> = Vec::with_capacity(axis_tracks.len() + 1);
    let mut running_sum = 0.0;
    let mut running_none = 0u32;
    prefix.push(PrefixSumMaybe {
      sum: running_sum,
      none: running_none,
    });
    for track in axis_tracks.iter() {
      match track
        .max_track_sizing_function
        .definite_limit(axis_inner_node_size, |val, basis| tree.calc(val, basis))
      {
        Some(v) => running_sum += v,
        None => running_none += 1,
      }
      prefix.push(PrefixSumMaybe {
        sum: running_sum,
        none: running_none,
      });
    }
    prefix
  } else {
    Vec::new()
  };

  // Pre-compute the other-axis track-size estimates used for intrinsic measurement.
  //
  // `GridItem::available_space()` historically summed these estimates per item by iterating the
  // spanned other-axis tracks (O(items * span)). We compute prefix sums so each item's sum can be
  // computed in O(1) while preserving `Option` sum semantics (None if any estimate in the range is
  // None).
  let other_axis_available_space = inner_node_size.get(axis.other());
  let mut running_sum = 0.0;
  let mut running_none = 0u32;
  let mut prefix: Vec<PrefixSumMaybe> = Vec::with_capacity(other_axis_tracks.len() + 1);
  prefix.push(PrefixSumMaybe {
    sum: running_sum,
    none: running_none,
  });
  for track in other_axis_tracks.iter() {
    match get_track_size_estimate(track, other_axis_available_space, tree) {
      Some(v) => {
        running_sum += v + track.content_alignment_adjustment;
      }
      None => {
        running_none += 1;
      }
    }
    prefix.push(PrefixSumMaybe {
      sum: running_sum,
      none: running_none,
    });
  }

  let other_axis = axis.other();
  for item in items.iter_mut() {
    let mut available_space = Size::NONE;
    let other_axis_size = sum_range(&prefix, item.track_range_excluding_lines(other_axis));
    available_space.set(other_axis, other_axis_size);
    item.available_space_cache = Some(available_space);
  }

  let mut item_sizer = IntrisicSizeMeasurer {
    tree,
    other_axis_tracks,
    axis,
    inner_node_size,
  };

  // Many intrinsic sizing sub-steps only affect specific kinds of tracks. Computing min/max-content
  // contributions is expensive (it can fan out into user measure callbacks) so we precompute whether
  // any relevant tracks exist at all, allowing us to skip whole steps when they would be a no-op.
  //
  // Note: for the flex-item batches, distribution further filters to `track.is_flexible()`, so we
  // also precompute flex-aware variants.
  let has_intrinsic_min_track_sizing_function =
    |track: &GridTrack| match track.min_track_sizing_function.0.tag() {
      CompactLength::AUTO_TAG | CompactLength::MIN_CONTENT_TAG | CompactLength::MAX_CONTENT_TAG => {
        true
      }
      CompactLength::PERCENT_TAG => percentage_basis(track).is_none(),
      #[cfg(feature = "calc")]
      _ if track.min_track_sizing_function.0.is_calc() => percentage_basis(track).is_none(),
      _ => false,
    };

  // The intrinsic sizing steps gate expensive measurements by checking whether an item spans at
  // least one affected track. Doing this via `.iter().any(..)` would be O(items * span). Instead,
  // build prefix sums of relevant track predicates so each range check is O(1).
  let track_count = axis_tracks.len();
  // These are consulted extremely frequently in the intrinsic sizing loops, so keep them as simple
  // `u32` prefix counts. Using a single backing buffer avoids 10 separate heap allocations per pass.
  const PREFIX_VEC_COUNT: usize = 10;
  let prefix_len = track_count + 1;
  let mut prefix_buf: Vec<u32> = Vec::with_capacity(prefix_len * PREFIX_VEC_COUNT);
  prefix_buf.resize(prefix_len * PREFIX_VEC_COUNT, 0);

  let remaining = prefix_buf.as_mut_slice();
  let (prefix_min_or_max_content_min, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_min_or_max_content_min_flex, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_max_content_min, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_max_content_min_flex, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_auto_min, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_auto_min_flex, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_intrinsic_min, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_intrinsic_min_flex, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_intrinsic_max, remaining) = remaining.split_at_mut(prefix_len);
  let (prefix_max_content_max, remaining) = remaining.split_at_mut(prefix_len);
  debug_assert!(remaining.is_empty());

  let mut running_min_or_max_content_min = 0u32;
  let mut running_min_or_max_content_min_flex = 0u32;
  let mut running_max_content_min = 0u32;
  let mut running_max_content_min_flex = 0u32;
  let mut running_auto_min = 0u32;
  let mut running_auto_min_flex = 0u32;
  let mut running_intrinsic_min = 0u32;
  let mut running_intrinsic_min_flex = 0u32;
  let mut running_intrinsic_max = 0u32;
  let mut running_max_content_max = 0u32;

  for (track_index, track) in axis_tracks.iter().enumerate() {
    let prefix_index = track_index + 1;
    let is_flexible = track.is_flexible();

    let is_min_or_max_content_min = track.min_track_sizing_function.is_min_or_max_content();
    running_min_or_max_content_min += u32::from(is_min_or_max_content_min);
    running_min_or_max_content_min_flex += u32::from(is_min_or_max_content_min && is_flexible);
    prefix_min_or_max_content_min[prefix_index] = running_min_or_max_content_min;
    prefix_min_or_max_content_min_flex[prefix_index] = running_min_or_max_content_min_flex;

    let is_max_content_min = track.min_track_sizing_function.is_max_content();
    running_max_content_min += u32::from(is_max_content_min);
    running_max_content_min_flex += u32::from(is_max_content_min && is_flexible);
    prefix_max_content_min[prefix_index] = running_max_content_min;
    prefix_max_content_min_flex[prefix_index] = running_max_content_min_flex;

    let is_auto_min = track.min_track_sizing_function.is_auto()
      && !track.max_track_sizing_function.is_min_content();
    running_auto_min += u32::from(is_auto_min);
    running_auto_min_flex += u32::from(is_auto_min && is_flexible);
    prefix_auto_min[prefix_index] = running_auto_min;
    prefix_auto_min_flex[prefix_index] = running_auto_min_flex;

    let is_intrinsic_min = has_intrinsic_min_track_sizing_function(track);
    running_intrinsic_min += u32::from(is_intrinsic_min);
    running_intrinsic_min_flex += u32::from(is_intrinsic_min && is_flexible);
    prefix_intrinsic_min[prefix_index] = running_intrinsic_min;
    prefix_intrinsic_min_flex[prefix_index] = running_intrinsic_min_flex;

    let is_intrinsic_max =
      !track.max_track_sizing_function.has_definite_value(percentage_basis(track));
    running_intrinsic_max += u32::from(is_intrinsic_max);
    prefix_intrinsic_max[prefix_index] = running_intrinsic_max;

    let is_max_content_max = track.max_track_sizing_function.is_max_content_alike()
      || (track.kind == GridTrackKind::Track
        && track.max_track_sizing_function.uses_percentage()
        && axis_inner_node_size.is_none());
    running_max_content_max += u32::from(is_max_content_max);
    prefix_max_content_max[prefix_index] = running_max_content_max;
  }

  let any_min_or_max_content_min_track_sizing_function = running_min_or_max_content_min > 0;
  let any_flexible_min_or_max_content_min_track_sizing_function =
    running_min_or_max_content_min_flex > 0;
  let any_max_content_min_track_sizing_function = running_max_content_min > 0;
  let any_flexible_max_content_min_track_sizing_function = running_max_content_min_flex > 0;
  let any_auto_min_track_sizing_function = running_auto_min > 0;
  let any_flexible_auto_min_track_sizing_function = running_auto_min_flex > 0;
  let any_intrinsic_min_track_sizing_function = running_intrinsic_min > 0;
  let any_flexible_intrinsic_min_track_sizing_function = running_intrinsic_min_flex > 0;
  let any_intrinsic_max_track_sizing_function = running_intrinsic_max > 0;
  let any_max_content_max_track_sizing_function = running_max_content_max > 0;

  #[inline(always)]
  fn range_has_any(prefix: &[u32], start: usize, end: usize) -> bool {
    prefix[end] - prefix[start] > 0
  }

  let mut batched_item_iterator = ItemBatcher::new(axis);
  while let Some((batch, is_flex)) = batched_item_iterator.next(items) {
    // 2. Size tracks to fit non-spanning items: For each track with an intrinsic track sizing function and not a flexible sizing function,
    // consider the items in it with a span of 1:
    let batch_span = batch[0].placement(axis).span();
    if !is_flex && batch_span == 1 {
      for item in batch.iter_mut() {
        let track_index = item.placement_indexes(axis).start + 1;
        let track = &axis_tracks[track_index as usize];

        // Handle base sizes
        let new_base_size = match track.min_track_sizing_function.0.tag() {
          CompactLength::MIN_CONTENT_TAG => {
            f32_max(track.base_size, item_sizer.min_content_contribution(item))
          }
          // If the container size is indefinite then percentage-sized tracks are treated as `auto`
          // for the purpose of intrinsic sizing (CSS Grid 1).
          CompactLength::PERCENT_TAG => {
            if axis_inner_node_size.is_none() {
              let space = match axis_available_grid_space {
                // QUIRK: The spec says that:
                //
                //   If the grid container is being sized under a min- or max-content constraint, use the items’ limited
                //   min-content contributions in place of their minimum contributions here.
                //
                // However, in practice browsers only seem to apply this rule if the item is not a scroll container
                // (note that overflow:hidden counts as a scroll container), giving the automatic minimum size of scroll
                // containers (zero) precedence over the min-content contributions.
                AvailableSpace::MinContent | AvailableSpace::MaxContent
                  if !item.overflow.get(axis).is_scroll_container() =>
                {
                  let axis_minimum_size = item_sizer.minimum_contribution(item, axis_tracks);
                  let axis_min_content_size = item_sizer.min_content_contribution(item);
                  let limit = track
                    .max_track_sizing_function
                    .definite_limit(axis_inner_node_size, |val, basis| {
                      item_sizer.calc(val, basis)
                    });
                  axis_min_content_size
                    .maybe_min(limit)
                    .max(axis_minimum_size)
                }
                _ => item_sizer.minimum_contribution(item, axis_tracks),
              };
              f32_max(track.base_size, space)
            } else {
              track.base_size
            }
          }
          CompactLength::MAX_CONTENT_TAG => {
            f32_max(track.base_size, item_sizer.max_content_contribution(item))
          }
          CompactLength::AUTO_TAG => {
            let space = match axis_available_grid_space {
              // QUIRK: The spec says that:
              //
              //   If the grid container is being sized under a min- or max-content constraint, use the items’ limited
              //   min-content contributions in place of their minimum contributions here.
              //
              // However, in practice browsers only seem to apply this rule if the item is not a scroll container
              // (note that overflow:hidden counts as a scroll container), giving the automatic minimum size of scroll
              // containers (zero) precedence over the min-content contributions.
              AvailableSpace::MinContent | AvailableSpace::MaxContent
                if !item.overflow.get(axis).is_scroll_container() =>
              {
                let axis_minimum_size = item_sizer.minimum_contribution(item, axis_tracks);
                let axis_min_content_size = item_sizer.min_content_contribution(item);
                let limit = track
                  .max_track_sizing_function
                  .definite_limit(axis_inner_node_size, |val, basis| {
                    item_sizer.calc(val, basis)
                  });
                axis_min_content_size
                  .maybe_min(limit)
                  .max(axis_minimum_size)
              }
              _ => item_sizer.minimum_contribution(item, axis_tracks),
            };
            f32_max(track.base_size, space)
          }
          CompactLength::LENGTH_TAG => {
            // Do nothing as it's not an intrinsic track sizing function
            track.base_size
          }
          // Handle calc() like percentage
          #[cfg(feature = "calc")]
          _ if track.min_track_sizing_function.0.is_calc() => {
            if axis_inner_node_size.is_none() {
              let space = match axis_available_grid_space {
                AvailableSpace::MinContent | AvailableSpace::MaxContent
                  if !item.overflow.get(axis).is_scroll_container() =>
                {
                  let axis_minimum_size = item_sizer.minimum_contribution(item, axis_tracks);
                  let axis_min_content_size = item_sizer.min_content_contribution(item);
                  let limit = track
                    .max_track_sizing_function
                    .definite_limit(axis_inner_node_size, |val, basis| {
                      item_sizer.calc(val, basis)
                    });
                  axis_min_content_size
                    .maybe_min(limit)
                    .max(axis_minimum_size)
                }
                _ => item_sizer.minimum_contribution(item, axis_tracks),
              };
              f32_max(track.base_size, space)
            } else {
              track.base_size
            }
          }
          _ => unreachable!(),
        };
        let track = &mut axis_tracks[track_index as usize];
        track.base_size = new_base_size;

        // Handle growth limits
        if track.max_track_sizing_function.is_fit_content() {
          // If item is not a scroll container, then increase the growth limit to at least the
          // size of the min-content contribution
          if !item.overflow.get(axis).is_scroll_container() {
            let min_content_contribution = item_sizer.min_content_contribution(item);
            track.growth_limit_planned_increase = f32_max(
              track.growth_limit_planned_increase,
              min_content_contribution,
            );
          }

          // Always increase the growth limit to at least the size of the *fit-content limited*
          // max-cotent contribution
          let fit_content_limit = track.fit_content_limit(axis_inner_node_size);
          let max_content_contribution =
            f32_min(item_sizer.max_content_contribution(item), fit_content_limit);
          track.growth_limit_planned_increase = f32_max(
            track.growth_limit_planned_increase,
            max_content_contribution,
          );
        } else if track.max_track_sizing_function.is_max_content_alike()
          || track.max_track_sizing_function.uses_percentage() && axis_inner_node_size.is_none()
        {
          // If the container size is indefinite and has not yet been resolved then percentage sized
          // tracks should be treated as auto (this matches Chrome's behaviour and seems sensible)
          track.growth_limit_planned_increase = f32_max(
            track.growth_limit_planned_increase,
            item_sizer.max_content_contribution(item),
          );
        } else if track.max_track_sizing_function.is_intrinsic() {
          track.growth_limit_planned_increase = f32_max(
            track.growth_limit_planned_increase,
            item_sizer.min_content_contribution(item),
          );
        }
      }

      for track in axis_tracks.iter_mut() {
        if track.growth_limit_planned_increase > 0.0 {
          track.growth_limit = if track.growth_limit == f32::INFINITY {
            track.growth_limit_planned_increase
          } else {
            f32_max(track.growth_limit, track.growth_limit_planned_increase)
          };
        }
        track.infinitely_growable = false;
        track.growth_limit_planned_increase = 0.0;
        if track.growth_limit < track.base_size {
          track.growth_limit = track.base_size;
        }
      }

      continue;
    }

    let use_flex_factor_for_distribution = is_flex && flex_factor_sum != 0.0;

    // 1. For intrinsic minimums:
    // First increase the base size of tracks with an intrinsic min track sizing function
    let any_track_affected_by_step = if is_flex {
      any_flexible_intrinsic_min_track_sizing_function
    } else {
      any_intrinsic_min_track_sizing_function
    };
    if any_track_affected_by_step {
      for item in batch
        .iter_mut()
        .filter(|item| item.crosses_intrinsic_track(axis))
      {
        // Skip expensive intrinsic contribution computations if none of the tracks spanned by this
        // item can be affected by this step.
        let item_track_range = item.track_range_excluding_lines(axis);
        let start = item_track_range.start;
        let end = item_track_range.end;
        let item_spans_affected_track = if is_flex {
          range_has_any(prefix_intrinsic_min_flex, start, end)
        } else {
          range_has_any(prefix_intrinsic_min, start, end)
        };
        if !item_spans_affected_track {
          continue;
        }

        // ...by distributing extra space as needed to accommodate these items’ minimum contributions.
        //
        // QUIRK: The spec says that:
        //
        //   If the grid container is being sized under a min- or max-content constraint, use the items’ limited min-content contributions
        //   in place of their minimum contributions here.
        //
        // However, in practice browsers only seem to apply this rule if the item is not a scroll container (note that overflow:hidden counts as
        // a scroll container), giving the automatic minimum size of scroll containers (zero) precedence over the min-content contributions.
      let space = match axis_available_grid_space {
        AvailableSpace::MinContent | AvailableSpace::MaxContent
          if !item.overflow.get(axis).is_scroll_container() =>
        {
          let axis_minimum_size = item_sizer.minimum_contribution(item, axis_tracks);
          let axis_min_content_size = item_sizer.min_content_contribution(item);
          let limit = sum_range(&max_limit_prefix, item_track_range.clone());
          axis_min_content_size
            .maybe_min(limit)
            .max(axis_minimum_size)
        }
        _ => item_sizer.minimum_contribution(item, axis_tracks),
      };
        let tracks = &mut axis_tracks[item_track_range];
        if space > 0.0 {
          if item.overflow.get(axis).is_scroll_container() {
            let fit_content_limit =
              |track: &GridTrack| track.fit_content_limited_growth_limit(percentage_basis(track));
            distribute_item_space_to_base_size(
              is_flex,
              use_flex_factor_for_distribution,
              space,
              tracks,
              &has_intrinsic_min_track_sizing_function,
              fit_content_limit,
              IntrinsicContributionType::Minimum,
            );
          } else {
            distribute_item_space_to_base_size(
              is_flex,
              use_flex_factor_for_distribution,
              space,
              tracks,
              &has_intrinsic_min_track_sizing_function,
              |track| track.growth_limit,
              IntrinsicContributionType::Minimum,
            );
          }
        }
      }
      flush_planned_base_size_increases(axis_tracks);
    }

    // 2. For content-based minimums:
    // Next continue to increase the base size of tracks with a min track sizing function of min-content or max-content
    // by distributing extra space as needed to account for these items' min-content contributions.
    let has_min_or_max_content_min_track_sizing_function =
      |track: &GridTrack| track.min_track_sizing_function.is_min_or_max_content();
    let any_track_affected_by_step = if is_flex {
      any_flexible_min_or_max_content_min_track_sizing_function
    } else {
      any_min_or_max_content_min_track_sizing_function
    };
    if any_track_affected_by_step {
      for item in batch.iter_mut() {
        let item_track_range = item.track_range_excluding_lines(axis);
        let start = item_track_range.start;
        let end = item_track_range.end;
        let item_spans_affected_track = if is_flex {
          range_has_any(prefix_min_or_max_content_min_flex, start, end)
        } else {
          range_has_any(prefix_min_or_max_content_min, start, end)
        };
        if !item_spans_affected_track {
          continue;
        }

        let space = item_sizer.min_content_contribution(item);
        let tracks = &mut axis_tracks[start..end];
        if space > 0.0 {
          if item.overflow.get(axis).is_scroll_container() {
            let fit_content_limit =
              |track: &GridTrack| track.fit_content_limited_growth_limit(percentage_basis(track));
            distribute_item_space_to_base_size(
              is_flex,
              use_flex_factor_for_distribution,
              space,
              tracks,
              has_min_or_max_content_min_track_sizing_function,
              fit_content_limit,
              IntrinsicContributionType::Minimum,
            );
          } else {
            distribute_item_space_to_base_size(
              is_flex,
              use_flex_factor_for_distribution,
              space,
              tracks,
              has_min_or_max_content_min_track_sizing_function,
              |track| track.growth_limit,
              IntrinsicContributionType::Minimum,
            );
          }
        }
      }
      flush_planned_base_size_increases(axis_tracks);
    }

    // 3. For max-content minimums:

    // If the grid container is being sized under a max-content constraint, continue to increase the base size of tracks with
    // a min track sizing function of auto or max-content by distributing extra space as needed to account for these items'
    // limited max-content contributions.

    // Define fit_content_limited_growth_limit function. This is passed to the distribute_space_up_to_limits
    // helper function, and is used to compute the limit to distribute up to for each track.
    // Wrapping the method on GridTrack is necessary in order to resolve percentage fit-content arguments.
    if axis_available_grid_space == AvailableSpace::MaxContent {
      /// Whether a track:
      ///   - has an Auto MIN track sizing function
      ///   - Does not have a MinContent MAX track sizing function
      ///
      /// The latter condition was added in order to match Chrome. But I believe it is due to the provision
      /// under minmax here https://www.w3.org/TR/css-grid-1/#track-sizes which states that:
      ///
      ///    "If the max is less than the min, then the max will be floored by the min (essentially yielding minmax(min, min))"
      #[inline(always)]
      fn has_auto_min_track_sizing_function(track: &GridTrack) -> bool {
        track.min_track_sizing_function.is_auto()
          && !track.max_track_sizing_function.is_min_content()
      }

      /// Whether a track has a MaxContent min track sizing function
      #[inline(always)]
      fn has_max_content_min_track_sizing_function(track: &GridTrack) -> bool {
        track.min_track_sizing_function.is_max_content()
      }

      let any_track_affected_by_step = if is_flex {
        any_flexible_max_content_min_track_sizing_function || any_flexible_auto_min_track_sizing_function
      } else {
        any_max_content_min_track_sizing_function || any_auto_min_track_sizing_function
      };

      if any_track_affected_by_step {
        for item in batch.iter_mut() {
          let item_track_range = item.track_range_excluding_lines(axis);
          let start = item_track_range.start;
          let end = item_track_range.end;
          let prioritize_max_content_minimums = range_has_any(prefix_max_content_min, start, end);
          let item_spans_affected_track = if prioritize_max_content_minimums {
            if is_flex {
              range_has_any(prefix_max_content_min_flex, start, end)
            } else {
              true
            }
          } else if is_flex {
            range_has_any(prefix_auto_min_flex, start, end)
          } else {
            range_has_any(prefix_auto_min, start, end)
          };

          if !item_spans_affected_track {
            continue;
          }

          let axis_max_content_size = item_sizer.max_content_contribution(item);
          let limit = sum_range(&max_limit_prefix, item_track_range.clone());
          let space = axis_max_content_size.maybe_min(limit);
          let tracks = &mut axis_tracks[start..end];
          if space > 0.0 {
            // If any of the tracks spanned by the item have a MaxContent min track sizing function then
            // distribute space only to those tracks. Otherwise distribute space to tracks with an Auto min
            // track sizing function.
            //
            // Note: this prioritisation of MaxContent over Auto is not mentioned in the spec (which suggests that
            // we ought to distribute space evenly between MaxContent and Auto tracks). But it is implemented like
            // this in both Chrome and Firefox (and it does have a certain logic to it), so we implement it too for
            // compatibility.
            //
            // See: https://www.w3.org/TR/css-grid-1/#track-size-max-content-min
            if prioritize_max_content_minimums {
              distribute_item_space_to_base_size(
                is_flex,
                use_flex_factor_for_distribution,
                space,
                tracks,
                has_max_content_min_track_sizing_function,
                |_| f32::INFINITY,
                IntrinsicContributionType::Maximum,
              );
            } else {
              let fit_content_limited_growth_limit =
                |track: &GridTrack| track.fit_content_limited_growth_limit(percentage_basis(track));
              distribute_item_space_to_base_size(
                is_flex,
                use_flex_factor_for_distribution,
                space,
                tracks,
                has_auto_min_track_sizing_function,
                fit_content_limited_growth_limit,
                IntrinsicContributionType::Maximum,
              );
            }
          }
        }
        flush_planned_base_size_increases(axis_tracks);
      }
    }

    // In all cases, continue to increase the base size of tracks with a min track sizing function of max-content by distributing
    // extra space as needed to account for these items' max-content contributions.
    let has_max_content_min_track_sizing_function =
      |track: &GridTrack| track.min_track_sizing_function.is_max_content();
    let any_track_affected_by_step = if is_flex {
      any_flexible_max_content_min_track_sizing_function
    } else {
      any_max_content_min_track_sizing_function
    };
    if any_track_affected_by_step {
      for item in batch.iter_mut() {
        let item_track_range = item.track_range_excluding_lines(axis);
        let start = item_track_range.start;
        let end = item_track_range.end;
        let item_spans_affected_track = if is_flex {
          range_has_any(prefix_max_content_min_flex, start, end)
        } else {
          range_has_any(prefix_max_content_min, start, end)
        };
        if !item_spans_affected_track {
          continue;
        }

        let axis_max_content_size = item_sizer.max_content_contribution(item);
        let space = axis_max_content_size;
        let tracks = &mut axis_tracks[start..end];
        if space > 0.0 {
          distribute_item_space_to_base_size(
            is_flex,
            use_flex_factor_for_distribution,
            space,
            tracks,
            has_max_content_min_track_sizing_function,
            |track| track.growth_limit,
            IntrinsicContributionType::Maximum,
          );
        }
      }
      flush_planned_base_size_increases(axis_tracks);
    }

    // 4. If at this point any track’s growth limit is now less than its base size, increase its growth limit to match its base size.
    for track in axis_tracks.iter_mut() {
      if track.growth_limit < track.base_size {
        track.growth_limit = track.base_size;
      }
    }

    // If a track is a flexible track, then it has flexible max track sizing function
    // It cannot also have an intrinsic max track sizing function, so these steps do not apply.
    if !is_flex {
      // 5. For intrinsic maximums: Next increase the growth limit of tracks with an intrinsic max track sizing function by
      // distributing extra space as needed to account for these items' min-content contributions.
      let has_intrinsic_max_track_sizing_function = |track: &GridTrack| {
        !track
          .max_track_sizing_function
          .has_definite_value(percentage_basis(track))
      };
      if any_intrinsic_max_track_sizing_function {
        for item in batch.iter_mut() {
          let item_track_range = item.track_range_excluding_lines(axis);
          let start = item_track_range.start;
          let end = item_track_range.end;
          if !range_has_any(prefix_intrinsic_max, start, end) {
            continue;
          }

          let axis_min_content_size = item_sizer.min_content_contribution(item);
          let space = axis_min_content_size;
          let tracks = &mut axis_tracks[start..end];
          if space > 0.0 {
            distribute_item_space_to_growth_limit(
              space,
              tracks,
              has_intrinsic_max_track_sizing_function,
              inner_node_size.get(axis),
            );
          }
        }
        // Mark any tracks whose growth limit changed from infinite to finite in this step as infinitely growable for the next step.
        flush_planned_growth_limit_increases(axis_tracks, true);
      }

      // 6. For max-content maximums: Lastly continue to increase the growth limit of tracks with a max track sizing function of max-content
      // by distributing extra space as needed to account for these items' max-content contributions. However, limit the growth of any
      // fit-content() tracks by their fit-content() argument.
      let has_max_content_max_track_sizing_function = |track: &GridTrack| {
        track.max_track_sizing_function.is_max_content_alike()
          || (track.kind == GridTrackKind::Track
            && track.max_track_sizing_function.uses_percentage()
            && axis_inner_node_size.is_none())
      };
      if any_max_content_max_track_sizing_function {
        for item in batch.iter_mut() {
          let item_track_range = item.track_range_excluding_lines(axis);
          let start = item_track_range.start;
          let end = item_track_range.end;
          if !range_has_any(prefix_max_content_max, start, end) {
            continue;
          }

          let axis_max_content_size = item_sizer.max_content_contribution(item);
          let space = axis_max_content_size;
          let tracks = &mut axis_tracks[start..end];
          if space > 0.0 {
            distribute_item_space_to_growth_limit(
              space,
              tracks,
              has_max_content_max_track_sizing_function,
              inner_node_size.get(axis),
            );
          }
        }
        // Mark any tracks whose growth limit changed from infinite to finite in this step as infinitely growable for the next step.
        flush_planned_growth_limit_increases(axis_tracks, false);
      }
    }
  }

  // Step 5. If any track still has an infinite growth limit (because, for example, it had no items placed
  // in it or it is a flexible track), set its growth limit to its base size.
  // NOTE: this step is super-important to ensure that the "Maximise Tracks" step doesn't affect flexible tracks
  axis_tracks
    .iter_mut()
    .filter(|track| track.growth_limit == f32::INFINITY)
    .for_each(|track| track.growth_limit = track.base_size);
}

/// 11.5.1. Distributing Extra Space Across Spanned Tracks
/// https://www.w3.org/TR/css-grid-1/#extra-space
#[inline(always)]
fn distribute_item_space_to_base_size(
  is_flex: bool,
  use_flex_factor_for_distribution: bool,
  space: f32,
  tracks: &mut [GridTrack],
  track_is_affected: impl Fn(&GridTrack) -> bool,
  track_limit: impl Fn(&GridTrack) -> f32,
  intrinsic_contribution_type: IntrinsicContributionType,
) {
  if is_flex {
    let filter = |track: &GridTrack| track.is_flexible() && track_is_affected(track);
    if use_flex_factor_for_distribution {
      distribute_item_space_to_base_size_inner(
        space,
        tracks,
        filter,
        |track| track.flex_factor(),
        track_limit,
        intrinsic_contribution_type,
      )
    } else {
      distribute_item_space_to_base_size_inner(
        space,
        tracks,
        filter,
        |_| 1.0,
        track_limit,
        intrinsic_contribution_type,
      )
    }
  } else {
    distribute_item_space_to_base_size_inner(
      space,
      tracks,
      track_is_affected,
      |_| 1.0,
      track_limit,
      intrinsic_contribution_type,
    )
  }

  /// Inner function that doesn't account for differences due to distributing to flex items
  /// This difference is handled by the closure passed in above
  fn distribute_item_space_to_base_size_inner(
    space: f32,
    tracks: &mut [GridTrack],
    track_is_affected: impl Fn(&GridTrack) -> bool,
    track_distribution_proportion: impl Fn(&GridTrack) -> f32,
    track_limit: impl Fn(&GridTrack) -> f32,
    intrinsic_contribution_type: IntrinsicContributionType,
  ) {
    // Skip this distribution if there is no space to distribute.
    //
    // Note: the intrinsic track sizing pipeline pre-checks whether an item spans any affected
    // tracks before calling into distribution. We keep a debug assertion to ensure this invariant
    // remains true without paying for an extra scan in release builds.
    if space == 0.0 {
      return;
    }
    debug_assert!(tracks.iter().any(&track_is_affected));

    // Define get_base_size function. This is passed to the distribute_space_up_to_limits helper function
    // to indicate that it is the base size that is being distributed to.
    let get_base_size = |track: &GridTrack| track.base_size;

    // 1. Find the space to distribute
    let track_sizes: f32 = tracks.iter().map(|track| track.base_size).sum();
    let extra_space: f32 = f32_max(0.0, space - track_sizes);

    // 2. Distribute space up to limits:
    // Note: there are two exit conditions to this loop:
    //   - We run out of space to distribute (extra_space falls below THRESHOLD)
    //   - We run out of growable tracks to distribute to

    /// Define a small constant to avoid infinite loops due to rounding errors. Rather than stopping distributing
    /// extra space when it gets to exactly zero, we will stop when it falls below this amount
    const THRESHOLD: f32 = 0.000001;

    let extra_space = distribute_space_up_to_limits(
      extra_space,
      tracks,
      &track_is_affected,
      &track_distribution_proportion,
      get_base_size,
      &track_limit,
    );

    // 3. Distribute remaining span beyond limits (if any)
    if extra_space > THRESHOLD {
      // When accommodating minimum contributions or accommodating min-content contributions:
      //   - any affected track that happens to also have an intrinsic max track sizing function;
      // When accommodating max-content contributions:
      //   - any affected track that happens to also have a max-content max track sizing function
      let mut filter = match intrinsic_contribution_type {
        IntrinsicContributionType::Minimum => {
          (|track: &GridTrack| track.max_track_sizing_function.is_intrinsic())
            as fn(&GridTrack) -> bool
        }
        IntrinsicContributionType::Maximum => {
          (|track: &GridTrack| {
            track.min_track_sizing_function.is_max_content()
              || track.max_track_sizing_function.is_max_or_fit_content()
          }) as fn(&GridTrack) -> bool
        }
      };

      // If there are no such tracks (matching filter above), then use all affected tracks.
      let number_of_tracks = tracks
        .iter()
        .filter(|track| track_is_affected(track))
        .filter(|track| filter(track))
        .count();
      if number_of_tracks == 0 {
        filter = (|_| true) as fn(&GridTrack) -> bool;
      }

      distribute_space_up_to_limits(
        extra_space,
        tracks,
        filter,
        &track_distribution_proportion,
        get_base_size,
        &track_limit, // Should apply only fit-content limit here?
      );
    }

    // 4. For each affected track, if the track’s item-incurred increase is larger than the track’s planned increase
    // set the track’s planned increase to that value.
    for track in tracks.iter_mut() {
      if track.item_incurred_increase > track.base_size_planned_increase {
        track.base_size_planned_increase = track.item_incurred_increase;
      }

      // Reset the item_incurresed increase ready for the next space distribution
      track.item_incurred_increase = 0.0;
    }
  }
}

/// 11.5.1. Distributing Extra Space Across Spanned Tracks
/// This is simplified (and faster) version of the algorithm for growth limits
/// https://www.w3.org/TR/css-grid-1/#extra-space
fn distribute_item_space_to_growth_limit(
  space: f32,
  tracks: &mut [GridTrack],
  track_is_affected: impl Fn(&GridTrack) -> bool,
  axis_inner_node_size: Option<f32>,
) {
  // Skip this distribution if there is no space to distribute.
  //
  // As with base-size distribution, the intrinsic track sizing pipeline pre-checks whether an
  // item spans any affected tracks before calling into distribution. Keep a debug assertion to
  // catch invariant violations without paying for an extra scan in release builds.
  if space == 0.0 {
    return;
  }
  debug_assert!(tracks.iter().any(|track| track_is_affected(track)));

  // 1. Find the space to distribute
  let track_sizes: f32 = tracks
    .iter()
    .map(|track| {
      if track.growth_limit == f32::INFINITY {
        track.base_size
      } else {
        track.growth_limit
      }
    })
    .sum();
  let extra_space: f32 = f32_max(0.0, space - track_sizes);

  // 2. Distribute space up to limits:
  // For growth limits the limit is either Infinity, or the growth limit itself. Which means that:
  //   - If there are any tracks with infinite limits then all space will be distributed to those track(s).
  //   - Otherwise no space will be distributed as part of this step
  let number_of_growable_tracks = tracks
    .iter()
    .filter(|track| track_is_affected(track))
    .filter(|track| {
      track.infinitely_growable
        || track.fit_content_limited_growth_limit(axis_inner_node_size) == f32::INFINITY
    })
    .count();
  if number_of_growable_tracks > 0 {
    let item_incurred_increase = extra_space / number_of_growable_tracks as f32;
    for track in tracks
      .iter_mut()
      .filter(|track| track_is_affected(track))
      .filter(|track| {
        track.infinitely_growable
          || track.fit_content_limited_growth_limit(axis_inner_node_size) == f32::INFINITY
      })
    {
      track.item_incurred_increase = item_incurred_increase;
    }
  } else {
    // 3. Distribute space beyond limits
    // If space remains after all tracks are frozen, unfreeze and continue to distribute space to the item-incurred increase
    // ...when handling any intrinsic growth limit: all affected tracks.
    distribute_space_up_to_limits(
      extra_space,
      tracks,
      track_is_affected,
      |_| 1.0,
      |track| {
        if track.growth_limit == f32::INFINITY {
          track.base_size
        } else {
          track.growth_limit
        }
      },
      move |track| track.fit_content_limit(axis_inner_node_size),
    );
  };

  // 4. For each affected track, if the track’s item-incurred increase is larger than the track’s planned increase
  // set the track’s planned increase to that value.
  for track in tracks.iter_mut() {
    if track.item_incurred_increase > track.growth_limit_planned_increase {
      track.growth_limit_planned_increase = track.item_incurred_increase;
    }

    // Reset the item_incurresed increase ready for the next space distribution
    track.item_incurred_increase = 0.0;
  }
}

/// 11.6 Maximise Tracks
/// Distributes free space (if any) to tracks with FINITE growth limits, up to their limits.
#[inline(always)]
fn maximise_tracks(
  axis_tracks: &mut [GridTrack],
  axis_inner_node_size: Option<f32>,
  axis_available_grid_space: AvailableSpace,
) {
  let used_space: f32 = axis_tracks.iter().map(|track| track.base_size).sum();
  let free_space = axis_available_grid_space.compute_free_space(used_space);
  if free_space == f32::INFINITY {
    axis_tracks
      .iter_mut()
      .for_each(|track| track.base_size = track.growth_limit);
  } else if free_space > 0.0 {
    distribute_space_up_to_limits(
      free_space,
      axis_tracks,
      |_| true,
      |_| 1.0,
      |track| track.base_size,
      move |track: &GridTrack| track.fit_content_limited_growth_limit(axis_inner_node_size),
    );
    for track in axis_tracks.iter_mut() {
      track.base_size += track.item_incurred_increase;
      track.item_incurred_increase = 0.0;
    }
  }
}

/// 11.7. Expand Flexible Tracks
/// This step sizes flexible tracks using the largest value it can assign to an fr without exceeding the available space.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn expand_flexible_tracks<Tree: LayoutPartialTree>(
  tree: &mut Tree,
  axis: AbstractAxis,
  axis_tracks: &mut [GridTrack],
  other_axis_tracks: &[GridTrack],
  items: &mut [GridItem],
  axis_min_size: Option<f32>,
  axis_max_size: Option<f32>,
  axis_available_space_for_expansion: AvailableSpace,
  inner_node_size: Size<Option<f32>>,
  get_track_size_estimate: fn(&GridTrack, Option<f32>, &Tree) -> Option<f32>,
) {
  // First, find the grid’s used flex fraction:
  let flex_fraction = match axis_available_space_for_expansion {
    // If the free space is zero:
    //    The used flex fraction is zero.
    // Otherwise, if the free space is a definite length:
    //   The used flex fraction is the result of finding the size of an fr using all of the grid tracks and
    //   a space to fill of the available grid space.
    AvailableSpace::Definite(available_space) => {
      let used_space: f32 = axis_tracks.iter().map(|track| track.base_size).sum();
      let free_space = available_space - used_space;
      if free_space <= 0.0 {
        0.0
      } else {
        find_size_of_fr(axis_tracks, available_space)
      }
    }
    // If ... sizing the grid container under a min-content constraint the used flex fraction is zero.
    AvailableSpace::MinContent => 0.0,
    // Otherwise, if the free space is an indefinite length:
    AvailableSpace::MaxContent => {
      let other_axis = axis.other();
      let other_axis_available_space = inner_node_size.get(other_axis);

      // Compute prefix sums of other-axis track size estimates. This is used when probing
      // max-content contributions of flex items under an indefinite sizing constraint to supply an
      // other-axis available space estimate without iterating each item's spanned tracks.
      //
      // We only compute these sums if we detect any flex item that still needs its
      // `available_space_cache` populated (e.g. when intrinsic sizing was skipped).
      let needs_other_axis_estimate = items
        .iter()
        .any(|item| item.crosses_flexible_track(axis) && item.available_space_cache.is_none());

      let other_axis_prefix = if needs_other_axis_estimate {
        let mut prefix: Vec<PrefixSumMaybe> = Vec::with_capacity(other_axis_tracks.len() + 1);
        let mut running_sum = 0.0;
        let mut running_none = 0u32;
        prefix.push(PrefixSumMaybe {
          sum: running_sum,
          none: running_none,
        });
        for track in other_axis_tracks.iter() {
          match get_track_size_estimate(track, other_axis_available_space, tree) {
            Some(v) => {
              running_sum += v + track.content_alignment_adjustment;
            }
            None => running_none += 1,
          }
          prefix.push(PrefixSumMaybe {
            sum: running_sum,
            none: running_none,
          });
        }
        Some(prefix)
      } else {
        None
      };

      // The used flex fraction is the maximum of:
      let flex_fraction = f32_max(
        // For each flexible track, if the flexible track’s flex factor is greater than one,
        // the result of dividing the track’s base size by its flex factor; otherwise, the track’s base size.
        axis_tracks
          .iter()
          .filter(|track| track.max_track_sizing_function.is_fr())
          .map(|track| {
            let flex_factor = track.flex_factor();
            if flex_factor > 1.0 {
              track.base_size / flex_factor
            } else {
              track.base_size
            }
          })
          .max_by(|a, b| a.total_cmp(b))
          .unwrap_or(0.0),
        // For each grid item that crosses a flexible track, the result of finding the size of an fr using all the grid tracks
        // that the item crosses and a space to fill of the item’s max-content contribution.
        items
          .iter_mut()
          .filter(|item| item.crosses_flexible_track(axis))
          .map(|item| {
            let tracks = &axis_tracks[item.track_range_excluding_lines(axis)];
            // When computing max-content contributions for flex tracks under a max-content constraint,
            // we must supply an estimate of the item's available space in the other axis. Otherwise
            // `GridItem::known_dimensions()` cannot apply stretch alignment + aspect-ratio, causing
            // intrinsic contributions to incorrectly collapse to 0.
            if item.available_space_cache.is_none() {
              debug_assert!(needs_other_axis_estimate);
              let other_axis_size = sum_range(
                other_axis_prefix
                  .as_ref()
                  .expect("other_axis_prefix must be computed when needs_other_axis_estimate is true"),
                item.track_range_excluding_lines(other_axis),
              );
              let mut size = Size::NONE;
              size.set(other_axis, other_axis_size);
              item.available_space_cache = Some(size);
            }
            let available_space = item.available_space_cached(
              axis,
              other_axis_tracks,
              other_axis_available_space,
              |track, basis| get_track_size_estimate(track, basis, tree),
            );
            let max_content_contribution =
              item.max_content_contribution_cached(axis, tree, available_space, inner_node_size);
            find_size_of_fr(tracks, max_content_contribution)
          })
          .max_by(|a, b| a.total_cmp(b))
          .unwrap_or(0.0),
      );

      // If using this flex fraction would cause the grid to be smaller than the grid container’s min-width/height (or larger than the
      // grid container’s max-width/height), then redo this step, treating the free space as definite and the available grid space as equal
      // to the grid container’s inner size when it’s sized to its min-width/height (max-width/height).
      // (Note: min_size takes precedence over max_size)
      let hypothetical_grid_size: f32 = axis_tracks
        .iter()
        .map(|track| {
          if track.max_track_sizing_function.is_fr() {
            let track_flex_factor = track.max_track_sizing_function.0.value();
            f32_max(track.base_size, track_flex_factor * flex_fraction)
          } else {
            track.base_size
          }
        })
        .sum();
      let axis_min_size = axis_min_size.unwrap_or(0.0);
      let axis_max_size = axis_max_size.unwrap_or(f32::INFINITY);
      if hypothetical_grid_size < axis_min_size {
        find_size_of_fr(axis_tracks, axis_min_size)
      } else if hypothetical_grid_size > axis_max_size {
        find_size_of_fr(axis_tracks, axis_max_size)
      } else {
        flex_fraction
      }
    }
  };

  // For each flexible track, if the product of the used flex fraction and the track’s flex factor is greater
  // than the track’s base size, set its base size to that product.
  for track in axis_tracks
    .iter_mut()
    .filter(|track| track.max_track_sizing_function.is_fr())
  {
    let track_flex_factor = track.max_track_sizing_function.0.value();
    track.base_size = f32_max(track.base_size, track_flex_factor * flex_fraction);
  }
}

/// 11.7.1. Find the Size of an fr
/// This algorithm finds the largest size that an fr unit can be without exceeding the target size.
/// It must be called with a set of grid tracks and some quantity of space to fill.
#[inline(always)]
fn find_size_of_fr(tracks: &[GridTrack], space_to_fill: f32) -> f32 {
  // Handle the trivial case where there is no space to fill
  // Do not remove as otherwise the loop below will loop infinitely
  if space_to_fill == 0.0 {
    return 0.0;
  }

  // The spec describes this algorithm in a way that can "restart" multiple times as tracks become
  // inflexible. A direct implementation can devolve into O(n^2) behaviour on large track sets.
  //
  // Observations:
  // - For an `fr` track with base size `b` and flex factor `f`, the track remains "flexible" iff
  //   `f * fr_size >= b`, i.e. `fr_size >= b / f`.
  // - Therefore the set of flexible tracks at the fixed point is a prefix when the tracks are
  //   sorted by `threshold = base_size / flex_factor`.
  //
  // We exploit this by sorting the flex tracks by threshold and iteratively "peeling off" the
  // highest-threshold tracks until the computed fr_size satisfies all remaining thresholds.
  // This yields O(n log n) time dominated by the sort.

  #[derive(Clone, Copy)]
  struct FlexTrackInfo {
    threshold: f32,
    base_size: f32,
    flex_factor: f32,
  }

  let mut fixed_used_space = 0.0;
  // Most grids only have a small number of `fr` tracks. Store flex tracks on the stack in the
  // common case to avoid heap allocation churn, and fall back to a heap `Vec` if we overflow.
  const STACK_FLEX_TRACK_CAPACITY: usize = 32;
  let mut flex_tracks_stack: ArrayVec<FlexTrackInfo, STACK_FLEX_TRACK_CAPACITY> = ArrayVec::new();
  let mut flex_tracks_heap: Option<Vec<FlexTrackInfo>> = None;
  let mut flex_factor_sum = 0.0;

  for track in tracks.iter() {
    if track.max_track_sizing_function.is_fr() {
      let flex_factor = track.max_track_sizing_function.0.value();

      // CSS spec expects non-negative flex factors, but handle <= 0 defensively to avoid NaNs and
      // infinite loops. Treat such tracks as inflexible.
      if flex_factor > 0.0 && flex_factor.is_finite() {
        let threshold = track.base_size / flex_factor;
        let track_info = FlexTrackInfo {
          threshold,
          base_size: track.base_size,
          flex_factor,
        };

        if let Some(ref mut vec) = flex_tracks_heap {
          vec.push(track_info);
        } else if flex_tracks_stack.len() < flex_tracks_stack.capacity() {
          flex_tracks_stack.push(track_info);
        } else {
          // Promote to a heap vec once we exceed the stack capacity.
          let mut vec = Vec::with_capacity(tracks.len());
          vec.extend(flex_tracks_stack.drain(..));
          vec.push(track_info);
          flex_tracks_heap = Some(vec);
        }

        flex_factor_sum += flex_factor;
      } else {
        fixed_used_space += track.base_size;
      }
    } else {
      fixed_used_space += track.base_size;
    }
  }

  // If there are no usable flex tracks, then the fr size is just the leftover space.
  let flex_tracks_is_empty = match flex_tracks_heap.as_ref() {
    Some(vec) => vec.is_empty(),
    None => flex_tracks_stack.is_empty(),
  };
  if flex_tracks_is_empty {
    return space_to_fill - fixed_used_space;
  }

  // Sort by threshold ascending so we can efficiently move tracks from "flexible" to "inflexible"
  // by popping from the end.
  let flex_tracks: &[FlexTrackInfo] = match flex_tracks_heap.as_mut() {
    Some(vec) => {
      vec.sort_unstable_by(|a, b| a.threshold.total_cmp(&b.threshold));
      vec.as_slice()
    }
    None => {
      flex_tracks_stack.sort_unstable_by(|a, b| a.threshold.total_cmp(&b.threshold));
      flex_tracks_stack.as_slice()
    }
  };

  // `flexible_len` is the number of tracks still considered flexible (prefix length).
  let mut flexible_len = flex_tracks.len();
  let mut flexible_flex_factor_sum = flex_factor_sum;

  loop {
    // `find_size_of_fr` is on the hot path for grids with many tracks. Ensure we periodically check
    // for cooperative abort requests so render deadlines can surface as structured timeouts rather
    // than hard kills.
    check_layout_abort();

    let denominator = f32_max(flexible_flex_factor_sum, 1.0);
    let fr_size = (space_to_fill - fixed_used_space) / denominator;

    if flexible_len == 0 {
      return fr_size;
    }

    // If the current fr size satisfies the largest remaining threshold then it satisfies all.
    if fr_size >= flex_tracks[flexible_len - 1].threshold {
      return fr_size;
    }

    // Otherwise, peel off all tracks whose thresholds exceed the fr_size. These tracks are treated
    // as inflexible in the fixed point.
    let mut removed_this_pass = 0usize;
    while flexible_len > 0 && fr_size < flex_tracks[flexible_len - 1].threshold {
      flexible_len -= 1;
      let track = flex_tracks[flexible_len];
      fixed_used_space += track.base_size;
      flexible_flex_factor_sum -= track.flex_factor;

      removed_this_pass += 1;
      if removed_this_pass % 64 == 0 {
        check_layout_abort();
      }
    }
  }
}

#[cfg(all(test, feature = "taffy_tree"))]
fn find_size_of_fr_reference(tracks: &[GridTrack], space_to_fill: f32) -> f32 {
  // Handle the trivial case where there is no space to fill
  // Do not remove as otherwise the loop below will loop infinitely
  if space_to_fill == 0.0 {
    return 0.0;
  }

  // Reference implementation (the previous "restart" loop approach).
  let mut hypothetical_fr_size = f32::INFINITY;
  let mut previous_iter_hypothetical_fr_size;
  loop {
    // Let leftover space be the space to fill minus the base sizes of the non-flexible grid tracks.
    // Let flex factor sum be the sum of the flex factors of the flexible tracks. If this value is less than 1, set it to 1 instead.
    // We compute both of these in a single loop to avoid iterating over the data twice
    let mut used_space = 0.0;
    let mut naive_flex_factor_sum = 0.0;
    for track in tracks.iter() {
      // Tracks for which flex_factor * hypothetical_fr_size < track.base_size are treated as inflexible
      if track.max_track_sizing_function.is_fr()
        && track.max_track_sizing_function.0.value() * hypothetical_fr_size >= track.base_size
      {
        naive_flex_factor_sum += track.max_track_sizing_function.0.value();
      } else {
        used_space += track.base_size;
      };
    }
    let leftover_space = space_to_fill - used_space;
    let flex_factor = f32_max(naive_flex_factor_sum, 1.0);

    // Let the hypothetical fr size be the leftover space divided by the flex factor sum.
    previous_iter_hypothetical_fr_size = hypothetical_fr_size;
    hypothetical_fr_size = leftover_space / flex_factor;

    // If the product of the hypothetical fr size and a flexible track’s flex factor is less than the track’s base size,
    // restart this algorithm treating all such tracks as inflexible.
    let hypothetical_fr_size_is_valid = tracks.iter().all(|track| {
      if track.max_track_sizing_function.is_fr() {
        let flex_factor = track.max_track_sizing_function.0.value();
        flex_factor * hypothetical_fr_size >= track.base_size
          || flex_factor * previous_iter_hypothetical_fr_size < track.base_size
      } else {
        true
      }
    });
    if hypothetical_fr_size_is_valid {
      break;
    }
  }

  hypothetical_fr_size
}

/// 11.8. Stretch auto Tracks
/// This step expands tracks that have an auto max track sizing function by dividing any remaining positive, definite free space equally amongst them.
#[inline(always)]
fn stretch_auto_tracks(
  axis_tracks: &mut [GridTrack],
  axis_min_size: Option<f32>,
  axis_available_space_for_expansion: AvailableSpace,
) {
  let num_auto_tracks = axis_tracks
    .iter()
    .filter(|track| track.max_track_sizing_function.is_auto())
    .count();
  if num_auto_tracks > 0 {
    let used_space: f32 = axis_tracks.iter().map(|track| track.base_size).sum();

    // If the free space is indefinite, but the grid container has a definite min-width/height
    // use that size to calculate the free space for this step instead.
    let free_space = if axis_available_space_for_expansion.is_definite() {
      axis_available_space_for_expansion.compute_free_space(used_space)
    } else {
      match axis_min_size {
        Some(size) => size - used_space,
        None => 0.0,
      }
    };
    if free_space > 0.0 {
      let extra_space_per_auto_track = free_space / num_auto_tracks as f32;
      axis_tracks
        .iter_mut()
        .filter(|track| track.max_track_sizing_function.is_auto())
        .for_each(|track| track.base_size += extra_space_per_auto_track);
    }
  }
}

/// Helper function for distributing space to tracks evenly
/// Used by both distribute_item_space_to_base_size and maximise_tracks steps
#[inline(always)]
fn distribute_space_up_to_limits(
  space_to_distribute: f32,
  tracks: &mut [GridTrack],
  track_is_affected: impl Fn(&GridTrack) -> bool,
  track_distribution_proportion: impl Fn(&GridTrack) -> f32,
  track_affected_property: impl Fn(&GridTrack) -> f32,
  track_limit: impl Fn(&GridTrack) -> f32,
) -> f32 {
  /// Define a small constant to avoid infinite loops due to rounding errors. Rather than stopping distributing
  /// extra space when it gets to exactly zero, we will stop when it falls below this amount
  const THRESHOLD: f32 = 0.01;

  let mut space_to_distribute = space_to_distribute;
  while space_to_distribute > THRESHOLD {
    check_layout_abort();
    // Compute (in one pass) both:
    // - The sum of distribution proportions of all affected tracks that are still growable
    // - The smallest per-track increase limit, taking into account already-allocated increases
    //
    // This avoids multiple full scans over `tracks` per distribution iteration, which is a hot
    // path for large grids and spanning item processing.
    let mut track_distribution_proportion_sum: f32 = 0.0;
    let mut min_increase_limit: f32 = f32::INFINITY;
    for track in tracks.iter() {
      // Preserve the original evaluation order:
      // 1) check whether the track is still below its limit (cheap arithmetic)
      // 2) only then check whether the track is affected (potentially more expensive closure)
      let current = track_affected_property(track) + track.item_incurred_increase;
      let limit = track_limit(track);
      if current < limit && track_is_affected(track) {
        let proportion = track_distribution_proportion(track);
        track_distribution_proportion_sum += proportion;

        // Guard against division by zero or negative proportions. Treating these as having an
        // infinite cap ensures they never become the limiting track for this iteration.
        if proportion > 0.0 {
          // When distributing space in multiple iterations we must take into account the amount of
          // space already allocated to a track in prior iterations. Failing to do so can cause
          // tracks to grow past their limit (and steal space from other tracks), which then
          // cascades into incorrect grid layouts (e.g. wrapped tracks when there is enough space
          // for max-content).
          let increase_limit = (limit - current) / proportion;
          if increase_limit.total_cmp(&min_increase_limit) == Ordering::Less {
            min_increase_limit = increase_limit;
          }
        }
      }
    }

    if track_distribution_proportion_sum == 0.0 {
      break;
    }

    // Compute item-incurred increase for this iteration
    let iteration_item_incurred_increase = f32_min(
      min_increase_limit,
      space_to_distribute / track_distribution_proportion_sum,
    );

    for track in tracks.iter_mut() {
      if !track_is_affected(track) {
        continue;
      }

      let increase = iteration_item_incurred_increase * track_distribution_proportion(track);
      if increase > 0.0 {
        let new_value =
          track_affected_property(track) + track.item_incurred_increase + increase;
        if new_value <= track_limit(track) + THRESHOLD {
          track.item_incurred_increase += increase;
          space_to_distribute -= increase;
        }
      }
    }
  }

  space_to_distribute
}

#[cfg(all(test, feature = "taffy_tree"))]
mod tests {
  use crate::prelude::*;
  use crate::tree::RunMode;
  use crate::tree::MeasureOutput;
  use crate::CacheTree;

  use crate::geometry::{AbstractAxis, Point};
  use crate::style::{MaxTrackSizingFunction, MinTrackSizingFunction};
  use super::super::types::OriginZeroLine;

  /// Reference implementation of `distribute_space_up_to_limits` (pre-optimisation).
  ///
  /// This is kept in the test module so behaviour changes in the space distribution hot-path can
  /// be validated without carrying two production implementations.
  fn reference_distribute_space_up_to_limits(
    space_to_distribute: f32,
    tracks: &mut [super::GridTrack],
    track_is_affected: impl Fn(&super::GridTrack) -> bool,
    track_distribution_proportion: impl Fn(&super::GridTrack) -> f32,
    track_affected_property: impl Fn(&super::GridTrack) -> f32,
    track_limit: impl Fn(&super::GridTrack) -> f32,
  ) -> f32 {
    /// Define a small constant to avoid infinite loops due to rounding errors. Rather than stopping distributing
    /// extra space when it gets to exactly zero, we will stop when it falls below this amount
    const THRESHOLD: f32 = 0.01;

    let mut space_to_distribute = space_to_distribute;
    while space_to_distribute > THRESHOLD {
      super::check_layout_abort();
      let track_distribution_proportion_sum: f32 = tracks
        .iter()
        .filter(|track| {
          track_affected_property(track) + track.item_incurred_increase < track_limit(track)
        })
        .filter(|track| track_is_affected(track))
        .map(&track_distribution_proportion)
        .sum();

      if track_distribution_proportion_sum == 0.0 {
        break;
      }

      // Compute item-incurred increase for this iteration
      let min_increase_limit = tracks
        .iter()
        .filter(|track| {
          track_affected_property(track) + track.item_incurred_increase < track_limit(track)
        })
        .filter(|track| track_is_affected(track))
        .map(|track| {
          // When distributing space in multiple iterations we must take into account the amount of
          // space already allocated to a track in prior iterations. Failing to do so can cause tracks
          // to grow past their limit (and steal space from other tracks), which then cascades into
          // incorrect grid layouts (e.g. wrapped tracks when there is enough space for max-content).
          (track_limit(track) - (track_affected_property(track) + track.item_incurred_increase))
            / track_distribution_proportion(track)
        })
        .min_by(|a, b| a.total_cmp(b))
        .unwrap(); // We will never pass an empty track list to this function
      let iteration_item_incurred_increase = super::f32_min(
        min_increase_limit,
        space_to_distribute / track_distribution_proportion_sum,
      );

      for track in tracks.iter_mut().filter(|track| track_is_affected(track)) {
        let increase = iteration_item_incurred_increase * track_distribution_proportion(track);
        if increase > 0.0
          && track_affected_property(track) + track.item_incurred_increase + increase
            <= track_limit(track) + THRESHOLD
        {
          track.item_incurred_increase += increase;
          space_to_distribute -= increase;
        }
      }
    }

    space_to_distribute
  }

  fn build_grid_baseline_tree() -> (TaffyTree<()>, NodeId, [NodeId; 2]) {
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child_style = Style {
      size: Size::from_lengths(10.0, 10.0),
      align_self: Some(AlignSelf::Baseline),
      ..Default::default()
    };

    let child1 = taffy.new_leaf(child_style.clone()).unwrap();
    let child2 = taffy.new_leaf(child_style).unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(100.0, 100.0),
          grid_template_columns: vec![fr(1.0); 2],
          grid_template_rows: vec![fr(1.0); 1],
          ..Default::default()
        },
        &[child1, child2],
      )
      .unwrap();

    (taffy, root, [child1, child2])
  }

  #[test]
  fn grid_non_finite_baseline_is_treated_as_none_for_baseline_shims() {
    let (mut taffy_expected, root_expected, [child1_expected, child2_expected]) =
      build_grid_baseline_tree();
    taffy_expected
      .compute_layout_with_measure(root_expected, Size::MAX_CONTENT, |_, _, node_id, _, _| {
        MeasureOutput {
          size: Size {
            width: 10.0,
            height: 10.0,
          },
          first_baselines: Point {
            x: None,
            y: if node_id == child1_expected {
              None
            } else {
              Some(0.0)
            },
          },
        }
      })
      .unwrap();

    let expected_child1 = taffy_expected.layout(child1_expected).unwrap();
    let expected_child2 = taffy_expected.layout(child2_expected).unwrap();

    for baseline in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      let (mut taffy, root, [child1, child2]) = build_grid_baseline_tree();
      taffy
        .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, node_id, _, _| {
          MeasureOutput {
            size: Size {
              width: 10.0,
              height: 10.0,
            },
            first_baselines: Point {
              x: None,
              y: if node_id == child1 {
                Some(baseline)
              } else {
                Some(0.0)
              },
            },
          }
        })
        .unwrap();

      let child1_layout = taffy.layout(child1).unwrap();
      let child2_layout = taffy.layout(child2).unwrap();
      assert!(child1_layout.location.x.is_finite());
      assert!(child1_layout.location.y.is_finite());
      assert!(child2_layout.location.x.is_finite());
      assert!(child2_layout.location.y.is_finite());

      assert_eq!(child1_layout.location, expected_child1.location);
      assert_eq!(child2_layout.location, expected_child2.location);
    }
  }

  #[test]
  fn grid_container_baseline_uses_first_item_in_row_when_no_baseline_aligned_items_exist() {
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child1 = taffy.new_leaf(Style::default()).unwrap();
    let child2 = taffy.new_leaf(Style::default()).unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(100.0, 100.0),
          grid_template_columns: vec![fr(1.0); 2],
          grid_template_rows: vec![fr(1.0); 1],
          align_items: Some(AlignItems::Start),
          justify_items: Some(JustifyItems::Start),
          align_content: Some(AlignContent::Start),
          justify_content: Some(JustifyContent::Start),
          ..Default::default()
        },
        &[child1, child2],
      )
      .unwrap();

    taffy
      .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, node_id, _, _| {
        if node_id == child1 {
          MeasureOutput::from_size(Size {
            width: 10.0,
            height: 10.0,
          })
        } else {
          MeasureOutput::from_size(Size {
            width: 10.0,
            height: 20.0,
          })
        }
      })
      .unwrap();

    let output = taffy
      .cache_get(root, Size::NONE, Size::MAX_CONTENT, RunMode::PerformLayout)
      .expect("expected grid layout output to be cached");

    assert_eq!(output.first_baselines.y, Some(10.0));
  }

  #[test]
  fn grid_container_baseline_prefers_first_baseline_aligned_item_in_first_row() {
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child1 = taffy.new_leaf(Style::default()).unwrap();
    let child2 = taffy
      .new_leaf(Style {
        align_self: Some(AlignSelf::Baseline),
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(100.0, 100.0),
          grid_template_columns: vec![fr(1.0); 2],
          grid_template_rows: vec![fr(1.0); 1],
          align_items: Some(AlignItems::Start),
          justify_items: Some(JustifyItems::Start),
          align_content: Some(AlignContent::Start),
          justify_content: Some(JustifyContent::Start),
          ..Default::default()
        },
        &[child1, child2],
      )
      .unwrap();

    taffy
      .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, node_id, _, _| {
        if node_id == child1 {
          MeasureOutput::from_size(Size {
            width: 10.0,
            height: 10.0,
          })
        } else {
          MeasureOutput {
            size: Size {
              width: 10.0,
              height: 20.0,
            },
            first_baselines: Point {
              x: None,
              y: Some(7.0),
            },
          }
        }
      })
      .unwrap();

    let output = taffy
      .cache_get(root, Size::NONE, Size::MAX_CONTENT, RunMode::PerformLayout)
      .expect("expected grid layout output to be cached");

    assert_eq!(output.first_baselines.y, Some(7.0));
  }

  #[test]
  fn distribute_space_up_to_limits_respects_existing_allocations() {
    // Regression test for `distribute_space_up_to_limits`: when space is distributed in multiple
    // iterations (because some tracks hit their growth limits before others), we must account for
    // the space already allocated to each track when computing both:
    // - the next iteration's per-track increase limit
    // - whether a given track can accept a proposed increase
    //
    // If we don't, a track can receive increases that push it past its limit, which in turn causes
    // later grid sizing steps to produce incorrect track widths.

    let mut tracks = vec![
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
    ];

    // Base sizes are 0, but growth limits differ.
    tracks[0].base_size = 0.0;
    tracks[0].growth_limit = 5.0;

    tracks[1].base_size = 0.0;
    tracks[1].growth_limit = 10.0;

    tracks[2].base_size = 0.0;
    tracks[2].growth_limit = 100.0;

    let remaining = super::distribute_space_up_to_limits(
      30.0,
      &mut tracks,
      |_| true,
      |_| 1.0,
      |track| track.base_size,
      |track| track.growth_limit,
    );

    assert!(
      remaining.abs() < 0.01,
      "expected all space to be distributed"
    );

    let final_sizes: Vec<f32> = tracks
      .iter()
      .map(|track| track.base_size + track.item_incurred_increase)
      .collect();

    assert!((final_sizes[0] - 5.0).abs() < 1e-5);
    assert!((final_sizes[1] - 10.0).abs() < 1e-5);
    assert!((final_sizes[2] - 15.0).abs() < 1e-5);
  }

  #[test]
  fn grid_available_space_other_axis_sum_is_none_if_any_track_estimate_is_none() {
    // `GridItem::available_space()` historically summed `Option<f32>` estimates across the spanned
    // other-axis tracks using `sum::<Option<f32>>()`, which yields `None` if *any* element is
    // `None`.
    //
    // `resolve_intrinsic_track_sizes` now precomputes these sums via prefix sums, and must preserve
    // the exact same semantics.

    let mut taffy: TaffyTree<()> = TaffyTree::new();

    // Item spans two rows.
    let child = taffy
      .new_leaf(Style {
        grid_row: Line {
          start: line(1),
          end: line(3),
        },
        ..Default::default()
      })
      .unwrap();

    // Second row is `max-content`, meaning `get_track_size_estimate` returns `None` for that track
    // during the inline-axis intrinsic sizing pass.
    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(100.0, 100.0),
          grid_template_columns: vec![min_content()],
          grid_template_rows: vec![length(10.0), max_content()],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let saw_intrinsic_measure = std::cell::Cell::new(false);
    taffy
      .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, available_space, node_id, _, _| {
        // During inline-axis intrinsic sizing we probe min-content widths with an unconstrained
        // available width (`MinContent`). If the other-axis track-size estimate sum is `None` then
        // the available height should also be treated as `MinContent`.
        if node_id == child && matches!(available_space.width, AvailableSpace::MinContent) {
          saw_intrinsic_measure.set(true);
          assert!(
            matches!(available_space.height, AvailableSpace::MinContent),
            "expected other-axis available space to be MinContent when any spanned track estimate is None (got {available_space:?})",
          );
        }

        MeasureOutput {
          size: Size {
            width: 10.0,
            height: 10.0,
          },
          first_baselines: Point { x: None, y: None },
        }
      })
      .unwrap();

    assert!(
      saw_intrinsic_measure.get(),
      "expected intrinsic measurement probe to run"
    );
  }

  #[test]
  fn grid_available_space_includes_alignment_gutter_adjustment_in_other_axis_sum() {
    // `GridItem::available_space()` includes `track.content_alignment_adjustment` in its other-axis
    // estimate sum. This test ensures the prefix-sum optimisation preserves that behaviour.

    let mut taffy: TaffyTree<()> = TaffyTree::new();

    // Item spans two rows so it crosses the inner row gutter, which will receive a
    // `content_alignment_adjustment` when `align-content: space-between` distributes free space.
    let child = taffy
      .new_leaf(Style {
        grid_row: Line {
          start: line(1),
          end: line(3),
        },
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(100.0, 100.0),
          // Force intrinsic measurement in the inline axis.
          grid_template_columns: vec![min_content()],
          // Two fixed-size rows (total 20). With `align-content: space-between` and a definite
          // height of 100, the free space (80) should be assigned to the inner gutter.
          grid_template_rows: vec![length(10.0), length(10.0)],
          align_content: Some(AlignContent::SpaceBetween),
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let saw_intrinsic_measure = std::cell::Cell::new(false);
    taffy
      .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, available_space, node_id, _, _| {
        // During inline-axis intrinsic sizing we probe min-content widths with an unconstrained
        // available width (`MinContent`). The other-axis available height should include the
        // alignment gutter adjustment, yielding the full container height (100).
        if node_id == child && matches!(available_space.width, AvailableSpace::MinContent) {
          saw_intrinsic_measure.set(true);
          assert_eq!(available_space.height, AvailableSpace::Definite(100.0));
        }

        MeasureOutput {
          size: Size {
            width: 10.0,
            height: 10.0,
          },
          first_baselines: Point { x: None, y: None },
        }
      })
      .unwrap();

    assert!(
      saw_intrinsic_measure.get(),
      "expected intrinsic measurement probe to run"
    );
  }

  #[test]
  fn flex_max_content_probe_uses_other_axis_estimate_prefix_sums() {
    // When intrinsic track sizing is skipped (because there are no intrinsic tracks in the axis),
    // `expand_flexible_tracks` can still need to probe max-content contributions for items crossing
    // flexible tracks under an indefinite (max-content) sizing constraint.
    //
    // Those probes require an estimate of the item's available space in the other axis. Ensure we
    // still preserve Option-sum semantics (None if any spanned other-axis track estimate is None)
    // in this code path.

    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child = taffy
      .new_leaf(Style {
        // Place the item in the first row/column.
        grid_column: Line {
          start: line(1),
          end: span(1),
        },
        grid_row: Line {
          start: line(1),
          end: span(1),
        },
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          // minmax(0, 1fr): no intrinsic track sizing functions in the inline axis, so intrinsic
          // sizing is skipped and `available_space_cache` is not pre-filled.
          grid_template_columns: vec![minmax(length(0.0), fr(1.0)); 1],
          // Max-content row: other-axis track-size estimate is `None`.
          grid_template_rows: vec![max_content(); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let saw_max_content_probe = std::cell::Cell::new(false);
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::MaxContent,
          height: AvailableSpace::Definite(100.0),
        },
        |_known_dimensions, available_space, node_id, _, _| {
          // `max-content` probes in `expand_flexible_tracks` supply `AvailableSpace::MaxContent`
          // for the axis being sized. If the spanned other-axis track estimate sum is `None` then
          // the other axis should also be treated as `MaxContent`.
          if node_id == child && matches!(available_space.width, AvailableSpace::MaxContent) {
            saw_max_content_probe.set(true);
            assert!(
              matches!(available_space.height, AvailableSpace::MaxContent),
              "expected other-axis available space to be MaxContent when any spanned track estimate is None (got {available_space:?})",
            );
          }

          MeasureOutput::from_size(Size { width: 10.0, height: 10.0 })
        },
      )
      .unwrap();

    assert!(
      saw_max_content_probe.get(),
      "expected max-content probe to run during flexible track expansion"
    );
  }

  #[test]
  fn update_item_crosses_intrinsic_tracks_is_skipped_without_percentage_tracks() {
    super::UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS.with(|c| c.set(0));

    let mut taffy: TaffyTree<()> = TaffyTree::new();
    let child = taffy
      .new_leaf(Style {
        size: Size::from_lengths(10.0, 10.0),
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          grid_template_columns: vec![length(10.0); 1],
          grid_template_rows: vec![length(10.0); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

    super::UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS.with(|c| assert_eq!(c.get(), 0));
  }

  #[test]
  fn update_item_crosses_intrinsic_tracks_runs_with_percentage_tracks() {
    super::UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS.with(|c| c.set(0));

    let mut taffy: TaffyTree<()> = TaffyTree::new();
    let child = taffy
      .new_leaf(Style {
        size: Size::from_lengths(10.0, 10.0),
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          grid_template_columns: vec![percent(0.5); 1],
          grid_template_rows: vec![percent(0.5); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

    super::UPDATE_ITEM_CROSSES_INTRINSIC_TRACKS_FOR_AXIS_CALLS
      .with(|c| assert!(c.get() > 0));
  }

  #[test]
  fn repeat_fr_grid_should_not_measure_max_content_under_definite_container_size() {
    // Regression test: under a definite container size, a grid made entirely of `fr` tracks should
    // not need to probe max-content contributions as there are no `max-content` min tracks.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    const COLUMN_COUNT: usize = 16;
    let child_style = Style { ..Default::default() };

    let children: Vec<NodeId> = (0..COLUMN_COUNT)
      .map(|_| taffy.new_leaf(child_style.clone()).unwrap())
      .collect();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(500.0, 500.0),
          grid_template_columns: vec![fr(1.0); COLUMN_COUNT],
          grid_template_rows: vec![fr(1.0); 1],
          ..Default::default()
        },
        &children,
      )
      .unwrap();

    let mut max_content_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::Definite(500.0),
          height: AvailableSpace::Definite(500.0),
        },
        |_, available_space, _, _, _| {
          if matches!(available_space.width, AvailableSpace::MaxContent)
            || matches!(available_space.height, AvailableSpace::MaxContent)
          {
            max_content_probe_count += 1;
          }
          MeasureOutput::from_size(Size { width: 10.0, height: 10.0 })
        },
      )
      .unwrap();

    assert_eq!(max_content_probe_count, 0);
  }

  #[test]
  fn repeat_minmax_0_1fr_grid_should_not_measure_min_or_max_content_under_definite_container_size() {
    // Regression test: `minmax(0, 1fr)` tracks have no intrinsic min/max sizing functions, so
    // intrinsic sizing shouldn't issue MinContent or MaxContent measurement probes.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    const COLUMN_COUNT: usize = 16;
    let child_style = Style { ..Default::default() };

    let children: Vec<NodeId> = (0..COLUMN_COUNT)
      .map(|_| taffy.new_leaf(child_style.clone()).unwrap())
      .collect();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(500.0, 500.0),
          grid_template_columns: vec![flex(1.0); COLUMN_COUNT],
          grid_template_rows: vec![flex(1.0); 1],
          ..Default::default()
        },
        &children,
      )
      .unwrap();

    let mut intrinsic_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::Definite(500.0),
          height: AvailableSpace::Definite(500.0),
        },
        |_, available_space, _, _, _| {
          if matches!(
            available_space.width,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          ) || matches!(
            available_space.height,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          ) {
            intrinsic_probe_count += 1;
          }
          MeasureOutput::from_size(Size { width: 10.0, height: 10.0 })
        },
      )
      .unwrap();

    assert_eq!(intrinsic_probe_count, 0);
  }

  #[test]
  fn crosses_flexible_and_intrinsic_flags_match_track_ranges_and_percentage_rules() {
    fn make_item(
      node: NodeId,
      column_indexes: Line<u16>,
      row_indexes: Line<u16>,
    ) -> super::GridItem {
      let style: Style = Style::default();
      let mut item = super::GridItem::new_with_placement_style_and_order(
        node,
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
      item.column_indexes = column_indexes;
      item.row_indexes = row_indexes;
      item
    }

    // Track vector indices use the pattern:
    //   [gutter, track, gutter, track, ..., gutter]
    //
    // Columns include:
    // - a percentage gutter (must *not* be treated as intrinsic due to percentage)
    // - a flexible track
    // - a percentage track (treated as intrinsic only when container size is indefinite)
    // - an intrinsic track
    let mut columns = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO), // 0
      super::GridTrack::new(
        MinTrackSizingFunction::length(10.0),
        MaxTrackSizingFunction::length(10.0),
      ), // 1 fixed
      super::GridTrack::gutter(LengthPercentage::percent(0.1)), // 2 percentage gutter
      super::GridTrack::new(
        MinTrackSizingFunction::ZERO,
        MaxTrackSizingFunction::fr(1.0),
      ), // 3 flexible
      super::GridTrack::gutter(LengthPercentage::ZERO), // 4
      super::GridTrack::new(
        MinTrackSizingFunction::percent(0.2),
        MaxTrackSizingFunction::length(20.0),
      ), // 5 percentage track
      super::GridTrack::gutter(LengthPercentage::ZERO), // 6
      super::GridTrack::new(
        MinTrackSizingFunction::MIN_CONTENT,
        MaxTrackSizingFunction::length(0.0),
      ), // 7 intrinsic track
      super::GridTrack::gutter(LengthPercentage::ZERO), // 8
    ];

    // Make a gutter intrinsically-sized to ensure `track_range_excluding_lines()` correctly
    // excludes bounding grid lines (off-by-one would incorrectly mark item 2 as intrinsic).
    columns[6].min_track_sizing_function = MinTrackSizingFunction::MIN_CONTENT;

    // Rows include one flexible track and one intrinsic track.
    let rows = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO), // 0
      super::GridTrack::new(
        MinTrackSizingFunction::length(10.0),
        MaxTrackSizingFunction::length(10.0),
      ), // 1 fixed
      super::GridTrack::gutter(LengthPercentage::ZERO), // 2
      super::GridTrack::new(
        MinTrackSizingFunction::ZERO,
        MaxTrackSizingFunction::fr(1.0),
      ), // 3 flexible
      super::GridTrack::gutter(LengthPercentage::ZERO), // 4
      super::GridTrack::new(
        MinTrackSizingFunction::MIN_CONTENT,
        MaxTrackSizingFunction::length(0.0),
      ), // 5 intrinsic
      super::GridTrack::gutter(LengthPercentage::ZERO), // 6
    ];

    let mut items = vec![
      // Spans only the first fixed track in each axis.
      make_item(
        NodeId::new(0),
        Line { start: 0, end: 2 },
        Line { start: 0, end: 2 },
      ),
      // Spans fixed + (percentage gutter) + flexible. Percentage gutter must not count as intrinsic.
      make_item(
        NodeId::new(1),
        Line { start: 0, end: 4 },
        Line { start: 0, end: 4 },
      ),
      // Spans flexible + fixed gutter + percentage track. Percentage track is intrinsic only when size is indefinite.
      // Note: end line is 6; we ensure the intrinsically-sized gutter at index 6 is excluded.
      make_item(
        NodeId::new(2),
        Line { start: 2, end: 6 },
        Line { start: 2, end: 6 },
      ),
      // Spans only the intrinsic track in each axis.
      make_item(
        NodeId::new(3),
        Line { start: 6, end: 8 },
        Line { start: 4, end: 6 },
      ),
    ];

    super::determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    // Item 0: fixed-only
    assert!(!items[0].crosses_flexible_column);
    assert!(!items[0].crosses_intrinsic_column);
    assert!(!items[0].crosses_flexible_row);
    assert!(!items[0].crosses_intrinsic_row);

    // Item 1: spans a flexible track, but no intrinsic tracks.
    assert!(items[1].crosses_flexible_column);
    assert!(!items[1].crosses_intrinsic_column);
    assert!(items[1].crosses_flexible_row);
    assert!(!items[1].crosses_intrinsic_row);

    // Item 2: spans a flexible track + a percentage track (percentage is *not* intrinsic yet).
    assert!(items[2].crosses_flexible_column);
    assert!(!items[2].crosses_intrinsic_column);
    assert!(items[2].crosses_flexible_row);
    assert!(items[2].crosses_intrinsic_row);

    // Item 3: intrinsic-only
    assert!(!items[3].crosses_flexible_column);
    assert!(items[3].crosses_intrinsic_column);
    assert!(!items[3].crosses_flexible_row);
    assert!(items[3].crosses_intrinsic_row);

    // Indefinite inline size: percentage tracks behave as intrinsic.
    super::update_item_crosses_intrinsic_tracks_for_axis(
      AbstractAxis::Inline,
      &mut items,
      None,
    );
    assert_eq!(
      items.iter().map(|it| it.crosses_intrinsic_column).collect::<Vec<_>>(),
      vec![false, false, true, true]
    );
    // Row flags were not updated by the inline-axis call.
    assert_eq!(
      items.iter().map(|it| it.crosses_intrinsic_row).collect::<Vec<_>>(),
      vec![false, false, true, true]
    );

    // Definite inline size: percentage tracks behave as fixed.
    super::update_item_crosses_intrinsic_tracks_for_axis(
      AbstractAxis::Inline,
      &mut items,
      Some(100.0),
    );
    assert_eq!(
      items.iter().map(|it| it.crosses_intrinsic_column).collect::<Vec<_>>(),
      vec![false, false, false, true]
    );

    // Sanity-check the block-axis update path as well (no percentage rows, so it should be stable).
    super::update_item_crosses_intrinsic_tracks_for_axis(
      AbstractAxis::Block,
      &mut items,
      None,
    );
    assert_eq!(
      items.iter().map(|it| it.crosses_intrinsic_row).collect::<Vec<_>>(),
      vec![false, false, true, true]
    );
    assert_eq!(
      items.iter().map(|it| it.crosses_intrinsic_column).collect::<Vec<_>>(),
      vec![false, false, false, true]
    );

  }

  #[test]
  fn overlapping_items_in_same_cell_use_max_track_contribution() {
    fn compute_grid_width(first_child_width: f32, second_child_width: f32) -> f32 {
      let mut taffy: TaffyTree<()> = TaffyTree::new();

      let child_style = Style {
        // Explicitly place both items into the same 1x1 grid cell.
        grid_column: Line {
          start: line(1),
          end: line(2),
        },
        grid_row: Line {
          start: line(1),
          end: line(2),
        },
        // Prevent stretch alignment from forcing both children to the track size. We want the
        // track size to be determined by the children's intrinsic contributions.
        justify_self: Some(AlignSelf::Start),
        align_self: Some(AlignSelf::Start),
        ..Default::default()
      };

      let first_child = taffy.new_leaf(child_style.clone()).unwrap();
      let second_child = taffy.new_leaf(child_style).unwrap();

      let root = taffy
        .new_with_children(
          Style {
            display: Display::Grid,
            grid_template_columns: vec![auto()],
            grid_template_rows: vec![auto()],
            ..Default::default()
          },
          &[first_child, second_child],
        )
        .unwrap();

      taffy
        .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, node_id, _, _| {
          let width = if node_id == first_child {
            first_child_width
          } else if node_id == second_child {
            second_child_width
          } else {
            0.0
          };

          MeasureOutput {
            size: Size {
              width,
              height: 10.0,
            },
            first_baselines: Point::NONE,
          }
        })
        .unwrap();

      taffy.layout(root).unwrap().size.width
    }

    // In both insertion orders, the auto track size should reflect the maximum of the children's
    // intrinsic sizes (not whichever item happens to come first in the sorted grid item slice).
    assert_eq!(compute_grid_width(10.0, 20.0), 20.0);
    assert_eq!(compute_grid_width(20.0, 10.0), 20.0);

  }

  #[test]
  fn max_content_constraint_without_auto_or_max_content_min_tracks_should_not_measure_max_content() {
    // Under a max-content sizing constraint, step 11.5.3 ("max-content minimums") only affects tracks
    // whose *min* track sizing function is `auto` or `max-content`. If no such tracks exist, the step is a
    // no-op and should not trigger max-content contribution probes.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child = taffy
      .new_leaf(Style {
        grid_column: Line {
          start: line(1),
          end: span(2),
        },
        ..Default::default()
      })
      .unwrap();

    let track = minmax(
      MinTrackSizingFunction::length(0.0),
      MaxTrackSizingFunction::min_content(),
    );
    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          grid_template_columns: vec![track; 2],
          grid_template_rows: vec![length(10.0); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let mut max_content_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::MaxContent,
          height: AvailableSpace::Definite(100.0),
        },
        |known_dimensions, available_space, _, _, _| {
          // `known_dimensions` is `Size::NONE` for PerformLayout (final layout), but is forwarded for
          // RunMode::ComputeSize intrinsic probes. Filter to avoid counting final layout measurements.
          if known_dimensions != Size::NONE
            && (matches!(available_space.width, AvailableSpace::MaxContent)
              || matches!(available_space.height, AvailableSpace::MaxContent))
          {
            max_content_probe_count += 1;
          }
          MeasureOutput::from_size(Size { width: 10.0, height: 10.0 })
        },
      )
      .unwrap();

    assert_eq!(max_content_probe_count, 0);

  }

  #[test]
  fn min_content_constraint_spanning_item_over_minmax_0_fr_should_not_measure_min_content_when_no_tracks_are_affected() {
    // Step 11.5.1 ("intrinsic minimums") only affects tracks with an intrinsic *min* sizing function.
    // For flex-item batches, distribution further restricts to flexible tracks. If an item crosses a
    // flexible track whose min sizing function is definite (e.g. `minmax(0, 1fr)`), then the step is a
    // no-op and should not trigger min-content contribution probes.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child = taffy
      .new_leaf(Style {
        grid_column: Line {
          start: line(1),
          end: span(2),
        },
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          // First track is flexible but with a fixed min (minmax(0, 1fr)).
          // Second track has an intrinsic max so the item is still part of the intrinsic-sizing batch.
          grid_template_columns: vec![
            flex(1.0),
            minmax(
              MinTrackSizingFunction::length(0.0),
              MaxTrackSizingFunction::auto(),
            ),
          ],
          grid_template_rows: vec![length(10.0); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let mut min_content_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::MinContent,
          height: AvailableSpace::Definite(100.0),
        },
        |_, available_space, _, _, _| {
          if matches!(available_space.width, AvailableSpace::MinContent)
            || matches!(available_space.height, AvailableSpace::MinContent)
          {
            min_content_probe_count += 1;
          }
          MeasureOutput::from_size(Size { width: 10.0, height: 10.0 })
        },
      )
      .unwrap();

    assert_eq!(min_content_probe_count, 0);
  }

  #[test]
  fn grid_minmax_0_1fr_avoids_intrinsic_measure_fanout() {
    // Regression test for excessive intrinsic measurement in `resolve_intrinsic_track_sizes`.
    //
    // Grids with `minmax(0, 1fr)` tracks should not need to probe every item multiple times during
    // intrinsic track sizing (the track sizing functions are definite/flex, so there is no
    // intrinsic distribution work to do).
    const NUM_COLS: usize = 20;
    const NUM_ITEMS: usize = 200;

    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let mut children = Vec::with_capacity(NUM_ITEMS);
    for _ in 0..NUM_ITEMS {
      children.push(taffy.new_leaf(Style::default()).unwrap());
    }

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(1000.0, 1000.0),
          grid_template_columns: vec![minmax(length(0.0), fr(1.0)); NUM_COLS],
          grid_template_rows: vec![length(10.0)],
          grid_auto_rows: vec![length(10.0)],
          ..Default::default()
        },
        &children,
      )
      .unwrap();

    let mut measure_calls = 0usize;
    taffy
      .compute_layout_with_measure(root, Size::MAX_CONTENT, |_, _, _, _, _| {
        measure_calls += 1;
        MeasureOutput {
          size: Size {
            width: 10.0,
            height: 10.0,
          },
          first_baselines: Point::NONE,
        }
      })
      .unwrap();

    assert!(
      measure_calls <= NUM_ITEMS * 2,
      "expected near-final-layout-only measure calls; got {measure_calls} for {NUM_ITEMS} items"
    );

    let root_layout = taffy.layout(root).unwrap();
    assert!(root_layout.size.width.is_finite());
    assert!(root_layout.size.height.is_finite());
    assert_eq!(root_layout.size.width, 1000.0);
    assert_eq!(root_layout.size.height, 1000.0);

    // Basic invariants: first row item widths sum to the container width and positions are finite.
    let expected_col_width = 1000.0 / NUM_COLS as f32;
    let mut width_sum = 0.0;
    for (i, child) in children.iter().take(NUM_COLS).enumerate() {
      let layout = taffy.layout(*child).unwrap();
      assert!(layout.location.x.is_finite());
      assert!(layout.location.y.is_finite());
      assert!(layout.size.width.is_finite());
      assert!(layout.size.height.is_finite());
      width_sum += layout.size.width;

      // We pick numbers that divide evenly to avoid rounding noise.
      assert!((layout.location.x - (expected_col_width * i as f32)).abs() < 0.01);
      assert!((layout.size.width - expected_col_width).abs() < 0.01);
    }
    assert!((width_sum - 1000.0).abs() < 0.01);
  }

  fn make_fixed_track(base_size: f32) -> super::GridTrack {
    let mut track =
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO);
    track.base_size = base_size;
    track
  }

  fn make_fr_track(base_size: f32, flex_factor: f32) -> super::GridTrack {
    let mut track =
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::fr(flex_factor));
    track.base_size = base_size;
    track
  }

  #[test]
  fn find_size_of_fr_matches_reference_implementation() {
    const EPS: f32 = 1e-5;

    // A set of deterministic scenarios to compare the optimised implementation against the
    // previous loop+restart algorithm.
    //
    // Note: while these tests don't assert runtime directly, they include cases (including a 512
    // track stress-case) that previously exhibited O(n^2) behaviour.
    let mut track_sets: Vec<Vec<super::GridTrack>> = Vec::new();

    // Hand-crafted mixtures of fixed and fr tracks.
    track_sets.push(vec![
      make_fixed_track(10.0),
      make_fr_track(30.0, 1.0),
      make_fr_track(5.0, 2.0),
      make_fixed_track(7.0),
    ]);

    track_sets.push(vec![
      make_fr_track(0.0, 1.0),
      make_fr_track(100.0, 0.25),
      make_fr_track(50.0, 3.0),
    ]);

    track_sets.push(vec![
      make_fixed_track(1000.0),
      make_fr_track(1_000_000.0, 2.0),
      make_fr_track(250.0, 0.5),
      make_fixed_track(5.0),
    ]);

    track_sets.push(vec![
      make_fixed_track(0.0),
      make_fixed_track(10.0),
      make_fixed_track(25.0),
    ]);

    // Deterministic pseudo-random-ish generation to cover more combinations without relying on RNG.
    for seed in 0..32usize {
      let track_count = 1 + (seed % 12);
      let mut tracks: Vec<super::GridTrack> = Vec::with_capacity(track_count);
      for i in 0..track_count {
        let is_fr = ((seed * 31 + i * 17) % 5) < 3;
        let mut base_size = ((seed * 7 + i * 13) % 23) as f32 * 3.25 + (i as f32) * 0.5;
        // Add a few very large values to exercise threshold sorting with large magnitudes.
        if seed % 11 == 0 && i == 0 {
          base_size += 1_000_000.0;
        }

        if is_fr {
          let flex_factor_bucket = (seed * 13 + i * 3) % 6;
          let flex_factor = match flex_factor_bucket {
            0 => 0.25,
            1 => 0.5,
            2 => 1.0,
            3 => 2.0,
            4 => 5.0,
            _ => 10.0,
          };
          tracks.push(make_fr_track(base_size, flex_factor));
        } else {
          tracks.push(make_fixed_track(base_size));
        }
      }
      track_sets.push(tracks);
    }

    // Compare results across a variety of fill sizes for each set of tracks.
    for tracks in track_sets.iter() {
      let sum_base_sizes: f32 = tracks.iter().map(|t| t.base_size).sum();
      let spaces_to_fill = [
        0.0,
        sum_base_sizes - 10.0,
        sum_base_sizes,
        sum_base_sizes + 10.0,
        sum_base_sizes + 1000.0,
        12345.0,
      ];

      for &space_to_fill in spaces_to_fill.iter() {
        let reference = super::find_size_of_fr_reference(tracks, space_to_fill);
        let optimised = super::find_size_of_fr(tracks, space_to_fill);

        if reference.is_finite() {
          assert!(
            optimised.is_finite(),
            "optimised result is not finite when reference is finite (space_to_fill: {space_to_fill}, reference: {reference}, optimised: {optimised})"
          );
          assert!(
            (reference - optimised).abs() <= EPS,
            "mismatch (space_to_fill: {space_to_fill}, reference: {reference}, optimised: {optimised})"
          );
        } else if reference.is_nan() {
          assert!(optimised.is_nan());
        } else {
          assert!(optimised.is_infinite());
          assert_eq!(reference.is_sign_positive(), optimised.is_sign_positive());
        }
      }
    }
  }

  #[test]
  fn find_size_of_fr_matches_reference_stress_case() {
    const EPS: f32 = 1e-5;

    // Stress-shaped case: many flex tracks with ascending thresholds.
    let mut tracks: Vec<super::GridTrack> = Vec::new();
    tracks.reserve(512);
    for i in 1..=512 {
      // Vary flex factors slightly while keeping thresholds strictly increasing.
      let flex_factor = if i % 2 == 0 { 1.0 } else { 0.5 };
      let threshold = i as f32;
      let base_size = threshold * flex_factor;
      tracks.push(make_fr_track(base_size, flex_factor));
    }

    // Choose a fill size that causes a non-trivial number of tracks to become inflexible.
    let space_to_fill = 300.5 * (256.0 * 1.0 + 256.0 * 0.5);

    let reference = super::find_size_of_fr_reference(&tracks, space_to_fill);
    let optimised = super::find_size_of_fr(&tracks, space_to_fill);

    assert!(reference.is_finite());
    assert!(optimised.is_finite());
    assert!(
      (reference - optimised).abs() <= EPS,
      "mismatch (reference: {reference}, optimised: {optimised})"
    );
  }

  #[test]
  fn intrinsic_sizing_is_skipped_when_axis_has_no_intrinsic_tracks() {
    let mut taffy: TaffyTree<()> = TaffyTree::new();
    let child_style = Style {
      size: Size::from_lengths(10.0, 10.0),
      ..Default::default()
    };

    let child_start_2 = taffy.new_leaf(child_style.clone()).unwrap();
    let child_start_1 = taffy.new_leaf(child_style.clone()).unwrap();

    // Two minmax(0, 1fr)-equivalent tracks plus gutters/lines.
    let mut columns = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::length(0.0), MaxTrackSizingFunction::fr(1.0)),
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::length(0.0), MaxTrackSizingFunction::fr(1.0)),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    // A single fixed-size row track plus gutters/lines.
    let mut rows = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(
        MinTrackSizingFunction::length(0.0),
        MaxTrackSizingFunction::length(0.0),
      ),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    // Items intentionally out-of-order by `placement(axis).start` (start-2 item first).
    let mut items = vec![
      super::GridItem::new_with_placement_style_and_order(
        child_start_2,
        Line {
          start: OriginZeroLine(1),
          end: OriginZeroLine(2),
        },
        Line {
          start: OriginZeroLine(0),
          end: OriginZeroLine(1),
        },
        child_style.clone(),
        AlignItems::Stretch,
        AlignItems::Stretch,
        0,
      ),
      super::GridItem::new_with_placement_style_and_order(
        child_start_1,
        Line {
          start: OriginZeroLine(0),
          end: OriginZeroLine(1),
        },
        Line {
          start: OriginZeroLine(0),
          end: OriginZeroLine(1),
        },
        child_style,
        AlignItems::Stretch,
        AlignItems::Stretch,
        1,
      ),
    ];

    assert!(
      items[0].placement(AbstractAxis::Inline).start > items[1].placement(AbstractAxis::Inline).start
    );

    super::resolve_item_track_indexes(
      items.as_mut_slice(),
      super::TrackCounts::from_raw(0, 2, 0),
      super::TrackCounts::from_raw(0, 1, 0),
    );
    super::determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    let initial_order: Vec<NodeId> = items.iter().map(|item| item.node).collect();

    let mut tree = taffy.as_layout_tree();
    super::track_sizing_algorithm(
      &mut tree,
      AbstractAxis::Inline,
      None,
      None,
      AlignContent::Start,
      AlignContent::Start,
      Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(100.0),
      },
      Size {
        width: Some(100.0),
        height: Some(100.0),
      },
      &mut columns,
      &mut rows,
      items.as_mut_slice(),
      |track: &super::GridTrack, _parent_size: Option<f32>, _tree| Some(track.base_size),
      false, // has_baseline_aligned_item
    );

    // If `resolve_intrinsic_track_sizes` ran its normal path, it would have sorted `items`.
    let final_order: Vec<NodeId> = items.iter().map(|item| item.node).collect();
    assert_eq!(final_order, initial_order);

    // Basic sizing sanity check: both `1fr` tracks should divide the available space equally.
    let fr_track_sum: f32 = columns
      .iter()
      .filter(|track| track.kind == super::GridTrackKind::Track)
      .map(|track| track.base_size)
      .sum();
    assert!((fr_track_sum - 100.0).abs() < 0.01);

  }

  #[test]
  fn distribute_space_up_to_limits_matches_reference_algorithm() {
    // Multi-iteration distribution scenario with:
    // - differing per-track limits
    // - non-uniform distribution proportions
    // - a track with a zero proportion (should never receive space)
    let mut tracks = vec![
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
    ];

    // Base sizes are 0, but growth limits differ.
    tracks[0].growth_limit = 5.0;
    tracks[1].growth_limit = 8.0;
    tracks[2].growth_limit = 100.0;
    tracks[3].growth_limit = 30.0;
    tracks[4].growth_limit = 1000.0;

    let mut tracks_reference = tracks.clone();
    let mut tracks_optimized = tracks;

    fn is_affected(_: &super::GridTrack) -> bool {
      true
    }

    fn distribution_proportion(track: &super::GridTrack) -> f32 {
      if track.growth_limit == 8.0 {
        2.0
      } else if track.growth_limit == 30.0 {
        4.0
      } else if track.growth_limit == 100.0 {
        0.0
      } else {
        1.0
      }
    }

    fn affected_property(track: &super::GridTrack) -> f32 {
      track.base_size
    }

    fn limit(track: &super::GridTrack) -> f32 {
      track.growth_limit
    }

    let space = 200.0;

    let remaining_reference = reference_distribute_space_up_to_limits(
      space,
      &mut tracks_reference,
      is_affected,
      distribution_proportion,
      affected_property,
      limit,
    );
    let remaining_optimized = super::distribute_space_up_to_limits(
      space,
      &mut tracks_optimized,
      is_affected,
      distribution_proportion,
      affected_property,
      limit,
    );

    assert!(
      (remaining_reference - remaining_optimized).abs() < 1e-5,
      "remaining space differs between reference and optimized implementations"
    );
    assert!(
      remaining_optimized.abs() < 0.01,
      "expected all distributable space to be allocated"
    );

    for (reference, optimized) in tracks_reference.iter().zip(tracks_optimized.iter()) {
      assert!(
        (reference.item_incurred_increase - optimized.item_incurred_increase).abs() < 1e-5,
        "per-track increases differ (reference: {}, optimized: {})",
        reference.item_incurred_increase,
        optimized.item_incurred_increase
      );
    }
  }
  #[test]
  fn percent_track_is_treated_as_intrinsic_when_container_size_is_indefinite() {
    // A percentage-sized *track* (not gutter) should be treated as intrinsic when the grid
    // container size is indefinite in that axis.

    let columns = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(
        MinTrackSizingFunction::percent(0.5),
        MaxTrackSizingFunction::percent(0.5),
      ),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    let rows = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    let mut item = super::GridItem::new_with_placement_style_and_order(
      NodeId::from(0usize),
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Style::<crate::sys::DefaultCheapStr>::default(),
      AlignItems::Stretch,
      AlignItems::Stretch,
      0,
    );
    // Span the single track (between the outer gutters).
    item.column_indexes = Line { start: 0, end: 2 };
    item.row_indexes = Line { start: 0, end: 2 };

    let mut items = vec![item];
    super::determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    // Indefinite container size => percentage track behaves as intrinsic.
    super::update_item_crosses_intrinsic_tracks_for_axis(AbstractAxis::Inline, &mut items, None);
    assert!(items[0].crosses_intrinsic_track(AbstractAxis::Inline));

    // Definite container size => percentage track behaves as fixed.
    super::update_item_crosses_intrinsic_tracks_for_axis(
      AbstractAxis::Inline,
      &mut items,
      Some(100.0),
    );
    assert!(!items[0].crosses_intrinsic_track(AbstractAxis::Inline));
  }

  #[test]
  fn percent_gutter_does_not_count_towards_crosses_percentage() {
    // Percentage-sized *gutters* (gaps) must not cause an item to be treated as crossing a
    // percentage track (which would then incorrectly make it participate in intrinsic sizing when
    // the container size is indefinite).

    let columns = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::gutter(LengthPercentage::percent(0.5)),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    let rows = vec![
      super::GridTrack::gutter(LengthPercentage::ZERO),
      super::GridTrack::new(MinTrackSizingFunction::ZERO, MaxTrackSizingFunction::ZERO),
      super::GridTrack::gutter(LengthPercentage::ZERO),
    ];

    let mut item = super::GridItem::new_with_placement_style_and_order(
      NodeId::from(0usize),
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(2),
      },
      Line {
        start: OriginZeroLine(0),
        end: OriginZeroLine(1),
      },
      Style::<crate::sys::DefaultCheapStr>::default(),
      AlignItems::Stretch,
      AlignItems::Stretch,
      0,
    );
    // Span both tracks so the internal percent gutter is included in the range.
    item.column_indexes = Line { start: 0, end: 4 };
    item.row_indexes = Line { start: 0, end: 2 };

    let mut items = vec![item];
    super::determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    assert!(!items[0].crosses_percentage_column);

    super::update_item_crosses_intrinsic_tracks_for_axis(AbstractAxis::Inline, &mut items, None);
    assert!(!items[0].crosses_intrinsic_track(AbstractAxis::Inline));
  }
}
