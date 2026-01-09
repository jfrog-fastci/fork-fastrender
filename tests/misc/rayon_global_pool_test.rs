use fastrender::FastRender;
use fastrender::FastRenderPool;
use std::ffi::OsString;

struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  fn unset(key: &'static str) -> Self {
    let previous = std::env::var_os(key);
    std::env::remove_var(key);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

#[test]
fn rayon_global_pool_is_capped_when_env_unset() {
  let _lock = super::global_test_lock();
  let _guard = EnvVarGuard::unset("RAYON_NUM_THREADS");

  let _renderer = FastRender::new().expect("renderer should build");

  let expected = fastrender::system::cpu_budget()
    .max(1)
    .min(fastrender::layout::engine::DEFAULT_LAYOUT_AUTO_MAX_THREADS)
    .max(1);
  assert_eq!(rayon::current_num_threads(), expected);
}

#[test]
fn rayon_global_pool_is_capped_for_renderer_pool() {
  let _lock = super::global_test_lock();
  let _guard = EnvVarGuard::unset("RAYON_NUM_THREADS");

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
