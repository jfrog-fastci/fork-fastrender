#![cfg(windows)]

use win_sandbox::{AppContainerProfile, SandboxSupport};

const APPCONTAINER_NAME: &str = "FastRender.Renderer";
const APPCONTAINER_DISPLAY_NAME: &str = "FastRender Renderer";
const APPCONTAINER_DESCRIPTION: &str = "FastRender renderer AppContainer profile";

/// Returns `true` when the host appears able to run the full Windows renderer sandbox (AppContainer
/// + nested jobs).
///
/// Some Windows environments expose the AppContainer APIs but still block creating/using profiles
/// (for example, hardened CI images). In that case, sandbox tests that require AppContainer should
/// skip with a clear message rather than failing the entire suite.
pub(crate) fn require_full_windows_sandbox(test_name: &str) -> bool {
  let support = SandboxSupport::detect();
  if support != SandboxSupport::Full {
    eprintln!(
      "skipping {test_name}: Windows sandbox is unavailable ({support})"
    );
    return false;
  }

  match AppContainerProfile::ensure(
    APPCONTAINER_NAME,
    APPCONTAINER_DISPLAY_NAME,
    APPCONTAINER_DESCRIPTION,
  ) {
    Ok(profile) => {
      if !profile.is_enabled() {
        eprintln!("skipping {test_name}: AppContainer profile is disabled");
        return false;
      }
    }
    Err(err) => {
      eprintln!(
        "skipping {test_name}: AppContainer profile could not be ensured ({err})"
      );
      return false;
    }
  }

  true
}

