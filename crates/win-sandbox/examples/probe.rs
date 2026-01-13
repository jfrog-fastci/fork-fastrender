//! Windows sandbox probe tool.
//!
//! This example is intended as a lightweight debugging / repro utility when the Windows renderer
//! sandbox regresses. It spawns *itself* under the production sandbox launcher and prints observed
//! sandbox state from inside the child process.
//!
//! Usage:
//!
//! ```text
//! cargo run -p win-sandbox --example probe -- [--read <PATH>] [--connect <IP:PORT>] [--connect-localhost]
//! ```
//!
//! Notes:
//! - `--connect-localhost` binds an ephemeral port on `127.0.0.1` in the parent and asks the child
//!   to connect to it. Under a no-capabilities AppContainer this should fail with `WSAEACCES`
//!   (10013), providing a deterministic "network is blocked" signal without requiring internet
//!   access.

#[cfg(not(windows))]
fn main() {
  eprintln!("win-sandbox example `probe` is only supported on Windows.");
  std::process::exit(2);
}

#[cfg(windows)]
fn main() {
  windows::main();
}

#[cfg(windows)]
mod windows {
  use std::ffi::c_void;
  use std::ffi::{OsStr, OsString};
  use std::io;
  use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
  use std::os::windows::io::{AsRawHandle, RawHandle};
  use std::path::PathBuf;
  use std::time::Duration;

  use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, BOOL, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
  };
  use windows_sys::Win32::Security::{
    ConvertSidToStringSidW, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    OpenProcessToken, TokenIntegrityLevel, TokenIsAppContainer, PSID, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY,
  };
  use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
  };
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessMitigationPolicy, TerminateProcess,
    WaitForSingleObject, ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy,
    ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy, ProcessSystemCallDisablePolicy,
    PROCESS_MITIGATION_POLICY,
  };

  use fastrender::sandbox::windows::spawn_sandboxed;

  // WaitForSingleObject return codes.
  const WAIT_OBJECT_0: u32 = 0;
  const WAIT_TIMEOUT: u32 = 0x0000_0102;

  // We keep the process creation probe simple: spawn the child, wait for it, and propagate its
  // exit code.
  const DEFAULT_TIMEOUT_MS: u32 = 30_000;

  #[derive(Debug, Default)]
  struct Args {
    child: bool,
    read_path: Option<PathBuf>,
    connect: Option<OsString>,
    connect_localhost: bool,
    timeout_ms: u32,
  }

  pub(super) fn main() {
    let args = match parse_args() {
      Ok(args) => args,
      Err(err) => {
        eprintln!("{err}\n");
        print_usage();
        std::process::exit(2);
      }
    };

    let exit_code = if args.child {
      run_child(&args)
    } else {
      run_parent(&args)
    };
    std::process::exit(exit_code);
  }

  fn parse_args() -> Result<Args, String> {
    let mut out = Args {
      timeout_ms: DEFAULT_TIMEOUT_MS,
      ..Args::default()
    };

    let mut iter = std::env::args_os().skip(1);
    while let Some(arg) = iter.next() {
      if arg == OsStr::new("--child") {
        out.child = true;
      } else if arg == OsStr::new("--read") {
        let Some(value) = iter.next() else {
          return Err("missing value for --read".to_string());
        };
        out.read_path = Some(PathBuf::from(value));
      } else if arg == OsStr::new("--connect") {
        let Some(value) = iter.next() else {
          return Err("missing value for --connect".to_string());
        };
        out.connect = Some(value);
      } else if arg == OsStr::new("--connect-localhost") {
        out.connect_localhost = true;
      } else if arg == OsStr::new("--timeout-ms") {
        let Some(value) = iter.next() else {
          return Err("missing value for --timeout-ms".to_string());
        };
        let value = value.to_string_lossy();
        out.timeout_ms = value
          .trim()
          .parse::<u32>()
          .map_err(|_| format!("invalid --timeout-ms value: {value}"))?;
      } else if arg == OsStr::new("--help") || arg == OsStr::new("-h") {
        print_usage();
        std::process::exit(0);
      } else {
        return Err(format!("unrecognized argument: {}", arg.to_string_lossy()));
      }
    }

    if out.connect_localhost && out.connect.is_some() {
      return Err("cannot combine --connect-localhost with --connect".to_string());
    }

    Ok(out)
  }

  fn print_usage() {
    eprintln!(
      "Usage:\n  cargo run -p win-sandbox --example probe -- [--read <PATH>] [--connect <IP:PORT>] [--connect-localhost] [--timeout-ms <MS>]\n\n\
Parent mode (default) spawns a sandboxed child.\nChild mode (--child) prints sandbox state and runs probes.\n"
    );
  }

  fn run_parent(args: &Args) -> i32 {
    println!("== win-sandbox probe (parent) ==");
    println!("pid: {}", std::process::id());

    if let Some(value) = std::env::var_os("FASTR_DISABLE_RENDERER_SANDBOX") {
      println!(
        "env: FASTR_DISABLE_RENDERER_SANDBOX={} (sandbox may be disabled)",
        value.to_string_lossy()
      );
    }
    if let Some(value) = std::env::var_os("FASTR_WINDOWS_RENDERER_SANDBOX") {
      println!(
        "env: FASTR_WINDOWS_RENDERER_SANDBOX={} (sandbox may be disabled)",
        value.to_string_lossy()
      );
    }

    let exe = match std::env::current_exe() {
      Ok(exe) => exe,
      Err(err) => {
        eprintln!("error: current_exe failed: {err}");
        return 2;
      }
    };

    let mut child_args: Vec<OsString> = Vec::new();
    child_args.push(OsString::from("--child"));

    if let Some(path) = args.read_path.as_ref() {
      child_args.push(OsString::from("--read"));
      child_args.push(path.as_os_str().to_owned());
    }

    let listener = if args.connect_localhost {
      match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => {
          let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
          println!("parent: bound localhost listener at 127.0.0.1:{port}");
          child_args.push(OsString::from("--connect"));
          child_args.push(OsString::from(format!("127.0.0.1:{port}")));
          Some(listener)
        }
        Err(err) => {
          eprintln!("parent: failed to bind localhost listener: {err}");
          None
        }
      }
    } else {
      None
    };

    if let Some(connect) = args.connect.as_ref() {
      child_args.push(OsString::from("--connect"));
      child_args.push(connect.clone());
    }

    let inherit = collect_stdio_handles_for_inheritance();
    println!(
      "parent: spawning child (inherit_handles={}, timeout_ms={})",
      inherit.len(),
      args.timeout_ms
    );

    let child = match spawn_sandboxed(&exe, &child_args, &inherit) {
      Ok(child) => child,
      Err(err) => {
        eprintln!("error: spawn_sandboxed failed: {err}");
        return 2;
      }
    };

    println!(
      "parent: spawned child pid={} level={:?} job_assigned={}",
      child.pid,
      child.level,
      child.job.is_some()
    );

    // Keep any listener alive until after the child returns from its connect probe.
    let _listener = listener;

    let handle = child.process.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let exit_code = match wait_process(handle, args.timeout_ms) {
      Ok(code) => code,
      Err(err) => {
        eprintln!("error: failed waiting for child: {err}");
        return 2;
      }
    };
    println!("parent: child exit_code={exit_code}");
    exit_code as i32
  }

  fn run_child(args: &Args) -> i32 {
    println!("== win-sandbox probe (child) ==");
    println!("pid: {}", std::process::id());

    let token = match open_process_token_query() {
      Ok(token) => token,
      Err(err) => {
        eprintln!("token: OpenProcessToken failed: {err}");
        return 2;
      }
    };

    match token_is_appcontainer(token.0) {
      Ok(is_ac) => println!("TokenIsAppContainer: {is_ac}"),
      Err(err) => eprintln!("TokenIsAppContainer: error: {err}"),
    }

    match token_integrity_level(token.0) {
      Ok(il) => println!("IntegrityLevel: {} (rid=0x{:X}, sid={})", il.name, il.rid, il.sid),
      Err(err) => eprintln!("IntegrityLevel: error: {err}"),
    }

    match current_process_in_job() {
      Ok(in_job) => println!("IsProcessInJob: {in_job}"),
      Err(err) => eprintln!("IsProcessInJob: error: {err}"),
    }

    println!();
    println!("== Process mitigations (GetProcessMitigationPolicy) ==");
    print_mitigations();

    println!();
    println!("== Probes ==");

    if let Some(path) = args.read_path.as_ref() {
      probe_read(path);
    } else {
      println!("fs: read skipped (pass --read <PATH>)");
    }

    if let Some(addr) = args.connect.as_ref() {
      probe_connect(addr);
    } else {
      println!("net: connect skipped (pass --connect <IP:PORT> or --connect-localhost)");
    }

    0
  }

  // -----------------------------------------------------------------------------
  // Parent helpers
  // -----------------------------------------------------------------------------

  fn collect_stdio_handles_for_inheritance() -> Vec<RawHandle> {
    let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let mut inherit = Vec::new();
    for h in [std_in, std_out, std_err] {
      if h == 0 || h == INVALID_HANDLE_VALUE {
        continue;
      }
      // Ensure the handle is inheritable so the sandbox spawner can forward it via
      // PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
      let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
      inherit.push(h as RawHandle);
    }
    inherit
  }

  fn wait_process(handle: windows_sys::Win32::Foundation::HANDLE, timeout_ms: u32) -> io::Result<u32> {
    let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if wait == WAIT_TIMEOUT {
      unsafe {
        let _ = TerminateProcess(handle, 1);
      }
      return Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("child timed out after {timeout_ms}ms (terminated)"),
      ));
    }
    if wait != WAIT_OBJECT_0 {
      return Err(io::Error::last_os_error());
    }

    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(exit_code)
  }

  // -----------------------------------------------------------------------------
  // Token / job queries
  // -----------------------------------------------------------------------------

  struct HandleGuard(windows_sys::Win32::Foundation::HANDLE);

  impl Drop for HandleGuard {
    fn drop(&mut self) {
      unsafe {
        if self.0 != 0 {
          CloseHandle(self.0);
        }
      }
    }
  }

  fn open_process_token_query() -> io::Result<HandleGuard> {
    let mut token: windows_sys::Win32::Foundation::HANDLE = 0;
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    if token == 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "OpenProcessToken returned null handle",
      ));
    }
    Ok(HandleGuard(token))
  }

  fn token_is_appcontainer(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<bool> {
    let mut is_appcontainer: u32 = 0;
    let mut returned: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIsAppContainer,
        (&mut is_appcontainer as *mut u32).cast::<c_void>(),
        std::mem::size_of::<u32>() as u32,
        &mut returned,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(is_appcontainer != 0)
  }

  #[derive(Debug)]
  struct IntegrityLevel {
    name: &'static str,
    rid: u32,
    sid: String,
  }

  fn token_integrity_level(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<IntegrityLevel> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIntegrityLevel,
        std::ptr::null_mut(),
        0,
        &mut needed,
      )
    };
    if ok != 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "GetTokenInformation(TokenIntegrityLevel) unexpectedly succeeded with null buffer",
      ));
    }
    if needed == 0 {
      return Err(io::Error::last_os_error());
    }

    // Ensure pointer alignment.
    let word_count = (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer_words = vec![0usize; word_count];
    let buffer_ptr = buffer_words.as_mut_ptr().cast::<c_void>();

    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIntegrityLevel,
        buffer_ptr,
        needed,
        &mut needed,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }

    let tml = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
    let sid = unsafe { (*tml).Label.Sid as PSID };
    if sid.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "TokenIntegrityLevel returned null SID",
      ));
    }

    let rid = unsafe { integrity_rid_from_sid(sid)? };
    let name = integrity_level_name(rid);
    let sid_str = unsafe { sid_to_string(sid)? };

    Ok(IntegrityLevel {
      name,
      rid,
      sid: sid_str,
    })
  }

  unsafe fn integrity_rid_from_sid(sid: PSID) -> io::Result<u32> {
    let count_ptr = GetSidSubAuthorityCount(sid);
    if count_ptr.is_null() {
      return Err(io::Error::last_os_error());
    }
    let count = *count_ptr as u32;
    if count == 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "SID has no subauthorities",
      ));
    }
    let rid_ptr = GetSidSubAuthority(sid, count - 1);
    if rid_ptr.is_null() {
      return Err(io::Error::last_os_error());
    }
    Ok(*rid_ptr)
  }

  fn integrity_level_name(rid: u32) -> &'static str {
    // Windows integrity levels are encoded in the RID of the mandatory label SID (S-1-16-...).
    match rid {
      0x0000 => "Untrusted",
      0x1000 => "Low",
      0x2000 => "Medium",
      0x3000 => "High",
      0x4000 => "System",
      0x5000 => "ProtectedProcess",
      _ => {
        if rid < 0x1000 {
          "Untrusted(?)"
        } else if rid < 0x2000 {
          "Low(?)"
        } else if rid < 0x3000 {
          "Medium(?)"
        } else if rid < 0x4000 {
          "High(?)"
        } else if rid < 0x5000 {
          "System(?)"
        } else {
          "Unknown"
        }
      }
    }
  }

  unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut sid_str: *mut u16 = std::ptr::null_mut();
    let ok = ConvertSidToStringSidW(sid, &mut sid_str);
    if ok == 0 || sid_str.is_null() {
      return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    while *sid_str.add(len) != 0 {
      len += 1;
    }
    let wide = std::slice::from_raw_parts(sid_str, len);
    let out = String::from_utf16_lossy(wide);
    windows_sys::Win32::Foundation::LocalFree(sid_str as isize);
    Ok(out)
  }

  fn current_process_in_job() -> io::Result<bool> {
    let mut in_job: BOOL = 0;
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), 0, &mut in_job) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(in_job != 0)
  }

  // -----------------------------------------------------------------------------
  // Mitigation printing
  // -----------------------------------------------------------------------------

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY {
    Flags: u32,
  }

  fn get_mitigation_policy<T>(policy: PROCESS_MITIGATION_POLICY) -> io::Result<T> {
    let mut data: T = unsafe { std::mem::zeroed() };
    let ok = unsafe {
      GetProcessMitigationPolicy(
        GetCurrentProcess(),
        policy,
        (&mut data as *mut T).cast::<c_void>(),
        std::mem::size_of::<T>(),
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(data)
  }

  fn print_mitigations() {
    // Report the mitigations we care about for the renderer sandbox.
    print_policy::<_, PROCESS_MITIGATION_DYNAMIC_CODE_POLICY>(
      "ProcessDynamicCodePolicy",
      ProcessDynamicCodePolicy,
      |flags| {
        let prohibit = flags & 0x1 != 0;
        format!("prohibit_dynamic_code={prohibit}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY>(
      "ProcessExtensionPointDisablePolicy",
      ProcessExtensionPointDisablePolicy,
      |flags| {
        let disabled = flags & 0x1 != 0;
        format!("disable_extension_points={disabled}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY>(
      "ProcessSystemCallDisablePolicy",
      ProcessSystemCallDisablePolicy,
      |flags| {
        let win32k = flags & 0x1 != 0;
        format!("disallow_win32k_system_calls={win32k}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_IMAGE_LOAD_POLICY>(
      "ProcessImageLoadPolicy",
      ProcessImageLoadPolicy,
      |flags| {
        let no_remote = flags & 0x1 != 0;
        let no_low = flags & 0x2 != 0;
        let prefer_system32 = flags & 0x4 != 0;
        format!(
          "no_remote_images={no_remote} no_low_mandatory_label_images={no_low} prefer_system32_images={prefer_system32}"
        )
      },
    );
    print_policy::<_, PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY>(
      "ProcessStrictHandleCheckPolicy",
      ProcessStrictHandleCheckPolicy,
      |flags| {
        let raise = flags & 0x1 != 0;
        format!("raise_exception_on_invalid_handle_reference={raise}")
      },
    );
  }

  fn print_policy<F, T>(name: &str, policy: PROCESS_MITIGATION_POLICY, describe: F)
  where
    F: FnOnce(u32) -> String,
    T: PolicyFlags,
  {
    match get_mitigation_policy::<T>(policy) {
      Ok(p) => println!("{name}: flags=0x{:08X} {}", p.flags(), describe(p.flags())),
      Err(err) => println!("{name}: unavailable ({err})"),
    }
  }

  trait PolicyFlags {
    fn flags(&self) -> u32;
  }

  impl PolicyFlags for PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }

  // -----------------------------------------------------------------------------
  // Probes
  // -----------------------------------------------------------------------------

  fn probe_read(path: &PathBuf) {
    match std::fs::read(path) {
      Ok(bytes) => println!(
        "fs: read {} bytes from {} (SUCCESS)",
        bytes.len(),
        path.display()
      ),
      Err(err) => {
        println!(
          "fs: read {} (FAILED): {} (raw_os_error={:?})",
          path.display(),
          err,
          err.raw_os_error()
        );
      }
    }
  }

  fn probe_connect(raw: &OsString) {
    let raw_str = raw.to_string_lossy();
    let addr: SocketAddr = match raw_str.parse() {
      Ok(addr) => addr,
      Err(_) => {
        println!("net: connect {raw_str} (SKIPPED): expected IP:PORT");
        return;
      }
    };
    let timeout = Duration::from_millis(500);
    match TcpStream::connect_timeout(&addr, timeout) {
      Ok(_stream) => println!("net: connect {addr} (SUCCESS)"),
      Err(err) => println!(
        "net: connect {addr} (FAILED): {} (raw_os_error={:?})",
        err,
        err.raw_os_error()
      ),
    }
  }
}
