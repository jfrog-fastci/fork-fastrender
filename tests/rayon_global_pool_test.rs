use fastrender::FastRender;

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

