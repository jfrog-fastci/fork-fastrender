use super::{
  RendererSandboxConfig, RendererSandboxReport, SandboxWarning, SandboxWarningKind,
};
use std::io;

pub(super) fn apply_linux_hardening(
  config: &RendererSandboxConfig,
  report: &mut RendererSandboxReport,
) {
  apply_pr_set_dumpable(report);
  apply_rlimit_as(config.address_space_limit_bytes, report);
  apply_rlimit_core(config.core_limit_bytes, report);
  apply_rlimit_nofile(config.nofile_limit, report);

  if let Some(max_processes) = config.nproc_limit {
    apply_rlimit_nproc(max_processes, report);
  }
}

fn apply_pr_set_dumpable(report: &mut RendererSandboxReport) {
  // SAFETY: `prctl` is a process-global syscall. We provide the required argument slots for the
  // PR_SET_DUMPABLE command.
  let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
  if rc == 0 {
    report.dumpable_disabled = Some(true);
    return;
  }
  report.warnings.push(SandboxWarning::new(
    SandboxWarningKind::PrctlDumpable,
    format!(
      "failed to set PR_SET_DUMPABLE=0: {}",
      io::Error::last_os_error()
    ),
  ));
  report.dumpable_disabled = Some(false);
}

fn apply_rlimit_core(limit_bytes: Option<u64>, report: &mut RendererSandboxReport) {
  let Some(limit_bytes) = limit_bytes else {
    report.rlimit_core = get_rlimit(libc::RLIMIT_CORE).ok();
    return;
  };

  apply_rlimit_clamp(
    libc::RLIMIT_CORE,
    limit_bytes,
    SandboxWarningKind::RlimitCore,
    "RLIMIT_CORE",
    report,
  );
  report.rlimit_core = get_rlimit(libc::RLIMIT_CORE).ok();
}

fn apply_rlimit_as(limit_bytes: Option<u64>, report: &mut RendererSandboxReport) {
  let Some(limit_bytes) = limit_bytes else {
    report.rlimit_as = get_rlimit(libc::RLIMIT_AS).ok();
    return;
  };

  apply_rlimit_clamp(
    libc::RLIMIT_AS,
    limit_bytes,
    SandboxWarningKind::RlimitAs,
    "RLIMIT_AS",
    report,
  );
  report.rlimit_as = get_rlimit(libc::RLIMIT_AS).ok();
}

fn apply_rlimit_nofile(max_open_files: Option<u64>, report: &mut RendererSandboxReport) {
  let Some(max_open_files) = max_open_files else {
    report.rlimit_nofile = get_rlimit(libc::RLIMIT_NOFILE).ok();
    return;
  };

  apply_rlimit_clamp(
    libc::RLIMIT_NOFILE,
    max_open_files,
    SandboxWarningKind::RlimitNofile,
    "RLIMIT_NOFILE",
    report,
  );
  report.rlimit_nofile = get_rlimit(libc::RLIMIT_NOFILE).ok();
}

fn apply_rlimit_nproc(max_processes: u64, report: &mut RendererSandboxReport) {
  apply_rlimit_clamp(
    libc::RLIMIT_NPROC,
    max_processes,
    SandboxWarningKind::RlimitNproc,
    "RLIMIT_NPROC",
    report,
  );
  report.rlimit_nproc = get_rlimit(libc::RLIMIT_NPROC).ok();
}

fn apply_rlimit_clamp(
  resource: libc::__rlimit_resource_t,
  value: u64,
  warning_kind: SandboxWarningKind,
  resource_name: &'static str,
  report: &mut RendererSandboxReport,
) {
  let (cur, max) = match get_rlimit_raw(resource) {
    Ok(value) => value,
    Err(err) => {
      report.warnings.push(SandboxWarning::new(
        warning_kind,
        format!("failed to query {resource_name}: {err}"),
      ));
      return;
    }
  };

  let requested: libc::rlim_t = match value.try_into() {
    Ok(value) => value,
    Err(_) => {
      report.warnings.push(SandboxWarning::new(
        warning_kind,
        format!("rlimit cap {value} for {resource_name} does not fit rlimit type"),
      ));
      return;
    }
  };

  // Never attempt to raise either limit. If a parent container already constrained us, honor it.
  let effective = std::cmp::min(requested, max);
  let new = libc::rlimit {
    rlim_cur: std::cmp::min(cur, effective),
    rlim_max: effective,
  };

  // SAFETY: `setrlimit` is a process-global syscall. We pass a properly-initialized `rlimit`.
  let rc = unsafe { libc::setrlimit(resource, &new) };
  if rc != 0 {
    report.warnings.push(SandboxWarning::new(
      warning_kind,
      format!(
        "failed to clamp {resource_name} to <= {value}: {}",
        io::Error::last_os_error()
      ),
    ));
  }
}

fn get_rlimit(resource: libc::__rlimit_resource_t) -> io::Result<(u64, u64)> {
  let (cur, max) = get_rlimit_raw(resource)?;
  Ok((cur as u64, max as u64))
}

fn get_rlimit_raw(
  resource: libc::__rlimit_resource_t,
) -> io::Result<(libc::rlim_t, libc::rlim_t)> {
  let mut current = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `getrlimit` writes to `current` for a valid pointer.
  let rc = unsafe { libc::getrlimit(resource, &mut current) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((current.rlim_cur, current.rlim_max))
}
