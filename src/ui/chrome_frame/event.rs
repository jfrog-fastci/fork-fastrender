/// Events emitted by the renderer-chrome runtime that describe DOM-driven state changes which the
/// embedding should mirror into `BrowserAppState.chrome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeFrameEvent {
  /// Emitted when the address bar `<input>`'s value changes due to text input/paste/IME commits.
  AddressBarTextChanged(String),
  /// Emitted when focus enters/leaves the address bar `<input>`.
  AddressBarFocusChanged(bool),
}

