use bitflags::bitflags;

bitflags! {
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct Interest: u8 {
    const READABLE = 0b01;
    const WRITABLE = 0b10;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PollOutcome {
  /// Number of non-wakeup I/O events processed.
  pub io_events: usize,
  /// Number of wakers woken during this poll call.
  pub wakers_woken: usize,
  /// Whether the wakeup fd (eventfd) was observed.
  pub was_woken_by_notify: bool,
}

impl PollOutcome {
  pub fn wakers_fired(&self) -> bool {
    self.wakers_woken > 0
  }
}

#[cfg(target_os = "linux")]
mod epoll;

#[cfg(target_os = "linux")]
pub use epoll::Reactor;

#[cfg(not(target_os = "linux"))]
pub struct Reactor {
  _private: (),
}

#[cfg(not(target_os = "linux"))]
impl Reactor {
  fn unsupported() -> std::io::Error {
    std::io::Error::new(
      std::io::ErrorKind::Unsupported,
      "runtime-native reactor only supports linux",
    )
  }

  pub fn new() -> std::io::Result<Self> {
    Err(Self::unsupported())
  }

  pub fn register(
    &self,
    _fd: std::os::unix::io::RawFd,
    _interest: Interest,
    _waker: &std::task::Waker,
  ) -> std::io::Result<()> {
    Err(Self::unsupported())
  }

  pub fn deregister(
    &self,
    _fd: std::os::unix::io::RawFd,
    _interest: Interest,
  ) -> std::io::Result<()> {
    Err(Self::unsupported())
  }

  pub fn notify(&self) -> std::io::Result<()> {
    Err(Self::unsupported())
  }

  pub fn poll(&self, _timeout: Option<std::time::Duration>) -> std::io::Result<PollOutcome> {
    Err(Self::unsupported())
  }
}

