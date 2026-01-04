use crate::geometry::Rect;
use crate::paint::blur::pixel_fingerprint;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::svg_filter::FilterCacheConfig;
use lru::LruCache;
use rustc_hash::FxHasher;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;
use tiny_skia::Pixmap;

type BackdropFilterHasher = BuildHasherDefault<FxHasher>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct BackdropFilterCacheKey {
  pub(crate) width: u32,
  pub(crate) height: u32,
  pub(crate) scale_bits: u32,
  pub(crate) filter_hash: u64,
  pub(crate) bbox_x_bits: u32,
  pub(crate) bbox_y_bits: u32,
  pub(crate) bbox_w_bits: u32,
  pub(crate) bbox_h_bits: u32,
  pub(crate) fingerprint: u64,
}

impl BackdropFilterCacheKey {
  pub(crate) fn new(filter_hash: u64, scale: f32, bbox: Rect, pixmap: &Pixmap) -> Option<Self> {
    if pixmap.width() == 0 || pixmap.height() == 0 {
      return None;
    }
    Some(Self {
      width: pixmap.width(),
      height: pixmap.height(),
      scale_bits: scale.to_bits(),
      filter_hash,
      bbox_x_bits: bbox.x().to_bits(),
      bbox_y_bits: bbox.y().to_bits(),
      bbox_w_bits: bbox.width().to_bits(),
      bbox_h_bits: bbox.height().to_bits(),
      fingerprint: pixel_fingerprint(pixmap.data()),
    })
  }
}

pub(crate) fn hash_filter_chain(filters: &[ResolvedFilter]) -> u64 {
  let mut hasher = FxHasher::default();
  hasher.write_u64(filters.len() as u64);
  for filter in filters {
    match filter {
      ResolvedFilter::Blur(radius) => {
        hasher.write_u8(0);
        hasher.write_u32(radius.to_bits());
      }
      ResolvedFilter::Brightness(v) => {
        hasher.write_u8(1);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Contrast(v) => {
        hasher.write_u8(2);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Grayscale(v) => {
        hasher.write_u8(3);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Sepia(v) => {
        hasher.write_u8(4);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Saturate(v) => {
        hasher.write_u8(5);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::HueRotate(v) => {
        hasher.write_u8(6);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Invert(v) => {
        hasher.write_u8(7);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::Opacity(v) => {
        hasher.write_u8(8);
        hasher.write_u32(v.to_bits());
      }
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        color,
      } => {
        hasher.write_u8(9);
        hasher.write_u32(offset_x.to_bits());
        hasher.write_u32(offset_y.to_bits());
        hasher.write_u32(blur_radius.to_bits());
        hasher.write_u32(spread.to_bits());
        hasher.write_u8(color.r);
        hasher.write_u8(color.g);
        hasher.write_u8(color.b);
        hasher.write_u32(color.a.to_bits());
      }
      ResolvedFilter::SvgFilter(filter) => {
        hasher.write_u8(10);
        hasher.write_u64(filter.fingerprint);
      }
    }
  }
  hasher.finish()
}

pub(crate) trait BackdropFilterCacheOps {
  fn config(&self) -> FilterCacheConfig;
  fn get(&mut self, key: &BackdropFilterCacheKey) -> Option<Arc<Pixmap>>;
  fn put(&mut self, key: BackdropFilterCacheKey, pixmap: &Pixmap);
}

pub(crate) struct BackdropFilterCache {
  lru: LruCache<BackdropFilterCacheKey, Arc<Pixmap>, BackdropFilterHasher>,
  current_bytes: usize,
  config: FilterCacheConfig,
}

impl BackdropFilterCache {
  pub(crate) fn new(config: FilterCacheConfig) -> Self {
    Self {
      lru: LruCache::unbounded_with_hasher(BackdropFilterHasher::default()),
      current_bytes: 0,
      config,
    }
  }

  pub(crate) fn get(&mut self, key: &BackdropFilterCacheKey) -> Option<Arc<Pixmap>> {
    self.lru.get(key).cloned()
  }

  pub(crate) fn put(&mut self, key: BackdropFilterCacheKey, pixmap: &Pixmap) {
    if self.config.max_items == 0 {
      return;
    }
    let weight = pixmap.data().len();
    if self.config.max_bytes > 0 && weight > self.config.max_bytes {
      return;
    }

    if let Some(existing) = self.lru.peek(&key) {
      let existing_weight: usize = existing.data().len();
      self.current_bytes = self.current_bytes.saturating_sub(existing_weight);
    }

    self.current_bytes = self.current_bytes.saturating_add(weight);
    self.lru.put(key, Arc::new(pixmap.clone()));
    self.evict();
  }

  fn evict(&mut self) {
    while (self.config.max_items > 0 && self.lru.len() > self.config.max_items)
      || (self.config.max_bytes > 0 && self.current_bytes > self.config.max_bytes)
    {
      if let Some((_key, value)) = self.lru.pop_lru() {
        let removed_weight: usize = value.data().len();
        self.current_bytes = self.current_bytes.saturating_sub(removed_weight);
      } else {
        break;
      }
    }
  }
}

impl Default for BackdropFilterCache {
  fn default() -> Self {
    Self::new(FilterCacheConfig::from_env())
  }
}

impl BackdropFilterCacheOps for BackdropFilterCache {
  fn config(&self) -> FilterCacheConfig {
    self.config
  }

  fn get(&mut self, key: &BackdropFilterCacheKey) -> Option<Arc<Pixmap>> {
    BackdropFilterCache::get(self, key)
  }

  fn put(&mut self, key: BackdropFilterCacheKey, pixmap: &Pixmap) {
    BackdropFilterCache::put(self, key, pixmap)
  }
}

#[derive(Clone)]
pub(crate) struct SharedBackdropFilterCache {
  inner: Arc<Mutex<BackdropFilterCache>>,
  config: FilterCacheConfig,
}

impl SharedBackdropFilterCache {
  pub(crate) fn new(config: FilterCacheConfig) -> Self {
    Self {
      inner: Arc::new(Mutex::new(BackdropFilterCache::new(config))),
      config,
    }
  }
}

impl Default for SharedBackdropFilterCache {
  fn default() -> Self {
    Self::new(FilterCacheConfig::from_env())
  }
}

impl BackdropFilterCacheOps for SharedBackdropFilterCache {
  fn config(&self) -> FilterCacheConfig {
    self.config
  }

  fn get(&mut self, key: &BackdropFilterCacheKey) -> Option<Arc<Pixmap>> {
    let mut cache = self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.get(key)
  }

  fn put(&mut self, key: BackdropFilterCacheKey, pixmap: &Pixmap) {
    let mut cache = self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.put(key, pixmap);
  }
}
