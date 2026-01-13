//! Windows sandbox compatibility smoke test: ensure a sandboxed child (preferably AppContainer)
//! can initialize FastRender and render a minimal HTML page.
//!
//! Motivation: AppContainer can restrict filesystem access to system fonts (e.g. `C:\Windows\Fonts`)
//! which FastRender relies on for text shaping. This test gives an early signal when the sandbox
//! policy makes the renderer unusable.
//!
//! Implementation notes:
//! - Uses the production Windows sandbox launcher (`fastrender::sandbox::windows::spawn_sandboxed`)
//!   so we validate the same code path used by future multiprocess renderer spawning.
//! - The child test is marked `#[ignore]` and is executed via `--ignored --exact ...` inside the
//!   sandboxed process.

#![cfg(windows)]

use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use win_sandbox::mitigations;
use windows_sys::Win32::Foundation::{
  CloseHandle, GetHandleInformation, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER, HANDLE,
  HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
  GetTokenInformation, OpenProcessToken, TokenCapabilities, TokenIntegrityLevel,
  TokenIsAppContainer, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Memory::LocalFree;
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

// WaitForSingleObject return codes.
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

/// The well-known capability SID for `internetClient`.
///
/// See: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/security-identifiers#capability-sids
const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";

const CHILD_TIMEOUT_MS: u32 = 60_000;

fn format_error_chain(err: &(dyn Error)) -> String {
  let mut out = String::new();
  out.push_str(&format!("{err}"));
  let mut source = err.source();
  while let Some(src) = source {
    out.push_str(&format!("\n  caused by: {src}"));
    source = src.source();
  }
  out
}

#[derive(Debug)]
struct TokenState {
  is_app_container: bool,
  integrity_sid: String,
  integrity_rid: u32,
  capability_sids: Vec<String>,
}

impl TokenState {
  fn is_low_or_untrusted_integrity(&self) -> bool {
    matches!(self.integrity_rid, 0 | 4096)
  }

  fn has_internet_client_capability(&self) -> bool {
    self
      .capability_sids
      .iter()
      .any(|sid| sid.eq_ignore_ascii_case(INTERNET_CLIENT_CAPABILITY_SID))
  }
}

fn query_current_process_token_state() -> Result<TokenState, String> {
  let mut token: HANDLE = 0;
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  if ok == 0 {
    return Err(format!(
      "OpenProcessToken(GetCurrentProcess, TOKEN_QUERY) failed: {}",
      io::Error::last_os_error()
    ));
  }
  if token == 0 {
    return Err("OpenProcessToken returned null token handle".to_string());
  }

  struct TokenHandle(HANDLE);
  impl Drop for TokenHandle {
    fn drop(&mut self) {
      unsafe {
        let _ = CloseHandle(self.0);
      }
    }
  }
  let token = TokenHandle(token);

  let is_app_container = query_token_is_app_container(token.0)?;
  let (integrity_sid, integrity_rid) = query_token_integrity_level(token.0)?;
  let capability_sids = if is_app_container {
    query_token_capabilities(token.0)?
  } else {
    Vec::new()
  };

  Ok(TokenState {
    is_app_container,
    integrity_sid,
    integrity_rid,
    capability_sids,
  })
}

fn query_token_is_app_container(token: HANDLE) -> Result<bool, String> {
  let mut value: u32 = 0;
  let mut returned: u32 = 0;
  let ok = unsafe {
    GetTokenInformation(
      token,
      TokenIsAppContainer as TOKEN_INFORMATION_CLASS,
      std::ptr::addr_of_mut!(value).cast(),
      std::mem::size_of::<u32>() as u32,
      std::ptr::addr_of_mut!(returned),
    )
  };
  if ok == 0 {
    return Err(format!(
      "GetTokenInformation(TokenIsAppContainer) failed: {}",
      io::Error::last_os_error()
    ));
  }
  Ok(value != 0)
}

fn query_token_integrity_level(token: HANDLE) -> Result<(String, u32), String> {
  let buf = get_token_information(token, TokenIntegrityLevel as TOKEN_INFORMATION_CLASS)?;
  if buf.len() < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
    return Err(format!(
      "TokenIntegrityLevel buffer too small ({} bytes)",
      buf.len()
    ));
  }

  // SAFETY: buffer is large enough to contain TOKEN_MANDATORY_LABEL.
  let label = unsafe { &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
  let sid = label.Label.Sid;
  if sid.is_null() {
    return Err("TokenIntegrityLevel returned null SID".to_string());
  }
  let sid_string = sid_to_string(sid)?;
  let rid = sid_string
    .rsplit('-')
    .next()
    .and_then(|tail| tail.parse::<u32>().ok())
    .ok_or_else(|| format!("unexpected integrity SID format: {sid_string}"))?;
  Ok((sid_string, rid))
}

fn query_token_capabilities(token: HANDLE) -> Result<Vec<String>, String> {
  let buf = get_token_information(token, TokenCapabilities as TOKEN_INFORMATION_CLASS)?;
  if buf.is_empty() {
    return Ok(Vec::new());
  }
  if buf.len() < std::mem::size_of::<TOKEN_GROUPS>() {
    return Err(format!(
      "TokenCapabilities buffer too small ({} bytes)",
      buf.len()
    ));
  }

  // SAFETY: buffer is large enough for TOKEN_GROUPS header.
  let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
  let count = groups.GroupCount as usize;

  // TOKEN_GROUPS is variable length. windows-sys models it with a 1-element array; walk with
  // pointer arithmetic.
  let first = groups.Groups.as_ptr();
  let mut out = Vec::new();
  for idx in 0..count {
    // SAFETY: buffer returned by GetTokenInformation is sized for `count` entries.
    let entry = unsafe { &*first.add(idx) };
    if entry.Sid.is_null() {
      continue;
    }
    out.push(sid_to_string(entry.Sid)?);
  }
  Ok(out)
}

fn get_token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>, String> {
  let mut needed: u32 = 0;
  let ok = unsafe {
    GetTokenInformation(
      token,
      class,
      std::ptr::null_mut(),
      0,
      std::ptr::addr_of_mut!(needed),
    )
  };
  if ok != 0 {
    // Unexpected but possible for fixed-size info classes.
    return Ok(Vec::new());
  }

  let err = io::Error::last_os_error();
  if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
    return Err(format!(
      "GetTokenInformation(size query) failed: {err} (raw_os_error={:?})",
      err.raw_os_error()
    ));
  }
  if needed == 0 {
    return Err(
      "GetTokenInformation returned ERROR_INSUFFICIENT_BUFFER but length was 0".to_string(),
    );
  }

  let mut buf = vec![0u8; needed as usize];
  let ok = unsafe {
    GetTokenInformation(
      token,
      class,
      buf.as_mut_ptr().cast(),
      needed,
      std::ptr::addr_of_mut!(needed),
    )
  };
  if ok == 0 {
    return Err(format!(
      "GetTokenInformation(data) failed: {}",
      io::Error::last_os_error()
    ));
  }
  buf.truncate(needed as usize);
  Ok(buf)
}

fn sid_to_string(sid: *mut std::ffi::c_void) -> Result<String, String> {
  let mut wide: *mut u16 = std::ptr::null_mut();
  let ok = unsafe { ConvertSidToStringSidW(sid, std::ptr::addr_of_mut!(wide)) };
  if ok == 0 {
    return Err(format!(
      "ConvertSidToStringSidW failed: {}",
      io::Error::last_os_error()
    ));
  }
  if wide.is_null() {
    return Err("ConvertSidToStringSidW succeeded but returned null pointer".to_string());
  }

  // SAFETY: pointer is NUL-terminated per Win32 contract.
  let mut len = 0usize;
  unsafe {
    while *wide.add(len) != 0 {
      len += 1;
    }
    let slice = std::slice::from_raw_parts(wide, len);
    let s = String::from_utf16_lossy(slice);
    LocalFree(wide as _);
    Ok(s)
  }
}

fn wait_process(handle: HANDLE, timeout_ms: u32) -> Result<u32, String> {
  let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
  if wait == WAIT_TIMEOUT {
    unsafe {
      // Best-effort kill so the test process doesn't hang indefinitely.
      TerminateProcess(handle, 1);
    }
    return Err(format!(
      "child process timed out after {timeout_ms}ms (terminated)"
    ));
  }
  if wait != WAIT_OBJECT_0 {
    return Err(format!(
      "WaitForSingleObject failed (code={wait}): {}",
      std::io::Error::last_os_error()
    ));
  }

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  if ok == 0 {
    return Err(format!(
      "GetExitCodeProcess failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  Ok(exit_code)
}

struct HandleInheritGuard {
  saved: Vec<(HANDLE, u32)>,
}

impl HandleInheritGuard {
  fn new(handles: &[HANDLE]) -> Self {
    let mut saved = Vec::with_capacity(handles.len());
    for handle in handles {
      if *handle == 0 || *handle == INVALID_HANDLE_VALUE {
        continue;
      }
      let mut flags: u32 = 0;
      let ok = unsafe { GetHandleInformation(*handle, &mut flags) };
      if ok == 0 {
        continue;
      }
      saved.push((*handle, flags));
      let _ = unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    Self { saved }
  }
}

impl Drop for HandleInheritGuard {
  fn drop(&mut self) {
    for (handle, flags) in self.saved.drain(..) {
      let inherit = if (flags & HANDLE_FLAG_INHERIT) != 0 {
        HANDLE_FLAG_INHERIT
      } else {
        0
      };
      let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit) };
    }
  }
}

fn collect_stdio_handles_for_inheritance() -> (Vec<RawHandle>, HandleInheritGuard) {
  // Limit handle inheritance to standard handles so we don't leak privileged handles into the
  // sandboxed process.
  let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
  let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
  let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

  let mut handles: Vec<HANDLE> = Vec::new();
  for h in [std_in, std_out, std_err] {
    if h == 0 || h == INVALID_HANDLE_VALUE {
      continue;
    }
    if !handles.contains(&h) {
      handles.push(h);
    }
  }

  let guard = HandleInheritGuard::new(&handles);
  let inherit = handles.iter().copied().map(|h| h as RawHandle).collect();
  (inherit, guard)
}

#[test]
fn appcontainer_renderer_can_render_minimal_html() {
  // Ensure developer environment overrides don't silently change test semantics.
  let _env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_WINDOWS_RENDERER_SANDBOX",
    "FASTR_ALLOW_UNSANDBOXED_RENDERER",
    "FASTR_DISABLE_WIN_MITIGATIONS",
    "FASTR_WINDOWS_SANDBOX_INHERIT_ENV",
  ]);

  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "appcontainer_renderer_can_render_minimal_html",
  ) {
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox::windows_renderer_smoke::appcontainer_renderer_smoke_child";

  let args = vec![
    OsString::from("--ignored"),
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let (inherit, _inherit_guard) = collect_stdio_handles_for_inheritance();

  let child = spawn_sandboxed(&exe, &args, &inherit).expect("spawn sandboxed child");
  let handle = child.process.as_raw_handle() as HANDLE;
  let exit_code = wait_process(handle, CHILD_TIMEOUT_MS).expect("wait for sandboxed child");

  assert_eq!(
    exit_code, 0,
    "sandboxed child exited with code {exit_code} (sandbox_level={:?})",
    child.level
  );
  assert_eq!(
    child.level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandboxing (no silent fallback)"
  );
}

#[test]
#[ignore]
fn appcontainer_renderer_smoke_child() {
  match query_current_process_token_state() {
    Ok(state) => {
      eprintln!("sandbox: token_state={state:?}");
      if state.is_app_container {
        assert!(
          !state.has_internet_client_capability(),
          "expected AppContainer token to NOT have internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}); token_state={state:?}"
        );
      } else {
        panic!(
          "expected sandbox child to run in an AppContainer token (no silent fallback); token_state={state:?}"
        );
      }
    }
    Err(err) => {
      panic!("failed to query sandbox token state in child: {err}");
    }
  }

  mitigations::verify_renderer_mitigations_current_process()
    .expect("expected renderer mitigations to be active in sandboxed child");

  if let Err(err) = renderer_smoke_child_inner() {
    let chain = format_error_chain(&err);
    panic!("Sandboxed child failed to initialize FastRender and render minimal HTML:\n{chain}");
  }
}

fn renderer_smoke_child_inner() -> fastrender::Result<()> {
  use fastrender::image_output::{encode_image, OutputFormat};
  use fastrender::FastRender;

  let mut renderer = FastRender::new()?;
  let pixmap = renderer.render_html("<!doctype html><p>Hello</p>", 256, 128)?;
  assert_eq!(pixmap.width(), 256);
  assert_eq!(pixmap.height(), 128);

  let png = encode_image(&pixmap, OutputFormat::Png)?;
  assert!(
    png.starts_with(b"\x89PNG\r\n\x1a\n"),
    "expected PNG signature, got {:?}",
    png.get(0..8)
  );
  Ok(())
}
