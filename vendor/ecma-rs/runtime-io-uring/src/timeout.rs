#[cfg(target_os = "linux")]
use io_uring::types::Timespec;

use std::time::Duration;

#[cfg(target_os = "linux")]
pub(crate) fn duration_to_timespec(timeout: Duration) -> Timespec {
  Timespec::new()
    .sec(timeout.as_secs())
    .nsec(timeout.subsec_nanos())
}
