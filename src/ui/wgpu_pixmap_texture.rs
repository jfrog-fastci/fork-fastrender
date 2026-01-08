use tiny_skia::Pixmap;

/// A wgpu-backed texture that can be used in egui, with fast uploads from a `tiny_skia::Pixmap`.
///
/// This avoids converting to `egui::ColorImage` (which can be CPU/alloc heavy for large frames),
/// and instead writes the premultiplied RGBA8 bytes directly into a `wgpu::Texture` via
/// `queue.write_texture`.
pub struct WgpuPixmapTexture {
  texture: wgpu::Texture,
  view: wgpu::TextureView,
  id: egui::TextureId,
  size_px: (u32, u32),
  staging: Vec<u8>,
  padded_bytes_per_row: u32,
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

    if self.size_px != (w, h) {
      // If the size changes, the texture has to be recreated.
      //
      // Note: `register_native_texture` allocates a new egui `TextureId`. The old one should be
      // freed to avoid growing the renderer's texture atlas indefinitely.
      egui_renderer.free_texture(&self.id);

      let (texture, view, id) = create_and_register_texture(device, egui_renderer, w, h);
      self.texture = texture;
      self.view = view;
      self.id = id;
      self.size_px = (w, h);
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
}

fn create_and_register_texture(
  device: &wgpu::Device,
  egui_renderer: &mut egui_wgpu::Renderer,
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

  let id = egui_renderer.register_native_texture(device, &view, wgpu::FilterMode::Nearest);
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
}
