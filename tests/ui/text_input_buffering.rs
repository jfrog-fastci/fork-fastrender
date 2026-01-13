use fastrender::ui::{TabId, TextInputBuffer, UiToWorker};

struct TestUiChannel {
  pending: TextInputBuffer,
  sent: Vec<UiToWorker>,
}

impl TestUiChannel {
  fn new() -> Self {
    Self {
      pending: TextInputBuffer::default(),
      sent: Vec::new(),
    }
  }

  fn received_character(&mut self, tab_id: TabId, ch: char) {
    // Mirror `src/bin/browser.rs` semantics: character events are buffered and *not* immediately
    // forwarded as `UiToWorker::TextInput`.
    let result = self.pending.push_char(tab_id, ch);
    if let Some((tab_id, text)) = result.flushed {
      self.sent.push(UiToWorker::TextInput { tab_id, text });
    }
  }

  fn flush_frame(&mut self) {
    if let Some((tab_id, text)) = self.pending.take() {
      self.sent.push(UiToWorker::TextInput { tab_id, text });
    }
  }

  fn send_page_msg(&mut self, msg: UiToWorker) {
    // Mirror "flush before sending any other page-directed worker message".
    self.flush_frame();
    self.sent.push(msg);
  }

  fn sent_text_inputs(&self) -> Vec<&str> {
    self
      .sent
      .iter()
      .filter_map(|msg| match msg {
        UiToWorker::TextInput { text, .. } => Some(text.as_str()),
        _ => None,
      })
      .collect()
  }
}

#[test]
fn typing_coalesces_multiple_characters_into_single_text_input_message() {
  let tab_id = TabId::new();

  let mut ch = TestUiChannel::new();
  for c in "hello world".chars() {
    ch.received_character(tab_id, c);
  }
  // Simulate the next UI frame boundary (`render_frame` start) flushing buffered text.
  ch.flush_frame();

  assert_eq!(ch.sent_text_inputs(), vec!["hello world"]);
}

#[test]
fn flushes_pending_text_before_other_page_messages_to_preserve_ordering() {
  let tab_id = TabId::new();

  let mut ch = TestUiChannel::new();
  ch.received_character(tab_id, 'a');
  ch.received_character(tab_id, 'b');
  ch.send_page_msg(UiToWorker::KeyAction {
    tab_id,
    key: fastrender::interaction::KeyAction::Backspace,
  });

  let mut iter = ch.sent.iter();
  match iter.next() {
    Some(UiToWorker::TextInput { tab_id: got, text }) => {
      assert_eq!(*got, tab_id);
      assert_eq!(text, "ab");
    }
    other => panic!("expected first message to be TextInput, got {other:?}"),
  }
  match iter.next() {
    Some(UiToWorker::KeyAction { tab_id: got, .. }) => assert_eq!(*got, tab_id),
    other => panic!("expected second message to be KeyAction, got {other:?}"),
  }
  assert!(iter.next().is_none(), "expected exactly two messages");
}
