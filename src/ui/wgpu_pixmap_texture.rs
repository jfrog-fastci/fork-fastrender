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
  size_px: (u32, u32),
  staging: Vec<u8>,
  padded_bytes_per_row: u32,
}

/// Filter mode used for displaying rasterized page content.
///
/// We default to `Nearest` to keep text crisp when we draw at 1:1 physical pixel mapping.
/// If the UI intentionally draws the image at fractional scale (e.g. smooth zooming), switching to
/// `Linear` may look better at the cost of potentially blurrier text.
const PAGE_TEXTURE_FILTER_MODE: wgpu::FilterMode = wgpu::FilterMode::Nearest;

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

impl WgpuPixmapTexture {
  pub fn new(
    device: &wgpu::Device,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
  ) -> Self {
    let (w, h) = (pixmap.width(), pixmap.height());
    let (texture, view, id) = create_and_register_texture(device, egui_renderer, w, h);

    let padded_bytes_per_row = align_to(w.saturating_mul(4), wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let mut staging = Vec::new();
    staging.resize(staging_len(padded_bytes_per_row, h), 0);

    Self {
      texture,
      view,
      id,
      size_px: (w, h),
      staging,
      padded_bytes_per_row,
    }
  }

  pub fn update(
    &mut self,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    egui_renderer: &mut egui_wgpu::Renderer,
    pixmap: &Pixmap,
  ) {
    let (w, h) = (pixmap.width(), pixmap.height());
    let src_stride = w.saturating_mul(4);

    if let Some((texture, view)) = recreate_on_resize(
      egui_renderer,
      &mut self.size_px,
      &mut self.id,
      (w, h),
      |egui_renderer| {
        let (texture, view, id) = create_and_register_texture(device, egui_renderer, w, h);
        ((texture, view), id)
      },
    ) {
      self.texture = texture;
      self.view = view;
      self.padded_bytes_per_row = align_to(src_stride, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
      self
        .staging
        .resize(staging_len(self.padded_bytes_per_row, h), 0);
    }

    // Fast path: if the source row stride is already 256-byte aligned, we can upload directly
    // from the pixmap's backing bytes (no staging copy).
    if self.padded_bytes_per_row == src_stride {
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
      return;
    }

    // Ensure staging is correctly sized even if the pixmap dimensions match but the alignment
    // constant changes (unlikely) or we loaded older serialized state.
    let expected_len = staging_len(self.padded_bytes_per_row, h);
    if self.staging.len() != expected_len {
      self.staging.resize(expected_len, 0);
    }

    copy_pixmap_to_padded_staging(pixmap, self.padded_bytes_per_row, &mut self.staging);

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
        bytes_per_row: Some(self.padded_bytes_per_row),
        rows_per_image: Some(h),
      },
      wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
      },
    );
  }

  pub fn id(&self) -> egui::TextureId {
    self.id
  }

  pub fn size_px(&self) -> (u32, u32) {
    self.size_px
  }

  pub fn size_points(&self, pixels_per_point: f32) -> egui::Vec2 {
    let (w, h) = self.size_px;
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

  let id = egui_renderer.register_native_texture(device, &view, PAGE_TEXTURE_FILTER_MODE);
  (texture, view, id)
}

fn staging_len(padded_bytes_per_row: u32, height: u32) -> usize {
  padded_bytes_per_row as usize * height as usize
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

    let recreated = recreate_on_resize(
      &mut registry,
      &mut size_px,
      &mut id,
      (20, 30),
      |registry| {
        // Ensure we freed the old id before trying to "register" a new one.
        assert_eq!(registry.freed, vec![old_id]);
        ((), egui::TextureId::User(2))
      },
    );

    assert!(recreated.is_some());
    assert_eq!(registry.freed, vec![old_id]);
    assert_eq!(size_px, (20, 30));
    assert_eq!(id, egui::TextureId::User(2));
  }
}
