use fastrender::FastRender;
use fastrender::FastRenderPool;

#[test]
fn rayon_global_pool_is_capped_when_env_unset() {
  std::env::remove_var("RAYON_NUM_THREADS");

  let _renderer = FastRender::new().expect("renderer should build");

  let expected = fastrender::system::cpu_budget()
    .max(1)
    .min(fastrender::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
    .max(1);
  assert_eq!(rayon::current_num_threads(), expected);
}

#[test]
fn rayon_global_pool_is_capped_for_renderer_pool() {
  std::env::remove_var("RAYON_NUM_THREADS");

  let pool = FastRenderPool::new().expect("pool should build");
  pool
    .with_renderer(|_| Ok(()))
    .expect("pool should build a renderer");

  let expected = fastrender::system::cpu_budget()
    .max(1)
    .min(fastrender::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
    .max(1);
  assert_eq!(rayon::current_num_threads(), expected);
}
