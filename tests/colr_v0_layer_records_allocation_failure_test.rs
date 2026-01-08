use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::ColorFontRenderer;
use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use fastrender::text::font_instance::FontInstance;
use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAILED_ALLOCS: AtomicUsize = AtomicUsize::new(0);

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && new_size == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: FailingAllocator = FailingAllocator;

static LOCK: Mutex<()> = Mutex::new(());

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
  let bytes = data.get(offset..offset + 2)?;
  Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
  let bytes = data.get(offset..offset + 4)?;
  Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn layer_count_for_glyph(face: &ttf_parser::Face<'_>, glyph_id: u16) -> Option<usize> {
  let colr = face
    .raw_face()
    .table(ttf_parser::Tag::from_bytes(b"COLR"))?;
  if read_u16(colr, 0)? != 0 {
    return None;
  }
  let num_base = read_u16(colr, 2)? as usize;
  let base_offset = read_u32(colr, 4)? as usize;
  let record_size = 6usize;
  for i in 0..num_base {
    let offset = base_offset.checked_add(i.checked_mul(record_size)?)?;
    if offset.checked_add(record_size)? > colr.len() {
      return None;
    }
    if read_u16(colr, offset)? == glyph_id {
      return Some(read_u16(colr, offset + 4)? as usize);
    }
  }
  None
}

fn load_colr_v0_font() -> LoadedFont {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts/ColorTestCOLR.ttf");
  let data = std::fs::read(&path).expect("read color test COLR v0 font");
  LoadedFont {
    id: None,
    data: Arc::new(data),
    index: 0,
    family: "ColorTestCOLR".into(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
    face_settings: Default::default(),
  }
}

#[repr(C)]
struct LayerRecordLayout {
  glyph_id: u16,
  palette_index: u16,
}

#[test]
fn colr_v0_layer_records_parse_survives_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  let renderer = ColorFontRenderer::new();

  let font_ok = load_colr_v0_font();
  let face_ok = font_ok.as_ttf_face().expect("parse face");
  let glyph_id = face_ok.glyph_index('A').expect("glyph index").0 as u32;
  let instance_ok = FontInstance::new(&font_ok, &[]).expect("font instance");
  assert!(
    renderer
      .render(
        &font_ok,
        &instance_ok,
        glyph_id,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .is_some(),
    "expected baseline COLR v0 glyph render to succeed"
  );

  let layer_count =
    layer_count_for_glyph(&face_ok, glyph_id as u16).expect("extract COLR layer count");
  assert!(layer_count > 0);

  let font_fail = load_colr_v0_font();
  let instance_fail = FontInstance::new(&font_fail, &[]).expect("font instance");

  let alloc_size = layer_count * mem::size_of::<LayerRecordLayout>();
  let alloc_align = mem::align_of::<LayerRecordLayout>();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(alloc_size, alloc_align);

  let rendered = renderer.render(
    &font_fail,
    &instance_fail,
    glyph_id,
    64.0,
    0,
    &[],
    0,
    Rgba::BLACK,
    0.0,
    &[],
    None,
  );

  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger layer-record allocation failure"
  );
  assert!(
    rendered.is_none(),
    "expected color glyph render to return None after allocation failure"
  );
}

