//! Small helper binary used by macOS sandbox regression tests.
//!
//! This binary is intentionally tiny and only implements a handful of probe
//! actions. Integration tests spawn it as a child process because applying a
//! Seatbelt sandbox profile is irreversible.

use std::process::ExitCode;

#[cfg(target_os = "macos")]
use fastrender::security::macos_renderer_sandbox::{
  apply_sbpl, build_renderer_sbpl, RendererIpcMechanism,
};

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
  match real_main() {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      eprintln!("{err}");
      ExitCode::from(1)
    }
  }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
  eprintln!("macos_renderer_sandbox_ipc_probe is only supported on macOS");
  ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn real_main() -> Result<(), String> {
  let mut args = std::env::args().skip(1);
  let ipc = args
    .next()
    .ok_or_else(|| usage("missing <ipc-mechanism>"))?;
  let action = args.next().ok_or_else(|| usage("missing <action>"))?;

  let ipc = parse_ipc_mechanism(&ipc)?;

  // Apply sandbox before performing the probe operation.
  let sbpl = build_renderer_sbpl(ipc);
  apply_sbpl(&sbpl).map_err(|e| e.to_string())?;

  match action.as_str() {
    "pipe-stdio" => action_pipe_stdio(),
    "posix-shm-create" => {
      let name = args
        .next()
        .ok_or_else(|| usage("posix-shm-create requires <name>"))?;
      action_posix_shm_create(&name)
    }
    "unix-socket-connect" => {
      let path = args
        .next()
        .ok_or_else(|| usage("unix-socket-connect requires <path>"))?;
      action_unix_socket_connect(&path)
    }
    "mach-lookup" => {
      let service = args
        .next()
        .ok_or_else(|| usage("mach-lookup requires <service-name>"))?;
      action_mach_lookup(&service)
    }
    _ => Err(usage(format!("unknown action: {action}"))),
  }
}

#[cfg(target_os = "macos")]
fn usage(msg: impl std::fmt::Display) -> String {
  format!(
    "{msg}\n\n\
Usage:\n\
  macos_renderer_sandbox_ipc_probe <ipc-mechanism> <action> [action-args]\n\n\
IPC mechanisms:\n\
  pipes-only | posix-shm | unix-socket | mach-port\n\n\
Actions:\n\
  pipe-stdio\n\
  posix-shm-create <name>\n\
  unix-socket-connect <path>\n\
  mach-lookup <service-name>\n"
  )
}

#[cfg(target_os = "macos")]
fn parse_ipc_mechanism(s: &str) -> Result<RendererIpcMechanism, String> {
  match s {
    "pipes-only" => Ok(RendererIpcMechanism::PipesOnly),
    "posix-shm" => Ok(RendererIpcMechanism::PosixShm),
    "unix-socket" => Ok(RendererIpcMechanism::UnixSocket),
    "mach-port" => Ok(RendererIpcMechanism::MachPort),
    _ => Err(usage(format!("unknown ipc mechanism: {s}"))),
  }
}

#[cfg(target_os = "macos")]
fn action_pipe_stdio() -> Result<(), String> {
  // Write a fixed string to stdout. Tests capture this via an inherited pipe.
  let msg = b"ok";
  let rc = unsafe { libc::write(libc::STDOUT_FILENO, msg.as_ptr().cast(), msg.len()) };
  if rc < 0 {
    return Err(format!(
      "write(STDOUT) failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(())
}

#[cfg(target_os = "macos")]
fn action_posix_shm_create(name: &str) -> Result<(), String> {
  use std::ffi::CString;

  if !name.starts_with('/') {
    return Err("POSIX shm name must start with '/'".to_string());
  }
  if name[1..].contains('/') {
    return Err("POSIX shm name must not contain '/' after the leading slash".to_string());
  }

  let name_c = CString::new(name).map_err(|_| "shm name contains NUL byte".to_string())?;

  let fd = unsafe {
    libc::shm_open(
      name_c.as_ptr(),
      libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
      0o600,
    )
  };
  if fd < 0 {
    return Err(format!(
      "shm_open({name}) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  unsafe {
    libc::close(fd);
    libc::shm_unlink(name_c.as_ptr());
  }

  Ok(())
}

#[cfg(target_os = "macos")]
fn action_unix_socket_connect(path: &str) -> Result<(), String> {
  use std::os::unix::net::UnixStream;

  UnixStream::connect(path).map_err(|e| format!("UnixStream::connect({path}) failed: {e}"))?;
  Ok(())
}

#[cfg(target_os = "macos")]
fn action_mach_lookup(service_name: &str) -> Result<(), String> {
  use std::ffi::CString;

  type MachPort = u32;
  type KernReturn = i32;

  extern "C" {
    static bootstrap_port: MachPort;
    fn bootstrap_look_up(
      bp: MachPort,
      service_name: *const std::os::raw::c_char,
      sp: *mut MachPort,
    ) -> KernReturn;
    fn mach_task_self() -> MachPort;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
  }

  let service_c =
    CString::new(service_name).map_err(|_| "service name contains NUL byte".to_string())?;

  let mut port: MachPort = 0;
  let kr = unsafe { bootstrap_look_up(bootstrap_port, service_c.as_ptr(), &mut port) };
  if kr != 0 {
    return Err(format!("bootstrap_look_up({service_name}) failed: {kr}"));
  }
  if port == 0 {
    return Err(format!(
      "bootstrap_look_up({service_name}) returned MACH_PORT_NULL"
    ));
  }

  // Best-effort cleanup.
  unsafe {
    let _ = mach_port_deallocate(mach_task_self(), port);
  }

  Ok(())
}
