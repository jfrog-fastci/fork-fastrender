use runtime_io_uring::IoUringDriver;

pub fn try_new_driver(entries: u32) -> Option<IoUringDriver> {
  match IoUringDriver::new(entries) {
    Ok(d) => Some(d),
    Err(e) => {
      let raw = e.raw_os_error();
      if matches!(
        raw,
        Some(libc::ENOSYS)
          | Some(libc::EINVAL)
          | Some(libc::EPERM)
          | Some(libc::EACCES)
          | Some(libc::EOPNOTSUPP)
      ) {
        eprintln!("skipping: io_uring unavailable ({e})");
        None
      } else {
        panic!("failed to create io_uring: {e}");
      }
    }
  }
}
