//! Font database - font discovery and loading
//!
//! This module provides a wrapper around the `fontdb` crate for font
//! discovery, matching, and loading. It implements CSS-compliant font
//! matching with fallback chains.
//!
//! # Overview
//!
//! The font database:
//! - Discovers system fonts on all platforms (Windows, macOS, Linux)
//! - Loads font files (TTF, OTF, TTC)
//! - Queries fonts by family, weight, style, and stretch
//! - Caches loaded font data with Arc for sharing
//! - Handles fallback chains (e.g., "Arial, Helvetica, sans-serif")
//!
//! # CSS Specification
//!
//! Font matching follows CSS Fonts Module Level 4:
//! - <https://www.w3.org/TR/css-fonts-4/#font-matching-algorithm>
//!
//! # Example
//!
//! ```rust,ignore
//! use fastrender::text::font_db::{FontDatabase, FontWeight, FontStyle};
//!
//! let db = FontDatabase::new();
//!
//! // Query for a specific font
//! if let Some(id) = db.query("Arial", FontWeight::NORMAL, FontStyle::Normal) {
//!     let font = db.load_font(id).expect("Should load font");
//!     println!("Loaded {} with {} bytes", font.family, font.data.len());
//! }
//! ```

use crate::css::types::FontFaceStyle as CssFontFaceStyle;
use crate::error::FontError;
use crate::error::Result;
use crate::style::types::FontFeatureSetting;
use crate::style::types::FontSizeAdjust;
use crate::style::types::FontSizeAdjustMetric;
use crate::style::types::FontVariationSetting;
use crate::text::emoji;
use crate::text::font_fallback::FontId;
use fontdb::Database as FontDbDatabase;
use fontdb::Family as FontDbFamily;
use fontdb::Query as FontDbQuery;
use fontdb::ID;
use lru::LruCache;
use parking_lot::Mutex;
use rustc_hash::FxHasher;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use ttf_parser::Tag;

use crate::text::face_cache::{self, CachedFace};

#[cfg(debug_assertions)]
pub use crate::text::face_cache::FaceParseCountGuard;
pub use crate::text::face_cache::{face_parse_count, reset_face_parse_counter_for_tests};

const GLYPH_COVERAGE_CACHE_SIZE: usize = 128;
const GLYPH_COVERAGE_CACHE_SHARD_TARGET: usize = 32;
const GLYPH_COVERAGE_CACHE_SHARD_LIMIT: usize = 16;
type GlyphCoverageCacheHasher = BuildHasherDefault<FxHasher>;

#[derive(Clone, Copy)]
pub(crate) struct BundledFont {
  pub(crate) name: &'static str,
  pub(crate) data: &'static [u8],
}

// Ordered from general text to narrower script fallbacks so generic families stay stable.
pub(crate) const BUNDLED_FONTS: &[BundledFont] = &[
  // Roboto Flex provides a modern variable sans face with sane line metrics, helping pages that
  // fall back to generic `sans-serif` (e.g. sites that request Helvetica/Arial but ship no web
  // fonts in offline fixtures).
  BundledFont {
    name: "Roboto Flex",
    data: include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf"),
  },
  BundledFont {
    name: "Noto Sans",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSans-subset.ttf"),
  },
  BundledFont {
    name: "FastRender Serif",
    data: include_bytes!("../../tests/fixtures/fonts/FastRenderSerif-subset.ttf"),
  },
  BundledFont {
    name: "Noto Serif",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSerif-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Mono",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansMono-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Arabic",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansArabic-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Hebrew",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansHebrew-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Devanagari",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansDevanagari-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Bengali",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansBengali-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Gurmukhi",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansGurmukhi-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Gujarati",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansGujarati-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Oriya",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansOriya-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Kannada",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansKannada-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Malayalam",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansMalayalam-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Sinhala",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSinhala-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Myanmar",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansMyanmar-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Telugu",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansTelugu-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Javanese",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansJavanese-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Tamil",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansTamil-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Thai",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansThai-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Thaana",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansThaana-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Syriac",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSyriac-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans NKo",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansNKo-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Armenian",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansArmenian-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Georgian",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansGeorgian-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Ethiopic",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansEthiopic-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Lao",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansLao-subset.ttf"),
  },
  BundledFont {
    name: "Noto Serif Tibetan",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSerifTibetan-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Cherokee",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansCherokee-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Canadian Aboriginal",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansCanadianAboriginal-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Khmer",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansKhmer-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Tai Le",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansTaiLe-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Ol Chiki",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansOlChiki-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Glagolitic",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansGlagolitic-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Tifinagh",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansTifinagh-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Syloti Nagri",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSylotiNagri-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Meetei Mayek",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansMeeteiMayek-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Gothic",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansGothic-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans SC",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSC-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans TC",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansTC-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans JP",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansJP-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans KR",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansKR-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Symbols",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSymbols-subset.ttf"),
  },
  BundledFont {
    name: "Noto Sans Symbols 2",
    data: include_bytes!("../../tests/fixtures/fonts/NotoSansSymbols2-subset.ttf"),
  },
  BundledFont {
    name: "STIX Two Math",
    data: include_bytes!("../../tests/fixtures/fonts/STIXTwoMath-Regular.otf"),
  },
  BundledFont {
    name: "DejaVu Sans",
    data: include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf"),
  },
];
pub(crate) const BUNDLED_EMOJI_FONTS: &[BundledFont] = &[BundledFont {
  name: "FastRender Emoji",
  data: include_bytes!("../../tests/fixtures/fonts/FastRenderEmoji.ttf"),
}];

pub(crate) fn bundled_font_data() -> impl Iterator<Item = &'static [u8]> {
  // Exposed for other subsystems (e.g. usvg/resvg SVG rasterization) that want to load the same
  // deterministic bundled fonts without depending on `FontDatabase` directly (which may use a
  // different `fontdb` crate version than the consumer).
  BUNDLED_FONTS.iter().map(|font| font.data)
}

pub(crate) fn bundled_emoji_font_data() -> impl Iterator<Item = &'static [u8]> {
  BUNDLED_EMOJI_FONTS.iter().map(|font| font.data)
}

fn env_flag(var: &str) -> Option<bool> {
  std::env::var(var).ok().map(|v| {
    !matches!(v.as_str(), "0" | "false" | "False" | "FALSE" | "") && !v.eq_ignore_ascii_case("off")
  })
}

fn should_load_bundled_emoji_fonts() -> bool {
  env_flag("FASTR_BUNDLE_EMOJI_FONT").unwrap_or(true)
}

pub(crate) fn bundled_emoji_fonts_enabled() -> bool {
  should_load_bundled_emoji_fonts()
}

/// Configuration for font discovery and loading.
#[derive(Debug, Clone)]
pub struct FontConfig {
  /// Whether to load system fonts discovered via fontdb/platform APIs.
  pub use_system_fonts: bool,
  /// Whether to always load the bundled fallback fonts shipped with FastRender.
  pub use_bundled_fonts: bool,
  /// Additional directories to scan for fonts.
  pub font_dirs: Vec<PathBuf>,
}

impl Default for FontConfig {
  fn default() -> Self {
    let bundled = env_flag("FASTR_USE_BUNDLED_FONTS")
      .or_else(|| env_flag("CI"))
      // The `browser_ui` feature is primarily used for interactive/headless browser UI builds
      // (including integration tests). Prefer bundled fonts by default in that configuration so
      // headless environments without a reliable system font database still render deterministically.
      .unwrap_or(cfg!(feature = "browser_ui"));
    Self {
      // In CI we prefer deterministic bundled fonts unless explicitly overridden.
      use_system_fonts: !bundled,
      use_bundled_fonts: bundled,
      font_dirs: Vec::new(),
    }
  }
}

impl FontConfig {
  /// Creates a new configuration with default values.
  pub fn new() -> Self {
    Self::default()
  }

  /// Enables or disables system font discovery.
  pub fn with_system_fonts(mut self, enabled: bool) -> Self {
    self.use_system_fonts = enabled;
    self
  }

  /// Enables or disables bundled fallback fonts.
  pub fn with_bundled_fonts(mut self, enabled: bool) -> Self {
    self.use_bundled_fonts = enabled;
    self
  }

  /// Adds a directory to search for fonts.
  pub fn add_font_dir(mut self, dir: impl Into<PathBuf>) -> Self {
    self.font_dirs.push(dir.into());
    self
  }

  /// Adds multiple directories to search for fonts.
  pub fn with_font_dirs<I, P>(mut self, dirs: I) -> Self
  where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
  {
    self.font_dirs.extend(dirs.into_iter().map(Into::into));
    self
  }

  /// Convenience for using only bundled fonts (no system discovery).
  pub fn bundled_only() -> Self {
    Self {
      use_system_fonts: false,
      use_bundled_fonts: true,
      font_dirs: Vec::new(),
    }
  }
}

/// Configuration for font caches.
#[derive(Debug, Clone, Copy)]
pub struct FontCacheConfig {
  /// Maximum number of parsed faces to retain for glyph coverage checks.
  pub glyph_coverage_cache_size: usize,
}

impl Default for FontCacheConfig {
  fn default() -> Self {
    Self {
      glyph_coverage_cache_size: GLYPH_COVERAGE_CACHE_SIZE,
    }
  }
}

fn shared_system_fontdb() -> Arc<FontDbDatabase> {
  static SYSTEM_FONT_DB: OnceLock<Arc<FontDbDatabase>> = OnceLock::new();
  Arc::clone(SYSTEM_FONT_DB.get_or_init(|| {
    let mut db = FontDbDatabase::new();
    db.load_system_fonts();
    // `FontDatabase::with_config` configures generic-family fallbacks (serif/sans-serif/etc)
    // after loading system fonts. When many `FontContext` instances are created (as happens in
    // the layout test suite), re-scanning + reconfiguring system fonts becomes prohibitively
    // expensive. We therefore load and configure system font metadata once and share it across
    // instances.
    let mut wrapper = FontDatabase {
      db: Arc::new(db),
      cache: RwLock::new(HashMap::new()),
      bundled_face_ids: Arc::new(HashSet::new()),
      math_fonts: RwLock::new(None),
      emoji_fonts: RwLock::new(None),
      glyph_coverage: GlyphCoverageCache::new(GLYPH_COVERAGE_CACHE_SIZE),
    };
    wrapper.set_generic_fallbacks();
    wrapper.db
  }))
}

fn set_bundled_generic_fallbacks(db: &mut FontDbDatabase) {
  let faces: Vec<&fontdb::FaceInfo> = db.faces().collect();
  let Some(primary_family) = faces
    .iter()
    .find(|face| {
      face
        .families
        .iter()
        .all(|(name, _)| !FontDatabase::family_name_is_emoji_font(name))
    })
    .or_else(|| faces.first())
    .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
  else {
    return;
  };

  let non_emoji_faces: Vec<&fontdb::FaceInfo> = faces
    .iter()
    .copied()
    .filter(|face| {
      face
        .families
        .iter()
        .all(|(name, _)| !FontDatabase::family_name_is_emoji_font(name))
    })
    .collect();

  let first_matching_family = |candidates: &[&str]| -> Option<String> {
    // Honor candidate priority order (instead of font load order). This matters because we insert
    // a large font fallback chain into the bundled fontdb, and callers expect:
    // - `first_matching_family(&["DejaVu Sans", "Noto Sans"])` to pick DejaVu when present, even if
    //   Noto was loaded earlier.
    let find_candidate = |candidate: &str, faces: &[&fontdb::FaceInfo]| -> Option<String> {
      for face in faces {
        for (name, _) in &face.families {
          if candidate.eq_ignore_ascii_case(name) {
            return Some(name.clone());
          }
        }
      }
      None
    };

    for candidate in candidates {
      if let Some(name) = find_candidate(candidate, &non_emoji_faces) {
        return Some(name);
      }
    }

    for candidate in candidates {
      if let Some(name) = find_candidate(candidate, &faces) {
        return Some(name);
      }
    }

    None
  };

  // Prefer stable bundled defaults; fall back to the first bundled family if those are missing.
  //
  // `STIX Two Math` is not a perfect serif text face, but its Times-like metrics are substantially
  // closer to common browser defaults than Noto Serif. This matters for offline fixtures that
  // omit webfonts and rely on the UA default `serif` font for line wrapping (e.g. microsoft.com).
  let serif = first_matching_family(&["STIX Two Math", "FastRender Serif", "Noto Serif"])
    .unwrap_or_else(|| primary_family.clone());
  // Prefer Noto Sans: it matches the default `sans-serif` font on many Linux/fontconfig setups
  // (including our CI containers), and therefore aligns better with headless Chrome baselines for
  // pages that request the generic `sans-serif` family directly (notably Wikipedia).
  //
  // Roboto Flex is still kept around as a deterministic alias for common named "system" faces
  // (Helvetica/Arial/etc) where sites explicitly request those families.
  let sans = first_matching_family(&["Noto Sans", "Roboto Flex", "DejaVu Sans"])
    .unwrap_or_else(|| primary_family.clone());
  let monospace =
    first_matching_family(&["Noto Sans Mono"]).unwrap_or_else(|| primary_family.clone());

  db.set_serif_family(serif);
  db.set_sans_serif_family(sans.clone());
  db.set_monospace_family(monospace);
  db.set_cursive_family(sans.clone());
  db.set_fantasy_family(sans);
}

fn shared_bundled_fontdb() -> Arc<FontDbDatabase> {
  static BUNDLED_FONT_DB: OnceLock<Arc<FontDbDatabase>> = OnceLock::new();
  Arc::clone(BUNDLED_FONT_DB.get_or_init(|| {
    let mut db = FontDbDatabase::new();

    for font in BUNDLED_FONTS {
      db.load_font_data(font.data.to_vec());
    }

    if should_load_bundled_emoji_fonts() {
      for font in BUNDLED_EMOJI_FONTS {
        db.load_font_data(font.data.to_vec());
      }
    }

    set_bundled_generic_fallbacks(&mut db);
    Arc::new(db)
  }))
}

#[inline]
fn parse_face_with_counter<'a>(
  data: &'a [u8],
  index: u32,
) -> std::result::Result<ttf_parser::Face<'a>, ttf_parser::FaceParsingError> {
  ttf_parser::Face::parse(data, index)
}

#[derive(Clone)]
struct GlyphCoverageCache {
  inner: Arc<GlyphCoverageCacheInner>,
}

#[derive(Debug)]
struct GlyphCoverageCacheInner {
  shards: Vec<Mutex<LruCache<ID, Arc<CachedFace>, GlyphCoverageCacheHasher>>>,
  shard_mask: usize,
}

impl GlyphCoverageCache {
  fn new(capacity: usize) -> Self {
    let capacity = capacity.max(1);
    let desired_shards =
      (capacity / GLYPH_COVERAGE_CACHE_SHARD_TARGET).clamp(1, GLYPH_COVERAGE_CACHE_SHARD_LIMIT);
    let shard_count = desired_shards
      .next_power_of_two()
      .min(GLYPH_COVERAGE_CACHE_SHARD_LIMIT)
      .max(1);
    let shard_mask = shard_count - 1;

    let base = capacity / shard_count;
    let rem = capacity % shard_count;
    let mut shards = Vec::with_capacity(shard_count);
    for idx in 0..shard_count {
      let shard_cap = base + usize::from(idx < rem);
      let cap = NonZeroUsize::new(shard_cap.max(1)).unwrap_or(NonZeroUsize::MIN);
      shards.push(Mutex::new(LruCache::with_hasher(
        cap,
        GlyphCoverageCacheHasher::default(),
      )));
    }

    Self {
      inner: Arc::new(GlyphCoverageCacheInner { shards, shard_mask }),
    }
  }

  #[inline]
  fn shard_index(&self, id: &ID) -> usize {
    use std::hash::Hash;
    use std::hash::Hasher;

    let mut hasher = FxHasher::default();
    id.hash(&mut hasher);
    (hasher.finish() as usize) & self.inner.shard_mask
  }

  fn get_or_put<F>(&self, id: ID, loader: F) -> Option<Arc<CachedFace>>
  where
    F: FnOnce() -> Option<Arc<CachedFace>>,
  {
    let shard_idx = self.shard_index(&id);
    {
      let mut cache = self.inner.shards[shard_idx].lock();
      if let Some(face) = cache.get(&id) {
        return Some(face.clone());
      }
    }

    let loaded = loader()?;

    {
      let mut cache = self.inner.shards[shard_idx].lock();
      if let Some(face) = cache.get(&id) {
        return Some(face.clone());
      }
      cache.put(id, loaded.clone());
    }

    Some(loaded)
  }

  fn clear(&self) {
    for shard in &self.inner.shards {
      shard.lock().clear();
    }
  }
}

/// Font weight (100-900)
///
/// CSS font-weight values range from 100 (thinnest) to 900 (heaviest).
/// Common keywords map to specific values:
/// - normal: 400
/// - bold: 700
///
/// # Examples
///
/// ```rust,ignore
/// use fastrender::text::font_db::FontWeight;
///
/// let normal = FontWeight::NORMAL; // 400
/// let bold = FontWeight::BOLD;     // 700
/// let custom = FontWeight(550);    // Between medium and semi-bold
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FontWeight(pub u16);

impl FontWeight {
  /// Black (900)
  pub const BLACK: Self = Self(900);
  /// Bold (700) - CSS `font-weight: bold`
  pub const BOLD: Self = Self(700);
  /// Extra Bold (800)
  pub const EXTRA_BOLD: Self = Self(800);
  /// Extra Light (200)
  pub const EXTRA_LIGHT: Self = Self(200);
  /// Light (300)
  pub const LIGHT: Self = Self(300);
  /// Medium (500)
  pub const MEDIUM: Self = Self(500);
  /// Normal/Regular (400) - CSS `font-weight: normal`
  pub const NORMAL: Self = Self(400);
  /// Semi Bold (600)
  pub const SEMI_BOLD: Self = Self(600);
  /// Thin (100)
  pub const THIN: Self = Self(100);

  /// Creates a new font weight, clamping to valid range [100, 900]
  #[inline]
  pub fn new(weight: u16) -> Self {
    Self(weight.clamp(100, 900))
  }

  /// Returns the numeric weight value
  #[inline]
  pub fn value(self) -> u16 {
    self.0
  }
}

impl Default for FontWeight {
  fn default() -> Self {
    Self::NORMAL
  }
}

impl From<u16> for FontWeight {
  fn from(weight: u16) -> Self {
    Self::new(weight)
  }
}

/// Font style (normal, italic, or oblique)
///
/// CSS font-style property values.
///
/// # Examples
///
/// ```rust,ignore
/// use fastrender::text::font_db::FontStyle;
///
/// let normal = FontStyle::Normal;
/// let italic = FontStyle::Italic;
/// let oblique = FontStyle::Oblique;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontStyle {
  /// Normal upright text
  #[default]
  Normal,
  /// Italic text (designed italic letterforms)
  Italic,
  /// Oblique text (slanted version of normal)
  Oblique,
}

// Conversion traits for fontdb interoperability

impl From<FontStyle> for fontdb::Style {
  fn from(style: FontStyle) -> Self {
    match style {
      FontStyle::Normal => fontdb::Style::Normal,
      FontStyle::Italic => fontdb::Style::Italic,
      FontStyle::Oblique => fontdb::Style::Oblique,
    }
  }
}

impl From<fontdb::Style> for FontStyle {
  fn from(style: fontdb::Style) -> Self {
    match style {
      fontdb::Style::Normal => FontStyle::Normal,
      fontdb::Style::Italic => FontStyle::Italic,
      fontdb::Style::Oblique => FontStyle::Oblique,
    }
  }
}

/// Font stretch/width (condensed to expanded)
///
/// CSS font-stretch property values for width variants.
///
/// # Examples
///
/// ```rust,ignore
/// use fastrender::text::font_db::FontStretch;
///
/// let normal = FontStretch::Normal;
/// let condensed = FontStretch::Condensed;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontStretch {
  /// Ultra Condensed (50%)
  UltraCondensed,
  /// Extra Condensed (62.5%)
  ExtraCondensed,
  /// Condensed (75%)
  Condensed,
  /// Semi Condensed (87.5%)
  SemiCondensed,
  /// Normal width (100%)
  #[default]
  Normal,
  /// Semi Expanded (112.5%)
  SemiExpanded,
  /// Expanded (125%)
  Expanded,
  /// Extra Expanded (150%)
  ExtraExpanded,
  /// Ultra Expanded (200%)
  UltraExpanded,
}

impl FontStretch {
  /// Convert to percentage value
  #[inline]
  pub fn to_percentage(self) -> f32 {
    match self {
      FontStretch::UltraCondensed => 50.0,
      FontStretch::ExtraCondensed => 62.5,
      FontStretch::Condensed => 75.0,
      FontStretch::SemiCondensed => 87.5,
      FontStretch::Normal => 100.0,
      FontStretch::SemiExpanded => 112.5,
      FontStretch::Expanded => 125.0,
      FontStretch::ExtraExpanded => 150.0,
      FontStretch::UltraExpanded => 200.0,
    }
  }

  /// Create from percentage value
  pub fn from_percentage(pct: f32) -> Self {
    if pct <= 56.0 {
      FontStretch::UltraCondensed
    } else if pct <= 69.0 {
      FontStretch::ExtraCondensed
    } else if pct <= 81.0 {
      FontStretch::Condensed
    } else if pct <= 94.0 {
      FontStretch::SemiCondensed
    } else if pct <= 106.0 {
      FontStretch::Normal
    } else if pct <= 119.0 {
      FontStretch::SemiExpanded
    } else if pct <= 137.0 {
      FontStretch::Expanded
    } else if pct <= 175.0 {
      FontStretch::ExtraExpanded
    } else {
      FontStretch::UltraExpanded
    }
  }
}

impl From<FontStretch> for fontdb::Stretch {
  fn from(stretch: FontStretch) -> Self {
    match stretch {
      FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
      FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
      FontStretch::Condensed => fontdb::Stretch::Condensed,
      FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
      FontStretch::Normal => fontdb::Stretch::Normal,
      FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
      FontStretch::Expanded => fontdb::Stretch::Expanded,
      FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
      FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
    }
  }
}

impl From<fontdb::Stretch> for FontStretch {
  fn from(stretch: fontdb::Stretch) -> Self {
    match stretch {
      fontdb::Stretch::UltraCondensed => FontStretch::UltraCondensed,
      fontdb::Stretch::ExtraCondensed => FontStretch::ExtraCondensed,
      fontdb::Stretch::Condensed => FontStretch::Condensed,
      fontdb::Stretch::SemiCondensed => FontStretch::SemiCondensed,
      fontdb::Stretch::Normal => FontStretch::Normal,
      fontdb::Stretch::SemiExpanded => FontStretch::SemiExpanded,
      fontdb::Stretch::Expanded => FontStretch::Expanded,
      fontdb::Stretch::ExtraExpanded => FontStretch::ExtraExpanded,
      fontdb::Stretch::UltraExpanded => FontStretch::UltraExpanded,
    }
  }
}

/// Metric overrides attached to an `@font-face` rule (CSS Fonts 4).
///
/// These descriptors are commonly used to make fallback faces (usually `local()` system fonts)
/// match the metrics of a remote webfont, reducing layout shift.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontFaceMetricsOverrides {
  /// `size-adjust` multiplier (e.g. `106.34%` => `1.0634`).
  pub size_adjust: f32,
  /// `ascent-override` multiplier of the face's used font size (after applying `size-adjust`).
  pub ascent_override: Option<f32>,
  /// `descent-override` multiplier of the face's used font size (after applying `size-adjust`).
  pub descent_override: Option<f32>,
  /// `line-gap-override` multiplier of the face's used font size (after applying `size-adjust`).
  pub line_gap_override: Option<f32>,
}

impl Default for FontFaceMetricsOverrides {
  fn default() -> Self {
    Self {
      size_adjust: 1.0,
      ascent_override: None,
      descent_override: None,
      line_gap_override: None,
    }
  }
}

impl FontFaceMetricsOverrides {
  #[inline]
  pub fn has_metric_overrides(&self) -> bool {
    self.ascent_override.is_some()
      || self.descent_override.is_some()
      || self.line_gap_override.is_some()
  }
}

/// Typography descriptors carried by an `@font-face` rule (CSS Fonts 4).
///
/// These settings apply to shaping for the specific face that was matched, and do not affect
/// font selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontFaceShapingDescriptors {
  /// `font-weight` descriptor range (used to clamp font matching variations).
  pub weight_range: Option<(u16, u16)>,
  /// `font-stretch` descriptor range (used to clamp font matching variations).
  pub stretch_range: Option<(f32, f32)>,
  /// `font-style` descriptor (used to clamp oblique angles for font matching variations).
  pub style: Option<CssFontFaceStyle>,

  /// `font-feature-settings` descriptor (None represents `normal`).
  pub font_feature_settings: Option<Arc<[FontFeatureSetting]>>,
  /// `font-variation-settings` descriptor (None represents `normal`).
  pub font_variation_settings: Option<Arc<[FontVariationSetting]>>,
  /// `font-named-instance` descriptor (None represents `auto`).
  pub font_named_instance: Option<String>,
  /// `font-language-override` descriptor (None represents `normal`).
  pub font_language_override: Option<String>,
}

/// A loaded font with cached data
///
/// Contains the font binary data (shared via Arc) along with
/// metadata extracted from the font file.
///
/// # Example
///
/// ```rust,ignore
/// use fastrender::text::font_db::FontDatabase;
///
/// let db = FontDatabase::new();
/// if let Some(id) = db.query("Arial", FontWeight::NORMAL, FontStyle::Normal) {
///     let font = db.load_font(id).expect("Should load font");
///     println!("Family: {}", font.family);
///     println!("Data size: {} bytes", font.data.len());
/// }
/// ```
#[derive(Debug, Clone)]
pub struct LoadedFont {
  /// Stable identifier for this font in the font database, when available.
  pub id: Option<FontId>,
  /// Font binary data (shared via Arc for efficiency)
  pub data: Arc<Vec<u8>>,
  /// Font index within the file (for TTC font collections)
  pub index: u32,
  /// `@font-face` metric overrides for this font face.
  ///
  /// System font lookups use the default (no overrides).
  pub face_metrics_overrides: FontFaceMetricsOverrides,
  /// Typography descriptors from the `@font-face` rule that supplied this font, if any.
  pub face_settings: FontFaceShapingDescriptors,
  /// Font family name
  pub family: String,
  /// Font weight
  pub weight: FontWeight,
  /// Font style
  pub style: FontStyle,
  /// Font stretch
  pub stretch: FontStretch,
}

impl LoadedFont {
  /// Extract font metrics
  ///
  /// Parses the font tables to extract dimensional information.
  ///
  /// # Example
  ///
  /// ```rust,ignore
  /// let font = ctx.get_sans_serif().unwrap();
  /// let metrics = font.metrics().unwrap();
  /// let scaled = metrics.scale(16.0);
  /// println!("Ascent: {}px", scaled.ascent);
  /// ```
  pub fn metrics(&self) -> Result<FontMetrics> {
    FontMetrics::from_font(self)
  }

  /// Extract font metrics using the provided variation coordinates.
  pub fn metrics_with_variations(&self, variations: &[(Tag, f32)]) -> Result<FontMetrics> {
    FontMetrics::from_font_with_variations(self, variations)
  }

  /// Returns a cached parsed face for reuse across shaping and paint.
  ///
  /// Callers that only need to borrow the face temporarily should prefer
  /// [`crate::text::face_cache::with_face`] to avoid cloning the Arc.
  pub fn as_cached_face(&self) -> Result<Arc<CachedFace>> {
    face_cache::get_ttf_face(self).ok_or_else(|| {
      FontError::LoadFailed {
        family: self.family.clone(),
        reason: "Failed to parse font".to_string(),
      }
      .into()
    })
  }

  /// Get ttf-parser Face for advanced operations
  ///
  /// Returns a parsed font face for accessing glyph data, kerning, etc.
  pub fn as_ttf_face(&self) -> Result<ttf_parser::Face<'_>> {
    parse_face_with_counter(&self.data, self.index).map_err(|e| {
      FontError::LoadFailed {
        family: self.family.clone(),
        reason: format!("Failed to parse font: {:?}", e),
      }
      .into()
    })
  }

  /// Returns the metric ratio used by CSS Fonts 4 `font-size-adjust`.
  ///
  /// The ratio is always expressed relative to the font's em square (i.e. design units divided by
  /// units-per-em).
  pub fn font_size_adjust_metric_ratio(&self, metric: FontSizeAdjustMetric) -> Option<f32> {
    const IDEOGRAPH: char = '\u{6C34}'; // U+6C34 '水'

    fn ratio_from_units(units_per_em: u16, units: i16) -> Option<f32> {
      if units_per_em == 0 {
        return None;
      }
      let ratio = units as f32 / (units_per_em as f32);
      (ratio.is_finite() && ratio > 0.0).then_some(ratio)
    }

    fn glyph_h_advance_ratio(face: &ttf_parser::Face<'_>, ch: char) -> Option<f32> {
      let units_per_em = face.units_per_em();
      if units_per_em == 0 {
        return None;
      }
      let glyph_id = face.glyph_index(ch)?;
      if glyph_id.0 == 0 {
        return None;
      }
      let advance = face.glyph_hor_advance(glyph_id)?;
      let ratio = advance as f32 / (units_per_em as f32);
      (ratio.is_finite() && ratio > 0.0).then_some(ratio)
    }

    fn glyph_v_advance_ratio(face: &ttf_parser::Face<'_>, ch: char) -> Option<f32> {
      let units_per_em = face.units_per_em();
      if units_per_em == 0 {
        return None;
      }
      let glyph_id = face.glyph_index(ch)?;
      if glyph_id.0 == 0 {
        return None;
      }
      let advance = face.glyph_ver_advance(glyph_id)?;
      let ratio = advance as f32 / (units_per_em as f32);
      (ratio.is_finite() && ratio > 0.0).then_some(ratio)
    }

    match metric {
      FontSizeAdjustMetric::ExHeight => self.metrics().ok().and_then(|m| {
        m.x_height
          .and_then(|xh| ratio_from_units(m.units_per_em, xh))
      }),
      FontSizeAdjustMetric::CapHeight => self.metrics().ok().and_then(|m| {
        m.cap_height
          .and_then(|ch| ratio_from_units(m.units_per_em, ch))
          .or_else(|| ratio_from_units(m.units_per_em, m.ascent))
      }),
      FontSizeAdjustMetric::ChWidth => {
        face_cache::with_face(self, |face| glyph_h_advance_ratio(face, '0')).flatten()
      }
      FontSizeAdjustMetric::IcWidth => {
        face_cache::with_face(self, |face| glyph_h_advance_ratio(face, IDEOGRAPH)).flatten()
      }
      FontSizeAdjustMetric::IcHeight => face_cache::with_face(self, |face| {
        glyph_v_advance_ratio(face, IDEOGRAPH).or_else(|| glyph_h_advance_ratio(face, IDEOGRAPH))
      })
      .flatten(),
    }
  }

  pub fn font_size_adjust_metric_ratio_or_fallback(&self, metric: FontSizeAdjustMetric) -> f32 {
    let fallback = match metric {
      FontSizeAdjustMetric::ExHeight => 0.5,
      FontSizeAdjustMetric::CapHeight => 0.7,
      FontSizeAdjustMetric::ChWidth => 0.5,
      FontSizeAdjustMetric::IcWidth | FontSizeAdjustMetric::IcHeight => 1.0,
    };
    self
      .font_size_adjust_metric_ratio(metric)
      .unwrap_or(fallback)
  }
}

/// Computes the used font size after applying CSS Fonts 4 `font-size-adjust`.
///
/// `preferred_ratio` is the metric ratio extracted from the *base font* when using `from-font`.
/// When it is `None`, `from-font` falls back to the current font's ratio, effectively disabling
/// adjustment (matching browser behavior when the base font cannot provide the metric).
pub fn compute_font_size_adjusted_size(
  base_size: f32,
  font_size_adjust: FontSizeAdjust,
  font: &LoadedFont,
  preferred_ratio: Option<f32>,
) -> f32 {
  let (metric, desired) = match font_size_adjust {
    FontSizeAdjust::None => return base_size,
    FontSizeAdjust::Number { ratio, metric } => {
      (metric, (ratio.is_finite() && ratio > 0.0).then_some(ratio))
    }
    FontSizeAdjust::FromFont { metric } => (
      metric,
      preferred_ratio.or_else(|| font.font_size_adjust_metric_ratio(metric)),
    ),
  };

  let Some(desired) = desired.filter(|ratio| ratio.is_finite() && *ratio > 0.0) else {
    return base_size;
  };

  let actual = font.font_size_adjust_metric_ratio_or_fallback(metric);
  if actual > 0.0 {
    base_size * (desired / actual)
  } else {
    base_size
  }
}

/// Returns true if the font advertises an OpenType GSUB feature with the given tag.
pub fn font_has_feature(font: &LoadedFont, tag: [u8; 4]) -> bool {
  face_cache::with_face(font, |face| {
    if let Some(gsub) = face.tables().gsub {
      return gsub
        .features
        .index(ttf_parser::Tag::from_bytes(&tag))
        .is_some();
    }
    false
  })
  .unwrap_or(false)
}

pub(crate) fn face_has_color_tables(face: &ttf_parser::Face<'_>) -> bool {
  let raw = face.raw_face();
  let has_colr = raw.table(ttf_parser::Tag::from_bytes(b"COLR")).is_some();
  let has_cpal = raw.table(ttf_parser::Tag::from_bytes(b"CPAL")).is_some();
  let has_cbdt = raw.table(ttf_parser::Tag::from_bytes(b"CBDT")).is_some();
  let has_cblc = raw.table(ttf_parser::Tag::from_bytes(b"CBLC")).is_some();
  let has_sbix = raw.table(ttf_parser::Tag::from_bytes(b"sbix")).is_some();
  let has_svg = raw.table(ttf_parser::Tag::from_bytes(b"SVG ")).is_some();

  // Treat COLR alone as color-capable to accommodate fonts that embed default palettes without
  // a CPAL table.
  (has_colr && has_cpal) || (has_cbdt && has_cblc) || has_sbix || has_svg || has_colr
}

/// Generic font families as defined by CSS
///
/// These are abstract font families that map to actual system fonts.
///
/// # CSS Specification
///
/// See CSS Fonts Module Level 4, Section 4.2:
/// <https://www.w3.org/TR/css-fonts-4/#generic-font-families>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericFamily {
  /// Serif fonts (e.g., Times New Roman, Georgia)
  Serif,
  /// Sans-serif fonts (e.g., Arial, Helvetica)
  SansSerif,
  /// Monospace fonts (e.g., Courier, Monaco)
  Monospace,
  /// Cursive/script fonts
  Cursive,
  /// Fantasy/decorative fonts
  Fantasy,
  /// System UI font
  SystemUi,
  /// UI serif font (serif intended for UI text)
  UiSerif,
  /// UI sans-serif font (sans-serif intended for UI text)
  UiSansSerif,
  /// UI monospace font (monospace intended for UI text)
  UiMonospace,
  /// UI rounded font (sans-serif with rounded letterforms)
  UiRounded,
  /// Emoji font (colored emoji glyphs)
  Emoji,
  /// Math font (mathematical notation)
  Math,
  /// Fangsong font (Chinese font style between serif and script)
  Fangsong,
}

/// Returns additional font family names that should be treated as fallbacks for a requested *named*
/// family.
///
/// Browsers typically integrate with platform font substitution (e.g. Fontconfig aliases on Linux)
/// so that web content using "core" font names like `Helvetica` or `Arial` still produces
/// reasonable results on systems where those exact families are not installed.
///
/// FastRender's font matching is based on `fontdb` and does not consult platform alias rules, so
/// we provide a small, deterministic alias table for common web fonts. This is not intended to be
/// exhaustive; it is a pragmatic compatibility layer to improve fidelity on pages that specify
/// these names as primary fonts.
pub(crate) fn named_family_aliases(name: &str) -> &'static [&'static str] {
  if name.eq_ignore_ascii_case("Helvetica") || name.eq_ignore_ascii_case("Arial") {
    // `fc-match -s Helvetica` on typical Linux environments prefers Liberation Sans first, then
    // falls back through common sans-serif faces.
    // In bundled-only mode we don't ship Liberation Sans, so prefer Roboto Flex next: its metrics
    // are closer to common browser Linux fallbacks than the wider Noto Sans.
    &["Liberation Sans", "Roboto Flex", "Noto Sans", "DejaVu Sans"]
  } else if name.eq_ignore_ascii_case("Helvetica Neue") {
    // `fc-match -s "Helvetica Neue"` on typical Linux/fontconfig environments prefers Noto Sans
    // (not Liberation Sans).
    &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Roboto Flex"]
  } else if name.eq_ignore_ascii_case("Times New Roman") || name.eq_ignore_ascii_case("Times") {
    &["Liberation Serif", "Noto Serif", "DejaVu Serif"]
  } else if name.eq_ignore_ascii_case("Courier New") || name.eq_ignore_ascii_case("Courier") {
    &["Liberation Mono", "Noto Sans Mono", "DejaVu Sans Mono"]
  } else {
    &[]
  }
}

impl GenericFamily {
  /// Parse a generic family name from a string
  ///
  /// Returns None if the string is not a recognized generic family.
  pub fn parse(s: &str) -> Option<Self> {
    if s.eq_ignore_ascii_case("serif") {
      Some(GenericFamily::Serif)
    } else if s.eq_ignore_ascii_case("sans-serif") {
      Some(GenericFamily::SansSerif)
    } else if s.eq_ignore_ascii_case("monospace") {
      Some(GenericFamily::Monospace)
    } else if s.eq_ignore_ascii_case("cursive") {
      Some(GenericFamily::Cursive)
    } else if s.eq_ignore_ascii_case("fantasy") {
      Some(GenericFamily::Fantasy)
    } else if s.eq_ignore_ascii_case("system-ui") {
      Some(GenericFamily::SystemUi)
    } else if s.eq_ignore_ascii_case("ui-serif") {
      Some(GenericFamily::UiSerif)
    } else if s.eq_ignore_ascii_case("ui-sans-serif") {
      Some(GenericFamily::UiSansSerif)
    } else if s.eq_ignore_ascii_case("ui-monospace") {
      Some(GenericFamily::UiMonospace)
    } else if s.eq_ignore_ascii_case("ui-rounded") {
      Some(GenericFamily::UiRounded)
    } else if s.eq_ignore_ascii_case("emoji") {
      Some(GenericFamily::Emoji)
    } else if s.eq_ignore_ascii_case("math") {
      Some(GenericFamily::Math)
    } else if s.eq_ignore_ascii_case("fangsong") {
      Some(GenericFamily::Fangsong)
    } else {
      None
    }
  }

  /// Get fallback font families for this generic family
  ///
  /// Returns a list of common fonts that typically implement this generic family.
  pub fn fallback_families(self) -> &'static [&'static str] {
    match self {
      GenericFamily::Serif | GenericFamily::UiSerif => &[
        "Times New Roman",
        "Times",
        "Georgia",
        "Liberation Serif",
        "Noto Serif",
        "DejaVu Serif",
        "FreeSerif",
      ],
      // Try to match common Linux fontconfig defaults: on many modern distributions `sans-serif`
      // resolves to Noto Sans with DejaVu as a fallback (see e.g. `fc-match -s sans-serif`).
      //
      // Keep legacy web font names (Arial/Helvetica/Verdana) near the front so pages that specify
      // those names as their primary face still land on reasonable substitutes when they're
      // available.
      GenericFamily::SansSerif | GenericFamily::UiSansSerif | GenericFamily::UiRounded => &[
        "Arial",
        "Helvetica",
        "Helvetica Neue",
        "Verdana",
        "Noto Sans",
        "Liberation Sans",
        "DejaVu Sans",
        "FreeSans",
        "Roboto",
      ],
      GenericFamily::Monospace | GenericFamily::UiMonospace => &[
        "Courier New",
        "Courier",
        "Consolas",
        "Monaco",
        "DejaVu Sans Mono",
        "Liberation Mono",
        "Noto Sans Mono",
        "FreeMono",
        "SF Mono",
      ],
      GenericFamily::Cursive => &[
        "Comic Sans MS",
        "Apple Chancery",
        "Zapf Chancery",
        "URW Chancery L",
        "Bradley Hand",
      ],
      GenericFamily::Fantasy => &["Impact", "Papyrus", "Copperplate", "Luminari"],
      GenericFamily::SystemUi => &[
        ".SF NS",
        "San Francisco",
        "Segoe UI",
        "Roboto",
        "Ubuntu",
        "Cantarell",
        "Noto Sans",
        "Liberation Sans",
        "DejaVu Sans",
      ],
      GenericFamily::Emoji => &[
        "FastRender Emoji",
        "Apple Color Emoji",
        "Segoe UI Emoji",
        "Noto Color Emoji",
        "Twemoji",
        "EmojiOne",
        "Symbola",
      ],
      GenericFamily::Math => &[
        "Cambria Math",
        "STIX Two Math",
        "Latin Modern Math",
        "DejaVu Math TeX Gyre",
      ],
      GenericFamily::Fangsong => &["FangSong", "STFangsong", "FangSong_GB2312"],
    }
  }

  /// Returns true if resolution should try explicit fallback names before mapping to a fontdb generic.
  pub fn prefers_named_fallbacks_first(self) -> bool {
    matches!(
      self,
      GenericFamily::SystemUi | GenericFamily::Emoji | GenericFamily::Math | GenericFamily::Fangsong
    )
  }

  /// Converts to fontdb Family for querying.
  pub fn to_fontdb(self) -> FontDbFamily<'static> {
    match self {
      Self::Serif | Self::UiSerif => FontDbFamily::Serif,
      Self::SansSerif | Self::SystemUi | Self::UiSansSerif | Self::UiRounded => {
        FontDbFamily::SansSerif
      }
      Self::Monospace | Self::UiMonospace => FontDbFamily::Monospace,
      Self::Cursive => FontDbFamily::Cursive,
      Self::Fantasy => FontDbFamily::Fantasy,
      // These don't have direct fontdb equivalents, fallback to sans-serif
      Self::Emoji | Self::Math | Self::Fangsong => FontDbFamily::SansSerif,
    }
  }
}

impl std::str::FromStr for GenericFamily {
  type Err = ();

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    Self::parse(s).ok_or(())
  }
}

/// Font database
///
/// Wraps `fontdb` and provides font loading, caching, and querying.
/// System fonts are loaded automatically unless disabled via [`FontConfig`], and
/// bundled fallback fonts are used when nothing else is available.
///
/// # Thread Safety
///
/// FontDatabase uses interior mutability with RwLock for thread-safe
/// cache access. Multiple threads can query fonts concurrently.
///
/// # Example
///
/// ```rust,ignore
/// use fastrender::text::font_db::{FontDatabase, FontWeight, FontStyle};
///
/// // Create database (loads system fonts)
/// let db = FontDatabase::new();
///
/// // Query for a font
/// if let Some(id) = db.query("Arial", FontWeight::NORMAL, FontStyle::Normal) {
///     // Load font data
///     let font = db.load_font(id).expect("Font should load");
///     println!("Loaded font: {}", font.family);
/// }
///
/// // Use fallback chain
/// let families = vec![
///     "NonExistentFont".to_string(),
///     "Arial".to_string(),
///     "sans-serif".to_string(),
/// ];
/// if let Some(id) = db.resolve_family_list(&families, FontWeight::NORMAL, FontStyle::Normal) {
///     let font = db.load_font(id).expect("Fallback should work");
///     println!("Found fallback font: {}", font.family);
/// }
/// ```
pub struct FontDatabase {
  /// Underlying fontdb database
  db: Arc<FontDbDatabase>,
  /// Cached font data (font ID -> binary data)
  cache: RwLock<HashMap<ID, Arc<Vec<u8>>>>,
  /// Fontdb face IDs corresponding to the bundled fallback fonts loaded into this database.
  ///
  /// When the font database is created with both system fonts and bundled fonts, we still want to
  /// apply built-in metric overrides to *only* the bundled fallback faces. Tracking IDs lets
  /// `create_loaded_font_with_info` distinguish bundled faces from system faces even when both are
  /// present in the same fontdb instance.
  bundled_face_ids: Arc<HashSet<ID>>,
  /// Cached list of math-capable fonts (IDs with a MATH table)
  math_fonts: RwLock<Option<Vec<ID>>>,
  /// Cached list of emoji-capable fonts (IDs likely to contain emoji glyphs)
  emoji_fonts: RwLock<Option<Vec<ID>>>,
  /// Glyph coverage cache to avoid repeated face parsing
  glyph_coverage: GlyphCoverageCache,
}

impl FontDatabase {
  /// Creates a new font database and loads system fonts
  ///
  /// This will scan the system font directories:
  /// - Windows: C:\Windows\Fonts
  /// - macOS: /Library/Fonts, /System/Library/Fonts, ~/Library/Fonts
  /// - Linux: /usr/share/fonts, ~/.fonts, ~/.local/share/fonts
  ///
  /// Defaults are driven by [`FontConfig::default()`], which honors
  /// `FASTR_USE_BUNDLED_FONTS` (and `CI`) to disable system discovery when
  /// deterministic renders are required.
  ///
  /// # Example
  ///
  /// ```rust,ignore
  /// let db = FontDatabase::new();
  /// ```
  pub fn new() -> Self {
    Self::with_config(&FontConfig::default())
  }

  /// Creates a new font database using the provided configuration.
  ///
  /// Bundled fonts are always used as a final fallback to ensure text shaping
  /// works in minimal environments where no system fonts are present.
  pub fn with_config(config: &FontConfig) -> Self {
    if !config.use_system_fonts && config.font_dirs.is_empty() {
      return Self::shared_bundled();
    }
    if config.use_system_fonts && !config.use_bundled_fonts && config.font_dirs.is_empty() {
      return Self::shared_system();
    }

    let mut this = Self {
      db: Arc::new(FontDbDatabase::new()),
      cache: RwLock::new(HashMap::new()),
      bundled_face_ids: Arc::new(HashSet::new()),
      math_fonts: RwLock::new(None),
      emoji_fonts: RwLock::new(None),
      glyph_coverage: GlyphCoverageCache::new(GLYPH_COVERAGE_CACHE_SIZE),
    };

    if config.use_system_fonts {
      Arc::make_mut(&mut this.db).load_system_fonts();
    }

    for dir in &config.font_dirs {
      this.load_fonts_dir(dir);
    }

    if config.use_bundled_fonts {
      this.load_bundled_fonts();
    }

    if this.font_count() == 0 && !config.use_bundled_fonts {
      // Bundled fonts are always loaded as a last resort to ensure shaping works even in
      // minimal environments. Reuse the shared bundled metadata handle instead of reparsing.
      return Self::shared_bundled();
    }

    this.set_generic_fallbacks();
    this
  }

  /// Creates an empty font database without loading system fonts
  ///
  /// Useful for testing or when you want to load specific fonts only.
  pub fn empty() -> Self {
    Self::with_shared_db_and_cache(Arc::new(FontDbDatabase::new()), FontCacheConfig::default())
  }

  /// Creates a new font database that reuses the shared system font list while keeping caches private.
  pub fn shared_system() -> Self {
    Self::with_shared_db_and_cache(shared_system_fontdb(), FontCacheConfig::default())
  }

  /// Returns the shared system font metadata handle.
  pub fn shared_system_db() -> Arc<FontDbDatabase> {
    shared_system_fontdb()
  }

  /// Creates a new font database that reuses the shared bundled font list while keeping caches private.
  pub fn shared_bundled() -> Self {
    Self::with_shared_bundled_db_and_cache(FontCacheConfig::default())
  }

  /// Returns the shared bundled font metadata handle.
  pub fn shared_bundled_db() -> Arc<FontDbDatabase> {
    shared_bundled_fontdb()
  }

  /// Creates a new font database that reuses the shared bundled font list with custom cache sizing.
  pub fn with_shared_bundled_db_and_cache(cache: FontCacheConfig) -> Self {
    Self::with_shared_db_and_cache(shared_bundled_fontdb(), cache)
  }

  /// Creates a new font database that shares the underlying font list with other instances
  /// while keeping caches isolated.
  pub fn with_shared_db(db: Arc<FontDbDatabase>) -> Self {
    Self::with_shared_db_and_cache(db, FontCacheConfig::default())
  }

  /// Creates a new font database with shared font metadata and custom cache sizing.
  pub fn with_shared_db_and_cache(db: Arc<FontDbDatabase>, cache: FontCacheConfig) -> Self {
    Self {
      db,
      cache: RwLock::new(HashMap::new()),
      bundled_face_ids: Arc::new(HashSet::new()),
      math_fonts: RwLock::new(None),
      emoji_fonts: RwLock::new(None),
      glyph_coverage: GlyphCoverageCache::new(cache.glyph_coverage_cache_size),
    }
  }

  /// Creates a new font database with shared font metadata, custom cache sizing, and a bundled-face
  /// ID set aligned to the shared metadata.
  pub(crate) fn with_shared_db_and_cache_and_bundled_face_ids(
    db: Arc<FontDbDatabase>,
    cache: FontCacheConfig,
    bundled_face_ids: Arc<HashSet<ID>>,
  ) -> Self {
    Self {
      db,
      cache: RwLock::new(HashMap::new()),
      bundled_face_ids,
      math_fonts: RwLock::new(None),
      emoji_fonts: RwLock::new(None),
      glyph_coverage: GlyphCoverageCache::new(cache.glyph_coverage_cache_size),
    }
  }

  /// Returns the shared font metadata backing this database.
  pub fn shared_db(&self) -> Arc<FontDbDatabase> {
    Arc::clone(&self.db)
  }

  pub(crate) fn bundled_face_ids(&self) -> Arc<HashSet<ID>> {
    Arc::clone(&self.bundled_face_ids)
  }

  /// Returns a new font database that reuses the same font metadata but has fresh caches.
  pub fn clone_with_shared_data(&self, cache: FontCacheConfig) -> Self {
    Self::with_shared_db_and_cache_and_bundled_face_ids(
      self.shared_db(),
      cache,
      Arc::clone(&self.bundled_face_ids),
    )
  }

  /// Loads fonts from a directory
  ///
  /// Recursively scans the directory for font files.
  pub fn load_fonts_dir<P: AsRef<Path>>(&mut self, path: P) {
    Arc::make_mut(&mut self.db).load_fonts_dir(path);
    if let Ok(mut cached) = self.math_fonts.write() {
      *cached = None;
    }
    if let Ok(mut cached) = self.emoji_fonts.write() {
      *cached = None;
    }
  }

  fn load_bundled_fonts(&mut self) {
    let before_ids: HashSet<ID> = self.db.faces().map(|face| face.id).collect();

    for font in BUNDLED_FONTS {
      if let Err(err) = self.load_font_data(font.data.to_vec()) {
        eprintln!("failed to load bundled font {}: {:?}", font.name, err);
      }
    }

    if should_load_bundled_emoji_fonts() {
      for font in BUNDLED_EMOJI_FONTS {
        if let Err(err) = self.load_font_data(font.data.to_vec()) {
          eprintln!("failed to load bundled emoji font {}: {:?}", font.name, err);
        }
      }
    }

    let after_ids: HashSet<ID> = self.db.faces().map(|face| face.id).collect();
    Arc::make_mut(&mut self.bundled_face_ids).extend(after_ids.difference(&before_ids).copied());
  }

  /// Recomputes the default families used for fontdb generic queries based on the currently loaded fonts.
  pub fn refresh_generic_fallbacks(&mut self) {
    self.set_generic_fallbacks();
  }
  fn set_generic_fallbacks(&mut self) {
    let faces: Vec<&fontdb::FaceInfo> = self.faces().collect();
    let bundled_ids = self.bundled_face_ids();

    // Prefer selecting generic fallbacks from non-bundled faces when system/user fonts are
    // available.
    //
    // This matters whenever we load bundled fonts alongside system fonts (e.g. `render_fixtures
    // --patch-html-for-chrome-baseline --system-fonts`): Chrome's generic families (`sans-serif`,
    // `serif`, etc.) resolve through the host's system font configuration, while FastRender always
    // has bundled Noto families available. Without preferring non-bundled faces, `sans-serif` would
    // map to the bundled Noto Sans even on systems where Chrome would pick e.g. DejaVu Sans,
    // producing widespread wrap/layout drift on text-heavy pages.

    let is_bundled_face = |face: &fontdb::FaceInfo| bundled_ids.contains(&face.id);
    let is_non_emoji_non_bundled = |face: &fontdb::FaceInfo| {
      !Self::face_is_emoji_font(face) && !is_bundled_face(face)
    };

    let Some(primary_family) = faces
      .iter()
      .find(|face| is_non_emoji_non_bundled(face) && self.face_has_basic_latin(face.id))
      .or_else(|| faces.iter().find(|face| is_non_emoji_non_bundled(face)))
      .or_else(|| {
        faces
          .iter()
          .find(|face| !Self::face_is_emoji_font(face) && self.face_has_basic_latin(face.id))
      })
      .or_else(|| faces.iter().find(|face| !Self::face_is_emoji_font(face)))
      .or_else(|| faces.first())
      .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
    else {
      return;
    };

    // Resolve generic families deterministically by honoring the fallback list order instead of
    // depending on the host font enumeration order (fontdb loads system fonts in whatever order the
    // OS/fontconfig reports). This matches typical browser font fallback behavior more closely and
    // keeps fixture renders stable as long as the same candidate fonts are present.
    let non_emoji_faces: Vec<&fontdb::FaceInfo> = faces
      .iter()
      .copied()
      .filter(|face| !Self::face_is_emoji_font(face))
      .collect();
    let non_emoji_non_bundled_faces: Vec<&fontdb::FaceInfo> = non_emoji_faces
      .iter()
      .copied()
      .filter(|face| !is_bundled_face(face))
      .collect();
    let find_candidate = |candidate: &str, faces: &[&fontdb::FaceInfo]| -> Option<String> {
      for face in faces {
        for (name, _) in &face.families {
          if candidate.eq_ignore_ascii_case(name) {
            return Some(name.clone());
          }
        }
      }
      None
    };
    let first_matching_family = |candidates: &[&str]| -> Option<String> {
      for candidate in candidates {
        if let Some(name) = find_candidate(candidate, &non_emoji_non_bundled_faces) {
          return Some(name);
        }
      }
      for candidate in candidates {
        if let Some(name) = find_candidate(candidate, &non_emoji_faces) {
          return Some(name);
        }
      }
      for candidate in candidates {
        if let Some(name) = find_candidate(candidate, &faces) {
          return Some(name);
        }
      }
      None
    };

    let serif = first_matching_family(GenericFamily::Serif.fallback_families())
      .unwrap_or_else(|| primary_family.clone());
    let sans = first_matching_family(GenericFamily::SansSerif.fallback_families())
      .unwrap_or_else(|| primary_family.clone());
    let monospace = first_matching_family(GenericFamily::Monospace.fallback_families())
      .unwrap_or_else(|| primary_family.clone());
    let cursive = first_matching_family(GenericFamily::Cursive.fallback_families())
      .unwrap_or_else(|| primary_family.clone());
    let fantasy =
      first_matching_family(GenericFamily::Fantasy.fallback_families()).unwrap_or(primary_family);

    let db = Arc::make_mut(&mut self.db);
    db.set_serif_family(serif);
    db.set_sans_serif_family(sans);
    db.set_monospace_family(monospace);
    db.set_cursive_family(cursive);
    db.set_fantasy_family(fantasy);
  }

  fn face_is_emoji_font(face: &fontdb::FaceInfo) -> bool {
    face
      .families
      .iter()
      .any(|(name, _)| Self::family_name_is_emoji_font(name))
  }

  fn face_has_basic_latin(&self, id: ID) -> bool {
    self.has_glyph(id, 'A') && self.has_glyph(id, 'a')
  }

  /// Loads a font from binary data
  ///
  /// Useful for loading embedded fonts or web fonts.
  ///
  /// # Errors
  ///
  /// Returns an error if the data is not a valid font file.
  pub fn load_font_data(&mut self, data: Vec<u8>) -> Result<()> {
    // Validate the data is a valid font
    parse_face_with_counter(&data, 0).map_err(|e| FontError::InvalidFontFile {
      path: format!("(memory): {:?}", e),
    })?;

    Arc::make_mut(&mut self.db).load_font_data(data);
    if let Ok(mut cached) = self.math_fonts.write() {
      *cached = None;
    }
    if let Ok(mut cached) = self.emoji_fonts.write() {
      *cached = None;
    }
    Ok(())
  }

  /// Queries for a font matching the given criteria
  ///
  /// Returns the font ID of the best match, or None if no fonts match.
  /// The fontdb library handles fuzzy matching for weight and style.
  ///
  /// # Arguments
  ///
  /// * `family` - Font family name (e.g., "Arial") or generic family (e.g., "sans-serif")
  /// * `weight` - Desired font weight (100-900)
  /// * `style` - Desired font style (normal, italic, oblique)
  ///
  /// # Example
  ///
  /// ```rust,ignore
  /// let db = FontDatabase::new();
  ///
  /// // Query specific font
  /// let id = db.query("Arial", FontWeight::BOLD, FontStyle::Normal);
  ///
  /// // Query generic family
  /// let id = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal);
  /// ```
  pub fn query(&self, family: &str, weight: FontWeight, style: FontStyle) -> Option<ID> {
    self.query_internal(family, weight, style, FontStretch::Normal, true)
  }

  /// Queries with weight, style, and stretch
  ///
  /// Full query with all font properties.
  pub fn query_full(
    &self,
    family: &str,
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
  ) -> Option<ID> {
    self.query_internal(family, weight, style, stretch, true)
  }

  pub(crate) fn query_named_family_with_aliases(
    &self,
    family: &str,
    weight: u16,
    style: FontStyle,
    stretch: FontStretch,
  ) -> Option<ID> {
    let family = family.trim();
    let families = [FontDbFamily::Name(family)];
    let query = FontDbQuery {
      families: &families,
      weight: fontdb::Weight(weight),
      style: style.into(),
      stretch: stretch.into(),
    };
    if let Some(found) = self.db.query(&query) {
      return Some(self.prefer_non_bundled_duplicate_face(found));
    }

    if !self.is_shared_bundled_db() {
      return None;
    }

    let aliases = Self::bundled_family_aliases(family)?;
    for alias in aliases {
      let families = [FontDbFamily::Name(*alias)];
      let query = FontDbQuery {
        families: &families,
        weight: fontdb::Weight(weight),
        style: style.into(),
        stretch: stretch.into(),
      };
      if let Some(found) = self.db.query(&query) {
        return Some(self.prefer_non_bundled_duplicate_face(found));
      }
    }

    None
  }

  fn query_internal(
    &self,
    family: &str,
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
    allow_aliases: bool,
  ) -> Option<ID> {
    // Check if this is a generic family.
    let families = if let Some(generic) = GenericFamily::parse(family) {
      // `fontdb` does not have a dedicated "math" generic, so `GenericFamily::Math` maps to
      // sans-serif by default. Prefer known math fonts first so `font-family: math` resolves to a
      // real OpenType MATH font when available.
      if matches!(generic, GenericFamily::Math) {
        let mut families: Vec<FontDbFamily> = generic
          .fallback_families()
          .iter()
          .map(|name| FontDbFamily::Name(*name))
          .collect();
        families.push(generic.to_fontdb());
        families
      } else {
        vec![generic.to_fontdb()]
      }
    } else {
      vec![FontDbFamily::Name(family)]
    };

    let query = FontDbQuery {
      families: &families,
      weight: fontdb::Weight(weight.0),
      style: style.into(),
      stretch: stretch.into(),
    };

    if let Some(found) = self.db.query(&query) {
      return Some(self.prefer_non_bundled_duplicate_face(found));
    }

    if !allow_aliases {
      return None;
    }

    if self.is_shared_bundled_db() {
      if let Some(aliases) = Self::bundled_family_aliases(family) {
        for alias in aliases {
          if let Some(found) = self.query_internal(alias, weight, style, stretch, false) {
            return Some(found);
          }
        }
      }
    }

    None
  }

  fn is_shared_bundled_db(&self) -> bool {
    Arc::ptr_eq(&self.db, &shared_bundled_fontdb())
  }

  /// When both bundled fonts and non-bundled fonts are loaded into the same fontdb instance,
  /// `fontdb::Database::query` can return either face when they share the same family/style/weight.
  ///
  /// We ship bundled fallback fonts for determinism, and attach built-in metric overrides to them
  /// (see `create_loaded_font_with_info`) so `line-height: normal` behaves closer to typical
  /// browsers when system fonts are unavailable. When system (or user-supplied) fonts are present,
  /// however, we should prefer those faces over the bundled duplicates so we don't accidentally
  /// apply the bundled overrides and distort layout metrics (notably line heights in table-heavy
  /// pages like Hacker News).
  fn prefer_non_bundled_duplicate_face(&self, found: ID) -> ID {
    if self.bundled_face_ids.is_empty() || !self.bundled_face_ids.contains(&found) {
      return found;
    }

    let Some(found_face) = self.db.face(found) else {
      return found;
    };
    let Some((primary_family, _)) = found_face.families.first() else {
      return found;
    };

    // Find an equivalent non-bundled face (same family + style/weight/stretch).
    for candidate in self.db.faces() {
      if self.bundled_face_ids.contains(&candidate.id) {
        continue;
      }
      if candidate.style != found_face.style
        || candidate.weight != found_face.weight
        || candidate.stretch != found_face.stretch
      {
        continue;
      }
      if candidate
        .families
        .iter()
        .any(|(name, _)| primary_family.eq_ignore_ascii_case(name))
      {
        return candidate.id;
      }
    }

    found
  }

  fn bundled_family_aliases(family: &str) -> Option<&'static [&'static str]> {
    let family = family.trim();

    // Prefer Roboto Flex first: its metrics are closer to typical Linux browser sans-serif
    // fallbacks, reducing wrap-driven layout drift when system fonts are disabled.
    const SANS: &[&str] = &["Roboto Flex", "Noto Sans"];
    const SERIF: &[&str] = &["STIX Two Math", "FastRender Serif", "Noto Serif"];
    const MONO: &[&str] = &["Noto Sans Mono"];

    if family.eq_ignore_ascii_case("liberation sans") {
      return Some(SANS);
    }

    if family.eq_ignore_ascii_case("helvetica")
      || family.eq_ignore_ascii_case("helveticaneue")
      || family.eq_ignore_ascii_case("helvetica neue")
      || family.eq_ignore_ascii_case("arial")
      || family.eq_ignore_ascii_case("arial narrow")
    {
      return Some(SANS);
    }

    // `system-ui` and many cross-platform stacks include `Roboto` as a candidate UI font.
    // In bundled-font mode we ship Roboto Flex (not the platform Roboto faces), so map requests
    // for `Roboto` to our deterministic sans fallback to reduce wrap-driven layout drift.
    if family.eq_ignore_ascii_case("roboto") {
      return Some(SANS);
    }

    if family.eq_ignore_ascii_case("times")
      || family.eq_ignore_ascii_case("times new roman")
      || family.eq_ignore_ascii_case("timesnewroman")
    {
      return Some(SERIF);
    }

    if family.eq_ignore_ascii_case("courier")
      || family.eq_ignore_ascii_case("courier new")
      || family.eq_ignore_ascii_case("couriernew")
    {
      return Some(MONO);
    }

    None
  }

  /// Loads font data for a given font ID
  ///
  /// Caches the data for subsequent requests. The cached data is
  /// shared via Arc to avoid duplication.
  ///
  /// # Arguments
  ///
  /// * `id` - Font ID obtained from `query()`
  ///
  /// # Returns
  ///
  /// Returns the loaded font with its data, or None if loading fails.
  pub fn load_font(&self, id: ID) -> Option<LoadedFont> {
    let data = self.get_or_load_font_data(id)?;
    let face_info = self.db.face(id)?;

    Some(self.create_loaded_font_with_info(id, data, &face_info))
  }

  fn get_or_load_font_data(&self, id: ID) -> Option<Arc<Vec<u8>>> {
    if let Ok(cache) = self.cache.read() {
      if let Some(data) = cache.get(&id) {
        return Some(Arc::clone(data));
      }
    }

    let mut data_result: Option<Arc<Vec<u8>>> = None;
    self.db.with_face_data(id, |font_data, _face_index| {
      data_result = Some(Arc::new(font_data.to_vec()));
    });

    let data = data_result?;

    if let Ok(mut cache) = self.cache.write() {
      cache.insert(id, Arc::clone(&data));
    }

    Some(data)
  }

  /// Creates a LoadedFont from cached data
  fn create_loaded_font(&self, id: ID, data: Arc<Vec<u8>>) -> LoadedFont {
    match self.db.face(id) {
      Some(face_info) => self.create_loaded_font_with_info(id, data, &face_info),
      None => LoadedFont {
        id: Some(FontId::new(id)),
        data,
        // Without a valid `FaceInfo`, we don't know the TTC face index. Use index 0 as a safe
        // default; downstream parsing surfaces errors via Result/Option rather than panicking.
        index: 0,
        face_metrics_overrides: FontFaceMetricsOverrides::default(),
        face_settings: FontFaceShapingDescriptors::default(),
        family: "Unknown".to_string(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        stretch: FontStretch::Normal,
      },
    }
  }

  /// Creates a LoadedFont from face info
  fn create_loaded_font_with_info(
    &self,
    id: ID,
    data: Arc<Vec<u8>>,
    face_info: &fontdb::FaceInfo,
  ) -> LoadedFont {
    let family = face_info
      .families
      .first()
      .map(|(name, _)| name.clone())
      .unwrap_or_else(|| "Unknown".to_string());

    // The offline fixture evidence loop diffs FastRender output against headless Chrome baselines.
    // In CI those FastRender renders use bundled fonts for determinism, but Chrome uses system
    // fonts. Some bundled fallback faces ship with line metrics that differ substantially from
    // Chrome's default Linux sans/serif fonts, causing large vertical drift on pages that rely on
    // `line-height: normal` (e.g. lite.cnn.com, microsoft.com).
    //
    // CSS Fonts 4 provides `size-adjust`/`ascent-override`/`descent-override`/`line-gap-override`
    // descriptors to align fallback font metrics. Apply small built-in overrides to bundled Latin
    // fallbacks so `line-height: normal` behaves closer to Chrome when fixtures omit webfonts.
    let mut face_metrics_overrides = FontFaceMetricsOverrides::default();
    let apply_bundled_overrides =
      self.is_shared_bundled_db() || self.bundled_face_ids.contains(&id);
    if apply_bundled_overrides {
      match family.as_str() {
        "Roboto Flex" => {
          // Roboto Flex is our bundled Latin sans fallback, but its default line metrics are a bit
          // taller than the typical Linux browser sans-serif fallback (Liberation Sans). When
          // offline fixtures omit webfonts (common for text-heavy pages), that difference in
          // `line-height: normal` accumulates into noticeable vertical drift and large Chrome diffs.
          //
          // Apply a small set of metric overrides approximating Liberation Sans `hhea` ratios so
          // normal line heights stay closer to Chrome without affecting shaping advances/wrapping.
          // (Overrides are percentages of the used font size; see CSS Fonts 4 §5.2.)
          face_metrics_overrides.ascent_override = Some(0.9053);
          face_metrics_overrides.descent_override = Some(0.2119);
          face_metrics_overrides.line_gap_override = Some(0.0327);
        }
        "Noto Sans" | "Noto Serif" | "Noto Sans Mono" => {
          face_metrics_overrides.ascent_override = Some(0.875);
          face_metrics_overrides.descent_override = Some(0.25);
          face_metrics_overrides.line_gap_override = Some(0.0);
        }
        "Noto Sans SC" | "Noto Sans TC" | "Noto Sans JP" | "Noto Sans KR" => {
          // Noto Sans CJK fallbacks are used when rendering Han/Hiragana/Katakana/Hangul glyphs in
          // offline fixtures. Their default (hhea) metrics are designed for Windows "win" values
          // and are substantially taller than typical browser defaults, causing large vertical drift
          // as many lines of text stack.
          //
          // Apply bundled-only metric overrides so `line-height: normal` matches the injected
          // Times-like generic serif font used in the Chrome baseline harness (STIX Two Math):
          // - ascent/descent ratios align baselines across Latin + CJK runs so mixed-script lines
          //   (e.g. "TF…", "Joe…") do not expand,
          // - 0.25em line gap yields a 1.25em default line height (15px at 12px), matching the
          //   baseline spacing on weibo.cn and other text-heavy CJK fixtures.
          face_metrics_overrides.ascent_override = Some(0.762);
          face_metrics_overrides.descent_override = Some(0.238);
          face_metrics_overrides.line_gap_override = Some(0.25);
        }
        "STIX Two Math" => {
          // STIX Two Math provides good Times-like glyph metrics for serif fallback, but ships with
          // a relatively large line gap (~0.25em) that makes `line-height: normal` taller than
          // Chrome's typical serif fallback defaults. Reduce the line gap to a more typical ~0.125em
          // to keep offline fixture renders closer to Chrome without changing text widths/wrapping.
          face_metrics_overrides.line_gap_override = Some(0.125);
        }
        _ => {}
      }
    }

    LoadedFont {
      id: Some(FontId::new(id)),
      data,
      index: face_info.index,
      face_metrics_overrides,
      face_settings: FontFaceShapingDescriptors::default(),
      family,
      weight: FontWeight(face_info.weight.0),
      style: face_info.style.into(),
      stretch: face_info.stretch.into(),
    }
  }

  /// Resolves a font family list with fallbacks
  ///
  /// Tries each family in the list until a match is found.
  /// This implements CSS font-family fallback behavior.
  ///
  /// # Arguments
  ///
  /// * `families` - List of font families in priority order
  /// * `weight` - Desired font weight
  /// * `style` - Desired font style
  ///
  /// # Example
  ///
  /// ```rust,ignore
  /// let db = FontDatabase::new();
  /// let families = vec![
  ///     "CustomFont".to_string(),  // First choice (may not exist)
  ///     "Arial".to_string(),       // Second choice
  ///     "sans-serif".to_string(),  // Final fallback
  /// ];
  /// let id = db.resolve_family_list(&families, FontWeight::NORMAL, FontStyle::Normal);
  /// ```
  pub fn resolve_family_list(
    &self,
    families: &[String],
    weight: FontWeight,
    style: FontStyle,
  ) -> Option<ID> {
    for family in families {
      if let Some(id) = self.query(family, weight, style) {
        return Some(id);
      }
    }

    // Final fallback to sans-serif
    self.query("sans-serif", weight, style)
  }

  /// Resolves a font family list with full properties
  pub fn resolve_family_list_full(
    &self,
    families: &[String],
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
  ) -> Option<ID> {
    for family in families {
      if let Some(id) = self.query_full(family, weight, style, stretch) {
        return Some(id);
      }
    }

    self.query_full("sans-serif", weight, style, stretch)
  }

  /// Returns the number of fonts in the database
  #[inline]
  pub fn font_count(&self) -> usize {
    self.db.len()
  }

  /// Returns whether the database is empty
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.db.is_empty()
  }

  /// Clears the font data cache
  ///
  /// Useful to free memory when fonts are no longer needed.
  pub fn clear_cache(&self) {
    if let Ok(mut cache) = self.cache.write() {
      cache.clear();
    }
    self.glyph_coverage.clear();
    if let Ok(mut cached) = self.math_fonts.write() {
      *cached = None;
    }
    if let Ok(mut cached) = self.emoji_fonts.write() {
      *cached = None;
    }
  }

  /// Returns the number of cached fonts
  pub fn cache_size(&self) -> usize {
    self.cache.read().map(|c| c.len()).unwrap_or(0)
  }

  // ========================================================================
  // Glyph checking methods (for fallback chain support)
  // ========================================================================

  /// Returns the underlying fontdb database.
  ///
  /// Provides direct access for advanced queries.
  #[inline]
  pub fn inner(&self) -> &FontDbDatabase {
    &self.db
  }

  /// Returns an iterator over all font faces in the database.
  #[inline]
  pub fn faces(&self) -> impl Iterator<Item = &fontdb::FaceInfo> {
    self.db.faces()
  }

  pub(crate) fn cached_face(&self, id: ID) -> Option<Arc<CachedFace>> {
    self.glyph_coverage.get_or_put(id, || {
      let face_index = self.db.face(id)?.index;
      let data = self.get_or_load_font_data(id)?;
      face_cache::get_ttf_face_with_data(&data, face_index)
    })
  }

  /// Loads the first available font in the database, if any
  pub fn first_font(&self) -> Option<LoadedFont> {
    self.faces().next().and_then(|face| self.load_font(face.id))
  }

  /// Checks if a font has a glyph for the given character.
  ///
  /// This is used during fallback resolution to find a font that
  /// can render a specific character.
  pub fn has_glyph(&self, id: ID, c: char) -> bool {
    self.has_glyph_cached(id, c)
  }

  /// Checks glyph support using the cached coverage table for the font.
  pub fn has_glyph_cached(&self, id: ID, c: char) -> bool {
    self
      .cached_face(id)
      .map(|f| f.has_glyph(c))
      .unwrap_or(false)
  }

  /// Returns true when any loaded face provides a glyph for the given character.
  ///
  /// This is a convenience helper for coverage auditing tools.
  pub fn any_face_has_glyph_cached(&self, c: char) -> bool {
    self.faces().any(|face| self.has_glyph_cached(face.id, c))
  }

  /// Returns true if the font advertises any color/emoji-capable tables.
  ///
  /// Detection is table-based (COLR/CPAL, CBDT/CBLC, sbix, SVG) instead of relying on family name
  /// heuristics. Returns None if the font could not be parsed.
  pub fn is_color_capable_font(&self, id: ID) -> Option<bool> {
    self
      .cached_face(id)
      .map(|face| face_has_color_tables(face.face()))
  }

  pub(crate) fn family_name_is_emoji_font(name: &str) -> bool {
    // Called from hot font-fallback paths; avoid allocating (all needles are ASCII).
    fn contains_ascii_ci(haystack: &str, needle_lower: &[u8]) -> bool {
      let haystack = haystack.as_bytes();
      if haystack.len() < needle_lower.len() {
        return false;
      }
      for start in 0..=(haystack.len() - needle_lower.len()) {
        let mut matches = true;
        for (offset, &b) in needle_lower.iter().enumerate() {
          if haystack[start + offset].to_ascii_lowercase() != b {
            matches = false;
            break;
          }
        }
        if matches {
          return true;
        }
      }
      false
    }

    contains_ascii_ci(name, b"emoji")
      || contains_ascii_ci(name, b"color")
      || contains_ascii_ci(name, b"twemoji")
      || contains_ascii_ci(name, b"symbola")
      || contains_ascii_ci(name, b"noto color")
      || contains_ascii_ci(name, b"apple color")
      || contains_ascii_ci(name, b"segoe ui emoji")
      || contains_ascii_ci(name, b"segoe ui symbol")
  }

  /// Checks if a character is an emoji.
  ///
  /// Uses Unicode properties to determine if a character should be
  /// rendered with an emoji font. Delegates to the shared emoji
  /// detection logic to keep font fallback and emoji sequence parsing
  /// in sync.
  pub fn is_emoji(c: char) -> bool {
    emoji::is_emoji(c)
  }

  /// Finds emoji fonts in the database.
  ///
  /// Returns font IDs for fonts that are likely to contain emoji glyphs.
  ///
  /// Prefer fast family-name heuristics (e.g. "Noto Color Emoji") and only fall back to
  /// table-based color font detection when no such candidates exist, to avoid parsing every font
  /// file in large system installations.
  pub fn find_emoji_fonts(&self) -> Vec<ID> {
    if let Ok(cache) = self.emoji_fonts.read() {
      if let Some(list) = &*cache {
        return list.clone();
      }
    }

    let mut emoji_fonts: Vec<ID> = self
      .db
      .faces()
      .filter(|face| {
        face
          .families
          .iter()
          .any(|(name, _)| Self::family_name_is_emoji_font(name))
      })
      .map(|face| face.id)
      .collect();

    if emoji_fonts.is_empty() {
      for face in self.db.faces() {
        if matches!(self.is_color_capable_font(face.id), Some(true)) {
          emoji_fonts.push(face.id);
        }
      }
    }

    if let Ok(mut cache) = self.emoji_fonts.write() {
      *cache = Some(emoji_fonts.clone());
    }
    emoji_fonts
  }

  /// Returns the IDs of fonts that advertise OpenType math support (MATH table present).
  pub fn find_math_fonts(&self) -> Vec<ID> {
    if let Ok(cache) = self.math_fonts.read() {
      if let Some(list) = &*cache {
        return list.clone();
      }
    }

    let mut math_fonts = Vec::new();
    for face in self.db.faces() {
      let has_math = self
        .db
        .with_face_data(face.id, |data, face_index| {
          parse_face_with_counter(data, face_index)
            .ok()
            .and_then(|f| f.tables().math)
            .is_some()
        })
        .unwrap_or(false);

      if has_math {
        math_fonts.push(face.id);
      }
    }

    if let Ok(mut cache) = self.math_fonts.write() {
      *cache = Some(math_fonts.clone());
    }

    math_fonts
  }
}

impl Default for FontDatabase {
  fn default() -> Self {
    Self::new()
  }
}

// Make FontDatabase thread-safe
unsafe impl Send for FontDatabase {}
unsafe impl Sync for FontDatabase {}

// ============================================================================
// Font Metrics
// ============================================================================

#[inline]
fn parse_face_for_metrics<'a>(data: &'a [u8], index: u32) -> Result<ttf_parser::Face<'a>> {
  Ok(
    parse_face_with_counter(data, index).map_err(|e| FontError::LoadFailed {
      family: String::new(),
      reason: format!("Failed to parse font: {:?}", e),
    })?,
  )
}

#[inline]
fn apply_face_variations(face: &mut ttf_parser::Face<'_>, variations: &[(Tag, f32)]) -> bool {
  let mut applied = false;
  for (tag, value) in variations.iter().copied() {
    if face.set_variation(tag, value).is_some() {
      applied = true;
    }
  }
  applied
}

#[inline]
fn apply_mvar_line_metric_deltas(
  face: &ttf_parser::Face<'_>,
  variations: &[(Tag, f32)],
  ascent: &mut i16,
  descent: &mut i16,
  line_gap: &mut i16,
) {
  use crate::text::otvar::item_variation_store::{parse_item_variation_store, DeltaSetIndex};

  let Some(mvar) = face.raw_face().table(Tag::from_bytes(b"MVAR")) else {
    return;
  };

  // MVAR table format (OpenType 1.8):
  // - Fixed Version (0x00010000)
  // - u16 reserved
  // - u16 valueRecordSize
  // - u16 valueRecordCount
  // - u16 itemVariationStoreOffset
  // - MetricsValueRecord[valueRecordCount] (Tag + VarIdx)
  //
  // The VarIdx values index into the VarStore immediately following the value records. We reuse
  // the ItemVariationStore parser (same VarStore encoding) to evaluate deltas.
  if mvar.len() < 12 {
    return;
  }

  let version = u32::from_be_bytes([mvar[0], mvar[1], mvar[2], mvar[3]]);
  if version != 0x0001_0000 {
    return;
  }

  let value_record_size = u16::from_be_bytes([mvar[6], mvar[7]]) as usize;
  let value_record_count = u16::from_be_bytes([mvar[8], mvar[9]]) as usize;
  let item_var_store_offset = u16::from_be_bytes([mvar[10], mvar[11]]) as usize;
  if value_record_size < 8 || item_var_store_offset > mvar.len() {
    return;
  }

  let records_offset = 12usize;
  let Some(records_len) = value_record_size.checked_mul(value_record_count) else {
    return;
  };
  let Some(records_end) = records_offset.checked_add(records_len) else {
    return;
  };
  if records_end > mvar.len() {
    return;
  }

  let var_store_data = mvar.get(item_var_store_offset..).unwrap_or(&[]);
  let store = match parse_item_variation_store(var_store_data) {
    Ok(store) => store,
    Err(_) => return,
  };

  let axis_count = store.region_list.axis_count as usize;
  if axis_count == 0 {
    return;
  }

  let axes: Vec<_> = face.variation_axes().into_iter().collect();
  if axes.len() < axis_count {
    return;
  }

  let mut coords: Vec<f32> = Vec::with_capacity(axis_count);
  for axis in axes.iter().take(axis_count) {
    let requested = variations
      .iter()
      .find(|(tag, _)| *tag == axis.tag)
      .map(|(_, v)| *v)
      .unwrap_or(axis.def_value)
      .clamp(axis.min_value, axis.max_value);

    let normalized = if requested < axis.def_value {
      let denom = axis.def_value - axis.min_value;
      if denom.abs() > f32::EPSILON {
        (requested - axis.def_value) / denom
      } else {
        0.0
      }
    } else if requested > axis.def_value {
      let denom = axis.max_value - axis.def_value;
      if denom.abs() > f32::EPSILON {
        (requested - axis.def_value) / denom
      } else {
        0.0
      }
    } else {
      0.0
    };

    coords.push(normalized.clamp(-1.0, 1.0));
  }

  let apply_delta = |base: &mut i16, delta: f32| {
    let value = (*base as f32) + delta;
    if !value.is_finite() {
      return;
    }
    *base = value.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
  };

  let lookup_delta = |tag: Tag| -> Option<f32> {
    for i in 0..value_record_count {
      let rec_offset = records_offset.checked_add(i.checked_mul(value_record_size)?)?;
      let bytes: [u8; 4] = mvar.get(rec_offset..rec_offset + 4)?.try_into().ok()?;
      if Tag::from_bytes(&bytes) != tag {
        continue;
      }
      let var_idx = u32::from_be_bytes(mvar.get(rec_offset + 4..rec_offset + 8)?.try_into().ok()?);
      let index = DeltaSetIndex::from_u32(var_idx);
      return store.evaluate_delta(index, &coords);
    }
    None
  };

  if let Some(delta) = lookup_delta(Tag::from_bytes(b"hasc")) {
    apply_delta(ascent, delta);
  }
  if let Some(delta) = lookup_delta(Tag::from_bytes(b"hdsc")) {
    apply_delta(descent, delta);
  }
  if let Some(delta) = lookup_delta(Tag::from_bytes(b"hlgp")) {
    apply_delta(line_gap, delta);
  }
}

#[inline]
fn select_css_line_metrics(
  face: &ttf_parser::Face<'_>,
  variations: &[(Tag, f32)],
) -> (i16, i16, i16) {
  // Use the same line metric selection as the shaping backend (FreeType) and browser engines:
  //
  // - When the OS/2 `USE_TYPO_METRICS` bit (fsSelection bit 7) is **set**, prefer OS/2
  //   `sTypoAscender`, `sTypoDescender`, and `sTypoLineGap`.
  // - Otherwise prefer `hhea` metrics (`ascender`, `descender`, `lineGap`).
  //
  // In practice this matches Chrome/FreeType and avoids surprising line-height differences for
  // common system fonts.
  //
  // Reference:
  // - CSS Inline 3 § Line Gap Metrics: <https://www.w3.org/TR/css-inline-3/#line-gap-metrics>
  // - OpenType OS/2 table: <https://learn.microsoft.com/en-us/typography/opentype/spec/os2>

  #[inline]
  fn read_be_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
  }

  #[inline]
  fn read_be_i16(data: &[u8], offset: usize) -> Option<i16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(i16::from_be_bytes([bytes[0], bytes[1]]))
  }

  // Default to hhea metrics.
  let mut ascent = face.ascender();
  let mut descent = face.descender();
  let mut line_gap = face.line_gap();

  // Prefer OS/2 typographic metrics when USE_TYPO_METRICS is set.
  if let Some(os2) = face.raw_face().table(Tag::from_bytes(b"OS/2")) {
    // fsSelection @ byte offset 62, sTypo* metrics start at byte offset 68.
    if let Some(fs_selection) = read_be_u16(os2, 62) {
      const USE_TYPO_METRICS: u16 = 1 << 7;
      if fs_selection & USE_TYPO_METRICS != 0 {
        if let (Some(os2_ascent), Some(os2_descent), Some(os2_line_gap)) = (
          read_be_i16(os2, 68),
          read_be_i16(os2, 70),
          read_be_i16(os2, 72),
        ) {
          ascent = os2_ascent;
          descent = os2_descent;
          line_gap = os2_line_gap;
        }
      }
    }
  }

  // `ttf-parser` does not currently apply MVAR deltas to hhea/OS/2 line metrics, so apply those
  // ourselves to keep variable font `line-height: normal` behavior variation-aware.
  apply_mvar_line_metric_deltas(face, variations, &mut ascent, &mut descent, &mut line_gap);

  // CSS Inline 3 § Line Gap Metrics: negative line-gap values are treated as 0.
  if line_gap < 0 {
    line_gap = 0;
  }

  (ascent, descent, line_gap)
}

fn extract_metrics(face: &ttf_parser::Face, variations: &[(Tag, f32)]) -> Result<FontMetrics> {
  let units_per_em = face.units_per_em();
  let (ascent, descent, line_gap) = select_css_line_metrics(face, variations);

  // CSS spec: line-height = ascent - descent + line-gap
  let line_height = ascent - descent + line_gap;

  // CSS defines the x-height as the height of the lowercase 'x' glyph. Many fonts also provide an
  // OS/2 `sxHeight` metric, but that value is not always identical to the glyph's bounds (e.g. the
  // Nunito webfont used by cdc.gov reports a smaller `sxHeight` than the actual glyph, which can
  // shift `vertical-align: middle` replaced elements down by a fraction of a pixel and cause
  // visible diffs).
  //
  // Prefer a glyph-derived x-height when possible, falling back to OS/2 when the font has no 'x'
  // (non-Latin scripts, icon fonts, etc.).
  let x_height = {
    let glyph_x_height = face
      .glyph_index('x')
      .filter(|gid| gid.0 != 0)
      .and_then(|gid| face.glyph_bounding_box(gid))
      .map(|bbox| bbox.y_max)
      .filter(|y_max| *y_max > 0);
    let os2_x_height = face.x_height().filter(|xh| *xh > 0);
    match (glyph_x_height, os2_x_height) {
      (Some(glyph), Some(os2)) => Some(glyph.max(os2)),
      (Some(glyph), None) => Some(glyph),
      (None, Some(os2)) => Some(os2),
      (None, None) => None,
    }
  };
  let cap_height = face.capital_height();

  // Underline metrics
  let (underline_position, underline_thickness) = face
    .underline_metrics()
    .map(|m| (m.position, m.thickness))
    .unwrap_or((-(units_per_em as i16) / 10, (units_per_em as i16) / 20));

  // Strikeout metrics
  let (strikeout_position, strikeout_thickness) = face
    .strikeout_metrics()
    .map(|m| (Some(m.position), Some(m.thickness)))
    .unwrap_or((None, None));

  Ok(FontMetrics {
    units_per_em,
    ascent,
    descent,
    line_gap,
    line_height,
    x_height,
    cap_height,
    underline_position,
    underline_thickness,
    strikeout_position,
    strikeout_thickness,
    is_bold: face.is_bold(),
    is_italic: face.is_italic(),
    is_monospace: face.is_monospaced(),
  })
}

/// Font metrics in font units
///
/// Contains dimensional information extracted from font tables.
/// All values are in font design units and must be scaled by font size.
///
/// # CSS Specification
///
/// These metrics are used for CSS line-height calculations:
/// - <https://www.w3.org/TR/css-inline-3/#line-height>
///
/// # Example
///
/// ```rust,ignore
/// use fastrender::text::font_db::{FontDatabase, FontWeight, FontStyle, FontMetrics};
///
/// let db = FontDatabase::new();
/// if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
///     let font = db.load_font(id).unwrap();
///     let metrics = FontMetrics::from_font(&font).unwrap();
///     let scaled = metrics.scale(16.0); // 16px font
///     println!("Line height: {}px", scaled.line_height);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct FontMetrics {
  /// Units per em (typically 1000 or 2048)
  pub units_per_em: u16,
  /// Ascent (distance from baseline to top, positive)
  pub ascent: i16,
  /// Descent (distance from baseline to bottom, typically negative)
  pub descent: i16,
  /// Line gap (additional spacing between lines)
  pub line_gap: i16,
  /// Calculated line height (ascent - descent + line_gap)
  pub line_height: i16,
  /// x-height (height of lowercase 'x')
  pub x_height: Option<i16>,
  /// Cap height (height of uppercase letters)
  pub cap_height: Option<i16>,
  /// Underline position (negative = below baseline)
  pub underline_position: i16,
  /// Underline thickness
  pub underline_thickness: i16,
  /// Strikeout position
  pub strikeout_position: Option<i16>,
  /// Strikeout thickness
  pub strikeout_thickness: Option<i16>,
  /// Is bold (from OS/2 table)
  pub is_bold: bool,
  /// Is italic (from OS/2 table)
  pub is_italic: bool,
  /// Is monospace
  pub is_monospace: bool,
}

impl FontMetrics {
  /// Extract metrics from a loaded font
  ///
  /// # Errors
  ///
  /// Returns an error if the font data cannot be parsed.
  pub fn from_font(font: &LoadedFont) -> Result<Self> {
    face_cache::with_face(font, |face| extract_metrics(face, &[]))
      .transpose()?
      .ok_or_else(|| {
        FontError::LoadFailed {
          family: font.family.clone(),
          reason: "Failed to parse font".to_string(),
        }
        .into()
      })
  }

  /// Extract metrics from a loaded font with specific variation coordinates.
  pub fn from_font_with_variations(font: &LoadedFont, variations: &[(Tag, f32)]) -> Result<Self> {
    if variations.is_empty() {
      return Self::from_font(font);
    }

    face_cache::with_face(font, |face| -> Result<FontMetrics> {
      let mut face = face.clone();
      apply_face_variations(&mut face, variations);
      extract_metrics(&face, variations)
    })
    .transpose()?
    .ok_or_else(|| {
      FontError::LoadFailed {
        family: font.family.clone(),
        reason: "Failed to parse font".to_string(),
      }
      .into()
    })
  }

  /// Extract metrics from font data
  pub fn from_data(data: &[u8], index: u32) -> Result<Self> {
    let face = parse_face_for_metrics(data, index)?;
    Self::from_face(&face)
  }

  /// Extract metrics from font data applying variation coordinates when provided.
  pub fn from_data_with_variations(
    data: &[u8],
    index: u32,
    variations: &[(Tag, f32)],
  ) -> Result<Self> {
    let mut face = parse_face_for_metrics(data, index)?;
    if variations.is_empty() {
      return Self::from_face(&face);
    }

    apply_face_variations(&mut face, variations);
    extract_metrics(&face, variations)
  }

  /// Extract metrics from ttf-parser Face
  pub fn from_face(face: &ttf_parser::Face) -> Result<Self> {
    extract_metrics(face, &[])
  }

  /// Extract metrics from ttf-parser Face with variation coordinates applied.
  pub fn from_face_with_variations(
    face: &ttf_parser::Face,
    variations: &[(Tag, f32)],
  ) -> Result<Self> {
    if variations.is_empty() {
      return Self::from_face(face);
    }

    let mut face = face.clone();
    apply_face_variations(&mut face, variations);
    extract_metrics(&face, variations)
  }

  /// Scale metrics to pixel size
  ///
  /// Converts font units to pixels for a given font size.
  pub fn scale(&self, font_size: f32) -> ScaledMetrics {
    let scale = font_size / (self.units_per_em as f32);
    let ascent = (self.ascent as f32) * scale;
    let descent = -(self.descent as f32) * scale; // Make positive
    let line_gap = (self.line_gap as f32) * scale;
    let raw_line_height = (self.line_height as f32) * scale;
    // Headless Chrome's layout ends up with stable, whole-pixel line spacing for many fonts (both
    // system and webfonts). Our exact font-unit scaling produces fractional `line-height: normal`
    // values, which can accumulate into visible vertical drift on text-heavy pages as many lines
    // stack.
    //
    // FreeType/Skia's hinted line metrics do not simply scale the font's line height and round
    // once at the end; ascent/descent are grid-fitted independently. As a result, rounding the
    // *sum* can be off by 1px compared to Chrome:
    // - Noto @ 16px: ascent=17.104px, descent=4.688px → Chrome lines at 22px (not 21px).
    // - Roboto Flex @ 16px: raw height 18.750px → Chrome lines at 19px (not 18px).
    // - Roboto Flex @ 21px: raw height 24.609px → Chrome lines at 24px (not 25px).
    //
    // Approximate this by snapping ascent, descent, and line-gap individually to whole CSS pixels
    // and summing them. Keep the returned ascent/descent values un-snapped so baselines and glyph
    // alignment remain driven by the font metrics; only the overall line box height is snapped.
    //
    // When the snapped sum underflows the raw height by almost a full pixel (e.g. 10.94px → 10px),
    // bump it up to avoid dropping ~1px of line height per row on dense tables. This keeps the
    // "don't over-inflate" invariant while avoiding pathological underflow for fractional font
    // sizes (pt/em/etc.).
    //
    // Defensive robustness: if callers construct an inconsistent `FontMetrics` where
    // `line_height != ascent - descent + line_gap`, fall back to snapping the raw line-height
    // value directly.
    let mut line_height = ascent.round() + descent.round() + line_gap.round();
    if raw_line_height.is_finite()
      && line_height.is_finite()
      && raw_line_height - line_height >= 0.9
    {
      line_height += 1.0;
    }
    let metric_snap_plausible = raw_line_height.is_finite()
      && line_height.is_finite()
      && (line_height - raw_line_height).abs() <= 2.0;
    if !metric_snap_plausible {
      line_height = raw_line_height.floor();
      if raw_line_height.is_finite() {
        let frac = raw_line_height - line_height;
        // 0.9 is intentionally conservative: only values very close to the next pixel round up.
        if frac >= 0.9 {
          line_height += 1.0;
        }
      }
    }

    ScaledMetrics {
      font_size,
      scale,
      ascent,
      descent,
      line_gap,
      line_height,
      x_height: self.x_height.map(|h| (h as f32) * scale),
      cap_height: self.cap_height.map(|h| (h as f32) * scale),
      underline_position: (self.underline_position as f32) * scale,
      underline_thickness: (self.underline_thickness as f32) * scale,
    }
  }

  /// Calculate normal line height for a font size
  ///
  /// CSS 'line-height: normal' uses font metrics.
  #[inline]
  pub fn normal_line_height(&self, font_size: f32) -> f32 {
    self.scale(font_size).line_height
  }

  /// Returns the aspect ratio (x-height / em).
  pub fn aspect_ratio(&self) -> Option<f32> {
    self
      .x_height
      .map(|xh| xh as f32 / (self.units_per_em as f32))
      .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
  }
}

/// Scaled font metrics in pixels
///
/// Pre-computed metrics for a specific font size, ready for layout.
#[derive(Debug, Clone)]
pub struct ScaledMetrics {
  /// Font size in pixels
  pub font_size: f32,
  /// Scale factor (font_size / units_per_em)
  pub scale: f32,
  /// Ascent in pixels (above baseline)
  pub ascent: f32,
  /// Descent in pixels (positive, below baseline)
  pub descent: f32,
  /// Line gap in pixels
  pub line_gap: f32,
  /// Line height in pixels
  pub line_height: f32,
  /// x-height in pixels
  pub x_height: Option<f32>,
  /// Cap height in pixels
  pub cap_height: Option<f32>,
  /// Underline position (positive = below baseline)
  pub underline_position: f32,
  /// Underline thickness
  pub underline_thickness: f32,
}

impl ScaledMetrics {
  /// Baseline offset from top of line box
  #[inline]
  pub fn baseline_offset(&self) -> f32 {
    self.ascent
  }

  /// Total height (ascent + descent)
  #[inline]
  pub fn total_height(&self) -> f32 {
    self.ascent + self.descent
  }

  /// Apply line-height factor (e.g., 1.5 for 150%)
  pub fn with_line_height_factor(&self, factor: f32) -> Self {
    Self {
      line_height: self.font_size * factor,
      ..*self
    }
  }

  /// Apply explicit line height
  pub fn with_line_height(&self, line_height: f32) -> Self {
    Self {
      line_height,
      ..*self
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_font_weight_constants() {
    assert_eq!(FontWeight::THIN.value(), 100);
    assert_eq!(FontWeight::NORMAL.value(), 400);
    assert_eq!(FontWeight::BOLD.value(), 700);
    assert_eq!(FontWeight::BLACK.value(), 900);
  }

  #[test]
  fn test_font_weight_clamping() {
    assert_eq!(FontWeight::new(0).value(), 100);
    assert_eq!(FontWeight::new(50).value(), 100);
    assert_eq!(FontWeight::new(1000).value(), 900);
    assert_eq!(FontWeight::new(500).value(), 500);
  }

  #[test]
  fn test_font_weight_default() {
    assert_eq!(FontWeight::default(), FontWeight::NORMAL);
  }

  #[test]
  fn test_font_style_default() {
    assert_eq!(FontStyle::default(), FontStyle::Normal);
  }

  #[test]
  fn test_font_stretch_default() {
    assert_eq!(FontStretch::default(), FontStretch::Normal);
  }

  #[test]
  fn test_font_stretch_percentage() {
    assert_eq!(FontStretch::Normal.to_percentage(), 100.0);
    assert_eq!(FontStretch::Condensed.to_percentage(), 75.0);
    assert_eq!(FontStretch::Expanded.to_percentage(), 125.0);
  }

  #[test]
  fn test_font_stretch_from_percentage() {
    assert_eq!(FontStretch::from_percentage(100.0), FontStretch::Normal);
    assert_eq!(FontStretch::from_percentage(75.0), FontStretch::Condensed);
    assert_eq!(FontStretch::from_percentage(125.0), FontStretch::Expanded);
  }

  #[test]
  fn create_loaded_font_does_not_panic_if_face_info_is_missing() {
    let mut db_with_face = FontDatabase::empty();
    db_with_face
      .load_font_data(include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf").to_vec())
      .expect("load DejaVu Sans");
    let id = db_with_face
      .db
      .faces()
      .next()
      .expect("face present after loading font data")
      .id;

    let db = FontDatabase::empty();
    let data = Arc::new(Vec::new());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      db.create_loaded_font(id, Arc::clone(&data))
    }));
    assert!(result.is_ok(), "create_loaded_font should not panic");
    let font = result.unwrap();
    assert_eq!(font.family, "Unknown");
    assert_eq!(font.index, 0);
    assert!(font.metrics().is_err());
  }

  #[test]
  fn generic_serif_fallback_order_prefers_liberation_first() {
    let families = GenericFamily::Serif.fallback_families();
    let liberation = families
      .iter()
      .position(|&name| name == "Liberation Serif")
      .expect("Liberation Serif present in Serif fallback list");
    let noto = families
      .iter()
      .position(|&name| name == "Noto Serif")
      .expect("Noto Serif present in Serif fallback list");
    let dejavu = families
      .iter()
      .position(|&name| name == "DejaVu Serif")
      .expect("DejaVu Serif present in Serif fallback list");

    assert!(
      liberation < noto && liberation < dejavu,
      "expected Liberation Serif to precede other common Linux serif fallbacks"
    );
  }

  #[test]
  fn generic_sans_serif_fallback_order_prefers_liberation_first() {
    let families = GenericFamily::SansSerif.fallback_families();
    let liberation = families
      .iter()
      .position(|&name| name == "Liberation Sans")
      .expect("Liberation Sans present in SansSerif fallback list");
    let noto = families
      .iter()
      .position(|&name| name == "Noto Sans")
      .expect("Noto Sans present in SansSerif fallback list");
    let dejavu = families
      .iter()
      .position(|&name| name == "DejaVu Sans")
      .expect("DejaVu Sans present in SansSerif fallback list");

    assert!(
      liberation < noto && liberation < dejavu,
      "expected Liberation Sans to precede other common Linux sans-serif fallbacks"
    );
  }

  #[test]
  fn generic_fallbacks_prefer_non_bundled_faces_when_both_available() {
    // Regression for `FontDatabase::set_generic_fallbacks`: when bundled fonts are loaded alongside
    // non-bundled fonts, generic family mapping should prefer non-bundled candidates so fixture
    // renders in `--system-fonts` mode align with browser/system font resolution.
    //
    // In particular, the `SansSerif` fallback list includes "Noto Sans" before "DejaVu Sans"
    // (matching many modern Linux fontconfig defaults). If the host system doesn't have Noto Sans
    // installed but FastRender loads its bundled Noto Sans, selecting the bundled face would cause
    // large wrap/layout drift vs Chrome. Prefer the non-bundled DejaVu Sans when present.
    let mut db = FontDatabase::empty();
    db.load_font_data(include_bytes!("../../tests/fixtures/fonts/NotoSans-subset.ttf").to_vec())
      .expect("load Noto Sans");
    db.load_font_data(include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf").to_vec())
      .expect("load DejaVu Sans");

    let noto_ids: Vec<ID> = db
      .faces()
      .filter(|face| {
        face
          .families
          .iter()
          .any(|(name, _)| name.eq_ignore_ascii_case("Noto Sans"))
      })
      .map(|face| face.id)
      .collect();
    assert!(!noto_ids.is_empty(), "expected a Noto Sans face in fontdb");
    Arc::make_mut(&mut db.bundled_face_ids).extend(noto_ids);

    db.refresh_generic_fallbacks();

    let id = db
      .query("sans-serif", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected sans-serif match");
    let font = db.load_font(id).expect("load selected font");
    assert_eq!(font.family, "DejaVu Sans");
  }

  #[test]
  fn font_metrics_respect_use_typo_metrics_bit() {
    // The fixture font explicitly sets different OS/2 typographic metrics vs hhea metrics while
    // leaving USE_TYPO_METRICS unset. We should follow the same selection logic as browsers /
    // FreeType and use the hhea values.
    let data = include_bytes!("../../tests/fixtures/fonts/line-metrics-selection-test.ttf");
    let metrics = FontMetrics::from_data(data, 0).expect("parse metrics");
    assert_eq!(metrics.units_per_em, 1000);
    assert_eq!(metrics.ascent, 900);
    assert_eq!(metrics.descent, -300);
    assert_eq!(metrics.line_gap, 100);
    assert_eq!(metrics.line_height, 1300);
  }

  #[test]
  fn font_metrics_use_os2_typo_metrics_when_use_typo_metrics_bit_is_set() {
    // Mirrors `font_metrics_respect_use_typo_metrics_bit`, but with OS/2.fsSelection bit 7 set.
    // We should switch to OS/2 typographic metrics (sTypo*).
    let data =
      include_bytes!("../../tests/fixtures/fonts/line-metrics-selection-test-use-typo.ttf");
    let metrics = FontMetrics::from_data(data, 0).expect("parse metrics");
    assert_eq!(metrics.units_per_em, 1000);
    assert_eq!(metrics.ascent, 800);
    assert_eq!(metrics.descent, -200);
    assert_eq!(metrics.line_gap, 0);
    assert_eq!(metrics.line_height, 1000);
  }

  #[test]
  fn normal_line_height_rounding_does_not_over_inflate() {
    // When scaling yields fractional ascent+descent slightly above an integer, rounding the font's
    // height should still round to the nearest pixel. Clamping to `ceil(ascent+descent)` would
    // inflate the line height to the next pixel (21px here), causing large vertical drift as text
    // lines stack.
    let metrics = FontMetrics {
      units_per_em: 1000,
      ascent: 800,
      descent: -460,
      line_gap: 0,
      line_height: 1260,
      x_height: None,
      cap_height: None,
      underline_position: 0,
      underline_thickness: 0,
      strikeout_position: None,
      strikeout_thickness: None,
      is_bold: false,
      is_italic: false,
      is_monospace: false,
    };

    let scaled = metrics.scale(16.0);
    assert_eq!(scaled.line_height, 20.0);
  }

  #[test]
  fn normal_line_height_rounding_avoids_large_underflow_near_next_pixel() {
    // When the scaled font height lands very close to the next CSS pixel, truncation can drop
    // nearly a full pixel of line height (e.g. 10.94px → 10px), which accumulates into visible
    // drift on text-heavy pages (notably Hacker News subtext rows).
    //
    // Keep the "biased toward floor" behavior from `normal_line_height_rounding_does_not_over_inflate`,
    // but ensure we don't underflow by almost a full pixel when the fractional part is extremely high.
    let metrics = FontMetrics {
      units_per_em: 64,
      ascent: 0,
      descent: 0,
      line_gap: 0,
      // 700 * (1px / 64 units) = 10.9375px.
      line_height: 700,
      x_height: None,
      cap_height: None,
      underline_position: 0,
      underline_thickness: 0,
      strikeout_position: None,
      strikeout_thickness: None,
      is_bold: false,
      is_italic: false,
      is_monospace: false,
    };

    let scaled = metrics.scale(1.0);
    assert_eq!(scaled.line_height, 11.0);
  }

  #[test]
  fn normal_line_height_rounding_can_round_up_when_descent_crosses_half_pixel() {
    // Mirrors the IANA webfont metrics: Noto @ 16px yields a raw `line-height: normal` of 21.792px
    // (ascent=17.104px, descent=4.688px). Chrome's hinted metrics lay out lines at 22px, so we
    // must not always truncate the total scaled line-height.
    let metrics = FontMetrics {
      units_per_em: 1000,
      ascent: 1069,
      descent: -293,
      line_gap: 0,
      line_height: 1362,
      x_height: None,
      cap_height: None,
      underline_position: 0,
      underline_thickness: 0,
      strikeout_position: None,
      strikeout_thickness: None,
      is_bold: false,
      is_italic: false,
      is_monospace: false,
    };

    let scaled = metrics.scale(16.0);
    assert_eq!(scaled.line_height, 22.0);
  }

  #[test]
  fn normal_line_height_rounding_does_not_round_total_height_directly() {
    // Roboto Flex @ 21px has a raw typographic line height of 24.609375px. Chrome ends up
    // truncating this to 24px; rounding the total height would incorrectly inflate it to 25px.
    let metrics = FontMetrics {
      units_per_em: 2048,
      ascent: 1900,
      descent: -500,
      line_gap: 0,
      line_height: 2400,
      x_height: None,
      cap_height: None,
      underline_position: 0,
      underline_thickness: 0,
      strikeout_position: None,
      strikeout_thickness: None,
      is_bold: false,
      is_italic: false,
      is_monospace: false,
    };

    let scaled = metrics.scale(21.0);
    assert_eq!(scaled.line_height, 24.0);
  }

  #[test]
  fn test_generic_family_from_str() {
    assert_eq!(GenericFamily::parse("serif"), Some(GenericFamily::Serif));
    assert_eq!(
      GenericFamily::parse("sans-serif"),
      Some(GenericFamily::SansSerif)
    );
    assert_eq!(
      GenericFamily::parse("MONOSPACE"),
      Some(GenericFamily::Monospace)
    );
    assert_eq!(GenericFamily::parse("Arial"), None);
  }

  #[test]
  fn test_generic_family_fallbacks() {
    let fallbacks = GenericFamily::SansSerif.fallback_families();
    assert!(fallbacks.contains(&"Arial"));
    assert!(fallbacks.contains(&"Helvetica"));
  }

  #[test]
  fn test_generic_family_prefers_named_fallbacks_only_for_non_fontdb_generics() {
    // Core generics should resolve via `fontdb` generics first (closest match to browser defaults).
    assert!(!GenericFamily::Serif.prefers_named_fallbacks_first());
    assert!(!GenericFamily::SansSerif.prefers_named_fallbacks_first());
    assert!(!GenericFamily::Monospace.prefers_named_fallbacks_first());
    assert!(!GenericFamily::UiSansSerif.prefers_named_fallbacks_first());
    assert!(!GenericFamily::UiSerif.prefers_named_fallbacks_first());
    assert!(!GenericFamily::UiMonospace.prefers_named_fallbacks_first());
    assert!(!GenericFamily::UiRounded.prefers_named_fallbacks_first());

    // `system-ui` maps to `fontdb`'s sans-serif generic, but has an explicit OS UI font fallback
    // list that should be consulted first so it can resolve to "real" UI families like Cantarell
    // when available.
    assert!(GenericFamily::SystemUi.prefers_named_fallbacks_first());

    // Emoji/math/fangsong map to sans-serif at the `fontdb` level, so we require named fallbacks first.
    assert!(GenericFamily::Emoji.prefers_named_fallbacks_first());
    assert!(GenericFamily::Math.prefers_named_fallbacks_first());
    assert!(GenericFamily::Fangsong.prefers_named_fallbacks_first());
  }

  #[test]
  fn generic_fallback_selection_honors_fallback_list_order() {
    // Historically `FontDatabase::set_generic_fallbacks` could pick the first matching font in
    // `fontdb`'s face enumeration order, which depends on load order / platform font discovery.
    //
    // This test loads multiple candidate sans-serif families in a deliberately "wrong" order
    // (DejaVu before Noto) and asserts we still select the preferred candidate according to
    // `GenericFamily::SansSerif.fallback_families()`.
    let dejavu = include_bytes!("../../tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let noto = include_bytes!("../../tests/fixtures/fonts/NotoSans-subset.ttf");

    let mut db = FontDatabase::empty();
    db.load_font_data(dejavu.to_vec())
      .expect("load DejaVu Sans");
    db.load_font_data(noto.to_vec()).expect("load Noto Sans");
    db.refresh_generic_fallbacks();

    let id = db
      .query("sans-serif", FontWeight::NORMAL, FontStyle::Normal)
      .expect("resolve sans-serif");
    let font = db.load_font(id).expect("load resolved sans-serif font");
    assert_eq!(font.family, "Noto Sans");
  }

  #[test]
  fn helvetica_neue_aliases_follow_fontconfig_order() {
    assert_eq!(
      named_family_aliases("Helvetica Neue"),
      &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Roboto Flex"]
    );
  }

  #[test]
  fn helvetica_neue_alias_prefers_noto_sans_in_non_bundled_db() {
    let noto = include_bytes!("../../tests/fixtures/fonts/NotoSans-subset.ttf");
    let roboto = include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf");

    let mut db = FontDatabase::empty();
    db.load_font_data(roboto.to_vec())
      .expect("load Roboto Flex");
    db.load_font_data(noto.to_vec()).expect("load Noto Sans");
    db.refresh_generic_fallbacks();

    let ctx = crate::FontContext::with_database(std::sync::Arc::new(db));
    let mut style = crate::ComputedStyle::default();
    style.font_family = vec!["Helvetica Neue".to_string()].into();

    let runs = crate::ShapingPipeline::new()
      .shape("Hello", &style, &ctx)
      .expect("shaping succeeds");
    assert!(!runs.is_empty(), "expected shaped runs");
    assert_eq!(runs[0].font.family, "Noto Sans");
  }

  #[test]
  fn test_font_database_creation() {
    let db = FontDatabase::new();
    // System should have at least some fonts
    // (may be 0 in minimal CI environments)
    // font_count() returns usize which is always >= 0
    let _ = db.font_count(); // Just verify it works
  }

  #[test]
  fn test_font_database_empty() {
    let db = FontDatabase::empty();
    assert!(db.is_empty());
    assert_eq!(db.font_count(), 0);
  }

  #[test]
  fn test_query_generic_sans_serif() {
    let db = FontDatabase::new();
    // Skip if no fonts available (CI environment)
    if db.is_empty() {
      return;
    }

    let id = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal);
    // Should find at least one sans-serif font on most systems
    if let Some(id) = id {
      let font = db.load_font(id);
      assert!(font.is_some());
      let font = font.unwrap();
      assert!(!font.data.is_empty());
    }
  }

  #[test]
  fn bundled_font_aliases_resolve_common_web_families() {
    let db = FontDatabase::with_config(&FontConfig::bundled_only());

    let id = db
      .query("Helvetica", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected Helvetica to alias to a bundled sans-serif");
    let font = db.load_font(id).expect("expected aliased font to load");
    assert_eq!(font.family, "Roboto Flex");

    let id = db
      .query("Arial", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected Arial to alias to a bundled sans-serif");
    let font = db.load_font(id).expect("expected aliased font to load");
    assert_eq!(font.family, "Roboto Flex");

    let id = db
      .query("Liberation Sans", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected Liberation Sans to alias to a bundled sans-serif");
    let font = db.load_font(id).expect("expected aliased font to load");
    assert_eq!(font.family, "Roboto Flex");

    let id = db
      .query("Times New Roman", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected Times New Roman to alias to a bundled serif");
    let font = db.load_font(id).expect("expected aliased font to load");
    assert_eq!(font.family, "STIX Two Math");

    let id = db
      .query("Courier New", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected Courier New to alias to a bundled monospace");
    let font = db.load_font(id).expect("expected aliased font to load");
    assert_eq!(font.family, "Noto Sans Mono");
  }

  #[test]
  fn test_family_name_is_emoji_font_case_insensitive() {
    assert!(FontDatabase::family_name_is_emoji_font("Noto Color Emoji"));
    assert!(FontDatabase::family_name_is_emoji_font("SEGOE UI EMOJI"));
    assert!(FontDatabase::family_name_is_emoji_font("Twemoji Mozilla"));
    assert!(!FontDatabase::family_name_is_emoji_font("Noto Sans"));
  }

  #[test]
  #[cfg(debug_assertions)]
  fn find_emoji_fonts_prefers_name_heuristics_without_parsing_faces() {
    reset_face_parse_counter_for_tests();
    let _guard = FaceParseCountGuard::start();

    let noto_sans = include_bytes!("../../tests/fixtures/fonts/NotoSans-subset.ttf");
    let emoji = include_bytes!("../../tests/fixtures/fonts/FastRenderEmoji.ttf");

    let mut db = FontDatabase::empty();
    db.load_font_data(noto_sans.to_vec())
      .expect("load Noto Sans subset");
    db.load_font_data(emoji.to_vec())
      .expect("load FastRender emoji font");

    let emoji_ids = db.find_emoji_fonts();
    assert!(
      !emoji_ids.is_empty(),
      "expected find_emoji_fonts to return at least one candidate"
    );
    assert_eq!(
      face_parse_count(),
      0,
      "expected find_emoji_fonts name-heuristic path to avoid parsing font files"
    );
  }

  #[test]
  fn test_query_generic_serif() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    let id = db.query("serif", FontWeight::NORMAL, FontStyle::Normal);
    if let Some(id) = id {
      let font = db.load_font(id);
      assert!(font.is_some());
    }
  }

  #[test]
  fn test_query_generic_monospace() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    let id = db.query("monospace", FontWeight::NORMAL, FontStyle::Normal);
    if let Some(id) = id {
      let font = db.load_font(id);
      assert!(font.is_some());
    }
  }

  #[test]
  fn test_fallback_chain() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    let families = vec![
      "NonExistentFontThatShouldNotExist12345".to_string(),
      "AnotherNonExistentFont67890".to_string(),
      "sans-serif".to_string(),
    ];

    let id = db.resolve_family_list(&families, FontWeight::NORMAL, FontStyle::Normal);
    // Should fall back to sans-serif
    if let Some(id) = id {
      let font = db.load_font(id);
      assert!(font.is_some());
    }
  }

  #[test]
  fn test_font_caching() {
    let mut db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    // `FontDatabase::new()` may warm caches while selecting generic fallbacks when system fonts are
    // enabled. Clear caches explicitly so this test exercises `load_font()` caching deterministically.
    db.clear_cache();

    let id = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal);
    if let Some(id) = id {
      // First load - not cached
      assert_eq!(db.cache_size(), 0);

      // Load font
      let font1 = db.load_font(id);
      assert!(font1.is_some());

      // Should be cached now
      assert_eq!(db.cache_size(), 1);

      // Load again - should use cache
      let font2 = db.load_font(id);
      assert!(font2.is_some());

      // Same data (Arc pointing to same allocation)
      let font1 = font1.unwrap();
      let font2 = font2.unwrap();
      assert!(Arc::ptr_eq(&font1.data, &font2.data));
    }
  }

  #[test]
  fn test_clear_cache() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    let id = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal);
    if let Some(id) = id {
      let _font = db.load_font(id);
      assert!(db.cache_size() > 0);

      db.clear_cache();
      assert_eq!(db.cache_size(), 0);
    }
  }

  #[test]
  fn test_loaded_font_properties() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();

      // Should have non-empty data
      assert!(!font.data.is_empty());

      // Should have a family name
      assert!(!font.family.is_empty());

      // Weight should be reasonable
      assert!(font.weight.value() >= 100 && font.weight.value() <= 900);
    }
  }

  #[test]
  fn test_query_with_different_weights() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    // Query for different weights - fontdb does fuzzy matching
    let weights = [
      FontWeight::THIN,
      FontWeight::NORMAL,
      FontWeight::BOLD,
      FontWeight::BLACK,
    ];

    for weight in &weights {
      let id = db.query("sans-serif", *weight, FontStyle::Normal);
      // Should find something for each weight (may be same font with fuzzy matching)
      if let Some(id) = id {
        let font = db.load_font(id);
        assert!(font.is_some());
      }
    }
  }

  #[test]
  fn test_query_with_different_styles() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    let styles = [FontStyle::Normal, FontStyle::Italic, FontStyle::Oblique];

    for style in &styles {
      let id = db.query("sans-serif", FontWeight::NORMAL, *style);
      // Should find something for each style (may use fallback)
      if let Some(id) = id {
        let font = db.load_font(id);
        assert!(font.is_some());
      }
    }
  }

  // ========================================================================
  // Font Metrics Tests
  // ========================================================================

  #[test]
  fn test_font_metrics_extraction() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");

      assert!(metrics.units_per_em > 0);
      assert!(metrics.ascent > 0);
      assert!(metrics.descent < 0); // Descent is typically negative
      assert!(metrics.line_height > 0);
    }
  }

  #[test]
  fn test_scaled_metrics() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");
      let scaled = metrics.scale(16.0);

      assert_eq!(scaled.font_size, 16.0);
      assert!(scaled.ascent > 0.0);
      assert!(scaled.descent > 0.0); // Scaled descent is positive
      assert!(scaled.line_height > 0.0);
    }
  }

  #[test]
  fn test_scaled_metrics_total_height() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");
      let scaled = metrics.scale(16.0);

      // Total height should be reasonable for 16px font
      let total = scaled.total_height();
      assert!(total > 10.0 && total < 30.0);
    }
  }

  #[test]
  fn test_scaled_metrics_baseline_offset() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");
      let scaled = metrics.scale(16.0);

      // Baseline offset equals ascent
      assert_eq!(scaled.baseline_offset(), scaled.ascent);
    }
  }

  #[test]
  fn test_scaled_metrics_with_line_height_factor() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");
      let scaled = metrics.scale(16.0);
      let with_factor = scaled.with_line_height_factor(1.5);

      assert_eq!(with_factor.line_height, 24.0); // 16 * 1.5
      assert_eq!(with_factor.font_size, 16.0); // Unchanged
    }
  }

  #[test]
  fn test_normal_line_height() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let metrics = font.metrics().expect("Should extract metrics");

      let normal_lh = metrics.normal_line_height(16.0);
      assert!(normal_lh > 0.0);
      // Normal line height is usually >= font size
      assert!(normal_lh >= 14.0 && normal_lh < 32.0);
    }
  }

  fn roboto_flex_metrics() -> FontMetrics {
    FontMetrics::from_data(include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf"), 0).unwrap()
  }

  #[test]
  fn roboto_flex_normal_line_height_snaps_like_chrome_at_16px() {
    // The fixture HTML patch used by `xtask page-loop` aliases common "system" families like
    // `Helvetica` to the repo's deterministic Roboto Flex variable font. Headless Chrome + FreeType
    // snap the font's ascender/descender metrics to whole pixels, yielding a 19px `line-height:
    // normal` at 16px.
    //
    // If we instead snap the *total* font height, text-heavy pages like `lite.cnn.com` drift
    // vertically as many list items stack, and the captured viewport shows different content vs
    // Chrome.
    let lh = roboto_flex_metrics().scale(16.0).line_height;
    assert!(
      (lh - 19.0).abs() < 1e-3,
      "expected 19px line height at 16px, got {lh}"
    );
  }

  #[test]
  fn roboto_flex_normal_line_height_snaps_like_chrome_at_21px() {
    // Chrome's hinted metrics for Roboto Flex at 21px snap to a 24px `line-height: normal` (not 25px).
    let lh = roboto_flex_metrics().scale(21.0).line_height;
    assert!(
      (lh - 24.0).abs() < 1e-3,
      "expected 24px line height at 21px, got {lh}"
    );
  }

  #[test]
  fn bundled_stix_two_math_overrides_line_gap() {
    let db = FontDatabase::shared_bundled();
    let id = db
      .query("STIX Two Math", FontWeight::NORMAL, FontStyle::Normal)
      .expect("Expected bundled STIX Two Math");
    let font = db.load_font(id).expect("Load bundled STIX Two Math");

    assert_eq!(font.face_metrics_overrides.line_gap_override, Some(0.125));

    let ctx = crate::text::font_loader::FontContext::empty();
    let scaled = ctx
      .get_scaled_metrics(&font, 16.0)
      .expect("Scaled metrics should apply metric overrides");
    assert!(
      (scaled.line_gap - 2.0).abs() < 1e-3,
      "expected 2px line gap at 16px, got {}",
      scaled.line_gap
    );
    assert!(
      (scaled.line_height - 18.0).abs() < 1e-3,
      "expected 18px line height at 16px, got {}",
      scaled.line_height
    );
  }

  #[test]
  fn bundled_metric_overrides_apply_in_non_shared_font_db() {
    // `FontDatabase::shared_bundled()` is used when only bundled fonts are enabled. When system
    // fonts and/or extra font dirs are enabled, `FontDatabase::with_config` builds a private
    // fontdb instance and loads bundled fonts into it. Ensure we still apply the built-in
    // line-metric overrides to those bundled faces in that configuration (important for
    // `xtask page-loop`, which enables `--system-fonts` when patching fixtures for Chrome).
    let tmp = tempfile::tempdir().expect("temp dir");
    let config = FontConfig::bundled_only().add_font_dir(tmp.path());
    let db = FontDatabase::with_config(&config);

    let roboto_id = db
      .query("Roboto Flex", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected bundled Roboto Flex");
    let roboto = db.load_font(roboto_id).expect("load Roboto Flex");
    assert_eq!(roboto.face_metrics_overrides.ascent_override, Some(0.9053));

    let noto_id = db
      .query("Noto Sans SC", FontWeight::NORMAL, FontStyle::Normal)
      .expect("expected bundled Noto Sans SC");
    let noto = db.load_font(noto_id).expect("load Noto Sans SC");
    assert_eq!(noto.face_metrics_overrides.ascent_override, Some(0.762));
    assert_eq!(noto.face_metrics_overrides.descent_override, Some(0.238));
    assert_eq!(noto.face_metrics_overrides.line_gap_override, Some(0.25));

    let ctx = crate::text::font_loader::FontContext::empty();
    let scaled = ctx.get_scaled_metrics(&noto, 12.0).expect("scaled metrics");
    assert!(
      (scaled.line_height - 15.0).abs() < 1e-3,
      "expected 15px line height at 12px, got {}",
      scaled.line_height
    );
  }

  #[test]
  fn test_loaded_font_as_ttf_face() {
    let db = FontDatabase::new();
    if db.is_empty() {
      return;
    }

    if let Some(id) = db.query("sans-serif", FontWeight::NORMAL, FontStyle::Normal) {
      let font = db.load_font(id).unwrap();
      let face = font.as_ttf_face().expect("Should parse font");

      // Verify we can access the face
      assert!(face.units_per_em() > 0);
    }
  }

  #[test]
  fn variable_font_metrics_apply_variations_without_panic() {
    let data = include_bytes!("../../tests/fonts/RobotoFlex-VF.ttf");
    let mut face = parse_face_for_metrics(data, 0).expect("Roboto Flex should parse");
    assert!(face.is_variable());
    assert!(!face.has_non_default_variation_coordinates());

    let coords = [
      (Tag::from_bytes(b"wght"), 720.0),
      (Tag::from_bytes(b"wdth"), 95.0),
    ];
    let applied = apply_face_variations(&mut face, &coords);
    assert!(
      applied,
      "Expected at least one variation axis to be applied"
    );
    assert!(face.has_non_default_variation_coordinates());
    assert!(face.variation_coordinates().iter().any(|c| c.get() != 0));

    let metrics_from_face = FontMetrics::from_face(&face).unwrap();
    let metrics_with_vars = FontMetrics::from_face_with_variations(&face, &coords).unwrap();
    assert_eq!(
      metrics_from_face.underline_position,
      metrics_with_vars.underline_position
    );

    let metrics_from_data = FontMetrics::from_data_with_variations(data, 0, &coords).unwrap();
    assert_eq!(
      metrics_with_vars.underline_thickness,
      metrics_from_data.underline_thickness
    );
  }

  #[test]
  fn has_glyph_cached_is_deterministic() {
    let db = FontDatabase::new();
    let Some(face) = db.faces().next() else {
      return;
    };

    let expected = db
      .inner()
      .with_face_data(face.id, |data, index| {
        parse_face_with_counter(data, index).ok().map(|f| {
          (
            f.glyph_index('A').is_some_and(|gid| gid.0 != 0),
            f.glyph_index(char::MAX).is_some_and(|gid| gid.0 != 0),
          )
        })
      })
      .unwrap_or(None);

    let Some((has_a, has_max)) = expected else {
      return;
    };

    for _ in 0..4 {
      assert_eq!(db.has_glyph_cached(face.id, 'A'), has_a);
      assert_eq!(db.has_glyph_cached(face.id, char::MAX), has_max);
    }
  }
}
