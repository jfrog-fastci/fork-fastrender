//! Grid Formatting Context - CSS Grid Layout via Taffy
//!
//! This module implements CSS Grid layout by wrapping the Taffy layout library.
//! It converts between fastrender's box/fragment tree representation and Taffy's
//! internal representation, delegating the actual grid algorithm to Taffy.
//!
//! # Architecture
//!
//! 1. **BoxNode → Taffy Tree**: Convert fastrender BoxNode tree to Taffy nodes
//! 2. **ComputedStyle → Taffy Style**: Map CSS properties to Taffy style values
//! 3. **Taffy Layout**: Run Taffy's grid layout algorithm
//! 4. **Taffy → FragmentNode**: Convert Taffy layout results to fragments
//!
//! # CSS Grid Support
//!
//! Supports core CSS Grid features:
//! - grid-template-columns/rows with track sizing functions
//! - grid-auto-columns/rows for implicit tracks
//! - grid-auto-flow (row, column, dense variants)
//! - gap (row-gap, column-gap)
//! - grid-column/row placement (line numbers, spans, auto)
//! - align-content, justify-content, align-items, justify-items
//! - align-self, justify-self
//!
//! # References
//!
//! - CSS Grid Layout Module Level 2: <https://www.w3.org/TR/css-grid-2/>
//! - Taffy: <https://github.com/DioxusLabs/taffy>

use crate::error::{RenderError, RenderStage};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::constraints::AvailableSpace as CrateAvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::contexts::flex_cache::ShardedFlexCache;
use crate::layout::contexts::positioned::compute_relative_offset as compute_relative_offset_for_relative;
use crate::layout::contexts::positioned::ContainingBlock;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::fragmentainer_axes_hint;
use crate::layout::formatting_context::fragmentainer_block_offset_hint;
use crate::layout::formatting_context::fragmentainer_block_size_hint;
use crate::layout::formatting_context::intrinsic_block_cache_lookup;
use crate::layout::formatting_context::intrinsic_block_cache_store;
use crate::layout::formatting_context::intrinsic_cache_lookup;
use crate::layout::formatting_context::intrinsic_cache_store;
use crate::layout::formatting_context::layout_cache_lookup;
use crate::layout::formatting_context::layout_cache_store;
use crate::layout::formatting_context::remembered_size_cache_lookup;
use crate::layout::formatting_context::remembered_size_cache_store;
use crate::layout::formatting_context::set_fragmentainer_axes_hint;
use crate::layout::formatting_context::set_fragmentainer_block_offset_hint;
use crate::layout::formatting_context::set_fragmentainer_block_size_hint;
use crate::layout::formatting_context::FormattingContext;
use crate::layout::formatting_context::IntrinsicSizingMode;
use crate::layout::formatting_context::LayoutError;
use crate::layout::fragment_clone_profile::{self, CloneSite};
use crate::layout::profile::layout_timer;
use crate::layout::profile::LayoutKind;
use crate::layout::running_elements::clear_running_position_in_box_tree;
use crate::layout::style_override::{push_style_override, style_override_for, StyleOverrideGuard};
use crate::layout::taffy_integration::{
  record_taffy_compute, record_taffy_invocation, record_taffy_measure_call,
  record_taffy_node_cache_hit, record_taffy_node_cache_miss, record_taffy_style_cache_hit,
  record_taffy_style_cache_miss, taffy_counters_enabled, taffy_grid_container_style_fingerprint,
  taffy_grid_item_style_fingerprint, taffy_template_cache_limit, CachedTaffyTemplate, CachedTaffyTree,
  SendSyncStyle, TaffyAdapterKind, TaffyNodeCache, TaffyNodeCacheKey, TAFFY_ABORT_CHECK_STRIDE,
};
use crate::layout::utils::border_size_from_box_sizing;
use crate::layout::utils::clamp_with_order;
use crate::layout::utils::resolve_length_with_percentage_metrics;
use crate::layout::utils::resolve_length_with_percentage_metrics_and_root_font_metrics;
use crate::layout::utils::resolve_scrollbar_width;
use crate::render_control::{
  active_deadline, active_heartbeat, active_stage, check_active, check_active_periodic,
  with_deadline, StageGuard, StageHeartbeatGuard,
};
use crate::style::display::Display as CssDisplay;
use crate::style::display::FormattingContextType;
use crate::style::grid::validate_area_rectangles;
use crate::style::types::AlignContent;
use crate::style::types::AlignItems;
use crate::style::types::AspectRatio;
use crate::style::types::BoxSizing;
use crate::style::types::Direction;
use crate::style::types::GridAutoFlow;
use crate::style::types::GridTrack;
use crate::style::types::IntrinsicSizeKeyword;
use crate::style::types::JustifyContent;
use crate::style::types::Overflow as CssOverflow;
use crate::style::types::WhiteSpace;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{
  FragmentContent, FragmentNode, GridFragmentationInfo, GridItemFragmentationData, GridTrackRanges,
};
use parking_lot::RwLock;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use taffy::geometry::Line;
use taffy::geometry::Size as TaffySize;
use taffy::prelude::TaffyFitContent;
use taffy::prelude::TaffyMaxContent;
use taffy::prelude::TaffyMinContent;
use taffy::style::AlignContent as TaffyAlignContent;
use taffy::style::CompactLength;
use taffy::style::Dimension;
use taffy::style::Display;
use taffy::style::GridPlacement as TaffyGridPlacement;
use taffy::style::GridTemplateArea;
use taffy::style::GridTemplateComponent;
use taffy::style::GridTemplateRepetition;
use taffy::style::LengthPercentage;
use taffy::style::LengthPercentageAuto;
use taffy::style::MaxTrackSizingFunction;
use taffy::style::MinTrackSizingFunction;
use taffy::style::Overflow as TaffyOverflow;
use taffy::style::RepetitionCount;
use taffy::style::Style as TaffyStyle;
use taffy::style::TrackSizingFunction;
use taffy::style_helpers::TaffyAuto;
use taffy::tree::DetailedLayoutInfo;
use taffy::tree::Layout as TaffyLayout;
use taffy::tree::NodeId as TaffyNodeId;
use taffy::tree::TaffyTree;
use taffy::tree::TraversePartialTree;
use taffy::DetailedGridTracksInfo;

const MAX_MEASURED_KEYS_PER_NODE: usize = 32;
const GRID_DEADLINE_CHECK_STRIDE: usize = 64;
const GRID_CONTENT_VISIBILITY_AUTO_MAX_PASSES: usize = 4;

fn attach_fragment_style_for_box(fragment: &mut FragmentNode, box_node: &BoxNode) {
  let style_override = style_override_for(box_node.id);
  let effective_style = style_override.unwrap_or_else(|| box_node.style.clone());
  fragment.style = Some(effective_style);
  fragment.starting_style = box_node.starting_style.clone();
}

struct StyleOverrideStack {
  guards: Vec<StyleOverrideGuard>,
}

impl StyleOverrideStack {
  fn new() -> Self {
    Self { guards: Vec::new() }
  }

  fn push(&mut self, guard: StyleOverrideGuard) {
    self.guards.push(guard);
  }
}

impl Drop for StyleOverrideStack {
  fn drop(&mut self) {
    while self.guards.pop().is_some() {}
  }
}

#[inline]
fn check_layout_deadline(counter: &mut usize) -> Result<(), LayoutError> {
  if let Err(RenderError::Timeout { elapsed, .. }) =
    check_active_periodic(counter, GRID_DEADLINE_CHECK_STRIDE, RenderStage::Layout)
  {
    return Err(LayoutError::Timeout { elapsed });
  }
  Ok(())
}

type FingerprintHasher = FxHasher;

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

fn hash_available_space(space: CrateAvailableSpace, hasher: &mut FingerprintHasher) {
  use std::hash::Hash;
  match space {
    CrateAvailableSpace::Definite(value) => {
      0u8.hash(hasher);
      f32_to_canonical_bits(value).hash(hasher);
    }
    CrateAvailableSpace::Indefinite => {
      1u8.hash(hasher);
    }
    CrateAvailableSpace::MinContent => {
      2u8.hash(hasher);
    }
    CrateAvailableSpace::MaxContent => {
      3u8.hash(hasher);
    }
  }
}

fn hash_opt_f32(value: Option<f32>, hasher: &mut FingerprintHasher) {
  use std::hash::Hash;
  match value.filter(|v| v.is_finite() && *v >= 0.0) {
    Some(v) => {
      1u8.hash(hasher);
      f32_to_canonical_bits(v).hash(hasher);
    }
    None => {
      0u8.hash(hasher);
    }
  }
}

fn grid_constraints_fingerprint(constraints: &LayoutConstraints) -> u64 {
  use std::hash::Hash;
  use std::hash::Hasher;
  let mut h = FingerprintHasher::default();
  hash_available_space(constraints.available_width, &mut h);
  hash_available_space(constraints.available_height, &mut h);
  hash_opt_f32(constraints.used_border_box_width, &mut h);
  hash_opt_f32(constraints.used_border_box_height, &mut h);
  constraints
    .used_border_box_size_forces_block_percentage_base
    .hash(&mut h);
  hash_opt_f32(constraints.inline_percentage_base, &mut h);
  hash_opt_f32(constraints.block_percentage_base, &mut h);
  h.finish()
}

fn combine_fingerprints(a: u64, b: u64) -> u64 {
  use std::hash::Hash;
  use std::hash::Hasher;
  let mut h = FingerprintHasher::default();
  a.hash(&mut h);
  b.hash(&mut h);
  h.finish()
}

#[derive(Clone, Copy)]
enum Axis {
  Horizontal,
  Vertical,
}

#[derive(Clone, Copy, Debug)]
struct GridAxisStyle {
  writing_mode: WritingMode,
  direction: Direction,
}

fn subgrid_line_name_list_is_omitted(line_names: &[Vec<String>]) -> bool {
  // The optional `<line-name-list>` for `subgrid` can be omitted. The style parser represents this
  // omission as an empty Vec. (An explicit empty line-name-list, i.e. `subgrid []`, is represented
  // as `[[]]` and must not be treated as omitted.)
  line_names.is_empty()
}

impl GridAxisStyle {
  fn from_style(style: &ComputedStyle) -> Self {
    Self {
      writing_mode: style.writing_mode,
      direction: style.direction,
    }
  }

  fn effective_for_grid_container(style: &ComputedStyle, parent_axis: Option<Self>) -> Self {
    // Taffy's subgrid support assumes that a subgrid's track inheritance is expressed in the same
    // axis mapping as the containing grid (physical X/Y). In particular, the inherited track sizes
    // and gutters are computed in the parent's coordinate system.
    //
    // CSS Grid 2 does say that a subgrid's line numbering and placement rules obey the subgrid's
    // own writing-mode (https://www.w3.org/TR/css-grid-2/#subgrid-indexing). FastRender implements
    // that by post-processing the final in-flow child fragments in
    // `apply_subgrid_writing_mode_transpose` when the subgrid establishes an orthogonal writing
    // mode.
    //
    // Therefore:
    // - Independent grids use their own writing-mode for axis mapping.
    // - Subgrids inherit the axis mapping of their containing grid for Taffy track inheritance.
    let is_subgrid = (style.grid_row_subgrid || style.grid_column_subgrid) && !style.containment.layout;
    if is_subgrid {
      parent_axis
        .map(|parent| Self {
          writing_mode: parent.writing_mode,
          // `direction` is still honored locally for subgrid line numbering and placement rules.
          // Track inheritance only requires that we share the same writing-mode axis mapping.
          direction: style.direction,
        })
        .unwrap_or_else(|| Self::from_style(style))
    } else {
      Self::from_style(style)
    }
  }

  /// Returns the effective axis style that was used when mapping this grid container's CSS axes
  /// (rows/columns) into Taffy's fixed horizontal/vertical axes.
  ///
  /// Subgrids inherit axis mapping from their parent grid so that shared tracks (including gaps and
  /// named lines) stay in the same coordinate space.
  ///
  /// This mirrors the inheritance rule implemented by `effective_for_grid_container`, but derives
  /// the parent axis by walking the Taffy parent chain while the current node is a subgrid.
  ///
  /// Note: traversal is bounded to avoid pathological cycles if the Taffy tree is corrupted.
  fn effective_for_grid_layout_node(
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    fallback_style: &ComputedStyle,
  ) -> Self {
    const MAX_SUBGRID_ANCESTORS: usize = 64;

    let mut effective_writing_mode = fallback_style.writing_mode;
    let mut effective_direction = fallback_style.direction;
    // Direction is only inherited when the inline axis is inherited
    // (`grid-template-columns: subgrid` / `grid_column_subgrid`).
    let mut inherit_direction = fallback_style.grid_column_subgrid && !fallback_style.containment.layout;

    let mut current_style = fallback_style;
    let mut current_id = node_id;
    let mut current_is_subgrid = (current_style.grid_row_subgrid || current_style.grid_column_subgrid)
      && !current_style.containment.layout;
    let mut depth = 0usize;

    while current_is_subgrid && depth < MAX_SUBGRID_ANCESTORS {
      let Some(parent_id) = taffy.parent(current_id) else {
        break;
      };
      if parent_id == current_id {
        break;
      }
      let Some(parent_ptr) = taffy.get_node_context(parent_id).copied() else {
        break;
      };
      let parent_box_node = unsafe { &*parent_ptr };
      let parent_style: &ComputedStyle = &parent_box_node.style;

      effective_writing_mode = parent_style.writing_mode;
      if inherit_direction {
        effective_direction = parent_style.direction;
        inherit_direction = parent_style.grid_column_subgrid && !parent_style.containment.layout;
      }

      current_style = parent_style;
      current_id = parent_id;
      current_is_subgrid = (current_style.grid_row_subgrid || current_style.grid_column_subgrid)
        && !current_style.containment.layout;
      depth += 1;
    }

    Self {
      writing_mode: effective_writing_mode,
      direction: effective_direction,
    }
  }

  fn inline_is_horizontal(self) -> bool {
    matches!(self.writing_mode, WritingMode::HorizontalTb)
  }

  fn inline_positive(self) -> bool {
    crate::style::inline_axis_positive(self.writing_mode, self.direction)
  }

  fn block_positive(self) -> bool {
    match self.writing_mode {
      WritingMode::VerticalRl | WritingMode::SidewaysRl => false,
      _ => true,
    }
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum MeasureAvailKey {
  Definite(u32),
  /// Represents "effectively indefinite" available space.
  ///
  /// `constraints_from_taffy` treats very small definite sizes (`<= 1px`) as
  /// `AvailableSpace::Indefinite` to avoid pathological 0px probes. Encode that
  /// normalization in the cache key so near-zero definites coalesce.
  Indefinite,
  MinContent,
  MaxContent,
  /// Marker used when the corresponding `known_dimensions` axis is definite.
  ///
  /// When Taffy provides a known size, `constraints_from_taffy` ignores the
  /// `AvailableSpace` value for that axis. Including the raw `AvailableSpace`
  /// in the cache key would therefore create redundant entries (and redundant
  /// `fc.layout` calls) for identical measurements.
  Ignored,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct MeasureKey {
  /// Stable identifier for the measured box.
  ///
  /// Most nodes have a deterministic `BoxNode::id`; however, callers can construct `BoxNode`s
  /// directly without running them through `BoxTree::new` (which assigns ids). For those ephemeral
  /// nodes we fall back to a pointer-derived sentinel id (see `ensure_box_id`).
  box_id: usize,
  /// Pointer identity for the base style `Arc<ComputedStyle>`.
  ///
  /// Style overrides are applied via TLS without changing the `Arc` pointer on `BoxNode`, so the
  /// active override fingerprint is tracked separately.
  style_ptr: usize,
  /// Fingerprint for the currently active style override (if any).
  ///
  /// `style_override` intentionally avoids allocating a new `BoxNode` subtree, so caching must
  /// account for override state to avoid reusing measurements computed under a different effective
  /// style.
  override_fingerprint: Option<u64>,
  known_width: Option<u32>,
  known_height: Option<u32>,
  available_width: MeasureAvailKey,
  available_height: MeasureAvailKey,
}

const EPHEMERAL_BOX_ID_BASE: usize = 1usize << (usize::BITS - 1);

const GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES: usize = 262_144;
const GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH: usize = 16_384;

/// Number of shards used for the shared (cross-thread) grid measurement cache.
///
/// Grid layout can fan out across rayon workers, and a purely thread-local measure cache would
/// fragment reuse across those workers (amplifying expensive nested layout). A shared cache avoids
/// this duplication while keeping lock contention low via sharding.
const GRID_MEASURE_SHARED_CACHE_SHARDS: usize = 64;
const GRID_MEASURE_SHARED_CACHE_MAX_ENTRIES_PER_SHARD: usize =
  GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES / GRID_MEASURE_SHARED_CACHE_SHARDS;
/// Override-key entries (`MeasureKey::override_fingerprint.is_some()`) are capped separately when
/// `FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES` is enabled so transient override probes cannot evict
/// the entire base-style keyset from the shared cache.
const GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD: usize =
  GRID_MEASURE_SHARED_CACHE_MAX_ENTRIES_PER_SHARD / 4;
const GRID_MEASURE_SHARED_CACHE_EVICTION_BATCH_PER_SHARD: usize =
  GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH / GRID_MEASURE_SHARED_CACHE_SHARDS;

#[derive(Clone, Copy)]
struct GridMeasureCacheEntry {
  epoch: usize,
  output: taffy::tree::MeasureOutput,
}

#[derive(Default)]
struct GridMeasureCacheShard {
  map: FxHashMap<MeasureKey, GridMeasureCacheEntry>,
  override_entries: usize,
}

struct ShardedGridMeasureCache {
  shards: [RwLock<GridMeasureCacheShard>; GRID_MEASURE_SHARED_CACHE_SHARDS],
}

impl ShardedGridMeasureCache {
  fn new() -> Self {
    Self {
      shards: std::array::from_fn(|_| RwLock::new(GridMeasureCacheShard::default())),
    }
  }

  #[inline]
  fn shard_index_for_box_id(box_id: usize) -> usize {
    box_id % GRID_MEASURE_SHARED_CACHE_SHARDS
  }

  #[inline]
  fn get(&self, key: &MeasureKey, epoch: usize) -> Option<taffy::tree::MeasureOutput> {
    let shard = &self.shards[Self::shard_index_for_box_id(key.box_id)];
    {
      let shard = shard.read();
      if let Some(entry) = shard.map.get(key) {
        if entry.epoch == epoch {
          return Some(entry.output);
        }
      } else {
        return None;
      }
    }

    // Slow path: if the entry is stale, remove it lazily under a write lock.
    let mut shard = shard.write();
    if let Some(entry) = shard.map.get(key) {
      if entry.epoch == epoch {
        return Some(entry.output);
      }
      if entry.epoch != epoch {
        shard.map.remove(key);
        if key.override_fingerprint.is_some() {
          shard.override_entries = shard.override_entries.saturating_sub(1);
        }
      }
    }
    None
  }

  #[inline]
  fn insert(&self, key: MeasureKey, epoch: usize, output: taffy::tree::MeasureOutput) {
    let shard = &self.shards[Self::shard_index_for_box_id(key.box_id)];
    let mut shard = shard.write();

    let max_entries = GRID_MEASURE_SHARED_CACHE_MAX_ENTRIES_PER_SHARD;
    if max_entries == 0 {
      shard.map.clear();
      shard.override_entries = 0;
      return;
    }

    let is_override = key.override_fingerprint.is_some();
    if is_override && GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD == 0 {
      return;
    }

    let contains = shard.map.contains_key(&key);

    let evict_override_keys = |shard: &mut GridMeasureCacheShard, count: usize| {
      if count == 0 || shard.override_entries == 0 {
        return;
      }
      let keys: Vec<_> = shard
        .map
        .keys()
        .filter(|candidate| candidate.override_fingerprint.is_some())
        .take(count)
        .cloned()
        .collect();
      for key in keys {
        if shard.map.remove(&key).is_some() {
          shard.override_entries = shard.override_entries.saturating_sub(1);
        }
      }
    };

    if !contains {
      // When override key sharing is enabled, keep override entries bounded so they cannot evict the
      // entire base-style keyset.
      if is_override && shard.override_entries >= GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD
      {
        let eviction_batch = GRID_MEASURE_SHARED_CACHE_EVICTION_BATCH_PER_SHARD
          .max(1)
          .min(GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD.max(1))
          .min(shard.override_entries);
        evict_override_keys(&mut shard, eviction_batch);
      }

      if shard.map.len() >= max_entries {
        // Under total-capacity pressure, prefer evicting override entries first so transient
        // override variants do not evict stable base-style measurements.
        if shard.override_entries > 0 {
          let eviction_batch =
            GRID_MEASURE_SHARED_CACHE_EVICTION_BATCH_PER_SHARD.max(1).min(shard.override_entries);
          evict_override_keys(&mut shard, eviction_batch);
        }
      }
    }

    if shard.map.len() >= max_entries && !contains {
      let eviction_batch = GRID_MEASURE_SHARED_CACHE_EVICTION_BATCH_PER_SHARD
        .max(1)
        .min(max_entries)
        .min(shard.map.len());
      if eviction_batch > 0 {
        let keys: Vec<_> = shard.map.keys().take(eviction_batch).cloned().collect();
        for key in keys {
          if shard.map.remove(&key).is_some() && key.override_fingerprint.is_some() {
            shard.override_entries = shard.override_entries.saturating_sub(1);
          }
        }
      }
    }
    let inserted = shard
      .map
      .insert(
      key,
      GridMeasureCacheEntry {
        epoch,
        output,
      },
    );
    if inserted.is_none() && is_override {
      shard.override_entries = shard.override_entries.saturating_add(1);
    }
  }

  fn clear(&self) {
    for shard in &self.shards {
      let mut shard = shard.write();
      shard.map.clear();
      shard.override_entries = 0;
    }
  }
}

static GLOBAL_GRID_MEASURE_SIZE_CACHE: LazyLock<ShardedGridMeasureCache> =
  LazyLock::new(ShardedGridMeasureCache::new);
static GLOBAL_GRID_MEASURE_SIZE_CACHE_EPOCH: AtomicUsize = AtomicUsize::new(0);

static GRID_MEASURE_CACHE_TLS_HITS: AtomicU64 = AtomicU64::new(0);
static GRID_MEASURE_CACHE_SHARED_HITS: AtomicU64 = AtomicU64::new(0);
static GRID_MEASURE_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static GRID_MEASURE_CACHE_OVERRIDE_LOOKUPS: AtomicU64 = AtomicU64::new(0);
static GRID_MEASURE_CACHE_OVERRIDE_SHARED_BYPASS_MISSES: AtomicU64 = AtomicU64::new(0);
static GRID_MEASURE_CACHE_OVERRIDE_SHARED_HITS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn grid_measure_cache_profile_enabled() -> bool {
  crate::debug::runtime::runtime_toggles().truthy("FASTR_GRID_MEASURE_CACHE_PROFILE")
    || crate::debug::runtime::runtime_toggles().truthy("FASTR_LAYOUT_PROFILE")
}

#[inline]
fn grid_measure_cache_share_overrides_enabled() -> bool {
  crate::debug::runtime::runtime_toggles().truthy("FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES")
}

#[inline]
fn record_grid_measure_cache_tls_hit() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_TLS_HITS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_grid_measure_cache_shared_hit() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_SHARED_HITS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_grid_measure_cache_miss() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_grid_measure_cache_override_lookup() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_OVERRIDE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_grid_measure_cache_override_shared_bypass_miss() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_OVERRIDE_SHARED_BYPASS_MISSES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_grid_measure_cache_override_shared_hit() {
  if !grid_measure_cache_profile_enabled() {
    return;
  }
  GRID_MEASURE_CACHE_OVERRIDE_SHARED_HITS.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GridMeasureCacheCounters {
  pub tls_hits: u64,
  pub shared_hits: u64,
  pub misses: u64,
  pub override_lookups: u64,
  pub override_shared_bypass_misses: u64,
  pub override_shared_hits: u64,
}

pub fn grid_measure_cache_counters() -> GridMeasureCacheCounters {
  GridMeasureCacheCounters {
    tls_hits: GRID_MEASURE_CACHE_TLS_HITS.load(Ordering::Relaxed),
    shared_hits: GRID_MEASURE_CACHE_SHARED_HITS.load(Ordering::Relaxed),
    misses: GRID_MEASURE_CACHE_MISSES.load(Ordering::Relaxed),
    override_lookups: GRID_MEASURE_CACHE_OVERRIDE_LOOKUPS.load(Ordering::Relaxed),
    override_shared_bypass_misses: GRID_MEASURE_CACHE_OVERRIDE_SHARED_BYPASS_MISSES
      .load(Ordering::Relaxed),
    override_shared_hits: GRID_MEASURE_CACHE_OVERRIDE_SHARED_HITS.load(Ordering::Relaxed),
  }
}

pub fn reset_grid_measure_cache_counters() {
  GRID_MEASURE_CACHE_TLS_HITS.store(0, Ordering::Relaxed);
  GRID_MEASURE_CACHE_SHARED_HITS.store(0, Ordering::Relaxed);
  GRID_MEASURE_CACHE_MISSES.store(0, Ordering::Relaxed);
  GRID_MEASURE_CACHE_OVERRIDE_LOOKUPS.store(0, Ordering::Relaxed);
  GRID_MEASURE_CACHE_OVERRIDE_SHARED_BYPASS_MISSES.store(0, Ordering::Relaxed);
  GRID_MEASURE_CACHE_OVERRIDE_SHARED_HITS.store(0, Ordering::Relaxed);
}

fn grid_measure_size_cache_store_with_policy(
  cache: &mut FxHashMap<MeasureKey, taffy::tree::MeasureOutput>,
  key: MeasureKey,
  output: taffy::tree::MeasureOutput,
  max_entries: usize,
  eviction_batch: usize,
) {
  if max_entries == 0 {
    cache.clear();
    return;
  }
  if cache.len() >= max_entries && !cache.contains_key(&key) {
    // The grid measure cache can grow extremely large on pages with many grid items + probes.
    // Clearing the cache in this case causes avoidable thrash (re-measuring recently-seen nodes).
    // Instead, evict a bounded batch of entries to make room while keeping reuse stable.
    let eviction_batch = eviction_batch.max(1).min(max_entries).min(cache.len());
    if eviction_batch > 0 {
      let keys: Vec<_> = cache.keys().take(eviction_batch).cloned().collect();
      for key in keys {
        cache.remove(&key);
      }
    }
  }
  cache.insert(key, output);
}

thread_local! {
  /// Cross-invocation cache for grid item measurements keyed by quantized constraints.
  ///
  /// Grid containers can be laid out repeatedly during track sizing and intrinsic measurement (and
  /// each layout call triggers a fresh Taffy tree + measurement pass). Persisting measurements per
  /// render avoids re-running nested formatting-context layout for the same items across those
  /// invocations.
  static GRID_MEASURE_SIZE_CACHE: RefCell<FxHashMap<MeasureKey, taffy::tree::MeasureOutput>> =
    RefCell::new(FxHashMap::default());
  static GRID_MEASURE_SIZE_CACHE_EPOCH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
thread_local! {
  static GRID_MEASURE_SIZE_CACHE_ENABLED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
static GRID_MEASURE_SIZE_CACHE_TEST_LOCK: LazyLock<parking_lot::ReentrantMutex<()>> =
  LazyLock::new(|| parking_lot::ReentrantMutex::new(()));

#[cfg(test)]
fn grid_measure_size_cache_test_lock() -> parking_lot::ReentrantMutexGuard<'static, ()> {
  GRID_MEASURE_SIZE_CACHE_TEST_LOCK.lock()
}

#[inline]
fn grid_measure_size_cache_enabled() -> bool {
  #[cfg(test)]
  {
    GRID_MEASURE_SIZE_CACHE_ENABLED.with(|cell| cell.get())
  }
  #[cfg(not(test))]
  {
    true
  }
}

#[cfg(test)]
struct GridMeasureSizeCacheThreadGuard {
  prev: bool,
}

#[cfg(test)]
impl Drop for GridMeasureSizeCacheThreadGuard {
  fn drop(&mut self) {
    GRID_MEASURE_SIZE_CACHE_ENABLED.with(|cell| cell.set(self.prev));
    GRID_MEASURE_SIZE_CACHE_EPOCH.with(|cell| cell.set(0));
    GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow_mut().clear());
  }
}

#[cfg(test)]
fn reset_grid_measure_size_cache_for_test() {
  GRID_MEASURE_SIZE_CACHE_EPOCH.with(|cell| cell.set(0));
  GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow_mut().clear());
  GLOBAL_GRID_MEASURE_SIZE_CACHE_EPOCH.store(0, Ordering::Relaxed);
  GLOBAL_GRID_MEASURE_SIZE_CACHE.clear();
}

#[cfg(test)]
fn enable_grid_measure_size_cache_for_test_thread() -> GridMeasureSizeCacheThreadGuard {
  let prev = GRID_MEASURE_SIZE_CACHE_ENABLED.with(|cell| {
    let prev = cell.get();
    cell.set(true);
    prev
  });
  GRID_MEASURE_SIZE_CACHE_EPOCH.with(|cell| cell.set(0));
  GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow_mut().clear());
  GridMeasureSizeCacheThreadGuard { prev }
}

fn grid_measure_size_cache_use_epoch() -> usize {
  let epoch = crate::layout::formatting_context::intrinsic_cache_epoch().max(1);
  GRID_MEASURE_SIZE_CACHE_EPOCH.with(|cell| {
    if cell.get() != epoch {
      cell.set(epoch);
      GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow_mut().clear());
      let previous_global = GLOBAL_GRID_MEASURE_SIZE_CACHE_EPOCH.swap(epoch, Ordering::Relaxed);
      if previous_global != epoch {
        GLOBAL_GRID_MEASURE_SIZE_CACHE.clear();
      }
    }
  });
  epoch
}

fn grid_measure_size_cache_lookup(key: &MeasureKey) -> Option<taffy::tree::MeasureOutput> {
  if !grid_measure_size_cache_enabled() || (key.box_id & EPHEMERAL_BOX_ID_BASE) != 0 {
    return None;
  }

  let has_override = key.override_fingerprint.is_some();
  if has_override {
    record_grid_measure_cache_override_lookup();
  }

  let epoch = grid_measure_size_cache_use_epoch();
  if let Some(hit) = GRID_MEASURE_SIZE_CACHE.with(|cache| cache.borrow().get(key).copied()) {
    record_grid_measure_cache_tls_hit();
    return Some(hit);
  }

  // Avoid polluting the shared cache with style-override probes; they are typically short-lived and
  // local to a single measurement pass. Override sharing can be enabled for profiling / workload
  // exploration via `FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES`.
  if has_override && !grid_measure_cache_share_overrides_enabled() {
    record_grid_measure_cache_miss();
    record_grid_measure_cache_override_shared_bypass_miss();
    return None;
  }

  let Some(hit) = GLOBAL_GRID_MEASURE_SIZE_CACHE.get(key, epoch) else {
    record_grid_measure_cache_miss();
    return None;
  };
  record_grid_measure_cache_shared_hit();
  if has_override {
    record_grid_measure_cache_override_shared_hit();
  }

  GRID_MEASURE_SIZE_CACHE.with(|cache| {
    let mut cache = cache.borrow_mut();
    grid_measure_size_cache_store_with_policy(
      &mut cache,
      *key,
      hit,
      GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES,
      GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH,
    );
  });
  Some(hit)
}

fn grid_measure_size_cache_store(key: MeasureKey, output: taffy::tree::MeasureOutput) {
  if !grid_measure_size_cache_enabled() || (key.box_id & EPHEMERAL_BOX_ID_BASE) != 0 {
    return;
  }

  let key_copy = key;
  let epoch = grid_measure_size_cache_use_epoch();
  GRID_MEASURE_SIZE_CACHE.with(|cache| {
    let mut cache = cache.borrow_mut();
    grid_measure_size_cache_store_with_policy(
      &mut cache,
      key,
      output,
      GRID_MEASURE_SIZE_CACHE_MAX_ENTRIES,
      GRID_MEASURE_SIZE_CACHE_EVICTION_BATCH,
    );
  });

  match key_copy.override_fingerprint {
    None => {
      GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key_copy, epoch, output);
    }
    Some(_) => {
      if grid_measure_cache_share_overrides_enabled() {
        GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key_copy, epoch, output);
      }
    }
  }
}

impl MeasureKey {
  fn quantize(val: f32) -> f32 {
    let abs = val.abs();
    let step = if abs > 4096.0 {
      64.0
    } else if abs > 2048.0 {
      32.0
    } else if abs > 1024.0 {
      16.0
    } else if abs > 512.0 {
      // Keep medium-sized measurements (e.g. responsive media at ~550px wide) more precise. Grid
      // items frequently use percentage padding to establish aspect ratios; snapping widths to 8px
      // steps can visibly shift those boxes by a couple of pixels.
      2.0
    } else if abs > 256.0 {
      4.0
    } else {
      2.0
    };
    let quantized = (val / step).round() * step;
    if quantized == 0.0 {
      0.0
    } else {
      quantized
    }
  }

  fn quantize_to_bits(val: f32) -> u32 {
    Self::quantize(val).to_bits()
  }

  fn sanitize_definite(val: f32) -> f32 {
    if val.is_finite() {
      val.max(0.0)
    } else {
      0.0
    }
  }

  fn new(
    node: &BoxNode,
    mut known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    _viewport: Size,
    drop_available_height: bool,
  ) -> Self {
    fn avail_key(space: taffy::style::AvailableSpace) -> MeasureAvailKey {
      match space {
        taffy::style::AvailableSpace::Definite(v) => {
          let v = MeasureKey::sanitize_definite(v);
          if v <= 1.0 {
            MeasureAvailKey::Indefinite
          } else {
            MeasureAvailKey::Definite(MeasureKey::quantize_to_bits(v))
          }
        }
        taffy::style::AvailableSpace::MinContent => MeasureAvailKey::MinContent,
        taffy::style::AvailableSpace::MaxContent => MeasureAvailKey::MaxContent,
      }
    }

    // `constraints_from_taffy` treats tiny definite sizes (`<= 1px`) as effectively indefinite.
    // Mirror that normalization in the cache key so "probe" sizes coalesce and we don't end up
    // caching (or returning) a 0px forced layout.
    if let Some(w) = known_dimensions.width {
      if w <= 1.0
        && matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(v) if v <= 1.0
        )
      {
        known_dimensions.width = None;
      }
    }
    if let Some(h) = known_dimensions.height {
      if h <= 1.0
        && matches!(
          available_space.height,
          taffy::style::AvailableSpace::Definite(v) if v <= 1.0
        )
      {
        known_dimensions.height = None;
      }
    }

    let known_width = known_dimensions
      .width
      .map(|w| Self::quantize_to_bits(Self::sanitize_definite(w)));
    let known_height = known_dimensions
      .height
      .map(|h| Self::quantize_to_bits(Self::sanitize_definite(h)));
    let box_id = ensure_box_id(node);
    let override_fingerprint =
      crate::layout::style_override::style_override_fingerprint_for(box_id);

    Self {
      box_id,
      style_ptr: Arc::as_ptr(&node.style) as usize,
      override_fingerprint,
      known_width,
      known_height,
      available_width: if known_width.is_some() {
        MeasureAvailKey::Ignored
      } else {
        avail_key(available_space.width)
      },
      available_height: if known_height.is_some() {
        MeasureAvailKey::Ignored
      } else if drop_available_height {
        MeasureAvailKey::Indefinite
      } else {
        avail_key(available_space.height)
      },
    }
  }

  fn new_with_snapped_sizes(
    node: &BoxNode,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    viewport: Size,
    drop_available_height: bool,
  ) -> (
    Self,
    taffy::geometry::Size<Option<f32>>,
    taffy::geometry::Size<taffy::style::AvailableSpace>,
  ) {
    let key = Self::new(
      node,
      known_dimensions,
      available_space,
      viewport,
      drop_available_height,
    );

    let known_dimensions = taffy::geometry::Size {
      width: key.known_width.map(f32::from_bits),
      height: key.known_height.map(f32::from_bits),
    };

    let available_width = match key.available_width {
      MeasureAvailKey::Definite(bits) => {
        taffy::style::AvailableSpace::Definite(f32::from_bits(bits))
      }
      MeasureAvailKey::Indefinite => taffy::style::AvailableSpace::Definite(0.0),
      MeasureAvailKey::MinContent => taffy::style::AvailableSpace::MinContent,
      MeasureAvailKey::MaxContent => taffy::style::AvailableSpace::MaxContent,
      MeasureAvailKey::Ignored => {
        taffy::style::AvailableSpace::Definite(key.known_width.map(f32::from_bits).unwrap_or(0.0))
      }
    };

    let available_height = match key.available_height {
      MeasureAvailKey::Definite(bits) => {
        taffy::style::AvailableSpace::Definite(f32::from_bits(bits))
      }
      MeasureAvailKey::Indefinite => taffy::style::AvailableSpace::Definite(0.0),
      MeasureAvailKey::MinContent => taffy::style::AvailableSpace::MinContent,
      MeasureAvailKey::MaxContent => taffy::style::AvailableSpace::MaxContent,
      MeasureAvailKey::Ignored => {
        taffy::style::AvailableSpace::Definite(key.known_height.map(f32::from_bits).unwrap_or(0.0))
      }
    };

    let available_space = taffy::geometry::Size {
      width: available_width,
      height: available_height,
    };

    (key, known_dimensions, available_space)
  }
}

fn height_depends_on_available_height(style: &ComputedStyle) -> bool {
  let height_depends = style.height.as_ref().is_some_and(Length::has_percentage)
    || style
      .height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
  let min_depends = style
    .min_height
    .as_ref()
    .is_some_and(Length::has_percentage)
    || style
      .min_height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
  let max_depends = style
    .max_height
    .as_ref()
    .is_some_and(Length::has_percentage)
    || style
      .max_height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));

  height_depends || min_depends || max_depends
}

fn taffy_available_space_for_grid_container(
  style: &ComputedStyle,
  constraints: &LayoutConstraints,
) -> taffy::geometry::Size<taffy::style::AvailableSpace> {
  let mut available_space = taffy::geometry::Size {
    width: match constraints.available_width {
      CrateAvailableSpace::Definite(w) => taffy::style::AvailableSpace::Definite(w),
      CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
      CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
      CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
    },
    height: match constraints.available_height {
      CrateAvailableSpace::Definite(h) => taffy::style::AvailableSpace::Definite(h),
      CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
      CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
      CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
    },
  };

  // In scrollable layout (no pagination/fragmentation), block-axis available space from the
  // containing block should not constrain auto-sized grid containers.
  //
  // FastRender's block formatting context threads a definite available block size from the
  // viewport down the tree so percentage sizes can resolve, but CSS layout does not constrain
  // in-flow children to that size — they can overflow and extend the scrollable content area.
  // Taffy, however, treats a definite available size in the block axis as an input to grid track
  // sizing and will stretch `auto` block sizes to fill it (notably when `align-content: stretch`,
  // which is common and the initial value for grid containers).
  //
  // When we're not in a fragmentation context and the grid container's block size is `auto`,
  // treat the available block size as indefinite so the grid's used block size is content-based.
  //
  // Note: in vertical writing modes, the CSS block axis maps to the physical X axis, so we must
  // drop the available *width* rather than the available *height*.
  if fragmentainer_block_size_hint().is_none() {
    // Percent heights compute to `auto` when the containing block's block size is not definite
    // (CSS2.1 §10.5). In that case, the grid container behaves like `height:auto` for sizing and
    // should not be constrained by a definite available height inherited from ancestors (e.g. the
    // viewport height threaded through scrollable layout).
    let physical_height_is_effectively_auto = physical_height_is_auto(style)
      || (style
        .height
        .as_ref()
        .is_some_and(crate::style::values::Length::has_percentage)
        && constraints.block_percentage_base.is_none());

    let physical_block_axis = if crate::style::inline_axis_is_horizontal(style.writing_mode) {
      PhysicalAxis::Y
    } else {
      PhysicalAxis::X
    };

    match physical_block_axis {
      PhysicalAxis::Y => {
        if constraints.used_border_box_height.is_none()
          && physical_height_is_effectively_auto
          && matches!(
            available_space.height,
            taffy::style::AvailableSpace::Definite(_)
          )
        {
          available_space.height = taffy::style::AvailableSpace::MaxContent;
        }
      }
      PhysicalAxis::X => {
        let width_computes_to_auto = physical_width_is_auto(style)
          || (style.position.is_in_flow()
            && constraints.block_percentage_base.is_none()
            && style.width_keyword.is_none()
            && style.width.as_ref().is_some_and(Length::has_percentage));
        if constraints.used_border_box_width.is_none()
          && width_computes_to_auto
          && matches!(
            available_space.width,
            taffy::style::AvailableSpace::Definite(_)
          )
        {
          available_space.width = taffy::style::AvailableSpace::MaxContent;
        }
      }
    }
  }

  available_space
}

#[inline]
fn physical_width_is_auto(style: &ComputedStyle) -> bool {
  style.width.is_none() && style.width_keyword.is_none()
}

#[inline]
fn physical_height_is_auto(style: &ComputedStyle) -> bool {
  style.height.is_none() && style.height_keyword.is_none()
}

fn grid_track_is_definite(track: &GridTrack, containing_block_definite: bool) -> bool {
  match track {
    GridTrack::Length(len) => !len.has_percentage() || containing_block_definite,
    GridTrack::Fr(_) => containing_block_definite,
    GridTrack::MinMax(min, max) => {
      grid_track_is_definite(min, containing_block_definite)
        && grid_track_is_definite(max, containing_block_definite)
    }
    // Content-based track sizing functions do not provide a definite track size for percentage
    // resolution.
    GridTrack::Auto
    | GridTrack::MinContent
    | GridTrack::MaxContent
    | GridTrack::FitContent(_)
    | GridTrack::RepeatAutoFill { .. }
    | GridTrack::RepeatAutoFit { .. } => false,
  }
}

fn grid_container_allows_stretch_block_size_override(
  style: &ComputedStyle,
  constraints: &LayoutConstraints,
) -> bool {
  // `align-self: stretch` makes grid items fill their grid area. We sometimes forward that
  // stretched size into nested layout via `used_border_box_height` so percentage heights inside
  // the item can resolve.
  //
  // For content-sized tracks (notably the default implicit `auto` tracks used by horizontal
  // carousels), treating the track size as a definite containing block for percentages is
  // incorrect and can create circular sizing dependencies (`height:100%` expands to the probe
  // size, inflating the track, which then inflates the probe again).
  //
  // Only forward the stretched size when the grid container's first track on the block axis is
  // definitely sized.
  let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
  if !inline_is_horizontal {
    // Writing-mode-aware track selection is more complex than the current needs; keep existing
    // behaviour for non-horizontal writing modes.
    return true;
  }

  // Grid containers can have a definite block size even when the parent layout algorithm provides
  // an indefinite available height (common for normal block flow). In that case, track sizing can
  // still produce definite `fr` sizes and `align-self:stretch` should establish a definite
  // containing block for percentage heights inside the item.
  //
  // Treat a non-percentage `height` as a definite block-size basis in addition to the parent
  // forwarding a definite available height / used border box height.
  let containing_block_definite = constraints.height().is_some()
    || constraints.used_border_box_height.is_some()
    || style
      .height
      .as_ref()
      .is_some_and(|len| !len.has_percentage());
  let track = if !style.grid_template_rows.is_empty() {
    &style.grid_template_rows[0]
  } else {
    style.grid_auto_rows.get(0).unwrap_or(&GridTrack::Auto)
  };
  grid_track_is_definite(track, containing_block_definite)
}

fn grid_item_allows_stretch_block_size_override(
  container_style: &ComputedStyle,
  container_constraints: &LayoutConstraints,
  item_style: &ComputedStyle,
) -> bool {
  // Preserve the existing behaviour for non-horizontal writing modes; determining which physical
  // axis maps to grid rows/columns is more complex than the current needs.
  let inline_is_horizontal = crate::style::inline_axis_is_horizontal(container_style.writing_mode);
  if !inline_is_horizontal {
    return true;
  }

  // Fall back to the container-level heuristic when we can't determine which tracks the item spans
  // (e.g. placement via named lines or auto-placement).
  let fallback =
    grid_container_allows_stretch_block_size_override(container_style, container_constraints);

  let row_start = item_style.grid_row_start;
  let mut row_end = item_style.grid_row_end;
  if row_start > 0 && row_end == 0 {
    // An explicit start with an auto end implies a 1-track span.
    row_end = row_start.saturating_add(1);
  }
  if row_start <= 0 || row_end <= 0 || row_end <= row_start {
    return fallback;
  }

  let containing_block_definite = container_constraints.height().is_some()
    || container_constraints.used_border_box_height.is_some()
    || container_style
      .height
      .as_ref()
      .is_some_and(|len| !len.has_percentage());
  let auto_track_fallback = GridTrack::Auto;
  let implicit_track = container_style
    .grid_auto_rows
    .get(0)
    .unwrap_or(&auto_track_fallback);

  // Track indices are 0-based while grid lines are 1-based.
  let start_track = (row_start - 1) as usize;
  let end_track_exclusive = (row_end - 1) as usize;
  for track_idx in start_track..end_track_exclusive {
    let track = container_style
      .grid_template_rows
      .get(track_idx)
      .unwrap_or(implicit_track);
    if !grid_track_is_definite(track, containing_block_definite) {
      return false;
    }
  }

  true
}

fn node_or_in_flow_children_depend_on_available_height(node: &BoxNode) -> bool {
  if height_depends_on_available_height(&node.style) {
    return true;
  }

  // Absolute/fixed-positioned boxes don't contribute to a parent's intrinsic block size, so don't
  // force cache key separation based on their percentage heights.
  node.children.iter().any(|child| {
    child.style.position.is_in_flow() && height_depends_on_available_height(&child.style)
  })
}

fn drop_definite_available_height_for_measure(
  node: &BoxNode,
  fc_type: FormattingContextType,
  known_height: Option<f32>,
  available_height: taffy::style::AvailableSpace,
  allow_stretch_block_size_override: bool,
) -> bool {
  if !matches!(available_height, taffy::style::AvailableSpace::Definite(_)) {
    return false;
  }
  if known_height.is_some() {
    return true;
  }

  // If the grid container is in content-sized block tracks, the definite heights Taffy probes with
  // are not a valid percentage basis (CSS2.1 §10.5). Treat them as effectively indefinite even when
  // descendants use percentage heights: those percentages must compute to `auto` to avoid cyclic
  // track sizing.
  let intrinsic_block_fc = matches!(
    fc_type,
    FormattingContextType::Block
      | FormattingContextType::Inline
      | FormattingContextType::Table
      | FormattingContextType::Flex
  );
  if intrinsic_block_fc && !allow_stretch_block_size_override {
    return true;
  }

  // Taffy can probe grid items with arbitrary definite heights during track sizing. Treat those as
  // indefinite for formatting contexts that resolve block sizes intrinsically, unless the box (or
  // any in-flow descendant) has percentage heights that require a definite block-size basis.
  if intrinsic_block_fc && !node_or_in_flow_children_depend_on_available_height(node) {
    return true;
  }

  false
}

fn push_measured_key(keys: &mut Vec<MeasureKey>, key: MeasureKey) -> Option<MeasureKey> {
  // Keep the list small, but de-dup and move recently seen keys to the end so we have a better
  // chance of reusing measured fragments when Taffy cycles through multiple sizing modes.
  if let Some(pos) = keys.iter().position(|existing| *existing == key) {
    keys.remove(pos);
  }
  let evicted = if keys.len() >= MAX_MEASURED_KEYS_PER_NODE {
    Some(keys.remove(0))
  } else {
    None
  };
  keys.push(key);
  evicted
}

#[cfg(test)]
thread_local! {
  // Grid measure callbacks can be triggered during many unrelated tests. Keep the counter
  // thread-local so tests can make assertions without racing other parallel test threads.
  static GRID_MEASURE_LAYOUT_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_grid_measure_layout_calls() {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.set(0));
}

#[cfg(test)]
fn grid_measure_layout_calls() -> usize {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.get())
}

#[cfg(test)]
fn record_measure_layout_call() {
  GRID_MEASURE_LAYOUT_CALLS.with(|counter| counter.set(counter.get() + 1));
}

#[cfg(not(test))]
fn record_measure_layout_call() {}

fn record_fragment_clone(site: CloneSite, fragment: &FragmentNode) {
  fragment_clone_profile::record_fragment_clone_from_fragment(site, fragment);
}

fn ensure_box_id(node: &BoxNode) -> usize {
  if node.id != 0 {
    return node.id;
  }
  EPHEMERAL_BOX_ID_BASE | (node as *const BoxNode as usize)
}

fn is_collapsible_ascii_whitespace_only(text: &str) -> bool {
  // Only treat the ASCII whitespace characters that participate in CSS white-space collapsing as
  // collapsible. In particular, do not consider NBSP collapsible.
  text.chars().all(|ch| {
    matches!(
      ch,
      '\r' | '\n' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' | ' ' | '\t'
    )
  })
}

fn is_collapsible_whitespace_grid_item(node: &BoxNode) -> bool {
  // Inter-element whitespace commonly appears in HTML source, but whitespace-only text nodes that
  // collapse to empty should not generate grid items (matching browser behavior). If we keep these
  // nodes, Taffy will place them into grid tracks, shifting real items and expanding the implicit
  // grid.
  if !matches!(
    node.style.white_space,
    WhiteSpace::Normal | WhiteSpace::Nowrap
  ) {
    return false;
  }

  fn subtree_is_whitespace_only(node: &BoxNode) -> bool {
    match &node.box_type {
      crate::tree::box_tree::BoxType::Text(text) => {
        is_collapsible_ascii_whitespace_only(&text.text)
      }
      crate::tree::box_tree::BoxType::Marker(marker) => match &marker.content {
        crate::tree::box_tree::MarkerContent::Text(text) => {
          is_collapsible_ascii_whitespace_only(text)
        }
        crate::tree::box_tree::MarkerContent::Image(_) => false,
      },
      crate::tree::box_tree::BoxType::Anonymous(_) => {
        node.children.iter().all(subtree_is_whitespace_only)
      }
      _ => false,
    }
  }

  match &node.box_type {
    crate::tree::box_tree::BoxType::Text(text) => is_collapsible_ascii_whitespace_only(&text.text),
    crate::tree::box_tree::BoxType::Marker(marker) => match &marker.content {
      crate::tree::box_tree::MarkerContent::Text(text) => {
        is_collapsible_ascii_whitespace_only(text)
      }
      crate::tree::box_tree::MarkerContent::Image(_) => false,
    },
    crate::tree::box_tree::BoxType::Anonymous(anon)
      if matches!(
        anon.anonymous_type,
        crate::tree::box_tree::AnonymousType::Block
          | crate::tree::box_tree::AnonymousType::FieldsetContent
          | crate::tree::box_tree::AnonymousType::Inline
      ) =>
    {
      subtree_is_whitespace_only(node)
    }
    _ => false,
  }
}

fn constraints_from_taffy(
  _viewport_size: crate::geometry::Size,
  mut known: taffy::geometry::Size<Option<f32>>,
  available: taffy::geometry::Size<taffy::style::AvailableSpace>,
  writing_mode: WritingMode,
  _inline_percentage_base: Option<f32>,
) -> LayoutConstraints {
  // Taffy can probe leaf nodes with "tiny" definite constraints (0px/1px) when it really means
  // "unknown"/unconstrained. Our integration treats these as indefinite; normalize known sizes in
  // the same cases so we don't accidentally force a 0px layout and then cache it.
  if let Some(w) = known.width {
    if w <= 1.0
      && matches!(
        available.width,
        taffy::style::AvailableSpace::Definite(v) if v <= 1.0
      )
    {
      known.width = None;
    }
  }
  if let Some(h) = known.height {
    if h <= 1.0
      && matches!(
        available.height,
        taffy::style::AvailableSpace::Definite(v) if v <= 1.0
      )
    {
      known.height = None;
    }
  }

  let sanitize_definite = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
  let known_width = known.width.map(sanitize_definite);
  let known_height = known.height.map(sanitize_definite);
  let width = match (known_width, available.width) {
    (Some(w), _) => CrateAvailableSpace::Definite(w),
    (_, taffy::style::AvailableSpace::Definite(w)) => {
      if w <= 1.0 {
        CrateAvailableSpace::Indefinite
      } else {
        CrateAvailableSpace::Definite(sanitize_definite(w))
      }
    }
    (_, taffy::style::AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
    (_, taffy::style::AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
  };
  let height = match (known_height, available.height) {
    (Some(h), _) => CrateAvailableSpace::Definite(h),
    (_, taffy::style::AvailableSpace::Definite(h)) => {
      if h <= 1.0 {
        CrateAvailableSpace::Indefinite
      } else {
        CrateAvailableSpace::Definite(sanitize_definite(h))
      }
    }
    (_, taffy::style::AvailableSpace::MinContent) => CrateAvailableSpace::MinContent,
    (_, taffy::style::AvailableSpace::MaxContent) => CrateAvailableSpace::MaxContent,
  };

  let mut constraints = LayoutConstraints::new(width, height);
  constraints.used_border_box_width = known_width;
  constraints.used_border_box_height = known_height;
  // Percentages on grid items resolve against the *grid area's* definite size (not the grid
  // container's size).
  //
  // This matters for both axes:
  // - `width: <percentage>` resolves against the grid area inline size (physical X in
  //   horizontal-tb).
  // - `height: <percentage>` resolves against the grid area block size (physical Y in
  //   horizontal-tb).
  //
  // When the grid area size is not definite (e.g. intrinsic sizing probes like min/max-content),
  // keep the base unset so percentages behave like `auto` per CSS2.1 §10.5 / CSS Sizing.
  //
  // Note: `LayoutConstraints::{inline,block}_percentage_base` are in the box's *logical* axes, so
  // map Taffy's physical available space through the box's writing mode.
  let inline_is_horizontal = crate::style::inline_axis_is_horizontal(writing_mode);
  let inline_avail = if inline_is_horizontal {
    available.width
  } else {
    available.height
  };
  let block_avail = if inline_is_horizontal {
    available.height
  } else {
    available.width
  };
  constraints.inline_percentage_base = match inline_avail {
    taffy::style::AvailableSpace::Definite(v) if v > 1.0 => Some(v),
    _ => None,
  };
  constraints.block_percentage_base = match block_avail {
    taffy::style::AvailableSpace::Definite(v) if v > 1.0 => Some(v),
    _ => None,
  };
  constraints
}

/// Grid Formatting Context
///
/// Implements CSS Grid layout by delegating to the Taffy library.
/// Each layout operation creates a fresh Taffy tree, performs layout,
/// and converts results back to FragmentNode.
///
/// # Thread Safety
///
/// GridFormattingContext is stateless and can be shared across threads.
/// Each layout operation is independent and creates its own Taffy tree.
///
/// # Example
///
/// ```ignore
/// use fastrender::layout::contexts::GridFormattingContext;
/// use fastrender::{FormattingContext, LayoutConstraints};
///
/// let fc = GridFormattingContext::new();
/// let fragment = fc.layout(&box_node, &constraints)?;
/// ```
#[derive(Clone)]
pub struct GridFormattingContext {
  /// Shared factory used to create child formatting contexts without losing shared caches.
  factory: FormattingContextFactory,
  viewport_size: crate::geometry::Size,
  font_context: crate::text::font_loader::FontContext,
  nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
  nearest_fixed_cb: crate::layout::contexts::positioned::ContainingBlock,
  taffy_cache: std::sync::Arc<crate::layout::taffy_integration::TaffyNodeCache>,
  parallelism: LayoutParallelism,
}

impl GridFormattingContext {
  fn grid_item_continuation_available_block_size(
    &self,
    bounds: Rect,
    constraints: &LayoutConstraints,
    container_block_size: f32,
  ) -> Option<f32> {
    if !container_block_size.is_finite() {
      return None;
    }

    // Only attempt continuation adjustments when we're in an actual fragmentation context.
    // Without a fragmentainer hint, the grid item is simply overflowing its containing block,
    // not flowing into a continuation fragment.
    let fragment_axes = fragmentainer_axes_hint()?;
    let fragmentainer_block_size =
      fragmentainer_block_size_hint().filter(|size| size.is_finite() && *size > 0.0)?;
    let available_block = match fragment_axes.block_axis() {
      PhysicalAxis::X => constraints.available_width,
      PhysicalAxis::Y => constraints.available_height,
    };
    let base_first_fragment_block_size = match available_block {
      CrateAvailableSpace::Definite(value) if value.is_finite() && value > 0.0 => {
        value.min(fragmentainer_block_size)
      }
      _ => fragmentainer_block_size,
    };
    let container_abs_block_start = fragmentainer_block_offset_hint();
    if !container_abs_block_start.is_finite() {
      return None;
    }
    let offset_in_fragment = container_abs_block_start.rem_euclid(fragmentainer_block_size);
    if !offset_in_fragment.is_finite() {
      return None;
    }
    let first_fragment_remaining = if offset_in_fragment <= 0.01 {
      fragmentainer_block_size
    } else {
      (fragmentainer_block_size - offset_in_fragment).max(0.0)
    };
    let first_fragment_block_size = base_first_fragment_block_size.min(first_fragment_remaining);

    let block_start = fragment_axes.block_start(&bounds, container_block_size);
    if !block_start.is_finite() {
      return None;
    }

    // Only adjust in continuation fragments. This is a best-effort heuristic based on the
    // box's flow position relative to the fragmentainer size.
    //
    // https://www.w3.org/TR/css-grid-2/#fragmentation
    if block_start < first_fragment_block_size {
      return None;
    }

    let relative = (block_start - first_fragment_block_size).max(0.0);
    let offset_in_fragment = relative.rem_euclid(fragmentainer_block_size);
    let remaining = if offset_in_fragment <= 0.01 {
      fragmentainer_block_size
    } else {
      (fragmentainer_block_size - offset_in_fragment).max(0.0)
    };
    remaining.is_finite().then_some(remaining)
  }

  fn is_simple_grid(
    &self,
    style: &ComputedStyle,
    children: &[&BoxNode],
    deadline_counter: &mut usize,
  ) -> Result<bool, LayoutError> {
    if !matches!(style.display, CssDisplay::Grid | CssDisplay::InlineGrid) {
      return Ok(false);
    }
    // The simple-grid optimization treats a grid container as a block container in Taffy. This is
    // only equivalent when CSS logical axes map to Taffy's fixed physical axes. In vertical writing
    // modes, the block axis maps to physical X, so a block fallback would stack items vertically
    // instead of along the block axis (physical X).
    if !crate::style::inline_axis_is_horizontal(style.writing_mode) {
      return Ok(false);
    }
    if !matches!(style.aspect_ratio, AspectRatio::Auto) {
      return Ok(false);
    }
    if style.grid_row_subgrid || style.grid_column_subgrid {
      return Ok(false);
    }
    if !style.grid_template_columns.is_empty() || !style.grid_template_rows.is_empty() {
      return Ok(false);
    }
    if !style.grid_template_areas.is_empty()
      || !style.grid_column_names.is_empty()
      || !style.grid_row_names.is_empty()
      || !style.grid_column_line_names.is_empty()
      || !style.grid_row_line_names.is_empty()
    {
      return Ok(false);
    }
    let auto_track =
      |tracks: &[GridTrack], deadline_counter: &mut usize| -> Result<bool, LayoutError> {
        for track in tracks {
          check_layout_deadline(deadline_counter)?;
          if !matches!(track, GridTrack::Auto) {
            return Ok(false);
          }
        }
        Ok(true)
      };
    if !auto_track(&style.grid_auto_rows, deadline_counter)?
      || !auto_track(&style.grid_auto_columns, deadline_counter)?
    {
      return Ok(false);
    }
    if style.grid_gap.value != 0.0
      || style.grid_row_gap.value != 0.0
      || style.grid_column_gap.value != 0.0
    {
      return Ok(false);
    }
    if style.grid_auto_flow != GridAutoFlow::Row {
      return Ok(false);
    }
    if style.align_items != AlignItems::Stretch
      || style.justify_items != AlignItems::Stretch
      || style.align_content != AlignContent::Stretch
      || style.justify_content != JustifyContent::Normal
    {
      return Ok(false);
    }

    for child in children {
      check_layout_deadline(deadline_counter)?;
      let cs = &child.style;
      if cs.grid_column_start != 0
        || cs.grid_column_end != 0
        || cs.grid_row_start != 0
        || cs.grid_row_end != 0
        || cs.align_self.is_some()
        || cs.justify_self.is_some()
      {
        return Ok(false);
      }
    }

    Ok(true)
  }

  /// Creates a new GridFormattingContext
  pub fn new() -> Self {
    let viewport_size = crate::geometry::Size::new(800.0, 600.0);
    Self::with_viewport_and_cb(
      viewport_size,
      crate::layout::contexts::positioned::ContainingBlock::viewport(viewport_size),
      crate::text::font_loader::FontContext::new(),
    )
  }

  // NOTE: Grid axis mapping logic (writing-mode + direction and subgrid inheritance) lives in
  // `GridAxisStyle`; avoid introducing parallel helpers in `GridFormattingContext`.

  pub fn with_viewport(viewport_size: crate::geometry::Size) -> Self {
    Self::with_viewport_and_cb(
      viewport_size,
      crate::layout::contexts::positioned::ContainingBlock::viewport(viewport_size),
      crate::text::font_loader::FontContext::new(),
    )
  }

  pub fn with_viewport_and_cb(
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
    font_context: crate::text::font_loader::FontContext,
  ) -> Self {
    Self::with_viewport_cb_and_cache(
      viewport_size,
      nearest_positioned_cb,
      font_context,
      std::sync::Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
        TaffyAdapterKind::Grid,
      ))),
    )
  }

  pub fn with_viewport_cb_and_cache(
    viewport_size: crate::geometry::Size,
    nearest_positioned_cb: crate::layout::contexts::positioned::ContainingBlock,
    font_context: crate::text::font_loader::FontContext,
    taffy_cache: std::sync::Arc<crate::layout::taffy_integration::TaffyNodeCache>,
  ) -> Self {
    let factory = FormattingContextFactory::with_font_context_viewport_cb_and_cache(
      font_context.clone(),
      viewport_size,
      nearest_positioned_cb,
      std::sync::Arc::new(ShardedFlexCache::new_measure()),
      std::sync::Arc::new(ShardedFlexCache::new_layout()),
      std::sync::Arc::new(TaffyNodeCache::new(taffy_template_cache_limit(
        TaffyAdapterKind::Flex,
      ))),
      taffy_cache.clone(),
    );
    Self::with_factory(factory)
  }

  pub(crate) fn with_factory(factory: FormattingContextFactory) -> Self {
    let viewport_size = factory.viewport_size();
    let nearest_positioned_cb = factory.nearest_positioned_cb();
    let nearest_fixed_cb = factory.nearest_fixed_cb();
    let font_context = factory.font_context().clone();
    let parallelism = factory.parallelism();
    let taffy_cache = factory.grid_taffy_cache();
    Self {
      factory,
      viewport_size,
      font_context,
      nearest_positioned_cb,
      nearest_fixed_cb,
      taffy_cache,
      parallelism,
    }
  }

  pub fn with_parallelism(mut self, parallelism: LayoutParallelism) -> Self {
    self.parallelism = parallelism;
    self.factory = self.factory.clone().with_parallelism(parallelism);
    self.taffy_cache = self.factory.grid_taffy_cache();
    self
  }

  fn horizontal_edges_px(&self, style: &ComputedStyle) -> Option<f32> {
    let left = self.resolve_length_px(&style.padding_left, style)?;
    let right = self.resolve_length_px(&style.padding_right, style)?;
    let bl = self.resolve_length_px(&style.used_border_left_width(), style)?;
    let br = self.resolve_length_px(&style.used_border_right_width(), style)?;
    Some(left + right + bl + br)
  }

  fn vertical_edges_px(&self, style: &ComputedStyle) -> Option<f32> {
    let top = self.resolve_length_px(&style.padding_top, style)?;
    let bottom = self.resolve_length_px(&style.padding_bottom, style)?;
    let bt = self.resolve_length_px(&style.used_border_top_width(), style)?;
    let bb = self.resolve_length_px(&style.used_border_bottom_width(), style)?;
    Some(top + bottom + bt + bb)
  }

  fn resolve_length_for_width(
    &self,
    length: Length,
    percentage_base: f32,
    style: &ComputedStyle,
  ) -> f32 {
    let base = if percentage_base.is_finite() {
      Some(percentage_base)
    } else {
      None
    };
    resolve_length_with_percentage_metrics_and_root_font_metrics(
      length,
      base,
      self.viewport_size,
      style.font_size,
      style.root_font_size,
      Some(style),
      Some(&self.font_context),
      self.factory.root_font_metrics(),
    )
    .unwrap_or(0.0)
  }

  /// Auto-sized grid containers in normal flow should size to their grid tracks and can overflow
  /// a definite-height containing block (CSS2.1 §10.6.3).
  ///
  /// FastRender represents this by passing `AvailableSpace::Indefinite` in the block axis for
  /// in-flow children, while still threading a definite percentage base via
  /// `LayoutConstraints::block_percentage_base` so percentage heights can resolve when appropriate.
  ///
  /// Taffy sometimes returns a grid container size equal to the viewport (or other unrelated
  /// percentage base) when the block-axis available space is indefinite, leaving large empty space
  /// inside the grid and vertically centering the in-flow items. This is visible on `bbc.co.uk`
  /// where the hero grid becomes as tall as the viewport.
  ///
  /// When the grid container has `height:auto` (and no min/max-height) and we're in scrollable
  /// layout (no fragmentation), shrink the used block size down to the in-flow content extent and
  /// remove any extra leading space by translating children toward the block-start edge.
  fn maybe_trim_auto_block_size_in_scrollable_layout(
    &self,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
    fragment: &mut FragmentNode,
  ) {
    if fragmentainer_block_size_hint().is_some() {
      return;
    }

    let axes = FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
    let available_block = match axes.block_axis() {
      PhysicalAxis::Y => constraints.available_height,
      PhysicalAxis::X => constraints.available_width,
    };
    if !matches!(available_block, CrateAvailableSpace::Indefinite) {
      return;
    }

    let (block_is_auto, has_used_override, has_block_min, has_block_max) = match axes.block_axis() {
      PhysicalAxis::Y => (
        physical_height_is_auto(style),
        constraints.used_border_box_height.is_some(),
        style.min_height.is_some() || style.min_height_keyword.is_some(),
        style.max_height.is_some() || style.max_height_keyword.is_some(),
      ),
      PhysicalAxis::X => (
        physical_width_is_auto(style),
        constraints.used_border_box_width.is_some(),
        style.min_width.is_some() || style.min_width_keyword.is_some(),
        style.max_width.is_some() || style.max_width_keyword.is_some(),
      ),
    };

    if !block_is_auto || has_used_override || has_block_min || has_block_max {
      return;
    }

    let container_block_size = axes.block_size(&fragment.bounds);
    if !container_block_size.is_finite() || container_block_size <= 0.0 {
      return;
    }

    // Percentages on padding/border resolve against the containing block width (CSS2.1).
    let percentage_base = constraints
      .inline_percentage_base
      .or_else(|| constraints.width())
      .unwrap_or(fragment.bounds.width())
      .max(0.0);
    let padding_left = self.resolve_length_for_width(style.padding_left, percentage_base, style);
    let padding_right = self.resolve_length_for_width(style.padding_right, percentage_base, style);
    let padding_top = self.resolve_length_for_width(style.padding_top, percentage_base, style);
    let padding_bottom =
      self.resolve_length_for_width(style.padding_bottom, percentage_base, style);
    let border_left =
      self.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
    let border_right =
      self.resolve_length_for_width(style.used_border_right_width(), percentage_base, style);
    let border_top =
      self.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
    let border_bottom =
      self.resolve_length_for_width(style.used_border_bottom_width(), percentage_base, style);

    let (block_start_inset, block_end_inset) = match axes.block_axis() {
      PhysicalAxis::Y => (border_top + padding_top, border_bottom + padding_bottom),
      PhysicalAxis::X => {
        if axes.block_positive() {
          (border_left + padding_left, border_right + padding_right)
        } else {
          (border_right + padding_right, border_left + padding_left)
        }
      }
    };

    let mut min_start = f32::INFINITY;
    let mut max_end = f32::NEG_INFINITY;
    // Prefer grid track extents when available so we don't trim away empty explicit tracks.
    //
    // Example: `grid-template-rows: 10px 10px` with a single item in `grid-row: 2 / 3` should
    // preserve the empty first row. Using only in-flow child extents would treat the first row as
    // "unwanted leading space" and incorrectly translate the item to `y=0` while shrinking the
    // grid container to 10px.
    if let Some(tracks) = fragment.grid_tracks.as_deref() {
      let ranges: &[(f32, f32)] = match axes.block_axis() {
        PhysicalAxis::Y => tracks.rows.as_slice(),
        PhysicalAxis::X => tracks.columns.as_slice(),
      };
      for &(start_phys, end_phys) in ranges {
        let (start, end) = if axes.block_positive() {
          (start_phys, end_phys)
        } else {
          (container_block_size - end_phys, container_block_size - start_phys)
        };
        if start.is_finite() && end.is_finite() {
          min_start = min_start.min(start);
          max_end = max_end.max(end);
        }
      }
    }
    for child in fragment.children.iter() {
      match child.content {
        FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. } => continue,
        _ => {}
      }
      let Some(child_style) = child.style.as_deref() else {
        continue;
      };
      if !child_style.position.is_in_flow() {
        continue;
      }
      let start = axes.block_start(&child.bounds, container_block_size);
      let end = start + axes.block_size(&child.bounds);
      if start.is_finite() && end.is_finite() {
        min_start = min_start.min(start);
        max_end = max_end.max(end);
      }
    }

    let (delta, desired_block_size) = if min_start.is_finite() && max_end.is_finite() {
      // Only translate toward block-start (negative delta) to remove unwanted leading free space.
      let delta = (block_start_inset - min_start).min(0.0);
      (delta, (max_end + delta + block_end_inset).max(0.0))
    } else {
      (0.0, (block_start_inset + block_end_inset).max(0.0))
    };

    let eps = 0.5;
    if !(desired_block_size.is_finite() && desired_block_size + eps < container_block_size) {
      return;
    }

    if delta.abs() > 0.01 {
      let offset = axes.block_offset(delta);
      for child in fragment.children_mut().iter_mut() {
        let Some(child_style) = child.style.as_deref() else {
          continue;
        };
        if !child_style.position.is_in_flow() {
          continue;
        }
        child.translate_root_in_place(offset);
      }

      if let Some(tracks) = fragment.grid_tracks.as_mut() {
        let tracks = Arc::make_mut(tracks);
        match axes.block_axis() {
          PhysicalAxis::Y => {
            for range in tracks.rows.iter_mut() {
              range.0 += offset.y;
              range.1 += offset.y;
            }
          }
          PhysicalAxis::X => {
            for range in tracks.columns.iter_mut() {
              range.0 += offset.x;
              range.1 += offset.x;
            }
          }
        }
      }
    }

    match axes.block_axis() {
      PhysicalAxis::Y => {
        fragment.bounds.size.height = desired_block_size;
        if let Some(logical) = fragment.logical_override.as_mut() {
          logical.size.height = desired_block_size;
        }
      }
      PhysicalAxis::X => {
        if !axes.block_positive() {
          fragment.bounds.origin.x =
            fragment.bounds.origin.x + fragment.bounds.size.width - desired_block_size;
        }
        fragment.bounds.size.width = desired_block_size;
        if let Some(logical) = fragment.logical_override.as_mut() {
          if !axes.block_positive() {
            logical.origin.x = logical.origin.x + logical.size.width - desired_block_size;
          }
          logical.size.width = desired_block_size;
        }
      }
    }
  }

  fn resolve_length_px_with_base(
    &self,
    length: Length,
    percentage_base: Option<f32>,
    style: &ComputedStyle,
  ) -> Option<f32> {
    resolve_length_with_percentage_metrics_and_root_font_metrics(
      length,
      percentage_base,
      self.viewport_size,
      style.font_size,
      style.root_font_size,
      Some(style),
      Some(&self.font_context),
      self.factory.root_font_metrics(),
    )
  }

  fn axis_padding_border_px(&self, style: &ComputedStyle, axis: Axis, percentage_base: f32) -> f32 {
    let percentage_base = if percentage_base.is_finite() && percentage_base >= 0.0 {
      percentage_base
    } else {
      0.0
    };

    let resolve = |len: Length| {
      let mut px = self.resolve_length_for_width(len, percentage_base, style);
      if !px.is_finite() {
        px = 0.0;
      }
      px.max(0.0)
    };

    match axis {
      Axis::Horizontal => {
        resolve(style.padding_left)
          + resolve(style.padding_right)
          + resolve(style.used_border_left_width())
          + resolve(style.used_border_right_width())
      }
      Axis::Vertical => {
        resolve(style.padding_top)
          + resolve(style.padding_bottom)
          + resolve(style.used_border_top_width())
          + resolve(style.used_border_bottom_width())
      }
    }
  }

  fn border_box_to_taffy_style_size(
    &self,
    border_box: f32,
    style: &ComputedStyle,
    axis: Axis,
    percentage_base: f32,
  ) -> f32 {
    let border_box = if border_box.is_finite() && border_box >= 0.0 {
      border_box
    } else {
      0.0
    };
    if style.box_sizing == BoxSizing::ContentBox {
      let edges = self.axis_padding_border_px(style, axis, percentage_base);
      (border_box - edges).max(0.0)
    } else {
      border_box
    }
  }

  fn specified_size_for_border_box(
    &self,
    border_box: f32,
    edges: f32,
    box_sizing: BoxSizing,
  ) -> f32 {
    match box_sizing {
      BoxSizing::BorderBox => border_box.max(0.0),
      BoxSizing::ContentBox => (border_box - edges).max(0.0),
    }
  }

  fn intrinsic_keyword_size_from_min_max(
    &self,
    keyword: IntrinsicSizeKeyword,
    min: f32,
    max: f32,
    available: Option<f32>,
    style: &ComputedStyle,
    edges: f32,
  ) -> f32 {
    match keyword {
      IntrinsicSizeKeyword::MinContent => min,
      IntrinsicSizeKeyword::MaxContent => max,
      IntrinsicSizeKeyword::FillAvailable => {
        available.filter(|v| v.is_finite()).unwrap_or(max).max(0.0)
      }
      IntrinsicSizeKeyword::FitContent { limit } => {
        let preferred = limit
          .and_then(|limit| self.resolve_length_px_with_base(limit, available, style))
          .map(|px| border_size_from_box_sizing(px, edges, style.box_sizing));
        crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
          available, preferred, min, max,
        )
      }
      IntrinsicSizeKeyword::CalcSize(calc) => {
        use crate::style::types::BoxSizing;
        use crate::style::types::CalcSizeBasis;
        let basis_border = match calc.basis {
          CalcSizeBasis::Auto => available.filter(|v| v.is_finite()).unwrap_or(max).max(0.0),
          CalcSizeBasis::MinContent => min,
          CalcSizeBasis::MaxContent => max,
          CalcSizeBasis::FillAvailable => {
            available.filter(|v| v.is_finite()).unwrap_or(max).max(0.0)
          }
          CalcSizeBasis::FitContent { limit } => {
            let preferred = limit
              .and_then(|limit| self.resolve_length_px_with_base(limit, available, style))
              .map(|px| border_size_from_box_sizing(px, edges, style.box_sizing));
            crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
              available, preferred, min, max,
            )
          }
          CalcSizeBasis::Length(len) => self
            .resolve_length_px_with_base(len, available, style)
            .map(|px| border_size_from_box_sizing(px, edges, style.box_sizing))
            .unwrap_or(max),
        }
        .max(0.0);
        let basis_content = (basis_border - edges).max(0.0);
        let basis_specified = match style.box_sizing {
          BoxSizing::ContentBox => basis_content,
          BoxSizing::BorderBox => basis_border,
        };
        crate::style::values::calc_size_expr_with_size(calc.expr, basis_specified)
          .and_then(|expr_sum| crate::css::properties::parse_length(&format!("calc({expr_sum})")))
          .and_then(|expr_len| self.resolve_length_px_with_base(expr_len, available, style))
          .map(|px| border_size_from_box_sizing(px, edges, style.box_sizing).max(0.0))
          .unwrap_or(basis_border)
      }
    }
  }

  fn intrinsic_min_max_for_physical_axis(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Result<(f32, f32), LayoutError> {
    let fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let fc = self.factory.get(fc_type);
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let axis_is_inline = match axis {
      Axis::Horizontal => inline_is_horizontal,
      Axis::Vertical => !inline_is_horizontal,
    };

    if axis_is_inline {
      match fc.compute_intrinsic_inline_sizes(box_node) {
        Ok((min, max)) => Ok((min.max(0.0), max.max(0.0))),
        Err(err @ LayoutError::Timeout { .. }) => Err(err),
        Err(_) => Ok((0.0, 0.0)),
      }
    } else {
      // When resolving intrinsic sizing keywords we need the box's *content-driven* sizes, which
      // must not depend on the box's own intrinsic keyword constraints along the same physical
      // axis. In vertical writing modes, `width: max-content` maps to the block axis; querying the
      // block intrinsic size without clearing that keyword would re-enter grid layout and recurse
      // indefinitely (stack overflow).
      //
      // Clear any intrinsic keyword sizing on the physical axis we're measuring so the nested
      // layout pass treats it as `auto` instead of trying to resolve the same keyword again.
      let needs_keyword_clear = match axis {
        Axis::Horizontal => {
          style.width_keyword.is_some()
            || style.min_width_keyword.is_some()
            || style.max_width_keyword.is_some()
        }
        Axis::Vertical => {
          style.height_keyword.is_some()
            || style.min_height_keyword.is_some()
            || style.max_height_keyword.is_some()
        }
      };

      let compute = |mode| -> Result<f32, LayoutError> {
        if needs_keyword_clear && box_node.id != 0 {
          let mut cleared: ComputedStyle = style.clone();
          match axis {
            Axis::Horizontal => {
              cleared.width = None;
              cleared.width_keyword = None;
              cleared.min_width_keyword = None;
              cleared.max_width_keyword = None;
            }
            Axis::Vertical => {
              cleared.height = None;
              cleared.height_keyword = None;
              cleared.min_height_keyword = None;
              cleared.max_height_keyword = None;
            }
          }
          crate::layout::style_override::with_style_override(box_node.id, Arc::new(cleared), || {
            fc.compute_intrinsic_block_size(box_node, mode)
          })
        } else {
          fc.compute_intrinsic_block_size(box_node, mode)
        }
      };

      let min = match compute(IntrinsicSizingMode::MinContent) {
        Ok(value) => value.max(0.0),
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => 0.0,
      };
      let max = match compute(IntrinsicSizingMode::MaxContent) {
        Ok(value) => value.max(0.0),
        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
        Err(_) => min,
      };
      Ok((min, max))
    }
  }

  fn definite_physical_available_size(
    &self,
    constraints: &LayoutConstraints,
    axis: Axis,
  ) -> Option<f32> {
    let space = match axis {
      Axis::Horizontal => constraints.available_width,
      Axis::Vertical => constraints.available_height,
    };
    match space {
      CrateAvailableSpace::Definite(value) if value.is_finite() && value >= 0.0 => Some(value),
      _ => None,
    }
  }

  fn resolve_intrinsic_sizing_keywords_for_node(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    available_width: Option<f32>,
    available_height: Option<f32>,
    resolve_fit_content: bool,
  ) -> Result<Option<Arc<ComputedStyle>>, LayoutError> {
    if box_node.id == 0 {
      return Ok(None);
    }

    // Resolving intrinsic sizing keywords (`min/max/fit-content`) requires querying the intrinsic
    // size of *this same node*. Those intrinsic size queries can re-enter grid layout, which in
    // turn will attempt to resolve intrinsic keywords again, causing infinite recursion
    // (manifesting as a stack overflow).
    //
    // Avoid this by clearing any intrinsic sizing keywords on the axis being measured while we
    // compute the node's intrinsic min/max sizes. We still apply the authored keyword constraints
    // after measuring by clamping against the computed intrinsic min/max values.
    let cleared_style_for_axis = |axis: Axis| -> Option<Arc<ComputedStyle>> {
      let mut next_style: ComputedStyle = style.clone();
      let mut changed = false;
      match axis {
        Axis::Horizontal => {
          if next_style.width_keyword.is_some() {
            next_style.width = None;
            next_style.width_keyword = None;
            changed = true;
          }
          if next_style.min_width_keyword.is_some() {
            next_style.min_width = None;
            next_style.min_width_keyword = None;
            changed = true;
          }
          if next_style.max_width_keyword.is_some() {
            next_style.max_width = None;
            next_style.max_width_keyword = None;
            changed = true;
          }
        }
        Axis::Vertical => {
          if next_style.height_keyword.is_some() {
            next_style.height = None;
            next_style.height_keyword = None;
            changed = true;
          }
          if next_style.min_height_keyword.is_some() {
            next_style.min_height = None;
            next_style.min_height_keyword = None;
            changed = true;
          }
          if next_style.max_height_keyword.is_some() {
            next_style.max_height = None;
            next_style.max_height_keyword = None;
            changed = true;
          }
        }
      }
      changed.then(|| Arc::new(next_style))
    };

    let should_resolve_keyword = |keyword: IntrinsicSizeKeyword| match keyword {
      IntrinsicSizeKeyword::FitContent { .. } => resolve_fit_content,
      _ => true,
    };

    // Block-axis intrinsic sizing keywords on grid items (e.g. `min-height: min-content`) depend on
    // the item's *used* inline size (text wrapping, percentage padding/borders, etc.). That inline
    // size is determined by grid track sizing and is therefore not known when pre-resolving sizing
    // keywords prior to running Taffy.
    //
    // Avoid resolving intrinsic sizing keywords on the physical block axis for non-root nodes.
    // (Root nodes have a definite inline size from parent constraints.)
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let resolve_horizontal_axis = inline_is_horizontal || resolve_fit_content;
    let resolve_vertical_axis = !inline_is_horizontal || resolve_fit_content;

    let has_horizontal_keyword = resolve_horizontal_axis
      && (style
        .width_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
        || style
          .min_width_keyword
          .is_some_and(|kw| should_resolve_keyword(kw))
        || style
          .max_width_keyword
          .is_some_and(|kw| should_resolve_keyword(kw)));
    let has_vertical_keyword = resolve_vertical_axis
      && (style
        .height_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
        || style
          .min_height_keyword
          .is_some_and(|kw| should_resolve_keyword(kw))
        || style
          .max_height_keyword
          .is_some_and(|kw| should_resolve_keyword(kw)));
    if !(has_horizontal_keyword || has_vertical_keyword) {
      return Ok(None);
    }

    let horizontal_edges = self.edges_px(style, Axis::Horizontal).unwrap_or(0.0);
    let vertical_edges = self.edges_px(style, Axis::Vertical).unwrap_or(0.0);

    let (intrinsic_min_w, intrinsic_max_w) = if has_horizontal_keyword {
      if let Some(cleared) = cleared_style_for_axis(Axis::Horizontal) {
        crate::layout::style_override::with_style_override(box_node.id, cleared.clone(), || {
          self.intrinsic_min_max_for_physical_axis(box_node, cleared.as_ref(), Axis::Horizontal)
        })?
      } else {
        self.intrinsic_min_max_for_physical_axis(box_node, style, Axis::Horizontal)?
      }
    } else {
      (0.0, 0.0)
    };
    let (intrinsic_min_h, intrinsic_max_h) = if has_vertical_keyword {
      if let Some(cleared) = cleared_style_for_axis(Axis::Vertical) {
        crate::layout::style_override::with_style_override(box_node.id, cleared.clone(), || {
          self.intrinsic_min_max_for_physical_axis(box_node, cleared.as_ref(), Axis::Vertical)
        })?
      } else {
        self.intrinsic_min_max_for_physical_axis(box_node, style, Axis::Vertical)?
      }
    } else {
      (0.0, 0.0)
    };

    let mut min_width_border_box = if resolve_horizontal_axis {
      if let Some(keyword) = style
        .min_width_keyword
        .filter(|kw| should_resolve_keyword(*kw))
      {
        self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_w,
          intrinsic_max_w,
          available_width,
          style,
          horizontal_edges,
        )
      } else if let Some(length) = style.min_width {
        self
          .resolve_length_px_with_base(length, available_width, style)
          .map(|px| border_size_from_box_sizing(px, horizontal_edges, style.box_sizing))
          .unwrap_or(0.0)
      } else {
        0.0
      }
    } else {
      0.0
    };
    let mut max_width_border_box = if resolve_horizontal_axis {
      if let Some(keyword) = style
        .max_width_keyword
        .filter(|kw| should_resolve_keyword(*kw))
      {
        self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_w,
          intrinsic_max_w,
          available_width,
          style,
          horizontal_edges,
        )
      } else if let Some(length) = style.max_width {
        self
          .resolve_length_px_with_base(length, available_width, style)
          .map(|px| border_size_from_box_sizing(px, horizontal_edges, style.box_sizing))
          .unwrap_or(f32::INFINITY)
      } else {
        f32::INFINITY
      }
    } else {
      f32::INFINITY
    };
    if max_width_border_box < min_width_border_box {
      max_width_border_box = min_width_border_box;
    }
    if !min_width_border_box.is_finite() {
      min_width_border_box = 0.0;
    }
    if !max_width_border_box.is_finite() {
      max_width_border_box = f32::INFINITY;
    }

    let mut min_height_border_box = if resolve_vertical_axis {
      if let Some(keyword) = style
        .min_height_keyword
        .filter(|kw| should_resolve_keyword(*kw))
      {
        self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_h,
          intrinsic_max_h,
          available_height,
          style,
          vertical_edges,
        )
      } else if let Some(length) = style.min_height {
        self
          .resolve_length_px_with_base(length, available_height, style)
          .map(|px| border_size_from_box_sizing(px, vertical_edges, style.box_sizing))
          .unwrap_or(0.0)
      } else {
        0.0
      }
    } else {
      0.0
    };
    let mut max_height_border_box = if resolve_vertical_axis {
      if let Some(keyword) = style
        .max_height_keyword
        .filter(|kw| should_resolve_keyword(*kw))
      {
        self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_h,
          intrinsic_max_h,
          available_height,
          style,
          vertical_edges,
        )
      } else if let Some(length) = style.max_height {
        self
          .resolve_length_px_with_base(length, available_height, style)
          .map(|px| border_size_from_box_sizing(px, vertical_edges, style.box_sizing))
          .unwrap_or(f32::INFINITY)
      } else {
        f32::INFINITY
      }
    } else {
      f32::INFINITY
    };
    if max_height_border_box < min_height_border_box {
      max_height_border_box = min_height_border_box;
    }
    if !min_height_border_box.is_finite() {
      min_height_border_box = 0.0;
    }
    if !max_height_border_box.is_finite() {
      max_height_border_box = f32::INFINITY;
    }

    let mut next_style: ComputedStyle = style.clone();
    let mut changed = false;

    if resolve_horizontal_axis
      && style
        .min_width_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified = self.specified_size_for_border_box(
        min_width_border_box,
        horizontal_edges,
        style.box_sizing,
      );
      next_style.min_width = Some(Length::px(specified));
      next_style.min_width_keyword = None;
      changed = true;
    }
    if resolve_horizontal_axis
      && style
        .max_width_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified = self.specified_size_for_border_box(
        max_width_border_box,
        horizontal_edges,
        style.box_sizing,
      );
      next_style.max_width = Some(Length::px(specified));
      next_style.max_width_keyword = None;
      changed = true;
    }
    if resolve_horizontal_axis {
      if let Some(keyword) = style.width_keyword.filter(|kw| should_resolve_keyword(*kw)) {
        let base = self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_w,
          intrinsic_max_w,
          available_width,
          style,
          horizontal_edges,
        );
        let border_box = clamp_with_order(base, min_width_border_box, max_width_border_box);
        let specified =
          self.specified_size_for_border_box(border_box, horizontal_edges, style.box_sizing);
        next_style.width = Some(Length::px(specified));
        next_style.width_keyword = None;
        changed = true;
      }
    }

    if resolve_vertical_axis
      && style
        .min_height_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified =
        self.specified_size_for_border_box(min_height_border_box, vertical_edges, style.box_sizing);
      next_style.min_height = Some(Length::px(specified));
      next_style.min_height_keyword = None;
      changed = true;
    }
    if resolve_vertical_axis
      && style
        .max_height_keyword
        .is_some_and(|kw| should_resolve_keyword(kw))
    {
      let specified =
        self.specified_size_for_border_box(max_height_border_box, vertical_edges, style.box_sizing);
      next_style.max_height = Some(Length::px(specified));
      next_style.max_height_keyword = None;
      changed = true;
    }
    if resolve_vertical_axis {
      if let Some(keyword) = style
        .height_keyword
        .filter(|kw| should_resolve_keyword(*kw))
      {
        let base = self.intrinsic_keyword_size_from_min_max(
          keyword,
          intrinsic_min_h,
          intrinsic_max_h,
          available_height,
          style,
          vertical_edges,
        );
        let border_box = clamp_with_order(base, min_height_border_box, max_height_border_box);
        let specified =
          self.specified_size_for_border_box(border_box, vertical_edges, style.box_sizing);
        next_style.height = Some(Length::px(specified));
        next_style.height_keyword = None;
        changed = true;
      }
    }

    if changed {
      Ok(Some(Arc::new(next_style)))
    } else {
      Ok(None)
    }
  }

  fn resolve_intrinsic_sizing_keywords_for_taffy_tree(
    &self,
    box_node: &BoxNode,
    root_children: &[&BoxNode],
    constraints: &LayoutConstraints,
    resolve_root: bool,
  ) -> Result<StyleOverrideStack, LayoutError> {
    let available_width = self.definite_physical_available_size(constraints, Axis::Horizontal);
    let available_height = self.definite_physical_available_size(constraints, Axis::Vertical);

    let mut stack = StyleOverrideStack::new();
    let mut deadline_counter = 0usize;
    self.resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
      box_node,
      Some(root_children),
      true,
      resolve_root,
      available_width,
      available_height,
      &mut deadline_counter,
      &mut stack,
    )?;
    Ok(stack)
  }

  #[allow(clippy::too_many_arguments)]
  fn resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
    &self,
    box_node: &BoxNode,
    children_override: Option<&[&BoxNode]>,
    is_root: bool,
    resolve_this_node: bool,
    available_width: Option<f32>,
    available_height: Option<f32>,
    deadline_counter: &mut usize,
    stack: &mut StyleOverrideStack,
  ) -> Result<(), LayoutError> {
    check_layout_deadline(deadline_counter)?;

    let style_override = style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    let is_grid_container = matches!(
      box_node.formatting_context(),
      Some(FormattingContextType::Grid)
    );

    let mut include_children = is_root;
    let mut children_iter: Vec<&BoxNode> = Vec::new();
    let mut has_subgrid_child = false;

    if is_grid_container {
      let children_source: Vec<&BoxNode> = match children_override {
        Some(children) => children.to_vec(),
        None => box_node.children.iter().collect(),
      };
      for child in children_source {
        check_layout_deadline(deadline_counter)?;
        match child.style.position {
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
            continue;
          }
          _ => {
            if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
              has_subgrid_child = true;
            }
            children_iter.push(child);
          }
        }
      }

      let is_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;
      include_children |= is_subgrid || has_subgrid_child;
    } else if is_root {
      children_iter = match children_override {
        Some(children) => children.to_vec(),
        None => box_node.children.iter().collect(),
      };
    }

    if include_children {
      for child in children_iter.iter() {
        self.resolve_intrinsic_sizing_keywords_for_taffy_tree_inner(
          child,
          None,
          false,
          true,
          available_width,
          available_height,
          deadline_counter,
          stack,
        )?;
      }
    }

    if resolve_this_node {
      if let Some(resolved) = self.resolve_intrinsic_sizing_keywords_for_node(
        box_node,
        style,
        available_width,
        available_height,
        is_root,
      )? {
        stack.push(push_style_override(box_node.id, resolved));
      }
    }

    Ok(())
  }

  fn resolved_padding_border_for_measure(
    &self,
    style: &ComputedStyle,
    percentage_base: f32,
  ) -> (f32, f32, f32, f32, f32, f32, f32, f32) {
    #[inline]
    fn length_is_zero(length: Length) -> bool {
      length.calc.is_none() && length.value == 0.0
    }

    let reserve_vertical_gutter = style.scrollbar_gutter.stable
      && matches!(
        style.overflow_y,
        CssOverflow::Hidden | CssOverflow::Auto | CssOverflow::Scroll
      );
    let reserve_horizontal_gutter = style.scrollbar_gutter.stable
      && matches!(
        style.overflow_x,
        CssOverflow::Hidden | CssOverflow::Auto | CssOverflow::Scroll
      );
    let gutter_width = if reserve_vertical_gutter || reserve_horizontal_gutter {
      resolve_scrollbar_width(style)
    } else {
      0.0
    };
    let border_left_len = style.used_border_left_width();
    let border_right_len = style.used_border_right_width();
    let border_top_len = style.used_border_top_width();
    let border_bottom_len = style.used_border_bottom_width();

    // Fast-path the common case (no padding/border/gutters) to avoid repeated length resolution
    // work in tight grid measurement loops.
    if gutter_width == 0.0
      && length_is_zero(style.padding_left)
      && length_is_zero(style.padding_right)
      && length_is_zero(style.padding_top)
      && length_is_zero(style.padding_bottom)
      && length_is_zero(border_left_len)
      && length_is_zero(border_right_len)
      && length_is_zero(border_top_len)
      && length_is_zero(border_bottom_len)
    {
      return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    }

    let resolve = |length: Length| {
      if length_is_zero(length) {
        0.0
      } else {
        self.resolve_length_for_width(length, percentage_base, style)
      }
    };

    let mut padding_left = resolve(style.padding_left);
    let mut padding_right = resolve(style.padding_right);
    let mut padding_top = resolve(style.padding_top);
    let mut padding_bottom = resolve(style.padding_bottom);

    // Reserve space for scrollbars when overflow/scrollbar-gutter request stable gutters. This
    // mirrors the block formatting context behaviour so grid measurement returns the true content
    // box size (excluding scrollbar gutters).
    if reserve_vertical_gutter && gutter_width > 0.0 {
      if style.scrollbar_gutter.both_edges {
        padding_left += gutter_width;
        padding_right += gutter_width;
      } else {
        padding_right += gutter_width;
      }
    }

    if reserve_horizontal_gutter && gutter_width > 0.0 {
      if style.scrollbar_gutter.both_edges {
        padding_top += gutter_width;
      }
      padding_bottom += gutter_width;
    }

    let border_left = resolve(border_left_len);
    let border_right = resolve(border_right_len);
    let border_top = resolve(border_top_len);
    let border_bottom = resolve(border_bottom_len);

    (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    )
  }

  fn content_box_size(
    &self,
    fragment: &FragmentNode,
    style: &ComputedStyle,
    percentage_base: f32,
  ) -> Size {
    let (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    ) = self.resolved_padding_border_for_measure(style, percentage_base);

    let content_width =
      (fragment.bounds.width() - padding_left - padding_right - border_left - border_right)
        .max(0.0);
    let content_height =
      (fragment.bounds.height() - padding_top - padding_bottom - border_top - border_bottom)
        .max(0.0);

    Size::new(content_width, content_height)
  }

  #[inline]
  fn resolve_taffy_length_percentage_px(value: LengthPercentage, percentage_base: f32) -> f32 {
    let percentage_base = if percentage_base.is_finite() && percentage_base >= 0.0 {
      percentage_base
    } else {
      0.0
    };
    let raw = value.into_raw();
    let resolved = match raw.tag() {
      CompactLength::LENGTH_TAG => raw.value(),
      CompactLength::PERCENT_TAG => raw.value() * percentage_base,
      _ => 0.0,
    };
    if resolved.is_finite() && resolved >= 0.0 {
      resolved
    } else {
      0.0
    }
  }

  #[inline]
  fn taffy_measure_insets_px(taffy_style: &TaffyStyle, percentage_base: f32) -> (f32, f32) {
    let resolve =
      |value: LengthPercentage| Self::resolve_taffy_length_percentage_px(value, percentage_base);

    let padding_left = resolve(taffy_style.padding.left);
    let padding_right = resolve(taffy_style.padding.right);
    let padding_top = resolve(taffy_style.padding.top);
    let padding_bottom = resolve(taffy_style.padding.bottom);
    let border_left = resolve(taffy_style.border.left);
    let border_right = resolve(taffy_style.border.right);
    let border_top = resolve(taffy_style.border.top);
    let border_bottom = resolve(taffy_style.border.bottom);

    let scrollbar_width =
      if taffy_style.scrollbar_width.is_finite() && taffy_style.scrollbar_width > 0.0 {
        taffy_style.scrollbar_width
      } else {
        0.0
      };
    let right_gutter = if taffy_style.overflow.y == TaffyOverflow::Scroll {
      scrollbar_width
    } else {
      0.0
    };
    let bottom_gutter = if taffy_style.overflow.x == TaffyOverflow::Scroll {
      scrollbar_width
    } else {
      0.0
    };

    let inset_w = padding_left + padding_right + border_left + border_right + right_gutter;
    let inset_h = padding_top + padding_bottom + border_top + border_bottom + bottom_gutter;
    (
      if inset_w.is_finite() {
        inset_w.max(0.0)
      } else {
        0.0
      },
      if inset_h.is_finite() {
        inset_h.max(0.0)
      } else {
        0.0
      },
    )
  }

  #[inline]
  fn content_box_size_for_taffy_style(
    border_box_size: Size,
    taffy_style: &TaffyStyle,
    percentage_base: f32,
  ) -> Size {
    let border_w = if border_box_size.width.is_finite() && border_box_size.width >= 0.0 {
      border_box_size.width
    } else {
      0.0
    };
    let border_h = if border_box_size.height.is_finite() && border_box_size.height >= 0.0 {
      border_box_size.height
    } else {
      0.0
    };
    let (inset_w, inset_h) = Self::taffy_measure_insets_px(taffy_style, percentage_base);
    let width = (border_w - inset_w).max(0.0);
    let height = (border_h - inset_h).max(0.0);
    Size::new(width, height)
  }

  fn resolve_root_fit_content_border_box_size(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
    axis: Axis,
    limit: Option<Length>,
  ) -> Result<Option<f32>, LayoutError> {
    let avail = match axis {
      Axis::Horizontal => constraints.available_width,
      Axis::Vertical => constraints.available_height,
    };
    let physical_axis = match axis {
      Axis::Horizontal => PhysicalAxis::X,
      Axis::Vertical => PhysicalAxis::Y,
    };

    let width_base = constraints
      .width()
      .or(constraints.inline_percentage_base)
      .unwrap_or(self.viewport_size.width.max(0.0));
    let (
      padding_left,
      padding_right,
      padding_top,
      padding_bottom,
      border_left,
      border_right,
      border_top,
      border_bottom,
    ) = self.resolved_padding_border_for_measure(style, width_base);
    let edges_w = padding_left + padding_right + border_left + border_right;
    let edges_h = padding_top + padding_bottom + border_top + border_bottom;
    let axis_edges = match axis {
      Axis::Horizontal => edges_w,
      Axis::Vertical => edges_h,
    };

    let intrinsic_range = |node: &BoxNode| -> Result<(f32, f32), LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
        self,
        node,
        physical_axis,
      )
    };

    // Avoid self-recursion when intrinsic sizing needs to re-enter layout by clearing any
    // fit-content keywords on the container before measuring its intrinsic contributions.
    let mut cleared_style: ComputedStyle = (*style).clone();
    if matches!(
      cleared_style.width_keyword,
      Some(IntrinsicSizeKeyword::FitContent { .. })
    ) {
      cleared_style.width = None;
      cleared_style.width_keyword = None;
    }
    if matches!(
      cleared_style.height_keyword,
      Some(IntrinsicSizeKeyword::FitContent { .. })
    ) {
      cleared_style.height = None;
      cleared_style.height_keyword = None;
    }
    let cleared_style = Arc::new(cleared_style);
    let (min_intrinsic, max_intrinsic) = if box_node.id != 0 {
      crate::layout::style_override::with_style_override(box_node.id, cleared_style, || {
        intrinsic_range(box_node)
      })
    } else {
      let mut cloned = box_node.clone();
      cloned.style = cleared_style;
      intrinsic_range(&cloned)
    }?;

    let min_intrinsic = min_intrinsic.max(0.0);
    let max_intrinsic = max_intrinsic.max(0.0);
    let available_border_box = match avail {
      CrateAvailableSpace::Definite(v) => v.max(0.0),
      CrateAvailableSpace::MinContent => min_intrinsic,
      CrateAvailableSpace::MaxContent => max_intrinsic,
      CrateAvailableSpace::Indefinite => max_intrinsic,
    };

    let percentage_base_opt = match axis {
      Axis::Horizontal => constraints.width().or(constraints.inline_percentage_base),
      Axis::Vertical => constraints.height(),
    };
    let resolve_length_px = |len: Length| -> Option<f32> {
      if len.has_percentage() && percentage_base_opt.is_none() {
        return None;
      }
      let base = match axis {
        Axis::Horizontal => percentage_base_opt.unwrap_or(self.viewport_size.width.max(0.0)),
        Axis::Vertical => percentage_base_opt.unwrap_or(self.viewport_size.height.max(0.0)),
      };
      Some(self.resolve_length_for_width(len, base, style).max(0.0))
    };
    let to_border_box = |value: f32| -> f32 {
      if style.box_sizing == BoxSizing::ContentBox {
        (value + axis_edges).max(0.0)
      } else {
        value.max(0.0)
      }
    };

    let preferred_border_box = match limit {
      None => None,
      Some(limit) => {
        let Some(resolved) = resolve_length_px(limit) else {
          return Ok(None);
        };
        Some(to_border_box(resolved))
      }
    };

    let mut border_box = crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
      Some(available_border_box),
      preferred_border_box,
      min_intrinsic,
      max_intrinsic,
    );

    // Apply authored min/max constraints on the axis, including intrinsic keyword constraints.
    let (author_min_len, author_max_len, author_min_kw, author_max_kw) = match axis {
      Axis::Horizontal => (
        style.min_width,
        style.max_width,
        style.min_width_keyword,
        style.max_width_keyword,
      ),
      Axis::Vertical => (
        style.min_height,
        style.max_height,
        style.min_height_keyword,
        style.max_height_keyword,
      ),
    };
    let keyword_to_bound = |kw: IntrinsicSizeKeyword| -> Option<f32> {
      match kw {
        IntrinsicSizeKeyword::MinContent => Some(min_intrinsic),
        IntrinsicSizeKeyword::MaxContent => Some(max_intrinsic),
        IntrinsicSizeKeyword::FillAvailable => None,
        IntrinsicSizeKeyword::FitContent { .. } => None,
        IntrinsicSizeKeyword::CalcSize(_) => None,
      }
    };
    let author_min = author_min_kw.and_then(keyword_to_bound).or_else(|| {
      author_min_len
        .and_then(resolve_length_px)
        .map(to_border_box)
    });
    let author_max = author_max_kw.and_then(keyword_to_bound).or_else(|| {
      author_max_len
        .and_then(resolve_length_px)
        .map(to_border_box)
    });
    if author_min.is_some() || author_max.is_some() {
      let min_bound = author_min.unwrap_or(0.0);
      let mut max_bound = author_max.unwrap_or(f32::INFINITY);
      if max_bound < min_bound {
        max_bound = min_bound;
      }
      border_box = crate::layout::utils::clamp_with_order(border_box, min_bound, max_bound);
    }

    Ok(border_box.is_finite().then(|| border_box.max(0.0)))
  }

  fn apply_grid_intrinsic_size_keywords(
    &self,
    box_node: &BoxNode,
    is_root: bool,
    taffy_style: &mut TaffyStyle,
  ) -> Result<(), LayoutError> {
    if is_root {
      return Ok(());
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    // As with `resolve_intrinsic_sizing_keywords_for_node`, avoid resolving intrinsic sizing
    // keywords on the physical block axis for non-root nodes. The used inline size for grid items is
    // determined during track sizing, so resolving block-axis keywords here (before Taffy runs) can
    // wildly mis-estimate sizes due to text wrapping at the element's min-content inline size.
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let resolve_width_axis = inline_is_horizontal;
    let resolve_height_axis = !inline_is_horizontal;
    let keyword_to_mode = |kw: IntrinsicSizeKeyword| match kw {
      IntrinsicSizeKeyword::MinContent => Some(IntrinsicSizingMode::MinContent),
      IntrinsicSizeKeyword::MaxContent => Some(IntrinsicSizingMode::MaxContent),
      IntrinsicSizeKeyword::FillAvailable => None,
      IntrinsicSizeKeyword::FitContent { .. } => None,
      IntrinsicSizeKeyword::CalcSize(_) => None,
    };
    let width_has_fit_content_max_constraint = style.max_width.is_none()
      && matches!(
        style.max_width_keyword,
        Some(IntrinsicSizeKeyword::FitContent { .. })
      );
    let height_has_fit_content_max_constraint = style.max_height.is_none()
      && matches!(
        style.max_height_keyword,
        Some(IntrinsicSizeKeyword::FitContent { .. })
      );

    let has_intrinsic_keyword = (resolve_width_axis
      && (style.width_keyword.and_then(keyword_to_mode).is_some()
        || style.min_width_keyword.and_then(keyword_to_mode).is_some()
        || style.max_width_keyword.and_then(keyword_to_mode).is_some()))
      || (resolve_height_axis
        && (style.height_keyword.and_then(keyword_to_mode).is_some()
          || style.min_height_keyword.and_then(keyword_to_mode).is_some()
          || style.max_height_keyword.and_then(keyword_to_mode).is_some()));
    if !has_intrinsic_keyword {
      return Ok(());
    }

    let horizontal_edges = self.edges_px(style, Axis::Horizontal).unwrap_or(0.0);
    let vertical_edges = self.edges_px(style, Axis::Vertical).unwrap_or(0.0);
    let to_taffy_size = |border_box: f32, axis: Axis| {
      let edges = match axis {
        Axis::Horizontal => horizontal_edges,
        Axis::Vertical => vertical_edges,
      };
      self.specified_size_for_border_box(border_box, edges, style.box_sizing)
    };

    let item_fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let item_fc = self.factory.get(item_fc_type);
    let box_id = box_node.id();

    // When computing intrinsic sizes for an axis that is itself specified as an intrinsic keyword,
    // clear that size property to avoid self-recursion.
    let width_override = style.width_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*style).clone();
      override_style.width = None;
      override_style.width_keyword = None;
      Arc::new(override_style)
    });
    let height_override = style.height_keyword.is_some().then(|| {
      let mut override_style: ComputedStyle = (*style).clone();
      override_style.height = None;
      override_style.height_keyword = None;
      Arc::new(override_style)
    });

    let run_with_override = |override_style: Option<Arc<ComputedStyle>>,
                             f: &dyn Fn(&BoxNode) -> Result<f32, LayoutError>|
     -> Result<f32, LayoutError> {
      if let Some(style_override) = override_style {
        if box_id != 0 {
          crate::layout::style_override::with_style_override(box_id, style_override, || f(box_node))
        } else {
          let mut cloned = box_node.clone();
          cloned.style = style_override;
          f(&cloned)
        }
      } else {
        f(box_node)
      }
    };

    let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      run_with_override(width_override.clone(), &|node| {
        crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
          item_fc.as_ref(),
          node,
          PhysicalAxis::X,
          mode,
        )
      })
    };

    let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      run_with_override(height_override.clone(), &|node| {
        crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
          item_fc.as_ref(),
          node,
          PhysicalAxis::Y,
          mode,
        )
      })
    };

    if resolve_width_axis {
      if let Some(mode) = style.width_keyword.and_then(keyword_to_mode) {
        // `max-width: fit-content(...)` must be resolved using the available grid area size. Since
        // the Taffy template cache can't represent this dependency, keep the preferred size as `auto`
        // and let the measure callback compute the final used size for this axis.
        if !width_has_fit_content_max_constraint {
          match intrinsic_physical_width(mode) {
            Ok(border_box) => {
              if border_box.is_finite() {
                taffy_style.size.width =
                  Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Horizontal));
              }
            }
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {}
          }
        }
      }

      if let Some(mode) = style.min_width_keyword.and_then(keyword_to_mode) {
        match intrinsic_physical_width(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.min_size.width =
                Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Horizontal));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }

      if let Some(mode) = style.max_width_keyword.and_then(keyword_to_mode) {
        match intrinsic_physical_width(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.max_size.width =
                Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Horizontal));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
    }

    if resolve_height_axis {
      if let Some(mode) = style.height_keyword.and_then(keyword_to_mode) {
        if !height_has_fit_content_max_constraint {
          match intrinsic_physical_height(mode) {
            Ok(border_box) => {
              if border_box.is_finite() {
                taffy_style.size.height =
                  Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Vertical));
              }
            }
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {}
          }
        }
      }

      if let Some(mode) = style.min_height_keyword.and_then(keyword_to_mode) {
        match intrinsic_physical_height(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.min_size.height =
                Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Vertical));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }

      if let Some(mode) = style.max_height_keyword.and_then(keyword_to_mode) {
        match intrinsic_physical_height(mode) {
          Ok(border_box) => {
            if border_box.is_finite() {
              taffy_style.max_size.height =
                Dimension::length(to_taffy_size(border_box.max(0.0), Axis::Vertical));
            }
          }
          Err(err @ LayoutError::Timeout { .. }) => return Err(err),
          Err(_) => {}
        }
      }
    }

    Ok(())
  }

  fn content_visibility_auto_has_definite_placeholder(&self, node: &BoxNode) -> bool {
    let style = node.style.as_ref();
    let axis_is_width = crate::style::block_axis_is_horizontal(style.writing_mode);
    let explicit = if axis_is_width {
      style.width.as_ref()
    } else {
      style.height.as_ref()
    };
    if explicit
      .and_then(|l| self.resolve_length_px(l, style))
      .is_some()
    {
      return true;
    }

    let axis = if axis_is_width {
      style.contain_intrinsic_width
    } else {
      style.contain_intrinsic_height
    };
    if axis
      .length
      .as_ref()
      .and_then(|l| self.resolve_length_px(l, style))
      .is_some()
    {
      return true;
    }

    if axis.auto {
      if let Some(size) = remembered_size_cache_lookup(node) {
        let value = if axis_is_width {
          size.width
        } else {
          size.height
        };
        if value.is_finite() {
          return true;
        }
      }
    }

    false
  }

  fn content_visibility_placeholder_content_size(
    &self,
    node: &BoxNode,
    constraints: &LayoutConstraints,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
  ) -> Size {
    let style = node.style.as_ref();
    let sanitize = |value: f32| {
      if value.is_finite() && value >= 0.0 {
        value
      } else {
        0.0
      }
    };

    let remembered = remembered_size_cache_lookup(node);
    let remembered_width = remembered.map(|size| size.width).filter(|v| v.is_finite());
    let remembered_height = remembered.map(|size| size.height).filter(|v| v.is_finite());

    let width = known_dimensions.width.unwrap_or_else(|| {
      let base = constraints
        .inline_percentage_base
        .or_else(|| constraints.width())
        .filter(|b| b.is_finite());
      crate::layout::utils::resolve_contain_intrinsic_size_axis(
        style.contain_intrinsic_width,
        remembered_width,
        base,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      )
    });

    let height = known_dimensions.height.unwrap_or_else(|| {
      let base = constraints.height().filter(|b| b.is_finite());
      crate::layout::utils::resolve_contain_intrinsic_size_axis(
        style.contain_intrinsic_height,
        remembered_height,
        base,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      )
    });

    Size::new(sanitize(width), sanitize(height))
  }

  /// Builds a Taffy tree from a BoxNode tree
  ///
  /// Recursively converts the BoxNode tree to Taffy nodes, returning
  /// the root node ID and a mapping from Taffy nodes to BoxNodes.
  fn build_taffy_tree(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    constraints: &LayoutConstraints,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
  ) -> Result<TaffyNodeId, LayoutError> {
    let root_children: Vec<&BoxNode> = box_node.children.iter().collect();
    self.build_taffy_tree_children(
      taffy,
      box_node,
      root_style,
      &root_children,
      constraints,
      positioned_children,
    )
  }

  /// Builds or updates a cached per-box Taffy tree for this grid container.
  ///
  /// When a cached tree is available and its root children still match the current in-flow child
  /// list, we avoid rebuilding nodes and only touch styles if the computed-style fingerprints have
  /// changed (e.g. because a style override is active for intrinsic sizing probes). This preserves
  /// Taffy's internal caches across repeated layout passes for the same box.
  fn build_or_update_taffy_tree_children_cached(
    &self,
    taffy: &mut CachedTaffyTree,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    root_children: &[&BoxNode],
    constraints: &LayoutConstraints,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
  ) -> Result<TaffyNodeId, LayoutError> {
    let has_subgrid = root_style.grid_row_subgrid || root_style.grid_column_subgrid;
    let child_has_subgrid = root_children
      .iter()
      .any(|child| child.style.grid_row_subgrid || child.style.grid_column_subgrid);
    let can_use_template_cache = !has_subgrid && !child_has_subgrid;

    if let Some(root_id) = taffy.cached_root() {
      let root_matches = taffy
        .get_node_context(root_id)
        .is_some_and(|ctx| *ctx == (box_node as *const BoxNode));
      let existing_count = taffy.child_count(root_id);
      let structure_matches = root_matches
        && existing_count == root_children.len()
        && (0..existing_count).all(|idx| {
          let node_id = taffy.get_child_id(root_id, idx);
          taffy
            .get_node_context(node_id)
            .is_some_and(|ctx| *ctx == (root_children[idx] as *const BoxNode))
        });

      if structure_matches {
        let mut deadline_counter = 0usize;
        let has_positioned_children = box_node.children.iter().any(|child| {
          matches!(
            child.style.position,
            crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
          )
        });
        let child_fingerprint =
          grid_child_fingerprint(root_children, has_positioned_children, &mut deadline_counter)?;
        let root_style_fingerprint = taffy_grid_container_style_fingerprint(root_style);
        let constraints_fingerprint = grid_constraints_fingerprint(constraints);
        let root_layout_fingerprint = combine_fingerprints(root_style_fingerprint, constraints_fingerprint);
        let fingerprints_match = taffy
          .cached_fingerprints()
          .is_some_and(|(prev_root, prev_child)| {
            prev_root == root_layout_fingerprint && prev_child == child_fingerprint
          });

        if !fingerprints_match {
          let root_axis_style = GridAxisStyle::from_style(root_style);
          let cache_key = TaffyNodeCacheKey::new(
            TaffyAdapterKind::Grid,
            root_style_fingerprint,
            child_fingerprint,
            self.viewport_size,
          );
          let cached = self.taffy_cache.get(&cache_key);
          let template: std::sync::Arc<CachedTaffyTemplate> = if let Some(template) = cached {
            record_taffy_node_cache_hit(TaffyAdapterKind::Grid, template.node_count());
            record_taffy_style_cache_hit(TaffyAdapterKind::Grid, template.node_count());
            template
          } else {
            let mut child_styles = Vec::with_capacity(root_children.len());
            for child in root_children.iter() {
              check_layout_deadline(&mut deadline_counter)?;
              let child_style_override = style_override_for(child.id);
              let child_style: &ComputedStyle = child_style_override
                .as_deref()
                .unwrap_or_else(|| child.style.as_ref());
              child_styles.push(std::sync::Arc::new(SendSyncStyle(self.convert_style(
                child_style,
                Some(root_style),
                Some(root_axis_style),
                None,
                false,
                false,
                child.is_replaced(),
              ))));
            }
            let simple_grid = !has_positioned_children
              && self.is_simple_grid(root_style, root_children, &mut deadline_counter)?;
            let root_style = std::sync::Arc::new(SendSyncStyle(self.convert_style(
              root_style,
              None,
              None,
              None,
              simple_grid,
              true,
              box_node.is_replaced(),
            )));
            let template = std::sync::Arc::new(CachedTaffyTemplate {
              root_style,
              child_styles,
            });
            self.taffy_cache.insert(cache_key, template.clone());
            record_taffy_node_cache_miss(TaffyAdapterKind::Grid, template.node_count());
            record_taffy_style_cache_miss(TaffyAdapterKind::Grid, template.node_count());
            template
          };

          for (idx, child) in root_children.iter().enumerate() {
            check_layout_deadline(&mut deadline_counter)?;
            let child = *child;
            let node_id = taffy.get_child_id(root_id, idx);
            let child_style = template.child_styles.get(idx).ok_or_else(|| {
              LayoutError::MissingContext(
                "Cached grid Taffy template missing child style for in-flow child".to_string(),
              )
            })?;
            let mut resolved_style = child_style.0.clone();
            self.apply_grid_intrinsic_size_keywords(child, false, &mut resolved_style)?;
            let needs_update = match taffy.style(node_id) {
              Ok(existing) => existing != &resolved_style,
              Err(_) => true,
            };
            if needs_update {
              taffy
                .set_style(node_id, resolved_style)
                .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
            }
          }

          let root_style = template.root_style.0.clone();
          let needs_root_update = match taffy.style(root_id) {
            Ok(existing) => existing != &root_style,
            Err(_) => true,
          };
          if needs_root_update {
            taffy
              .set_style(root_id, root_style)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }

        taffy.set_root(root_id);
        taffy.set_fingerprints(root_layout_fingerprint, child_fingerprint);
        return Ok(root_id);
      }

      // Structure mismatch: clear so we can safely rebuild.
      taffy.clear_and_invalidate();
    }

    if !can_use_template_cache {
      let root_id = self.build_taffy_tree_children(
        taffy,
        box_node,
        root_style,
        root_children,
        constraints,
        positioned_children,
      )?;
      taffy.set_root(root_id);
      return Ok(root_id);
    }

    let mut deadline_counter = 0usize;
    let has_positioned_children = box_node.children.iter().any(|child| {
      matches!(
        child.style.position,
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
      )
    });
    let child_fingerprint =
      grid_child_fingerprint(root_children, has_positioned_children, &mut deadline_counter)?;
    let root_style_fingerprint = taffy_grid_container_style_fingerprint(root_style);
    let constraints_fingerprint = grid_constraints_fingerprint(constraints);
    let root_layout_fingerprint = combine_fingerprints(root_style_fingerprint, constraints_fingerprint);

    let root_axis_style = GridAxisStyle::from_style(root_style);
    let cache_key = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Grid,
      root_style_fingerprint,
      child_fingerprint,
      self.viewport_size,
    );
    let cached = self.taffy_cache.get(&cache_key);
    let template: std::sync::Arc<CachedTaffyTemplate> = if let Some(template) = cached {
      record_taffy_node_cache_hit(TaffyAdapterKind::Grid, template.node_count());
      record_taffy_style_cache_hit(TaffyAdapterKind::Grid, template.node_count());
      template
    } else {
      let mut child_styles = Vec::with_capacity(root_children.len());
      for child in root_children.iter() {
        check_layout_deadline(&mut deadline_counter)?;
        let child_style_override = style_override_for(child.id);
        let child_style: &ComputedStyle = child_style_override
          .as_deref()
          .unwrap_or_else(|| child.style.as_ref());
        child_styles.push(std::sync::Arc::new(SendSyncStyle(self.convert_style(
          child_style,
          Some(root_style),
          Some(root_axis_style),
          None,
          false,
          false,
          child.is_replaced(),
        ))));
      }
      let simple_grid =
        !has_positioned_children && self.is_simple_grid(root_style, root_children, &mut deadline_counter)?;
      let root_style = std::sync::Arc::new(SendSyncStyle(self.convert_style(
        root_style,
        None,
        None,
        None,
        simple_grid,
        true,
        box_node.is_replaced(),
      )));
      let template = std::sync::Arc::new(CachedTaffyTemplate {
        root_style,
        child_styles,
      });
      self.taffy_cache.insert(cache_key, template.clone());
      record_taffy_node_cache_miss(TaffyAdapterKind::Grid, template.node_count());
      record_taffy_style_cache_miss(TaffyAdapterKind::Grid, template.node_count());
      template
    };

    let mut taffy_children = Vec::with_capacity(root_children.len());
    for (child_style, child) in template.child_styles.iter().zip(root_children.iter()) {
      check_layout_deadline(&mut deadline_counter)?;
      let child = *child;
      let mut resolved_style = child_style.0.clone();
      self.apply_grid_intrinsic_size_keywords(child, false, &mut resolved_style)?;
      let node = taffy
        .new_leaf_with_context(resolved_style, child as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      taffy_children.push(node);
    }

    let node_id = if taffy_children.is_empty() {
      taffy
        .new_leaf_with_context(template.root_style.0.clone(), box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?
    } else {
      let node_id = taffy
        .new_with_children(template.root_style.0.clone(), &taffy_children)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      taffy
        .set_node_context(node_id, Some(box_node as *const BoxNode))
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      node_id
    };

    taffy.set_root(node_id);
    taffy.set_fingerprints(root_layout_fingerprint, child_fingerprint);
    Ok(node_id)
  }

  /// Builds a Taffy tree using an explicit slice of root children (used to exclude out-of-flow boxes).
  fn build_taffy_tree_children(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    root_style: &ComputedStyle,
    root_children: &[&BoxNode],
    _constraints: &LayoutConstraints,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
  ) -> Result<TaffyNodeId, LayoutError> {
    let mut deadline_counter = 0usize;
    // CSS `order` applies to grid items (as well as flex items) and participates in the grid
    // auto-placement algorithm + paint order. Taffy does not model `order`, so we must explicitly
    // reorder in-flow grid children before building the Taffy tree.
    //
    // Spec: https://drafts.csswg.org/css-display-3/#order-property
    let mut in_flow_children: Vec<(usize, &BoxNode)> = Vec::new();
    let mut positioned: Vec<*const BoxNode> = Vec::new();
    let mut child_has_subgrid = false;
    let mut in_flow_children_need_sort = false;
    let mut last_in_flow_order: Option<i32> = None;
    for (idx, child) in root_children.iter().enumerate() {
      check_layout_deadline(&mut deadline_counter)?;
      let child = *child;
      match child.style.position {
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
          positioned.push(child as *const BoxNode)
        }
        _ => {
          if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
            child_has_subgrid = true;
          }
          if let Some(prev) = last_in_flow_order {
            if child.style.order < prev {
              in_flow_children_need_sort = true;
            }
          }
          last_in_flow_order = Some(child.style.order);
          in_flow_children.push((idx, child))
        }
      }
    }
    if in_flow_children_need_sort {
      // `check_layout_deadline()` is periodic; ensure we still perform a definite check before doing
      // potentially expensive sort work.
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      in_flow_children.sort_by(|(a_idx, a), (b_idx, b)| {
        a.style
          .order
          .cmp(&b.style.order)
          .then_with(|| a_idx.cmp(b_idx))
      });
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
    }
    let in_flow_children: Vec<&BoxNode> = in_flow_children
      .into_iter()
      .map(|(_, child)| child)
      .collect();

    let has_subgrid = root_style.grid_row_subgrid || root_style.grid_column_subgrid;

    if !has_subgrid && !child_has_subgrid {
      let has_positioned_children = box_node.children.iter().any(|child| {
        matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      });
      let root_axis_style = GridAxisStyle::from_style(root_style);
      let child_fingerprint = grid_child_fingerprint(
        &in_flow_children,
        has_positioned_children,
        &mut deadline_counter,
      )?;
      let root_style_fingerprint = taffy_grid_container_style_fingerprint(root_style);
      let cache_key = TaffyNodeCacheKey::new(
        TaffyAdapterKind::Grid,
        root_style_fingerprint,
        child_fingerprint,
        self.viewport_size,
      );
      let cached = self.taffy_cache.get(&cache_key);
      let template: std::sync::Arc<CachedTaffyTemplate> = if let Some(template) = cached {
        record_taffy_node_cache_hit(TaffyAdapterKind::Grid, template.node_count());
        record_taffy_style_cache_hit(TaffyAdapterKind::Grid, template.node_count());
        template
      } else {
        let mut child_styles = Vec::with_capacity(in_flow_children.len());
        for child in in_flow_children.iter() {
          check_layout_deadline(&mut deadline_counter)?;
          let child_style_override = style_override_for(child.id);
          let child_style: &ComputedStyle = child_style_override
            .as_deref()
            .unwrap_or_else(|| child.style.as_ref());
          child_styles.push(std::sync::Arc::new(SendSyncStyle(self.convert_style(
            child_style,
            Some(root_style),
            Some(root_axis_style),
            None,
            false,
            false,
            child.is_replaced(),
          ))));
        }
        let simple_grid = !has_positioned_children
          && self.is_simple_grid(root_style, &in_flow_children, &mut deadline_counter)?;
        let root_style = std::sync::Arc::new(SendSyncStyle(self.convert_style(
          root_style,
          None,
          None,
          None,
          simple_grid,
          true,
          box_node.is_replaced(),
        )));
        let template = std::sync::Arc::new(CachedTaffyTemplate {
          root_style,
          child_styles,
        });
        self.taffy_cache.insert(cache_key, template.clone());
        record_taffy_node_cache_miss(TaffyAdapterKind::Grid, template.node_count());
        record_taffy_style_cache_miss(TaffyAdapterKind::Grid, template.node_count());
        template
      };

      let mut taffy_children = Vec::with_capacity(in_flow_children.len());
      for (child_style, child) in template.child_styles.iter().zip(in_flow_children.iter()) {
        check_layout_deadline(&mut deadline_counter)?;
        let child = *child;
        let mut resolved_style = child_style.0.clone();
        self.apply_grid_intrinsic_size_keywords(child, false, &mut resolved_style)?;
        let node = taffy
          .new_leaf_with_context(resolved_style, child as *const BoxNode)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        taffy_children.push(node);
      }

      let node_id = if taffy_children.is_empty() {
        taffy
          .new_leaf_with_context(template.root_style.0.clone(), box_node as *const BoxNode)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?
      } else {
        let node_id = taffy
          .new_with_children(template.root_style.0.clone(), &taffy_children)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        taffy
          .set_node_context(node_id, Some(box_node as *const BoxNode))
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        node_id
      };

      if !positioned.is_empty() {
        positioned_children.insert(node_id, positioned);
      }

      return Ok(node_id);
    }

    self.build_taffy_tree_inner(
      taffy,
      box_node,
      root_style,
      true,
      None,
      None,
      None,
      Some(root_children),
      positioned_children,
      &mut deadline_counter,
    )
  }

  fn build_taffy_tree_inner(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    box_node: &BoxNode,
    style: &ComputedStyle,
    is_root: bool,
    containing_grid: Option<&ComputedStyle>,
    containing_grid_axis: Option<GridAxisStyle>,
    containing_grid_line_counts: Option<TaffySize<u16>>,
    children_override: Option<&[&BoxNode]>,
    positioned_children: &mut FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
    deadline_counter: &mut usize,
  ) -> Result<TaffyNodeId, LayoutError> {
    // Child collection for grid containers uses (order, DOM index) so we can implement CSS `order`
    // in a deterministic way (Taffy does not support it natively).
    let mut children_iter: Vec<(usize, &BoxNode)> = Vec::new();
    let mut positioned: Vec<*const BoxNode> = Vec::new();
    let mut has_subgrid_child = false;

    // Determine whether this grid container should be represented in the Taffy tree.
    let is_grid_container = matches!(
      box_node.formatting_context(),
      Some(FormattingContextType::Grid)
    );
    // Non-grid descendants are represented as leaf nodes in the Taffy tree. Avoid collecting or
    // scanning their children here; their full subtree will be laid out later when we re-run the
    // child formatting context with the definite sizes resolved by Taffy.
    if !is_grid_container && !is_root {
      let mut taffy_style = self.convert_style(
        style,
        containing_grid,
        containing_grid_axis,
        containing_grid_line_counts,
        false,
        false,
        box_node.is_replaced(),
      );
      self.apply_grid_intrinsic_size_keywords(box_node, false, &mut taffy_style)?;
      return taffy
        .new_leaf_with_context(taffy_style, box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)));
    }
    let mut include_children = is_root;

    // Partition children into in-flow vs positioned for grid containers we expand in the tree.
    if is_grid_container {
      let mut children_need_sort = false;
      let mut last_order: Option<i32> = None;
      if let Some(children_override) = children_override {
        for (idx, child) in children_override.iter().enumerate() {
          check_layout_deadline(deadline_counter)?;
          let child = *child;
          if is_collapsible_whitespace_grid_item(child) {
            continue;
          }
          match child.style.position {
            crate::style::position::Position::Absolute
            | crate::style::position::Position::Fixed => positioned.push(child as *const BoxNode),
            _ => {
              if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
                has_subgrid_child = true;
              }
              if let Some(prev) = last_order {
                if child.style.order < prev {
                  children_need_sort = true;
                }
              }
              last_order = Some(child.style.order);
              children_iter.push((idx, child))
            }
          }
        }
      } else {
        for (idx, child) in box_node.children.iter().enumerate() {
          check_layout_deadline(deadline_counter)?;
          if is_collapsible_whitespace_grid_item(child) {
            continue;
          }
          match child.style.position {
            crate::style::position::Position::Absolute
            | crate::style::position::Position::Fixed => positioned.push(child as *const BoxNode),
            _ => {
              if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
                has_subgrid_child = true;
              }
              if let Some(prev) = last_order {
                if child.style.order < prev {
                  children_need_sort = true;
                }
              }
              last_order = Some(child.style.order);
              children_iter.push((idx, child))
            }
          }
        }
      }

      if children_need_sort {
        if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
          return Err(LayoutError::Timeout { elapsed });
        }
        children_iter.sort_by(|(a_idx, a), (b_idx, b)| {
          a.style
            .order
            .cmp(&b.style.order)
            .then_with(|| a_idx.cmp(b_idx))
        });
        if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
          return Err(LayoutError::Timeout { elapsed });
        }
      }
    } else if is_root {
      // Non-grid roots should preserve DOM order.
      let ordered = match children_override {
        Some(children_override) => children_override.to_vec(),
        None => box_node.children.iter().collect(),
      };
      children_iter = ordered.into_iter().enumerate().collect();
    }

    let children_iter: Vec<&BoxNode> = children_iter.into_iter().map(|(_, child)| child).collect();

    // Expand subgrids (and any grid that hosts a subgrid child) into the Taffy tree so tracks can be shared.
    if is_grid_container {
      let is_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;
      include_children |= is_subgrid || has_subgrid_child;
    }

    let has_positioned_children = if is_grid_container && is_root {
      box_node.children.iter().any(|child| {
        matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      })
    } else {
      !positioned.is_empty()
    };

    let simple_grid = include_children
      && is_root
      && !has_positioned_children
      && self.is_simple_grid(style, &children_iter, deadline_counter)?;
    let mut taffy_style = self.convert_style(
      style,
      containing_grid,
      containing_grid_axis,
      containing_grid_line_counts,
      simple_grid,
      include_children,
      box_node.is_replaced(),
    );

    self.apply_grid_intrinsic_size_keywords(box_node, is_root, &mut taffy_style)?;

    let axis_style_for_children = if is_grid_container {
      GridAxisStyle::effective_for_grid_container(style, containing_grid_axis)
    } else {
      GridAxisStyle::from_style(style)
    };
    let line_counts_for_children = if is_grid_container && !simple_grid {
      Some(self.compute_grid_line_counts_for_container(
        style,
        containing_grid,
        containing_grid_line_counts,
        axis_style_for_children,
      ))
    } else {
      None
    };

    // CSS Grid 2 §9.7 ("Subgrid") constrains the subgrid's explicit grid to the tracks it spans in
    // the parent. If the author provides a `<line-name-list>` in a subgridded axis, the used value
    // is truncated to match the used number of explicit tracks (#subgrid-span). Additionally, when
    // the `<line-name-list>` is omitted, FastRender synthesizes a placeholder name list to convey
    // the subgrid span to Taffy.
    //
    // `compute_grid_line_counts_for_container` yields the used line counts in Taffy's physical axes.
    // Normalize the corresponding `subgrid_*_names` vectors so Taffy sees the correct explicit grid
    // span even when placement is clamped.
    if let Some(line_counts) = line_counts_for_children {
      let normalize = |names: &mut Vec<Vec<String>>, required: usize| {
        if required == 0 {
          return;
        }
        if names.len() < required {
          names.resize(required, Vec::new());
        } else if names.len() > required {
          names.truncate(required);
        }
      };

      if taffy_style.subgrid_columns {
        normalize(
          &mut taffy_style.subgrid_column_names,
          (line_counts.width.max(1)) as usize,
        );
      }
      if taffy_style.subgrid_rows {
        normalize(
          &mut taffy_style.subgrid_row_names,
          (line_counts.height.max(1)) as usize,
        );
      }
    }

    // Subgrid containers with an omitted `<line-name-list>` implicitly span all parent tracks. That
    // span is encoded for Taffy via a synthetic line-name vector whose length equals the parent
    // line count. Nested subgrids need to see that synthesized line count as well, so when we are
    // about to convert children under such a subgrid we pass a derived containing-grid style with
    // `grid_*_line_names` populated from the parent grid.
    //
    // This is intentionally limited to the fully-auto placement case: if the subgrid item is
    // explicitly placed, its span is derived from placement and may cover only a subset of parent
    // tracks.
    let containing_grid_for_children_override: Option<Arc<ComputedStyle>> = if is_grid_container
      && (style.grid_row_subgrid || style.grid_column_subgrid)
    {
      if let Some(parent_grid) = containing_grid {
        let column_omitted_auto = style.grid_column_subgrid
          && subgrid_line_name_list_is_omitted(&style.subgrid_column_line_names)
          && style.grid_column_start == 0
          && style.grid_column_end == 0
          && style.grid_column_raw.is_none();
        let row_omitted_auto = style.grid_row_subgrid
          && subgrid_line_name_list_is_omitted(&style.subgrid_row_line_names)
          && style.grid_row_start == 0
          && style.grid_row_end == 0
          && style.grid_row_raw.is_none();

        if column_omitted_auto || row_omitted_auto {
          let mut derived = style.clone();
          if column_omitted_auto {
            derived.grid_column_line_names = parent_grid.grid_column_line_names.clone();
            derived.grid_column_names = parent_grid.grid_column_names.clone();
          }
          if row_omitted_auto {
            derived.grid_row_line_names = parent_grid.grid_row_line_names.clone();
            derived.grid_row_names = parent_grid.grid_row_names.clone();
          }
          Some(Arc::new(derived))
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    };
    let containing_grid_for_children: &ComputedStyle = containing_grid_for_children_override
      .as_deref()
      .unwrap_or(style);

    let node_id = if include_children {
      let mut taffy_children = Vec::with_capacity(children_iter.len());
      for child in children_iter {
        check_layout_deadline(deadline_counter)?;
        let child_style_override = style_override_for(child.id);
        let child_style: &ComputedStyle = child_style_override
          .as_deref()
          .unwrap_or_else(|| child.style.as_ref());
        taffy_children.push(self.build_taffy_tree_inner(
          taffy,
          child,
          child_style,
          false,
          Some(containing_grid_for_children),
          Some(axis_style_for_children),
          line_counts_for_children,
          None,
          positioned_children,
          deadline_counter,
        )?);
      }
      let node_id = taffy
        .new_with_children(taffy_style, &taffy_children)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      if !positioned.is_empty() {
        positioned_children.insert(node_id, positioned);
      }
      taffy
        .set_node_context(node_id, Some(box_node as *const BoxNode))
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      node_id
    } else {
      taffy
        .new_leaf_with_context(taffy_style, box_node as *const BoxNode)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?
    };

    Ok(node_id)
  }

  fn compute_grid_line_counts_for_container(
    &self,
    style: &ComputedStyle,
    containing_grid: Option<&ComputedStyle>,
    containing_grid_line_counts: Option<TaffySize<u16>>,
    axis_style: GridAxisStyle,
  ) -> TaffySize<u16> {
    let swap_grid_axes = !axis_style.inline_is_horizontal();
    let has_parent_grid = containing_grid_line_counts.is_some();

    let (parent_css_col_lines, parent_css_row_lines) = match containing_grid_line_counts {
      Some(parent) => {
        if swap_grid_axes {
          (parent.height, parent.width)
        } else {
          (parent.width, parent.height)
        }
      }
      None => (0u16, 0u16),
    };

    let default_subgrid_span = |names: &[Vec<String>], parent_line_count: u16| -> u16 {
      if !subgrid_line_name_list_is_omitted(names) {
        let len = names.len().max(1);
        let tracks = len.saturating_sub(1).max(1);
        u16::try_from(tracks).unwrap_or(u16::MAX)
      } else {
        parent_line_count.saturating_sub(1).max(1)
      }
    };

    let span_from_placement = |start: i32,
                               end: i32,
                               raw: Option<&str>,
                               parent_line_count: u16,
                               parent_line_names: Option<&[Vec<String>]>,
                               default_span: u16|
     -> u16 {
      if start == 0 && end == 0 && raw.is_none() {
        return default_span;
      }

      let line_count = (parent_line_count > 0).then_some(parent_line_count);
      let resolved =
        resolve_grid_line_range_from_style(start, end, raw, line_count, parent_line_names);
      resolved
        .and_then(|(start_line, end_line)| end_line.checked_sub(start_line))
        .filter(|span| *span > 0)
        .unwrap_or(default_span)
    };

    let area_row_count = style.grid_template_areas.len();
    let area_col_count = style
      .grid_template_areas
      .iter()
      .map(|row| row.len())
      .max()
      .unwrap_or(0);

    let css_column_subgrid = style.grid_column_subgrid && has_parent_grid;
    let css_row_subgrid = style.grid_row_subgrid && has_parent_grid;

    let parent_column_names = containing_grid.map(|grid| CssGridAxis::Column.line_names(grid));
    let parent_row_names = containing_grid.map(|grid| CssGridAxis::Row.line_names(grid));

    let css_col_lines = if css_column_subgrid {
      let default_span =
        default_subgrid_span(&style.subgrid_column_line_names, parent_css_col_lines);
      let span = span_from_placement(
        style.grid_column_start,
        style.grid_column_end,
        style.grid_column_raw.as_deref(),
        parent_css_col_lines,
        parent_column_names,
        default_span,
      );
      span.saturating_add(1)
    } else {
      let tracks = style.grid_template_columns.len().max(area_col_count).max(1);
      u16::try_from(tracks.saturating_add(1)).unwrap_or(u16::MAX)
    };

    let css_row_lines = if css_row_subgrid {
      let default_span = default_subgrid_span(&style.subgrid_row_line_names, parent_css_row_lines);
      let span = span_from_placement(
        style.grid_row_start,
        style.grid_row_end,
        style.grid_row_raw.as_deref(),
        parent_css_row_lines,
        parent_row_names,
        default_span,
      );
      span.saturating_add(1)
    } else {
      let tracks = style.grid_template_rows.len().max(area_row_count).max(1);
      u16::try_from(tracks.saturating_add(1)).unwrap_or(u16::MAX)
    };

    if swap_grid_axes {
      TaffySize {
        width: css_row_lines,
        height: css_col_lines,
      }
    } else {
      TaffySize {
        width: css_col_lines,
        height: css_row_lines,
      }
    }
  }

  /// Converts ComputedStyle to Taffy Style
  fn convert_style(
    &self,
    style: &ComputedStyle,
    containing_grid: Option<&ComputedStyle>,
    containing_grid_axis: Option<GridAxisStyle>,
    containing_grid_line_counts: Option<TaffySize<u16>>,
    simple_grid: bool,
    is_grid_node: bool,
    item_is_replaced: bool,
  ) -> TaffyStyle {
    let mut taffy_style = TaffyStyle::default();
    taffy_style.item_is_replaced = item_is_replaced;
    // CSS `box-sizing` controls whether `width/height/min/max` apply to the content box or the
    // border box. Use Taffy's native support rather than manually converting sizes (which breaks
    // percentage sizing for content-box elements with padding/border).
    taffy_style.box_sizing = if style.box_sizing == BoxSizing::ContentBox {
      taffy::style::BoxSizing::ContentBox
    } else {
      taffy::style::BoxSizing::BorderBox
    };
    let is_grid = is_grid_node && !simple_grid;
    let container_axis_style = if is_grid {
      GridAxisStyle::effective_for_grid_container(style, containing_grid_axis)
    } else {
      GridAxisStyle::from_style(style)
    };
    let inline_positive_container = container_axis_style.inline_positive();
    let block_positive_container = container_axis_style.block_positive();
    let inline_is_horizontal_container = container_axis_style.inline_is_horizontal();
    taffy_style.start_end_axis_positive = taffy::geometry::Point {
      x: if inline_is_horizontal_container {
        inline_positive_container
      } else {
        block_positive_container
      },
      y: if inline_is_horizontal_container {
        block_positive_container
      } else {
        inline_positive_container
      },
    };

    let reserve_scroll_x = style.scrollbar_gutter.stable
      && matches!(
        style.overflow_x,
        CssOverflow::Hidden | CssOverflow::Auto | CssOverflow::Scroll
      );
    let reserve_scroll_y = style.scrollbar_gutter.stable
      && matches!(
        style.overflow_y,
        CssOverflow::Hidden | CssOverflow::Auto | CssOverflow::Scroll
      );
    let map_overflow = |value: CssOverflow, reserve: bool| match value {
      // Taffy lacks a distinct `Auto` variant. CSS `overflow: auto` is still a scroll container
      // (automatic min size = 0), but it should only reserve scrollbar space when
      // `scrollbar-gutter: stable` requests it. (Overlay scrollbars are modeled by default.)
      // The same applies to `overflow: hidden` when stable gutters are requested.
      CssOverflow::Visible => TaffyOverflow::Visible,
      CssOverflow::Clip => TaffyOverflow::Clip,
      CssOverflow::Scroll => TaffyOverflow::Scroll,
      CssOverflow::Hidden | CssOverflow::Auto => {
        if reserve {
          TaffyOverflow::Scroll
        } else {
          TaffyOverflow::Hidden
        }
      }
    };

    // Grid item axes follow the containing grid's writing mode, not the item's own.
    let item_axis_style = containing_grid_axis.unwrap_or_else(|| GridAxisStyle::from_style(style));
    let inline_positive_item = item_axis_style.inline_positive();
    let block_positive_item = item_axis_style.block_positive();
    let inline_is_horizontal_item = item_axis_style.inline_is_horizontal();
    // `self-start`/`self-end` resolve against the alignment subject's (item's) writing-mode and
    // direction, but on the physical axis being aligned. Compute the polarity for each physical
    // axis so we can map these keywords correctly even when writing modes differ.
    let self_axes =
      FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
    let self_axis_positive = |axis: PhysicalAxis| {
      if axis == self_axes.inline_axis() {
        self_axes.inline_positive()
      } else {
        self_axes.block_positive()
      }
    };
    let self_physical_x_positive = self_axis_positive(PhysicalAxis::X);
    let self_physical_y_positive = self_axis_positive(PhysicalAxis::Y);
    let container_physical_x_positive = if inline_is_horizontal_item {
      inline_positive_item
    } else {
      block_positive_item
    };
    let container_physical_y_positive = if inline_is_horizontal_item {
      block_positive_item
    } else {
      inline_positive_item
    };
    let convert_item_alignment = |align: AlignItems, axis: PhysicalAxis| {
      let (container_positive, self_positive) = match axis {
        PhysicalAxis::X => (container_physical_x_positive, self_physical_x_positive),
        PhysicalAxis::Y => (container_physical_y_positive, self_physical_y_positive),
      };
      let axis_positive = match align {
        AlignItems::SelfStart | AlignItems::SelfEnd => self_positive,
        _ => container_positive,
      };
      self.convert_align_items(&align, axis_positive)
    };

    // Display mode
    if is_grid {
      taffy_style.display = Display::Grid;
      taffy_style.axes_swapped = !inline_is_horizontal_container;
    } else {
      taffy_style.display = Display::Block;
    }

    // Size
    let width = self.convert_sizing_property_to_dimension_box_sizing(
      &style.width,
      &style.width_keyword,
      style,
      Axis::Horizontal,
    );
    let height = self.convert_sizing_property_to_dimension_box_sizing(
      &style.height,
      &style.height_keyword,
      style,
      Axis::Vertical,
    );
    taffy_style.size = taffy::geometry::Size { width, height };

    // Min/Max size
    taffy_style.min_size = taffy::geometry::Size {
      width: self.convert_sizing_property_to_dimension_box_sizing(
        &style.min_width,
        &style.min_width_keyword,
        style,
        Axis::Horizontal,
      ),
      height: self.convert_sizing_property_to_dimension_box_sizing(
        &style.min_height,
        &style.min_height_keyword,
        style,
        Axis::Vertical,
      ),
    };
    taffy_style.max_size = taffy::geometry::Size {
      width: self.convert_sizing_property_to_dimension_box_sizing(
        &style.max_width,
        &style.max_width_keyword,
        style,
        Axis::Horizontal,
      ),
      height: self.convert_sizing_property_to_dimension_box_sizing(
        &style.max_height,
        &style.max_height_keyword,
        style,
        Axis::Vertical,
      ),
    };
    if containing_grid.is_some() {
      // Like the preferred size above, CSS percentage minimum size constraints on grid items
      // resolve against the grid *area* size (the track size after layout). Taffy resolves these
      // percentages against the grid container instead, which can force an item in a narrow track
      // to be as wide as the entire grid, causing large horizontal overflow.
      //
      // Treat percentage min-size constraints as `auto` in Taffy so the measure callback can
      // resolve them against the definite grid area size that Taffy provides during layout.
      //
      // Note: Keep max-size constraints intact here; the grid integration has explicit tests that
      // rely on `max-width: <percentage>` clamping stretched items.
      if style.min_width.is_some_and(|len| len.has_percentage()) {
        taffy_style.min_size.width = Dimension::auto();
      }
      if style.min_height.is_some_and(|len| len.has_percentage()) {
        taffy_style.min_size.height = Dimension::auto();
      }
    }

    // Percentage sizes on grid items resolve against the *grid area* size, not the grid container.
    // Taffy resolves percentage dimensions relative to the node's parent, so if we pass percentage
    // widths/heights through directly, intrinsic track sizing can end up treating `width:100%`
    // items as if they were as wide as the whole grid container. This can incorrectly inflate
    // `fr` tracks and cause grids to overflow instead of distributing remaining space.
    //
    // Represent percentage-based sizes as `auto` in the Taffy tree for grid items, and let our
    // measure callback (which receives the resolved grid area size via `available_space`) handle
    // percentage resolution with the correct base.
    let is_grid_item = containing_grid.is_some() && !is_grid_node;
    if is_grid_item {
      let has_percent =
        |length: &Option<Length>| length.as_ref().is_some_and(|len| len.has_percentage());
      if has_percent(&style.width) {
        taffy_style.size.width = Dimension::auto();
      }
      if has_percent(&style.height) {
        taffy_style.size.height = Dimension::auto();
      }
      if has_percent(&style.min_width) {
        taffy_style.min_size.width = Dimension::auto();
      }
      if has_percent(&style.min_height) {
        taffy_style.min_size.height = Dimension::auto();
      }
      if has_percent(&style.max_width) {
        taffy_style.max_size.width = Dimension::auto();
      }
      if has_percent(&style.max_height) {
        taffy_style.max_size.height = Dimension::auto();
      }
    }

    // Margin
    let margin_left_auto = style.margin_left.is_none();
    let margin_right_auto = style.margin_right.is_none();
    let margin_top_auto = style.margin_top.is_none();
    let margin_bottom_auto = style.margin_bottom.is_none();
    taffy_style.margin = taffy::geometry::Rect {
      left: self.convert_opt_length_to_lpa(&style.margin_left, style),
      right: self.convert_opt_length_to_lpa(&style.margin_right, style),
      top: self.convert_opt_length_to_lpa(&style.margin_top, style),
      bottom: self.convert_opt_length_to_lpa(&style.margin_bottom, style),
    };

    // Padding
    taffy_style.padding = taffy::geometry::Rect {
      left: self.convert_length_to_lp(&style.padding_left, style),
      right: self.convert_length_to_lp(&style.padding_right, style),
      top: self.convert_length_to_lp(&style.padding_top, style),
      bottom: self.convert_length_to_lp(&style.padding_bottom, style),
    };

    // Border
    taffy_style.border = taffy::geometry::Rect {
      left: self.convert_length_to_lp(&style.used_border_left_width(), style),
      right: self.convert_length_to_lp(&style.used_border_right_width(), style),
      top: self.convert_length_to_lp(&style.used_border_top_width(), style),
      bottom: self.convert_length_to_lp(&style.used_border_bottom_width(), style),
    };
    taffy_style.aspect_ratio = self.convert_aspect_ratio(style.aspect_ratio);

    taffy_style.overflow = taffy::geometry::Point {
      x: map_overflow(style.overflow_x, reserve_scroll_x),
      y: map_overflow(style.overflow_y, reserve_scroll_y),
    };
    // Model overlay scrollbars by default: only reserve layout space when the author opts in via
    // `scrollbar-gutter: stable`.
    taffy_style.scrollbar_width = if reserve_scroll_x || reserve_scroll_y {
      resolve_scrollbar_width(style)
    } else {
      0.0
    };

    // Grid container properties
    if is_grid {
      let has_parent_grid = containing_grid_axis.is_some();
      // Per CSS Grid 2 §9.7 ("Subgrid"), subgrid is disabled when the grid container is forced to
      // establish an independent formatting context. Layout containment implies such a context, so
      // treat `grid-template-*: subgrid` as used value `none` across this boundary.
      let disables_subgrid = style.containment.layout;
      let css_row_subgrid = style.grid_row_subgrid && has_parent_grid && !disables_subgrid;
      let css_column_subgrid = style.grid_column_subgrid && has_parent_grid && !disables_subgrid;
      let css_column_line_names_omitted =
        css_column_subgrid && subgrid_line_name_list_is_omitted(&style.subgrid_column_line_names);
      let css_row_line_names_omitted =
        css_row_subgrid && subgrid_line_name_list_is_omitted(&style.subgrid_row_line_names);

      // Taffy always interprets columns as the physical X axis and rows as the physical Y axis.
      // CSS Grid axes, however, are defined in terms of the element's writing-mode (inline/block).
      // When the inline axis is vertical we transpose all axis-dependent grid properties so that
      // Taffy sees column data for the physical X axis and row data for the physical Y axis.
      let swap_grid_axes = !inline_is_horizontal_container;

      // Grid template columns/rows.
      if swap_grid_axes {
        taffy_style.grid_template_columns =
          self.convert_grid_template(&style.grid_template_rows, style);
        taffy_style.grid_template_rows =
          self.convert_grid_template(&style.grid_template_columns, style);
      } else {
        taffy_style.grid_template_columns =
          self.convert_grid_template(&style.grid_template_columns, style);
        taffy_style.grid_template_rows =
          self.convert_grid_template(&style.grid_template_rows, style);
      }

      // `grid-template-areas` participates in defining the explicit grid size. When the area
      // matrix is larger than the template track list, CSS creates additional tracks and sizes them
      // using `grid-auto-rows` / `grid-auto-columns` (CSS Grid §7.2: The Implicit Grid).
      //
      // Taffy has historically only handled the “area-only” case (no explicit template tracks at
      // all). Ensure we also synthesize the extra tracks when `grid-template-rows/columns` exists
      // but is shorter than the area matrix — even when no grid items occupy those tracks (BBC uses
      // this pattern with an empty "supplementary" row at some breakpoints).
      if !style.grid_template_areas.is_empty() {
        let css_row_count = style.grid_template_areas.len();
        let css_col_count = style
          .grid_template_areas
          .iter()
          .map(|row| row.len())
          .max()
          .unwrap_or(0);

        let template_track_count = |template: &[GridTemplateComponent<String>]| -> Option<usize> {
          let mut count = 0usize;
          for component in template {
            match component {
              GridTemplateComponent::Single(_) => count += 1,
              GridTemplateComponent::Repeat(rep) => match rep.count {
                // Explicit repeat counts are expanded during style parsing; keep this for
                // completeness in case a caller constructs Taffy styles directly.
                RepetitionCount::Count(n) => {
                  count = count.saturating_add((n as usize).saturating_mul(rep.tracks.len()));
                }
                // Auto-repeat resolves during layout. We can't safely reason about the final
                // track count here, so skip synthesis in that case.
                RepetitionCount::AutoFill | RepetitionCount::AutoFit => return None,
              },
            }
          }
          Some(count)
        };

        let ensure_tracks = |template: &mut Vec<GridTemplateComponent<String>>,
                             required: usize,
                             implicit_tracks: &Arc<[GridTrack]>| {
          if required == 0 {
            return;
          }
          let Some(current) = template_track_count(template) else {
            return;
          };
          if current == 0 {
            // With no explicit template tracks, CSS defaults the area-defined tracks to `auto`.
            template.extend(
              (0..required).map(|_| GridTemplateComponent::Single(TrackSizingFunction::AUTO)),
            );
            return;
          }
          if current >= required {
            return;
          }
          let missing = required - current;
          let default_auto_track = GridTrack::Auto;
          for i in 0..missing {
            let track = if implicit_tracks.is_empty() {
              &default_auto_track
            } else {
              &implicit_tracks[i % implicit_tracks.len()]
            };
            template.push(GridTemplateComponent::Single(
              self.convert_track_size(track, style),
            ));
          }
        };

        if swap_grid_axes {
          // CSS rows map to physical columns; CSS columns map to physical rows.
          ensure_tracks(
            &mut taffy_style.grid_template_columns,
            css_row_count,
            &style.grid_auto_rows,
          );
          ensure_tracks(
            &mut taffy_style.grid_template_rows,
            css_col_count,
            &style.grid_auto_columns,
          );
        } else {
          ensure_tracks(
            &mut taffy_style.grid_template_columns,
            css_col_count,
            &style.grid_auto_columns,
          );
          ensure_tracks(
            &mut taffy_style.grid_template_rows,
            css_row_count,
            &style.grid_auto_rows,
          );
        }
      }

      // Subgrid flags + extra line names.
      taffy_style.subgrid_columns = if swap_grid_axes {
        css_row_subgrid
      } else {
        css_column_subgrid
      };
      taffy_style.subgrid_rows = if swap_grid_axes {
        css_column_subgrid
      } else {
        css_row_subgrid
      };
      let (parent_physical_column_line_count, parent_physical_row_line_count) =
        if let Some(parent_counts) = containing_grid_line_counts {
          (
            (parent_counts.width.max(1)) as usize,
            (parent_counts.height.max(1)) as usize,
          )
        } else if has_parent_grid {
          if let Some(containing_grid) = containing_grid {
            let physical_column_line_count = if swap_grid_axes {
              // CSS rows map to physical columns when axes are swapped.
              let len = containing_grid.grid_row_line_names.len();
              let tracks = if len > 0 {
                len.saturating_sub(1)
              } else {
                containing_grid.grid_template_rows.len()
              };
              tracks.saturating_add(1)
            } else {
              let len = containing_grid.grid_column_line_names.len();
              let tracks = if len > 0 {
                len.saturating_sub(1)
              } else {
                containing_grid.grid_template_columns.len()
              };
              tracks.saturating_add(1)
            };

            let physical_row_line_count = if swap_grid_axes {
              // CSS columns map to physical rows when axes are swapped.
              let len = containing_grid.grid_column_line_names.len();
              let tracks = if len > 0 {
                len.saturating_sub(1)
              } else {
                containing_grid.grid_template_columns.len()
              };
              tracks.saturating_add(1)
            } else {
              let len = containing_grid.grid_row_line_names.len();
              let tracks = if len > 0 {
                len.saturating_sub(1)
              } else {
                containing_grid.grid_template_rows.len()
              };
              tracks.saturating_add(1)
            };

            (physical_column_line_count.max(1), physical_row_line_count.max(1))
          } else {
            (1usize, 1usize)
          }
        } else {
          (1usize, 1usize)
        };

      if swap_grid_axes {
        if css_row_subgrid {
          if css_row_line_names_omitted {
            taffy_style.subgrid_column_names =
              vec![Vec::<String>::new(); parent_physical_column_line_count];
          } else if !style.subgrid_row_line_names.is_empty() {
            taffy_style.subgrid_column_names = style.subgrid_row_line_names.clone();
          }
        }
        if css_column_subgrid {
          if css_column_line_names_omitted {
            taffy_style.subgrid_row_names =
              vec![Vec::<String>::new(); parent_physical_row_line_count];
          } else if !style.subgrid_column_line_names.is_empty() {
            taffy_style.subgrid_row_names = style.subgrid_column_line_names.clone();
          }
        }
      } else {
        if css_column_subgrid {
          if css_column_line_names_omitted {
            taffy_style.subgrid_column_names =
              vec![Vec::<String>::new(); parent_physical_column_line_count];
          } else if !style.subgrid_column_line_names.is_empty() {
            taffy_style.subgrid_column_names = style.subgrid_column_line_names.clone();
          }
        }
        if css_row_subgrid {
          if css_row_line_names_omitted {
            taffy_style.subgrid_row_names =
              vec![Vec::<String>::new(); parent_physical_row_line_count];
          } else if !style.subgrid_row_line_names.is_empty() {
            taffy_style.subgrid_row_names = style.subgrid_row_line_names.clone();
          }
        }
      }

      // Line names.
      if swap_grid_axes {
        if !style.grid_row_line_names.is_empty() {
          taffy_style.grid_template_column_names = style.grid_row_line_names.clone();
        }
        if !style.grid_column_line_names.is_empty() {
          taffy_style.grid_template_row_names = style.grid_column_line_names.clone();
        }
      } else {
        if !style.grid_column_line_names.is_empty() {
          taffy_style.grid_template_column_names = style.grid_column_line_names.clone();
        }
        if !style.grid_row_line_names.is_empty() {
          taffy_style.grid_template_row_names = style.grid_row_line_names.clone();
        }
      }
      // Implicit track sizing
      if inline_is_horizontal_container {
        if !style.grid_auto_rows.is_empty() {
          taffy_style.grid_auto_rows = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
        if !style.grid_auto_columns.is_empty() {
          taffy_style.grid_auto_columns = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
      } else {
        if !style.grid_auto_columns.is_empty() {
          taffy_style.grid_auto_rows = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
        if !style.grid_auto_rows.is_empty() {
          taffy_style.grid_auto_columns = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
        }
      }
      taffy_style.grid_auto_flow = match (style.grid_auto_flow, inline_is_horizontal_container) {
        (GridAutoFlow::Row, true) => taffy::style::GridAutoFlow::Row,
        (GridAutoFlow::RowDense, true) => taffy::style::GridAutoFlow::RowDense,
        (GridAutoFlow::Column, true) => taffy::style::GridAutoFlow::Column,
        (GridAutoFlow::ColumnDense, true) => taffy::style::GridAutoFlow::ColumnDense,
        (GridAutoFlow::Row, false) => taffy::style::GridAutoFlow::Column,
        (GridAutoFlow::RowDense, false) => taffy::style::GridAutoFlow::ColumnDense,
        (GridAutoFlow::Column, false) => taffy::style::GridAutoFlow::Row,
        (GridAutoFlow::ColumnDense, false) => taffy::style::GridAutoFlow::RowDense,
      };
      if !style.grid_template_areas.is_empty() {
        if let Some(mut bounds) = validate_area_rectangles(&style.grid_template_areas) {
          let mut entries: Vec<_> = bounds.drain().collect();
          entries.sort_by(|a, b| a.0.cmp(&b.0));
          let mut areas = Vec::with_capacity(entries.len());
          for (name, (top, bottom, left, right)) in entries {
            let (row_start, row_end, column_start, column_end) = if swap_grid_axes {
              (
                (left as u16) + 1,
                (right as u16) + 2,
                (top as u16) + 1,
                (bottom as u16) + 2,
              )
            } else {
              (
                (top as u16) + 1,
                (bottom as u16) + 2,
                (left as u16) + 1,
                (right as u16) + 2,
              )
            };
            areas.push(GridTemplateArea {
              name,
              row_start,
              row_end,
              column_start,
              column_end,
            });
          }
          taffy_style.grid_template_areas = areas;
        }
      }

      // Gap
      taffy_style.gap = taffy::geometry::Size {
        // Column gap follows the inline axis; row gap follows the block axis.
        width: if inline_is_horizontal_container {
          self.convert_gap_length_to_lp(&style.grid_column_gap, style)
        } else {
          self.convert_gap_length_to_lp(&style.grid_row_gap, style)
        },
        height: if inline_is_horizontal_container {
          self.convert_gap_length_to_lp(&style.grid_row_gap, style)
        } else {
          self.convert_gap_length_to_lp(&style.grid_column_gap, style)
        },
      };
      taffy_style.subgrid_gap = taffy::geometry::Size {
        width: if inline_is_horizontal_container {
          self.convert_gap_length_to_lp(&style.grid_column_gap, style)
        } else {
          self.convert_gap_length_to_lp(&style.grid_row_gap, style)
        },
        height: if inline_is_horizontal_container {
          self.convert_gap_length_to_lp(&style.grid_row_gap, style)
        } else {
          self.convert_gap_length_to_lp(&style.grid_column_gap, style)
        },
      };
      taffy_style.subgrid_gap_is_normal = taffy::geometry::Size {
        width: if inline_is_horizontal_container {
          style.grid_column_gap_is_normal
        } else {
          style.grid_row_gap_is_normal
        },
        height: if inline_is_horizontal_container {
          style.grid_row_gap_is_normal
        } else {
          style.grid_column_gap_is_normal
        },
      };

      // Alignment
      if swap_grid_axes {
        taffy_style.align_content =
          Some(self.convert_justify_content(&style.justify_content, inline_positive_container));
        taffy_style.justify_content =
          Some(self.convert_align_content(&style.align_content, block_positive_container));
      } else {
        taffy_style.align_content =
          Some(self.convert_align_content(&style.align_content, block_positive_container));
        taffy_style.justify_content =
          Some(self.convert_justify_content(&style.justify_content, inline_positive_container));
      }
    }
    if inline_is_horizontal_container {
      taffy_style.align_items =
        Some(self.convert_align_items(&style.align_items, block_positive_container));
      taffy_style.justify_items =
        Some(self.convert_align_items(&style.justify_items, inline_positive_container));
    } else {
      taffy_style.align_items =
        Some(self.convert_align_items(&style.justify_items, inline_positive_container));
      taffy_style.justify_items =
        Some(self.convert_align_items(&style.align_items, block_positive_container));
    }

    if inline_is_horizontal_item {
      taffy_style.align_self = style
        .align_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::Y));
      taffy_style.justify_self = style
        .justify_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::X));
    } else {
      taffy_style.align_self = style
        .justify_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::Y));
      taffy_style.justify_self = style
        .align_self
        .map(|a| convert_item_alignment(a, PhysicalAxis::X));
    }

    if let Some(containing_grid) = containing_grid {
      if inline_is_horizontal_item {
        if taffy_style.justify_self.is_none() {
          taffy_style.justify_self = Some(convert_item_alignment(
            containing_grid.justify_items,
            PhysicalAxis::X,
          ));
        }
        if taffy_style.align_self.is_none() {
          taffy_style.align_self = Some(convert_item_alignment(
            containing_grid.align_items,
            PhysicalAxis::Y,
          ));
        }
      } else {
        if taffy_style.align_self.is_none() {
          taffy_style.align_self = Some(convert_item_alignment(
            containing_grid.justify_items,
            PhysicalAxis::Y,
          ));
        }
        if taffy_style.justify_self.is_none() {
          taffy_style.justify_self = Some(convert_item_alignment(
            containing_grid.align_items,
            PhysicalAxis::X,
          ));
        }
      }
    }

    if containing_grid.is_some() {
      // CSS Grid track sizing treats percentage-based preferred sizes as `auto` when the grid
      // area's size is indefinite. (Percentages depend on the final track size, and resolving
      // them early can create a cyclic dependency that collapses sibling tracks; this pattern
      // occurs on apnews.com where a `width: 100%` grid item lives in a `fit-content(<percentage>)`
      // column.)
      //
      // Taffy, however, will eagerly resolve percentage `width` values against whatever available
      // inline size it sees during track sizing. For leaf grid items (non-grid descendants in the
      // Taffy tree), represent percentage widths as `auto` so the track sizing algorithm queries
      // intrinsic sizes instead. These leaf items are re-laid out later via our own formatting
      // contexts; that layout pass still sees the authored percentage `width` and can resolve it
      // against the now-definite grid area width.
      let width_has_percentage = !is_grid_node
        && style
          .width
          .as_ref()
          .is_some_and(crate::style::values::Length::has_percentage);
      let height_has_percentage = !is_grid_node
        && style
          .height
          .as_ref()
          .is_some_and(crate::style::values::Length::has_percentage);
      if width_has_percentage || height_has_percentage {
        // CSS Grid treats percentage preferred sizes as non-auto (resolved later against the grid
        // area size), but we often represent them as `auto` inside the cached Taffy style so that
        // intrinsic track sizing doesn't eagerly resolve them against the full grid container.
        //
        // `stretch` alignment only stretches auto-sized items; when the authored size is a
        // percentage, it must therefore behave like `start` even if we encode it as `auto` for
        // track sizing.
        if width_has_percentage && taffy_style.size.width.tag() == taffy::style::CompactLength::PERCENT_TAG {
          taffy_style.size.width = Dimension::auto();
        }
        if height_has_percentage
          && taffy_style.size.height.tag() == taffy::style::CompactLength::PERCENT_TAG
        {
          taffy_style.size.height = Dimension::auto();
        }
        if width_has_percentage
          && matches!(
            taffy_style.justify_self,
            Some(taffy::style::AlignItems::Stretch)
          )
        {
          taffy_style.justify_self = Some(convert_item_alignment(AlignItems::Start, PhysicalAxis::X));
        }
        if height_has_percentage
          && matches!(
            taffy_style.align_self,
            Some(taffy::style::AlignItems::Stretch)
          )
        {
          taffy_style.align_self = Some(convert_item_alignment(AlignItems::Start, PhysicalAxis::Y));
        }
      }

      // Auto margins override alignment per-axis; map them to self-alignment to keep grid items centered or pushed.
      let inline_start_auto = if inline_is_horizontal_item {
        if inline_positive_item {
          margin_left_auto
        } else {
          margin_right_auto
        }
      } else {
        if inline_positive_item {
          margin_top_auto
        } else {
          margin_bottom_auto
        }
      };
      let inline_end_auto = if inline_is_horizontal_item {
        if inline_positive_item {
          margin_right_auto
        } else {
          margin_left_auto
        }
      } else {
        if inline_positive_item {
          margin_bottom_auto
        } else {
          margin_top_auto
        }
      };

      let block_start_auto = if inline_is_horizontal_item {
        if block_positive_item {
          margin_top_auto
        } else {
          margin_bottom_auto
        }
      } else {
        if block_positive_item {
          margin_left_auto
        } else {
          margin_right_auto
        }
      };
      let block_end_auto = if inline_is_horizontal_item {
        if block_positive_item {
          margin_bottom_auto
        } else {
          margin_top_auto
        }
      } else {
        if block_positive_item {
          margin_right_auto
        } else {
          margin_left_auto
        }
      };

      let justify_override = match (inline_start_auto, inline_end_auto) {
        (true, true) => Some(AlignItems::Center),
        (true, false) => Some(if inline_positive_item {
          AlignItems::FlexEnd
        } else {
          AlignItems::FlexStart
        }),
        (false, true) => Some(if inline_positive_item {
          AlignItems::FlexStart
        } else {
          AlignItems::FlexEnd
        }),
        _ => None,
      };
      if let Some(justify) = justify_override {
        let converted = Some(self.convert_align_items(&justify, inline_positive_item));
        if inline_is_horizontal_item {
          taffy_style.justify_self = converted;
        } else {
          taffy_style.align_self = converted;
        }
      }

      let align_override = match (block_start_auto, block_end_auto) {
        (true, true) => Some(AlignItems::Center),
        (true, false) => Some(if block_positive_item {
          AlignItems::FlexEnd
        } else {
          AlignItems::FlexStart
        }),
        (false, true) => Some(if block_positive_item {
          AlignItems::FlexStart
        } else {
          AlignItems::FlexEnd
        }),
        _ => None,
      };
      if let Some(align) = align_override {
        let converted = Some(self.convert_align_items(&align, block_positive_item));
        if inline_is_horizontal_item {
          taffy_style.align_self = converted;
        } else {
          taffy_style.justify_self = converted;
        }
      }

      // `fit-content` keywords in max-size properties can't be represented directly in cached
      // Taffy styles (they depend on available space). Let the measure callback compute the final
      // size by preventing Taffy from treating the preferred size as definite or stretching the
      // item to the full grid area.
      let width_has_fit_content_max_constraint = style.max_width.is_none()
        && matches!(
          style.max_width_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        );
      let height_has_fit_content_max_constraint = style.max_height.is_none()
        && matches!(
          style.max_height_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        );
      if width_has_fit_content_max_constraint {
        taffy_style.size.width = Dimension::auto();
      }
      if height_has_fit_content_max_constraint {
        taffy_style.size.height = Dimension::auto();
      }

      // Intrinsic size keywords act like non-auto preferred sizes, but we represent them as `auto`
      // in the cached Taffy template so they can be resolved per box instance (or per measure call
      // for fit-content). `stretch` alignment only stretches auto-sized items, so when an intrinsic
      // keyword is specified on an axis, force `stretch` to fall back to `start` to avoid
      // incorrectly stretching the item to fill the grid area.
      let physical_width_positive = if inline_is_horizontal_item {
        inline_positive_item
      } else {
        block_positive_item
      };
      let physical_height_positive = if inline_is_horizontal_item {
        block_positive_item
      } else {
        inline_positive_item
      };
      if (style.width_keyword.is_some() || width_has_fit_content_max_constraint)
        && matches!(
          taffy_style.justify_self,
          Some(taffy::style::AlignItems::Stretch)
        )
      {
        taffy_style.justify_self =
          Some(self.convert_align_items(&AlignItems::Start, physical_width_positive));
      }
      if (style.height_keyword.is_some() || height_has_fit_content_max_constraint)
        && matches!(
          taffy_style.align_self,
          Some(taffy::style::AlignItems::Stretch)
        )
      {
        taffy_style.align_self =
          Some(self.convert_align_items(&AlignItems::Start, physical_height_positive));
      }
    }

    // Grid item properties using raw line numbers.
    let css_grid_column = self.convert_grid_placement(
      CssGridAxis::Column,
      style.grid_column_raw.as_deref(),
      style.grid_column_start,
      style.grid_column_end,
      containing_grid,
    );
    let css_grid_row = self.convert_grid_placement(
      CssGridAxis::Row,
      style.grid_row_raw.as_deref(),
      style.grid_row_start,
      style.grid_row_end,
      containing_grid,
    );
    if inline_is_horizontal_item {
      taffy_style.grid_column = css_grid_column;
      taffy_style.grid_row = css_grid_row;
    } else {
      // Inline axis is vertical in the containing grid; swap row/column placement into Taffy's
      // physical axes.
      taffy_style.grid_column = css_grid_row;
      taffy_style.grid_row = css_grid_column;
    }

    taffy_style
  }

  fn convert_sizing_property_to_dimension_box_sizing(
    &self,
    length: &Option<Length>,
    keyword: &Option<IntrinsicSizeKeyword>,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Dimension {
    if length.is_some() {
      return self.convert_opt_length_to_dimension_box_sizing(length, style, axis);
    }
    if keyword.is_some() {
      // Intrinsic sizing keywords depend on the box's contents and/or available space. Represent
      // them as `auto` in the cached template and resolve per box instance (or inside the measure
      // callback for fit-content).
      return Dimension::auto();
    }
    Dimension::auto()
  }

  /// Converts Option<Length> to Taffy Dimension
  fn convert_opt_length_to_dimension_box_sizing(
    &self,
    length: &Option<Length>,
    style: &ComputedStyle,
    axis: Axis,
  ) -> Dimension {
    match length {
      None => Dimension::auto(),
      Some(len) => self.dimension_for_box_sizing(len, style, axis),
    }
  }

  /// Converts Length to Taffy Dimension
  fn convert_length_to_dimension(&self, length: &Length, style: &ComputedStyle) -> Dimension {
    use crate::style::values::LengthUnit;
    match length.unit {
      LengthUnit::Percent => Dimension::percent(length.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(length, style) {
          Dimension::length(px)
        } else if length.has_percentage() {
          // `calc()` values mixing lengths + percentages cannot be represented in Taffy without a
          // percentage base. Falling back to `Length::to_px()` would treat the percentage term as a
          // raw number and bake a bogus definite pixel size into the cached template.
          Dimension::auto()
        } else {
          Dimension::length(length.to_px())
        }
      }
    }
  }

  /// Converts Option<Length> to Taffy LengthPercentageAuto
  fn convert_opt_length_to_lpa(
    &self,
    length: &Option<Length>,
    style: &ComputedStyle,
  ) -> LengthPercentageAuto {
    use crate::style::values::LengthUnit;
    match length {
      None => LengthPercentageAuto::auto(),
      Some(len) => match len.unit {
        LengthUnit::Percent => LengthPercentageAuto::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            LengthPercentageAuto::length(px)
          } else if len.has_percentage() {
            // Unresolved `calc(<percentage> + <length>)` values cannot be represented directly in
            // Taffy. Avoid `Length::to_px()` (which treats percentages as raw numbers) by
            // resolving with a 0 percentage base so only absolute terms contribute.
            LengthPercentageAuto::length(
              self
                .resolve_length_px_with_base(*len, Some(0.0), style)
                .unwrap_or(0.0),
            )
          } else {
            LengthPercentageAuto::length(len.to_px())
          }
        }
      },
    }
  }

  /// Converts Length to Taffy LengthPercentage
  fn convert_length_to_lp(&self, length: &Length, style: &ComputedStyle) -> LengthPercentage {
    use crate::style::values::LengthUnit;
    match length.unit {
      LengthUnit::Percent => LengthPercentage::percent(length.value / 100.0),
      _ => {
        if let Some(px) = self.resolve_length_px(length, style) {
          LengthPercentage::length(px)
        } else if length.has_percentage() {
          // See note in `convert_opt_length_to_lpa`: treat unresolved percentage terms as 0 instead
          // of falling back to `Length::to_px()`.
          LengthPercentage::length(
            self
              .resolve_length_px_with_base(*length, Some(0.0), style)
              .unwrap_or(0.0)
              .max(0.0),
          )
        } else {
          LengthPercentage::length(length.to_px())
        }
      }
    }
  }

  fn convert_gap_length_to_lp(&self, length: &Length, style: &ComputedStyle) -> LengthPercentage {
    use crate::style::values::LengthUnit;
    // Taffy gap fields are `LengthPercentage`, so we cannot represent `calc(<percentage> + <length>)`.
    // Avoid falling back to `Length::to_px()` for `calc(%)`, which treats percentages as raw
    // numbers and can yield large negative gaps when the percentage base is not definite.
    if length.has_percentage() && (length.unit == LengthUnit::Calc || length.calc.is_some()) {
      return LengthPercentage::length(0.0);
    }
    self.convert_length_to_lp(length, style)
  }

  fn dimension_for_box_sizing(&self, len: &Length, style: &ComputedStyle, axis: Axis) -> Dimension {
    let _ = axis;
    self.convert_length_to_dimension(len, style)
  }

  fn edges_px(&self, style: &ComputedStyle, axis: Axis) -> Option<f32> {
    let resolve_edge = |len: &Length| -> Option<f32> {
      self
        .resolve_length_px(len, style)
        .or_else(|| self.resolve_length_px_with_base(*len, Some(0.0), style))
        .map(|px| if px.is_finite() { px.max(0.0) } else { 0.0 })
    };
    match axis {
      Axis::Horizontal => {
        let p1 = resolve_edge(&style.padding_left)?;
        let p2 = resolve_edge(&style.padding_right)?;
        let b1 = resolve_edge(&style.used_border_left_width())?;
        let b2 = resolve_edge(&style.used_border_right_width())?;
        Some(p1 + p2 + b1 + b2)
      }
      Axis::Vertical => {
        let p1 = resolve_edge(&style.padding_top)?;
        let p2 = resolve_edge(&style.padding_bottom)?;
        let b1 = resolve_edge(&style.used_border_top_width())?;
        let b2 = resolve_edge(&style.used_border_bottom_width())?;
        Some(p1 + p2 + b1 + b2)
      }
    }
  }

  fn resolve_length_px(&self, len: &Length, style: &ComputedStyle) -> Option<f32> {
    use crate::style::values::LengthUnit;
    // `Length::to_px()` intentionally falls back to summing raw coefficients when it can't resolve
    // font/viewport-relative units inside calc(). That is fine for "best effort" debugging, but it
    // breaks layout when we feed those values into Taffy as absolute track/sizing numbers.
    //
    // Modern grid templates frequently use calc/min/max/clamp with viewport units (e.g.
    // `max(180px, calc(100vw - 170px))`). Ensure we resolve those expressions here so Taffy
    // receives concrete track sizes.
    if len.unit == LengthUnit::Calc || len.calc.is_some() {
      return self.resolve_length_px_with_base(*len, None, style);
    }
    let root_metrics = self.font_context.root_font_metrics();
    match len.unit {
      LengthUnit::Calc => {
        if len.has_percentage() {
          return None;
        }
        resolve_length_with_percentage_metrics_and_root_font_metrics(
          *len,
          None,
          self.viewport_size,
          style.font_size,
          style.root_font_size,
          Some(style),
          Some(&self.font_context),
          self.factory.root_font_metrics(),
        )
      }
      LengthUnit::Percent => None,
      _ if len.unit.is_absolute() => Some(len.to_px()),
      unit if unit.is_viewport_relative() => len.resolve_with_viewport_for_writing_mode(
        self.viewport_size.width,
        self.viewport_size.height,
        style.writing_mode,
      ),
      LengthUnit::Rem => Some(len.value * style.root_font_size),
      LengthUnit::Em => Some(len.value * style.font_size),
      LengthUnit::Ex => Some(len.value * style.font_size * 0.5),
      LengthUnit::Ch => Some(len.value * style.font_size * 0.5),
      LengthUnit::Cap => Some(len.value * style.font_size * 0.7),
      LengthUnit::Ic => Some(len.value * style.font_size),
      LengthUnit::Rex => Some(
        len.value
          * root_metrics
            .map(|m| m.root_x_height_px)
            .unwrap_or(style.root_font_size * 0.5),
      ),
      LengthUnit::Rch => Some(
        len.value
          * root_metrics
            .map(|m| m.root_ch_advance_px)
            .unwrap_or(style.root_font_size * 0.5),
      ),
      LengthUnit::Rcap => Some(
        len.value
          * root_metrics
            .map(|m| m.root_cap_height_px)
            .unwrap_or(style.root_font_size * 0.7),
      ),
      LengthUnit::Ric => Some(
        len.value
          * root_metrics
            .map(|m| m.root_ic_advance_px)
            .unwrap_or(style.root_font_size),
      ),
      LengthUnit::Rlh => Some(
        len.value
          * root_metrics
            .map(|m| m.root_used_line_height_px)
            .unwrap_or(style.root_font_size * 1.2),
      ),
      LengthUnit::Lh => resolve_length_with_percentage_metrics(
        *len,
        None,
        self.viewport_size,
        style.font_size,
        style.root_font_size,
        Some(style),
        Some(&self.font_context),
      ),
      _ => None,
    }
  }

  fn length_has_calc_percentage(len: &Length) -> bool {
    use crate::style::values::LengthUnit;
    len.has_percentage() && (len.unit == LengthUnit::Calc || len.calc.is_some())
  }

  fn style_has_calc_percentage_sizing_or_edges(style: &ComputedStyle) -> bool {
    Self::length_has_calc_percentage(&style.padding_left)
      || Self::length_has_calc_percentage(&style.padding_right)
      || Self::length_has_calc_percentage(&style.padding_top)
      || Self::length_has_calc_percentage(&style.padding_bottom)
      || Self::length_has_calc_percentage(&style.used_border_left_width())
      || Self::length_has_calc_percentage(&style.used_border_right_width())
      || Self::length_has_calc_percentage(&style.used_border_top_width())
      || Self::length_has_calc_percentage(&style.used_border_bottom_width())
      || style
        .margin_left
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_right
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_top
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_bottom
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .width
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .height
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .min_width
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .min_height
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .max_width
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .max_height
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
  }

  /// Patch root sizing + edge properties that contain `calc()` expressions with percentage terms.
  ///
  /// Taffy supports raw percentages for many style fields but does not support arbitrary `calc()`
  /// expressions. When the percentage base is unknown during cached template conversion, falling
  /// back to `Length::to_px()` can bake bogus/negative definite pixel values into the style (because
  /// `Length::to_px()` treats unresolved percentages as raw numbers).
  ///
  /// Instead:
  /// - sizing properties (`width/height/min/max`) fall back to `auto` when the base is unknown, and
  ///   are resolved here when the root container has a definite base available.
  /// - padding/border/margin fall back to `0px` when the base is unknown, and are resolved here for
  ///   the root container when the containing block width is definite.
  fn patch_root_calc_percentage_sizing_and_edges(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
  ) -> Result<(), LayoutError> {
    use crate::style::values::LengthUnit;

    if !Self::style_has_calc_percentage_sizing_or_edges(style) {
      return Ok(());
    }

    let Ok(existing) = taffy.style(root_id) else {
      return Ok(());
    };

    // Percentage widths resolve against the containing block's width. For horizontal writing modes
    // we additionally thread a stable `inline_percentage_base` even when `available_width` is
    // intrinsic/indefinite so percentages do not fall back to the viewport.
    let cb_width = constraints
      .width()
      .filter(|w| w.is_finite() && *w >= 0.0)
      .or_else(|| {
        if crate::style::inline_axis_is_horizontal(style.writing_mode) {
          constraints
            .inline_percentage_base
            .filter(|w| w.is_finite() && *w >= 0.0)
        } else {
          None
        }
      });
    // Percentage block sizes resolve against a *definite* containing block size (CSS2.1 §10.5).
    // `LayoutConstraints::block_percentage_base` captures that definiteness without accidentally
    // using a viewport-derived available height.
    let cb_height = constraints
      .block_percentage_base
      .filter(|h| h.is_finite() && *h >= 0.0);

    let resolve = |len: Length, base: Option<f32>| -> Option<f32> {
      self
        .resolve_length_px_with_base(len, base.filter(|b| b.is_finite()), style)
        .filter(|v| v.is_finite())
    };

    let mut updated = existing.clone();
    let mut changed = false;

    // Patch edge properties first so cached template conversion never leaks bogus `calc(%)`
    // resolution into layout.
    let mut patch_edge = |len: Length, base: Option<f32>, slot: &mut LengthPercentage| {
      if len.unit == LengthUnit::Calc && len.has_percentage() {
        let px = resolve(len, base).unwrap_or(0.0).max(0.0);
        let next = LengthPercentage::length(px);
        if *slot != next {
          *slot = next;
          changed = true;
        }
      }
    };
    patch_edge(style.padding_left, cb_width, &mut updated.padding.left);
    patch_edge(style.padding_right, cb_width, &mut updated.padding.right);
    patch_edge(style.padding_top, cb_width, &mut updated.padding.top);
    patch_edge(style.padding_bottom, cb_width, &mut updated.padding.bottom);

    patch_edge(
      style.used_border_left_width(),
      cb_width,
      &mut updated.border.left,
    );
    patch_edge(
      style.used_border_right_width(),
      cb_width,
      &mut updated.border.right,
    );
    patch_edge(
      style.used_border_top_width(),
      cb_width,
      &mut updated.border.top,
    );
    patch_edge(
      style.used_border_bottom_width(),
      cb_width,
      &mut updated.border.bottom,
    );

    let mut patch_margin =
      |len: Option<Length>, base: Option<f32>, slot: &mut LengthPercentageAuto| {
        let Some(len) = len else { return };
        if len.unit == LengthUnit::Calc && len.has_percentage() {
          let px = resolve(len, base).unwrap_or(0.0);
          let next = LengthPercentageAuto::length(px);
          if *slot != next {
            *slot = next;
            changed = true;
          }
        }
      };
    patch_margin(style.margin_left, cb_width, &mut updated.margin.left);
    patch_margin(style.margin_right, cb_width, &mut updated.margin.right);
    patch_margin(style.margin_top, cb_width, &mut updated.margin.top);
    patch_margin(style.margin_bottom, cb_width, &mut updated.margin.bottom);

    let resolve_dimension = |len: &Length, base: Option<f32>| -> Dimension {
      let Some(px) = resolve(*len, base) else {
        return Dimension::auto();
      };
      Dimension::length(px.max(0.0))
    };

    if constraints.used_border_box_width.is_none() {
      if let Some(len) = style.width.as_ref() {
        if len.unit == LengthUnit::Calc && len.has_percentage() {
          let next = resolve_dimension(len, cb_width);
          if updated.size.width != next {
            updated.size.width = next;
            changed = true;
          }
        }
      }
    }
    if constraints.used_border_box_height.is_none() {
      if let Some(len) = style.height.as_ref() {
        if len.unit == LengthUnit::Calc && len.has_percentage() {
          let next = resolve_dimension(len, cb_height);
          if updated.size.height != next {
            updated.size.height = next;
            changed = true;
          }
        }
      }
    }

    if let Some(len) = style.min_width.as_ref() {
      if len.unit == LengthUnit::Calc && len.has_percentage() {
        let next = resolve_dimension(len, cb_width);
        if updated.min_size.width != next {
          updated.min_size.width = next;
          changed = true;
        }
      }
    }
    if let Some(len) = style.min_height.as_ref() {
      if len.unit == LengthUnit::Calc && len.has_percentage() {
        let next = resolve_dimension(len, cb_height);
        if updated.min_size.height != next {
          updated.min_size.height = next;
          changed = true;
        }
      }
    }
    if let Some(len) = style.max_width.as_ref() {
      if len.unit == LengthUnit::Calc && len.has_percentage() {
        let next = resolve_dimension(len, cb_width);
        if updated.max_size.width != next {
          updated.max_size.width = next;
          changed = true;
        }
      }
    }
    if let Some(len) = style.max_height.as_ref() {
      if len.unit == LengthUnit::Calc && len.has_percentage() {
        let next = resolve_dimension(len, cb_height);
        if updated.max_size.height != next {
          updated.max_size.height = next;
          changed = true;
        }
      }
    }

    if changed {
      taffy
        .set_style(root_id, updated)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    Ok(())
  }

  /// Patch percentage block-size properties on the grid root.
  ///
  /// Taffy resolves percentage preferred sizes against the available space passed to the root.
  /// For in-flow elements, CSS2.1 §10.5 requires percentage block sizes (e.g. `height: 100%` in
  /// horizontal writing modes) to compute to `auto` unless the containing block has a definite
  /// size. FastRender threads the containing block's *definite* block size separately via
  /// `LayoutConstraints::block_percentage_base`, so use that here:
  /// - When the base is absent, treat percentage block sizes as `auto`.
  /// - When the base is present but the root's available space is intrinsic/indefinite, resolve the
  ///   percentage into a concrete length so Taffy still sees a definite preferred size.
  fn patch_root_percentage_block_size(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
  ) -> Result<(), LayoutError> {
    // Only apply to in-flow elements: absolutely/fixed-positioned boxes are allowed to resolve
    // percentage heights against their containing block even when that height depends on content.
    if !style.position.is_in_flow() {
      return Ok(());
    }

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let (block_len, used_block_border_box) = if inline_is_horizontal {
      (style.height, constraints.used_border_box_height)
    } else {
      (style.width, constraints.used_border_box_width)
    };
    if used_block_border_box.is_some() {
      return Ok(());
    }
    let Some(block_len) = block_len else {
      return Ok(());
    };
    if !block_len.has_percentage() {
      return Ok(());
    }

    let Ok(existing) = taffy.style(root_id) else {
      return Ok(());
    };

    let base = constraints
      .block_percentage_base
      .filter(|b| b.is_finite() && *b >= 0.0);

    let mut updated = existing.clone();
    let mut changed = false;
    let mut patch = |slot: &mut Dimension| {
      let next = match base.and_then(|base| {
        self
          .resolve_length_px_with_base(block_len, Some(base), style)
          .filter(|v| v.is_finite())
      }) {
        Some(px) => Dimension::length(px.max(0.0)),
        None => Dimension::auto(),
      };
      if *slot != next {
        *slot = next;
        changed = true;
      }
    };

    if inline_is_horizontal {
      patch(&mut updated.size.height);
    } else {
      patch(&mut updated.size.width);
    }

    if changed {
      taffy
        .set_style(root_id, updated)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    Ok(())
  }

  fn grid_track_has_calc_percentage(track: &GridTrack) -> bool {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => len.unit == LengthUnit::Calc && len.has_percentage(),
      // `fit-content(<percentage>)` needs the grid container size to resolve the percentage clamp.
      // When the container size is treated as indefinite, Taffy falls back to an infinite clamp,
      // allowing the track to consume all available space and collapsing sibling tracks.
      //
      // Patch the template to resolve these percent clamps against a definite content-box size
      // when we have one (otherwise treat as `auto`, per CSS Grid rules for percentage tracks).
      GridTrack::FitContent(len) => {
        (len.unit == LengthUnit::Calc && len.has_percentage()) || len.unit == LengthUnit::Percent
      }
      GridTrack::MinMax(min, max) => {
        Self::grid_track_has_calc_percentage(min) || Self::grid_track_has_calc_percentage(max)
      }
      GridTrack::RepeatAutoFill { tracks, .. } | GridTrack::RepeatAutoFit { tracks, .. } => {
        tracks.iter().any(Self::grid_track_has_calc_percentage)
      }
      GridTrack::Auto | GridTrack::Fr(_) | GridTrack::MinContent | GridTrack::MaxContent => false,
    }
  }

  fn style_has_calc_percentage_tracks(style: &ComputedStyle) -> bool {
    style
      .grid_template_columns
      .iter()
      .chain(style.grid_template_rows.iter())
      .any(Self::grid_track_has_calc_percentage)
      || style
        .grid_auto_columns
        .iter()
        .chain(style.grid_auto_rows.iter())
        .any(Self::grid_track_has_calc_percentage)
  }

  fn style_has_calc_percentage_gaps(style: &ComputedStyle) -> bool {
    use crate::style::values::LengthUnit;
    let has_calc_percentage =
      |len: &Length| len.has_percentage() && (len.unit == LengthUnit::Calc || len.calc.is_some());
    has_calc_percentage(&style.grid_row_gap) || has_calc_percentage(&style.grid_column_gap)
  }

  /// Patch root grid track definitions (and gap sizes) so percentage-based sizing functions that
  /// Taffy cannot directly resolve are converted into absolute pixel lengths when the grid
  /// container’s content box size is definite.
  ///
  /// This handles:
  /// - `calc()` expressions mixing percentages and lengths (Taffy does not support these directly).
  /// - `fit-content(<percentage>)` clamps, which Taffy resolves via the container size; if the
  ///   container size is treated as indefinite, the clamp becomes infinite and can collapse
  ///   sibling tracks.
  ///
  /// When we know the grid container’s definite content-box size in a given axis, resolve these
  /// tracks to an absolute pixel length. When the base is not definite, treat them as `auto`
  /// (CSS Grid treats percentages as auto when the container size is indefinite).
  fn patch_root_calc_percentage_tracks(
    &self,
    taffy: &mut TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    style: &ComputedStyle,
    constraints: &LayoutConstraints,
  ) -> Result<(), LayoutError> {
    let has_tracks = Self::style_has_calc_percentage_tracks(style);
    let has_gaps = Self::style_has_calc_percentage_gaps(style);
    if !has_tracks && !has_gaps {
      return Ok(());
    }

    let Ok(existing) = taffy.style(root_id) else {
      return Ok(());
    };

    let cb_width = constraints
      .width()
      .filter(|w| w.is_finite() && *w >= 0.0)
      .or_else(|| {
        if crate::style::inline_axis_is_horizontal(style.writing_mode) {
          constraints
            .inline_percentage_base
            .filter(|w| w.is_finite() && *w >= 0.0)
        } else {
          None
        }
      });
    let cb_height = constraints.height().filter(|h| h.is_finite() && *h >= 0.0);

    let border_box_width = constraints
      .used_border_box_width
      .filter(|w| w.is_finite() && *w >= 0.0)
      .or_else(|| {
        existing
          .size
          .width
          .into_option()
          .filter(|w| w.is_finite() && *w >= 0.0)
      })
      .or_else(|| {
        if existing.size.width.tag() == taffy::style::CompactLength::PERCENT_TAG {
          cb_width
            .map(|base| existing.size.width.value() * base)
            .filter(|w| w.is_finite() && *w >= 0.0)
        } else {
          None
        }
      })
      .or(cb_width);
    let border_box_height = constraints
      .used_border_box_height
      .filter(|h| h.is_finite() && *h >= 0.0)
      .or_else(|| {
        existing
          .size
          .height
          .into_option()
          .filter(|h| h.is_finite() && *h >= 0.0)
      })
      .or_else(|| {
        if existing.size.height.tag() == taffy::style::CompactLength::PERCENT_TAG {
          cb_height
            .map(|base| existing.size.height.value() * base)
            .filter(|h| h.is_finite() && *h >= 0.0)
        } else {
          None
        }
      })
      .or(cb_height);

    // Padding percentages resolve against the containing block's width, so use the (physical)
    // containing block width as the percentage base when we have one.
    let padding_percentage_base = cb_width.unwrap_or(0.0);
    let padding_left =
      self.resolve_length_for_width(style.padding_left, padding_percentage_base, style);
    let padding_right =
      self.resolve_length_for_width(style.padding_right, padding_percentage_base, style);
    let padding_top =
      self.resolve_length_for_width(style.padding_top, padding_percentage_base, style);
    let padding_bottom =
      self.resolve_length_for_width(style.padding_bottom, padding_percentage_base, style);
    let border_left = self.resolve_length_for_width(
      style.used_border_left_width(),
      padding_percentage_base,
      style,
    );
    let border_right = self.resolve_length_for_width(
      style.used_border_right_width(),
      padding_percentage_base,
      style,
    );
    let border_top = self.resolve_length_for_width(
      style.used_border_top_width(),
      padding_percentage_base,
      style,
    );
    let border_bottom = self.resolve_length_for_width(
      style.used_border_bottom_width(),
      padding_percentage_base,
      style,
    );

    let content_width_base = border_box_width
      .map(|w| (w - padding_left - padding_right - border_left - border_right).max(0.0));
    let content_height_base = border_box_height
      .map(|h| (h - padding_top - padding_bottom - border_top - border_bottom).max(0.0));

    let swap_grid_axes = existing.axes_swapped;
    let inline_is_horizontal = !swap_grid_axes;
    let inline_gap_base = if inline_is_horizontal {
      content_width_base
    } else {
      content_height_base
    }
    .filter(|b| b.is_finite() && *b >= 0.0);
    let template_columns = if swap_grid_axes {
      &style.grid_template_rows
    } else {
      &style.grid_template_columns
    };
    let template_rows = if swap_grid_axes {
      &style.grid_template_columns
    } else {
      &style.grid_template_rows
    };

    let mut updated = existing.clone();
    let mut changed = false;

    if has_gaps {
      use crate::style::values::LengthUnit;
      let resolve_gap_calc = |len: Length| -> Option<f32> {
        if len.unit != LengthUnit::Calc || !len.has_percentage() {
          return None;
        }
        self.resolve_length_px_with_base(len, inline_gap_base, style)
      };
      let clamp_gap = |px: f32| -> LengthPercentage {
        if px.is_finite() {
          LengthPercentage::length(px.max(0.0))
        } else {
          LengthPercentage::length(0.0)
        }
      };

      let gap_width_len = if inline_is_horizontal {
        style.grid_column_gap
      } else {
        style.grid_row_gap
      };
      let gap_height_len = if inline_is_horizontal {
        style.grid_row_gap
      } else {
        style.grid_column_gap
      };

      if gap_width_len.unit == LengthUnit::Calc && gap_width_len.has_percentage() {
        let px = resolve_gap_calc(gap_width_len).unwrap_or(0.0);
        let next = clamp_gap(px);
        if updated.gap.width != next {
          updated.gap.width = next;
          updated.subgrid_gap.width = next;
          changed = true;
        }
      }
      if gap_height_len.unit == LengthUnit::Calc && gap_height_len.has_percentage() {
        let px = resolve_gap_calc(gap_height_len).unwrap_or(0.0);
        let next = clamp_gap(px);
        if updated.gap.height != next {
          updated.gap.height = next;
          updated.subgrid_gap.height = next;
          changed = true;
        }
      }
    }

    if has_tracks {
      let next_cols =
        self.convert_grid_template_with_percentage_base(template_columns, style, content_width_base);
      if updated.grid_template_columns != next_cols {
        updated.grid_template_columns = next_cols;
        changed = true;
      }
      let next_rows =
        self.convert_grid_template_with_percentage_base(template_rows, style, content_height_base);
      if updated.grid_template_rows != next_rows {
        updated.grid_template_rows = next_rows;
        changed = true;
      }
    }

    if has_tracks && (!style.grid_auto_columns.is_empty() || !style.grid_auto_rows.is_empty()) {
      if swap_grid_axes {
        if !style.grid_auto_rows.is_empty() {
          let next = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, content_width_base))
            .collect::<Vec<_>>();
          if updated.grid_auto_columns != next {
            updated.grid_auto_columns = next;
            changed = true;
          }
        }
        if !style.grid_auto_columns.is_empty() {
          let next = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, content_height_base))
            .collect::<Vec<_>>();
          if updated.grid_auto_rows != next {
            updated.grid_auto_rows = next;
            changed = true;
          }
        }
      } else {
        if !style.grid_auto_columns.is_empty() {
          let next = style
            .grid_auto_columns
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, content_width_base))
            .collect::<Vec<_>>();
          if updated.grid_auto_columns != next {
            updated.grid_auto_columns = next;
            changed = true;
          }
        }
        if !style.grid_auto_rows.is_empty() {
          let next = style
            .grid_auto_rows
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, content_height_base))
            .collect::<Vec<_>>();
          if updated.grid_auto_rows != next {
            updated.grid_auto_rows = next;
            changed = true;
          }
        }
      }
    }

    if changed {
      taffy
        .set_style(root_id, updated)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    }

    Ok(())
  }

  /// Converts GridTrack Vec to Taffy track list
  fn convert_grid_template(
    &self,
    tracks: &[GridTrack],
    style: &ComputedStyle,
  ) -> Vec<GridTemplateComponent<String>> {
    let mut components = Vec::new();
    for track in tracks {
      match track {
        GridTrack::RepeatAutoFill {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFill,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        GridTrack::RepeatAutoFit {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size(t, style))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFit,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        _ => components.push(GridTemplateComponent::Single(
          self.convert_track_size(track, style),
        )),
      }
    }
    components
  }

  fn convert_grid_template_with_percentage_base(
    &self,
    tracks: &[GridTrack],
    style: &ComputedStyle,
    percentage_base: Option<f32>,
  ) -> Vec<GridTemplateComponent<String>> {
    let mut components = Vec::new();
    for track in tracks {
      match track {
        GridTrack::RepeatAutoFill {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, percentage_base))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFill,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        GridTrack::RepeatAutoFit {
          tracks: inner,
          line_names,
        } => {
          let converted: Vec<TrackSizingFunction> = inner
            .iter()
            .map(|t| self.convert_track_size_with_percentage_base(t, style, percentage_base))
            .collect();
          let repetition = GridTemplateRepetition {
            count: RepetitionCount::AutoFit,
            tracks: converted,
            line_names: line_names.clone(),
          };
          components.push(GridTemplateComponent::Repeat(repetition));
        }
        _ => components.push(GridTemplateComponent::Single(
          self.convert_track_size_with_percentage_base(track, style, percentage_base),
        )),
      }
    }
    components
  }

  fn convert_length_to_lp_with_percentage_base(
    &self,
    length: &Length,
    style: &ComputedStyle,
    percentage_base: Option<f32>,
  ) -> Option<LengthPercentage> {
    use crate::style::values::LengthUnit;
    match length.unit {
      LengthUnit::Percent => percentage_base
        .map(|base| LengthPercentage::length((base * (length.value / 100.0)).max(0.0))),
      LengthUnit::Calc if length.has_percentage() => self
        .resolve_length_px_with_base(*length, percentage_base, style)
        .map(LengthPercentage::length),
      _ => self
        .resolve_length_px_with_base(*length, None, style)
        .or_else(|| self.resolve_length_px_with_base(*length, percentage_base, style))
        .or_else(|| (!length.has_percentage()).then(|| length.to_px()))
        .map(LengthPercentage::length),
    }
  }

  fn convert_track_size_with_percentage_base(
    &self,
    track: &GridTrack,
    style: &ComputedStyle,
    percentage_base: Option<f32>,
  ) -> TrackSizingFunction {
    match track {
      GridTrack::Length(len) => {
        match self.convert_length_to_lp_with_percentage_base(len, style, percentage_base) {
          Some(lp) => TrackSizingFunction::from(lp),
          None => TrackSizingFunction::AUTO,
        }
      }
      GridTrack::MinContent => TrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => TrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        match self.convert_length_to_lp_with_percentage_base(len, style, percentage_base) {
          Some(lp) => TrackSizingFunction::fit_content(lp),
          None => TrackSizingFunction::AUTO,
        }
      }
      GridTrack::Fr(fr) => TrackSizingFunction {
        min: MinTrackSizingFunction::AUTO,
        max: MaxTrackSizingFunction::fr(*fr),
      },
      GridTrack::Auto => TrackSizingFunction::AUTO,
      GridTrack::MinMax(min, max) => {
        let min_fn = self.convert_min_track_with_percentage_base(min, style, percentage_base);
        let max_fn = self.convert_max_track_with_percentage_base(max, style, percentage_base);
        TrackSizingFunction {
          min: min_fn,
          max: max_fn,
        }
      }
      GridTrack::RepeatAutoFill { .. } | GridTrack::RepeatAutoFit { .. } => {
        TrackSizingFunction::AUTO
      }
    }
  }

  fn convert_min_track_with_percentage_base(
    &self,
    track: &GridTrack,
    style: &ComputedStyle,
    percentage_base: Option<f32>,
  ) -> MinTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MinTrackSizingFunction::percent(len.value / 100.0),
        LengthUnit::Calc if len.has_percentage() => self
          .resolve_length_px_with_base(*len, percentage_base, style)
          .map(MinTrackSizingFunction::length)
          .unwrap_or_else(MinTrackSizingFunction::auto),
        _ => self
          .resolve_length_px_with_base(*len, None, style)
          .or_else(|| self.resolve_length_px_with_base(*len, percentage_base, style))
          .or_else(|| (!len.has_percentage()).then(|| len.to_px()))
          .map(MinTrackSizingFunction::length)
          .unwrap_or_else(MinTrackSizingFunction::auto),
      },
      GridTrack::MinContent => MinTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MinTrackSizingFunction::MAX_CONTENT,
      GridTrack::Auto => MinTrackSizingFunction::auto(),
      _ => MinTrackSizingFunction::auto(),
    }
  }

  fn convert_max_track_with_percentage_base(
    &self,
    track: &GridTrack,
    style: &ComputedStyle,
    percentage_base: Option<f32>,
  ) -> MaxTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MaxTrackSizingFunction::percent(len.value / 100.0),
        LengthUnit::Calc if len.has_percentage() => self
          .resolve_length_px_with_base(*len, percentage_base, style)
          .map(MaxTrackSizingFunction::length)
          .unwrap_or_else(MaxTrackSizingFunction::auto),
        _ => self
          .resolve_length_px_with_base(*len, None, style)
          .or_else(|| self.resolve_length_px_with_base(*len, percentage_base, style))
          .or_else(|| (!len.has_percentage()).then(|| len.to_px()))
          .map(MaxTrackSizingFunction::length)
          .unwrap_or_else(MaxTrackSizingFunction::auto),
      },
      GridTrack::Fr(fr) => MaxTrackSizingFunction::fr(*fr),
      GridTrack::MinContent => MaxTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MaxTrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        match self.convert_length_to_lp_with_percentage_base(len, style, percentage_base) {
          Some(lp) => MaxTrackSizingFunction::fit_content(lp),
          None => MaxTrackSizingFunction::auto(),
        }
      }
      GridTrack::Auto
      | GridTrack::MinMax(..)
      | GridTrack::RepeatAutoFill { .. }
      | GridTrack::RepeatAutoFit { .. } => MaxTrackSizingFunction::auto(),
    }
  }

  /// Converts a single GridTrack to TrackSizingFunction
  fn convert_track_size(&self, track: &GridTrack, style: &ComputedStyle) -> TrackSizingFunction {
    match track {
      GridTrack::Length(len) => {
        let lp = self.convert_length_to_lp(len, style);
        TrackSizingFunction::from(lp)
      }
      GridTrack::MinContent => TrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => TrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        TrackSizingFunction::fit_content(self.convert_length_to_lp(len, style))
      }
      GridTrack::Fr(fr) => TrackSizingFunction {
        min: MinTrackSizingFunction::AUTO,
        max: MaxTrackSizingFunction::fr(*fr),
      },
      GridTrack::Auto => TrackSizingFunction::AUTO,
      GridTrack::MinMax(min, max) => {
        let min_fn = self.convert_min_track(min, style);
        let max_fn = self.convert_max_track(max, style);
        TrackSizingFunction {
          min: min_fn,
          max: max_fn,
        }
      }
      GridTrack::RepeatAutoFill { .. } | GridTrack::RepeatAutoFit { .. } => {
        TrackSizingFunction::AUTO
      }
    }
  }

  /// Converts GridTrack to MinTrackSizingFunction
  fn convert_min_track(&self, track: &GridTrack, style: &ComputedStyle) -> MinTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MinTrackSizingFunction::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            MinTrackSizingFunction::length(px)
          } else if len.has_percentage() {
            // `calc()` lengths containing percentages require a percentage base. When the base is
            // unknown (cached template conversion), treat them as `auto` instead of falling back to
            // `Length::to_px()` (which treats percentages as raw numbers).
            MinTrackSizingFunction::auto()
          } else {
            MinTrackSizingFunction::length(len.to_px())
          }
        }
      },
      GridTrack::MinContent => MinTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MinTrackSizingFunction::MAX_CONTENT,
      GridTrack::Auto => MinTrackSizingFunction::auto(),
      _ => MinTrackSizingFunction::auto(),
    }
  }

  /// Converts GridTrack to MaxTrackSizingFunction
  fn convert_max_track(&self, track: &GridTrack, style: &ComputedStyle) -> MaxTrackSizingFunction {
    use crate::style::values::LengthUnit;
    match track {
      GridTrack::Length(len) => match len.unit {
        LengthUnit::Percent => MaxTrackSizingFunction::percent(len.value / 100.0),
        _ => {
          if let Some(px) = self.resolve_length_px(len, style) {
            MaxTrackSizingFunction::length(px)
          } else if len.has_percentage() {
            // `calc()` lengths containing percentages require a percentage base. When the base is
            // unknown (cached template conversion), treat them as `auto` instead of falling back to
            // `Length::to_px()` (which treats percentages as raw numbers).
            MaxTrackSizingFunction::auto()
          } else {
            MaxTrackSizingFunction::length(len.to_px())
          }
        }
      },
      GridTrack::Fr(fr) => MaxTrackSizingFunction::fr(*fr),
      GridTrack::MinContent => MaxTrackSizingFunction::MIN_CONTENT,
      GridTrack::MaxContent => MaxTrackSizingFunction::MAX_CONTENT,
      GridTrack::FitContent(len) => {
        MaxTrackSizingFunction::fit_content(self.convert_length_to_lp(len, style))
      }
      GridTrack::Auto
      | GridTrack::MinMax(..)
      | GridTrack::RepeatAutoFill { .. }
      | GridTrack::RepeatAutoFit { .. } => MaxTrackSizingFunction::auto(),
    }
  }

  /// Converts grid placements (with optional named lines) to Taffy Line<GridPlacement>
  fn convert_grid_placement(
    &self,
    axis: CssGridAxis,
    raw: Option<&str>,
    start: i32,
    end: i32,
    containing_grid: Option<&ComputedStyle>,
  ) -> Line<TaffyGridPlacement<String>> {
    // Resolve a <custom-ident> grid-line against any named grid areas in the containing grid.
    //
    // CSS Grid allows area names (from `grid-template-areas`) to be referenced directly in
    // placement properties (e.g. `grid-row-start: main`). These are conceptually aliases for the
    // synthesized `<name>-start` / `<name>-end` line names. Area names take precedence over
    // explicitly named lines of the same spelling.
    //
    // Spec: https://www.w3.org/TR/css-grid-2/#grid-placement-slot
    let map_named_area = |placement: TaffyGridPlacement<String>, is_start: bool| {
      let Some(grid) = containing_grid else {
        return placement;
      };
      if grid.grid_template_areas.is_empty() {
        return placement;
      }

      match placement {
        TaffyGridPlacement::NamedLine(name, idx) if idx == 1 => {
          let mut has_area = false;
          for row in &grid.grid_template_areas {
            for cell in row {
              if cell.as_ref().is_some_and(|cell_name| cell_name == &name) {
                has_area = true;
                break;
              }
            }
            if has_area {
              break;
            }
          }

          if has_area {
            let suffix = if is_start { "start" } else { "end" };
            TaffyGridPlacement::NamedLine(format!("{name}-{suffix}"), 1)
          } else {
            TaffyGridPlacement::NamedLine(name, idx)
          }
        }
        other => other,
      }
    };

    let line_names = containing_grid.map(|grid| axis.line_names(grid));

    let mut placement = if let Some(raw_str) = raw {
      parse_grid_line_placement_raw(raw_str, line_names)
    } else {
      Line {
        start: if start == 0 {
          TaffyGridPlacement::Auto
        } else {
          TaffyGridPlacement::Line((start as i16).into())
        },
        end: if end == 0 {
          TaffyGridPlacement::Auto
        } else {
          TaffyGridPlacement::Line((end as i16).into())
        },
      }
    };

    placement.start = map_named_area(placement.start, true);
    placement.end = map_named_area(placement.end, false);
    normalize_grid_placement_conflicts(&mut placement);
    placement
  }

  /// Converts AlignContent to Taffy AlignContent
  fn convert_align_content(&self, align: &AlignContent, axis_positive: bool) -> TaffyAlignContent {
    match align {
      AlignContent::Start | AlignContent::FlexStart => {
        if axis_positive {
          TaffyAlignContent::Start
        } else {
          TaffyAlignContent::End
        }
      }
      AlignContent::End | AlignContent::FlexEnd => {
        if axis_positive {
          TaffyAlignContent::End
        } else {
          TaffyAlignContent::Start
        }
      }
      AlignContent::Center => TaffyAlignContent::Center,
      AlignContent::Stretch => TaffyAlignContent::Stretch,
      AlignContent::SpaceBetween => TaffyAlignContent::SpaceBetween,
      AlignContent::SpaceEvenly => TaffyAlignContent::SpaceEvenly,
      AlignContent::SpaceAround => TaffyAlignContent::SpaceAround,
    }
  }

  fn convert_justify_content(
    &self,
    justify: &JustifyContent,
    axis_positive: bool,
  ) -> TaffyAlignContent {
    match justify {
      JustifyContent::Normal => TaffyAlignContent::Stretch,
      JustifyContent::Start | JustifyContent::FlexStart => {
        if axis_positive {
          TaffyAlignContent::Start
        } else {
          TaffyAlignContent::End
        }
      }
      JustifyContent::End | JustifyContent::FlexEnd => {
        if axis_positive {
          TaffyAlignContent::End
        } else {
          TaffyAlignContent::Start
        }
      }
      JustifyContent::Center => TaffyAlignContent::Center,
      JustifyContent::Stretch => TaffyAlignContent::Stretch,
      JustifyContent::SpaceBetween => TaffyAlignContent::SpaceBetween,
      JustifyContent::SpaceAround => TaffyAlignContent::SpaceAround,
      JustifyContent::SpaceEvenly => TaffyAlignContent::SpaceEvenly,
    }
  }

  fn convert_align_items(
    &self,
    align: &AlignItems,
    axis_positive: bool,
  ) -> taffy::style::AlignItems {
    match align {
      AlignItems::Start | AlignItems::SelfStart => {
        if axis_positive {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::End
        }
      }
      AlignItems::End | AlignItems::SelfEnd => {
        if axis_positive {
          taffy::style::AlignItems::End
        } else {
          taffy::style::AlignItems::Start
        }
      }
      AlignItems::FlexStart => taffy::style::AlignItems::FlexStart,
      AlignItems::FlexEnd => taffy::style::AlignItems::FlexEnd,
      AlignItems::Center | AlignItems::AnchorCenter => taffy::style::AlignItems::Center,
      AlignItems::Baseline => taffy::style::AlignItems::Baseline,
      AlignItems::Stretch => taffy::style::AlignItems::Stretch,
    }
  }

  fn convert_aspect_ratio(&self, aspect_ratio: AspectRatio) -> Option<f32> {
    match aspect_ratio {
      AspectRatio::Auto => None,
      AspectRatio::Ratio(ratio) => Some(ratio),
      AspectRatio::AutoRatio(ratio) => Some(ratio),
    }
  }

  fn grid_item_calc_percentage_margin_offset(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    style: &ComputedStyle,
    taffy_style: &taffy::style::Style,
  ) -> Option<(f32, f32)> {
    let has_calc_percentage_margins = style
      .margin_left
      .as_ref()
      .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_right
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_top
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage)
      || style
        .margin_bottom
        .as_ref()
        .is_some_and(Self::length_has_calc_percentage);
    if !has_calc_percentage_margins {
      return None;
    }

    let parent_id = taffy.parent(node_id)?;
    let parent_style = taffy.style(parent_id).ok()?;
    if parent_style.display != Display::Grid {
      return None;
    }

    // Percentages in margins (including block-axis margins) resolve against the containing block's
    // *width*, which for grid items is the physical width of the grid area.
    let base_width = {
      let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(parent_id) else {
        return None;
      };
      let parent_layout = taffy.layout(parent_id).ok()?;
      let child_count = taffy.child_count(parent_id);
      let idx = (0..child_count)
        .find(|&idx| taffy.get_child_id(parent_id, idx) == node_id)?;
      let item = info.items.get(idx)?;

      let col_offsets = compute_track_offsets(
        &info.columns,
        parent_layout.size.width,
        parent_layout.padding.left,
        parent_layout.padding.right,
        parent_layout.border.left,
        parent_layout.border.right,
        parent_style
          .justify_content
          .unwrap_or(TaffyAlignContent::Stretch),
      );
      let (start, end) = grid_area_for_item(&col_offsets, item.column_start, item.column_end)?;
      let width = (end - start).max(0.0);
      if width.is_finite() && width > 1.0 {
        width
      } else {
        return None;
      }
    };

    let resolve_actual = |value: Option<Length>| {
      value.map(|len| self.resolve_length_for_width(len, base_width, style))
    };
    let resolve_taffy = |value: LengthPercentageAuto| {
      let raw = value.into_raw();
      match raw.tag() {
        CompactLength::LENGTH_TAG => Some(raw.value()),
        CompactLength::PERCENT_TAG => Some(raw.value() * base_width),
        CompactLength::AUTO_TAG => None,
        _ => None,
      }
    };

    let calc_left = style
      .margin_left
      .as_ref()
      .is_some_and(Self::length_has_calc_percentage);
    let calc_right = style
      .margin_right
      .as_ref()
      .is_some_and(Self::length_has_calc_percentage);
    let calc_top = style
      .margin_top
      .as_ref()
      .is_some_and(Self::length_has_calc_percentage);
    let calc_bottom = style
      .margin_bottom
      .as_ref()
      .is_some_and(Self::length_has_calc_percentage);

    let dl = if calc_left {
      let actual = resolve_actual(style.margin_left).unwrap_or(0.0);
      let taffy_margin = resolve_taffy(taffy_style.margin.left).unwrap_or(0.0);
      actual - taffy_margin
    } else {
      0.0
    };
    let dr = if calc_right {
      let actual = resolve_actual(style.margin_right).unwrap_or(0.0);
      let taffy = resolve_taffy(taffy_style.margin.right).unwrap_or(0.0);
      actual - taffy
    } else {
      0.0
    };
    let dt = if calc_top {
      let actual = resolve_actual(style.margin_top).unwrap_or(0.0);
      let taffy = resolve_taffy(taffy_style.margin.top).unwrap_or(0.0);
      actual - taffy
    } else {
      0.0
    };
    let db = if calc_bottom {
      let actual = resolve_actual(style.margin_bottom).unwrap_or(0.0);
      let taffy = resolve_taffy(taffy_style.margin.bottom).unwrap_or(0.0);
      actual - taffy
    } else {
      0.0
    };

    let dl = if dl.is_finite() { dl } else { 0.0 };
    let dr = if dr.is_finite() { dr } else { 0.0 };
    let dt = if dt.is_finite() { dt } else { 0.0 };
    let db = if db.is_finite() { db } else { 0.0 };

    let align_x = taffy_style
      .justify_self
      .unwrap_or(taffy::style::AlignItems::Stretch);
    let align_y = taffy_style
      .align_self
      .unwrap_or(taffy::style::AlignItems::Stretch);

    let dx = match align_x {
      taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => -dr,
      taffy::style::AlignItems::Center => (dl - dr) / 2.0,
      _ => dl,
    };
    let dy = match align_y {
      taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => -db,
      taffy::style::AlignItems::Center => (dt - db) / 2.0,
      _ => dt,
    };

    if dx == 0.0 && dy == 0.0 {
      None
    } else {
      Some((dx, dy))
    }
  }

  fn take_matching_measured_fragment(
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    keys: &[MeasureKey],
    width: f32,
    height: f32,
  ) -> Option<FragmentNode> {
    let matched_key = keys.iter().copied().find(|key| {
      measured_fragments.get(key).map_or(false, |fragment| {
        (fragment.bounds.width() - width).abs() < 0.1
          && (fragment.bounds.height() - height).abs() < 0.1
      })
    })?;
    measured_fragments.remove(&matched_key)
  }

  #[allow(clippy::too_many_arguments)]
  fn try_parallel_root_children_conversion(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
    in_flow_children: &[&BoxNode],
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
  ) -> Option<Result<FragmentNode, LayoutError>> {
    let child_count = taffy.child_count(root_id);
    if child_count == 0
      || child_count != in_flow_children.len()
      || !self.parallelism.should_parallelize(child_count)
    {
      return None;
    }

    let mut deadline_counter = 0usize;
    for idx in 0..child_count {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let child_id = taffy.get_child_id(root_id, idx);
      if taffy.child_count(child_id) != 0 {
        return None;
      }
    }

    let root_layout = match taffy.layout(root_id) {
      Ok(layout) => layout,
      Err(e) => {
        return Some(Err(LayoutError::MissingContext(format!(
          "Taffy layout error: {:?}",
          e
        ))));
      }
    };
    let root_style_override = style_override_for(box_node.id);
    let root_style: &ComputedStyle = root_style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());
    let allow_stretch_block_size_override =
      grid_container_allows_stretch_block_size_override(root_style, constraints);
    let fragment_block_axis = fragmentainer_axes_hint()
      .map(|axes| axes.block_axis())
      .unwrap_or(PhysicalAxis::Y);
    let container_block_size = match fragment_block_axis {
      PhysicalAxis::X => root_layout.size.width,
      PhysicalAxis::Y => root_layout.size.height,
    };
    let root_axis_style =
      GridAxisStyle::effective_for_grid_layout_node(taffy, root_id, box_node.style.as_ref());
    let root_inline_is_horizontal = root_axis_style.inline_is_horizontal();
    let mut mirror_x = false;
    let mut mirror_y = false;
    if !root_axis_style.inline_positive() {
      if root_inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !root_axis_style.block_positive() {
      if root_inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }
    let block_axis_mirrored = match fragment_block_axis {
      PhysicalAxis::X => mirror_x,
      PhysicalAxis::Y => mirror_y,
    };

    let mut child_bounds: Vec<Rect> = Vec::with_capacity(child_count);
    let mut reused_fragments: Vec<Option<FragmentNode>> = vec![None; child_count];
    let mut child_skipped: Vec<bool> = vec![false; child_count];
    let mut child_continuation_available: Vec<Option<f32>> = vec![None; child_count];
    for idx in 0..child_count {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let child_id = taffy.get_child_id(root_id, idx);
      let layout = match taffy.layout(child_id) {
        Ok(layout) => layout,
        Err(e) => {
          return Some(Err(LayoutError::MissingContext(format!(
            "Taffy layout error: {:?}",
            e
          ))));
        }
      };
      let mut bounds = Rect::from_xywh(
        layout.location.x,
        layout.location.y,
        layout.size.width,
        layout.size.height,
      );
      let child = in_flow_children[idx];
      let style_override = style_override_for(child.id);
      let style: &ComputedStyle = style_override
        .as_deref()
        .unwrap_or_else(|| child.style.as_ref());
      if let Ok(taffy_style) = taffy.style(child_id) {
        if let Some((dx, dy)) =
          self.grid_item_calc_percentage_margin_offset(taffy, child_id, style, taffy_style)
        {
          let x = bounds.x() + dx;
          let y = bounds.y() + dy;
          if x.is_finite() && y.is_finite() {
            bounds = Rect::from_xywh(x, y, bounds.width(), bounds.height());
          }
        }
      }
      child_bounds.push(bounds);
      let skip_contents = match child.style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          self.content_visibility_auto_has_definite_placeholder(child)
            && auto_unskipped
              .map(|set| !set.contains(&(child as *const BoxNode)))
              .unwrap_or(false)
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      child_skipped[idx] = skip_contents;
      if skip_contents {
        let mut fragment = FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(child.id),
          },
          vec![],
          child.style.clone(),
        );
        attach_fragment_style_for_box(&mut fragment, child);
        reused_fragments[idx] = Some(fragment);
        continue;
      }
    }

    let container_style = taffy.style(root_id).ok();
    let is_grid_style = matches!(
      container_style.map(|style| style.display),
      Some(Display::Grid)
    );

    if fragmentainer_axes_hint().is_some()
      && fragmentainer_block_size_hint()
        .filter(|size| size.is_finite() && *size > 0.0)
        .is_some()
      && container_block_size.is_finite()
    {
      if is_grid_style {
        let axis_style =
          GridAxisStyle::effective_for_grid_layout_node(taffy, root_id, box_node.style.as_ref());
        let area_bounds: Option<Vec<Rect>> = container_style.and_then(|container_style| {
          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
            if info.items.len() != child_count {
              return None;
            }
            let row_offsets = compute_track_offsets(
              &info.rows,
              root_layout.size.height,
              root_layout.padding.top,
              root_layout.padding.bottom,
              root_layout.border.top,
              root_layout.border.bottom,
              container_style
                .align_content
                .unwrap_or(TaffyAlignContent::Stretch),
            );
            let col_offsets = compute_track_offsets(
              &info.columns,
              root_layout.size.width,
              root_layout.padding.left,
              root_layout.padding.right,
              root_layout.border.left,
              root_layout.border.right,
              container_style
                .justify_content
                .unwrap_or(TaffyAlignContent::Stretch),
            );

            let mut out = Vec::with_capacity(child_count);
            for placement in info.items.iter() {
              let (x_start, x_end) =
                grid_area_for_item(&col_offsets, placement.column_start, placement.column_end)?;
              let (y_start, y_end) =
                grid_area_for_item(&row_offsets, placement.row_start, placement.row_end)?;
              out.push(Rect::from_xywh(
                x_start,
                y_start,
                (x_end - x_start).max(0.0),
                (y_end - y_start).max(0.0),
              ));
            }
            return Some(out);
          }
          None
        });

        let mut dummy_children = Vec::with_capacity(child_count);
        for idx in 0..child_count {
          let bounds = area_bounds
            .as_ref()
            .and_then(|bounds| bounds.get(idx))
            .copied()
            .unwrap_or(child_bounds[idx]);
          dummy_children.push(FragmentNode::new_block(bounds, vec![]));
        }
        let root_bounds = Rect::from_xywh(
          root_layout.location.x,
          root_layout.location.y,
          root_layout.size.width,
          root_layout.size.height,
        );
        let mut dummy_root = FragmentNode::new_block(root_bounds, dummy_children);
        if let Err(err) = self.apply_grid_axis_mirroring(
          taffy,
          root_id,
          root_layout,
          &mut dummy_root,
          axis_style,
          &mut deadline_counter,
        ) {
          return Some(Err(err));
        }

        for idx in 0..child_count {
          child_continuation_available[idx] = self.grid_item_continuation_available_block_size(
            dummy_root.children[idx].bounds,
            constraints,
            container_block_size,
          );
        }
      } else {
        for idx in 0..child_count {
          child_continuation_available[idx] = self.grid_item_continuation_available_block_size(
            child_bounds[idx],
            constraints,
            container_block_size,
          );
        }
      }
    }

    for idx in 0..child_count {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let child_id = taffy.get_child_id(root_id, idx);
      if reused_fragments[idx].is_some() || child_continuation_available[idx].is_some() {
        // Continuation fragments need to re-run layout under a reduced fragmentainer size; skip any
        // cached subtree captured during Taffy measurement.
        continue;
      }
      if let Some(keys) = measured_node_keys.get(&child_id) {
        if let Some(mut reused) = Self::take_matching_measured_fragment(
          measured_fragments,
          keys,
          taffy.unrounded_layout(child_id).size.width,
          taffy.unrounded_layout(child_id).size.height,
        ) {
          fragment_clone_profile::record_fragment_reuse_without_clone(CloneSite::GridMeasureReuse);
          let bounds = child_bounds[idx];
          let delta = Point::new(
            bounds.x() - reused.bounds.x(),
            bounds.y() - reused.bounds.y(),
          );
          if let Err(err) = translate_fragment_tree(&mut reused, delta, &mut deadline_counter) {
            return Some(Err(err));
          }
          let child = in_flow_children[idx];
          attach_fragment_style_for_box(&mut reused, child);
          reused_fragments[idx] = Some(reused);
        }
      }
    }

    let mut indices_to_layout: Vec<usize> = Vec::new();
    for (idx, reused) in reused_fragments.iter().enumerate() {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      if reused.is_none() {
        indices_to_layout.push(idx);
      }
    }

    let factory = std::sync::Arc::new(self.factory.clone());
    let fragmentainer_size_hint = fragmentainer_block_size_hint();
    let fragmentainer_axes = fragmentainer_axes_hint();
    let fragmentainer_axes_resolved = fragmentainer_axes.unwrap_or_default();
    let parent_fragmentainer_offset = fragmentainer_block_offset_hint();
    let propagated_fragmentainer_offset_for_bounds = |bounds: Rect| -> Option<f32> {
      if !(parent_fragmentainer_offset.is_finite() && parent_fragmentainer_offset >= 0.0) {
        return None;
      }
      if !(container_block_size.is_finite() && container_block_size >= 0.0) {
        return None;
      }
      let child_block_start_phys = match fragmentainer_axes_resolved.block_axis() {
        PhysicalAxis::Y => bounds.y(),
        PhysicalAxis::X => bounds.x(),
      };
      let child_block_size = match fragmentainer_axes_resolved.block_axis() {
        PhysicalAxis::Y => bounds.height(),
        PhysicalAxis::X => bounds.width(),
      };
      if !(child_block_start_phys.is_finite()
        && child_block_start_phys >= 0.0
        && child_block_size.is_finite()
        && child_block_size >= 0.0)
      {
        return None;
      }
      let child_rel_flow_start =
        if fragmentainer_axes_resolved.block_positive() ^ block_axis_mirrored {
          child_block_start_phys
        } else {
          container_block_size - child_block_start_phys - child_block_size
        };
      if !(child_rel_flow_start.is_finite() && child_rel_flow_start >= 0.0) {
        return None;
      }
      let child_abs_flow_start = parent_fragmentainer_offset + child_rel_flow_start;
      (child_abs_flow_start.is_finite() && child_abs_flow_start >= 0.0)
        .then_some(child_abs_flow_start)
    };

    let deadline = active_deadline();
    let stage = active_stage();
    let heartbeat = active_heartbeat();
    let should_parallelize_children = self.parallelism.should_parallelize(indices_to_layout.len());
    let child_results = if should_parallelize_children {
      indices_to_layout
        .par_iter()
        .map(|&idx| {
          with_deadline(deadline.as_ref(), || {
            let _hb_guard = StageHeartbeatGuard::install(heartbeat);
            let _stage_guard = StageGuard::install(stage);
            factory.debug_record_parallel_work();

            let child = in_flow_children[idx];
            let bounds = child_bounds[idx];
            let continuation_available = child_continuation_available[idx];
            let fc_type = child
              .formatting_context()
              .unwrap_or(FormattingContextType::Block);
            let origin = Point::new(bounds.x(), bounds.y());
            let origin = if origin.x.is_finite() && origin.y.is_finite() {
              origin
            } else {
              Point::ZERO
            };
            let parent_scroll = factory.viewport_scroll();
            let parent_scroll = if parent_scroll.x.is_finite() && parent_scroll.y.is_finite() {
              parent_scroll
            } else {
              Point::ZERO
            };
            let child_scroll = Point::new(parent_scroll.x - origin.x, parent_scroll.y - origin.y);

            let _fragmentainer_hint_guard =
              set_fragmentainer_block_size_hint(fragmentainer_size_hint);
            let _fragmentainer_axes_guard = set_fragmentainer_axes_hint(fragmentainer_axes);
            let _fragmentainer_offset_guard = propagated_fragmentainer_offset_for_bounds(bounds)
              .map(set_fragmentainer_block_offset_hint);

            let child_factory = factory.translated_for_child(origin);
            let fc: Arc<dyn FormattingContext> = if matches!(fc_type, FormattingContextType::Block)
            {
              Arc::new(
                BlockFormattingContext::for_independent_context_root_with_factory(
                  child_factory.clone(),
                ),
              )
            } else {
              child_factory.get(fc_type)
            };

            let (available_width, available_height, force_width, force_height) =
              match fragment_block_axis {
                PhysicalAxis::X => {
                  let available_width = continuation_available.unwrap_or(bounds.width());
                  let force_width = continuation_available
                    .map(|available| available + 0.01 >= bounds.width())
                    .unwrap_or(true);
                  (available_width, bounds.height(), force_width, true)
                }
                PhysicalAxis::Y => {
                  let available_height = continuation_available.unwrap_or(bounds.height());
                  let force_height = continuation_available
                    .map(|available| available + 0.01 >= bounds.height())
                    .unwrap_or(true);
                  (bounds.width(), available_height, true, force_height)
                }
              };
            let inline_percentage_base = if root_axis_style.inline_is_horizontal() {
              bounds.width()
            } else {
              bounds.height()
            };
            let child_constraints = LayoutConstraints::new(
              CrateAvailableSpace::Definite(available_width),
              CrateAvailableSpace::Definite(available_height),
            )
            // Percentage padding/margins on grid items (and their descendants) must resolve against
            // the grid area's definite inline size. Inheriting the parent `inline_percentage_base`
            // can be wrong when the grid container itself is a flex item (where the container's
            // percentage base is the flex container's width).
            .with_inline_percentage_base(Some(inline_percentage_base.max(0.0)));

            let supports_used_border_box = matches!(
              fc_type,
              FormattingContextType::Block
                | FormattingContextType::Flex
                | FormattingContextType::Grid
                | FormattingContextType::Inline
                | FormattingContextType::Table
            );
            let used_border_box_width = Some(if force_width {
              bounds.width()
            } else {
              available_width
            });
            let used_border_box_height = Some(if force_height {
              bounds.height()
            } else {
              available_height
            });

            let mut laid_out =
              FormattingContextFactory::with_viewport_scroll_override(child_scroll, || {
                if supports_used_border_box {
                  let child_constraints = if allow_stretch_block_size_override {
                    child_constraints
                      .with_used_border_box_size(used_border_box_width, used_border_box_height)
                  } else {
                    child_constraints.with_used_border_box_size_for_layout_only(
                      used_border_box_width,
                      used_border_box_height,
                    )
                  };
                  if continuation_available.is_some() {
                    crate::layout::style_override::with_style_override(
                      child.id,
                      child.style.clone(),
                      || fc.layout(child, &child_constraints),
                    )
                  } else {
                    fc.layout(child, &child_constraints)
                  }
                } else if matches!(
                  fc_type,
                  FormattingContextType::Flex | FormattingContextType::Grid
                ) {
                  let mut layout_style = (*child.style).clone();
                  if force_width {
                    layout_style.width = Some(Length::px(bounds.width()));
                    layout_style.width_keyword = None;
                  }
                  if force_height {
                    layout_style.height = Some(Length::px(bounds.height()));
                    layout_style.height_keyword = None;
                  }
                  crate::layout::style_override::with_style_override(
                    child.id,
                    Arc::new(layout_style),
                    || fc.layout(child, &child_constraints),
                  )
                } else {
                  let mut layout_style = (*child.style).clone();
                  if force_width {
                    layout_style.width = Some(Length::px(bounds.width()));
                    layout_style.width_keyword = None;
                  }
                  if force_height {
                    layout_style.height = Some(Length::px(bounds.height()));
                    layout_style.height_keyword = None;
                  }
                  let layout_style = Arc::new(layout_style);
                  if child.id != 0 {
                    crate::layout::style_override::with_style_override(
                      child.id,
                      layout_style,
                      || fc.layout(child, &child_constraints),
                    )
                  } else {
                    let mut layout_child = (*child).clone();
                    layout_child.style = layout_style;
                    fc.layout(&layout_child, &child_constraints)
                  }
                }
              })?;
            let mut translate_deadline_counter = 0usize;
            let delta = Point::new(
              bounds.x() - laid_out.bounds.x(),
              bounds.y() - laid_out.bounds.y(),
            );
            translate_fragment_tree(&mut laid_out, delta, &mut translate_deadline_counter)?;
            laid_out.content = match &child.box_type {
              crate::tree::box_tree::BoxType::Replaced(replaced_box) => FragmentContent::Replaced {
                replaced_type: replaced_box.replaced_type.clone(),
                box_id: Some(child.id),
              },
              _ => FragmentContent::Block {
                box_id: Some(child.id),
              },
            };
            attach_fragment_style_for_box(&mut laid_out, child);
            Ok((idx, laid_out))
          })
        })
        .collect::<Result<Vec<_>, LayoutError>>()
    } else {
      indices_to_layout
        .iter()
        .map(|&idx| {
          with_deadline(deadline.as_ref(), || {
            let _stage_guard = StageGuard::install(stage);

            let child = in_flow_children[idx];
            let bounds = child_bounds[idx];
            let continuation_available = child_continuation_available[idx];
            let fc_type = child
              .formatting_context()
              .unwrap_or(FormattingContextType::Block);
            let origin = Point::new(bounds.x(), bounds.y());
            let origin = if origin.x.is_finite() && origin.y.is_finite() {
              origin
            } else {
              Point::ZERO
            };

            let _fragmentainer_hint_guard =
              set_fragmentainer_block_size_hint(fragmentainer_size_hint);
            let _fragmentainer_axes_guard = set_fragmentainer_axes_hint(fragmentainer_axes);
            let _fragmentainer_offset_guard = propagated_fragmentainer_offset_for_bounds(bounds)
              .map(set_fragmentainer_block_offset_hint);

            let child_factory = factory.translated_for_child(origin);
            let fc: std::sync::Arc<dyn FormattingContext> =
              if matches!(fc_type, FormattingContextType::Block) {
                std::sync::Arc::new(
                  BlockFormattingContext::for_independent_context_root_with_factory(
                    child_factory.clone(),
                  ),
                )
              } else {
                child_factory.get(fc_type)
              };

            let (available_width, available_height, force_width, force_height) =
              match fragment_block_axis {
                PhysicalAxis::X => {
                  let available_width = continuation_available.unwrap_or(bounds.width());
                  let force_width = continuation_available
                    .map(|available| available + 0.01 >= bounds.width())
                    .unwrap_or(true);
                  (available_width, bounds.height(), force_width, true)
                }
                PhysicalAxis::Y => {
                  let available_height = continuation_available.unwrap_or(bounds.height());
                  let force_height = continuation_available
                    .map(|available| available + 0.01 >= bounds.height())
                    .unwrap_or(true);
                  (bounds.width(), available_height, true, force_height)
                }
              };
            let inline_percentage_base = if root_axis_style.inline_is_horizontal() {
              bounds.width()
            } else {
              bounds.height()
            };
            let child_constraints = LayoutConstraints::new(
              CrateAvailableSpace::Definite(available_width),
              CrateAvailableSpace::Definite(available_height),
            )
            // Percentage padding/margins on grid items (and their descendants) must resolve against
            // the grid area's definite inline size. Inheriting the parent `inline_percentage_base`
            // can be wrong when the grid container itself is a flex item (where the container's
            // percentage base is the flex container's width).
            .with_inline_percentage_base(Some(inline_percentage_base.max(0.0)));

            let supports_used_border_box = matches!(
              fc_type,
              FormattingContextType::Block
                | FormattingContextType::Flex
                | FormattingContextType::Grid
                | FormattingContextType::Inline
                | FormattingContextType::Table
            );

            let mut laid_out = if supports_used_border_box {
              let used_border_box_width = Some(if force_width {
                bounds.width()
              } else {
                available_width
              });
              let used_border_box_height = Some(if force_height {
                bounds.height()
              } else {
                available_height
              });
              let child_constraints = if allow_stretch_block_size_override {
                child_constraints
                  .with_used_border_box_size(used_border_box_width, used_border_box_height)
              } else {
                child_constraints.with_used_border_box_size_for_layout_only(
                  used_border_box_width,
                  used_border_box_height,
                )
              };
              if continuation_available.is_some() {
                crate::layout::style_override::with_style_override(
                  child.id,
                  child.style.clone(),
                  || fc.layout(child, &child_constraints),
                )?
              } else {
                fc.layout(child, &child_constraints)?
              }
            } else if matches!(
              fc_type,
              FormattingContextType::Flex | FormattingContextType::Grid
            ) {
              let mut layout_style = (*child.style).clone();
              if force_width {
                layout_style.width = Some(Length::px(bounds.width()));
                layout_style.width_keyword = None;
              }
              if force_height {
                layout_style.height = Some(Length::px(bounds.height()));
                layout_style.height_keyword = None;
              }
              crate::layout::style_override::with_style_override(
                child.id,
                Arc::new(layout_style),
                || fc.layout(child, &child_constraints),
              )?
            } else {
              let mut layout_style = (*child.style).clone();
              if force_width {
                layout_style.width = Some(Length::px(bounds.width()));
                layout_style.width_keyword = None;
              }
              if force_height {
                layout_style.height = Some(Length::px(bounds.height()));
                layout_style.height_keyword = None;
              }
              let layout_style = Arc::new(layout_style);
              if child.id != 0 {
                crate::layout::style_override::with_style_override(child.id, layout_style, || {
                  fc.layout(child, &child_constraints)
                })?
              } else {
                let mut layout_child = (*child).clone();
                layout_child.style = layout_style;
                fc.layout(&layout_child, &child_constraints)?
              }
            };

            let mut translate_deadline_counter = 0usize;
            let delta = Point::new(
              bounds.x() - laid_out.bounds.x(),
              bounds.y() - laid_out.bounds.y(),
            );
            translate_fragment_tree(&mut laid_out, delta, &mut translate_deadline_counter)?;
            laid_out.content = match &child.box_type {
              crate::tree::box_tree::BoxType::Replaced(replaced_box) => FragmentContent::Replaced {
                replaced_type: replaced_box.replaced_type.clone(),
                box_id: Some(child.id),
              },
              _ => FragmentContent::Block {
                box_id: Some(child.id),
              },
            };
            attach_fragment_style_for_box(&mut laid_out, child);
            Ok((idx, laid_out))
          })
        })
        .collect::<Result<Vec<_>, LayoutError>>()
    };
    let mut child_results = match child_results {
      Ok(results) => results,
      Err(err) => return Some(Err(err)),
    };
    // `indices_to_layout` is collected in index order, and Rayon will usually preserve that order
    // when collecting into a Vec. Avoid an unconditional sort (which would allocate in
    // `sort_by_key`) unless we actually observe out-of-order results.
    let mut ordered = true;
    let mut prev_idx: Option<usize> = None;
    for (idx, _) in &child_results {
      if let Some(prev) = prev_idx {
        if *idx <= prev {
          ordered = false;
          break;
        }
      }
      prev_idx = Some(*idx);
    }
    if !ordered {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      child_results.sort_unstable_by_key(|(idx, _)| *idx);
    }

    let mut child_fragments = Vec::with_capacity(child_count);
    let mut child_results = child_results.into_iter();
    let mut next_child = child_results.next();
    for idx in 0..child_count {
      if let Err(err) = check_layout_deadline(&mut deadline_counter) {
        return Some(Err(err));
      }
      let child = in_flow_children[idx];
      let percentage_base = constraints
        .inline_percentage_base
        .filter(|base| base.is_finite())
        .unwrap_or(child_bounds[idx].width());
      if let Some(fragment) = reused_fragments[idx].take() {
        if !child_skipped[idx] {
          let content_size =
            self.content_box_size(&fragment, child.style.as_ref(), percentage_base);
          remembered_size_cache_store(child, content_size);
        }
        child_fragments.push(fragment);
        continue;
      }

      let Some((result_idx, fragment)) = next_child.take() else {
        return Some(Err(LayoutError::MissingContext(
          "Missing parallel grid child fragment".into(),
        )));
      };
      debug_assert_eq!(result_idx, idx, "parallel grid conversion index mismatch");
      if !child_skipped[idx] {
        let content_size = self.content_box_size(&fragment, child.style.as_ref(), percentage_base);
        remembered_size_cache_store(child, content_size);
      }
      child_fragments.push(fragment);
      next_child = child_results.next();
    }

    let bounds = Rect::from_xywh(
      root_layout.location.x,
      root_layout.location.y,
      root_layout.size.width,
      root_layout.size.height,
    );
    let mut fragment = FragmentNode::new_with_style(
      bounds,
      FragmentContent::Block {
        box_id: Some(box_node.id),
      },
      child_fragments,
      box_node.style.clone(),
    );
    attach_fragment_style_for_box(&mut fragment, box_node);
    let has_in_flow_children = !fragment.children.is_empty();

    if is_grid_style {
      let axis_style =
        GridAxisStyle::effective_for_grid_layout_node(taffy, root_id, box_node.style.as_ref());
      if let Err(err) = self.apply_grid_baseline_alignment(
        taffy,
        root_id,
        root_layout,
        child_count,
        &mut fragment,
        &mut deadline_counter,
      ) {
        return Some(Err(err));
      }
      if let Err(err) = self.apply_grid_axis_mirroring(
        taffy,
        root_id,
        root_layout,
        &mut fragment,
        axis_style,
        &mut deadline_counter,
      ) {
        return Some(Err(err));
      }
      if let Some(container_style) = container_style {
        fragment.grid_tracks = grid_track_ranges_for_container(
          taffy,
          root_id,
          root_layout,
          container_style,
          axis_style,
          has_in_flow_children,
        )
        .map(Arc::new);
      }

      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
        if info.items.len() == in_flow_children.len() {
          let mut items = Vec::with_capacity(info.items.len());
          for (idx, placement) in info.items.iter().enumerate() {
            items.push(GridItemFragmentationData {
              box_id: in_flow_children[idx].id,
              row_start: placement.row_start,
              row_end: placement.row_end,
              column_start: placement.column_start,
              column_end: placement.column_end,
            });
          }
          fragment.grid_fragmentation = Some(Arc::new(GridFragmentationInfo { items }));
        }
      }

      // Apply `position: relative` offsets for grid items (CSS 2.1 §9.4.3).
      //
      // Unlike block/flex formatting contexts, grid item placement is computed by Taffy. After
      // obtaining the normal-flow grid position, we must apply relative offsets as a post-pass so
      // they do not affect track sizing.
      let percentage_base = constraints
        .width()
        .or(constraints.inline_percentage_base)
        .filter(|base| base.is_finite())
        .unwrap_or(fragment.bounds.width().max(0.0));
      let (
        padding_left,
        padding_right,
        padding_top,
        padding_bottom,
        border_left,
        border_right,
        border_top,
        border_bottom,
      ) = self.resolved_padding_border_for_measure(&box_node.style, percentage_base);
      let cb_width =
        (fragment.bounds.width() - padding_left - padding_right - border_left - border_right)
          .max(0.0);
      let cb_height =
        (fragment.bounds.height() - padding_top - padding_bottom - border_top - border_bottom)
          .max(0.0);
      let block_base = box_node.style.height.is_some().then_some(cb_height);
      let relative_cb = ContainingBlock::with_viewport_and_bases(
        Rect::new(Point::ZERO, Size::new(cb_width, cb_height)),
        self.viewport_size,
        Some(cb_width),
        block_base,
      )
      .with_writing_mode_and_direction(box_node.style.writing_mode, box_node.style.direction);

      for (child_node, child_fragment) in in_flow_children
        .iter()
        .zip(fragment.children_mut().iter_mut())
      {
        if !child_node.style.position.is_relative() {
          continue;
        }
        let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
          &child_node.style,
          &relative_cb,
          self.viewport_size,
          &self.font_context,
        );
        let offset =
          compute_relative_offset_for_relative(&positioned_style, &relative_cb, &self.font_context);
        if offset.x != 0.0 || offset.y != 0.0 {
          child_fragment.bounds = child_fragment.bounds.translate(offset);
          child_fragment.logical_override = child_fragment
            .logical_override
            .map(|logical| logical.translate(offset));
        }
      }
    }

    Some(Ok(fragment))
  }

  /// Converts Taffy layout results to FragmentNode tree
  fn convert_to_fragments(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    root_id: TaffyNodeId,
    constraints: &LayoutConstraints,
    containing_grid_axis: Option<GridAxisStyle>,
    containing_grid_area: Option<Rect>,
    root_child_continuation_available: Option<&FxHashMap<TaffyNodeId, f32>>,
    auto_unskipped: Option<&FxHashSet<*const BoxNode>>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
    positioned_children: &FxHashMap<TaffyNodeId, Vec<*const BoxNode>>,
    deadline_counter: &mut usize,
  ) -> Result<FragmentNode, LayoutError> {
    let layout = taffy
      .layout(node_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let child_count = taffy.child_count(node_id);

    let node_taffy_style = taffy.style(node_id).ok();
    let is_grid_style = matches!(
      node_taffy_style.as_ref().map(|s| s.display),
      Some(Display::Grid)
    );
    let node_axis_style = if is_grid_style {
      taffy
        .get_node_context(node_id)
        .copied()
        .map(|ptr| unsafe { &*ptr })
        .map(|node| GridAxisStyle::effective_for_grid_container(&node.style, containing_grid_axis))
    } else {
      None
    };
    let child_axis_style = if is_grid_style {
      node_axis_style
    } else {
      containing_grid_axis
    };

    let child_grid_areas: Option<Vec<Rect>> = if is_grid_style {
      node_taffy_style.and_then(|container_style| {
        if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
          if info.items.len() != child_count {
            return None;
          }
          let row_offsets = compute_track_offsets(
            &info.rows,
            layout.size.height,
            layout.padding.top,
            layout.padding.bottom,
            layout.border.top,
            layout.border.bottom,
            container_style
              .align_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let col_offsets = compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );

          let mut out = Vec::with_capacity(child_count);
          for placement in info.items.iter() {
            let (x_start, x_end) =
              grid_area_for_item(&col_offsets, placement.column_start, placement.column_end)?;
            let (y_start, y_end) =
              grid_area_for_item(&row_offsets, placement.row_start, placement.row_end)?;
            out.push(Rect::from_xywh(
              x_start,
              y_start,
              (x_end - x_start).max(0.0),
              (y_end - y_start).max(0.0),
            ));
          }
          return Some(out);
        }
        None
      })
    } else {
      None
    };

    let mut root_child_continuation_available_owned: Option<FxHashMap<TaffyNodeId, f32>> = None;
    let root_child_continuation_available = if node_id == root_id
      && root_child_continuation_available.is_none()
      && fragmentainer_axes_hint().is_some()
      && fragmentainer_block_size_hint()
        .filter(|size| size.is_finite() && *size > 0.0)
        .is_some()
    {
      let fragment_block_axis = fragmentainer_axes_hint()
        .map(|axes| axes.block_axis())
        .unwrap_or(PhysicalAxis::Y);
      let container_block_size = match fragment_block_axis {
        PhysicalAxis::X => layout.size.width,
        PhysicalAxis::Y => layout.size.height,
      };

      if let Some(axis_style) = node_axis_style {
        let area_bounds: Option<Vec<Rect>> = node_taffy_style.and_then(|container_style| {
          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
            if info.items.len() != child_count {
              return None;
            }
            let row_offsets = compute_track_offsets(
              &info.rows,
              layout.size.height,
              layout.padding.top,
              layout.padding.bottom,
              layout.border.top,
              layout.border.bottom,
              container_style
                .align_content
                .unwrap_or(TaffyAlignContent::Stretch),
            );
            let col_offsets = compute_track_offsets(
              &info.columns,
              layout.size.width,
              layout.padding.left,
              layout.padding.right,
              layout.border.left,
              layout.border.right,
              container_style
                .justify_content
                .unwrap_or(TaffyAlignContent::Stretch),
            );

            let mut out = Vec::with_capacity(child_count);
            for placement in info.items.iter() {
              let (x_start, x_end) =
                grid_area_for_item(&col_offsets, placement.column_start, placement.column_end)?;
              let (y_start, y_end) =
                grid_area_for_item(&row_offsets, placement.row_start, placement.row_end)?;
              out.push(Rect::from_xywh(
                x_start,
                y_start,
                (x_end - x_start).max(0.0),
                (y_end - y_start).max(0.0),
              ));
            }
            return Some(out);
          }
          None
        });

        let mut dummy_children = Vec::with_capacity(child_count);
        for idx in 0..child_count {
          let child_id = taffy.get_child_id(node_id, idx);
          let child_bounds = area_bounds
            .as_ref()
            .and_then(|bounds| bounds.get(idx))
            .copied();
          let child_bounds = match child_bounds {
            Some(bounds) => bounds,
            None => {
              let child_layout = taffy
                .layout(child_id)
                .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;
              Rect::from_xywh(
                child_layout.location.x,
                child_layout.location.y,
                child_layout.size.width,
                child_layout.size.height,
              )
            }
          };
          dummy_children.push(FragmentNode::new_block(child_bounds, vec![]));
        }
        let root_bounds = Rect::from_xywh(
          layout.location.x,
          layout.location.y,
          layout.size.width,
          layout.size.height,
        );
        let mut dummy_root = FragmentNode::new_block(root_bounds, dummy_children);
        self.apply_grid_axis_mirroring(
          taffy,
          node_id,
          layout,
          &mut dummy_root,
          axis_style,
          deadline_counter,
        )?;

        let mut map: FxHashMap<TaffyNodeId, f32> = FxHashMap::default();
        for idx in 0..child_count {
          let child_id = taffy.get_child_id(node_id, idx);
          if let Some(available) = self.grid_item_continuation_available_block_size(
            dummy_root.children[idx].bounds,
            constraints,
            container_block_size,
          ) {
            map.insert(child_id, available);
          }
        }
        root_child_continuation_available_owned = Some(map);
      }

      root_child_continuation_available_owned.as_ref()
    } else {
      root_child_continuation_available
    };

    // Convert children recursively, propagating the effective grid axes to descendants.
    let mut child_fragments = Vec::with_capacity(child_count);
    for idx in 0..child_count {
      check_layout_deadline(deadline_counter)?;
      let child_id = taffy.get_child_id(node_id, idx);
      let child_area = child_grid_areas
        .as_ref()
        .and_then(|areas| areas.get(idx))
        .copied();
      child_fragments.push(self.convert_to_fragments(
        taffy,
        child_id,
        root_id,
        constraints,
        child_axis_style,
        child_area,
        root_child_continuation_available,
        auto_unskipped,
        measured_fragments,
        measured_node_keys,
        positioned_children,
        deadline_counter,
      )?);
    }

    // Create fragment bounds from Taffy layout.
    let mut bounds = Rect::from_xywh(
      layout.location.x,
      layout.location.y,
      layout.size.width,
      layout.size.height,
    );

    // Get style from node context if available
    if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
      let box_node = unsafe { &*box_node_ptr };
      let style_override = style_override_for(box_node.id);
      let style: &ComputedStyle = style_override
        .as_deref()
        .unwrap_or_else(|| box_node.style.as_ref());
      if let Some(taffy_style) = node_taffy_style {
        if let Some((dx, dy)) =
          self.grid_item_calc_percentage_margin_offset(taffy, node_id, style, taffy_style)
        {
          let x = bounds.x() + dx;
          let y = bounds.y() + dy;
          if x.is_finite() && y.is_finite() {
            bounds = Rect::from_xywh(x, y, bounds.width(), bounds.height());
          }
        }
      }

      if let Some(grid_area) = containing_grid_area {
        // CSS percentage min/max sizes on grid items resolve against the grid *area* size (CSS Grid
        // §11.5). Taffy currently resolves percentage min-sizes against the grid container, so we
        // treat them as `auto` in `convert_style` and apply the constraint here using the area's
        // definite size (when provided by the parent grid container).
        let area_width = if grid_area.width().is_finite() {
          grid_area.width().max(0.0)
        } else {
          0.0
        };
        let area_height = if grid_area.height().is_finite() {
          grid_area.height().max(0.0)
        } else {
          0.0
        };

        if area_width > 0.0
          && (style.min_width.is_some_and(|len| len.has_percentage())
            || style.max_width.is_some_and(|len| len.has_percentage()))
        {
          let horizontal_edges = self.axis_padding_border_px(style, Axis::Horizontal, area_width);
          let resolve_border_box = |len: Length| {
            let specified = self
              .resolve_length_for_width(len, area_width, style)
              .max(0.0);
            border_size_from_box_sizing(specified, horizontal_edges, style.box_sizing).max(0.0)
          };
          let min_border = style
            .min_width
            .filter(|len| len.has_percentage())
            .map(resolve_border_box)
            .unwrap_or(0.0);
          let max_border = style
            .max_width
            .filter(|len| len.has_percentage())
            .map(resolve_border_box)
            .unwrap_or(f32::INFINITY);
          let width = clamp_with_order(bounds.width(), min_border, max_border);
          bounds = Rect::from_xywh(bounds.x(), bounds.y(), width.max(0.0), bounds.height());
        }

        if area_height > 0.0
          && (style.min_height.is_some_and(|len| len.has_percentage())
            || style.max_height.is_some_and(|len| len.has_percentage()))
        {
          // Percentage padding/border resolve against the containing block's *width*, even for the
          // vertical axis (CSS2.1 §10.5).
          let vertical_edges = self.axis_padding_border_px(style, Axis::Vertical, area_width);
          let resolve_border_box = |len: Length| {
            let specified = self
              .resolve_length_px_with_base(len, Some(area_height), style)
              .unwrap_or(0.0)
              .max(0.0);
            border_size_from_box_sizing(specified, vertical_edges, style.box_sizing).max(0.0)
          };
          let min_border = style
            .min_height
            .filter(|len| len.has_percentage())
            .map(resolve_border_box)
            .unwrap_or(0.0);
          let max_border = style
            .max_height
            .filter(|len| len.has_percentage())
            .map(resolve_border_box)
            .unwrap_or(f32::INFINITY);
          let height = clamp_with_order(bounds.height(), min_border, max_border);
          bounds = Rect::from_xywh(bounds.x(), bounds.y(), bounds.width(), height.max(0.0));
        }
      }

      if is_grid_style {
        if let Some(axis_style) = node_axis_style {
          self.maybe_shrink_auto_span_subgrid_bounds(
            taffy,
            node_id,
            box_node,
            axis_style,
            positioned_children
              .get(&node_id)
              .is_some_and(|positioned| !positioned.is_empty()),
            &mut bounds,
            deadline_counter,
          )?;
        }
      }
      let fc_type = box_node
        .formatting_context()
        .unwrap_or(FormattingContextType::Block);
      if node_id == root_id || is_grid_style || !child_fragments.is_empty() {
        let mut fragment = FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(box_node.id),
          },
          child_fragments,
          box_node.style.clone(),
        );
        attach_fragment_style_for_box(&mut fragment, box_node);
        let has_in_flow_children = !fragment.children.is_empty();
        if is_grid_style {
          self.apply_grid_baseline_alignment(
            taffy,
            node_id,
            layout,
            child_count,
            &mut fragment,
            deadline_counter,
          )?;
          if let Some(axis_style) = node_axis_style {
            self.apply_grid_axis_mirroring(
              taffy,
              node_id,
              layout,
              &mut fragment,
              axis_style,
              deadline_counter,
            )?;
          }
          self.apply_subgrid_writing_mode_transpose(
            taffy,
            node_id,
            layout,
            box_node,
            &mut fragment,
            deadline_counter,
          )?;
          if let (Some(container_style), Some(axis_style)) = (node_taffy_style, node_axis_style) {
            fragment.grid_tracks = grid_track_ranges_for_container(
              taffy,
              node_id,
              layout,
              container_style,
              axis_style,
              has_in_flow_children,
            )
            .map(Arc::new);
          }

          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
            if info.items.len() == child_count {
              let mut items = Vec::with_capacity(info.items.len());
              for (idx, placement) in info.items.iter().enumerate() {
                let child_id = taffy.get_child_id(node_id, idx);
                let Some(&child_ptr) = taffy.get_node_context(child_id) else {
                  break;
                };
                let child_node = unsafe { &*child_ptr };
                items.push(GridItemFragmentationData {
                  box_id: child_node.id,
                  row_start: placement.row_start,
                  row_end: placement.row_end,
                  column_start: placement.column_start,
                  column_end: placement.column_end,
                });
              }
              if items.len() == child_count {
                fragment.grid_fragmentation = Some(Arc::new(GridFragmentationInfo { items }));
              }
            }
          }

          // Apply `position: relative` offsets for grid items (CSS 2.1 §9.4.3).
          //
          // Grid item positions are computed by Taffy. Relative positioning is a post-process that
          // shifts the item's border box without influencing grid track sizing.
          let percentage_base = constraints
            .width()
            .or(constraints.inline_percentage_base)
            .filter(|base| base.is_finite())
            .unwrap_or(fragment.bounds.width().max(0.0));
          let (
            padding_left,
            padding_right,
            padding_top,
            padding_bottom,
            border_left,
            border_right,
            border_top,
            border_bottom,
          ) = self.resolved_padding_border_for_measure(&box_node.style, percentage_base);
          let cb_width =
            (fragment.bounds.width() - padding_left - padding_right - border_left - border_right)
              .max(0.0);
          let cb_height =
            (fragment.bounds.height() - padding_top - padding_bottom - border_top - border_bottom)
              .max(0.0);
          let block_base = box_node.style.height.is_some().then_some(cb_height);
          let relative_cb = ContainingBlock::with_viewport_and_bases(
            Rect::new(Point::ZERO, Size::new(cb_width, cb_height)),
            self.viewport_size,
            Some(cb_width),
            block_base,
          )
          .with_writing_mode_and_direction(box_node.style.writing_mode, box_node.style.direction);
          let fragment_children = fragment.children_mut();
          let child_len = child_count.min(fragment_children.len());
          for idx in 0..child_len {
            let child_id = taffy.get_child_id(node_id, idx);
            let child_fragment = &mut fragment_children[idx];
            let Some(&child_ptr) = taffy.get_node_context(child_id) else {
              continue;
            };
            let child_node = unsafe { &*child_ptr };
            if !child_node.style.position.is_relative() {
              continue;
            }
            let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
              &child_node.style,
              &relative_cb,
              self.viewport_size,
              &self.font_context,
            );
            let offset = compute_relative_offset_for_relative(
              &positioned_style,
              &relative_cb,
              &self.font_context,
            );
            if offset.x != 0.0 || offset.y != 0.0 {
              child_fragment.bounds = child_fragment.bounds.translate(offset);
              child_fragment.logical_override = child_fragment
                .logical_override
                .map(|logical| logical.translate(offset));
            }
          }
        }
        if let Some(positioned) = positioned_children.get(&node_id) {
          let mut abs_children = self
            .layout_positioned_children_for_node(taffy, node_id, box_node, bounds, positioned)?;
          fragment.children_mut().append(&mut abs_children);
        }
        return Ok(fragment);
      }

      let skip_contents = match box_node.style.content_visibility {
        crate::style::types::ContentVisibility::Hidden => true,
        crate::style::types::ContentVisibility::Auto => {
          self.content_visibility_auto_has_definite_placeholder(box_node)
            && auto_unskipped
              .map(|set| !set.contains(&box_node_ptr))
              .unwrap_or(false)
        }
        crate::style::types::ContentVisibility::Visible => false,
      };
      if skip_contents {
        let mut fragment = FragmentNode::new_with_style(
          bounds,
          FragmentContent::Block {
            box_id: Some(box_node.id),
          },
          vec![],
          box_node.style.clone(),
        );
        attach_fragment_style_for_box(&mut fragment, box_node);
        return Ok(fragment);
      }

      let fragment_block_axis = fragmentainer_axes_hint()
        .map(|axes| axes.block_axis())
        .unwrap_or(PhysicalAxis::Y);
      let block_axis_mirrored = containing_grid_axis
        .map(|axis_style| {
          let inline_is_horizontal = axis_style.inline_is_horizontal();
          let mut mirror_x = false;
          let mut mirror_y = false;
          if !axis_style.inline_positive() {
            if inline_is_horizontal {
              mirror_x = true;
            } else {
              mirror_y = true;
            }
          }
          if !axis_style.block_positive() {
            if inline_is_horizontal {
              mirror_y = true;
            } else {
              mirror_x = true;
            }
          }
          match fragment_block_axis {
            PhysicalAxis::X => mirror_x,
            PhysicalAxis::Y => mirror_y,
          }
        })
        .unwrap_or(false);
      let container_block_size = if taffy.parent(node_id) == Some(root_id) {
        let root_layout = taffy
          .layout(root_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;
        Some(match fragment_block_axis {
          PhysicalAxis::X => root_layout.size.width,
          PhysicalAxis::Y => root_layout.size.height,
        })
      } else {
        None
      };
      let continuation_available = if taffy.parent(node_id) == Some(root_id) {
        if let Some(map) = root_child_continuation_available {
          map.get(&node_id).copied()
        } else {
          container_block_size.and_then(|size| {
            self.grid_item_continuation_available_block_size(bounds, constraints, size)
          })
        }
      } else {
        None
      };

      let axis_style =
        containing_grid_axis.unwrap_or_else(|| GridAxisStyle::from_style(box_node.style.as_ref()));
      let inline_is_horizontal = axis_style.inline_is_horizontal();
      let allow_stretch_block_size_override = taffy
        .get_node_context(root_id)
        .copied()
        .map(|ptr| unsafe { &*ptr })
        .map(|root_box| {
          let root_style_override = style_override_for(root_box.id);
          let root_style: &ComputedStyle = root_style_override
            .as_deref()
            .unwrap_or_else(|| root_box.style.as_ref());
          grid_container_allows_stretch_block_size_override(root_style, constraints)
        })
        .unwrap_or(true);

      // Grid items resolve percentage sizes against their *grid area* (not their own used size).
      // Taffy reports the used size/offset of the grid item itself (which includes self-alignment
      // offsets). When an item has `height: 100%` (e.g. Tailwind `h-full`) in an auto-sized row, the
      // grid area's block size is known after track sizing but the item may still be aligned as if
      // its height were `auto`. Chrome resolves `height:100%` against the grid area's used size,
      // making the item fill the row and eliminating the alignment offset.
      let grid_area_bounds = (|| -> Option<Rect> {
        let parent_id = taffy.parent(node_id)?;
        let parent_style = taffy.style(parent_id).ok()?;
        if parent_style.display != Display::Grid {
          return None;
        }
        let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(parent_id) else {
          return None;
        };
        let parent_child_count = taffy.child_count(parent_id);
        if info.items.len() != parent_child_count {
          return None;
        }
        let idx = (0..parent_child_count)
          .find(|&idx| taffy.get_child_id(parent_id, idx) == node_id)?;
        let placement = info.items.get(idx)?;
        let parent_layout = taffy.layout(parent_id).ok()?;
        let row_offsets = compute_track_offsets(
          &info.rows,
          parent_layout.size.height,
          parent_layout.padding.top,
          parent_layout.padding.bottom,
          parent_layout.border.top,
          parent_layout.border.bottom,
          parent_style
            .align_content
            .unwrap_or(TaffyAlignContent::Stretch),
        );
        let col_offsets = compute_track_offsets(
          &info.columns,
          parent_layout.size.width,
          parent_layout.padding.left,
          parent_layout.padding.right,
          parent_layout.border.left,
          parent_layout.border.right,
          parent_style
            .justify_content
            .unwrap_or(TaffyAlignContent::Stretch),
        );
        let (x_start, x_end) =
          grid_area_for_item(&col_offsets, placement.column_start, placement.column_end)?;
        let (y_start, y_end) =
          grid_area_for_item(&row_offsets, placement.row_start, placement.row_end)?;
        Some(Rect::from_xywh(
          x_start,
          y_start,
          (x_end - x_start).max(0.0),
          (y_end - y_start).max(0.0),
        ))
      })();

      let inline_percentage_base = grid_area_bounds
        .map(|rect| {
          if inline_is_horizontal {
            rect.width()
          } else {
            rect.height()
          }
        })
        .unwrap_or_else(|| {
          if inline_is_horizontal {
            bounds.width()
          } else {
            bounds.height()
          }
        });
      let block_percentage_base = grid_area_bounds.map(|rect| {
        if inline_is_horizontal {
          rect.height()
        } else {
          rect.width()
        }
      });
      let block_percentage_base = allow_stretch_block_size_override
        .then_some(block_percentage_base)
        .flatten();

      let mut allow_measured_reuse = true;
      let height_is_full = inline_is_horizontal
        && style.height.is_some_and(|len| {
          len.calc.is_none()
            && len.unit == crate::style::values::LengthUnit::Percent
            && (len.value - 100.0).abs() < 0.01
        });
      if height_is_full {
        if let Some(grid_area) = grid_area_bounds {
          let area_height = grid_area.height();
          if area_height.is_finite() && area_height > bounds.height() + 0.5 {
            bounds = Rect::from_xywh(
              bounds.x(),
              grid_area.y(),
              bounds.width(),
              area_height.max(0.0),
            );
            allow_measured_reuse = false;
          }
        }
      }

      if allow_measured_reuse && continuation_available.is_none() {
        if let Some(keys) = measured_node_keys.get(&node_id) {
          if let Some(mut reused) = Self::take_matching_measured_fragment(
            measured_fragments,
            keys,
            bounds.width(),
            bounds.height(),
          ) {
            fragment_clone_profile::record_fragment_reuse_without_clone(
              CloneSite::GridMeasureReuse,
            );
            let delta = Point::new(
              bounds.x() - reused.bounds.x(),
              bounds.y() - reused.bounds.y(),
            );
            translate_fragment_tree(&mut reused, delta, deadline_counter)?;
            // Measured grid item fragments are laid out at `origin=(0, 0)` using the grid
            // container's containing blocks (because the final item placement isn't known during
            // Taffy measurement). When we later translate the item root into its final grid-area
            // position, out-of-flow positioned descendants whose containing block is outside the
            // item subtree must *not* inherit that translation.
            //
            // This mirrors the translation-cancellation logic in parallel block layout.
            let viewport_fixed_cb = self.factory.viewport_fixed_cb();
            let external_fixed_cb = self.factory.nearest_fixed_cb() != viewport_fixed_cb;
            cancel_translation_for_out_of_flow_positioned_descendants(
              &mut reused,
              delta,
              external_fixed_cb,
              deadline_counter,
            )?;
            attach_fragment_style_for_box(&mut reused, box_node);
            let percentage_base = constraints
              .inline_percentage_base
              .filter(|base| base.is_finite())
              .unwrap_or(bounds.width());
            let content_size = self.content_box_size(
              &reused,
              reused.get_style().unwrap_or_else(|| box_node.style.as_ref()),
              percentage_base,
            );
            remembered_size_cache_store(box_node, content_size);
            return Ok(reused);
          }
        }
      }

      let origin = Point::new(bounds.x(), bounds.y());
      let origin = if origin.x.is_finite() && origin.y.is_finite() {
        origin
      } else {
        Point::ZERO
      };
      let parent_scroll = self.factory.viewport_scroll();
      let parent_scroll = if parent_scroll.x.is_finite() && parent_scroll.y.is_finite() {
        parent_scroll
      } else {
        Point::ZERO
      };
      let child_scroll = Point::new(parent_scroll.x - origin.x, parent_scroll.y - origin.y);

      let fragmentainer_size_hint = fragmentainer_block_size_hint();
      let fragmentainer_axes = fragmentainer_axes_hint();
      let fragmentainer_axes_resolved = fragmentainer_axes.unwrap_or_default();
      let parent_fragmentainer_offset = fragmentainer_block_offset_hint();
      let _fragmentainer_hint_guard = set_fragmentainer_block_size_hint(fragmentainer_size_hint);
      let _fragmentainer_axes_guard = set_fragmentainer_axes_hint(fragmentainer_axes);
      let _fragmentainer_offset_guard = fragmentainer_size_hint
        .is_some_and(|size| size.is_finite() && size > 0.0)
        .then(|| {
          let parent_block_size = taffy
            .parent(node_id)
            .and_then(|parent| taffy.layout(parent).ok())
            .map(|layout| match fragmentainer_axes_resolved.block_axis() {
              PhysicalAxis::X => layout.size.width,
              PhysicalAxis::Y => layout.size.height,
            })
            .filter(|size| size.is_finite())
            .unwrap_or(0.0);
          let child_block_start_phys = match fragmentainer_axes_resolved.block_axis() {
            PhysicalAxis::X => bounds.x(),
            PhysicalAxis::Y => bounds.y(),
          };
          let child_block_size = match fragmentainer_axes_resolved.block_axis() {
            PhysicalAxis::X => bounds.width(),
            PhysicalAxis::Y => bounds.height(),
          };
          let child_block_start =
            if fragmentainer_axes_resolved.block_positive() ^ block_axis_mirrored {
              child_block_start_phys
            } else {
              parent_block_size - child_block_start_phys - child_block_size
            };
          let child_block_start = if child_block_start.is_finite() {
            child_block_start
          } else {
            0.0
          };
          set_fragmentainer_block_offset_hint(parent_fragmentainer_offset + child_block_start)
        });

      let child_factory = self.factory.translated_for_child(origin);
      let fc: Arc<dyn FormattingContext> = if matches!(fc_type, FormattingContextType::Block) {
        Arc::new(
          BlockFormattingContext::for_independent_context_root_with_factory(child_factory.clone()),
        )
      } else {
        child_factory.get(fc_type)
      };

      let (available_width, available_height, force_width, force_height) = match fragment_block_axis
      {
        PhysicalAxis::X => {
          let available_width = continuation_available.unwrap_or(bounds.width());
          let force_width = continuation_available
            .map(|available| available + 0.01 >= bounds.width())
            .unwrap_or(true);
          (available_width, bounds.height(), force_width, true)
        }
        PhysicalAxis::Y => {
          let available_height = continuation_available.unwrap_or(bounds.height());
          let force_height = continuation_available
            .map(|available| available + 0.01 >= bounds.height())
            .unwrap_or(true);
          (bounds.width(), available_height, true, force_height)
        }
      };
      let child_constraints = LayoutConstraints::new(
        CrateAvailableSpace::Definite(available_width),
        CrateAvailableSpace::Definite(available_height),
      )
      // Keep percentage resolution anchored to the grid area's inline size for the laid-out item.
      // See comment in the parallel in-flow path.
      .with_inline_percentage_base(Some(inline_percentage_base.max(0.0)))
      .with_block_percentage_base(
        block_percentage_base
          .filter(|b| b.is_finite())
          .map(|b| b.max(0.0)),
      );
      let supports_used_border_box = matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Flex
          | FormattingContextType::Grid
          | FormattingContextType::Inline
          | FormattingContextType::Table
      );

      let used_border_box_width = Some(if force_width {
        bounds.width()
      } else {
        available_width
      });
      let used_border_box_height = Some(if force_height {
        bounds.height()
      } else {
        available_height
      });

      let mut laid_out =
        FormattingContextFactory::with_viewport_scroll_override(child_scroll, || {
          if supports_used_border_box {
            let child_constraints = if allow_stretch_block_size_override {
              child_constraints
                .with_used_border_box_size(used_border_box_width, used_border_box_height)
            } else {
              child_constraints.with_used_border_box_size_for_layout_only(
                used_border_box_width,
                used_border_box_height,
              )
            };
            if continuation_available.is_some() {
              // Grid intrinsic sizing keywords are resolved against the initial fragmentainer size when
              // building the Taffy tree. When the grid container continues after a break, re-layout
              // the item using its original style so keywords like `fill-available` can be resolved
              // against the reduced available block size.
              crate::layout::style_override::with_style_override(
                box_node.id,
                box_node.style.clone(),
                || fc.layout(box_node, &child_constraints),
              )
            } else {
              fc.layout(box_node, &child_constraints)
            }
          } else if matches!(
            fc_type,
            FormattingContextType::Flex | FormattingContextType::Grid
          ) {
            let mut layout_style = (*box_node.style).clone();
            if force_width {
              layout_style.width = Some(Length::px(bounds.width()));
              layout_style.width_keyword = None;
            }
            if force_height {
              layout_style.height = Some(Length::px(bounds.height()));
              layout_style.height_keyword = None;
            }
            crate::layout::style_override::with_style_override(
              box_node.id,
              Arc::new(layout_style),
              || fc.layout(box_node, &child_constraints),
            )
          } else {
            let mut layout_style = (*box_node.style).clone();
            if force_width {
              layout_style.width = Some(Length::px(bounds.width()));
              layout_style.width_keyword = None;
            }
            if force_height {
              layout_style.height = Some(Length::px(bounds.height()));
              layout_style.height_keyword = None;
            }
            let layout_style = Arc::new(layout_style);
            if box_node.id != 0 {
              crate::layout::style_override::with_style_override(box_node.id, layout_style, || {
                fc.layout(box_node, &child_constraints)
              })
            } else {
              let mut layout_child = (*box_node).clone();
              layout_child.style = layout_style;
              fc.layout(&layout_child, &child_constraints)
            }
          }
        })?;
      let delta = Point::new(
        bounds.x() - laid_out.bounds.x(),
        bounds.y() - laid_out.bounds.y(),
      );
      translate_fragment_tree(&mut laid_out, delta, deadline_counter)?;
      laid_out.content = match &box_node.box_type {
        crate::tree::box_tree::BoxType::Replaced(replaced_box) => FragmentContent::Replaced {
          replaced_type: replaced_box.replaced_type.clone(),
          box_id: Some(box_node.id),
        },
        _ => FragmentContent::Block {
          box_id: Some(box_node.id),
        },
      };
      attach_fragment_style_for_box(&mut laid_out, box_node);
      let percentage_base = constraints
        .inline_percentage_base
        .filter(|base| base.is_finite())
        .unwrap_or(bounds.width());
      let content_size = self.content_box_size(&laid_out, box_node.style.as_ref(), percentage_base);
      remembered_size_cache_store(box_node, content_size);
      Ok(laid_out)
    } else {
      Ok(FragmentNode::new_block(bounds, child_fragments))
    }
  }

  fn layout_positioned_children_for_node(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    box_node: &BoxNode,
    bounds: Rect,
    positioned_children: &[*const BoxNode],
  ) -> Result<Vec<FragmentNode>, LayoutError> {
    if positioned_children.is_empty() {
      return Ok(Vec::new());
    }

    let padding_left =
      self.resolve_length_for_width(box_node.style.padding_left, bounds.width(), &box_node.style);
    let padding_top =
      self.resolve_length_for_width(box_node.style.padding_top, bounds.width(), &box_node.style);
    let padding_right = self.resolve_length_for_width(
      box_node.style.padding_right,
      bounds.width(),
      &box_node.style,
    );
    let padding_bottom = self.resolve_length_for_width(
      box_node.style.padding_bottom,
      bounds.width(),
      &box_node.style,
    );
    let border_left = self.resolve_length_for_width(
      box_node.style.used_border_left_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_top = self.resolve_length_for_width(
      box_node.style.used_border_top_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_right = self.resolve_length_for_width(
      box_node.style.used_border_right_width(),
      bounds.width(),
      &box_node.style,
    );
    let border_bottom = self.resolve_length_for_width(
      box_node.style.used_border_bottom_width(),
      bounds.width(),
      &box_node.style,
    );
    // CSS 2.1 §10.1: the containing block for absolute positioned descendants is the padding box
    // of the nearest positioned ancestor, i.e. the rectangle bounded by the padding edge (border
    // box minus borders).
    let padding_origin = crate::geometry::Point::new(border_left, border_top);
    let padding_size = crate::geometry::Size::new(
      (bounds.width() - border_left - border_right).max(0.0),
      (bounds.height() - border_top - border_bottom).max(0.0),
    );
    let padding_rect = crate::geometry::Rect::new(padding_origin, padding_size);

    // Percentage sizes/offsets on absolutely positioned boxes resolve against the used size of the
    // containing block, even when the containing block's own height is `auto` (CSS 2.1 §10.5).
    let block_base = Some(padding_rect.size.height);
    let establishes_abs_cb = box_node.style.establishes_abs_containing_block();
    let establishes_fixed_cb = box_node.style.establishes_fixed_containing_block();
    let root_box_id = ensure_box_id(box_node);
    let padding_cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
      padding_rect,
      self.viewport_size,
      Some(padding_rect.size.width),
      block_base,
    )
    .with_writing_mode_and_direction(box_node.style.writing_mode, box_node.style.direction)
    .with_box_id(Some(root_box_id));
    let cb_for_absolute = if establishes_abs_cb {
      padding_cb
    } else {
      self.nearest_positioned_cb
    };
    let cb_for_fixed = if establishes_fixed_cb {
      padding_cb
    } else {
      self.nearest_fixed_cb
    };

    // `FormattingContextFactory::with_fixed_cb` / `with_positioned_cb` reset the per-factory cached
    // formatting contexts store. Build factory variants once so multiple positioned children can
    // reuse cached formatting contexts instead of rebuilding detached ones per child.
    let positioned_factory = self.factory.clone();
    let positioned_factory = if cb_for_fixed == positioned_factory.nearest_fixed_cb() {
      positioned_factory
    } else {
      positioned_factory.with_fixed_cb(cb_for_fixed)
    };
    let abs_factory = if cb_for_absolute == positioned_factory.nearest_positioned_cb() {
      positioned_factory.clone()
    } else {
      positioned_factory.with_positioned_cb(cb_for_absolute)
    };

    #[derive(Clone)]
    struct SubgridAxisContext {
      offsets: Vec<f32>,
      line_offset: u16,
      node_offset: f32,
      span_start: f32,
      span_end: f32,
    }

    // Axis mapping for absolute-positioned static-position resolution generally matches the grid
    // layout algorithm's effective writing-mode/direction rules (for subgrids, this is the parent
    // grid's writing-mode so track inheritance stays stable).
    //
    // When the subgrid establishes an orthogonal writing-mode, we still want line numbering and
    // placement to obey the subgrid's own writing-mode (CSS Grid 2 #subgrid-indexing). In that
    // case we transpose the track offsets (matching `apply_subgrid_writing_mode_transpose`) and
    // apply mirroring based on the subgrid's *local* writing-mode/direction.
    let container_style = taffy
      .style(node_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
    let mut axes_swapped = container_style.axes_swapped;
    let mut mirror_x = !container_style.start_end_axis_positive.x;
    let mut mirror_y = !container_style.start_end_axis_positive.y;

    let mut subgrid_writing_mode_mismatch = false;
    if box_node.style.grid_row_subgrid || box_node.style.grid_column_subgrid {
      if let Some(parent_id) = taffy.parent(node_id) {
        if let Some(&parent_ptr) = taffy.get_node_context(parent_id) {
          let parent_node = unsafe { &*parent_ptr };
          let this_inline_is_horizontal =
            GridAxisStyle::from_style(&box_node.style).inline_is_horizontal();
          let parent_inline_is_horizontal =
            GridAxisStyle::from_style(&parent_node.style).inline_is_horizontal();
          subgrid_writing_mode_mismatch = this_inline_is_horizontal != parent_inline_is_horizontal;
        }
      }
    }

    // For orthogonal writing-mode mismatches, use local axis mapping + mirroring even when we
    // can't access per-node track info (fallback subgrid-offset path).
    let mut mismatch_align_x: Option<TaffyAlignContent> = None;
    let mut mismatch_align_y: Option<TaffyAlignContent> = None;
    if subgrid_writing_mode_mismatch {
      let local_axis_style = GridAxisStyle::from_style(&box_node.style);
      let inline_positive = local_axis_style.inline_positive();
      let block_positive = local_axis_style.block_positive();
      let inline_is_horizontal = local_axis_style.inline_is_horizontal();

      let (align_x, align_y) = if inline_is_horizontal {
        (
          self.convert_justify_content(&box_node.style.justify_content, inline_positive),
          self.convert_align_content(&box_node.style.align_content, block_positive),
        )
      } else {
        (
          self.convert_align_content(&box_node.style.align_content, block_positive),
          self.convert_justify_content(&box_node.style.justify_content, inline_positive),
        )
      };
      mismatch_align_x = Some(align_x);
      mismatch_align_y = Some(align_y);

      axes_swapped = !inline_is_horizontal;
      mirror_x = false;
      mirror_y = false;
      if !inline_positive {
        if inline_is_horizontal {
          mirror_x = true;
        } else {
          mirror_y = true;
        }
      }
      if !block_positive {
        if inline_is_horizontal {
          mirror_y = true;
        } else {
          mirror_x = true;
        }
      }
    }

    let mut row_offsets: Option<Vec<f32>> = None;
    let mut col_offsets: Option<Vec<f32>> = None;
    let mut row_alignment: Option<TaffyAlignContent> = None;
    let mut col_alignment: Option<TaffyAlignContent> = None;
    let mut row_explicit_line_count: Option<u16> = None;
    let mut col_explicit_line_count: Option<u16> = None;
    let mut row_subgrid_ctx: Option<SubgridAxisContext> = None;
    let mut col_subgrid_ctx: Option<SubgridAxisContext> = None;

    if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
      if subgrid_writing_mode_mismatch {
        let align_x = mismatch_align_x.unwrap_or(TaffyAlignContent::Stretch);
        let align_y = mismatch_align_y.unwrap_or(TaffyAlignContent::Stretch);

        // Build the column track list without gutters so that a containing grid's physical-X gaps do
        // not incorrectly transpose onto the physical-Y axis. This matches WPT
        // `css/subgrid/subgrid-writing-mode-001`.
        let mut columns_no_gutters = info.columns.clone();
        columns_no_gutters.gutters.clear();
        columns_no_gutters
          .gutters
          .resize(columns_no_gutters.sizes.len() + 1, 0.0);

        // In the transposed coordinate space, the Taffy "row" track vector becomes physical X and
        // the (gap-less) "column" vector becomes physical Y.
        col_alignment = Some(align_x);
        row_alignment = Some(align_y);
        col_explicit_line_count = Some(info.rows.explicit_tracks.saturating_add(1));
        row_explicit_line_count = Some(info.columns.explicit_tracks.saturating_add(1));
        col_offsets = Some(compute_track_offsets(
          &info.rows,
          bounds.width(),
          padding_left,
          padding_right,
          border_left,
          border_right,
          align_x,
        ));
        row_offsets = Some(compute_track_offsets(
          &columns_no_gutters,
          bounds.height(),
          padding_top,
          padding_bottom,
          border_top,
          border_bottom,
          align_y,
        ));
      } else {
        let row_align = container_style
          .align_content
          .unwrap_or(taffy::style::AlignContent::Stretch);
        let col_align = container_style
          .justify_content
          .unwrap_or(taffy::style::AlignContent::Stretch);
        row_alignment = Some(row_align);
        col_alignment = Some(col_align);
        row_explicit_line_count = Some(info.rows.explicit_tracks.saturating_add(1));
        col_explicit_line_count = Some(info.columns.explicit_tracks.saturating_add(1));
        row_offsets = Some(compute_track_offsets(
          &info.rows,
          bounds.height(),
          padding_top,
          padding_bottom,
          border_top,
          border_bottom,
          row_align,
        ));
        col_offsets = Some(compute_track_offsets(
          &info.columns,
          bounds.width(),
          padding_left,
          padding_right,
          border_left,
          border_right,
          col_align,
        ));
      }
    } else {
      // Subgrid nodes do not always expose per-node track info in Taffy. When that happens,
      // derive track offsets from the nearest ancestor grid that does provide them, then map
      // local grid line numbers into the ancestor grid's line space.
      let maybe_axis_ctx = |axis_is_columns: bool| -> Option<SubgridAxisContext> {
        let mut line_offset: i32 = 0;
        // Translation from the ancestor grid into the origin subgrid's coordinate space.
        //
        // Accumulate both physical components and select the relevant one once we know which
        // physical axis the ancestor grid uses for this CSS grid axis.
        let mut node_offset_x: f32 = 0.0;
        let mut node_offset_y: f32 = 0.0;
        let mut current = node_id;
        // Number of grid lines (tracks + 1) for the *origin* subgrid node in this axis. Used to
        // compute the spanned region so axis mirroring is performed relative to the subgrid rather
        // than the ancestor grid.
        let mut origin_line_count: Option<u16> = None;

        loop {
          let current_ptr = *taffy.get_node_context(current)?;
          let current_box_node = unsafe { &*current_ptr };
          let is_subgrid_axis = if axis_is_columns {
            current_box_node.style.grid_column_subgrid
          } else {
            current_box_node.style.grid_row_subgrid
          };
          if !is_subgrid_axis {
            return None;
          }
          let mut start_line = if axis_is_columns {
            current_box_node.style.grid_column_start
          } else {
            current_box_node.style.grid_row_start
          };
          let mut end_line = if axis_is_columns {
            current_box_node.style.grid_column_end
          } else {
            current_box_node.style.grid_row_end
          };
          if start_line <= 0 || end_line <= 0 {
            let axis = if axis_is_columns {
              CssGridAxis::Column
            } else {
              CssGridAxis::Row
            };
            // Auto-placed subgrids have `grid-*-start/end` set to `auto` in computed style. In that
            // case, recover the resolved placement from the parent grid's detailed layout info so
            // we can still map local line numbers into the ancestor grid's line space.
            let (resolved_start, resolved_end) =
              resolved_grid_item_range_from_parent_layout(taffy, current, axis)?;
            if start_line <= 0 {
              start_line = resolved_start as i32;
            }
            if end_line <= 0 {
              end_line = resolved_end as i32;
            }
          }
          if start_line <= 0 {
            return None;
          }
          if origin_line_count.is_none() && current == node_id && end_line > start_line {
            origin_line_count = u16::try_from((end_line - start_line + 1).max(0)).ok();
          }
          line_offset = line_offset.saturating_add(start_line.saturating_sub(1) as i32);

          let layout = taffy.layout(current).ok()?;
          node_offset_x += layout.location.x;
          node_offset_y += layout.location.y;

          let parent = taffy.parent(current)?;
          current = parent;

          if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
            let container_style = taffy.style(current).ok()?;
            // Map this CSS grid axis onto the ancestor grid's physical axis using the ancestor's
            // `axes_swapped` value (which is derived from its effective writing-mode).
            let axis_is_physical_x = if container_style.axes_swapped {
              !axis_is_columns
            } else {
              axis_is_columns
            };
            let node_offset = if axis_is_physical_x {
              node_offset_x
            } else {
              node_offset_y
            };
            let ancestor_layout = taffy.layout(current).ok()?;
            let ancestor_bounds = Rect::from_xywh(
              ancestor_layout.location.x,
              ancestor_layout.location.y,
              ancestor_layout.size.width,
              ancestor_layout.size.height,
            );

            let ancestor_ptr = *taffy.get_node_context(current)?;
            let ancestor_box_node = unsafe { &*ancestor_ptr };

            let ancestor_padding_left = self.resolve_length_for_width(
              ancestor_box_node.style.padding_left,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_top = self.resolve_length_for_width(
              ancestor_box_node.style.padding_top,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_right = self.resolve_length_for_width(
              ancestor_box_node.style.padding_right,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_padding_bottom = self.resolve_length_for_width(
              ancestor_box_node.style.padding_bottom,
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_left = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_left_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_top = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_top_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_right = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_right_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );
            let ancestor_border_bottom = self.resolve_length_for_width(
              ancestor_box_node.style.used_border_bottom_width(),
              ancestor_bounds.width(),
              &ancestor_box_node.style,
            );

            // Taffy stores tracks in physical axes (columns=x, rows=y). Pick the appropriate track
            // vector for the physical axis that this CSS grid axis maps onto.
            let axis_tracks = if axis_is_physical_x {
              &info.columns
            } else {
              &info.rows
            };
            let offsets = if axis_is_physical_x {
              compute_track_offsets(
                axis_tracks,
                ancestor_bounds.width(),
                ancestor_padding_left,
                ancestor_padding_right,
                ancestor_border_left,
                ancestor_border_right,
                container_style
                  .justify_content
                  .unwrap_or(taffy::style::AlignContent::Stretch),
              )
            } else {
              compute_track_offsets(
                axis_tracks,
                ancestor_bounds.height(),
                ancestor_padding_top,
                ancestor_padding_bottom,
                ancestor_border_top,
                ancestor_border_bottom,
                container_style
                  .align_content
                  .unwrap_or(taffy::style::AlignContent::Stretch),
              )
            };

            let line_offset = u16::try_from(line_offset).ok()?;
            let (span_start, span_end) = origin_line_count
              .and_then(|count| {
                let mapped_start = line_offset.saturating_add(1);
                let mapped_end = line_offset.saturating_add(count);
                (mapped_end > mapped_start)
                  .then(|| grid_area_for_positioned_item(&offsets, mapped_start, mapped_end))
              })
              .flatten()
              .map(|(start, end)| (start - node_offset, end - node_offset))
              // If we can't determine the exact spanned region, fall back to mirroring around the
              // ancestor grid span translated into the subgrid's coordinate space.
              .or_else(|| {
                let start = offsets.get(1).copied().unwrap_or(0.0) - node_offset;
                let end = offsets.last().copied().unwrap_or(start) - node_offset;
                Some((start, end))
              })?;
            return Some(SubgridAxisContext {
              offsets,
              line_offset,
              node_offset,
              span_start,
              span_end,
            });
          }
        }
      };

      if box_node.style.grid_row_subgrid {
        row_subgrid_ctx = maybe_axis_ctx(false);
      }
      if box_node.style.grid_column_subgrid {
        col_subgrid_ctx = maybe_axis_ctx(true);
      }
    }

    #[derive(Clone, Copy, Default)]
    struct GridAreaOverride {
      x: Option<(f32, f32)>,
      y: Option<(f32, f32)>,
    }

    let mut static_positions: FxHashMap<usize, Point> = FxHashMap::default();
    let mut grid_area_overrides: FxHashMap<usize, GridAreaOverride> = FxHashMap::default();

    // Taffy resolves named line placement for in-flow grid items, but out-of-flow positioned items
    // are excluded from the Taffy tree so their static positions are resolved here.
    //
    // - For non-subgrids we prefer Taffy's expanded line-name vectors (they include auto-repeat
    //   expansion for `repeat(auto-fill|auto-fit, ...)` plus area-derived implicit names).
    // - For subgrid axes, computed styles omit inherited names, so we reconstruct them by slicing
    //   the parent's expanded names and merging any `subgrid [...]` overrides.
    let needs_column_effective_names = positioned_children
      .iter()
      .any(|&child_ptr| unsafe { &*child_ptr }.style.grid_column_raw.is_some());
    let needs_row_effective_names = positioned_children
      .iter()
      .any(|&child_ptr| unsafe { &*child_ptr }.style.grid_row_raw.is_some());

    let effective_column_line_names = needs_column_effective_names
      .then(|| {
        resolve_effective_grid_line_names_for_node_axis(
          taffy,
          node_id,
          CssGridAxis::Column,
        )
      })
      .flatten();
    let effective_row_line_names = needs_row_effective_names
      .then(|| {
        resolve_effective_grid_line_names_for_node_axis(
          taffy,
          node_id,
          CssGridAxis::Row,
        )
      })
      .flatten();

    for &child_ptr in positioned_children {
      let child = unsafe { &*child_ptr };
      let child_id = ensure_box_id(child);
      let mut pos = Point::ZERO;
      let mut override_area = GridAreaOverride::default();
      let (x_start, x_end, x_raw, x_ctx, x_line_count) = if axes_swapped {
        (
          child.style.grid_row_start,
          child.style.grid_row_end,
          child.style.grid_row_raw.as_deref(),
          row_subgrid_ctx.as_ref(),
          col_explicit_line_count.or_else(|| {
            // Subgrid fallback: line numbers are local to the subgrid, which inherits the number
            // of tracks it spans in the parent axis.
            let start = box_node.style.grid_row_start;
            let end = box_node.style.grid_row_end;
            (start > 0 && end > start)
              .then(|| u16::try_from((end - start + 1).max(0)).ok())
              .flatten()
          }),
        )
      } else {
        (
          child.style.grid_column_start,
          child.style.grid_column_end,
          child.style.grid_column_raw.as_deref(),
          col_subgrid_ctx.as_ref(),
          col_explicit_line_count.or_else(|| {
            let start = box_node.style.grid_column_start;
            let end = box_node.style.grid_column_end;
            (start > 0 && end > start)
              .then(|| u16::try_from((end - start + 1).max(0)).ok())
              .flatten()
          }),
        )
      };
      let x_line_names: &[Vec<String>] = if axes_swapped {
        effective_row_line_names
          .as_deref()
          .unwrap_or_else(|| box_node.style.grid_row_line_names.as_slice())
      } else {
        effective_column_line_names
          .as_deref()
          .unwrap_or_else(|| box_node.style.grid_column_line_names.as_slice())
      };
      // For abspos/fixed children, the grid container's padding box is the default containing block.
      // Per CSS Grid, if the child specifies a *non-auto* placement on an axis, that axis should
      // resolve percentages against the grid area instead.
      //
      // Treat explicit `grid-*-*: auto` (raw placements) the same as unset/auto.
      let x_is_auto = if let Some(raw) = x_raw {
        let parsed = parse_grid_line_placement_raw(raw, Some(x_line_names));
        matches!(
          grid_placement_component_from_taffy(&parsed.start),
          ResolvedGridPlacementComponent::Auto
        ) && matches!(
          grid_placement_component_from_taffy(&parsed.end),
          ResolvedGridPlacementComponent::Auto
        )
      } else {
        x_start == 0 && x_end == 0
      };
      if let Some((start_line, end_line)) =
        resolve_grid_line_range_from_style(x_start, x_end, x_raw, x_line_count, Some(x_line_names))
      {
        if let Some(col_offsets) = col_offsets.as_ref() {
          if let Some((area_start, area_end)) =
            grid_area_for_positioned_item(col_offsets, start_line, end_line)
          {
            let mut start = area_start;
            if mirror_x {
              if let Some(&span_start) = col_offsets.get(1) {
                if let Some(mut span_end) = col_offsets.last().copied() {
                  if col_alignment == Some(TaffyAlignContent::Stretch) {
                    let content_end = bounds.width() - padding_right - border_right;
                    span_end = content_end.max(span_end);
                  }
                  start = span_start + (span_end - area_end);
                }
              }
            }
            pos.x = start - padding_origin.x;
            if !x_is_auto {
              let size = (area_end - area_start).max(0.0);
              if size.is_finite() && start.is_finite() {
                override_area.x = Some((start, start + size));
              }
            }
          }
        } else if let Some(ctx) = x_ctx {
          let mapped_start = ctx.line_offset.saturating_add(start_line);
          let mapped_end = ctx.line_offset.saturating_add(end_line);
          if let Some((area_start, area_end)) =
            grid_area_for_positioned_item(&ctx.offsets, mapped_start, mapped_end)
          {
            let area_start = area_start - ctx.node_offset;
            let area_end = area_end - ctx.node_offset;
            let mut start = area_start;
            if mirror_x {
              start = ctx.span_start + (ctx.span_end - area_end);
            }
            pos.x = start - padding_origin.x;
            if !x_is_auto {
              let size = (area_end - area_start).max(0.0);
              if size.is_finite() && start.is_finite() {
                override_area.x = Some((start, start + size));
              }
            }
          }
        }
      }

      let (y_start, y_end, y_raw, y_ctx, y_line_count) = if axes_swapped {
        (
          child.style.grid_column_start,
          child.style.grid_column_end,
          child.style.grid_column_raw.as_deref(),
          col_subgrid_ctx.as_ref(),
          row_explicit_line_count.or_else(|| {
            let start = box_node.style.grid_column_start;
            let end = box_node.style.grid_column_end;
            (start > 0 && end > start)
              .then(|| u16::try_from((end - start + 1).max(0)).ok())
              .flatten()
          }),
        )
      } else {
        (
          child.style.grid_row_start,
          child.style.grid_row_end,
          child.style.grid_row_raw.as_deref(),
          row_subgrid_ctx.as_ref(),
          row_explicit_line_count.or_else(|| {
            let start = box_node.style.grid_row_start;
            let end = box_node.style.grid_row_end;
            (start > 0 && end > start)
              .then(|| u16::try_from((end - start + 1).max(0)).ok())
              .flatten()
          }),
        )
      };
      let y_line_names: &[Vec<String>] = if axes_swapped {
        effective_column_line_names
          .as_deref()
          .unwrap_or_else(|| box_node.style.grid_column_line_names.as_slice())
      } else {
        effective_row_line_names
          .as_deref()
          .unwrap_or_else(|| box_node.style.grid_row_line_names.as_slice())
      };
      let y_is_auto = if let Some(raw) = y_raw {
        let parsed = parse_grid_line_placement_raw(raw, Some(y_line_names));
        matches!(
          grid_placement_component_from_taffy(&parsed.start),
          ResolvedGridPlacementComponent::Auto
        ) && matches!(
          grid_placement_component_from_taffy(&parsed.end),
          ResolvedGridPlacementComponent::Auto
        )
      } else {
        y_start == 0 && y_end == 0
      };
      if let Some((start_line, end_line)) =
        resolve_grid_line_range_from_style(y_start, y_end, y_raw, y_line_count, Some(y_line_names))
      {
        if let Some(row_offsets) = row_offsets.as_ref() {
          if let Some((area_start, area_end)) =
            grid_area_for_positioned_item(row_offsets, start_line, end_line)
          {
            let mut start = area_start;
            if mirror_y {
              if let Some(&span_start) = row_offsets.get(1) {
                if let Some(mut span_end) = row_offsets.last().copied() {
                  if row_alignment == Some(TaffyAlignContent::Stretch) {
                    let content_end = bounds.height() - padding_bottom - border_bottom;
                    span_end = content_end.max(span_end);
                  }
                  start = span_start + (span_end - area_end);
                }
              }
            }
            pos.y = start - padding_origin.y;
            if !y_is_auto {
              let size = (area_end - area_start).max(0.0);
              if size.is_finite() && start.is_finite() {
                override_area.y = Some((start, start + size));
              }
            }
          }
        } else if let Some(ctx) = y_ctx {
          let mapped_start = ctx.line_offset.saturating_add(start_line);
          let mapped_end = ctx.line_offset.saturating_add(end_line);
          if let Some((area_start, area_end)) =
            grid_area_for_positioned_item(&ctx.offsets, mapped_start, mapped_end)
          {
            let area_start = area_start - ctx.node_offset;
            let area_end = area_end - ctx.node_offset;
            let mut start = area_start;
            if mirror_y {
              start = ctx.span_start + (ctx.span_end - area_end);
            }
            pos.y = start - padding_origin.y;
            if !y_is_auto {
              let size = (area_end - area_start).max(0.0);
              if size.is_finite() && start.is_finite() {
                override_area.y = Some((start, start + size));
              }
            }
          }
        }
      }
      static_positions.insert(child_id, pos);
      if override_area.x.is_some() || override_area.y.is_some() {
        grid_area_overrides.insert(child_id, override_area);
      }
    }

    let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
      self.font_context.clone(),
    );
    let mut fragments = Vec::with_capacity(positioned_children.len());
    let mut deadline_counter = 0usize;
    for &child_ptr in positioned_children {
      check_layout_deadline(&mut deadline_counter)?;
      let child = unsafe { &*child_ptr };

      let child_id = ensure_box_id(child);

      let mut cb = match child.style.position {
        crate::style::position::Position::Fixed => cb_for_fixed,
        _ => cb_for_absolute,
      };
      let mut cb_origin_delta = Point::ZERO;
      if cb == padding_cb {
        if let Some(area) = grid_area_overrides.get(&child_id) {
          let mut rect = cb.rect;
          let old_origin = rect.origin;
          if let Some((x0, x1)) = area.x {
            rect.origin.x = x0;
            rect.size.width = (x1 - x0).max(0.0);
          }
          if let Some((y0, y1)) = area.y {
            rect.origin.y = y0;
            rect.size.height = (y1 - y0).max(0.0);
          }
          if rect != cb.rect {
            cb_origin_delta =
              Point::new(rect.origin.x - old_origin.x, rect.origin.y - old_origin.y);
            let wm = cb.writing_mode;
            let dir = cb.direction;
            let box_id = cb.box_id();
            cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
              rect,
              cb.viewport_size(),
              cb.inline_percentage_base().map(|_| rect.size.width),
              cb.block_percentage_base().map(|_| rect.size.height),
            )
            .with_writing_mode_and_direction(wm, dir)
            .with_box_id(box_id);
          }
        }
      }

      let fc_type = child
        .formatting_context()
        .unwrap_or(crate::style::display::FormattingContextType::Block);
      let fc = abs_factory.get(fc_type);
      let child_constraints = LayoutConstraints::new(
        CrateAvailableSpace::Definite(padding_rect.size.width),
        block_base
          .map(CrateAvailableSpace::Definite)
          .unwrap_or(CrateAvailableSpace::Indefinite),
      );

      let positioned_style = crate::layout::absolute_positioning::resolve_positioned_style(
        &child.style,
        &cb,
        self.viewport_size,
        &self.font_context,
      );
      // Static position resolves to where the element would be in flow, relative to the containing
      // block origin (padding edge).
      let mut static_pos = static_positions
        .get(&child_id)
        .copied()
        .unwrap_or(crate::geometry::Point::ZERO);
      if cb_origin_delta != Point::ZERO {
        static_pos = Point::new(
          static_pos.x - cb_origin_delta.x,
          static_pos.y - cb_origin_delta.y,
        );
      }
      let width_keyword = child.style.width_keyword;
      let min_width_keyword = child.style.min_width_keyword;
      let max_width_keyword = child.style.max_width_keyword;
      let height_keyword = child.style.height_keyword;
      let min_height_keyword = child.style.min_height_keyword;
      let max_height_keyword = child.style.max_height_keyword;
      let has_inline_keyword =
        width_keyword.is_some() || min_width_keyword.is_some() || max_width_keyword.is_some();
      let has_block_keyword =
        height_keyword.is_some() || min_height_keyword.is_some() || max_height_keyword.is_some();
      let needs_inline_intrinsics = has_inline_keyword
        || (positioned_style.width.is_auto()
          && (positioned_style.left.is_auto()
            || positioned_style.right.is_auto()
            || child.is_replaced()));
      let needs_block_intrinsics = has_block_keyword
        || (positioned_style.height.is_auto()
          && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()));
      let mut static_style = (*child.style).clone();
      static_style.position = crate::style::position::Position::Relative;
      static_style.top = crate::style::types::InsetValue::Auto;
      static_style.right = crate::style::types::InsetValue::Auto;
      static_style.bottom = crate::style::types::InsetValue::Auto;
      static_style.left = crate::style::types::InsetValue::Auto;
      let static_style = Arc::new(static_style);

      let (
        mut child_fragment,
        preferred_min_inline,
        preferred_inline,
        preferred_min_block,
        preferred_block,
      ) = if child.id != 0 {
        crate::layout::style_override::with_style_override(child.id, static_style.clone(), || {
          let child_fragment = fc.layout(child, &child_constraints)?;
          let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
            match fc.compute_intrinsic_inline_sizes(child) {
              Ok((min, max)) => (Some(min), Some(max)),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => {
                let min =
                  match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MinContent) {
                    Ok(size) => Some(size),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                let max =
                  match fc.compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent) {
                    Ok(size) => Some(size),
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
            match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MinContent) {
              Ok(size) => Some(size),
              Err(err @ LayoutError::Timeout { .. }) => return Err(err),
              Err(_) => None,
            }
          } else {
            None
          };
          let preferred_block = if needs_block_intrinsics {
            match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MaxContent) {
              Ok(size) => Some(size),
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
        })?
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
                Ok(size) => Some(size),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              };
              let max = match fc
                .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
              {
                Ok(size) => Some(size),
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
            Ok(size) => Some(size),
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => None,
          }
        } else {
          None
        };
        let preferred_block = if needs_block_intrinsics {
          match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent) {
            Ok(size) => Some(size),
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
          &child.style,
          self.viewport_size,
          &self.font_context,
        );
      let preferred_min_inline = preferred_min_inline.map(|v| (v - intrinsic_horizontal).max(0.0));
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
      input.style.width_keyword = width_keyword;
      input.style.min_width_keyword = min_width_keyword;
      input.style.max_width_keyword = max_width_keyword;
      input.style.height_keyword = height_keyword;
      input.style.min_height_keyword = min_height_keyword;
      input.style.max_height_keyword = max_height_keyword;
      input.is_replaced = child.is_replaced();
      input.preferred_min_inline_size = preferred_min_inline;
      input.preferred_inline_size = preferred_inline;
      input.preferred_min_block_size = preferred_min_block;
      input.preferred_block_size = preferred_block;
      let supports_used_border_box = matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Flex
          | FormattingContextType::Grid
          | FormattingContextType::Inline
          | FormattingContextType::Table
      );

      let anchor_query = crate::layout::anchor_positioning::AnchorQueryContext::default();
      let (mut layout_positioned_style, mut result) =
        crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
          &abs,
          &input,
          &child.style,
          &cb,
          self.viewport_size,
          &self.font_context,
          None,
          anchor_query,
        )?;
      let mut border_size = Size::new(
        result.size.width + actual_horizontal,
        result.size.height + actual_vertical,
      );
      let mut border_origin = Point::new(
        result.position.x - content_offset.x,
        result.position.y - content_offset.y,
      );

      if crate::layout::absolute_positioning::auto_height_uses_intrinsic_size(
        &layout_positioned_style,
        input.is_replaced,
      ) && (border_size.width - child_fragment.bounds.width()).abs() > 0.01
      {
        let measure_constraints = child_constraints
          .with_width(CrateAvailableSpace::Definite(border_size.width))
          .with_height(CrateAvailableSpace::Indefinite)
          .with_used_border_box_size(Some(border_size.width), None);
        child_fragment = if child.id != 0 {
          if supports_used_border_box {
            crate::layout::style_override::with_style_override(
              child.id,
              static_style.clone(),
              || fc.layout(child, &measure_constraints),
            )?
          } else {
            let mut measure_style = (*static_style).clone();
            measure_style.width = Some(Length::px(border_size.width));
            measure_style.width_keyword = None;
            measure_style.min_width_keyword = None;
            measure_style.max_width_keyword = None;
            crate::layout::style_override::with_style_override(
              child.id,
              Arc::new(measure_style),
              || fc.layout(child, &measure_constraints),
            )?
          }
        } else {
          let mut relayout_child = child.clone();
          if supports_used_border_box {
            relayout_child.style = static_style.clone();
          } else {
            let mut measure_style = (*static_style).clone();
            measure_style.width = Some(Length::px(border_size.width));
            measure_style.width_keyword = None;
            measure_style.min_width_keyword = None;
            measure_style.max_width_keyword = None;
            relayout_child.style = Arc::new(measure_style);
          }
          fc.layout(&relayout_child, &measure_constraints)?
        };
        input.intrinsic_size.height =
          (child_fragment.bounds.size.height - actual_vertical).max(0.0);
        (layout_positioned_style, result) =
          crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
            &abs,
            &input,
            &child.style,
            &cb,
            self.viewport_size,
            &self.font_context,
            None,
            anchor_query,
          )?;
        border_size = Size::new(
          result.size.width + actual_horizontal,
          result.size.height + actual_vertical,
        );
        border_origin = Point::new(
          result.position.x - content_offset.x,
          result.position.y - content_offset.y,
        );
      }

      let relayout_for_inset_resolved_size =
        crate::layout::absolute_positioning::auto_size_resolved_by_insets(&layout_positioned_style);
      let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
        || (border_size.height - child_fragment.bounds.height()).abs() > 0.01
        || relayout_for_inset_resolved_size;
      if needs_relayout {
        let relayout_constraints = child_constraints
          .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
        if child.id != 0 {
          if supports_used_border_box {
            child_fragment = crate::layout::style_override::with_style_override(
              child.id,
              static_style.clone(),
              || fc.layout(child, &relayout_constraints),
            )?;
          } else {
            let mut relayout_style = (*static_style).clone();
            relayout_style.width = Some(Length::px(border_size.width));
            relayout_style.height = Some(Length::px(border_size.height));
            relayout_style.width_keyword = None;
            relayout_style.height_keyword = None;
            child_fragment = crate::layout::style_override::with_style_override(
              child.id,
              Arc::new(relayout_style),
              || fc.layout(child, &relayout_constraints),
            )?;
          }
        } else {
          let mut relayout_child = child.clone();
          if supports_used_border_box {
            relayout_child.style = static_style.clone();
          } else {
            let mut relayout_style = (*static_style).clone();
            relayout_style.width = Some(Length::px(border_size.width));
            relayout_style.height = Some(Length::px(border_size.height));
            relayout_style.width_keyword = None;
            relayout_style.height_keyword = None;
            relayout_child.style = Arc::new(relayout_style);
          }
          child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
        }
      }
      child_fragment.bounds = crate::geometry::Rect::new(border_origin, border_size);
      if matches!(child.style.position, crate::style::position::Position::Absolute) {
        child_fragment.abs_containing_block_box_id = cb.box_id();
      }
      child_fragment.style = Some(child.style.clone());
      fragments.push(child_fragment);
    }

    Ok(fragments)
  }

  fn alignment_for_axis(
    &self,
    axis: Axis,
    child_style: &taffy::style::Style,
    container_style: &taffy::style::Style,
  ) -> taffy::style::AlignItems {
    let fallback = match axis {
      Axis::Vertical => {
        if !child_style.size.height.is_auto() || child_style.aspect_ratio.is_some() {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::Stretch
        }
      }
      Axis::Horizontal => {
        if !child_style.size.width.is_auto() {
          taffy::style::AlignItems::Start
        } else {
          taffy::style::AlignItems::Stretch
        }
      }
    };

    match axis {
      Axis::Vertical => child_style
        .align_self
        .or(container_style.align_items)
        .unwrap_or(fallback),
      Axis::Horizontal => child_style
        .justify_self
        .or(container_style.justify_items)
        .unwrap_or(fallback),
    }
  }

  fn baseline_offset_with_fallback(
    &self,
    fragment: &FragmentNode,
    axis: Axis,
    deadline_counter: &mut usize,
  ) -> Result<Option<f32>, LayoutError> {
    let baseline = match axis {
      Axis::Vertical => first_baseline_offset(fragment, deadline_counter)?,
      Axis::Horizontal => {
        let writing_mode = fragment
          .style
          .as_ref()
          .map(|style| style.writing_mode)
          .unwrap_or(WritingMode::HorizontalTb);
        first_baseline_offset_x(fragment, writing_mode, deadline_counter)?
      }
    };
    if let Some(offset) = baseline {
      return Ok(Some(offset));
    }

    let size = match axis {
      Axis::Vertical => fragment.bounds.height(),
      Axis::Horizontal => fragment.bounds.width(),
    };
    if size.is_finite() && size > 0.0 {
      Ok(Some(size))
    } else {
      Ok(None)
    }
  }

  fn apply_baseline_group(
    &self,
    axis: Axis,
    group: &[BaselineItem],
    fragment: &mut FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    // Baseline alignment only has an effect when two or more items participate in the same
    // baseline-sharing group.
    if group.len() <= 1 {
      return Ok(());
    }

    let debug_baseline =
      crate::debug::runtime::runtime_toggles().truthy("FASTR_DEBUG_GRID_BASELINE");

    let mut target = 0.0;
    for item in group {
      check_layout_deadline(deadline_counter)?;
      let area_size = (item.area_end - item.area_start).max(0.0);
      let clamped = item.baseline.min(area_size);
      if clamped > target {
        target = clamped;
      }
    }

    if debug_baseline {
      let axis_name = match axis {
        Axis::Horizontal => "horizontal",
        Axis::Vertical => "vertical",
      };
      eprintln!(
        "[grid-baseline] axis={} group_len={} target={:.2}",
        axis_name,
        group.len(),
        target
      );
    }

    for item in group {
      check_layout_deadline(deadline_counter)?;
      let area_size = (item.area_end - item.area_start).max(0.0);
      if area_size <= 0.0 {
        if debug_baseline {
          eprintln!(
            "[grid-baseline] idx={} non-positive area (area_start={:.2} area_end={:.2})",
            item.idx, item.area_start, item.area_end
          );
        }
        continue;
      }
      let upper = (item.area_end - item.size).max(item.area_start);
      let desired_start = (item.area_start + target - item.baseline).clamp(item.area_start, upper);
      let delta = desired_start - item.start;
      if delta.abs() > 0.01 {
        if debug_baseline {
          eprintln!(
            "[grid-baseline] idx={} area=({:.2},{:.2}) baseline={:.2} start={:.2} size={:.2} desired_start={:.2} delta={:.2}",
            item.idx,
            item.area_start,
            item.area_end,
            item.baseline,
            item.start,
            item.size,
            desired_start,
            delta
          );
        }
        if let Some(child) = fragment.children_mut().get_mut(item.idx) {
          translate_along_axis(child, axis, delta, deadline_counter)?;
        }
      } else if debug_baseline {
        eprintln!(
          "[grid-baseline] idx={} area=({:.2},{:.2}) baseline={:.2} start={:.2} size={:.2} desired_start={:.2} delta={:.4}",
          item.idx,
          item.area_start,
          item.area_end,
          item.baseline,
          item.start,
          item.size,
          desired_start,
          delta
        );
      }
    }
    Ok(())
  }

  fn apply_grid_baseline_alignment(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    layout: &TaffyLayout,
    child_count: usize,
    fragment: &mut FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    let detailed = match taffy.detailed_layout_info(node_id) {
      DetailedLayoutInfo::Grid(info) => info,
      _ => return Ok(()),
    };
    if fragment.children.is_empty() {
      return Ok(());
    }
    if child_count != fragment.children.len() {
      return Ok(());
    }
    if detailed.items.len() < child_count {
      return Ok(());
    }
    let item_infos = &detailed.items[..child_count];

    let container_style = match taffy.style(node_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };

    let row_offsets = compute_track_offsets(
      &detailed.rows,
      layout.size.height,
      layout.padding.top,
      layout.padding.bottom,
      layout.border.top,
      layout.border.bottom,
      container_style
        .align_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );
    let col_offsets = compute_track_offsets(
      &detailed.columns,
      layout.size.width,
      layout.padding.left,
      layout.padding.right,
      layout.border.left,
      layout.border.right,
      container_style
        .justify_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );

    let debug_baseline =
      crate::debug::runtime::runtime_toggles().truthy("FASTR_DEBUG_GRID_BASELINE");
    if debug_baseline {
      eprintln!(
        "[grid-baseline] rows sizes={:?} gutters={:?} offsets={:?}",
        detailed.rows.sizes, detailed.rows.gutters, row_offsets
      );
      eprintln!(
        "[grid-baseline] cols sizes={:?} gutters={:?} offsets={:?}",
        detailed.columns.sizes, detailed.columns.gutters, col_offsets
      );
    }

    let mut row_groups: FxHashMap<u16, Vec<BaselineItem>> = FxHashMap::default();
    let mut col_groups: FxHashMap<u16, Vec<BaselineItem>> = FxHashMap::default();

    for idx in 0..child_count {
      check_layout_deadline(deadline_counter)?;
      let child_id = taffy.get_child_id(node_id, idx);
      let item_info = &item_infos[idx];
      let child_fragment = &fragment.children[idx];
      let child_style = match taffy.style(child_id) {
        Ok(style) => style,
        Err(_) => continue,
      };

      if self.alignment_for_axis(Axis::Vertical, child_style, container_style)
        == taffy::style::AlignItems::Baseline
      {
        let baseline =
          self.baseline_offset_with_fallback(child_fragment, Axis::Vertical, deadline_counter)?;
        if let (Some((area_start, area_end)), Some(baseline)) = (
          grid_area_for_item(&row_offsets, item_info.row_start, item_info.row_end),
          baseline,
        ) {
          if debug_baseline {
            eprintln!(
              "[grid-baseline] idx={} row=({},{}) area=({:.2},{:.2}) start={:.2} size={:.2} baseline={:.2}",
              idx,
              item_info.row_start,
              item_info.row_end,
              area_start,
              area_end,
              child_fragment.bounds.y(),
              child_fragment.bounds.height(),
              baseline
            );
          }
          row_groups
            .entry(item_info.row_start)
            .or_default()
            .push(BaselineItem {
              idx,
              area_start,
              area_end,
              baseline,
              start: child_fragment.bounds.y(),
              size: child_fragment.bounds.height(),
            });
        }
      }

      if self.alignment_for_axis(Axis::Horizontal, child_style, container_style)
        == taffy::style::AlignItems::Baseline
      {
        let baseline =
          self.baseline_offset_with_fallback(child_fragment, Axis::Horizontal, deadline_counter)?;
        if let (Some((area_start, area_end)), Some(baseline)) = (
          grid_area_for_item(&col_offsets, item_info.column_start, item_info.column_end),
          baseline,
        ) {
          if debug_baseline {
            eprintln!(
              "[grid-baseline] idx={} col=({},{}) area=({:.2},{:.2}) start={:.2} size={:.2} baseline={:.2}",
              idx,
              item_info.column_start,
              item_info.column_end,
              area_start,
              area_end,
              child_fragment.bounds.x(),
              child_fragment.bounds.width(),
              baseline
            );
          }
          col_groups
            .entry(item_info.column_start)
            .or_default()
            .push(BaselineItem {
              idx,
              area_start,
              area_end,
              baseline,
              start: child_fragment.bounds.x(),
              size: child_fragment.bounds.width(),
            });
        }
      }
    }

    for group in row_groups.values() {
      self.apply_baseline_group(Axis::Vertical, group, fragment, deadline_counter)?;
    }
    for group in col_groups.values() {
      self.apply_baseline_group(Axis::Horizontal, group, fragment, deadline_counter)?;
    }
    Ok(())
  }

  fn apply_grid_axis_mirroring(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    layout: &TaffyLayout,
    fragment: &mut FragmentNode,
    axis_style: GridAxisStyle,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    if fragment.children.is_empty() {
      return Ok(());
    }

    let inline_is_horizontal = axis_style.inline_is_horizontal();
    let mut mirror_x = false;
    let mut mirror_y = false;

    if !axis_style.inline_positive() {
      if inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !axis_style.block_positive() {
      if inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }

    if !mirror_x && !mirror_y {
      return Ok(());
    }

    let container_style = match taffy.style(node_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };
    let taffy_child_count = taffy.child_count(node_id);

    let compute_region = |axis: Axis, children: &[FragmentNode]| -> Option<(f32, f32)> {
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        let (offsets, alignment, content_end) = match axis {
          Axis::Horizontal => (
            compute_track_offsets(
              &info.columns,
              layout.size.width,
              layout.padding.left,
              layout.padding.right,
              layout.border.left,
              layout.border.right,
              container_style
                .justify_content
                .unwrap_or(TaffyAlignContent::Stretch),
            ),
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
            layout.size.width - layout.padding.right - layout.border.right,
          ),
          Axis::Vertical => (
            compute_track_offsets(
              &info.rows,
              layout.size.height,
              layout.padding.top,
              layout.padding.bottom,
              layout.border.top,
              layout.border.bottom,
              container_style
                .align_content
                .unwrap_or(TaffyAlignContent::Stretch),
            ),
            container_style
              .align_content
              .unwrap_or(TaffyAlignContent::Stretch),
            layout.size.height - layout.padding.bottom - layout.border.bottom,
          ),
        };
        if offsets.len() >= 2 {
          let start = *offsets.get(1)?;
          let mut end = *offsets.last()?;
          if alignment == TaffyAlignContent::Stretch {
            end = content_end.max(end);
          }
          return Some((start, end));
        }
      }

      // Fallback: infer the span of the used grid tracks from child positions. This is less robust
      // when tracks are empty, but avoids direction bugs for subgrids that don't expose detailed
      // track info via Taffy.
      let mut min_start = f32::INFINITY;
      let mut max_end = f32::NEG_INFINITY;
      for child in children {
        match axis {
          Axis::Horizontal => {
            min_start = min_start.min(child.bounds.x());
            max_end = max_end.max(child.bounds.x() + child.bounds.width());
          }
          Axis::Vertical => {
            min_start = min_start.min(child.bounds.y());
            max_end = max_end.max(child.bounds.y() + child.bounds.height());
          }
        }
      }
      if min_start.is_finite() && max_end.is_finite() && max_end >= min_start {
        Some((min_start, max_end))
      } else {
        None
      }
    };

    let (x_region, y_region) = {
      let children = fragment.children.as_slice();
      (
        mirror_x
          .then(|| compute_region(Axis::Horizontal, children))
          .flatten(),
        mirror_y
          .then(|| compute_region(Axis::Vertical, children))
          .flatten(),
      )
    };

    let mut rtl_mirrored_x = false;
    if mirror_x && inline_is_horizontal && !axis_style.inline_positive() {
      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let width = child_fragment.bounds.width();
        if !width.is_finite() || width <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.x();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + width);

        let alignment = self.alignment_for_axis(Axis::Horizontal, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - width - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - width).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Horizontal, delta, deadline_counter)
      };

      // Prefer using detailed track/item info from Taffy when available.
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && taffy_child_count == fragment.children.len()
          && !info.columns.sizes.is_empty()
        {
          let col_offsets = compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.columns.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&col_offsets, 1, track_count.saturating_add(1))
          {
            let mut span_end = span_end;
            if container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch)
              == TaffyAlignContent::Stretch
            {
              let content_end = layout.size.width - layout.padding.right - layout.border.right;
              span_end = content_end.max(span_end);
            }
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&col_offsets, item.column_start, item.column_end)
              {
                let child_id = taffy.get_child_id(node_id, idx);
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_id,
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let width = child.bounds.width();
                let old_start = child.bounds.x();
                let new_start = span_start + (span_end - (old_start + width));
                translate_along_axis(
                  child,
                  Axis::Horizontal,
                  new_start - old_start,
                  deadline_counter,
                )?;
              }
            }
            rtl_mirrored_x = true;
          }
        }
      }

      // Subgrid nodes sometimes don't expose detailed track info. When possible, derive the
      // inherited column offsets from the nearest ancestor grid and map local line numbers into that
      // line space.
      if !rtl_mirrored_x {
        if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
          let box_node = unsafe { &*box_node_ptr };
          if box_node.style.grid_column_subgrid {
            #[derive(Clone)]
            struct SubgridAxisContext {
              offsets: Vec<f32>,
              line_offset: u16,
              node_offset: f32,
            }

            let mut line_offset: i32 = 0;
            let mut node_offset: f32 = 0.0;
            let mut current = node_id;
            let mut ctx: Option<SubgridAxisContext> = None;
            loop {
              let Some(&current_ptr) = taffy.get_node_context(current) else {
                break;
              };
              let current_box_node = unsafe { &*current_ptr };
              if !current_box_node.style.grid_column_subgrid {
                break;
              }
              let start_line = current_box_node.style.grid_column_start;
              if start_line <= 0 {
                break;
              }
              line_offset = line_offset.saturating_add(start_line.saturating_sub(1));
              let layout = match taffy.layout(current) {
                Ok(layout) => layout,
                Err(_) => break,
              };
              node_offset += layout.location.x;

              let Some(parent) = taffy.parent(current) else {
                break;
              };
              current = parent;

              if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
                let Ok(ancestor_style) = taffy.style(current) else {
                  break;
                };
                let Ok(ancestor_layout) = taffy.layout(current) else {
                  break;
                };
                let offsets = compute_track_offsets(
                  &info.columns,
                  ancestor_layout.size.width,
                  ancestor_layout.padding.left,
                  ancestor_layout.padding.right,
                  ancestor_layout.border.left,
                  ancestor_layout.border.right,
                  ancestor_style
                    .justify_content
                    .unwrap_or(TaffyAlignContent::Stretch),
                );
                let line_offset = match u16::try_from(line_offset) {
                  Ok(offset) => offset,
                  Err(_) => break,
                };
                ctx = Some(SubgridAxisContext {
                  offsets,
                  line_offset,
                  node_offset,
                });
                break;
              }
            }

            if let Some(ctx) = ctx {
              let span_tracks =
                (box_node.style.grid_column_end - box_node.style.grid_column_start).max(0);
              if span_tracks > 0 {
                let local_end_line = (span_tracks as u16).saturating_add(1);
                let mapped_start = ctx.line_offset.saturating_add(1);
                let mapped_end = ctx.line_offset.saturating_add(local_end_line);
                if let Some((span_start, span_end)) =
                  grid_area_for_item(&ctx.offsets, mapped_start, mapped_end)
                {
                  let span_start = span_start - ctx.node_offset;
                  let span_end = span_end - ctx.node_offset;
                  let children = fragment.children_mut();
                  for idx in 0..children.len() {
                    check_layout_deadline(deadline_counter)?;
                    let child_id = if idx < taffy_child_count {
                      taffy.get_child_id(node_id, idx)
                    } else {
                      node_id
                    };
                    let mut translated = false;
                    if let Some(&child_ptr) = taffy.get_node_context(child_id) {
                      let child_node = unsafe { &*child_ptr };
                      if child_node.style.grid_column_start > 0
                        && child_node.style.grid_column_end > 0
                      {
                        let child_start = child_node.style.grid_column_start as u16;
                        let child_end = child_node.style.grid_column_end as u16;
                        let mapped_child_start = ctx.line_offset.saturating_add(child_start);
                        let mapped_child_end = ctx.line_offset.saturating_add(child_end);
                        if let Some((area_start, area_end)) =
                          grid_area_for_item(&ctx.offsets, mapped_child_start, mapped_child_end)
                        {
                          let area_start = area_start - ctx.node_offset;
                          let area_end = area_end - ctx.node_offset;
                          apply_translation(
                            area_start,
                            area_end,
                            span_start,
                            span_end,
                            child_id,
                            &mut children[idx],
                            deadline_counter,
                          )?;
                          translated = true;
                        }
                      }
                    }

                    if !translated {
                      let child = &mut children[idx];
                      let width = child.bounds.width();
                      let old_start = child.bounds.x();
                      let new_start = span_start + (span_end - (old_start + width));
                      translate_along_axis(
                        child,
                        Axis::Horizontal,
                        new_start - old_start,
                        deadline_counter,
                      )?;
                    }
                  }
                  rtl_mirrored_x = true;
                }
              }
            }
          }
        }
      }
    }

    let mut block_mirrored_x = false;
    if mirror_x && !inline_is_horizontal && !axis_style.block_positive() {
      // For vertical writing modes with a negative block axis (e.g. `writing-mode: vertical-rl`),
      // we mirror the horizontal axis. The style conversion already maps `start`/`end` alignment
      // values into Taffy's coordinate system, so the mirroring step must preserve an item's
      // alignment within its grid area (rather than blindly mirroring the child bounding box).
      //
      // This mirrors the RTL-specific handling above, but applies when the block axis is the one
      // running right-to-left.
      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let width = child_fragment.bounds.width();
        if !width.is_finite() || width <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.x();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + width);

        let alignment = self.alignment_for_axis(Axis::Horizontal, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - width - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - width).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Horizontal, delta, deadline_counter)
      };

      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && taffy_child_count == fragment.children.len()
          && !info.columns.sizes.is_empty()
        {
          let col_offsets = compute_track_offsets(
            &info.columns,
            layout.size.width,
            layout.padding.left,
            layout.padding.right,
            layout.border.left,
            layout.border.right,
            container_style
              .justify_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.columns.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&col_offsets, 1, track_count.saturating_add(1))
          {
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&col_offsets, item.column_start, item.column_end)
              {
                let child_id = taffy.get_child_id(node_id, idx);
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_id,
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let width = child.bounds.width();
                let old_start = child.bounds.x();
                let new_start = span_start + (span_end - (old_start + width));
                translate_along_axis(
                  child,
                  Axis::Horizontal,
                  new_start - old_start,
                  deadline_counter,
                )?;
              }
            }
            block_mirrored_x = true;
          }
        }
      }
    }

    let mut rtl_mirrored_y = false;
    if mirror_y && !inline_is_horizontal && !axis_style.inline_positive() {
      let apply_translation = |area_start: f32,
                               area_end: f32,
                               span_start: f32,
                               span_end: f32,
                               child_id: TaffyNodeId,
                               child_fragment: &mut FragmentNode,
                               deadline_counter: &mut usize|
       -> Result<(), LayoutError> {
        let child_style = match taffy.style(child_id) {
          Ok(style) => style,
          Err(_) => return Ok(()),
        };
        let height = child_fragment.bounds.height();
        if !height.is_finite() || height <= 0.0 {
          return Ok(());
        }

        let (area_start, area_end) = if area_start <= area_end {
          (area_start, area_end)
        } else {
          (area_end, area_start)
        };
        let (span_start, span_end) = if span_start <= span_end {
          (span_start, span_end)
        } else {
          (span_end, span_start)
        };
        let span_size = span_end - span_start;
        if !span_size.is_finite() || span_size <= 0.0 {
          return Ok(());
        }

        let mirrored_area_start = span_start + (span_end - area_end);
        let mirrored_area_end = span_start + (span_end - area_start);

        let old_start = child_fragment.bounds.y();
        let offset_from_start = old_start - area_start;
        let offset_from_end = area_end - (old_start + height);

        let alignment = self.alignment_for_axis(Axis::Vertical, child_style, container_style);
        let unclamped_new_start = match alignment {
          taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => {
            mirrored_area_end - height - offset_from_end
          }
          _ => mirrored_area_start + offset_from_start,
        };

        let upper = (mirrored_area_end - height).max(mirrored_area_start);
        let new_start = unclamped_new_start.clamp(mirrored_area_start, upper);
        let delta = new_start - old_start;
        translate_along_axis(child_fragment, Axis::Vertical, delta, deadline_counter)
      };

      // Prefer using detailed track/item info from Taffy when available.
      if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
        if info.items.len() == fragment.children.len()
          && taffy_child_count == fragment.children.len()
          && !info.rows.sizes.is_empty()
        {
          let row_offsets = compute_track_offsets(
            &info.rows,
            layout.size.height,
            layout.padding.top,
            layout.padding.bottom,
            layout.border.top,
            layout.border.bottom,
            container_style
              .align_content
              .unwrap_or(TaffyAlignContent::Stretch),
          );
          let track_count = info.rows.sizes.len() as u16;
          if let Some((span_start, span_end)) =
            grid_area_for_item(&row_offsets, 1, track_count.saturating_add(1))
          {
            let children = fragment.children_mut();
            for idx in 0..children.len() {
              check_layout_deadline(deadline_counter)?;
              let item = &info.items[idx];
              if let Some((area_start, area_end)) =
                grid_area_for_item(&row_offsets, item.row_start, item.row_end)
              {
                let child_id = taffy.get_child_id(node_id, idx);
                apply_translation(
                  area_start,
                  area_end,
                  span_start,
                  span_end,
                  child_id,
                  &mut children[idx],
                  deadline_counter,
                )?;
              } else {
                let child = &mut children[idx];
                let height = child.bounds.height();
                let old_start = child.bounds.y();
                let new_start = span_start + (span_end - (old_start + height));
                translate_along_axis(
                  child,
                  Axis::Vertical,
                  new_start - old_start,
                  deadline_counter,
                )?;
              }
            }
            rtl_mirrored_y = true;
          }
        }
      }

      // Subgrid nodes sometimes don't expose detailed track info. When possible, derive the
      // inherited inline axis offsets from the nearest ancestor grid and map local line numbers
      // into that line space.
      if !rtl_mirrored_y {
        if let Some(&box_node_ptr) = taffy.get_node_context(node_id) {
          let box_node = unsafe { &*box_node_ptr };
          if box_node.style.grid_column_subgrid {
            #[derive(Clone)]
            struct SubgridAxisContext {
              offsets: Vec<f32>,
              line_offset: u16,
              node_offset: f32,
            }

            let mut line_offset: i32 = 0;
            let mut node_offset: f32 = 0.0;
            let mut current = node_id;
            let mut ctx: Option<SubgridAxisContext> = None;
            loop {
              let Some(&current_ptr) = taffy.get_node_context(current) else {
                break;
              };
              let current_box_node = unsafe { &*current_ptr };
              if !current_box_node.style.grid_column_subgrid {
                break;
              }
              let start_line = current_box_node.style.grid_column_start;
              if start_line <= 0 {
                break;
              }
              line_offset = line_offset.saturating_add(start_line.saturating_sub(1));
              let layout = match taffy.layout(current) {
                Ok(layout) => layout,
                Err(_) => break,
              };
              node_offset += layout.location.y;

              let Some(parent) = taffy.parent(current) else {
                break;
              };
              current = parent;

              if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(current) {
                let Ok(ancestor_style) = taffy.style(current) else {
                  break;
                };
                let Ok(ancestor_layout) = taffy.layout(current) else {
                  break;
                };
                let offsets = compute_track_offsets(
                  &info.rows,
                  ancestor_layout.size.height,
                  ancestor_layout.padding.top,
                  ancestor_layout.padding.bottom,
                  ancestor_layout.border.top,
                  ancestor_layout.border.bottom,
                  ancestor_style
                    .align_content
                    .unwrap_or(TaffyAlignContent::Stretch),
                );
                let line_offset = match u16::try_from(line_offset) {
                  Ok(offset) => offset,
                  Err(_) => break,
                };
                ctx = Some(SubgridAxisContext {
                  offsets,
                  line_offset,
                  node_offset,
                });
                break;
              }
            }

            if let Some(ctx) = ctx {
              let span_tracks =
                (box_node.style.grid_column_end - box_node.style.grid_column_start).max(0);
              if span_tracks > 0 {
                let local_end_line = (span_tracks as u16).saturating_add(1);
                let mapped_start = ctx.line_offset.saturating_add(1);
                let mapped_end = ctx.line_offset.saturating_add(local_end_line);
                if let Some((span_start, span_end)) =
                  grid_area_for_item(&ctx.offsets, mapped_start, mapped_end)
                {
                  let span_start = span_start - ctx.node_offset;
                  let span_end = span_end - ctx.node_offset;
                  let children = fragment.children_mut();
                  for idx in 0..children.len() {
                    check_layout_deadline(deadline_counter)?;
                    let child_id = if idx < taffy_child_count {
                      taffy.get_child_id(node_id, idx)
                    } else {
                      node_id
                    };
                    let mut translated = false;
                    if let Some(&child_ptr) = taffy.get_node_context(child_id) {
                      let child_node = unsafe { &*child_ptr };
                      if child_node.style.grid_column_start > 0
                        && child_node.style.grid_column_end > 0
                      {
                        let child_start = child_node.style.grid_column_start as u16;
                        let child_end = child_node.style.grid_column_end as u16;
                        let mapped_child_start = ctx.line_offset.saturating_add(child_start);
                        let mapped_child_end = ctx.line_offset.saturating_add(child_end);
                        if let Some((area_start, area_end)) =
                          grid_area_for_item(&ctx.offsets, mapped_child_start, mapped_child_end)
                        {
                          let area_start = area_start - ctx.node_offset;
                          let area_end = area_end - ctx.node_offset;
                          apply_translation(
                            area_start,
                            area_end,
                            span_start,
                            span_end,
                            child_id,
                            &mut children[idx],
                            deadline_counter,
                          )?;
                          translated = true;
                        }
                      }
                    }

                    if !translated {
                      let child = &mut children[idx];
                      let height = child.bounds.height();
                      let old_start = child.bounds.y();
                      let new_start = span_start + (span_end - (old_start + height));
                      translate_along_axis(
                        child,
                        Axis::Vertical,
                        new_start - old_start,
                        deadline_counter,
                      )?;
                    }
                  }
                  rtl_mirrored_y = true;
                }
              }
            }
          }
        }
      }
    }

    if mirror_x && !rtl_mirrored_x && !block_mirrored_x {
      if let Some((region_start, region_end)) = x_region {
        if region_end > region_start {
          for child in fragment.children_mut().iter_mut() {
            check_layout_deadline(deadline_counter)?;
            let child_end = child.bounds.x() + child.bounds.width();
            let new_start = region_start + region_end - child_end;
            let delta = new_start - child.bounds.x();
            translate_along_axis(child, Axis::Horizontal, delta, deadline_counter)?;
          }
        }
      }
    }

    if mirror_y && !rtl_mirrored_y {
      if let Some((region_start, region_end)) = y_region {
        if region_end > region_start {
          for child in fragment.children_mut().iter_mut() {
            check_layout_deadline(deadline_counter)?;
            let child_end = child.bounds.y() + child.bounds.height();
            let new_start = region_start + region_end - child_end;
            let delta = new_start - child.bounds.y();
            translate_along_axis(child, Axis::Vertical, delta, deadline_counter)?;
          }
        }
      }
    }

    Ok(())
  }

  fn maybe_shrink_auto_span_subgrid_bounds(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    box_node: &BoxNode,
    effective_axis_style: GridAxisStyle,
    has_positioned_children: bool,
    bounds: &mut Rect,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    // Workaround for Taffy's subgrid modeling: Taffy ties the subgrid item's span in the parent
    // grid to the number of inherited explicit tracks, and derives automatic spans from the length
    // of `subgrid_*_names`. For `grid-template-*: subgrid` without an explicit `<line-name-list>`,
    // FastRender expands the synthesized name list to the parent's track count so grandchildren can
    // be placed on inherited lines.
    //
    // In `subgrid-nested-writing-mode-001`, browsers expect the subgrid element itself to paint
    // only in its auto-placed single-track area (letting grandchildren overflow across inherited
    // tracks). Without this adjustment, the subgrid's semi-transparent background covers the gap
    // between inherited tracks and fails the reftest.
    if has_positioned_children {
      return Ok(());
    }
    if !(box_node.style.grid_row_subgrid || box_node.style.grid_column_subgrid) {
      return Ok(());
    }

    // Only apply this workaround when the subgrid establishes an orthogonal writing-mode relative
    // to the axis mapping used for track inheritance. This keeps normal subgrids (same writing-mode
    // as the containing grid) using Taffy's border-box sizing.
    let this_inline_is_horizontal =
      GridAxisStyle::from_style(&box_node.style).inline_is_horizontal();
    if this_inline_is_horizontal == effective_axis_style.inline_is_horizontal() {
      return Ok(());
    }

    let line_names_is_default = |names: &[Vec<String>]| -> bool {
      names.is_empty() || (names.len() == 1 && names[0].is_empty())
    };

    let auto_columns = box_node.style.grid_column_start == 0
      && box_node.style.grid_column_end == 0
      && box_node.style.grid_column_raw.is_none();
    let auto_rows =
      box_node.style.grid_row_start == 0 && box_node.style.grid_row_end == 0 && box_node.style.grid_row_raw.is_none();

    let wants_columns = box_node.style.grid_column_subgrid
      && auto_columns
      && line_names_is_default(&box_node.style.subgrid_column_line_names);
    let wants_rows = box_node.style.grid_row_subgrid
      && auto_rows
      && line_names_is_default(&box_node.style.subgrid_row_line_names);

    if !wants_columns && !wants_rows {
      return Ok(());
    }

    let Some(parent_id) = taffy.parent(node_id) else {
      return Ok(());
    };
    let parent_style = match taffy.style(parent_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };
    if parent_style.display != Display::Grid {
      return Ok(());
    }
    let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(parent_id) else {
      return Ok(());
    };
    let parent_child_count = taffy.child_count(parent_id);
    if info.items.len() != parent_child_count {
      return Ok(());
    }
    let idx = (0..parent_child_count).find(|&idx| taffy.get_child_id(parent_id, idx) == node_id);
    let Some(idx) = idx else {
      return Ok(());
    };
    let Some(placement) = info.items.get(idx) else {
      return Ok(());
    };

    let parent_layout = match taffy.layout(parent_id) {
      Ok(layout) => layout,
      Err(_) => return Ok(()),
    };
    let row_offsets = compute_track_offsets(
      &info.rows,
      parent_layout.size.height,
      parent_layout.padding.top,
      parent_layout.padding.bottom,
      parent_layout.border.top,
      parent_layout.border.bottom,
      parent_style
        .align_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );
    let col_offsets = compute_track_offsets(
      &info.columns,
      parent_layout.size.width,
      parent_layout.padding.left,
      parent_layout.padding.right,
      parent_layout.border.left,
      parent_layout.border.right,
      parent_style
        .justify_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );

    let cols_axis = if effective_axis_style.inline_is_horizontal() {
      PhysicalAxis::X
    } else {
      PhysicalAxis::Y
    };
    let rows_axis = if effective_axis_style.inline_is_horizontal() {
      PhysicalAxis::Y
    } else {
      PhysicalAxis::X
    };

    let shrink_axis = |axis: PhysicalAxis,
                           start_line: u16,
                           end_line: u16,
                           offsets: &[f32],
                           bounds: &mut Rect| {
      if end_line.saturating_sub(start_line) <= 1 {
        return;
      }
      let end_line = start_line.saturating_add(1);
      let Some((start, end)) = grid_area_for_item(offsets, start_line, end_line) else {
        return;
      };
      let size = (end - start).max(0.0);
      if !size.is_finite() || size <= 0.0 {
        return;
      }
      match axis {
        PhysicalAxis::X => {
          if size + 0.1 < bounds.width() {
            bounds.size.width = size;
          }
        }
        PhysicalAxis::Y => {
          if size + 0.1 < bounds.height() {
            bounds.size.height = size;
          }
        }
      }
    };

    check_layout_deadline(deadline_counter)?;
    if wants_columns {
      match cols_axis {
        PhysicalAxis::X => shrink_axis(
          PhysicalAxis::X,
          placement.column_start,
          placement.column_end,
          &col_offsets,
          bounds,
        ),
        PhysicalAxis::Y => shrink_axis(
          PhysicalAxis::Y,
          placement.row_start,
          placement.row_end,
          &row_offsets,
          bounds,
        ),
      }
    }
    if wants_rows {
      match rows_axis {
        PhysicalAxis::Y => shrink_axis(
          PhysicalAxis::Y,
          placement.row_start,
          placement.row_end,
          &row_offsets,
          bounds,
        ),
        PhysicalAxis::X => shrink_axis(
          PhysicalAxis::X,
          placement.column_start,
          placement.column_end,
          &col_offsets,
          bounds,
        ),
      }
    }

    Ok(())
  }

  fn apply_subgrid_writing_mode_transpose(
    &self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    layout: &TaffyLayout,
    box_node: &BoxNode,
    fragment: &mut FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    // CSS Grid 2 says subgrid line numbering and placement follow the subgrid's own writing-mode
    // (#subgrid-indexing). Taffy currently models subgrid axes using the parent grid's writing-mode
    // so track inheritance works. When the subgrid establishes an orthogonal writing-mode, WPT
    // expects the subgrid's in-flow children to be placed against transposed axes.
    //
    // We keep Taffy's axis mapping (so inherited track sizing remains stable) and then transpose
    // the final fragments for non-grid children. Nested subgrids are intentionally excluded so they
    // continue to inherit their parent's track orientation.
    if fragment.children.is_empty()
      || !(box_node.style.grid_row_subgrid || box_node.style.grid_column_subgrid)
    {
      return Ok(());
    }

    let Some(parent_id) = taffy.parent(node_id) else {
      return Ok(());
    };
    let Some(&parent_ptr) = taffy.get_node_context(parent_id) else {
      return Ok(());
    };
    let parent_node = unsafe { &*parent_ptr };

    let this_inline_is_horizontal = GridAxisStyle::from_style(&box_node.style).inline_is_horizontal();
    let parent_inline_is_horizontal =
      GridAxisStyle::from_style(&parent_node.style).inline_is_horizontal();
    if this_inline_is_horizontal == parent_inline_is_horizontal {
      return Ok(());
    }

    let container_style = match taffy.style(node_id) {
      Ok(style) => style,
      Err(_) => return Ok(()),
    };
    let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) else {
      return Ok(());
    };
    let child_count = taffy.child_count(node_id);
    if info.items.len() != child_count {
      return Ok(());
    }

    // Track offsets in the subgrid's *current* Taffy coordinate space (parent writing-mode). Used
    // to detect whether children filled their original grid areas.
    let row_offsets_old = compute_track_offsets(
      &info.rows,
      layout.size.height,
      layout.padding.top,
      layout.padding.bottom,
      layout.border.top,
      layout.border.bottom,
      container_style
        .align_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );
    let col_offsets_old = compute_track_offsets(
      &info.columns,
      layout.size.width,
      layout.padding.left,
      layout.padding.right,
      layout.border.left,
      layout.border.right,
      container_style
        .justify_content
        .unwrap_or(TaffyAlignContent::Stretch),
    );

    // Build the column track list without gutters so that a containing grid's physical-X gaps do
    // not incorrectly transpose onto the physical-Y axis. This matches WPT
    // `css/subgrid/subgrid-writing-mode-001`.
    let mut columns_no_gutters = info.columns.clone();
    columns_no_gutters.gutters.clear();
    columns_no_gutters
      .gutters
      .resize(columns_no_gutters.sizes.len() + 1, 0.0);

    // When axes are orthogonal, we transpose the track offsets used for placement, but apply
    // mirroring and alignment based on the subgrid's *own* writing-mode/direction.
    let local_axis_style = GridAxisStyle::from_style(&box_node.style);
    let inline_positive = local_axis_style.inline_positive();
    let block_positive = local_axis_style.block_positive();
    let inline_is_horizontal = local_axis_style.inline_is_horizontal();

    // Alignment values in Taffy's coordinate system (start = min coordinate).
    let (align_x, align_y) = if inline_is_horizontal {
      // Inline axis = physical X; block axis = physical Y.
      (
        self.convert_justify_content(&box_node.style.justify_content, inline_positive),
        self.convert_align_content(&box_node.style.align_content, block_positive),
      )
    } else {
      // Inline axis = physical Y; block axis = physical X.
      (
        self.convert_align_content(&box_node.style.align_content, block_positive),
        self.convert_justify_content(&box_node.style.justify_content, inline_positive),
      )
    };

    // Track offsets for the transposed coordinate space.
    let row_offsets_transposed = compute_track_offsets(
      &info.rows,
      layout.size.width,
      layout.padding.left,
      layout.padding.right,
      layout.border.left,
      layout.border.right,
      align_x,
    );
    let col_offsets_transposed = compute_track_offsets(
      &columns_no_gutters,
      layout.size.height,
      layout.padding.top,
      layout.padding.bottom,
      layout.border.top,
      layout.border.bottom,
      align_y,
    );

    let mut mirror_x = false;
    let mut mirror_y = false;
    if !inline_positive {
      if inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !block_positive {
      if inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }

    let span_alignment = |tracks: &DetailedGridTracksInfo,
                          axis_size: f32,
                          padding_start: f32,
                          padding_end: f32,
                          border_start: f32,
                          border_end: f32,
                          alignment: TaffyAlignContent| {
      let track_count = tracks.sizes.len();
      if track_count == 0 {
        return alignment;
      }

      let mut gutters = tracks.gutters.clone();
      if gutters.len() < track_count + 1 {
        gutters.resize(track_count + 1, 0.0);
      }

      let used_gutters: f32 = gutters.iter().take(track_count + 1).copied().sum::<f32>();
      let used_size: f32 = tracks.sizes.iter().copied().sum::<f32>() + used_gutters;
      let content_size = axis_size - padding_start - padding_end - border_start - border_end;
      let free_space = content_size - used_size;
      apply_alignment_fallback_for_grid(free_space, track_count, alignment)
    };

    let span_for_axis = |offsets: &[f32],
                         track_count: usize,
                         axis_size: f32,
                         padding_end: f32,
                         border_end: f32,
                         alignment: TaffyAlignContent|
     -> Option<(f32, f32)> {
      if track_count == 0 || offsets.len() < 2 {
        return None;
      }
      let start = offsets.get(1).copied()?;
      let mut end = offsets
        .get(track_count.saturating_mul(2))
        .copied()
        .or_else(|| offsets.last().copied())
        .unwrap_or(start);
      if alignment == TaffyAlignContent::Stretch {
        let content_end = axis_size - padding_end - border_end;
        end = content_end.max(end);
      }
      Some((start, end))
    };

    let x_span = mirror_x.then(|| {
      let aligned = span_alignment(
        &info.rows,
        layout.size.width,
        layout.padding.left,
        layout.padding.right,
        layout.border.left,
        layout.border.right,
        align_x,
      );
      span_for_axis(
        &row_offsets_transposed,
        info.rows.sizes.len(),
        layout.size.width,
        layout.padding.right,
        layout.border.right,
        aligned,
      )
    })
    .flatten();
    let y_span = mirror_y.then(|| {
      let aligned = span_alignment(
        &columns_no_gutters,
        layout.size.height,
        layout.padding.top,
        layout.padding.bottom,
        layout.border.top,
        layout.border.bottom,
        align_y,
      );
      span_for_axis(
        &col_offsets_transposed,
        columns_no_gutters.sizes.len(),
        layout.size.height,
        layout.padding.bottom,
        layout.border.bottom,
        aligned,
      )
    })
    .flatten();

    let container_axes =
      FragmentAxes::from_writing_mode_and_direction(box_node.style.writing_mode, box_node.style.direction);
    let container_inline_axis = container_axes.inline_axis();
    let container_axis_positive = |axis: PhysicalAxis| {
      if axis == container_inline_axis {
        container_axes.inline_positive()
      } else {
        container_axes.block_positive()
      }
    };

    let children = fragment.children_mut();
    let child_len = children.len().min(child_count);
    for idx in 0..child_len {
      check_layout_deadline(deadline_counter)?;
      let child_id = taffy.get_child_id(node_id, idx);
      let Some(&child_ptr) = taffy.get_node_context(child_id) else {
        continue;
      };
      let child_node = unsafe { &*child_ptr };
      if matches!(
        child_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ) {
        continue;
      }

      let placement = &info.items[idx];
      let Some((mut x_start, mut x_end)) =
        grid_area_for_item(&row_offsets_transposed, placement.row_start, placement.row_end)
      else {
        continue;
      };
      let Some((mut y_start, mut y_end)) =
        grid_area_for_item(&col_offsets_transposed, placement.column_start, placement.column_end)
      else {
        continue;
      };

      // Mirror the grid area itself when the corresponding axis is reversed in the subgrid's
      // writing-mode/direction.
      if let Some((span_start, span_end)) = x_span {
        if span_end > span_start {
          let new_start = span_start + (span_end - x_end);
          let new_end = span_start + (span_end - x_start);
          x_start = new_start;
          x_end = new_end;
        }
      }
      if let Some((span_start, span_end)) = y_span {
        if span_end > span_start {
          let new_start = span_start + (span_end - y_end);
          let new_end = span_start + (span_end - y_start);
          y_start = new_start;
          y_end = new_end;
        }
      }

      let (x_start, x_end) = if x_start <= x_end {
        (x_start, x_end)
      } else {
        (x_end, x_start)
      };
      let (y_start, y_end) = if y_start <= y_end {
        (y_start, y_end)
      } else {
        (y_end, y_start)
      };
      let area_width = (x_end - x_start).max(0.0);
      let area_height = (y_end - y_start).max(0.0);

      // Determine whether the child filled its original grid area in Taffy's coordinate system so
      // we can transpose stretch sizing without re-running layout.
      let Some((old_x_start, old_x_end)) =
        grid_area_for_item(&col_offsets_old, placement.column_start, placement.column_end)
      else {
        continue;
      };
      let Some((old_y_start, old_y_end)) =
        grid_area_for_item(&row_offsets_old, placement.row_start, placement.row_end)
      else {
        continue;
      };
      let old_area_width = (old_x_end - old_x_start).max(0.0);
      let old_area_height = (old_y_end - old_y_start).max(0.0);

      let child_fragment = &mut children[idx];
      let old_bounds = child_fragment.bounds;
      let old_width = old_bounds.width().max(0.0);
      let old_height = old_bounds.height().max(0.0);

      let fills_old_width = (old_width - old_area_width).abs() < 0.1;
      let fills_old_height = (old_height - old_area_height).abs() < 0.1;

      let mut new_width = old_width;
      let mut new_height = old_height;
      // Old Y (row axis) becomes new X; old X (column axis) becomes new Y.
      if fills_old_height {
        new_width = area_width;
      }
      if fills_old_width {
        new_height = area_height;
      }

      // Resolve the effective alignment keyword for the given axis in the subgrid's coordinate
      // system.
      let child_axes = FragmentAxes::from_writing_mode_and_direction(
        child_node.style.writing_mode,
        child_node.style.direction,
      );
      let child_inline_axis = child_axes.inline_axis();
      let child_axis_positive = |axis: PhysicalAxis| {
        if axis == child_inline_axis {
          child_axes.inline_positive()
        } else {
          child_axes.block_positive()
        }
      };
      let convert_item_alignment = |align: AlignItems, axis: PhysicalAxis| {
        let container_positive = container_axis_positive(axis);
        let self_positive = child_axis_positive(axis);
        let axis_positive = match align {
          AlignItems::SelfStart | AlignItems::SelfEnd => self_positive,
          _ => container_positive,
        };
        self.convert_align_items(&align, axis_positive)
      };

      let mut align_x = if container_inline_axis == PhysicalAxis::X {
        child_node
          .style
          .justify_self
          .unwrap_or(box_node.style.justify_items)
      } else {
        child_node
          .style
          .align_self
          .unwrap_or(box_node.style.align_items)
      };
      if align_x == AlignItems::Stretch
        && (child_node.style.width.is_some() || child_node.style.width_keyword.is_some())
      {
        align_x = AlignItems::Start;
      }
      let mut align_y = if container_inline_axis == PhysicalAxis::Y {
        child_node
          .style
          .justify_self
          .unwrap_or(box_node.style.justify_items)
      } else {
        child_node
          .style
          .align_self
          .unwrap_or(box_node.style.align_items)
      };
      if align_y == AlignItems::Stretch
        && (child_node.style.height.is_some()
          || child_node.style.height_keyword.is_some()
          || child_node.style.aspect_ratio != AspectRatio::Auto)
      {
        align_y = AlignItems::Start;
      }

      let place_within_area =
        |start: f32, end: f32, size: f32, alignment: taffy::style::AlignItems| -> f32 {
          let (start, end) = if start <= end { (start, end) } else { (end, start) };
          let size = if size.is_finite() { size.max(0.0) } else { 0.0 };
          let span = (end - start).max(0.0);
          let upper = (end - size).max(start);
          match alignment {
            taffy::style::AlignItems::End | taffy::style::AlignItems::FlexEnd => upper,
            taffy::style::AlignItems::Center => {
              (start + (span - size) / 2.0).clamp(start, upper)
            }
            _ => start,
          }
        };

      let mapped_align_x = convert_item_alignment(align_x, PhysicalAxis::X);
      let mapped_align_y = convert_item_alignment(align_y, PhysicalAxis::Y);
      let new_x = place_within_area(x_start, x_end, new_width, mapped_align_x);
      let new_y = place_within_area(y_start, y_end, new_height, mapped_align_y);

      let delta = Point::new(new_x - old_bounds.x(), new_y - old_bounds.y());
      if delta.x != 0.0 || delta.y != 0.0 {
        child_fragment.translate_root_in_place(delta);
      }
      child_fragment.bounds.size = Size::new(new_width, new_height);
      if let Some(logical) = child_fragment.logical_override.as_mut() {
        logical.size = Size::new(new_width, new_height);
      }
    }

    Ok(())
  }

  fn taffy_layout_subtree_size(
    taffy: &TaffyTree<*const BoxNode>,
    root_id: TaffyNodeId,
    deadline_counter: &mut usize,
  ) -> Result<Size, LayoutError> {
    fn sanitize(val: f32) -> f32 {
      if val.is_finite() {
        val
      } else {
        0.0
      }
    }

    let mut min = Point::new(0.0, 0.0);
    let mut max = Point::new(0.0, 0.0);
    let mut stack: Vec<(TaffyNodeId, Point)> = vec![(root_id, Point::ZERO)];
    while let Some((node_id, offset)) = stack.pop() {
      check_layout_deadline(deadline_counter)?;

      let layout = taffy
        .layout(node_id)
        .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;
      let origin = Point::new(
        sanitize(layout.location.x) + offset.x,
        sanitize(layout.location.y) + offset.y,
      );
      let bounds = Rect::from_xywh(
        origin.x,
        origin.y,
        sanitize(layout.size.width).max(0.0),
        sanitize(layout.size.height).max(0.0),
      );
      min.x = min.x.min(bounds.x());
      min.y = min.y.min(bounds.y());
      max.x = max.x.max(bounds.max_x());
      max.y = max.y.max(bounds.max_y());

      let child_count = taffy.child_count(node_id);
      for idx in 0..child_count {
        let child_id = taffy.get_child_id(node_id, idx);
        stack.push((child_id, origin));
      }
    }

    Ok(Size::new(
      (max.x - min.x).max(0.0),
      (max.y - min.y).max(0.0),
    ))
  }

  /// Returns the tight bounds of all in-flow descendants, excluding the root node's own bounds.
  ///
  /// This is used as a fallback during Taffy measurement: some probe paths can force the measured
  /// fragment's border box to 0px even though it contains non-zero in-flow descendants. When that
  /// happens, using the descendant span produces a more accurate intrinsic contribution for track
  /// sizing (notably for nested grids).
  fn fragment_descendant_span(
    fragment: &FragmentNode,
    deadline_counter: &mut usize,
  ) -> Result<Option<Size>, LayoutError> {
    #[inline]
    fn is_out_of_flow_positioned(fragment: &FragmentNode) -> bool {
      fragment.style.as_deref().is_some_and(|style| {
        style.running_position.is_some()
          || matches!(
            style.position,
            crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
          )
      })
    }

    fn walk(
      node: &FragmentNode,
      offset: Point,
      min: &mut Point,
      max: &mut Point,
      found: &mut bool,
      deadline_counter: &mut usize,
    ) -> Result<(), LayoutError> {
      check_layout_deadline(deadline_counter)?;
      for child in node.children.iter() {
        if is_out_of_flow_positioned(child) {
          continue;
        }
        let origin = Point::new(child.bounds.x() + offset.x, child.bounds.y() + offset.y);
        let bounds = Rect::new(origin, child.bounds.size);
        *found = true;
        min.x = min.x.min(bounds.x());
        min.y = min.y.min(bounds.y());
        max.x = max.x.max(bounds.max_x());
        max.y = max.y.max(bounds.max_y());
        walk(child, origin, min, max, found, deadline_counter)?;
      }
      Ok(())
    }

    let mut min = Point::new(f32::INFINITY, f32::INFINITY);
    let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut found = false;
    walk(
      fragment,
      Point::ZERO,
      &mut min,
      &mut max,
      &mut found,
      deadline_counter,
    )?;
    Ok(found.then(|| Size::new((max.x - min.x).max(0.0), (max.y - min.y).max(0.0))))
  }
  /// Computes intrinsic size using Taffy
  fn compute_intrinsic_size(
    &self,
    box_node: &BoxNode,
    style: &ComputedStyle,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ),
      "GridFormattingContext must only query grid containers",
    );
    if let Some(cached) = intrinsic_cache_lookup(box_node, mode) {
      return Ok(cached);
    }
    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override.as_deref().unwrap_or(style);
    if style.containment.isolates_inline_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let edges = if inline_is_horizontal {
        self.horizontal_edges_px(style).unwrap_or(0.0)
      } else {
        self.vertical_edges_px(style).unwrap_or(0.0)
      };
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_width
      } else {
        style.contain_intrinsic_height
      };
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        None,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let size = (edges + fallback).max(0.0);
      intrinsic_cache_store(box_node, mode, size);
      return Ok(size);
    }

    let mut deadline_counter = 0usize;
    let mut in_flow_children: Vec<(usize, &BoxNode)> = box_node
      .children
      .iter()
      .enumerate()
      .filter(|(_, child)| {
        !matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      })
      .collect();
    let mut in_flow_children_need_sort = false;
    let mut last_in_flow_order: Option<i32> = None;
    for (_, child) in in_flow_children.iter() {
      check_layout_deadline(&mut deadline_counter)?;
      if let Some(prev) = last_in_flow_order {
        if child.style.order < prev {
          in_flow_children_need_sort = true;
          break;
        }
      }
      last_in_flow_order = Some(child.style.order);
    }
    if in_flow_children_need_sort {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      in_flow_children.sort_by(|(a_idx, a), (b_idx, b)| {
        a.style.order.cmp(&b.style.order).then_with(|| a_idx.cmp(b_idx))
      });
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
    }
    let in_flow_children: Vec<&BoxNode> = in_flow_children
      .into_iter()
      .map(|(_, child)| child)
      .collect();

    let child_has_subgrid = in_flow_children
      .iter()
      .any(|child| child.style.grid_row_subgrid || child.style.grid_column_subgrid);
    let cacheable_tree =
      !(style.grid_row_subgrid || style.grid_column_subgrid || child_has_subgrid);

    // Reuse a per-box cached Taffy tree so intrinsic sizing probes can populate Taffy's internal
    // caches and later layout passes can reuse them.
    let mut taffy = CachedTaffyTree::new(TaffyAdapterKind::Grid, box_node.id, cacheable_tree);
    let mut positioned_children: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> = FxHashMap::default();
    let intrinsic_constraints = LayoutConstraints::new(
      match mode {
        IntrinsicSizingMode::MinContent => CrateAvailableSpace::MinContent,
        IntrinsicSizingMode::MaxContent => CrateAvailableSpace::MaxContent,
      },
      CrateAvailableSpace::Indefinite,
    );

    // Resolve intrinsic sizing keywords for descendants while computing this grid container's
    // intrinsic inline sizes. Skip resolving the root itself to avoid recursion: resolving
    // `width: max-content` requires asking for the max-content size of the same box.
    let _intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      &intrinsic_constraints,
      false,
    )?;

    let root_id = self.build_or_update_taffy_tree_children_cached(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      &intrinsic_constraints,
      &mut positioned_children,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      let mut style_deadline_counter = 0usize;
      let has_positioned_children = box_node.children.iter().any(|child| {
        matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      });
      let simple_grid = !has_positioned_children
        && self.is_simple_grid(
          style_override,
          &in_flow_children,
          &mut style_deadline_counter,
        )?;
      let override_taffy_style = self.convert_style(
        style_override,
        None,
        None,
        None,
        simple_grid,
        true,
        box_node.is_replaced(),
      );
      let needs_override_update = match taffy.style(root_id) {
        Ok(existing) => existing != &override_taffy_style,
        Err(_) => true,
      };
      if needs_override_update {
        taffy
          .set_style(root_id, override_taffy_style)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }
    }

    self.patch_root_calc_percentage_sizing_and_edges(
      &mut taffy,
      root_id,
      style,
      &intrinsic_constraints,
    )?;
    self.patch_root_percentage_block_size(&mut taffy, root_id, style, &intrinsic_constraints)?;
    self.patch_root_calc_percentage_tracks(&mut taffy, root_id, style, &intrinsic_constraints)?;

    // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block height
    // is not definite for in-flow elements.
    //
    // The regular `layout()` path applies this normalization for the grid container root, but the
    // intrinsic sizing fast-path (`compute_intrinsic_size`) also needs it. Without it,
    // `height:100%` can incorrectly resolve to 0px during max-content measurement (e.g. WIRED's
    // sticky nav rows), collapsing `fr` tracks and clipping overflow-hidden descendants.
    if intrinsic_constraints.used_border_box_height.is_none()
      && intrinsic_constraints.height().is_none()
      && style
        .height
        .as_ref()
        .is_some_and(crate::style::values::Length::has_percentage)
      && !matches!(
        style.position,
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
      )
    {
      if let Ok(existing) = taffy.style(root_id) {
        if existing.size.height.tag() == taffy::style::CompactLength::PERCENT_TAG {
          let mut updated = existing.clone();
          updated.size.height = Dimension::auto();
          taffy
            .set_style(root_id, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // Use appropriate available space for intrinsic sizing
    let available_space = match mode {
      IntrinsicSizingMode::MinContent => taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MinContent,
        height: taffy::style::AvailableSpace::MinContent,
      },
      IntrinsicSizingMode::MaxContent => taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MaxContent,
        height: taffy::style::AvailableSpace::MaxContent,
      },
    };

    let allow_stretch_block_size_override =
      grid_container_allows_stretch_block_size_override(style, &intrinsic_constraints);

    record_taffy_invocation(TaffyAdapterKind::Grid);
    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
    let trace_measure_id =
      crate::debug::runtime::runtime_toggles().usize("FASTR_TRACE_GRID_MEASURE_ID");
    // Render pipeline always installs a deadline guard (even when disabled), so only enable
    // the Taffy cancellation path when the active deadline is actually configured.
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let compute_result = taffy.compute_layout_with_measure_and_cancel(
      root_id,
      available_space,
      {
        let factory = self.factory.clone();
        let viewport_size = self.viewport_size;
        let trace_measure_id = trace_measure_id;
        let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
        move |known_dimensions,
              available_space,
              node_id,
              node_context,
              taffy_style: &taffy::style::Style| {
          if taffy_perf_enabled {
            record_taffy_measure_call(TaffyAdapterKind::Grid);
          }
          if node_id == root_id {
            return taffy::tree::MeasureOutput::ZERO;
          }
          let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
            known.unwrap_or(match avail_dim {
              taffy::style::AvailableSpace::Definite(v) => v,
              _ => 0.0,
            })
          };
          let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
            return taffy::tree::MeasureOutput::ZERO;
          };
          let box_node = unsafe { &*node_ptr };
          if trace_measure_id.is_some_and(|id| id == box_node.id) {
            eprintln!(
              "[grid-measure] box_id={} taffy_node_id={:?} known={:?} avail={:?}",
              box_node.id, node_id, known_dimensions, available_space
            );
          }
          let style_override = style_override_for(box_node.id);
          let style: &ComputedStyle = style_override
            .as_deref()
            .unwrap_or_else(|| box_node.style.as_ref());
          let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
          let wants_baseline_x = taffy_style.justify_self
            == Some(taffy::style::AlignItems::Baseline)
            && crate::style::block_axis_is_horizontal(style.writing_mode);
          let mut available_space = available_space;
          if known_dimensions.width.is_none()
            && matches!(
              available_space.width,
              taffy::style::AvailableSpace::Definite(_)
            )
            && physical_width_is_auto(style)
          {
            available_space.width = taffy::style::AvailableSpace::MaxContent;
          }

          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let drop_available_height = drop_definite_available_height_for_measure(
            box_node,
            fc_type,
            known_dimensions.height,
            available_space.height,
            allow_stretch_block_size_override,
          );
          let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
            box_node,
            known_dimensions,
            available_space,
            viewport_size,
            drop_available_height,
          );
          if let Some(size) = cache.get(&key) {
            return *size;
          }
          if let Some(output) = grid_measure_size_cache_lookup(&key) {
            cache.insert(key, output);
            return output;
          }
          let fc: std::sync::Arc<dyn FormattingContext> =
            if matches!(fc_type, FormattingContextType::Block) {
              std::sync::Arc::new(
                BlockFormattingContext::for_independent_context_root_with_factory(factory.clone()),
              )
            } else {
              factory.get(fc_type)
            };
          let inline_is_horizontal =
            crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);
          let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_inline_size(box_node, mode)
            } else {
              fc.compute_intrinsic_block_size(box_node, mode)
            }
          };
          let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_block_size(box_node, mode)
            } else {
              fc.compute_intrinsic_inline_size(box_node, mode)
            }
          };

          // Taffy expresses intrinsic sizing probes via `AvailableSpace::{MinContent,MaxContent}`
          // on the physical axis being queried. Unlike the `FormattingContext` APIs, these probes
          // are not expressed in the box's logical axes, so we must always answer them in physical
          // width/height regardless of `writing-mode`.
          let mut intrinsic_width: Option<f32> = None;
          if known_dimensions.width.is_none() {
            intrinsic_width = match available_space.width {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          let mut intrinsic_height: Option<f32> = None;
          if known_dimensions.height.is_none() {
            intrinsic_height = match available_space.height {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          // CSS Grid auto minimum size (min-width/min-height:auto) clamps grid items so they do not
          // shrink below their content-based minimum size when overflow is visible. Taffy currently
          // passes stretched track sizes as `known_dimensions` to the measure callback and does not
          // apply this clamp itself. When we have a definite `known_dimensions` in an axis and the
          // CSS min-size is `auto`, compute the min-content size and treat it as a floor.
          //
          // This is particularly important for nested grid containers: their min-content size is
          // often defined by their explicit track sizes, and the WPT reference cases rely on the
          // background painting area including those tracks even when the parent track is smaller.
          let min_width_is_auto = style.min_width.is_none() && style.min_width_keyword.is_none();
          let min_height_is_auto = style.min_height.is_none() && style.min_height_keyword.is_none();

          let overflow_x_allows_auto_min = matches!(style.overflow_x, CssOverflow::Visible);
          let overflow_y_allows_auto_min = matches!(style.overflow_y, CssOverflow::Visible);

          let needs_auto_min_width = known_dimensions.width.is_some() && min_width_is_auto && overflow_x_allows_auto_min;
          let needs_auto_min_height =
            known_dimensions.height.is_some() && min_height_is_auto && overflow_y_allows_auto_min;

          let mut auto_min_border_width: Option<f32> = None;
          if needs_auto_min_width {
            auto_min_border_width = Some(match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
              Ok(size) => size,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => 0.0,
            });
          }
          let mut auto_min_border_height: Option<f32> = None;
          if needs_auto_min_height {
            auto_min_border_height =
              Some(match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                Ok(size) => size,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => 0.0,
              });
          }
          if trace_measure_id.is_some_and(|id| id == box_node.id) {
            eprintln!(
              "[grid-measure] box_id={} intrinsic_w={intrinsic_width:?} intrinsic_h={intrinsic_height:?}",
              box_node.id
            );
          }

          if !(wants_baseline_y || wants_baseline_x)
            && (intrinsic_width.is_some() || intrinsic_height.is_some())
          {
            let percentage_base = match available_space.width {
              taffy::style::AvailableSpace::Definite(w) => w,
              _ => 0.0,
            };
            let (inset_w, inset_h) = GridFormattingContext::taffy_measure_insets_px(
              taffy_style,
              percentage_base,
            );
            let mut width = intrinsic_width
              .map(|border_width| (border_width - inset_w).max(0.0))
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.width, available_space.width).max(0.0)
              });

            let mut height = if let Some(border_height) = intrinsic_height {
              (border_height - inset_h).max(0.0)
            } else {
              // When Taffy probes intrinsic inline sizes (min-/max-content width), it can still
              // provide a "known" or definite block size. That value is not the box's intrinsic
              // height; it's a sizing artifact of the grid algorithm and must not leak back into
              // track sizing / alignment.
              //
              // Always answer width probes with a content-based intrinsic block size. This also
              // keeps Taffy-internal measurement caches safe to reuse across subsequent layout
              // passes that swap in a different measure closure (e.g. grid containers that were
              // first queried for intrinsic sizes and later laid out normally).
              let mode = match available_space.width {
                taffy::style::AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
                taffy::style::AvailableSpace::MaxContent => IntrinsicSizingMode::MaxContent,
                _ => IntrinsicSizingMode::MaxContent,
              };
              let border_block_size = match intrinsic_physical_height(mode) {
                Ok(size) => size,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => 0.0,
              };
              (border_block_size - inset_h).max(0.0)
            };
            if let Some(border_width) = auto_min_border_width {
              width = width.max((border_width - inset_w).max(0.0));
            }
            if let Some(border_height) = auto_min_border_height {
              height = height.max((border_height - inset_h).max(0.0));
            }
            let size = taffy::geometry::Size { width, height };
            let output = taffy::tree::MeasureOutput::from_size(size);
            cache.insert(key, output);
            if trace_measure_id.is_some_and(|id| id == box_node.id) {
              eprintln!("[grid-measure] box_id={} fast_path size={size:?}", box_node.id);
            }
            return output;
          }
          let constraints = constraints_from_taffy(
            viewport_size,
            known_dimensions,
            available_space,
            style.writing_mode,
            None,
          );
          if trace_measure_id.is_some_and(|id| id == box_node.id) {
            eprintln!(
              "[grid-measure] box_id={} layout_path constraints={:?}",
              box_node.id, constraints
            );
          }
          let fragment = match fc.layout(box_node, &constraints) {
            Ok(fragment) => fragment,
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => return taffy::tree::MeasureOutput::ZERO,
          };
          let percentage_base = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) => w,
            _ => constraints
              .width()
              .unwrap_or_else(|| fragment.bounds.width()),
          };
          let content_size = GridFormattingContext::content_box_size_for_taffy_style(
            Size::new(fragment.bounds.width(), fragment.bounds.height()),
            taffy_style,
            percentage_base,
          );
          let (inset_w, inset_h) = GridFormattingContext::taffy_measure_insets_px(
            taffy_style,
            percentage_base,
          );
          let mut size = taffy::geometry::Size {
            width: content_size.width.max(0.0),
            height: content_size.height.max(0.0),
          };
          if let Some(border_width) = auto_min_border_width {
            size.width = size.width.max((border_width - inset_w).max(0.0));
          }
          if let Some(border_height) = auto_min_border_height {
            size.height = size.height.max((border_height - inset_h).max(0.0));
          }
          let mut baseline_deadline_counter = 0usize;
          let baseline_y = if wants_baseline_y {
            match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let baseline_x = if wants_baseline_x {
            match first_baseline_offset_x(
              &fragment,
              style.writing_mode,
              &mut baseline_deadline_counter,
            ) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let output = taffy::tree::MeasureOutput::from_size_and_baselines(
            size,
            taffy::geometry::Point {
              x: baseline_x,
              y: baseline_y,
            },
          );
          grid_measure_size_cache_store(key, output);
          cache.insert(key, output);
          output
        }
      },
      cancel,
      TAFFY_ABORT_CHECK_STRIDE,
    );
    if let Some(start) = taffy_compute_start {
      record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
    }
    compute_result.map_err(|e| match e {
      taffy::TaffyError::LayoutAborted => match active_deadline() {
        Some(deadline) => LayoutError::Timeout {
          elapsed: deadline.elapsed(),
        },
        None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
      },
      _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
    })?;

    let layout = taffy
      .layout(root_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let mut result = if inline_is_horizontal {
      layout.size.width
    } else {
      layout.size.height
    }
    .max(0.0);

    let eps = 0.01;
    if result <= eps || !result.is_finite() {
      let mut deadline_counter = 0usize;
      if let Ok(size) = Self::taffy_layout_subtree_size(&taffy, root_id, &mut deadline_counter) {
        let subtree = if inline_is_horizontal {
          size.width
        } else {
          size.height
        };
        if subtree.is_finite() && subtree > result {
          result = subtree;
        }
      }
    }

    intrinsic_cache_store(box_node, mode, result);
    Ok(result)
  }

  #[allow(clippy::too_many_arguments)]
  fn measure_grid_item(
    &self,
    node_ptr: *const BoxNode,
    node_id: TaffyNodeId,
    known_dimensions: taffy::geometry::Size<Option<f32>>,
    available_space: taffy::geometry::Size<taffy::style::AvailableSpace>,
    parent_inline_base: Option<f32>,
    container_style: &ComputedStyle,
    container_constraints: &LayoutConstraints,
    taffy_style: &taffy::style::Style,
    auto_unskipped: &FxHashSet<*const BoxNode>,
    factory: &crate::layout::contexts::factory::FormattingContextFactory,
    measure_cache: &mut FxHashMap<MeasureKey, taffy::tree::MeasureOutput>,
    measured_fragments: &mut FxHashMap<MeasureKey, FragmentNode>,
    measured_node_keys: &mut FxHashMap<TaffyNodeId, Vec<MeasureKey>>,
  ) -> taffy::tree::MeasureOutput {
    let box_node = unsafe { &*node_ptr };
    let mut style_override = style_override_for(box_node.id);
    let mut style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    let trace_measure_id =
      crate::debug::runtime::runtime_toggles().usize("FASTR_TRACE_GRID_MEASURE_ID");

    let allow_stretch_block_size_override =
      grid_item_allows_stretch_block_size_override(container_style, container_constraints, style);

    // Taffy sometimes reports the grid area's block size as a "known" size for stretched items.
    // For content-sized grid tracks we must *not* treat that stretched size as a definite basis for
    // nested percentage heights; doing so can create cyclic sizing feedback (`height:100%` expands
    // to the probe size, inflating the track, which then inflates the probe again).
    //
    // Instead, keep the block size "unknown" so the item's intrinsic height is measured from
    // content, matching CSS Grid's percentage resolution rules for indefinite track sizes.
    let mut known_dimensions = known_dimensions;
    if !allow_stretch_block_size_override
      && physical_height_is_auto(style)
      && taffy_style.align_self == Some(taffy::style::AlignItems::Stretch)
    {
      known_dimensions.height = None;
    }

    // Some real-world grid items (notably howtogeek.com's featured hero card) specify
    // `height: fit-content`. Our general intrinsic keyword resolver computes the min/max intrinsic
    // block sizes via `compute_intrinsic_block_size`, which varies the inline constraint between
    // min/max-content widths. When the grid item's inline size is already definite (i.e. the grid
    // area width is known), that variation can massively overestimate the height and then
    // destabilize track sizing (and waste a lot of time re-laying out large subtrees).
    //
    // For `height: fit-content` *with a definite inline size*, treat it like `auto` during grid
    // measurement so the item is laid out exactly once at that width and contributes its true
    // content height.
    let mut _fit_content_height_override: Option<StyleOverrideGuard> = None;
    if box_node.id != 0
      && known_dimensions.height.is_none()
      && matches!(
        available_space.width,
        taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
      )
      && crate::style::inline_axis_is_horizontal(style.writing_mode)
      && style
        .height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { limit: None }))
    {
      let mut cleared: ComputedStyle = style.clone();
      cleared.height = None;
      cleared.height_keyword = None;
      _fit_content_height_override = Some(push_style_override(box_node.id, Arc::new(cleared)));
      style_override = style_override_for(box_node.id);
      style = style_override
        .as_deref()
        .unwrap_or_else(|| box_node.style.as_ref());
    }

    let skip_contents = match style.content_visibility {
      crate::style::types::ContentVisibility::Hidden => true,
      crate::style::types::ContentVisibility::Auto => {
        self.content_visibility_auto_has_definite_placeholder(box_node)
          && !auto_unskipped.contains(&node_ptr)
      }
      crate::style::types::ContentVisibility::Visible => false,
    };
    let fc_type = box_node
      .formatting_context()
      .unwrap_or(FormattingContextType::Block);
    let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
    let wants_baseline_x = taffy_style.justify_self == Some(taffy::style::AlignItems::Baseline)
      && crate::style::block_axis_is_horizontal(style.writing_mode);
    let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
      known.unwrap_or(match avail_dim {
        taffy::style::AvailableSpace::Definite(v) => v,
        _ => 0.0,
      })
    };

    let mut drop_available_height = drop_definite_available_height_for_measure(
      box_node,
      fc_type,
      known_dimensions.height,
      available_space.height,
      allow_stretch_block_size_override,
    );
    // When the grid item's block-axis tracks are content-sized (`auto`, `min-content`, etc.), the
    // grid area block size is *not* definite for percentage resolution (CSS2.1 §10.5 / CSS Grid
    // §6.5). Taffy still probes leaf nodes with the current track size estimate, which can be much
    // smaller than the item's max-content size.
    //
    let treat_definite_height_probe_as_indefinite_for_layout = !allow_stretch_block_size_override
      && known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::Definite(h) if h.is_finite() && h > 1.0
      )
      && matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Inline
          | FormattingContextType::Table
          | FormattingContextType::Flex
      )
      && node_or_in_flow_children_depend_on_available_height(box_node);
    if treat_definite_height_probe_as_indefinite_for_layout {
      // When we intentionally treat a definite probe height as effectively indefinite for nested
      // layout, reflect that in the cache key by dropping the available height entirely. This keeps
      // key cardinality bounded on pages where Taffy iterates through many intermediate track-size
      // guesses.
      drop_available_height = true;
    }
    // Taffy expresses intrinsic height probes (min-/max-content contributions) by setting
    // `available_space.height` to `MinContent`/`MaxContent`.
    //
    // When the grid area width is already definite, browsers compute the item's contribution by
    // laying it out at that width with an effectively indefinite height. Doing that here avoids
    // extreme overestimation from formatting-context intrinsic block sizes (which are defined in
    // terms of the box's own intrinsic inline sizes).
    //
    // Treat these probes as having an effectively indefinite available height so the measured
    // result can be reused for subsequent definite-height calls when heights are otherwise ignored
    // (see `drop_definite_available_height_for_measure`).
    let drop_intrinsic_height_probe = known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::MinContent | taffy::style::AvailableSpace::MaxContent
      )
      && (known_dimensions.width.is_some()
        || matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
        ))
      && matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Inline
          | FormattingContextType::Table
          | FormattingContextType::Flex
      )
      && physical_height_is_auto(style)
      && !node_or_in_flow_children_depend_on_available_height(box_node);
    if drop_intrinsic_height_probe {
      drop_available_height = true;
    }
    let drop_intrinsic_height_probe_for_fit_content = !allow_stretch_block_size_override
      && known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::MinContent | taffy::style::AvailableSpace::MaxContent
      )
      && (known_dimensions.width.is_some()
        || matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
        ))
      && matches!(
        fc_type,
        FormattingContextType::Block
          | FormattingContextType::Inline
          | FormattingContextType::Table
          | FormattingContextType::Flex
      )
      && style
        .height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }));
    if drop_intrinsic_height_probe_for_fit_content {
      drop_available_height = true;
    }
    if !drop_available_height
      && !allow_stretch_block_size_override
      && known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::Definite(h) if h > 1.0
      )
      && (style
        .height_keyword
        .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
        || style
          .min_height_keyword
          .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. }))
        || style
          .max_height_keyword
          .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { .. })))
    {
      // Taffy can pass arbitrary definite heights while it is still sizing content-based grid
      // tracks. `fit-content` sizing keywords treat the available size as a clamp *only* when that
      // size is actually definite in CSS; on implicit/auto rows the available block size is
      // indefinite, so `fit-content` should behave like `max-content`. Drop these definite probes so
      // `fit-content` grid items contribute their intrinsic height instead of clamping to an
      // intermediate guess (si.edu: two-column intro sections collapsing and pulling subsequent
      // content upward).
      drop_available_height = true;
    }
    let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
      box_node,
      known_dimensions,
      available_space,
      self.viewport_size,
      drop_available_height,
    );
    if trace_measure_id.is_some_and(|id| id == box_node.id) {
      eprintln!(
        "[grid-measure-item] box_id={} node_id={:?} known={:?} avail={:?} fc={:?} height={:?} height_kw={:?} allow_stretch={} treat_height_probe_indef={} drop_avail_height={}",
        box_node.id,
        node_id,
        known_dimensions,
        available_space,
        fc_type,
        style.height,
        style.height_keyword,
        allow_stretch_block_size_override,
        treat_definite_height_probe_as_indefinite_for_layout,
        drop_available_height
      );
    }
    let has_calc_percentage_edges = Self::length_has_calc_percentage(&style.padding_left)
      || Self::length_has_calc_percentage(&style.padding_right)
      || Self::length_has_calc_percentage(&style.padding_top)
      || Self::length_has_calc_percentage(&style.padding_bottom)
      || Self::length_has_calc_percentage(&style.used_border_left_width())
      || Self::length_has_calc_percentage(&style.used_border_right_width())
      || Self::length_has_calc_percentage(&style.used_border_top_width())
      || Self::length_has_calc_percentage(&style.used_border_bottom_width());
    let should_adjust_for_calc_percentage_edges = has_calc_percentage_edges
      && matches!(
        available_space.width,
        taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
      );

    if skip_contents {
      let constraints = constraints_from_taffy(
        self.viewport_size,
        known_dimensions,
        available_space,
        style.writing_mode,
        parent_inline_base,
      );
      let placeholder =
        self.content_visibility_placeholder_content_size(box_node, &constraints, known_dimensions);
      let size = taffy::geometry::Size {
        width: placeholder.width.max(0.0),
        height: placeholder.height.max(0.0),
      };
      let output = taffy::tree::MeasureOutput::from_size(size);
      measure_cache.insert(key, output);
      return output;
    }

    if let Some(output) = measure_cache.get(&key) {
      return *output;
    }
    if let Some(output) = grid_measure_size_cache_lookup(&key) {
      measure_cache.insert(key, output);
      return output;
    }
    let mut constraints = constraints_from_taffy(
      self.viewport_size,
      known_dimensions,
      available_space,
      style.writing_mode,
      parent_inline_base,
    );
    let trace_measure = crate::debug::runtime::runtime_toggles().truthy("FASTR_TRACE_GRID_MEASURE");
    if trace_measure {
      eprintln!(
        "[grid-measure] start node_id={:?} known={:?} avail={:?} justify_self={:?} align_self={:?}",
        node_id,
        known_dimensions,
        available_space,
        taffy_style.justify_self,
        taffy_style.align_self
      );
    }
    if constraints.used_border_box_width.is_none()
      && known_dimensions.width.is_none()
      && physical_width_is_auto(style)
      && taffy_style.justify_self == Some(taffy::style::AlignItems::Stretch)
    {
      if let taffy::style::AvailableSpace::Definite(w) = available_space.width {
        if w.is_finite() && w > 1.0 {
          // `justify-self: stretch` makes grid items fill their grid area, but the stretched size
          // is still constrained by `min/max-width` (CSS Grid §11.5, CSS Sizing §10).
          //
          // When `min/max-width` use percentages, the basis is the grid *area* width, not the grid
          // container. Taffy currently resolves percentage min-sizes against the container, so we
          // treat them as `auto` in `convert_style` and apply the constraint here against the
          // definite grid-area width that Taffy provides in `available_space`.
          let base = w.max(0.0);
          let horizontal_edges = self.axis_padding_border_px(style, Axis::Horizontal, base);
          let resolve_border_box = |len: Length| {
            let specified = self.resolve_length_for_width(len, base, style).max(0.0);
            border_size_from_box_sizing(specified, horizontal_edges, style.box_sizing).max(0.0)
          };
          let min_border = style.min_width.map(resolve_border_box).unwrap_or(0.0);
          let max_border = style
            .max_width
            .map(resolve_border_box)
            .unwrap_or(f32::INFINITY);
          let forced = clamp_with_order(base, min_border, max_border);
          constraints.used_border_box_width = Some(forced.max(0.0));
        }
      }
    }
    if constraints.used_border_box_height.is_none()
      && known_dimensions.height.is_none()
      && physical_height_is_auto(style)
      && taffy_style.align_self == Some(taffy::style::AlignItems::Stretch)
      && allow_stretch_block_size_override
    {
      if let taffy::style::AvailableSpace::Definite(h) = available_space.height {
        if h.is_finite() && h > 1.0 {
          // Mirror the `min/max-height` handling above for the block axis. Note that percentage
          // padding/border still resolve against the grid area's *width* (CSS2.1 §10.5), so use the
          // available width as the padding percentage base when converting between content-box and
          // border-box sizes.
          let stretched = h.max(0.0);
          let width_base = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 0.0 => w,
            _ => 0.0,
          };
          let vertical_edges = self.axis_padding_border_px(style, Axis::Vertical, width_base);
          let resolve_border_box = |len: Length| {
            let specified = self
              .resolve_length_px_with_base(len, Some(stretched), style)
              .unwrap_or(0.0)
              .max(0.0);
            border_size_from_box_sizing(specified, vertical_edges, style.box_sizing).max(0.0)
          };
          let min_border = style.min_height.map(resolve_border_box).unwrap_or(0.0);
          let max_border = style
            .max_height
            .map(resolve_border_box)
            .unwrap_or(f32::INFINITY);
          let forced = clamp_with_order(stretched, min_border, max_border);
          constraints.used_border_box_height = Some(forced.max(0.0));
        }
      }
    }

    // Taffy frequently probes min/max-content sizes during track sizing.
    // Avoid running full layout for these probes; use intrinsic sizing APIs instead.
    #[cfg(test)]
    if let Some(mut fragment) =
      GRID_TEST_MEASURE_HOOK.with(|hook| hook.borrow().as_ref().and_then(|hook| hook(box_node)))
    {
      let mut normalize_deadline_counter = 0usize;
      fragment = match normalize_fragment_origin(fragment, &mut normalize_deadline_counter) {
        Ok(fragment) => fragment,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => return taffy::tree::MeasureOutput::ZERO,
      };
      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => constraints
          .width()
          .unwrap_or_else(|| fragment.bounds.width()),
      };
      fragment.content = match &box_node.box_type {
        crate::tree::box_tree::BoxType::Replaced(replaced_box) => FragmentContent::Replaced {
          replaced_type: replaced_box.replaced_type.clone(),
          box_id: Some(box_node.id),
        },
        _ => FragmentContent::Block {
          box_id: Some(box_node.id),
        },
      };
      attach_fragment_style_for_box(&mut fragment, box_node);
      let mut content_size = Self::content_box_size_for_taffy_style(
        Size::new(fragment.bounds.width(), fragment.bounds.height()),
        taffy_style,
        percentage_base,
      );
      let eps = 0.01;
      if content_size.width <= eps || content_size.height <= eps {
        let mut deadline_counter = 0usize;
        let span = match Self::fragment_descendant_span(&fragment, &mut deadline_counter) {
          Ok(span) => span,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        };
        if let Some(span) = span {
          if content_size.width <= eps && span.width > eps {
            content_size.width = span.width;
          }
          if content_size.height <= eps && span.height > eps {
            content_size.height = span.height;
          }
        }
      }
      let size = taffy::geometry::Size {
        width: content_size.width.max(0.0),
        height: content_size.height.max(0.0),
      };
      let mut baseline_deadline_counter = 0usize;
      let baseline_y = if wants_baseline_y {
        match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let baseline_x = if wants_baseline_x {
        match first_baseline_offset_x(
          &fragment,
          style.writing_mode,
          &mut baseline_deadline_counter,
        ) {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let output = taffy::tree::MeasureOutput::from_size_and_baselines(
        size,
        taffy::geometry::Point {
          x: baseline_x,
          y: baseline_y,
        },
      );
      grid_measure_size_cache_store(key, output);
      if let Some(evicted) = push_measured_key(measured_node_keys.entry(node_id).or_default(), key)
      {
        measured_fragments.remove(&evicted);
      }
      measured_fragments.insert(key, fragment);
      measure_cache.insert(key, output);
      return output;
    }

    let fc: std::sync::Arc<dyn FormattingContext> =
      if matches!(fc_type, FormattingContextType::Block) {
        std::sync::Arc::new(
          BlockFormattingContext::for_independent_context_root_with_factory(factory.clone()),
        )
      } else {
        factory.get(fc_type)
      };

    let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
        fc.as_ref(),
        box_node,
        PhysicalAxis::X,
        mode,
      )
    };
    let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
      crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_size(
        fc.as_ref(),
        box_node,
        PhysicalAxis::Y,
        mode,
      )
    };

    // CSS Grid 1 §11.5 (Grid Item Sizing): when a grid item's preferred size is `auto` and
    // `justify-self` is not `stretch`, the item is sized as `fit-content` in that axis.
    //
    // Our block formatting context follows CSS 2.1 and treats `width:auto` as "fill available
    // inline size", which is correct for normal-flow blocks but wrong for non-stretch grid items.
    // Precompute the fit-content border-box width and pass it down as a definite used size so the
    // item's formatting context (including nested grid/flex containers) lays out at the correct
    // shrink-to-fit size.
    if physical_width_is_auto(style)
      && taffy_style.justify_self != Some(taffy::style::AlignItems::Stretch)
    {
      if let taffy::style::AvailableSpace::Definite(avail_content) = available_space.width {
        if avail_content.is_finite() && avail_content > 1.0 {
          let percentage_base = avail_content.max(0.0);
          let (
            padding_left,
            padding_right,
            _padding_top,
            _padding_bottom,
            border_left,
            border_right,
            _border_top,
            _border_bottom,
          ) = self.resolved_padding_border_for_measure(style, percentage_base);
          let axis_inset = (padding_left + padding_right + border_left + border_right).max(0.0);
          let available_border_box = (percentage_base + axis_inset).max(0.0);

          let (min_intrinsic, max_intrinsic) =
            match crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
              fc.as_ref(),
              box_node,
              PhysicalAxis::X,
            ) {
              Ok(values) => values,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => (0.0, 0.0),
            };
          let min_intrinsic = min_intrinsic.max(0.0);
          let max_intrinsic = max_intrinsic.max(0.0);

          let mut border_box =
            crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
              Some(available_border_box),
              None,
              min_intrinsic,
              max_intrinsic,
            )
            .max(0.0);

          // Clamp the fit-content result by authored min/max constraints (including intrinsic
          // keyword constraints) so nested layout receives the same used size that Taffy expects.
          let keyword_to_bound = |kw: IntrinsicSizeKeyword| -> Option<f32> {
            match kw {
              IntrinsicSizeKeyword::MinContent => Some(min_intrinsic),
              IntrinsicSizeKeyword::MaxContent => Some(max_intrinsic),
              IntrinsicSizeKeyword::FillAvailable => None,
              IntrinsicSizeKeyword::FitContent { .. } => None,
              IntrinsicSizeKeyword::CalcSize(_) => None,
            }
          };
          let resolve_length_px = |len: Length| -> Option<f32> {
            if len.has_percentage() && !percentage_base.is_finite() {
              return None;
            }
            Some(
              self
                .resolve_length_for_width(len, percentage_base, style)
                .max(0.0),
            )
          };
          let to_border_box = |value: f32| -> f32 {
            if style.box_sizing == BoxSizing::ContentBox {
              (value + axis_inset).max(0.0)
            } else {
              value.max(0.0)
            }
          };

          let author_min = style
            .min_width_keyword
            .and_then(keyword_to_bound)
            .or_else(|| {
              style
                .min_width
                .and_then(resolve_length_px)
                .map(to_border_box)
            });
          let author_max = style
            .max_width_keyword
            .and_then(keyword_to_bound)
            .or_else(|| {
              style
                .max_width
                .and_then(resolve_length_px)
                .map(to_border_box)
            });
          if author_min.is_some() || author_max.is_some() {
            let min_bound = author_min.unwrap_or(0.0);
            let mut max_bound = author_max.unwrap_or(f32::INFINITY);
            if max_bound < min_bound {
              max_bound = min_bound;
            }
            border_box = crate::layout::utils::clamp_with_order(border_box, min_bound, max_bound);
          }

          if border_box.is_finite() {
            constraints.used_border_box_width = Some(border_box);
          }
        }
      }
    }

    // Fit-content depends on the available space passed by Taffy. Resolve it here (per measure call)
    // instead of during style conversion so cached templates remain valid.
    let fit_width_limit = match (known_dimensions.width, style.width_keyword) {
      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_height_limit = match (known_dimensions.height, style.height_keyword) {
      (None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_max_width_limit = match (
      known_dimensions.width,
      style.max_width,
      style.max_width_keyword,
    ) {
      (None, None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let fit_max_height_limit = match (
      known_dimensions.height,
      style.max_height,
      style.max_height_keyword,
    ) {
      (None, None, Some(IntrinsicSizeKeyword::FitContent { limit })) => Some(limit),
      _ => None,
    };
    let resolve_fit_max_width = fit_max_width_limit.is_some() && !physical_width_is_auto(style);
    let resolve_fit_max_height = fit_max_height_limit.is_some() && !physical_height_is_auto(style);
    let mut fit_border_box_width: Option<f32> = None;
    let mut fit_border_box_height: Option<f32> = None;

    if fit_width_limit.is_some()
      || fit_height_limit.is_some()
      || resolve_fit_max_width
      || resolve_fit_max_height
    {
      fn run_with_override<T, F>(
        box_node: &BoxNode,
        override_style: Option<Arc<ComputedStyle>>,
        f: F,
      ) -> Result<T, LayoutError>
      where
        F: FnOnce(&BoxNode) -> Result<T, LayoutError>,
      {
        if let Some(style_override) = override_style {
          if box_node.id() != 0 {
            crate::layout::style_override::with_style_override(
              box_node.id(),
              style_override,
              || f(box_node),
            )
          } else {
            let mut cloned = box_node.clone();
            cloned.style = style_override;
            f(&cloned)
          }
        } else {
          f(box_node)
        }
      }

      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => parent_inline_base.unwrap_or(0.0),
      };
      let (
        padding_left,
        padding_right,
        padding_top,
        padding_bottom,
        border_left,
        border_right,
        border_top,
        border_bottom,
      ) = self.resolved_padding_border_for_measure(style, percentage_base);
      let fit_inset_w = padding_left + padding_right + border_left + border_right;
      let fit_inset_h = padding_top + padding_bottom + border_top + border_bottom;

      // Avoid self-recursion when computing intrinsic sizes for a fit-content axis by clearing the
      // corresponding preferred size property before calling into intrinsic APIs.
      let fit_width_override = (fit_width_limit.is_some() || resolve_fit_max_width).then(|| {
        let mut override_style: ComputedStyle = style.clone();
        override_style.width = None;
        override_style.width_keyword = None;
        if resolve_fit_max_width {
          override_style.max_width = None;
          override_style.max_width_keyword = None;
        }
        Arc::new(override_style)
      });
      let fit_height_override = (fit_height_limit.is_some() || resolve_fit_max_height).then(|| {
        let mut override_style: ComputedStyle = style.clone();
        override_style.height = None;
        override_style.height_keyword = None;
        if resolve_fit_max_height {
          override_style.max_height = None;
          override_style.max_height_keyword = None;
        }
        Arc::new(override_style)
      });

      let override_for_axis = |axis: Axis| match axis {
        Axis::Horizontal => fit_width_override.clone(),
        Axis::Vertical => fit_height_override.clone(),
      };

      let intrinsic_range_for_physical_axis = |axis: Axis| -> Result<(f32, f32), LayoutError> {
        let physical_axis = match axis {
          Axis::Horizontal => PhysicalAxis::X,
          Axis::Vertical => PhysicalAxis::Y,
        };
        run_with_override(box_node, override_for_axis(axis), |node| {
          crate::layout::intrinsic_sizing_keywords::physical_axis_intrinsic_border_box_sizes(
            fc.as_ref(),
            node,
            physical_axis,
          )
        })
      };

      let compute_fit_border_box = |axis: Axis,
                                    limit: Option<Length>,
                                    avail_dim: taffy::style::AvailableSpace|
       -> Result<f32, LayoutError> {
        // Intrinsic block sizes depend on the element's inline constraint. When the inline size is
        // already definite (e.g. a grid item's resolved column width), the content-driven block
        // size should be measured at that inline size rather than varying between min/max-content
        // widths. Doing so avoids exaggerated `height: fit-content` resolutions on real pages
        // (howtogeek.com hero grid).
        let (min_intrinsic, max_intrinsic) = if matches!(axis, Axis::Vertical)
          && crate::style::inline_axis_is_horizontal(style.writing_mode)
          && matches!(
            available_space.width,
            taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
          ) {
          let content_w = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) => w.max(0.0),
            _ => 0.0,
          };
          let border_box_w = (content_w + fit_inset_w).max(0.0);
          let mut probe_constraints = LayoutConstraints::new(
            CrateAvailableSpace::Definite(border_box_w),
            CrateAvailableSpace::Indefinite,
          );
          probe_constraints.used_border_box_width = Some(border_box_w);
          probe_constraints.inline_percentage_base = Some(content_w);
          let fragment = run_with_override(box_node, override_for_axis(axis), |node| {
            fc.layout(node, &probe_constraints)
          })?;
          let h = fragment.bounds.height().max(0.0);
          (h, h)
        } else {
          intrinsic_range_for_physical_axis(axis)?
        };
        let min_intrinsic = min_intrinsic.max(0.0);
        let max_intrinsic = max_intrinsic.max(0.0);
        let axis_inset = match axis {
          Axis::Horizontal => fit_inset_w,
          Axis::Vertical => fit_inset_h,
        };

        // In our Taffy integration, `AvailableSpace::Definite` sizes `<= 1px` represent "unknown"
        // (see `constraints_from_taffy`). When sizing `fit-content`, treat those probes as
        // effectively indefinite so we don't clamp to a transient 0/1px track estimate.
        let definite_space = match avail_dim {
          taffy::style::AvailableSpace::Definite(v) if v.is_finite() && v > 1.0 => Some(v.max(0.0)),
          _ => None,
        };

        let available_border_box = match avail_dim {
          taffy::style::AvailableSpace::Definite(_v) => {
            definite_space.map(|v| (v + axis_inset).max(0.0))
          }
          taffy::style::AvailableSpace::MinContent => Some(min_intrinsic),
          taffy::style::AvailableSpace::MaxContent => Some(max_intrinsic),
        };

        let preferred_border_box = match limit {
          None => None,
          Some(arg) => {
            let base_content = definite_space.unwrap_or_else(|| {
              (available_border_box.unwrap_or(max_intrinsic) - axis_inset).max(0.0)
            });
            let resolved = self
              .resolve_length_for_width(arg, base_content, style)
              .max(0.0);
            Some(if style.box_sizing == BoxSizing::ContentBox {
              (resolved + axis_inset).max(0.0)
            } else {
              resolved
            })
          }
        };
        let mut border_box =
          crate::layout::intrinsic_sizing_keywords::resolve_fit_content_border_box(
            available_border_box,
            preferred_border_box,
            min_intrinsic,
            max_intrinsic,
          );

        // Apply authored min/max constraints on the axis, including intrinsic keyword constraints.
        // These clamp the fit-content result so the subtree is laid out at the same size Taffy will
        // ultimately use.
        let (author_min_len, author_max_len, author_min_kw, author_max_kw) = match axis {
          Axis::Horizontal => (
            style.min_width,
            style.max_width,
            style.min_width_keyword,
            style.max_width_keyword,
          ),
          Axis::Vertical => (
            style.min_height,
            style.max_height,
            style.min_height_keyword,
            style.max_height_keyword,
          ),
        };
        let keyword_to_bound = |kw: IntrinsicSizeKeyword| -> Option<f32> {
          match kw {
            IntrinsicSizeKeyword::MinContent => Some(min_intrinsic),
            IntrinsicSizeKeyword::MaxContent => Some(max_intrinsic),
            IntrinsicSizeKeyword::FillAvailable => None,
            IntrinsicSizeKeyword::FitContent { .. } => None,
            IntrinsicSizeKeyword::CalcSize(_) => None,
          }
        };
        let percentage_base_opt = definite_space;
        let resolve_length_px = |len: Length| -> Option<f32> {
          if len.has_percentage() && percentage_base_opt.is_none() {
            return None;
          }
          let base = percentage_base_opt.unwrap_or(self.viewport_size.width.max(0.0));
          Some(self.resolve_length_for_width(len, base, style).max(0.0))
        };
        let to_border_box = |value: f32| -> f32 {
          if style.box_sizing == BoxSizing::ContentBox {
            (value + axis_inset).max(0.0)
          } else {
            value.max(0.0)
          }
        };

        let author_min = author_min_kw.and_then(keyword_to_bound).or_else(|| {
          author_min_len
            .and_then(resolve_length_px)
            .map(to_border_box)
        });
        let author_max = author_max_kw.and_then(keyword_to_bound).or_else(|| {
          author_max_len
            .and_then(resolve_length_px)
            .map(to_border_box)
        });
        if author_min.is_some() || author_max.is_some() {
          let min_bound = author_min.unwrap_or(0.0);
          let mut max_bound = author_max.unwrap_or(f32::INFINITY);
          if max_bound < min_bound {
            max_bound = min_bound;
          }
          border_box = crate::layout::utils::clamp_with_order(border_box, min_bound, max_bound);
        }

        Ok(border_box)
      };

      if let Some(limit) = fit_width_limit {
        match compute_fit_border_box(Axis::Horizontal, limit, available_space.width) {
          Ok(border_box) if border_box.is_finite() => {
            let border_box = border_box.max(0.0);
            fit_border_box_width = Some(border_box);
            constraints.used_border_box_width = Some(border_box);
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => {}
          _ => {}
        }
      }

      if let Some(limit) = fit_height_limit {
        let can_use_available_height = matches!(
          available_space.height,
          taffy::style::AvailableSpace::Definite(h) if h.is_finite() && h > 1.0
        );
        if limit.is_none() && !can_use_available_height {
          // `height: fit-content` without an explicit limit falls back to max-content when the
          // available block size is indefinite. In that case, the keyword behaves like `auto` for
          // typical grid items: let nested layout determine its natural height instead of forcing a
          // precomputed border-box size based on intrinsic probes (which can ignore the actual
          // column width and collapse content).
        } else {
          match compute_fit_border_box(Axis::Vertical, limit, available_space.height) {
            Ok(border_box) if border_box.is_finite() => {
              let border_box = border_box.max(0.0);
              fit_border_box_height = Some(border_box);
              constraints.used_border_box_height = Some(border_box);
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
            _ => {}
          }
        }
      }

      if resolve_fit_max_width {
        if let Some(limit) = fit_max_width_limit {
          match compute_fit_border_box(Axis::Horizontal, limit, available_space.width) {
            Ok(max_border_box) if max_border_box.is_finite() => {
              let max_border_box = max_border_box.max(0.0);
              if let Some(existing) = constraints.used_border_box_width {
                constraints.used_border_box_width = Some(existing.min(max_border_box));
              } else if let Some(width) = style.width {
                let base = constraints
                  .inline_percentage_base
                  .unwrap_or(percentage_base)
                  .max(0.0);
                let resolved = self.resolve_length_for_width(width, base, style).max(0.0);
                let border_box = if style.box_sizing == BoxSizing::ContentBox {
                  (resolved + fit_inset_w).max(0.0)
                } else {
                  resolved.max(0.0)
                };
                constraints.used_border_box_width = Some(border_box.min(max_border_box));
              } else if let Some(keyword) = style.width_keyword {
                match keyword {
                  IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent => {
                    let use_min = matches!(keyword, IntrinsicSizeKeyword::MinContent);
                    match intrinsic_range_for_physical_axis(Axis::Horizontal) {
                      Ok((min_intrinsic, max_intrinsic)) => {
                        let preferred = if use_min {
                          min_intrinsic.max(0.0)
                        } else {
                          max_intrinsic.max(0.0)
                        };
                        constraints.used_border_box_width = Some(preferred.min(max_border_box));
                      }
                      Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                      Err(_) => {}
                    }
                  }
                  IntrinsicSizeKeyword::FillAvailable => {
                    let base = constraints
                      .inline_percentage_base
                      .unwrap_or(percentage_base)
                      .max(0.0);
                    let border_box = if style.box_sizing == BoxSizing::ContentBox {
                      (base + fit_inset_w).max(0.0)
                    } else {
                      base.max(0.0)
                    };
                    constraints.used_border_box_width = Some(border_box.min(max_border_box));
                  }
                  IntrinsicSizeKeyword::FitContent { .. } => {}
                  IntrinsicSizeKeyword::CalcSize(_) => {}
                }
              }
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
            _ => {}
          }
        }
      }

      if resolve_fit_max_height {
        if let Some(limit) = fit_max_height_limit {
          match compute_fit_border_box(Axis::Vertical, limit, available_space.height) {
            Ok(max_border_box) if max_border_box.is_finite() => {
              let max_border_box = max_border_box.max(0.0);
              if let Some(existing) = constraints.used_border_box_height {
                constraints.used_border_box_height = Some(existing.min(max_border_box));
              } else if let Some(height) = style.height {
                let base = match available_space.height {
                  taffy::style::AvailableSpace::Definite(v) => v,
                  _ => 0.0,
                }
                .max(0.0);
                let resolved = self.resolve_length_for_width(height, base, style).max(0.0);
                let border_box = if style.box_sizing == BoxSizing::ContentBox {
                  (resolved + fit_inset_h).max(0.0)
                } else {
                  resolved.max(0.0)
                };
                constraints.used_border_box_height = Some(border_box.min(max_border_box));
              } else if let Some(keyword) = style.height_keyword {
                match keyword {
                  IntrinsicSizeKeyword::MinContent | IntrinsicSizeKeyword::MaxContent => {
                    let use_min = matches!(keyword, IntrinsicSizeKeyword::MinContent);
                    match intrinsic_range_for_physical_axis(Axis::Vertical) {
                      Ok((min_intrinsic, max_intrinsic)) => {
                        let preferred = if use_min {
                          min_intrinsic.max(0.0)
                        } else {
                          max_intrinsic.max(0.0)
                        };
                        constraints.used_border_box_height = Some(preferred.min(max_border_box));
                      }
                      Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                      Err(_) => {}
                    }
                  }
                  IntrinsicSizeKeyword::FillAvailable => {
                    let base = match available_space.height {
                      taffy::style::AvailableSpace::Definite(v) => v,
                      _ => 0.0,
                    }
                    .max(0.0);
                    let border_box = if style.box_sizing == BoxSizing::ContentBox {
                      (base + fit_inset_h).max(0.0)
                    } else {
                      base.max(0.0)
                    };
                    constraints.used_border_box_height = Some(border_box.min(max_border_box));
                  }
                  IntrinsicSizeKeyword::FitContent { .. } => {}
                  IntrinsicSizeKeyword::CalcSize(_) => {}
                }
              }
            }
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => {}
            _ => {}
          }
        }
      }
    }

    // Taffy requests intrinsic min/max-content *height* contributions by setting
    // `AvailableSpace::{MinContent,MaxContent}` on the physical height axis. It can still provide a
    // definite width (the grid area's inline size) for those probes.
    //
    // When that happens, intrinsic block-size APIs are insufficient because they vary the inline
    // constraint between min/max-content widths. Grid row sizing, however, needs the height at the
    // *definite* inline size. The mismatch caused howtogeek.com's featured card grid to collapse to
    // a tiny row height, clipping away most of the card's contents.
    //
    // Measure these probes by laying out the item with the known width and an indefinite height so
    // percentage padding / aspect-ratio hacks resolve correctly.
    let height_probe_needs_definite_width_layout = known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::MinContent | taffy::style::AvailableSpace::MaxContent
      )
      && matches!(
        available_space.width,
        taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
      );

    if height_probe_needs_definite_width_layout {
      // Apply the same shrink-to-fit width behaviour as the main layout path for
      // `justify-self`/`justify-items` values other than `stretch`.
      let mut probe_constraints = constraints;
      probe_constraints.available_height = CrateAvailableSpace::Indefinite;
      probe_constraints.used_border_box_height = None;

      if known_dimensions.width.is_none()
        && matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(_)
        )
        && physical_width_is_auto(style)
      {
        if let taffy::style::AvailableSpace::Definite(area_width) = available_space.width {
          let justify = taffy_style
            .justify_self
            .unwrap_or(taffy::style::AlignItems::Stretch);
          if justify != taffy::style::AlignItems::Stretch {
            match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
              Ok(intrinsic_width) => {
                let mut used_width = intrinsic_width.max(0.0);
                if should_adjust_for_calc_percentage_edges
                  && area_width.is_finite()
                  && area_width > 1.0
                {
                  let (
                    padding_left,
                    padding_right,
                    _padding_top,
                    _padding_bottom,
                    border_left,
                    border_right,
                    _border_top,
                    _border_bottom,
                  ) = self.resolved_padding_border_for_measure(style, area_width);
                  let base0_insets_w = {
                    let (p_l, p_r, _p_t, _p_b, b_l, b_r, _b_t, _b_b) =
                      self.resolved_padding_border_for_measure(style, 0.0);
                    p_l + p_r + b_l + b_r
                  };
                  let insets_w = padding_left + padding_right + border_left + border_right;
                  let delta = insets_w - base0_insets_w;
                  if delta.is_finite() {
                    used_width = (used_width + delta).max(0.0);
                  }
                }
                used_width = used_width.min(area_width.max(0.0));
                probe_constraints.used_border_box_width = Some(used_width);
              }
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => {}
            }
          }
        }
      }

      record_measure_layout_call();
      let mut fragment = match fc.layout(box_node, &probe_constraints) {
        Ok(fragment) => fragment,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => return taffy::tree::MeasureOutput::ZERO,
      };

      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => probe_constraints
          .width()
          .unwrap_or_else(|| fragment.bounds.width()),
      };
      fragment.content = FragmentContent::Block {
        box_id: Some(box_node.id),
      };
      attach_fragment_style_for_box(&mut fragment, box_node);
      let content_size = Self::content_box_size_for_taffy_style(
        Size::new(fragment.bounds.width(), fragment.bounds.height()),
        taffy_style,
        percentage_base,
      );
      let size = taffy::geometry::Size {
        width: content_size.width.max(0.0),
        height: content_size.height.max(0.0),
      };

      let mut baseline_deadline_counter = 0usize;
      let baseline_y = if wants_baseline_y {
        match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let baseline_x = if wants_baseline_x {
        match first_baseline_offset_x(
          &fragment,
          style.writing_mode,
          &mut baseline_deadline_counter,
        ) {
          Ok(baseline) => baseline,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };
      let output = taffy::tree::MeasureOutput::from_size_and_baselines(
        size,
        taffy::geometry::Point {
          x: baseline_x,
          y: baseline_y,
        },
      );

      grid_measure_size_cache_store(key, output);
      if let Some(evicted) = push_measured_key(measured_node_keys.entry(node_id).or_default(), key)
      {
        measured_fragments.remove(&evicted);
      }
      measured_fragments.insert(key, fragment);
      measure_cache.insert(key, output);

      // Min/max-content height probes share the same measurement once the inline size is fixed.
      let mut alt_key = key;
      alt_key.available_height = match key.available_height {
        MeasureAvailKey::MinContent => MeasureAvailKey::MaxContent,
        MeasureAvailKey::MaxContent => MeasureAvailKey::MinContent,
        other => other,
      };
      if alt_key != key {
        grid_measure_size_cache_store(alt_key, output);
        measure_cache.insert(alt_key, output);
      }

      return output;
    }

    // Taffy requests intrinsic min-/max-content measurements by setting
    // `AvailableSpace::{MinContent,MaxContent}` on the *physical* axis being queried. These probes
    // are independent of CSS writing mode, so always answer them in physical X/Y space.
    //
    // Note: intrinsic *block* sizes depend on the used size in the inline axis (e.g. text wrapping),
    // so when Taffy probes the physical block axis while still providing a definite inline size we
    // must answer by performing layout at that inline size rather than using intrinsic sizing APIs.
    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
    let inline_size_is_definite = if inline_is_horizontal {
      known_dimensions
        .width
        .is_some_and(|w| w.is_finite() && w > 1.0)
        || matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(w) if w.is_finite() && w > 1.0
        )
    } else {
      known_dimensions
        .height
        .is_some_and(|h| h.is_finite() && h > 1.0)
        || matches!(
          available_space.height,
          taffy::style::AvailableSpace::Definite(h) if h.is_finite() && h > 1.0
        )
    };

    let mut intrinsic_width: Option<f32> = None;
    if known_dimensions.width.is_none() {
      // In vertical writing-modes, the physical X axis is the block axis. Skip the intrinsic fast
      // path when the inline axis is definite so we can measure block size via layout at that inline
      // size.
      let needs_layout_for_width_probe = !inline_is_horizontal
        && inline_size_is_definite
        && matches!(
          available_space.width,
          taffy::style::AvailableSpace::MinContent | taffy::style::AvailableSpace::MaxContent
        );
      intrinsic_width = if needs_layout_for_width_probe {
        None
      } else {
        match available_space.width {
          taffy::style::AvailableSpace::MinContent => fit_border_box_width.or_else(|| {
            Some(
              match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
                Ok(size) => size,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => 0.0,
              },
            )
          }),
          taffy::style::AvailableSpace::MaxContent => fit_border_box_width.or_else(|| {
            Some(
              match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
                Ok(size) => size,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => 0.0,
              },
            )
          }),
          _ => None,
        }
      };
    }

    let mut intrinsic_height: Option<f32> = None;
    if known_dimensions.height.is_none()
      && matches!(
        available_space.height,
        taffy::style::AvailableSpace::MinContent | taffy::style::AvailableSpace::MaxContent
      )
    {
      // In horizontal writing-modes, the physical Y axis is the block axis. When the inline axis is
      // definite (the resolved grid area width), measure block size via full layout at that inline
      // size so wrapped content contributes correctly.
      let needs_layout_for_height_probe = inline_is_horizontal && inline_size_is_definite;

      // CSS Grid track sizing uses intrinsic min/max-content contributions. When the item has a
      // definite preferred block size (e.g. `height: 100px`), browsers use that size as the
      // contribution even when the track is `min-content`/`max-content`. Taffy requests these
      // contributions via `AvailableSpace::{MinContent,MaxContent}` probe calls; ensure we answer
      // with the box's definite border-box size so fixed-size grid items don't collapse their
      // tracks and overlap subsequent rows (MDN pageset: sticky header overlapping the top banner).
      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => parent_inline_base.unwrap_or(0.0),
      };
      let (
        _padding_left,
        _padding_right,
        padding_top,
        padding_bottom,
        _border_left,
        _border_right,
        border_top,
        border_bottom,
      ) = self.resolved_padding_border_for_measure(style, percentage_base);
      let edges_h = padding_top + padding_bottom + border_top + border_bottom;

      let definite_border_box_height = style
        .height
        .and_then(|len| self.resolve_length_px_with_base(len, None, style))
        .map(|px| border_size_from_box_sizing(px.max(0.0), edges_h, style.box_sizing));

      intrinsic_height = match available_space.height {
        taffy::style::AvailableSpace::MinContent => fit_border_box_height
          .or(definite_border_box_height)
          .or_else(|| {
            if needs_layout_for_height_probe {
              None
            } else {
              Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              )
            }
          }),
        taffy::style::AvailableSpace::MaxContent => fit_border_box_height
          .or(definite_border_box_height)
          .or_else(|| {
            if needs_layout_for_height_probe {
              None
            } else {
              Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              )
            }
          }),
        _ => None,
      };
    }

    if !(wants_baseline_y || wants_baseline_x)
      && (intrinsic_width.is_some() || intrinsic_height.is_some())
    {
      let percentage_base = match available_space.width {
        taffy::style::AvailableSpace::Definite(w) => w,
        _ => parent_inline_base.unwrap_or(0.0),
      };
      let (width_inset, height_inset) = Self::taffy_measure_insets_px(taffy_style, percentage_base);

      // Grid items with `justify-self`/`justify-items` values other than `stretch` use a
      // content-based size in the physical X axis (roughly: max-content clamped to the grid
      // area).
      //
      // When answering an intrinsic probe via the intrinsic fast-path (i.e. without running full
      // `fc.layout`), mirror the shrink-to-fit width logic from the full layout path so a definite
      // grid area width does not incorrectly cause the item to stretch.
      let shrink_width = (intrinsic_width.is_none()
        && physical_width_is_auto(style)
        && matches!(
          available_space.width,
          taffy::style::AvailableSpace::Definite(_)
        ))
      .then(|| {
        let taffy::style::AvailableSpace::Definite(area_width) = available_space.width else {
          return None;
        };
        let justify = taffy_style
          .justify_self
          .unwrap_or(taffy::style::AlignItems::Stretch);
        if justify == taffy::style::AlignItems::Stretch {
          return None;
        }

        match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
          Ok(intrinsic_border_width) => {
            let mut used_width = intrinsic_border_width.max(0.0);

            if should_adjust_for_calc_percentage_edges && area_width.is_finite() && area_width > 1.0
            {
              let (
                padding_left,
                padding_right,
                _padding_top,
                _padding_bottom,
                border_left,
                border_right,
                _border_top,
                _border_bottom,
              ) = self.resolved_padding_border_for_measure(style, area_width);
              let base0_insets_w = {
                let (p_l, p_r, _p_t, _p_b, b_l, b_r, _b_t, _b_b) =
                  self.resolved_padding_border_for_measure(style, 0.0);
                p_l + p_r + b_l + b_r
              };
              let insets_w = padding_left + padding_right + border_left + border_right;
              let delta = insets_w - base0_insets_w;
              if delta.is_finite() {
                used_width = (used_width + delta).max(0.0);
              }
            }

            used_width = used_width.min(area_width.max(0.0));
            Some((used_width - width_inset).max(0.0))
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      })
      .flatten();

      // Even when Taffy is probing intrinsic sizes on the *other* axis (e.g. `height: min-content`),
      // it still expects the returned width to reflect the item's actual used width. For grid items,
      // `width: auto` does **not** always stretch to fill the grid area — when the inline-axis
      // self-alignment isn't `stretch`, auto sizing resolves to a shrink-to-fit size (clamped to the
      // grid area's definite inline size).
      //
      // Taffy doesn't always request an explicit width measurement in these mixed-axis probe calls
      // (notably with subgrids), so compute the shrink-to-fit width here as well to avoid falling
      // back to the full available width.
      let shrink_to_fit_border_box_width = if intrinsic_width.is_none()
        && physical_width_is_auto(style)
        && taffy_style.aspect_ratio.is_none()
        && taffy_style
          .justify_self
          .unwrap_or(taffy::style::AlignItems::Stretch)
          != taffy::style::AlignItems::Stretch
      {
        let area_width = match available_space.width {
          taffy::style::AvailableSpace::Definite(w) => Some(w),
          _ => known_dimensions.width,
        }
        .filter(|w| w.is_finite() && *w > 1.0);
        match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
          Ok(intrinsic_width) => {
            let mut used_width = intrinsic_width.max(0.0);
            if let Some(area_width) = area_width {
              if has_calc_percentage_edges && area_width.is_finite() && area_width > 1.0 {
                let (
                  padding_left,
                  padding_right,
                  _padding_top,
                  _padding_bottom,
                  border_left,
                  border_right,
                  _border_top,
                  _border_bottom,
                ) = self.resolved_padding_border_for_measure(style, area_width);
                let base0_insets_w = {
                  let (p_l, p_r, _p_t, _p_b, b_l, b_r, _b_t, _b_b) =
                    self.resolved_padding_border_for_measure(style, 0.0);
                  p_l + p_r + b_l + b_r
                };
                let insets_w = padding_left + padding_right + border_left + border_right;
                let delta = insets_w - base0_insets_w;
                if delta.is_finite() {
                  used_width = (used_width + delta).max(0.0);
                }
              }
              used_width = used_width.min(area_width.max(0.0));
            }
            Some(used_width)
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => None,
        }
      } else {
        None
      };

      let width = intrinsic_width
        .or(shrink_to_fit_border_box_width)
        .map(|border_width| (border_width - width_inset).max(0.0))
        .or(shrink_width)
        .unwrap_or_else(|| fallback_size(known_dimensions.width, available_space.width).max(0.0));

      let height = if let Some(border_height) = intrinsic_height {
        (border_height - height_inset).max(0.0)
      } else {
        // When Taffy probes intrinsic inline sizes (min-/max-content width), it can still provide
        // a "known" or definite block size. That value is not the box's intrinsic height; it's a
        // sizing artifact of the grid algorithm and must not leak back into track sizing (doing so
        // can balloon auto rows and stretch every grid item to a viewport-sized height).
        //
        // Always answer width probes with a content-based intrinsic block size.
        let mode = match available_space.width {
          taffy::style::AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
          taffy::style::AvailableSpace::MaxContent => IntrinsicSizingMode::MaxContent,
          _ => IntrinsicSizingMode::MaxContent,
        };
        let border_block_size = match fc.compute_intrinsic_block_size(box_node, mode) {
          Ok(size) => size,
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => 0.0,
        };
        (border_block_size - height_inset).max(0.0)
      };

      let size = taffy::geometry::Size { width, height };
      let output = taffy::tree::MeasureOutput::from_size(size);
      if trace_measure_id.is_some_and(|id| id == box_node.id) {
        eprintln!(
          "[grid-measure-item-out] box_id={} node_id={:?} key={:?} (intrinsic) -> size={:?}",
          box_node.id, node_id, key, output.size
        );
      }
      grid_measure_size_cache_store(key, output);
      measure_cache.insert(key, output);
      return output;
    }

    // CSS Grid §11.5: Auto-sized, non-stretch aligned grid items use a shrink-to-fit size
    // (clamped to the grid area's definite size).
    //
    // Taffy sometimes reports the grid area's size as `known_dimensions` instead of (or in addition
    // to) `available_space` (notably when subgrids participate in track sizing). Treat either as
    // the clamp base for shrink-to-fit sizing.
    if physical_width_is_auto(style) && taffy_style.aspect_ratio.is_none() {
      let justify = taffy_style
        .justify_self
        .unwrap_or(taffy::style::AlignItems::Stretch);
      if justify != taffy::style::AlignItems::Stretch {
        let area_width = match available_space.width {
          taffy::style::AvailableSpace::Definite(w) => Some(w),
          _ => known_dimensions.width,
        }
        .filter(|w| w.is_finite() && *w > 1.0);
        match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
          Ok(intrinsic_width) => {
            let mut used_width = intrinsic_width.max(0.0);
            if let Some(area_width) = area_width {
              if has_calc_percentage_edges && area_width.is_finite() && area_width > 1.0 {
                let (
                  padding_left,
                  padding_right,
                  _padding_top,
                  _padding_bottom,
                  border_left,
                  border_right,
                  _border_top,
                  _border_bottom,
                ) = self.resolved_padding_border_for_measure(style, area_width);
                let base0_insets_w = {
                  let (p_l, p_r, _p_t, _p_b, b_l, b_r, _b_t, _b_b) =
                    self.resolved_padding_border_for_measure(style, 0.0);
                  p_l + p_r + b_l + b_r
                };
                let insets_w = padding_left + padding_right + border_left + border_right;
                let delta = insets_w - base0_insets_w;
                if delta.is_finite() {
                  used_width = (used_width + delta).max(0.0);
                }
              }
              used_width = used_width.min(area_width.max(0.0));
            }
            if trace_measure {
              eprintln!(
                "[grid-measure] shrink-to-fit node_id={:?} intrinsic={:.2} area={:?} used={:.2}",
                node_id, intrinsic_width, area_width, used_width
              );
            }
            constraints.used_border_box_width = Some(used_width);
          }
          Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
          Err(_) => {}
        }
      }
    }

    record_measure_layout_call();
    // `height: fit-content` uses the available height as its clamp target when that height is
    // definite; when the available height is indefinite, it should behave like max-content sizing.
    //
    // During grid track sizing, Taffy represents "unknown" heights as intrinsic probes (min/max
    // content) or as a tiny definite `0px/1px` probe (see `constraints_from_taffy`). In those cases,
    // treat the fit-content keyword as `auto` so we measure the true content height at the current
    // definite inline size (matching browser behaviour and avoiding overestimation from formatting
    // context intrinsic block sizes).
    let clear_fit_content_height_for_layout = style
      .height_keyword
      .is_some_and(|kw| matches!(kw, IntrinsicSizeKeyword::FitContent { limit: None }))
      && !matches!(
        constraints.available_height,
        CrateAvailableSpace::Definite(_)
      );
    let fragment = {
      let run_layout = |node: &BoxNode| fc.layout(node, &constraints);
      let result = if clear_fit_content_height_for_layout {
        let mut override_style: ComputedStyle = (*style).clone();
        override_style.height = None;
        override_style.height_keyword = None;
        let override_style = Arc::new(override_style);
        if box_node.id != 0 {
          crate::layout::style_override::with_style_override(box_node.id, override_style, || {
            run_layout(box_node)
          })
        } else {
          let mut cloned = box_node.clone();
          cloned.style = override_style;
          run_layout(&cloned)
        }
      } else {
        run_layout(box_node)
      };
      match result {
        Ok(fragment) => fragment,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => return taffy::tree::MeasureOutput::ZERO,
      }
    };
    let mut fragment = fragment;
    if trace_measure {
      eprintln!(
        "[grid-measure] laid node_id={:?} border_box={:?} used_border_box_width={:?}",
        node_id, fragment.bounds, constraints.used_border_box_width
      );
    }
    let percentage_base = match available_space.width {
      taffy::style::AvailableSpace::Definite(w) => w,
      _ => constraints
        .width()
        .unwrap_or_else(|| fragment.bounds.width()),
    };
    fragment.content = match &box_node.box_type {
      crate::tree::box_tree::BoxType::Replaced(replaced_box) => FragmentContent::Replaced {
        replaced_type: replaced_box.replaced_type.clone(),
        box_id: Some(box_node.id),
      },
      _ => FragmentContent::Block {
        box_id: Some(box_node.id),
      },
    };
    attach_fragment_style_for_box(&mut fragment, box_node);
    let mut content_size = Self::content_box_size_for_taffy_style(
      Size::new(fragment.bounds.width(), fragment.bounds.height()),
      taffy_style,
      percentage_base,
    );
    let eps = 0.01;
    if content_size.width <= eps || content_size.height <= eps {
      let mut deadline_counter = 0usize;
      let span = match Self::fragment_descendant_span(&fragment, &mut deadline_counter) {
        Ok(span) => span,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => None,
      };
      if let Some(span) = span {
        if content_size.width <= eps && span.width > eps {
          content_size.width = span.width;
        }
        if content_size.height <= eps && span.height > eps {
          content_size.height = span.height;
        }
      }
    }
    let size = taffy::geometry::Size {
      width: content_size.width.max(0.0),
      height: content_size.height.max(0.0),
    };
    let mut baseline_deadline_counter = 0usize;
    let baseline_y = if wants_baseline_y {
      match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
        Ok(baseline) => baseline,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => None,
      }
    } else {
      None
    };
    let baseline_x = if wants_baseline_x {
      match first_baseline_offset_x(
        &fragment,
        style.writing_mode,
        &mut baseline_deadline_counter,
      ) {
        Ok(baseline) => baseline,
        Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
        Err(_) => None,
      }
    } else {
      None
    };
    let output = taffy::tree::MeasureOutput::from_size_and_baselines(
      size,
      taffy::geometry::Point {
        x: baseline_x,
        y: baseline_y,
      },
    );
    if trace_measure_id.is_some_and(|id| id == box_node.id) {
      eprintln!(
        "[grid-measure-item-out] box_id={} node_id={:?} key={:?} (layout) -> size={:?}",
        box_node.id, node_id, key, output.size
      );
    }
    grid_measure_size_cache_store(key, output);
    if let Some(evicted) = push_measured_key(measured_node_keys.entry(node_id).or_default(), key) {
      measured_fragments.remove(&evicted);
    }
    measured_fragments.insert(key, fragment);
    measure_cache.insert(key, output);
    output
  }
}

fn grid_child_fingerprint(
  children: &[&BoxNode],
  has_positioned_children: bool,
  deadline_counter: &mut usize,
) -> Result<u64, LayoutError> {
  use std::hash::Hash;
  use std::hash::Hasher;
  let mut h = FingerprintHasher::default();
  // Whether the container has any out-of-flow positioned children impacts whether the grid can be
  // safely represented as a block in Taffy (see `simple_grid`). Include this in the cache key so
  // templates built for "simple" in-flow-only grids are not reused for grids that need track data
  // for positioned static positioning.
  has_positioned_children.hash(&mut h);
  children.len().hash(&mut h);
  for child in children {
    check_layout_deadline(deadline_counter)?;
    // The cached Taffy template stores converted styles; most conversion output depends only on
    // `ComputedStyle`, but some bits (such as whether the node is a replaced element) come from the
    // BoxNode metadata. Include those so we don't reuse templates across incompatible leaf styles.
    let child_style_override = style_override_for(child.id);
    let child_style: &ComputedStyle = child_style_override
      .as_deref()
      .unwrap_or_else(|| child.style.as_ref());
    taffy_grid_item_style_fingerprint(child_style).hash(&mut h);
    child.is_replaced().hash(&mut h);
  }
  Ok(h.finish())
}

fn translate_fragment_tree(
  fragment: &mut FragmentNode,
  delta: Point,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  check_layout_deadline(deadline_counter)?;
  fragment.bounds = Rect::new(
    Point::new(fragment.bounds.x() + delta.x, fragment.bounds.y() + delta.y),
    fragment.bounds.size,
  );
  if let Some(logical) = fragment.logical_override {
    fragment.logical_override = Some(Rect::new(
      Point::new(logical.x() + delta.x, logical.y() + delta.y),
      logical.size,
    ));
  }
  Ok(())
}

fn cancel_translation_for_out_of_flow_positioned_descendants(
  fragment: &mut FragmentNode,
  delta: Point,
  external_fixed_cb: bool,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  if delta.x == 0.0 && delta.y == 0.0 {
    return Ok(());
  }
  let cancel = Point::new(-delta.x, -delta.y);

  fn walk(
    node: &mut FragmentNode,
    cancel: Point,
    has_abs_cb: bool,
    has_fixed_cb: bool,
    external_fixed_cb: bool,
    deadline_counter: &mut usize,
  ) -> Result<(), LayoutError> {
    check_layout_deadline(deadline_counter)?;
    let (has_abs_cb_here, has_fixed_cb_here) = node
      .style
      .as_deref()
      .map(|style| {
        (
          has_abs_cb || style.establishes_abs_containing_block(),
          has_fixed_cb || style.establishes_fixed_containing_block(),
        )
      })
      .unwrap_or((has_abs_cb, has_fixed_cb));

    for child in node.children_mut() {
      let is_abs = child
        .style
        .as_deref()
        .is_some_and(|style| matches!(style.position, crate::style::position::Position::Absolute));
      let is_fixed = child
        .style
        .as_deref()
        .is_some_and(|style| matches!(style.position, crate::style::position::Position::Fixed));

      let needs_cancel =
        (is_abs && !has_abs_cb_here) || (is_fixed && external_fixed_cb && !has_fixed_cb_here);
      if needs_cancel {
        child.bounds = child.bounds.translate(cancel);
        child.logical_override = child
          .logical_override
          .map(|logical| logical.translate(cancel));
        // The out-of-flow subtree inherits the cancelled translation. Avoid walking into it so we
        // don't double-apply the adjustment.
        continue;
      }

      walk(
        child,
        cancel,
        has_abs_cb_here,
        has_fixed_cb_here,
        external_fixed_cb,
        deadline_counter,
      )?;
    }

    Ok(())
  }

  let has_abs_cb_root = fragment
    .style
    .as_deref()
    .is_some_and(|style| style.establishes_abs_containing_block());
  let has_fixed_cb_root = fragment
    .style
    .as_deref()
    .is_some_and(|style| style.establishes_fixed_containing_block());
  walk(
    fragment,
    cancel,
    has_abs_cb_root,
    has_fixed_cb_root,
    external_fixed_cb,
    deadline_counter,
  )
}

fn normalize_fragment_origin(
  mut fragment: FragmentNode,
  deadline_counter: &mut usize,
) -> Result<FragmentNode, LayoutError> {
  let origin = fragment.bounds.origin;
  if origin.x != 0.0 || origin.y != 0.0 {
    translate_fragment_tree(
      &mut fragment,
      Point::new(-origin.x, -origin.y),
      deadline_counter,
    )?;
  }
  Ok(fragment)
}

#[cfg(test)]
thread_local! {
  static GRID_TEST_MEASURE_HOOK: RefCell<Option<Box<dyn Fn(&BoxNode) -> Option<FragmentNode>>>> =
    RefCell::new(None);
}

#[cfg(test)]
struct GridTestMeasureHookGuard;

#[cfg(test)]
fn set_grid_test_measure_hook(
  hook: impl Fn(&BoxNode) -> Option<FragmentNode> + 'static,
) -> GridTestMeasureHookGuard {
  GRID_TEST_MEASURE_HOOK.with(|slot| {
    *slot.borrow_mut() = Some(Box::new(hook));
  });
  GridTestMeasureHookGuard
}

#[cfg(test)]
impl Drop for GridTestMeasureHookGuard {
  fn drop(&mut self) {
    GRID_TEST_MEASURE_HOOK.with(|slot| {
      slot.borrow_mut().take();
    });
  }
}

fn find_first_baseline_offset(
  fragment: &FragmentNode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  check_layout_deadline(deadline_counter)?;
  if let Some(baseline) = fragment.baseline {
    return Ok(Some(baseline));
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => return Ok(Some(*baseline)),
    FragmentContent::Text {
      baseline_offset, ..
    } => return Ok(Some(*baseline_offset)),
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(b) = find_first_baseline_offset(child, deadline_counter)? {
      return Ok(Some(child.bounds.y() + b));
    }
  }

  Ok(None)
}

fn find_first_baseline_offset_x(
  fragment: &FragmentNode,
  block_positive: bool,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  check_layout_deadline(deadline_counter)?;

  let resolve_from_block_start = |offset: f32, extent: f32| -> f32 {
    if block_positive {
      offset
    } else if extent.is_finite() && extent > 0.0 {
      (extent - offset).max(0.0)
    } else {
      offset
    }
  };

  let extent = fragment.bounds.width();
  if let Some(baseline) = fragment.baseline {
    return Ok(Some(resolve_from_block_start(baseline, extent)));
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => {
      return Ok(Some(resolve_from_block_start(*baseline, extent)));
    }
    FragmentContent::Text {
      baseline_offset, ..
    } => {
      return Ok(Some(resolve_from_block_start(*baseline_offset, extent)));
    }
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(b) = find_first_baseline_offset_x(child, block_positive, deadline_counter)? {
      return Ok(Some(child.bounds.x() + b));
    }
  }

  Ok(None)
}

fn first_baseline_offset(
  fragment: &FragmentNode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  find_first_baseline_offset(fragment, deadline_counter)
}

fn first_baseline_offset_x(
  fragment: &FragmentNode,
  writing_mode: WritingMode,
  deadline_counter: &mut usize,
) -> Result<Option<f32>, LayoutError> {
  if !crate::style::block_axis_is_horizontal(writing_mode) {
    return Ok(None);
  }
  find_first_baseline_offset_x(
    fragment,
    crate::style::block_axis_positive(writing_mode),
    deadline_counter,
  )
}

fn apply_alignment_fallback_for_grid(
  free_space: f32,
  num_items: usize,
  alignment_mode: TaffyAlignContent,
) -> TaffyAlignContent {
  // Mirrors taffy's alignment fallback, but scoped locally for grid post-processing.
  if num_items <= 1 || free_space <= 0.0 {
    return match alignment_mode {
      TaffyAlignContent::Stretch => TaffyAlignContent::FlexStart,
      TaffyAlignContent::SpaceBetween => TaffyAlignContent::FlexStart,
      TaffyAlignContent::SpaceAround => TaffyAlignContent::Center,
      TaffyAlignContent::SpaceEvenly => TaffyAlignContent::Center,
      other => other,
    };
  }

  alignment_mode
}

fn compute_alignment_offset_for_grid(
  free_space: f32,
  num_items: usize,
  alignment_mode: TaffyAlignContent,
  is_first: bool,
) -> f32 {
  if is_first {
    match alignment_mode {
      TaffyAlignContent::Start => 0.0,
      TaffyAlignContent::FlexStart => 0.0,
      TaffyAlignContent::End => free_space,
      TaffyAlignContent::FlexEnd => free_space,
      TaffyAlignContent::Center => free_space / 2.0,
      TaffyAlignContent::Stretch => 0.0,
      TaffyAlignContent::SpaceBetween => 0.0,
      TaffyAlignContent::SpaceAround => {
        if free_space >= 0.0 {
          (free_space / num_items as f32) / 2.0
        } else {
          free_space / 2.0
        }
      }
      TaffyAlignContent::SpaceEvenly => {
        if free_space >= 0.0 {
          free_space / (num_items + 1) as f32
        } else {
          free_space / 2.0
        }
      }
    }
  } else {
    let free_space = free_space.max(0.0);
    match alignment_mode {
      TaffyAlignContent::Start
      | TaffyAlignContent::FlexStart
      | TaffyAlignContent::End
      | TaffyAlignContent::FlexEnd
      | TaffyAlignContent::Center
      | TaffyAlignContent::Stretch => 0.0,
      TaffyAlignContent::SpaceBetween => free_space / (num_items.saturating_sub(1).max(1) as f32),
      TaffyAlignContent::SpaceAround => free_space / (num_items.max(1) as f32),
      TaffyAlignContent::SpaceEvenly => free_space / (num_items + 1) as f32,
    }
  }
}

fn grid_track_ranges_for_container(
  taffy: &TaffyTree<*const BoxNode>,
  node_id: TaffyNodeId,
  layout: &TaffyLayout,
  container_style: &taffy::style::Style,
  axis_style: GridAxisStyle,
  apply_axis_mirroring: bool,
) -> Option<GridTrackRanges> {
  let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) else {
    return None;
  };

  if info.rows.sizes.is_empty() && info.columns.sizes.is_empty() {
    return None;
  }

  let row_offsets = compute_track_offsets(
    &info.rows,
    layout.size.height,
    layout.padding.top,
    layout.padding.bottom,
    layout.border.top,
    layout.border.bottom,
    container_style
      .align_content
      .unwrap_or(TaffyAlignContent::Stretch),
  );
  let col_offsets = compute_track_offsets(
    &info.columns,
    layout.size.width,
    layout.padding.left,
    layout.padding.right,
    layout.border.left,
    layout.border.right,
    container_style
      .justify_content
      .unwrap_or(TaffyAlignContent::Stretch),
  );

  let mut rows = Vec::with_capacity(info.rows.sizes.len());
  for (idx, size) in info.rows.sizes.iter().copied().enumerate() {
    let start_idx = 1usize + 2usize * idx;
    let Some(&start) = row_offsets.get(start_idx) else {
      break;
    };
    let end = start + size;
    if start.is_finite() && end.is_finite() && end > start {
      rows.push((start, end));
    }
  }

  let mut columns = Vec::with_capacity(info.columns.sizes.len());
  for (idx, size) in info.columns.sizes.iter().copied().enumerate() {
    let start_idx = 1usize + 2usize * idx;
    let Some(&start) = col_offsets.get(start_idx) else {
      break;
    };
    let end = start + size;
    if start.is_finite() && end.is_finite() && end > start {
      columns.push((start, end));
    }
  }

  if apply_axis_mirroring && (!rows.is_empty() || !columns.is_empty()) {
    let inline_is_horizontal = axis_style.inline_is_horizontal();
    let mut mirror_x = false;
    let mut mirror_y = false;

    if !axis_style.inline_positive() {
      if inline_is_horizontal {
        mirror_x = true;
      } else {
        mirror_y = true;
      }
    }
    if !axis_style.block_positive() {
      if inline_is_horizontal {
        mirror_y = true;
      } else {
        mirror_x = true;
      }
    }

    if mirror_y && !rows.is_empty() {
      let track_count = info.rows.sizes.len();
      if track_count > 0 && row_offsets.len() >= 2 {
        let span_start = row_offsets[1];
        let span_end = row_offsets
          .get(track_count * 2)
          .copied()
          .or_else(|| row_offsets.last().copied())
          .unwrap_or(span_start);
        if span_end > span_start {
          for range in rows.iter_mut() {
            let (start, end) = *range;
            let new_start = span_start + (span_end - end);
            let new_end = span_start + (span_end - start);
            *range = (new_start, new_end);
          }
        }
      }
    }

    if mirror_x && !columns.is_empty() {
      let track_count = info.columns.sizes.len();
      if track_count > 0 && col_offsets.len() >= 2 {
        let span_start = col_offsets[1];
        let span_end = col_offsets
          .get(track_count * 2)
          .copied()
          .or_else(|| col_offsets.last().copied())
          .unwrap_or(span_start);
        if span_end > span_start {
          for range in columns.iter_mut() {
            let (start, end) = *range;
            let new_start = span_start + (span_end - end);
            let new_end = span_start + (span_end - start);
            *range = (new_start, new_end);
          }
        }
      }
    }
  }

  if rows.is_empty() && columns.is_empty() {
    None
  } else {
    Some(GridTrackRanges { rows, columns })
  }
}

fn compute_track_offsets(
  tracks: &DetailedGridTracksInfo,
  axis_size: f32,
  padding_start: f32,
  padding_end: f32,
  border_start: f32,
  border_end: f32,
  alignment: TaffyAlignContent,
) -> Vec<f32> {
  let track_count = tracks.sizes.len();
  if track_count == 0 {
    return Vec::new();
  }

  let mut gutters = tracks.gutters.clone();
  if gutters.len() < track_count + 1 {
    gutters.resize(track_count + 1, 0.0);
  }

  #[derive(Clone, Copy)]
  struct TrackEntry {
    size: f32,
    is_gutter: bool,
  }

  let mut entries: Vec<TrackEntry> = Vec::with_capacity(track_count * 2 + 1);
  entries.push(TrackEntry {
    size: gutters.get(0).copied().unwrap_or(0.0),
    is_gutter: true,
  });
  for i in 0..track_count {
    entries.push(TrackEntry {
      size: tracks.sizes[i],
      is_gutter: false,
    });
    entries.push(TrackEntry {
      size: gutters.get(i + 1).copied().unwrap_or(0.0),
      is_gutter: true,
    });
  }

  let used_size: f32 = entries.iter().map(|t| t.size).sum();
  let content_size = axis_size - padding_start - padding_end - border_start - border_end;
  let free_space = content_size - used_size;
  let aligned = apply_alignment_fallback_for_grid(free_space, track_count, alignment);

  let mut offsets = Vec::with_capacity(entries.len());
  let mut total_offset = padding_start + border_start;
  let mut first_track_seen = false;
  for entry in entries {
    let is_first_track = !first_track_seen && !entry.is_gutter;
    if is_first_track {
      first_track_seen = true;
    }
    let offset_within = if entry.is_gutter {
      0.0
    } else {
      compute_alignment_offset_for_grid(free_space, track_count.max(1), aligned, is_first_track)
    };
    offsets.push(total_offset + offset_within);
    total_offset += offset_within + entry.size;
  }

  offsets
}

fn grid_area_for_item(offsets: &[f32], start_line: u16, end_line: u16) -> Option<(f32, f32)> {
  let start_idx = (start_line.saturating_sub(1) as usize).saturating_mul(2);
  let end_idx = (end_line.saturating_sub(1) as usize).saturating_mul(2);
  if start_idx + 1 >= offsets.len() || end_idx >= offsets.len() {
    return None;
  }
  Some((offsets[start_idx + 1], offsets[end_idx]))
}

fn grid_area_for_positioned_item(
  offsets: &[f32],
  start_line: u16,
  end_line: u16,
) -> Option<(f32, f32)> {
  let start_idx = (start_line.saturating_sub(1) as usize).saturating_mul(2);
  let end_idx = (end_line.saturating_sub(1) as usize).saturating_mul(2);
  let last = offsets.last().copied()?;

  // Absolutely positioned items can reference implicit grid lines beyond the tracks that were
  // realized for in-flow items. Those extra implicit tracks do not participate in sizing, so treat
  // any missing line offsets as zero-sized tracks at the end of the realized grid.
  //
  // This fallback is intentionally narrow: it only affects static-position resolution and preserves
  // the stricter behavior of `grid_area_for_item` for in-flow grid processing.
  let start = offsets.get(start_idx + 1).copied().unwrap_or(last);
  let end = offsets.get(end_idx).copied().unwrap_or(last);
  Some((start, end))
}

#[derive(Clone, Copy, Debug)]
enum ResolvedGridPlacementComponent<'a> {
  Auto,
  Line(i32),
  Span(u16),
  NamedLine(&'a str, i16),
  NamedSpan(&'a str, u16),
  Unsupported,
}

fn grid_placement_component_from_taffy<'a>(
  placement: &'a TaffyGridPlacement<String>,
) -> ResolvedGridPlacementComponent<'a> {
  match placement {
    TaffyGridPlacement::Auto => ResolvedGridPlacementComponent::Auto,
    TaffyGridPlacement::Line(line) => ResolvedGridPlacementComponent::Line(line.as_i16() as i32),
    TaffyGridPlacement::Span(span) => ResolvedGridPlacementComponent::Span(*span),
    TaffyGridPlacement::NamedLine(name, idx) => {
      ResolvedGridPlacementComponent::NamedLine(name, *idx)
    }
    TaffyGridPlacement::NamedSpan(name, span) => {
      ResolvedGridPlacementComponent::NamedSpan(name, *span)
    }
  }
}

fn resolved_grid_item_range_from_parent_layout(
  taffy: &TaffyTree<*const BoxNode>,
  node_id: TaffyNodeId,
  axis: CssGridAxis,
) -> Option<(u16, u16)> {
  let parent_id = taffy.parent(node_id)?;
  let parent_axes_swapped = taffy.style(parent_id).ok()?.axes_swapped;
  let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(parent_id) else {
    return None;
  };

  let child_count = taffy.child_count(parent_id);
  let idx = (0..child_count).find(|&idx| taffy.get_child_id(parent_id, idx) == node_id)?;
  let item = info.items.get(idx)?;

  // `DetailedLayoutInfo` stores item placement in Taffy's physical axes (columns=x, rows=y).
  // Map those back into CSS grid axes using *the parent grid's* `axes_swapped` value (which is
  // derived from the parent's effective grid axis style).
  let (start_line, end_line) = match (axis, parent_axes_swapped) {
    (CssGridAxis::Column, false) => (item.column_start, item.column_end),
    (CssGridAxis::Column, true) => (item.row_start, item.row_end),
    (CssGridAxis::Row, false) => (item.row_start, item.row_end),
    (CssGridAxis::Row, true) => (item.column_start, item.column_end),
  };
  (end_line > start_line).then_some((start_line, end_line))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CssGridAxis {
  Column,
  Row,
}

impl CssGridAxis {
  fn is_subgrid(self, style: &ComputedStyle) -> bool {
    match self {
      CssGridAxis::Column => style.grid_column_subgrid,
      CssGridAxis::Row => style.grid_row_subgrid,
    }
  }

  fn line_names<'a>(self, style: &'a ComputedStyle) -> &'a [Vec<String>] {
    match self {
      CssGridAxis::Column => style.grid_column_line_names.as_slice(),
      CssGridAxis::Row => style.grid_row_line_names.as_slice(),
    }
  }

  fn subgrid_extra_line_names<'a>(self, style: &'a ComputedStyle) -> &'a [Vec<String>] {
    match self {
      CssGridAxis::Column => style.subgrid_column_line_names.as_slice(),
      CssGridAxis::Row => style.subgrid_row_line_names.as_slice(),
    }
  }

  fn placement_start(self, style: &ComputedStyle) -> i32 {
    match self {
      CssGridAxis::Column => style.grid_column_start,
      CssGridAxis::Row => style.grid_row_start,
    }
  }

  fn placement_end(self, style: &ComputedStyle) -> i32 {
    match self {
      CssGridAxis::Column => style.grid_column_end,
      CssGridAxis::Row => style.grid_row_end,
    }
  }

  fn placement_raw<'a>(self, style: &'a ComputedStyle) -> Option<&'a str> {
    match self {
      CssGridAxis::Column => style.grid_column_raw.as_deref(),
      CssGridAxis::Row => style.grid_row_raw.as_deref(),
    }
  }

  /// Returns the number of grid lines for `node_id` on this axis (tracks + 1), if available.
  fn node_line_count(
    self,
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
  ) -> Option<u16> {
    if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) {
      let axes_swapped = taffy.style(node_id).ok()?.axes_swapped;
      let track_count = match (self, axes_swapped) {
        // When axes are swapped, CSS columns map to physical Y (Taffy rows) and CSS rows map to
        // physical X (Taffy columns).
        (CssGridAxis::Column, false) => info.columns.sizes.len(),
        (CssGridAxis::Column, true) => info.rows.sizes.len(),
        (CssGridAxis::Row, false) => info.rows.sizes.len(),
        (CssGridAxis::Row, true) => info.columns.sizes.len(),
      };
      return u16::try_from(track_count.saturating_add(1)).ok();
    }

    // Subgrid nodes don't always expose detailed track info. When this axis is subgridded we can
    // derive the local line count from the parent's span (which is stored as start/end placement
    // coordinates on the subgrid container itself).
    let node_ptr = *taffy.get_node_context(node_id)?;
    let node = unsafe { &*node_ptr };
    let start = self.placement_start(&node.style);
    let end = self.placement_end(&node.style);
    (start > 0 && end > start)
      .then(|| u16::try_from((end - start + 1).max(0)).ok())
      .flatten()
  }
}

const MAX_SUBGRID_LINE_NAME_INHERITANCE_DEPTH: usize = 64;

fn merge_line_names(base: &mut [Vec<String>], extra: &[Vec<String>]) {
  for (i, extra_names) in extra.iter().enumerate() {
    if let Some(target) = base.get_mut(i) {
      target.extend(extra_names.iter().cloned());
    }
  }
}

fn resolve_effective_grid_line_names_for_node_axis(
  taffy: &TaffyTree<*const BoxNode>,
  node_id: TaffyNodeId,
  axis: CssGridAxis,
) -> Option<Vec<Vec<String>>> {
  fn taffy_expanded_line_names_for_axis(
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    axis: CssGridAxis,
  ) -> Option<Vec<Vec<String>>> {
    let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(node_id) else {
      return None;
    };
    let axes_swapped = taffy.style(node_id).ok()?.axes_swapped;
    // Taffy stores named line data in physical axes (columns=x, rows=y). Map CSS axes to the
    // appropriate physical axis based on whether the grid axes are swapped by the writing mode.
    let names = match (axis, axes_swapped) {
      (CssGridAxis::Column, false) => &info.column_line_names,
      (CssGridAxis::Column, true) => &info.row_line_names,
      (CssGridAxis::Row, false) => &info.row_line_names,
      (CssGridAxis::Row, true) => &info.column_line_names,
    };
    Some(names.clone())
  }

  fn inner(
    taffy: &TaffyTree<*const BoxNode>,
    node_id: TaffyNodeId,
    axis: CssGridAxis,
    depth: usize,
  ) -> Option<Vec<Vec<String>>> {
    if depth >= MAX_SUBGRID_LINE_NAME_INHERITANCE_DEPTH {
      return None;
    }

    if let Some(names) = taffy_expanded_line_names_for_axis(taffy, node_id, axis) {
      return Some(names);
    }

    let node_ptr = *taffy.get_node_context(node_id)?;
    let node = unsafe { &*node_ptr };
    if !axis.is_subgrid(&node.style) {
      return Some(axis.line_names(&node.style).to_vec());
    }

    let parent_id = taffy.parent(node_id)?;

    let parent_names = inner(taffy, parent_id, axis, depth + 1)?;

    let parent_line_count = axis
      .node_line_count(taffy, parent_id)
      .or_else(|| u16::try_from(parent_names.len()).ok())
      .filter(|count| *count > 0);

    let resolved_from_parent =
      resolved_grid_item_range_from_parent_layout(taffy, node_id, axis);
    let (start_line, end_line) = match resolved_from_parent {
      Some((start_line, end_line)) => (start_line, end_line),
      None => {
        let start = axis.placement_start(&node.style);
        let end = axis.placement_end(&node.style);
        let raw = axis.placement_raw(&node.style);
        resolve_grid_line_range_from_style(
          start,
          end,
          raw,
          parent_line_count,
          Some(parent_names.as_slice()),
        )?
      }
    };

    let span = end_line.checked_sub(start_line)?;
    let start_index = start_line.saturating_sub(1) as usize;
    let mut result: Vec<Vec<String>> = Vec::with_capacity(span as usize + 1);
    for i in 0..=span {
      let idx = start_index.saturating_add(i as usize);
      let mut names: Vec<String> = Vec::new();
      if let Some(parent_line) = parent_names.get(idx) {
        names.extend(parent_line.iter().cloned());
      }
      result.push(names);
    }

    // CSS Grid 2 §7.12.1 “subgrid-area-inheritance”:
    // When a subgrid begins/ends inside a named grid area, implicit `<area>-start`/`<area>-end`
    // line names must be clamped to the subgrid boundaries so that descendants can resolve
    // placements like `grid-column: main-start / main-end` even for partial overlaps.
    if let Some(parent_ptr) = taffy.get_node_context(parent_id).copied() {
      let parent_node = unsafe { &*parent_ptr };
      if !parent_node.style.grid_template_areas.is_empty() {
        if let Some(bounds) = validate_area_rectangles(&parent_node.style.grid_template_areas) {
          for (name, (top, bottom, left, right)) in bounds {
            let (area_start, area_end) = match axis {
              CssGridAxis::Column => (left.saturating_add(1), right.saturating_add(2)),
              CssGridAxis::Row => (top.saturating_add(1), bottom.saturating_add(2)),
            };
            let Some(area_start) = u16::try_from(area_start).ok() else {
              continue;
            };
            let Some(area_end) = u16::try_from(area_end).ok() else {
              continue;
            };

            let clamped_start = area_start.max(start_line);
            let clamped_end = area_end.min(end_line);
            if clamped_end <= clamped_start {
              continue;
            }

            let local_start = (clamped_start - start_line) as usize;
            let local_end = (clamped_end - start_line) as usize;

            if let Some(target) = result.get_mut(local_start) {
              target.push(format!("{name}-start"));
            }
            if let Some(target) = result.get_mut(local_end) {
              target.push(format!("{name}-end"));
            }
          }
        }
      }
    }

    let extra = axis.subgrid_extra_line_names(&node.style);
    if !extra.is_empty() {
      merge_line_names(&mut result, extra);
    }

    // Preserve any explicit names authored on the subgrid container itself, in case the computed
    // style stores them on `grid_*_line_names` rather than `subgrid_*_line_names`.
    let authored = axis.line_names(&node.style);
    if !authored.is_empty() {
      merge_line_names(&mut result, authored);
    }

    Some(result)
  }

  inner(taffy, node_id, axis, 0)
}

fn resolve_css_grid_line_to_u16(line: i32, line_count: Option<u16>) -> Option<u16> {
  match line.cmp(&0) {
    std::cmp::Ordering::Equal => None,
    std::cmp::Ordering::Greater => u16::try_from(line).ok(),
    std::cmp::Ordering::Less => {
      let line_count = line_count? as i32;
      let resolved = line_count + line + 1;
      if resolved >= 1 && resolved <= line_count {
        Some(resolved as u16)
      } else {
        None
      }
    }
  }
}

fn resolve_named_line_to_u16(line_names: &[Vec<String>], name: &str, idx: i16) -> Option<u16> {
  if idx == 0 {
    return None;
  }
  if idx > 0 {
    let mut count = 0i16;
    for (line_idx, names) in line_names.iter().enumerate() {
      if names.iter().any(|n| n == name) {
        count = count.saturating_add(1);
        if count == idx {
          return u16::try_from(line_idx.saturating_add(1)).ok();
        }
      }
    }
    None
  } else {
    // `idx` counts from the end.
    let target = (idx as i32).saturating_abs() as i16;
    let mut count = 0i16;
    for (line_idx, names) in line_names.iter().enumerate().rev() {
      if names.iter().any(|n| n == name) {
        count = count.saturating_add(1);
        if count == target {
          return u16::try_from(line_idx.saturating_add(1)).ok();
        }
      }
    }
    None
  }
}

fn resolve_named_span_forward(
  line_names: &[Vec<String>],
  name: &str,
  span: u16,
  start_line: u16,
) -> Option<u16> {
  let mut seen = 0u16;
  for (line_idx, names) in line_names.iter().enumerate() {
    let line = u16::try_from(line_idx.saturating_add(1)).ok()?;
    if line <= start_line {
      continue;
    }
    if names.iter().any(|n| n == name) {
      seen = seen.saturating_add(1);
      if seen == span {
        return Some(line);
      }
    }
  }
  None
}

fn resolve_named_span_backward(
  line_names: &[Vec<String>],
  name: &str,
  span: u16,
  end_line: u16,
) -> Option<u16> {
  let mut seen = 0u16;
  for (line_idx, names) in line_names.iter().enumerate().rev() {
    let line = u16::try_from(line_idx.saturating_add(1)).ok()?;
    if line >= end_line {
      continue;
    }
    if names.iter().any(|n| n == name) {
      seen = seen.saturating_add(1);
      if seen == span {
        return Some(line);
      }
    }
  }
  None
}

fn resolve_grid_line_range_from_style(
  start: i32,
  end: i32,
  raw: Option<&str>,
  line_count: Option<u16>,
  line_names: Option<&[Vec<String>]>,
) -> Option<(u16, u16)> {
  let parsed = raw
    .filter(|_| start == 0 || end == 0)
    .map(|raw| parse_grid_line_placement_raw(raw, line_names));

  let start_component: ResolvedGridPlacementComponent<'_> = if start != 0 {
    ResolvedGridPlacementComponent::Line(start)
  } else {
    parsed
      .as_ref()
      .map(|line| grid_placement_component_from_taffy(&line.start))
      .unwrap_or(ResolvedGridPlacementComponent::Auto)
  };
  let end_component: ResolvedGridPlacementComponent<'_> = if end != 0 {
    ResolvedGridPlacementComponent::Line(end)
  } else {
    parsed
      .as_ref()
      .map(|line| grid_placement_component_from_taffy(&line.end))
      .unwrap_or(ResolvedGridPlacementComponent::Auto)
  };

  use ResolvedGridPlacementComponent as Comp;
  match (start_component, end_component) {
    (Comp::Line(start), Comp::Line(end)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::Span(span)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = start.checked_add(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::Span(span), Comp::Line(end)) => {
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      let start = end.checked_sub(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedLine(start_name, start_idx), Comp::Line(end)) => {
      let start = resolve_named_line_to_u16(line_names?, start_name, start_idx)?;
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::NamedLine(end_name, end_idx)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = resolve_named_line_to_u16(line_names?, end_name, end_idx)?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedLine(start_name, start_idx), Comp::NamedLine(end_name, end_idx)) => {
      let start = resolve_named_line_to_u16(line_names?, start_name, start_idx)?;
      let end = resolve_named_line_to_u16(line_names?, end_name, end_idx)?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedLine(start_name, start_idx), Comp::Span(span)) => {
      let start = resolve_named_line_to_u16(line_names?, start_name, start_idx)?;
      let end = start.checked_add(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::Span(span), Comp::NamedLine(end_name, end_idx)) => {
      let end = resolve_named_line_to_u16(line_names?, end_name, end_idx)?;
      let start = end.checked_sub(span)?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::NamedSpan(name, span)) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = line_names
        .and_then(|names| resolve_named_span_forward(names, name, span, start))
        .or_else(|| start.checked_add(span))?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedSpan(name, span), Comp::Line(end)) => {
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      let start = line_names
        .and_then(|names| resolve_named_span_backward(names, name, span, end))
        .or_else(|| end.checked_sub(span))?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedLine(start_name, start_idx), Comp::NamedSpan(name, span)) => {
      let start = resolve_named_line_to_u16(line_names?, start_name, start_idx)?;
      let end = resolve_named_span_forward(line_names?, name, span, start)
        .or_else(|| start.checked_add(span))?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedSpan(name, span), Comp::NamedLine(end_name, end_idx)) => {
      let end = resolve_named_line_to_u16(line_names?, end_name, end_idx)?;
      let start = resolve_named_span_backward(line_names?, name, span, end)
        .or_else(|| end.checked_sub(span))?;
      (end > start).then_some((start, end))
    }
    (Comp::Line(start), Comp::Auto) => {
      let start = resolve_css_grid_line_to_u16(start, line_count)?;
      let end = start.checked_add(1)?;
      (end > start).then_some((start, end))
    }
    (Comp::Auto, Comp::Line(end)) => {
      let end = resolve_css_grid_line_to_u16(end, line_count)?;
      let start = end.checked_sub(1)?;
      (end > start).then_some((start, end))
    }
    (Comp::NamedLine(start_name, start_idx), Comp::Auto) => {
      let start = resolve_named_line_to_u16(line_names?, start_name, start_idx)?;
      let end = start.checked_add(1)?;
      (end > start).then_some((start, end))
    }
    (Comp::Auto, Comp::NamedLine(end_name, end_idx)) => {
      let end = resolve_named_line_to_u16(line_names?, end_name, end_idx)?;
      let start = end.checked_sub(1)?;
      (end > start).then_some((start, end))
    }
    // Absolutely positioned items with both edges auto (or with span-based placement components
    // that rely on auto-placement) still need a deterministic static position. Per CSS Grid, the
    // static position is defined as if the item were the sole grid item, so default auto
    // placements to the first track.
    (Comp::Auto, Comp::Auto) => Some((1, 2)),
    (Comp::Auto, Comp::Span(span)) | (Comp::Span(span), Comp::Auto) => {
      let start = 1u16;
      let end = start.checked_add(span)?;
      Some((start, end))
    }
    (Comp::Auto, Comp::NamedSpan(name, span)) | (Comp::NamedSpan(name, span), Comp::Auto) => {
      let start = 1u16;
      let end = line_names
        .and_then(|names| resolve_named_span_forward(names, name, span, start))
        .or_else(|| start.checked_add(span))?;
      (end > start).then_some((start, end))
    }
    _ => None,
  }
}

#[derive(Clone, Copy)]
struct BaselineItem {
  idx: usize,
  area_start: f32,
  area_end: f32,
  baseline: f32,
  start: f32,
  size: f32,
}

fn translate_along_axis(
  fragment: &mut FragmentNode,
  axis: Axis,
  delta: f32,
  deadline_counter: &mut usize,
) -> Result<(), LayoutError> {
  if delta == 0.0 {
    return Ok(());
  }
  let delta_point = match axis {
    Axis::Horizontal => Point::new(delta, 0.0),
    Axis::Vertical => Point::new(0.0, delta),
  };
  translate_fragment_tree(fragment, delta_point, deadline_counter)
}

fn is_css_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_css_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_css_ascii_whitespace)
}

fn split_css_ascii_whitespace(value: &str) -> impl Iterator<Item = &str> {
  value
    .split(is_css_ascii_whitespace)
    .filter(|part| !part.is_empty())
}

fn parse_grid_line_placement_raw(
  raw: &str,
  line_names: Option<&[Vec<String>]>,
) -> Line<TaffyGridPlacement<String>> {
  // The `grid-row`/`grid-column` shorthands have a special case: when the second component is
  // omitted and the first component is a `<custom-ident>`, the end component is set to the same
  // `<custom-ident>` instead of `auto`.
  //
  // Spec: https://www.w3.org/TR/css-grid-2/#placement-shorthands
  fn custom_ident_component(token: &str) -> Option<&str> {
    let token = trim_css_ascii_whitespace(token);
    if token.is_empty() || token.eq_ignore_ascii_case("auto") {
      return None;
    }
    // Shorthand expansion only applies to a single `<custom-ident>` token. Anything with whitespace
    // (e.g. `span 2`, `foo 2`) uses the normal grid-line grammar.
    if token.chars().any(is_css_ascii_whitespace) {
      return None;
    }
    // Numeric line references should keep the default `/ auto` behaviour.
    if token.parse::<i32>().is_ok() {
      return None;
    }
    // `span` is reserved by the `<grid-line>` grammar.
    if token.eq_ignore_ascii_case("span") {
      return None;
    }
    Some(token)
  }

  fn line_names_contain(line_names: &[Vec<String>], name: &str) -> bool {
    line_names
      .iter()
      .any(|names| names.iter().any(|n| n == name))
  }

  fn maybe_resolve_named_area_edge(
    token: &str,
    edge_suffix: &str,
    line_names: Option<&[Vec<String>]>,
  ) -> String {
    let Some(ident) = custom_ident_component(token) else {
      return token.to_string();
    };
    let Some(line_names) = line_names else {
      return ident.to_string();
    };

    // `<grid-line>` values specified as a bare `<custom-ident>` first attempt to match the
    // corresponding edge of a named grid area by looking for implicit line names of the form
    // `foo-start`/`foo-end`.
    //
    // Spec: https://www.w3.org/TR/css-grid-2/#grid-placement-slot
    let candidate = format!("{ident}-{edge_suffix}");
    if line_names_contain(line_names, &candidate) {
      candidate
    } else {
      ident.to_string()
    }
  }

  let raw = trim_css_ascii_whitespace(raw);
  if raw.is_empty() {
    return Line {
      start: TaffyGridPlacement::Auto,
      end: TaffyGridPlacement::Auto,
    };
  }

  let mut parts = raw.splitn(2, '/').map(trim_css_ascii_whitespace);
  let start_str = parts.next().unwrap_or("auto");
  let end_part = parts.next();

  let end_str = end_part.unwrap_or_else(|| {
    if custom_ident_component(start_str).is_some() {
      start_str
    } else {
      "auto"
    }
  });

  let start = maybe_resolve_named_area_edge(start_str, "start", line_names);
  let end = maybe_resolve_named_area_edge(end_str, "end", line_names);

  Line {
    start: parse_grid_line_component(&start),
    end: parse_grid_line_component(&end),
  }
}

fn normalize_grid_placement_conflicts(line: &mut Line<TaffyGridPlacement<String>>) {
  // Grid Placement Conflict Handling:
  // https://www.w3.org/TR/css-grid-2/#grid-placement-errors
  //
  // If placement contains two lines and they resolve to the same line, drop the end line so the
  // item defaults to a span of 1.
  let start_is_line = matches!(
    line.start,
    TaffyGridPlacement::Line(_) | TaffyGridPlacement::NamedLine(_, _)
  );
  let end_is_line = matches!(
    line.end,
    TaffyGridPlacement::Line(_) | TaffyGridPlacement::NamedLine(_, _)
  );
  if start_is_line && end_is_line && line.start == line.end {
    line.end = TaffyGridPlacement::Auto;
  }
}

fn parse_grid_line_component(token: &str) -> TaffyGridPlacement<String> {
  let trimmed = trim_css_ascii_whitespace(token);
  if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
    return TaffyGridPlacement::Auto;
  }

  let parts: Vec<&str> = split_css_ascii_whitespace(trimmed).collect();
  if parts.is_empty() {
    return TaffyGridPlacement::Auto;
  }

  // Span syntax: span && (<integer> || <custom-ident>) in any order
  //
  // The `span` keyword can appear anywhere within the component, not just as the first token.
  // See: https://www.w3.org/TR/css-grid-2/#typedef-grid-line
  if parts.iter().any(|part| part.eq_ignore_ascii_case("span")) {
    let mut name: Option<String> = None;
    let mut count: Option<u16> = None;

    for part in parts
      .iter()
      .filter(|part| !part.eq_ignore_ascii_case("span"))
    {
      if let Ok(n) = part.parse::<i32>() {
        if n > 0 && count.is_none() {
          count = Some(n as u16);
        }
        continue;
      }

      if name.is_none() {
        name = Some((*part).to_string());
      }
    }

    let count = count.unwrap_or(1).max(1);
    return match name {
      Some(name) => TaffyGridPlacement::NamedSpan(name, count),
      None => TaffyGridPlacement::Span(count),
    };
  }

  // Non-span grammar: <custom-ident>? <integer>? in any order (integer controls the nth occurrence)
  let mut number: Option<i16> = None;
  let mut name: Option<String> = None;
  for part in &parts {
    if number.is_none() {
      if let Ok(n) = part.parse::<i16>() {
        number = Some(n);
        continue;
      }
    }
    if name.is_none() {
      name = Some((*part).to_string());
    }
  }

  match (name, number) {
    (Some(name), Some(idx)) => TaffyGridPlacement::NamedLine(name, idx),
    (Some(name), None) => TaffyGridPlacement::NamedLine(name, 1),
    (None, Some(idx)) => TaffyGridPlacement::Line(idx.into()),
    (None, None) => TaffyGridPlacement::Auto,
  }
}

impl Default for GridFormattingContext {
  fn default() -> Self {
    Self::new()
  }
}

impl FormattingContext for GridFormattingContext {
  fn layout(
    &self,
    box_node: &BoxNode,
    constraints: &LayoutConstraints,
  ) -> Result<FragmentNode, LayoutError> {
    if crate::layout::auto_scrollbars::should_bypass(box_node) {
      debug_assert!(
        matches!(
          box_node.formatting_context(),
          Some(FormattingContextType::Grid)
        ),
        "GridFormattingContext must only layout grid containers",
      );
      let _profile = layout_timer(LayoutKind::Grid);
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      let mut has_running_children = false;
      let mut has_running_deadline_counter = 0usize;
      for child in &box_node.children {
        check_layout_deadline(&mut has_running_deadline_counter)?;
        if child.style.running_position.is_some() {
          has_running_children = true;
          break;
        }
      }
      let trace_grid_layout =
        crate::debug::runtime::runtime_toggles().truthy("FASTR_TRACE_GRID_LAYOUT");
      let grid_trace_start = trace_grid_layout.then(std::time::Instant::now);

      // Partition children into running, in-flow vs. out-of-flow positioned.
      let mut in_flow_children: Vec<(usize, &BoxNode)> = Vec::new();
      let mut positioned_children: Vec<&BoxNode> = Vec::new();
      let mut running_children: Vec<(usize, BoxNode)> = Vec::new();
      let mut deadline_counter = 0usize;
      let mut child_has_subgrid = false;
      let mut in_flow_children_need_sort = false;
      let mut last_in_flow_order: Option<i32> = None;
      for (idx, child) in box_node.children.iter().enumerate() {
        check_layout_deadline(&mut deadline_counter)?;
        if child.style.running_position.is_some() {
          // Running elements do not participate in grid layout; instead, capture a snapshot at the
          // position the element would have occupied in flow.
          running_children.push((idx, child.clone()));
          continue;
        }
        match child.style.position {
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed => {
            positioned_children.push(child);
          }
          _ => {
            if child.style.grid_row_subgrid || child.style.grid_column_subgrid {
              child_has_subgrid = true;
            }
            if let Some(prev) = last_in_flow_order {
              if child.style.order < prev {
                in_flow_children_need_sort = true;
              }
            }
            last_in_flow_order = Some(child.style.order);
            in_flow_children.push((idx, child))
          }
        }
      }
      if in_flow_children_need_sort {
        // `check_layout_deadline()` is periodic; ensure we still perform a definite check before doing
        // potentially expensive sort work.
        if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
          return Err(LayoutError::Timeout { elapsed });
        }
        in_flow_children.sort_by(|(a_idx, a), (b_idx, b)| {
          a.style
            .order
            .cmp(&b.style.order)
            .then_with(|| a_idx.cmp(b_idx))
        });
        if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
          return Err(LayoutError::Timeout { elapsed });
        }
      }
      let in_flow_children: Vec<&BoxNode> = in_flow_children
        .into_iter()
        .map(|(_, child)| child)
        .collect();

      let intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      constraints,
      true,
    )?;

    let style_override = style_override_for(box_node.id);
    // Avoid short-circuiting grid layout with higher-level fragment caches when collecting Taffy
    // usage stats; we want to observe the underlying template reuse behavior.
    // Do not cache grid containers that contain running elements: running anchors are synthesized
    // based on in-flow position, so reusing cached fragments can capture the wrong snapshot.
    let disable_global_layout_cache = has_running_children || taffy_counters_enabled();
    if !disable_global_layout_cache {
      if let Some(cached) = layout_cache_lookup(
        box_node,
        FormattingContextType::Grid,
        constraints,
        self.factory.viewport_scroll(),
        self.viewport_size,
        self.nearest_positioned_cb,
        self.nearest_fixed_cb,
      ) {
        drop(intrinsic_keyword_overrides);
        return Ok(cached);
      }
    }

    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    // Grid containers that establish positioned containing blocks need to thread that containing
    // block into the factories used for laying out descendants. Otherwise positioned descendants
    // (especially those nested inside in-flow grid items) can incorrectly double-apply the grid
    // item placement offset or fall back to an ancestor containing block.
    let mut ctx = self.clone();
    let establishes_abs_cb = style.establishes_abs_containing_block();
    let establishes_fixed_cb = style.establishes_fixed_containing_block();
    if establishes_abs_cb || establishes_fixed_cb {
      let percentage_base = constraints
        .inline_percentage_base
        .or_else(|| constraints.width())
        .unwrap_or(self.viewport_size.width);
      let padding_left = ctx.resolve_length_for_width(style.padding_left, percentage_base, style);
      let padding_right = ctx.resolve_length_for_width(style.padding_right, percentage_base, style);
      let padding_top = ctx.resolve_length_for_width(style.padding_top, percentage_base, style);
      let padding_bottom =
        ctx.resolve_length_for_width(style.padding_bottom, percentage_base, style);
      let border_left =
        ctx.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
      let border_top =
        ctx.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
      let padding_origin = Point::new(border_left, border_top);
      let content_width = constraints.width().unwrap_or(0.0).max(0.0);
      let content_height = constraints.height().unwrap_or(0.0).max(0.0);
      let padding_size = Size::new(
        content_width + padding_left + padding_right,
        content_height + padding_top + padding_bottom,
      );
      let padding_rect = Rect::new(padding_origin, padding_size);
      let padding_cb =
        crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
          padding_rect,
          ctx.viewport_size,
          Some(padding_rect.size.width),
          constraints.height().map(|_| padding_rect.size.height),
        )
        .with_writing_mode_and_direction(style.writing_mode, style.direction);
      if establishes_abs_cb {
        ctx.nearest_positioned_cb = padding_cb;
        ctx.factory = ctx.factory.with_positioned_cb(padding_cb);
      }
      if establishes_fixed_cb {
        ctx.nearest_fixed_cb = padding_cb;
        ctx.factory = ctx.factory.with_fixed_cb(padding_cb);
      }
    }
    let ctx = &ctx;

    let has_subgrid = style.grid_row_subgrid || style.grid_column_subgrid;

    let cacheable_tree = !(has_subgrid || child_has_subgrid);
    let mut taffy = CachedTaffyTree::new(TaffyAdapterKind::Grid, box_node.id, cacheable_tree);
    let mut positioned_children_map: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> =
      FxHashMap::default();

    // Build Taffy tree from in-flow children
    let root_id = ctx.build_or_update_taffy_tree_children_cached(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      constraints,
      &mut positioned_children_map,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      // Patch the root style in-place so we can avoid deep-cloning box subtrees when sizing hints
      // are temporarily overridden (e.g. flex/grid intrinsic probes or final item sizing).
      let mut style_deadline_counter = 0usize;
      let simple_grid = positioned_children.is_empty()
        && ctx.is_simple_grid(
          style_override,
          &in_flow_children,
          &mut style_deadline_counter,
        )?;
      let override_taffy_style = self.convert_style(
        style_override,
        None,
        None,
        None,
        simple_grid,
        true,
        box_node.is_replaced(),
      );
      let needs_override_update = match taffy.style(root_id) {
        Ok(existing) => existing != &override_taffy_style,
        Err(_) => true,
      };
      if needs_override_update {
        taffy
          .set_style(root_id, override_taffy_style)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }
    }

    if trace_grid_layout {
        let selector = box_node
          .debug_info
          .as_ref()
          .map(|d| d.to_selector())
          .unwrap_or_else(|| "<anon>".to_string());
        eprintln!(
        "[grid-layout] start id={} in_flow_children={} selector={} constraints={{avail_w={:?} avail_h={:?} used_w={:?} used_h={:?} inline_base={:?} block_base={:?}}} style={{writing_mode={:?} direction={:?} width={:?} width_kw={:?} height={:?} height_kw={:?}}} fragmentainer_hint={:?}",
        box_node.id,
        in_flow_children.len(),
        selector,
        constraints.available_width,
        constraints.available_height,
        constraints.used_border_box_width,
        constraints.used_border_box_height,
        constraints.inline_percentage_base,
        constraints.block_percentage_base,
        style.writing_mode,
        style.direction,
        style.width,
        style.width_keyword,
        style.height,
        style.height_keyword,
        fragmentainer_block_size_hint(),
      );
      }

      // Block-level grid containers with `width: auto` should stretch to the available inline size.
      // Taffy treats `size: auto` as shrink-to-fit in some grid cases, so force a definite width
      // when we have one from the parent constraints.
      if let CrateAvailableSpace::Definite(outer_width) = constraints.available_width {
        if outer_width.is_finite()
          && physical_width_is_auto(style)
          && box_node.is_block_level()
          && crate::style::inline_axis_is_horizontal(style.writing_mode)
        {
          if let Ok(existing) = taffy.style(root_id) {
            if existing.size.width.is_auto() {
              let mut updated = existing.clone();
              let base_width = constraints.inline_percentage_base.unwrap_or(outer_width);
              let border_box_width = outer_width.max(0.0);
              updated.size.width = Dimension::length(ctx.border_box_to_taffy_style_size(
                border_box_width,
                style,
                Axis::Horizontal,
                base_width,
              ));
              taffy
                .set_style(root_id, updated)
                .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
            }
          }
        }
      }

      // If a parent layout mode (flex/grid) already resolved a definite used border-box size for
      // this grid item, force that size on the root node without cloning/mutating styles.
      if constraints.used_border_box_width.is_some() || constraints.used_border_box_height.is_some()
      {
        if let Ok(existing) = taffy.style(root_id) {
          let mut updated = existing.clone();
          let mut changed = false;
          let base_width = constraints
            .inline_percentage_base
            .or_else(|| constraints.width())
            .unwrap_or(ctx.viewport_size.width);
          if let Some(w) = constraints
            .used_border_box_width
            .filter(|w| w.is_finite() && *w >= 0.0)
          {
            updated.size.width = Dimension::length(ctx.border_box_to_taffy_style_size(
              w,
              style,
              Axis::Horizontal,
              base_width,
            ));
            changed = true;
          }
          if let Some(h) = constraints
            .used_border_box_height
            .filter(|h| h.is_finite() && *h >= 0.0)
          {
            updated.size.height = Dimension::length(ctx.border_box_to_taffy_style_size(
              h,
              style,
              Axis::Vertical,
              base_width,
            ));
            changed = true;
          }
          if changed {
            taffy
              .set_style(root_id, updated)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }
      }

      // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block height
      // is not definite (i.e. it depends on content height) for in-flow elements.
      //
      // Grid layout commonly runs inside block-flow where the block-size is indefinite. Taffy
      // resolves percentage heights against the available space it receives, so without this
      // normalization `height:100%` can incorrectly expand to viewport/probe sizes when the
      // containing block height is actually indefinite.
      if constraints.used_border_box_height.is_none()
        && constraints.height().is_none()
        && style
          .height
          .as_ref()
          .is_some_and(crate::style::values::Length::has_percentage)
        && !matches!(
          style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      {
        if let Ok(existing) = taffy.style(root_id) {
          if existing.size.height.tag() == taffy::style::CompactLength::PERCENT_TAG {
            let mut updated = existing.clone();
            updated.size.height = Dimension::auto();
            taffy
              .set_style(root_id, updated)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }
      }

      // `width/height: fit-content` clamps the used size between the box's min- and max-content
      // contributions. When this grid formatting context is invoked without a parent precomputing
      // a used border-box size (notably at the root of the layout tree), resolve it here so Taffy
      // runs track sizing against the correct container size.
      if (constraints.used_border_box_width.is_none()
        && matches!(
          style.width_keyword,
          Some(IntrinsicSizeKeyword::FitContent { .. })
        ))
        || (constraints.used_border_box_height.is_none()
          && matches!(
            style.height_keyword,
            Some(IntrinsicSizeKeyword::FitContent { .. })
          ))
      {
        if let Ok(existing) = taffy.style(root_id) {
          let mut updated = existing.clone();
          let mut changed = false;
          let base_width = constraints
            .inline_percentage_base
            .or_else(|| constraints.width())
            .unwrap_or(ctx.viewport_size.width);

          if constraints.used_border_box_width.is_none() {
            if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.width_keyword {
              match self.resolve_root_fit_content_border_box_size(
                box_node,
                style,
                constraints,
                Axis::Horizontal,
                limit,
              ) {
                Ok(Some(width)) if width.is_finite() && width >= 0.0 => {
                  updated.size.width = Dimension::length(ctx.border_box_to_taffy_style_size(
                    width,
                    style,
                    Axis::Horizontal,
                    base_width,
                  ));
                  changed = true;
                }
                Ok(_) => {}
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => {}
              }
            }
          }

          if constraints.used_border_box_height.is_none() {
            if let Some(IntrinsicSizeKeyword::FitContent { limit }) = style.height_keyword {
              match self.resolve_root_fit_content_border_box_size(
                box_node,
                style,
                constraints,
                Axis::Vertical,
                limit,
              ) {
                Ok(Some(height)) if height.is_finite() && height >= 0.0 => {
                  updated.size.height = Dimension::length(ctx.border_box_to_taffy_style_size(
                    height,
                    style,
                    Axis::Vertical,
                    base_width,
                  ));
                  changed = true;
                }
                Ok(_) => {}
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => {}
              }
            }
          }

          if changed {
            taffy
              .set_style(root_id, updated)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }
      }

      // Resolve intrinsic sizing keywords used in root min/max size properties. The cached Taffy
      // template represents these keywords as `auto`, but the root node is never resolved via the
      // per-item keyword path.
      if (constraints.used_border_box_width.is_none()
        && (style.min_width_keyword.is_some() || style.max_width_keyword.is_some()))
        || (constraints.used_border_box_height.is_none()
          && (style.min_height_keyword.is_some() || style.max_height_keyword.is_some()))
      {
        if let Ok(existing) = taffy.style(root_id) {
          let mut updated = existing.clone();
          let mut changed = false;
          let base_width = constraints
            .inline_percentage_base
            .or_else(|| constraints.width())
            .unwrap_or(ctx.viewport_size.width);

          let keyword_to_mode = |kw: IntrinsicSizeKeyword| match kw {
            IntrinsicSizeKeyword::MinContent => Some(IntrinsicSizingMode::MinContent),
            IntrinsicSizeKeyword::MaxContent => Some(IntrinsicSizingMode::MaxContent),
            IntrinsicSizeKeyword::FillAvailable => None,
            IntrinsicSizeKeyword::FitContent { .. } => None,
            IntrinsicSizeKeyword::CalcSize(_) => None,
          };

          let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
          let run_with_override = |override_style: Arc<ComputedStyle>,
                                   f: &dyn Fn(&BoxNode) -> Result<f32, LayoutError>|
           -> Result<f32, LayoutError> {
            if box_node.id() != 0 {
              crate::layout::style_override::with_style_override(
                box_node.id(),
                override_style,
                || f(box_node),
              )
            } else {
              let mut cloned = box_node.clone();
              cloned.style = override_style;
              f(&cloned)
            }
          };

          let intrinsic_physical_width =
            |node: &BoxNode, mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
              if inline_is_horizontal {
                self.compute_intrinsic_inline_size(node, mode)
              } else {
                self.compute_intrinsic_block_size(node, mode)
              }
            };
          let intrinsic_physical_height =
            |node: &BoxNode, mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
              if inline_is_horizontal {
                self.compute_intrinsic_block_size(node, mode)
              } else {
                self.compute_intrinsic_inline_size(node, mode)
              }
            };

          if constraints.used_border_box_width.is_none() {
            let width_override_needed =
              style.min_width_keyword.is_some() || style.max_width_keyword.is_some();
            if width_override_needed {
              let mut override_style: ComputedStyle = (*style).clone();
              override_style.width = None;
              override_style.width_keyword = None;
              override_style.min_width = None;
              override_style.min_width_keyword = None;
              override_style.max_width = None;
              override_style.max_width_keyword = None;
              let override_style = Arc::new(override_style);

              if style.min_width.is_none() {
                if let Some(mode) = style.min_width_keyword.and_then(keyword_to_mode) {
                  match run_with_override(override_style.clone(), &|node| {
                    intrinsic_physical_width(node, mode)
                  }) {
                    Ok(border_box) if border_box.is_finite() => {
                      updated.min_size.width =
                        Dimension::length(ctx.border_box_to_taffy_style_size(
                          border_box.max(0.0),
                          style,
                          Axis::Horizontal,
                          base_width,
                        ));
                      changed = true;
                    }
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {}
                    _ => {}
                  }
                }
              }
              if style.max_width.is_none() {
                if let Some(mode) = style.max_width_keyword.and_then(keyword_to_mode) {
                  match run_with_override(override_style.clone(), &|node| {
                    intrinsic_physical_width(node, mode)
                  }) {
                    Ok(border_box) if border_box.is_finite() => {
                      updated.max_size.width =
                        Dimension::length(ctx.border_box_to_taffy_style_size(
                          border_box.max(0.0),
                          style,
                          Axis::Horizontal,
                          base_width,
                        ));
                      changed = true;
                    }
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {}
                    _ => {}
                  }
                }
              }
            }
          }

          if constraints.used_border_box_height.is_none() {
            let height_override_needed =
              style.min_height_keyword.is_some() || style.max_height_keyword.is_some();
            if height_override_needed {
              let mut override_style: ComputedStyle = (*style).clone();
              override_style.height = None;
              override_style.height_keyword = None;
              override_style.min_height = None;
              override_style.min_height_keyword = None;
              override_style.max_height = None;
              override_style.max_height_keyword = None;
              let override_style = Arc::new(override_style);

              if style.min_height.is_none() {
                if let Some(mode) = style.min_height_keyword.and_then(keyword_to_mode) {
                  match run_with_override(override_style.clone(), &|node| {
                    intrinsic_physical_height(node, mode)
                  }) {
                    Ok(border_box) if border_box.is_finite() => {
                      updated.min_size.height =
                        Dimension::length(ctx.border_box_to_taffy_style_size(
                          border_box.max(0.0),
                          style,
                          Axis::Vertical,
                          base_width,
                        ));
                      changed = true;
                    }
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {}
                    _ => {}
                  }
                }
              }
              if style.max_height.is_none() {
                if let Some(mode) = style.max_height_keyword.and_then(keyword_to_mode) {
                  match run_with_override(override_style.clone(), &|node| {
                    intrinsic_physical_height(node, mode)
                  }) {
                    Ok(border_box) if border_box.is_finite() => {
                      updated.max_size.height =
                        Dimension::length(ctx.border_box_to_taffy_style_size(
                          border_box.max(0.0),
                          style,
                          Axis::Vertical,
                          base_width,
                        ));
                      changed = true;
                    }
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {}
                    _ => {}
                  }
                }
              }
            }
          }

          if changed {
            taffy
              .set_style(root_id, updated)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
        }
      }

      ctx.patch_root_calc_percentage_sizing_and_edges(&mut taffy, root_id, style, constraints)?;
      ctx.patch_root_calc_percentage_tracks(&mut taffy, root_id, style, constraints)?;
      ctx.patch_root_percentage_block_size(&mut taffy, root_id, style, constraints)?;

      let mut available_space = taffy_available_space_for_grid_container(style, constraints);
      // When the grid container has a definite physical width/height from its own `width`/`height`
      // properties, but the parent provides indefinite available space (common for the block axis in
      // normal flow), forward that definite size into Taffy as available space so `fr` tracks resolve
      // and stretched items can establish a definite containing block for percentage sizing.
      if matches!(constraints.available_width, CrateAvailableSpace::Indefinite)
        && constraints.used_border_box_width.is_none()
        && matches!(
          available_space.width,
          taffy::style::AvailableSpace::MaxContent
        )
      {
        if let Ok(existing) = taffy.style(root_id) {
          if let Some(width) = existing.size.width.into_option() {
            if width.is_finite() && width >= 0.0 {
              available_space.width = taffy::style::AvailableSpace::Definite(width);
            }
          }
        }
      }
      if matches!(
        constraints.available_height,
        CrateAvailableSpace::Indefinite
      ) && constraints.used_border_box_height.is_none()
        && matches!(
          available_space.height,
          taffy::style::AvailableSpace::MaxContent
        )
      {
        if let Ok(existing) = taffy.style(root_id) {
          if let Some(height) = existing.size.height.into_option() {
            if height.is_finite() && height >= 0.0 {
              available_space.height = taffy::style::AvailableSpace::Definite(height);
            }
          }
        }
      }
      if trace_grid_layout {
        eprintln!(
          "[grid-layout] available_space id={} width={:?} height={:?}",
          box_node.id, available_space.width, available_space.height
        );
      }
      // If the grid container itself is sized with intrinsic keywords, map the available space we
      // pass into Taffy so it performs the corresponding intrinsic probe. Fit-content needs a
      // definite available size (fill-available), so only map min-/max-content here.
      if constraints.used_border_box_width.is_none() {
        match style.width_keyword {
          Some(IntrinsicSizeKeyword::MinContent) => {
            available_space.width = taffy::style::AvailableSpace::MinContent
          }
          Some(IntrinsicSizeKeyword::MaxContent) => {
            available_space.width = taffy::style::AvailableSpace::MaxContent
          }
          _ => {}
        }
      }
      if constraints.used_border_box_height.is_none() {
        match style.height_keyword {
          Some(IntrinsicSizeKeyword::MinContent) => {
            available_space.height = taffy::style::AvailableSpace::MinContent
          }
          Some(IntrinsicSizeKeyword::MaxContent) => {
            available_space.height = taffy::style::AvailableSpace::MaxContent
          }
          _ => {}
        }
      }

      let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
      // Render pipeline always installs a deadline guard (even when disabled), so only enable
      // the Taffy cancellation path when the active deadline is actually configured.
      let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
        .filter(|deadline| deadline.is_enabled())
        .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> =
        FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> =
        FxHashMap::with_capacity_and_hasher(in_flow_children.len(), Default::default());
      let mut auto_item_nodes: Vec<(*const BoxNode, TaffyNodeId)> = Vec::new();
      if !in_flow_children.is_empty() {
        let mut auto_deadline_counter = 0usize;
        let mut stack = vec![root_id];
        while let Some(node_id) = stack.pop() {
          check_layout_deadline(&mut auto_deadline_counter)?;
          let child_count = taffy.child_count(node_id);
          if child_count == 0 && node_id != root_id {
            if let Some(ptr) = taffy.get_node_context(node_id).copied() {
              let node = unsafe { &*ptr };
              if matches!(
                node.style.content_visibility,
                crate::style::types::ContentVisibility::Auto
              ) && self.content_visibility_auto_has_definite_placeholder(node)
              {
                auto_item_nodes.push((ptr, node_id));
              }
            }
          } else {
            for idx in 0..child_count {
              stack.push(taffy.get_child_id(node_id, idx));
            }
          }
        }
      }
      let auto_item_count = auto_item_nodes.len();
      let auto_all_nodes: FxHashSet<*const BoxNode> =
        auto_item_nodes.iter().map(|(ptr, _)| *ptr).collect();
      let mut auto_unskipped_nodes: FxHashSet<*const BoxNode> = FxHashSet::default();
      let max_passes = if auto_item_count == 0 {
        1
      } else {
        GRID_CONTENT_VISIBILITY_AUTO_MAX_PASSES
      };

      let parent_inline_base = constraints.inline_percentage_base;
      for pass_idx in 0..max_passes {
        if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
          return Err(LayoutError::Timeout { elapsed });
        }
        let is_last_pass = pass_idx + 1 == max_passes;
        if is_last_pass
          && auto_item_count > 0
          && auto_unskipped_nodes.len() < auto_all_nodes.len()
          && pass_idx > 0
        {
          // If we hit the pass cap without reaching a stable viewport set, fall back to fully
          // laying out all `content-visibility:auto` items so in-viewport content is never skipped.
          auto_unskipped_nodes = auto_all_nodes.clone();
          for (_, node_id) in auto_item_nodes.iter() {
            taffy
              .mark_dirty(*node_id)
              .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
          }
          taffy
            .mark_dirty(root_id)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }

        measure_cache.clear();
        record_taffy_invocation(TaffyAdapterKind::Grid);
        let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
        let auto_unskipped_for_pass = &auto_unskipped_nodes;

        let compute_result = {
          let this = self.clone();
          let cache = &mut measure_cache;
          let measured = &mut measured_fragments;
          let measured_keys = &mut measured_node_keys;
          taffy.compute_layout_with_measure_and_cancel(
            root_id,
            available_space,
            move |known_dimensions,
                  available_space,
                  node_id,
                  node_context,
                  taffy_style: &taffy::style::Style| {
              if taffy_perf_enabled {
                record_taffy_measure_call(TaffyAdapterKind::Grid);
              }
              let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
                known.unwrap_or(match avail_dim {
                  taffy::style::AvailableSpace::Definite(v) => v,
                  _ => 0.0,
                })
              };

              if node_id == root_id {
                let outer_width = fallback_size(known_dimensions.width, available_space.width);
                let outer_height = known_dimensions.height.unwrap_or(0.0);
                let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
                  return taffy::tree::MeasureOutput::from_size(taffy::geometry::Size {
                    width: outer_width,
                    height: outer_height,
                  });
                };
                let root_box_node = unsafe { &*node_ptr };
                let percentage_base = outer_width;
                let padding_left = this.resolve_length_for_width(
                  root_box_node.style.padding_left,
                  percentage_base,
                  &root_box_node.style,
                );
                let padding_right = this.resolve_length_for_width(
                  root_box_node.style.padding_right,
                  percentage_base,
                  &root_box_node.style,
                );
                let padding_top = this.resolve_length_for_width(
                  root_box_node.style.padding_top,
                  percentage_base,
                  &root_box_node.style,
                );
                let padding_bottom = this.resolve_length_for_width(
                  root_box_node.style.padding_bottom,
                  percentage_base,
                  &root_box_node.style,
                );
                let border_left = this.resolve_length_for_width(
                  root_box_node.style.used_border_left_width(),
                  percentage_base,
                  &root_box_node.style,
                );
                let border_right = this.resolve_length_for_width(
                  root_box_node.style.used_border_right_width(),
                  percentage_base,
                  &root_box_node.style,
                );
                let border_top = this.resolve_length_for_width(
                  root_box_node.style.used_border_top_width(),
                  percentage_base,
                  &root_box_node.style,
                );
                let border_bottom = this.resolve_length_for_width(
                  root_box_node.style.used_border_bottom_width(),
                  percentage_base,
                  &root_box_node.style,
                );
                let content_width =
                  (outer_width - padding_left - padding_right - border_left - border_right)
                    .max(0.0);
                let content_height =
                  (outer_height - padding_top - padding_bottom - border_top - border_bottom)
                    .max(0.0);
                return taffy::tree::MeasureOutput::from_size(taffy::geometry::Size {
                  width: content_width,
                  height: content_height,
                });
              }

              let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
                return taffy::tree::MeasureOutput::ZERO;
              };
              this.measure_grid_item(
                node_ptr,
                node_id,
                known_dimensions,
                available_space,
                parent_inline_base,
                style,
                constraints,
                taffy_style,
                auto_unskipped_for_pass,
                &this.factory,
                cache,
                measured,
                measured_keys,
              )
            },
            cancel.clone(),
            TAFFY_ABORT_CHECK_STRIDE,
          )
        };
        if let Some(start) = taffy_compute_start {
          record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
        }
        compute_result.map_err(|e| match e {
          taffy::TaffyError::LayoutAborted => match active_deadline() {
            Some(deadline) => LayoutError::Timeout {
              elapsed: deadline.elapsed(),
            },
            None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
          },
          _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
        })?;

        if auto_item_count == 0 || is_last_pass {
          break;
        }

        let mut changed = false;
        for (ptr, node_id) in auto_item_nodes.iter().copied() {
          if auto_unskipped_nodes.contains(&ptr) {
            continue;
          }
          let mut top = 0.0;
          let mut current = node_id;
          while current != root_id {
            let layout = taffy.layout(current).map_err(|e| {
              LayoutError::MissingContext(format!("Failed to get Taffy layout: {:?}", e))
            })?;
            top += layout.location.y;
            current = match taffy.parent(current) {
              Some(parent) => parent,
              None => break,
            };
          }
          let node = unsafe { &*ptr };
          let should_unskip = if crate::style::block_axis_is_horizontal(node.style.writing_mode) {
            true
          } else {
            top < self.viewport_size.height
          };
          if should_unskip {
            auto_unskipped_nodes.insert(ptr);
            changed = true;
          }
        }

        if !changed {
          break;
        }

        for (_, node_id) in auto_item_nodes.iter() {
          taffy
            .mark_dirty(*node_id)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
        taffy
          .mark_dirty(root_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }

      if let Some(start) = grid_trace_start {
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        match taffy.layout(root_id) {
          Ok(layout) => {
            eprintln!(
              "[grid-layout] done id={} ms={elapsed_ms:.2} size=({:.2}x{:.2})",
              box_node.id, layout.size.width, layout.size.height
            );
          }
          Err(_) => {
            eprintln!("[grid-layout] done id={} ms={elapsed_ms:.2}", box_node.id);
          }
        }
      }

      // Convert back to FragmentNode tree and layout each in-flow child using its formatting context.
      let auto_unskipped = (auto_item_count > 0).then_some(&auto_unskipped_nodes);
      let mut deadline_counter = 0usize;
      let mut fragment = if !has_subgrid && !child_has_subgrid {
        if let Some(result) = ctx.try_parallel_root_children_conversion(
          &taffy,
          root_id,
          box_node,
          constraints,
          &in_flow_children,
          auto_unskipped,
          &mut measured_fragments,
          &measured_node_keys,
        ) {
          result?
        } else {
          ctx.convert_to_fragments(
            &taffy,
            root_id,
            root_id,
            &constraints,
            None,
            None,
            None,
            auto_unskipped,
            &mut measured_fragments,
            &measured_node_keys,
            &positioned_children_map,
            &mut deadline_counter,
          )?
        }
      } else {
        ctx.convert_to_fragments(
          &taffy,
          root_id,
          root_id,
          &constraints,
          None,
          None,
          None,
          auto_unskipped,
          &mut measured_fragments,
          &measured_node_keys,
          &positioned_children_map,
          &mut deadline_counter,
        )?
      };
      if let Some(trace_id) =
        crate::debug::runtime::runtime_toggles().usize("FASTR_TRACE_GRID_TEXT")
      {
        if trace_id == box_node.id {
          fn count_texts(node: &FragmentNode) -> usize {
            let mut count = 0;
            fn walk(node: &FragmentNode, count: &mut usize) {
              if matches!(node.content, FragmentContent::Text { .. }) {
                *count += 1;
              }
              for child in node.children.iter() {
                walk(child, count);
              }
            }
            walk(node, &mut count);
            count
          }
          eprintln!(
            "[grid-text] id={} selector={:?} text_fragments={}",
            box_node.id,
            box_node.debug_info.as_ref().map(|d| d.to_selector()),
            count_texts(&fragment)
          );
        }
      }

      ctx.maybe_trim_auto_block_size_in_scrollable_layout(style, constraints, &mut fragment);

      // Position out-of-flow children against the appropriate containing block.
      if !positioned_children.is_empty() {
        let padding_left = ctx.resolve_length_for_width(
          box_node.style.padding_left,
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let padding_top = ctx.resolve_length_for_width(
          box_node.style.padding_top,
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let padding_right = ctx.resolve_length_for_width(
          box_node.style.padding_right,
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let padding_bottom = ctx.resolve_length_for_width(
          box_node.style.padding_bottom,
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let border_left = ctx.resolve_length_for_width(
          box_node.style.used_border_left_width(),
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let border_top = ctx.resolve_length_for_width(
          box_node.style.used_border_top_width(),
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let border_right = ctx.resolve_length_for_width(
          box_node.style.used_border_right_width(),
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        let border_bottom = ctx.resolve_length_for_width(
          box_node.style.used_border_bottom_width(),
          constraints.width().unwrap_or(0.0),
          &box_node.style,
        );
        // CSS 2.1 §10.1: the containing block for absolute positioned descendants is the padding box
        // of the nearest positioned ancestor, i.e. the rectangle bounded by the padding edge (border
        // box minus borders).
        let padding_origin = crate::geometry::Point::new(border_left, border_top);
        let padding_size = crate::geometry::Size::new(
          (fragment.bounds.width() - border_left - border_right).max(0.0),
          (fragment.bounds.height() - border_top - border_bottom).max(0.0),
        );
        let padding_rect = crate::geometry::Rect::new(padding_origin, padding_size);

        // Percentage sizes/offsets on absolutely positioned boxes resolve against the used size of the
        // containing block, even when the containing block's own height is `auto` (CSS 2.1 §10.5).
        let block_base = Some(padding_rect.size.height);
        let establishes_abs_cb = box_node.style.establishes_abs_containing_block();
        let establishes_fixed_cb = box_node.style.establishes_fixed_containing_block();
        let root_box_id = ensure_box_id(box_node);
        let padding_cb =
          crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
            padding_rect,
            ctx.viewport_size,
            Some(padding_rect.size.width),
            block_base,
          )
          .with_writing_mode_and_direction(box_node.style.writing_mode, box_node.style.direction)
          .with_box_id(Some(root_box_id));
        let mut anchor_index =
          crate::layout::anchor_positioning::AnchorIndex::from_fragments_with_root_scope(
            fragment.children_ref(),
            root_box_id,
            &box_node.style.anchor_scope,
            ctx.viewport_size,
          );
        anchor_index.insert_names_for_box(
          root_box_id,
          &box_node.style.anchor_names,
          crate::layout::anchor_positioning::AnchorBox {
            rect: crate::geometry::Rect::new(crate::geometry::Point::ZERO, fragment.bounds.size),
            writing_mode: box_node.style.writing_mode,
            direction: box_node.style.direction,
          },
        );
        let cb_for_absolute = if establishes_abs_cb {
          padding_cb
        } else {
          ctx.nearest_positioned_cb
        };
        let cb_for_fixed = if establishes_fixed_cb {
          padding_cb
        } else {
          ctx.nearest_fixed_cb
        };

        // `FormattingContextFactory::with_fixed_cb` / `with_positioned_cb` reset the per-factory cached
        // formatting contexts store. Build factory variants once so multiple positioned children can
        // reuse cached formatting contexts instead of rebuilding detached ones per child.
        let positioned_factory = ctx.factory.clone();
        let positioned_factory = if cb_for_fixed == positioned_factory.nearest_fixed_cb() {
          positioned_factory
        } else {
          positioned_factory.with_fixed_cb(cb_for_fixed)
        };
        let abs_factory = if cb_for_absolute == positioned_factory.nearest_positioned_cb() {
          positioned_factory.clone()
        } else {
          positioned_factory.with_positioned_cb(cb_for_absolute)
        };

        let container_style = taffy
          .style(root_id)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        let axes_swapped = container_style.axes_swapped;
        let mirror_x = !container_style.start_end_axis_positive.x;
        let mirror_y = !container_style.start_end_axis_positive.y;

        #[derive(Clone, Copy, Default)]
        struct GridAreaOverride {
          x: Option<(f32, f32)>,
          y: Option<(f32, f32)>,
        }

        let mut static_positions: FxHashMap<usize, Point> = FxHashMap::default();
        let mut grid_area_overrides: FxHashMap<usize, GridAreaOverride> = FxHashMap::default();
        if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
            let row_offsets = compute_track_offsets(
              &info.rows,
              fragment.bounds.height(),
              padding_top,
              padding_bottom,
              border_top,
              border_bottom,
              container_style
                .align_content
                .unwrap_or(taffy::style::AlignContent::Stretch),
            );
            let col_offsets = compute_track_offsets(
              &info.columns,
              fragment.bounds.width(),
              padding_left,
              padding_right,
              border_left,
              border_right,
              container_style
                .justify_content
                .unwrap_or(taffy::style::AlignContent::Stretch),
            );

            let col_line_count = Some(info.columns.explicit_tracks.saturating_add(1));
            let row_line_count = Some(info.rows.explicit_tracks.saturating_add(1));
            let row_alignment = container_style
              .align_content
              .unwrap_or(taffy::style::AlignContent::Stretch);
            let col_alignment = container_style
              .justify_content
              .unwrap_or(taffy::style::AlignContent::Stretch);
            let col_span = mirror_x
              .then(|| {
                let span_start = col_offsets.get(1).copied()?;
                let grid_end = col_offsets.last().copied()?;
                let span_end = if col_alignment == taffy::style::AlignContent::Stretch {
                  let content_end = fragment.bounds.width() - padding_right - border_right;
                  content_end.max(grid_end)
                } else {
                  grid_end
                };
                Some((span_start, span_end))
              })
              .flatten();
            let row_span = mirror_y
              .then(|| {
                let span_start = row_offsets.get(1).copied()?;
                let grid_end = row_offsets.last().copied()?;
                let span_end = if row_alignment == taffy::style::AlignContent::Stretch {
                  let content_end = fragment.bounds.height() - padding_bottom - border_bottom;
                  content_end.max(grid_end)
                } else {
                  grid_end
                };
                Some((span_start, span_end))
              })
              .flatten();

            let needs_column_effective_names = positioned_children
              .iter()
              .any(|child| child.style.grid_column_raw.is_some());
            let needs_row_effective_names = positioned_children
              .iter()
              .any(|child| child.style.grid_row_raw.is_some());

              let effective_column_line_names = needs_column_effective_names
                .then(|| {
                  resolve_effective_grid_line_names_for_node_axis(
                    &taffy,
                    root_id,
                    CssGridAxis::Column,
                  )
                })
                .flatten();
              let effective_row_line_names = needs_row_effective_names
                .then(|| {
                  resolve_effective_grid_line_names_for_node_axis(
                    &taffy,
                    root_id,
                    CssGridAxis::Row,
                  )
                })
                .flatten();

            for child in &positioned_children {
              let mut pos = Point::ZERO;
              let child_id = ensure_box_id(child);
              let mut override_area = GridAreaOverride::default();
              let (x_start, x_end, x_raw) = if axes_swapped {
                (
                  child.style.grid_row_start,
                  child.style.grid_row_end,
                  child.style.grid_row_raw.as_deref(),
                )
              } else {
                (
                  child.style.grid_column_start,
                  child.style.grid_column_end,
                  child.style.grid_column_raw.as_deref(),
                )
              };
              let x_line_names: &[Vec<String>] = if axes_swapped {
                effective_row_line_names
                  .as_deref()
                  .unwrap_or_else(|| box_node.style.grid_row_line_names.as_slice())
              } else {
                effective_column_line_names
                  .as_deref()
                  .unwrap_or_else(|| box_node.style.grid_column_line_names.as_slice())
              };
              let x_is_auto = if let Some(raw) = x_raw {
                let parsed = parse_grid_line_placement_raw(raw, Some(x_line_names));
                matches!(
                  grid_placement_component_from_taffy(&parsed.start),
                  ResolvedGridPlacementComponent::Auto
                ) && matches!(
                  grid_placement_component_from_taffy(&parsed.end),
                  ResolvedGridPlacementComponent::Auto
                )
              } else {
                x_start == 0 && x_end == 0
              };
              if let Some((start_line, end_line)) = resolve_grid_line_range_from_style(
                x_start,
                x_end,
                x_raw,
                col_line_count,
                Some(x_line_names),
              ) {
                if let Some((area_start, area_end)) =
                  grid_area_for_positioned_item(&col_offsets, start_line, end_line)
                {
                  let mut start = area_start;
                  if mirror_x {
                    if let Some((span_start, span_end)) = col_span {
                      start = span_start + (span_end - area_end);
                    }
                  }
                  pos.x = start - padding_origin.x;
                  if !x_is_auto {
                    let size = (area_end - area_start).max(0.0);
                    if size.is_finite() && start.is_finite() {
                      override_area.x = Some((start, start + size));
                    }
                  }
                }
              }

              let (y_start, y_end, y_raw) = if axes_swapped {
                (
                  child.style.grid_column_start,
                  child.style.grid_column_end,
                  child.style.grid_column_raw.as_deref(),
                )
              } else {
                (
                  child.style.grid_row_start,
                  child.style.grid_row_end,
                  child.style.grid_row_raw.as_deref(),
                )
              };
              let y_line_names: &[Vec<String>] = if axes_swapped {
                effective_column_line_names
                  .as_deref()
                  .unwrap_or_else(|| box_node.style.grid_column_line_names.as_slice())
              } else {
                effective_row_line_names
                  .as_deref()
                  .unwrap_or_else(|| box_node.style.grid_row_line_names.as_slice())
              };
              let y_is_auto = if let Some(raw) = y_raw {
                let parsed = parse_grid_line_placement_raw(raw, Some(y_line_names));
                matches!(
                  grid_placement_component_from_taffy(&parsed.start),
                  ResolvedGridPlacementComponent::Auto
                ) && matches!(
                  grid_placement_component_from_taffy(&parsed.end),
                  ResolvedGridPlacementComponent::Auto
                )
              } else {
                y_start == 0 && y_end == 0
              };
              if let Some((start_line, end_line)) = resolve_grid_line_range_from_style(
                y_start,
                y_end,
                y_raw,
                row_line_count,
                Some(y_line_names),
              ) {
                if let Some((area_start, area_end)) =
                  grid_area_for_positioned_item(&row_offsets, start_line, end_line)
                {
                  let mut start = area_start;
                  if mirror_y {
                    if let Some((span_start, span_end)) = row_span {
                      start = span_start + (span_end - area_end);
                    }
                  }
                  pos.y = start - padding_origin.y;
                  if !y_is_auto {
                    let size = (area_end - area_start).max(0.0);
                    if size.is_finite() && start.is_finite() {
                      override_area.y = Some((start, start + size));
                    }
                  }
                }
              }
              static_positions.insert(child_id, pos);
              if override_area.x.is_some() || override_area.y.is_some() {
                grid_area_overrides.insert(child_id, override_area);
              }
            }
        }

        let abs = crate::layout::absolute_positioning::AbsoluteLayout::with_font_context(
          ctx.font_context.clone(),
        );
        let mut abs_deadline_counter = 0usize;
        for child in &positioned_children {
          check_layout_deadline(&mut abs_deadline_counter)?;

          let child_id = ensure_box_id(child);

          let mut cb = match child.style.position {
            crate::style::position::Position::Fixed => cb_for_fixed,
            _ => cb_for_absolute,
          };
          let mut cb_origin_delta = Point::ZERO;
          if cb == padding_cb {
            if let Some(area) = grid_area_overrides.get(&child_id) {
              let mut rect = cb.rect;
              let old_origin = rect.origin;
              if let Some((x0, x1)) = area.x {
                rect.origin.x = x0;
                rect.size.width = (x1 - x0).max(0.0);
              }
              if let Some((y0, y1)) = area.y {
                rect.origin.y = y0;
                rect.size.height = (y1 - y0).max(0.0);
              }
              if rect != cb.rect {
                cb_origin_delta =
                  Point::new(rect.origin.x - old_origin.x, rect.origin.y - old_origin.y);
                let wm = cb.writing_mode;
                let dir = cb.direction;
                let box_id = cb.box_id();
                cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport_and_bases(
                  rect,
                  cb.viewport_size(),
                  cb.inline_percentage_base().map(|_| rect.size.width),
                  cb.block_percentage_base().map(|_| rect.size.height),
                )
                .with_writing_mode_and_direction(wm, dir)
                .with_box_id(box_id);
              }
            }
          }

          let fc_type = child
            .formatting_context()
            .unwrap_or(crate::style::display::FormattingContextType::Block);
          let fc = abs_factory.get(fc_type);
          let child_constraints = LayoutConstraints::new(
            CrateAvailableSpace::Definite(padding_rect.size.width),
            block_base
              .map(CrateAvailableSpace::Definite)
              .unwrap_or(CrateAvailableSpace::Indefinite),
          );

          let anchors_for_cb = Some(&anchor_index);
          let implicit_anchor_box_id = child.implicit_anchor_box_id;
          let positioned_style =
            crate::layout::absolute_positioning::resolve_positioned_style_with_anchors(
              &child.style,
              &cb,
              ctx.viewport_size,
              &ctx.font_context,
              anchors_for_cb,
              crate::layout::anchor_positioning::AnchorQueryContext {
                query_parent_box_id: Some(root_box_id),
                implicit_anchor_box_id,
              },
            );
          // Static position resolves to where the element would be in flow, relative to the containing
          // block origin (padding edge).
          let mut static_pos = static_positions
            .get(&child_id)
            .copied()
            .unwrap_or(crate::geometry::Point::ZERO);
          if cb_origin_delta != Point::ZERO {
            static_pos = Point::new(
              static_pos.x - cb_origin_delta.x,
              static_pos.y - cb_origin_delta.y,
            );
          }
          let width_keyword = child.style.width_keyword;
          let min_width_keyword = child.style.min_width_keyword;
          let max_width_keyword = child.style.max_width_keyword;
          let height_keyword = child.style.height_keyword;
          let min_height_keyword = child.style.min_height_keyword;
          let max_height_keyword = child.style.max_height_keyword;
          let has_inline_keyword =
            width_keyword.is_some() || min_width_keyword.is_some() || max_width_keyword.is_some();
          let has_block_keyword = height_keyword.is_some()
            || min_height_keyword.is_some()
            || max_height_keyword.is_some();
          let needs_inline_intrinsics = has_inline_keyword
            || (positioned_style.width.is_auto()
              && (positioned_style.left.is_auto()
                || positioned_style.right.is_auto()
                || child.is_replaced()));
          let needs_block_intrinsics = has_block_keyword
            || (positioned_style.height.is_auto()
              && (positioned_style.top.is_auto() || positioned_style.bottom.is_auto()));
          let mut static_style = (*child.style).clone();
          static_style.position = crate::style::position::Position::Relative;
          static_style.top = crate::style::types::InsetValue::Auto;
          static_style.right = crate::style::types::InsetValue::Auto;
          static_style.bottom = crate::style::types::InsetValue::Auto;
          static_style.left = crate::style::types::InsetValue::Auto;
          let static_style = Arc::new(static_style);

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
                let child_fragment = fc.layout(child, &child_constraints)?;
                let (preferred_min_inline, preferred_inline) = if needs_inline_intrinsics {
                  match fc.compute_intrinsic_inline_sizes(child) {
                    Ok((min, max)) => (Some(min), Some(max)),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => {
                      let min = match fc
                        .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MinContent)
                      {
                        Ok(size) => Some(size),
                        Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                        Err(_) => None,
                      };
                      let max = match fc
                        .compute_intrinsic_inline_size(child, IntrinsicSizingMode::MaxContent)
                      {
                        Ok(size) => Some(size),
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
                  match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MinContent) {
                    Ok(size) => Some(size),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  }
                } else {
                  None
                };
                let preferred_block = if needs_block_intrinsics {
                  match fc.compute_intrinsic_block_size(child, IntrinsicSizingMode::MaxContent) {
                    Ok(size) => Some(size),
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
            let mut layout_child = (*child).clone();
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
                    Ok(size) => Some(size),
                    Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                    Err(_) => None,
                  };
                  let max = match fc
                    .compute_intrinsic_inline_size(&layout_child, IntrinsicSizingMode::MaxContent)
                  {
                    Ok(size) => Some(size),
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
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MinContent)
              {
                Ok(size) => Some(size),
                Err(err @ LayoutError::Timeout { .. }) => return Err(err),
                Err(_) => None,
              }
            } else {
              None
            };
            let preferred_block = if needs_block_intrinsics {
              match fc.compute_intrinsic_block_size(&layout_child, IntrinsicSizingMode::MaxContent)
              {
                Ok(size) => Some(size),
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
              &child.style,
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
          input.style.width_keyword = width_keyword;
          input.style.min_width_keyword = min_width_keyword;
          input.style.max_width_keyword = max_width_keyword;
          input.style.height_keyword = height_keyword;
          input.style.min_height_keyword = min_height_keyword;
          input.style.max_height_keyword = max_height_keyword;
          input.is_replaced = child.is_replaced();
          input.preferred_min_inline_size = preferred_min_inline;
          input.preferred_inline_size = preferred_inline;
          input.preferred_min_block_size = preferred_min_block;
          input.preferred_block_size = preferred_block;
          let supports_used_border_box = matches!(
            fc_type,
            FormattingContextType::Block
              | FormattingContextType::Flex
              | FormattingContextType::Grid
              | FormattingContextType::Inline
              | FormattingContextType::Table
          );
          let anchor_query = crate::layout::anchor_positioning::AnchorQueryContext {
            query_parent_box_id: Some(root_box_id),
            implicit_anchor_box_id,
          };
          let (mut layout_positioned_style, mut result) =
            crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
              &abs,
              &input,
              &child.style,
              &cb,
              ctx.viewport_size,
              &ctx.font_context,
              anchors_for_cb,
              anchor_query,
            )?;
          let mut border_size = Size::new(
            result.size.width + actual_horizontal,
            result.size.height + actual_vertical,
          );
          let mut border_origin = Point::new(
            result.position.x - content_offset.x,
            result.position.y - content_offset.y,
          );
          if crate::layout::absolute_positioning::auto_height_uses_intrinsic_size(
            &layout_positioned_style,
            input.is_replaced,
          ) && (border_size.width - child_fragment.bounds.width()).abs() > 0.01
          {
            let measure_constraints = child_constraints
              .with_width(CrateAvailableSpace::Definite(border_size.width))
              .with_height(CrateAvailableSpace::Indefinite)
              .with_used_border_box_size(Some(border_size.width), None);
            child_fragment = if child.id != 0 {
              if supports_used_border_box {
                crate::layout::style_override::with_style_override(
                  child.id,
                  static_style.clone(),
                  || fc.layout(child, &measure_constraints),
                )?
              } else {
                let mut measure_style = (*static_style).clone();
                measure_style.width = Some(Length::px(border_size.width));
                measure_style.width_keyword = None;
                measure_style.min_width_keyword = None;
                measure_style.max_width_keyword = None;
                crate::layout::style_override::with_style_override(
                  child.id,
                  Arc::new(measure_style),
                  || fc.layout(child, &measure_constraints),
                )?
              }
            } else {
              let mut measure_child = (*child).clone();
              if supports_used_border_box {
                measure_child.style = static_style.clone();
              } else {
                let mut measure_style = (*static_style).clone();
                measure_style.width = Some(Length::px(border_size.width));
                measure_style.width_keyword = None;
                measure_style.min_width_keyword = None;
                measure_style.max_width_keyword = None;
                measure_child.style = Arc::new(measure_style);
              }
              fc.layout(&measure_child, &measure_constraints)?
            };

            input.intrinsic_size.height =
              (child_fragment.bounds.size.height - actual_vertical).max(0.0);
            (layout_positioned_style, result) =
              crate::layout::absolute_positioning::layout_absolute_with_position_try_fallbacks(
                &abs,
                &input,
                &child.style,
                &cb,
                ctx.viewport_size,
                &ctx.font_context,
                anchors_for_cb,
                anchor_query,
              )?;
            border_size = Size::new(
              result.size.width + actual_horizontal,
              result.size.height + actual_vertical,
            );
            border_origin = Point::new(
              result.position.x - content_offset.x,
              result.position.y - content_offset.y,
            );
          }
          let relayout_for_inset_resolved_size =
            crate::layout::absolute_positioning::auto_size_resolved_by_insets(
              &layout_positioned_style,
            );
          let needs_relayout = (border_size.width - child_fragment.bounds.width()).abs() > 0.01
            || (border_size.height - child_fragment.bounds.height()).abs() > 0.01
            || relayout_for_inset_resolved_size;
          if needs_relayout {
            let relayout_constraints = child_constraints
              .with_used_border_box_size(Some(border_size.width), Some(border_size.height));
            if child.id != 0 {
              if supports_used_border_box {
                child_fragment = crate::layout::style_override::with_style_override(
                  child.id,
                  static_style.clone(),
                  || fc.layout(child, &relayout_constraints),
                )?;
              } else {
                let mut relayout_style = (*static_style).clone();
                relayout_style.width = Some(Length::px(border_size.width));
                relayout_style.height = Some(Length::px(border_size.height));
                relayout_style.width_keyword = None;
                relayout_style.height_keyword = None;
                child_fragment = crate::layout::style_override::with_style_override(
                  child.id,
                  Arc::new(relayout_style),
                  || fc.layout(child, &relayout_constraints),
                )?;
              }
            } else {
              let mut relayout_child = (*child).clone();
              if supports_used_border_box {
                relayout_child.style = static_style.clone();
              } else {
                let mut relayout_style = (*static_style).clone();
                relayout_style.width = Some(Length::px(border_size.width));
                relayout_style.height = Some(Length::px(border_size.height));
                relayout_style.width_keyword = None;
                relayout_style.height_keyword = None;
                relayout_child.style = Arc::new(relayout_style);
              }
              child_fragment = fc.layout(&relayout_child, &relayout_constraints)?;
            }
          }
          child_fragment.bounds = crate::geometry::Rect::new(border_origin, border_size);
          if matches!(child.style.position, crate::style::position::Position::Absolute) {
            child_fragment.abs_containing_block_box_id = cb.box_id();
          }
          child_fragment.style = Some(child.style.clone());
          let child_box_id = ensure_box_id(child);
          match &mut child_fragment.content {
            FragmentContent::Block { box_id }
            | FragmentContent::Inline { box_id, .. }
            | FragmentContent::Text { box_id, .. }
            | FragmentContent::Replaced { box_id, .. } => *box_id = Some(child_box_id),
            FragmentContent::Line { .. }
            | FragmentContent::RunningAnchor { .. }
            | FragmentContent::FootnoteAnchor { .. } => {}
          }
          fragment.children_mut().push(child_fragment);
        }
      }

      let mut footnote_anchors: Vec<FragmentNode> = Vec::new();
      for (idx, child) in in_flow_children.iter().enumerate() {
        let Some(body) = child.footnote_body.as_deref() else {
          continue;
        };

        let snapshot_node = body.clone();
        let hinted_inline_size = crate::layout::formatting_context::footnote_area_inline_size_hint()
          .filter(|size| size.is_finite())
          .map(|size| size.max(0.0));
        let viewport_inline_size =
          if crate::style::inline_axis_is_horizontal(body.style.writing_mode) {
            ctx.viewport_size.width
          } else {
            ctx.viewport_size.height
          };
        let inline_size = if let Some(size) = hinted_inline_size {
          size
        } else if viewport_inline_size.is_finite() {
          viewport_inline_size.max(0.0)
        } else {
          0.0
        };

        let fc_type = snapshot_node.formatting_context().unwrap_or_else(|| {
          if snapshot_node.is_block_level() {
            FormattingContextType::Block
          } else {
            FormattingContextType::Inline
          }
        });
        let fc = ctx.factory.get(fc_type);
        let snapshot_constraints = LayoutConstraints::new(
          CrateAvailableSpace::Definite(inline_size),
          CrateAvailableSpace::Indefinite,
        );
        let snapshot_fragment = fc.layout(&snapshot_node, &snapshot_constraints)?;

        let Some(item_fragment) = fragment.children.get(idx) else {
          continue;
        };
        let origin = item_fragment.bounds.origin;
        let anchor_bounds = Rect::from_xywh(origin.x, origin.y, 0.0, 0.01);
        let mut anchor = FragmentNode::new_footnote_anchor(
          anchor_bounds,
          snapshot_fragment,
          child.style.footnote_policy,
        );
        anchor.style = Some(child.style.clone());
        footnote_anchors.push(anchor);
      }
      if !footnote_anchors.is_empty() {
        fragment.children_mut().extend(footnote_anchors);
      }

      if !running_children.is_empty() {
        let mut id_to_bounds: FxHashMap<usize, Rect> =
          FxHashMap::with_capacity_and_hasher(fragment.children.len(), Default::default());
        let mut deadline_counter = 0usize;
        for child in fragment.children.iter() {
          check_layout_deadline(&mut deadline_counter)?;
          let Some(box_id) = (match &child.content {
            FragmentContent::Block { box_id }
            | FragmentContent::Inline { box_id, .. }
            | FragmentContent::Text { box_id, .. }
            | FragmentContent::Replaced { box_id, .. } => *box_id,
            FragmentContent::Line { .. }
            | FragmentContent::RunningAnchor { .. }
            | FragmentContent::FootnoteAnchor { .. } => None,
          }) else {
            continue;
          };
          id_to_bounds.entry(box_id).or_insert(child.bounds);
        }

        let mut row_offsets: Option<Vec<f32>> = None;
        let mut col_offsets: Option<Vec<f32>> = None;
        if let DetailedLayoutInfo::Grid(info) = taffy.detailed_layout_info(root_id) {
          if let Ok(container_style) = taffy.style(root_id) {
            let percentage_base = constraints.width().unwrap_or(fragment.bounds.width());
            let padding_left =
              ctx.resolve_length_for_width(style.padding_left, percentage_base, style);
            let padding_right =
              ctx.resolve_length_for_width(style.padding_right, percentage_base, style);
            let padding_top =
              ctx.resolve_length_for_width(style.padding_top, percentage_base, style);
            let padding_bottom =
              ctx.resolve_length_for_width(style.padding_bottom, percentage_base, style);
            let border_left =
              ctx.resolve_length_for_width(style.used_border_left_width(), percentage_base, style);
            let border_right =
              ctx.resolve_length_for_width(style.used_border_right_width(), percentage_base, style);
            let border_top =
              ctx.resolve_length_for_width(style.used_border_top_width(), percentage_base, style);
            let border_bottom = ctx.resolve_length_for_width(
              style.used_border_bottom_width(),
              percentage_base,
              style,
            );

            row_offsets = Some(compute_track_offsets(
              &info.rows,
              fragment.bounds.height(),
              padding_top,
              padding_bottom,
              border_top,
              border_bottom,
              container_style
                .align_content
                .unwrap_or(taffy::style::AlignContent::Stretch),
            ));
            col_offsets = Some(compute_track_offsets(
              &info.columns,
              fragment.bounds.width(),
              padding_left,
              padding_right,
              border_left,
              border_right,
              container_style
                .justify_content
                .unwrap_or(taffy::style::AlignContent::Stretch),
            ));
          }
        }

        let axes =
          FragmentAxes::from_writing_mode_and_direction(style.writing_mode, style.direction);
        let snapshot_factory = ctx.factory.clone();

        let parse_explicit_single_track =
          |raw: Option<&str>, start: i32, end: i32| -> Option<(u16, u16)> {
            if start > 0 && end > 0 && end == start + 1 {
              return Some((start as u16, end as u16));
            }
            let raw = raw?;
            let placement = parse_grid_line_placement_raw(raw, None);
            let TaffyGridPlacement::Line(start_line) = placement.start else {
              return None;
            };
            let TaffyGridPlacement::Line(end_line) = placement.end else {
              return None;
            };
            let start = start_line.as_i16();
            let end = end_line.as_i16();
            if start > 0 && end > 0 && end == start + 1 {
              Some((start as u16, end as u16))
            } else {
              None
            }
          };

        for (order, (running_idx, running_child)) in running_children.into_iter().enumerate() {
          check_layout_deadline(&mut deadline_counter)?;
          let Some(name) = running_child.style.running_position.clone() else {
            continue;
          };

          let mut anchor_x = 0.0f32;
          let mut anchor_y = 0.0f32;

          if let Some((_, next_child)) = box_node
            .children
            .iter()
            .enumerate()
            .filter(|(idx, child)| {
              *idx > running_idx
                && child.style.running_position.is_none()
                && !matches!(
                  child.style.position,
                  crate::style::position::Position::Absolute
                    | crate::style::position::Position::Fixed
                )
            })
            .min_by_key(|(idx, _)| *idx)
          {
            if let Some(bounds) = id_to_bounds.get(&ensure_box_id(next_child)) {
              anchor_x = bounds.x();
              anchor_y = bounds.y();
            }
          } else if let Some((_, last_child)) = box_node
            .children
            .iter()
            .enumerate()
            .filter(|(_, child)| {
              child.style.running_position.is_none()
                && !matches!(
                  child.style.position,
                  crate::style::position::Position::Absolute
                    | crate::style::position::Position::Fixed
                )
            })
            .max_by_key(|(idx, _)| *idx)
          {
            if let Some(bounds) = id_to_bounds.get(&ensure_box_id(last_child)) {
              match axes.block_axis() {
                PhysicalAxis::Y => {
                  anchor_x = bounds.x();
                  anchor_y = bounds.max_y();
                }
                PhysicalAxis::X => {
                  anchor_y = bounds.y();
                  anchor_x = if axes.block_positive() {
                    bounds.max_x()
                  } else {
                    bounds.x()
                  };
                }
              }
            }
          } else {
            match axes.block_axis() {
              PhysicalAxis::Y => {
                anchor_y = fragment.bounds.height();
              }
              PhysicalAxis::X => {
                anchor_x = if axes.block_positive() {
                  fragment.bounds.width()
                } else {
                  0.0
                };
              }
            }
          }

          let mut area_w: Option<f32> = None;
          let mut area_h: Option<f32> = None;

          if let Some(col_offsets) = col_offsets.as_ref() {
            if let Some((start_line, end_line)) = parse_explicit_single_track(
              running_child.style.grid_column_raw.as_deref(),
              running_child.style.grid_column_start,
              running_child.style.grid_column_end,
            ) {
              if let Some((start, end)) = grid_area_for_item(col_offsets, start_line, end_line) {
                anchor_x = start;
                area_w = Some((end - start).max(0.0));
              }
            }
          }

          if let Some(row_offsets) = row_offsets.as_ref() {
            if let Some((start_line, end_line)) = parse_explicit_single_track(
              running_child.style.grid_row_raw.as_deref(),
              running_child.style.grid_row_start,
              running_child.style.grid_row_end,
            ) {
              if let Some((start, end)) = grid_area_for_item(row_offsets, start_line, end_line) {
                anchor_y = start;
                area_h = Some((end - start).max(0.0));
              }
            }
          }

          let mut snapshot_node = running_child.clone();
          let mut snapshot_style = snapshot_node.style.as_ref().clone();
          snapshot_style.running_position = None;
          snapshot_style.position = crate::style::position::Position::Static;
          snapshot_node.style = Arc::new(snapshot_style);
          clear_running_position_in_box_tree(&mut snapshot_node);

          let fc_type = snapshot_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let fc = snapshot_factory.get(fc_type);

          let snapshot_constraints = if let (Some(w), Some(h)) = (area_w, area_h) {
            LayoutConstraints::new(
              CrateAvailableSpace::Definite(w.max(0.0)),
              CrateAvailableSpace::Definite(h.max(0.0)),
            )
            .with_used_border_box_size(Some(w.max(0.0)), Some(h.max(0.0)))
            .with_inline_percentage_base(Some(w.max(0.0)))
          } else {
            let container_w = fragment.bounds.width().max(0.0);
            LayoutConstraints::new(
              CrateAvailableSpace::Definite(container_w),
              CrateAvailableSpace::Indefinite,
            )
          };

          match fc.layout(&snapshot_node, &snapshot_constraints) {
            Ok(snapshot_fragment) => {
              let eps = (order as f32) * 1e-4;
              let offset = axes.block_offset(eps);
              let anchor_bounds =
                Rect::from_xywh(anchor_x + offset.x, anchor_y + offset.y, 0.0, 0.01);
              let mut anchor =
                FragmentNode::new_running_anchor(anchor_bounds, name, snapshot_fragment);
              anchor.style = Some(running_child.style.clone());
              fragment.children_mut().push(anchor);
            }
            Err(err @ LayoutError::Timeout { .. }) => return Err(err),
            Err(_) => {}
          }
        }
      }

      fragment.scrollbar_reservation = crate::layout::utils::scrollbar_reservation_for_style(style);

      if !disable_global_layout_cache {
        layout_cache_store(
          box_node,
          FormattingContextType::Grid,
          constraints,
          &fragment,
          self.factory.viewport_scroll(),
          self.viewport_size,
          self.nearest_positioned_cb,
          self.nearest_fixed_cb,
        );
      }

      return Ok(fragment);
    }

    // FastRender models scrollbars as overlay by default: scrollbars do not affect layout unless the
    // author opts into reserving space via `scrollbar-gutter: stable`.
    //
    // The `overflow:auto` convergence loop below exists for classic scrollbar experiments (where
    // scrollbars can reduce the scrollport size). Skip it under the default overlay model to avoid
    // redundant layout passes.
    if !crate::debug::runtime::runtime_toggles().truthy("FASTR_CLASSIC_SCROLLBARS") {
      return crate::layout::auto_scrollbars::with_bypass(box_node, || {
        self.layout(box_node, constraints)
      });
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let base_style = style_override.unwrap_or_else(|| box_node.style.clone());
    let gutter = crate::layout::utils::resolve_scrollbar_width(&base_style);
    if gutter <= 0.0
      || !base_style.scrollbar_gutter.stable
      || (!matches!(base_style.overflow_x, CssOverflow::Auto)
        && !matches!(base_style.overflow_y, CssOverflow::Auto))
    {
      return crate::layout::auto_scrollbars::with_bypass(box_node, || {
        self.layout(box_node, constraints)
      });
    }

    let containing_width = constraints
      .inline_percentage_base
      .or_else(|| constraints.width())
      .unwrap_or(self.viewport_size.width);
    let mut force_x = false;
    let mut force_y = false;
    let mut last: Option<FragmentNode> = None;
    for _ in 0..3 {
      let mut override_style = base_style.clone();
      let mut overridden = false;
      {
        let s = Arc::make_mut(&mut override_style);
        if force_x
          && matches!(base_style.overflow_x, CssOverflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_x = CssOverflow::Scroll;
          overridden = true;
        }
        if force_y
          && matches!(base_style.overflow_y, CssOverflow::Auto)
          && !base_style.scrollbar_gutter.stable
        {
          s.overflow_y = CssOverflow::Scroll;
          overridden = true;
        }
      }

      let fragment = if overridden && box_node.id != 0 {
        crate::layout::style_override::with_style_override(
          box_node.id,
          override_style.clone(),
          || {
            crate::layout::auto_scrollbars::with_bypass(box_node, || {
              self.layout(box_node, constraints)
            })
          },
        )
      } else if overridden {
        let mut cloned = box_node.clone();
        cloned.style = override_style.clone();
        crate::layout::auto_scrollbars::with_bypass(&cloned, || self.layout(&cloned, constraints))
      } else {
        crate::layout::auto_scrollbars::with_bypass(box_node, || self.layout(box_node, constraints))
      }?;

      let (overflow_x, overflow_y) = crate::layout::utils::fragment_overflows_content_box(
        &fragment,
        override_style.as_ref(),
        containing_width,
        self.viewport_size,
        Some(&self.font_context),
      );
      let need_x = gutter > 0.0
        && matches!(base_style.overflow_x, CssOverflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_x;
      let need_y = gutter > 0.0
        && matches!(base_style.overflow_y, CssOverflow::Auto)
        && !base_style.scrollbar_gutter.stable
        && overflow_y;

      last = Some(fragment);
      if need_x == force_x && need_y == force_y {
        break;
      }
      force_x = need_x;
      force_y = need_y;
    }

    let Some(last) = last else {
      debug_assert!(false, "at least one layout pass");
      return Err(LayoutError::MissingContext(
        "grid layout produced no fragments".to_string(),
      ));
    };
    Ok(last)
  }

  fn compute_intrinsic_inline_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    self.compute_intrinsic_size(box_node, box_node.style.as_ref(), mode)
  }

  fn compute_intrinsic_block_size(
    &self,
    box_node: &BoxNode,
    mode: IntrinsicSizingMode,
  ) -> Result<f32, LayoutError> {
    debug_assert!(
      matches!(
        box_node.formatting_context(),
        Some(FormattingContextType::Grid)
      ),
      "GridFormattingContext must only query grid containers",
    );

    if let Some(cached) = intrinsic_block_cache_lookup(box_node, mode) {
      return Ok(cached);
    }

    let style_override = crate::layout::style_override::style_override_for(box_node.id);
    let style: &ComputedStyle = style_override
      .as_deref()
      .unwrap_or_else(|| box_node.style.as_ref());

    if style.containment.isolates_block_size() {
      let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);
      let edges = if inline_is_horizontal {
        self.vertical_edges_px(style).unwrap_or(0.0)
      } else {
        self.horizontal_edges_px(style).unwrap_or(0.0)
      };
      let axis = if inline_is_horizontal {
        style.contain_intrinsic_height
      } else {
        style.contain_intrinsic_width
      };
      let fallback = crate::layout::utils::resolve_contain_intrinsic_size_axis(
        axis,
        None,
        Some(0.0),
        self.viewport_size,
        style.font_size,
        style.root_font_size,
      );
      let size = (edges + fallback).max(0.0);
      intrinsic_block_cache_store(box_node, mode, size);
      return Ok(size);
    }

    let inline_is_horizontal = crate::style::inline_axis_is_horizontal(style.writing_mode);

    // The intrinsic block-size algorithms are defined in terms of the box's intrinsic *inline*
    // size. For example, a horizontal-tb box's max-content block-size is the height the box would
    // take when laid out at its max-content inline size.
    //
    // Letting Taffy interpret `AvailableSpace::{MinContent,MaxContent}` directly can cause it to
    // shrink-to-fit the root grid container's inline size (especially for `width:auto` grids),
    // collapsing the min-/max-content block-size distinction. This shows up on real pages as
    // `height: fit-content` resolving to an exaggerated max-content height, because both intrinsic
    // probes report the same wrapped/narrow layout.
    //
    // Instead, compute the intrinsic inline size first and run a block-size probe with that inline
    // size as a *definite* constraint.
    let inline_border_box = match self.compute_intrinsic_inline_size(box_node, mode) {
      Ok(v) if v.is_finite() => v.max(0.0),
      Ok(_) => 0.0,
      Err(err @ LayoutError::Timeout { .. }) => return Err(err),
      Err(_) => 0.0,
    };

    let intrinsic_constraints = if inline_is_horizontal {
      LayoutConstraints::new(
        CrateAvailableSpace::Definite(inline_border_box),
        CrateAvailableSpace::Indefinite,
      )
    } else {
      // In vertical writing modes the inline axis maps to the physical Y axis. Keep the existing
      // physical-axis layout model by constraining `available_height`.
      let mut constraints = LayoutConstraints::new(
        CrateAvailableSpace::Indefinite,
        CrateAvailableSpace::Definite(inline_border_box),
      );
      constraints.inline_percentage_base = Some(inline_border_box);
      constraints
    };

    let mut deadline_counter = 0usize;
    let mut in_flow_children: Vec<(usize, &BoxNode)> = box_node
      .children
      .iter()
      .enumerate()
      .filter(|(_, child)| {
        !matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      })
      .collect();
    let mut in_flow_children_need_sort = false;
    let mut last_in_flow_order: Option<i32> = None;
    for (_, child) in in_flow_children.iter() {
      check_layout_deadline(&mut deadline_counter)?;
      if let Some(prev) = last_in_flow_order {
        if child.style.order < prev {
          in_flow_children_need_sort = true;
          break;
        }
      }
      last_in_flow_order = Some(child.style.order);
    }
    if in_flow_children_need_sort {
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
      in_flow_children.sort_by(|(a_idx, a), (b_idx, b)| {
        a.style.order.cmp(&b.style.order).then_with(|| a_idx.cmp(b_idx))
      });
      if let Err(RenderError::Timeout { elapsed, .. }) = check_active(RenderStage::Layout) {
        return Err(LayoutError::Timeout { elapsed });
      }
    }
    let in_flow_children: Vec<&BoxNode> = in_flow_children
      .into_iter()
      .map(|(_, child)| child)
      .collect();

    let child_has_subgrid = in_flow_children
      .iter()
      .any(|child| child.style.grid_row_subgrid || child.style.grid_column_subgrid);
    let cacheable_tree =
      !(style.grid_row_subgrid || style.grid_column_subgrid || child_has_subgrid);

    let mut taffy = CachedTaffyTree::new(TaffyAdapterKind::Grid, box_node.id, cacheable_tree);
    let mut positioned_children: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> = FxHashMap::default();

    // Resolve intrinsic sizing keywords for descendants while computing this grid container's
    // intrinsic block sizes. Skip resolving the root itself to avoid recursion: resolving
    // `height/width: max-content` requires asking for the same intrinsic block-size we are
    // computing.
    let _intrinsic_keyword_overrides = self.resolve_intrinsic_sizing_keywords_for_taffy_tree(
      box_node,
      &in_flow_children,
      &intrinsic_constraints,
      false,
    )?;

    let root_id = self.build_or_update_taffy_tree_children_cached(
      &mut taffy,
      box_node,
      style,
      &in_flow_children,
      &intrinsic_constraints,
      &mut positioned_children,
    )?;
    if let Some(style_override) = style_override.as_deref() {
      let mut style_deadline_counter = 0usize;
      let has_positioned_children = box_node.children.iter().any(|child| {
        matches!(
          child.style.position,
          crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
        )
      });
      let simple_grid = !has_positioned_children
        && self.is_simple_grid(
          style_override,
          &in_flow_children,
          &mut style_deadline_counter,
        )?;
      let override_taffy_style = self.convert_style(
        style_override,
        None,
        None,
        None,
        simple_grid,
        true,
        box_node.is_replaced(),
      );
      let needs_override_update = match taffy.style(root_id) {
        Ok(existing) => existing != &override_taffy_style,
        Err(_) => true,
      };
      if needs_override_update {
        taffy
          .set_style(root_id, override_taffy_style)
          .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
      }
    }

    // Ensure block-level grids with `width:auto` stretch to the definite inline size used for this
    // intrinsic probe (mirrors the main `layout()` logic).
    if inline_is_horizontal {
      if let CrateAvailableSpace::Definite(outer_width) = intrinsic_constraints.available_width {
        if outer_width.is_finite()
          && physical_width_is_auto(style)
          && box_node.is_block_level()
          && crate::style::inline_axis_is_horizontal(style.writing_mode)
        {
          if let Ok(existing) = taffy.style(root_id) {
            if existing.size.width.is_auto() {
              let mut updated = existing.clone();
              let base_width = intrinsic_constraints
                .inline_percentage_base
                .unwrap_or(outer_width);
              let border_box_width = outer_width.max(0.0);
              updated.size.width = Dimension::length(self.border_box_to_taffy_style_size(
                border_box_width,
                style,
                Axis::Horizontal,
                base_width,
              ));
              taffy
                .set_style(root_id, updated)
                .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
            }
          }
        }
      }
    }

    self.patch_root_calc_percentage_tracks(&mut taffy, root_id, style, &intrinsic_constraints)?;

    // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block height
    // is not definite for in-flow elements.
    //
    // `GridFormattingContext` overrides `compute_intrinsic_block_size` to avoid constructing full
    // fragment trees, but still relies on Taffy for sizing. Normalizing percent heights here keeps
    // `height:100%` from collapsing to 0px during intrinsic measurement, which would in turn
    // collapse `fr` tracks (e.g. WIRED's sticky nav rows).
    if intrinsic_constraints.used_border_box_height.is_none()
      && intrinsic_constraints.height().is_none()
      && style
        .height
        .as_ref()
        .is_some_and(crate::style::values::Length::has_percentage)
      && !matches!(
        style.position,
        crate::style::position::Position::Absolute | crate::style::position::Position::Fixed
      )
    {
      if let Ok(existing) = taffy.style(root_id) {
        if existing.size.height.tag() == taffy::style::CompactLength::PERCENT_TAG {
          let mut updated = existing.clone();
          updated.size.height = Dimension::auto();
          taffy
            .set_style(root_id, updated)
            .map_err(|e| LayoutError::MissingContext(format!("Taffy error: {:?}", e)))?;
        }
      }
    }

    // Convert constraints to Taffy available space. The intrinsic sizing probes for block-size
    // mirror the default `FormattingContext::compute_intrinsic_block_size` implementation:
    // intrinsic available space in the inline axis, indefinite in the block axis.
    let available_space = taffy::geometry::Size {
      width: match intrinsic_constraints.available_width {
        CrateAvailableSpace::Definite(w) => taffy::style::AvailableSpace::Definite(w),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
      height: match intrinsic_constraints.available_height {
        CrateAvailableSpace::Definite(h) => taffy::style::AvailableSpace::Definite(h),
        CrateAvailableSpace::Indefinite => taffy::style::AvailableSpace::MaxContent,
        CrateAvailableSpace::MinContent => taffy::style::AvailableSpace::MinContent,
        CrateAvailableSpace::MaxContent => taffy::style::AvailableSpace::MaxContent,
      },
    };

    let allow_stretch_block_size_override =
      grid_container_allows_stretch_block_size_override(style, &intrinsic_constraints);

    record_taffy_invocation(TaffyAdapterKind::Grid);
    let taffy_perf_enabled = crate::layout::taffy_integration::taffy_perf_enabled();
    let taffy_compute_start = taffy_perf_enabled.then(std::time::Instant::now);
    let cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>> = active_deadline()
      .filter(|deadline| deadline.is_enabled())
      .map(|_| Arc::new(|| check_active(RenderStage::Layout).is_err()) as _);
    let compute_result = taffy.compute_layout_with_measure_and_cancel(
      root_id,
      available_space,
      {
        let factory = self.factory.clone();
        let viewport_size = self.viewport_size;
        let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
        move |known_dimensions,
              available_space,
              node_id,
              node_context,
              taffy_style: &taffy::style::Style| {
          if taffy_perf_enabled {
            record_taffy_measure_call(TaffyAdapterKind::Grid);
          }
          if node_id == root_id {
            return taffy::tree::MeasureOutput::ZERO;
          }
          let fallback_size = |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
            known.unwrap_or(match avail_dim {
              taffy::style::AvailableSpace::Definite(v) => v,
              _ => 0.0,
            })
          };
          let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
            return taffy::tree::MeasureOutput::ZERO;
          };
          let box_node = unsafe { &*node_ptr };
          let style_override = style_override_for(box_node.id);
          let style: &ComputedStyle = style_override
            .as_deref()
            .unwrap_or_else(|| box_node.style.as_ref());
          let wants_baseline_y = taffy_style.align_self == Some(taffy::style::AlignItems::Baseline);
          let wants_baseline_x = taffy_style.justify_self
            == Some(taffy::style::AlignItems::Baseline)
            && crate::style::block_axis_is_horizontal(style.writing_mode);
          let mut available_space = available_space;
          if known_dimensions.width.is_none()
            && matches!(
              available_space.width,
              taffy::style::AvailableSpace::Definite(_)
            )
            && physical_width_is_auto(style)
          {
            available_space.width = taffy::style::AvailableSpace::MaxContent;
          }

          let fc_type = box_node
            .formatting_context()
            .unwrap_or(FormattingContextType::Block);
          let drop_available_height = drop_definite_available_height_for_measure(
            box_node,
            fc_type,
            known_dimensions.height,
            available_space.height,
            allow_stretch_block_size_override,
          );
          let (key, known_dimensions, available_space) = MeasureKey::new_with_snapped_sizes(
            box_node,
            known_dimensions,
            available_space,
            viewport_size,
            drop_available_height,
          );
          if let Some(output) = cache.get(&key) {
            return *output;
          }
          if let Some(output) = grid_measure_size_cache_lookup(&key) {
            cache.insert(key, output);
            return output;
          }
          let fc: std::sync::Arc<dyn FormattingContext> =
            if matches!(fc_type, FormattingContextType::Block) {
              std::sync::Arc::new(
                BlockFormattingContext::for_independent_context_root_with_factory(factory.clone()),
              )
            } else {
              factory.get(fc_type)
            };
          let inline_is_horizontal =
            crate::style::inline_axis_is_horizontal(box_node.style.writing_mode);
          let intrinsic_physical_width = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_inline_size(box_node, mode)
            } else {
              fc.compute_intrinsic_block_size(box_node, mode)
            }
          };
          let intrinsic_physical_height = |mode: IntrinsicSizingMode| -> Result<f32, LayoutError> {
            if inline_is_horizontal {
              fc.compute_intrinsic_block_size(box_node, mode)
            } else {
              fc.compute_intrinsic_inline_size(box_node, mode)
            }
          };

          // Taffy expresses intrinsic sizing probes via `AvailableSpace::{MinContent,MaxContent}`
          // on the physical axis being queried. Unlike the `FormattingContext` APIs, these probes
          // are not expressed in the box's logical axes, so we must always answer them in physical
          // width/height regardless of `writing-mode`.
          let mut intrinsic_width: Option<f32> = None;
          if known_dimensions.width.is_none() {
            intrinsic_width = match available_space.width {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_width(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          let mut intrinsic_height: Option<f32> = None;
          if known_dimensions.height.is_none() {
            intrinsic_height = match available_space.height {
              taffy::style::AvailableSpace::MinContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MinContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              taffy::style::AvailableSpace::MaxContent => Some(
                match intrinsic_physical_height(IntrinsicSizingMode::MaxContent) {
                  Ok(size) => size,
                  Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                  Err(_) => 0.0,
                },
              ),
              _ => None,
            };
          }

          if !(wants_baseline_y || wants_baseline_x)
            && (intrinsic_width.is_some() || intrinsic_height.is_some())
          {
            let percentage_base = match available_space.width {
              taffy::style::AvailableSpace::Definite(w) => w,
              _ => 0.0,
            };
            let (inset_w, inset_h) =
              GridFormattingContext::taffy_measure_insets_px(taffy_style, percentage_base);
            let width = intrinsic_width
              .map(|border_width| (border_width - inset_w).max(0.0))
              .unwrap_or_else(|| {
                fallback_size(known_dimensions.width, available_space.width).max(0.0)
              });

            let height = if let Some(border_height) = intrinsic_height {
              (border_height - inset_h).max(0.0)
            } else {
              // See `compute_intrinsic_size`'s measure closure for rationale: width probes must
              // return a content-based intrinsic block size rather than leaking Taffy-internal
              // definite-height guesses (or our 0px sentinel for "indefinite").
              let mode = match available_space.width {
                taffy::style::AvailableSpace::MinContent => IntrinsicSizingMode::MinContent,
                taffy::style::AvailableSpace::MaxContent => IntrinsicSizingMode::MaxContent,
                _ => IntrinsicSizingMode::MaxContent,
              };
              let border_block_size = match intrinsic_physical_height(mode) {
                Ok(size) => size,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => 0.0,
              };
              (border_block_size - inset_h).max(0.0)
            };
            let size = taffy::geometry::Size { width, height };
            let output = taffy::tree::MeasureOutput::from_size(size);
            cache.insert(key, output);
            return output;
          }
          let constraints = constraints_from_taffy(
            viewport_size,
            known_dimensions,
            available_space,
            style.writing_mode,
            None,
          );
          let fragment = match fc.layout(box_node, &constraints) {
            Ok(fragment) => fragment,
            Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
            Err(_) => return taffy::tree::MeasureOutput::ZERO,
          };
          let percentage_base = match available_space.width {
            taffy::style::AvailableSpace::Definite(w) => w,
            _ => constraints
              .width()
              .unwrap_or_else(|| fragment.bounds.width()),
          };
          let content_size = GridFormattingContext::content_box_size_for_taffy_style(
            Size::new(fragment.bounds.width(), fragment.bounds.height()),
            taffy_style,
            percentage_base,
          );
          let size = taffy::geometry::Size {
            width: content_size.width.max(0.0),
            height: content_size.height.max(0.0),
          };
          let mut baseline_deadline_counter = 0usize;
          let baseline_y = if wants_baseline_y {
            match first_baseline_offset(&fragment, &mut baseline_deadline_counter) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let baseline_x = if wants_baseline_x {
            match first_baseline_offset_x(
              &fragment,
              style.writing_mode,
              &mut baseline_deadline_counter,
            ) {
              Ok(baseline) => baseline,
              Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
              Err(_) => None,
            }
          } else {
            None
          };
          let output = taffy::tree::MeasureOutput::from_size_and_baselines(
            size,
            taffy::geometry::Point {
              x: baseline_x,
              y: baseline_y,
            },
          );
          grid_measure_size_cache_store(key, output);
          cache.insert(key, output);
          output
        }
      },
      cancel,
      TAFFY_ABORT_CHECK_STRIDE,
    );
    if let Some(start) = taffy_compute_start {
      record_taffy_compute(TaffyAdapterKind::Grid, start.elapsed());
    }
    compute_result.map_err(|e| match e {
      taffy::TaffyError::LayoutAborted => match active_deadline() {
        Some(deadline) => LayoutError::Timeout {
          elapsed: deadline.elapsed(),
        },
        None => LayoutError::MissingContext("Taffy layout aborted".to_string()),
      },
      _ => LayoutError::MissingContext(format!("Taffy compute error: {:?}", e)),
    })?;

    let layout = taffy
      .layout(root_id)
      .map_err(|e| LayoutError::MissingContext(format!("Taffy layout error: {:?}", e)))?;

    let mut result = if inline_is_horizontal {
      layout.size.height
    } else {
      layout.size.width
    }
    .max(0.0);

    let eps = 0.01;
    if result <= eps || !result.is_finite() {
      let mut deadline_counter = 0usize;
      if let Ok(size) = Self::taffy_layout_subtree_size(&taffy, root_id, &mut deadline_counter) {
        let subtree = if inline_is_horizontal {
          size.height
        } else {
          size.width
        };
        if subtree.is_finite() && subtree > result {
          result = subtree;
        }
      }
    }

    intrinsic_block_cache_store(box_node, mode, result);
    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{DiagnosticsLevel, FastRender, FastRenderConfig, RenderOptions};
  use crate::debug::runtime;
  use crate::layout::contexts::block::BlockFormattingContext;
  use crate::layout::formatting_context::{
    set_fragmentainer_axes_hint, set_fragmentainer_block_offset_hint,
    set_fragmentainer_block_size_hint,
  };
  use crate::style::display::FormattingContextType;
  use crate::style::properties::apply_container_type_implied_containment;
  use crate::style::properties::apply_content_visibility_implied_containment;
  use crate::style::types::AlignItems;
  use crate::style::types::AspectRatio;
  use crate::style::types::BorderStyle;
  use crate::style::types::ContentVisibility;
  use crate::style::types::Direction;
  use crate::style::types::GridAutoFlow;
  use crate::style::types::GridTrack;
  use crate::style::types::JustifyContent;
  use crate::style::types::Overflow;
  use crate::style::types::ScrollbarWidth;
  use crate::style::types::WhiteSpace;
  use crate::style::types::WordBreak;
  use crate::style::types::WritingMode;
  use crate::style::values::{CalcLength, LengthUnit};
  use crate::text::font_db::FontConfig;
  use crate::tree::box_tree::BoxTree;
  use std::collections::HashMap;
  use std::sync::Arc;

  mod placement_test;

  fn make_grid_style() -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    Arc::new(style)
  }

  fn make_item_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  #[test]
  fn grid_trim_auto_block_size_preserves_explicit_tracks() {
    let fc = GridFormattingContext::new();
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::HorizontalTb;
    style.direction = Direction::Ltr;
    let style = Arc::new(style);

    let child_style = Arc::new(ComputedStyle::default());
    let child = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 10.0, 10.0, 10.0),
      vec![],
      child_style,
    );

    // Start with an exaggerated container height to simulate Taffy returning the viewport height
    // for an auto-sized grid with indefinite available block space.
    let mut fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 600.0),
      vec![child],
      style.clone(),
    );
    fragment.grid_tracks = Some(Arc::new(GridTrackRanges {
      rows: vec![(0.0, 10.0), (10.0, 20.0)],
      columns: Vec::new(),
    }));

    let constraints =
      LayoutConstraints::new(CrateAvailableSpace::Definite(100.0), CrateAvailableSpace::Indefinite);
    let _hint_guard = set_fragmentainer_block_size_hint(None);
    fc.maybe_trim_auto_block_size_in_scrollable_layout(style.as_ref(), &constraints, &mut fragment);

    assert!((fragment.bounds.height() - 20.0).abs() < 1e-6);
    assert!((fragment.children[0].bounds.y() - 10.0).abs() < 1e-6);
    assert_eq!(
      fragment
        .grid_tracks
        .as_deref()
        .expect("grid tracks should remain present")
        .rows,
      vec![(0.0, 10.0), (10.0, 20.0)]
    );
  }

  #[test]
  fn grid_resolve_length_px_resolves_root_font_relative_units() {
    let fc = GridFormattingContext::new();
    let mut style = ComputedStyle::default();
    style.font_size = 20.0;
    style.root_font_size = 10.0;

    let rch = Length::new(1.0, LengthUnit::Rch);
    let resolved_rch = fc.resolve_length_px(&rch, &style).expect("resolve rch");
    assert!((resolved_rch - 5.0).abs() < 1e-6);

    let rlh = Length::new(1.0, LengthUnit::Rlh);
    let resolved_rlh = fc.resolve_length_px(&rlh, &style).expect("resolve rlh");
    assert!((resolved_rlh - 12.0).abs() < 1e-6);
  }

  fn make_grid_style_with_tracks(cols: Vec<GridTrack>, rows: Vec<GridTrack>) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = cols;
    style.grid_template_rows = rows;
    Arc::new(style)
  }

  fn make_text_item(text: &str, font_size: f32) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.font_size = font_size;
    let style = Arc::new(style);
    let text_child = BoxNode::new_text(style.clone(), text.to_string());
    BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child])
  }

  fn make_text_item_with_writing_mode(
    text: &str,
    font_size: f32,
    writing_mode: WritingMode,
    width: f32,
  ) -> BoxNode {
    let mut style = ComputedStyle::default();
    style.font_size = font_size;
    style.writing_mode = writing_mode;
    style.width = Some(Length::px(width));
    let style = Arc::new(style);
    let text_child = BoxNode::new_text(style.clone(), text.to_string());
    BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child])
  }

  fn find_block_fragment<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
    fn walk<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
      if matches!(
        node.content,
        FragmentContent::Block {
          box_id: Some(id), ..
        } if id == box_id
      ) {
        return Some(node);
      }
      node.children.iter().find_map(|child| walk(child, box_id))
    }

    walk(fragment, box_id).unwrap_or_else(|| panic!("missing fragment for box_id={box_id}"))
  }

  fn content_visibility_test_guard() -> runtime::ThreadRuntimeTogglesGuard {
    runtime::set_thread_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(HashMap::from(
      [(
        "FASTR_CONTENT_VISIBILITY_AUTO_MARGIN_PX".to_string(),
        "0".to_string(),
      )],
    ))))
  }

  #[test]
  fn grid_scrollable_available_space_safeguard_horizontal_tb_drops_height() {
    let _hint_guard = crate::layout::formatting_context::set_fragmentainer_block_size_hint(None);

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::HorizontalTb;
    // Scrollable along the block axis (physical Y for horizontal-tb).
    style.overflow_y = Overflow::Auto;
    style.width = None;
    style.width_keyword = None;
    style.height = None;
    style.height_keyword = None;

    let constraints = LayoutConstraints::definite(100.0, 200.0);
    let available = taffy_available_space_for_grid_container(&style, &constraints);

    assert!(matches!(
      available.width,
      taffy::style::AvailableSpace::Definite(w) if w == 100.0
    ));
    assert!(matches!(
      available.height,
      taffy::style::AvailableSpace::MaxContent
    ));
  }

  #[test]
  fn grid_scrollable_available_space_safeguard_vertical_writing_modes_drop_width() {
    let _hint_guard = crate::layout::formatting_context::set_fragmentainer_block_size_hint(None);

    let writing_modes = [
      WritingMode::VerticalRl,
      WritingMode::VerticalLr,
      WritingMode::SidewaysRl,
      WritingMode::SidewaysLr,
    ];

    let constraints = LayoutConstraints::definite(100.0, 200.0);
    for writing_mode in writing_modes {
      let mut style = ComputedStyle::default();
      style.display = CssDisplay::Grid;
      style.writing_mode = writing_mode;
      // Scrollable along the block axis (physical X for vertical/sideways writing modes).
      style.overflow_x = Overflow::Auto;
      style.width = None;
      style.width_keyword = None;
      style.height = None;
      style.height_keyword = None;

      let available = taffy_available_space_for_grid_container(&style, &constraints);

      assert!(matches!(
        available.width,
        taffy::style::AvailableSpace::MaxContent
      ));
      assert!(matches!(
        available.height,
        taffy::style::AvailableSpace::Definite(h) if h == 200.0
      ));
    }
  }

  #[test]
  fn grid_scrollable_percentage_block_tracks_do_not_resolve_against_definite_available_space_horizontal_tb(
  ) {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::HorizontalTb;
    style.overflow_y = Overflow::Auto;
    style.grid_template_columns = vec![GridTrack::Length(Length::px(10.0))];
    // Percentages should behave like `auto` when the grid's block size is indefinite.
    style.grid_template_rows = vec![
      GridTrack::Length(Length::percent(50.0)),
      GridTrack::Length(Length::percent(50.0)),
    ];
    let style = Arc::new(style);

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(10.0));
    item_style.height_keyword = None;
    let item_style = Arc::new(item_style);
    let child1 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert!(
      (fragment.bounds.height() - 20.0).abs() < 0.5,
      "expected grid height to size to content (20px), got {:.2}",
      fragment.bounds.height()
    );
  }

  #[test]
  fn grid_scrollable_percentage_block_tracks_do_not_resolve_against_definite_available_space_vertical_writing_modes(
  ) {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let writing_modes = [
      WritingMode::VerticalRl,
      WritingMode::VerticalLr,
      WritingMode::SidewaysRl,
      WritingMode::SidewaysLr,
    ];

    for writing_mode in writing_modes {
      let mut style = ComputedStyle::default();
      style.display = CssDisplay::Grid;
      style.writing_mode = writing_mode;
      style.overflow_x = Overflow::Auto;
      style.grid_template_columns = vec![GridTrack::Length(Length::px(10.0))];
      // In vertical writing modes the block axis maps to the physical X axis. The scroll-layout
      // safeguard must therefore drop the *available width* so percentages behave like `auto`.
      style.grid_template_rows = vec![
        GridTrack::Length(Length::percent(50.0)),
        GridTrack::Length(Length::percent(50.0)),
      ];
      let style = Arc::new(style);

      let mut item_style = ComputedStyle::default();
      item_style.width = Some(Length::px(10.0));
      item_style.width_keyword = None;
      let item_style = Arc::new(item_style);
      let child1 = BoxNode::new_block(item_style.clone(), FormattingContextType::Block, vec![]);
      let child2 = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);

      let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);
      let constraints = LayoutConstraints::definite(200.0, 200.0);
      let fragment = fc.layout(&grid, &constraints).unwrap();

      assert!(
        (fragment.bounds.width() - 20.0).abs() < 0.5,
        "expected {writing_mode:?} grid width to size to content (20px), got {:.2}",
        fragment.bounds.width()
      );
    }
  }

  #[test]
  fn grid_item_percentage_height_resolves_against_definite_grid_area_block_size() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // A single-row grid with a definite track size. The grid area height is therefore definite, so
    // percentage `height` values on grid items should resolve against it.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.writing_mode = WritingMode::HorizontalTb;
    grid_style.align_items = AlignItems::Center;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
    ];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
    let grid_style = Arc::new(grid_style);

    let mut fixed_style = ComputedStyle::default();
    fixed_style.height = Some(Length::px(100.0));
    fixed_style.height_keyword = None;
    let fixed_style = Arc::new(fixed_style);
    let mut fixed_item = BoxNode::new_block(fixed_style, FormattingContextType::Block, vec![]);
    fixed_item.id = 901;

    // This item has `height: 100%` and a small intrinsic child. If percentage heights are treated
    // as `auto` (incorrect for definite grid areas), its height collapses to the intrinsic child
    // height and it gets centered with an offset. Correct behaviour is to resolve to the full grid
    // area height (100px), producing no vertical offset under `align-items: center`.
    let mut percent_style = ComputedStyle::default();
    percent_style.height = Some(Length::percent(100.0));
    percent_style.height_keyword = None;
    percent_style.align_self = Some(AlignItems::Center);
    let percent_style = Arc::new(percent_style);

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    let child_style = Arc::new(child_style);
    let child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);

    let mut percent_item =
      BoxNode::new_block(percent_style, FormattingContextType::Block, vec![child]);
    percent_item.id = 902;

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![fixed_item, percent_item],
    );
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
      .expect("layout should succeed");

    let percent_fragment = find_block_fragment(&fragment, 902);
    assert!(
      percent_fragment.bounds.height().is_finite(),
      "expected finite percent item height"
    );
    assert!(
      (percent_fragment.bounds.height() - 100.0).abs() < 0.5,
      "expected percent item to fill the 100px grid area, got {:.2}",
      percent_fragment.bounds.height()
    );
    assert!(
      percent_fragment.bounds.y().abs() < 0.5,
      "expected percent item y to be aligned to the grid area's start (0px), got {:.2}",
      percent_fragment.bounds.y()
    );
  }

  #[test]
  fn grid_item_height_100_percent_fills_auto_row_grid_area() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // Auto row sizing (the default) is content-based. Percentage `height` values on grid items
    // still resolve against the resolved grid area size, matching Chrome's behaviour on
    // manjaro.org where a `h-full` grid item should fill the row and not be vertically centered.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.writing_mode = WritingMode::HorizontalTb;
    grid_style.align_items = AlignItems::Center;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
    ];
    // Leave `grid_template_rows` empty so the implicit row uses `auto` sizing.
    let grid_style = Arc::new(grid_style);

    let mut tall_style = ComputedStyle::default();
    tall_style.height = Some(Length::px(100.0));
    tall_style.height_keyword = None;
    let tall_style = Arc::new(tall_style);
    let mut tall_item = BoxNode::new_block(tall_style, FormattingContextType::Block, vec![]);
    tall_item.id = 910;

    let mut percent_style = ComputedStyle::default();
    percent_style.height = Some(Length::percent(100.0));
    percent_style.height_keyword = None;
    percent_style.align_self = Some(AlignItems::Center);
    let percent_style = Arc::new(percent_style);

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::px(20.0));
    child_style.height_keyword = None;
    let child_style = Arc::new(child_style);
    let child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);

    let mut percent_item =
      BoxNode::new_block(percent_style, FormattingContextType::Block, vec![child]);
    percent_item.id = 911;

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![tall_item, percent_item],
    );
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
      .expect("layout should succeed");

    let percent_fragment = find_block_fragment(&fragment, 911);
    assert!(
      (percent_fragment.bounds.height() - 100.0).abs() < 0.5,
      "expected 100% height item to fill the auto row (100px), got {:.2}",
      percent_fragment.bounds.height()
    );
    assert!(
      percent_fragment.bounds.y().abs() < 0.5,
      "expected 100% height item to align to row start (0px), got {:.2}",
      percent_fragment.bounds.y()
    );
  }

  #[test]
  fn grid_continuation_available_block_size_horizontal_tb() {
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr),
    ));
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let bounds = Rect::from_xywh(0.0, 150.0, 10.0, 10.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 50.0).abs() < 0.001);
  }

  #[test]
  fn grid_continuation_available_block_size_first_fragment_shorter_than_fragmentainer() {
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr),
    ));
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 80.0);
    let bounds = Rect::from_xywh(0.0, 90.0, 10.0, 10.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 90.0).abs() < 0.001);
  }

  #[test]
  fn grid_continuation_available_block_size_vertical_rl_negative_progression() {
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
    ));
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 300.0);
    // Container block-size is physical width (200px). With negative block progression, a box at
    // x=60..70 starts 130px into the flow coordinate system (200 - 60 - 10).
    let bounds = Rect::from_xywh(60.0, 0.0, 10.0, 20.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 200.0)
      .expect("expected continuation fragment");
    assert!((remaining - 70.0).abs() < 0.001);
  }

  #[test]
  fn grid_continuation_available_block_size_rejects_non_finite_inputs() {
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let bounds = Rect::from_xywh(0.0, 150.0, 10.0, 10.0);

    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr),
    ));

    assert_eq!(
      ctx.grid_item_continuation_available_block_size(
        Rect::from_xywh(0.0, f32::NAN, 10.0, 10.0),
        &constraints,
        1000.0,
      ),
      None
    );
    assert_eq!(
      ctx.grid_item_continuation_available_block_size(
        Rect::from_xywh(0.0, f32::INFINITY, 10.0, 10.0),
        &constraints,
        1000.0,
      ),
      None
    );
    assert_eq!(
      ctx.grid_item_continuation_available_block_size(bounds, &constraints, f32::INFINITY),
      None
    );

    {
      let _bad_hint = set_fragmentainer_block_size_hint(Some(f32::NAN));
      assert_eq!(
        ctx.grid_item_continuation_available_block_size(bounds, &constraints, 1000.0),
        None
      );
    }
    {
      let _bad_hint = set_fragmentainer_block_size_hint(Some(f32::INFINITY));
      assert_eq!(
        ctx.grid_item_continuation_available_block_size(bounds, &constraints, 1000.0),
        None
      );
    }
  }

  #[test]
  fn grid_item_continuation_relayout_reduces_physical_width_in_vertical_writing_mode() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalLr, Direction::Ltr),
    ));

    // Force the parallel root-children conversion path so this regression covers continuation
    // detection under `apply_grid_axis_mirroring` (vertical writing modes + negative progression).
    let fc = GridFormattingContext::new()
      .with_parallelism(LayoutParallelism::enabled(3).with_max_threads(Some(2)));

    // In vertical writing modes the fragmentation block axis can be physical X. Place the target
    // item in a later block track so it starts in a continuation fragment (x >= 100px), then
    // ensure the relayout path constrains the *width* (not height) to the remaining space.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.writing_mode = WritingMode::VerticalLr;
    // Rows map to the physical horizontal axis in vertical writing modes.
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(90.0)),
    ];
    // One inline track so the item's physical height is stable.
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(20.0))];
    let grid_style = Arc::new(grid_style);

    let mut item_a = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_a.id = 501;
    let mut item_b = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_b.id = 502;

    let mut target_style = ComputedStyle::default();
    target_style.writing_mode = WritingMode::VerticalLr;
    target_style.width = None;
    target_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
    let mut target =
      BoxNode::new_block(Arc::new(target_style), FormattingContextType::Block, vec![]);
    target.id = 503;

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_a, item_b, target],
    );

    // The fragmentainer block-size hint is 100px. The third row starts at x=120px, so the target
    // item begins 20px into the continuation fragment, leaving 80px remaining.
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(500.0, 500.0))
      .expect("layout should succeed");

    let target_fragment = find_block_fragment(&fragment, 503);
    assert!(
      (target_fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected continuation relayout to clamp item width to remaining fragmentainer space (80px), got {:.2}",
      target_fragment.bounds.width()
    );
  }

  #[test]
  fn grid_item_continuation_relayout_reduces_physical_width_in_vertical_rl_negative_progression() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
    ));

    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // `writing-mode: vertical-rl` has a horizontal block axis with negative progression. Place the
    // target item in a later block track so its *block-start* edge (the right edge) begins 20px
    // into the continuation fragment (flow position 120px with a 100px fragmentainer), leaving 80px
    // remaining.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.writing_mode = WritingMode::VerticalRl;
    grid_style.width = Some(Length::px(210.0));
    // One inline track so the item's physical height is stable.
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(20.0))];
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(90.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let mut item_a = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_a.id = 511;
    let mut item_b = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_b.id = 512;

    let mut target_style = ComputedStyle::default();
    target_style.writing_mode = WritingMode::VerticalRl;
    target_style.width = None;
    target_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
    let mut target =
      BoxNode::new_block(Arc::new(target_style), FormattingContextType::Block, vec![]);
    target.id = 513;

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_a, item_b, target],
    );

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(500.0, 500.0))
      .expect("layout should succeed");

    let target_fragment = find_block_fragment(&fragment, 513);
    let container_width = fragment.bounds.width();
    let block_start = container_width - target_fragment.bounds.x() - target_fragment.bounds.width();
    assert!(
      (target_fragment.bounds.width() - 80.0).abs() < 0.5,
      "expected continuation relayout to clamp item width to remaining fragmentainer space (80px), got width={:.2} x={:.2} container_width={:.2} block_start={:.2}",
      target_fragment.bounds.width(),
      target_fragment.bounds.x(),
      container_width,
      block_start,
    );
  }

  #[test]
  fn grid_container_fragmentainer_offset_hint_propagates_in_vertical_rl_negative_progression() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
    ));

    // Offset the grid container 60px into the fragmentainer's block axis. With a 100px
    // fragmentainer, that leaves 40px in the first fragment, so an item starting at 120px into the
    // grid's flow will land 80px into the continuation fragment, leaving 20px remaining.
    let fc = BlockFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut spacer_style = ComputedStyle::default();
    spacer_style.writing_mode = WritingMode::VerticalRl;
    spacer_style.width = Some(Length::px(60.0));
    spacer_style.height = Some(Length::px(20.0));
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = 521;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.writing_mode = WritingMode::VerticalRl;
    // Make the grid container's block size (physical width) definite so Taffy produces stable
    // non-zero item widths. This keeps the regression focused on fragmentainer offset propagation
    // rather than max-content sizing behaviour.
    grid_style.width = Some(Length::px(210.0));
    // One inline track so the item's physical height is stable.
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(20.0))];
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(60.0)),
      GridTrack::Length(Length::px(90.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let mut item_a = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_a.id = 522;
    let mut item_b = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    item_b.id = 523;

    let mut target_style = ComputedStyle::default();
    target_style.writing_mode = WritingMode::VerticalRl;
    target_style.width = None;
    target_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
    let mut target =
      BoxNode::new_block(Arc::new(target_style), FormattingContextType::Block, vec![]);
    target.id = 524;

    let mut grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_a, item_b, target],
    );
    grid.id = 525;

    let mut parent_style = ComputedStyle::default();
    parent_style.writing_mode = WritingMode::VerticalRl;
    let mut parent = BoxNode::new_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![spacer, grid],
    );
    parent.id = 520;

    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite(500.0, 500.0))
      .expect("layout should succeed");

    let grid_fragment = find_block_fragment(&fragment, 525);
    let target_fragment = find_block_fragment(&fragment, 524);
    let parent_width = fragment.bounds.width();
    let grid_width = grid_fragment.bounds.width();
    let grid_block_start = parent_width - grid_fragment.bounds.x() - grid_width;
    let target_block_start =
      grid_width - target_fragment.bounds.x() - target_fragment.bounds.width();
    assert!(
      (target_fragment.bounds.width() - 20.0).abs() < 0.5,
      "expected continuation relayout to clamp item width to remaining fragmentainer space (20px), got width={:.2} target_bounds={:?} grid_bounds={:?} grid_block_start={:.2} target_block_start={:.2}",
      target_fragment.bounds.width(),
      target_fragment.bounds,
      grid_fragment.bounds,
      grid_block_start,
      target_block_start,
    );
  }

  #[test]
  fn grid_item_continuation_available_block_size_accounts_for_fragmentainer_offset() {
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::HorizontalTb, Direction::Ltr),
    ));
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(200.0, 1000.0);
    let bounds = Rect::from_xywh(0.0, 160.0, 10.0, 10.0);

    let _offset = set_fragmentainer_block_offset_hint(0.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 40.0).abs() < 0.001);

    drop(_offset);
    let _offset = set_fragmentainer_block_offset_hint(60.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 80.0).abs() < 0.001);
  }

  #[test]
  fn grid_item_continuation_available_block_size_accounts_for_fragmentainer_offset_vertical_rl() {
    let _block_hint = set_fragmentainer_block_size_hint(Some(100.0));
    let _axes_hint = set_fragmentainer_axes_hint(Some(
      FragmentAxes::from_writing_mode_and_direction(WritingMode::VerticalRl, Direction::Ltr),
    ));
    let ctx = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(1000.0, 200.0);
    // Container block-size is physical width (1000px). With negative block progression, a box at
    // x=830..840 starts 160px into the flow coordinate system (1000 - 830 - 10).
    let bounds = Rect::from_xywh(830.0, 0.0, 10.0, 10.0);

    let _offset = set_fragmentainer_block_offset_hint(0.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 40.0).abs() < 0.001);

    drop(_offset);
    let _offset = set_fragmentainer_block_offset_hint(60.0);
    let remaining = ctx
      .grid_item_continuation_available_block_size(bounds, &constraints, 1000.0)
      .expect("expected continuation fragment");
    assert!((remaining - 80.0).abs() < 0.001);
  }

  #[test]
  fn grid_content_visibility_auto_in_view_does_not_skip() {
    fn has_text(fragment: &FragmentNode) -> bool {
      matches!(fragment.content, FragmentContent::Text { .. })
        || fragment.children.iter().any(has_text)
    }

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    container_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];
    let container_style = Arc::new(container_style);

    let mut item_style = ComputedStyle::default();
    item_style.content_visibility = ContentVisibility::Auto;
    item_style.contain_intrinsic_height.length = Some(Length::px(30.0));
    crate::style::properties::apply_container_type_implied_containment(&mut item_style);
    crate::style::properties::apply_content_visibility_implied_containment(&mut item_style);
    let item_style = Arc::new(item_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = CssDisplay::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let item = BoxNode::new_block(
      item_style,
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style, "hello".to_string())],
    );
    let container = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);

    let gc = GridFormattingContext::with_viewport(Size::new(200.0, 200.0));
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = gc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    assert_eq!(fragment.children.len(), 1);
    assert!(
      has_text(&fragment.children[0]),
      "content-visibility:auto in viewport must not skip layout"
    );
  }

  #[test]
  fn contain_intrinsic_size_auto_uses_remembered_size_when_skipped_in_grid() {
    let _intrinsic_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let _toggles = content_visibility_test_guard();

    fn has_text(fragment: &FragmentNode) -> bool {
      matches!(fragment.content, FragmentContent::Text { .. })
        || fragment.children.iter().any(has_text)
    }

    let spacer_id = 301usize;
    let auto_id = 302usize;
    let after_id = 303usize;

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    container_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto, GridTrack::Auto];
    let container_style = Arc::new(container_style);

    let mut spacer_style = ComputedStyle::default();
    spacer_style.height = Some(Length::px(200.0));
    let mut spacer =
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);
    spacer.id = spacer_id;

    let padding_top = 5.0;
    let padding_bottom = 7.0;
    let border_top = 2.0;
    let border_bottom = 3.0;

    let mut auto_style = ComputedStyle::default();
    auto_style.content_visibility = ContentVisibility::Auto;
    auto_style.padding_top = Length::px(padding_top);
    auto_style.padding_bottom = Length::px(padding_bottom);
    auto_style.border_top_width = Length::px(border_top);
    auto_style.border_bottom_width = Length::px(border_bottom);
    auto_style.border_top_style = BorderStyle::Solid;
    auto_style.border_bottom_style = BorderStyle::Solid;
    apply_container_type_implied_containment(&mut auto_style);
    apply_content_visibility_implied_containment(&mut auto_style);
    let auto_style = Arc::new(auto_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = CssDisplay::Inline;
    text_style.font_size = 16.0;
    let text_style = Arc::new(text_style);

    let mut auto_item = BoxNode::new_block(
      auto_style,
      FormattingContextType::Block,
      vec![BoxNode::new_text(text_style, "remembered".to_string())],
    );
    auto_item.id = auto_id;

    let mut after_style = ComputedStyle::default();
    after_style.height = Some(Length::px(50.0));
    let mut after_item =
      BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]);
    after_item.id = after_id;

    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Grid,
      vec![spacer, auto_item, after_item],
    );
    let constraints = LayoutConstraints::definite(200.0, 1000.0);

    let gc_large = GridFormattingContext::with_viewport(Size::new(200.0, 400.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_large = gc_large
      .layout(&container, &constraints)
      .expect("layout should succeed");
    let auto_fragment_large = find_block_fragment(&fragment_large, auto_id);
    assert!(
      has_text(auto_fragment_large),
      "expected first pass to fully lay out the content-visibility:auto item"
    );
    let remembered_border_box_height = auto_fragment_large.bounds.height();
    let after_y_large = find_block_fragment(&fragment_large, after_id).bounds.y();

    let gc_small = GridFormattingContext::with_viewport(Size::new(200.0, 100.0))
      .with_parallelism(LayoutParallelism::disabled());
    let fragment_small = gc_small
      .layout(&container, &constraints)
      .expect("layout should succeed");
    let auto_fragment_small = find_block_fragment(&fragment_small, auto_id);
    assert!(
      !has_text(auto_fragment_small),
      "expected second pass to skip the offscreen content-visibility:auto item"
    );
    assert!(
      auto_fragment_small.children.is_empty(),
      "expected skipped grid item to be represented by a placeholder fragment"
    );
    assert!(
      (auto_fragment_small.bounds.height() - remembered_border_box_height).abs() < 0.5,
      "expected placeholder height {:.1}, got {:.1}",
      remembered_border_box_height,
      auto_fragment_small.bounds.height()
    );

    let after_y_small = find_block_fragment(&fragment_small, after_id).bounds.y();
    assert!(
      (after_y_small - after_y_large).abs() < 0.5,
      "expected following grid item y to remain stable (first={after_y_large:.1}, second={after_y_small:.1})"
    );
  }

  #[test]
  fn grid_border_box_width_accounts_for_padding_and_border_with_explicit_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = Some(Length::px(100.0));
    style.width_keyword = None;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![]);

    // CSS `width` applies to the content box by default (`box-sizing: content-box`), so the used
    // border-box width includes padding and borders.
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(1000.0, 200.0))
      .unwrap();
    assert_eq!(fragment.bounds.width(), 124.0);
  }

  #[test]
  fn grid_auto_width_stretches_border_box_including_padding_and_border() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.padding_left = Length::px(10.0);
    style.padding_right = Length::px(10.0);
    style.border_left_style = crate::style::types::BorderStyle::Solid;
    style.border_right_style = crate::style::types::BorderStyle::Solid;
    style.border_left_width = Length::px(2.0);
    style.border_right_width = Length::px(2.0);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![]);

    // With `width: auto`, block-level boxes should stretch to fill the available inline size. The
    // used width is the border-box width, so padding/borders must be included in the total size.
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(124.0, 200.0))
      .unwrap();
    assert_eq!(fragment.bounds.width(), 124.0);
  }

  #[test]
  fn grid_measure_key_uses_box_id_when_available() {
    use taffy::style::AvailableSpace;

    let mut node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    node.id = 123;
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let available = taffy::geometry::Size {
      width: AvailableSpace::MaxContent,
      height: AvailableSpace::MaxContent,
    };

    let key = MeasureKey::new(&node, known, available, viewport, false);
    assert_eq!(key.box_id, 123);
  }

  #[test]
  fn grid_measure_key_canonicalizes_negative_zero() {
    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let available = taffy::geometry::Size {
      width: taffy::style::AvailableSpace::MaxContent,
      height: taffy::style::AvailableSpace::MaxContent,
    };

    let key_zero = MeasureKey::new(
      &node,
      taffy::geometry::Size {
        width: Some(0.0),
        height: None,
      },
      available,
      viewport,
      false,
    );
    let key_neg_zero = MeasureKey::new(
      &node,
      taffy::geometry::Size {
        width: Some(-0.0),
        height: None,
      },
      available,
      viewport,
      false,
    );

    assert_eq!(key_zero, key_neg_zero);
  }

  #[test]
  fn grid_intrinsic_width_probe_returns_nonzero_height() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut item = make_text_item("Flipped side", 16.0);
    item.id = 1;
    let node_ptr: *const BoxNode = &item;

    // Use a dummy node id for the measured item (only used for per-node key tracking).
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let node_id = taffy
      .new_leaf(taffy::style::Style::default())
      .expect("create leaf node");

    let known_dimensions = taffy::geometry::Size {
      width: None,
      height: Some(0.0),
    };
    let available_space = taffy::geometry::Size {
      width: taffy::style::AvailableSpace::MaxContent,
      height: taffy::style::AvailableSpace::Definite(0.0),
    };

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let auto_unskipped: FxHashSet<*const BoxNode> = FxHashSet::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let size = fc.measure_grid_item(
      node_ptr,
      node_id,
      known_dimensions,
      available_space,
      Some(260.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      size.size.height > 0.1,
      "expected intrinsic width probes to compute a non-zero height (got {size:?})"
    );
  }

  #[test]
  fn grid_intrinsic_width_probe_does_not_use_definite_available_height_as_height() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // Use a percentage height to prevent `drop_definite_available_height_for_measure` from
    // normalizing away the definite height, so this test exercises the intrinsic-width probe
    // early-return path with a non-trivial available height.
    let mut style = ComputedStyle::default();
    style.font_size = 16.0;
    style.height = Some(Length::percent(100.0));
    let style = Arc::new(style);
    let text_child = BoxNode::new_text(style.clone(), "Flipped side".to_string());
    let mut item = BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child]);
    item.id = 1;
    let node_ptr: *const BoxNode = &item;

    // Dummy node id for the measured item (only used for per-node key tracking).
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let node_id = taffy
      .new_leaf(taffy::style::Style::default())
      .expect("create leaf node");

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let auto_unskipped: FxHashSet<*const BoxNode> = FxHashSet::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let unconstrained = fc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MaxContent,
        height: taffy::style::AvailableSpace::Definite(0.0),
      },
      Some(260.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    let definite_height = fc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MaxContent,
        height: taffy::style::AvailableSpace::Definite(1040.0),
      },
      Some(260.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    let known_height = fc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: Some(1040.0),
      },
      taffy::geometry::Size {
        width: taffy::style::AvailableSpace::MaxContent,
        height: taffy::style::AvailableSpace::Definite(1040.0),
      },
      Some(260.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      unconstrained.size.height > 0.1,
      "expected intrinsic-width probe to compute a non-zero height (got {unconstrained:?})"
    );
    assert!(
      unconstrained.size.height < 200.0,
      "expected intrinsic-width probe height to stay content-sized (got {unconstrained:?})"
    );
    assert!(
      (definite_height.size.height - unconstrained.size.height).abs() < 0.5,
      "expected intrinsic-width probe height to be content-based even when a definite height is passed (got unconstrained={unconstrained:?} definite_height={definite_height:?})"
    );
    assert!(
      (known_height.size.height - unconstrained.size.height).abs() < 0.5,
      "expected intrinsic-width probe height to ignore known height (got unconstrained={unconstrained:?} known_height={known_height:?})"
    );
  }

  #[test]
  fn grid_intrinsic_height_probe_respects_definite_available_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // Construct a text-heavy item where the laid-out height depends strongly on the available width
    // (lots of wrap opportunities).
    let mut style = ComputedStyle::default();
    style.font_size = 16.0;
    let style = Arc::new(style);
    let text = std::iter::repeat("a")
      .take(200)
      .collect::<Vec<_>>()
      .join(" ");
    let text_child = BoxNode::new_text(style.clone(), text);
    let mut item = BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child]);
    item.id = 1;
    let node_ptr: *const BoxNode = &item;

    // Dummy node id for the measured item (only used for per-node key tracking).
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let node_id = taffy
      .new_leaf(taffy::style::Style::default())
      .expect("create leaf node");

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let auto_unskipped: FxHashSet<*const BoxNode> = FxHashSet::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let narrow = fc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      taffy::geometry::Size {
        width: taffy::style::AvailableSpace::Definite(50.0),
        height: taffy::style::AvailableSpace::MinContent,
      },
      Some(400.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    let wide = fc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      taffy::geometry::Size {
        width: taffy::style::AvailableSpace::Definite(400.0),
        height: taffy::style::AvailableSpace::MinContent,
      },
      Some(400.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      narrow.size.height > 0.1 && wide.size.height > 0.1,
      "expected intrinsic height probes to return non-zero heights (narrow={narrow:?} wide={wide:?})"
    );
    assert!(
      narrow.size.height > wide.size.height + 1.0,
      "expected intrinsic height probe to depend on the definite available width (narrow={narrow:?} wide={wide:?})"
    );
  }

  #[test]
  fn grid_measure_falls_back_to_descendant_span_when_layout_forced_to_zero_height() {
    // Regression test for cases where the measurement layout is forced to 0px (via known sizes)
    // even though the box contains non-zero in-flow descendants. Track sizing should still see the
    // descendant contribution so 1fr/auto rows don't collapse and clip overflow-hidden content
    // (wired.com sticky header).
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::px(80.0));
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let mut item_style = ComputedStyle::default();
    item_style.overflow_x = Overflow::Hidden;
    item_style.overflow_y = Overflow::Hidden;
    let mut item = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![child],
    );
    item.id = 1;
    let node_ptr: *const BoxNode = &item;

    // Dummy node id for the measured item (only used for per-node key tracking).
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let node_id = taffy
      .new_leaf(taffy::style::Style::default())
      .expect("create leaf node");

    // Force the measurement layout height to 0px while leaving the available height unconstrained.
    // This produces a fragment with a 0px border box but non-zero children.
    let known_dimensions = taffy::geometry::Size {
      width: Some(100.0),
      height: Some(0.0),
    };
    let available_space = taffy::geometry::Size {
      width: taffy::style::AvailableSpace::Definite(100.0),
      height: taffy::style::AvailableSpace::MaxContent,
    };

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let auto_unskipped: FxHashSet<*const BoxNode> = FxHashSet::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let size = fc.measure_grid_item(
      node_ptr,
      node_id,
      known_dimensions,
      available_space,
      Some(100.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy::style::Style::default(),
      &auto_unskipped,
      &fc.factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      size.size.height > 10.0,
      "expected measure to use descendant span when fragment bounds are 0 (got {size:?})"
    );
  }

  #[test]
  fn grid_percent_height_in_auto_height_flex_container_computes_to_auto() {
    // Regression test for `height: 100%` on a grid container inside an auto-height flex container.
    //
    // In CSS2.1 §10.5, percentage heights compute to `auto` when the containing block height is not
    // specified explicitly. In flex layout, the container can have a definite *available* height
    // (viewport), but an auto used height; the percentage must still behave like `auto` to avoid
    // collapsing sticky headers like wired.com's nav rows.
    use crate::layout::constraints::AvailableSpace;
    use crate::layout::contexts::flex::FlexFormattingContext;
    use crate::style::types::FlexDirection;

    let mut flex_style = ComputedStyle::default();
    flex_style.display = CssDisplay::Flex;
    flex_style.flex_direction = FlexDirection::Column;
    let flex_style = Arc::new(flex_style);

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.width = Some(Length::px(100.0));
    grid_style.height = Some(Length::percent(100.0));
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
    grid_style.grid_template_rows = vec![GridTrack::Fr(1.0)];
    let grid_style = Arc::new(grid_style);

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::px(80.0));
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![child]);
    let container = BoxNode::new_block(flex_style, FormattingContextType::Flex, vec![grid]);

    let fc = FlexFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let fragment = fc
      .layout(
        &container,
        &LayoutConstraints::new(
          AvailableSpace::Definite(100.0),
          AvailableSpace::Definite(500.0),
        )
        .with_used_border_box_size(Some(100.0), None),
      )
      .expect("layout succeeds");

    assert_eq!(fragment.children.len(), 1);
    let grid_fragment = &fragment.children[0];
    assert!(
      (grid_fragment.bounds.height() - 80.0).abs() <= 0.5,
      "expected grid percent height to behave like auto (80px), got {:.2}",
      grid_fragment.bounds.height()
    );
  }

  #[test]
  fn grid_respects_used_border_box_size_overrides() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::InlineGrid;
    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(10.0));
    item_style.height = Some(Length::px(10.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let container = BoxNode::new_inline_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![BoxNode::new_block(
        Arc::new(item_style),
        FormattingContextType::Block,
        vec![],
      )],
    );

    let constraints =
      LayoutConstraints::definite(500.0, 100.0).with_used_border_box_size(Some(500.0), Some(100.0));
    let fragment = fc
      .layout(&container, &constraints)
      .expect("inline-grid layout should succeed");

    assert_eq!(fragment.bounds.width(), 500.0);
    assert_eq!(fragment.bounds.height(), 100.0);
  }

  #[test]
  fn grid_respects_style_override_for_root() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut base_style = ComputedStyle::default();
    base_style.display = CssDisplay::InlineGrid;
    base_style.width = Some(Length::px(50.0));
    base_style.width_keyword = None;

    let mut grid = BoxNode::new_inline_block(
      Arc::new(base_style.clone()),
      FormattingContextType::Grid,
      vec![],
    );
    grid.id = 1;

    let constraints = LayoutConstraints::new(
      CrateAvailableSpace::MaxContent,
      CrateAvailableSpace::Indefinite,
    );
    let fragment = fc.layout(&grid, &constraints).expect("inline-grid layout");
    assert_eq!(fragment.bounds.width(), 50.0);

    let mut override_style = base_style;
    override_style.width = Some(Length::px(100.0));
    override_style.width_keyword = None;
    let fragment =
      crate::layout::style_override::with_style_override(grid.id, Arc::new(override_style), || {
        fc.layout(&grid, &constraints)
      })
      .expect("layout with style override");

    assert_eq!(fragment.bounds.width(), 100.0);
  }

  #[test]
  fn grid_fragments_attach_style_overrides_for_child_boxes() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let child_id = 2usize;

    let mut child_style = ComputedStyle::default();
    child_style.display = CssDisplay::Block;
    child_style.width = Some(Length::px(10.0));
    child_style.height = Some(Length::px(10.0));
    child_style.width_keyword = None;
    child_style.height_keyword = None;
    let child_style = Arc::new(child_style);

    let mut child = BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]);
    child.id = child_id;

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.width = Some(Length::px(100.0));
    container_style.height = Some(Length::px(10.0));
    container_style.width_keyword = None;
    container_style.height_keyword = None;

    let mut container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Grid,
      vec![child],
    );
    container.id = 1usize;

    let constraints = LayoutConstraints::definite(100.0, 10.0);
    let fragment = fc.layout(&container, &constraints).expect("layout");
    let child_fragment = find_block_fragment(&fragment, child_id);
    let attached = child_fragment.style.as_ref().expect("fragment style");
    assert!(
      Arc::ptr_eq(attached, &child_style),
      "expected fragment.style to use the BoxNode style when no override is active"
    );

    let mut override_style = child_style.as_ref().clone();
    override_style.width = Some(Length::px(20.0));
    override_style.width_keyword = None;
    let override_style = Arc::new(override_style);

    let fragment = crate::layout::style_override::with_style_override(
      child_id,
      override_style.clone(),
      || fc.layout(&container, &constraints),
    )
    .expect("layout with style override");

    let child_fragment = find_block_fragment(&fragment, child_id);
    let attached = child_fragment.style.as_ref().expect("fragment style");
    assert!(
      Arc::ptr_eq(attached, &override_style),
      "expected fragment.style to use the active style override"
    );

    let fragment = fc.layout(&container, &constraints).expect("layout after override");
    let child_fragment = find_block_fragment(&fragment, child_id);
    let attached = child_fragment.style.as_ref().expect("fragment style");
    assert!(
      Arc::ptr_eq(attached, &child_style),
      "expected style override to be scoped to the guard"
    );
  }

  #[test]
  fn grid_root_width_keyword_min_content_is_narrower_than_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
    let make_item = || make_text_item("hello world goodbye", 16.0);

    let make_container = |keyword: IntrinsicSizeKeyword| {
      let mut style = ComputedStyle::default();
      style.display = CssDisplay::Grid;
      style.grid_template_columns = vec![GridTrack::Auto];
      style.grid_template_rows = vec![GridTrack::Auto];
      style.width = None;
      style.width_keyword = Some(keyword);
      BoxNode::new_block(
        Arc::new(style),
        FormattingContextType::Grid,
        vec![make_item()],
      )
    };

    let constraints = LayoutConstraints::definite(1000.0, 200.0);
    let min_fragment = fc
      .layout(
        &make_container(IntrinsicSizeKeyword::MinContent),
        &constraints,
      )
      .unwrap();
    let max_fragment = fc
      .layout(
        &make_container(IntrinsicSizeKeyword::MaxContent),
        &constraints,
      )
      .unwrap();
    assert!(
      min_fragment.bounds.width() + 0.5 < max_fragment.bounds.width(),
      "expected min-content width ({:.2}) < max-content width ({:.2})",
      min_fragment.bounds.width(),
      max_fragment.bounds.width(),
    );
  }

  #[test]
  fn grid_root_width_keyword_fit_content_clamps_between_min_and_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_item = || make_text_item("fit content prefers available", 16.0);
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Auto];
    style.grid_template_rows = vec![GridTrack::Auto];
    style.width = None;
    style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });

    let container = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Grid,
      vec![make_item()],
    );

    let (min_intrinsic, max_intrinsic) = fc
      .compute_intrinsic_inline_sizes(&container)
      .expect("intrinsic inline sizes");
    assert!(
      max_intrinsic > min_intrinsic + 1.0,
      "expected distinct min/max intrinsic widths (min={min_intrinsic:.2}, max={max_intrinsic:.2})"
    );
    let available_width = (min_intrinsic + (max_intrinsic - min_intrinsic) / 2.0)
      .clamp(min_intrinsic + 1.0, max_intrinsic - 1.0);

    let fragment = fc
      .layout(
        &container,
        &LayoutConstraints::definite(available_width, 200.0),
      )
      .unwrap();
    assert!(
      (fragment.bounds.width() - available_width).abs() < 1.0,
      "expected fit-content width {:.2} to match available width {:.2}",
      fragment.bounds.width(),
      available_width
    );
    assert!(
      fragment.bounds.width() + 0.5 < max_intrinsic,
      "expected fit-content width {:.2} < max-content width {:.2}",
      fragment.bounds.width(),
      max_intrinsic,
    );
  }

  #[test]
  fn grid_root_max_content_width_does_not_stretch_to_available_space() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_max_width_keyword_max_content_clamps_auto_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.max_width = None;
    style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-width:max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_min_width_keyword_max_content_overflows_available_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.width = None;
    style.width_keyword = None;
    style.min_width = None;
    style.min_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(100.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected min-width:max-content grid width to equal the sum of fixed tracks, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_root_max_width_keyword_max_content_clamps_explicit_width_in_vertical_writing_mode() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.width = Some(Length::px(500.0));
    style.width_keyword = None;
    style.max_width = None;
    style.max_width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    // In vertical writing modes, CSS grid rows map to the physical horizontal axis.
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(80.0)),
      GridTrack::Length(Length::px(120.0)),
    ];
    style.grid_template_columns = vec![GridTrack::Length(Length::px(40.0))];
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(Arc::new(style), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(1000.0, 1000.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert!(
      (fragment.bounds.width() - 200.0).abs() < 0.01,
      "expected max-width:max-content to clamp explicit width, got {:.2}",
      fragment.bounds.width()
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      max_width + 0.5 < auto_width,
      "expected max-content grid item width ({max_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_spanning_item_with_auto_column_end_stretches_to_track_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns =
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(50.0)),
      GridTrack::Length(Length::px(50.0)),
    ];
    grid_style.grid_column_gap = Length::px(8.0);
    grid_style.grid_column_gap_is_normal = false;
    // Mirrors the CSS initial value for grid containers.
    grid_style.justify_items = AlignItems::Stretch;
    grid_style.align_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    // An empty grid item spanning multiple rows should still stretch to the width of its grid
    // area (it is common for `::before` pseudo-elements used as full-card backgrounds).
    let mut item_style = ComputedStyle::default();
    item_style.width = None;
    item_style.width_keyword = None;
    item_style.grid_column_start = 1;
    item_style.grid_column_end = 0; // auto
    item_style.grid_row_start = 1;
    item_style.grid_row_end = 3; // spans 2 rows
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(952.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert_eq!(fragment.children.len(), 1);
    let item = &fragment.children[0];
    let expected_width = (952.0 - 8.0 * 2.0) / 3.0;
    assert!(
      (item.bounds.width() - expected_width).abs() < 0.5,
      "expected spanning grid item to stretch to track width (expected={expected_width:.2}, got={:.2})",
      item.bounds.width()
    );
    assert!(
      (item.bounds.height() - 100.0).abs() < 0.5,
      "expected spanning grid item height to equal summed track height (expected=100, got={:.2})",
      item.bounds.height()
    );
  }

  #[test]
  fn grid_spanning_item_with_grid_column_raw_stretches_to_track_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns =
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(50.0)),
      GridTrack::Length(Length::px(50.0)),
    ];
    grid_style.grid_column_gap = Length::px(8.0);
    grid_style.grid_column_gap_is_normal = false;
    // Mirrors the CSS initial value for grid containers.
    grid_style.justify_items = AlignItems::Stretch;
    grid_style.align_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    // When placement comes through `grid-column` shorthands we store the raw placement string for
    // Taffy to parse. Empty spanning items (e.g. `::before` backgrounds) should still stretch to
    // their track width.
    let mut item_style = ComputedStyle::default();
    item_style.width = None;
    item_style.width_keyword = None;
    item_style.grid_column_raw = Some("2".to_string()); // start=2, end=auto
    item_style.grid_row_raw = Some("1 / 3".to_string()); // spans 2 rows
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(952.0, 200.0);
    let fragment = fc
      .layout(&grid, &constraints)
      .expect("grid layout should succeed");

    assert_eq!(fragment.children.len(), 1);
    let item = &fragment.children[0];
    let expected_width = (952.0 - 8.0 * 2.0) / 3.0;
    assert!(
      (item.bounds.width() - expected_width).abs() < 0.5,
      "expected spanning grid item to stretch to track width (expected={expected_width:.2}, got={:.2})",
      item.bounds.width()
    );
  }

  #[test]
  fn grid_auto_items_do_not_skip_earlier_rows_after_definite_row_items() {
    // w3.org uses a 2-row grid as a sticky-footer pattern:
    //   .grid-wrap { display: grid; grid-template-rows: 1fr auto; }
    //   footer      { grid-row-start: 2; grid-row-end: 3; }
    // The main content wrapper is auto-placed; browsers place it into the first row even though
    // the footer has a definite row placement.
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
    grid_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(10.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let wrap_id = 1001usize;
    let footer_id = 1002usize;

    let mut wrap = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    wrap.id = wrap_id;

    let mut footer_style = ComputedStyle::default();
    footer_style.grid_row_raw = Some("2 / 3".to_string());
    let mut footer =
      BoxNode::new_block(Arc::new(footer_style), FormattingContextType::Block, vec![]);
    footer.id = footer_id;

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![wrap, footer]);
    let constraints = LayoutConstraints::definite_width(100.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let wrap_fragment = find_block_fragment(&fragment, wrap_id);
    let footer_fragment = find_block_fragment(&fragment, footer_id);

    assert!(
      (wrap_fragment.bounds.y() - 0.0).abs() < 0.5,
      "expected auto-placed first child to occupy row 1 at y=0, got {:.2}",
      wrap_fragment.bounds.y()
    );
    assert!(
      (footer_fragment.bounds.y() - 10.0).abs() < 0.5,
      "expected definite row-placed footer to occupy row 2 at y=10, got {:.2}",
      footer_fragment.bounds.y()
    );
  }

  #[test]
  fn grid_item_width_keyword_fit_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let fit_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(fit_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let fit_width = fit_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      fit_width + 0.5 < auto_width,
      "expected fit-content grid item width ({fit_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_fit_content_with_max_width_affects_row_sizing() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width: Option<Length>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
      item_style.max_width = max_width;
      item_style.max_width_keyword = None;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let unconstrained = fc.layout(&make_grid(None), &constraints).unwrap();
    let constrained = fc
      .layout(&make_grid(Some(Length::px(50.0))), &constraints)
      .unwrap();

    assert_eq!(unconstrained.children.len(), 1);
    assert_eq!(constrained.children.len(), 1);
    let unconstrained_bounds = unconstrained.children[0].bounds;
    let constrained_bounds = constrained.children[0].bounds;

    assert!(
      constrained_bounds.width() + 0.5 < unconstrained_bounds.width(),
      "expected max-width to narrow fit-content width (unconstrained={:.2}, constrained={:.2})",
      unconstrained_bounds.width(),
      constrained_bounds.width()
    );
    assert!(
      constrained_bounds.height() > unconstrained_bounds.height() + 5.0,
      "expected narrower fit-content width to increase row height (unconstrained={:.2}, constrained={:.2})",
      unconstrained_bounds.height(),
      constrained_bounds.height()
    );
  }

  #[test]
  fn grid_item_max_width_keyword_max_content_clamps_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = None;
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let clamped_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(clamped_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let clamped_width = clamped_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      clamped_width + 0.5 < auto_width,
      "expected max-width:max-content grid item width ({clamped_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_max_width_keyword_fit_content_clamps_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>,
                     explicit_width: Option<Length>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.display = CssDisplay::Block;
      item_style.font_size = 16.0;
      item_style.width = explicit_width;
      item_style.width_keyword = None;
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);

    let auto_fragment = fc
      .layout(&make_grid(None, None), &constraints)
      .expect("auto grid should layout");
    let clamped_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None }), None),
        &constraints,
      )
      .expect("fit-content clamped grid should layout");
    let explicit_fragment = fc
      .layout(
        &make_grid(
          Some(IntrinsicSizeKeyword::FitContent { limit: None }),
          Some(Length::px(500.0)),
        ),
        &constraints,
      )
      .expect("explicit width grid should layout");

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(clamped_fragment.children.len(), 1);
    assert_eq!(explicit_fragment.children.len(), 1);

    let auto_width = auto_fragment.children[0].bounds.width();
    let clamped_width = clamped_fragment.children[0].bounds.width();
    let explicit_width = explicit_fragment.children[0].bounds.width();

    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      clamped_width + 0.5 < auto_width,
      "expected max-width:fit-content to cap below stretched width (auto={auto_width:.2}, clamped={clamped_width:.2})",
    );
    assert!(
      (explicit_width - clamped_width).abs() < 0.5,
      "expected max-width:fit-content to clamp explicit widths the same as auto (clamped={clamped_width:.2}, explicit={explicit_width:.2})",
    );
  }

  #[test]
  fn grid_item_min_width_keyword_fit_content_does_not_prevent_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = None;
    item_style.min_width = None;
    item_style.min_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 200.0).abs() < 0.1,
      "expected min-width:fit-content item to still stretch to 200px, got {width:.2}"
    );
  }

  #[test]
  fn grid_item_min_height_keyword_fit_content_does_not_prevent_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.align_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.height = None;
    item_style.height_keyword = None;
    item_style.min_height = None;
    item_style.min_height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let height = fragment.children[0].bounds.height();
    assert!(
      (height - 200.0).abs() < 0.1,
      "expected min-height:fit-content item to still stretch to 200px, got {height:.2}"
    );
  }

  #[test]
  fn grid_item_width_keyword_min_content_is_narrower_than_max_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: IntrinsicSizeKeyword| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = Some(width_keyword);
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let min_fragment = fc
      .layout(&make_grid(IntrinsicSizeKeyword::MinContent), &constraints)
      .unwrap();
    let max_fragment = fc
      .layout(&make_grid(IntrinsicSizeKeyword::MaxContent), &constraints)
      .unwrap();

    assert_eq!(min_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let min_width = min_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      min_width + 0.5 < max_width,
      "expected min-content width ({min_width:.2}) < max-content width ({max_width:.2})"
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_prevents_stretch_in_vertical_writing_mode() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.writing_mode = WritingMode::VerticalRl;
      // In vertical writing modes, grid rows are the physical horizontal axis. Use a fixed row
      // track to create a definite 200px inline size for stretching.
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.width = None;
      item_style.width_keyword = width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_width = auto_fragment.children[0].bounds.width();
    let max_width = max_fragment.children[0].bounds.width();
    assert!(
      (auto_width - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_width:.2}"
    );
    assert!(
      max_width + 0.5 < auto_width,
      "expected max-content grid item width ({max_width:.2}) to be smaller than stretched auto width ({auto_width:.2})",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_is_clamped_by_max_width() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    item_style.max_width = Some(Length::px(50.0));
    item_style.max_width_keyword = None;
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 50.0).abs() < 0.5,
      "expected max-content width to be clamped to 50px, got {width:.2}"
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_is_clamped_by_max_width_keyword_fit_content() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    // Make the max-content width wider than the grid area, but keep min-content smaller so
    // fit-content clamps to the available width.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    item_style.max_width = None;
    item_style.max_width_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(80.0, 200.0))
      .unwrap();

    assert_eq!(fragment.children.len(), 1);
    let width = fragment.children[0].bounds.width();
    assert!(
      (width - 80.0).abs() < 0.5,
      "expected max-width:fit-content to clamp max-content width to available 80px, got {width:.2}",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_with_max_width_keyword_fit_content_can_overflow() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |max_width_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(80.0))];
      grid_style.grid_template_rows = vec![GridTrack::Auto];
      grid_style.justify_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.white_space = WhiteSpace::Nowrap;
      item_style.width = None;
      item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
      item_style.max_width = None;
      item_style.max_width_keyword = max_width_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world goodbye".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(80.0, 200.0);
    let unconstrained = fc
      .layout(&make_grid(None), &constraints)
      .expect("unconstrained grid should layout");
    let fit_content_max = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .expect("fit-content max grid should layout");

    assert_eq!(unconstrained.children.len(), 1);
    assert_eq!(fit_content_max.children.len(), 1);
    let base_width = unconstrained.children[0].bounds.width();
    let fit_width = fit_content_max.children[0].bounds.width();

    assert!(
      base_width > 80.0 + 0.5,
      "expected max-content width to overflow the 80px grid area under nowrap text, got {base_width:.2}",
    );
    assert!(
      (fit_width - base_width).abs() < 0.5,
      "expected max-width:fit-content to match max-content width when min-content exceeds available (base={base_width:.2}, fit={fit_width:.2})",
    );
  }

  #[test]
  fn grid_item_width_keyword_max_content_uses_start_alignment_in_rtl() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.direction = Direction::Rtl;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_items = AlignItems::Stretch;
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.font_size = 16.0;
    item_style.width = None;
    item_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
    let item_style = Arc::new(item_style);
    let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
    let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let child = &fragment.children[0];
    let width = child.bounds.width();
    assert!(
      width + 0.5 < 200.0,
      "expected max-content item width to be smaller than 200px, got {width:.2}"
    );

    let expected_x = 200.0 - width;
    assert!(
      (child.bounds.x() - expected_x).abs() < 1.0,
      "expected RTL start alignment to place item at x≈{expected_x:.2}, got {:.2} (width={width:.2})",
      child.bounds.x()
    );
  }

  #[test]
  fn grid_item_height_keyword_max_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |height_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.height = None;
      item_style.height_keyword = height_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(300.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let max_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::MaxContent)),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(max_fragment.children.len(), 1);
    let auto_height = auto_fragment.children[0].bounds.height();
    let max_height = max_fragment.children[0].bounds.height();
    assert!(
      (auto_height - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_height:.2}"
    );
    assert!(
      max_height + 0.5 < auto_height,
      "expected max-content grid item height ({max_height:.2}) to be smaller than stretched auto height ({auto_height:.2})",
    );
  }

  #[test]
  fn grid_item_height_keyword_fit_content_prevents_stretch() {
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let make_grid = |height_keyword: Option<IntrinsicSizeKeyword>| {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(300.0))];
      grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(200.0))];
      grid_style.align_items = AlignItems::Stretch;
      let grid_style = Arc::new(grid_style);

      let mut item_style = ComputedStyle::default();
      item_style.font_size = 16.0;
      item_style.height = None;
      item_style.height_keyword = height_keyword;
      let item_style = Arc::new(item_style);
      let text_child = BoxNode::new_text(item_style.clone(), "hello world".into());
      let item = BoxNode::new_block(item_style, FormattingContextType::Inline, vec![text_child]);

      BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item])
    };

    let constraints = LayoutConstraints::definite(300.0, 200.0);
    let auto_fragment = fc.layout(&make_grid(None), &constraints).unwrap();
    let fit_fragment = fc
      .layout(
        &make_grid(Some(IntrinsicSizeKeyword::FitContent { limit: None })),
        &constraints,
      )
      .unwrap();

    assert_eq!(auto_fragment.children.len(), 1);
    assert_eq!(fit_fragment.children.len(), 1);
    let auto_height = auto_fragment.children[0].bounds.height();
    let fit_height = fit_fragment.children[0].bounds.height();
    assert!(
      (auto_height - 200.0).abs() < 0.1,
      "expected auto grid item to stretch to 200px, got {auto_height:.2}"
    );
    assert!(
      fit_height + 0.5 < auto_height,
      "expected fit-content grid item height ({fit_height:.2}) to be smaller than stretched auto height ({auto_height:.2})",
    );
  }

  #[test]
  fn taffy_template_cache_reuses_grid_templates_with_equal_styles() {
    use crate::layout::taffy_integration::{
      taffy_grid_container_style_fingerprint, TaffyNodeCacheKey,
    };
    use taffy::TaffyTree;

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(20.0)), GridTrack::Fr(1.0)];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.grid_column_gap = Length::px(8.0);
    grid_style.grid_row_gap = Length::px(4.0);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(10.0));
    item_style.height = Some(Length::px(10.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    item_style.grid_column_start = 1;
    item_style.grid_row_start = 1;

    let grid_style_a = Arc::new(grid_style.clone());
    let grid_style_b = Arc::new(grid_style);
    assert!(
      !Arc::ptr_eq(&grid_style_a, &grid_style_b),
      "expected distinct Arc pointers for grid container styles"
    );
    let item_style_a = Arc::new(item_style.clone());
    let item_style_b = Arc::new(item_style);
    assert!(
      !Arc::ptr_eq(&item_style_a, &item_style_b),
      "expected distinct Arc pointers for grid item styles"
    );

    let mut container_a = BoxNode::new_block(
      grid_style_a,
      FormattingContextType::Grid,
      vec![
        BoxNode::new_block(item_style_a.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_a, FormattingContextType::Block, vec![]),
      ],
    );
    container_a.id = 1;
    container_a.children[0].id = 10;
    container_a.children[1].id = 11;

    let mut container_b = BoxNode::new_block(
      grid_style_b,
      FormattingContextType::Grid,
      vec![
        BoxNode::new_block(item_style_b.clone(), FormattingContextType::Block, vec![]),
        BoxNode::new_block(item_style_b, FormattingContextType::Block, vec![]),
      ],
    );
    container_b.id = 2;
    container_b.children[0].id = 20;
    container_b.children[1].id = 21;

    let gc = GridFormattingContext::new();
    assert_eq!(gc.taffy_cache.template_count(), 0);

    let children_a: Vec<&BoxNode> = container_a.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_a = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Grid,
      taffy_grid_container_style_fingerprint(container_a.style.as_ref()),
      super::grid_child_fingerprint(&children_a, false, &mut deadline_counter)
        .expect("fingerprint"),
      gc.viewport_size,
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut positioned_children: FxHashMap<taffy::prelude::NodeId, Vec<*const BoxNode>> =
      FxHashMap::default();
    let constraints = LayoutConstraints::definite(200.0, 200.0);
    gc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_a,
      container_a.style.as_ref(),
      &children_a,
      &constraints,
      &mut positioned_children,
    )
    .expect("build taffy tree");

    assert_eq!(
      gc.taffy_cache.template_count(),
      1,
      "first build should insert a single cached template"
    );
    let template_a = gc
      .taffy_cache
      .get(&key_a)
      .expect("template should be cached after first build");

    let children_b: Vec<&BoxNode> = container_b.children.iter().collect();
    let mut deadline_counter = 0usize;
    let key_b = TaffyNodeCacheKey::new(
      TaffyAdapterKind::Grid,
      taffy_grid_container_style_fingerprint(container_b.style.as_ref()),
      super::grid_child_fingerprint(&children_b, false, &mut deadline_counter)
        .expect("fingerprint"),
      gc.viewport_size,
    );
    assert_eq!(
      key_a, key_b,
      "cache keys should match for identical style values regardless of ids/pointers"
    );

    let mut taffy_tree: TaffyTree<*const BoxNode> = TaffyTree::new();
    let mut positioned_children: FxHashMap<taffy::prelude::NodeId, Vec<*const BoxNode>> =
      FxHashMap::default();
    gc.build_taffy_tree_children(
      &mut taffy_tree,
      &container_b,
      container_b.style.as_ref(),
      &children_b,
      &constraints,
      &mut positioned_children,
    )
    .expect("build taffy tree");

    assert_eq!(
      gc.taffy_cache.template_count(),
      1,
      "second build should hit the template cache instead of inserting a new entry"
    );
    let template_b = gc
      .taffy_cache
      .get(&key_b)
      .expect("template should be cached after second build");
    assert!(
      Arc::ptr_eq(&template_a, &template_b),
      "expected second build to reuse existing cached template"
    );
  }

  #[test]
  fn grid_child_fingerprint_includes_replacedness() {
    use crate::tree::box_tree::ReplacedType;

    let shared_style = Arc::new(ComputedStyle::default());
    let normal = BoxNode::new_block(shared_style.clone(), FormattingContextType::Block, vec![]);
    let replaced = BoxNode::new_replaced(shared_style, ReplacedType::Canvas, None, None);

    let children_normal: Vec<&BoxNode> = vec![&normal];
    let children_replaced: Vec<&BoxNode> = vec![&replaced];

    let mut deadline_counter = 0usize;
    let normal_fp = super::grid_child_fingerprint(&children_normal, false, &mut deadline_counter)
      .expect("fingerprint");
    let mut deadline_counter = 0usize;
    let replaced_fp =
      super::grid_child_fingerprint(&children_replaced, false, &mut deadline_counter)
        .expect("fingerprint");

    assert_ne!(
      normal_fp, replaced_fp,
      "grid template fingerprints should differ for replaced vs non-replaced leaves"
    );
  }

  #[test]
  fn grid_tree_build_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let children: Vec<BoxNode> = (0..64)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let container = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, children);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let mut positioned_children = FxHashMap::default();
    let result = gc.build_taffy_tree_children(
      &mut taffy,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut positioned_children,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_subgrid_tree_build_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.grid_row_subgrid = true;
    let subgrid_style = Arc::new(subgrid_style);

    let mut root = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      root = BoxNode::new_block(
        subgrid_style.clone(),
        FormattingContextType::Grid,
        vec![root],
      );
    }
    let container = root;
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
    let root_children: Vec<&BoxNode> = container.children.iter().collect();
    let mut positioned_children = FxHashMap::default();
    let result = gc.build_taffy_tree_children(
      &mut taffy,
      &container,
      container.style.as_ref(),
      &root_children,
      &constraints,
      &mut positioned_children,
    );

    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_baseline_traversal_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let leaf = FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), "x", 0.5);
    let mut fragment = leaf;
    // Ensure the baseline walk hits GRID_DEADLINE_CHECK_STRIDE checks before reaching the leaf.
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![fragment]);
    }

    let mut deadline_counter = 0usize;
    let result = find_first_baseline_offset(&fragment, &mut deadline_counter);
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_translate_fragment_tree_times_out_via_deadline_checks() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::ZERO), None);
    let _guard = DeadlineGuard::install(Some(&deadline));

    let mut deadline_counter = 0usize;
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
    let mut result = Ok(());
    for _ in 0..(GRID_DEADLINE_CHECK_STRIDE + 16) {
      result = translate_fragment_tree(&mut fragment, Point::new(0.0, 0.0), &mut deadline_counter);
      if result.is_err() {
        break;
      }
    }
    assert!(matches!(result, Err(LayoutError::Timeout { .. })));
  }

  #[test]
  fn grid_translate_fragment_tree_moves_root_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(5.0, 7.0, 1.0, 1.0), vec![]);
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![child]);

    let mut deadline_counter = 0usize;
    translate_fragment_tree(&mut fragment, Point::new(10.0, 5.0), &mut deadline_counter).unwrap();

    assert_eq!(fragment.bounds.x(), 10.0);
    assert_eq!(fragment.bounds.y(), 5.0);
    assert_eq!(fragment.children[0].bounds.x(), 5.0);
    assert_eq!(fragment.children[0].bounds.y(), 7.0);
  }

  #[test]
  fn grid_taffy_abort_surfaces_as_timeout() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Ensure the deadline is not tripped by the initial check, but is tripped during Taffy.
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let deadline = RenderDeadline::new(
      None,
      Some(Arc::new(move || {
        let prev = counter_clone.fetch_add(1, Ordering::SeqCst);
        prev == 1
      })),
    );
    let _guard = DeadlineGuard::install(Some(&deadline));

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let container = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);
    let constraints = LayoutConstraints::definite(100.0, 100.0);

    let gc = GridFormattingContext::new();
    let result = gc.layout(&container, &constraints);

    match result {
      Err(LayoutError::Timeout { elapsed }) => {
        assert!(elapsed >= Duration::from_secs(0));
      }
      other => panic!("expected LayoutError::Timeout from Taffy abort, got {other:?}"),
    }
  }

  #[test]
  fn measure_key_quantizes_definite_sizes() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known_a = taffy::geometry::Size {
      width: Some(300.3),
      height: Some(150.7),
    };
    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(500.2),
      height: AvailableSpace::Definite(600.4),
    };
    let known_b = taffy::geometry::Size {
      width: Some(300.6),
      height: Some(150.4),
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(500.6),
      height: AvailableSpace::Definite(600.2),
    };

    let key_a = MeasureKey::new(&node, known_a, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known_b, avail_b, viewport, false);
    assert_eq!(
      key_a, key_b,
      "near-identical definite sizes should quantize to the same key"
    );

    let intrinsic_known = taffy::geometry::Size {
      width: None,
      height: known_a.height,
    };
    let min_key = MeasureKey::new(
      &node,
      intrinsic_known,
      taffy::geometry::Size {
        width: AvailableSpace::MinContent,
        height: avail_a.height,
      },
      viewport,
      false,
    );
    let max_key = MeasureKey::new(
      &node,
      intrinsic_known,
      taffy::geometry::Size {
        width: AvailableSpace::MaxContent,
        height: avail_a.height,
      },
      viewport,
      false,
    );
    assert_ne!(
      min_key.available_width, max_key.available_width,
      "min/max-content probes should remain distinct when width is unknown"
    );
  }

  #[test]
  fn measure_key_does_not_snap_medium_known_widths_to_coarse_steps() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: Some(550.0),
      height: None,
    };
    let avail = taffy::geometry::Size {
      width: AvailableSpace::Definite(550.0),
      height: AvailableSpace::MaxContent,
    };

    let (_key, snapped_known, snapped_avail) =
      MeasureKey::new_with_snapped_sizes(&node, known, avail, viewport, false);
    assert_eq!(
      snapped_known.width,
      Some(550.0),
      "550px-wide grid items should not be snapped up to 552px (8px quantization)"
    );
    match snapped_avail.width {
      AvailableSpace::Definite(w) => assert_eq!(w, 550.0),
      other => panic!("expected snapped width to remain definite, got {other:?}"),
    }
  }

  #[test]
  fn grid_measurement_is_deterministic_within_quantized_measure_key() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();

    let mut style = ComputedStyle::default();
    style.width = Some(Length::percent(50.0));
    let node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    let node_ptr = &node as *const _;
    let node_id = TaffyNodeId::from(1u64);
    let viewport = gc.viewport_size;

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(100.4),
      height: AvailableSpace::MaxContent,
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(100.6),
      height: AvailableSpace::MaxContent,
    };

    let key_a = MeasureKey::new(&node, known, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known, avail_b, viewport, false);
    assert_eq!(
      key_a, key_b,
      "expected probes to map to the same quantized MeasureKey"
    );

    let size_a = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail_a,
        None,
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    let size_b = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail_b,
        None,
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    assert_eq!(
      size_a.size.width.to_bits(),
      size_b.size.width.to_bits(),
      "measured widths should match exactly after snapping to the quantized key"
    );
    assert_eq!(
      size_a.size.height.to_bits(),
      size_b.size.height.to_bits(),
      "measured heights should match exactly after snapping to the quantized key"
    );
  }

  #[test]
  fn measure_key_ignores_available_width_when_known_width_is_some() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: Some(200.0),
      height: None,
    };

    let key_a = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(10.0),
      },
      viewport,
      false,
    );
    let key_b = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(400.0),
        height: AvailableSpace::Definite(10.0),
      },
      viewport,
      false,
    );

    assert_eq!(key_a.available_width, MeasureAvailKey::Ignored);
    assert_eq!(key_b.available_width, MeasureAvailKey::Ignored);
    assert_eq!(
      key_a, key_b,
      "when Taffy supplies a known width, varying AvailableSpace::width should not change the key"
    );

    let key_c = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(50.0),
      },
      viewport,
      false,
    );
    assert_ne!(
      key_a, key_c,
      "unknown height should remain sensitive to AvailableSpace::height"
    );
  }

  #[test]
  fn measure_key_ignores_available_height_when_known_height_is_some() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: Some(200.0),
    };

    let key_a = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(10.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    let key_b = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(10.0),
        height: AvailableSpace::Definite(400.0),
      },
      viewport,
      false,
    );

    assert_eq!(key_a.available_height, MeasureAvailKey::Ignored);
    assert_eq!(key_b.available_height, MeasureAvailKey::Ignored);
    assert_eq!(
      key_a, key_b,
      "when Taffy supplies a known height, varying AvailableSpace::height should not change the key"
    );

    let key_c = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(50.0),
        height: AvailableSpace::Definite(100.0),
      },
      viewport,
      false,
    );
    assert_ne!(
      key_a, key_c,
      "unknown width should remain sensitive to AvailableSpace::width"
    );
  }

  #[test]
  fn grid_constraints_from_taffy_does_not_clamp_definite_width_to_viewport() {
    use taffy::style::AvailableSpace;

    let viewport = Size::new(200.0, 200.0);
    let known = taffy::geometry::Size {
      width: Some(1000.0),
      height: None,
    };
    let available = taffy::geometry::Size {
      width: AvailableSpace::Definite(1000.0),
      height: AvailableSpace::MaxContent,
    };

    let constraints =
      constraints_from_taffy(viewport, known, available, WritingMode::HorizontalTb, None);
    assert_eq!(
      constraints.available_width,
      CrateAvailableSpace::Definite(1000.0)
    );
  }

  #[test]
  fn measure_key_does_not_clamp_definite_widths_to_viewport() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);

    let known_a = taffy::geometry::Size {
      width: Some(2000.0),
      height: Some(100.0),
    };
    let known_b = taffy::geometry::Size {
      width: Some(3000.0),
      height: Some(100.0),
    };
    let avail_a = taffy::geometry::Size {
      width: AvailableSpace::Definite(2000.0),
      height: AvailableSpace::Definite(400.0),
    };
    let avail_b = taffy::geometry::Size {
      width: AvailableSpace::Definite(3000.0),
      height: AvailableSpace::Definite(400.0),
    };

    let key_a = MeasureKey::new(&node, known_a, avail_a, viewport, false);
    let key_b = MeasureKey::new(&node, known_b, avail_b, viewport, false);
    assert_ne!(
      key_a, key_b,
      "definite widths larger than the viewport should remain distinct cache keys"
    );
  }

  #[test]
  fn measure_key_treats_tiny_definite_available_space_as_indefinite() {
    use taffy::style::AvailableSpace;

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let viewport = Size::new(800.0, 600.0);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };

    let zero_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(0.0),
        height: AvailableSpace::Definite(0.0),
      },
      viewport,
      false,
    );
    let one_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(1.0),
        height: AvailableSpace::Definite(1.0),
      },
      viewport,
      false,
    );
    assert_eq!(zero_key.available_width, MeasureAvailKey::Indefinite);
    assert_eq!(zero_key.available_height, MeasureAvailKey::Indefinite);
    assert_eq!(
      zero_key, one_key,
      "tiny definite available sizes should coalesce to the same key"
    );

    let two_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(2.0),
        height: AvailableSpace::Definite(2.0),
      },
      viewport,
      false,
    );
    assert_ne!(zero_key.available_width, two_key.available_width);
    assert_ne!(zero_key.available_height, two_key.available_height);
  }

  #[test]
  fn measured_node_keys_are_capped_per_node() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let node_ptr = &node as *const _;
    let node_id = TaffyNodeId::from(1u64);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    for i in 0..(MAX_MEASURED_KEYS_PER_NODE + 5) {
      let avail = taffy::geometry::Size {
        width: AvailableSpace::Definite(20.0 + (i as f32 * 10.0)),
        height: AvailableSpace::Definite(40.0 + (i as f32 * 5.0)),
      };
      let _ = gc.measure_grid_item(
        node_ptr,
        node_id,
        known,
        avail,
        None,
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
    }

    let node_keys = measured_node_keys.get(&node_id).expect("measured keys");
    assert!(
      node_keys.len() <= MAX_MEASURED_KEYS_PER_NODE,
      "measured keys should be capped per node"
    );
    let first_key = MeasureKey::new(
      &node,
      known,
      taffy::geometry::Size {
        width: AvailableSpace::Definite(20.0),
        height: AvailableSpace::Definite(40.0),
      },
      gc.viewport_size,
      false,
    );
    assert!(
      !node_keys.contains(&first_key),
      "oldest measured keys should be evicted when exceeding the cap"
    );
  }

  #[test]
  fn grid_measure_size_cache_policy_evicts_instead_of_clearing() {
    let mut cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let max_entries = 4;
    let eviction_batch = 2;
    let output: taffy::tree::MeasureOutput = taffy::geometry::Size {
      width: 1.0,
      height: 2.0,
    }
    .into();
    for i in 0..max_entries {
      let key = MeasureKey {
        box_id: i + 1,
        style_ptr: 0,
        override_fingerprint: None,
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      grid_measure_size_cache_store_with_policy(
        &mut cache,
        key,
        output,
        max_entries,
        eviction_batch,
      );
    }
    assert_eq!(cache.len(), max_entries);

    let key = MeasureKey {
      box_id: 999,
      style_ptr: 0,
      override_fingerprint: None,
      known_width: None,
      known_height: None,
      available_width: MeasureAvailKey::Indefinite,
      available_height: MeasureAvailKey::Indefinite,
    };
    grid_measure_size_cache_store_with_policy(&mut cache, key, output, max_entries, eviction_batch);
    assert_eq!(cache.len(), max_entries - eviction_batch + 1);
    assert!(
      cache.len() > 1,
      "eviction should preserve cache reuse instead of clearing everything"
    );

    let before = cache.len();
    grid_measure_size_cache_store_with_policy(&mut cache, key, output, max_entries, eviction_batch);
    assert_eq!(
      cache.len(),
      before,
      "updating an existing key should not evict"
    );
  }

  #[test]
  fn grid_measure_cross_invocation_cache_reuses_layout_results_within_epoch() {
    use taffy::style::AvailableSpace;

    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();
    let _cache_guard = enable_grid_measure_size_cache_for_test_thread();

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let taffy_style: taffy::style::Style = taffy::style::Style::default();

    let mut node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    node.id = 4242;
    let node_ptr = &node as *const _;

    let node_id = TaffyNodeId::from(1u64);
    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let avail = taffy::geometry::Size {
      width: AvailableSpace::Definite(200.0),
      height: AvailableSpace::Definite(100.0),
    };

    reset_grid_measure_layout_calls();

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let first = gc.measure_grid_item(
      node_ptr,
      node_id,
      known,
      avail,
      None,
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert_eq!(grid_measure_layout_calls(), 1);

    // Simulate a fresh Taffy invocation: new per-invocation caches should still reuse the
    // cross-invocation measure cache.
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let second = gc.measure_grid_item(
      node_ptr,
      node_id,
      known,
      avail,
      None,
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert_eq!(
      grid_measure_layout_calls(),
      1,
      "repeated measurements in the same epoch should hit the cross-invocation cache"
    );
    assert_eq!(second.size, first.size);
  }

  #[test]
  fn grid_measure_key_coalesces_tiny_known_probes() {
    use taffy::style::AvailableSpace;

    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();
    let _cache_guard = enable_grid_measure_size_cache_for_test_thread();

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let taffy_style: taffy::style::Style = taffy::style::Style::default();

    let mut node = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    node.id = 4243;
    let node_ptr = &node as *const _;

    let node_id = TaffyNodeId::from(1u64);
    let avail = taffy::geometry::Size {
      width: AvailableSpace::Definite(0.0),
      height: AvailableSpace::Definite(100.0),
    };

    reset_grid_measure_layout_calls();

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let first = gc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: Some(0.0),
        height: None,
      },
      avail,
      None,
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert_eq!(grid_measure_layout_calls(), 1);

    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let second = gc.measure_grid_item(
      node_ptr,
      node_id,
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      avail,
      None,
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert_eq!(
      grid_measure_layout_calls(),
      1,
      "tiny definite probes should normalize so cache keys coalesce (<=1px)"
    );
    assert_eq!(second.size, first.size);
  }

  #[test]
  fn grid_measure_shared_cache_reuses_across_threads() {
    use taffy::style::AvailableSpace;

    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();
    reset_grid_measure_cache_counters();

    let item_style = make_item_style();
    let box_id = 4244usize;

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_GRID_MEASURE_CACHE_PROFILE".to_string(),
      "1".to_string(),
    )])));

    let thread_1 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            1,
            "first thread should miss and perform nested layout"
          );
          output.size
        })
      })
    };

    let size_1 = thread_1.join().expect("thread 1 join");

    let thread_2 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            0,
            "second thread should hit the shared cache and skip nested layout"
          );
          output.size
        })
      })
    };

    let size_2 = thread_2.join().expect("thread 2 join");
    assert_eq!(size_2, size_1);

    let counters = grid_measure_cache_counters();
    assert!(
      counters.shared_hits >= 1,
      "expected at least one shared cache hit, got shared_hits={}",
      counters.shared_hits
    );
    assert!(
      counters.misses >= 1,
      "expected at least one miss for the first fill, got misses={}",
      counters.misses
    );
  }

  #[test]
  fn grid_measure_override_keys_bypass_shared_cache_by_default() {
    use taffy::style::AvailableSpace;

    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();
    reset_grid_measure_cache_counters();

    let item_style = make_item_style();
    let box_id = 4245usize;
    let override_style = Arc::new((*item_style).clone());

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_GRID_MEASURE_CACHE_PROFILE".to_string(),
      "1".to_string(),
    )])));

    let thread_1 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      let override_style = override_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          let _override_guard = push_style_override(box_id, override_style);

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            1,
            "first thread should miss and perform nested layout"
          );
          output.size
        })
      })
    };

    let size_1 = thread_1.join().expect("thread 1 join");

    let thread_2 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      let override_style = override_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          let _override_guard = push_style_override(box_id, override_style);

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            1,
            "override keys should bypass the shared cache by default"
          );
          output.size
        })
      })
    };

    let size_2 = thread_2.join().expect("thread 2 join");
    assert_eq!(size_2, size_1);

    let counters = grid_measure_cache_counters();
    assert!(
      counters.override_lookups >= 2,
      "expected override lookups to be recorded (got {})",
      counters.override_lookups
    );
    assert!(
      counters.override_shared_bypass_misses >= 2,
      "expected override bypass misses to be recorded (got {})",
      counters.override_shared_bypass_misses
    );
    assert_eq!(
      counters.override_shared_hits, 0,
      "override keys should not hit shared cache when sharing is disabled"
    );
  }

  #[test]
  fn grid_measure_override_keys_can_share_across_threads_when_enabled() {
    use taffy::style::AvailableSpace;

    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();
    reset_grid_measure_cache_counters();

    let item_style = make_item_style();
    let box_id = 4246usize;
    let override_style = Arc::new((*item_style).clone());

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([
      (
        "FASTR_GRID_MEASURE_CACHE_PROFILE".to_string(),
        "1".to_string(),
      ),
      (
        "FASTR_GRID_MEASURE_CACHE_SHARE_OVERRIDES".to_string(),
        "1".to_string(),
      ),
    ])));

    let thread_1 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      let override_style = override_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          let _override_guard = push_style_override(box_id, override_style);

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            1,
            "first thread should miss and perform nested layout"
          );
          output.size
        })
      })
    };

    let size_1 = thread_1.join().expect("thread 1 join");

    let thread_2 = {
      let toggles = toggles.clone();
      let item_style = item_style.clone();
      let override_style = override_style.clone();
      std::thread::spawn(move || {
        runtime::with_thread_runtime_toggles(toggles, || {
          let _cache_guard = enable_grid_measure_size_cache_for_test_thread();
          let mut node = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
          node.id = box_id;
          let node_ptr = &node as *const _;
          let gc = GridFormattingContext::new();
          let factory = gc.factory.clone();
          let container_style = make_grid_style();
          let container_constraints = LayoutConstraints::indefinite();
          let taffy_style: taffy::style::Style = taffy::style::Style::default();

          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(200.0),
            height: AvailableSpace::Definite(100.0),
          };
          let known = taffy::geometry::Size {
            width: None,
            height: None,
          };

          let _override_guard = push_style_override(box_id, override_style);

          reset_grid_measure_layout_calls();
          let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
            FxHashMap::default();
          let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
          let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

          let output = gc.measure_grid_item(
            node_ptr,
            TaffyNodeId::from(1u64),
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );

          assert_eq!(
            grid_measure_layout_calls(),
            0,
            "override keys should hit the shared cache when sharing is enabled"
          );
          output.size
        })
      })
    };

    let size_2 = thread_2.join().expect("thread 2 join");
    assert_eq!(size_2, size_1);

    let counters = grid_measure_cache_counters();
    assert!(
      counters.override_shared_hits >= 1,
      "expected override shared hits when sharing enabled (got {})",
      counters.override_shared_hits
    );
    assert!(
      counters.override_shared_bypass_misses == 0,
      "expected no bypass misses when override sharing enabled (got {})",
      counters.override_shared_bypass_misses
    );
  }

  #[test]
  fn grid_measure_shared_cache_caps_override_entries_per_shard() {
    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();

    let epoch = 1usize;
    // Seed a few base-style entries in a single shard.
    let base_entries = 64usize;
    for i in 0..base_entries {
      let key = MeasureKey {
        box_id: i * GRID_MEASURE_SHARED_CACHE_SHARDS, // force shard 0
        style_ptr: 0,
        override_fingerprint: None,
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key, epoch, taffy::tree::MeasureOutput::ZERO);
    }

    // Insert more override-key entries than the per-shard cap and ensure they don't grow
    // unbounded or evict the base-style keyset.
    let override_attempts = GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD * 3;
    for i in 0..override_attempts {
      let key = MeasureKey {
        box_id: (i + 10_000) * GRID_MEASURE_SHARED_CACHE_SHARDS, // keep shard 0, distinct ids
        style_ptr: 0,
        override_fingerprint: Some(i as u64),
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key, epoch, taffy::tree::MeasureOutput::ZERO);
    }

    let shard = GLOBAL_GRID_MEASURE_SIZE_CACHE
      .shards
      .get(0)
      .expect("shard 0");
    let shard = shard.read();
    assert!(
      shard.override_entries <= GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD,
      "override entries should be capped (got {}, cap {})",
      shard.override_entries,
      GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD
    );
    let actual_override_entries = shard
      .map
      .keys()
      .filter(|key| key.override_fingerprint.is_some())
      .count();
    assert_eq!(
      actual_override_entries, shard.override_entries,
      "override entry counter must match actual override keys"
    );
    let base_count = shard
      .map
      .keys()
      .filter(|key| key.override_fingerprint.is_none())
      .count();
    assert_eq!(
      base_count, base_entries,
      "override eviction should not evict base-style entries when there is free capacity"
    );
  }

  #[test]
  fn grid_measure_shared_cache_evicts_override_entries_before_base_entries() {
    let _lock = grid_measure_size_cache_test_lock();
    reset_grid_measure_size_cache_for_test();

    let epoch = 1usize;
    let shard_index = 0usize;
    let max_entries = GRID_MEASURE_SHARED_CACHE_MAX_ENTRIES_PER_SHARD;
    assert!(max_entries > 0);

    // Fill a shard to capacity with mostly base-style keys plus a small number of override keys.
    let override_seed = 6usize.min(GRID_MEASURE_SHARED_CACHE_MAX_OVERRIDE_ENTRIES_PER_SHARD.max(1));
    let base_seed = max_entries.saturating_sub(override_seed);

    for i in 0..base_seed {
      let key = MeasureKey {
        box_id: i * GRID_MEASURE_SHARED_CACHE_SHARDS, // force shard 0
        style_ptr: 0,
        override_fingerprint: None,
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key, epoch, taffy::tree::MeasureOutput::ZERO);
    }
    for i in 0..override_seed {
      let key = MeasureKey {
        box_id: (10_000 + i) * GRID_MEASURE_SHARED_CACHE_SHARDS, // same shard, distinct ids
        style_ptr: 0,
        override_fingerprint: Some(i as u64),
        known_width: None,
        known_height: None,
        available_width: MeasureAvailKey::Indefinite,
        available_height: MeasureAvailKey::Indefinite,
      };
      GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(key, epoch, taffy::tree::MeasureOutput::ZERO);
    }

    let shard = GLOBAL_GRID_MEASURE_SIZE_CACHE
      .shards
      .get(shard_index)
      .expect("shard");
    {
      let shard = shard.read();
      assert_eq!(
        shard.map.len(),
        base_seed + override_seed,
        "sanity check: shard should be full"
      );
      assert_eq!(
        shard
          .map
          .keys()
          .filter(|key| key.override_fingerprint.is_none())
          .count(),
        base_seed
      );
      assert_eq!(
        shard
          .map
          .keys()
          .filter(|key| key.override_fingerprint.is_some())
          .count(),
        override_seed
      );
    }

    // Insert a new base-style entry into the full shard. The cache should evict override entries
    // first (freeing space) and preserve the base keyset.
    let new_key = MeasureKey {
      box_id: 999_999 * GRID_MEASURE_SHARED_CACHE_SHARDS,
      style_ptr: 0,
      override_fingerprint: None,
      known_width: None,
      known_height: None,
      available_width: MeasureAvailKey::Indefinite,
      available_height: MeasureAvailKey::Indefinite,
    };
    GLOBAL_GRID_MEASURE_SIZE_CACHE.insert(new_key, epoch, taffy::tree::MeasureOutput::ZERO);

    let shard = shard.read();
    assert!(
      shard.map.len() <= max_entries,
      "shard must remain within capacity (len={}, max={max_entries})",
      shard.map.len()
    );
    let base_count = shard
      .map
      .keys()
      .filter(|key| key.override_fingerprint.is_none())
      .count();
    let override_count = shard
      .map
      .keys()
      .filter(|key| key.override_fingerprint.is_some())
      .count();
    assert_eq!(
      base_count,
      base_seed + 1,
      "base entries should be preserved and include the inserted entry"
    );
    assert_eq!(
      override_count, 0,
      "override entries should be evicted first under capacity pressure"
    );
  }

  #[test]
  fn grid_measure_quantization_limits_layout_calls() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let widths = [300.2, 300.6, 300.1, 300.8, 300.4];
    let heights = [150.7, 150.4, 150.9];

    let nodes: Vec<BoxNode> = (0..12)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for width in widths {
        for height in heights {
          let avail = taffy::geometry::Size {
            width: AvailableSpace::Definite(width + (i as f32 * 0.01)),
            height: AvailableSpace::Definite(height),
          };
          let _ = gc.measure_grid_item(
            node_ptr,
            node_id,
            known,
            avail,
            None,
            container_style.as_ref(),
            &container_constraints,
            &taffy_style,
            &FxHashSet::default(),
            &factory,
            &mut measure_cache,
            &mut measured_fragments,
            &mut measured_node_keys,
          );
        }
      }
    }

    let calls = grid_measure_layout_calls();
    assert!(
      calls > 0 && calls <= nodes.len() * 3,
      "quantized keys should coalesce near-identical probes (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_ignores_definite_available_height_when_safe() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let nodes: Vec<BoxNode> = (0..12)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len(),
      "available height differences should be ignored when measurement doesn't depend on them (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_ignores_definite_available_height_when_safe_for_flex_items() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let mut flex_style = ComputedStyle::default();
    flex_style.display = CssDisplay::Flex;
    let flex_style = Arc::new(flex_style);

    let nodes: Vec<BoxNode> = (0..12)
      .map(|_| BoxNode::new_block(flex_style.clone(), FormattingContextType::Flex, vec![]))
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len(),
      "available height differences should be ignored for flex items when measurement doesn't depend on them (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_ignores_definite_available_height_when_percentage_children_present_in_auto_tracks() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let mut child_style = ComputedStyle::default();
    child_style.height = Some(Length::percent(50.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let nodes: Vec<BoxNode> = (0..4)
      .map(|_| {
        BoxNode::new_block(
          make_item_style(),
          FormattingContextType::Block,
          vec![child.clone()],
        )
      })
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len(),
      "percentage-height children should not force distinct cache keys for content-sized tracks (calls={calls})"
    );
  }

  #[test]
  fn grid_measure_ignores_definite_available_height_when_fit_content_children_present_in_auto_tracks()
  {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    reset_grid_measure_layout_calls();

    let known = taffy::geometry::Size {
      width: None,
      height: None,
    };
    let taffy_style: taffy::style::Style = taffy::style::Style::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let width = 300.0;
    let heights = [150.0, 400.0, 650.0];

    let mut child_style = ComputedStyle::default();
    child_style.height = None;
    child_style.height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let nodes: Vec<BoxNode> = (0..4)
      .map(|_| {
        BoxNode::new_block(
          make_item_style(),
          FormattingContextType::Block,
          vec![child.clone()],
        )
      })
      .collect();

    for (i, node) in nodes.iter().enumerate() {
      let node_id = TaffyNodeId::from((i + 1) as u64);
      let node_ptr = node as *const _;
      for height in heights {
        let avail = taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(height),
        };
        let _ = gc.measure_grid_item(
          node_ptr,
          node_id,
          known,
          avail,
          None,
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
      }
    }

    let calls = grid_measure_layout_calls();
    assert_eq!(
      calls,
      nodes.len(),
      "fit-content children should not force distinct cache keys for content-sized tracks (calls={calls})"
    );
  }

  #[test]
  fn measure_grid_item_height_probes_respect_known_inline_size() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        gc.font_context.clone(),
        gc.viewport_size,
        gc.nearest_positioned_cb,
      );
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    for (probe_height, label) in [
      (AvailableSpace::MinContent, "min-content"),
      (AvailableSpace::MaxContent, "max-content"),
    ] {
      let mut style = ComputedStyle::default();
      style.font_size = 16.0;
      style.word_break = WordBreak::BreakAll;
      let style = Arc::new(style);
      let text_child = BoxNode::new_text(style.clone(), "a".repeat(64));
      let mut node = BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child]);
      node.id = 1;
      let node_ptr = &node as *const _;
      let taffy_style: taffy::style::Style = taffy::style::Style::default();

      for (known_label, known_width) in [
        ("known width", Some(gc.viewport_size.width)),
        ("definite available width", None),
      ] {
        let wide_width = gc.viewport_size.width;
        reset_grid_measure_layout_calls();
        let wide = gc.measure_grid_item(
          node_ptr,
          TaffyNodeId::from(1u64),
          taffy::geometry::Size {
            width: known_width.map(|_| wide_width),
            height: None,
          },
          taffy::geometry::Size {
            width: AvailableSpace::Definite(wide_width),
            height: probe_height,
          },
          Some(wide_width),
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
        assert!(
          wide.size.height > 0.0,
          "{label} height probe should return a non-zero intrinsic height ({known_label})"
        );
        assert_eq!(
          grid_measure_layout_calls(),
          1,
          "{label} height probe should measure by laying out the item with the known inline size ({known_label})"
        );

        let narrow_width = 50.0;
        reset_grid_measure_layout_calls();
        let narrow = gc.measure_grid_item(
          node_ptr,
          TaffyNodeId::from(1u64),
          taffy::geometry::Size {
            width: known_width.map(|_| narrow_width),
            height: None,
          },
          taffy::geometry::Size {
            width: AvailableSpace::Definite(narrow_width),
            height: probe_height,
          },
          Some(narrow_width),
          container_style.as_ref(),
          &container_constraints,
          &taffy_style,
          &FxHashSet::default(),
          &factory,
          &mut measure_cache,
          &mut measured_fragments,
          &mut measured_node_keys,
        );
        assert!(
          narrow.size.height > wide.size.height,
          "{label} height probe should respect the known inline size ({known_label}) (narrow={:.2}, wide={:.2})",
          narrow.size.height,
          wide.size.height
        );
        assert_eq!(
          grid_measure_layout_calls(),
          1,
          "{label} height probe should measure by laying out the item with the known inline size ({known_label})"
        );
      }
    }
  }

  #[test]
  fn measure_grid_item_height_probes_respect_definite_available_inline_size() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        gc.font_context.clone(),
        gc.viewport_size,
        gc.nearest_positioned_cb,
      );
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    for (probe_height, label) in [
      (AvailableSpace::MinContent, "min-content"),
      (AvailableSpace::MaxContent, "max-content"),
    ] {
      let mut style = ComputedStyle::default();
      style.font_size = 16.0;
      style.word_break = WordBreak::BreakAll;
      let style = Arc::new(style);
      let text_child = BoxNode::new_text(style.clone(), "a".repeat(64));
      let mut node = BoxNode::new_block(style, FormattingContextType::Inline, vec![text_child]);
      node.id = 2;
      let node_ptr = &node as *const _;
      let taffy_style: taffy::style::Style = taffy::style::Style::default();

      let wide_width = 200.0;
      reset_grid_measure_layout_calls();
      let wide = gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(2u64),
        taffy::geometry::Size {
          width: None,
          height: None,
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(wide_width),
          height: probe_height,
        },
        Some(wide_width),
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
      assert!(
        wide.size.height > 0.0,
        "{label} height probe should return a non-zero intrinsic height"
      );
      assert_eq!(
        grid_measure_layout_calls(),
        1,
        "{label} height probe should measure by laying out the item with the available inline size"
      );

      let narrow_width = 50.0;
      reset_grid_measure_layout_calls();
      let narrow = gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(2u64),
        taffy::geometry::Size {
          width: None,
          height: None,
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(narrow_width),
          height: probe_height,
        },
        Some(narrow_width),
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      );
      assert!(
        narrow.size.height > wide.size.height,
        "{label} height probe should respect the available inline size (narrow={:.2}, wide={:.2})",
        narrow.size.height,
        wide.size.height
      );
      assert_eq!(
        grid_measure_layout_calls(),
        1,
        "{label} height probe should measure by laying out the item with the available inline size"
      );
    }
  }

  #[test]
  fn measure_grid_item_intrinsic_width_probe_does_not_echo_definite_available_height() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory = gc.factory.clone();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.height = Some(Length::px(50.0));
    let style = Arc::new(style);
    let mut node = BoxNode::new_block(style, FormattingContextType::Grid, vec![]);
    node.id = 1;
    let taffy_style: taffy::style::Style = taffy::style::Style::default();

    let output = gc.measure_grid_item(
      &node as *const _,
      TaffyNodeId::from(1u64),
      taffy::geometry::Size {
        width: None,
        height: None,
      },
      taffy::geometry::Size {
        width: AvailableSpace::MinContent,
        height: AvailableSpace::Definite(1000.0),
      },
      Some(100.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      (output.size.height - 50.0).abs() <= 0.5,
      "expected intrinsic width probe to preserve the node's intrinsic block size, got {:.2}",
      output.size.height
    );
  }

  #[test]
  fn measure_grid_item_ignores_stretched_known_height_when_override_not_allowed() {
    use taffy::style::AvailableSpace;

    let gc = GridFormattingContext::new();
    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        gc.font_context.clone(),
        gc.viewport_size,
        gc.nearest_positioned_cb,
      );
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();

    let width = 200.0;
    let stretched_height = 500.0;

    let mut child_style = ComputedStyle::default();
    child_style.display = CssDisplay::Block;
    child_style.height = Some(Length::px(10.0));
    child_style.height_keyword = None;
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Block;
    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![child]);
    node.id = 1;
    let node_ptr = &node as *const _;

    let mut taffy_style: taffy::style::Style = taffy::style::Style::default();
    taffy_style.align_self = Some(taffy::style::AlignItems::Stretch);

    let baseline = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(1u64),
        taffy::geometry::Size {
          width: Some(width),
          height: None,
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(stretched_height),
        },
        Some(width),
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    assert!(
      (baseline.size.height - 10.0).abs() < 0.5,
      "expected auto-height grid item to remain content-sized, got {:.2}px",
      baseline.size.height
    );

    let stretched = {
      let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
        FxHashMap::default();
      let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
      let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();
      gc.measure_grid_item(
        node_ptr,
        TaffyNodeId::from(1u64),
        taffy::geometry::Size {
          width: Some(width),
          height: Some(stretched_height),
        },
        taffy::geometry::Size {
          width: AvailableSpace::Definite(width),
          height: AvailableSpace::Definite(stretched_height),
        },
        Some(width),
        container_style.as_ref(),
        &container_constraints,
        &taffy_style,
        &FxHashSet::default(),
        &factory,
        &mut measure_cache,
        &mut measured_fragments,
        &mut measured_node_keys,
      )
    };

    assert!(
      (stretched.size.height - baseline.size.height).abs() < 1e-6,
      "stretched known height should not change measurement when overrides are disabled (baseline={:.2}px, stretched={:.2}px)",
      baseline.size.height,
      stretched.size.height
    );
  }

  #[test]
  fn grid_spanning_item_fit_content_height_does_not_clamp_to_auto_track_probe() {
    use crate::style::types::FlexDirection;

    // Regression: Taffy may probe spanning items with a definite available height equal to the
    // current auto-track estimate (e.g. the height of other items in the first row). When the grid
    // area's block size is *not* definite, `height: fit-content` must not clamp to that probe size,
    // otherwise the spanning item can fail to contribute to track sizing (and end up clipped by an
    // `overflow:hidden` ancestor).
    //
    // This matches a pattern on howtogeek.com where the hero card spans multiple auto rows and uses
    // `display:flex` + `height:fit-content`.
    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.width = Some(Length::px(200.0));
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
    ];
    grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
    grid_style.justify_items = AlignItems::Start;
    grid_style.align_items = AlignItems::Start;
    let grid_style = Arc::new(grid_style);

    let mut tall_child_style = ComputedStyle::default();
    tall_child_style.display = CssDisplay::Block;
    tall_child_style.height = Some(Length::px(200.0));
    tall_child_style.height_keyword = None;
    let tall_child = BoxNode::new_block(
      Arc::new(tall_child_style),
      FormattingContextType::Block,
      vec![],
    );

    let mut spanning_style = ComputedStyle::default();
    spanning_style.display = CssDisplay::Flex;
    spanning_style.flex_direction = FlexDirection::Column;
    spanning_style.height = None;
    spanning_style.height_keyword = Some(IntrinsicSizeKeyword::FitContent { limit: None });
    spanning_style.align_self = Some(AlignItems::Start);
    spanning_style.overflow_x = Overflow::Hidden;
    spanning_style.overflow_y = Overflow::Hidden;
    spanning_style.grid_column_start = 1;
    spanning_style.grid_column_end = 2;
    spanning_style.grid_row_start = 1;
    spanning_style.grid_row_end = 3;
    let spanning_item = BoxNode::new_block(
      Arc::new(spanning_style),
      FormattingContextType::Flex,
      vec![tall_child],
    );

    let mut small_style = ComputedStyle::default();
    small_style.display = CssDisplay::Block;
    small_style.height = Some(Length::px(20.0));
    small_style.height_keyword = None;
    small_style.grid_column_start = 2;
    small_style.grid_column_end = 3;
    small_style.grid_row_start = 1;
    small_style.grid_row_end = 2;
    let small_item =
      BoxNode::new_block(Arc::new(small_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![spanning_item, small_item],
    );
    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 500.0))
      .expect("layout should succeed");

    assert_eq!(fragment.children.len(), 2);
    let (left, right) = if fragment.children[0].bounds.x() <= fragment.children[1].bounds.x() {
      (&fragment.children[0], &fragment.children[1])
    } else {
      (&fragment.children[1], &fragment.children[0])
    };

    assert!(
      (right.bounds.height() - 20.0).abs() < 0.5,
      "expected non-spanning item height≈20px, got {:.2}",
      right.bounds.height()
    );
    assert!(
      (left.bounds.height() - 200.0).abs() < 0.5,
      "expected spanning fit-content item height≈200px, got {:.2}",
      left.bounds.height()
    );
    assert!(
      (fragment.bounds.height() - 200.0).abs() < 0.5,
      "expected grid auto height≈200px, got {:.2}",
      fragment.bounds.height()
    );
  }

  #[test]
  fn measure_grid_item_does_not_resolve_percent_heights_against_stretched_auto_track_probe() {
    use taffy::style::AvailableSpace;

    // Regression: Taffy can supply a definite "known height" for stretched items even when the
    // grid track is content-sized (auto). That stretched probe must not become a definite
    // containing-block height for percentage descendants, otherwise `height:100%` feeds back into
    // track sizing and inflates the row.
    let gc = GridFormattingContext::new();
    let factory =
      crate::layout::contexts::factory::FormattingContextFactory::with_font_context_viewport_and_cb(
        gc.font_context.clone(),
        gc.viewport_size,
        gc.nearest_positioned_cb,
      );
    let container_style = make_grid_style();
    let container_constraints = LayoutConstraints::indefinite();
    let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> = FxHashMap::default();
    let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
    let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

    let mut fixed_style = ComputedStyle::default();
    fixed_style.display = CssDisplay::Block;
    fixed_style.height = Some(Length::px(10.0));
    fixed_style.height_keyword = None;
    let fixed_child =
      BoxNode::new_block(Arc::new(fixed_style), FormattingContextType::Block, vec![]);

    let mut percent_style = ComputedStyle::default();
    percent_style.display = CssDisplay::Block;
    percent_style.height = Some(Length::percent(100.0));
    percent_style.height_keyword = None;
    let percent_child = BoxNode::new_block(
      Arc::new(percent_style),
      FormattingContextType::Block,
      vec![fixed_child],
    );

    let mut item_style = ComputedStyle::default();
    item_style.display = CssDisplay::Block;
    let mut item = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![percent_child],
    );
    item.id = 1;

    let mut taffy_style: taffy::style::Style = taffy::style::Style::default();
    taffy_style.align_self = Some(taffy::style::AlignItems::Stretch);

    let output = gc.measure_grid_item(
      &item as *const _,
      TaffyNodeId::from(1u64),
      taffy::geometry::Size {
        width: Some(100.0),
        height: Some(200.0),
      },
      taffy::geometry::Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(200.0),
      },
      Some(100.0),
      container_style.as_ref(),
      &container_constraints,
      &taffy_style,
      &FxHashSet::default(),
      &factory,
      &mut measure_cache,
      &mut measured_fragments,
      &mut measured_node_keys,
    );

    assert!(
      (output.size.height - 10.0).abs() < 0.5,
      "expected `height:100%` descendant to compute to auto under an auto track probe (got {:.2})",
      output.size.height
    );
  }

  #[test]
  fn grid_auto_rows_min_and_max_content_measure_text_height() {
    let gc = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite_width(320.0);

    for (track, label) in [
      (GridTrack::MinContent, "min-content"),
      (GridTrack::MaxContent, "max-content"),
    ] {
      let mut grid_style = ComputedStyle::default();
      grid_style.display = CssDisplay::Grid;
      grid_style.grid_auto_rows = vec![track].into();
      let grid_style = Arc::new(grid_style);

      let item = make_text_item(
        "This grid item should contribute a non-zero intrinsic height",
        16.0,
      );

      let tree = BoxTree::new(BoxNode::new_block(
        grid_style,
        FormattingContextType::Grid,
        vec![item],
      ));

      let fragment = gc.layout(&tree.root, &constraints).expect("grid layout");
      assert_eq!(fragment.children.len(), 1);
      assert!(
        fragment.children[0].bounds.height() > 0.0,
        "{label} auto row should be sized from inline text height"
      );
    }
  }

  #[test]
  fn convert_style_sets_overflow_and_scrollbar_width() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::new())),
      || {
        let mut style = ComputedStyle::default();
        style.display = CssDisplay::Grid;
        style.overflow_x = Overflow::Scroll;
        style.overflow_y = Overflow::Clip;
        style.scrollbar_width = ScrollbarWidth::Thin;

        let gc = GridFormattingContext::new();
        // Model overlay scrollbars by default: no gutter reservation unless `scrollbar-gutter: stable`
        // is requested.
        let node = BoxNode::new_block(Arc::new(style.clone()), FormattingContextType::Grid, vec![]);
        let taffy_style =
          gc.convert_style(&node.style, None, None, None, false, true, node.is_replaced());

        assert_eq!(taffy_style.overflow.x, TaffyOverflow::Scroll);
        assert_eq!(taffy_style.overflow.y, TaffyOverflow::Clip);
        assert_eq!(taffy_style.scrollbar_width, 0.0);

        // Stable gutter requests should propagate the scrollbar width into Taffy so it can reserve
        // space for scroll containers.
        let mut stable = style;
        stable.scrollbar_gutter.stable = true;
        let node = BoxNode::new_block(Arc::new(stable), FormattingContextType::Grid, vec![]);
        let taffy_style =
          gc.convert_style(&node.style, None, None, None, false, true, node.is_replaced());

        assert_eq!(taffy_style.overflow.x, TaffyOverflow::Scroll);
        assert_eq!(taffy_style.overflow.y, TaffyOverflow::Clip);
        assert_eq!(
          taffy_style.scrollbar_width,
          resolve_scrollbar_width(&node.style)
        );
      },
    );
  }

  #[test]
  fn convert_style_sets_axes_swapped_for_vertical_writing_modes() {
    let gc = GridFormattingContext::new();
    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;

    style.writing_mode = WritingMode::HorizontalTb;
    let taffy_style = gc.convert_style(&style, None, None, None, false, true, false);
    assert!(
      !taffy_style.axes_swapped,
      "horizontal-tb should not transpose inline/block axes"
    );

    style.writing_mode = WritingMode::VerticalRl;
    let taffy_style = gc.convert_style(&style, None, None, None, false, true, false);
    assert!(
      taffy_style.axes_swapped,
      "vertical writing-modes should transpose inline/block axes into physical axes"
    );
  }

  #[test]
  fn convert_style_subgrids_inherit_axis_mapping_from_parent_grid() {
    let gc = GridFormattingContext::new();

    let mut parent_style = ComputedStyle::default();
    parent_style.display = CssDisplay::Grid;
    parent_style.writing_mode = WritingMode::HorizontalTb;
    let parent_axis = GridAxisStyle::from_style(&parent_style);

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.writing_mode = WritingMode::VerticalRl;
    subgrid_style.grid_column_subgrid = true;

    let taffy_style = gc.convert_style(
      &subgrid_style,
      Some(&parent_style),
      Some(parent_axis),
      None,
      false,
      true,
      false,
    );
    assert!(
      !taffy_style.axes_swapped,
      "subgrid track inheritance is expressed in the containing grid's axis mapping"
    );

    parent_style.writing_mode = WritingMode::VerticalRl;
    let parent_axis = GridAxisStyle::from_style(&parent_style);
    subgrid_style.writing_mode = WritingMode::HorizontalTb;
    let taffy_style = gc.convert_style(
      &subgrid_style,
      Some(&parent_style),
      Some(parent_axis),
      None,
      false,
      true,
      false,
    );
    assert!(
      taffy_style.axes_swapped,
      "subgrid axis mapping should follow the containing grid when subgrid is enabled"
    );
  }

  #[test]
  fn simple_grids_use_block_fast_path() {
    // Grid with default implicit tracks and a single child should use the simple fast path.
    let mut parent = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Grid,
      vec![],
    );
    let child1 = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    let child2 = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![],
    );
    parent.children.push(child1);
    parent.children.push(child2);

    let gc = GridFormattingContext::new();
    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let _root_id = gc
      .build_taffy_tree(
        &mut TaffyTree::new(),
        &parent,
        parent.style.as_ref(),
        &constraints,
        &mut FxHashMap::default(),
    )
      .expect("grid conversion");
    // If the fast path is taken, the parent container style should have been converted to block.
    let taffy_style =
      gc.convert_style(&parent.style, None, None, None, true, true, parent.is_replaced());
    assert_eq!(taffy_style.display, Display::Block);
  }

  // Test 1: Basic grid container creation
  #[test]
  fn test_grid_fc_creation() {
    let fc = GridFormattingContext::new();
    let default_fc = GridFormattingContext::default();
    assert_eq!(
      std::mem::size_of_val(&fc),
      std::mem::size_of_val(&default_fc)
    );
  }

  // Test 2: Empty grid layout
  #[test]
  fn test_empty_grid_layout() {
    let fc = GridFormattingContext::new();
    let box_node = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![]);
    let constraints = LayoutConstraints::definite(800.0, 600.0);

    let fragment = fc.layout(&box_node, &constraints).unwrap();
    assert!(fragment.bounds.width() >= 0.0);
    assert!(fragment.bounds.height() >= 0.0);
  }

  // Test 3: Grid with single child
  #[test]
  fn test_grid_single_child() {
    let fc = GridFormattingContext::new();

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn grid_template_calc_percentage_resolves_against_definite_container_size() {
    let fc = GridFormattingContext::with_viewport(Size::new(1000.0, 100.0));

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_column_gap = Length::px(40.0);
    // `calc(100% - 40px - 320px)`
    let calc = CalcLength::single(LengthUnit::Percent, 100.0)
      .add_scaled(&CalcLength::single(LengthUnit::Px, 360.0), -1.0)
      .expect("build calc length");
    container_style.grid_template_columns = vec![
      GridTrack::Length(Length::calc(calc)),
      GridTrack::Length(Length::px(320.0)),
    ];
    container_style.grid_template_rows = vec![GridTrack::Auto];
    let container_style = Arc::new(container_style);

    let mut item1_style = ComputedStyle::default();
    item1_style.height = Some(Length::px(10.0));
    item1_style.grid_column_start = 1;
    item1_style.grid_column_end = 2;
    item1_style.grid_row_start = 1;
    item1_style.grid_row_end = 2;
    let mut item1 = BoxNode::new_block(Arc::new(item1_style), FormattingContextType::Block, vec![]);
    item1.id = 101;

    let mut item2_style = ComputedStyle::default();
    item2_style.height = Some(Length::px(10.0));
    item2_style.grid_column_start = 2;
    item2_style.grid_column_end = 3;
    item2_style.grid_row_start = 1;
    item2_style.grid_row_end = 2;
    let mut item2 = BoxNode::new_block(Arc::new(item2_style), FormattingContextType::Block, vec![]);
    item2.id = 102;

    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Grid,
      vec![item1, item2],
    );
    let constraints = LayoutConstraints::definite(1000.0, 100.0);
    let fragment = fc.layout(&container, &constraints).expect("layout");

    let item1_fragment = find_block_fragment(&fragment, 101);
    let item2_fragment = find_block_fragment(&fragment, 102);

    let expected_first_track_width = 1000.0 - 40.0 - 320.0;
    let expected_second_x = expected_first_track_width + 40.0;

    assert!(
      (item1_fragment.bounds.width() - expected_first_track_width).abs() < 0.5,
      "expected first column width {:.1}, got {:.1}",
      expected_first_track_width,
      item1_fragment.bounds.width()
    );
    assert!(
      (item2_fragment.bounds.x() - expected_second_x).abs() < 0.5,
      "expected second column x {:.1}, got {:.1}",
      expected_second_x,
      item2_fragment.bounds.x()
    );
    assert!(
      (item2_fragment.bounds.width() - 320.0).abs() < 0.5,
      "expected second column width 320.0, got {:.1}",
      item2_fragment.bounds.width()
    );
  }

  #[test]
  fn intrinsic_block_size_does_not_recurse_for_vertical_writing_mode_intrinsic_width() {
    let fc = GridFormattingContext::with_viewport(Size::new(200.0, 200.0));

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.writing_mode = WritingMode::VerticalRl;
    container_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);

    let mut child_style = ComputedStyle::default();
    child_style.width = Some(Length::px(40.0));
    child_style.height = Some(Length::px(20.0));
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      Arc::new(container_style),
      FormattingContextType::Grid,
      vec![child],
    );

    let size = fc
      .compute_intrinsic_block_size(&container, IntrinsicSizingMode::MaxContent)
      .expect("compute intrinsic block size");

    assert!(size.is_finite() && size > 0.0);
  }

  // Test 4: Grid with multiple children
  #[test]
  fn test_grid_multiple_children() {
    let fc = GridFormattingContext::new();

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child3 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![child1, child2, child3],
    );

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 3);
  }

  #[test]
  fn grid_order_reorders_auto_placement_and_preserves_fragment_mapping() {
    let first_id = 601usize;
    let second_id = 602usize;

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
    container_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(10.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    let container_style = Arc::new(container_style);

    let mut first_style = ComputedStyle::default();
    first_style.order = 1;
    let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
    first.id = first_id;

    let mut second_style = ComputedStyle::default();
    second_style.order = 0;
    let mut second =
      BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
    second.id = second_id;

    // DOM order places `first` before `second`, but `order` should cause `second` to be placed in
    // the first row and `first` in the second row.
    let container = BoxNode::new_block(
      container_style,
      FormattingContextType::Grid,
      vec![first, second],
    );

    // Force the parallel child conversion path so `in_flow_children` ordering must match the
    // Taffy child order.
    let fc = GridFormattingContext::with_viewport(Size::new(100.0, 100.0))
      .with_parallelism(LayoutParallelism::enabled(1).with_max_threads(Some(2)));
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc
      .layout(&container, &constraints)
      .expect("layout should succeed");

    let first_fragment = find_block_fragment(&fragment, first_id);
    let second_fragment = find_block_fragment(&fragment, second_id);
    assert!(
      (second_fragment.bounds.y() - 0.0).abs() < 0.1,
      "expected second (order 0) item at y=0, got {:?}",
      second_fragment.bounds
    );
    assert!(
      (first_fragment.bounds.y() - 10.0).abs() < 0.1,
      "expected first (order 1) item at y=10, got {:?}",
      first_fragment.bounds
    );
  }

  #[test]
  fn grid_layout_is_deterministic_across_rayon_threads() {
    // This test intentionally compares the *full fragment positions* produced by grid layout under
    // different rayon pool sizes. The grid code paths use rayon for child layout and fragment
    // conversion; the output must remain deterministic regardless of how work is partitioned.

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let mut children = Vec::new();
    let mut ids = Vec::new();
    for i in 0..32usize {
      let mut style = ComputedStyle::default();
      style.height = Some(Length::px(10.0 + (i % 3) as f32));
      let style = Arc::new(style);
      let id = 1000 + i;
      let mut child = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
      child.id = id;
      children.push(child);
      ids.push(id);
    }

    let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, children);
    grid.id = 999;
    let constraints = LayoutConstraints::definite(400.0, 400.0);

    let run = |threads| -> Vec<(usize, Rect)> {
      let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("build rayon pool");
      pool.install(|| {
        let fc = GridFormattingContext::with_viewport(Size::new(400.0, 400.0)).with_parallelism(
          LayoutParallelism::enabled(1).with_max_threads(Some(threads)),
        );
        let fragment = fc.layout(&grid, &constraints).expect("layout should succeed");
        let mut result: Vec<(usize, Rect)> = ids
          .iter()
          .map(|id| (*id, find_block_fragment(&fragment, *id).bounds))
          .collect();
        result.sort_by_key(|(id, _)| *id);
        result
      })
    };

    let layout_2 = run(2);
    let layout_4 = run(4);

    assert_eq!(layout_2.len(), layout_4.len());
    let eps = 0.01;
    for ((id_2, rect_2), (id_4, rect_4)) in layout_2.iter().zip(layout_4.iter()) {
      assert_eq!(id_2, id_4);
      assert!(
        (rect_2.x() - rect_4.x()).abs() < eps
          && (rect_2.y() - rect_4.y()).abs() < eps
          && (rect_2.width() - rect_4.width()).abs() < eps
          && (rect_2.height() - rect_4.height()).abs() < eps,
        "grid layout mismatch for id={id_2}: rect_2={rect_2:?} rect_4={rect_4:?}"
      );
    }
  }

  #[test]
  fn measured_fragments_are_reused_without_cloning() {
    runtime::with_thread_runtime_toggles(
      Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PROFILE_FRAGMENT_CLONES".to_string(),
        "1".to_string(),
      )]))),
      || {
        fragment_clone_profile::reset_fragment_clone_profile();

        let fc = GridFormattingContext::new();

        let mut item_style = ComputedStyle::default();
        item_style.font_size = 12.0;
        item_style.grid_column_start = 2;
        let item_style = Arc::new(item_style);
        let text_child = BoxNode::new_text(item_style.clone(), "reuse-me".to_string());
        let mut item = BoxNode::new_block(
          item_style.clone(),
          FormattingContextType::Inline,
          vec![text_child],
        );
        item.id = 2;

        let mut grid_style = ComputedStyle::default();
        grid_style.display = CssDisplay::Grid;
        grid_style.grid_template_columns =
          vec![GridTrack::Length(Length::px(50.0)), GridTrack::Auto];
        let grid_style = Arc::new(grid_style);
        let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
        grid.id = 1;

        let constraints = LayoutConstraints::definite(180.0, 100.0);

        let mut taffy: TaffyTree<*const BoxNode> = TaffyTree::new();
        let mut positioned_children_map: FxHashMap<TaffyNodeId, Vec<*const BoxNode>> =
          FxHashMap::default();
        let in_flow_children: Vec<&BoxNode> = grid.children.iter().collect();
        let root_id = fc
          .build_taffy_tree_children(
            &mut taffy,
            &grid,
            grid.style.as_ref(),
            &in_flow_children,
            &constraints,
            &mut positioned_children_map,
          )
          .expect("build taffy tree");

        let mut measure_cache: FxHashMap<MeasureKey, taffy::tree::MeasureOutput> =
          FxHashMap::default();
        let mut measured_fragments: FxHashMap<MeasureKey, FragmentNode> = FxHashMap::default();
        let mut measured_node_keys: FxHashMap<TaffyNodeId, Vec<MeasureKey>> = FxHashMap::default();

        let available_space = taffy::geometry::Size {
          width: taffy::style::AvailableSpace::Definite(constraints.width().unwrap()),
          height: taffy::style::AvailableSpace::Definite(constraints.height().unwrap()),
        };

        let factory = fc.factory.clone();
        let viewport_size = fc.viewport_size;
        let parent_inline_base = constraints.inline_percentage_base;
        let cache = &mut measure_cache;
        let measured = &mut measured_fragments;
        let measured_keys = &mut measured_node_keys;
        let this = fc.clone();
        taffy
          .compute_layout_with_measure(
            root_id,
            available_space,
            move |known_dimensions,
                  available_space,
                  node_id,
                  node_context,
                  _style: &taffy::style::Style| {
              if node_id == root_id {
                let fallback_size =
                  |known: Option<f32>, avail_dim: taffy::style::AvailableSpace| {
                    known.unwrap_or(match avail_dim {
                      taffy::style::AvailableSpace::Definite(v) => v,
                      _ => 0.0,
                    })
                  };
                return taffy::geometry::Size {
                  width: fallback_size(known_dimensions.width, available_space.width),
                  height: fallback_size(known_dimensions.height, available_space.height),
                }
                .into();
              }

              let Some(node_ptr) = node_context.as_ref().map(|p| **p) else {
                return taffy::tree::MeasureOutput::ZERO;
              };
              let box_node = unsafe { &*node_ptr };

              let key = MeasureKey::new(
                box_node,
                known_dimensions,
                available_space,
                viewport_size,
                false,
              );
              if let Some(size) = cache.get(&key) {
                return *size;
              }
              let fc_type = box_node
                .formatting_context()
                .unwrap_or(FormattingContextType::Block);
              let fc = factory.create(fc_type);

              let child_constraints = constraints_from_taffy(
                viewport_size,
                known_dimensions,
                available_space,
                box_node.style.writing_mode,
                parent_inline_base,
              );

              let mut fragment = match fc.layout(box_node, &child_constraints) {
                Ok(fragment) => fragment,
                Err(LayoutError::Timeout { .. }) => taffy::abort_layout_now(),
                Err(_) => return taffy::tree::MeasureOutput::ZERO,
              };
              let percentage_base = match available_space.width {
                taffy::style::AvailableSpace::Definite(w) => w,
                _ => child_constraints
                  .width()
                  .unwrap_or_else(|| fragment.bounds.width()),
              };
              fragment.content = match &box_node.box_type {
                crate::tree::box_tree::BoxType::Replaced(replaced_box) => {
                  FragmentContent::Replaced {
                    replaced_type: replaced_box.replaced_type.clone(),
                    box_id: Some(box_node.id),
                  }
                }
                _ => FragmentContent::Block {
                  box_id: Some(box_node.id),
                },
              };
              fragment.style = Some(box_node.style.clone());
              let content_size = this.content_box_size(&fragment, &box_node.style, percentage_base);
              let size = taffy::geometry::Size {
                width: content_size.width.max(0.0),
                height: content_size.height.max(0.0),
              };
              let output = taffy::tree::MeasureOutput::from_size(size);
              measured_keys.entry(node_id).or_default().push(key);
              measured.insert(key, fragment);
              cache.insert(key, output);
              output
            },
          )
          .expect("taffy layout");

        let child_id = *taffy.children(root_id).unwrap().first().unwrap();
        let child_layout = taffy.layout(child_id).unwrap();
        let before_len = measured_fragments.len();
        assert!(before_len > 0, "expected measured fragments to be recorded");
        let child_layout_unrounded = taffy.unrounded_layout(child_id);
        let matched_key = measured_node_keys
          .get(&child_id)
          .and_then(|keys| {
            keys.iter().copied().find(|key| {
              measured_fragments.get(key).map_or(false, |fragment| {
                (fragment.bounds.width() - child_layout_unrounded.size.width).abs() < 0.1
                  && (fragment.bounds.height() - child_layout_unrounded.size.height).abs() < 0.1
              })
            })
          })
          .expect("matching measured fragment should exist");

        let mut deadline_counter = 0usize;
        let mut fragment = fc
          .convert_to_fragments(
            &taffy,
            root_id,
            root_id,
            &constraints,
            None,
            None,
            None,
            None,
            &mut measured_fragments,
            &measured_node_keys,
            &positioned_children_map,
            &mut deadline_counter,
          )
          .expect("convert fragments");

        assert!(
          !measured_fragments.contains_key(&matched_key),
          "reused fragments should be removed from the cache"
        );
        assert!(
          measured_fragments.len() < before_len,
          "reusing a fragment should reduce the cache size"
        );
        assert_eq!(fragment.children.len(), 1);
        let child_fragment = fragment.children.pop().unwrap();
        match &child_fragment.content {
          FragmentContent::Block { box_id } => assert_eq!(*box_id, Some(2)),
          other => panic!("unexpected fragment content: {other:?}"),
        }
        assert!(child_fragment.style.is_some());
        assert!(
          (child_fragment.bounds.x() - child_layout.location.x).abs() < 0.01,
          "expected translated x to match layout"
        );
        assert!(
          (child_fragment.bounds.y() - child_layout.location.y).abs() < 0.01,
          "expected translated y to match layout"
        );

        fn has_text(node: &FragmentNode) -> bool {
          if matches!(node.content, FragmentContent::Text { .. }) {
            return true;
          }
          node.children.iter().any(has_text)
        }
        assert!(has_text(&child_fragment));

        let stats = fragment_clone_profile::fragment_clone_profile_stats();
        assert_eq!(
          stats.grid_measure_reuse.nodes, 0,
          "reuse should not record cloned nodes"
        );
        assert!(
          stats.grid_measure_reuse.events > 0,
          "reuse should be recorded when clone profiling is enabled"
        );
        fragment_clone_profile::reset_fragment_clone_profile();
      },
    );
  }

  // Test 5: Grid with explicit columns
  #[test]
  fn test_grid_explicit_columns() {
    let fc = GridFormattingContext::new();

    let style = make_grid_style_with_tracks(
      vec![GridTrack::Length(Length::px(100.0)), GridTrack::Fr(1.0)],
      vec![],
    );

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  // Test 6: Grid with explicit rows
  #[test]
  fn test_grid_explicit_rows() {
    let fc = GridFormattingContext::new();

    let style = make_grid_style_with_tracks(
      vec![],
      vec![
        GridTrack::Length(Length::px(50.0)),
        GridTrack::Length(Length::px(100.0)),
      ],
    );

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 300.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  #[test]
  fn absolute_child_inherits_positioned_cb_into_grid() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;

    let mut abs_style = ComputedStyle::default();
    abs_style.display = CssDisplay::Block;
    abs_style.position = crate::style::position::Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(5.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(7.0));
    abs_style.width = Some(Length::px(12.0));
    abs_style.height = Some(Length::px(9.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;

    let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![abs_child],
    );

    let viewport = crate::geometry::Size::new(300.0, 300.0);
    let cb_rect = crate::geometry::Rect::from_xywh(20.0, 30.0, 150.0, 150.0);
    let cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport(cb_rect, viewport);
    let fc = GridFormattingContext::with_viewport_and_cb(
      viewport,
      cb,
      crate::text::font_loader::FontContext::new(),
    );
    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    let abs_fragment = &fragment.children[0];
    assert_eq!(abs_fragment.bounds.x(), 25.0);
    assert_eq!(abs_fragment.bounds.y(), 37.0);
    assert_eq!(abs_fragment.bounds.width(), 12.0);
    assert_eq!(abs_fragment.bounds.height(), 9.0);
  }

  #[test]
  fn abspos_grid_item_percentage_width_resolves_against_grid_area() {
    // CSS Grid § Absolute positioning: when a grid container establishes the containing block for an
    // absolutely positioned child, the child's grid placement rectangle becomes its containing
    // block. Percentage sizes and inset offsets should resolve against that grid area, not the
    // entire grid container.
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.position = crate::style::position::Position::Relative;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(100.0)),
    ];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];

    let mut abs_style = ComputedStyle::default();
    abs_style.display = CssDisplay::Block;
    abs_style.position = crate::style::position::Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Auto;
    abs_style.right = crate::style::types::InsetValue::Auto;
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::percent(100.0));
    abs_style.height = Some(Length::px(10.0));
    abs_style.width_keyword = None;
    abs_style.height_keyword = None;
    // Place the absolute element in the second column only (line 2..3).
    abs_style.grid_column_start = 2;
    abs_style.grid_column_end = 3;

    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = 2;
    let mut grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![abs_child],
    );
    grid.id = 1;

    let fragment = fc
      .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
      .expect("grid layout");
    assert_eq!(fragment.children.len(), 1);

    let abs_fragment = &fragment.children[0];
    assert!((abs_fragment.bounds.x() - 100.0).abs() < 0.01);
    assert!((abs_fragment.bounds.width() - 100.0).abs() < 0.01);
  }

  #[test]
  fn subgrid_content_visibility_establishes_positioned_cb_for_absolute_children() {
    fn find_by_box_id<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
      if node.box_id() == Some(box_id) {
        return Some(node);
      }
      node
        .children
        .iter()
        .find_map(|child| find_by_box_id(child, box_id))
    }

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0))];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];

    let mut subgrid_style = ComputedStyle::default();
    subgrid_style.display = CssDisplay::Grid;
    subgrid_style.grid_row_subgrid = true;
    subgrid_style.grid_column_subgrid = true;
    subgrid_style.content_visibility = ContentVisibility::Auto;
    apply_container_type_implied_containment(&mut subgrid_style);
    apply_content_visibility_implied_containment(&mut subgrid_style);

    let mut abs_style = ComputedStyle::default();
    abs_style.display = CssDisplay::Block;
    abs_style.position = crate::style::position::Position::Absolute;
    abs_style.left = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.top = crate::style::types::InsetValue::Length(Length::px(0.0));
    abs_style.width = Some(Length::px(10.0));
    abs_style.height = Some(Length::px(10.0));

    let mut abs_child =
      BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
    abs_child.id = 3;

    let mut subgrid = BoxNode::new_block(
      Arc::new(subgrid_style),
      FormattingContextType::Grid,
      vec![abs_child],
    );
    subgrid.id = 2;

    let mut grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![subgrid],
    );
    grid.id = 1;

    let viewport = crate::geometry::Size::new(300.0, 300.0);
    let cb_rect = crate::geometry::Rect::from_xywh(20.0, 30.0, 150.0, 150.0);
    let cb = crate::layout::contexts::positioned::ContainingBlock::with_viewport(cb_rect, viewport);
    let fc = GridFormattingContext::with_viewport_and_cb(
      viewport,
      cb,
      crate::text::font_loader::FontContext::new(),
    )
    .with_parallelism(LayoutParallelism::disabled());
    let constraints = LayoutConstraints::definite(50.0, 50.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let subgrid_fragment = find_by_box_id(&fragment, 2).expect("subgrid fragment should exist");
    let abs_fragment = subgrid_fragment
      .children
      .iter()
      .find(|child| child.box_id() == Some(3))
      .expect("absolute child fragment should be nested under subgrid fragment");

    // Content-visibility's implied containment should establish the containing block for the
    // absolutely positioned child. This prevents it from being offset by the outer positioned CB.
    assert_eq!(abs_fragment.bounds.x(), 0.0);
    assert_eq!(abs_fragment.bounds.y(), 0.0);
  }

  // Test 7: Grid with gap
  #[test]
  fn test_grid_with_gap() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_column_gap = Length::px(10.0);
    style.grid_row_gap = Length::px(20.0);
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(410.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  #[test]
  fn grid_item_percentage_width_does_not_expand_fr_tracks() {
    // Regression test for cases like CNET where a grid item with `width: 100%` spans multiple
    // `fr` tracks. The percentage depends on the grid area size and must not force track sizing to
    // expand the spanned tracks to the full container width.
    let fc = GridFormattingContext::new();

    let gap = 2.0;
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_column_gap = Length::px(gap);
    grid_style.grid_row_gap = Length::px(0.0);
    grid_style.grid_template_columns = vec![GridTrack::Fr(1.0); 10];
    let grid_style = Arc::new(grid_style);

    let mut media_style = ComputedStyle::default();
    media_style.width = Some(Length::percent(100.0));
    media_style.height = Some(Length::px(100.0));
    media_style.grid_column_raw = Some("auto / span 5".to_string());
    let mut media = BoxNode::new_block(Arc::new(media_style), FormattingContextType::Block, vec![]);
    media.id = 2;

    let mut meta_style = ComputedStyle::default();
    meta_style.height = Some(Length::px(100.0));
    meta_style.grid_column_raw = Some("auto / span 5".to_string());
    let mut meta = BoxNode::new_block(Arc::new(meta_style), FormattingContextType::Block, vec![]);
    meta.id = 3;

    let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![media, meta]);
    grid.id = 1;

    let container_width = 1000.0;
    let constraints = LayoutConstraints::definite(container_width, 200.0);
    let fragment = fc.layout(&grid, &constraints).expect("grid layout");

    let media_fragment = find_block_fragment(&fragment, 2);
    let meta_fragment = find_block_fragment(&fragment, 3);

    let track_width = (container_width - gap * 9.0) / 10.0;
    let expected_item_width = track_width * 5.0 + gap * 4.0;
    assert!(
      (media_fragment.bounds.width() - expected_item_width).abs() < 0.5,
      "expected media width≈{expected_item_width:.2}, got {:.2}",
      media_fragment.bounds.width()
    );
    assert!(
      (meta_fragment.bounds.width() - expected_item_width).abs() < 0.5,
      "expected meta width≈{expected_item_width:.2}, got {:.2}",
      meta_fragment.bounds.width()
    );
    assert!(
      (meta_fragment.bounds.x() - (expected_item_width + gap)).abs() < 0.5,
      "expected meta x≈{:.2}, got {:.2}",
      expected_item_width + gap,
      meta_fragment.bounds.x()
    );
  }

  #[test]
  fn grid_item_descendant_percentage_width_does_not_expand_fr_tracks() {
    // Regression test for cases like buzzfeed.com where a grid item contains descendants with
    // percentage widths (e.g. `img { width: 100% }`). During grid track sizing those percentages
    // must behave like `auto` (because the grid area is not yet definite) so they don't inflate the
    // item's intrinsic contributions and force `fr` tracks to overflow the container.
    let fc = GridFormattingContext::new();

    let gap = 2.0;
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_column_gap = Length::px(gap);
    grid_style.grid_row_gap = Length::px(0.0);
    grid_style.grid_template_columns = vec![GridTrack::Fr(1.0); 10];
    let grid_style = Arc::new(grid_style);

    let mut inner_style = ComputedStyle::default();
    inner_style.width = Some(Length::percent(100.0));
    inner_style.height = Some(Length::px(100.0));
    let inner_style = Arc::new(inner_style);

    let mut item_style = ComputedStyle::default();
    item_style.grid_column_raw = Some("auto / span 5".to_string());
    item_style.height = Some(Length::px(100.0));

    let mut inner1 = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);
    inner1.id = 4;
    let mut item1 = BoxNode::new_block(
      Arc::new(item_style.clone()),
      FormattingContextType::Block,
      vec![inner1],
    );
    item1.id = 2;

    let mut inner2 = BoxNode::new_block(inner_style, FormattingContextType::Block, vec![]);
    inner2.id = 5;
    let mut item2 = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![inner2],
    );
    item2.id = 3;

    let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item1, item2]);
    grid.id = 1;

    let container_width = 1000.0;
    let constraints = LayoutConstraints::definite(container_width, 200.0);
    let fragment = fc.layout(&grid, &constraints).expect("grid layout");

    let item1_fragment = find_block_fragment(&fragment, 2);
    let item2_fragment = find_block_fragment(&fragment, 3);
    let inner1_fragment = find_block_fragment(&fragment, 4);
    let inner2_fragment = find_block_fragment(&fragment, 5);

    let track_width = (container_width - gap * 9.0) / 10.0;
    let expected_item_width = track_width * 5.0 + gap * 4.0;
    for (label, frag) in [("item1", item1_fragment), ("item2", item2_fragment)] {
      assert!(
        (frag.bounds.width() - expected_item_width).abs() < 0.5,
        "expected {label} width≈{expected_item_width:.2}, got {:.2}",
        frag.bounds.width()
      );
    }
    for (label, frag) in [("inner1", inner1_fragment), ("inner2", inner2_fragment)] {
      assert!(
        (frag.bounds.width() - expected_item_width).abs() < 0.5,
        "expected {label} width≈{expected_item_width:.2}, got {:.2}",
        frag.bounds.width()
      );
    }
  }

  #[test]
  fn grid_item_descendant_replaced_percentage_width_does_not_expand_fr_tracks() {
    use crate::tree::box_tree::ReplacedType;

    let fc = GridFormattingContext::new();

    let gap = 2.0;
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_column_gap = Length::px(gap);
    grid_style.grid_row_gap = Length::px(0.0);
    grid_style.grid_template_columns = vec![GridTrack::Fr(1.0); 10];
    let grid_style = Arc::new(grid_style);

    let mut image_style = ComputedStyle::default();
    image_style.width = Some(Length::percent(100.0));
    image_style.width_keyword = None;
    let image_style = Arc::new(image_style);

    let mut item_style = ComputedStyle::default();
    item_style.grid_column_raw = Some("auto / span 5".to_string());
    item_style.height = Some(Length::px(100.0));

    let mut image1 = BoxNode::new_replaced(
      image_style.clone(),
      ReplacedType::Canvas,
      Some(Size::new(2000.0, 2000.0)),
      None,
    );
    image1.id = 4;
    let mut item1 = BoxNode::new_block(
      Arc::new(item_style.clone()),
      FormattingContextType::Block,
      vec![image1],
    );
    item1.id = 2;

    let mut image2 = BoxNode::new_replaced(
      image_style,
      ReplacedType::Canvas,
      Some(Size::new(2000.0, 2000.0)),
      None,
    );
    image2.id = 5;
    let mut item2 = BoxNode::new_block(
      Arc::new(item_style),
      FormattingContextType::Block,
      vec![image2],
    );
    item2.id = 3;

    let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item1, item2]);
    grid.id = 1;

    let container_width = 1000.0;
    let constraints = LayoutConstraints::definite(container_width, 200.0);
    let fragment = fc.layout(&grid, &constraints).expect("grid layout");

    let item1_fragment = find_block_fragment(&fragment, 2);
    let item2_fragment = find_block_fragment(&fragment, 3);

    let track_width = (container_width - gap * 9.0) / 10.0;
    let expected_item_width = track_width * 5.0 + gap * 4.0;
    for (label, frag) in [("item1", item1_fragment), ("item2", item2_fragment)] {
      assert!(
        (frag.bounds.width() - expected_item_width).abs() < 0.5,
        "expected {label} width≈{expected_item_width:.2}, got {:.2}",
        frag.bounds.width()
      );
    }
  }

  #[test]
  fn grid_item_intrinsic_height_probe_respects_definite_cross_axis_size() {
    // Regression test for grid track sizing when an item's block-size depends on its inline size
    // (e.g. percentage padding/aspect-ratio boxes). Taffy requests min/max-content height
    // contributions while still providing a definite width. Our grid measure callback must honor
    // that width; otherwise percentage padding resolves against an indefinite base and the row can
    // collapse, clipping the item's contents (howtogeek.com featured card grid).
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    let grid_style = Arc::new(grid_style);

    let mut aspect_style = ComputedStyle::default();
    // Percentage padding resolves against the containing block's width, so at 200px this yields a
    // 200px-tall box.
    aspect_style.padding_top = Length::percent(100.0);
    let aspect_style = Arc::new(aspect_style);

    let mut aspect_box = BoxNode::new_block(aspect_style, FormattingContextType::Block, vec![]);
    aspect_box.id = 3;

    let mut item = BoxNode::new_block(
      make_item_style(),
      FormattingContextType::Block,
      vec![aspect_box],
    );
    item.id = 2;

    let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    grid.id = 1;

    let constraints = LayoutConstraints::definite_width(200.0);
    let fragment = fc.layout(&grid, &constraints).expect("grid layout");

    let item_fragment = find_block_fragment(&fragment, 2);
    assert!(
      (item_fragment.bounds.height() - 200.0).abs() < 0.5,
      "expected grid item height≈200px (got {:.2})",
      item_fragment.bounds.height()
    );
  }

  // Test 8: Grid with multiple rows
  #[test]
  fn test_grid_multiple_rows() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let children: Vec<BoxNode> = (0..4)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 4);
  }

  // Test 9: Grid item placement with line numbers
  #[test]
  fn test_grid_item_placement() {
    let fc = GridFormattingContext::new();

    let grid_style = make_grid_style_with_tracks(
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)],
      vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)],
    );

    let mut item_style = ComputedStyle::default();
    item_style.grid_column_start = 2;
    item_style.grid_row_start = 1;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  // Test 10: Intrinsic sizing - min content
  #[test]
  fn test_intrinsic_min_content() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(30.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let size = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();

    assert!(size >= 0.0);
  }

  // Test 11: Intrinsic sizing - max content
  #[test]
  fn test_intrinsic_max_content() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(100.0));
    item_style.height = Some(Length::px(50.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;

    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let size = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(size >= 0.0);
  }

  #[test]
  fn grid_intrinsic_inline_size_aborts_taffy_when_nested_grid_times_out() {
    let _lock = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::intrinsic_cache_clear();
    let _taffy_guard = crate::layout::taffy_integration::enable_taffy_counters(true);
    crate::layout::taffy_integration::reset_taffy_counters();

    // This regression ensures that a timeout from a nested formatting context (triggered here by a
    // deeply nested grid intrinsic sizing probe) aborts the outer Taffy computation and is surfaced
    // as `LayoutError::Timeout` rather than a panic or a generic MissingContext error.
    let fc = GridFormattingContext::new();

    let mut outer_style = ComputedStyle::default();
    outer_style.display = CssDisplay::Grid;
    outer_style.grid_template_columns = vec![GridTrack::Auto];
    let outer_style = Arc::new(outer_style);

    let mut inner_style = ComputedStyle::default();
    inner_style.display = CssDisplay::Grid;
    inner_style.grid_template_columns = vec![GridTrack::Auto];
    let inner_style = Arc::new(inner_style);

    // Use enough in-flow children to trip the periodic `check_layout_deadline` call inside the
    // nested grid intrinsic sizing code path (stride = 64). This makes the nested call return
    // `LayoutError::Timeout`, which should then abort the outer Taffy pass.
    let inner_children: Vec<BoxNode> = (0..32)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let inner_grid = BoxNode::new_block(inner_style, FormattingContextType::Grid, inner_children);

    let outer_grid = BoxNode::new_block(outer_style, FormattingContextType::Grid, vec![inner_grid]);

    let deadline =
      crate::render_control::RenderDeadline::new(Some(std::time::Duration::from_millis(0)), None);
    let result = crate::render_control::with_deadline(Some(&deadline), || {
      fc.compute_intrinsic_inline_size(&outer_grid, IntrinsicSizingMode::MinContent)
    });

    assert!(
      matches!(result, Err(LayoutError::Timeout { .. })),
      "expected timeout error, got {result:?}"
    );
    assert_eq!(
      crate::layout::taffy_integration::taffy_counters().grid,
      1,
      "expected outer grid intrinsic sizing to invoke Taffy before aborting"
    );
  }

  #[test]
  fn grid_intrinsic_inline_size_reuses_intrinsic_cache() {
    let _lock = crate::layout::formatting_context::intrinsic_cache_test_lock();
    crate::layout::formatting_context::intrinsic_cache_clear();

    let _taffy_guard = crate::layout::taffy_integration::enable_taffy_counters(true);
    crate::layout::taffy_integration::reset_taffy_counters();

    let fc = GridFormattingContext::new();
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let mut grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);
    grid.id = 1;
    grid.children[0].id = 2;

    let first = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert_eq!(
      crate::layout::taffy_integration::taffy_counters().grid,
      1,
      "expected first intrinsic call to invoke Taffy"
    );

    let second = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    assert_eq!(second, first);
    assert_eq!(
      crate::layout::taffy_integration::taffy_counters().grid,
      1,
      "expected repeated intrinsic calls to reuse intrinsic_cache without re-running Taffy"
    );
  }

  #[test]
  fn grid_intrinsic_size_does_not_double_count_padding_and_border() {
    let fc = GridFormattingContext::new();

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(100.0));
    item_style.width_keyword = None;
    item_style.padding_left = Length::px(10.0);
    item_style.padding_right = Length::px(10.0);
    item_style.border_left_style = BorderStyle::Solid;
    item_style.border_right_style = BorderStyle::Solid;
    item_style.border_left_width = Length::px(5.0);
    item_style.border_right_width = Length::px(5.0);
    let child = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      make_grid_style_with_tracks(vec![GridTrack::Auto], vec![GridTrack::Auto]),
      FormattingContextType::Grid,
      vec![child],
    );

    let expected = 100.0 + 10.0 + 10.0 + 5.0 + 5.0;
    let min = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MinContent)
      .unwrap();
    let max = fc
      .compute_intrinsic_inline_size(&grid, IntrinsicSizingMode::MaxContent)
      .unwrap();

    assert!(
      (min - expected).abs() < 0.01,
      "min-content intrinsic width should match the grid item's border-box width (got {:.2}, expected {:.2})",
      min,
      expected
    );
    assert!(
      (max - expected).abs() < 0.01,
      "max-content intrinsic width should match the grid item's border-box width (got {:.2}, expected {:.2})",
      max,
      expected
    );
  }

  // Test 12: Grid with minmax track
  #[test]
  fn test_grid_minmax_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::MinMax(
      Box::new(GridTrack::Length(Length::px(100.0))),
      Box::new(GridTrack::Fr(1.0)),
    )];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(500.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  // Test 13: Grid indefinite constraints
  #[test]
  fn test_grid_indefinite_constraints() {
    let fc = GridFormattingContext::new();

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(make_grid_style(), FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::indefinite();
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert!(fragment.bounds.width() >= 0.0);
    assert!(fragment.bounds.height() >= 0.0);
  }

  // Test 14: Grid with align-content
  #[test]
  fn test_grid_align_content() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.align_content = AlignContent::Center;
    style.grid_template_columns = vec![GridTrack::Fr(1.0)];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 400.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn grid_justify_items_centers_children() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
    style.justify_items = AlignItems::Center;
    style.align_items = AlignItems::FlexStart;
    let style = Arc::new(style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(20.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.x(), 75.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
  }

  #[test]
  fn grid_align_self_and_justify_self_override_container_alignment() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];
    style.justify_items = AlignItems::FlexStart;
    style.align_items = AlignItems::FlexStart;
    let style = Arc::new(style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(50.0));
    item_style.height = Some(Length::px(30.0));
    item_style.width_keyword = None;
    item_style.height_keyword = None;
    item_style.justify_self = Some(AlignItems::FlexEnd);
    item_style.align_self = Some(AlignItems::FlexEnd);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.x(), 150.0);
    assert_eq!(fragment.children[0].bounds.y(), 70.0);
  }

  #[test]
  fn grid_item_aspect_ratio_sets_height_from_width() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::FlexStart;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.width = Some(Length::px(80.0));
    item_style.width_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(2.0);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.width(), 80.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);
  }

  #[test]
  fn grid_item_aspect_ratio_sets_width_from_height() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::FlexStart;
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(60.0))];
    let grid_style = Arc::new(grid_style);

    let mut item_style = ComputedStyle::default();
    item_style.height = Some(Length::px(60.0));
    item_style.height_keyword = None;
    item_style.aspect_ratio = AspectRatio::Ratio(1.5);
    let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children[0].bounds.height(), 60.0);
    assert_eq!(fragment.children[0].bounds.width(), 90.0);
  }

  // Test 15: Grid with nested grid
  #[test]
  fn test_nested_grid() {
    let fc = GridFormattingContext::new();

    let inner_child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let inner_grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![inner_child],
    );
    let outer_grid = BoxNode::new_block(
      make_grid_style(),
      FormattingContextType::Grid,
      vec![inner_grid],
    );

    let constraints = LayoutConstraints::definite(800.0, 600.0);
    let fragment = fc.layout(&outer_grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
    assert_eq!(fragment.children[0].children.len(), 1);
  }

  // Test 16: FormattingContext trait is Send + Sync
  #[test]
  fn test_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<GridFormattingContext>();
  }

  // Test 17: Grid with percentage widths
  #[test]
  fn test_grid_percentage_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::percent(50.0)),
      GridTrack::Length(Length::percent(50.0)),
    ];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child1, child2]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 2);
  }

  // Test 18: Grid auto track
  #[test]
  fn test_grid_auto_track() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Auto];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 1);
  }

  #[test]
  fn vertical_writing_mode_swaps_tracks_for_template_sizes() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![GridTrack::Length(Length::px(30.0))];
    let style = Arc::new(style);

    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, vec![child]);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    // Inline axis vertical: column tracks become rows (height = 20+20), row tracks become columns (width = 30).
    assert_eq!(fragment.bounds.width(), 30.0);
    assert_eq!(fragment.bounds.height(), 40.0);
    assert_eq!(fragment.children[0].bounds.width(), 30.0);
    assert_eq!(fragment.children[0].bounds.height(), 20.0);
  }

  #[test]
  fn vertical_writing_mode_swaps_placements_to_physical_axes() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(30.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(200.0)),
    ];
    let style = Arc::new(style);

    let mut inline_item_style = ComputedStyle::default();
    inline_item_style.grid_column_start = 2;
    inline_item_style.grid_column_end = 3;
    inline_item_style.grid_row_start = 1;
    inline_item_style.grid_row_end = 2;
    let inline_item = BoxNode::new_block(
      Arc::new(inline_item_style),
      FormattingContextType::Block,
      vec![],
    );

    let mut block_item_style = ComputedStyle::default();
    block_item_style.grid_row_start = 2;
    block_item_style.grid_row_end = 3;
    block_item_style.grid_column_start = 1;
    block_item_style.grid_column_end = 2;
    let block_item = BoxNode::new_block(
      Arc::new(block_item_style),
      FormattingContextType::Block,
      vec![],
    );

    let grid = BoxNode::new_block(
      style,
      FormattingContextType::Grid,
      vec![inline_item, block_item],
    );

    let constraints = LayoutConstraints::definite(500.0, 500.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    // Tracks transpose: block tracks become Taffy columns (width), inline tracks become Taffy rows (height).
    assert_eq!(fragment.bounds.width(), 300.0);
    assert_eq!(fragment.bounds.height(), 70.0);

    // grid-column maps to the inline axis (vertical), so it should affect y/height.
    assert_eq!(fragment.children[0].bounds.x(), 200.0);
    assert_eq!(fragment.children[0].bounds.y(), 30.0);
    assert_eq!(fragment.children[0].bounds.width(), 100.0);
    assert_eq!(fragment.children[0].bounds.height(), 40.0);

    // grid-row maps to the block axis (horizontal), so it should affect x/width.
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.width(), 200.0);
    assert_eq!(fragment.children[1].bounds.height(), 30.0);
  }

  #[test]
  fn vertical_writing_mode_row_autoflow_fills_inline_axis_first() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_auto_flow = GridAutoFlow::Row;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(40.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    let style = Arc::new(style);

    let children: Vec<_> = (0..3)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 80.0);
    assert_eq!(fragment.bounds.height(), 40.0);

    // Row auto-flow maps to column auto-flow when inline is vertical: fill inline tracks top→bottom, then start a new block track.
    assert_eq!(fragment.children[0].bounds.x(), 40.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 40.0);
    assert_eq!(fragment.children[1].bounds.y(), 20.0);
    assert_eq!(fragment.children[2].bounds.x(), 0.0);
    assert_eq!(fragment.children[2].bounds.y(), 0.0);
  }

  #[test]
  fn vertical_writing_mode_column_autoflow_fills_block_axis_first() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.writing_mode = WritingMode::VerticalRl;
    style.grid_auto_flow = GridAutoFlow::Column;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(20.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    style.grid_template_rows = vec![
      GridTrack::Length(Length::px(40.0)),
      GridTrack::Length(Length::px(40.0)),
    ];
    let style = Arc::new(style);

    let children: Vec<_> = (0..3)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();
    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(200.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.bounds.width(), 80.0);
    assert_eq!(fragment.bounds.height(), 40.0);

    // Column auto-flow maps to row auto-flow when inline is vertical: fill block tracks first, then wrap inline.
    assert_eq!(fragment.children[0].bounds.x(), 40.0);
    assert_eq!(fragment.children[0].bounds.y(), 0.0);
    assert_eq!(fragment.children[1].bounds.x(), 0.0);
    assert_eq!(fragment.children[1].bounds.y(), 0.0);
    assert_eq!(fragment.children[2].bounds.x(), 40.0);
    assert_eq!(fragment.children[2].bounds.y(), 20.0);
  }

  #[test]
  fn test_grid_fixed_and_fr() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Fr(1.0),
      GridTrack::Fr(2.0),
    ];
    let style = Arc::new(style);

    let child1 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child3 = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);

    let grid = BoxNode::new_block(
      style,
      FormattingContextType::Grid,
      vec![child1, child2, child3],
    );

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 3);
  }

  #[test]
  fn grid_reuses_normalized_measured_fragments() {
    let child = BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]);
    let child_id = child.id;
    // Disable `justify-content: stretch` so the auto track stays at its content size. This keeps
    // the grid item's used size equal to the measured fragment size, allowing us to exercise the
    // measured-fragment reuse path.
    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Auto];
    grid_style.grid_template_rows = vec![GridTrack::Auto];
    grid_style.justify_content = JustifyContent::Start;
    let grid = BoxNode::new_block(
      Arc::new(grid_style),
      FormattingContextType::Grid,
      vec![child],
    );

    let constraints = LayoutConstraints::definite(100.0, 100.0);
    let fc = GridFormattingContext::new();
    let _guard = set_grid_test_measure_hook(move |node| {
      (node.id == child_id)
        .then(|| FragmentNode::new_block(Rect::from_xywh(5.0, 7.0, 30.0, 10.0), vec![]))
    });

    let fragment = fc.layout(&grid, &constraints).unwrap();
    assert_eq!(fragment.children.len(), 1);
    let child_fragment = &fragment.children[0];
    assert_eq!(child_fragment.bounds.x(), 0.0);
    assert_eq!(child_fragment.bounds.y(), 0.0);
    assert_eq!(child_fragment.bounds.width(), 30.0);
    assert_eq!(child_fragment.bounds.height(), 10.0);
  }

  #[test]
  fn parses_named_line_with_integer_in_any_order() {
    let placement = parse_grid_line_placement_raw("foo 2", None);
    match placement.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "foo");
        assert_eq!(idx, 2);
      }
      other => panic!("expected named line, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("2 foo", None);
    match placement_rev.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "foo");
        assert_eq!(idx, 2);
      }
      other => panic!("expected named line, got {:?}", other),
    }
  }

  #[test]
  fn parses_named_area_shorthand_expands_to_start_end_lines() {
    let line_names: &[Vec<String>] = &[
      vec!["content-start".to_string()],
      vec![],
      vec!["content-end".to_string()],
      vec![],
    ];
    let placement = parse_grid_line_placement_raw("content", Some(line_names));
    match placement.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "content-start");
        assert_eq!(idx, 1);
      }
      other => panic!("expected named line, got {:?}", other),
    }
    match placement.end {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, "content-end");
        assert_eq!(idx, 1);
      }
      other => panic!("expected named line, got {:?}", other),
    }
  }

  #[test]
  fn grid_column_named_area_shorthand_spans_start_end_lines() {
    let fc = GridFormattingContext::new();

    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(100.0)),
      GridTrack::Length(Length::px(200.0)),
      GridTrack::Length(Length::px(50.0)),
    ];
    container_style.grid_template_rows = vec![GridTrack::Auto];
    // `grid-template-columns: [content-start] 100px 200px [content-end] 50px`
    container_style.grid_column_line_names = vec![
      vec!["content-start".to_string()],
      vec![],
      vec!["content-end".to_string()],
      vec![],
    ];
    container_style.grid_row_line_names = vec![vec![], vec![]];
    let container_style = Arc::new(container_style);

    let mut item_style = ComputedStyle::default();
    item_style.grid_column_raw = Some("content".to_string());
    let item_style = Arc::new(item_style);
    let item_id = 4242usize;
    let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
    item.id = item_id;

    let grid = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);

    let constraints = LayoutConstraints::definite(350.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();
    let placed = find_block_fragment(&fragment, item_id);

    assert!(
      (placed.bounds.x() - 0.0).abs() < 0.5,
      "expected x=0, got {:.2}",
      placed.bounds.x()
    );
    assert!(
      (placed.bounds.width() - 300.0).abs() < 0.5,
      "expected to span first two tracks (100+200=300), got {:.2}",
      placed.bounds.width()
    );
  }

  #[test]
  fn parses_grid_line_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let placement = parse_grid_line_placement_raw(&format!("{nbsp}auto"), None);
    match placement.start {
      TaffyGridPlacement::NamedLine(name, idx) => {
        assert_eq!(name, format!("{nbsp}auto"));
        assert_eq!(idx, 1);
      }
      other => panic!("expected named line, got {:?}", other),
    }
  }

  #[test]
  fn parses_span_with_integer_in_any_order() {
    let placement = parse_grid_line_placement_raw("2 span", None);
    match placement.start {
      TaffyGridPlacement::Span(count) => assert_eq!(count, 2),
      other => panic!("expected span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("span 2", None);
    match placement_rev.start {
      TaffyGridPlacement::Span(count) => assert_eq!(count, 2),
      other => panic!("expected span, got {:?}", other),
    }
  }

  #[test]
  fn parses_named_span_in_any_order() {
    let placement = parse_grid_line_placement_raw("span foo 3", None);
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("span 3 foo", None);
    match placement_rev.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement = parse_grid_line_placement_raw("foo span", None);
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 1);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement = parse_grid_line_placement_raw("foo 3 span", None);
    match placement.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }

    let placement_rev = parse_grid_line_placement_raw("3 foo span", None);
    match placement_rev.start {
      TaffyGridPlacement::NamedSpan(name, count) => {
        assert_eq!(name, "foo");
        assert_eq!(count, 3);
      }
      other => panic!("expected named span, got {:?}", other),
    }
  }

  #[test]
  fn grid_area_custom_ident_can_target_named_lines() {
    let fc = GridFormattingContext::new();

    // Mimic yelp.com's hero grid:
    //   grid-template-columns: [slide-selection] 24px [slides] 1fr;
    //   grid-gap: 0 24px;
    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns =
      vec![GridTrack::Length(Length::px(24.0)), GridTrack::Fr(1.0)];
    container_style.grid_column_line_names = vec![
      vec!["slide-selection".to_string()],
      vec!["slides".to_string()],
      Vec::new(),
    ];
    container_style.grid_column_gap = Length::px(24.0);
    let container_style = Arc::new(container_style);

    let mut item1_style = ComputedStyle::default();
    item1_style.grid_row_raw = Some("slide-selection".to_string());
    item1_style.grid_column_raw = Some("slide-selection".to_string());
    let item1_style = Arc::new(item1_style);
    let mut item1 = BoxNode::new_block(item1_style, FormattingContextType::Block, vec![]);
    item1.id = 101;
    let item1_id = item1.id;

    let mut item2_style = ComputedStyle::default();
    item2_style.grid_row_raw = Some("slides".to_string());
    item2_style.grid_column_raw = Some("slides".to_string());
    let item2_style = Arc::new(item2_style);
    let mut item2 = BoxNode::new_block(item2_style, FormattingContextType::Block, vec![]);
    item2.id = 102;
    let item2_id = item2.id;

    let grid = BoxNode::new_block(
      container_style,
      FormattingContextType::Grid,
      vec![item1, item2],
    );

    let constraints = LayoutConstraints::definite(200.0, 50.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let item1_fragment = find_block_fragment(&fragment, item1_id);
    let item2_fragment = find_block_fragment(&fragment, item2_id);

    // First column is 24px wide and there is a 24px column gap.
    assert!(
      (item1_fragment.bounds.x() - 0.0).abs() < 0.05,
      "expected item1.x≈0, got {}",
      item1_fragment.bounds.x()
    );
    assert!(
      (item2_fragment.bounds.x() - 48.0).abs() < 0.05,
      "expected item2.x≈48, got {}",
      item2_fragment.bounds.x()
    );
  }

  #[test]
  fn grid_area_custom_ident_maps_to_area_start_end_lines() {
    let fc = GridFormattingContext::new();

    // Create explicit "hero-start"/"hero-end" line names and place an item with `grid-area: hero`.
    let mut container_style = ComputedStyle::default();
    container_style.display = CssDisplay::Grid;
    container_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(10.0)),
      GridTrack::Length(Length::px(20.0)),
    ];
    container_style.grid_column_line_names = vec![
      vec!["hero-start".to_string()],
      vec!["hero-end".to_string()],
      Vec::new(),
    ];
    container_style.grid_template_rows = vec![
      GridTrack::Length(Length::px(5.0)),
      GridTrack::Length(Length::px(15.0)),
    ];
    container_style.grid_row_line_names = vec![
      vec!["hero-start".to_string()],
      vec!["hero-end".to_string()],
      Vec::new(),
    ];
    let container_style = Arc::new(container_style);

    let mut item_style = ComputedStyle::default();
    item_style.grid_row_raw = Some("hero".to_string());
    item_style.grid_column_raw = Some("hero".to_string());
    let item_style = Arc::new(item_style);
    let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
    item.id = 201;
    let item_id = item.id;

    let grid = BoxNode::new_block(container_style, FormattingContextType::Grid, vec![item]);
    let constraints = LayoutConstraints::definite(30.0, 20.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let item_fragment = find_block_fragment(&fragment, item_id);
    assert!(
      (item_fragment.bounds.x() - 0.0).abs() < 0.05,
      "expected x≈0, got {}",
      item_fragment.bounds.x()
    );
    assert!(
      (item_fragment.bounds.y() - 0.0).abs() < 0.05,
      "expected y≈0, got {}",
      item_fragment.bounds.y()
    );
    assert!(
      (item_fragment.bounds.width() - 10.0).abs() < 0.05,
      "expected width≈10, got {}",
      item_fragment.bounds.width()
    );
    assert!(
      (item_fragment.bounds.height() - 5.0).abs() < 0.05,
      "expected height≈5, got {}",
      item_fragment.bounds.height()
    );
  }

  // Test 20: Grid with both row and column gaps
  #[test]
  fn test_grid_both_gaps() {
    let fc = GridFormattingContext::new();

    let mut style = ComputedStyle::default();
    style.display = CssDisplay::Grid;
    style.grid_template_columns = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    style.grid_template_rows = vec![GridTrack::Fr(1.0), GridTrack::Fr(1.0)];
    style.grid_column_gap = Length::px(10.0);
    style.grid_row_gap = Length::px(10.0);
    let style = Arc::new(style);

    let children: Vec<BoxNode> = (0..4)
      .map(|_| BoxNode::new_block(make_item_style(), FormattingContextType::Block, vec![]))
      .collect();

    let grid = BoxNode::new_block(style, FormattingContextType::Grid, children);

    let constraints = LayoutConstraints::definite(410.0, 210.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    assert_eq!(fragment.children.len(), 4);
  }

  #[test]
  fn grid_align_items_baseline_aligns_children() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.align_items = AlignItems::Baseline;
    grid_style.grid_template_columns = vec![
      GridTrack::Length(Length::px(140.0)),
      GridTrack::Length(Length::px(140.0)),
    ];
    let grid_style = Arc::new(grid_style);

    let item_small = make_text_item("small", 14.0);
    let item_large = make_text_item("large", 28.0);

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_small, item_large],
    );

    let constraints = LayoutConstraints::definite(400.0, 200.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let mut deadline_counter = 0usize;
    let baseline0_offset =
      super::first_baseline_offset(&fragment.children[0], &mut deadline_counter)
        .expect("baseline computation")
        .expect("baseline");
    let baseline0 = fragment.children[0].bounds.y() + baseline0_offset;
    let mut deadline_counter = 0usize;
    let baseline1_offset =
      super::first_baseline_offset(&fragment.children[1], &mut deadline_counter)
        .expect("baseline computation")
        .expect("baseline");
    let baseline1 = fragment.children[1].bounds.y() + baseline1_offset;

    assert!(
      (baseline0 - baseline1).abs() < 0.05,
      "baselines should align: {:.2} vs {:.2}",
      baseline0,
      baseline1
    );
    assert!(
      fragment.children[0].bounds.y() > fragment.children[1].bounds.y(),
      "smaller baseline item should be offset to align"
    );
  }

  #[test]
  fn grid_justify_items_baseline_aligns_columns() {
    let fc = GridFormattingContext::new();

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.justify_items = AlignItems::Baseline;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(180.0))];
    grid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
    let grid_style = Arc::new(grid_style);

    let item_writing_mode = WritingMode::VerticalLr;
    let fixed_width = 60.0;
    let item_small =
      make_text_item_with_writing_mode("small", 12.0, item_writing_mode, fixed_width);
    let item_large =
      make_text_item_with_writing_mode("large", 24.0, item_writing_mode, fixed_width);

    let grid = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![item_small, item_large],
    );

    let constraints = LayoutConstraints::definite(300.0, 300.0);
    let fragment = fc.layout(&grid, &constraints).unwrap();

    let mut deadline_counter = 0usize;
    let baseline_offset0 = super::first_baseline_offset_x(
      &fragment.children[0],
      item_writing_mode,
      &mut deadline_counter,
    )
    .expect("baseline computation")
    .expect("baseline");
    let mut deadline_counter = 0usize;
    let baseline_offset1 = super::first_baseline_offset_x(
      &fragment.children[1],
      item_writing_mode,
      &mut deadline_counter,
    )
    .expect("baseline computation")
    .expect("baseline");

    let baseline0 = fragment.children[0].bounds.x() + baseline_offset0;
    let baseline1 = fragment.children[1].bounds.x() + baseline_offset1;

    assert!(
      (baseline0 - baseline1).abs() < 0.05,
      "inline-axis baselines should align: {:.2} vs {:.2}",
      baseline0,
      baseline1
    );
    assert!(
      fragment.children[0].bounds.x() >= 0.0 || fragment.children[1].bounds.x() >= 0.0,
      "baseline alignment should not move items outside the column"
    );
    assert!(
      fragment.children[0].bounds.x() > fragment.children[1].bounds.x(),
      "smaller baseline item should be offset horizontally to align"
    );
  }

  #[test]
  fn grid_taffy_tree_cache_hits_on_repeat_layout() {
    let _epoch_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let epoch = crate::layout::formatting_context::intrinsic_cache_epoch().saturating_add(1);
    crate::layout::formatting_context::intrinsic_cache_use_epoch(epoch, true);
    crate::layout::taffy_integration::taffy_style_fingerprint_cache_use_epoch(epoch);

    crate::layout::taffy_integration::reset_taffy_tree_cache_counters();
    let _taffy_guard = crate::layout::taffy_integration::enable_taffy_counters(true);

    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(50.0)); 4];
    grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0)); 2];

    let children: Vec<BoxNode> = (0..8)
      .map(|idx| {
        let mut style = ComputedStyle::default();
        style.width = Some(Length::px(10.0));
        style.height = Some(Length::px(10.0));
        style.width_keyword = None;
        style.height_keyword = None;
        let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
        node.id = 30_000 + idx;
        node
      })
      .collect();

    let mut container =
      BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, children);
    container.id = 99_0001;
    let constraints = LayoutConstraints::definite(200.0, 200.0);

    fc.layout(&container, &constraints).expect("first layout");
    fc.layout(&container, &constraints).expect("second layout");

    let (hits, misses) = crate::layout::taffy_integration::taffy_tree_cache_counters();
    assert_eq!(misses, 1, "first layout should miss the per-box tree cache");
    assert_eq!(hits, 1, "second layout should reuse the cached Taffy tree");
  }

  #[test]
  fn grid_taffy_tree_cache_avoids_deadline_abort_by_reducing_measure_calls() {
    use crate::render_control::{DeadlineGuard, RenderDeadline};
    use std::sync::Arc;

    let _epoch_guard = crate::layout::formatting_context::intrinsic_cache_test_lock();
    let epoch = crate::layout::formatting_context::intrinsic_cache_epoch().saturating_add(1);
    crate::layout::formatting_context::intrinsic_cache_use_epoch(epoch, true);
    crate::layout::taffy_integration::taffy_style_fingerprint_cache_use_epoch(epoch);

    let _taffy_guard = crate::layout::taffy_integration::enable_taffy_counters(true);
    let _perf_guard = crate::layout::taffy_integration::TaffyPerfCountersGuard::new();

    let fc = GridFormattingContext::new().with_parallelism(LayoutParallelism::disabled());

    let mut grid_style = ComputedStyle::default();
    grid_style.display = CssDisplay::Grid;
    grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(20.0)); 8];

    let children: Vec<BoxNode> = (0..256)
      .map(|idx| {
        let mut style = ComputedStyle::default();
        style.width = Some(Length::px(10.0));
        style.height = Some(Length::px(10.0));
        style.width_keyword = None;
        style.height_keyword = None;
        let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
        node.id = 40_000 + idx;
        node
      })
      .collect();

    let mut container =
      BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, children);
    container.id = 99_0002;
    let constraints = LayoutConstraints::definite(400.0, 400.0);

    let before_first = crate::layout::taffy_integration::taffy_perf_counters().grid_measure_calls;
    fc.layout(&container, &constraints).expect("warm layout");
    let after_first = crate::layout::taffy_integration::taffy_perf_counters().grid_measure_calls;
    let first_calls = after_first.saturating_sub(before_first);
    assert!(first_calls > 0, "expected at least one Taffy measure call");

    let before_second = crate::layout::taffy_integration::taffy_perf_counters().grid_measure_calls;
    let threshold = (first_calls / 4).max(1);
    let limit = before_second + threshold;
    let deadline = RenderDeadline::new(
      None,
      Some(Arc::new(move || {
        crate::layout::taffy_integration::taffy_perf_counters().grid_measure_calls > limit
      })),
    );
    let _guard = DeadlineGuard::install(Some(&deadline));

    fc.layout(&container, &constraints)
      .expect("cached layout should complete under strict measure-call deadline");
    let after_second = crate::layout::taffy_integration::taffy_perf_counters().grid_measure_calls;
    let second_calls = after_second.saturating_sub(before_second);
    assert!(
      second_calls <= threshold,
      "expected cached layout to use <= {threshold} measure calls, got {second_calls}"
    );
  }

  #[test]
  fn taffy_perf_counters_record_grid_measure_and_compute_time() {
    let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
    let mut renderer = FastRender::with_config(config).expect("renderer");
    let html = r#"<!doctype html>
      <html>
        <body>
          <div style="display:grid">
            <div>hello</div>
          </div>
        </body>
      </html>"#;
    let options = RenderOptions::default()
      .with_viewport(200, 200)
      .with_diagnostics_level(DiagnosticsLevel::Basic);
    let result = renderer
      .render_html_with_diagnostics(html, options)
      .expect("render");
    let stats = result
      .diagnostics
      .stats
      .as_ref()
      .expect("diagnostics stats should be captured");
    let measure_calls = stats
      .layout
      .taffy_grid_measure_calls
      .expect("grid measure call count should be recorded");
    assert!(measure_calls > 0, "expected grid measure calls > 0");
    let compute_ms = stats
      .layout
      .taffy_grid_compute_cpu_ms
      .expect("grid compute_cpu_ms should be recorded");
    assert!(compute_ms >= 0.0, "expected grid compute_cpu_ms >= 0");
  }
}
