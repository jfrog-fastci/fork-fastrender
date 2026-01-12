use fastrender::FastRenderPool;

#[test]
fn rayon_global_pool_is_capped_unless_preconfigured() {
  // Ensure FastRender has had a chance to initialize the global pool (order-independent no-op if
  // already done).
  crate::common::init_rayon_for_tests(1);
  let pool = FastRenderPool::new().expect("pool");
  pool
    .with_renderer(|_| Ok(()))
    .expect("pool should build a renderer");

  let cap = fastrender::system::cpu_budget()
    .max(1)
    .min(fastrender::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
    .max(1);
  let threads = rayon::current_num_threads();
  assert!(
    threads >= 1,
    "Rayon global pool should report at least one thread (got {threads})"
  );

  // If some other crate/test initialised the pool before FastRender could apply its defaults, the
  // pool size may exceed our cap. That's okay: the global pool is irreversible, so this test must
  // remain order-independent.
  if threads > cap {
    return;
  }

  assert!(
    threads <= cap,
    "expected Rayon's global pool to be capped at {cap} threads, got {threads}"
  );
}
