use fastrender::FastRender;

#[test]
fn global_rayon_thread_pool_is_initialized_with_a_safe_default() {
  // In CI/container environments `available_parallelism()` may report a very high core count.
  // If Rayon initialises its global pool with that many threads while the Rust test harness is
  // also running with many worker threads, thread creation can fail and panic. Creating a
  // `FastRender` instance should remain robust regardless of the harness thread count.
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html("<div>hello</div>").expect("parse");
  renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");
}

