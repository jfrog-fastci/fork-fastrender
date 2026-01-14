use crate::debug::runtime::runtime_toggles;
use tiny_skia::Pixmap;

/// A wgpu-backed texture that can be used in egui, with fast uploads from a `tiny_skia::Pixmap`.
///
/// This avoids converting to `egui::ColorImage` (which can be CPU/alloc heavy for large frames),
/// and instead writes the premultiplied RGBA8 bytes directly into a `wgpu::Texture` via
/// `queue.write_texture`.
///
/// Important: `WgpuPixmapTexture` registers a `TextureId` inside `egui_wgpu::Renderer`. Dropping
/// this type does **not** automatically unregister that ID (because `Drop` doesn't have access to
/// the renderer). Call [`WgpuPixmapTexture::destroy`] when the texture is no longer needed (e.g.
/// when closing a tab) to avoid leaking texture IDs in the renderer.
pub struct WgpuPixmapTexture {
  texture: wgpu::Texture,
  view: wgpu::TextureView,
  id: egui::TextureId,
  content_size_px: (u32, u32),
  alloc_size_px: (u32, u32),
  staging: Vec<u8>,
  padded_bytes_per_row: u32,
  filter: wgpu::FilterMode,
  allocation: WgpuPixmapTextureAllocation,
}

/// Filter mode used for displaying rasterized page content.
///
/// We default to `Nearest` to keep text crisp when we draw at 1:1 physical pixel mapping.
/// If the UI intentionally draws the image at fractional scale (e.g. smooth zooming), switching to
/// `Linear` may look better at the cost of potentially blurrier text.
const PAGE_TEXTURE_FILTER_MODE: wgpu::FilterMode = wgpu::FilterMode::Nearest;

/// Allocation bucket size used for page textures.
///
/// Resizing a browser window can produce many intermediate pixmap sizes. We avoid per-frame GPU
/// texture reallocations by rounding allocation up to a coarse grid and reusing the texture until
/// the content exceeds its capacity.
const ENV_BROWSER_PAGE_TEXTURE_BUCKET_PX: &str = "FASTR_BROWSER_PAGE_TEXTURE_BUCKET_PX";
const DEFAULT_PAGE_TEXTURE_BUCKET_PX: u32 = 64;
const MIN_PAGE_TEXTURE_BUCKET_PX: u32 = 1;
const MAX_PAGE_TEXTURE_BUCKET_PX: u32 = 512;

fn page_texture_bucket_px() -> u32 {
  // `runtime_toggles()` returns an `Arc`, so we must keep it alive for the duration of the borrow.
  let toggles = runtime_toggles();
  let Some(raw) = toggles.get(ENV_BROWSER_PAGE_TEXTURE_BUCKET_PX) else {
    return DEFAULT_PAGE_TEXTURE_BUCKET_PX;
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return DEFAULT_PAGE_TEXTURE_BUCKET_PX;
  }

  let raw = raw.replace('_', "");
  raw
    .parse::<u32>()
    .map(|value| value.clamp(MIN_PAGE_TEXTURE_BUCKET_PX, MAX_PAGE_TEXTURE_BUCKET_PX))
    .unwrap_or(DEFAULT_PAGE_TEXTURE_BUCKET_PX)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum WgpuPixmapTextureAllocation {
  /// Allocate exactly the pixmap size, recreating the underlying wgpu texture on any size change.
  ///
  /// This is used for small UI assets such as favicons where we don't have per-widget UV cropping.
  Exact,
  /// Allocate in buckets and rely on UV cropping to display only the active content region.
  PageBucketed,
}

/// Minimal interface we need from `egui_wgpu::Renderer` to manage texture IDs.
///
/// This is kept internal so we can unit test ID lifecycle logic without requiring a real GPU.
trait EguiWgpuTextureRegistry {
  fn register_native_texture(
    &mut self,
    device: &wgpu::Device,
    view: &wgpu::TextureView,
    filter: wgpu::FilterMode,
  ) -> egui::TextureId;

  fn free_texture(&mut self, id: &egui::TextureId);
}

impl EguiWgpuTextureRegistry for egui_wgpu::Renderer {
  fn register_native_texture(
    &mut self,
    device: &wgpu::Device,
    view: &wgpu::TextureView,
    filter: wgpu::FilterMode,
  ) -> egui::TextureId {
    egui_wgpu::Renderer::register_native_texture(self, device, view, filter)
  }

  fn free_texture(&mut self, id: &egui::TextureId) {
    egui_wgpu::Renderer::free_texture(self, id)
  }
}

fn recreate_on_resize<R, T>(
  egui_renderer: &mut R,
  size_px: &mut (u32, u32),
  id: &mut egui::TextureId,
  new_size_px: (u32, u32),
  create: impl FnOnce(&mut R) -> (T, egui::TextureId),
) -> Option<T>
where
  R: EguiWgpuTextureRegistry,
{
  if *size_px == new_size_px {
    return None;
  }

  // Free the old egui `TextureId` first to avoid the renderer's texture registry growing
  // indefinitely as we resize/recreate.
  egui_renderer.free_texture(id);

  let (resource, new_id) = create(egui_renderer);
  *id = new_id;
  *size_px = new_size_px;
  Some(resource)
}

fn reregister_on_filter_change<R>(
  egui_renderer: &mut R,
  id: &mut egui::TextureId,
  filter: &mut wgpu::FilterMode,
  new_filter: wgpu::FilterMode,
  register: impl FnOnce(&mut R) -> egui::TextureId,
) -> bool
where
  R: EguiWgpuTextureRegistry,
{
  if *filter == new_filter {
    return false;
  }

  // Free the old egui `TextureId` first to avoid the renderer's texture registry growing
  // indefinitely as we re-register the same `TextureView` with different sampler settings.
  egui_renderer.free_texture(id);

  let new_id = register(egui_renderer);
  *id = new_id;
  *filter = new_filter;
  true
}

fn round_up_to_bucket(value: u32, bucket: u32) -> u32 {
  if bucket <= 1 {
    return value;
  }
  let rem = value % bucket;
  if rem == 0 {
    value
  } else {
    value.saturating_add(bucket - rem)
  }
}

fn page_alloc_size_for_content(content_size_px: (u32, u32)) -> (u32, u32) {
  let bucket = page_texture_bucket_px();
  (
    round_up_to_bucket(content_size_px.0, bucket),
    round_up_to_bucket(content_size_px.1, bucket),
  )
}

fn recreate_on_capacity_exceeded<R, T>(
  egui_renderer: &mut R,
  content_size_px: &mut (u32, u32),
  alloc_size_px: &mut (u32, u32),
  id: &mut egui::TextureId,
  new_content_size_px: (u32, u32),
  create: impl FnOnce(&mut R, (u32, u32)) -> (T, egui::TextureId),
) -> Option<T>
where
  R: EguiWgpuTextureRegistry,
{
  *content_size_px = new_content_size_px;

  let required_alloc_size_px = page_alloc_size_for_content(new_content_size_px);
  if required_alloc_size_px.0 <= alloc_size_px.0 && required_alloc_size_px.1 <= alloc_size_px.1 {
    return None;
  }

  // Free the old egui `TextureId` first to avoid the renderer's texture registry growing
  // indefinitely as we resize/recreate.
  egui_renderer.free_texture(id);

  let (resource, new_id) = create(egui_renderer, required_alloc_size_px);
  *id = new_id;
  *alloc_size_px = required_alloc_size_px;
  Some(resource)
}

impl WgpuPixmapTexture {
  pub fn new(
    device: &wgpu::Device,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
  ) -> Self {
    Self::new_with_filter(device, egui_renderer, pixmap, PAGE_TEXTURE_FILTER_MODE)
  }

  /// Create a texture for rasterized page content.
  ///
  /// Unlike [`WgpuPixmapTexture::new`], this uses a bucketed allocation strategy so intermediate
  /// window resizes don't constantly recreate the underlying GPU texture.
  pub fn new_page(
    device: &wgpu::Device,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
  ) -> Self {
    let (w, h) = (pixmap.width(), pixmap.height());
    let content_size_px = (w, h);
    let alloc_size_px = page_alloc_size_for_content(content_size_px);
    let (texture, view, id) = create_and_register_texture(
      device,
      egui_renderer,
      alloc_size_px.0,
      alloc_size_px.1,
      PAGE_TEXTURE_FILTER_MODE,
    );

    let padded_bytes_per_row = align_to(w.saturating_mul(4), wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let mut staging = Vec::new();
    // Pre-reserve the expected size so the first upload can grow `staging` without reallocating,
    // but keep `len == 0` so we don't treat the allocation as initialized until we actually write
    // into it.
    staging.reserve_exact(staging_len(padded_bytes_per_row, h));

    Self {
      texture,
      view,
      id,
      content_size_px,
      alloc_size_px,
      staging,
      padded_bytes_per_row,
      filter: PAGE_TEXTURE_FILTER_MODE,
      allocation: WgpuPixmapTextureAllocation::PageBucketed,
    }
  }

  /// Create a texture with an explicit filter mode.
  ///
  /// This is used by browser-UI integrations for small UI assets (e.g. favicons) where linear
  /// filtering is usually preferable when the UI scales the icon to match DPI.
  pub fn new_with_filter(
    device: &wgpu::Device,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
    filter: wgpu::FilterMode,
  ) -> Self {
    let (w, h) = (pixmap.width(), pixmap.height());
    let (texture, view, id) = create_and_register_texture(device, egui_renderer, w, h, filter);

    let padded_bytes_per_row = align_to(w.saturating_mul(4), wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let mut staging = Vec::new();
    // Pre-reserve the expected size so the first upload can grow `staging` without reallocating,
    // but keep `len == 0` so we don't treat the allocation as initialized until we actually write
    // into it.
    staging.reserve_exact(staging_len(padded_bytes_per_row, h));

    Self {
      texture,
      view,
      id,
      content_size_px: (w, h),
      alloc_size_px: (w, h),
      staging,
      padded_bytes_per_row,
      filter,
      allocation: WgpuPixmapTextureAllocation::Exact,
    }
  }

  /// Switch the texture's sampler filter mode as registered in `egui_wgpu::Renderer`.
  ///
  /// This re-registers the existing `TextureView` with a new sampler (and therefore a new
  /// `TextureId`), without recreating the underlying wgpu texture.
  pub fn set_filter_mode(
    &mut self,
    device: &wgpu::Device,
    egui_renderer: &mut egui_wgpu::Renderer,
    filter: wgpu::FilterMode,
  ) {
    // Borrow the view separately so we can mutably borrow `self.id`/`self.filter` while still
    // passing the view into the register closure.
    let view = &self.view;
    reregister_on_filter_change(
      egui_renderer,
      &mut self.id,
      &mut self.filter,
      filter,
      |egui_renderer| egui_renderer.register_native_texture(device, view, filter),
    );
  }

  pub fn update(
    &mut self,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
  ) -> bool {
    let (w, h) = (pixmap.width(), pixmap.height());
    let new_content_size_px = (w, h);
    let src_stride = w.saturating_mul(4);
    let mut recreated = false;

    match self.allocation {
      WgpuPixmapTextureAllocation::Exact => {
        self.content_size_px = new_content_size_px;
        if let Some((texture, view)) = recreate_on_resize(
          egui_renderer,
          &mut self.alloc_size_px,
          &mut self.id,
          new_content_size_px,
          |egui_renderer| {
            let (texture, view, id) =
              create_and_register_texture(device, egui_renderer, w, h, self.filter);
            ((texture, view), id)
          },
        ) {
          self.texture = texture;
          self.view = view;
          recreated = true;
        }
      }
      WgpuPixmapTextureAllocation::PageBucketed => {
        if let Some((texture, view)) = recreate_on_capacity_exceeded(
          egui_renderer,
          &mut self.content_size_px,
          &mut self.alloc_size_px,
          &mut self.id,
          new_content_size_px,
          |egui_renderer, alloc_size_px| {
            let (alloc_w, alloc_h) = alloc_size_px;
            let (texture, view, id) =
              create_and_register_texture(device, egui_renderer, alloc_w, alloc_h, self.filter);
            ((texture, view), id)
          },
        ) {
          self.texture = texture;
          self.view = view;
          recreated = true;
        }
      }
    }

    // Upload staging buffers are based on *content* dimensions, not allocation size.
    let padded_bytes_per_row = align_to(src_stride, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    self.padded_bytes_per_row = padded_bytes_per_row;

    // Fast path: if the source row stride is already 256-byte aligned, we can upload directly
    // from the pixmap's backing bytes (no staging copy).
    if padded_bytes_per_row == src_stride {
      // Preserve shrinking behavior from the previous `Vec::resize` implementation, but avoid any
      // growth/initialization (staging is unused in this fast path).
      let expected_len = staging_len(padded_bytes_per_row, h);
      if self.staging.len() > expected_len {
        self.staging.truncate(expected_len);
      }

      queue.write_texture(
        wgpu::ImageCopyTexture {
          texture: &self.texture,
          mip_level: 0,
          origin: wgpu::Origin3d::ZERO,
          aspect: wgpu::TextureAspect::All,
        },
        pixmap.data(),
        wgpu::ImageDataLayout {
          offset: 0,
          bytes_per_row: Some(src_stride),
          rows_per_image: Some(h),
        },
        wgpu::Extent3d {
          width: w,
          height: h,
          depth_or_array_layers: 1,
        },
      );
      return recreated;
    }

    // Ensure staging is correctly sized even if the pixmap dimensions match but the alignment
    // constant changes (unlikely) or we loaded older serialized state.
    let expected_len = staging_len(padded_bytes_per_row, h);
    if self.staging.len() != expected_len {
      resize_staging_buffer_for_copy(&mut self.staging, expected_len);
    }
    debug_assert_eq!(self.staging.len(), expected_len);

    copy_pixmap_to_padded_staging(pixmap, padded_bytes_per_row, &mut self.staging);

    queue.write_texture(
      wgpu::ImageCopyTexture {
        texture: &self.texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
      },
      &self.staging,
      wgpu::ImageDataLayout {
        offset: 0,
        bytes_per_row: Some(padded_bytes_per_row),
        rows_per_image: Some(h),
      },
      wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
      },
    );
    recreated
  }

  pub fn id(&self) -> egui::TextureId {
    self.id
  }

  /// Current sampler filter mode used by this texture's `TextureId` registration.
  pub fn filter_mode(&self) -> wgpu::FilterMode {
    self.filter
  }

  pub fn size_px(&self) -> (u32, u32) {
    self.content_size_px
  }

  pub fn alloc_size_px(&self) -> (u32, u32) {
    self.alloc_size_px
  }

  /// Normalized UV rect that crops the displayed region to the active pixmap content.
  pub fn uv_rect(&self) -> egui::Rect {
    let (content_w, content_h) = self.content_size_px;
    let (alloc_w, alloc_h) = self.alloc_size_px;
    if alloc_w == 0 || alloc_h == 0 {
      return egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.0, 0.0));
    }
    let max_x = (content_w as f32 / alloc_w as f32).min(1.0);
    let max_y = (content_h as f32 / alloc_h as f32).min(1.0);
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(max_x, max_y))
  }

  pub fn size_points(&self, pixels_per_point: f32) -> egui::Vec2 {
    let (w, h) = self.content_size_px;
    egui::vec2(w as f32 / pixels_per_point, h as f32 / pixels_per_point)
  }

  /// Unregister the underlying `TextureId` from `egui_wgpu::Renderer`.
  ///
  /// This should be called when dropping the page texture (e.g. when closing a tab) to avoid
  /// leaking texture IDs in the renderer. It is safe to drop the returned `WgpuPixmapTexture`
  /// afterwards; the underlying wgpu texture/view resources will be released normally via `Drop`.
  pub fn destroy(self, egui_renderer: &mut egui_wgpu::Renderer) {
    egui_renderer.free_texture(&self.id);
  }
}

fn create_and_register_texture(
  device: &wgpu::Device,
  egui_renderer: &mut impl EguiWgpuTextureRegistry,
  width: u32,
  height: u32,
  filter: wgpu::FilterMode,
) -> (wgpu::Texture, wgpu::TextureView, egui::TextureId) {
  let texture = device.create_texture(&wgpu::TextureDescriptor {
    label: Some("fastrender_pixmap"),
    size: wgpu::Extent3d {
      width,
      height,
      depth_or_array_layers: 1,
    },
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    // `tiny_skia::Pixmap` is RGBA8 in sRGB space.
    format: wgpu::TextureFormat::Rgba8UnormSrgb,
    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    view_formats: &[],
  });

  let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

  let id = egui_renderer.register_native_texture(device, &view, filter);
  (texture, view, id)
}

fn staging_len(padded_bytes_per_row: u32, height: u32) -> usize {
  padded_bytes_per_row as usize * height as usize
}

fn resize_staging_buffer_for_copy(dst: &mut Vec<u8>, new_len: usize) {
  match dst.len().cmp(&new_len) {
    std::cmp::Ordering::Less => {
      // Avoid zero-filling potentially large allocations; the caller guarantees it will fully
      // initialize the extended region before any read.
      dst.reserve_exact(new_len - dst.len());
      // SAFETY: The caller must fully initialize all bytes in `[old_len, new_len)` before any read
      // or any subsequent operation that could copy elements (e.g. another growth reallocation).
      unsafe {
        dst.set_len(new_len);
      }
    }
    std::cmp::Ordering::Equal => {}
    std::cmp::Ordering::Greater => dst.truncate(new_len),
  }
}

fn align_to(value: u32, alignment: u32) -> u32 {
  let rem = value % alignment;
  if rem == 0 {
    value
  } else {
    value.saturating_add(alignment - rem)
  }
}

fn copy_pixmap_to_padded_staging(pixmap: &Pixmap, padded_bytes_per_row: u32, dst: &mut [u8]) {
  let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
  let src_stride = w * 4;
  let dst_stride = padded_bytes_per_row as usize;

  debug_assert!(dst_stride >= src_stride);
  debug_assert_eq!(dst.len(), dst_stride * h);

  let src = pixmap.data();
  debug_assert_eq!(src.len(), src_stride * h);

  for row in 0..h {
    let src_off = row * src_stride;
    let dst_off = row * dst_stride;
    dst[dst_off..dst_off + src_stride].copy_from_slice(&src[src_off..src_off + src_stride]);
    // Make padding deterministic (helps tests/debugging); wgpu will ignore these bytes.
    if dst_stride > src_stride {
      dst[dst_off + src_stride..dst_off + dst_stride].fill(0);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::Arc;

  fn with_test_thread_toggles<T>(raw: HashMap<String, String>, f: impl FnOnce() -> T) -> T {
    with_thread_runtime_toggles(Arc::new(RuntimeToggles::from_map(raw)), f)
  }

  fn with_default_page_texture_bucket<T>(f: impl FnOnce() -> T) -> T {
    with_test_thread_toggles(HashMap::new(), f)
  }

  #[test]
  fn align_to_works() {
    assert_eq!(align_to(0, 256), 0);
    assert_eq!(align_to(1, 256), 256);
    assert_eq!(align_to(255, 256), 256);
    assert_eq!(align_to(256, 256), 256);
    assert_eq!(align_to(257, 256), 512);
  }

  #[test]
  fn staging_copy_pads_rows_to_256() {
    let mut pixmap = Pixmap::new(3, 2).unwrap();

    // Fill pixmap with deterministic bytes.
    for (i, b) in pixmap.data_mut().iter_mut().enumerate() {
      *b = i as u8;
    }

    let padded = align_to(3 * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    assert_eq!(padded, 256);

    let mut staging = vec![0xAA; staging_len(padded, 2)];
    copy_pixmap_to_padded_staging(&pixmap, padded, &mut staging);

    let src = pixmap.data();
    assert_eq!(&staging[0..12], &src[0..12]);
    assert!(staging[12..256].iter().all(|&b| b == 0));

    assert_eq!(&staging[256..256 + 12], &src[12..24]);
    assert!(staging[256 + 12..512].iter().all(|&b| b == 0));
  }

  #[test]
  fn staging_copy_fully_initializes_after_growing_without_zero_fill() {
    let mut pixmap = Pixmap::new(3, 2).unwrap();

    // Fill pixmap with deterministic bytes.
    for (i, b) in pixmap.data_mut().iter_mut().enumerate() {
      *b = i as u8;
    }

    let padded = align_to(3 * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    assert_eq!(padded, 256);

    let expected_len = staging_len(padded, 2);
    let mut staging = vec![0xAA; expected_len / 2];

    resize_staging_buffer_for_copy(&mut staging, expected_len);

    // Safety contract: `resize_staging_buffer_for_copy` may leave new bytes uninitialized. Ensure
    // we fully overwrite before reading.
    copy_pixmap_to_padded_staging(&pixmap, padded, &mut staging);

    let src = pixmap.data();
    assert_eq!(&staging[0..12], &src[0..12]);
    assert!(staging[12..256].iter().all(|&b| b == 0));

    assert_eq!(&staging[256..256 + 12], &src[12..24]);
    assert!(staging[256 + 12..512].iter().all(|&b| b == 0));
  }

  #[test]
  fn recreate_on_resize_frees_old_texture_id_before_registering_new() {
    #[derive(Default)]
    struct MockRegistry {
      freed: Vec<egui::TextureId>,
    }

    impl EguiWgpuTextureRegistry for MockRegistry {
      fn register_native_texture(
        &mut self,
        _device: &wgpu::Device,
        _view: &wgpu::TextureView,
        _filter: wgpu::FilterMode,
      ) -> egui::TextureId {
        unreachable!("not used by this test")
      }

      fn free_texture(&mut self, id: &egui::TextureId) {
        self.freed.push(*id);
      }
    }

    let mut registry = MockRegistry::default();
    let mut size_px = (10, 10);
    let mut id = egui::TextureId::User(1);
    let old_id = id;

    let recreated =
      recreate_on_resize(&mut registry, &mut size_px, &mut id, (20, 30), |registry| {
        // Ensure we freed the old id before trying to "register" a new one.
        assert_eq!(registry.freed, vec![old_id]);
        ((), egui::TextureId::User(2))
      });

    assert!(recreated.is_some());
    assert_eq!(registry.freed, vec![old_id]);
    assert_eq!(size_px, (20, 30));
    assert_eq!(id, egui::TextureId::User(2));
  }

  #[test]
  fn reregister_on_filter_change_frees_old_texture_id_before_registering_new() {
    #[derive(Default)]
    struct MockRegistry {
      freed: Vec<egui::TextureId>,
      registered_filters: Vec<wgpu::FilterMode>,
    }

    impl EguiWgpuTextureRegistry for MockRegistry {
      fn register_native_texture(
        &mut self,
        _device: &wgpu::Device,
        _view: &wgpu::TextureView,
        _filter: wgpu::FilterMode,
      ) -> egui::TextureId {
        unreachable!("not used by this test")
      }

      fn free_texture(&mut self, id: &egui::TextureId) {
        self.freed.push(*id);
      }
    }

    let mut registry = MockRegistry::default();
    let mut id = egui::TextureId::User(1);
    let mut filter = wgpu::FilterMode::Nearest;
    let old_id = id;

    let changed = reregister_on_filter_change(
      &mut registry,
      &mut id,
      &mut filter,
      wgpu::FilterMode::Linear,
      |registry| {
        // Ensure we freed the old id before trying to "register" a new one.
        assert_eq!(registry.freed, vec![old_id]);
        registry.registered_filters.push(wgpu::FilterMode::Linear);
        egui::TextureId::User(2)
      },
    );

    assert!(changed);
    assert_eq!(registry.freed, vec![old_id]);
    assert_eq!(registry.registered_filters, vec![wgpu::FilterMode::Linear]);
    assert_eq!(id, egui::TextureId::User(2));
    assert_eq!(filter, wgpu::FilterMode::Linear);
  }

  #[test]
  fn reregister_on_filter_change_is_noop_when_filter_is_unchanged() {
    #[derive(Default)]
    struct MockRegistry {
      freed: Vec<egui::TextureId>,
    }

    impl EguiWgpuTextureRegistry for MockRegistry {
      fn register_native_texture(
        &mut self,
        _device: &wgpu::Device,
        _view: &wgpu::TextureView,
        _filter: wgpu::FilterMode,
      ) -> egui::TextureId {
        unreachable!("not used by this test")
      }

      fn free_texture(&mut self, id: &egui::TextureId) {
        self.freed.push(*id);
      }
    }

    let mut registry = MockRegistry::default();
    let mut id = egui::TextureId::User(1);
    let mut filter = wgpu::FilterMode::Nearest;

    let changed = reregister_on_filter_change(
      &mut registry,
      &mut id,
      &mut filter,
      wgpu::FilterMode::Nearest,
      |_registry| unreachable!("register should not be called when filter is unchanged"),
    );

    assert!(!changed);
    assert!(registry.freed.is_empty());
    assert_eq!(id, egui::TextureId::User(1));
    assert_eq!(filter, wgpu::FilterMode::Nearest);
  }

  #[test]
  fn recreate_on_capacity_exceeded_is_noop_when_new_content_fits_existing_alloc() {
    with_default_page_texture_bucket(|| {
      #[derive(Default)]
      struct MockRegistry {
        freed: Vec<egui::TextureId>,
      }

      impl EguiWgpuTextureRegistry for MockRegistry {
        fn register_native_texture(
          &mut self,
          _device: &wgpu::Device,
          _view: &wgpu::TextureView,
          _filter: wgpu::FilterMode,
        ) -> egui::TextureId {
          unreachable!("not used by this test")
        }

        fn free_texture(&mut self, id: &egui::TextureId) {
          self.freed.push(*id);
        }
      }

      let mut registry = MockRegistry::default();
      let mut content_size_px = (100, 100);
      let mut alloc_size_px = page_alloc_size_for_content(content_size_px);
      assert_eq!(alloc_size_px, (128, 128));
      let mut id = egui::TextureId::User(1);

      let recreated: Option<()> = recreate_on_capacity_exceeded(
        &mut registry,
        &mut content_size_px,
        &mut alloc_size_px,
        &mut id,
        (120, 127),
        |_registry, _new_alloc| unreachable!("should not recreate when content fits in alloc"),
      );

      assert!(recreated.is_none());
      assert!(registry.freed.is_empty());
      assert_eq!(id, egui::TextureId::User(1));
      assert_eq!(content_size_px, (120, 127));
      assert_eq!(alloc_size_px, (128, 128));
    });
  }

  #[test]
  fn recreate_on_capacity_exceeded_frees_old_texture_id_once_when_bucket_is_exceeded() {
    with_default_page_texture_bucket(|| {
      #[derive(Default)]
      struct MockRegistry {
        freed: Vec<egui::TextureId>,
      }

      impl EguiWgpuTextureRegistry for MockRegistry {
        fn register_native_texture(
          &mut self,
          _device: &wgpu::Device,
          _view: &wgpu::TextureView,
          _filter: wgpu::FilterMode,
        ) -> egui::TextureId {
          unreachable!("not used by this test")
        }

        fn free_texture(&mut self, id: &egui::TextureId) {
          self.freed.push(*id);
        }
      }

      let mut registry = MockRegistry::default();
      let mut content_size_px = (100, 100);
      let mut alloc_size_px = page_alloc_size_for_content(content_size_px);
      let mut id = egui::TextureId::User(1);
      let old_id = id;

      let recreated = recreate_on_capacity_exceeded(
        &mut registry,
        &mut content_size_px,
        &mut alloc_size_px,
        &mut id,
        (129, 127),
        |registry, new_alloc_size_px| {
          // Ensure we freed the old id before trying to "register" a new one.
          assert_eq!(registry.freed, vec![old_id]);
          assert_eq!(new_alloc_size_px, (192, 128));
          ((), egui::TextureId::User(2))
        },
      );

      assert!(recreated.is_some());
      assert_eq!(registry.freed, vec![old_id]);
      assert_eq!(id, egui::TextureId::User(2));
      assert_eq!(content_size_px, (129, 127));
      assert_eq!(alloc_size_px, (192, 128));

      // Updating again within the same bucket should not free/recreate again.
      let recreated: Option<()> = recreate_on_capacity_exceeded(
        &mut registry,
        &mut content_size_px,
        &mut alloc_size_px,
        &mut id,
        (191, 128),
        |_registry, _new_alloc| unreachable!("should not recreate when content fits in alloc"),
      );

      assert!(recreated.is_none());
      assert_eq!(registry.freed, vec![old_id]);
      assert_eq!(id, egui::TextureId::User(2));
      assert_eq!(content_size_px, (191, 128));
      assert_eq!(alloc_size_px, (192, 128));
    });
  }

  #[test]
  fn page_alloc_size_respects_bucket_override_in_thread_toggles() {
    let raw = HashMap::from([(
      ENV_BROWSER_PAGE_TEXTURE_BUCKET_PX.to_string(),
      "12_8".to_string(),
    )]);
    with_test_thread_toggles(raw, || {
      assert_eq!(page_alloc_size_for_content((129, 127)), (256, 128));
    });
  }
}
