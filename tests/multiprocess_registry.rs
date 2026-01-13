use fastrender::multiprocess::{
  FrameId, ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
  RendererProcessRegistryConfig, SiteKey,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct FakeHandle {
  id: RendererProcessId,
  terminate_count: Arc<AtomicUsize>,
  drop_count: Arc<AtomicUsize>,
}

impl ProcessHandle for FakeHandle {
  fn id(&self) -> RendererProcessId {
    self.id
  }

  fn terminate(&mut self) {
    self.terminate_count.fetch_add(1, Ordering::Relaxed);
  }
}

impl Drop for FakeHandle {
  fn drop(&mut self) {
    self.drop_count.fetch_add(1, Ordering::Relaxed);
  }
}

#[derive(Debug)]
struct FakeSpawner {
  next_id: u64,
  spawn_count: Arc<AtomicUsize>,
  terminate_count: Arc<AtomicUsize>,
  drop_count: Arc<AtomicUsize>,
}

impl FakeSpawner {
  fn new(
    spawn_count: Arc<AtomicUsize>,
    terminate_count: Arc<AtomicUsize>,
    drop_count: Arc<AtomicUsize>,
  ) -> Self {
    Self {
      next_id: 1,
      spawn_count,
      terminate_count,
      drop_count,
    }
  }
}

impl ProcessSpawner for FakeSpawner {
  type Handle = FakeHandle;

  fn spawn(&mut self, _site: &SiteKey) -> Self::Handle {
    self.spawn_count.fetch_add(1, Ordering::Relaxed);
    let id = RendererProcessId::new(self.next_id);
    self.next_id += 1;
    FakeHandle {
      id,
      terminate_count: Arc::clone(&self.terminate_count),
      drop_count: Arc::clone(&self.drop_count),
    }
  }
}

fn site(url: &str) -> SiteKey {
  fastrender::site_isolation::site_key_for_navigation(url, None, false)
}

#[test]
fn registry_reuses_process_for_same_site() {
  let spawn_count = Arc::new(AtomicUsize::new(0));
  let terminate_count = Arc::new(AtomicUsize::new(0));
  let drop_count = Arc::new(AtomicUsize::new(0));

  let spawner = FakeSpawner::new(
    Arc::clone(&spawn_count),
    Arc::clone(&terminate_count),
    Arc::clone(&drop_count),
  );
  let mut reg = RendererProcessRegistry::new(spawner);

  let key = site("https://example.com/a");
  let p1 = reg.get_or_spawn(key.clone());
  let p2 = reg.get_or_spawn(key);

  assert_eq!(p1, p2);
  assert_eq!(spawn_count.load(Ordering::Relaxed), 1);
  assert_eq!(reg.process_count(), 1);
  assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
}

#[test]
fn registry_spawns_different_processes_for_different_sites() {
  let spawn_count = Arc::new(AtomicUsize::new(0));
  let terminate_count = Arc::new(AtomicUsize::new(0));
  let drop_count = Arc::new(AtomicUsize::new(0));

  let spawner = FakeSpawner::new(
    Arc::clone(&spawn_count),
    Arc::clone(&terminate_count),
    Arc::clone(&drop_count),
  );
  let mut reg = RendererProcessRegistry::new(spawner);

  let p1 = reg.get_or_spawn(site("https://a.example/"));
  let p2 = reg.get_or_spawn(site("https://b.example/"));

  assert_ne!(p1, p2);
  assert_eq!(spawn_count.load(Ordering::Relaxed), 2);
  assert_eq!(reg.process_count(), 2);
}

#[test]
fn registry_terminates_process_after_last_frame_release() {
  let spawn_count = Arc::new(AtomicUsize::new(0));
  let terminate_count = Arc::new(AtomicUsize::new(0));
  let drop_count = Arc::new(AtomicUsize::new(0));

  let spawner = FakeSpawner::new(
    Arc::clone(&spawn_count),
    Arc::clone(&terminate_count),
    Arc::clone(&drop_count),
  );
  let mut reg = RendererProcessRegistry::new_with_config(
    spawner,
    RendererProcessRegistryConfig { keep_alive: false },
  );

  let pid = reg.get_or_spawn(site("https://example.com/"));
  let frame = FrameId::new(1);
  reg.retain_frame(pid, frame);
  reg.release_frame(pid, frame);

  assert_eq!(reg.process_count(), 0);
  assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
  assert_eq!(drop_count.load(Ordering::Relaxed), 1);
}

#[test]
fn registry_frame_refcounting_is_balanced() {
  let spawn_count = Arc::new(AtomicUsize::new(0));
  let terminate_count = Arc::new(AtomicUsize::new(0));
  let drop_count = Arc::new(AtomicUsize::new(0));

  let spawner = FakeSpawner::new(
    Arc::clone(&spawn_count),
    Arc::clone(&terminate_count),
    Arc::clone(&drop_count),
  );
  let mut reg = RendererProcessRegistry::new_with_config(
    spawner,
    RendererProcessRegistryConfig { keep_alive: false },
  );

  let pid = reg.get_or_spawn(site("https://example.com/"));
  let frame = FrameId::new(1);
  reg.retain_frame(pid, frame);
  reg.retain_frame(pid, frame);

  reg.release_frame(pid, frame);
  assert_eq!(reg.process_count(), 1);
  assert_eq!(terminate_count.load(Ordering::Relaxed), 0);

  reg.release_frame(pid, frame);
  assert_eq!(reg.process_count(), 0);
  assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
  assert_eq!(drop_count.load(Ordering::Relaxed), 1);
}

#[test]
fn registry_can_return_mutable_handle() {
  let spawn_count = Arc::new(AtomicUsize::new(0));
  let terminate_count = Arc::new(AtomicUsize::new(0));
  let drop_count = Arc::new(AtomicUsize::new(0));

  let spawner = FakeSpawner::new(
    Arc::clone(&spawn_count),
    Arc::clone(&terminate_count),
    Arc::clone(&drop_count),
  );
  let mut reg = RendererProcessRegistry::new(spawner);

  let site_key = site("https://example.com/");
  let (pid, handle) = reg
    .get_or_spawn_handle_mut(site_key.clone())
    .expect("handle");
  assert_eq!(pid, handle.id());
  assert_eq!(reg.handle_for_site(&site_key).map(ProcessHandle::id), Some(pid));
  assert_eq!(spawn_count.load(Ordering::Relaxed), 1);
}
