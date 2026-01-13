use std::io;

pub fn linux_set_parent_death_signal() -> io::Result<()> {
  // SAFETY: `prctl` is a process-global syscall. We provide valid arguments and check for errors.
  let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }

  // There is a race: if the parent dies between `fork`/`exec` and `prctl`, the child would become
  // orphaned and would not receive the death signal (because it wasn't configured yet).
  //
  // Checking `getppid() == 1` after setting PDEATHSIG closes that hole for normal process trees.
  // If it triggers, exit (or self-kill) immediately to avoid running unsupervised.
  //
  // SAFETY: `getppid` is a safe libc wrapper.
  if unsafe { libc::getppid() } == 1 {
    // Best-effort self-kill (should not return).
    unsafe {
      libc::raise(libc::SIGKILL);
      libc::_exit(1);
    }
  }

  Ok(())
}

