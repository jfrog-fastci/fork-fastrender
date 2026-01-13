use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "browser_ui"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

/// Stable identifier for a renderer process managed by the browser process.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct RendererProcessId(u64);

impl RendererProcessId {
  pub const fn new(raw: u64) -> Self {
    Self(raw)
  }

  pub const fn raw(self) -> u64 {
    self.0
  }
}

/// Stable identifier for a frame hosted in a renderer process.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct FrameId(u64);

impl FrameId {
  pub const fn new(raw: u64) -> Self {
    Self(raw)
  }

  pub const fn raw(self) -> u64 {
    self.0
  }
}

/// Site isolation key used to map frames to renderer processes.
///
/// Today this corresponds to an origin-like tuple (scheme, host, port), where HTTP(S) URLs use the
/// "effective" port (explicit or default).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteKey {
  scheme: String,
  host: Option<String>,
  port: Option<u16>,
}

impl SiteKey {
  pub fn new(scheme: String, host: Option<String>, port: Option<u16>) -> Self {
    Self { scheme, host, port }
  }

  /// Construct a `SiteKey` from a parsed URL.
  pub fn from_url(url: &Url) -> Self {
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url.host_str().map(|h| h.to_ascii_lowercase());
    let port = match scheme.as_str() {
      "http" | "https" => url.port_or_known_default(),
      _ => url.port(),
    };
    Self::new(scheme, host, port)
  }

  pub fn scheme(&self) -> &str {
    &self.scheme
  }

  pub fn host(&self) -> Option<&str> {
    self.host.as_deref()
  }

  pub fn port(&self) -> Option<u16> {
    self.port
  }
}

impl std::fmt::Display for SiteKey {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let host = self.host.as_deref().unwrap_or("<unknown>");
    match self.port {
      Some(port) => write!(f, "{}://{}:{}", self.scheme, host, port),
      None => write!(f, "{}://{}", self.scheme, host),
    }
  }
}

/// Handle to a spawned renderer process.
///
/// The underlying process/IPC details live elsewhere; the registry only needs stable IDs and a
/// termination hook.
pub trait ProcessHandle: std::fmt::Debug {
  fn id(&self) -> RendererProcessId;
  fn terminate(&mut self);
}

/// Abstraction for spawning renderer processes.
///
/// Unit tests provide fake implementations so we can validate process-per-origin reuse without
/// spawning OS processes.
pub trait ProcessSpawner {
  type Handle: ProcessHandle;

  fn spawn(&mut self, site: &SiteKey) -> Self::Handle;
}

/// Controls process lifetime behaviour within a [`RendererProcessRegistry`].
#[derive(Debug, Clone)]
pub struct RendererProcessRegistryConfig {
  /// When true, processes are kept alive even when no frames are retained.
  pub keep_alive: bool,
}

impl Default for RendererProcessRegistryConfig {
  fn default() -> Self {
    Self { keep_alive: false }
  }
}

#[cfg(any(test, feature = "browser_ui"))]
static RENDERER_PROCESS_SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(test, feature = "browser_ui"))]
static RENDERER_PROCESS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Total renderer process spawn operations observed (test hook).
#[cfg(any(test, feature = "browser_ui"))]
pub fn renderer_process_spawn_count_for_test() -> usize {
  RENDERER_PROCESS_SPAWN_COUNT.load(Ordering::Relaxed)
}

/// Current number of live renderer processes tracked by registries (test hook).
#[cfg(any(test, feature = "browser_ui"))]
pub fn renderer_process_count_for_test() -> usize {
  RENDERER_PROCESS_COUNT.load(Ordering::Relaxed)
}

fn record_process_spawn_for_test() {
  #[cfg(any(test, feature = "browser_ui"))]
  {
    RENDERER_PROCESS_SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
    RENDERER_PROCESS_COUNT.fetch_add(1, Ordering::Relaxed);
  }
}

fn record_process_terminated_for_test() {
  #[cfg(any(test, feature = "browser_ui"))]
  {
    let prev = RENDERER_PROCESS_COUNT.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "renderer process count underflow");
    if prev == 0 {
      RENDERER_PROCESS_COUNT.store(0, Ordering::Relaxed);
    }
  }
}

fn record_process_terminated_n_for_test(n: usize) {
  #[cfg(any(test, feature = "browser_ui"))]
  {
    let prev = RENDERER_PROCESS_COUNT.fetch_sub(n, Ordering::Relaxed);
    debug_assert!(prev >= n, "renderer process count underflow");
    if prev < n {
      RENDERER_PROCESS_COUNT.store(0, Ordering::Relaxed);
    }
  }
}

/// Browser-side mapping of `SiteKey` → renderer process.
///
/// The registry also tracks which frames are retained per process so unused processes can be
/// terminated deterministically.
#[derive(Debug)]
pub struct RendererProcessRegistry<S: ProcessSpawner> {
  spawner: S,
  config: RendererProcessRegistryConfig,
  by_site: HashMap<SiteKey, S::Handle>,
  by_process: HashMap<RendererProcessId, SiteKey>,
  frames_by_process: HashMap<RendererProcessId, HashSet<FrameId>>,
}

impl<S: ProcessSpawner> RendererProcessRegistry<S> {
  pub fn new(spawner: S) -> Self {
    Self::new_with_config(spawner, RendererProcessRegistryConfig::default())
  }

  pub fn new_with_config(spawner: S, config: RendererProcessRegistryConfig) -> Self {
    Self {
      spawner,
      config,
      by_site: HashMap::new(),
      by_process: HashMap::new(),
      frames_by_process: HashMap::new(),
    }
  }

  /// Return the renderer process ID for `site`, spawning a new renderer process if needed.
  pub fn get_or_spawn(&mut self, site: SiteKey) -> RendererProcessId {
    if let Some(handle) = self.by_site.get(&site) {
      return handle.id();
    }

    let handle = self.spawner.spawn(&site);
    let id = handle.id();

    debug_assert!(
      !self.by_process.contains_key(&id),
      "process spawner returned duplicate renderer process id: {:?}",
      id
    );

    self.by_site.insert(site.clone(), handle);
    self.by_process.insert(id, site);

    record_process_spawn_for_test();

    id
  }

  /// Retain a frame in `process`.
  ///
  /// Retaining the same `frame_id` more than once is idempotent.
  pub fn retain_frame(&mut self, process: RendererProcessId, frame_id: FrameId) {
    if !self.by_process.contains_key(&process) {
      debug_assert!(
        false,
        "retain_frame called for unknown renderer process id: {:?}",
        process
      );
      return;
    }
    self
      .frames_by_process
      .entry(process)
      .or_default()
      .insert(frame_id);
  }

  /// Release a previously-retained frame.
  ///
  /// When the last frame is released (and `keep_alive` is false), the process will be terminated and
  /// removed from the registry.
  pub fn release_frame(&mut self, process: RendererProcessId, frame_id: FrameId) {
    let Some(frames) = self.frames_by_process.get_mut(&process) else {
      debug_assert!(
        false,
        "release_frame called for process without retained frames: {:?}",
        process
      );
      return;
    };
    if !frames.remove(&frame_id) {
      debug_assert!(
        false,
        "release_frame called for unknown frame {:?} in process {:?}",
        frame_id,
        process
      );
      return;
    }
    if !frames.is_empty() {
      return;
    }
    self.frames_by_process.remove(&process);

    if self.config.keep_alive {
      return;
    }

    self.terminate_process(process);
  }

  /// Current number of spawned renderer processes held by this registry.
  pub fn process_count(&self) -> usize {
    self.by_site.len()
  }

  /// Lookup the `SiteKey` associated with a renderer process.
  pub fn site_for_process(&self, process: RendererProcessId) -> Option<&SiteKey> {
    self.by_process.get(&process)
  }

  /// Lookup the renderer process ID currently assigned to a site.
  pub fn process_for_site(&self, site: &SiteKey) -> Option<RendererProcessId> {
    self.by_site.get(site).map(ProcessHandle::id)
  }

  fn terminate_process(&mut self, process: RendererProcessId) {
    let Some(site) = self.by_process.remove(&process) else {
      debug_assert!(
        false,
        "terminate_process called for unknown renderer process id: {:?}",
        process
      );
      return;
    };

    let Some(mut handle) = self.by_site.remove(&site) else {
      debug_assert!(
        false,
        "renderer process registry missing site entry for {:?}",
        site
      );
      return;
    };

    handle.terminate();
    record_process_terminated_for_test();
  }
}

impl<S: ProcessSpawner> Drop for RendererProcessRegistry<S> {
  fn drop(&mut self) {
    let remaining = self.by_site.len();
    if remaining == 0 {
      return;
    }
    for handle in self.by_site.values_mut() {
      handle.terminate();
    }
    record_process_terminated_n_for_test(remaining);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
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
    SiteKey::from_url(&Url::parse(url).expect("test url should parse"))
  }

  #[test]
  fn same_site_reuses_process() {
    let spawn_count = Arc::new(AtomicUsize::new(0));
    let terminate_count = Arc::new(AtomicUsize::new(0));
    let drop_count = Arc::new(AtomicUsize::new(0));

    let spawner = FakeSpawner::new(
      Arc::clone(&spawn_count),
      Arc::clone(&terminate_count),
      Arc::clone(&drop_count),
    );
    let mut reg = RendererProcessRegistry::new(spawner);

    let site = site("https://example.com/a");
    let p1 = reg.get_or_spawn(site.clone());
    let p2 = reg.get_or_spawn(site);

    assert_eq!(p1, p2);
    assert_eq!(reg.process_count(), 1);
    assert_eq!(spawn_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn different_sites_spawn_different_processes() {
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
    assert_eq!(reg.process_count(), 2);
    assert_eq!(spawn_count.load(Ordering::Relaxed), 2);
  }

  #[test]
  fn process_terminated_after_last_frame_release() {
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
    let f1 = FrameId::new(1);
    reg.retain_frame(pid, f1);

    assert_eq!(reg.process_count(), 1);

    reg.release_frame(pid, f1);

    assert_eq!(reg.process_count(), 0);
    assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
    assert_eq!(drop_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn process_lingers_until_all_frames_released() {
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
    let f1 = FrameId::new(1);
    let f2 = FrameId::new(2);
    reg.retain_frame(pid, f1);
    reg.retain_frame(pid, f2);

    reg.release_frame(pid, f1);
    assert_eq!(reg.process_count(), 1);
    assert_eq!(terminate_count.load(Ordering::Relaxed), 0);
    assert_eq!(drop_count.load(Ordering::Relaxed), 0);

    reg.release_frame(pid, f2);
    assert_eq!(reg.process_count(), 0);
    assert_eq!(terminate_count.load(Ordering::Relaxed), 1);
    assert_eq!(drop_count.load(Ordering::Relaxed), 1);
  }
}
