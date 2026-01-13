use crate::ui::TabId;

/// Coalesces repeated UI text input events into a single pending payload.
///
/// The windowed browser UI receives one `WindowEvent::ReceivedCharacter` per typed character.
/// Forwarding each event directly to the render worker can create large UI→worker backlogs when the
/// worker is slow to paint. Instead, the UI accumulates characters for the active tab and flushes
/// them at logical boundaries (frame start, before other input messages, focus changes).
#[derive(Debug, Default, Clone)]
pub struct TextInputBuffer {
  pending: Option<(TabId, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputPushResult {
  /// When switching tabs without flushing, the previous pending payload is returned so callers can
  /// preserve ordering and avoid dropping text.
  pub flushed: Option<(TabId, String)>,
  /// True when this call started a new pending buffer (i.e. the buffer was empty or belonged to a
  /// different tab).
  pub started_new: bool,
}

impl TextInputBuffer {
  pub fn is_empty(&self) -> bool {
    self.pending.is_none()
  }

  pub fn pending_tab(&self) -> Option<TabId> {
    self.pending.as_ref().map(|(tab_id, _)| *tab_id)
  }

  pub fn clear(&mut self) {
    self.pending = None;
  }

  pub fn take(&mut self) -> Option<(TabId, String)> {
    self.pending.take()
  }

  pub fn push_char(&mut self, tab_id: TabId, ch: char) -> TextInputPushResult {
    match self.pending.as_mut() {
      Some((pending_tab, pending_text)) if *pending_tab == tab_id => {
        pending_text.push(ch);
        TextInputPushResult {
          flushed: None,
          started_new: false,
        }
      }
      _ => {
        let flushed = self.pending.take();
        let mut text = String::new();
        text.push(ch);
        self.pending = Some((tab_id, text));
        TextInputPushResult {
          flushed,
          started_new: true,
        }
      }
    }
  }

  pub fn push_str(&mut self, tab_id: TabId, text: &str) -> TextInputPushResult {
    if text.is_empty() {
      return TextInputPushResult {
        flushed: None,
        started_new: false,
      };
    }

    match self.pending.as_mut() {
      Some((pending_tab, pending_text)) if *pending_tab == tab_id => {
        pending_text.push_str(text);
        TextInputPushResult {
          flushed: None,
          started_new: false,
        }
      }
      _ => {
        let flushed = self.pending.take();
        self.pending = Some((tab_id, text.to_string()));
        TextInputPushResult {
          flushed,
          started_new: true,
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::messages::TAB_ID_TEST_LOCK;

  #[test]
  fn accumulates_chars_for_same_tab() {
    let _lock = TAB_ID_TEST_LOCK.lock().unwrap();
    let tab = TabId::new();
    let mut buf = TextInputBuffer::default();

    let r1 = buf.push_char(tab, 'a');
    assert_eq!(r1.flushed, None);
    assert!(r1.started_new);
    assert_eq!(buf.pending_tab(), Some(tab));

    let r2 = buf.push_char(tab, 'b');
    assert_eq!(r2.flushed, None);
    assert!(!r2.started_new);

    assert_eq!(buf.take(), Some((tab, "ab".to_string())));
    assert!(buf.is_empty());
  }

  #[test]
  fn flushes_when_switching_tabs_without_dropping_text() {
    let _lock = TAB_ID_TEST_LOCK.lock().unwrap();
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let mut buf = TextInputBuffer::default();

    let _ = buf.push_str(tab_a, "hello");
    let res = buf.push_char(tab_b, 'x');
    assert_eq!(res.flushed, Some((tab_a, "hello".to_string())));
    assert!(res.started_new);
    assert_eq!(buf.take(), Some((tab_b, "x".to_string())));
  }

  #[test]
  fn push_str_appends_to_existing_buffer() {
    let _lock = TAB_ID_TEST_LOCK.lock().unwrap();
    let tab = TabId::new();
    let mut buf = TextInputBuffer::default();

    let r1 = buf.push_str(tab, "hi");
    assert!(r1.started_new);
    assert_eq!(r1.flushed, None);

    let r2 = buf.push_str(tab, " there");
    assert!(!r2.started_new);
    assert_eq!(r2.flushed, None);

    assert_eq!(buf.take(), Some((tab, "hi there".to_string())));
  }
}

