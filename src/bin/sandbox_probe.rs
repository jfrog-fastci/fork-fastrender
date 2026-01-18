//! Linux renderer sandbox probe tool.
//!
//! This binary is intended to be a quick “is the sandbox working on this host?” diagnostic and an
//! iteration aid for the renderer sandbox policy (seccomp/landlock) without needing to run the full
//! multi-process browser stack.
//!
//! The probe also honours the renderer sandbox environment variables documented in
//! `docs/env-vars.md` (notably `FASTR_DISABLE_RENDERER_SANDBOX` and the `FASTR_RENDERER_*` layer
//! toggles) so developers can quickly disable specific layers when diagnosing issues.
//!
//! See docs:
//! - `docs/sandboxing.md` (overview)
//! - `docs/security/sandbox.md` (Linux-focused)

#[cfg(feature = "renderer_tools")]
fn main() {
  eprintln!(
    "sandbox_probe is disabled when built with the `renderer_tools` feature (sandbox support is gated off)."
  );
  eprintln!("Rebuild without `renderer_tools` to use this tool.");
  std::process::exit(2);
}

#[cfg(not(feature = "renderer_tools"))]
include!("_real/sandbox_probe.rs");
