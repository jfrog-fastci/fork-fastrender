/// Pointer input types shared across the renderer, interaction engine, and optional UI stacks.
///
/// These live at the crate root (rather than under `ui`) so renderer-only builds (`--no-default-features`)
/// can still compile interaction code without pulling in the full UI/runtime stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
  None,
  Primary,
  Secondary,
  Middle,
  Back,
  Forward,
  Other(u16),
}

/// Snapshot of modifier keys/buttons active during a pointer event.
///
/// This is part of the UI↔worker protocol, so it must remain small, `Copy`, and independent of any
/// specific windowing backend types (e.g. winit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PointerModifiers(u8);

impl PointerModifiers {
  pub const NONE: Self = Self(0);
  pub const CTRL: Self = Self(1 << 0);
  pub const SHIFT: Self = Self(1 << 1);
  pub const ALT: Self = Self(1 << 2);
  pub const META: Self = Self(1 << 3);

  /// Cross-platform "command" modifier (Cmd on macOS, Ctrl elsewhere).
  ///
  /// This is useful for browser-style gestures like Cmd/Ctrl-click to open links in a new tab.
  #[must_use]
  pub fn command(self) -> bool {
    if cfg!(target_os = "macos") {
      self.meta()
    } else {
      self.ctrl()
    }
  }

  #[must_use]
  pub fn ctrl(self) -> bool {
    (self.0 & Self::CTRL.0) != 0
  }

  #[must_use]
  pub fn shift(self) -> bool {
    (self.0 & Self::SHIFT.0) != 0
  }

  #[must_use]
  pub fn alt(self) -> bool {
    (self.0 & Self::ALT.0) != 0
  }

  #[must_use]
  pub fn meta(self) -> bool {
    (self.0 & Self::META.0) != 0
  }
}

impl std::ops::BitOr for PointerModifiers {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

impl std::ops::BitOrAssign for PointerModifiers {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

