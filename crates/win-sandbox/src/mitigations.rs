use crate::Result;

#[cfg(windows)]
use crate::WinSandboxError;

#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, GetProcessMitigationPolicy, ProcessDynamicCodePolicy,
  ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy,
  ProcessSystemCallDisablePolicy, PROCESS_MITIGATION_POLICY,
};

// windows-sys does not currently expose the policy-specific structs, but `GetProcessMitigationPolicy`
// just expects a caller-provided buffer with the correct layout.
#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
struct PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
  Flags: u32,
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
struct PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
  Flags: u32,
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
struct PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY {
  Flags: u32,
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
struct PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
  Flags: u32,
}

#[cfg(windows)]
#[allow(non_snake_case)]
#[repr(C)]
struct PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY {
  Flags: u32,
}

// These constants are consumed by `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` (a `DWORD64` / `u64`
// bitmask) during `CreateProcessW` and come from the Windows SDK headers (`winbase.h`).
//
// `windows-sys` does not currently expose the `PROCESS_CREATION_MITIGATION_POLICY_*` macro values,
// so we define the ones we need here.
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_STRICT_HANDLE_CHECKS_ALWAYS_ON: u64 = 0x0000_4000;
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON: u64 = 0x0001_0000;
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_EXTENSION_POINT_DISABLE_ALWAYS_ON: u64 = 0x0004_0000;
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_PROHIBIT_DYNAMIC_CODE_ALWAYS_ON: u64 = 0x0010_0000;
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON: u64 = 0x1000_0000;
#[cfg(windows)]
const PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON: u64 = 0x4000_0000;

/// Helpers for building process creation mitigation policy bitmasks.
///
/// The bitmask is passed via `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` in `STARTUPINFOEX`.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default)]
struct MitigationPolicyBuilder {
  bits: u64,
}

#[cfg(windows)]
impl MitigationPolicyBuilder {
  const fn new() -> Self {
    Self { bits: 0 }
  }

  fn enable(&mut self, bits: u64) {
    self.bits |= bits;
  }

  const fn build(self) -> u64 {
    self.bits
  }
}

#[cfg(windows)]
fn get_mitigation_policy<T>(policy: PROCESS_MITIGATION_POLICY) -> Result<T> {
  // SAFETY: The caller ensures `T` is the correct struct for `policy`.
  let mut data: T = unsafe { std::mem::zeroed() };
  let ok = unsafe {
    GetProcessMitigationPolicy(
      GetCurrentProcess(),
      policy,
      &mut data as *mut T as *mut _,
      std::mem::size_of::<T>(),
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last("GetProcessMitigationPolicy"));
  }
  Ok(data)
}

#[cfg(windows)]
fn is_policy_supported<T>(policy: PROCESS_MITIGATION_POLICY) -> bool {
  let mut data: T = unsafe { std::mem::zeroed() };
  let ok = unsafe {
    GetProcessMitigationPolicy(
      GetCurrentProcess(),
      policy,
      &mut data as *mut T as *mut _,
      std::mem::size_of::<T>(),
    )
  };
  if ok != 0 {
    return true;
  }
  false
}

/// Returns a `u64` bitmask suitable for `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` for a *headless*
/// renderer process.
///
/// Mitigations included (when supported by the current OS):
/// - **Win32k lockdown**: disables `win32k.sys` system calls, blocking `user32`/`gdi32` attack
///   surface. This generally breaks GUI operations and should only be enabled for headless
///   processes.
/// - **Prohibit dynamic code**: disallows JIT/codegen and some forms of injection. Low-risk for
///   typical Rust binaries that do not generate executable memory.
/// - **Disable extension points**: blocks legacy global injection mechanisms like AppInit DLLs and
///   Win32 hooks.
/// - **Image load hardening**: prevents loading DLLs from remote/low-integrity locations.
/// - **Strict handle checks**: raises exceptions on invalid handle usage.
///
/// Escape hatch:
/// - Set `FASTR_DISABLE_WIN_MITIGATIONS=1` to disable applying these mitigation policies during
///   process spawn (but *not* other sandboxing layers like AppContainer / job objects).
pub fn renderer_mitigation_policy() -> u64 {
  // Escape hatch: allow users to disable *mitigation policies* (but not the higher-level sandbox
  // primitives like job objects / AppContainer) if a particular Windows build breaks.
  if std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_some() {
    return 0;
  }

  #[cfg(not(windows))]
  {
    0
  }

  #[cfg(windows)]
  {
    let mut builder = MitigationPolicyBuilder::new();

    // Win32k lockdown (headless-only).
    if is_policy_supported::<PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY>(
      ProcessSystemCallDisablePolicy,
    ) {
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON);
    }

    // Arbitrary Code Guard (ACG): ban dynamic/JIT code.
    if is_policy_supported::<PROCESS_MITIGATION_DYNAMIC_CODE_POLICY>(ProcessDynamicCodePolicy) {
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_PROHIBIT_DYNAMIC_CODE_ALWAYS_ON);
    }

    // Disable extension point injection mechanisms (AppInit DLLs, hooks).
    if is_policy_supported::<PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY>(
      ProcessExtensionPointDisablePolicy,
    ) {
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_EXTENSION_POINT_DISABLE_ALWAYS_ON);
    }

    // Harden DLL/image note: these only affect image loads (DLLs), not file IO in general.
    if is_policy_supported::<PROCESS_MITIGATION_IMAGE_LOAD_POLICY>(ProcessImageLoadPolicy) {
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON);
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON);
    }

    // Raise exceptions on invalid handles.
    if is_policy_supported::<PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY>(
      ProcessStrictHandleCheckPolicy,
    ) {
      builder.enable(PROCESS_CREATION_MITIGATION_POLICY_STRICT_HANDLE_CHECKS_ALWAYS_ON);
    }

    builder.build()
  }
}

/// Verifies the current process has the expected renderer mitigation policies enabled.
///
/// This is intended for Windows-only tests; it uses `GetProcessMitigationPolicy` to check runtime
/// state.
pub fn verify_renderer_mitigations_current_process() -> Result<()> {
  #[cfg(not(windows))]
  {
    Ok(())
  }

  #[cfg(windows)]
  {
    const DYNAMIC_CODE_PROHIBIT: u32 = 0x1;
    const EXTENSION_POINT_DISABLE: u32 = 0x1;
    const SYSTEM_CALL_DISABLE_WIN32K: u32 = 0x1;
    const IMAGE_LOAD_NO_REMOTE: u32 = 0x1;
    const IMAGE_LOAD_NO_LOW_LABEL: u32 = 0x2;
    const STRICT_HANDLE_RAISE_EXCEPTION: u32 = 0x1;

    let expected = renderer_mitigation_policy();

    if expected & PROCESS_CREATION_MITIGATION_POLICY_PROHIBIT_DYNAMIC_CODE_ALWAYS_ON != 0 {
      let policy: PROCESS_MITIGATION_DYNAMIC_CODE_POLICY =
        get_mitigation_policy(ProcessDynamicCodePolicy)?;
      if (policy.Flags & DYNAMIC_CODE_PROHIBIT) == 0 {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message: "PROCESS_MITIGATION_DYNAMIC_CODE_POLICY.ProhibitDynamicCode not enabled"
            .to_string(),
        });
      }
    }

    if expected & PROCESS_CREATION_MITIGATION_POLICY_EXTENSION_POINT_DISABLE_ALWAYS_ON != 0 {
      let policy: PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY =
        get_mitigation_policy(ProcessExtensionPointDisablePolicy)?;
      if (policy.Flags & EXTENSION_POINT_DISABLE) == 0 {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message:
            "PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY.DisableExtensionPoints not enabled"
              .to_string(),
        });
      }
    }

    if expected & PROCESS_CREATION_MITIGATION_POLICY_WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON != 0 {
      let policy: PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY =
        get_mitigation_policy(ProcessSystemCallDisablePolicy)?;
      if (policy.Flags & SYSTEM_CALL_DISABLE_WIN32K) == 0 {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message:
            "PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY.DisallowWin32kSystemCalls not enabled"
              .to_string(),
        });
      }
    }

    if expected
      & (PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON
        | PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON)
      != 0
    {
      let policy: PROCESS_MITIGATION_IMAGE_LOAD_POLICY =
        get_mitigation_policy(ProcessImageLoadPolicy)?;
      if expected & PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON != 0
        && (policy.Flags & IMAGE_LOAD_NO_REMOTE) == 0
      {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message: "PROCESS_MITIGATION_IMAGE_LOAD_POLICY.NoRemoteImages not enabled".to_string(),
        });
      }
      if expected & PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON != 0
        && (policy.Flags & IMAGE_LOAD_NO_LOW_LABEL) == 0
      {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message: "PROCESS_MITIGATION_IMAGE_LOAD_POLICY.NoLowMandatoryLabelImages not enabled"
            .to_string(),
        });
      }
    }

    if expected & PROCESS_CREATION_MITIGATION_POLICY_STRICT_HANDLE_CHECKS_ALWAYS_ON != 0 {
      let policy: PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY =
        get_mitigation_policy(ProcessStrictHandleCheckPolicy)?;
      if (policy.Flags & STRICT_HANDLE_RAISE_EXCEPTION) == 0 {
        return Err(WinSandboxError::MitigationVerificationFailed {
          message: "PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY.RaiseExceptionOnInvalidHandleReference not enabled".to_string(),
        });
      }
    }

    Ok(())
  }
}
