use url::Url;

pub const CHROME_DIALOG_SCHEME: &str = "chrome-dialog";

/// Parsed representation of a `chrome-dialog:` URL.
///
/// This scheme is reserved for *trusted* renderer-chrome HTML documents to encode modal/dialog
/// "button result" actions (accept/cancel) in a way that is unambiguous from `chrome-action:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeDialogActionUrl {
  Accept,
  Cancel,
}

impl ChromeDialogActionUrl {
  /// Parse a `chrome-dialog:` URL string.
  pub fn parse(raw: &str) -> Result<Self, String> {
    let raw = trim_ascii_whitespace(raw);
    let url = Url::parse(raw).map_err(|err| err.to_string())?;
    Self::parse_url(&url)
  }

  /// Parse a `chrome-dialog:` action from an already-parsed URL.
  pub fn parse_url(url: &Url) -> Result<Self, String> {
    if !url.scheme().eq_ignore_ascii_case(CHROME_DIALOG_SCHEME) {
      return Err(format!(
        "expected {CHROME_DIALOG_SCHEME}: URL, got scheme={:?}",
        url.scheme()
      ));
    }

    // Canonical form: `chrome-dialog:<action>` (opaque/cannot-be-a-base URLs).
    //
    // Reject `chrome-dialog://<action>` to avoid ambiguous host/path parsing, and to keep the scheme
    // "command-shaped" (not fetchable/URL-like).
    if !url.cannot_be_a_base() || url.host_str().is_some() {
      return Err(format!(
        "{CHROME_DIALOG_SCHEME}: URLs must not use an authority form (expected `chrome-dialog:accept`, not `chrome-dialog://...`)"
      ));
    }
    if url.fragment().is_some() {
      return Err(format!(
        "{CHROME_DIALOG_SCHEME}: URLs must not include a fragment (`#...`)"
      ));
    }

    let action = url.path().to_ascii_lowercase();
    match action.as_str() {
      "accept" => Ok(Self::Accept),
      "cancel" => Ok(Self::Cancel),
      "" => Err("missing chrome-dialog action".to_string()),
      _ => Err(format!("unknown chrome-dialog action: {action:?}")),
    }
  }

  /// Format as a canonical `chrome-dialog:` URL string.
  pub fn to_url_string(&self) -> String {
    match self {
      Self::Accept => format!("{CHROME_DIALOG_SCHEME}:accept"),
      Self::Cancel => format!("{CHROME_DIALOG_SCHEME}:cancel"),
    }
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  // Match HTML URL-ish attribute whitespace rules (TAB/LF/FF/CR/SPACE).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn chrome_dialog_url_round_trips() {
    let cases = [ChromeDialogActionUrl::Accept, ChromeDialogActionUrl::Cancel];
    for action in cases {
      let url = action.to_url_string();
      let parsed = ChromeDialogActionUrl::parse(&url).unwrap_or_else(|err| {
        panic!("failed to parse {url:?}: {err}");
      });
      assert_eq!(parsed, action);
    }
  }

  #[test]
  fn chrome_dialog_parsing_allows_query_payloads() {
    assert_eq!(
      ChromeDialogActionUrl::parse("chrome-dialog:accept?value=abc").unwrap(),
      ChromeDialogActionUrl::Accept
    );
  }

  #[test]
  fn chrome_dialog_parsing_rejects_authority_form() {
    assert!(ChromeDialogActionUrl::parse("chrome-dialog://accept").is_err());
    assert!(ChromeDialogActionUrl::parse("chrome-dialog://accept/").is_err());
    assert!(ChromeDialogActionUrl::parse("chrome-dialog://accept?value=abc").is_err());
  }

  #[test]
  fn chrome_dialog_parsing_rejects_fragments() {
    let err = ChromeDialogActionUrl::parse("chrome-dialog:accept#frag").unwrap_err();
    assert!(
      err.to_ascii_lowercase().contains("fragment"),
      "unexpected error: {err}"
    );
  }

  #[test]
  fn chrome_dialog_parsing_trims_ascii_whitespace() {
    assert_eq!(
      ChromeDialogActionUrl::parse(" chrome-dialog:accept \n").unwrap(),
      ChromeDialogActionUrl::Accept
    );
  }

  #[test]
  fn chrome_dialog_parsing_rejects_unknown_actions() {
    let err = ChromeDialogActionUrl::parse("chrome-dialog:maybe").unwrap_err();
    assert!(err.to_ascii_lowercase().contains("unknown"));
  }
}
