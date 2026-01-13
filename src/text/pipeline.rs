//! Text Shaping Pipeline
//!
//! Coordinates bidi analysis, script itemization, and text shaping into a unified process.
//!
//! # Architecture
//!
//! The shaping pipeline processes text through multiple stages:
//!
//! ```text
//! Input Text → Bidi Analysis → Script Itemization → Font Matching → Shaping → Output
//! ```
//!
//! Each stage handles a specific aspect of text processing:
//!
//! 1. **Bidi Analysis**: Determines text direction (LTR/RTL) using UAX #9
//! 2. **Script Itemization**: Splits text into runs of the same script
//! 3. **Font Matching**: Assigns fonts to each run with fallback support
//! 4. **Shaping**: Converts characters to positioned glyphs using HarfBuzz
//!
//! # Example
//!
//! ```rust,no_run
//! # use fastrender::text::pipeline::{Direction, ShapingPipeline};
//! # use fastrender::FontContext;
//! # use fastrender::ComputedStyle;
//! # fn main() -> fastrender::Result<()> {
//! let mut pipeline = ShapingPipeline::new();
//! let font_context = FontContext::new();
//! let style = ComputedStyle::default();
//!
//! let shaped_runs = pipeline.shape("Hello, world!", &style, &font_context)?;
//! for run in shaped_runs {
//!     println!("Run: {} glyphs, {}px advance", run.glyphs.len(), run.advance);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Unicode Compliance
//!
//! This implementation aims to support:
//! - **UAX #9**: Unicode Bidirectional Algorithm
//! - **UAX #24**: Script Property
//! - **OpenType GSUB/GPOS**: Glyph substitution and positioning
//!
//! # References
//!
//! - Unicode Bidirectional Algorithm (UAX #9): <https://www.unicode.org/reports/tr9/>
//! - HarfBuzz documentation: <https://harfbuzz.github.io/>
//! - rustybuzz documentation: <https://docs.rs/rustybuzz/>

use crate::css::types::FontFeatureValueType;
use crate::css::types::FontPaletteBase;
use crate::error::Result;
use crate::error::TextError;
use crate::style::color::Rgba;
use crate::style::font_palette::resolve_font_palette_for_font;
use crate::style::types::Direction as CssDirection;
use crate::style::types::EastAsianVariant;
use crate::style::types::EastAsianWidth;
use crate::style::types::FontKerning;
use crate::style::types::FontLanguageOverride;
use crate::style::types::FontPalette;
use crate::style::types::FontSizeAdjust;
use crate::style::types::FontSizeAdjustMetric;
use crate::style::types::FontStyle as CssFontStyle;
use crate::style::types::FontVariant;
use crate::style::types::FontVariantAlternateValue;
use crate::style::types::FontVariantCaps;
use crate::style::types::FontVariantEmoji;
use crate::style::types::FontVariantPosition;
use crate::style::types::NumericFigure;
use crate::style::types::NumericFraction;
use crate::style::types::NumericSpacing;
use crate::style::types::TextRendering;
use crate::style::ComputedStyle;
use crate::text::bidi_controls::is_bidi_format_char;
use crate::text::color_fonts::select_cpal_palette;
use crate::text::emoji;
use crate::text::emoji_presentation::font_is_emoji_font;
use crate::text::font_db::compute_font_size_adjusted_size;
use crate::text::font_db::FontDatabase;
use crate::text::font_db::FontStretch as DbFontStretch;
use crate::text::font_db::FontStyle;
use crate::text::font_db::LoadedFont;
use crate::text::font_fallback::families_signature;
use crate::text::font_fallback::ClusterFallbackCacheKey;
use crate::text::font_fallback::EmojiPreference;
use crate::text::font_fallback::FallbackCache;
use crate::text::font_fallback::FallbackCacheDescriptor;
use crate::text::font_fallback::FallbackCacheStatsSnapshot;
use crate::text::font_fallback::GlyphFallbackCacheKey;
use crate::text::font_loader::FontContext;
use crate::text::script_fallback;
use lru::LruCache;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use rustc_hash::FxHasher;
use rustybuzz::Direction as HbDirection;
use rustybuzz::Feature;
use rustybuzz::Language as HbLanguage;
use rustybuzz::UnicodeBuffer;
use rustybuzz::Variation;
use std::borrow::Cow;
#[cfg(any(test, debug_assertions))]
use std::cell::Cell;
use std::cell::RefCell;
use std::hash::BuildHasherDefault;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;
use ttf_parser::Tag;
use unicode_bidi::BidiInfo;
use unicode_bidi::BidiClass;
use unicode_bidi::Level;
use unicode_bidi_mirroring::get_mirrored;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_segmentation::UnicodeSegmentation;
use unicode_vo::char_orientation;
use unicode_vo::Orientation as VerticalOrientation;

pub(crate) const DEFAULT_OBLIQUE_ANGLE_DEG: f32 = 14.0;
const SHAPING_CACHE_CAPACITY: usize = 65536;
const SHAPING_CACHE_HASH_COLLISION_BUCKET_LIMIT: usize = 8;
const FONT_RESOLUTION_CACHE_SIZE: usize = 131072;
#[cfg(any(test, debug_assertions))]
thread_local! {
  // These counters are used by unit/integration tests as guardrails against excessive churn.
  //
  // They are thread-local so tests can run in parallel without racing on global state. Layout
  // tests frequently run in parallel (default `cargo test` behavior), and using a process-wide
  // counter makes "reset + assert" patterns flaky.
  static SHAPE_FONT_RUN_INVOCATIONS: Cell<usize> = Cell::new(0);
  static SHAPING_PIPELINE_NEW_CALLS: Cell<usize> = Cell::new(0);
  static ITEMIZE_TEXT_RUNS_PRODUCED: Cell<usize> = Cell::new(0);
  static ITEMIZE_TEXT_BYTES_COPIED: Cell<usize> = Cell::new(0);
}

#[cfg(test)]
thread_local! {
  static SHAPING_STYLE_HASH_CALLS: Cell<usize> = Cell::new(0);
  /// Counts how many times we allocate/clone the *full input text* into an owning `Arc<str>` while
  /// shaping.
  ///
  /// This is used by tests as a guardrail to ensure a single shaping-cache miss does not allocate
  /// the full text multiple times (e.g. once for bidi analysis and again for the shaping cache).
  static SHAPE_FULL_TEXT_ARC_ALLOCS: Cell<usize> = Cell::new(0);
}

#[cfg(test)]
#[inline]
fn record_full_text_arc_alloc() {
  SHAPE_FULL_TEXT_ARC_ALLOCS.with(|calls| calls.set(calls.get().saturating_add(1)));
}

#[cfg(not(test))]
#[inline]
fn record_full_text_arc_alloc() {}

#[inline]
fn empty_arc_str() -> Arc<str> {
  static EMPTY: OnceLock<Arc<str>> = OnceLock::new();
  Arc::clone(EMPTY.get_or_init(|| Arc::from("")))
}

type ShapingCacheHasher = BuildHasherDefault<FxHasher>;

#[derive(Debug, Default, Clone, Copy)]
pub struct TextCacheStats {
  pub hits: u64,
  pub misses: u64,
  pub evictions: u64,
  pub bytes: usize,
}

#[derive(Debug, Default, Clone)]
pub struct TextDiagnostics {
  pub shape_ms: f64,
  pub coverage_ms: f64,
  pub rasterize_ms: f64,
  pub shaped_runs: usize,
  /// Count of shaped runs that used an `@font-face` `size-adjust` descriptor.
  pub font_face_size_adjust_runs: usize,
  /// Count of shaped runs that used `@font-face` metric overrides (`ascent-override`,
  /// `descent-override`, or `line-gap-override`).
  pub font_face_metric_override_runs: usize,
  pub glyphs: usize,
  pub color_glyph_rasters: usize,
  pub fallback_cache_hits: usize,
  pub fallback_cache_misses: usize,
  pub fallback_cache_glyph_evictions: usize,
  pub fallback_cache_cluster_evictions: usize,
  pub fallback_cache_clears: usize,
  pub fallback_cache_glyph_entries: Option<usize>,
  pub fallback_cache_cluster_entries: Option<usize>,
  pub fallback_cache_glyph_capacity: Option<usize>,
  pub fallback_cache_cluster_capacity: Option<usize>,
  pub fallback_cache_shards: Option<usize>,
  pub last_resort_fallbacks: usize,
  pub last_resort_samples: Vec<String>,
  pub fallback_descriptor_unique_descriptors: Option<usize>,
  pub fallback_descriptor_unique_family_signatures: Option<usize>,
  pub fallback_descriptor_unique_languages: Option<usize>,
  pub fallback_descriptor_unique_weights: Option<usize>,
  pub fallback_descriptor_samples: Vec<String>,
  pub shaping_cache_hits: u64,
  pub shaping_cache_misses: u64,
  pub shaping_cache_evictions: u64,
  pub shaping_cache_entries: usize,
  pub glyph_cache: TextCacheStats,
  pub color_glyph_cache: TextCacheStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDiagnosticsStage {
  Coverage,
  Shape,
  Rasterize,
}

#[must_use]
#[derive(Debug)]
pub(crate) struct TextDiagnosticsTimer {
  stage: TextDiagnosticsStage,
  session: u64,
}

impl Drop for TextDiagnosticsTimer {
  fn drop(&mut self) {
    let now = Instant::now();
    if let Ok(mut state) = diagnostics_cell().lock() {
      if state.session != self.session {
        return;
      }
      state.end_stage(self.stage, now);
    }
  }
}

#[derive(Debug, Default)]
struct TextDiagnosticsState {
  diag: TextDiagnostics,
  session: u64,
  coverage_active: usize,
  coverage_start: Option<Instant>,
  shape_active: usize,
  shape_start: Option<Instant>,
  rasterize_active: usize,
  rasterize_start: Option<Instant>,
  fallback_descriptor_stats: Option<FallbackDescriptorStatsState>,
}

#[derive(Debug, Default)]
struct FallbackDescriptorStatsState {
  descriptors: FxHashSet<FallbackCacheDescriptor>,
  families: FxHashSet<u64>,
  languages: FxHashSet<u64>,
  weights: FxHashSet<u16>,
}

impl TextDiagnosticsState {
  fn new(session: u64) -> Self {
    Self {
      session,
      ..Self::default()
    }
  }

  fn start_stage(&mut self, stage: TextDiagnosticsStage, now: Instant) {
    match stage {
      TextDiagnosticsStage::Coverage => {
        let prev = self.coverage_active;
        self.coverage_active = self.coverage_active.saturating_add(1);
        debug_assert!(
          self.coverage_active > prev,
          "text diagnostics coverage_active overflow"
        );
        if self.coverage_active == 1 {
          debug_assert!(
            self.coverage_start.is_none(),
            "text diagnostics coverage_start should be unset when inactive"
          );
          self.coverage_start = Some(now);
        } else {
          debug_assert!(
            self.coverage_start.is_some(),
            "text diagnostics coverage_start missing while stage is active"
          );
        }
      }
      TextDiagnosticsStage::Shape => {
        let prev = self.shape_active;
        self.shape_active = self.shape_active.saturating_add(1);
        debug_assert!(
          self.shape_active > prev,
          "text diagnostics shape_active overflow"
        );
        if self.shape_active == 1 {
          debug_assert!(
            self.shape_start.is_none(),
            "text diagnostics shape_start should be unset when inactive"
          );
          self.shape_start = Some(now);
        } else {
          debug_assert!(
            self.shape_start.is_some(),
            "text diagnostics shape_start missing while stage is active"
          );
        }
      }
      TextDiagnosticsStage::Rasterize => {
        let prev = self.rasterize_active;
        self.rasterize_active = self.rasterize_active.saturating_add(1);
        debug_assert!(
          self.rasterize_active > prev,
          "text diagnostics rasterize_active overflow"
        );
        if self.rasterize_active == 1 {
          debug_assert!(
            self.rasterize_start.is_none(),
            "text diagnostics rasterize_start should be unset when inactive"
          );
          self.rasterize_start = Some(now);
        } else {
          debug_assert!(
            self.rasterize_start.is_some(),
            "text diagnostics rasterize_start missing while stage is active"
          );
        }
      }
    }
  }

  fn end_stage(&mut self, stage: TextDiagnosticsStage, now: Instant) {
    match stage {
      TextDiagnosticsStage::Coverage => {
        if self.coverage_active == 0 {
          debug_assert!(
            false,
            "text diagnostics coverage_active underflow (timer dropped without start)"
          );
          return;
        }
        self.coverage_active -= 1;
        if self.coverage_active == 0 {
          if let Some(start) = self.coverage_start.take() {
            self.diag.coverage_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
          } else {
            debug_assert!(
              false,
              "text diagnostics coverage_start missing when closing stage"
            );
          }
        }
      }
      TextDiagnosticsStage::Shape => {
        if self.shape_active == 0 {
          debug_assert!(
            false,
            "text diagnostics shape_active underflow (timer dropped without start)"
          );
          return;
        }
        self.shape_active -= 1;
        if self.shape_active == 0 {
          if let Some(start) = self.shape_start.take() {
            self.diag.shape_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
          } else {
            debug_assert!(
              false,
              "text diagnostics shape_start missing when closing stage"
            );
          }
        }
      }
      TextDiagnosticsStage::Rasterize => {
        if self.rasterize_active == 0 {
          debug_assert!(
            false,
            "text diagnostics rasterize_active underflow (timer dropped without start)"
          );
          return;
        }
        self.rasterize_active -= 1;
        if self.rasterize_active == 0 {
          if let Some(start) = self.rasterize_start.take() {
            self.diag.rasterize_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
          } else {
            debug_assert!(
              false,
              "text diagnostics rasterize_start missing when closing stage"
            );
          }
        }
      }
    }
  }

  fn finalize_open_stages(&mut self, now: Instant) {
    if self.coverage_active > 0 {
      if let Some(start) = self.coverage_start.take() {
        self.diag.coverage_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
      }
    }
    if self.shape_active > 0 {
      if let Some(start) = self.shape_start.take() {
        self.diag.shape_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
      }
    }
    if self.rasterize_active > 0 {
      if let Some(start) = self.rasterize_start.take() {
        self.diag.rasterize_ms += now.saturating_duration_since(start).as_secs_f64() * 1000.0;
      }
    }
    // Clear any inconsistent starts even if the active counts were already zero (defensive against
    // corrupted state in debug builds).
    self.coverage_start = None;
    self.shape_start = None;
    self.rasterize_start = None;
    self.coverage_active = 0;
    self.shape_active = 0;
    self.rasterize_active = 0;
  }
}

static TEXT_DIAGNOSTICS: OnceLock<Mutex<TextDiagnosticsState>> = OnceLock::new();

static TEXT_DIAGNOSTICS_ENABLED: AtomicBool = AtomicBool::new(false);
static TEXT_DIAGNOSTICS_SESSION: AtomicU64 = AtomicU64::new(0);

thread_local! {
  /// Thread-local marker indicating which text diagnostics session (if any) this thread is allowed
  /// to record into.
  ///
  /// Text diagnostics are stored in process-global state. When a render enables diagnostics we
  /// want to collect counters from that render without being polluted by unrelated shaping work
  /// happening concurrently on other threads (e.g. in parallel tests or other renders that are not
  /// collecting diagnostics). The FastRender API serializes diagnostics-enabled renders via
  /// [`crate::api::DiagnosticsSessionGuard`], but other shaping calls can still run concurrently.
  ///
  /// By additionally gating recording on a thread-local session id we ensure that only the thread
  /// that enabled diagnostics (and any threads it explicitly opts into the session) contributes to
  /// the counters.
  static TEXT_DIAGNOSTICS_THREAD_SESSION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
static LAST_RESORT_LOGGED: AtomicUsize = AtomicUsize::new(0);
static SHAPING_FALLBACK_LOGGED: AtomicUsize = AtomicUsize::new(0);
static SHAPING_CACHE_DIAG_HITS: AtomicU64 = AtomicU64::new(0);
static SHAPING_CACHE_DIAG_MISSES: AtomicU64 = AtomicU64::new(0);
static SHAPING_CACHE_DIAG_EVICTIONS: AtomicU64 = AtomicU64::new(0);
static SHAPING_CACHE_DIAG_ENTRIES: AtomicUsize = AtomicUsize::new(0);

const LAST_RESORT_SAMPLE_LIMIT: usize = 8;
const FALLBACK_DESCRIPTOR_SAMPLE_LIMIT: usize = 16;

fn diagnostics_cell() -> &'static Mutex<TextDiagnosticsState> {
  TEXT_DIAGNOSTICS.get_or_init(|| Mutex::new(TextDiagnosticsState::default()))
}

pub(crate) fn enable_text_diagnostics() {
  let session = TEXT_DIAGNOSTICS_SESSION
    .fetch_add(1, Ordering::AcqRel)
    .wrapping_add(1);
  if let Ok(mut state) = diagnostics_cell().lock() {
    *state = TextDiagnosticsState::new(session);
    TEXT_DIAGNOSTICS_ENABLED.store(true, Ordering::Release);
    TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| current.set(session));
  } else {
    TEXT_DIAGNOSTICS_ENABLED.store(false, Ordering::Release);
    TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| current.set(0));
  }
  SHAPING_CACHE_DIAG_HITS.store(0, Ordering::Relaxed);
  SHAPING_CACHE_DIAG_MISSES.store(0, Ordering::Relaxed);
  SHAPING_CACHE_DIAG_EVICTIONS.store(0, Ordering::Relaxed);
  SHAPING_CACHE_DIAG_ENTRIES.store(0, Ordering::Relaxed);
}

pub(crate) fn take_text_diagnostics() -> Option<TextDiagnostics> {
  let was_enabled = TEXT_DIAGNOSTICS_ENABLED.swap(false, Ordering::AcqRel);
  if !was_enabled {
    return None;
  }

  let now = Instant::now();
  let result = diagnostics_cell().lock().ok().map(|mut state| {
    state.finalize_open_stages(now);
    if text_fallback_descriptor_stats_enabled() {
      let stats = state.fallback_descriptor_stats.take().unwrap_or_default();
      state.diag.fallback_descriptor_unique_descriptors = Some(stats.descriptors.len());
      state.diag.fallback_descriptor_unique_family_signatures = Some(stats.families.len());
      state.diag.fallback_descriptor_unique_languages = Some(stats.languages.len());
      state.diag.fallback_descriptor_unique_weights = Some(stats.weights.len());
    }
    let mut diag = state.diag.clone();
    diag.shaping_cache_hits = SHAPING_CACHE_DIAG_HITS.load(Ordering::Relaxed);
    diag.shaping_cache_misses = SHAPING_CACHE_DIAG_MISSES.load(Ordering::Relaxed);
    diag.shaping_cache_evictions = SHAPING_CACHE_DIAG_EVICTIONS.load(Ordering::Relaxed);
    diag.shaping_cache_entries = SHAPING_CACHE_DIAG_ENTRIES.load(Ordering::Relaxed);
    // Prevent any late-dropped timers from mutating the taken snapshot (or a later session).
    state.session = 0;
    diag
  });
  TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| current.set(0));
  result
}

pub(crate) fn text_diagnostics_enabled() -> bool {
  // Use Acquire so threads reliably observe `enable_text_diagnostics()`'s Release store when work is
  // parallelized across the renderer thread pool.
  if !TEXT_DIAGNOSTICS_ENABLED.load(Ordering::Acquire) {
    return false;
  }

  let active_session = TEXT_DIAGNOSTICS_SESSION.load(Ordering::Acquire);
  TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| current.get() == active_session)
}

/// Returns the active text diagnostics session id (if any).
///
/// This is used by paint code to opt parallel worker threads into the currently active session via
/// [`TextDiagnosticsThreadGuard`].
pub(crate) fn text_diagnostics_session_id() -> Option<u64> {
  if !TEXT_DIAGNOSTICS_ENABLED.load(Ordering::Acquire) {
    return None;
  }
  let active_session = TEXT_DIAGNOSTICS_SESSION.load(Ordering::Acquire);
  (active_session != 0).then_some(active_session)
}

/// Temporarily opts the current thread into a text diagnostics session.
///
/// Text work can execute on rayon worker threads (e.g., parallel paint tiling). The render thread
/// must explicitly opt worker threads into diagnostics so thread-local `text_diagnostics_enabled()`
/// checks remain cheap and so unrelated shaping work on other threads isn't counted.
#[must_use]
pub(crate) struct TextDiagnosticsThreadGuard {
  prev_session: u64,
}

impl TextDiagnosticsThreadGuard {
  pub(crate) fn enter(session: u64) -> Self {
    let prev_session = TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| {
      let prev = current.get();
      current.set(session);
      prev
    });
    Self { prev_session }
  }
}

impl Drop for TextDiagnosticsThreadGuard {
  fn drop(&mut self) {
    TEXT_DIAGNOSTICS_THREAD_SESSION.with(|current| current.set(self.prev_session));
  }
}

pub(crate) fn text_diagnostics_timer(stage: TextDiagnosticsStage) -> Option<TextDiagnosticsTimer> {
  if !text_diagnostics_enabled() {
    return None;
  }
  let now = Instant::now();
  let mut state = diagnostics_cell().lock().ok()?;
  let session = state.session;
  if session == 0 {
    return None;
  }
  state.start_stage(stage, now);
  Some(TextDiagnosticsTimer { stage, session })
}

fn with_text_diagnostics_state(f: impl FnOnce(&mut TextDiagnosticsState)) {
  if !text_diagnostics_enabled() {
    return;
  }

  if let Ok(mut state) = diagnostics_cell().lock() {
    f(&mut state);
  }
}

fn with_text_diagnostics(f: impl FnOnce(&mut TextDiagnostics)) {
  with_text_diagnostics_state(|state| f(&mut state.diag));
}

fn text_diagnostics_verbose_logging() -> bool {
  static VERBOSE: OnceLock<bool> = OnceLock::new();
  *VERBOSE.get_or_init(|| {
    std::env::var("FASTR_TEXT_DIAGNOSTICS")
      .map(|value| {
        let value = value.to_ascii_lowercase();
        matches!(
          value.as_str(),
          "verbose" | "2" | "debug" | "true" | "1" | "on"
        )
      })
      .unwrap_or(false)
  })
}

fn text_fallback_descriptor_stats_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| {
    std::env::var("FASTR_TEXT_FALLBACK_DESCRIPTOR_STATS")
      .map(|value| {
        let value = value.trim().to_ascii_lowercase();
        matches!(
          value.as_str(),
          "1" | "true" | "on" | "yes" | "debug" | "verbose"
        )
      })
      .unwrap_or(false)
  })
}

fn format_codepoints_for_log(text: &str) -> String {
  use std::fmt::Write;

  let mut out = String::new();
  let mut added = 0usize;
  for ch in text.chars() {
    if added >= LAST_RESORT_SAMPLE_LIMIT {
      out.push_str(" …");
      break;
    }
    if added > 0 {
      out.push(' ');
    }
    let _ = write!(&mut out, "U+{:04X}", ch as u32);
    added += 1;
  }
  out
}

fn script_from_u8(value: u8) -> Option<Script> {
  Some(match value {
    v if v == Script::Common as u8 => Script::Common,
    v if v == Script::Inherited as u8 => Script::Inherited,
    v if v == Script::Unknown as u8 => Script::Unknown,
    v if v == Script::Latin as u8 => Script::Latin,
    v if v == Script::Arabic as u8 => Script::Arabic,
    v if v == Script::Syriac as u8 => Script::Syriac,
    v if v == Script::Thaana as u8 => Script::Thaana,
    v if v == Script::Nko as u8 => Script::Nko,
    v if v == Script::Hebrew as u8 => Script::Hebrew,
    v if v == Script::Greek as u8 => Script::Greek,
    v if v == Script::Cyrillic as u8 => Script::Cyrillic,
    v if v == Script::Devanagari as u8 => Script::Devanagari,
    v if v == Script::Bengali as u8 => Script::Bengali,
    v if v == Script::Tamil as u8 => Script::Tamil,
    v if v == Script::Myanmar as u8 => Script::Myanmar,
    v if v == Script::Telugu as u8 => Script::Telugu,
    v if v == Script::Thai as u8 => Script::Thai,
    v if v == Script::Javanese as u8 => Script::Javanese,
    v if v == Script::Han as u8 => Script::Han,
    v if v == Script::Hiragana as u8 => Script::Hiragana,
    v if v == Script::Katakana as u8 => Script::Katakana,
    v if v == Script::Hangul as u8 => Script::Hangul,
    v if v == Script::Gurmukhi as u8 => Script::Gurmukhi,
    v if v == Script::Gujarati as u8 => Script::Gujarati,
    v if v == Script::Oriya as u8 => Script::Oriya,
    v if v == Script::Kannada as u8 => Script::Kannada,
    v if v == Script::Malayalam as u8 => Script::Malayalam,
    v if v == Script::Sinhala as u8 => Script::Sinhala,
    v if v == Script::Armenian as u8 => Script::Armenian,
    v if v == Script::Georgian as u8 => Script::Georgian,
    v if v == Script::Ethiopic as u8 => Script::Ethiopic,
    v if v == Script::Lao as u8 => Script::Lao,
    v if v == Script::Tibetan as u8 => Script::Tibetan,
    v if v == Script::Khmer as u8 => Script::Khmer,
    v if v == Script::Cherokee as u8 => Script::Cherokee,
    v if v == Script::CanadianAboriginal as u8 => Script::CanadianAboriginal,
    v if v == Script::TaiLe as u8 => Script::TaiLe,
    v if v == Script::OlChiki as u8 => Script::OlChiki,
    v if v == Script::Glagolitic as u8 => Script::Glagolitic,
    v if v == Script::Tifinagh as u8 => Script::Tifinagh,
    v if v == Script::SylotiNagri as u8 => Script::SylotiNagri,
    v if v == Script::MeeteiMayek as u8 => Script::MeeteiMayek,
    v if v == Script::Gothic as u8 => Script::Gothic,
    _ => return None,
  })
}

fn generic_family_for_sample(generic: crate::text::font_db::GenericFamily) -> &'static str {
  match generic {
    crate::text::font_db::GenericFamily::Serif => "serif",
    crate::text::font_db::GenericFamily::SansSerif => "sans-serif",
    crate::text::font_db::GenericFamily::Monospace => "monospace",
    crate::text::font_db::GenericFamily::Cursive => "cursive",
    crate::text::font_db::GenericFamily::Fantasy => "fantasy",
    crate::text::font_db::GenericFamily::SystemUi => "system-ui",
    crate::text::font_db::GenericFamily::UiSerif => "ui-serif",
    crate::text::font_db::GenericFamily::UiSansSerif => "ui-sans-serif",
    crate::text::font_db::GenericFamily::UiMonospace => "ui-monospace",
    crate::text::font_db::GenericFamily::UiRounded => "ui-rounded",
    crate::text::font_db::GenericFamily::Emoji => "emoji",
    crate::text::font_db::GenericFamily::Math => "math",
    crate::text::font_db::GenericFamily::Fangsong => "fangsong",
  }
}

fn format_family_entries_for_sample(
  families: &[crate::text::font_fallback::FamilyEntry],
) -> String {
  use crate::text::font_fallback::FamilyEntry;

  const MAX_ENTRIES: usize = 8;
  const MAX_LEN_BYTES: usize = 160;

  let mut out = String::new();
  for (idx, family) in families.iter().enumerate() {
    if idx >= MAX_ENTRIES {
      out.push_str(", …");
      break;
    }
    if idx > 0 {
      out.push_str(", ");
    }
    match family {
      FamilyEntry::Named(name) => {
        if name
          .chars()
          .any(|c| c.is_whitespace() || matches!(c, ',' | '"' | '\''))
        {
          out.push_str(&format!("{name:?}"));
        } else {
          out.push_str(name);
        }
      }
      FamilyEntry::Generic(generic) => out.push_str(generic_family_for_sample(*generic)),
    }

    if out.len() > MAX_LEN_BYTES {
      // Truncate at a char boundary.
      let mut cutoff = MAX_LEN_BYTES;
      while cutoff > 0 && !out.is_char_boundary(cutoff) {
        cutoff -= 1;
      }
      out.truncate(cutoff);
      out.push('…');
      break;
    }
  }
  out
}

fn format_stretch_pct(pct: f32) -> String {
  if pct.fract().abs() < f32::EPSILON {
    format!("{pct:.0}")
  } else {
    format!("{pct:.1}")
  }
}

fn format_oblique_degrees(degrees: i16) -> String {
  if degrees == 0 {
    "0".to_string()
  } else {
    format!("{:.1}", degrees as f32 / 10.0)
  }
}

fn format_descriptor_sample(
  descriptor: FallbackCacheDescriptor,
  language: &str,
  families_display: &str,
) -> String {
  let script = script_from_u8(descriptor.script)
    .map(|s| format!("{s:?}"))
    .unwrap_or_else(|| format!("{}", descriptor.script));
  let stretch_pct = format_stretch_pct(descriptor.stretch.to_percentage());
  let oblique_deg = format_oblique_degrees(descriptor.oblique_degrees);
  format!(
    "lang={language:?} lang_sig=0x{lang_sig:016x} \
script={script} weight={weight} style={style:?} stretch={stretch_pct}% oblique={oblique_deg}deg \
emoji_pref={emoji:?} require_base={require_base} families_sig=0x{families_sig:016x} \
families=[{families_display}]",
    lang_sig = descriptor.language,
    weight = descriptor.weight,
    style = descriptor.style,
    emoji = descriptor.emoji_pref,
    require_base = descriptor.require_base,
    families_sig = descriptor.families,
  )
}

fn record_last_resort_fallback(cluster_text: &str) {
  with_text_diagnostics(|diag| {
    diag.last_resort_fallbacks = diag.last_resort_fallbacks.saturating_add(1);
    if diag.last_resort_samples.len() < LAST_RESORT_SAMPLE_LIMIT {
      diag
        .last_resort_samples
        .push(format_codepoints_for_log(cluster_text));
    }
  });

  if text_diagnostics_verbose_logging() {
    let idx = LAST_RESORT_LOGGED.fetch_add(1, Ordering::Relaxed);
    if idx < LAST_RESORT_SAMPLE_LIMIT {
      eprintln!(
        "FASTR_TEXT_DIAGNOSTICS: last-resort font fallback for cluster {}",
        format_codepoints_for_log(cluster_text)
      );
    }
  }
}

pub(crate) fn record_text_shape(
  timer: Option<TextDiagnosticsTimer>,
  shaped_runs: usize,
  glyphs: usize,
) {
  with_text_diagnostics(|diag| {
    diag.shaped_runs = diag.shaped_runs.saturating_add(shaped_runs);
    diag.glyphs = diag.glyphs.saturating_add(glyphs);
  });
  drop(timer);
}

fn record_font_face_override_usage(runs: &[ShapedRun]) {
  let mut size_adjust_runs = 0usize;
  let mut metric_override_runs = 0usize;

  for run in runs {
    let overrides = run.font.face_metrics_overrides;
    if overrides.size_adjust.is_finite() && (overrides.size_adjust - 1.0).abs() > f32::EPSILON {
      size_adjust_runs += 1;
    }
    if overrides.has_metric_overrides() {
      metric_override_runs += 1;
    }
  }

  if size_adjust_runs == 0 && metric_override_runs == 0 {
    return;
  }

  with_text_diagnostics(|diag| {
    diag.font_face_size_adjust_runs = diag
      .font_face_size_adjust_runs
      .saturating_add(size_adjust_runs);
    diag.font_face_metric_override_runs = diag
      .font_face_metric_override_runs
      .saturating_add(metric_override_runs);
  });
}

pub(crate) fn record_text_coverage(timer: Option<TextDiagnosticsTimer>) {
  drop(timer);
}

pub(crate) fn record_fallback_cache_stats_delta(
  before: FallbackCacheStatsSnapshot,
  after: FallbackCacheStatsSnapshot,
) {
  with_text_diagnostics(|diag| {
    let hit_delta = after
      .cluster_hits
      .saturating_sub(before.cluster_hits)
      .saturating_add(after.glyph_hits.saturating_sub(before.glyph_hits));
    let miss_delta = after
      .cluster_misses
      .saturating_sub(before.cluster_misses)
      .saturating_add(after.glyph_misses.saturating_sub(before.glyph_misses));
    let glyph_eviction_delta = after.glyph_evictions.saturating_sub(before.glyph_evictions);
    let cluster_eviction_delta = after
      .cluster_evictions
      .saturating_sub(before.cluster_evictions);
    let clear_delta = after.clears.saturating_sub(before.clears);
    diag.fallback_cache_hits = diag.fallback_cache_hits.saturating_add(hit_delta as usize);
    diag.fallback_cache_misses = diag
      .fallback_cache_misses
      .saturating_add(miss_delta as usize);
    diag.fallback_cache_glyph_evictions = diag
      .fallback_cache_glyph_evictions
      .saturating_add(glyph_eviction_delta as usize);
    diag.fallback_cache_cluster_evictions = diag
      .fallback_cache_cluster_evictions
      .saturating_add(cluster_eviction_delta as usize);
    diag.fallback_cache_clears = diag
      .fallback_cache_clears
      .saturating_add(clear_delta as usize);
    diag.fallback_cache_glyph_entries = Some(
      diag
        .fallback_cache_glyph_entries
        .unwrap_or(0)
        .max(after.glyph_entries as usize),
    );
    diag.fallback_cache_cluster_entries = Some(
      diag
        .fallback_cache_cluster_entries
        .unwrap_or(0)
        .max(after.cluster_entries as usize),
    );
    diag.fallback_cache_glyph_capacity = Some(after.glyph_capacity as usize);
    diag.fallback_cache_cluster_capacity = Some(after.cluster_capacity as usize);
    diag.fallback_cache_shards = Some(after.shards as usize);
  });
}

fn record_shaping_cache_diag_entries(entries: usize) {
  let mut current = SHAPING_CACHE_DIAG_ENTRIES.load(Ordering::Relaxed);
  while entries > current {
    match SHAPING_CACHE_DIAG_ENTRIES.compare_exchange_weak(
      current,
      entries,
      Ordering::Relaxed,
      Ordering::Relaxed,
    ) {
      Ok(_) => break,
      Err(next) => current = next,
    }
  }
}

pub(crate) fn record_text_rasterize(
  timer: Option<TextDiagnosticsTimer>,
  color_glyph_rasters: usize,
  cache: TextCacheStats,
  color_cache: TextCacheStats,
) {
  if timer.is_none()
    && color_glyph_rasters == 0
    && cache.hits == 0
    && cache.misses == 0
    && color_cache.hits == 0
    && color_cache.misses == 0
  {
    return;
  }
  with_text_diagnostics(|diag| {
    diag.color_glyph_rasters += color_glyph_rasters;
    diag.glyph_cache.hits += cache.hits;
    diag.glyph_cache.misses += cache.misses;
    diag.glyph_cache.evictions += cache.evictions;
    diag.glyph_cache.bytes = diag.glyph_cache.bytes.max(cache.bytes);
    diag.color_glyph_cache.hits += color_cache.hits;
    diag.color_glyph_cache.misses += color_cache.misses;
    diag.color_glyph_cache.evictions += color_cache.evictions;
    diag.color_glyph_cache.bytes = diag.color_glyph_cache.bytes.max(color_cache.bytes);
  });
  drop(timer);
}

// ============================================================================
// Core Types
// ============================================================================

/// Text direction for layout and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
  /// Left-to-right text (English, most European languages)
  #[default]
  LeftToRight,
  /// Right-to-left text (Arabic, Hebrew)
  RightToLeft,
}

fn has_native_small_caps(style: &ComputedStyle, font_context: &FontContext) -> bool {
  let needs_c2sc = matches!(style.font_variant_caps, FontVariantCaps::AllSmallCaps);
  if let Some(font) = font_context.get_font_full(
    &style.font_family,
    style.font_weight.to_u16(),
    match style.font_style {
      CssFontStyle::Normal => FontStyle::Normal,
      CssFontStyle::Italic => FontStyle::Italic,
      CssFontStyle::Oblique(_) => FontStyle::Oblique,
    },
    DbFontStretch::from_percentage(style.font_stretch.to_percentage()),
  ) {
    let has_smcp = font_context.supports_feature(&font, *b"smcp");
    let has_c2sc = !needs_c2sc || font_context.supports_feature(&font, *b"c2sc");
    return has_smcp && has_c2sc;
  }

  false
}

impl Direction {
  /// Creates direction from a bidi level.
  ///
  /// Even levels (0, 2, 4...) are LTR, odd levels (1, 3, 5...) are RTL.
  #[inline]
  pub fn from_level(level: Level) -> Self {
    if level.is_ltr() {
      Self::LeftToRight
    } else {
      Self::RightToLeft
    }
  }

  /// Converts to rustybuzz Direction.
  #[inline]
  pub fn to_harfbuzz(self) -> HbDirection {
    match self {
      Self::LeftToRight => HbDirection::LeftToRight,
      Self::RightToLeft => HbDirection::RightToLeft,
    }
  }

  /// Returns true if this is left-to-right.
  #[inline]
  pub fn is_ltr(self) -> bool {
    matches!(self, Self::LeftToRight)
  }

  /// Returns true if this is right-to-left.
  #[inline]
  pub fn is_rtl(self) -> bool {
    matches!(self, Self::RightToLeft)
  }
}

/// Unicode script category for text itemization.
///
/// Based on Unicode Standard Annex #24 (Script Property).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Script {
  /// Common script (punctuation, numbers, symbols)
  Common,
  /// Inherited script (combining marks)
  Inherited,
  /// Unknown/unassigned script
  Unknown,
  /// Latin script (English, most European languages)
  #[default]
  Latin,
  /// Arabic script
  Arabic,
  /// Syriac script
  Syriac,
  /// Thaana script
  Thaana,
  /// N'Ko script
  Nko,
  /// Hebrew script
  Hebrew,
  /// Greek script
  Greek,
  /// Cyrillic script (Russian, etc.)
  Cyrillic,
  /// Devanagari script (Hindi, Sanskrit)
  Devanagari,
  /// Bengali script
  Bengali,
  /// Myanmar script
  Myanmar,
  /// Telugu script
  Telugu,
  /// Tamil script
  Tamil,
  /// Thai script
  Thai,
  /// Javanese script
  Javanese,
  /// Han script (Chinese, Japanese kanji, Korean hanja)
  Han,
  /// Hiragana script (Japanese)
  Hiragana,
  /// Katakana script (Japanese)
  Katakana,
  /// Hangul script (Korean)
  Hangul,
  /// Gurmukhi script
  Gurmukhi,
  /// Gujarati script
  Gujarati,
  /// Oriya/Odia script
  Oriya,
  /// Kannada script
  Kannada,
  /// Malayalam script
  Malayalam,
  /// Sinhala script
  Sinhala,
  /// Armenian script
  Armenian,
  /// Georgian script
  Georgian,
  /// Ethiopic script
  Ethiopic,
  /// Lao script
  Lao,
  /// Tibetan script
  Tibetan,
  /// Khmer script
  Khmer,
  /// Cherokee script
  Cherokee,
  /// Canadian Aboriginal Syllabics script
  CanadianAboriginal,
  /// Tai Le script
  TaiLe,
  /// Ol Chiki script
  OlChiki,
  /// Glagolitic script
  Glagolitic,
  /// Tifinagh script
  Tifinagh,
  /// Syloti Nagri script
  SylotiNagri,
  /// Meetei Mayek script
  MeeteiMayek,
  /// Gothic script
  Gothic,
}

impl Script {
  /// Detects the script of a character.
  ///
  /// Uses Unicode character ranges for script detection.
  pub fn detect(c: char) -> Self {
    let cp = c as u32;

    // Bidi format characters are default-ignorable and should not trigger script run splits, even
    // when Unicode assigns them a concrete script (e.g. ALM is in the Arabic block).
    if is_bidi_format_char(c) {
      return Self::Common;
    }

    // Combining marks should never trigger a script run split. Itemization will
    // treat them as inheriting the surrounding script, which keeps extended
    // grapheme clusters intact even when a mark lives in an "unexpected"
    // Unicode block (seen in real-world pagesets).
    if matches!(
      get_general_category(c),
      GeneralCategory::NonspacingMark
        | GeneralCategory::SpacingMark
        | GeneralCategory::EnclosingMark
    ) {
      return Self::Inherited;
    }

    // ASCII and Basic Latin
    if (0x0000..=0x007f).contains(&cp) {
      if c.is_ascii_alphabetic() {
        return Self::Latin;
      }
      return Self::Common;
    }

    // Latin Extended
    if (0x0080..=0x024f).contains(&cp) || (0x1e00..=0x1eff).contains(&cp) {
      return Self::Latin;
    }

    // Greek
    if (0x0370..=0x03ff).contains(&cp) || (0x1f00..=0x1fff).contains(&cp) {
      return Self::Greek;
    }

    // Cyrillic
    if (0x0400..=0x04ff).contains(&cp)
      || (0x0500..=0x052f).contains(&cp)
      || (0x2de0..=0x2dff).contains(&cp)
      || (0xa640..=0xa69f).contains(&cp)
    {
      return Self::Cyrillic;
    }

    // Armenian
    if (0x0530..=0x058f).contains(&cp) || (0xfb13..=0xfb17).contains(&cp) {
      return Self::Armenian;
    }

    // Hebrew
    if (0x0590..=0x05ff).contains(&cp) || (0xfb1d..=0xfb4f).contains(&cp) {
      return Self::Hebrew;
    }

    // Arabic
    if (0x0600..=0x06ff).contains(&cp)
      || (0x0750..=0x077f).contains(&cp)
      || (0x08a0..=0x08ff).contains(&cp)
      || (0xfb50..=0xfdff).contains(&cp)
      || (0xfe70..=0xfeff).contains(&cp)
    {
      return Self::Arabic;
    }

    // Syriac
    if (0x0700..=0x074f).contains(&cp) {
      return Self::Syriac;
    }

    // Thaana
    if (0x0780..=0x07bf).contains(&cp) {
      return Self::Thaana;
    }

    // N'Ko
    if (0x07c0..=0x07ff).contains(&cp) {
      return Self::Nko;
    }

    // Devanagari
    if (0x0900..=0x097f).contains(&cp) || (0xa8e0..=0xa8ff).contains(&cp) {
      return Self::Devanagari;
    }

    // Bengali
    if (0x0980..=0x09ff).contains(&cp) {
      return Self::Bengali;
    }

    // Gurmukhi
    if (0x0a00..=0x0a7f).contains(&cp) || (0xa830..=0xa83f).contains(&cp) {
      return Self::Gurmukhi;
    }

    // Gujarati
    if (0x0a80..=0x0aff).contains(&cp) {
      return Self::Gujarati;
    }

    // Oriya/Odia
    if (0x0b00..=0x0b7f).contains(&cp) {
      return Self::Oriya;
    }

    // Tamil
    if (0x0b80..=0x0bff).contains(&cp) {
      return Self::Tamil;
    }

    // Telugu
    if (0x0c00..=0x0c7f).contains(&cp) {
      return Self::Telugu;
    }

    // Kannada
    if (0x0c80..=0x0cff).contains(&cp) {
      return Self::Kannada;
    }

    // Malayalam
    if (0x0d00..=0x0d7f).contains(&cp) {
      return Self::Malayalam;
    }

    // Sinhala
    if (0x0d80..=0x0dff).contains(&cp) {
      return Self::Sinhala;
    }

    // Thai
    if (0x0e00..=0x0e7f).contains(&cp) {
      return Self::Thai;
    }

    // Lao
    if (0x0e80..=0x0eff).contains(&cp) {
      return Self::Lao;
    }

    // Tibetan
    if (0x0f00..=0x0fff).contains(&cp) {
      return Self::Tibetan;
    }

    // Myanmar
    if (0x1000..=0x109f).contains(&cp)
      || (0xa9e0..=0xa9ff).contains(&cp)
      || (0xaa60..=0xaa7f).contains(&cp)
    {
      return Self::Myanmar;
    }

    // Georgian
    if (0x10a0..=0x10ff).contains(&cp)
      || (0x1c90..=0x1cbf).contains(&cp)
      || (0x2d00..=0x2d2f).contains(&cp)
    {
      return Self::Georgian;
    }

    // Ethiopic
    if (0x1200..=0x137f).contains(&cp)
      || (0x1380..=0x139f).contains(&cp)
      || (0x2d80..=0x2ddf).contains(&cp)
      || (0xab00..=0xab2f).contains(&cp)
    {
      return Self::Ethiopic;
    }

    // Cherokee
    if (0x13a0..=0x13ff).contains(&cp) || (0xab70..=0xabbf).contains(&cp) {
      return Self::Cherokee;
    }

    // Canadian Aboriginal Syllabics
    if (0x1400..=0x167f).contains(&cp) || (0x18b0..=0x18ff).contains(&cp) {
      return Self::CanadianAboriginal;
    }

    // Khmer
    if (0x1780..=0x17ff).contains(&cp) || (0x19e0..=0x19ff).contains(&cp) {
      return Self::Khmer;
    }

    // Tai Le
    if (0x1950..=0x197f).contains(&cp) {
      return Self::TaiLe;
    }

    // Ol Chiki
    if (0x1c50..=0x1c7f).contains(&cp) {
      return Self::OlChiki;
    }

    // Javanese
    if (0xa980..=0xa9df).contains(&cp) {
      return Self::Javanese;
    }

    // Syloti Nagri
    if (0xa800..=0xa82f).contains(&cp) {
      return Self::SylotiNagri;
    }

    // Meetei Mayek
    if (0xaae0..=0xaaff).contains(&cp) || (0xabc0..=0xabff).contains(&cp) {
      return Self::MeeteiMayek;
    }

    // Hangul (Korean)
    if (0x1100..=0x11ff).contains(&cp)
      || (0x3130..=0x318f).contains(&cp)
      || (0xa960..=0xa97f).contains(&cp)
      || (0xac00..=0xd7af).contains(&cp)
      || (0xd7b0..=0xd7ff).contains(&cp)
    {
      return Self::Hangul;
    }

    // CJK Symbols and Punctuation (U+3000..=U+303F) are Script=Common in Unicode, but they need
    // language-specific glyph selection when we fall back across multiple bundled CJK faces (JP,
    // KR, SC). Treat them as Han so script-aware fallback will route through the language-specific
    // CJK mapping instead of the generic `Common` fallback list (which defaults to SC).
    if (0x3000..=0x303f).contains(&cp) {
      return Self::Han;
    }

    // Hiragana
    if (0x3040..=0x309f).contains(&cp) {
      return Self::Hiragana;
    }

    // Katakana
    if (0x30a0..=0x30ff).contains(&cp) || (0x31f0..=0x31ff).contains(&cp) {
      return Self::Katakana;
    }

    // Glagolitic
    if (0x2c00..=0x2c5f).contains(&cp) {
      return Self::Glagolitic;
    }

    // Tifinagh
    if (0x2d30..=0x2d7f).contains(&cp) {
      return Self::Tifinagh;
    }

    // CJK (Han)
    if (0x4e00..=0x9fff).contains(&cp)
      || (0x3400..=0x4dbf).contains(&cp)
      || (0x20000..=0x2a6df).contains(&cp)
      || (0x2a700..=0x2b73f).contains(&cp)
      || (0x2b740..=0x2b81f).contains(&cp)
      || (0x2b820..=0x2ceaf).contains(&cp)
      || (0x2ceb0..=0x2ebef).contains(&cp)
      || (0x30000..=0x3134f).contains(&cp)
      || (0xf900..=0xfaff).contains(&cp)
      || (0x2f800..=0x2fa1f).contains(&cp)
    {
      return Self::Han;
    }

    // Gothic
    if (0x10330..=0x1034f).contains(&cp) {
      return Self::Gothic;
    }

    // Halfwidth and Fullwidth Forms (U+FF00..=U+FFEF).
    //
    // Many of the punctuation codepoints in this block are Script=Common in Unicode, but they
    // are commonly used as East Asian typography forms (e.g. fullwidth parentheses, fullwidth
    // comma). When we fall back across multiple bundled CJK faces (JP/KR/SC), we want these
    // punctuation glyphs to use the language-specific CJK face instead of the generic `Common`
    // fallback list.
    if (0xff01..=0xff0f).contains(&cp)
      || (0xff1a..=0xff20).contains(&cp)
      || (0xff3b..=0xff40).contains(&cp)
      || (0xff5b..=0xff65).contains(&cp)
    {
      return Self::Han;
    }

    // Halfwidth Katakana (U+FF66..=U+FF9F).
    //
    // These are strong Katakana letters, not neutral punctuation. Classifying them as `Unknown`
    // causes script-aware fallback to treat them as Latin/Common, which in turn prefers the
    // default SC CJK face instead of the language-specific JP/KR mapping.
    if (0xff66..=0xff9f).contains(&cp) {
      return Self::Katakana;
    }

    // General punctuation, symbols, numbers
    if (0x2000..=0x206f).contains(&cp)
      || (0x2070..=0x209f).contains(&cp)
      || (0x20a0..=0x20cf).contains(&cp)
      || (0x2100..=0x214f).contains(&cp)
    {
      return Self::Common;
    }

    Self::Unknown
  }

  /// Returns true if this script can merge with any script.
  ///
  /// Common and Inherited scripts can merge with surrounding scripts.
  #[inline]
  pub fn is_neutral(self) -> bool {
    matches!(self, Self::Common | Self::Inherited | Self::Unknown)
  }

  /// Detects the dominant script of a text string.
  ///
  /// Neutral codepoints are ignored; if no strong script is found,
  /// Latin is returned by default.
  pub fn detect_text(text: &str) -> Self {
    use std::collections::HashMap;

    let mut counts: HashMap<Script, usize> = HashMap::new();
    for ch in text.chars() {
      let script = Script::detect(ch);
      if script.is_neutral() {
        continue;
      }
      *counts.entry(script).or_insert(0) += 1;
    }

    counts
      .into_iter()
      .max_by_key(|(_, count)| *count)
      .map(|(script, _)| script)
      .unwrap_or(Script::Latin)
  }

  /// Converts to rustybuzz Script using ISO 15924 tags.
  ///
  /// Returns None for scripts that should be auto-detected.
  pub fn to_harfbuzz(self) -> Option<rustybuzz::Script> {
    // Use ISO 15924 4-letter tags
    let tag: Option<[u8; 4]> = match self {
      Self::Latin => Some(*b"Latn"),
      Self::Arabic => Some(*b"Arab"),
      Self::Syriac => Some(*b"Syrc"),
      Self::Thaana => Some(*b"Thaa"),
      Self::Nko => Some(*b"Nkoo"),
      Self::Hebrew => Some(*b"Hebr"),
      Self::Greek => Some(*b"Grek"),
      Self::Cyrillic => Some(*b"Cyrl"),
      Self::Devanagari => Some(*b"Deva"),
      Self::Bengali => Some(*b"Beng"),
      Self::Myanmar => Some(*b"Mymr"),
      Self::Telugu => Some(*b"Telu"),
      Self::Tamil => Some(*b"Taml"),
      Self::Thai => Some(*b"Thai"),
      Self::Javanese => Some(*b"Java"),
      Self::Han => Some(*b"Hani"),
      Self::Hiragana => Some(*b"Hira"),
      Self::Katakana => Some(*b"Kana"),
      Self::Hangul => Some(*b"Hang"),
      Self::Gurmukhi => Some(*b"Guru"),
      Self::Gujarati => Some(*b"Gujr"),
      Self::Oriya => Some(*b"Orya"),
      Self::Kannada => Some(*b"Knda"),
      Self::Malayalam => Some(*b"Mlym"),
      Self::Sinhala => Some(*b"Sinh"),
      Self::Armenian => Some(*b"Armn"),
      Self::Georgian => Some(*b"Geor"),
      Self::Ethiopic => Some(*b"Ethi"),
      Self::Lao => Some(*b"Laoo"),
      Self::Tibetan => Some(*b"Tibt"),
      Self::Khmer => Some(*b"Khmr"),
      Self::Cherokee => Some(*b"Cher"),
      Self::CanadianAboriginal => Some(*b"Cans"),
      Self::TaiLe => Some(*b"Tale"),
      Self::OlChiki => Some(*b"Olck"),
      Self::Glagolitic => Some(*b"Glag"),
      Self::Tifinagh => Some(*b"Tfng"),
      Self::SylotiNagri => Some(*b"Sylo"),
      Self::MeeteiMayek => Some(*b"Mtei"),
      Self::Gothic => Some(*b"Goth"),
      Self::Common | Self::Inherited | Self::Unknown => None,
    };

    tag.and_then(|t| {
      let tag = rustybuzz::ttf_parser::Tag::from_bytes(&t);
      rustybuzz::Script::from_iso15924_tag(tag)
    })
  }
}

// ============================================================================
// Bidi Analysis
// ============================================================================

/// Explicit bidi context passed in by layout when `unicode-bidi` establishes
/// embedding/override/isolation scopes that should affect the entire text run
/// without injecting control characters.
#[derive(Clone, Copy, Debug)]
pub struct ExplicitBidiContext {
  pub level: Level,
  pub override_all: bool,
}

/// Result of bidirectional text analysis.
///
/// Contains the bidi levels for each character and metadata about
/// whether reordering is needed.
#[derive(Debug)]
pub struct BidiAnalysis {
  /// The original text being analyzed (reserved for future RTL improvements).
  text: Arc<str>,
  /// Bidi levels for each byte position (matching BidiInfo).
  levels: Vec<Level>,
  /// Paragraph boundaries in byte offsets with their base level.
  paragraphs: Vec<ParagraphBoundary>,
  /// Base paragraph level.
  base_level: Level,
  /// Whether the text contains RTL content requiring reordering.
  needs_reordering: bool,
}

#[cfg(test)]
thread_local! {
  static BIDI_INFO_NEW_CALLS: Cell<usize> = Cell::new(0);
}

#[cfg(test)]
pub(crate) fn debug_reset_bidi_info_calls() {
  BIDI_INFO_NEW_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn debug_bidi_info_calls() -> usize {
  BIDI_INFO_NEW_CALLS.with(|calls| calls.get())
}

#[inline]
fn new_bidi_info(text: &str, base_level: Option<Level>) -> BidiInfo<'_> {
  #[cfg(test)]
  BIDI_INFO_NEW_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
  BidiInfo::new(text, base_level)
}

/// Paragraph boundaries derived from bidi analysis.
#[derive(Debug, Clone, Copy)]
pub struct ParagraphBoundary {
  pub start_byte: usize,
  pub end_byte: usize,
  pub level: Level,
}

/// A directional run produced by bidi analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidiRun {
  /// Start byte offset in the original text.
  pub start: usize,
  /// End byte offset (exclusive) in the original text.
  pub end: usize,
  /// Bidi embedding level for this run.
  pub level: u8,
  /// Direction of the run derived from the level.
  pub direction: Direction,
}

impl BidiRun {
  /// Returns true if the run covers no bytes.
  pub fn is_empty(&self) -> bool {
    self.start >= self.end
  }

  /// Returns the substring covered by this run.
  pub fn text_slice<'a>(&self, text: &'a str) -> &'a str {
    &text[self.start.min(text.len())..self.end.min(text.len())]
  }
}

fn paragraph_boundaries_for_overridden_levels(
  text: &str,
  level: Level,
) -> Vec<ParagraphBoundary> {
  if text.is_empty() {
    return Vec::new();
  }

  let mut paragraphs = Vec::new();
  let mut start_byte = 0usize;
  for (idx, ch) in text.char_indices() {
    if unicode_bidi::bidi_class(ch) == BidiClass::B {
      let end_byte = idx.saturating_add(ch.len_utf8()).min(text.len());
      paragraphs.push(ParagraphBoundary {
        start_byte,
        end_byte,
        level,
      });
      start_byte = end_byte;
    }
  }

  if start_byte < text.len() {
    paragraphs.push(ParagraphBoundary {
      start_byte,
      end_byte: text.len(),
      level,
    });
  }

  if paragraphs.is_empty() {
    paragraphs.push(ParagraphBoundary {
      start_byte: 0,
      end_byte: text.len(),
      level,
    });
  }

  paragraphs
}

impl BidiAnalysis {
  /// Analyzes text for bidirectional properties.
  ///
  /// Uses the Unicode Bidirectional Algorithm (UAX #9) to determine
  /// text direction at each position.
  ///
  /// # Arguments
  ///
  /// * `text` - The text to analyze
  /// * `style` - ComputedStyle containing CSS direction property
  pub fn analyze(text: &str, style: &ComputedStyle) -> Self {
    let base_direction = match style.direction {
      CssDirection::Ltr => Direction::LeftToRight,
      CssDirection::Rtl => Direction::RightToLeft,
    };
    Self::analyze_with_base(text, style, base_direction, None)
  }

  /// Analyzes text for bidirectional properties with an explicit base direction.
  ///
  /// This mirrors CSS paragraph base resolution while allowing callers (e.g. layout)
  /// to supply the containing block's resolved direction when it differs from the
  /// style value.
  pub fn analyze_with_base(
    text: &str,
    style: &ComputedStyle,
    base_direction: Direction,
    explicit: Option<ExplicitBidiContext>,
  ) -> Self {
    if text.is_empty() {
      // Determine base direction from CSS direction property (inherited, initial LTR)
      let mut base_level = match base_direction {
        Direction::LeftToRight => Level::ltr(),
        Direction::RightToLeft => Level::rtl(),
      };
      if let Some(ctx) = explicit {
        base_level = ctx.level;
      }
      return Self {
        text: empty_arc_str(),
        levels: Vec::new(),
        paragraphs: Vec::new(),
        base_level,
        needs_reordering: false,
      };
    }

    let text = Arc::<str>::from(text);
    record_full_text_arc_alloc();
    Self::analyze_with_base_arc(text, style, base_direction, explicit)
  }

  fn analyze_with_base_arc(
    text: Arc<str>,
    style: &ComputedStyle,
    base_direction: Direction,
    explicit: Option<ExplicitBidiContext>,
  ) -> Self {
    // Determine base direction from CSS direction property (inherited, initial LTR)
    let mut base_level = match base_direction {
      Direction::LeftToRight => Level::ltr(),
      Direction::RightToLeft => Level::rtl(),
    };
    let has_explicit_context = explicit.is_some();
    let override_all = if let Some(ctx) = explicit {
      base_level = ctx.level;
      ctx.override_all
    } else {
      false
    };

    use crate::style::types::UnicodeBidi;
    // CSS bidi overrides force all characters to the element's direction and are trivially
    // resolved without running the full Unicode algorithm.
    if override_all
      || matches!(
        style.unicode_bidi,
        UnicodeBidi::BidiOverride | UnicodeBidi::IsolateOverride
      )
    {
      let levels = vec![base_level; text.len()];
      let paragraphs = paragraph_boundaries_for_overridden_levels(text.as_ref(), base_level);
      return Self {
        text,
        levels,
        paragraphs,
        base_level,
        needs_reordering: false,
      };
    }

    // Common-case LTR fast path: skip bidi analysis entirely when we know the result is
    // `needs_reordering = false` and all bytes will have the same LTR embedding level.
    //
    // This avoids the heavy `unicode_bidi::BidiInfo::new` work on typical Latin-heavy pages.
    let resolved_base_level = if matches!(style.unicode_bidi, UnicodeBidi::Plaintext)
      && !has_explicit_context
    {
      // `unicode-bidi: plaintext` resolves the paragraph base direction from first-strong when the
      // layout engine has not provided an explicit embedding context. When our fast-path checks
      // below succeed, the first-strong resolution will be LTR.
      Level::ltr()
    } else {
      base_level
    };
    if resolved_base_level.is_ltr() {
      let mut can_fast_path = true;
      if text.is_ascii() {
        // ASCII cannot contain bidi format controls or strong RTL characters.
        if text.as_bytes().iter().any(|b| matches!(b, b'\n' | b'\r')) {
          can_fast_path = false;
        }
      } else {
        for ch in text.chars() {
          if is_bidi_format_char(ch) {
            can_fast_path = false;
            break;
          }
          let cls = unicode_bidi::bidi_class(ch);
          if matches!(cls, BidiClass::R | BidiClass::AL) {
            can_fast_path = false;
            break;
          }
          // Paragraph separators/newlines require full paragraph processing; they are rare in the
          // hot LTR path so just fall back to the full algorithm.
          if cls == BidiClass::B {
            can_fast_path = false;
            break;
          }
        }
      }

      if can_fast_path {
        let levels = vec![resolved_base_level; text.len()];
        let paragraphs = vec![ParagraphBoundary {
          start_byte: 0,
          end_byte: text.len(),
          level: resolved_base_level,
        }];
        return Self {
          text,
          levels,
          paragraphs,
          base_level: resolved_base_level,
          needs_reordering: false,
        };
      }
    }

    // Run Unicode bidi algorithm (slow path).
    let base_override = match style.unicode_bidi {
      // unicode-bidi: plaintext normally resolves the paragraph base direction from the first
      // strong character in the shaped slice (UAX#9). When layout provides an explicit bidi
      // context, preserve its embedding depth so sub-range shaping cannot "flip" by re-running
      // first-strong resolution on a slice that starts with neutrals.
      UnicodeBidi::Plaintext if has_explicit_context => Some(base_level),
      UnicodeBidi::Plaintext => None,
      _ => Some(base_level),
    };
    let bidi_info = new_bidi_info(text.as_ref(), base_override);
    let BidiInfo {
      levels,
      paragraphs: info_paragraphs,
      ..
    } = bidi_info;

    // Check if any RTL content exists.
    let needs_reordering = levels.iter().any(|&level| level.is_rtl());

    let text_len = text.len();

    let para_level = info_paragraphs
      .first()
      .map(|p| p.level)
      .unwrap_or(base_level);
    let base_level = match style.unicode_bidi {
      UnicodeBidi::Plaintext => para_level,
      _ => base_level,
    };

    let paragraphs = info_paragraphs
      .into_iter()
      .map(|p| ParagraphBoundary {
        start_byte: p.range.start.min(text_len),
        end_byte: p.range.end.min(text_len),
        level: p.level,
      })
      .collect();

    Self {
      text,
      levels,
      paragraphs,
      base_level,
      needs_reordering,
    }
  }

  /// Returns true if reordering is needed for display.
  #[inline]
  pub fn needs_reordering(&self) -> bool {
    self.needs_reordering
  }

  /// Gets the bidi level at a byte index.
  #[inline]
  pub fn level_at(&self, index: usize) -> Level {
    if self.levels.is_empty() {
      return self.base_level;
    }
    let idx = index.min(self.levels.len().saturating_sub(1));
    self.levels.get(idx).copied().unwrap_or(self.base_level)
  }

  /// Gets the direction at a byte index.
  #[inline]
  pub fn direction_at(&self, index: usize) -> Direction {
    Direction::from_level(self.level_at(index))
  }

  /// Returns the base paragraph level.
  #[inline]
  pub fn base_level(&self) -> Level {
    self.base_level
  }

  /// Returns the base direction.
  #[inline]
  pub fn base_direction(&self) -> Direction {
    Direction::from_level(self.base_level)
  }

  /// Paragraph boundaries detected during bidi analysis.
  #[inline]
  pub fn paragraphs(&self) -> &[ParagraphBoundary] {
    &self.paragraphs
  }

  /// Returns the original text that was analyzed.
  pub fn text(&self) -> &str {
    self.text.as_ref()
  }

  /// Returns logical bidi runs in source order.
  pub fn logical_runs(&self) -> Vec<BidiRun> {
    if self.text.is_empty() || self.levels.is_empty() {
      return Vec::new();
    }

    let mut runs = Vec::new();
    let mut run_start = 0usize;
    let mut current_level = self.levels[0];

    for (idx, &level) in self.levels.iter().enumerate().skip(1) {
      if level != current_level {
        runs.push(self.build_run(run_start, idx, current_level));
        run_start = idx;
        current_level = level;
      }
    }

    runs.push(self.build_run(run_start, self.levels.len(), current_level));

    split_bidi_runs_by_paragraph(runs, &self.paragraphs, self.text.len())
  }

  /// Returns visual runs reordered for display.
  pub fn visual_runs(&self) -> Vec<BidiRun> {
    let runs = self.logical_runs();
    if runs.len() <= 1 {
      return runs;
    }

    fn reorder_slice(slice: &[BidiRun]) -> Vec<BidiRun> {
      if slice.len() <= 1 {
        return slice.to_vec();
      }

      let mut levels: Vec<Level> = Vec::with_capacity(slice.len());
      for run in slice {
        let Ok(level) = Level::new(run.level) else {
          return slice.to_vec();
        };
        levels.push(level);
      }

      unicode_bidi::BidiInfo::reorder_visual(&levels)
        .into_iter()
        .map(|idx| slice[idx].clone())
        .collect()
    }

    if self.paragraphs.is_empty() {
      return reorder_slice(&runs);
    }

    let mut out = Vec::with_capacity(runs.len());
    let mut idx = 0usize;
    for para in &self.paragraphs {
      while idx < runs.len() && runs[idx].end <= para.start_byte {
        idx += 1;
      }
      let start = idx;
      while idx < runs.len() && runs[idx].start < para.end_byte {
        idx += 1;
      }
      if start < idx {
        out.extend(reorder_slice(&runs[start..idx]));
      }
    }

    if idx < runs.len() {
      out.extend(reorder_slice(&runs[idx..]));
    }

    out
  }

  fn build_run(&self, start_byte: usize, end_byte: usize, level: Level) -> BidiRun {
    BidiRun {
      start: start_byte.min(self.text.len()),
      end: end_byte.min(self.text.len()),
      level: level.number(),
      direction: Direction::from_level(level),
    }
  }
}

fn split_bidi_runs_by_paragraph(
  runs: Vec<BidiRun>,
  paragraphs: &[ParagraphBoundary],
  text_len: usize,
) -> Vec<BidiRun> {
  if runs.is_empty() || paragraphs.is_empty() {
    return runs;
  }

  let mut out = Vec::with_capacity(runs.len());
  let mut para_iter = paragraphs.iter().peekable();

  for mut run in runs {
    while let Some(para) = para_iter.peek().copied() {
      if run.start >= para.end_byte {
        para_iter.next();
        continue;
      }

      let para_end = para.end_byte.min(text_len);
      if run.end <= para_end {
        out.push(run);
        break;
      }

      let left = BidiRun {
        end: para_end,
        ..run.clone()
      };
      run.start = para_end;
      out.push(left);
      para_iter.next();
    }
  }

  out
}

// ============================================================================
// Script Itemization
// ============================================================================

/// A run of text with uniform properties.
///
/// After itemization, text is split into runs where each run has:
/// - Consistent script (Latin, Arabic, etc.)
/// - Consistent direction (LTR or RTL)
/// - Consistent bidi level
#[derive(Debug, Clone)]
pub struct ItemizedRun {
  /// Start byte index in original text.
  pub start: usize,
  /// End byte index in original text (exclusive).
  pub end: usize,
  /// The text content of this run.
  pub text: String,
  /// Script for this run.
  pub script: Script,
  /// Text direction for this run.
  pub direction: Direction,
  /// Bidi embedding level.
  pub level: u8,
}

impl ItemizedRun {
  /// Returns the length of this run in bytes.
  #[inline]
  pub fn len(&self) -> usize {
    self.end - self.start
  }

  /// Returns true if this run is empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.start >= self.end
  }
}

/// Itemizes text into runs of uniform script and direction.
pub fn itemize_text(text: &str, bidi: &BidiAnalysis) -> Vec<ItemizedRun> {
  if text.is_empty() {
    return Vec::new();
  }

  #[inline]
  fn slice_to_string(text: &str, start: usize, end: usize) -> String {
    let start = start.min(text.len());
    let end = end.min(text.len());
    if start >= end {
      return String::new();
    }

    // `start`/`end` are expected to be char boundaries (they originate from `char_indices()`),
    // but use a lossy fallback to avoid panicking if upstream indices are ever inconsistent.
    match text.get(start..end) {
      Some(slice) => slice.to_string(),
      None => String::from_utf8_lossy(&text.as_bytes()[start..end]).into_owned(),
    }
  }

  // Bidi levels are only needed when we will reorder runs for visual display. When the bidi
  // analysis reports no reordering is required (e.g. pure LTR text with embedded isolates),
  // splitting runs on level boundaries is pure churn and can break adjacency-sensitive shaping
  // like kerning/ligatures even though the control characters are default-ignorable.
  let split_by_level = bidi.needs_reordering();

  let paragraphs = bidi.paragraphs();
  let mut paragraph_index = 0usize;
  let mut paragraph_end = paragraphs
    .first()
    .map(|para| para.end_byte.min(text.len()))
    .unwrap_or(text.len());
  let compute_paragraph_first_strong_script = |start: usize, end: usize| {
    let start = start.min(text.len());
    let end = end.min(text.len());
    let Some(slice) = text.get(start..end) else {
      return Script::Latin;
    };
    for ch in slice.chars() {
      let script = Script::detect(ch);
      if !script.is_neutral() {
        return script;
      }
    }
    Script::Latin
  };
  let mut paragraph_first_strong_script = match paragraphs.first() {
    Some(first_para) => {
      compute_paragraph_first_strong_script(first_para.start_byte, first_para.end_byte)
    }
    None => compute_paragraph_first_strong_script(0, text.len()),
  };

  let mut runs = Vec::new();
  let mut current_start = 0;
  let mut current_script: Option<Script> = None;
  let mut current_direction: Option<Direction> = None;
  let mut current_level: Option<u8> = None;

  let mut flush_run = |end: usize,
                       runs: &mut Vec<ItemizedRun>,
                       current_start: usize,
                       current_script: Option<Script>,
                       current_direction: Option<Direction>,
                       current_level: Option<u8>| {
    if current_start >= end {
      return;
    }
    let (Some(script), Some(direction), Some(level)) =
      (current_script, current_direction, current_level)
    else {
      return;
    };

    let run_text = slice_to_string(text, current_start, end);
    if run_text.is_empty() {
      return;
    }

    #[cfg(any(test, debug_assertions))]
    {
      ITEMIZE_TEXT_RUNS_PRODUCED.with(|runs_count| runs_count.set(runs_count.get() + 1));
      ITEMIZE_TEXT_BYTES_COPIED.with(|bytes| bytes.set(bytes.get() + run_text.len()));
    }

    runs.push(ItemizedRun {
      start: current_start,
      end,
      text: run_text,
      script,
      direction,
      level,
    });
  };

  for (idx, ch) in text.char_indices() {
    while idx >= paragraph_end && paragraph_index + 1 < paragraphs.len() {
      flush_run(
        idx,
        &mut runs,
        current_start,
        current_script,
        current_direction,
        current_level,
      );

      current_start = idx;
      current_script = None;
      current_direction = None;
      current_level = None;

      paragraph_index += 1;
      let para = paragraphs[paragraph_index];
      paragraph_end = para.end_byte.min(text.len());
      paragraph_first_strong_script =
        compute_paragraph_first_strong_script(para.start_byte, para.end_byte);
    }

    let char_script = Script::detect(ch);
    let level = bidi.level_at(idx);
    let mut char_direction = Direction::from_level(level);
    let mut char_level = if split_by_level {
      level.number()
    } else {
      // When we skip splitting by explicit levels (because the bidi analysis reports no visual
      // reordering is needed), keep a stable level to avoid churn. However, explicit embedding
      // contexts can set an RTL base even for slices that contain only LTR characters; in that case
      // using the base level would produce an odd level for an LTR run. Fall back to the resolved
      // character level so the run level always matches its direction parity.
      let base_level = bidi.base_level();
      if Direction::from_level(base_level) == char_direction {
        base_level.number()
      } else {
        level.number()
      }
    };

    // Bidi format characters (embeddings/isolates/marks) are default-ignorable and should not
    // break adjacency-sensitive shaping (kerning/ligatures). Treat them as belonging to the
    // surrounding run so they never introduce direction/level boundaries during itemization.
    if is_bidi_format_char(ch) {
      if let Some(dir) = current_direction {
        char_direction = dir;
      }
      if let Some(run_level) = current_level {
        char_level = run_level;
      }
    }

    // Resolve neutral scripts based on context
    let resolved_script = if char_script.is_neutral() {
      current_script.unwrap_or(paragraph_first_strong_script)
    } else {
      char_script
    };

    // Check if we need to start a new run
    let needs_new_run = match (current_script, current_direction, current_level) {
      (None, _, _) => false, // First character, no current run
      (Some(script), Some(dir), Some(level)) => {
        // New run if direction, level, or script changes
        dir != char_direction
          || (split_by_level && level != char_level)
          || (!char_script.is_neutral() && script != resolved_script)
      }
      _ => false,
    };

    if needs_new_run {
      // Finish current run
      flush_run(
        idx,
        &mut runs,
        current_start,
        current_script,
        current_direction,
        current_level,
      );
      current_start = idx;
    }

    // Update current run properties
    if !char_script.is_neutral() {
      current_script = Some(resolved_script);
    } else if current_script.is_none() {
      current_script = Some(paragraph_first_strong_script);
    }
    current_direction = Some(char_direction);
    current_level = Some(char_level);
  }

  // Finish last run
  flush_run(
    text.len(),
    &mut runs,
    current_start,
    current_script,
    current_direction,
    current_level,
  );

  runs
}

// ============================================================================
// Shaping Clusters
// ============================================================================

#[inline]
fn is_hangul_jamo(cp: u32) -> bool {
  // Hangul syllable composition rules (UAX#29 GB6-GB8) allow multiple Jamo codepoints
  // to form a single extended grapheme cluster. Treat any Jamo codepoint as requiring
  // full grapheme segmentation so we never split these clusters when doing font
  // fallback/shaping.
  (0x1100..=0x11ff).contains(&cp)
    || (0xa960..=0xa97f).contains(&cp)
    || (0xd7b0..=0xd7ff).contains(&cp)
}

#[inline]
fn is_grapheme_prepend(cp: u32) -> bool {
  // Grapheme_Cluster_Break=Prepend characters attach to the following cluster (UAX#29 GB9b).
  // They are rare in the pageset, but misclassifying them as "cluster-trivial" can cause
  // us to split clusters in the fallback pipeline.
  matches!(
    cp,
    0x0600..=0x0605 | 0x06dd | 0x070f | 0x08e2 | 0x110bd | 0x110cd
  )
}

#[inline]
fn is_emoji_sequence_trigger(ch: char) -> bool {
  // We only need to run `emoji::find_emoji_sequence_spans` when the text could contain a
  // multi-codepoint emoji sequence. Those sequences are always signaled by one of:
  // - VS15/VS16 (emoji/text presentation)
  // - ZWJ
  // - Regional indicator symbols (flags)
  // - Emoji modifier + base
  // - Tag characters (subdivision flags)
  // - Combining enclosing keycap (keycap sequences)
  let cp = ch as u32;
  matches!(cp, 0x200d | 0xfe0e | 0xfe0f | 0x20e3)
    || (0x1f1e0..=0x1f1ff).contains(&cp)
    || (0x1f3fb..=0x1f3ff).contains(&cp)
    || (0xe0020..=0xe007f).contains(&cp)
}

#[inline]
fn requires_full_grapheme_segmentation(ch: char) -> bool {
  // Most real-world "Latin" text is cluster-trivial even when it contains a small amount of
  // non-ASCII punctuation (curly quotes, dashes, etc). We only fall back to full grapheme
  // segmentation when we see codepoints that can participate in multi-scalar grapheme clusters
  // (marks, joiners, variation selectors, flags, emoji modifiers, etc).
  if ch.is_ascii() {
    return false;
  }

  let cp = ch as u32;
  if is_unicode_mark(ch) {
    return true;
  }

  // Join controls and variation selectors participate in the UAX#29 rules as "Extend"/"ZWJ" and
  // must not be split into separate fallback/shaping clusters.
  if matches!(cp, 0x200c | 0x200d)
    || (0xfe00..=0xfe0f).contains(&cp)
    || (0xe0100..=0xe01ef).contains(&cp)
    || (0x180b..=0x180d).contains(&cp)
  {
    return true;
  }

  // Regional indicator symbols join into flag sequences.
  if (0x1f1e0..=0x1f1ff).contains(&cp) {
    return true;
  }

  // Emoji modifiers participate in emoji modifier sequences.
  if (0x1f3fb..=0x1f3ff).contains(&cp) {
    return true;
  }

  // Tag characters are used for emoji subdivision flags.
  if (0xe0020..=0xe007f).contains(&cp) {
    return true;
  }

  // Halfwidth Katakana voiced/semi-voiced sound marks are Grapheme_Extend characters even though
  // they are not in the Unicode Mark (M*) general categories.
  if matches!(cp, 0xff9e | 0xff9f) {
    return true;
  }

  is_hangul_jamo(cp) || is_grapheme_prepend(cp)
}

fn atomic_shaping_clusters_into(text: &str, clusters: &mut Vec<(usize, usize)>) {
  clusters.clear();
  if text.is_empty() {
    return;
  }

  if text.is_ascii() {
    // ASCII text cannot contain combining marks, ZWJ sequences, or variation selectors, so the
    // grapheme boundaries are trivial apart from the CRLF special-case in UAX#29.
    let bytes = text.as_bytes();
    clusters.reserve(bytes.len().saturating_sub(clusters.len()));
    let mut idx = 0usize;
    while idx < bytes.len() {
      if bytes[idx] == b'\r' && bytes.get(idx + 1) == Some(&b'\n') {
        clusters.push((idx, idx + 2));
        idx += 2;
      } else {
        clusters.push((idx, idx + 1));
        idx += 1;
      }
    }
    return;
  }

  // Try the "cluster-trivial" Unicode path while scanning for codepoints that require full
  // grapheme segmentation. This avoids a separate pre-scan pass for long runs that are safe to
  // cluster by scalar boundaries (e.g. CJK text or Latin text with curly quotes/dashes).
  const CLUSTER_TRIVIAL_RESERVE_LIMIT: usize = 4096;
  clusters.reserve(text.len().min(CLUSTER_TRIVIAL_RESERVE_LIMIT));
  let mut iter = text.char_indices().peekable();
  let mut needs_full_segmentation = false;
  let mut needs_emoji_sequence_spans = false;

  while let Some((start, ch)) = iter.next() {
    if !needs_emoji_sequence_spans && is_emoji_sequence_trigger(ch) {
      needs_emoji_sequence_spans = true;
      if needs_full_segmentation {
        break;
      }
    }

    if needs_full_segmentation {
      continue;
    }

    if requires_full_grapheme_segmentation(ch) {
      needs_full_segmentation = true;
      clusters.clear();
      if needs_emoji_sequence_spans {
        break;
      }
      continue;
    }

    if ch == '\r' {
      if let Some(&(next_start, next_ch)) = iter.peek() {
        if next_ch == '\n' {
          iter.next();
          clusters.push((start, next_start + next_ch.len_utf8()));
          continue;
        }
      }
    }
    clusters.push((start, start + ch.len_utf8()));
  }

  if !needs_full_segmentation {
    return;
  }

  let sequences = needs_emoji_sequence_spans
    .then(|| emoji::find_emoji_sequence_spans(text))
    .unwrap_or_default();

  if sequences.is_empty() {
    let mut prev = 0usize;
    for boundary in UnicodeSegmentation::grapheme_indices(text, true)
      .map(|(idx, _)| idx)
      .skip(1)
      .chain(std::iter::once(text.len()))
    {
      if prev < boundary {
        clusters.push((prev, boundary));
      }
      prev = boundary;
    }
    return;
  }

  let mut seq_iter = sequences.iter().peekable();
  let mut prev_boundary = 0usize;
  for boundary in UnicodeSegmentation::grapheme_indices(text, true)
    .map(|(idx, _)| idx)
    .chain(std::iter::once(text.len()))
  {
    while let Some(seq) = seq_iter.peek() {
      if seq.end <= boundary {
        seq_iter.next();
      } else {
        break;
      }
    }
    if let Some(seq) = seq_iter.peek() {
      if seq.start < boundary && boundary < seq.end {
        continue;
      }
    }

    if prev_boundary < boundary {
      clusters.push((prev_boundary, boundary));
    }
    prev_boundary = boundary;
  }
}

/// Returns byte spans for atomic shaping clusters within the text.
///
/// Clusters combine extended grapheme clusters with emoji sequences so that
/// shaping and font fallback never split them.
pub fn atomic_shaping_clusters(text: &str) -> Vec<(usize, usize)> {
  let mut clusters = Vec::new();
  atomic_shaping_clusters_into(text, &mut clusters);
  clusters
}

// ============================================================================
// Font Matching
// ============================================================================

/// A run of text with an assigned font.
#[derive(Debug, Clone)]
pub struct FontRun {
  /// The text content.
  pub text: String,
  /// Start byte index in original text.
  pub start: usize,
  /// End byte index in original text.
  pub end: usize,
  /// The assigned font.
  pub font: Arc<LoadedFont>,
  /// Synthetic bold stroke width in pixels (0 = none).
  pub synthetic_bold: f32,
  /// Synthetic oblique shear factor (tan(angle); 0 = none).
  pub synthetic_oblique: f32,
  /// Script for this run.
  pub script: Script,
  /// Text direction.
  pub direction: Direction,
  /// Bidi level.
  pub level: u8,
  /// Font size in pixels.
  pub font_size: f32,
  /// Additional baseline shift in pixels (positive raises text).
  pub baseline_shift: f32,
  /// HarfBuzz language for shaping, if one is provided and can be parsed.
  pub language: Option<HbLanguage>,
  /// OpenType features to apply for this run.
  pub features: Arc<[Feature]>,
  /// Font variation settings to apply for this run.
  pub variations: Vec<Variation>,

  /// Palette index for color fonts (CPAL).
  pub palette_index: u16,
  /// Palette overrides resolved for this run.
  pub palette_overrides: Arc<Vec<(u16, Rgba)>>,
  /// Stable hash of palette overrides for cache keys.
  pub palette_override_hash: u64,

  /// Optional rotation hint for vertical writing modes.
  pub rotation: RunRotation,

  /// Whether this run should participate in vertical shaping (inline progression vertical).
  pub vertical: bool,
}

/// Collects OpenType features from computed style for a particular font family.
fn collect_opentype_features(style: &ComputedStyle, font_family: &str) -> Vec<Feature> {
  let mut features = Vec::new();
  let lig = style.font_variant_ligatures;
  let numeric = &style.font_variant_numeric;
  let east = &style.font_variant_east_asian;
  let position = style.font_variant_position;
  let caps = style.font_variant_caps;
  let alternates = &style.font_variant_alternates;
  let optimize_speed = matches!(style.text_rendering, TextRendering::OptimizeSpeed);

  let push_toggle = |features: &mut Vec<Feature>, tag: [u8; 4], enabled: bool| {
    features.push(Feature {
      tag: Tag::from_bytes(&tag),
      value: u32::from(enabled),
      start: 0,
      end: u32::MAX,
    });
  };

  // font-variant-ligatures keywords map to OpenType features
  // In browsers, `text-rendering: optimizeSpeed` is effectively treated as a hint to skip optional
  // shaping work (kerning/ligatures) for performance. Mirror that behavior by disabling the common
  // optional ligature features unless explicitly re-enabled via `font-feature-settings`.
  push_toggle(&mut features, *b"liga", lig.common && !optimize_speed);
  push_toggle(&mut features, *b"clig", lig.common && !optimize_speed);
  push_toggle(&mut features, *b"dlig", lig.discretionary && !optimize_speed);
  push_toggle(&mut features, *b"hlig", lig.historical && !optimize_speed);
  push_toggle(&mut features, *b"calt", lig.contextual && !optimize_speed);

  // font-variant-numeric mappings
  match numeric.figure {
    NumericFigure::Lining => push_toggle(&mut features, *b"lnum", true),
    NumericFigure::Oldstyle => push_toggle(&mut features, *b"onum", true),
    NumericFigure::Normal => {}
  }
  match numeric.spacing {
    NumericSpacing::Proportional => push_toggle(&mut features, *b"pnum", true),
    NumericSpacing::Tabular => push_toggle(&mut features, *b"tnum", true),
    NumericSpacing::Normal => {}
  }
  match numeric.fraction {
    NumericFraction::Diagonal => push_toggle(&mut features, *b"frac", true),
    NumericFraction::Stacked => push_toggle(&mut features, *b"afrc", true),
    NumericFraction::Normal => {}
  }
  if numeric.ordinal {
    push_toggle(&mut features, *b"ordn", true);
  }
  if numeric.slashed_zero {
    push_toggle(&mut features, *b"zero", true);
  }

  // font-variant-east-asian
  if let Some(variant) = east.variant {
    match variant {
      EastAsianVariant::Jis78 => push_toggle(&mut features, *b"jp78", true),
      EastAsianVariant::Jis83 => push_toggle(&mut features, *b"jp83", true),
      EastAsianVariant::Jis90 => push_toggle(&mut features, *b"jp90", true),
      EastAsianVariant::Jis04 => push_toggle(&mut features, *b"jp04", true),
      EastAsianVariant::Simplified => push_toggle(&mut features, *b"smpl", true),
      EastAsianVariant::Traditional => push_toggle(&mut features, *b"trad", true),
    }
  }
  if let Some(width) = east.width {
    match width {
      EastAsianWidth::FullWidth => push_toggle(&mut features, *b"fwid", true),
      EastAsianWidth::ProportionalWidth => push_toggle(&mut features, *b"pwid", true),
    }
  }
  if east.ruby {
    push_toggle(&mut features, *b"ruby", true);
  }

  match caps {
    FontVariantCaps::Normal => {}
    FontVariantCaps::SmallCaps => push_toggle(&mut features, *b"smcp", true),
    FontVariantCaps::AllSmallCaps => {
      push_toggle(&mut features, *b"smcp", true);
      push_toggle(&mut features, *b"c2sc", true);
    }
    FontVariantCaps::PetiteCaps => push_toggle(&mut features, *b"pcap", true),
    FontVariantCaps::AllPetiteCaps => {
      push_toggle(&mut features, *b"pcap", true);
      push_toggle(&mut features, *b"c2pc", true);
    }
    FontVariantCaps::Unicase => push_toggle(&mut features, *b"unic", true),
    FontVariantCaps::TitlingCaps => push_toggle(&mut features, *b"titl", true),
  }

  match position {
    FontVariantPosition::Normal => {}
    FontVariantPosition::Sub => push_toggle(&mut features, *b"subs", true),
    FontVariantPosition::Super => push_toggle(&mut features, *b"sups", true),
  }

  if alternates.historical_forms {
    push_toggle(&mut features, *b"hist", true);
  }
  let resolve_list = |ty: FontFeatureValueType, name: &str| {
    style
      .font_feature_values
      .lookup(font_family, ty, name)
      .unwrap_or(&[])
  };

  // CSS Fonts 4 requires @stylistic/@swash/@ornaments/@annotation definitions to be single-valued;
  // multi-valued definitions are syntax errors and must be ignored.
  let resolve_single = |ty: FontFeatureValueType, name: &str| {
    let values = resolve_list(ty, name);
    (values.len() == 1).then(|| values[0])
  };

  if let Some(set) = alternates.stylistic.as_ref() {
    // CSS Fonts 4 `stylistic(<feature-value-name>)` maps to `salt <feature-index>`.
    let tag = Tag::from_bytes(b"salt");
    features.retain(|f| f.tag != tag);

    let value = match set {
      FontVariantAlternateValue::Number(v) => Some(u32::from(*v)),
      FontVariantAlternateValue::Name(name) => {
        resolve_single(FontFeatureValueType::Stylistic, name.as_str())
      }
    };
    if let Some(value) = value {
      features.push(Feature {
        tag,
        value,
        start: 0,
        end: u32::MAX,
      });
    }
  }

  for set in &alternates.stylesets {
    match set {
      FontVariantAlternateValue::Number(idx) => {
        if let Some(tag) = number_tag(b"ss", u32::from(*idx)) {
          push_toggle(&mut features, tag, true);
        }
      }
      FontVariantAlternateValue::Name(name) => {
        for &idx in resolve_list(FontFeatureValueType::Styleset, name.as_str()) {
          if let Some(tag) = number_tag(b"ss", idx) {
            push_toggle(&mut features, tag, true);
          }
        }
      }
    }
  }

  for cv in &alternates.character_variants {
    match cv {
      FontVariantAlternateValue::Number(idx) => {
        if let Some(tag) = number_tag(b"cv", u32::from(*idx)) {
          let tag = Tag::from_bytes(&tag);
          features.retain(|f| f.tag != tag);
          features.push(Feature {
            tag,
            value: 1,
            start: 0,
            end: u32::MAX,
          });
        }
      }
      FontVariantAlternateValue::Name(name) => {
        let values = resolve_list(FontFeatureValueType::CharacterVariant, name.as_str());
        let Some((&feature_index, feature_value)) = (match values.len() {
          1 => values.first().map(|idx| (idx, 1u32)),
          2 => Some((&values[0], values[1])),
          _ => None,
        }) else {
          continue;
        };
        let Some(tag) = number_tag(b"cv", feature_index) else {
          continue;
        };
        let tag = Tag::from_bytes(&tag);
        features.retain(|f| f.tag != tag);
        features.push(Feature {
          tag,
          value: feature_value,
          start: 0,
          end: u32::MAX,
        });
      }
    }
  }

  if let Some(swash) = alternates.swash.as_ref() {
    // CSS Fonts 4 `swash(<feature-value-name>)` maps to both `swsh <feature-index>` and
    // `cswh <feature-index>`.
    let swsh_tag = Tag::from_bytes(b"swsh");
    let cswh_tag = Tag::from_bytes(b"cswh");
    features.retain(|f| f.tag != swsh_tag && f.tag != cswh_tag);

    let value = match swash {
      FontVariantAlternateValue::Number(v) => Some(u32::from(*v)),
      FontVariantAlternateValue::Name(name) => {
        resolve_single(FontFeatureValueType::Swash, name.as_str())
      }
    };
    if let Some(value) = value {
      features.push(Feature {
        tag: swsh_tag,
        value,
        start: 0,
        end: u32::MAX,
      });
      features.push(Feature {
        tag: cswh_tag,
        value,
        start: 0,
        end: u32::MAX,
      });
    }
  }

  if let Some(orn) = alternates.ornaments.as_ref() {
    let tag = Tag::from_bytes(b"ornm");
    features.retain(|f| f.tag != tag);

    let value = match orn {
      FontVariantAlternateValue::Number(v) => Some(u32::from(*v)),
      FontVariantAlternateValue::Name(name) => {
        resolve_single(FontFeatureValueType::Ornaments, name)
      }
    };
    if let Some(value) = value {
      features.push(Feature {
        tag,
        value,
        start: 0,
        end: u32::MAX,
      });
    }
  }

  if let Some(annotation) = alternates.annotation.as_ref() {
    // CSS `annotation()` maps to the OpenType `nalt` feature.
    let tag = Tag::from_bytes(b"nalt");
    features.retain(|f| f.tag != tag);

    let value = match annotation {
      FontVariantAlternateValue::Number(v) => Some(u32::from(*v)),
      FontVariantAlternateValue::Name(name) => {
        resolve_single(FontFeatureValueType::Annotation, name)
      }
    };
    if let Some(value) = value {
      features.push(Feature {
        tag,
        value,
        start: 0,
        end: u32::MAX,
      });
    }
  }

  // `font-kerning: auto` leaves the decision to the user agent. Chrome enables kerning for
  // typical Latin text runs by default, and real-world pages (Tailwind resets, etc) rarely set
  // `font-kerning` explicitly. Treating `auto` as "kerning enabled" matches browser behavior and
  // avoids pervasive text metric/layout diffs on pages that rely on kerning pairs.
  //
  // `text-rendering: optimizeSpeed` is a stronger hint: it disables kerning unless explicitly
  // re-enabled via `font-feature-settings`.
  let kern_enabled = !optimize_speed
    && matches!(style.font_kerning, FontKerning::Auto | FontKerning::Normal);
  push_toggle(&mut features, *b"kern", kern_enabled);

  if style.letter_spacing != 0.0 {
    for tag in [*b"liga", *b"clig", *b"dlig", *b"hlig"] {
      let tag = Tag::from_bytes(&tag);
      features.retain(|f| f.tag != tag);
      features.push(Feature {
        tag,
        value: 0,
        start: 0,
        end: u32::MAX,
      });
    }
  }

  // Low-level font-feature-settings override defaults and prior toggles.
  for setting in style.font_feature_settings.iter() {
    let tag = Tag::from_bytes(&setting.tag);
    features.retain(|f| f.tag != tag);
    features.push(Feature {
      tag,
      value: setting.value,
      start: 0,
      end: u32::MAX,
    });
  }

  features
}

fn font_metric_ratio(font: &LoadedFont, metric: FontSizeAdjustMetric) -> Option<f32> {
  font.font_size_adjust_metric_ratio(metric)
}

#[inline]
fn is_ascii_whitespace_html_css(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace_html_css(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

/// Script fallback selection only distinguishes Japanese, Korean, and Traditional Chinese
/// (Simplified Chinese + all other tags share the same fallback order).
///
/// This helper uses HTML/CSS ASCII whitespace semantics so NBSP is not treated as ignorable.
#[inline]
fn language_signature_for_script_fallback(language: &str) -> u64 {
  let language = trim_ascii_whitespace_html_css(language);
  let mut subtags = language
    .split(|ch| ch == '-' || ch == '_')
    .filter(|segment| !segment.is_empty())
    .peekable();
  let primary = subtags.next().unwrap_or_default();

  if primary.eq_ignore_ascii_case("ja") {
    return 1;
  }
  if primary.eq_ignore_ascii_case("ko") {
    return 2;
  }
  if !primary.eq_ignore_ascii_case("zh") {
    return 0;
  }

  // Keep logic in sync with `script_fallback::cjk_fallback_families`, but use ASCII whitespace
  // trimming semantics (matching HTML/CSS language tag handling in other parts of the pipeline).
  let mut extlang_count = 0usize;
  while extlang_count < 3 {
    let Some(&next) = subtags.peek() else { break };
    if next.len() == 1 {
      break;
    }
    if next.len() == 3 && next.chars().all(|ch| ch.is_ascii_alphabetic()) {
      subtags.next();
      extlang_count += 1;
      continue;
    }
    break;
  }

  let script = match subtags.peek().copied() {
    Some(subtag) if subtag.len() == 4 && subtag.chars().all(|ch| ch.is_ascii_alphabetic()) => {
      subtags.next();
      Some(subtag)
    }
    _ => None,
  };

  let region = match subtags.peek().copied() {
    Some(subtag)
      if (subtag.len() == 2 && subtag.chars().all(|ch| ch.is_ascii_alphabetic()))
        || (subtag.len() == 3 && subtag.chars().all(|ch| ch.is_ascii_digit())) =>
    {
      subtags.next();
      Some(subtag)
    }
    _ => None,
  };

  let use_traditional = match script {
    Some(script) if script.eq_ignore_ascii_case("Hant") => true,
    Some(script) if script.eq_ignore_ascii_case("Hans") => false,
    _ => region.is_some_and(|region| {
      region.eq_ignore_ascii_case("TW")
        || region.eq_ignore_ascii_case("HK")
        || region.eq_ignore_ascii_case("MO")
    }),
  };

  if use_traditional {
    3
  } else {
    0
  }
}

fn resolve_opentype_language(style: &ComputedStyle, font: &LoadedFont) -> Option<HbLanguage> {
  let (tag, opentype_lang_tag) = match &style.font_language_override {
    FontLanguageOverride::Override(tag) => (tag.as_str(), true),
    FontLanguageOverride::Normal => match font.face_settings.font_language_override.as_deref() {
      Some(tag) => (tag, true),
      None => (style.language.as_ref(), false),
    },
  };
  let tag = trim_ascii_whitespace_html_css(tag);
  if tag.is_empty() {
    return None;
  }
  if opentype_lang_tag && (tag.len() > 4 || !tag.bytes().all(|b| b.is_ascii_alphabetic())) {
    return None;
  }
  HbLanguage::from_str(tag).ok()
}

fn merge_font_face_features(style_features: &Arc<[Feature]>, font: &LoadedFont) -> Arc<[Feature]> {
  let Some(descriptor_features) = font.face_settings.font_feature_settings.as_deref() else {
    return Arc::clone(style_features);
  };

  let mut merged: Vec<Feature> = Vec::new();

  for setting in descriptor_features {
    let tag = Tag::from_bytes(&setting.tag);
    merged.retain(|f| f.tag != tag);
    merged.push(Feature {
      tag,
      value: setting.value,
      start: 0,
      end: u32::MAX,
    });
  }

  for feature in style_features.iter() {
    merged.retain(|f| f.tag != feature.tag);
    merged.push(feature.clone());
  }

  merged.into_boxed_slice().into()
}

fn font_aspect_ratio(font: &LoadedFont) -> Option<f32> {
  font.metrics().ok().and_then(|m| m.aspect_ratio())
}

/// Returns the author-preferred aspect ratio for font-size-adjust, if any.
pub fn preferred_font_aspect(style: &ComputedStyle, font_context: &FontContext) -> Option<f32> {
  match style.font_size_adjust {
    FontSizeAdjust::None => None,
    FontSizeAdjust::Number { ratio, .. } if ratio > 0.0 => Some(ratio),
    FontSizeAdjust::Number { .. } => None,
    FontSizeAdjust::FromFont { metric } => {
      let font_style = match style.font_style {
        CssFontStyle::Normal => FontStyle::Normal,
        CssFontStyle::Italic => FontStyle::Italic,
        CssFontStyle::Oblique(_) => FontStyle::Oblique,
      };
      let font_stretch = DbFontStretch::from_percentage(style.font_stretch.to_percentage());
      font_context
        .get_font_full(
          &style.font_family,
          style.font_weight.to_u16(),
          font_style,
          font_stretch,
        )
        .and_then(|font| font_metric_ratio(&font, metric))
    }
  }
}

/// Computes the used font size after applying font-size-adjust.
pub fn compute_adjusted_font_size(
  style: &ComputedStyle,
  font: &LoadedFont,
  preferred_aspect: Option<f32>,
) -> f32 {
  compute_font_size_adjusted_size(
    style.font_size,
    style.font_size_adjust,
    font,
    preferred_aspect,
  )
}

fn is_non_rendering_for_coverage(ch: char) -> bool {
  is_bidi_format_char(ch)
    || matches!(ch, '\u{200c}' | '\u{200d}')
    || ('\u{fe00}'..='\u{fe0f}').contains(&ch)
    || ('\u{e0100}'..='\u{e01ef}').contains(&ch)
    || ('\u{180b}'..='\u{180d}').contains(&ch)
    || emoji::is_tag_character(ch)
}

fn is_unicode_mark(ch: char) -> bool {
  if ch < '\u{0300}' {
    return false;
  }
  matches!(
    get_general_category(ch),
    GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark
  )
}

fn required_coverage_chars_for_cluster<'a>(
  cluster_text: &str,
  _first_char: char,
  base_char: char,
  coverage_chars_all: &'a [char],
  required_chars: &'a mut ClusterCharBuf,
) -> &'a [char] {
  let has_marks = coverage_chars_all.iter().copied().any(is_unicode_mark);
  if !has_marks {
    return coverage_chars_all;
  }

  // Keycap sequences use U+20E3 (Combining Enclosing Keycap), which is an enclosing mark.
  //
  // The general "optional marks" fallback (Task 72) treats marks as optional when resolving fonts,
  // but for keycap sequences the mark is required to render the emoji cluster as a single keycap
  // glyph. Keep marks required for these clusters so we prefer emoji fonts that support both the
  // base and the keycap mark (and avoid `.notdef` for the mark).
  if is_keycap_cluster(cluster_text) {
    return coverage_chars_all;
  }

  required_chars.clear();
  for ch in coverage_chars_all.iter().copied() {
    if !is_unicode_mark(ch) {
      required_chars.push(ch);
    }
  }
  if required_chars.is_empty() {
    required_chars.push(base_char);
  }
  required_chars.as_slice()
}

// Optional-mark fallback invariants (Task 72):
// - Only treat marks (General Category M*) as optional when the cluster also contains at least one
//   non-mark glyph that must render.
// - For mark-only clusters, at least one mark remains required so coverage requirements never
//   collapse to "empty" (which could erase the cluster entirely).
// - If we ever skip `.notdef` for missing marks, only do so when the cluster has a non-mark glyph;
//   for mark-only clusters keep `.notdef` so there's always something visible.
fn is_mark_only_cluster(text: &str) -> bool {
  let mut saw_mark = false;
  for ch in text.chars() {
    if is_non_rendering_for_coverage(ch) {
      continue;
    }
    if is_unicode_mark(ch) {
      saw_mark = true;
    } else {
      return false;
    }
  }
  saw_mark
}

#[inline]
fn is_keycap_cluster(text: &str) -> bool {
  let mut iter = text.chars();
  let Some(base) = iter.next() else {
    return false;
  };
  if !emoji::is_keycap_base(base) {
    return false;
  }

  let Some(mut ch) = iter.next() else {
    return false;
  };
  if emoji::is_variation_selector(ch) {
    let Some(next) = iter.next() else {
      return false;
    };
    ch = next;
  }

  emoji::is_combining_enclosing_keycap(ch) && iter.next().is_none()
}

/// Inline character buffer tuned for shaping clusters.
///
/// We frequently need a small list of "relevant" codepoints for a cluster when
/// doing font coverage checks. The common case is a single codepoint cluster,
/// so this keeps a small inline array and only spills to `Vec` for rare large
/// clusters (e.g. long emoji ZWJ sequences).
struct ClusterCharBuf {
  inline: [char; 4],
  inline_len: usize,
  heap: Vec<char>,
  using_heap: bool,
}

impl ClusterCharBuf {
  #[inline]
  fn new() -> Self {
    Self {
      inline: ['\0'; 4],
      inline_len: 0,
      heap: Vec::new(),
      using_heap: false,
    }
  }

  #[inline]
  fn clear(&mut self) {
    self.inline_len = 0;
    self.heap.clear();
    self.using_heap = false;
  }

  #[inline]
  fn push(&mut self, ch: char) {
    if self.using_heap {
      self.heap.push(ch);
      return;
    }

    if self.inline_len < self.inline.len() {
      self.inline[self.inline_len] = ch;
      self.inline_len += 1;
      return;
    }

    self.heap.extend_from_slice(&self.inline);
    self.heap.push(ch);
    self.using_heap = true;
  }

  #[inline]
  fn is_empty(&self) -> bool {
    self.len() == 0
  }

  #[inline]
  fn len(&self) -> usize {
    if self.using_heap {
      self.heap.len()
    } else {
      self.inline_len
    }
  }

  #[inline]
  fn as_slice(&self) -> &[char] {
    if self.using_heap {
      self.heap.as_slice()
    } else {
      &self.inline[..self.inline_len]
    }
  }
}

/// Small inline set for deduplicating local fontdb query results.
///
/// Font matching often tries many different weight/style/stretch combinations, but `fontdb::Database`
/// can return the same face ID for several adjacent queries due to fuzzy matching. Tracking a small
/// number of seen IDs avoids redundant cached-face lookups + `LoadedFont` construction without
/// allocating on the hot path.
struct SeenFontIds {
  ids: [Option<fontdb::ID>; 16],
  len: usize,
}

impl SeenFontIds {
  #[inline]
  fn new() -> Self {
    Self {
      ids: [None; 16],
      len: 0,
    }
  }

  /// Returns `true` if the ID was already present, otherwise records it and returns `false`.
  #[inline]
  fn contains_or_insert(&mut self, id: fontdb::ID) -> bool {
    for existing in &self.ids[..self.len] {
      if *existing == Some(id) {
        return true;
      }
    }
    if self.len < self.ids.len() {
      self.ids[self.len] = Some(id);
      self.len += 1;
    }
    false
  }
}

/// Inline set of unique codepoints (for run-level font coverage fast paths).
struct SeenChars {
  chars: [char; 128],
  len: usize,
}

impl SeenChars {
  #[inline]
  fn new() -> Self {
    Self {
      chars: ['\0'; 128],
      len: 0,
    }
  }

  #[inline]
  fn is_empty(&self) -> bool {
    self.len == 0
  }

  #[inline]
  fn as_slice(&self) -> &[char] {
    &self.chars[..self.len]
  }

  /// Returns `true` if the char was already present. Returns `false` if it was newly inserted.
  /// If the inline set is full, returns `None` to signal overflow.
  #[inline]
  fn contains_or_insert(&mut self, ch: char) -> Option<bool> {
    for existing in &self.chars[..self.len] {
      if *existing == ch {
        return Some(true);
      }
    }
    if self.len >= self.chars.len() {
      return None;
    }
    self.chars[self.len] = ch;
    self.len += 1;
    Some(false)
  }
}

fn cluster_signature(text: &str) -> u64 {
  use std::hash::Hash;
  use std::hash::Hasher;

  let mut hasher = FxHasher::default();
  text.hash(&mut hasher);
  hasher.finish()
}

fn quantize_oblique_degrees(angle: Option<f32>) -> i16 {
  angle.map(|a| (a * 10.0).round() as i16).unwrap_or(0)
}

fn font_supports_all_chars(font: &LoadedFont, chars: &[char]) -> bool {
  if chars.is_empty() {
    return true;
  }
  let Some(face) = crate::text::face_cache::get_ttf_face(font) else {
    return false;
  };
  chars.iter().all(|c| face.has_glyph(*c))
}

fn same_font_face(a: &LoadedFont, b: &LoadedFont) -> bool {
  Arc::ptr_eq(&a.data, &b.data)
    && a.index == b.index
    && a.face_metrics_overrides == b.face_metrics_overrides
    && a.face_settings == b.face_settings
    && a.weight == b.weight
    && a.style == b.style
    && a.stretch == b.stretch
}

fn font_has_glyph_fast(
  db: &FontDatabase,
  font: &LoadedFont,
  cached_face: &mut Option<Arc<crate::text::face_cache::CachedFace>>,
  ch: char,
) -> bool {
  if cached_face.is_none() {
    *cached_face = font
      .id
      .and_then(|id| db.cached_face(id.inner()))
      .or_else(|| crate::text::face_cache::get_ttf_face(font));
  }

  match cached_face.as_ref() {
    Some(face) => face.has_glyph(ch),
    None => false,
  }
}

fn font_supports_all_chars_fast(
  db: &FontDatabase,
  font: &LoadedFont,
  cached_face: &mut Option<Arc<crate::text::face_cache::CachedFace>>,
  chars: &[char],
) -> bool {
  match chars {
    [] => true,
    [ch] => font_has_glyph_fast(db, font, cached_face, *ch),
    _ => {
      for &ch in chars {
        if !font_has_glyph_fast(db, font, cached_face, ch) {
          return false;
        }
      }
      true
    }
  }
}

fn last_resort_font(font_context: &FontContext) -> Option<Arc<LoadedFont>> {
  font_context.last_resort_loaded_font().map(Arc::new)
}

/// Assigns fonts to itemized runs.
///
/// Uses the font context to find appropriate fonts for each script,
/// falling back through the font family list as needed.
pub fn assign_fonts(
  runs: &[ItemizedRun],
  style: &ComputedStyle,
  font_context: &FontContext,
) -> Result<Vec<FontRun>> {
  assign_fonts_internal(
    runs,
    style,
    font_context,
    None,
    font_context.font_generation(),
    true,
  )
}

fn assign_fonts_internal(
  runs: &[ItemizedRun],
  style: &ComputedStyle,
  font_context: &FontContext,
  font_cache: Option<&FallbackCache>,
  font_generation: u64,
  enable_ascii_fast_path: bool,
) -> Result<Vec<FontRun>> {
  let descriptor_stats_enabled =
    font_cache.is_some() && text_diagnostics_enabled() && text_fallback_descriptor_stats_enabled();
  let track_last_resort_fallbacks =
    text_diagnostics_enabled() || text_diagnostics_verbose_logging();

  fn style_features_for_font(
    style: &ComputedStyle,
    font: &LoadedFont,
    cache: &mut FxHashMap<String, Arc<[Feature]>>,
  ) -> Arc<[Feature]> {
    let key = font.family.to_ascii_lowercase();
    if let Some(hit) = cache.get(&key) {
      return Arc::clone(hit);
    }
    let features: Arc<[Feature]> = collect_opentype_features(style, &font.family)
      .into_boxed_slice()
      .into();
    cache.insert(key, Arc::clone(&features));
    features
  }

  let mut feature_cache: FxHashMap<String, Arc<[Feature]>> = FxHashMap::default();
  let authored_variations = crate::text::variations::authored_variations_from_style(style);
  let preferred_aspect = preferred_font_aspect(style, font_context);
  let (font_style, requested_oblique) = match style.font_style {
    CssFontStyle::Normal => (FontStyle::Normal, None),
    CssFontStyle::Italic => (FontStyle::Italic, None),
    CssFontStyle::Oblique(angle) => (
      FontStyle::Oblique,
      Some(angle.unwrap_or(DEFAULT_OBLIQUE_ANGLE_DEG)),
    ),
  };
  let font_stretch = DbFontStretch::from_percentage(style.font_stretch.to_percentage());
  let families = build_family_entries(style);
  if let Some(cache) = font_cache {
    cache.prepare(font_generation);
  }
  let families_signature = families_signature(&families);
  let oblique_degrees = quantize_oblique_degrees(requested_oblique);
  let weight_value = style.font_weight.to_u16();
  let language = match &style.font_language_override {
    FontLanguageOverride::Normal => style.language.as_ref(),
    FontLanguageOverride::Override(tag) => tag.as_str(),
  };
  let language_signature = language_signature_for_script_fallback(language);
  let families_display =
    descriptor_stats_enabled.then(|| format_family_entries_for_sample(&families));
  let mut local_descriptors: Option<FxHashSet<FallbackCacheDescriptor>> =
    descriptor_stats_enabled.then(|| FxHashSet::default());
  let db = font_context.database();
  let slope_preferences = slope_preference_order(font_style);
  let weight_preferences = weight_preference_order(weight_value);
  let stretch_preferences = stretch_preference_order(font_stretch);
  let has_math_family = families.iter().any(|entry| {
    matches!(
      entry,
      crate::text::font_fallback::FamilyEntry::Generic(crate::text::font_db::GenericFamily::Math)
    )
  });
  let math_families = has_math_family.then(|| font_context.math_family_names());
  let math_families = math_families.as_deref();

  if font_context.is_effectively_empty() {
    let sample = runs.first().map(|run| run.text.clone()).unwrap_or_default();
    return Err(
      TextError::ShapingFailed {
        text: sample,
        reason: "Font context has no fonts; enable bundled fonts or system discovery".to_string(),
      }
      .into(),
    );
  }

  let mut font_runs = Vec::new();
  let mut cluster_spans: Vec<(usize, usize)> = Vec::new();
  for run in runs {
    // Language only influences script fallback selection for CJK scripts. Treating every distinct
    // `lang=` tag as a unique fallback-cache descriptor causes pathological churn on pages like
    // Wikipedia that annotate each language name with its own BCP47 tag.
    //
    // Collapse language signatures for scripts where it has no effect so the glyph/cluster caches
    // and descriptor hint cache can be reused across such runs.
    let language_signature_for_run = if matches!(
      run.script,
      Script::Han | Script::Hiragana | Script::Katakana
    ) {
      language_signature
    } else {
      0
    };

    // Fast path: ASCII runs (e.g. page text + code blocks) spend a lot of time in
    // per-codepoint cluster handling, even when a single font covers the run.
    if enable_ascii_fast_path
      && !run.text.is_empty()
      && !matches!(style.font_variant_emoji, FontVariantEmoji::Emoji)
    {
      let resolve_for_sample = |sample_char: char| -> Option<Arc<LoadedFont>> {
        let emoji_pref = emoji_preference_for_char(sample_char, style.font_variant_emoji);
        let require_base_glyph = !is_non_rendering_for_coverage(sample_char);
        let descriptor = font_cache.map(|_| {
          FallbackCacheDescriptor::new(
            families_signature,
            language_signature_for_run,
            run.script as u8,
            weight_value,
            font_style,
            font_stretch,
            oblique_degrees,
            emoji_pref,
            require_base_glyph,
          )
        });
        let char_cache_key = descriptor.map(|descriptor| GlyphFallbackCacheKey {
          descriptor,
          ch: sample_char,
        });

        let mut resolved: Option<Arc<LoadedFont>> = None;
        let mut skip_resolution = false;
        if let (Some(cache), Some(key)) = (font_cache, char_cache_key.as_ref()) {
          let cached = cache.get_glyph(key);
          match cached {
            Some(Some(font)) => resolved = Some(font),
            Some(None) => skip_resolution = true,
            None => {}
          }
        }
        if !skip_resolution && resolved.is_none() {
          let mut picker = FontPreferencePicker::new(emoji_pref);
          let candidate = resolve_font_for_char_with_preferences(
            sample_char,
            run.script,
            language,
            &families,
            weight_value,
            font_style,
            requested_oblique,
            font_stretch,
            font_context,
            &mut picker,
            &weight_preferences,
            &stretch_preferences,
            slope_preferences,
            math_families,
          );
          if let (Some(cache), Some(key)) = (font_cache, char_cache_key) {
            cache.insert_glyph(key, candidate.clone());
          }
          resolved = candidate;
        }

        resolved.or_else(|| last_resort_font(font_context))
      };

      if run.text.is_ascii() {
        let sample_char = run
          .text
          .bytes()
          .find(|b| !(char::from(*b)).is_ascii_control())
          .map(char::from)
          .unwrap_or('A');
        if let Some(font_arc) = resolve_for_sample(sample_char) {
          let mut face = None;
          let mut mask_low = 0u64;
          let mut mask_high = 0u64;
          for &b in run.text.as_bytes() {
            // Only check printable ASCII for glyph coverage. Control bytes like newlines and tabs do
            // not require glyph fallback, and including them here would defeat the fast path.
            if (char::from(b)).is_ascii_control() {
              continue;
            }
            if b < 64 {
              mask_low |= 1u64 << b;
            } else {
              mask_high |= 1u64 << (b - 64);
            }
          }
          let mut covers = true;
          for bit in 0..64 {
            if (mask_low >> bit) & 1 == 1 {
              let ch = bit as u8 as char;
              if !font_has_glyph_fast(db, font_arc.as_ref(), &mut face, ch) {
                covers = false;
                break;
              }
            }
          }
          if covers {
            for bit in 0..64 {
              if (mask_high >> bit) & 1 == 1 {
                let ch = (bit as u8 + 64) as char;
                if !font_has_glyph_fast(db, font_arc.as_ref(), &mut face, ch) {
                  covers = false;
                  break;
                }
              }
            }
          }

          if covers {
            let used_font_size =
              compute_adjusted_font_size(style, font_arc.as_ref(), preferred_aspect);
            let (mut synthetic_bold, synthetic_oblique) =
              compute_synthetic_styles(style, font_arc.as_ref());
            if style.font_size > 0.0 {
              synthetic_bold *= used_font_size / style.font_size;
            }
            let style_features =
              style_features_for_font(style, font_arc.as_ref(), &mut feature_cache);
            push_font_run(
              &mut font_runs,
              run,
              0,
              run.text.len(),
              font_arc,
              synthetic_bold,
              synthetic_oblique,
              used_font_size,
              0.0,
              &style_features,
              &authored_variations,
              style,
            );
            continue;
          }
        }
      } else if run.text.len() >= 32 {
        // Many pages contain mostly Latin text plus a few non-ASCII punctuation characters
        // (e.g. curly quotes). For sufficiently long runs, the Unicode grapheme segmentation +
        // per-cluster coverage checks can dominate, even though a single font covers the entire run.
        let mut unique = SeenChars::new();
        let mut sample_char: Option<char> = None;
        let mut sample_is_ascii_alnum = false;
        let mut eligible = true;

        for ch in run.text.chars() {
          if ch.is_ascii_control() || is_bidi_format_char(ch) {
            continue;
          }
          // The goal is to quickly prove that a *single* font covers the whole run. Emoji that
          // default to emoji presentation (or tag sequence components) need cluster-level
          // emoji preference handling, so keep those on the slow path. Characters with the
          // `Emoji` property but text presentation (e.g. ©, digits) are safe here as long as
          // we bail out when selectors/modifiers are present.
          if emoji::is_emoji_presentation(ch)
            || emoji::is_tag_character(ch)
            || is_unicode_mark(ch)
            || is_non_rendering_for_coverage(ch)
          {
            eligible = false;
            break;
          }

          if sample_char.is_none() || (!sample_is_ascii_alnum && ch.is_ascii_alphanumeric()) {
            sample_char = Some(ch);
            sample_is_ascii_alnum = ch.is_ascii_alphanumeric();
          }
          if unique.contains_or_insert(ch).is_none() {
            eligible = false;
            break;
          }
        }

        if eligible && !unique.is_empty() {
          let sample_char = sample_char.unwrap_or_else(|| unique.as_slice()[0]);
          if let Some(font_arc) = resolve_for_sample(sample_char) {
            let mut face = None;
            let mut covers = true;
            for ch in unique.as_slice() {
              if !font_has_glyph_fast(db, font_arc.as_ref(), &mut face, *ch) {
                covers = false;
                break;
              }
            }

            if covers {
              let used_font_size =
                compute_adjusted_font_size(style, font_arc.as_ref(), preferred_aspect);
              let (mut synthetic_bold, synthetic_oblique) =
                compute_synthetic_styles(style, font_arc.as_ref());
              if style.font_size > 0.0 {
                synthetic_bold *= used_font_size / style.font_size;
              }
              let style_features =
                style_features_for_font(style, font_arc.as_ref(), &mut feature_cache);
              push_font_run(
                &mut font_runs,
                run,
                0,
                run.text.len(),
                font_arc,
                synthetic_bold,
                synthetic_oblique,
                used_font_size,
                0.0,
                &style_features,
                &authored_variations,
                style,
              );
              continue;
            }
          }
        }
      }
    }

    struct PrimaryFont {
      font: Arc<LoadedFont>,
      is_emoji_font: bool,
      cached_face: Option<Arc<crate::text::face_cache::CachedFace>>,
    }

    struct CurrentFontRun {
      font: Arc<LoadedFont>,
      is_emoji_font: bool,
      cached_face: Option<Arc<crate::text::face_cache::CachedFace>>,
      synthetic_bold: f32,
      synthetic_oblique: f32,
      font_size: f32,
      baseline_shift: f32,
      start: usize,
    }

    let mut primary: Option<PrimaryFont> = None;
    let mut current: Option<CurrentFontRun> = None;
    let mut last_cluster_end = 0usize;
    let mut relevant_chars = ClusterCharBuf::new();
    let mut required_chars = ClusterCharBuf::new();

    atomic_shaping_clusters_into(&run.text, &mut cluster_spans);
    for (cluster_start, cluster_end) in cluster_spans.iter().copied() {
      let cluster_text = &run.text[cluster_start..cluster_end];
      let mut cluster_iter = cluster_text.chars();
      let first_char = cluster_iter.next().unwrap_or(' ');
      let is_single_char_cluster = cluster_iter.as_str().is_empty();
      let emoji_variant = style.font_variant_emoji;
      let emoji_pref = emoji_preference_for_cluster(cluster_text, emoji_variant);

      let (base_char, cluster_chars) = if is_single_char_cluster {
        (first_char, &[] as &[char])
      } else {
        relevant_chars.clear();
        let mut base: Option<char> = None;
        if !is_non_rendering_for_coverage(first_char) {
          base = Some(first_char);
          relevant_chars.push(first_char);
        }

        for ch in cluster_iter {
          if is_non_rendering_for_coverage(ch) {
            continue;
          }
          base.get_or_insert(ch);
          relevant_chars.push(ch);
        }

        let base_char = base.unwrap_or(first_char);
        let cluster_chars = relevant_chars.as_slice();
        (base_char, cluster_chars)
      };
      let require_base_glyph = !is_non_rendering_for_coverage(base_char);
      let base_arr = [base_char];
      let coverage_chars_all: &[char] = if !cluster_chars.is_empty() {
        cluster_chars
      } else if require_base_glyph {
        &base_arr[..]
      } else {
        &[]
      };
      let has_marks = coverage_chars_all.iter().copied().any(is_unicode_mark);
      let coverage_chars_required: &[char] = required_coverage_chars_for_cluster(
        cluster_text,
        first_char,
        base_char,
        coverage_chars_all,
        &mut required_chars,
      );

      let wants_emoji = matches!(emoji_pref, EmojiPreference::PreferEmoji);
      let avoids_emoji = matches!(emoji_pref, EmojiPreference::AvoidEmoji);

      if let (Some(primary_font), Some(cur)) = (primary.as_mut(), current.as_ref()) {
        if !same_font_face(primary_font.font.as_ref(), cur.font.as_ref()) {
          let emoji_ok = !(wants_emoji && !primary_font.is_emoji_font)
            && !(avoids_emoji && primary_font.is_emoji_font);
          if emoji_ok
            && (coverage_chars_required.is_empty()
              || font_supports_all_chars_fast(
                db,
                primary_font.font.as_ref(),
                &mut primary_font.cached_face,
                coverage_chars_required,
              ))
          {
            // Switch back to the run's primary font without consulting the fallback cache.
            if let Some(cur) = current.take() {
              let style_features =
                style_features_for_font(style, cur.font.as_ref(), &mut feature_cache);
              push_font_run(
                &mut font_runs,
                run,
                cur.start,
                cluster_start,
                cur.font,
                cur.synthetic_bold,
                cur.synthetic_oblique,
                cur.font_size,
                cur.baseline_shift,
                &style_features,
                &authored_variations,
                style,
              );
            }

            let font_arc = Arc::clone(&primary_font.font);
            let used_font_size =
              compute_adjusted_font_size(style, font_arc.as_ref(), preferred_aspect);
            let (mut synthetic_bold, synthetic_oblique) =
              compute_synthetic_styles(style, font_arc.as_ref());
            if style.font_size > 0.0 {
              synthetic_bold *= used_font_size / style.font_size;
            }
            current = Some(CurrentFontRun {
              font: font_arc,
              is_emoji_font: primary_font.is_emoji_font,
              cached_face: primary_font.cached_face.clone(),
              synthetic_bold,
              synthetic_oblique,
              font_size: used_font_size,
              baseline_shift: 0.0,
              start: cluster_start,
            });
            last_cluster_end = cluster_end;
            continue;
          }
        }
      }

      if let Some(cur) = current.as_mut() {
        let emoji_ok = !(wants_emoji && !cur.is_emoji_font) && !(avoids_emoji && cur.is_emoji_font);
        if emoji_ok
          && (coverage_chars_required.is_empty()
            || font_supports_all_chars_fast(
              db,
              cur.font.as_ref(),
              &mut cur.cached_face,
              coverage_chars_required,
            ))
        {
          last_cluster_end = cluster_end;
          continue;
        }
      }

      let descriptor = font_cache.map(|_| {
        FallbackCacheDescriptor::new(
          families_signature,
          language_signature_for_run,
          run.script as u8,
          weight_value,
          font_style,
          font_stretch,
          oblique_degrees,
          emoji_pref,
          require_base_glyph,
        )
      });
      if let Some(set) = local_descriptors.as_mut() {
        if let Some(descriptor) = descriptor {
          set.insert(descriptor);
        }
      }
      // Only use the cluster cache when the full scalar sequence influences font selection.
      //
      // Most clusters with a single renderable base (e.g. variation selectors) can be cached by
      // the base character alone. Emoji tag sequences are the notable exception: many distinct tag
      // sequences share the same base (U+1F3F4), so caching solely by the base codepoint would
      // conflate different flags and introduce fallback-cache churn.
      let use_cluster_cache = if coverage_chars_all.len() > 1 {
        true
      } else {
        cluster_text.chars().any(emoji::is_tag_character)
      };
      let cluster_cache_key = if use_cluster_cache {
        descriptor.map(|descriptor| ClusterFallbackCacheKey {
          descriptor,
          signature: cluster_signature(cluster_text),
        })
      } else {
        None
      };

      let cached_cluster = match (font_cache, cluster_cache_key.as_ref()) {
        (Some(cache), Some(key)) => cache.get_cluster(key),
        _ => None,
      };

      let mut resolved: Option<Arc<LoadedFont>> = cached_cluster.clone().flatten();
      let mut skip_resolution = matches!(cached_cluster, Some(None));

      if !skip_resolution
        && resolved.is_none()
        && !use_cluster_cache
        && coverage_chars_all.len() <= 1
      {
        let char_cache_key = descriptor.map(|descriptor| GlyphFallbackCacheKey {
          descriptor,
          ch: base_char,
        });
        if let (Some(cache), Some(key)) = (font_cache, char_cache_key.as_ref()) {
          let cached = cache.get_glyph(key);
          match cached {
            Some(Some(font)) => resolved = Some(font),
            Some(None) => skip_resolution = true,
            None => {}
          }
        }
        if !skip_resolution && resolved.is_none() {
          if let (Some(cache), Some(descriptor)) = (font_cache, descriptor) {
            if let Some(hint) = cache.get_descriptor_hint(&descriptor) {
              let mut face = hint.cached_face(db);
              let hint_font = hint.font;
              if coverage_chars_required.is_empty()
                || font_supports_all_chars_fast(
                  db,
                  hint_font.as_ref(),
                  &mut face,
                  coverage_chars_required,
                )
              {
                resolved = Some(hint_font);
                if let Some(key) = char_cache_key {
                  cache.insert_glyph(key, resolved.clone());
                }
              }
            }
          }
        }
        if !skip_resolution && resolved.is_none() {
          let mut picker = FontPreferencePicker::new(emoji_pref);
          let candidate = resolve_font_for_char_with_preferences(
            base_char,
            run.script,
            language,
            &families,
            weight_value,
            font_style,
            requested_oblique,
            font_stretch,
            font_context,
            &mut picker,
            &weight_preferences,
            &stretch_preferences,
            slope_preferences,
            math_families,
          );
          if let (Some(cache), Some(key)) = (font_cache, char_cache_key) {
            cache.insert_glyph(key, candidate.clone());
          }
          resolved = candidate;
        }
      }

      if !skip_resolution && resolved.is_none() {
        if let (Some(cache), Some(descriptor)) = (font_cache, descriptor) {
          if let Some(hint) = cache.get_descriptor_hint(&descriptor) {
            let mut face = hint.cached_face(db);
            let hint_font = hint.font;
            if coverage_chars_required.is_empty()
              || font_supports_all_chars_fast(
                db,
                hint_font.as_ref(),
                &mut face,
                coverage_chars_required,
              )
            {
              resolved = Some(hint_font);
            }
          }
        }
      }

      if !skip_resolution && resolved.is_none() {
        resolved = resolve_font_for_cluster_with_preferences(
          base_char,
          run.script,
          language,
          coverage_chars_all,
          &families,
          weight_value,
          font_style,
          requested_oblique,
          font_stretch,
          font_context,
          emoji_pref,
          &weight_preferences,
          &stretch_preferences,
          slope_preferences,
          math_families,
        );

        if has_marks {
          let needs_retry = match resolved.as_ref() {
            Some(font) => {
              let mut face = None;
              !font_supports_all_chars_fast(db, font.as_ref(), &mut face, coverage_chars_all)
            }
            None => true,
          };
          if needs_retry && coverage_chars_required.len() != coverage_chars_all.len() {
            if coverage_chars_required.len() <= 1 {
              // Marks are optional in mixed clusters (Task 72), so when the only required codepoint
              // is the base glyph we can reuse the glyph fallback cache rather than running the
              // full cluster resolver a second time.
              if let Some(font) = resolved.as_ref() {
                let mut face = None;
                if !font_supports_all_chars_fast(
                  db,
                  font.as_ref(),
                  &mut face,
                  coverage_chars_required,
                ) {
                  // The cluster resolver can return a best-effort font that doesn't cover the base
                  // glyph when no single face covers the entire cluster (including marks). Ensure
                  // we still resolve the required (non-mark) codepoints.
                  resolved = None;
                }
              }
              let char_cache_key = descriptor.map(|descriptor| GlyphFallbackCacheKey {
                descriptor,
                ch: base_char,
              });
              let mut glyph_cached_none = false;
              if let (Some(cache), Some(key)) = (font_cache, char_cache_key.as_ref()) {
                let cached = cache.get_glyph(key);
                match cached {
                  Some(Some(font)) => {
                    let mut face = None;
                    if font_supports_all_chars_fast(
                      db,
                      font.as_ref(),
                      &mut face,
                      coverage_chars_required,
                    ) {
                      resolved = Some(font);
                    }
                  }
                  Some(None) => glyph_cached_none = true,
                  None => {}
                }
              }
              if !glyph_cached_none && resolved.is_none() {
                let mut picker = FontPreferencePicker::new(emoji_pref);
                let candidate = resolve_font_for_char_with_preferences(
                  base_char,
                  run.script,
                  language,
                  &families,
                  weight_value,
                  font_style,
                  requested_oblique,
                  font_stretch,
                  font_context,
                  &mut picker,
                  &weight_preferences,
                  &stretch_preferences,
                  slope_preferences,
                  math_families,
                );
                if let (Some(cache), Some(key)) = (font_cache, char_cache_key) {
                  cache.insert_glyph(key, candidate.clone());
                }
                resolved = candidate;
              }
            } else {
              resolved = resolve_font_for_cluster_with_preferences(
                base_char,
                run.script,
                language,
                coverage_chars_required,
                &families,
                weight_value,
                font_style,
                requested_oblique,
                font_stretch,
                font_context,
                emoji_pref,
                &weight_preferences,
                &stretch_preferences,
                slope_preferences,
                math_families,
              );
            }
          }
        }
      }

      if track_last_resort_fallbacks && require_base_glyph {
        if let Some(font) = resolved.as_ref() {
          let base_supported = font
            .id
            .map(|id| {
              font_context
                .database()
                .has_glyph_cached(id.inner(), base_char)
            })
            .unwrap_or_else(|| font_supports_all_chars(font.as_ref(), &[base_char]));
          if !base_supported {
            record_last_resort_fallback(cluster_text);
          }
        }
      }

      if resolved.is_none() {
        resolved = last_resort_font(font_context);
        if resolved.is_some() {
          record_last_resort_fallback(cluster_text);
        }
      }

      if let (Some(cache), Some(descriptor), Some(font)) =
        (font_cache, descriptor, resolved.as_ref())
      {
        cache.insert_descriptor_hint(descriptor, Arc::clone(font));
      }

      if let (Some(cache), Some(key)) = (font_cache, cluster_cache_key) {
        cache.insert_cluster(key, resolved.clone());
      }

      let font_arc = resolved.ok_or_else(|| {
        let reason = if font_context.is_effectively_empty() {
          "No fonts are available in the font context; enable bundled fonts or system discovery"
            .to_string()
        } else {
          "Font context failed to provide a last-resort font; check font configuration".to_string()
        };
        TextError::ShapingFailed {
          text: run.text.clone(),
          reason,
        }
      })?;

      let is_emoji_font =
        font_is_emoji_font(db, font_arc.id.map(|id| id.inner()), font_arc.as_ref());
      if primary.is_none() {
        primary = Some(PrimaryFont {
          font: Arc::clone(&font_arc),
          is_emoji_font,
          cached_face: None,
        });
      }

      if current
        .as_ref()
        .is_some_and(|cur| same_font_face(cur.font.as_ref(), font_arc.as_ref()))
      {
        last_cluster_end = cluster_end;
        continue;
      }

      if let Some(cur) = current.take() {
        let style_features = style_features_for_font(style, cur.font.as_ref(), &mut feature_cache);
        push_font_run(
          &mut font_runs,
          run,
          cur.start,
          cluster_start,
          cur.font,
          cur.synthetic_bold,
          cur.synthetic_oblique,
          cur.font_size,
          cur.baseline_shift,
          &style_features,
          &authored_variations,
          style,
        );
      }

      let used_font_size = compute_adjusted_font_size(style, font_arc.as_ref(), preferred_aspect);
      let (mut synthetic_bold, synthetic_oblique) =
        compute_synthetic_styles(style, font_arc.as_ref());
      if style.font_size > 0.0 {
        synthetic_bold *= used_font_size / style.font_size;
      }
      current = Some(CurrentFontRun {
        font: font_arc,
        is_emoji_font,
        cached_face: None,
        synthetic_bold,
        synthetic_oblique,
        font_size: used_font_size,
        baseline_shift: 0.0,
        start: cluster_start,
      });

      last_cluster_end = cluster_end;
    }

    if let Some(cur) = current.take() {
      let style_features = style_features_for_font(style, cur.font.as_ref(), &mut feature_cache);
      push_font_run(
        &mut font_runs,
        run,
        cur.start,
        last_cluster_end,
        cur.font,
        cur.synthetic_bold,
        cur.synthetic_oblique,
        cur.font_size,
        cur.baseline_shift,
        &style_features,
        &authored_variations,
        style,
      );
    }
  }

  if let Some(local_descriptors) = local_descriptors {
    if !local_descriptors.is_empty() {
      let families_display = families_display.as_deref().unwrap_or_default();
      with_text_diagnostics_state(|state| {
        let stats = state
          .fallback_descriptor_stats
          .get_or_insert_with(FallbackDescriptorStatsState::default);
        stats.families.insert(families_signature);
        stats.languages.insert(language_signature);
        stats.weights.insert(weight_value);

        if state.diag.fallback_descriptor_samples.len() >= FALLBACK_DESCRIPTOR_SAMPLE_LIMIT {
          stats.descriptors.extend(local_descriptors);
          return;
        }

        for descriptor in local_descriptors {
          let inserted = stats.descriptors.insert(descriptor);
          if inserted
            && state.diag.fallback_descriptor_samples.len() < FALLBACK_DESCRIPTOR_SAMPLE_LIMIT
          {
            state
              .diag
              .fallback_descriptor_samples
              .push(format_descriptor_sample(
                descriptor,
                language,
                families_display,
              ));
          }
        }
      });
    }
  }

  Ok(coalesce_adjacent_font_runs(font_runs))
}

fn coalesce_adjacent_font_runs(runs: Vec<FontRun>) -> Vec<FontRun> {
  if runs.len() <= 1 {
    return runs;
  }

  fn features_equal(a: &Arc<[Feature]>, b: &Arc<[Feature]>) -> bool {
    if Arc::ptr_eq(a, b) {
      return true;
    }
    let a = a.as_ref();
    let b = b.as_ref();
    if a.len() != b.len() {
      return false;
    }
    a.iter().zip(b.iter()).all(|(a, b)| {
      a.tag == b.tag && a.value == b.value && a.start == b.start && a.end == b.end
    })
  }

  fn variations_equal(a: &[Variation], b: &[Variation]) -> bool {
    if a.len() != b.len() {
      return false;
    }
    a.iter()
      .zip(b.iter())
      .all(|(a, b)| a.tag == b.tag && a.value == b.value)
  }

  #[inline]
  fn starts_with_hard_break(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.starts_with(b"\n")
      || bytes.starts_with(b"\r")
      || bytes.starts_with(b"\x0B") // U+000B VERTICAL TAB
      || bytes.starts_with(b"\x0C") // U+000C FORM FEED
      || bytes.starts_with(b"\xC2\x85") // U+0085 NEXT LINE
      || bytes.starts_with(b"\xE2\x80\xA8") // U+2028 LINE SEPARATOR
      || bytes.starts_with(b"\xE2\x80\xA9") // U+2029 PARAGRAPH SEPARATOR
  }

  #[inline]
  fn ends_with_hard_break(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.ends_with(b"\n")
      || bytes.ends_with(b"\r")
      || bytes.ends_with(b"\x0B") // U+000B VERTICAL TAB
      || bytes.ends_with(b"\x0C") // U+000C FORM FEED
      || bytes.ends_with(b"\xC2\x85") // U+0085 NEXT LINE
      || bytes.ends_with(b"\xE2\x80\xA8") // U+2028 LINE SEPARATOR
      || bytes.ends_with(b"\xE2\x80\xA9") // U+2029 PARAGRAPH SEPARATOR
  }

  fn can_merge(prev: &FontRun, next: &FontRun) -> bool {
    if prev.end != next.start {
      return false;
    }

    // Avoid coalescing across paragraph boundaries or hard breaks; those are adjacency breaks for
    // shaping (and often come from `BidiInfo` paragraph segmentation).
    if ends_with_hard_break(&prev.text) || starts_with_hard_break(&next.text) {
      return false;
    }

    same_font_face(prev.font.as_ref(), next.font.as_ref())
      && prev.font_size == next.font_size
      && prev.baseline_shift == next.baseline_shift
      && prev.synthetic_bold == next.synthetic_bold
      && prev.synthetic_oblique == next.synthetic_oblique
      && prev.script == next.script
      && prev.direction == next.direction
      && prev.level == next.level
      && prev.vertical == next.vertical
      && prev.rotation == next.rotation
      && prev.language == next.language
      && features_equal(&prev.features, &next.features)
      && variations_equal(&prev.variations, &next.variations)
      && prev.palette_index == next.palette_index
      && prev.palette_override_hash == next.palette_override_hash
      && (Arc::ptr_eq(&prev.palette_overrides, &next.palette_overrides)
        || prev.palette_overrides.as_ref() == next.palette_overrides.as_ref())
  }

  let mut out: Vec<FontRun> = Vec::with_capacity(runs.len());
  for run in runs {
    if let Some(prev) = out.last_mut() {
      if can_merge(prev, &run) {
        prev.text.push_str(&run.text);
        prev.end = run.end;
        continue;
      }
    }
    out.push(run);
  }

  out
}

fn is_vertical_typographic_mode(mode: crate::style::types::WritingMode) -> bool {
  matches!(
    mode,
    crate::style::types::WritingMode::VerticalRl | crate::style::types::WritingMode::VerticalLr
  )
}

fn is_sideways_writing_mode(mode: crate::style::types::WritingMode) -> bool {
  matches!(
    mode,
    crate::style::types::WritingMode::SidewaysRl | crate::style::types::WritingMode::SidewaysLr
  )
}

fn apply_vertical_text_orientation(
  runs: Vec<FontRun>,
  orientation: crate::style::types::TextOrientation,
) -> Vec<FontRun> {
  use crate::style::types::TextOrientation;

  match orientation {
    TextOrientation::Sideways | TextOrientation::SidewaysRight => runs
      .into_iter()
      .map(|mut run| {
        run.rotation = RunRotation::Cw90;
        run
      })
      .collect(),
    TextOrientation::SidewaysLeft => runs
      .into_iter()
      .map(|mut run| {
        run.rotation = RunRotation::Ccw90;
        run
      })
      .collect(),
    TextOrientation::Upright => runs
      .into_iter()
      .map(|mut run| {
        run.rotation = RunRotation::None;
        run.vertical = true;
        run
      })
      .collect(),
    TextOrientation::Mixed => runs
      .into_iter()
      .flat_map(split_run_by_vertical_orientation)
      .collect(),
  }
}

fn apply_sideways_text_orientation(
  runs: Vec<FontRun>,
  mode: crate::style::types::WritingMode,
) -> Vec<FontRun> {
  use crate::style::types::WritingMode;

  debug_assert!(
    is_sideways_writing_mode(mode),
    "apply_sideways_text_orientation_with_mode called with non-sideways writing mode: {mode:?}"
  );

  let rotation = match mode {
    WritingMode::SidewaysRl => RunRotation::Cw90,
    WritingMode::SidewaysLr => RunRotation::Ccw90,
    _ => RunRotation::Cw90,
  };

  runs
    .into_iter()
    .map(|mut run| {
      run.rotation = rotation;
      // Sideways writing modes use horizontal typographic metrics (they rotate at paint time but
      // do not participate in vertical shaping).
      run.vertical = false;
      run
    })
    .collect()
}

fn split_run_by_vertical_orientation(run: FontRun) -> Vec<FontRun> {
  if run.text.is_empty() {
    return Vec::new();
  }

  let mut segments: Vec<FontRun> = Vec::new();
  let mut iter = run.text.char_indices();
  let (first_idx, first_char) = match iter.next() {
    Some(pair) => pair,
    None => return segments,
  };
  let mut current_rotation = rotation_for_mixed_char(first_char);
  let mut current_start = first_idx;

  for (idx, ch) in iter {
    let rotation = rotation_for_mixed_char(ch);
    if rotation != current_rotation {
      push_oriented_segment(&run, current_start, idx, current_rotation, &mut segments);
      current_start = idx;
      current_rotation = rotation;
    }
  }

  push_oriented_segment(
    &run,
    current_start,
    run.text.len(),
    current_rotation,
    &mut segments,
  );
  segments
}

fn rotation_for_mixed_char(ch: char) -> RunRotation {
  match char_orientation(ch) {
    VerticalOrientation::Upright | VerticalOrientation::TransformedOrUpright => RunRotation::None,
    VerticalOrientation::Rotated | VerticalOrientation::TransformedOrRotated => RunRotation::Cw90,
  }
}

fn push_oriented_segment(
  run: &FontRun,
  start: usize,
  end: usize,
  rotation: RunRotation,
  out: &mut Vec<FontRun>,
) {
  if start >= end {
    return;
  }

  let mut segment = run.clone();
  segment.text = run.text[start..end].to_string();
  segment.start = run.start + start;
  segment.end = run.start + end;
  segment.rotation = rotation;
  segment.vertical = matches!(rotation, RunRotation::None);
  out.push(segment);
}

fn requested_slant_angle(style: &ComputedStyle) -> Option<f32> {
  match style.font_style {
    CssFontStyle::Normal => None,
    CssFontStyle::Italic => Some(DEFAULT_OBLIQUE_ANGLE_DEG),
    CssFontStyle::Oblique(angle) => Some(angle.unwrap_or(DEFAULT_OBLIQUE_ANGLE_DEG)),
  }
}

fn compute_synthetic_styles(style: &ComputedStyle, font: &LoadedFont) -> (f32, f32) {
  let mut synthetic_bold = 0.0;
  let mut synthetic_oblique = 0.0;

  if style.font_synthesis.weight
    && style.font_weight.to_u16() > font.weight.value()
    && !font_has_axis(font, *b"wght")
  {
    let delta = (style.font_weight.to_u16() as f32 - font.weight.value() as f32).max(0.0);
    let strength = (delta / 400.0).clamp(0.0, 1.0);
    synthetic_bold = style.font_size * 0.04 * strength;
  }

  if style.font_synthesis.style && matches!(font.style, FontStyle::Normal) {
    let Some(angle) = requested_slant_angle(style) else {
      return (synthetic_bold, synthetic_oblique);
    };

    let (has_slnt_axis, has_ital_axis) = crate::text::face_cache::with_face(font, |face| {
      let mut has_slnt = false;
      let mut has_ital = false;
      for axis in face.variation_axes() {
        if axis.tag == Tag::from_bytes(b"slnt") {
          has_slnt = true;
        } else if axis.tag == Tag::from_bytes(b"ital") {
          has_ital = true;
        }
      }
      (has_slnt, has_ital)
    })
    .unwrap_or((false, false));

    let variation_covers_slant = match style.font_style {
      CssFontStyle::Oblique(_) => has_slnt_axis || has_ital_axis,
      CssFontStyle::Italic => has_ital_axis || has_slnt_axis,
      CssFontStyle::Normal => false,
    };

    if !variation_covers_slant {
      synthetic_oblique = angle.to_radians().tan();
    }
  }

  (synthetic_bold, synthetic_oblique)
}

fn font_has_axis(font: &LoadedFont, tag: [u8; 4]) -> bool {
  crate::text::face_cache::with_face(font, |face| {
    let target = Tag::from_bytes(&tag);
    face
      .variation_axes()
      .into_iter()
      .any(|axis| axis.tag == target)
  })
  .unwrap_or(false)
}

fn font_variant_position_synthesis(
  style: &ComputedStyle,
  font: &LoadedFont,
  base_font_size: f32,
  text: &str,
) -> Option<(f32, f32, Option<[u8; 4]>)> {
  if !style.font_synthesis.position {
    return None;
  }

  const SYNTHETIC_SCALE: f32 = 0.8;
  const SUPER_SHIFT: f32 = 0.34;
  const SUB_SHIFT: f32 = -0.2;

  let (position, feature_tag, fallback_shift) = match style.font_variant_position {
    crate::style::types::FontVariantPosition::Normal => return None,
    crate::style::types::FontVariantPosition::Super => (
      crate::style::types::FontVariantPosition::Super,
      *b"sups",
      SUPER_SHIFT,
    ),
    crate::style::types::FontVariantPosition::Sub => (
      crate::style::types::FontVariantPosition::Sub,
      *b"subs",
      SUB_SHIFT,
    ),
  };

  let Some(rb_face) = crate::text::face_cache::get_rustybuzz_face(font).map(|face| face.face())
  else {
    // If we can't get a shaping face, fall back to the legacy constants so we still apply
    // `font-synthesis-position` behavior when requested.
    return Some((
      SYNTHETIC_SCALE,
      base_font_size * fallback_shift,
      Some(feature_tag),
    ));
  };

  let feature = Feature {
    tag: Tag::from_bytes(&feature_tag),
    value: 1,
    start: 0,
    end: u32::MAX,
  };

  let mut unique = SeenChars::new();
  for ch in text.chars() {
    if ch.is_whitespace()
      || ch.is_ascii_control()
      || is_bidi_format_char(ch)
      || is_unicode_mark(ch)
      || is_non_rendering_for_coverage(ch)
    {
      continue;
    }

    if unique.contains_or_insert(ch).is_none() {
      // Too many unique characters; fall back to synthesis instead of doing unbounded per-glyph
      // shaping probes.
      let (scale, shift) = os2_position_metrics(font, base_font_size, position)
        .unwrap_or((SYNTHETIC_SCALE, base_font_size * fallback_shift));
      return Some((scale, shift, Some(feature_tag)));
    }

    let mut utf8 = [0u8; 4];
    let encoded = ch.encode_utf8(&mut utf8);

    let mut base_buf = UnicodeBuffer::new();
    base_buf.push_str(encoded);
    let base_shape = rustybuzz::shape(rb_face.as_ref(), &[], base_buf);

    let mut feature_buf = UnicodeBuffer::new();
    feature_buf.push_str(encoded);
    let feature_shape = rustybuzz::shape(rb_face.as_ref(), &[feature], feature_buf);

    let base_infos = base_shape.glyph_infos();
    let feature_infos = feature_shape.glyph_infos();
    let base_positions = base_shape.glyph_positions();
    let feature_positions = feature_shape.glyph_positions();
    if base_infos.is_empty()
      || feature_infos.is_empty()
      || base_positions.is_empty()
      || feature_positions.is_empty()
      || base_infos.len() != base_positions.len()
      || feature_infos.len() != feature_positions.len()
    {
      let (scale, shift) = os2_position_metrics(font, base_font_size, position)
        .unwrap_or((SYNTHETIC_SCALE, base_font_size * fallback_shift));
      return Some((scale, shift, Some(feature_tag)));
    }

    let affected = base_infos.len() != feature_infos.len()
      || base_positions.len() != feature_positions.len()
      || base_infos
        .iter()
        .zip(feature_infos)
        .any(|(base, feature)| base.glyph_id != feature.glyph_id)
      || base_positions
        .iter()
        .zip(feature_positions)
        .any(|(base, feature)| {
          base.x_advance != feature.x_advance
            || base.y_advance != feature.y_advance
            || base.x_offset != feature.x_offset
            || base.y_offset != feature.y_offset
        });
    if !affected {
      let (scale, shift) = os2_position_metrics(font, base_font_size, position)
        .unwrap_or((SYNTHETIC_SCALE, base_font_size * fallback_shift));
      return Some((scale, shift, Some(feature_tag)));
    }
  }

  None
}

fn os2_position_metrics(
  font: &LoadedFont,
  base_font_size: f32,
  position: crate::style::types::FontVariantPosition,
) -> Option<(f32, f32)> {
  let face = crate::text::face_cache::get_ttf_face(font)?;
  let face = face.face();
  let os2 = face.tables().os2?;
  let units = face.units_per_em() as f32;

  match position {
    crate::style::types::FontVariantPosition::Super => {
      let metrics = os2.superscript_metrics();
      if metrics.y_size > 0 {
        let scale = (metrics.y_size as f32 / units).clamp(0.3, 1.2);
        let shift = metrics.y_offset as f32 * (base_font_size / units);
        Some((scale, shift))
      } else {
        None
      }
    }
    crate::style::types::FontVariantPosition::Sub => {
      let metrics = os2.subscript_metrics();
      if metrics.y_size > 0 {
        let scale = (metrics.y_size as f32 / units).clamp(0.3, 1.2);
        let shift = -(metrics.y_offset as f32 * (base_font_size / units));
        Some((scale, shift))
      } else {
        None
      }
    }
    crate::style::types::FontVariantPosition::Normal => None,
  }
}

pub(crate) fn slope_preference_order(style: FontStyle) -> &'static [FontStyle] {
  match style {
    FontStyle::Normal => &[FontStyle::Normal],
    FontStyle::Italic => &[FontStyle::Italic, FontStyle::Oblique, FontStyle::Normal],
    FontStyle::Oblique => &[FontStyle::Oblique, FontStyle::Italic, FontStyle::Normal],
  }
}

pub(crate) fn weight_preference_order(weight: u16) -> Vec<u16> {
  let desired = weight.clamp(1, 1000);
  let mut candidates: Vec<u16> = (1..=9).map(|i| i * 100).collect();
  if !candidates.contains(&desired) {
    candidates.push(desired);
  }
  candidates.sort_by(|a, b| {
    weight_order_key(*a, desired)
      .cmp(&weight_order_key(*b, desired))
      .then_with(|| a.cmp(b))
  });
  candidates.dedup();
  candidates
}

pub(crate) fn stretch_preference_order(stretch: DbFontStretch) -> Vec<DbFontStretch> {
  let target = stretch.to_percentage();
  let mut variants = [
    DbFontStretch::UltraCondensed,
    DbFontStretch::ExtraCondensed,
    DbFontStretch::Condensed,
    DbFontStretch::SemiCondensed,
    DbFontStretch::Normal,
    DbFontStretch::SemiExpanded,
    DbFontStretch::Expanded,
    DbFontStretch::ExtraExpanded,
    DbFontStretch::UltraExpanded,
  ];
  variants.sort_by(|a, b| {
    let ka = stretch_order_key(a.to_percentage(), target);
    let kb = stretch_order_key(b.to_percentage(), target);
    match ka.0.cmp(&kb.0) {
      std::cmp::Ordering::Equal => ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal),
      other => other,
    }
  });
  variants.to_vec()
}

fn weight_order_key(candidate: u16, desired: u16) -> (u8, i32) {
  let desired = desired.clamp(1, 1000) as i32;
  let candidate = candidate as i32;

  if (400..=500).contains(&desired) {
    if candidate >= desired && candidate <= 500 {
      return (0, (candidate - desired).abs());
    }
    if candidate < desired {
      return (1, (desired - candidate).abs());
    }
    return (2, (candidate - desired).abs());
  }

  if desired < 400 {
    if candidate <= desired {
      return (0, (desired - candidate).abs());
    }
    return (1, (candidate - desired).abs());
  }

  // desired > 500
  if candidate >= desired {
    (0, (candidate - desired).abs())
  } else {
    (1, (desired - candidate).abs())
  }
}

fn stretch_order_key(candidate: f32, desired: f32) -> (u8, f32) {
  if (candidate - desired).abs() < f32::EPSILON {
    return (0, 0.0);
  }

  if desired <= 100.0 {
    if candidate <= desired {
      return (0, (desired - candidate).abs());
    }
    return (1, (candidate - desired).abs());
  }

  // desired > 100
  if candidate >= desired {
    (0, (candidate - desired).abs())
  } else {
    (1, (desired - candidate).abs())
  }
}

#[cfg(test)]
fn is_emoji_dominant(text: &str) -> bool {
  let mut saw_emoji = false;
  for c in text.chars() {
    if crate::text::emoji::is_emoji(c) {
      saw_emoji = true;
      continue;
    }
    if c.is_whitespace() || matches!(c, '\u{200d}' | '\u{fe0f}' | '\u{fe0e}') {
      continue;
    }
    return false;
  }
  saw_emoji
}

fn consider_local_font_candidate(
  db: &FontDatabase,
  picker: &mut FontPreferencePicker,
  id: fontdb::ID,
  cached_face: Option<&crate::text::face_cache::CachedFace>,
  covers: bool,
) -> Option<Arc<LoadedFont>> {
  if !covers && picker.first_emoji_any.is_some() && picker.first_text_any.is_some() {
    return None;
  }

  let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
    let has_color_tables =
      cached_face.is_some_and(|face| crate::text::font_db::face_has_color_tables(face.face()));
    has_color_tables
      || db.inner().face(id).is_some_and(|face| {
        face
          .families
          .iter()
          .any(|(name, _)| FontDatabase::family_name_is_emoji_font(name))
      })
  } else {
    false
  };
  if covers {
    if is_emoji_font {
      if picker.avoid_emoji && picker.first_emoji.is_some() {
        return None;
      }
    } else if picker.prefer_emoji && picker.first_text.is_some() {
      return None;
    }
  }

  let needs_any = if is_emoji_font {
    picker.first_emoji_any.is_none()
  } else {
    picker.first_text_any.is_none()
  };
  if !covers && !needs_any {
    return None;
  }

  let font = db.load_font(id)?;
  let font = Arc::new(font);
  let idx = picker.bump_order();
  picker.record_any(&font, is_emoji_font, idx);
  if covers {
    picker.consider(font, is_emoji_font, idx)
  } else {
    None
  }
}

#[derive(Default)]
struct FontPreferencePicker {
  prefer_emoji: bool,
  avoid_emoji: bool,
  first_emoji: Option<(Arc<LoadedFont>, usize)>,
  first_text: Option<(Arc<LoadedFont>, usize)>,
  first_emoji_any: Option<(Arc<LoadedFont>, usize)>,
  first_text_any: Option<(Arc<LoadedFont>, usize)>,
  order: usize,
}

impl FontPreferencePicker {
  fn new(pref: EmojiPreference) -> Self {
    Self {
      prefer_emoji: matches!(pref, EmojiPreference::PreferEmoji),
      avoid_emoji: matches!(pref, EmojiPreference::AvoidEmoji),
      ..Self::default()
    }
  }

  fn bump_order(&mut self) -> usize {
    let idx = self.order;
    self.order += 1;
    idx
  }

  fn record_any(&mut self, font: &Arc<LoadedFont>, is_emoji_font: bool, idx: usize) {
    if is_emoji_font {
      if self.first_emoji_any.is_none() {
        self.first_emoji_any = Some((Arc::clone(font), idx));
      }
    } else if self.first_text_any.is_none() {
      self.first_text_any = Some((Arc::clone(font), idx));
    }
  }

  fn consider(
    &mut self,
    font: Arc<LoadedFont>,
    is_emoji_font: bool,
    idx: usize,
  ) -> Option<Arc<LoadedFont>> {
    if is_emoji_font {
      if self.first_emoji.is_none() {
        self.first_emoji = Some((Arc::clone(&font), idx));
      }
      if self.prefer_emoji && !self.avoid_emoji {
        return Some(font);
      }
      if !self.prefer_emoji && !self.avoid_emoji {
        return Some(font);
      }
      // avoid_emoji => keep as fallback if nothing else has a glyph
    } else {
      if self.first_text.is_none() {
        self.first_text = Some((font.clone(), idx));
      }
      if self.avoid_emoji {
        return Some(font);
      }
      if !self.prefer_emoji {
        return Some(font);
      }
      // prefer_emoji => keep searching for an emoji font with coverage
    }

    None
  }

  fn finish(&mut self) -> Option<Arc<LoadedFont>> {
    let first_emoji = self.first_emoji.take();
    let first_text = self.first_text.take();

    if self.prefer_emoji && !self.avoid_emoji {
      first_emoji
        .map(|(f, _)| f)
        .or_else(|| first_text.map(|(f, _)| f))
    } else if self.avoid_emoji {
      first_text
        .map(|(f, _)| f)
        .or_else(|| first_emoji.map(|(f, _)| f))
    } else {
      match (first_text, first_emoji) {
        (Some((text, ti)), Some((emoji, ei))) => {
          if ti <= ei {
            Some(text)
          } else {
            Some(emoji)
          }
        }
        (Some((text, _)), None) => Some(text),
        (None, Some((emoji, _))) => Some(emoji),
        (None, None) => None,
      }
    }
  }
}

pub(crate) fn emoji_preference_for_char(ch: char, variant: FontVariantEmoji) -> EmojiPreference {
  crate::text::emoji_presentation::emoji_preference_for_char(ch, variant)
}

pub(crate) fn emoji_preference_with_selector(
  ch: char,
  next: Option<char>,
  variant: FontVariantEmoji,
) -> EmojiPreference {
  crate::text::emoji_presentation::emoji_preference_with_selector(ch, next, variant)
}

fn emoji_preference_for_cluster(cluster_text: &str, variant: FontVariantEmoji) -> EmojiPreference {
  crate::text::emoji_presentation::emoji_preference_for_cluster(cluster_text, variant)
}

fn build_family_entries(style: &ComputedStyle) -> Vec<crate::text::font_fallback::FamilyEntry> {
  use crate::text::font_fallback::family_name_signature;
  use crate::text::font_fallback::FamilyEntry;
  use rustc_hash::FxHashSet;
  let mut entries = Vec::new();
  let mut seen_named: FxHashSet<u64> = FxHashSet::default();
  let push_named = |name: &str, entries: &mut Vec<FamilyEntry>, seen: &mut FxHashSet<u64>| {
    let sig = family_name_signature(name);
    if seen.insert(sig) {
      entries.push(FamilyEntry::Named(name.to_string()));
    }
  };
  for family in style.font_family.iter() {
    if let Some(generic) = crate::text::font_db::GenericFamily::parse(family) {
      // Expand generic families into a deterministic fallback list so they can match injected
      // `@font-face` rules from the fixture harness (e.g. `serif` -> `Times New Roman`) before
      // falling back to the `fontdb` generic mapping.
      //
      // This keeps text layout closer to browser behavior and avoids large vertical drift when the
      // browser's default generic maps to a web font that our generic fontdb query cannot see.
      let prefer_named = generic.prefers_named_fallbacks_first();
      if prefer_named {
        for name in generic.fallback_families() {
          push_named(name, &mut entries, &mut seen_named);
        }
      }
      entries.push(FamilyEntry::Generic(generic));
      if !prefer_named {
        for name in generic.fallback_families() {
          push_named(name, &mut entries, &mut seen_named);
        }
      }
    } else {
      push_named(family, &mut entries, &mut seen_named);
      for alias in crate::text::font_db::named_family_aliases(family) {
        push_named(alias, &mut entries, &mut seen_named);
      }
    }
  }

  if matches!(style.font_variant_emoji, FontVariantEmoji::Emoji)
    && !entries.iter().any(|e| {
      matches!(
        e,
        crate::text::font_fallback::FamilyEntry::Generic(
          crate::text::font_db::GenericFamily::Emoji
        )
      )
    })
  {
    entries.insert(
      0,
      crate::text::font_fallback::FamilyEntry::Generic(crate::text::font_db::GenericFamily::Emoji),
    );
  }

  if !entries.iter().any(|e| {
    matches!(
      e,
      crate::text::font_fallback::FamilyEntry::Generic(
        crate::text::font_db::GenericFamily::SansSerif
      )
    )
  }) {
    entries.push(crate::text::font_fallback::FamilyEntry::Generic(
      crate::text::font_db::GenericFamily::SansSerif,
    ));
  }

  entries
}

#[allow(clippy::cognitive_complexity)]
fn resolve_font_for_char_with_preferences(
  ch: char,
  script: Script,
  language: &str,
  families: &[crate::text::font_fallback::FamilyEntry],
  weight: u16,
  style: FontStyle,
  oblique_angle: Option<f32>,
  stretch: DbFontStretch,
  font_context: &FontContext,
  picker: &mut FontPreferencePicker,
  weight_preferences: &[u16],
  stretch_preferences: &[DbFontStretch],
  slope_preferences: &[FontStyle],
  math_families: Option<&[String]>,
) -> Option<Arc<LoadedFont>> {
  use crate::text::font_fallback::FamilyEntry;
  let db = font_context.database();
  let is_emoji = emoji::is_emoji(ch);
  let mut math_families_storage: Option<Vec<String>> = None;
  let mut math_families_ref: &[String] = math_families.unwrap_or(&[]);
  let glyph_face_and_covers = |id: fontdb::ID| {
    let cached_face = db.cached_face(id);
    let covers = cached_face.as_ref().is_some_and(|face| face.has_glyph(ch));
    (cached_face, covers)
  };
  for entry in families {
    if let FamilyEntry::Generic(crate::text::font_db::GenericFamily::Math) = entry {
      if math_families_ref.is_empty() && math_families.is_none() {
        math_families_ref = math_families_storage
          .get_or_insert_with(|| font_context.math_family_names())
          .as_slice();
      }
      for family in math_families_ref {
        if let Some(font) =
          font_context.match_web_font_for_char(family, weight, style, stretch, oblique_angle, ch)
        {
          let font = Arc::new(font);
          if picker.prefer_emoji || picker.avoid_emoji {
            let is_emoji_font = font_is_emoji_font(db, None, font.as_ref());
            let idx = picker.bump_order();
            picker.record_any(&font, is_emoji_font, idx);
            if font_supports_all_chars(font.as_ref(), &[ch]) {
              if let Some(font) = picker.consider(font, is_emoji_font, idx) {
                return Some(font);
              }
            }
          } else {
            return Some(font);
          }
        }
        if let Some(font) =
          font_context.match_web_font_for_family(family, weight, style, stretch, oblique_angle)
        {
          let font = Arc::new(font);
          let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
            font_is_emoji_font(db, None, font.as_ref())
          } else {
            false
          };
          let idx = picker.bump_order();
          picker.record_any(&font, is_emoji_font, idx);
        }
        let mut seen_ids = SeenFontIds::new();
        for stretch_choice in stretch_preferences {
          for slope in slope_preferences {
            for weight_choice in weight_preferences {
              if let Some(id) = db.query_named_family_with_aliases(
                family.as_str(),
                *weight_choice,
                *slope,
                *stretch_choice,
              ) {
                if seen_ids.contains_or_insert(id) {
                  continue;
                }
                let (cached_face, covers) = glyph_face_and_covers(id);
                if let Some(font) =
                  consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
                {
                  return Some(font);
                }
              }
            }
          }
        }
      }
    }
    if let FamilyEntry::Named(name) = entry {
      if let Some(font) =
        font_context.match_web_font_for_char(name, weight, style, stretch, oblique_angle, ch)
      {
        let font = Arc::new(font);
        if picker.prefer_emoji || picker.avoid_emoji {
          let is_emoji_font = font_is_emoji_font(db, None, font.as_ref());
          let idx = picker.bump_order();
          picker.record_any(&font, is_emoji_font, idx);
          if font_supports_all_chars(font.as_ref(), &[ch]) {
            if let Some(font) = picker.consider(font, is_emoji_font, idx) {
              return Some(font);
            }
          }
        } else {
          return Some(font);
        }
      }
      if let Some(font) =
        font_context.match_web_font_for_family(name, weight, style, stretch, oblique_angle)
      {
        let font = Arc::new(font);
        let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
          font_is_emoji_font(db, None, font.as_ref())
        } else {
          false
        };
        let idx = picker.bump_order();
        picker.record_any(&font, is_emoji_font, idx);
      }
      if font_context.is_web_family_declared(name) {
        continue;
      }
    }

    let mut seen_ids = SeenFontIds::new();
    // Generic families can resolve to a configured platform default family name. Fixture harnesses
    // often provide those defaults via `@font-face` (e.g. mapping Times New Roman to a bundled
    // font) to keep Chrome baselines deterministic. Try the generic's named fallback list for web
    // fonts so `font-family: serif` can pick those aliased faces.
    if let FamilyEntry::Generic(generic) = entry {
      if generic.prefers_named_fallbacks_first() {
        for name in generic.fallback_families() {
          if let Some(font) =
            font_context.match_web_font_for_char(name, weight, style, stretch, oblique_angle, ch)
          {
            let font = Arc::new(font);
            if picker.prefer_emoji || picker.avoid_emoji {
              let is_emoji_font = font_is_emoji_font(db, None, font.as_ref());
              let idx = picker.bump_order();
              picker.record_any(&font, is_emoji_font, idx);
              if font_supports_all_chars(font.as_ref(), &[ch]) {
                if let Some(font) = picker.consider(font, is_emoji_font, idx) {
                  return Some(font);
                }
              }
            } else {
              return Some(font);
            }
          }
          if let Some(font) =
            font_context.match_web_font_for_family(name, weight, style, stretch, oblique_angle)
          {
            let font = Arc::new(font);
            let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
              font_is_emoji_font(db, None, font.as_ref())
            } else {
              false
            };
            let idx = picker.bump_order();
            picker.record_any(&font, is_emoji_font, idx);
          }
        }
      }
    }

    let try_generic_named_fallbacks = |generic: crate::text::font_db::GenericFamily,
                                       seen_ids: &mut SeenFontIds,
                                       picker: &mut FontPreferencePicker|
     -> Option<Arc<LoadedFont>> {
      for name in generic.fallback_families() {
        // Web fonts shadow local fonts for the same family name; if the family is declared, do not
        // fall back to local lookup (CSS Fonts 4 §5.2).
        if font_context.is_web_family_declared(name) {
          continue;
        }
        let mut seen_fallback_ids = SeenFontIds::new();
        for weight_choice in weight_preferences {
          for slope in slope_preferences {
            for stretch_choice in stretch_preferences {
              if let Some(id) =
                db.query_named_family_with_aliases(name, *weight_choice, *slope, *stretch_choice)
              {
                if seen_ids.contains_or_insert(id) || seen_fallback_ids.contains_or_insert(id) {
                  continue;
                }
                let (cached_face, covers) = glyph_face_and_covers(id);
                if let Some(font) =
                  consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
                {
                  return Some(font);
                }
              }
            }
          }
        }
      }
      None
    };

    let prefer_named_fallbacks_for_generic = match entry {
      FamilyEntry::Generic(generic) => {
        generic.prefers_named_fallbacks_first()
          && !matches!(
            generic,
            crate::text::font_db::GenericFamily::Serif
              | crate::text::font_db::GenericFamily::SansSerif
              | crate::text::font_db::GenericFamily::Monospace
          )
      }
      _ => false,
    };

    if prefer_named_fallbacks_for_generic {
      if let FamilyEntry::Generic(generic) = entry {
        if let Some(font) = try_generic_named_fallbacks(*generic, &mut seen_ids, picker) {
          return Some(font);
        }
      }
    }
    for stretch_choice in stretch_preferences {
      for slope in slope_preferences {
        for weight_choice in weight_preferences {
          let id = match entry {
            FamilyEntry::Named(name) => {
              db.query_named_family_with_aliases(name, *weight_choice, *slope, *stretch_choice)
            }
            FamilyEntry::Generic(generic) => {
              let query = fontdb::Query {
                families: &[generic.to_fontdb()],
                weight: fontdb::Weight(*weight_choice),
                stretch: (*stretch_choice).into(),
                style: (*slope).into(),
              };
              db.inner().query(&query)
            }
          };

          if let Some(id) = id {
            if seen_ids.contains_or_insert(id) {
              continue;
            }
            let (cached_face, covers) = glyph_face_and_covers(id);
            if let Some(font) =
              consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
            {
              return Some(font);
            }
          }
        }
      }
    }

    if !prefer_named_fallbacks_for_generic {
      if let FamilyEntry::Generic(generic) = entry {
        if let Some(font) = try_generic_named_fallbacks(*generic, &mut seen_ids, picker) {
          return Some(font);
        }
      }
    }
  }

  if is_emoji && !picker.avoid_emoji {
    for id in db.find_emoji_fonts() {
      let (cached_face, covers) = glyph_face_and_covers(id);
      if let Some(font) =
        consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
      {
        return Some(font);
      }
    }
  }

  for family in script_fallback::preferred_families(script, language) {
    let mut seen_ids = SeenFontIds::new();
    for stretch_choice in stretch_preferences {
      for slope in slope_preferences {
        for weight_choice in weight_preferences {
          if let Some(id) =
            db.query_named_family_with_aliases(family, *weight_choice, *slope, *stretch_choice)
          {
            if seen_ids.contains_or_insert(id) {
              continue;
            }
            let (cached_face, covers) = glyph_face_and_covers(id);
            if let Some(font) =
              consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
            {
              return Some(font);
            }
          }
        }
      }
    }
  }

  for face in db.faces() {
    let (cached_face, covers) = glyph_face_and_covers(face.id);
    if let Some(font) =
      consider_local_font_candidate(db, picker, face.id, cached_face.as_deref(), covers)
    {
      return Some(font);
    }
  }

  picker.finish()
}

#[allow(clippy::cognitive_complexity)]
fn resolve_font_for_char(
  ch: char,
  script: Script,
  language: &str,
  families: &[crate::text::font_fallback::FamilyEntry],
  weight: u16,
  style: FontStyle,
  oblique_angle: Option<f32>,
  stretch: DbFontStretch,
  font_context: &FontContext,
  picker: &mut FontPreferencePicker,
) -> Option<Arc<LoadedFont>> {
  let weight_preferences = weight_preference_order(weight);
  let stretch_preferences = stretch_preference_order(stretch);
  let slope_preferences = slope_preference_order(style);
  let has_math_family = families.iter().any(|entry| {
    matches!(
      entry,
      crate::text::font_fallback::FamilyEntry::Generic(crate::text::font_db::GenericFamily::Math)
    )
  });
  let math_families = has_math_family.then(|| font_context.math_family_names());
  resolve_font_for_char_with_preferences(
    ch,
    script,
    language,
    families,
    weight,
    style,
    oblique_angle,
    stretch,
    font_context,
    picker,
    &weight_preferences,
    &stretch_preferences,
    slope_preferences,
    math_families.as_deref(),
  )
}

#[allow(clippy::cognitive_complexity)]
fn resolve_font_for_cluster_with_preferences(
  base_char: char,
  script: Script,
  language: &str,
  coverage_chars: &[char],
  families: &[crate::text::font_fallback::FamilyEntry],
  weight: u16,
  style: FontStyle,
  oblique_angle: Option<f32>,
  stretch: DbFontStretch,
  font_context: &FontContext,
  emoji_pref: EmojiPreference,
  weight_preferences: &[u16],
  stretch_preferences: &[DbFontStretch],
  slope_preferences: &[FontStyle],
  math_families: Option<&[String]>,
) -> Option<Arc<LoadedFont>> {
  use crate::text::font_fallback::FamilyEntry;
  let db = font_context.database();
  let is_emoji = crate::text::font_db::FontDatabase::is_emoji(base_char);
  let mut math_families_storage: Option<Vec<String>> = None;
  let mut math_families_ref: &[String] = math_families.unwrap_or(&[]);
  let mut picker = FontPreferencePicker::new(emoji_pref);

  let face_and_covers_needed = |id: fontdb::ID| {
    let cached_face = db.cached_face(id);
    let covers = coverage_chars.is_empty()
      || cached_face
        .as_ref()
        .is_some_and(|face| coverage_chars.iter().all(|c| face.has_glyph(*c)));
    (cached_face, covers)
  };

  for entry in families {
    if let FamilyEntry::Generic(crate::text::font_db::GenericFamily::Math) = entry {
      if math_families_ref.is_empty() && math_families.is_none() {
        math_families_ref = math_families_storage
          .get_or_insert_with(|| font_context.math_family_names())
          .as_slice();
      }
      for family in math_families_ref {
        let web_font = font_context
          .match_web_font_for_char(family, weight, style, stretch, oblique_angle, base_char)
          .or_else(|| {
            font_context.match_web_font_for_family(family, weight, style, stretch, oblique_angle)
          });
        if let Some(font) = web_font {
          let font = Arc::new(font);
          let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
            font_is_emoji_font(db, None, font.as_ref())
          } else {
            false
          };
          let idx = picker.bump_order();
          picker.record_any(&font, is_emoji_font, idx);
          if font_supports_all_chars(font.as_ref(), coverage_chars) {
            if let Some(font) = picker.consider(font, is_emoji_font, idx) {
              return Some(font);
            }
          }
        }
        let mut seen_ids = SeenFontIds::new();
        for stretch_choice in stretch_preferences {
          for slope in slope_preferences {
            for weight_choice in weight_preferences {
              if let Some(id) = db.query_named_family_with_aliases(
                family.as_str(),
                *weight_choice,
                *slope,
                *stretch_choice,
              ) {
                if seen_ids.contains_or_insert(id) {
                  continue;
                }
                let (cached_face, covers) = face_and_covers_needed(id);
                if let Some(font) =
                  consider_local_font_candidate(db, &mut picker, id, cached_face.as_deref(), covers)
                {
                  return Some(font);
                }
              }
            }
          }
        }
      }
    }
    if let FamilyEntry::Named(name) = entry {
      let web_font = font_context
        .match_web_font_for_char(name, weight, style, stretch, oblique_angle, base_char)
        .or_else(|| {
          font_context.match_web_font_for_family(name, weight, style, stretch, oblique_angle)
        });
      if let Some(font) = web_font {
        let font = Arc::new(font);
        let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
          font_is_emoji_font(db, None, font.as_ref())
        } else {
          false
        };
        let idx = picker.bump_order();
        picker.record_any(&font, is_emoji_font, idx);
        if font_supports_all_chars(font.as_ref(), coverage_chars) {
          if let Some(font) = picker.consider(font, is_emoji_font, idx) {
            return Some(font);
          }
        }
      }
      if font_context.is_web_family_declared(name) {
        continue;
      }
    }

    let mut seen_ids = SeenFontIds::new();
    // See `resolve_font_for_char_with_preferences`: generic families can map to platform default
    // family names which fixtures provide via `@font-face`. Try the generic's fallback names for
    // web fonts before consulting local generics.
    if let FamilyEntry::Generic(generic) = entry {
      if generic.prefers_named_fallbacks_first() {
        for name in generic.fallback_families() {
          let web_font = font_context
            .match_web_font_for_char(name, weight, style, stretch, oblique_angle, base_char)
            .or_else(|| {
              font_context.match_web_font_for_family(name, weight, style, stretch, oblique_angle)
            });
          if let Some(font) = web_font {
            let font = Arc::new(font);
            let is_emoji_font = if picker.prefer_emoji || picker.avoid_emoji {
              font_is_emoji_font(db, None, font.as_ref())
            } else {
              false
            };
            let idx = picker.bump_order();
            picker.record_any(&font, is_emoji_font, idx);
            if font_supports_all_chars(font.as_ref(), coverage_chars) {
              if let Some(font) = picker.consider(font, is_emoji_font, idx) {
                return Some(font);
              }
            }
          }
        }
      }
    }

    let try_generic_named_fallbacks = |generic: crate::text::font_db::GenericFamily,
                                       seen_ids: &mut SeenFontIds,
                                       picker: &mut FontPreferencePicker|
     -> Option<Arc<LoadedFont>> {
      for name in generic.fallback_families() {
        if font_context.is_web_family_declared(name) {
          continue;
        }
        let mut seen_fallback_ids = SeenFontIds::new();
        for weight_choice in weight_preferences {
          for slope in slope_preferences {
            for stretch_choice in stretch_preferences {
              if let Some(id) =
                db.query_named_family_with_aliases(name, *weight_choice, *slope, *stretch_choice)
              {
                if seen_ids.contains_or_insert(id) || seen_fallback_ids.contains_or_insert(id) {
                  continue;
                }
                let (cached_face, covers) = face_and_covers_needed(id);
                if let Some(font) =
                  consider_local_font_candidate(db, picker, id, cached_face.as_deref(), covers)
                {
                  return Some(font);
                }
              }
            }
          }
        }
      }
      None
    };

    let prefer_named_fallbacks_for_generic = match entry {
      FamilyEntry::Generic(generic) => {
        generic.prefers_named_fallbacks_first()
          && !matches!(
            generic,
            crate::text::font_db::GenericFamily::Serif
              | crate::text::font_db::GenericFamily::SansSerif
              | crate::text::font_db::GenericFamily::Monospace
          )
      }
      _ => false,
    };

    if prefer_named_fallbacks_for_generic {
      if let FamilyEntry::Generic(generic) = entry {
        if let Some(font) = try_generic_named_fallbacks(*generic, &mut seen_ids, &mut picker) {
          return Some(font);
        }
      }
    }
    for stretch_choice in stretch_preferences {
      for slope in slope_preferences {
        for weight_choice in weight_preferences {
          let id = match entry {
            FamilyEntry::Named(name) => {
              db.query_named_family_with_aliases(name, *weight_choice, *slope, *stretch_choice)
            }
            FamilyEntry::Generic(generic) => {
              let query = fontdb::Query {
                families: &[generic.to_fontdb()],
                weight: fontdb::Weight(*weight_choice),
                stretch: (*stretch_choice).into(),
                style: (*slope).into(),
              };
              db.inner().query(&query)
            }
          };

          if let Some(id) = id {
            if seen_ids.contains_or_insert(id) {
              continue;
            }
            let (cached_face, covers) = face_and_covers_needed(id);
            if let Some(font) =
              consider_local_font_candidate(db, &mut picker, id, cached_face.as_deref(), covers)
            {
              return Some(font);
            }
          }
        }
      }
    }

    if !prefer_named_fallbacks_for_generic {
      if let FamilyEntry::Generic(generic) = entry {
        if let Some(font) = try_generic_named_fallbacks(*generic, &mut seen_ids, &mut picker) {
          return Some(font);
        }
      }
    }
  }

  if is_emoji && !picker.avoid_emoji {
    for id in db.find_emoji_fonts() {
      let (cached_face, covers) = face_and_covers_needed(id);
      if let Some(font) =
        consider_local_font_candidate(db, &mut picker, id, cached_face.as_deref(), covers)
      {
        return Some(font);
      }
    }
  }

  for family in script_fallback::preferred_families(script, language) {
    let mut seen_ids = SeenFontIds::new();
    for stretch_choice in stretch_preferences {
      for slope in slope_preferences {
        for weight_choice in weight_preferences {
          if let Some(id) =
            db.query_named_family_with_aliases(family, *weight_choice, *slope, *stretch_choice)
          {
            if seen_ids.contains_or_insert(id) {
              continue;
            }
            let (cached_face, covers) = face_and_covers_needed(id);
            if let Some(font) =
              consider_local_font_candidate(db, &mut picker, id, cached_face.as_deref(), covers)
            {
              return Some(font);
            }
          }
        }
      }
    }
  }

  for face in db.faces() {
    let (cached_face, covers) = face_and_covers_needed(face.id);
    if let Some(font) =
      consider_local_font_candidate(db, &mut picker, face.id, cached_face.as_deref(), covers)
    {
      return Some(font);
    }
  }

  picker.finish()
}

#[allow(clippy::too_many_arguments)]
fn push_font_run(
  out: &mut Vec<FontRun>,
  run: &ItemizedRun,
  start: usize,
  end: usize,
  font: Arc<LoadedFont>,
  synthetic_bold: f32,
  synthetic_oblique: f32,
  font_size: f32,
  baseline_shift: f32,
  style_features: &Arc<[Feature]>,
  authored_variations: &[Variation],
  style: &ComputedStyle,
) {
  let segment_text = &run.text[start..end];
  let mut run_font_size = font_size;
  let mut run_baseline_shift = baseline_shift;
  let mut run_synthetic_bold = synthetic_bold;
  let language = resolve_opentype_language(style, font.as_ref());

  let mut features = merge_font_face_features(style_features, font.as_ref());

  let mut position_disable_tag: Option<[u8; 4]> = None;
  if let Some((position_scale, position_shift, disable_tag)) =
    font_variant_position_synthesis(style, font.as_ref(), run_font_size, segment_text)
  {
    run_font_size *= position_scale;
    run_synthetic_bold *= position_scale;
    run_baseline_shift += position_shift;
    position_disable_tag = disable_tag;
  }

  if position_disable_tag.is_some() {
    let mut vec = features.to_vec();
    if let Some(tag_bytes) = position_disable_tag {
      let tag = Tag::from_bytes(&tag_bytes);
      vec.retain(|f| f.tag != tag);
      vec.push(Feature {
        tag,
        value: 0,
        start: 0,
        end: u32::MAX,
      });
    }
    features = vec.into_boxed_slice().into();
  }

  let variations = crate::text::face_cache::with_face(&font, |face| {
    crate::text::variations::collect_variations_for_face(
      face,
      style,
      font.as_ref(),
      run_font_size,
      authored_variations,
    )
  })
  .unwrap_or_else(|| authored_variations.to_vec());

  let resolved_palette = resolve_font_palette_for_font(
    &style.font_palette,
    &style.font_palettes,
    &font.family,
    style.color,
    style.used_dark_color_scheme,
    style.forced_colors,
  );
  let palette_index = select_palette_index(&font, resolved_palette.base);
  let palette_overrides = Arc::new(resolved_palette.overrides);

  out.push(FontRun {
    text: segment_text.to_string(),
    start: run.start + start,
    end: run.start + end,
    font,
    synthetic_bold: run_synthetic_bold,
    synthetic_oblique,
    script: run.script,
    direction: run.direction,
    level: run.level,
    font_size: run_font_size,
    baseline_shift: run_baseline_shift,
    language,
    features,
    variations,
    palette_index,
    palette_overrides: Arc::clone(&palette_overrides),
    palette_override_hash: resolved_palette.override_hash,
    rotation: RunRotation::None,
    vertical: false,
  });
}

fn select_palette_index(font: &LoadedFont, base: FontPaletteBase) -> u16 {
  crate::text::face_cache::with_face(font, |face| select_cpal_palette(face, base)).unwrap_or(0)
}

// ============================================================================
// Text Shaping
// ============================================================================

/// Information about a single glyph.
#[derive(Debug, Clone, Copy)]
pub struct GlyphPosition {
  /// Glyph ID in the font.
  pub glyph_id: u32,
  /// Cluster index (maps to character position in original text).
  pub cluster: u32,
  /// X position relative to run start.
  pub x_offset: f32,
  /// Y offset from baseline.
  pub y_offset: f32,
  /// Horizontal advance (distance to next glyph).
  pub x_advance: f32,
  /// Vertical advance (usually 0 for horizontal text).
  pub y_advance: f32,
}

/// A shaped run of text, ready for rendering.
#[derive(Debug, Clone)]
pub struct ShapedRun {
  /// The original text.
  pub text: String,
  /// Start byte index in original text.
  pub start: usize,
  /// End byte index in original text.
  pub end: usize,
  /// Positioned glyphs.
  pub glyphs: Vec<GlyphPosition>,
  /// Text direction.
  pub direction: Direction,
  /// Bidi level.
  pub level: u8,
  /// Total advance of this run along its inline axis.
  pub advance: f32,
  /// Font used for this run.
  pub font: Arc<LoadedFont>,
  /// Font size in pixels.
  pub font_size: f32,
  /// Additional baseline shift in pixels (positive raises text).
  pub baseline_shift: f32,
  /// Language set on the shaping buffer (if provided).
  pub language: Option<HbLanguage>,
  /// OpenType features applied when shaping this run.
  pub features: Arc<[Feature]>,
  /// Synthetic bold stroke width in pixels (0 = none).
  pub synthetic_bold: f32,
  /// Synthetic oblique shear factor (tan(angle); 0 = none).
  pub synthetic_oblique: f32,
  /// Optional rotation to apply when painting.
  pub rotation: RunRotation,
  /// Whether this run uses vertical shaping metrics (inline advances stored in `y_advance`).
  pub vertical: bool,

  /// Palette index for color glyph rendering.
  pub palette_index: u16,
  /// Palette overrides for color glyph rendering.
  pub palette_overrides: Arc<Vec<(u16, Rgba)>>,
  /// Stable hash of palette overrides for cache keys.
  pub palette_override_hash: u64,

  /// Active variation settings for the run (used for cache keys).
  pub variations: Vec<Variation>,

  /// Optional additional scale factor (1.0 = none).
  pub scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRotation {
  None,
  Ccw90,
  Cw90,
}

impl ShapedRun {
  /// Returns true if this run is empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.glyphs.is_empty()
  }

  /// Returns the number of glyphs.
  #[inline]
  pub fn glyph_count(&self) -> usize {
    self.glyphs.len()
  }
}

fn mirror_text_for_direction<'a>(
  text: &'a str,
  direction: Direction,
) -> (Cow<'a, str>, Option<Vec<(usize, usize)>>) {
  if !direction.is_rtl() {
    return (Cow::Borrowed(text), None);
  }

  let mut mirrored = String::with_capacity(text.len());
  let mut mapping: Vec<(usize, usize)> = Vec::new();
  let mut changed = false;

  for (orig_idx, ch) in text.char_indices() {
    let mapped = get_mirrored(ch).unwrap_or(ch);
    changed |= mapped != ch;
    mapping.push((mirrored.len(), orig_idx));
    mirrored.push(mapped);
  }

  if changed {
    (Cow::Owned(mirrored), Some(mapping))
  } else {
    (Cow::Borrowed(text), None)
  }
}

fn map_cluster_offset(cluster: usize, mapping: &[(usize, usize)]) -> usize {
  match mapping.binary_search_by_key(&cluster, |(shaped, _)| *shaped) {
    Ok(idx) => mapping.get(idx).map(|(_, orig)| *orig).unwrap_or(cluster),
    Err(0) => mapping.first().map(|(_, orig)| *orig).unwrap_or(cluster),
    Err(idx) => mapping
      .get(idx.saturating_sub(1))
      .map(|(_, orig)| *orig)
      .unwrap_or(cluster),
  }
}

/// Converts HarfBuzz glyph positioning into engine coordinates.
///
/// Vertical runs keep HarfBuzz's cross-axis offset on `x_offset` and inline offset on
/// `y_offset`, applying `baseline_shift` along the cross axis.
fn map_hb_position(
  vertical: bool,
  baseline_shift: f32,
  scale: f32,
  pos: &rustybuzz::GlyphPosition,
) -> (f32, f32, f32, f32) {
  let mut inline_advance_raw = if vertical {
    pos.y_advance
  } else {
    pos.x_advance
  };
  let mut cross_advance_raw = if vertical {
    pos.x_advance
  } else {
    pos.y_advance
  };

  // Some fonts (notably bitmap color fonts) do not provide vertical advances via HarfBuzz and
  // instead populate `x_advance` even when shaping in vertical mode. When we fall back to using
  // `x_advance` as the inline advance, ensure we do not also treat it as cross-axis movement.
  if vertical && inline_advance_raw == 0 {
    inline_advance_raw = cross_advance_raw;
    cross_advance_raw = 0;
  }
  let inline_offset_raw = if vertical { pos.y_offset } else { pos.x_offset };
  let cross_offset_raw = if vertical { pos.x_offset } else { pos.y_offset };

  let mut inline_advance = inline_advance_raw as f32 * scale;
  if vertical {
    inline_advance = inline_advance.abs();
  }
  let cross_advance = cross_advance_raw as f32 * scale;

  if vertical {
    let x_offset = cross_offset_raw as f32 * scale + baseline_shift;
    let y_offset = inline_offset_raw as f32 * scale;
    (x_offset, y_offset, cross_advance, inline_advance)
  } else {
    let x_offset = inline_offset_raw as f32 * scale;
    let y_offset = cross_offset_raw as f32 * scale + baseline_shift;
    (x_offset, y_offset, inline_advance, cross_advance)
  }
}

pub(crate) fn notdef_advance_for_font(font: &LoadedFont, font_size: f32) -> f32 {
  let (units_per_em, notdef_advance) = crate::text::face_cache::with_face(font, |face| {
    (
      face.units_per_em() as f32,
      face
        .glyph_hor_advance(ttf_parser::GlyphId(0))
        .unwrap_or(face.units_per_em()) as f32,
    )
  })
  .unwrap_or((0.0, 0.0));

  let mut inline_advance = if units_per_em > 0.0 {
    notdef_advance * (font_size / units_per_em)
  } else {
    0.0
  };

  if !inline_advance.is_finite() || inline_advance <= 0.0 {
    inline_advance = font_size * 0.5;
  }

  inline_advance
}

fn fallback_notdef_advance(run: &FontRun) -> f32 {
  notdef_advance_for_font(
    &run.font,
    run.font_size * run.font.face_metrics_overrides.size_adjust,
  )
}

fn synthesize_notdef_run(run: &FontRun) -> ShapedRun {
  let run_scale = run.font.face_metrics_overrides.size_adjust;
  let baseline_shift = run.baseline_shift * run_scale;
  let inline_advance = fallback_notdef_advance(run);

  let mut glyphs = Vec::new();
  let mut advance = 0.0_f32;
  for (cluster, ch) in run.text.char_indices() {
    if is_bidi_format_char(ch) {
      continue;
    }
    let (x_offset, y_offset, x_advance, y_advance) = if run.vertical {
      (baseline_shift, 0.0, 0.0, inline_advance)
    } else {
      (0.0, baseline_shift, inline_advance, 0.0)
    };
    glyphs.push(GlyphPosition {
      glyph_id: 0,
      cluster: cluster as u32,
      x_offset,
      y_offset,
      x_advance,
      y_advance,
    });
    advance += if run.vertical { y_advance } else { x_advance };
  }

  ShapedRun {
    text: run.text.clone(),
    start: run.start,
    end: run.end,
    glyphs,
    direction: run.direction,
    level: run.level,
    advance,
    font: Arc::clone(&run.font),
    font_size: run.font_size,
    baseline_shift,
    language: run.language.clone(),
    features: Arc::clone(&run.features),
    synthetic_bold: run.synthetic_bold,
    synthetic_oblique: run.synthetic_oblique,
    rotation: run.rotation,
    vertical: run.vertical,
    palette_index: run.palette_index,
    palette_overrides: Arc::clone(&run.palette_overrides),
    palette_override_hash: run.palette_override_hash,
    variations: run.variations.clone(),
    scale: run_scale,
  }
}

thread_local! {
  static THREAD_UNICODE_BUFFER: RefCell<Option<UnicodeBuffer>> = RefCell::new(None);
}

fn take_unicode_buffer() -> UnicodeBuffer {
  THREAD_UNICODE_BUFFER.with(|cell| cell.borrow_mut().take().unwrap_or_else(UnicodeBuffer::new))
}

fn recycle_unicode_buffer(buffer: UnicodeBuffer) {
  THREAD_UNICODE_BUFFER.with(|cell| {
    *cell.borrow_mut() = Some(buffer);
  });
}

/// Shapes a single font run into positioned glyphs.
fn shape_font_run(run: &FontRun) -> Result<ShapedRun> {
  #[cfg(any(test, debug_assertions))]
  SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(calls.get() + 1));

  enum ShaperFace {
    Shared(Arc<rustybuzz::Face<'static>>),
    Owned(rustybuzz::Face<'static>),
  }

  impl ShaperFace {
    #[inline]
    fn as_face(&self) -> &rustybuzz::Face<'static> {
      match self {
        Self::Shared(face) => face.as_ref(),
        Self::Owned(face) => face,
      }
    }
  }

  // Create rustybuzz face from cached font data to avoid reparsing per run.
  let cached_face = crate::text::face_cache::get_rustybuzz_face(&run.font).ok_or_else(|| {
    TextError::ShapingFailed {
      text: run.text.clone(),
      reason: "Failed to create HarfBuzz face".to_string(),
    }
  })?;
  let rb_face = if run.variations.is_empty() {
    ShaperFace::Shared(cached_face.face())
  } else {
    let mut rb_face = cached_face.clone_face();
    rb_face.set_variations(&run.variations);
    ShaperFace::Owned(rb_face)
  };

  // Create Unicode buffer
  let mut buffer = take_unicode_buffer();

  // Bidi format characters are default-ignorable and must not affect shaping results (e.g.
  // kerning/ligature formation). Passing them through to HarfBuzz can disrupt
  // adjacency-dependent features even if we later drop the resulting glyphs, so strip them from
  // the shaping input and keep a mapping back to the original byte indices.
  let has_bidi_formats = run.text.chars().any(is_bidi_format_char);
  let (mut shape_text, mut cluster_map_override) =
    mirror_text_for_direction(&run.text, run.direction);
  if has_bidi_formats {
    let original_map = cluster_map_override.as_ref();
    let mut filtered = String::with_capacity(shape_text.len());
    let mut mapping: Vec<(usize, usize)> = Vec::new();

    for (idx, ch) in shape_text.char_indices() {
      if is_bidi_format_char(ch) {
        continue;
      }
      let orig_idx = original_map
        .map(|map| map_cluster_offset(idx, map))
        .unwrap_or(idx);
      mapping.push((filtered.len(), orig_idx));
      filtered.push(ch);
    }

    shape_text = Cow::Owned(filtered);
    cluster_map_override = Some(mapping);
  }
  buffer.push_str(&shape_text);

  let language = run.language.clone();

  // Set buffer properties
  let hb_direction = if run.vertical {
    HbDirection::TopToBottom
  } else {
    run.direction.to_harfbuzz()
  };
  buffer.set_direction(hb_direction);
  if let Some(script) = run.script.to_harfbuzz() {
    buffer.set_script(script);
  }
  if let Some(lang) = language.as_ref() {
    buffer.set_language(lang.clone());
  }

  let features = if run.vertical {
    let base = run.features.as_ref();
    let need_vert = !base.iter().any(|f| f.tag.to_bytes() == *b"vert");
    let need_vrt2 = !base.iter().any(|f| f.tag.to_bytes() == *b"vrt2");
    if need_vert || need_vrt2 {
      let mut features = base.to_vec();
      if need_vert {
        features.push(Feature {
          tag: Tag::from_bytes(b"vert"),
          value: 1,
          start: 0,
          end: u32::MAX,
        });
      }
      if need_vrt2 {
        features.push(Feature {
          tag: Tag::from_bytes(b"vrt2"),
          value: 1,
          start: 0,
          end: u32::MAX,
        });
      }
      Cow::Owned(features)
    } else {
      Cow::Borrowed(base)
    }
  } else {
    Cow::Borrowed(run.features.as_ref())
  };

  // Shape the text
  let output = rustybuzz::shape(rb_face.as_face(), features.as_ref(), buffer);
  let features_used: Arc<[Feature]> = match features {
    Cow::Borrowed(_) => Arc::clone(&run.features),
    Cow::Owned(vec) => vec.into_boxed_slice().into(),
  };

  // Calculate scale factor
  let units_per_em = rb_face.as_face().units_per_em() as f32;
  let run_scale = run.font.face_metrics_overrides.size_adjust;
  let effective_font_size = run.font_size * run_scale;
  let scale = effective_font_size / units_per_em;
  let baseline_shift = run.baseline_shift * run_scale;

  // Extract glyph information
  let glyph_infos = output.glyph_infos();
  let glyph_positions = output.glyph_positions();

  let mut glyphs = Vec::with_capacity(glyph_infos.len());
  let mut inline_position = 0.0_f32;
  let mark_only = is_mark_only_cluster(&run.text);
  let suppress_optional_mark_notdefs = if run.text.chars().any(is_unicode_mark) && !mark_only {
    let clusters = atomic_shaping_clusters(&run.text);
    let face = crate::text::face_cache::get_ttf_face(&run.font);
    let mut can_suppress = Vec::with_capacity(clusters.len());
    for (start, end) in &clusters {
      let cluster_text = &run.text[*start..*end];
      let mut saw_required_non_mark = false;
      let mut saw_mark = false;
      let mut required_non_marks_supported = true;
      let mut saw_unsupported_mark = false;

      for ch in cluster_text.chars() {
        if is_bidi_format_char(ch) || is_non_rendering_for_coverage(ch) {
          continue;
        }

        if is_unicode_mark(ch) {
          saw_mark = true;
          if let Some(face) = face.as_ref() {
            saw_unsupported_mark |= !face.has_glyph(ch);
          }
        } else {
          saw_required_non_mark = true;
          if let Some(face) = face.as_ref() {
            required_non_marks_supported &= face.has_glyph(ch);
          } else {
            required_non_marks_supported = false;
          }
        }
      }

      can_suppress.push(
        saw_mark && saw_required_non_mark && required_non_marks_supported && saw_unsupported_mark,
      );
    }

    Some((clusters, can_suppress))
  } else {
    None
  };

  for (info, pos) in glyph_infos.iter().zip(glyph_positions.iter()) {
    let cluster_in_shape = info.cluster as usize;
    let logical_cluster = cluster_map_override
      .as_ref()
      .map(|map| map_cluster_offset(cluster_in_shape, map))
      .unwrap_or(cluster_in_shape);

    if info.glyph_id == 0 {
      if let Some((clusters, can_suppress)) = suppress_optional_mark_notdefs.as_ref() {
        let cluster_idx = match clusters.binary_search_by_key(&logical_cluster, |(start, _)| *start)
        {
          Ok(idx) => idx,
          Err(0) => 0,
          Err(idx) => idx.saturating_sub(1),
        };
        if can_suppress.get(cluster_idx).copied().unwrap_or(false) {
          continue;
        }
      }
    }

    let (x_offset, y_offset, x_advance, y_advance) =
      map_hb_position(run.vertical, baseline_shift, scale, pos);

    glyphs.push(GlyphPosition {
      glyph_id: info.glyph_id,
      cluster: logical_cluster as u32,
      x_offset,
      y_offset,
      x_advance,
      y_advance,
    });
    inline_position += if run.vertical { y_advance } else { x_advance };
  }

  if inline_position.abs() <= f32::EPSILON && !glyphs.is_empty() && mark_only {
    // HarfBuzz gives combining marks zero advance because they normally attach to a base glyph.
    // When the entire run is marks (e.g. standalone Arabic diacritics), that would collapse the
    // run to zero width and allow higher-level layout to drop it entirely. Ensure mark-only runs
    // remain visible by assigning a fallback advance to the final glyph.
    let fallback_advance = fallback_notdef_advance(run);
    if run.vertical {
      if let Some(last) = glyphs.last_mut() {
        last.y_advance = fallback_advance;
      }
    } else if let Some(last) = glyphs.last_mut() {
      last.x_advance = fallback_advance;
    }
    inline_position = fallback_advance;
  }

  let shaped = ShapedRun {
    text: run.text.clone(),
    start: run.start,
    end: run.end,
    glyphs,
    direction: run.direction,
    level: run.level,
    advance: inline_position,
    font: Arc::clone(&run.font),
    font_size: run.font_size,
    baseline_shift,
    language,
    features: features_used,
    synthetic_bold: run.synthetic_bold,
    synthetic_oblique: run.synthetic_oblique,
    rotation: run.rotation,
    vertical: run.vertical,
    palette_index: run.palette_index,
    palette_overrides: Arc::clone(&run.palette_overrides),
    palette_override_hash: run.palette_override_hash,
    variations: run.variations.clone(),
    scale: run_scale,
  };

  recycle_unicode_buffer(output.clear());
  Ok(shaped)
}

// ============================================================================
// Shaping Pipeline
// ============================================================================

/// The main text shaping pipeline.
///
/// Coordinates bidi analysis, script itemization, font matching, and text shaping
/// into a unified process.
///
/// # Example
///
/// ```rust,ignore
/// let pipeline = ShapingPipeline::new();
/// let font_context = FontContext::new();
/// let style = ComputedStyle::default();
///
/// let runs = pipeline.shape("Hello, world!", &style, &font_context)?;
/// ```
#[derive(Debug, Clone)]
pub struct ShapingPipeline {
  cache: ShapingCache,
  font_cache: FallbackCache,
}

impl Default for ShapingPipeline {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
struct ShapingCacheKey {
  style_hash: u64,
  font_generation: u64,
  text_hash: u64,
  text_len: usize,
  bidi_signature: u32,
}

#[derive(Debug)]
struct ShapingCacheEntry {
  text: Arc<str>,
  runs: Arc<Vec<ShapedRun>>,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

pub(crate) fn shaping_style_hash(style: &ComputedStyle) -> u64 {
  #[cfg(test)]
  SHAPING_STYLE_HASH_CALLS.with(|calls| calls.set(calls.get() + 1));

  use std::hash::Hash;
  use std::hash::Hasher;
  let mut hasher = FxHasher::default();

  std::mem::discriminant(&style.direction).hash(&mut hasher);
  std::mem::discriminant(&style.unicode_bidi).hash(&mut hasher);
  std::mem::discriminant(&style.writing_mode).hash(&mut hasher);
  std::mem::discriminant(&style.text_orientation).hash(&mut hasher);
  std::mem::discriminant(&style.text_rendering).hash(&mut hasher);
  style.language.hash(&mut hasher);
  style.font_family.hash(&mut hasher);
  f32_to_canonical_bits(style.font_size).hash(&mut hasher);
  // `letter-spacing` does not affect the HarfBuzz shaping result directly (spacing is applied later
  // during inline layout). The only shaping-relevant distinction is "zero vs non-zero", because we
  // disable optional ligature features when spacing is enabled (see `collect_opentype_features`).
  //
  // Hashing the raw float bits here would create distinct shaping cache keys for e.g. `1px` vs
  // `2px`, even though the shaped glyph stream is identical. Collapse all non-zero values to
  // improve cache hit-rate and reduce cache growth.
  let has_letter_spacing = style.letter_spacing != 0.0;
  has_letter_spacing.hash(&mut hasher);
  style.font_weight.to_u16().hash(&mut hasher);
  f32_to_canonical_bits(style.font_stretch.to_percentage()).hash(&mut hasher);

  match style.font_style {
    CssFontStyle::Normal => 0u8.hash(&mut hasher),
    CssFontStyle::Italic => 1u8.hash(&mut hasher),
    CssFontStyle::Oblique(angle) => {
      2u8.hash(&mut hasher);
      f32_to_canonical_bits(angle.unwrap_or_default()).hash(&mut hasher);
    }
  }

  // Palette overrides can resolve `currentColor` and `light-dark()`, so cache entries must vary
  // with text color RGB and the element's used color scheme.
  style.color.r.hash(&mut hasher);
  style.color.g.hash(&mut hasher);
  style.color.b.hash(&mut hasher);
  style.used_dark_color_scheme.hash(&mut hasher);

  match &style.font_palette {
    FontPalette::Normal => 0u8.hash(&mut hasher),
    FontPalette::Light => 1u8.hash(&mut hasher),
    FontPalette::Dark => 2u8.hash(&mut hasher),
    FontPalette::Named(name) => {
      3u8.hash(&mut hasher);
      name.hash(&mut hasher);
    }
  }
  (Arc::as_ptr(&style.font_palettes) as usize).hash(&mut hasher);
  (Arc::as_ptr(&style.font_feature_values) as usize).hash(&mut hasher);

  std::mem::discriminant(&style.font_variant).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_caps).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_emoji).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_position).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_numeric.figure).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_numeric.spacing).hash(&mut hasher);
  std::mem::discriminant(&style.font_variant_numeric.fraction).hash(&mut hasher);
  (style.font_variant_numeric.ordinal as u8).hash(&mut hasher);
  (style.font_variant_numeric.slashed_zero as u8).hash(&mut hasher);

  (style.font_variant_ligatures.common as u8).hash(&mut hasher);
  (style.font_variant_ligatures.discretionary as u8).hash(&mut hasher);
  (style.font_variant_ligatures.historical as u8).hash(&mut hasher);
  (style.font_variant_ligatures.contextual as u8).hash(&mut hasher);

  style
    .font_variant_alternates
    .historical_forms
    .hash(&mut hasher);
  style.font_variant_alternates.stylistic.hash(&mut hasher);
  style.font_variant_alternates.stylesets.hash(&mut hasher);
  style
    .font_variant_alternates
    .character_variants
    .hash(&mut hasher);
  style.font_variant_alternates.swash.hash(&mut hasher);
  style.font_variant_alternates.ornaments.hash(&mut hasher);
  style.font_variant_alternates.annotation.hash(&mut hasher);

  match style.font_variant_east_asian.variant {
    Some(var) => {
      1u8.hash(&mut hasher);
      std::mem::discriminant(&var).hash(&mut hasher);
    }
    None => 0u8.hash(&mut hasher),
  }
  match style.font_variant_east_asian.width {
    Some(width) => {
      1u8.hash(&mut hasher);
      std::mem::discriminant(&width).hash(&mut hasher);
    }
    None => 0u8.hash(&mut hasher),
  }
  (style.font_variant_east_asian.ruby as u8).hash(&mut hasher);

  std::mem::discriminant(&style.font_kerning).hash(&mut hasher);
  (style.font_synthesis.weight as u8).hash(&mut hasher);
  (style.font_synthesis.style as u8).hash(&mut hasher);
  (style.font_synthesis.small_caps as u8).hash(&mut hasher);
  (style.font_synthesis.position as u8).hash(&mut hasher);
  std::mem::discriminant(&style.font_optical_sizing).hash(&mut hasher);

  match style.font_language_override.clone() {
    FontLanguageOverride::Normal => 0u8.hash(&mut hasher),
    FontLanguageOverride::Override(tag) => {
      1u8.hash(&mut hasher);
      tag.hash(&mut hasher);
    }
  }

  match style.font_size_adjust {
    FontSizeAdjust::None => 0u8.hash(&mut hasher),
    FontSizeAdjust::Number { ratio, metric } => {
      1u8.hash(&mut hasher);
      f32_to_canonical_bits(ratio).hash(&mut hasher);
      std::mem::discriminant(&metric).hash(&mut hasher);
    }
    FontSizeAdjust::FromFont { metric } => {
      2u8.hash(&mut hasher);
      std::mem::discriminant(&metric).hash(&mut hasher);
    }
  }

  for setting in style.font_feature_settings.iter() {
    setting.tag.hash(&mut hasher);
    setting.value.hash(&mut hasher);
  }
  for setting in style.font_variation_settings.iter() {
    setting.tag.hash(&mut hasher);
    f32_to_canonical_bits(setting.value).hash(&mut hasher);
  }

  hasher.finish()
}

#[cfg(test)]
#[doc(hidden)]
pub(crate) fn debug_reset_style_hash_calls() {
  SHAPING_STYLE_HASH_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
#[doc(hidden)]
pub(crate) fn debug_style_hash_calls() -> usize {
  SHAPING_STYLE_HASH_CALLS.with(|calls| calls.get())
}

fn shaping_text_hash(text: &str) -> u64 {
  use std::hash::Hash;
  use std::hash::Hasher;

  let mut hasher = FxHasher::default();
  text.hash(&mut hasher);
  hasher.finish()
}

fn shaping_bidi_signature(
  base_direction: Direction,
  explicit_bidi: Option<ExplicitBidiContext>,
) -> u32 {
  let mut sig = 0u32;
  if base_direction.is_rtl() {
    sig |= 1;
  }
  if let Some(ctx) = explicit_bidi {
    sig |= 1 << 1;
    sig |= (ctx.level.number() as u32) << 2;
    if ctx.override_all {
      sig |= 1 << 10;
    }
  }
  sig
}

#[derive(Default, Debug)]
struct ShapingCacheStats {
  hits: AtomicU64,
  misses: AtomicU64,
  evictions: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ShapingCacheStatsSnapshot {
  hits: u64,
  misses: u64,
  evictions: u64,
  entries: usize,
}

impl ShapingCacheStats {
  fn snapshot(&self, entries: usize) -> ShapingCacheStatsSnapshot {
    ShapingCacheStatsSnapshot {
      hits: self.hits.load(Ordering::Relaxed),
      misses: self.misses.load(Ordering::Relaxed),
      evictions: self.evictions.load(Ordering::Relaxed),
      entries,
    }
  }

  fn clear(&self) {
    self.hits.store(0, Ordering::Relaxed);
    self.misses.store(0, Ordering::Relaxed);
    self.evictions.store(0, Ordering::Relaxed);
  }
}

#[derive(Debug)]
struct ShapingCacheInner {
  shards:
    Vec<parking_lot::Mutex<LruCache<ShapingCacheKey, Vec<ShapingCacheEntry>, ShapingCacheHasher>>>,
  shard_mask: usize,
  entries: AtomicUsize,
  stats: ShapingCacheStats,
}

#[derive(Clone, Debug)]
struct ShapingCache {
  inner: Arc<ShapingCacheInner>,
}

impl ShapingCache {
  fn new(capacity: usize) -> Self {
    const SHARD_TARGET: usize = 64;
    const SHARD_LIMIT: usize = 16;

    let capacity = capacity.max(1);
    let desired_shards = (capacity / SHARD_TARGET).clamp(1, SHARD_LIMIT);
    let shard_count = if desired_shards.is_power_of_two() {
      desired_shards
    } else {
      1usize << (usize::BITS - 1 - desired_shards.leading_zeros()) as usize
    };
    let shard_count = shard_count.max(1);
    let shard_mask = shard_count - 1;

    let base = capacity / shard_count;
    let rem = capacity % shard_count;
    let mut shards = Vec::with_capacity(shard_count);
    for idx in 0..shard_count {
      let shard_cap = base + usize::from(idx < rem);
      let cap = NonZeroUsize::new(shard_cap.max(1)).unwrap_or(NonZeroUsize::MIN);
      shards.push(parking_lot::Mutex::new(LruCache::with_hasher(
        cap,
        ShapingCacheHasher::default(),
      )));
    }

    Self {
      inner: Arc::new(ShapingCacheInner {
        shards,
        shard_mask,
        entries: AtomicUsize::new(0),
        stats: ShapingCacheStats::default(),
      }),
    }
  }

  #[inline]
  fn shard_index(&self, key: &ShapingCacheKey) -> usize {
    use std::hash::Hash;
    use std::hash::Hasher;

    let mut hasher = FxHasher::default();
    key.hash(&mut hasher);
    (hasher.finish() as usize) & self.inner.shard_mask
  }

  fn get(&self, key: &ShapingCacheKey, text: &str) -> Option<Arc<Vec<ShapedRun>>> {
    let shard_idx = self.shard_index(key);
    let result = {
      let mut cache = self.inner.shards[shard_idx].lock();
      cache.get(key).and_then(|bucket| {
        bucket
          .iter()
          .find(|entry| entry.text.as_ref() == text)
          .map(|entry| Arc::clone(&entry.runs))
      })
    };
    let diag_enabled = text_diagnostics_enabled();

    if result.is_some() {
      self.inner.stats.hits.fetch_add(1, Ordering::Relaxed);
      if diag_enabled {
        SHAPING_CACHE_DIAG_HITS.fetch_add(1, Ordering::Relaxed);
      }
    } else {
      self.inner.stats.misses.fetch_add(1, Ordering::Relaxed);
      if diag_enabled {
        SHAPING_CACHE_DIAG_MISSES.fetch_add(1, Ordering::Relaxed);
      }
    }

    if diag_enabled {
      record_shaping_cache_diag_entries(self.len());
    }

    result
  }

  fn insert(
    &self,
    key: ShapingCacheKey,
    text: Arc<str>,
    runs: Arc<Vec<ShapedRun>>,
  ) -> Arc<Vec<ShapedRun>> {
    let shard_idx = self.shard_index(&key);
    let mut cached = Arc::clone(&runs);
    let mut inserted_new_bucket = false;
    let mut evicted = false;

    {
      let mut cache = self.inner.shards[shard_idx].lock();
      if let Some(bucket) = cache.get_mut(&key) {
        if let Some(existing) = bucket
          .iter()
          .find(|entry| entry.text.as_ref() == text.as_ref())
        {
          cached = Arc::clone(&existing.runs);
        } else {
          if bucket.len() >= SHAPING_CACHE_HASH_COLLISION_BUCKET_LIMIT {
            bucket.remove(0);
          }
          bucket.push(ShapingCacheEntry {
            text: Arc::clone(&text),
            runs: Arc::clone(&runs),
          });
        }
      } else {
        let at_capacity = cache.len() >= cache.cap().get();
        cache.put(
          key,
          vec![ShapingCacheEntry {
            text,
            runs: Arc::clone(&runs),
          }],
        );
        inserted_new_bucket = true;
        evicted = at_capacity;
      }
    }

    let diag_enabled = text_diagnostics_enabled();
    if inserted_new_bucket {
      if evicted {
        self.inner.stats.evictions.fetch_add(1, Ordering::Relaxed);
        if diag_enabled {
          SHAPING_CACHE_DIAG_EVICTIONS.fetch_add(1, Ordering::Relaxed);
        }
      } else {
        self.inner.entries.fetch_add(1, Ordering::Relaxed);
      }
    }

    if diag_enabled {
      record_shaping_cache_diag_entries(self.len());
    }

    cached
  }

  fn clear(&self) {
    for shard in &self.inner.shards {
      shard.lock().clear();
    }
    self.inner.entries.store(0, Ordering::Relaxed);
    self.inner.stats.clear();
  }

  fn len(&self) -> usize {
    self.inner.entries.load(Ordering::Relaxed)
  }

  fn stats(&self) -> ShapingCacheStatsSnapshot {
    self.inner.stats.snapshot(self.len())
  }
}

fn shaping_cache_capacity_from_env() -> usize {
  static CAPACITY: OnceLock<usize> = OnceLock::new();
  *CAPACITY.get_or_init(|| {
    std::env::var("FASTR_TEXT_SHAPING_CACHE_CAPACITY")
      .ok()
      .and_then(|value| value.parse::<usize>().ok())
      .filter(|value| *value > 0)
      .unwrap_or(SHAPING_CACHE_CAPACITY)
  })
}

impl ShapingPipeline {
  /// Creates a new shaping pipeline.
  pub fn new() -> Self {
    #[cfg(any(test, debug_assertions))]
    SHAPING_PIPELINE_NEW_CALLS.with(|calls| calls.set(calls.get() + 1));
    Self {
      cache: ShapingCache::new(shaping_cache_capacity_from_env()),
      font_cache: FallbackCache::new(FONT_RESOLUTION_CACHE_SIZE),
    }
  }

  #[cfg(any(test, debug_assertions))]
  #[doc(hidden)]
  pub fn debug_new_call_count() -> usize {
    SHAPING_PIPELINE_NEW_CALLS.with(|calls| calls.get())
  }

  #[cfg(any(test, debug_assertions))]
  #[doc(hidden)]
  pub fn debug_reset_new_call_count() {
    SHAPING_PIPELINE_NEW_CALLS.with(|calls| calls.set(0));
  }

  #[cfg(test)]
  #[doc(hidden)]
  pub fn debug_full_text_arc_alloc_count() -> usize {
    SHAPE_FULL_TEXT_ARC_ALLOCS.with(|calls| calls.get())
  }

  #[cfg(test)]
  #[doc(hidden)]
  pub fn debug_reset_full_text_arc_alloc_count() {
    SHAPE_FULL_TEXT_ARC_ALLOCS.with(|calls| calls.set(0));
  }

  #[cfg(test)]
  fn with_cache_capacity_for_test(capacity: usize) -> Self {
    Self {
      cache: ShapingCache::new(capacity),
      font_cache: FallbackCache::new(FONT_RESOLUTION_CACHE_SIZE),
    }
  }

  /// Clears the shaping cache. Useful when reusing a pipeline across multiple documents.
  pub fn clear_cache(&self) {
    self.cache.clear();
    self.font_cache.clear();
  }

  #[cfg(any(test, debug_assertions))]
  fn cache_stats(&self) -> ShapingCacheStatsSnapshot {
    self.cache.stats()
  }

  #[cfg(any(test, debug_assertions))]
  fn fallback_cache_stats(&self) -> FallbackCacheStatsSnapshot {
    self.font_cache.stats()
  }

  /// Returns the number of cached shaped runs currently stored.
  pub fn cache_len(&self) -> usize {
    self.cache.len()
  }

  /// Shapes text into positioned glyphs.
  ///
  /// This is the main entry point for text shaping. It performs:
  /// 1. Bidi analysis
  /// 2. Script itemization
  /// 3. Font matching
  /// 4. Text shaping
  ///
  /// # Arguments
  ///
  /// * `text` - The text to shape
  /// * `style` - Computed style containing font properties
  /// * `font_context` - Font context for font resolution
  ///
  /// # Returns
  ///
  /// A vector of shaped runs, ready for rendering.
  ///
  /// # Errors
  ///
  /// Returns an error if font matching or shaping fails.
  pub fn shape(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
  ) -> Result<Vec<ShapedRun>> {
    Ok(self.shape_arc(text, style, font_context)?.as_ref().clone())
  }

  /// Shapes text into positioned glyphs and returns an `Arc` to the cached runs.
  ///
  /// This is equivalent to [`Self::shape`], but avoids cloning on shaping-cache hits.
  pub fn shape_arc(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    self.shape_core(text, style, font_context, None, None)
  }

  fn shape_core_vec(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
  ) -> Result<Vec<ShapedRun>> {
    Ok(
      self
        .shape_core(text, style, font_context, base_direction, explicit_bidi)?
        .as_ref()
        .clone(),
    )
  }

  fn shape_core(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    self.shape_core_impl(
      text,
      style,
      font_context,
      base_direction,
      explicit_bidi,
      None,
    )
  }

  fn shape_core_with_style_hash(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
    style_hash: u64,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    self.shape_core_impl(
      text,
      style,
      font_context,
      base_direction,
      explicit_bidi,
      Some(style_hash),
    )
  }

  fn shape_core_impl(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
    style_hash: Option<u64>,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    // Handle empty text
    if text.is_empty() {
      return Ok(Arc::new(Vec::new()));
    }

    let diag_enabled = text_diagnostics_enabled();

    if matches!(style.font_variant, FontVariant::SmallCaps)
      || matches!(
        style.font_variant_caps,
        FontVariantCaps::SmallCaps | FontVariantCaps::AllSmallCaps
      )
    {
      if !has_native_small_caps(style, font_context) && style.font_synthesis.small_caps {
        return Ok(Arc::new(self.shape_small_caps(
          text,
          style,
          font_context,
          base_direction,
          explicit_bidi,
        )?));
      }
    }

    let style_hash = style_hash.unwrap_or_else(|| shaping_style_hash(style));
    let font_generation = font_context.font_generation();
    let text_hash = shaping_text_hash(text);

    // Step 1: Bidi analysis inputs (needed for cache key + analysis).
    let mut resolved_base_dir = base_direction.unwrap_or(match style.direction {
      crate::style::types::Direction::Ltr => Direction::LeftToRight,
      crate::style::types::Direction::Rtl => Direction::RightToLeft,
    });
    let mut bidi_context = explicit_bidi;

    // text-orientation: upright sets the used direction to ltr and treats all characters as
    // strong LTR for bidi reordering in vertical typographic modes per CSS Writing Modes.
    if is_vertical_typographic_mode(style.writing_mode)
      && matches!(
        style.text_orientation,
        crate::style::types::TextOrientation::Upright
      )
    {
      resolved_base_dir = Direction::LeftToRight;
      let level = Level::ltr();
      let mut ctx = bidi_context.unwrap_or(ExplicitBidiContext {
        level,
        override_all: false,
      });
      ctx.level = level;
      ctx.override_all = true;
      bidi_context = Some(ctx);
    }

    let cache_key = ShapingCacheKey {
      style_hash,
      font_generation,
      text_hash,
      text_len: text.len(),
      bidi_signature: shaping_bidi_signature(resolved_base_dir, bidi_context),
    };
    if let Some(cached) = self.cache.get(&cache_key, text) {
      if diag_enabled {
        let glyphs: usize = cached.iter().map(|run| run.glyphs.len()).sum();
        record_text_shape(None, cached.len(), glyphs);
        record_font_face_override_usage(cached.as_ref());
      }
      return Ok(cached);
    }

    let cache_stats_before = diag_enabled.then(|| self.font_cache.stats());

    let bidi = BidiAnalysis::analyze_with_base(text, style, resolved_base_dir, bidi_context);

    // Step 2: Script itemization
    let itemized_runs = itemize_text(text, &bidi);

    // Step 3: Font matching
    let coverage_timer = text_diagnostics_timer(TextDiagnosticsStage::Coverage);
    let font_runs = assign_fonts_internal(
      &itemized_runs,
      style,
      font_context,
      Some(&self.font_cache),
      font_generation,
      true,
    )?;
    record_text_coverage(coverage_timer);

    // Step 4: Shape each run, applying vertical text-orientation when needed.
    let mut font_runs = font_runs;
    if is_vertical_typographic_mode(style.writing_mode) {
      font_runs = apply_vertical_text_orientation(font_runs, style.text_orientation);
    } else if is_sideways_writing_mode(style.writing_mode) {
      font_runs = apply_sideways_text_orientation(font_runs, style.writing_mode);
    }

    let shape_timer = text_diagnostics_timer(TextDiagnosticsStage::Shape);
    let mut shaped_runs = Vec::with_capacity(font_runs.len());
    for run in &font_runs {
      let mut shaped = match shape_font_run(run) {
        Ok(shaped) => shaped,
        Err(err) => {
          if text_diagnostics_verbose_logging() {
            let idx = SHAPING_FALLBACK_LOGGED.fetch_add(1, Ordering::Relaxed);
            if idx < LAST_RESORT_SAMPLE_LIMIT {
              eprintln!(
                "FASTR_TEXT_DIAGNOSTICS: shaping failed for run {}; using notdef placeholders: {}",
                format_codepoints_for_log(&run.text),
                err
              );
            }
          }
          synthesize_notdef_run(run)
        }
      };
      shaped.rotation = run.rotation;
      shaped.vertical = run.vertical;
      shaped_runs.push(shaped);
    }

    // Step 5: Reorder for bidi if needed
    if bidi.needs_reordering() {
      reorder_runs(&mut shaped_runs, bidi.paragraphs());
    }

    let shaped_runs = Arc::new(shaped_runs);
    let shaped_runs = self
      .cache
      // Reuse the `Arc<str>` allocated by bidi analysis so a single cache miss does not clone the
      // full text multiple times (see `SHAPE_FULL_TEXT_ARC_ALLOCS` test guardrail).
      .insert(cache_key, Arc::clone(&bidi.text), Arc::clone(&shaped_runs));

    let glyphs = shape_timer
      .as_ref()
      .map(|_| shaped_runs.iter().map(|run| run.glyphs.len()).sum())
      .unwrap_or(0);
    record_text_shape(shape_timer, shaped_runs.len(), glyphs);
    if diag_enabled {
      record_font_face_override_usage(shaped_runs.as_ref());
    }
    if let (Some(before), Some(after)) = (
      cache_stats_before,
      diag_enabled.then(|| self.font_cache.stats()),
    ) {
      record_fallback_cache_stats_delta(before, after);
    }

    Ok(shaped_runs)
  }

  /// Shapes text with explicit direction override.
  ///
  /// Useful when the CSS direction property is known.
  pub fn shape_with_direction_arc(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Direction,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    self.shape_core(text, style, font_context, Some(base_direction), None)
  }

  pub fn shape_with_direction(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Direction,
  ) -> Result<Vec<ShapedRun>> {
    Ok(
      self
        .shape_with_direction_arc(text, style, font_context, base_direction)?
        .as_ref()
        .clone(),
    )
  }

  /// Shapes text with an explicit bidi context (embedding level/override).
  pub fn shape_with_context_arc(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Direction,
    bidi_context: Option<ExplicitBidiContext>,
  ) -> Result<Arc<Vec<ShapedRun>>> {
    self.shape_core(
      text,
      style,
      font_context,
      Some(base_direction),
      bidi_context,
    )
  }

  pub fn shape_with_context(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Direction,
    bidi_context: Option<ExplicitBidiContext>,
  ) -> Result<Vec<ShapedRun>> {
    Ok(
      self
        .shape_with_context_arc(text, style, font_context, base_direction, bidi_context)?
        .as_ref()
        .clone(),
    )
  }

  /// Shapes text with an explicit bidi context (embedding level/override), using a precomputed
  /// style hash.
  ///
  /// This is equivalent to [`Self::shape_with_context`], but avoids recomputing
  /// [`shaping_style_hash`] when callers already needed the hash for their own cache keys.
  pub fn shape_with_context_hashed(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Direction,
    bidi_context: Option<ExplicitBidiContext>,
    style_hash: u64,
  ) -> Result<Vec<ShapedRun>> {
    Ok(
      self
        .shape_core_with_style_hash(
          text,
          style,
          font_context,
          Some(base_direction),
          bidi_context,
          style_hash,
        )?
        .as_ref()
        .clone(),
    )
  }

  /// Shapes text using a precomputed style hash.
  ///
  /// Equivalent to [`Self::shape`], but avoids recomputing [`shaping_style_hash`].
  pub fn shape_hashed(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    style_hash: u64,
  ) -> Result<Vec<ShapedRun>> {
    Ok(
      self
        .shape_core_with_style_hash(text, style, font_context, None, None, style_hash)?
        .as_ref()
        .clone(),
    )
  }

  /// Measures the total advance width of shaped text.
  ///
  /// Convenience method that shapes text and returns only the width.
  pub fn measure_width(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
  ) -> Result<f32> {
    let runs = self.shape_arc(text, style, font_context)?;
    // Shaping backends may express inline advances with negative values for RTL runs (e.g.
    // HarfBuzz uses a leftward pen direction). Callers expect a physical width, so sum the
    // magnitude of each run's advance.
    Ok(runs.iter().map(|r| r.advance.abs()).sum())
  }

  fn shape_small_caps(
    &self,
    text: &str,
    style: &ComputedStyle,
    font_context: &FontContext,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
  ) -> Result<Vec<ShapedRun>> {
    const SMALL_CAPS_SCALE: f32 = 0.8;

    let mut runs = Vec::new();
    let mut segment_start: usize = 0;
    let mut buffer = String::new();
    let mut mapping: Vec<(usize, usize)> = Vec::new();
    let mut current_small = None;
    let all_small = matches!(style.font_variant_caps, FontVariantCaps::AllSmallCaps);

    for (idx, ch) in text.char_indices() {
      let mut is_small = ch.is_lowercase() || (all_small && ch.is_uppercase());
      if is_unicode_mark(ch) {
        if let Some(flag) = current_small {
          is_small = flag;
        }
      }
      if let Some(flag) = current_small {
        if flag != is_small {
          let Some(segment_original) = text.get(segment_start..idx) else {
            return Err(
              TextError::ShapingFailed {
                text: text.to_string(),
                reason: "Invalid UTF-8 boundary for small-caps segment".to_string(),
              }
              .into(),
            );
          };
          self.flush_small_caps_segment(
            &mut runs,
            segment_original,
            &buffer,
            &mapping,
            segment_start,
            idx,
            flag,
            style,
            font_context,
            SMALL_CAPS_SCALE,
            base_direction,
            explicit_bidi,
          )?;
          buffer.clear();
          mapping.clear();
          segment_start = idx;
          current_small = Some(is_small);
        }
      } else {
        current_small = Some(is_small);
      }

      let original_offset = idx.saturating_sub(segment_start);
      if is_small {
        for up in ch.to_uppercase() {
          mapping.push((buffer.len(), original_offset));
          buffer.push(up);
        }
      } else {
        mapping.push((buffer.len(), original_offset));
        buffer.push(ch);
      }
    }

    if !buffer.is_empty() {
      let Some(segment_original) = text.get(segment_start..) else {
        return Err(
          TextError::ShapingFailed {
            text: text.to_string(),
            reason: "Invalid UTF-8 boundary for small-caps segment".to_string(),
          }
          .into(),
        );
      };
      self.flush_small_caps_segment(
        &mut runs,
        segment_original,
        &buffer,
        &mapping,
        segment_start,
        text.len(),
        current_small.unwrap_or(false),
        style,
        font_context,
        SMALL_CAPS_SCALE,
        base_direction,
        explicit_bidi,
      )?;
    }

    Ok(runs)
  }

  fn flush_small_caps_segment(
    &self,
    out: &mut Vec<ShapedRun>,
    original_text: &str,
    segment_text: &str,
    mapping: &[(usize, usize)],
    base_offset: usize,
    segment_end: usize,
    is_small: bool,
    style: &ComputedStyle,
    font_context: &FontContext,
    scale: f32,
    base_direction: Option<Direction>,
    explicit_bidi: Option<ExplicitBidiContext>,
  ) -> Result<()> {
    let mut seg_style = style.clone();
    seg_style.font_variant = FontVariant::Normal;
    seg_style.font_variant_caps = FontVariantCaps::Normal;
    if is_small {
      seg_style.font_size *= scale;
    }
    let mut shaped = self.shape_core_vec(
      segment_text,
      &seg_style,
      font_context,
      base_direction,
      explicit_bidi,
    )?;

    let segment_len = original_text.len();
    let mut mapping_with_end = Vec::with_capacity(mapping.len().saturating_add(1));
    mapping_with_end.extend_from_slice(mapping);
    mapping_with_end.push((segment_text.len(), segment_len));

    let map_boundary = |offset: usize| -> usize {
      match mapping_with_end.binary_search_by_key(&offset, |(shaped, _)| *shaped) {
        Ok(idx) => mapping_with_end
          .get(idx)
          .map(|(_, orig)| *orig)
          .unwrap_or(offset),
        Err(0) => mapping_with_end.first().map(|(_, orig)| *orig).unwrap_or(0),
        Err(idx) => mapping_with_end
          .get(idx.saturating_sub(1))
          .map(|(_, orig)| *orig)
          .unwrap_or(segment_len),
      }
    };

    // Like `map_boundary`, but maps an *exclusive* shaped boundary to the end of the last original
    // character that contributes to `[0, shaped_end)`.
    //
    // This matters when a single original character expands to multiple codepoints via
    // `to_uppercase()` (e.g. U+FB03 "ﬃ" → "FFI"). Font fallback can legitimately split the
    // expanded string mid-expansion, and we still need to map every shaped run back to a non-empty
    // slice of the original text so cluster indices remain valid.
    let map_boundary_end = |shaped_end: usize| -> usize {
      if shaped_end == 0 {
        return 0;
      }

      let idx = match mapping_with_end.binary_search_by_key(&shaped_end, |(shaped, _)| *shaped) {
        Ok(idx) | Err(idx) => idx,
      };

      let Some((_, last_orig_start)) = mapping_with_end.get(idx.saturating_sub(1)) else {
        return 0;
      };

      for (_, next_orig) in mapping_with_end.iter().skip(idx) {
        if next_orig != last_orig_start {
          return *next_orig;
        }
      }

      segment_len
    };

    for run in &mut shaped {
      let shaped_start = run.start;
      let shaped_end = run.end;
      let orig_start = map_boundary(shaped_start).min(segment_len);
      let orig_end = map_boundary_end(shaped_end).min(segment_len);

      run.start = base_offset.saturating_add(orig_start);
      run.end = base_offset.saturating_add(orig_end).min(segment_end);

      let Some(run_text) = original_text.get(orig_start..orig_end) else {
        return Err(
          TextError::ShapingFailed {
            text: original_text.to_string(),
            reason: "Invalid UTF-8 boundary for synthetic small-caps segment".to_string(),
          }
          .into(),
        );
      };
      run.text = run_text.to_string();

      for glyph in &mut run.glyphs {
        let cluster_in_run = glyph.cluster as usize;
        let cluster_in_segment = shaped_start.saturating_add(cluster_in_run);
        let orig_cluster =
          map_cluster_offset(cluster_in_segment, &mapping_with_end).saturating_sub(orig_start);
        glyph.cluster = orig_cluster as u32;
      }
    }
    out.extend(shaped);
    Ok(())
  }
}

/// Reorders shaped runs for bidi display.
///
/// Implements visual reordering based on bidi levels.
fn reorder_runs(runs: &mut [ShapedRun], paragraphs: &[ParagraphBoundary]) {
  if runs.is_empty() {
    return;
  }

  fn reorder_slice(slice: &mut [ShapedRun]) {
    if slice.is_empty() {
      return;
    }

    let max_level = slice.iter().map(|r| r.level).max().unwrap_or(0);
    for level in (1..=max_level).rev() {
      let mut start: Option<usize> = None;
      for i in 0..slice.len() {
        if slice[i].level >= level {
          start.get_or_insert(i);
        } else if let Some(s) = start {
          slice[s..i].reverse();
          start = None;
        }
      }
      if let Some(s) = start {
        slice[s..].reverse();
      }
    }
  }

  if paragraphs.is_empty() {
    reorder_slice(runs);
    return;
  }

  let mut idx = 0;
  for para in paragraphs {
    while idx < runs.len() && runs[idx].end <= para.start_byte {
      idx += 1;
    }
    let mut end = idx;
    while end < runs.len() && runs[end].start < para.end_byte {
      end += 1;
    }
    if idx < end {
      reorder_slice(&mut runs[idx..end]);
    }
    idx = end;
  }

  if idx < runs.len() {
    reorder_slice(&mut runs[idx..]);
  }
}

// ============================================================================
// Cluster Mapping
// ============================================================================

#[cfg(test)]
static CLUSTER_MAP_CHAR_ITERATIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[inline]
fn reset_cluster_map_char_iterations() {
  CLUSTER_MAP_CHAR_ITERATIONS.store(0, Ordering::Relaxed);
}

#[cfg(not(test))]
#[inline]
fn reset_cluster_map_char_iterations() {}

#[cfg(test)]
#[inline]
fn record_cluster_map_char_iteration() {
  CLUSTER_MAP_CHAR_ITERATIONS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
#[inline]
fn record_cluster_map_char_iteration() {}

#[cfg(test)]
#[inline]
fn cluster_map_char_iterations() -> usize {
  CLUSTER_MAP_CHAR_ITERATIONS.load(Ordering::Relaxed)
}

/// Maps between character positions and glyph positions.
///
/// Needed for hit testing, selection, and cursor positioning.
#[derive(Debug, Clone)]
pub struct ClusterMap {
  /// Maps character index to glyph index.
  char_to_glyph: Vec<usize>,
  /// Maps glyph index to character index.
  glyph_to_char: Vec<usize>,
}

impl ClusterMap {
  /// Builds a cluster map from a shaped run.
  pub fn from_shaped_run(run: &ShapedRun) -> Self {
    reset_cluster_map_char_iterations();

    let mut char_offsets = Vec::new();
    for (byte_idx, _) in run.text.char_indices() {
      record_cluster_map_char_iteration();
      if char_offsets.len() == char_offsets.capacity() {
        let additional = char_offsets.len().max(1);
        if char_offsets.try_reserve(additional).is_err() {
          return Self {
            char_to_glyph: Vec::new(),
            glyph_to_char: Vec::new(),
          };
        }
      }
      char_offsets.push(byte_idx);
    }

    let char_count = char_offsets.len();
    let glyph_count = run.glyphs.len();

    if char_count == 0 || glyph_count == 0 {
      return Self {
        char_to_glyph: Vec::new(),
        glyph_to_char: Vec::new(),
      };
    }

    let mut char_to_glyph = Vec::new();
    if char_to_glyph.try_reserve_exact(char_count).is_err() {
      return Self {
        char_to_glyph: Vec::new(),
        glyph_to_char: Vec::new(),
      };
    }
    char_to_glyph.resize(char_count, 0);

    let mut glyph_to_char = Vec::new();
    if glyph_to_char.try_reserve_exact(glyph_count).is_err() {
      return Self {
        char_to_glyph: Vec::new(),
        glyph_to_char: Vec::new(),
      };
    }
    glyph_to_char.resize(glyph_count, 0);

    // Build glyph to char mapping from cluster info
    for (glyph_idx, glyph) in run.glyphs.iter().enumerate() {
      let cluster = glyph.cluster as usize;
      let char_idx = char_offsets.partition_point(|&offset| offset < cluster);
      glyph_to_char[glyph_idx] = char_idx.min(char_count.saturating_sub(1));
    }

    // Build char to glyph mapping
    for (glyph_idx, &char_idx) in glyph_to_char.iter().enumerate() {
      if char_idx < char_count {
        char_to_glyph[char_idx] = glyph_idx;
      }
    }

    Self {
      char_to_glyph,
      glyph_to_char,
    }
  }

  /// Gets the glyph index for a character index.
  pub fn glyph_for_char(&self, char_idx: usize) -> Option<usize> {
    self.char_to_glyph.get(char_idx).copied()
  }

  /// Gets the character index for a glyph index.
  pub fn char_for_glyph(&self, glyph_idx: usize) -> Option<usize> {
    self.glyph_to_char.get(glyph_idx).copied()
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::{
    FontFaceRule, FontFaceSource, FontFeatureValueType, FontFeatureValuesRule,
  };
  use crate::style::font_feature_values::FontFeatureValuesRegistry;
  use crate::style::types::EastAsianVariant;
  use crate::style::types::EastAsianWidth;
  use crate::style::types::FontFeatureSetting;
  use crate::style::types::FontKerning;
  use crate::style::types::FontStretch;
  use crate::style::types::FontVariantAlternateValue;
  use crate::style::types::FontVariantLigatures;
  use crate::style::types::FontVariationSetting;
  use crate::style::types::FontWeight;
  use crate::style::types::NumericFigure;
  use crate::style::types::NumericFraction;
  use crate::style::types::NumericSpacing;
  use crate::style::types::TextRendering;
  use crate::style::types::TextOrientation;
  use crate::style::types::WritingMode;
  use crate::text::font_db::FontConfig;
  use crate::text::font_db::FontDatabase;
  use crate::text::font_db::FontStretch as DbFontStretch;
  use crate::text::font_db::FontStyle as DbFontStyle;
  use crate::text::font_db::FontWeight as DbFontWeight;
  use crate::text::font_db::GenericFamily;
  use crate::text::font_fallback::FamilyEntry;
  use rustc_hash::FxHashMap;
  use std::fs;
  use std::sync::Arc;
  use std::time::Duration;
  use unicode_bidi::Level;
  use url::Url;

  fn dejavu_sans_fixture_context() -> FontContext {
    let mut db = FontDatabase::empty();
    db.load_font_data(include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf").to_vec())
      .expect("fixture font should load");
    db.refresh_generic_fallbacks();
    FontContext::with_database(Arc::new(db))
  }

  fn system_font_for_char(ch: char) -> Option<(Vec<u8>, String)> {
    let db = FontDatabase::new();
    let id = db
      .faces()
      .find(|face| db.has_glyph_cached(face.id, ch))
      .map(|face| face.id)?;
    let font = db.load_font(id)?;
    Some(((*font.data).clone(), font.family))
  }

  fn variable_system_font_with_axes(required: &[Tag]) -> Option<(Vec<u8>, u32, String)> {
    let db = FontDatabase::new();
    for face_info in db.faces() {
      let Some(font) = db.load_font(face_info.id) else {
        continue;
      };
      let Ok(face) = ttf_parser::Face::parse(font.data.as_ref(), font.index) else {
        continue;
      };
      let axes: Vec<Tag> = face.variation_axes().into_iter().map(|a| a.tag).collect();
      if required.iter().all(|tag| axes.contains(tag)) {
        return Some(((*font.data).clone(), font.index, font.family));
      }
    }
    None
  }

  fn load_font_context_with_data(data: Vec<u8>) -> Option<FontContext> {
    let mut db = FontDatabase::empty();
    db.load_font_data(data).ok()?;
    Some(FontContext::with_database(Arc::new(db)))
  }

  fn temp_font_url(data: &[u8]) -> Option<(tempfile::TempDir, Url)> {
    let dir = tempfile::tempdir().ok()?;
    let path = dir.path().join("fixture.ttf");
    fs::write(&path, data).ok()?;
    let url = Url::from_file_path(&path).ok()?;
    Some((dir, url))
  }

  fn variable_font_context_with_axes(
    required: &[Tag],
  ) -> Option<(FontContext, Vec<u8>, u32, String)> {
    let (data, index, family) = variable_system_font_with_axes(required)?;
    let ctx = load_font_context_with_data(data.clone())?;
    Some((ctx, data, index, family))
  }

  fn system_math_font_for_char(ch: char) -> Option<(Vec<u8>, u32, String)> {
    let db = FontDatabase::new();
    for id in db.find_math_fonts() {
      if !db.has_glyph_cached(id, ch) {
        continue;
      }
      let font = db.load_font(id)?;
      return Some(((*font.data).clone(), font.index, font.family));
    }
    None
  }

  fn system_non_math_font_for_char(ch: char) -> Option<(Vec<u8>, u32, String)> {
    let db = FontDatabase::new();
    for face in db.faces() {
      if !db.has_glyph_cached(face.id, ch) {
        continue;
      }
      let font = db.load_font(face.id)?;
      let has_math_table = ttf_parser::Face::parse(font.data.as_ref(), font.index)
        .ok()
        .and_then(|f| f.tables().math)
        .is_some();
      if has_math_table {
        continue;
      }
      return Some(((*font.data).clone(), font.index, font.family));
    }
    None
  }

  #[test]
  fn test_direction_from_level() {
    assert!(Direction::from_level(Level::ltr()).is_ltr());
    assert!(Direction::from_level(Level::rtl()).is_rtl());
  }

  #[test]
  fn test_direction_to_harfbuzz() {
    assert_eq!(
      Direction::LeftToRight.to_harfbuzz(),
      HbDirection::LeftToRight
    );
    assert_eq!(
      Direction::RightToLeft.to_harfbuzz(),
      HbDirection::RightToLeft
    );
  }

  #[test]
  fn test_script_detection_latin() {
    assert_eq!(Script::detect('A'), Script::Latin);
    assert_eq!(Script::detect('z'), Script::Latin);
    assert_eq!(Script::detect('é'), Script::Latin);
  }

  #[test]
  fn test_script_detection_arabic() {
    assert_eq!(Script::detect('م'), Script::Arabic);
    assert_eq!(Script::detect('ر'), Script::Arabic);
  }

  #[test]
  fn test_script_detection_syriac() {
    assert_eq!(Script::detect('ܐ'), Script::Syriac);
    assert_eq!(Script::detect('ܒ'), Script::Syriac);
  }

  #[test]
  fn test_script_detection_thaana() {
    assert_eq!(Script::detect('ހ'), Script::Thaana);
    assert_eq!(Script::detect('ށ'), Script::Thaana);
  }

  #[test]
  fn test_script_detection_nko() {
    assert_eq!(Script::detect('ߊ'), Script::Nko);
    assert_eq!(Script::detect('ߞ'), Script::Nko);
  }

  #[test]
  fn test_script_detection_javanese() {
    assert_eq!(Script::detect('ꦄ'), Script::Javanese);
    assert_eq!(Script::detect('ꦧ'), Script::Javanese);
  }

  #[test]
  fn test_script_detection_myanmar() {
    assert_eq!(Script::detect('မ'), Script::Myanmar);
  }

  #[test]
  fn test_script_detection_telugu() {
    assert_eq!(Script::detect('త'), Script::Telugu);
  }

  #[test]
  fn test_script_detection_additional_bundled_scripts() {
    assert_eq!(Script::detect('ਗ'), Script::Gurmukhi);
    assert_eq!(Script::detect('ગ'), Script::Gujarati);
    assert_eq!(Script::detect('ଓ'), Script::Oriya);
    assert_eq!(Script::detect('ಕ'), Script::Kannada);
    assert_eq!(Script::detect('മ'), Script::Malayalam);
    assert_eq!(Script::detect('ස'), Script::Sinhala);
    assert_eq!(Script::detect('Ա'), Script::Armenian);
    assert_eq!(Script::detect('ა'), Script::Georgian);
    assert_eq!(Script::detect('አ'), Script::Ethiopic);
    assert_eq!(Script::detect('ກ'), Script::Lao);
    assert_eq!(Script::detect('ཀ'), Script::Tibetan);
    assert_eq!(Script::detect('ក'), Script::Khmer);
    assert_eq!(Script::detect('Ꭰ'), Script::Cherokee);
    assert_eq!(Script::detect('ᐁ'), Script::CanadianAboriginal);
    assert_eq!(Script::detect('ᥐ'), Script::TaiLe);
    assert_eq!(Script::detect('ᱚ'), Script::OlChiki);
    assert_eq!(Script::detect('Ⰰ'), Script::Glagolitic);
    assert_eq!(Script::detect('ⴰ'), Script::Tifinagh);
    assert_eq!(Script::detect('ꠅ'), Script::SylotiNagri);
    assert_eq!(Script::detect('ꯀ'), Script::MeeteiMayek);
    assert_eq!(Script::detect('𐌰'), Script::Gothic);
  }

  #[test]
  fn test_script_detection_hebrew() {
    assert_eq!(Script::detect('ש'), Script::Hebrew);
    assert_eq!(Script::detect('ל'), Script::Hebrew);
  }

  #[test]
  fn test_script_detection_greek() {
    assert_eq!(Script::detect('α'), Script::Greek);
    assert_eq!(Script::detect('Ω'), Script::Greek);
  }

  #[test]
  fn test_script_detection_cyrillic() {
    assert_eq!(Script::detect('А'), Script::Cyrillic);
    assert_eq!(Script::detect('я'), Script::Cyrillic);
  }

  #[test]
  fn test_script_detection_cjk() {
    assert_eq!(Script::detect('中'), Script::Han);
    assert_eq!(Script::detect('あ'), Script::Hiragana);
    assert_eq!(Script::detect('カ'), Script::Katakana);
    assert_eq!(Script::detect('ｶ'), Script::Katakana);
    assert_eq!(Script::detect('한'), Script::Hangul);
  }

  #[test]
  fn test_script_detection_common() {
    assert_eq!(Script::detect(' '), Script::Common);
    assert_eq!(Script::detect('1'), Script::Common);
    assert_eq!(Script::detect('.'), Script::Common);
  }

  #[test]
  fn test_script_detection_combining_marks_are_inherited() {
    // U+07EB is a N'Ko combining tone mark. It must not be classified as N'Ko,
    // otherwise script itemization could split an extended grapheme cluster.
    assert_eq!(Script::detect('\u{07EB}'), Script::Inherited);
  }

  #[test]
  fn test_bidi_analysis_ltr() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("Hello", &style);

    assert!(!bidi.needs_reordering());
    assert!(bidi.base_direction().is_ltr());
  }

  #[test]
  fn test_bidi_analysis_rtl() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("שלום", &style);

    assert!(bidi.needs_reordering());
  }

  #[test]
  fn test_bidi_analysis_mixed() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("Hello שלום World", &style);

    assert!(bidi.needs_reordering());
  }

  #[test]
  fn bidi_analysis_uses_char_indices() {
    let style = ComputedStyle::default();
    let text = "a\u{05d0}\u{05d1}"; // a + two Hebrew letters
    let bidi = BidiAnalysis::analyze(text, &style);

    let indices: Vec<_> = text.char_indices().collect();
    let a_level = bidi.level_at(indices[0].0);
    let hebrew_level = bidi.level_at(indices[1].0);

    assert!(a_level.is_ltr(), "latin char should be LTR");
    assert!(hebrew_level.is_rtl(), "hebrew char should be RTL");
  }

  #[test]
  fn cluster_map_construction_is_linear() {
    let char_count = 10_000;
    let text: String = "a".repeat(char_count);
    let text_len = text.len();
    let glyphs: Vec<GlyphPosition> = (0..char_count)
      .map(|i| GlyphPosition {
        glyph_id: i as u32,
        cluster: i as u32,
        x_offset: 0.0,
        y_offset: 0.0,
        x_advance: 1.0,
        y_advance: 0.0,
      })
      .collect();

    let run = ShapedRun {
      text,
      start: 0,
      end: text_len,
      glyphs,
      direction: Direction::LeftToRight,
      level: 0,
      advance: char_count as f32,
      font: Arc::new(LoadedFont {
        id: None,
        family: "Test".to_string(),
        data: Arc::new(Vec::new()),
        index: 0,
        face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
        face_settings: Default::default(),
        weight: DbFontWeight::NORMAL,
        style: DbFontStyle::Normal,
        stretch: DbFontStretch::Normal,
      }),
      font_size: 16.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::from(Vec::new()),
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      rotation: RunRotation::None,
      vertical: false,
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      variations: Vec::new(),
      scale: 1.0,
    };

    reset_cluster_map_char_iterations();

    let cluster_map = ClusterMap::from_shaped_run(&run);

    for idx in 0..char_count {
      assert_eq!(cluster_map.glyph_for_char(idx), Some(idx));
      assert_eq!(cluster_map.char_for_glyph(idx), Some(idx));
    }

    let iterations = cluster_map_char_iterations();
    assert!(
      iterations >= char_count,
      "expected to scan at least {} characters, got {}",
      char_count,
      iterations
    );
    assert!(
      iterations <= char_count + 1,
      "expected to scan characters once, got {} iterations for {} chars",
      iterations,
      char_count
    );
  }

  #[test]
  fn bidi_override_forces_base_direction() {
    let mut style = ComputedStyle::default();
    style.direction = CssDirection::Rtl;
    style.unicode_bidi = crate::style::types::UnicodeBidi::BidiOverride;
    let text = "abc";
    let bidi = BidiAnalysis::analyze(text, &style);

    let indices: Vec<_> = text.char_indices().collect();
    for (byte_idx, _) in indices {
      assert!(
        bidi.level_at(byte_idx).is_rtl(),
        "override should force RTL level"
      );
    }
    assert!(
      !bidi.needs_reordering(),
      "override should not request reordering"
    );
  }

  #[test]
  fn bidi_plaintext_uses_first_strong_for_base() {
    let mut style = ComputedStyle::default();
    style.unicode_bidi = crate::style::types::UnicodeBidi::Plaintext;

    let rtl_text = "שלום abc";
    let bidi_rtl = BidiAnalysis::analyze(rtl_text, &style);
    assert!(
      bidi_rtl.base_level().is_rtl(),
      "plaintext should pick RTL base from first strong"
    );

    let ltr_text = "abc שלום";
    let bidi_ltr = BidiAnalysis::analyze(ltr_text, &style);
    assert!(
      bidi_ltr.base_level().is_ltr(),
      "plaintext should pick LTR base from first strong"
    );
  }

  #[test]
  fn bidi_controls_do_not_contribute_advance() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );
    let pipeline = ShapingPipeline::new();
    let clean = pipeline.shape("abc", &style, &ctx).expect("shape clean");
    // Inject an isolate sequence around b.
    let isolated = pipeline
      .shape("a\u{2067}b\u{2069}c", &style, &ctx)
      .expect("shape with controls");
    let clean_adv: f32 = clean.iter().map(|r| r.advance).sum();
    let iso_adv: f32 = isolated.iter().map(|r| r.advance).sum();
    assert!(
      (clean_adv - iso_adv).abs() < 0.1,
      "bidi controls should not change advance ({} vs {})",
      clean_adv,
      iso_adv
    );
  }

  #[test]
  fn bidi_format_marks_do_not_break_kerning() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 48.0;

    let pipeline = ShapingPipeline::new();

    let clean_adv = pipeline
      .measure_width("AV", &style, &ctx)
      .expect("measure clean advance");
    let separate_adv = pipeline
      .measure_width("A", &style, &ctx)
      .expect("measure A advance")
      + pipeline
        .measure_width("V", &style, &ctx)
        .expect("measure V advance");
    let kerning_delta = separate_adv - clean_adv;
    assert!(
      kerning_delta.abs() > 0.1,
      "expected kerning for \"AV\" in bundled fonts (A+V={} AV={} delta={})",
      separate_adv,
      clean_adv,
      kerning_delta
    );

    for (label, text) in [
      ("LRM", "A\u{200e}V"),
      ("RLM", "A\u{200f}V"),
      ("ALM", "A\u{061c}V"),
    ] {
      let adv = pipeline
        .measure_width(text, &style, &ctx)
        .unwrap_or_else(|_| panic!("measure advance with {label}"));
      assert!(
        (adv - clean_adv).abs() < 0.01,
        "bidi format mark {label} should not change kerning-sensitive advance ({} vs {})",
        adv,
        clean_adv
      );
    }
  }

  #[test]
  fn text_rendering_optimize_speed_disables_kerning() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 48.0;
    style.text_rendering = TextRendering::OptimizeSpeed;

    let pipeline = ShapingPipeline::new();
    let av_adv = pipeline
      .measure_width("AV", &style, &ctx)
      .expect("measure AV advance");
    let separate_adv = pipeline
      .measure_width("A", &style, &ctx)
      .expect("measure A advance")
      + pipeline
        .measure_width("V", &style, &ctx)
        .expect("measure V advance");

    let delta = separate_adv - av_adv;
    assert!(
      delta.abs() < 0.05,
      "expected optimizeSpeed to disable kerning (A+V={} AV={} delta={})",
      separate_adv,
      av_adv,
      delta
    );
  }

  #[test]
  fn text_rendering_optimize_speed_disables_ligatures() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut auto_style = ComputedStyle::default();
    auto_style.font_family = vec!["sans-serif".to_string()].into();
    auto_style.font_size = 48.0;
    auto_style.text_rendering = TextRendering::Auto;

    let mut speed_style = auto_style.clone();
    speed_style.text_rendering = TextRendering::OptimizeSpeed;

    let pipeline = ShapingPipeline::new();
    let text = "ffi";
    let char_count = text.chars().count();

    let auto = pipeline.shape(text, &auto_style, &ctx).expect("shape auto");
    let auto_glyphs: usize = auto.iter().map(|run| run.glyphs.len()).sum();

    let speed = pipeline
      .shape(text, &speed_style, &ctx)
      .expect("shape optimizeSpeed");
    let speed_glyphs: usize = speed.iter().map(|run| run.glyphs.len()).sum();

    assert!(
      auto_glyphs < char_count,
      "expected bundled fonts to form a ligature for {:?} under auto (chars={} glyphs={})",
      text,
      char_count,
      auto_glyphs
    );
    assert_eq!(
      speed_glyphs, char_count,
      "expected optimizeSpeed to disable ligatures for {:?} (chars={} glyphs={})",
      text, char_count, speed_glyphs
    );
    assert!(
      speed_glyphs > auto_glyphs,
      "expected optimizeSpeed to increase glyph count for {:?} (auto={} speed={})",
      text,
      auto_glyphs,
      speed_glyphs
    );
  }

  #[test]
  fn font_feature_settings_override_text_rendering_optimize_speed_for_kerning() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 48.0;
    style.text_rendering = TextRendering::OptimizeSpeed;
    style.font_feature_settings = vec![FontFeatureSetting {
      tag: *b"kern",
      value: 1,
    }]
    .into();

    let pipeline = ShapingPipeline::new();
    let av_adv = pipeline
      .measure_width("AV", &style, &ctx)
      .expect("measure AV advance");
    let separate_adv = pipeline
      .measure_width("A", &style, &ctx)
      .expect("measure A advance")
      + pipeline
        .measure_width("V", &style, &ctx)
        .expect("measure V advance");

    let delta = separate_adv - av_adv;
    assert!(
      delta.abs() > 0.1,
      "expected font-feature-settings to re-enable kerning even under optimizeSpeed (A+V={} AV={} delta={})",
      separate_adv,
      av_adv,
      delta
    );
  }

  #[test]
  fn font_feature_settings_override_text_rendering_optimize_speed_for_ligatures() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 48.0;
    style.text_rendering = TextRendering::OptimizeSpeed;

    let pipeline = ShapingPipeline::new();
    let text = "ffi";
    let char_count = text.chars().count();

    let speed = pipeline
      .shape(text, &style, &ctx)
      .expect("shape optimizeSpeed");
    let speed_glyphs: usize = speed.iter().map(|run| run.glyphs.len()).sum();
    assert_eq!(
      speed_glyphs, char_count,
      "optimizeSpeed baseline should disable ligatures for {:?}",
      text
    );

    let mut override_style = style.clone();
    override_style.font_feature_settings = vec![
      FontFeatureSetting {
        tag: *b"liga",
        value: 1,
      },
      FontFeatureSetting {
        tag: *b"clig",
        value: 1,
      },
    ]
    .into();

    let override_runs = pipeline
      .shape(text, &override_style, &ctx)
      .expect("shape optimizeSpeed with feature overrides");
    let override_glyphs: usize = override_runs.iter().map(|run| run.glyphs.len()).sum();

    assert!(
      override_glyphs < char_count,
      "expected font-feature-settings to re-enable ligatures even under optimizeSpeed (chars={} glyphs={})",
      char_count,
      override_glyphs
    );
    assert!(
      override_glyphs < speed_glyphs,
      "expected font-feature-settings override to reduce glyph count relative to optimizeSpeed baseline (baseline={} override={})",
      speed_glyphs,
      override_glyphs
    );
  }

  #[test]
  fn combining_marks_do_not_force_last_resort_fallback_or_render_notdef() {
    let style = ComputedStyle::default();
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let pipeline = ShapingPipeline::new();
    let text = "中\u{1AB0}";

    let shaped = pipeline.shape(text, &style, &ctx).expect("shape succeeds");
    assert!(
      !shaped.is_empty(),
      "shaping should produce at least one run"
    );

    let base_has_glyph = shaped
      .iter()
      .flat_map(|run| run.glyphs.iter())
      .any(|glyph| glyph.cluster == 0 && glyph.glyph_id != 0);
    assert!(
      base_has_glyph,
      "base character should render with a real glyph even when followed by an unsupported combining mark"
    );

    let has_notdef_for_mark = shaped
      .iter()
      .flat_map(|run| run.glyphs.iter())
      .any(|glyph| glyph.glyph_id == 0);
    assert!(
      !has_notdef_for_mark,
      "unsupported combining marks should not emit .notdef glyphs"
    );
  }

  #[test]
  fn ascii_runs_use_single_font_run_and_match_slow_path_shaping() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let text = "The quick brown fox jumps over the lazy dog 0123456789.!? "
      .repeat(128)
      .trim_end()
      .to_string();
    assert!(text.is_ascii());

    let run = ItemizedRun {
      start: 0,
      end: text.len(),
      text: text.clone(),
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let fast = assign_fonts_internal(
      &[run.clone()],
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts with ASCII fast path");
    assert_eq!(
      fast.len(),
      1,
      "ASCII fast-path should keep a single font run"
    );

    let slow = assign_fonts_internal(&[run], &style, &ctx, None, ctx.font_generation(), false)
      .expect("assign fonts without ASCII fast path");
    assert_eq!(
      slow.len(),
      1,
      "slow path should also keep a single font run"
    );

    assert_eq!(fast[0].text, text);
    assert_eq!(slow[0].text, text);
    assert_eq!(fast[0].font.family, slow[0].font.family);
    assert_eq!(fast[0].font.index, slow[0].font.index);

    let shaped_fast = shape_font_run(&fast[0]).expect("shape fast run");
    let shaped_slow = shape_font_run(&slow[0]).expect("shape slow run");
    assert_eq!(
      shaped_fast.glyphs.len(),
      shaped_slow.glyphs.len(),
      "fast/slow shaping should produce the same number of glyphs"
    );
    for (a, b) in shaped_fast.glyphs.iter().zip(shaped_slow.glyphs.iter()) {
      assert_eq!(a.glyph_id, b.glyph_id);
      assert_eq!(a.cluster, b.cluster);
      assert!((a.x_advance - b.x_advance).abs() < 0.0001);
      assert!((a.y_advance - b.y_advance).abs() < 0.0001);
      assert!((a.x_offset - b.x_offset).abs() < 0.0001);
      assert!((a.y_offset - b.y_offset).abs() < 0.0001);
    }
    assert!((shaped_fast.advance - shaped_slow.advance).abs() < 0.0001);
  }

  #[test]
  fn non_ascii_runs_use_single_font_run_and_match_slow_path_shaping() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let text = "中".repeat(256);
    assert!(!text.is_ascii());

    let run = ItemizedRun {
      start: 0,
      end: text.len(),
      text: text.clone(),
      script: Script::Han,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let fast = assign_fonts_internal(
      &[run.clone()],
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts with run fast path enabled");
    assert_eq!(
      fast.len(),
      1,
      "non-ASCII run fast path should keep a single font run"
    );

    let slow = assign_fonts_internal(&[run], &style, &ctx, None, ctx.font_generation(), false)
      .expect("assign fonts with run fast path disabled");
    assert_eq!(
      slow.len(),
      1,
      "slow path should also keep a single font run"
    );

    assert_eq!(fast[0].text, text);
    assert_eq!(slow[0].text, text);
    assert_eq!(fast[0].font.family, slow[0].font.family);
    assert_eq!(fast[0].font.index, slow[0].font.index);

    let shaped_fast = shape_font_run(&fast[0]).expect("shape fast run");
    let shaped_slow = shape_font_run(&slow[0]).expect("shape slow run");
    assert_eq!(
      shaped_fast.glyphs.len(),
      shaped_slow.glyphs.len(),
      "fast/slow shaping should produce the same number of glyphs"
    );
    for (a, b) in shaped_fast.glyphs.iter().zip(shaped_slow.glyphs.iter()) {
      assert_eq!(a.glyph_id, b.glyph_id);
      assert_eq!(a.cluster, b.cluster);
      assert!((a.x_advance - b.x_advance).abs() < 0.0001);
      assert!((a.y_advance - b.y_advance).abs() < 0.0001);
      assert!((a.x_offset - b.x_offset).abs() < 0.0001);
      assert!((a.y_offset - b.y_offset).abs() < 0.0001);
    }
    assert!((shaped_fast.advance - shaped_slow.advance).abs() < 0.0001);
  }

  #[test]
  fn coalesces_adjacent_font_runs_with_identical_shaping_parameters() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_size = 16.0;

    let text = "Hello world";
    let split = 5;
    let runs = [
      ItemizedRun {
        start: 0,
        end: split,
        text: text[..split].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
      ItemizedRun {
        start: split,
        end: text.len(),
        text: text[split..].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
    ];

    let font_runs = assign_fonts_internal(
      &runs,
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    assert_eq!(
      font_runs.len(),
      1,
      "adjacent runs with identical shaping params should be coalesced"
    );
    let run = &font_runs[0];
    assert_eq!(run.start, 0);
    assert_eq!(run.end, text.len());
    assert_eq!(run.text, text);
    assert_eq!(run.text.len(), run.end - run.start);
  }

  #[test]
  fn coalescing_avoids_hard_break_boundaries() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_size = 16.0;

    let text = "Hello\nworld";
    let split = "Hello\n".len();
    let runs = [
      ItemizedRun {
        start: 0,
        end: split,
        text: text[..split].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
      ItemizedRun {
        start: split,
        end: text.len(),
        text: text[split..].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
    ];

    let font_runs = assign_fonts_internal(
      &runs,
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    assert_eq!(
      font_runs.len(),
      2,
      "font runs should not coalesce across hard line breaks"
    );
    assert_eq!(
      font_runs.iter().map(|run| run.text.as_str()).collect::<String>(),
      text
    );
  }

  #[test]
  fn coalescing_avoids_other_paragraph_separators() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_size = 16.0;

    // U+0085 (NEL) is treated as a paragraph separator by the Unicode bidi algorithm. Coalescing
    // must avoid bridging it so shaping never tries to form adjacency-sensitive features across
    // a hard break boundary.
    let text = "Hello\u{0085}world";
    let split = "Hello\u{0085}".len();
    let runs = [
      ItemizedRun {
        start: 0,
        end: split,
        text: text[..split].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
      ItemizedRun {
        start: split,
        end: text.len(),
        text: text[split..].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      },
    ];

    let font_runs = assign_fonts_internal(
      &runs,
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    assert_eq!(
      font_runs.len(),
      2,
      "font runs should not coalesce across Unicode paragraph separators"
    );
    assert_eq!(
      font_runs.iter().map(|run| run.text.as_str()).collect::<String>(),
      text
    );
  }

  #[test]
  fn coalescing_reduces_shape_font_run_invocations_for_churny_input() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_size = 16.0;

    let text = "The quick brown fox jumps over the lazy dog. "
      .repeat(64)
      .trim_end()
      .to_string();

    // Simulate an upstream stage splitting a paragraph into many adjacent itemized runs even
    // though they share script/direction/level. Coalescing should collapse these back down so
    // shaping work stays bounded.
    let mut itemized = Vec::new();
    let chunk_len = 5;
    let mut start = 0;
    while start < text.len() {
      let end = (start + chunk_len).min(text.len());
      itemized.push(ItemizedRun {
        start,
        end,
        text: text[start..end].to_string(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      });
      start = end;
    }
    let segments = itemized.len();
    assert!(segments > 8, "expected to build a churny run list");

    let font_runs = assign_fonts_internal(
      &itemized,
      &style,
      &ctx,
      None,
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    assert!(
      font_runs.len() < segments,
      "coalescing should reduce the number of font runs"
    );
    assert_eq!(
      font_runs.iter().map(|run| run.text.as_str()).collect::<String>(),
      text
    );
    assert_eq!(font_runs.first().unwrap().start, 0);
    assert_eq!(font_runs.last().unwrap().end, text.len());

    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));
    for run in &font_runs {
      shape_font_run(run).expect("shape succeeds");
    }
    let first_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
    assert_eq!(first_calls, font_runs.len());
    assert!(
      first_calls < segments,
      "expected fewer HarfBuzz shaping calls after coalescing"
    );

    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));
    for run in &font_runs {
      shape_font_run(run).expect("shape succeeds");
    }
    let second_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
    assert_eq!(second_calls, font_runs.len());
    assert!(
      second_calls <= first_calls,
      "shaping the same paragraph twice should not increase shaping churn"
    );
  }

  #[test]
  fn size_adjust_scales_shaping_advances() {
    let data =
      Arc::new(include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf").to_vec());
    let base_font = Arc::new(LoadedFont {
      id: None,
      family: "DejaVu Sans".to_string(),
      data: Arc::clone(&data),
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      weight: DbFontWeight::NORMAL,
      style: DbFontStyle::Normal,
      stretch: DbFontStretch::Normal,
    });
    let mut overrides = crate::text::font_db::FontFaceMetricsOverrides::default();
    overrides.size_adjust = 2.0;
    let adjusted_font = Arc::new(LoadedFont {
      id: None,
      family: "DejaVu Sans".to_string(),
      data: Arc::clone(&data),
      index: 0,
      face_metrics_overrides: overrides,
      face_settings: Default::default(),
      weight: DbFontWeight::NORMAL,
      style: DbFontStyle::Normal,
      stretch: DbFontStretch::Normal,
    });

    let features = Arc::from(Vec::<Feature>::new().into_boxed_slice());
    let text = "Hello";
    let make_run = |font: Arc<LoadedFont>| FontRun {
      text: text.to_string(),
      start: 0,
      end: text.len(),
      font,
      synthetic_bold: 0.0,
      synthetic_oblique: 0.0,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
      font_size: 16.0,
      baseline_shift: 0.0,
      language: None,
      features: Arc::clone(&features),
      variations: Vec::new(),
      palette_index: 0,
      palette_overrides: Arc::new(Vec::new()),
      palette_override_hash: 0,
      rotation: RunRotation::None,
      vertical: false,
    };

    let shaped_base = shape_font_run(&make_run(Arc::clone(&base_font))).expect("shape base run");
    let shaped_adjusted =
      shape_font_run(&make_run(Arc::clone(&adjusted_font))).expect("shape adjusted run");

    assert!(
      (shaped_base.scale - 1.0).abs() < 1e-6,
      "expected base run scale 1.0, got {}",
      shaped_base.scale
    );
    assert!(
      (shaped_adjusted.scale - 2.0).abs() < 1e-6,
      "expected adjusted run scale 2.0, got {}",
      shaped_adjusted.scale
    );
    assert_eq!(
      shaped_base.glyphs.len(),
      shaped_adjusted.glyphs.len(),
      "size-adjust should not change shaping output glyph count"
    );
    assert!(
      (shaped_adjusted.advance - shaped_base.advance * 2.0).abs() < 0.01,
      "expected adjusted advance to scale by 2.0 ({} vs {})",
      shaped_adjusted.advance,
      shaped_base.advance
    );
  }

  #[test]
  fn missing_base_glyph_clusters_fall_back_to_notdef() {
    let data = fs::read(crate::testing::fixtures_dir().join("fonts/NotoSans-subset.ttf"))
      .expect("read Noto Sans subset");
    let ctx = load_font_context_with_data(data).expect("load font context");
    let font = ctx
      .database()
      .first_font()
      .expect("font context should contain at least one font");
    let face = crate::text::face_cache::get_ttf_face(&font).expect("parse font");

    let pipeline = ShapingPipeline::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec![font.family.clone()].into();
    style.font_size = 16.0;

    for text in ["뮝\u{07FD}", "犧\u{0345}"] {
      let clusters = atomic_shaping_clusters(text);
      assert_eq!(clusters, vec![(0, text.len())], "expected a single cluster");

      let base_char = text.chars().next().expect("string should have base char");
      assert!(
        !face.has_glyph(base_char),
        "fixture font unexpectedly supports base character U+{:04X}",
        base_char as u32
      );

      let shaped = pipeline.shape(text, &style, &ctx).expect("shape succeeds");
      let glyphs: Vec<_> = shaped.iter().flat_map(|run| run.glyphs.iter()).collect();
      assert!(
        !glyphs.is_empty(),
        "shaping should produce at least one glyph"
      );
      assert!(
        glyphs
          .iter()
          .any(|glyph| glyph.cluster == 0 && glyph.glyph_id == 0),
        "base cluster should emit .notdef when glyph is missing"
      );
    }
  }

  #[test]
  fn atomic_clusters_skip_emoji_sequence_spans_for_ascii_text() {
    crate::text::emoji::debug_reset_emoji_sequence_span_calls();
    let text = "Hello";
    let clusters = atomic_shaping_clusters(text);
    assert_eq!(
      clusters,
      vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)],
      "expected ASCII text to cluster by scalar boundaries"
    );
    assert_eq!(
      crate::text::emoji::debug_emoji_sequence_span_calls(),
      0,
      "ASCII text should not run emoji sequence detection"
    );
  }

  #[test]
  fn tag_sequence_components_are_non_rendering_for_coverage() {
    // TAG LATIN SMALL LETTER A (emoji tag character) and CANCEL TAG.
    assert!(is_non_rendering_for_coverage('\u{E0061}'));
    assert!(is_non_rendering_for_coverage('\u{E007F}'));
  }

  #[test]
  fn tag_sequences_do_not_require_tag_character_coverage() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );

    // Declare a "web emoji" font family that does *not* support the base emoji. This ensures the
    // fallback resolver sees an emoji font early that can't render the cluster, and would otherwise
    // short-circuit before considering the bundled emoji font when tag characters are treated as
    // required for coverage.
    let web_data = fs::read(crate::testing::fixtures_dir().join("fonts/NotoSans-subset.ttf"))
      .expect("read web font data");
    let Some((_dir, web_url)) = temp_font_url(&web_data) else {
      return;
    };
    let face = FontFaceRule {
      family: Some("WebEmoji".to_string()),
      sources: vec![FontFaceSource::url(web_url.to_string())],
      ..Default::default()
    };
    ctx
      .load_web_fonts(&[face], None, None)
      .expect("load web emoji font");

    let pipeline = ShapingPipeline::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["WebEmoji".to_string(), "FastRender Emoji".to_string()].into();
    style.font_size = 24.0;
    style.font_variant_emoji = FontVariantEmoji::Normal;

    // Base emoji + tag letters + cancel tag. The tag characters should not be considered required
    // for font coverage; otherwise the resolver can end up stuck on the "WebEmoji" font and lose
    // the base glyph.
    let text = "\u{1F600}\u{E0061}\u{E007F}";
    let shaped = pipeline
      .shape(text, &style, &ctx)
      .expect("shape tag cluster");
    assert_eq!(shaped.len(), 1, "expected a single run for tag cluster");

    let run = &shaped[0];
    assert_eq!(
      run.font.family, "FastRender Emoji",
      "tag components should not force fallback away from the bundled emoji font"
    );
    let face = crate::text::face_cache::get_ttf_face(&run.font).expect("parse resolved font");
    assert!(
      face.has_glyph('\u{1F600}'),
      "resolved font ({}) should have glyph for base emoji",
      run.font.family
    );
    assert!(
      run
        .glyphs
        .iter()
        .any(|glyph| glyph.cluster == 0 && glyph.glyph_id != 0),
      "base emoji should not shape to .notdef"
    );

    for glyph in run.glyphs.iter().filter(|glyph| glyph.glyph_id == 0) {
      assert!(
        glyph.x_advance.abs() <= 0.0001 && glyph.y_advance.abs() <= 0.0001,
        "unexpected visible .notdef glyph (cluster={}, advance={})",
        glyph.cluster,
        glyph.x_advance
      );
    }
  }

  #[test]
  fn atomic_clusters_skip_emoji_sequence_spans_for_non_emoji_text() {
    crate::text::emoji::debug_reset_emoji_sequence_span_calls();
    let text = "a\u{0301}";
    let clusters = atomic_shaping_clusters(text);
    assert_eq!(clusters, vec![(0, text.len())]);
    assert_eq!(
      crate::text::emoji::debug_emoji_sequence_span_calls(),
      0,
      "combining-mark clusters should not run emoji sequence detection"
    );
  }

  #[test]
  fn atomic_clusters_run_emoji_sequence_spans_for_zwj_sequences() {
    crate::text::emoji::debug_reset_emoji_sequence_span_calls();
    let text = "👨\u{200d}👩";
    let clusters = atomic_shaping_clusters(text);
    assert_eq!(clusters, vec![(0, text.len())]);
    assert_eq!(
      crate::text::emoji::debug_emoji_sequence_span_calls(),
      1,
      "ZWJ sequences need emoji sequence detection to stay atomic"
    );
  }

  #[test]
  fn atomic_clusters_keep_emoji_sequences_atomic() {
    // Emoji sequences that contain joiners/variation selectors/keycaps/tag chars must never be
    // split into multiple shaping/fallback clusters.
    for text in [
      "👨\u{200D}👩\u{200D}👧",                                   // ZWJ sequence
      "1\u{20E3}", // keycap (digit + combining enclosing keycap)
      "❤️",        // variation selector-16
      "🏴\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}", // tag sequence (gb-sct)
    ] {
      let clusters = atomic_shaping_clusters(text);
      assert_eq!(
        clusters,
        vec![(0, text.len())],
        "expected emoji sequence to remain atomic: {text:?}"
      );
    }
  }

  #[test]
  fn map_hb_position_applies_baseline_shift_horizontally() {
    let mut pos = rustybuzz::GlyphPosition::default();
    pos.x_advance = 10;
    pos.y_advance = 3;
    pos.x_offset = 2;
    pos.y_offset = -4;
    let (x_offset, y_offset, x_advance, y_advance) = map_hb_position(false, 1.5, 2.0, &pos);

    assert_eq!(x_offset, 4.0, "x offset should use x_offset with scale");
    assert_eq!(
      y_offset, -6.5,
      "baseline shift applies to y offset in horizontal mode"
    );
    assert_eq!(x_advance, 20.0, "x advance scales inline axis");
    assert_eq!(y_advance, 6.0, "y advance scales cross axis");
  }

  #[test]
  fn map_hb_position_applies_vertical_offsets_without_swapping() {
    let mut pos = rustybuzz::GlyphPosition::default();
    pos.x_advance = 10;
    pos.y_advance = -20;
    pos.x_offset = 7;
    pos.y_offset = -3;
    let (x_offset, y_offset, x_advance, y_advance) = map_hb_position(true, 2.0, 1.0, &pos);

    assert_eq!(
      x_offset, 9.0,
      "baseline shift should move the cross axis (x) for vertical text"
    );
    assert_eq!(y_offset, -3.0, "inline offset should come from y_offset");
    assert_eq!(x_advance, 10.0, "cross advance maps to x advance");
    assert_eq!(y_advance, 20.0, "inline advance uses absolute y_advance");
  }

  #[test]
  fn map_hb_position_falls_back_to_x_advance_in_vertical_mode() {
    let mut pos = rustybuzz::GlyphPosition::default();
    pos.x_advance = -15;
    pos.y_advance = 0;
    pos.x_offset = 1;
    pos.y_offset = 2;
    let (x_offset, y_offset, x_advance, y_advance) = map_hb_position(true, 5.0, 1.0, &pos);

    assert_eq!(
      x_offset, 6.0,
      "baseline shift should still apply to x offset in vertical mode"
    );
    assert_eq!(y_offset, 2.0, "inline offset stays on the inline axis");
    assert_eq!(
      x_advance, 0.0,
      "cross-axis advance should be cleared when falling back to x_advance as inline advance"
    );
    assert_eq!(
      y_advance, 15.0,
      "inline advance falls back to x_advance and is absolute"
    );
  }

  #[test]
  fn test_bidi_analysis_empty() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("", &style);

    assert!(!bidi.needs_reordering());
  }

  #[test]
  fn test_itemize_single_script() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("Hello", &style);
    let runs = itemize_text("Hello", &bidi);

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "Hello");
    assert_eq!(runs[0].script, Script::Latin);
  }

  #[test]
  fn test_itemize_mixed_scripts() {
    let style = ComputedStyle::default();
    let text = "Hello שלום";
    let bidi = BidiAnalysis::analyze(text, &style);
    let runs = itemize_text(text, &bidi);

    // Should have at least 2 runs (Latin and Hebrew)
    assert!(runs.len() >= 2);
  }

  #[test]
  fn itemization_assigns_leading_neutral_to_first_strong_script() {
    // U+201C LEFT DOUBLE QUOTATION MARK is Script=Common, but it should inherit the following CJK
    // script instead of defaulting to Latin.
    let style = ComputedStyle::default();
    let text = "\u{201C}漢字";
    let bidi = BidiAnalysis::analyze(text, &style);
    let runs = itemize_text(text, &bidi);

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, Script::Han);
    assert_eq!(runs[0].text, text);
  }

  #[test]
  fn itemization_assigns_leading_neutral_per_paragraph() {
    // Leading neutral characters after a paragraph break should not inherit the previous
    // paragraph's script.
    let style = ComputedStyle::default();
    let text = "abc\n\u{201C}漢字";
    let bidi = BidiAnalysis::analyze(text, &style);
    let runs = itemize_text(text, &bidi);

    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].text, "abc\n");
    assert_eq!(runs[0].script, Script::Latin);
    assert_eq!(runs[1].text, "\u{201C}漢字");
    assert_eq!(runs[1].script, Script::Han);
  }

  #[test]
  fn itemization_keeps_combining_marks_with_base_script() {
    // U+07EB is a N'Ko combining mark. When it appears after a Hangul base
    // character it should not force a script run break.
    let style = ComputedStyle::default();
    let text = "가\u{07EB}\u{07CA}"; // Hangul + N'Ko combining mark + N'Ko letter
    let bidi = BidiAnalysis::analyze(text, &style);
    let runs = itemize_text(text, &bidi);

    assert_eq!(
      runs.len(),
      2,
      "combining marks should not start a new script run"
    );
    assert_eq!(runs[0].text, "가\u{07EB}");
    assert_eq!(runs[0].script, Script::Hangul);
    assert_eq!(runs[1].text, "\u{07CA}");
    assert_eq!(runs[1].script, Script::Nko);
  }

  #[test]
  fn test_itemize_empty() {
    let style = ComputedStyle::default();
    let bidi = BidiAnalysis::analyze("", &style);
    let runs = itemize_text("", &bidi);

    assert!(runs.is_empty());
  }

  fn legacy_itemize_text(text: &str, bidi: &BidiAnalysis) -> Vec<ItemizedRun> {
    if text.is_empty() {
      return Vec::new();
    }

    let split_by_level = bidi.needs_reordering();

    let paragraphs = bidi.paragraphs();
    let mut paragraph_index = 0usize;
    let mut paragraph_end = paragraphs
      .first()
      .map(|para| para.end_byte.min(text.len()))
      .unwrap_or(text.len());
    let compute_paragraph_first_strong_script = |start: usize, end: usize| {
      let start = start.min(text.len());
      let end = end.min(text.len());
      let Some(slice) = text.get(start..end) else {
        return Script::Latin;
      };
      for ch in slice.chars() {
        let script = Script::detect(ch);
        if !script.is_neutral() {
          return script;
        }
      }
      Script::Latin
    };
    let mut paragraph_first_strong_script = match paragraphs.first() {
      Some(first_para) => {
        compute_paragraph_first_strong_script(first_para.start_byte, first_para.end_byte)
      }
      None => compute_paragraph_first_strong_script(0, text.len()),
    };

    let mut runs = Vec::new();
    let mut current_start = 0;
    let mut current_text = String::new();
    let mut current_script: Option<Script> = None;
    let mut current_direction: Option<Direction> = None;
    let mut current_level: Option<u8> = None;

    for (idx, ch) in text.char_indices() {
      while idx >= paragraph_end && paragraph_index + 1 < paragraphs.len() {
        if let (Some(script), Some(direction), Some(level)) =
          (current_script, current_direction, current_level)
        {
          if !current_text.is_empty() {
            runs.push(ItemizedRun {
              start: current_start,
              end: idx,
              text: std::mem::take(&mut current_text),
              script,
              direction,
              level,
            });
          }
        }

        current_start = idx;
        current_text.clear();
        current_script = None;
        current_direction = None;
        current_level = None;

        paragraph_index += 1;
        let para = paragraphs[paragraph_index];
        paragraph_end = para.end_byte.min(text.len());
        paragraph_first_strong_script =
          compute_paragraph_first_strong_script(para.start_byte, para.end_byte);
      }

      let char_script = Script::detect(ch);
      let level = bidi.level_at(idx);
      let mut char_direction = Direction::from_level(level);
      let mut char_level = if split_by_level {
        level.number()
      } else {
        let base_level = bidi.base_level();
        if Direction::from_level(base_level) == char_direction {
          base_level.number()
        } else {
          level.number()
        }
      };

      if is_bidi_format_char(ch) {
        if let Some(dir) = current_direction {
          char_direction = dir;
        }
        if let Some(run_level) = current_level {
          char_level = run_level;
        }
      }

      let resolved_script = if char_script.is_neutral() {
        current_script.unwrap_or(paragraph_first_strong_script)
      } else {
        char_script
      };

      let needs_new_run = match (current_script, current_direction, current_level) {
        (None, _, _) => false,
        (Some(script), Some(dir), Some(level)) => {
          dir != char_direction
            || (split_by_level && level != char_level)
            || (!char_script.is_neutral() && script != resolved_script)
        }
        _ => false,
      };

      if needs_new_run {
        if let (Some(script), Some(direction), Some(level)) =
          (current_script, current_direction, current_level)
        {
          runs.push(ItemizedRun {
            start: current_start,
            end: idx,
            text: std::mem::take(&mut current_text),
            script,
            direction,
            level,
          });
        }
        current_start = idx;
      }

      if !char_script.is_neutral() {
        current_script = Some(resolved_script);
      } else if current_script.is_none() {
        current_script = Some(paragraph_first_strong_script);
      }
      current_direction = Some(char_direction);
      current_level = Some(char_level);
      current_text.push(ch);
    }

    if !current_text.is_empty() {
      if let (Some(script), Some(direction), Some(level)) =
        (current_script, current_direction, current_level)
      {
        runs.push(ItemizedRun {
          start: current_start,
          end: text.len(),
          text: current_text,
          script,
          direction,
          level,
        });
      }
    }

    runs
  }

  #[test]
  fn itemize_text_matches_legacy_mixed_scripts_neutrals_and_bidi_controls() {
    // Regression test for the itemizer: mixed scripts + neutrals + bidi formatting chars should
    // produce stable run boundaries and script assignments.
    let style = ComputedStyle::default();
    let text = "Hello,\u{200E} 世界123 \u{202B}שלום\u{202C}!";
    let bidi = BidiAnalysis::analyze(text, &style);

    let legacy = legacy_itemize_text(text, &bidi);
    let runs = itemize_text(text, &bidi);

    assert_eq!(
      runs.len(),
      legacy.len(),
      "run count mismatch\nnew={runs:#?}\nold={legacy:#?}"
    );
    for (new, old) in runs.iter().zip(legacy.iter()) {
      assert_eq!(new.start, old.start);
      assert_eq!(new.end, old.end);
      assert_eq!(new.text, old.text);
      assert_eq!(new.script, old.script);
      assert_eq!(new.direction, old.direction);
      assert_eq!(new.level, old.level);
    }
  }

  #[test]
  fn itemize_text_long_ltr_paragraph_does_not_overproduce_runs() {
    // Guardrail against pathological per-character itemization on long paragraphs.
    let style = ComputedStyle::default();
    let text = "a".repeat(20_000);
    let bidi = BidiAnalysis::analyze(&text, &style);

    ITEMIZE_TEXT_RUNS_PRODUCED.with(|runs| runs.set(0));
    ITEMIZE_TEXT_BYTES_COPIED.with(|bytes| bytes.set(0));

    let runs = itemize_text(&text, &bidi);
    assert_eq!(runs.len(), 1, "expected single run for pure LTR paragraph");
    assert_eq!(runs[0].text.len(), text.len());

    assert_eq!(
      ITEMIZE_TEXT_RUNS_PRODUCED.with(|runs| runs.get()),
      1,
      "expected exactly one allocated run text"
    );
    assert_eq!(
      ITEMIZE_TEXT_BYTES_COPIED.with(|bytes| bytes.get()),
      text.len(),
      "expected copied bytes to match input length"
    );
  }

  #[test]
  fn test_script_is_neutral() {
    assert!(Script::Common.is_neutral());
    assert!(Script::Inherited.is_neutral());
    assert!(Script::Unknown.is_neutral());
    assert!(!Script::Latin.is_neutral());
    assert!(!Script::Arabic.is_neutral());
  }

  #[test]
  fn test_pipeline_new() {
    let pipeline = ShapingPipeline::new();
    // Should not panic
    let _ = pipeline;
  }

  #[test]
  fn shaping_cache_hits_increment_stats_and_skip_work() {
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let pipeline = ShapingPipeline::new();
    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));

    let first = pipeline
      .shape("Cached text", &style, &ctx)
      .expect("first shape should succeed");
    assert!(!first.is_empty());
    let first_run_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
    assert!(
      first_run_calls > 0,
      "initial shape should call shape_font_run"
    );

    let initial_stats = pipeline.cache_stats();
    assert_eq!(initial_stats.misses, 1);
    assert_eq!(initial_stats.hits, 0);

    let second = pipeline
      .shape("Cached text", &style, &ctx)
      .expect("cache hit should succeed");
    assert_eq!(
      SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get()),
      first_run_calls,
      "cache hit should not invoke shaping again"
    );

    let stats_after_hit = pipeline.cache_stats();
    assert_eq!(stats_after_hit.hits, 1);
    assert_eq!(stats_after_hit.misses, 1);
    assert_eq!(first.len(), second.len());
  }

  #[test]
  fn shaping_cache_evicts_when_capacity_exceeded() {
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let capacity = 4;
    let pipeline = ShapingPipeline::with_cache_capacity_for_test(capacity);
    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));

    let texts: Vec<String> = (0..(capacity + 2))
      .map(|i| format!("Eviction {}", i))
      .collect();
    for text in &texts {
      pipeline.shape(text, &style, &ctx).expect("shape succeeds");
    }

    let stats = pipeline.cache_stats();
    assert!(
      stats.evictions >= 2,
      "expected evictions once cache exceeded capacity, saw {}",
      stats.evictions
    );
    assert_eq!(
      pipeline.cache_len(),
      capacity,
      "cache should stay bounded to its configured capacity"
    );
  }

  #[test]
  fn shaping_cache_handles_hash_collisions_by_verifying_text() {
    let cache = ShapingCache::new(8);
    let key = ShapingCacheKey {
      style_hash: 42,
      font_generation: 123,
      text_hash: 999,
      text_len: 5,
      bidi_signature: 0,
    };

    let runs_hello = Arc::new(Vec::new());
    let runs_world = Arc::new(Vec::new());

    cache.insert(key, Arc::from("hello"), Arc::clone(&runs_hello));
    cache.insert(key, Arc::from("world"), Arc::clone(&runs_world));

    let got_hello = cache.get(&key, "hello").expect("expected entry for hello");
    let got_world = cache.get(&key, "world").expect("expected entry for world");

    assert!(Arc::ptr_eq(&got_hello, &runs_hello));
    assert!(Arc::ptr_eq(&got_world, &runs_world));
    assert_eq!(
      cache.len(),
      1,
      "hash collision bucket should share cache key"
    );
  }

  #[test]
  fn shaping_cache_key_includes_explicit_bidi_context() {
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let pipeline = ShapingPipeline::with_cache_capacity_for_test(8);
    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));

    pipeline
      .shape_arc("Cached bidi", &style, &ctx)
      .expect("initial shape succeeds");

    let stats_after_first = pipeline.cache_stats();
    assert_eq!(stats_after_first.misses, 1);
    assert_eq!(stats_after_first.hits, 0);
    assert_eq!(pipeline.cache_len(), 1);

    pipeline
      .shape_with_context_arc(
        "Cached bidi",
        &style,
        &ctx,
        Direction::LeftToRight,
        Some(ExplicitBidiContext {
          level: Level::rtl(),
          override_all: true,
        }),
      )
      .expect("shaping with explicit bidi succeeds");

    let stats_after_second = pipeline.cache_stats();
    assert_eq!(stats_after_second.misses, 2);
    assert_eq!(stats_after_second.hits, 0);
    assert_eq!(pipeline.cache_len(), 2);
  }

  #[test]
  fn shaping_cache_misses_when_text_rendering_changes() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );
    let pipeline = ShapingPipeline::with_cache_capacity_for_test(8);
    SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));

    let text = "Cached text-rendering";
    let style = ComputedStyle::default();

    pipeline
      .shape(text, &style, &ctx)
      .expect("initial shape succeeds");
    let first_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
    assert!(first_calls > 0);

    let stats_after_first = pipeline.cache_stats();
    assert_eq!(stats_after_first.misses, 1);
    assert_eq!(stats_after_first.hits, 0);

    let mut speed_style = style.clone();
    speed_style.text_rendering = TextRendering::OptimizeSpeed;
    pipeline
      .shape(text, &speed_style, &ctx)
      .expect("optimizeSpeed shape succeeds");
    let second_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
    assert!(
      second_calls > first_calls,
      "text-rendering change should miss the shaping cache"
    );

    let stats_after_second = pipeline.cache_stats();
    assert_eq!(stats_after_second.misses, 2);
    assert_eq!(stats_after_second.hits, 0);
    assert_eq!(pipeline.cache_len(), 2);
  }

  #[test]
  fn shaping_style_hash_includes_used_color_scheme() {
    let mut light = ComputedStyle::default();
    light.used_dark_color_scheme = false;
    let light_hash = shaping_style_hash(&light);

    let mut dark = light.clone();
    dark.used_dark_color_scheme = true;

    assert_ne!(
      light_hash,
      shaping_style_hash(&dark),
      "used_dark_color_scheme should affect shaping cache key (palette overrides can depend on light-dark())"
    );
  }

  #[test]
  fn shaping_style_hash_includes_text_rendering() {
    let mut auto = ComputedStyle::default();
    auto.text_rendering = TextRendering::Auto;
    let auto_hash = shaping_style_hash(&auto);

    let mut speed = auto.clone();
    speed.text_rendering = TextRendering::OptimizeSpeed;

    assert_ne!(
      auto_hash,
      shaping_style_hash(&speed),
      "text-rendering affects OpenType feature selection and must be part of the shaping cache key"
    );
  }

  #[test]
  fn shaping_style_hash_includes_font_variant_alternates_fields() {
    let base = ComputedStyle::default();
    let base_hash = shaping_style_hash(&base);

    let mut with_character_variants = base.clone();
    with_character_variants
      .font_variant_alternates
      .character_variants = vec![FontVariantAlternateValue::Number(1)];
    assert_ne!(
      base_hash,
      shaping_style_hash(&with_character_variants),
      "character-variant() should affect shaping cache key"
    );

    let mut with_swash = base.clone();
    with_swash.font_variant_alternates.swash = Some(FontVariantAlternateValue::Number(1));
    assert_ne!(
      base_hash,
      shaping_style_hash(&with_swash),
      "swash() should affect shaping cache key"
    );

    let mut with_ornaments = base.clone();
    with_ornaments.font_variant_alternates.ornaments = Some(FontVariantAlternateValue::Number(1));
    assert_ne!(
      base_hash,
      shaping_style_hash(&with_ornaments),
      "ornaments() should affect shaping cache key"
    );

    let mut with_annotation = base.clone();
    with_annotation.font_variant_alternates.annotation =
      Some(FontVariantAlternateValue::Name("test".to_string()));
    assert_ne!(
      base_hash,
      shaping_style_hash(&with_annotation),
      "annotation() should affect shaping cache key"
    );
  }

  #[test]
  fn shaping_style_hash_includes_font_feature_values_registry() {
    let base = ComputedStyle::default();
    let base_hash = shaping_style_hash(&base);

    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("disambiguation".to_string(), vec![2u32])]),
    );
    registry.register(rule);

    let mut with_feature_values = base.clone();
    with_feature_values.font_feature_values = Arc::new(registry);

    assert_ne!(
      base_hash,
      shaping_style_hash(&with_feature_values),
      "font-feature-values should affect shaping cache key (named alternates depend on them)"
    );
  }

  #[test]
  fn shaping_cache_misses_when_font_variant_alternates_change() {
    let style = ComputedStyle::default();
    let ctx = FontContext::new();
    let pipeline = ShapingPipeline::with_cache_capacity_for_test(8);
    let text = "Cached alternates";

    fn assert_cache_miss_for_variant<F>(
      pipeline: &ShapingPipeline,
      ctx: &FontContext,
      text: &str,
      base_style: &ComputedStyle,
      label: &'static str,
      mutate: F,
    ) where
      F: FnOnce(&mut ComputedStyle),
    {
      pipeline.clear_cache();
      SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.set(0));

      pipeline
        .shape(text, base_style, ctx)
        .expect("initial shape should succeed");
      let first_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
      assert!(
        first_calls > 0,
        "{label}: initial shape should call shape_font_run"
      );
      let stats_after_first = pipeline.cache_stats();
      assert_eq!(
        stats_after_first.misses, 1,
        "{label}: expected miss for first shape"
      );
      assert_eq!(
        stats_after_first.hits, 0,
        "{label}: expected no hits for first shape"
      );

      let mut variant_style = base_style.clone();
      mutate(&mut variant_style);
      pipeline
        .shape(text, &variant_style, ctx)
        .expect("variant shape should succeed");
      let second_calls = SHAPE_FONT_RUN_INVOCATIONS.with(|calls| calls.get());
      assert!(
        second_calls > first_calls,
        "{label}: shaping cache should miss when style differs"
      );

      let stats_after_second = pipeline.cache_stats();
      assert_eq!(
        stats_after_second.misses, 2,
        "{label}: expected miss for variant style"
      );
      assert_eq!(
        stats_after_second.hits, 0,
        "{label}: expected no hits when style differs"
      );
      assert_eq!(
        pipeline.cache_len(),
        2,
        "{label}: expected both style variants to populate shaping cache"
      );
    }

    assert_cache_miss_for_variant(
      &pipeline,
      &ctx,
      text,
      &style,
      "character_variants",
      |style| {
        style.font_variant_alternates.character_variants =
          vec![FontVariantAlternateValue::Number(1)];
      },
    );
    assert_cache_miss_for_variant(&pipeline, &ctx, text, &style, "swash", |style| {
      style.font_variant_alternates.swash = Some(FontVariantAlternateValue::Number(1));
    });
    assert_cache_miss_for_variant(&pipeline, &ctx, text, &style, "ornaments", |style| {
      style.font_variant_alternates.ornaments = Some(FontVariantAlternateValue::Number(1));
    });
    assert_cache_miss_for_variant(&pipeline, &ctx, text, &style, "annotation", |style| {
      style.font_variant_alternates.annotation =
        Some(FontVariantAlternateValue::Name("test".to_string()))
    });
  }

  #[test]
  fn shaping_style_hash_canonicalizes_negative_zero() {
    let mut positive = ComputedStyle::default();
    positive.font_size = 0.0;
    positive.font_style = CssFontStyle::Oblique(Some(0.0));
    positive.font_size_adjust = FontSizeAdjust::Number {
      ratio: 0.0,
      metric: FontSizeAdjustMetric::ExHeight,
    };
    positive.font_variation_settings = vec![FontVariationSetting {
      tag: *b"wght",
      value: 0.0,
    }]
    .into();
    let positive_hash = shaping_style_hash(&positive);

    let mut negative = positive.clone();
    negative.font_size = -0.0;
    negative.font_style = CssFontStyle::Oblique(Some(-0.0));
    negative.font_size_adjust = FontSizeAdjust::Number {
      ratio: -0.0,
      metric: FontSizeAdjustMetric::ExHeight,
    };
    negative.font_variation_settings = vec![FontVariationSetting {
      tag: *b"wght",
      value: -0.0,
    }]
    .into();

    assert_eq!(positive_hash, shaping_style_hash(&negative));
  }

  #[test]
  fn shaping_sets_language_from_style() {
    let mut style = ComputedStyle::default();
    style.language = "tr-TR".into();
    let pipeline = ShapingPipeline::new();
    let font_ctx = FontContext::new();

    let runs = pipeline
      .shape("i", &style, &font_ctx)
      .expect("shape succeeds");
    assert!(!runs.is_empty());
    assert_eq!(runs[0].language.as_ref().map(|l| l.as_str()), Some("tr-tr"));
  }

  #[test]
  fn font_size_adjust_number_scales_font_run() {
    let font_ctx = FontContext::new();
    let Some(font) = font_ctx.get_sans_serif() else {
      return;
    };
    let Some(aspect) = font.metrics().ok().and_then(|m| m.aspect_ratio()) else {
      return;
    };

    let mut style = ComputedStyle::default();
    style.font_family = vec![font.family.clone()].into();
    style.font_size = 20.0;
    let desired = aspect * 2.0;
    style.font_size_adjust = FontSizeAdjust::Number {
      ratio: desired,
      metric: FontSizeAdjustMetric::ExHeight,
    };

    let pipeline = ShapingPipeline::new();
    let runs = pipeline
      .shape("Hello", &style, &font_ctx)
      .expect("shape succeeds");
    assert!(!runs.is_empty());

    let expected = style.font_size * (desired / aspect);
    assert!((runs[0].font_size - expected).abs() < 0.01);
  }

  #[test]
  fn font_size_adjust_from_font_defaults_to_base_font() {
    let font_ctx = FontContext::new();
    let Some(font) = font_ctx.get_sans_serif() else {
      return;
    };
    let Some(aspect) = font.metrics().ok().and_then(|m| m.aspect_ratio()) else {
      return;
    };

    let mut style = ComputedStyle::default();
    style.font_family = vec![font.family.clone()].into();
    style.font_size = 18.0;
    style.font_size_adjust = FontSizeAdjust::FromFont {
      metric: FontSizeAdjustMetric::ExHeight,
    };

    let pipeline = ShapingPipeline::new();
    let runs = pipeline
      .shape("Hello", &style, &font_ctx)
      .expect("shape succeeds");
    assert!(!runs.is_empty());

    // Using the same font as the reference should preserve the base size.
    assert!((runs[0].font_size - style.font_size).abs() < 0.01);
    // Sanity: aspect was available to avoid the test vacuously passing.
    assert!(aspect > 0.0);
  }

  #[test]
  fn test_itemized_run_len() {
    let run = ItemizedRun {
      start: 0,
      end: 5,
      text: "Hello".to_string(),
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    assert_eq!(run.len(), 5);
    assert!(!run.is_empty());
  }

  #[test]
  fn test_itemized_run_empty() {
    let run = ItemizedRun {
      start: 5,
      end: 5,
      text: String::new(),
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    assert_eq!(run.len(), 0);
    assert!(run.is_empty());
  }

  #[test]
  fn test_script_to_harfbuzz() {
    // Specific scripts should return Some
    assert!(Script::Latin.to_harfbuzz().is_some());
    assert!(Script::Arabic.to_harfbuzz().is_some());
    assert!(Script::Syriac.to_harfbuzz().is_some());
    assert!(Script::Thaana.to_harfbuzz().is_some());
    assert!(Script::Nko.to_harfbuzz().is_some());
    assert!(Script::Hebrew.to_harfbuzz().is_some());
    assert!(Script::Javanese.to_harfbuzz().is_some());
    assert!(Script::Myanmar.to_harfbuzz().is_some());
    assert!(Script::Telugu.to_harfbuzz().is_some());
    assert!(Script::Gujarati.to_harfbuzz().is_some());
    // Common/neutral scripts should return None (auto-detect)
    assert!(Script::Common.to_harfbuzz().is_none());
    assert!(Script::Inherited.to_harfbuzz().is_none());
  }

  #[test]
  fn test_reorder_runs_empty() {
    let mut runs: Vec<ShapedRun> = Vec::new();
    reorder_runs(&mut runs, &[]);
    assert!(runs.is_empty());
  }

  #[test]
  fn itemization_splits_runs_at_paragraph_boundaries() {
    let mut style = ComputedStyle::default();
    style.direction = CssDirection::Ltr;
    let text = "abc\nאבג";

    let bidi = BidiAnalysis::analyze(text, &style);
    let runs = itemize_text(text, &bidi);

    assert_eq!(
      runs.len(),
      2,
      "should yield one run per paragraph in this case"
    );
    let paras = bidi.paragraphs();
    assert_eq!(paras.len(), 2);
    assert_eq!(runs[0].start, paras[0].start_byte);
    assert_eq!(runs[0].end, paras[0].end_byte);
    assert_eq!(runs[1].start, paras[1].start_byte);
    assert_eq!(runs[1].end, paras[1].end_byte);
  }

  #[test]
  fn bidi_analysis_records_paragraph_boundaries() {
    let mut style = ComputedStyle::default();
    style.direction = CssDirection::Ltr;
    let text = "abc\nאבג";

    let analysis = BidiAnalysis::analyze(text, &style);
    let paragraphs = analysis.paragraphs();
    assert_eq!(paragraphs.len(), 2);
    assert_eq!(paragraphs[0].start_byte, 0);
    assert!(paragraphs[0].end_byte > paragraphs[0].start_byte);
    assert_eq!(paragraphs[1].start_byte, paragraphs[0].end_byte);
    assert_eq!(paragraphs[1].end_byte, text.len());
  }

  #[test]
  fn reorder_runs_respects_paragraph_boundaries() {
    fn run(start: usize, end: usize, level: u8) -> ShapedRun {
      ShapedRun {
        text: String::new(),
        start,
        end,
        glyphs: Vec::new(),
        direction: if level % 2 == 0 {
          Direction::LeftToRight
        } else {
          Direction::RightToLeft
        },
        level,
        advance: 0.0,
        font: Arc::new(LoadedFont {
          id: None,
          family: "Test".to_string(),
          data: Arc::new(Vec::new()),
          index: 0,
          face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
          face_settings: Default::default(),
          weight: DbFontWeight::NORMAL,
          style: DbFontStyle::Normal,
          stretch: DbFontStretch::Normal,
        }),
        font_size: 16.0,
        baseline_shift: 0.0,
        language: None,
        features: Arc::from(Vec::new()),
        synthetic_bold: 0.0,
        synthetic_oblique: 0.0,
        rotation: RunRotation::None,
        vertical: false,
        palette_index: 0,
        palette_overrides: Arc::new(Vec::new()),
        palette_override_hash: 0,
        variations: Vec::new(),
        scale: 1.0,
      }
    }

    // Two paragraphs whose runs all share the same level; reordering should not swap paragraphs.
    let mut runs = vec![run(0, 3, 1), run(3, 6, 1)];
    let paragraphs = vec![
      ParagraphBoundary {
        start_byte: 0,
        end_byte: 3,
        level: Level::ltr(),
      },
      ParagraphBoundary {
        start_byte: 3,
        end_byte: 6,
        level: Level::ltr(),
      },
    ];

    reorder_runs(&mut runs, &paragraphs);
    assert_eq!(runs[0].start, 0);
    assert_eq!(runs[1].start, 3);
  }

  #[test]
  fn shape_with_direction_uses_explicit_base() {
    let mut style = ComputedStyle::default();
    style.unicode_bidi = crate::style::types::UnicodeBidi::Normal;
    let ctx = FontContext::new();
    let text = "!?";

    let shaped = ShapingPipeline::new()
      .shape_with_direction(text, &style, &ctx, Direction::RightToLeft)
      .expect("shape_with_direction");

    assert_eq!(shaped.len(), 1);
    let run = &shaped[0];
    assert_eq!(run.direction, Direction::RightToLeft);
    assert_eq!(run.level % 2, 1, "bidi level should reflect RTL base");
  }

  #[test]
  fn itemize_text_keeps_level_parity_consistent_without_reordering() {
    let style = ComputedStyle::default();
    let text = "ABC";

    // Force an RTL paragraph base but shape text that is entirely LTR. The Unicode bidi algorithm
    // assigns an even embedding level (typically 2) to the LTR letters. Even though no visual
    // reordering is required, itemization must keep the run level parity consistent with the run
    // direction so downstream layout code can rely on it.
    let bidi = BidiAnalysis::analyze_with_base(text, &style, Direction::RightToLeft, None);
    assert!(!bidi.needs_reordering());

    let runs = itemize_text(text, &bidi);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].direction.is_ltr());
    assert_eq!(runs[0].level, 2);
  }

  #[test]
  fn small_caps_shapes_lowercase_with_scaled_size() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_variant = FontVariant::SmallCaps;
    style.font_size = 20.0;
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert!(shaped.iter().any(|r| (r.font_size - 16.0).abs() < 0.1));
    assert!(shaped.iter().any(|r| (r.font_size - 20.0).abs() < 0.1));
  }

  #[test]
  fn small_caps_keeps_combining_marks_in_scaled_run() {
    let ctx = dejavu_sans_fixture_context();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_variant = FontVariant::SmallCaps;
    style.font_size = 20.0;

    let shaped = ShapingPipeline::new()
      .shape("a\u{0301}", &style, &ctx)
      .expect("shape with combining mark");

    assert!(
      shaped.iter().all(|run| (run.font_size - 16.0).abs() < 0.1),
      "synthetic small-caps should keep combining marks with the scaled segment"
    );
  }

  #[test]
  fn font_synthesis_none_disables_synthetic_small_caps() {
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_variant_caps = FontVariantCaps::SmallCaps;
    style.font_size = 18.0;
    style.font_synthesis.small_caps = false;
    let ctx = dejavu_sans_fixture_context();
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert_eq!(
      shaped.len(),
      1,
      "synthetic small-caps should not split runs"
    );
    assert!(shaped.iter().all(|r| (r.font_size - 18.0).abs() < 0.1));
  }

  #[test]
  fn synthetic_super_position_applies_without_feature() {
    let pipeline = ShapingPipeline::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_variant_position = FontVariantPosition::Super;
    style.font_size = 20.0;

    let ctx = dejavu_sans_fixture_context();
    let runs = pipeline
      .shape("x", &style, &ctx)
      .expect("shape superscript");
    assert!(!runs.is_empty(), "expected at least one shaped run");
    let run = &runs[0];

    assert!(
      run.font_size < style.font_size,
      "synthetic superscript should shrink font size"
    );
    assert!(
      run.glyphs.iter().any(|g| g.y_offset > 0.0),
      "synthetic superscript should raise glyphs"
    );
  }

  #[test]
  fn font_synthesis_position_none_disables_synthetic_shift() {
    let pipeline = ShapingPipeline::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["DejaVu Sans".to_string()].into();
    style.font_size = 20.0;
    style.font_variant_position = FontVariantPosition::Super;
    style.font_synthesis.position = false;

    let ctx = dejavu_sans_fixture_context();
    let runs = pipeline
      .shape("x", &style, &ctx)
      .expect("shape superscript");
    assert!(!runs.is_empty(), "expected at least one shaped run");
    let run = &runs[0];

    assert!((run.font_size - style.font_size).abs() < 0.01);
    assert!(
      run.glyphs.iter().all(|g| g.y_offset.abs() < 0.01),
      "synthesis disabled should keep glyphs on the baseline"
    );
  }

  #[test]
  fn font_variation_settings_adjust_variable_font_axes() {
    let Some((data, _index, family)) = variable_system_font_with_axes(&[Tag::from_bytes(b"wdth")])
    else {
      return;
    };
    let Some(ctx) = load_font_context_with_data(data) else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let default_run = pipeline.shape("mmmm", &style, &ctx).expect("default run");

    style.font_variation_settings = vec![FontVariationSetting {
      tag: *b"wdth",
      value: 75.0,
    }]
    .into();
    let narrow_run = pipeline.shape("mmmm", &style, &ctx).expect("narrow run");

    style.font_variation_settings = vec![FontVariationSetting {
      tag: *b"wdth",
      value: 130.0,
    }]
    .into();
    let wide_run = pipeline.shape("mmmm", &style, &ctx).expect("wide run");

    let default_advance: f32 = default_run.iter().map(|r| r.advance).sum();
    let narrow_advance: f32 = narrow_run.iter().map(|r| r.advance).sum();
    let wide_advance: f32 = wide_run.iter().map(|r| r.advance).sum();

    assert!(
      narrow_advance < default_advance,
      "wdth axis should shrink glyph advances"
    );
    assert!(
      wide_advance > default_advance,
      "wdth axis should widen glyph advances"
    );
  }

  #[test]
  fn shaping_cache_resets_when_fonts_change() {
    let Some((fallback_data, fallback_family)) = system_font_for_char('m') else {
      return;
    };
    let mut db = FontDatabase::empty();
    db.load_font_data(fallback_data)
      .expect("load fallback font");
    let ctx = FontContext::with_database(Arc::new(db));

    let mut style = ComputedStyle::default();
    style.font_family = vec!["Webby".to_string(), fallback_family.clone()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let fallback_runs = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if fallback_runs.is_empty() {
      return;
    }
    let fallback_font = &fallback_runs[0].font;
    assert_eq!(fallback_font.family, fallback_family);

    let Some((web_data, _web_family)) = system_font_for_char('A') else {
      return;
    };
    let Some((_dir, web_url)) = temp_font_url(&web_data) else {
      return;
    };
    let face = FontFaceRule {
      family: Some("Webby".to_string()),
      sources: vec![FontFaceSource::url(web_url.to_string())],
      ..Default::default()
    };
    ctx
      .load_web_fonts(&[face], None, None)
      .expect("load web font");

    let web_runs = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if web_runs.is_empty() {
      return;
    }
    let web_font = &web_runs[0].font;
    assert_eq!(web_font.family, "Webby");
    assert!(
      !Arc::ptr_eq(&web_font.data, &fallback_font.data)
        || (web_runs[0].advance - fallback_runs[0].advance).abs() > 0.1,
      "web font selection should invalidate cached shaping"
    );
  }

  #[test]
  fn font_optical_sizing_none_skips_opsz_axis() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"opsz")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family].into();
    style.font_size = 20.0;
    let runs_auto = match assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    ) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs_auto.is_empty() {
      return;
    }
    let opsz_tag = Tag::from_bytes(b"opsz");
    assert!(runs_auto[0].variations.iter().any(|v| v.tag == opsz_tag));

    style.font_optical_sizing = crate::style::types::FontOpticalSizing::None;
    let runs_none = match assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    ) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs_none.is_empty() {
      return;
    }
    assert!(runs_none[0].variations.iter().all(|v| v.tag != opsz_tag));
  }

  #[test]
  fn wght_axis_prevents_synthetic_bold() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"wght")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family].into();
    style.font_weight = FontWeight::Number(900);

    let runs = match ShapingPipeline::new().shape("m", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }
    assert!(
      runs.iter().all(|r| r.synthetic_bold.abs() < f32::EPSILON),
      "wght axis should satisfy requested weight without synthetic bolding"
    );
  }

  #[test]
  fn font_language_override_sets_language_tag() {
    let ctx = FontContext::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["serif".to_string()].into();
    style.font_language_override =
      crate::style::types::FontLanguageOverride::Override("SRB".to_string());

    let runs = ShapingPipeline::new()
      .shape("text", &style, &ctx)
      .expect("shape with override");
    assert!(runs
      .iter()
      .all(|r| r.language.as_ref().map(|l| l.as_str()) == Some("srb")));
  }

  #[test]
  fn non_ascii_whitespace_font_language_override_does_not_trim_nbsp() {
    let ctx = FontContext::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["serif".to_string()].into();
    style.font_language_override =
      crate::style::types::FontLanguageOverride::Override(format!("\u{00A0}SRB\u{00A0}"));

    let runs = ShapingPipeline::new()
      .shape("text", &style, &ctx)
      .expect("shape");
    assert!(runs.iter().all(|r| r.language.is_none()));
  }

  #[test]
  fn non_ascii_whitespace_language_signature_for_script_fallback_does_not_trim_nbsp() {
    assert_eq!(language_signature_for_script_fallback("ja"), 1);
    assert_eq!(language_signature_for_script_fallback("ko"), 2);
    assert_eq!(language_signature_for_script_fallback("zh"), 0);
    assert_eq!(language_signature_for_script_fallback("zh-Hans"), 0);
    assert_eq!(language_signature_for_script_fallback("zh-Hant"), 3);
    assert_eq!(language_signature_for_script_fallback("zh-TW"), 3);
    assert_eq!(language_signature_for_script_fallback("\u{00A0}ja"), 0);
    assert_eq!(language_signature_for_script_fallback("ko\u{00A0}"), 0);
  }

  #[test]
  fn generic_serif_prefers_named_fallback_web_fonts() {
    // Fixture harnesses commonly inject a deterministic "Times New Roman" via `@font-face`, while
    // page styles rely on the default `serif` generic. Ensure the shaping pipeline can resolve the
    // generic to the injected web font (via `GenericFamily::fallback_families`) instead of
    // immediately falling back to a local generic mapping.
    let ctx = FontContext::with_config(FontConfig::bundled_only());

    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let font_path = manifest_dir.join("tests/fixtures/fonts/STIXTwoMath-Regular.otf");
    assert!(
      font_path.is_file(),
      "missing test font at {}",
      font_path.display()
    );
    let font_url = Url::from_file_path(font_path)
      .expect("font path is absolute")
      .to_string();
    let face = FontFaceRule {
      family: Some("Times New Roman".to_string()),
      sources: vec![FontFaceSource::url(font_url)],
      ..Default::default()
    };

    ctx
      .load_web_fonts(&[face], None, None)
      .expect("load web font");

    let mut style = ComputedStyle::default();
    style.font_family = vec!["serif".to_string()].into();
    style.font_size = 12.0;

    let runs = ShapingPipeline::new()
      .shape("x", &style, &ctx)
      .expect("shape");
    assert!(!runs.is_empty());
    assert_eq!(runs[0].font.family, "Times New Roman");
  }

  #[test]
  fn math_generic_prefers_math_fonts() {
    let ctx = FontContext::new();
    if ctx.database().find_math_fonts().is_empty() {
      return;
    }
    if !ctx
      .database()
      .find_math_fonts()
      .iter()
      .copied()
      .any(|id| ctx.database().has_glyph(id, '∑'))
    {
      return;
    }
    let mut style = ComputedStyle::default();
    style.font_family = vec!["math".to_string()].into();
    style.font_size = 18.0;

    let runs = match ShapingPipeline::new().shape("∑", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }
    let has_math_table = ttf_parser::Face::parse(runs[0].font.data.as_ref(), runs[0].font.index)
      .ok()
      .and_then(|f| f.tables().math)
      .is_some();
    assert!(
      has_math_table,
      "math generic should select a font advertising a MATH table"
    );
  }

  #[test]
  fn math_generic_prefers_web_math_fonts() {
    let Some((math_data, _math_index, _math_family)) = system_math_font_for_char('∑') else {
      return;
    };
    let Some((_dir, font_url)) = temp_font_url(&math_data) else {
      return;
    };
    let face = FontFaceRule {
      family: Some("WebMath".to_string()),
      sources: vec![FontFaceSource::url(font_url.to_string())],
      ..Default::default()
    };

    let mut db = FontDatabase::empty();
    let _ = db.load_font_data(math_data);
    let ctx = FontContext::with_database(Arc::new(db));
    if ctx.load_web_fonts(&[face], None, None).is_err() {
      return;
    }

    let mut style = ComputedStyle::default();
    style.font_family = vec!["math".to_string()].into();
    style.font_size = 16.0;

    let runs = match ShapingPipeline::new().shape("∑", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }
    assert_eq!(runs[0].font.family, "WebMath");
  }

  #[test]
  fn math_generic_falls_back_without_math_fonts() {
    let Some((fallback_data, _fallback_index, fallback_family)) =
      system_non_math_font_for_char('∑')
    else {
      return;
    };

    let mut db = FontDatabase::empty();
    db.load_font_data(fallback_data)
      .expect("load fallback font");
    let ctx = FontContext::with_database(Arc::new(db));
    if !ctx.database().find_math_fonts().is_empty() {
      return;
    }

    let mut style = ComputedStyle::default();
    style.font_family = vec!["math".to_string(), fallback_family.clone()].into();
    style.font_size = 16.0;

    let runs = match ShapingPipeline::new().shape("∑", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }
    assert_eq!(runs[0].font.family, fallback_family);
  }

  #[test]
  fn bundled_font_aliases_resolve_in_text_pipeline() {
    let ctx = FontContext::with_database(Arc::new(FontDatabase::shared_bundled()));
    let families = vec![FamilyEntry::Named("Helvetica".to_string())];

    let mut picker = FontPreferencePicker::new(EmojiPreference::Neutral);
    let font = resolve_font_for_char(
      'A',
      Script::Latin,
      "",
      &families,
      400,
      DbFontStyle::Normal,
      None,
      DbFontStretch::Normal,
      &ctx,
      &mut picker,
    )
    .expect("font for basic latin");
    assert_eq!(font.family.as_str(), "Noto Sans");
  }

  #[test]
  fn unicode_range_limits_web_font_usage() {
    let Some((fallback_data, fallback_family)) = system_font_for_char('a') else {
      return;
    };
    let Some((_dir, font_url)) = temp_font_url(&fallback_data) else {
      return;
    };
    let face = FontFaceRule {
      family: Some("RangeFace".to_string()),
      sources: vec![FontFaceSource::url(font_url.to_string())],
      unicode_ranges: vec![(0x0041, 0x005a)],
      ..Default::default()
    };

    let mut db = FontDatabase::empty();
    db.load_font_data(fallback_data)
      .expect("load fallback font");
    let ctx = FontContext::with_database(Arc::new(db));
    if ctx.load_web_fonts(&[face], None, None).is_err() {
      return;
    }

    let families = vec![
      FamilyEntry::Named("RangeFace".to_string()),
      FamilyEntry::Named(fallback_family),
      FamilyEntry::Generic(GenericFamily::SansSerif),
    ];

    let mut picker = FontPreferencePicker::new(EmojiPreference::Neutral);
    let upper = resolve_font_for_char(
      'A',
      Script::Latin,
      "",
      &families,
      400,
      DbFontStyle::Normal,
      None,
      DbFontStretch::Normal,
      &ctx,
      &mut picker,
    )
    .expect("font for uppercase");
    assert_eq!(upper.family.as_str(), "RangeFace");

    let mut picker = FontPreferencePicker::new(EmojiPreference::Neutral);
    let lower = resolve_font_for_char(
      'a',
      Script::Latin,
      "",
      &families,
      400,
      DbFontStyle::Normal,
      None,
      DbFontStretch::Normal,
      &ctx,
      &mut picker,
    )
    .expect("fallback font");
    assert_ne!(lower.family.as_str(), "RangeFace");
  }

  #[test]
  fn web_font_only_context_shapes_missing_glyphs_with_notdef() {
    let font_data = fs::read(crate::testing::fixtures_dir().join("fonts/NotoSans-subset.ttf"))
      .expect("read NotoSans subset fixture");
    let parsed = ttf_parser::Face::parse(&font_data, 0).expect("parse NotoSans subset fixture");
    assert!(parsed.glyph_index('A').is_some());
    assert!(parsed.glyph_index('a').is_some());

    let missing = 'א';
    assert!(
      parsed.glyph_index(missing).is_none(),
      "fixture should not cover the missing codepoint"
    );

    let Some((_dir, font_url)) = temp_font_url(&font_data) else {
      panic!("expected temp file URL for font fixture");
    };

    let ctx = FontContext::with_database(Arc::new(FontDatabase::empty()));
    assert_eq!(ctx.database().font_count(), 0);

    let face = FontFaceRule {
      family: Some("WebOnlySans".to_string()),
      sources: vec![FontFaceSource::url(font_url.to_string())],
      ..Default::default()
    };
    ctx
      .load_web_fonts(&[face], None, None)
      .expect("load web fonts");
    assert!(
      ctx.wait_for_pending_web_fonts(Duration::from_secs(1)),
      "web font load should settle"
    );
    assert!(
      !ctx.is_effectively_empty(),
      "web fonts should make the font context non-empty"
    );

    let pipeline = ShapingPipeline::new();
    let mut style = ComputedStyle::default();
    style.font_family = vec!["WebOnlySans".to_string()].into();
    style.font_size = 16.0;

    let ok_runs = pipeline
      .shape("Aa", &style, &ctx)
      .expect("shape basic Latin with web font");
    assert!(!ok_runs.is_empty());
    assert!(ok_runs.iter().all(|run| run.font.family == "WebOnlySans"));
    assert!(
      ok_runs
        .iter()
        .all(|run| run.glyphs.iter().all(|glyph| glyph.glyph_id != 0)),
      "covered ASCII should not produce `.notdef`"
    );

    let missing_runs = pipeline
      .shape(&missing.to_string(), &style, &ctx)
      .expect("shape missing glyph with web font last-resort");
    assert!(!missing_runs.is_empty());
    assert!(missing_runs
      .iter()
      .all(|run| run.font.family == "WebOnlySans"));
    assert!(
      missing_runs
        .iter()
        .any(|run| run.glyphs.iter().any(|glyph| glyph.glyph_id == 0)),
      "missing codepoint should be shaped into `.notdef` glyphs, not a hard error"
    );
  }

  #[test]
  fn generic_family_fallbacks_can_resolve_web_font_aliases() {
    // The Chrome baseline harness aliases common serif families (e.g. Times New Roman) via
    // `@font-face` to keep fixture renders deterministic. When content requests the generic family
    // (`font-family: serif`), browsers can end up selecting the configured serif fallback and then
    // using the aliased web font. Ensure the shaping pipeline considers the generic fallback names
    // for web font resolution too.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let font_path = manifest_dir.join("tests/fixtures/fonts/STIXTwoMath-Regular.otf");
    assert!(
      font_path.is_file(),
      "missing test font at {}",
      font_path.display()
    );
    let font_url = Url::from_file_path(&font_path)
      .expect("font path is absolute")
      .to_string();

    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let face = FontFaceRule {
      family: Some("Times New Roman".to_string()),
      sources: vec![FontFaceSource::url(font_url)],
      weight: (100, 1000),
      ..Default::default()
    };
    ctx
      .load_web_fonts(&[face], None, None)
      .expect("load web font alias");
    assert!(
      ctx.wait_for_pending_web_fonts(Duration::from_secs(1)),
      "web font load should settle"
    );

    let pipeline = ShapingPipeline::new();
    let style = ComputedStyle::default(); // `font-family: serif` by default.
    let runs = pipeline.shape("Hello", &style, &ctx).expect("shape");
    assert!(!runs.is_empty());
    assert!(
      runs.iter().all(|run| run.font.family == "Times New Roman"),
      "expected generic serif to resolve to aliased web font"
    );
  }

  #[test]
  fn script_aware_fallback_prefers_bundled_script_faces() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let pipeline = ShapingPipeline::new();

    let mut base_style = ComputedStyle::default();
    base_style.font_family = vec!["sans-serif".to_string()].into();
    base_style.font_size = 16.0;

    let mut latin_style = base_style.clone();
    latin_style.language = "en".into();
    let latin_runs = pipeline
      .shape("Hello", &latin_style, &ctx)
      .expect("shape latin");
    assert!(!latin_runs.is_empty());
    assert_eq!(latin_runs[0].font.family, "Noto Sans");

    let mut devanagari_style = base_style.clone();
    devanagari_style.language = "hi".into();
    let devanagari_runs = pipeline
      .shape("ह", &devanagari_style, &ctx)
      .expect("shape devanagari");
    assert_eq!(devanagari_runs[0].font.family, "Noto Sans Devanagari");

    let mut bengali_style = base_style.clone();
    bengali_style.language = "bn".into();
    let bengali_runs = pipeline
      .shape("আ", &bengali_style, &ctx)
      .expect("shape bengali");
    assert_eq!(bengali_runs[0].font.family, "Noto Sans Bengali");

    let mut arabic_style = base_style.clone();
    arabic_style.language = "ar".into();
    let arabic_runs = pipeline
      .shape("م", &arabic_style, &ctx)
      .expect("shape arabic");
    assert_eq!(arabic_runs[0].font.family, "Noto Sans Arabic");

    // CJK Han characters exist in multiple bundled faces. We prefer a language-specific
    // face, mirroring browser system fallback.
    let mut ja_style = base_style.clone();
    ja_style.language = "ja".into();
    let ja_runs = pipeline
      .shape("漢、字", &ja_style, &ctx)
      .expect("shape japanese han");
    assert_eq!(
      ja_runs.len(),
      1,
      "expected CJK punctuation to share the JP face"
    );
    assert_eq!(ja_runs[0].font.family, "Noto Sans JP");

    let ja_leading_punct = pipeline
      .shape("、漢字", &ja_style, &ctx)
      .expect("shape japanese leading punctuation");
    assert_eq!(
      ja_leading_punct.len(),
      1,
      "leading neutral characters should inherit the CJK script and stay on the JP face"
    );
    assert_eq!(ja_leading_punct[0].font.family, "Noto Sans JP");

    let ja_multiline = pipeline
      .shape("Hello\n、漢字", &ja_style, &ctx)
      .expect("shape japanese multiline");
    assert_eq!(
      ja_multiline.len(),
      2,
      "expected paragraph break to split runs but keep leading punctuation with the CJK script"
    );
    assert_eq!(ja_multiline[1].text, "、漢字");
    assert_eq!(ja_multiline[1].font.family, "Noto Sans JP");

    let ja_mixed = pipeline
      .shape("Hello、漢字", &ja_style, &ctx)
      .expect("shape japanese punctuation after latin");
    assert_eq!(
      ja_mixed.len(),
      2,
      "expected CJK punctuation following Latin to use the JP face instead of generic common fallbacks"
    );
    assert_eq!(ja_mixed[1].text, "、漢字");
    assert_eq!(ja_mixed[1].font.family, "Noto Sans JP");

    let ja_fullwidth_comma = pipeline
      .shape("Hello，漢字", &ja_style, &ctx)
      .expect("shape japanese fullwidth comma after latin");
    assert_eq!(
      ja_fullwidth_comma.len(),
      2,
      "expected fullwidth punctuation following Latin to start the CJK run"
    );
    assert_eq!(ja_fullwidth_comma[1].text, "，漢字");
    assert_eq!(ja_fullwidth_comma[1].font.family, "Noto Sans JP");

    let ja_fullwidth_parens = pipeline
      .shape("Daruma（だるま）", &ja_style, &ctx)
      .expect("shape japanese fullwidth parens after latin");
    let paren_runs = ja_fullwidth_parens
      .iter()
      .filter(|run| run.text.contains('（') || run.text.contains('）'))
      .collect::<Vec<_>>();
    assert!(
      !paren_runs.is_empty(),
      "expected fullwidth parentheses to survive shaping runs"
    );
    for run in paren_runs {
      assert_eq!(
        run.font.family, "Noto Sans JP",
        "fullwidth parentheses should follow JP glyph shapes, not generic CJK fallbacks"
      );
    }

    let ja_halfwidth_katakana = pipeline
      .shape("Helloｶﾀｶﾅ", &ja_style, &ctx)
      .expect("shape japanese halfwidth katakana after latin");
    assert_eq!(
      ja_halfwidth_katakana.len(),
      2,
      "expected halfwidth Katakana to start a CJK run under ja language"
    );
    assert_eq!(ja_halfwidth_katakana[1].text, "ｶﾀｶﾅ");
    assert_eq!(ja_halfwidth_katakana[1].font.family, "Noto Sans JP");

    let mut ko_style = base_style.clone();
    ko_style.language = "ko".into();
    let ko_runs = pipeline
      .shape("漢、字", &ko_style, &ctx)
      .expect("shape korean han");
    assert_eq!(
      ko_runs.len(),
      1,
      "expected CJK punctuation to share the KR face"
    );
    assert_eq!(ko_runs[0].font.family, "Noto Sans KR");

    let mut zh_style = base_style.clone();
    zh_style.language = "zh".into();
    let zh_runs = pipeline
      .shape("漢、字", &zh_style, &ctx)
      .expect("shape chinese han");
    assert_eq!(zh_runs.len(), 1);
    assert_eq!(zh_runs[0].font.family, "Noto Sans SC");

    let mut zh_hant_style = base_style.clone();
    zh_hant_style.language = "zh-Hant".into();
    let zh_hant_runs = pipeline
      .shape("漢、字", &zh_hant_style, &ctx)
      .expect("shape traditional chinese han");
    assert_eq!(zh_hant_runs.len(), 1);
    assert_eq!(zh_hant_runs[0].font.family, "Noto Sans TC");

    // Hebrew/Thai mapping is exercised only when those faces are present (they may be
    // added by a separate bundled-font task).
    for (lang, sample, expected) in [
      ("he", "א", "Noto Sans Hebrew"),
      ("th", "ก", "Noto Sans Thai"),
    ] {
      let has_face = ctx
        .database()
        .faces()
        .any(|face| face.families.iter().any(|(family, _)| family == expected));
      if !has_face {
        continue;
      }
      let mut style = base_style.clone();
      style.language = lang.into();
      let runs = pipeline
        .shape(sample, &style, &ctx)
        .expect("shape optional");
      assert_eq!(runs[0].font.family, expected);
    }
  }

  #[test]
  fn fallback_cache_hits_for_reused_clusters() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    assert!(
      !ctx.database().is_empty(),
      "bundled font context should load deterministic fonts for tests"
    );
    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::with_cache_capacity_for_test(1);

    let before = pipeline.fallback_cache_stats();
    // Keep the first non-control character stable across both runs so the ASCII fast path reuses
    // the same glyph fallback cache key.
    let first = match pipeline.shape("cache me please", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if first.is_empty() {
      return;
    }
    let mid = pipeline.fallback_cache_stats();

    let second = match pipeline.shape("cache me again", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if second.is_empty() {
      return;
    }
    let after = pipeline.fallback_cache_stats();

    let miss_delta = (mid.glyph_misses + mid.cluster_misses)
      .saturating_sub(before.glyph_misses + before.cluster_misses);
    let hit_delta =
      (after.glyph_hits + after.cluster_hits).saturating_sub(mid.glyph_hits + mid.cluster_hits);
    assert!(
      miss_delta > 0,
      "first shaping should populate fallback cache"
    );
    assert!(
      hit_delta > 0,
      "subsequent shaping should reuse fallback cache (hits {hit_delta}, misses {miss_delta})"
    );
  }

  #[test]
  fn fallback_cache_generation_is_shared_across_pipeline_clones() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let first_pipeline = pipeline.clone();
    let second_pipeline = pipeline.clone();

    // The ASCII fast path probes the fallback cache using only the first non-control character, so
    // both runs must start with the same byte to observe cache hits across pipeline clones.
    let text1 = "abcdefghijklmnopqrstuvwxyz";
    let text2 = "azyxwvutsrqponmlkjihgfedcb";

    let before = pipeline.fallback_cache_stats();
    let first_runs = first_pipeline
      .shape(text1, &style, &ctx)
      .expect("shape first run");
    assert!(!first_runs.is_empty());
    let mid = pipeline.fallback_cache_stats();

    let second_runs = second_pipeline
      .shape(text2, &style, &ctx)
      .expect("shape second run");
    assert!(!second_runs.is_empty());
    let after = pipeline.fallback_cache_stats();

    assert_eq!(
      mid.clears,
      before.clears + 1,
      "first shaping should clear fallback cache when font generation changes"
    );
    assert_eq!(
      after.clears, mid.clears,
      "subsequent clones must not clear the shared fallback cache for the same generation"
    );

    let second_hit_delta = after.glyph_hits.saturating_sub(mid.glyph_hits);
    let second_miss_delta = after.glyph_misses.saturating_sub(mid.glyph_misses);
    assert!(
      second_hit_delta > 0,
      "expected glyph fallback cache hits across pipeline clones"
    );
    assert_eq!(
      second_miss_delta, 0,
      "expected the second run to reuse cached glyph resolutions"
    );
  }

  #[test]
  fn fallback_cache_reuses_cjk_fonts_across_language_region_variants() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let text = "漢字";

    style.language = "ja-JP".into();
    let before = pipeline.fallback_cache_stats();
    let runs = pipeline.shape(text, &style, &ctx).expect("shape ja-JP");
    assert!(!runs.is_empty());
    let mid = pipeline.fallback_cache_stats();

    style.language = "ja".into();
    let runs = pipeline.shape(text, &style, &ctx).expect("shape ja");
    assert!(!runs.is_empty());
    let after = pipeline.fallback_cache_stats();

    let miss_delta_first = (mid.glyph_misses + mid.cluster_misses)
      .saturating_sub(before.glyph_misses + before.cluster_misses);
    assert!(
      miss_delta_first > 0,
      "expected initial shaping to populate fallback cache"
    );

    let miss_delta_second = (after.glyph_misses + after.cluster_misses)
      .saturating_sub(mid.glyph_misses + mid.cluster_misses);
    let hit_delta_second =
      (after.glyph_hits + after.cluster_hits).saturating_sub(mid.glyph_hits + mid.cluster_hits);
    assert_eq!(
      miss_delta_second, 0,
      "expected region variants to reuse the same CJK fallback cache entries"
    );
    assert!(
      hit_delta_second > 0,
      "expected fallback cache hits when reusing region-variant language buckets"
    );
  }

  #[test]
  fn fallback_cache_reuses_non_cjk_fallback_across_languages() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let text = "ܐ";

    style.language = "en".into();
    let before = pipeline.fallback_cache_stats();
    let runs = pipeline.shape(text, &style, &ctx).expect("shape en");
    assert!(!runs.is_empty());
    let mid = pipeline.fallback_cache_stats();

    style.language = "fr".into();
    let runs = pipeline.shape(text, &style, &ctx).expect("shape fr");
    assert!(!runs.is_empty());
    let after = pipeline.fallback_cache_stats();

    let miss_delta_first = (mid.glyph_misses + mid.cluster_misses)
      .saturating_sub(before.glyph_misses + before.cluster_misses);
    assert!(
      miss_delta_first > 0,
      "expected initial shaping to populate fallback cache"
    );

    let miss_delta_second = (after.glyph_misses + after.cluster_misses)
      .saturating_sub(mid.glyph_misses + mid.cluster_misses);
    let hit_delta_second =
      (after.glyph_hits + after.cluster_hits).saturating_sub(mid.glyph_hits + mid.cluster_hits);
    assert_eq!(
      miss_delta_second, 0,
      "expected non-CJK script fallback to ignore language tags in cache keys"
    );
    assert!(
      hit_delta_second > 0,
      "expected fallback cache hits when shaping the same non-CJK script under different languages"
    );
  }

  #[cfg(debug_assertions)]
  #[test]
  fn face_parse_counts_stop_scaling_with_length() {
    let ctx = FontContext::new();
    let _guard = crate::text::face_cache::FaceParseCountGuard::start();

    let mut style = ComputedStyle::default();
    style.font_family = vec!["sans-serif".to_string()].into();
    let pipeline = ShapingPipeline::new();

    let short = "fast render text ".repeat(4);
    let long = "fast render text ".repeat(200);

    if pipeline.shape(&short, &style, &ctx).is_err() {
      return;
    }
    let short_count = crate::text::face_cache::face_parse_count();

    if pipeline.shape(&long, &style, &ctx).is_err() {
      return;
    }
    let long_count = crate::text::face_cache::face_parse_count();

    assert!(
      long_count.saturating_sub(short_count) < 10,
      "face parse count should not scale with text length ({} -> {})",
      short_count,
      long_count
    );
  }

  #[cfg(debug_assertions)]
  #[test]
  fn rustybuzz_faces_are_cached_across_runs() {
    let font_data = match fs::read(crate::testing::tests_dir().join("fonts/ColorTestCOLR.ttf")) {
      Ok(data) => data,
      Err(_) => return,
    };

    let mut db = FontDatabase::empty();
    if db.load_font_data(font_data).is_err() {
      return;
    }
    let ctx = FontContext::with_database(Arc::new(db));

    let mut style = ComputedStyle::default();
    style.font_family = vec!["ColorTestCOLR".to_string()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();
    let _guard = crate::text::face_cache::RustybuzzFaceParseCountGuard::start();

    let first = match pipeline.shape("A", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if first.is_empty() {
      return;
    }
    assert!(
      first.iter().all(|run| run.font.family == "ColorTestCOLR"),
      "shaping should use the loaded ColorTestCOLR font"
    );
    let first_count = crate::text::face_cache::rustybuzz_face_parse_count();
    assert_eq!(
      first_count, 1,
      "expected exactly one rustybuzz face parse for first shape"
    );

    let second = match pipeline.shape("B", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if second.is_empty() {
      return;
    }
    assert!(
      second.iter().all(|run| run.font.family == "ColorTestCOLR"),
      "subsequent shaping should keep using the cached color font"
    );

    let second_count = crate::text::face_cache::rustybuzz_face_parse_count();
    assert_eq!(
      second_count, first_count,
      "rustybuzz face should be cached between shaping runs"
    );
  }

  #[test]
  fn font_stretch_maps_to_wdth_axis_when_present() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"wdth")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();

    let base = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if base.is_empty() {
      return;
    }
    let base_adv: f32 = base.iter().map(|r| r.advance).sum();

    style.font_stretch = FontStretch::from_percentage(75.0);
    let narrow = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if narrow.is_empty() {
      return;
    }
    let narrow_adv: f32 = narrow.iter().map(|r| r.advance).sum();

    style.font_stretch = FontStretch::from_percentage(125.0);
    let wide = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if wide.is_empty() {
      return;
    }
    let wide_adv: f32 = wide.iter().map(|r| r.advance).sum();

    assert!(
      narrow_adv < base_adv,
      "narrow stretch should reduce advance with wdth axis"
    );
    assert!(
      wide_adv > base_adv,
      "wide stretch should increase advance with wdth axis"
    );
    assert!(
      (wide_adv - narrow_adv) > 1.0,
      "wdth axis should produce a noticeable difference"
    );
  }

  #[test]
  fn auto_variations_clamp_to_axis_bounds() {
    const ROBOTO_FLEX: &[u8] = include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf");

    let mut db = FontDatabase::empty();
    db.load_font_data(ROBOTO_FLEX.to_vec())
      .expect("load Roboto Flex fixture");
    let ctx = FontContext::with_database(Arc::new(db));

    let face_info = ctx
      .database()
      .faces()
      .next()
      .expect("Roboto Flex face is available");
    let family = face_info
      .families
      .first()
      .map(|(name, _)| name.clone())
      .expect("Roboto Flex has a family name");
    let face =
      ttf_parser::Face::parse(ROBOTO_FLEX, face_info.index).expect("parse Roboto Flex face");
    let axes: Vec<_> = face.variation_axes().into_iter().collect();
    let wght_tag = Tag::from_bytes(b"wght");
    let wdth_tag = Tag::from_bytes(b"wdth");
    let opsz_tag = Tag::from_bytes(b"opsz");
    let wght_axis = axes
      .iter()
      .find(|a| a.tag == wght_tag)
      .expect("Roboto Flex exposes wght axis");
    let wdth_axis = axes
      .iter()
      .find(|a| a.tag == wdth_tag)
      .expect("Roboto Flex exposes wdth axis");
    let opsz_axis = axes
      .iter()
      .find(|a| a.tag == opsz_tag)
      .expect("Roboto Flex exposes opsz axis");

    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_weight = FontWeight::Number(1);
    style.font_stretch = FontStretch::from_percentage(200.0);
    style.font_size = opsz_axis.max_value + 1000.0;
    style.font_optical_sizing = crate::style::types::FontOpticalSizing::Auto;

    let runs = ShapingPipeline::new()
      .shape("RobotoFlex", &style, &ctx)
      .expect("shape with Roboto Flex fixture");
    assert!(
      !runs.is_empty(),
      "fixture font should shape test string without fallback"
    );
    let run = &runs[0];
    assert_eq!(run.font.family, family);
    let variations = &run.variations;

    let wght_value = variations
      .iter()
      .find(|v| v.tag == wght_tag)
      .map(|v| v.value)
      .expect("auto variations should include wght");
    let wdth_value = variations
      .iter()
      .find(|v| v.tag == wdth_tag)
      .map(|v| v.value)
      .expect("auto variations should include wdth");
    let opsz_value = variations
      .iter()
      .find(|v| v.tag == opsz_tag)
      .map(|v| v.value)
      .expect("auto variations should include opsz when optical sizing is auto");

    let expected_wght =
      (style.font_weight.to_u16() as f32).clamp(wght_axis.min_value, wght_axis.max_value);
    let expected_wdth = style
      .font_stretch
      .to_percentage()
      .clamp(wdth_axis.min_value, wdth_axis.max_value);
    let expected_opsz = style
      .font_size
      .clamp(opsz_axis.min_value, opsz_axis.max_value);

    assert!(
      (wght_value - expected_wght).abs() < 0.001,
      "wght axis should clamp to font bounds (expected {expected_wght}, got {wght_value})"
    );
    assert!(
      (wdth_value - expected_wdth).abs() < 0.001,
      "wdth axis should clamp to font bounds (expected {expected_wdth}, got {wdth_value})"
    );
    assert!(
      (opsz_value - expected_opsz).abs() < 0.001,
      "opsz axis should clamp to font bounds (expected {expected_opsz}, got {opsz_value})"
    );
  }

  #[test]
  fn authored_font_variations_override_auto_axes() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"wdth")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;

    let pipeline = ShapingPipeline::new();

    style.font_stretch = FontStretch::from_percentage(75.0);
    style.font_variation_settings = vec![FontVariationSetting {
      tag: *b"wdth",
      value: 100.0,
    }]
    .into();
    let forced = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if forced.is_empty() {
      return;
    }
    let forced_adv: f32 = forced.iter().map(|r| r.advance).sum();

    style.font_variation_settings = Vec::new().into();
    style.font_stretch = FontStretch::Normal;
    let baseline = match pipeline.shape("mmmm", &style, &ctx) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if baseline.is_empty() {
      return;
    }
    let baseline_adv: f32 = baseline.iter().map(|r| r.advance).sum();

    assert!(
      (forced_adv - baseline_adv).abs() < 0.25,
      "authored wdth should override font-stretch mapping"
    );
  }

  #[test]
  fn oblique_angle_maps_to_slnt_axis_with_clamp() {
    let Some((ctx, data, index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"slnt")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;
    style.font_style = CssFontStyle::Oblique(Some(20.0));

    let runs = match assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    ) {
      Ok(runs) => runs,
      Err(_) => return,
    };
    if runs.is_empty() {
      return;
    }
    let slnt_tag = Tag::from_bytes(b"slnt");
    let slnt_value = runs[0]
      .variations
      .iter()
      .find(|v| v.tag == slnt_tag)
      .map(|v| v.value)
      .expect("slnt axis should be populated for oblique styles");

    let face = match ttf_parser::Face::parse(&data, index) {
      Ok(face) => face,
      Err(_) => return,
    };
    let axis = face
      .variation_axes()
      .into_iter()
      .find(|a| a.tag == slnt_tag)
      .expect("variable font exposes slnt axis");
    let expected = (-20.0_f32).clamp(axis.min_value, axis.max_value);
    assert!((slnt_value - expected).abs() < 0.001);
  }

  #[test]
  fn authored_slnt_variation_preserves_authored_value() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"slnt")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family].into();
    style.font_size = 16.0;
    style.font_style = CssFontStyle::Oblique(Some(10.0));
    style.font_variation_settings = vec![FontVariationSetting {
      tag: *b"slnt",
      value: -5.0,
    }]
    .into();

    let runs = assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    )
    .expect("assign fonts");
    let slnt_tag = Tag::from_bytes(b"slnt");
    let slnt_value = runs[0]
      .variations
      .iter()
      .find(|v| v.tag == slnt_tag)
      .map(|v| v.value)
      .expect("authored slnt should survive auto mapping");

    assert!((slnt_value + 5.0).abs() < 0.001);
  }

  #[test]
  fn oblique_prefers_slnt_axis_over_ital() {
    let Some((ctx, data, index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"slnt")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;
    style.font_style = CssFontStyle::Oblique(Some(12.0));

    let runs = assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    )
    .expect("assign fonts");

    let slnt_tag = Tag::from_bytes(b"slnt");
    let ital_tag = Tag::from_bytes(b"ital");
    let tags: Vec<_> = runs[0].variations.iter().map(|v| v.tag).collect();
    assert!(tags.contains(&slnt_tag), "slnt axis should be populated");
    let face = match ttf_parser::Face::parse(&data, index) {
      Ok(face) => face,
      Err(_) => return,
    };
    let has_ital_axis = face.variation_axes().into_iter().any(|a| a.tag == ital_tag);
    if has_ital_axis {
      assert!(
        !tags.contains(&ital_tag),
        "ital axis should not be set when slnt axis is available for oblique"
      );
    }
    let axis = face
      .variation_axes()
      .into_iter()
      .find(|a| a.tag == slnt_tag)
      .expect("variable font exposes slnt axis");
    let slnt_value = runs[0]
      .variations
      .iter()
      .find(|v| v.tag == slnt_tag)
      .map(|v| v.value)
      .unwrap();
    let expected = (-12.0_f32).clamp(axis.min_value, axis.max_value);
    assert!((slnt_value - expected).abs() < 0.001);
  }

  #[test]
  fn italic_maps_to_available_axis() {
    let ital_tag = Tag::from_bytes(b"ital");
    let slnt_tag = Tag::from_bytes(b"slnt");
    let Some((ctx, data, index, family)) = variable_font_context_with_axes(&[ital_tag])
      .or_else(|| variable_font_context_with_axes(&[slnt_tag]))
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;
    style.font_style = CssFontStyle::Italic;

    let runs = assign_fonts(
      &[ItemizedRun {
        text: "mmmm".to_string(),
        start: 0,
        end: 4,
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    )
    .expect("assign fonts");
    if runs.is_empty() {
      return;
    }
    let face = match ttf_parser::Face::parse(&data, index) {
      Ok(face) => face,
      Err(_) => return,
    };
    let has_ital_axis = face.variation_axes().into_iter().any(|a| a.tag == ital_tag);

    if has_ital_axis {
      let ital_value = runs[0]
        .variations
        .iter()
        .find(|v| v.tag == ital_tag)
        .map(|v| v.value);
      assert_eq!(ital_value, Some(1.0));
      assert!(
        runs[0].variations.iter().all(|v| v.tag != slnt_tag),
        "italic should not use slnt when ital axis is present"
      );
    } else {
      let slnt_value = runs[0]
        .variations
        .iter()
        .find(|v| v.tag == slnt_tag)
        .map(|v| v.value)
        .expect("italic should map to slnt when ital axis is absent");
      let axis = face
        .variation_axes()
        .into_iter()
        .find(|a| a.tag == slnt_tag)
        .expect("variable font exposes slnt axis");
      let expected = (-DEFAULT_OBLIQUE_ANGLE_DEG).clamp(axis.min_value, axis.max_value);
      assert!((slnt_value - expected).abs() < 0.001);
    }
  }

  #[test]
  fn slnt_axis_disables_synthetic_slant() {
    let Some((ctx, _data, _index, family)) =
      variable_font_context_with_axes(&[Tag::from_bytes(b"slnt")])
    else {
      return;
    };
    let mut style = ComputedStyle::default();
    style.font_family = vec![family.clone()].into();
    style.font_size = 16.0;
    style.font_style = CssFontStyle::Oblique(Some(10.0));

    let font = ctx
      .get_font_full(
        &style.font_family,
        style.font_weight.to_u16(),
        DbFontStyle::Normal,
        DbFontStretch::from_percentage(style.font_stretch.to_percentage()),
      )
      .expect("load font with slnt axis");
    let (_, synthetic_slant) = compute_synthetic_styles(&style, &font);
    assert_eq!(
      synthetic_slant, 0.0,
      "slnt axis should satisfy slant without synthesis"
    );
  }

  #[test]
  fn slope_preferences_follow_css_slope_order() {
    use crate::text::font_db::FontStyle as DbStyle;
    assert_eq!(slope_preference_order(DbStyle::Normal), &[DbStyle::Normal]);
    assert_eq!(
      slope_preference_order(DbStyle::Italic),
      &[DbStyle::Italic, DbStyle::Oblique, DbStyle::Normal]
    );
    assert_eq!(
      slope_preference_order(DbStyle::Oblique),
      &[DbStyle::Oblique, DbStyle::Italic, DbStyle::Normal]
    );
  }

  #[test]
  fn weight_preferences_prefer_closest_with_bias() {
    let order_350 = weight_preference_order(350);
    assert!(order_350.iter().position(|w| *w == 300) < order_350.iter().position(|w| *w == 400));
    let order_600 = weight_preference_order(600);
    assert!(order_600.iter().position(|w| *w == 700) < order_600.iter().position(|w| *w == 500));
    let order_450 = weight_preference_order(450);
    assert!(
      order_450.iter().position(|w| *w == 500) < order_450.iter().position(|w| *w == 400),
      "for weights between 400-500, heavier weights up to 500 should be preferred first"
    );
  }

  #[test]
  fn stretch_preferences_follow_css_ordering() {
    use crate::text::font_db::FontStretch as DbStretch;
    let order_narrow = stretch_preference_order(DbStretch::SemiCondensed);
    assert!(
      order_narrow.iter().position(|s| *s == DbStretch::Condensed)
        < order_narrow.iter().position(|s| *s == DbStretch::Normal),
      "when desired stretch is below 100%, narrower widths are tried before wider ones"
    );

    let order_wide = stretch_preference_order(DbStretch::Expanded);
    assert!(
      order_wide
        .iter()
        .position(|s| *s == DbStretch::ExtraExpanded)
        < order_wide.iter().position(|s| *s == DbStretch::Normal),
      "when desired stretch is above 100%, wider widths are tried before narrower ones"
    );
  }

  #[test]
  fn vertical_sideways_text_marks_runs_rotated() {
    let mut style = ComputedStyle::default();
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    style.text_orientation = crate::style::types::TextOrientation::Sideways;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert!(shaped.iter().all(|r| r.rotation == RunRotation::Cw90));
  }

  #[test]
  fn vertical_sideways_left_rotates_runs_counter_clockwise() {
    let mut style = ComputedStyle::default();
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    style.text_orientation = crate::style::types::TextOrientation::SidewaysLeft;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert!(
      shaped.iter().all(|r| r.rotation == RunRotation::Ccw90),
      "sideways-left should rotate runs counter-clockwise"
    );
  }

  #[test]
  fn sideways_writing_rotates_all_runs_regardless_of_text_orientation() {
    let mut style = ComputedStyle::default();
    style.writing_mode = crate::style::types::WritingMode::SidewaysRl;
    style.text_orientation = crate::style::types::TextOrientation::Upright;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc本", &style, &ctx).unwrap();
    assert!(
            shaped.iter().all(|r| r.rotation == RunRotation::Cw90),
            "sideways writing should set horizontal typographic mode and rotate text regardless of text-orientation"
        );
  }

  #[test]
  fn sideways_writing_uses_horizontal_metrics() {
    let mut style = ComputedStyle::default();
    style.writing_mode = crate::style::types::WritingMode::SidewaysLr;
    style.text_orientation = crate::style::types::TextOrientation::Mixed;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert!(
      shaped.iter().all(|r| r.rotation == RunRotation::Ccw90),
      "sideways-lr should rotate glyphs counter-clockwise (toward the left) while still using horizontal metrics"
    );
  }

  #[test]
  fn sideways_writing_rl_rotates_runs_clockwise() {
    let mut style = ComputedStyle::default();
    style.writing_mode = crate::style::types::WritingMode::SidewaysRl;
    style.text_orientation = crate::style::types::TextOrientation::Mixed;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc", &style, &ctx).unwrap();
    assert!(
      shaped.iter().all(|r| r.rotation == RunRotation::Cw90),
      "sideways-rl should rotate glyphs clockwise (toward the right)"
    );
  }

  #[test]
  fn upright_in_vertical_forces_ltr_bidi_and_preserves_order() {
    let mut style = ComputedStyle::default();
    style.direction = crate::style::types::Direction::Rtl;
    style.writing_mode = crate::style::types::WritingMode::VerticalRl;
    style.text_orientation = crate::style::types::TextOrientation::Upright;

    let text = "אבג abc";
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape(text, &style, &ctx).unwrap();
    assert!(
      shaped.iter().all(|r| r.direction == Direction::LeftToRight),
      "upright should force LTR direction for all runs in vertical text"
    );
    let reconstructed: String = shaped.iter().flat_map(|r| r.text.chars()).collect();
    assert_eq!(
      reconstructed, text,
      "upright should avoid bidi reordering and preserve textual order"
    );
  }

  #[test]
  fn vertical_mixed_rotates_non_upright_segments() {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalRl;
    style.text_orientation = TextOrientation::Mixed;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new()
      .shape("A。B本", &style, &ctx)
      .unwrap();
    assert!(shaped.iter().any(|r| r.rotation == RunRotation::Cw90));
    assert!(shaped.iter().any(|r| r.rotation == RunRotation::None));
  }

  #[test]
  fn vertical_upright_forces_all_runs_upright() {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalLr;
    style.text_orientation = TextOrientation::Upright;
    let ctx = FontContext::new();
    let shaped = ShapingPipeline::new().shape("Abc本", &style, &ctx).unwrap();
    assert!(shaped.iter().all(|r| r.rotation == RunRotation::None));
  }

  #[test]
  fn collect_features_respects_ligature_variants_and_overrides() {
    let mut style = ComputedStyle::default();
    style.font_variant_ligatures = FontVariantLigatures {
      common: false,
      discretionary: true,
      historical: false,
      contextual: false,
    };
    style.font_feature_settings = vec![FontFeatureSetting {
      tag: *b"liga",
      value: 1,
    }]
    .into();

    let feats = collect_opentype_features(&style, "serif");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert_eq!(seen.get(b"liga"), Some(&1));
    assert_eq!(seen.get(b"clig"), Some(&0));
    assert_eq!(seen.get(b"dlig"), Some(&1));
    assert_eq!(seen.get(b"calt"), Some(&0));
  }

  #[test]
  fn collect_features_includes_numeric_variants_and_kerning() {
    let mut style = ComputedStyle::default();
    style.font_variant_numeric.figure = NumericFigure::Oldstyle;
    style.font_variant_numeric.spacing = NumericSpacing::Tabular;
    style.font_variant_numeric.fraction = NumericFraction::Stacked;
    style.font_variant_numeric.ordinal = true;
    style.font_variant_numeric.slashed_zero = true;
    style.font_kerning = FontKerning::None;
    style.font_variant_east_asian.variant = Some(EastAsianVariant::Jis04);
    style.font_variant_east_asian.width = Some(EastAsianWidth::FullWidth);
    style.font_variant_east_asian.ruby = true;
    style.font_variant_position = FontVariantPosition::Super;

    let feats = collect_opentype_features(&style, "serif");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert_eq!(seen.get(b"onum"), Some(&1));
    assert_eq!(seen.get(b"tnum"), Some(&1));
    assert_eq!(seen.get(b"afrc"), Some(&1));
    assert_eq!(seen.get(b"ordn"), Some(&1));
    assert_eq!(seen.get(b"zero"), Some(&1));
    assert_eq!(seen.get(b"kern"), Some(&0));
    assert_eq!(seen.get(b"jp04"), Some(&1));
    assert_eq!(seen.get(b"fwid"), Some(&1));
    assert_eq!(seen.get(b"ruby"), Some(&1));
    assert_eq!(seen.get(b"sups"), Some(&1));
  }

  #[test]
  fn collect_features_auto_enables_kerning() {
    let style = ComputedStyle::default();
    let feats = collect_opentype_features(&style, "serif");
    assert_eq!(
      tag_value(&feats, b"kern"),
      Some(1),
      "expected font-kerning:auto to enable the OpenType kern feature"
    );
  }

  #[test]
  fn collect_features_includes_caps_variants() {
    let mut style = ComputedStyle::default();
    style.font_variant_caps = FontVariantCaps::AllSmallCaps;

    let feats = collect_opentype_features(&style, "serif");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert_eq!(seen.get(b"smcp"), Some(&1));
    assert_eq!(seen.get(b"c2sc"), Some(&1));

    style.font_variant_caps = FontVariantCaps::AllPetiteCaps;
    let feats = collect_opentype_features(&style, "serif");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert_eq!(seen.get(b"pcap"), Some(&1));
    assert_eq!(seen.get(b"c2pc"), Some(&1));

    style.font_variant_caps = FontVariantCaps::Unicase;
    let feats = collect_opentype_features(&style, "serif");
    assert!(feats.iter().any(|f| f.tag.to_bytes() == *b"unic"));
  }

  #[test]
  fn collect_features_includes_historical_forms_only() {
    let mut style = ComputedStyle::default();
    style.font_variant_alternates.historical_forms = true;
    style.font_variant_alternates.stylistic = Some(FontVariantAlternateValue::Number(3));
    style.font_variant_alternates.stylesets = vec![
      FontVariantAlternateValue::Number(1),
      FontVariantAlternateValue::Number(2),
    ];
    style.font_variant_alternates.character_variants = vec![FontVariantAlternateValue::Number(4)];
    style.font_variant_alternates.swash = Some(FontVariantAlternateValue::Number(1));
    style.font_variant_alternates.ornaments = Some(FontVariantAlternateValue::Number(2));
    style.font_variant_alternates.annotation = Some(FontVariantAlternateValue::Number(1));

    let feats = collect_opentype_features(&style, "serif");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert_eq!(seen.get(b"hist"), Some(&1));
    assert_eq!(seen.get(b"salt"), Some(&3));
    assert!(
      seen.get(b"ss03").is_none(),
      "stylistic() should map to OpenType salt (not ssNN)"
    );
    assert_eq!(seen.get(b"ss01"), Some(&1));
    assert_eq!(seen.get(b"ss02"), Some(&1));
    assert_eq!(seen.get(b"cv04"), Some(&1));
    assert_eq!(seen.get(b"swsh"), Some(&1));
    assert_eq!(seen.get(b"cswh"), Some(&1));
    assert_eq!(seen.get(b"ornm"), Some(&2));
    assert_eq!(
      seen.get(b"nalt"),
      Some(&1),
      "annotation() should map to OpenType nalt"
    );
  }

  fn tag_value(features: &[Feature], tag: &[u8; 4]) -> Option<u32> {
    features
      .iter()
      .find(|f| f.tag.to_bytes() == *tag)
      .map(|f| f.value)
  }

  #[test]
  fn named_alternates_resolve_styleset() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("disambiguation".to_string(), vec![2u32])]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style
      .font_variant_alternates
      .stylesets
      .push(FontVariantAlternateValue::Name(
        "disambiguation".to_string(),
      ));

    let features = collect_opentype_features(&style, "Inter");
    assert_eq!(tag_value(&features, b"ss02"), Some(1));
  }

  #[test]
  fn missing_named_alternate_emits_no_feature_tags() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("disambiguation".to_string(), vec![2u32])]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style
      .font_variant_alternates
      .stylesets
      .push(FontVariantAlternateValue::Name("unknown".to_string()));

    let features = collect_opentype_features(&style, "Inter");
    assert!(
      tag_value(&features, b"ss02").is_none(),
      "unknown named alternate should not emit OpenType tags"
    );
  }

  #[test]
  fn font_feature_settings_override_named_alternates() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Styleset,
      FxHashMap::from_iter([("disambiguation".to_string(), vec![2u32])]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style
      .font_variant_alternates
      .stylesets
      .push(FontVariantAlternateValue::Name(
        "disambiguation".to_string(),
      ));
    style.font_feature_settings = Arc::from([FontFeatureSetting {
      tag: *b"ss02",
      value: 0,
    }]);

    let features = collect_opentype_features(&style, "Inter");
    assert_eq!(tag_value(&features, b"ss02"), Some(0));
  }

  #[test]
  fn named_alternates_swash_emits_both_features() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Swash,
      FxHashMap::from_iter([("flowing".to_string(), vec![3u32])]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.swash =
      Some(FontVariantAlternateValue::Name("flowing".to_string()));

    let features = collect_opentype_features(&style, "Inter");
    assert_eq!(tag_value(&features, b"swsh"), Some(3));
    assert_eq!(tag_value(&features, b"cswh"), Some(3));
  }

  #[test]
  fn named_alternates_annotation_emits_nalt() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::Annotation,
      FxHashMap::from_iter([("note".to_string(), vec![4u32])]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.annotation =
      Some(FontVariantAlternateValue::Name("note".to_string()));

    let features = collect_opentype_features(&style, "Inter");
    assert_eq!(tag_value(&features, b"nalt"), Some(4));
  }

  #[test]
  fn named_alternates_character_variant_last_wins() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Inter".to_string()]);
    rule.groups.insert(
      FontFeatureValueType::CharacterVariant,
      FxHashMap::from_iter([
        ("one".to_string(), vec![2u32, 1u32]),
        ("two".to_string(), vec![2u32, 7u32]),
      ]),
    );
    registry.register(rule);

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.character_variants.extend([
      FontVariantAlternateValue::Name("one".to_string()),
      FontVariantAlternateValue::Name("two".to_string()),
    ]);

    let features = collect_opentype_features(&style, "Inter");
    assert_eq!(tag_value(&features, b"cv02"), Some(7));
  }

  #[test]
  fn named_swash_ignores_multi_value_definitions() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Example".to_string()]);
    let mut values = FxHashMap::default();
    values.insert("fancy".to_string(), vec![3u32, 5u32]);
    rule.groups.insert(FontFeatureValueType::Swash, values);
    registry.register(rule);
    assert_eq!(
      registry.lookup("Example", FontFeatureValueType::Swash, "fancy"),
      Some([3u32, 5u32].as_slice())
    );

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.swash =
      Some(FontVariantAlternateValue::Name("fancy".to_string()));

    let feats = collect_opentype_features(&style, "Example");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert!(
      seen.get(b"swsh").is_none(),
      "expected invalid @swash definition to be ignored"
    );
    assert!(
      seen.get(b"cswh").is_none(),
      "expected invalid @swash definition to be ignored"
    );
  }

  #[test]
  fn named_annotation_ignores_multi_value_definitions() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Example".to_string()]);
    let mut values = FxHashMap::default();
    values.insert("circled".to_string(), vec![1u32, 2u32]);
    rule.groups.insert(FontFeatureValueType::Annotation, values);
    registry.register(rule);
    assert_eq!(
      registry.lookup("Example", FontFeatureValueType::Annotation, "circled"),
      Some([1u32, 2u32].as_slice())
    );

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.annotation =
      Some(FontVariantAlternateValue::Name("circled".to_string()));

    let feats = collect_opentype_features(&style, "Example");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert!(
      seen.get(b"nalt").is_none(),
      "expected invalid @annotation definition to be ignored"
    );
  }

  #[test]
  fn named_stylistic_ignores_multi_value_definitions() {
    let mut registry = FontFeatureValuesRegistry::default();
    let mut rule = FontFeatureValuesRule::new(vec!["Example".to_string()]);
    let mut values = FxHashMap::default();
    values.insert("alt".to_string(), vec![2u32, 3u32]);
    rule.groups.insert(FontFeatureValueType::Stylistic, values);
    registry.register(rule);
    assert_eq!(
      registry.lookup("Example", FontFeatureValueType::Stylistic, "alt"),
      Some([2u32, 3u32].as_slice())
    );

    let mut style = ComputedStyle::default();
    style.font_feature_values = Arc::new(registry);
    style.font_variant_alternates.stylistic =
      Some(FontVariantAlternateValue::Name("alt".to_string()));

    let feats = collect_opentype_features(&style, "Example");
    let mut seen: std::collections::HashMap<[u8; 4], u32> = std::collections::HashMap::new();
    for f in feats {
      seen.insert(f.tag.to_bytes(), f.value);
    }
    assert!(
      seen.get(b"salt").is_none(),
      "expected invalid @stylistic definition to be ignored"
    );
  }

  #[test]
  fn emoji_dominant_detection_respects_non_emoji_content() {
    assert!(is_emoji_dominant("😀 😀"));
    assert!(is_emoji_dominant("😀\u{200d}\u{1f9d1}"));
    assert!(!is_emoji_dominant("😀a"));
  }

  #[test]
  fn variation_selector_stays_with_current_font_run() {
    let ctx = FontContext::new();
    let style = ComputedStyle::default();
    let runs = assign_fonts(
      &[ItemizedRun {
        text: "A\u{fe0f}".to_string(),
        start: 0,
        end: "A\u{fe0f}".len(),
        script: Script::Latin,
        direction: Direction::LeftToRight,
        level: 0,
      }],
      &style,
      &ctx,
    )
    .expect("assign fonts");
    assert_eq!(
      runs.len(),
      1,
      "variation selector should not split font runs"
    );
    assert_eq!(runs[0].text, "A\u{fe0f}");
  }

  #[test]
  fn variation_selector_clusters_use_glyph_fallback_cache() {
    let ctx = FontContext::new();
    let style = ComputedStyle::default();
    let text = "A\u{fe0f}".to_string();
    let text_len = text.len();

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let _ = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    let stats = cache.stats();
    let cluster_lookups = stats.cluster_hits + stats.cluster_misses;
    assert_eq!(
      cluster_lookups, 0,
      "expected variation selector clusters to use glyph fallback caching (cluster lookups={cluster_lookups})"
    );
    let glyph_lookups = stats.glyph_hits + stats.glyph_misses;
    assert!(
      glyph_lookups > 0,
      "expected variation selector clusters to consult glyph fallback cache"
    );
  }

  #[test]
  fn keycap_sequences_use_cluster_fallback_cache() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    let style = ComputedStyle::default();
    let text = "1\u{fe0f}\u{20e3}".to_string();
    let text_len = text.len();

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let font_runs = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");
    assert!(
      !font_runs.is_empty(),
      "expected bundled fonts to cover keycap sequence"
    );
    assert!(
      font_runs
        .iter()
        .any(|run| FontDatabase::family_name_is_emoji_font(&run.font.family)),
      "expected keycap sequence to select an emoji font (got {:?})",
      font_runs
        .iter()
        .map(|run| run.font.family.as_str())
        .collect::<Vec<_>>()
    );

    let stats = cache.stats();
    let cluster_lookups = stats.cluster_hits + stats.cluster_misses;
    assert!(
      cluster_lookups > 0,
      "expected keycap sequence clusters to consult cluster fallback cache (cluster lookups={cluster_lookups})"
    );
  }

  #[test]
  fn tag_sequences_use_cluster_fallback_cache_without_glyph_churn() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    let style = ComputedStyle::default();
    // Emoji tag sequence for the Scotland flag (🏴).
    let text = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}".to_string();
    let text_len = text.len();

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let font_runs = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");
    assert!(
      !font_runs.is_empty(),
      "expected bundled fonts to cover tag sequence"
    );
    assert!(
      font_runs
        .iter()
        .any(|run| FontDatabase::family_name_is_emoji_font(&run.font.family)),
      "expected tag sequence to select an emoji font (got {:?})",
      font_runs
        .iter()
        .map(|run| run.font.family.as_str())
        .collect::<Vec<_>>()
    );

    let stats = cache.stats();
    let cluster_lookups = stats.cluster_hits + stats.cluster_misses;
    let glyph_lookups = stats.glyph_hits + stats.glyph_misses;
    assert!(
      cluster_lookups > 0,
      "expected tag sequence clusters to consult cluster fallback cache (cluster lookups={cluster_lookups})"
    );
    assert_eq!(
      glyph_lookups, 0,
      "expected tag sequence clusters to avoid per-codepoint glyph cache lookups (glyph lookups={glyph_lookups})"
    );
  }

  #[test]
  fn repeated_zwj_sequences_hit_cluster_fallback_cache() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    ctx.clear_web_fonts();
    let style = ComputedStyle::default();

    // Woman scientist (👩‍🔬), a ZWJ emoji sequence covered by the bundled emoji font.
    let zwj_sequence = "👩\u{200d}🔬";
    let repetitions = 128usize;
    let mut text = String::new();
    for _ in 0..repetitions {
      // Include ASCII in between so we switch back to a non-emoji primary font, forcing
      // repeated resolution of the ZWJ cluster via the fallback cache.
      text.push('a');
      text.push_str(zwj_sequence);
    }
    let text_len = text.len();

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let font_runs = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");
    assert!(
      font_runs
        .iter()
        .any(|run| FontDatabase::family_name_is_emoji_font(&run.font.family)),
      "expected ZWJ sequence to select an emoji font (got {:?})",
      font_runs
        .iter()
        .map(|run| run.font.family.as_str())
        .collect::<Vec<_>>()
    );

    let stats = cache.stats();
    assert!(
      stats.cluster_hits > 0,
      "expected repeated ZWJ sequences to hit the cluster fallback cache"
    );
    assert!(
      stats.cluster_misses <= 2,
      "expected repeated ZWJ sequences to avoid cluster cache churn (cluster misses={}, hits={})",
      stats.cluster_misses,
      stats.cluster_hits
    );
    assert!(
      stats.cluster_hits >= repetitions.saturating_sub(2) as u64,
      "expected most repeated ZWJ clusters to be cache hits (hits={}, misses={}, repetitions={repetitions})",
      stats.cluster_hits,
      stats.cluster_misses
    );
  }

  #[test]
  fn mark_relax_retry_uses_glyph_fallback_cache() {
    let ctx = FontContext::with_config(FontConfig::bundled_only());
    let style = ComputedStyle::default();
    let db = ctx.database();

    let candidates = [
      '\u{1AB0}', '\u{1AB1}', '\u{1AB2}', '\u{1AB3}', '\u{1AB4}', '\u{1ABE}', '\u{1AC0}',
      '\u{1AC1}', '\u{1AC2}',
    ];

    let missing_mark = candidates
      .into_iter()
      .find(|ch| !db.faces().any(|face| db.has_glyph_cached(face.id, *ch)))
      .expect("expected at least one missing combining mark in bundled fonts");

    let text = format!("中{missing_mark}");
    let text_len = text.len();

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Han,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let _ = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    let stats = cache.stats();
    let glyph_lookups = stats.glyph_hits + stats.glyph_misses;
    assert!(
      glyph_lookups > 0,
      "expected mark-relax fallback to consult glyph fallback cache (glyph lookups={glyph_lookups})"
    );
  }

  #[test]
  fn ascii_runs_do_not_hit_fallback_cache_per_cluster() {
    let ctx = FontContext::new();
    let style = ComputedStyle::default();
    // Include an ASCII control character (newline) to ensure the ASCII fast path still applies
    // without requiring glyph coverage for it.
    let text = "a\n".repeat(5_000);
    let text_len = text.len();
    assert!(text.is_ascii());

    let run = ItemizedRun {
      text,
      start: 0,
      end: text_len,
      script: Script::Latin,
      direction: Direction::LeftToRight,
      level: 0,
    };

    let cache = FallbackCache::new(256);
    let _ = assign_fonts_internal(
      &[run],
      &style,
      &ctx,
      Some(&cache),
      ctx.font_generation(),
      true,
    )
    .expect("assign fonts");

    let stats = cache.stats();
    let lookups = stats.glyph_hits + stats.glyph_misses + stats.cluster_hits + stats.cluster_misses;
    assert!(
      lookups <= 4,
      "expected ASCII fast path to avoid per-cluster fallback cache lookups (saw {})",
      lookups
    );
  }

  #[test]
  fn emoji_preference_matches_variant_and_character() {
    assert_eq!(
      emoji_preference_for_char('😀', FontVariantEmoji::Emoji),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_for_char('😀', FontVariantEmoji::Text),
      EmojiPreference::AvoidEmoji
    );
    assert_eq!(
      emoji_preference_for_char('😀', FontVariantEmoji::Unicode),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_for_char('A', FontVariantEmoji::Emoji),
      EmojiPreference::Neutral
    );
    assert_eq!(
      emoji_preference_for_char('A', FontVariantEmoji::Text),
      EmojiPreference::Neutral
    );
    // '#' defaults to text presentation.
    assert_eq!(
      emoji_preference_for_char('#', FontVariantEmoji::Unicode),
      EmojiPreference::AvoidEmoji
    );
  }

  #[test]
  fn emoji_variation_selectors_override_property_preference() {
    assert_eq!(
      emoji_preference_for_cluster("😀\u{fe0e}", FontVariantEmoji::Emoji),
      EmojiPreference::AvoidEmoji
    );
    assert_eq!(
      emoji_preference_for_cluster("😀\u{fe0f}", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn zwj_sequences_prefer_emoji_fonts() {
    assert_eq!(
      emoji_preference_for_cluster("👩\u{200d}🔬", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn keycap_sequences_prefer_emoji_unless_forced_text() {
    assert_eq!(
      emoji_preference_for_cluster("1\u{20e3}", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_for_cluster("1\u{fe0f}\u{20e3}", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
    assert_eq!(
      emoji_preference_for_cluster("1\u{fe0e}\u{20e3}", FontVariantEmoji::Emoji),
      EmojiPreference::AvoidEmoji
    );
  }

  #[test]
  fn keycap_sequences_keep_keycap_mark_required_for_coverage() {
    let mut required = ClusterCharBuf::new();
    let coverage = ['1', '\u{20e3}'];
    let required_slice =
      required_coverage_chars_for_cluster("1\u{20e3}", '1', '1', &coverage, &mut required);
    assert_eq!(
      required_slice, &coverage,
      "keycap clusters should keep U+20E3 required for font coverage"
    );
  }

  #[test]
  fn optional_mark_fallback_drops_marks_from_required_coverage() {
    let mut required = ClusterCharBuf::new();
    let coverage = ['a', '\u{0301}'];
    let required_slice =
      required_coverage_chars_for_cluster("a\u{0301}", 'a', 'a', &coverage, &mut required);
    assert_eq!(
      required_slice,
      &['a'],
      "combining marks should be treated as optional for font coverage when possible"
    );
  }

  #[test]
  fn zwj_sequences_prefer_emoji_even_when_zwj_after_modifier() {
    assert_eq!(
      emoji_preference_for_cluster("👩\u{1f3fb}\u{200d}🔬", FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  #[test]
  fn tag_sequences_prefer_emoji_fonts() {
    // England subdivision flag tag sequence.
    let england = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}";
    assert_eq!(
      emoji_preference_for_cluster(england, FontVariantEmoji::Text),
      EmojiPreference::PreferEmoji
    );
  }

  fn dummy_font(name: &str) -> Arc<LoadedFont> {
    Arc::new(LoadedFont {
      id: None,
      data: Arc::new(Vec::new()),
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: name.into(),
      weight: DbFontWeight::NORMAL,
      style: DbFontStyle::Normal,
      stretch: DbFontStretch::Normal,
    })
  }

  #[test]
  fn emoji_preference_picker_prefers_emoji_when_available() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("Noto Color Emoji");
    let mut picker = FontPreferencePicker::new(EmojiPreference::PreferEmoji);
    let idx = picker.bump_order();
    assert!(picker.consider(text_font.clone(), false, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(emoji_font.clone(), true, idx)
      .expect("should pick emoji font");
    assert_eq!(chosen.family.as_str(), emoji_font.family.as_str());
  }

  #[test]
  fn emoji_preference_picker_falls_back_to_text_when_no_emoji_font() {
    let text_font = dummy_font("Example Text");
    let mut picker = FontPreferencePicker::new(EmojiPreference::PreferEmoji);
    let idx = picker.bump_order();
    assert!(picker.consider(text_font.clone(), false, idx).is_none());
    let chosen = picker.finish().expect("fallback text font");
    assert_eq!(chosen.family.as_str(), text_font.family.as_str());
  }

  #[test]
  fn emoji_preference_picker_avoids_emoji_but_allows_fallback() {
    let emoji_font = dummy_font("Twemoji");
    let mut picker = FontPreferencePicker::new(EmojiPreference::AvoidEmoji);
    let idx = picker.bump_order();
    assert!(picker.consider(emoji_font.clone(), true, idx).is_none());
    let chosen = picker.finish().expect("fallback emoji font");
    assert_eq!(chosen.family.as_str(), emoji_font.family.as_str());
  }

  #[test]
  fn font_variant_emoji_text_prefers_text_fonts() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("Noto Color Emoji");
    let pref = emoji_preference_for_char('😀', FontVariantEmoji::Text);
    assert_eq!(pref, EmojiPreference::AvoidEmoji);
    let mut picker = FontPreferencePicker::new(pref);
    let idx = picker.bump_order();
    assert!(picker.consider(emoji_font.clone(), true, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(text_font.clone(), false, idx)
      .expect("should pick text font when avoiding emoji");
    assert_eq!(chosen.family.as_str(), text_font.family.as_str());
  }

  #[test]
  fn font_variant_emoji_emoji_prefers_emoji_fonts() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("Twemoji");
    let pref = emoji_preference_for_char('😀', FontVariantEmoji::Emoji);
    assert_eq!(pref, EmojiPreference::PreferEmoji);
    let mut picker = FontPreferencePicker::new(pref);
    let idx = picker.bump_order();
    assert!(picker.consider(text_font.clone(), false, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(emoji_font.clone(), true, idx)
      .expect("should pick emoji font for emoji preference");
    assert_eq!(chosen.family.as_str(), emoji_font.family.as_str());
  }

  #[test]
  fn font_variant_emoji_unicode_prefers_text_for_non_emoji() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("EmojiOne");
    let pref = emoji_preference_for_char('#', FontVariantEmoji::Unicode);
    assert_eq!(pref, EmojiPreference::AvoidEmoji);
    let mut picker = FontPreferencePicker::new(pref);
    let idx = picker.bump_order();
    assert!(picker.consider(emoji_font.clone(), true, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(text_font.clone(), false, idx)
      .expect("unicode text should pick text font for non-emoji chars");
    assert_eq!(chosen.family.as_str(), text_font.family.as_str());
  }

  #[test]
  fn emoji_variation_selector_fe0e_forces_text_font() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("Noto Color Emoji");
    let pref = emoji_preference_for_cluster("😀\u{fe0e}", FontVariantEmoji::Emoji);
    assert_eq!(pref, EmojiPreference::AvoidEmoji);
    let mut picker = FontPreferencePicker::new(pref);
    let idx = picker.bump_order();
    assert!(picker.consider(emoji_font.clone(), true, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(text_font.clone(), false, idx)
      .expect("FE0E should force text presentation");
    assert_eq!(chosen.family.as_str(), text_font.family.as_str());
  }

  #[test]
  fn emoji_variation_selector_fe0f_prefers_emoji_font() {
    let text_font = dummy_font("Example Text");
    let emoji_font = dummy_font("Twemoji");
    let pref = emoji_preference_for_cluster("😀\u{fe0f}", FontVariantEmoji::Text);
    assert_eq!(pref, EmojiPreference::PreferEmoji);
    let mut picker = FontPreferencePicker::new(pref);
    let idx = picker.bump_order();
    assert!(picker.consider(text_font.clone(), false, idx).is_none());
    let idx = picker.bump_order();
    let chosen = picker
      .consider(emoji_font.clone(), true, idx)
      .expect("FE0F should prefer emoji font even when property requests text");
    assert_eq!(chosen.family.as_str(), emoji_font.family.as_str());
  }

  #[test]
  fn text_diagnostics_session_resets_counters() {
    let _session = crate::api::DiagnosticsSessionGuard::acquire();
    enable_text_diagnostics();
    record_text_shape(None, 3, 7);
    let first = take_text_diagnostics().expect("expected diagnostics snapshot");
    assert_eq!(first.shaped_runs, 3);
    assert_eq!(first.glyphs, 7);

    enable_text_diagnostics();
    record_text_shape(None, 1, 2);
    let second = take_text_diagnostics().expect("expected diagnostics snapshot");
    assert_eq!(second.shaped_runs, 1);
    assert_eq!(second.glyphs, 2);
  }

  #[test]
  fn text_diagnostics_do_not_capture_other_threads() {
    let _session = crate::api::DiagnosticsSessionGuard::acquire();
    enable_text_diagnostics();

    std::thread::spawn(|| {
      record_text_shape(None, 5, 10);
    })
    .join()
    .expect("join thread");

    let snapshot = take_text_diagnostics().expect("expected diagnostics snapshot");
    assert_eq!(
      snapshot.shaped_runs, 0,
      "expected text diagnostics to ignore other threads"
    );
    assert_eq!(snapshot.glyphs, 0);
  }

  #[test]
  fn text_diagnostics_stage_tracks_union_time() {
    let base = Instant::now();
    let mut state = TextDiagnosticsState::new(1);
    state.start_stage(TextDiagnosticsStage::Coverage, base);
    state.start_stage(
      TextDiagnosticsStage::Coverage,
      base + Duration::from_millis(10),
    );
    state.end_stage(
      TextDiagnosticsStage::Coverage,
      base + Duration::from_millis(20),
    );
    state.end_stage(
      TextDiagnosticsStage::Coverage,
      base + Duration::from_millis(30),
    );

    assert!(
      (state.diag.coverage_ms - 30.0).abs() < 0.001,
      "expected union duration of 30ms, got {:.3}ms",
      state.diag.coverage_ms
    );
  }
}
fn number_tag(prefix: &[u8; 2], n: u32) -> Option<[u8; 4]> {
  if n == 0 || n > 99 {
    return None;
  }
  let mut tag = [b' ', b' ', b' ', b' '];
  tag[0] = prefix[0];
  tag[1] = prefix[1];
  let tens = (n / 10) % 10;
  let ones = n % 10;
  tag[2] = b'0' + (tens as u8);
  tag[3] = b'0' + (ones as u8);
  Some(tag)
}
