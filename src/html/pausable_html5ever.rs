use html5ever::driver::Parser;
use html5ever::tendril::StrTendril;
use html5ever::tree_builder::TreeSink;
use html5ever::ParseOpts;
use markup5ever::interface::TokenizerResult;

/// Result of a [`PausableHtml5everParser::pump`] call.
pub enum Html5everPump<Handle, Output> {
  /// Parser hit a `</script>` end tag and must yield to the host.
  Script(Handle),
  /// Parser consumed all buffered input but has not been told EOF yet.
  NeedMoreInput,
  /// EOF was signalled and the DOM is finished.
  Finished(Output),
}

/// A script-aware, pausable wrapper around html5ever's `Parser`.
///
/// html5ever's built-in driver (`TendrilSink::process`) currently ignores
/// `TokenizerResult::Script` suspension points by looping until `Done` (see
/// html5ever `driver.rs` FIXME). This wrapper exposes those suspension points so
/// callers can implement the HTML script processing model.
pub struct PausableHtml5everParser<Sink: TreeSink> {
  parser: Option<Parser<Sink>>,
  eof: bool,
}

impl<Sink: TreeSink> PausableHtml5everParser<Sink> {
  pub fn new_document(sink: Sink, opts: ParseOpts) -> Self {
    Self {
      parser: Some(html5ever::parse_document(sink, opts)),
      eof: false,
    }
  }

  /// Append decoded Unicode input.
  pub fn push_str(&self, chunk: &str) {
    if chunk.is_empty() {
      return;
    }
    let parser = self
      .parser
      .as_ref()
      .expect("push_str called after parser finished");
    parser
      .input_buffer
      .push_back(StrTendril::from_slice(chunk));
  }

  /// Like `document.write`: inject text before any buffered “remaining input”.
  pub fn push_front_str(&self, chunk: &str) {
    if chunk.is_empty() {
      return;
    }
    let parser = self
      .parser
      .as_ref()
      .expect("push_front_str called after parser finished");
    parser
      .input_buffer
      .push_front(StrTendril::from_slice(chunk));
  }

  /// Signal no more input will arrive.
  pub fn set_eof(&mut self) {
    self.eof = true;
  }

  /// Run the tokenizer/tree-builder until it either needs a script, needs more
  /// input, or finishes.
  pub fn pump(&mut self) -> Html5everPump<Sink::Handle, Sink::Output> {
    loop {
      let result = {
        let parser = self
          .parser
          .as_mut()
          .expect("pump called after parser finished");
        // Drive html5ever directly so `TokenizerResult::Script` can be observed.
        parser.tokenizer.feed(&parser.input_buffer)
      };

      match result {
        TokenizerResult::Script(handle) => return Html5everPump::Script(handle),
        TokenizerResult::Done => {
          if !self.eof {
            return Html5everPump::NeedMoreInput;
          }

          let parser = self
            .parser
            .take()
            .expect("pump called after parser finished");

          parser.tokenizer.end();
          let output = parser.tokenizer.sink.sink.finish();
          return Html5everPump::Finished(output);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{Html5everPump, PausableHtml5everParser};
  use html5ever::tree_builder::TreeBuilderOpts;
  use html5ever::ParseOpts;
  use markup5ever_rcdom::{Handle, NodeData, RcDom};

  fn assert_script_element_with_text(handle: &Handle, expected_text: &str) {
    match &handle.data {
      NodeData::Element { name, .. } => assert_eq!(name.local.as_ref(), "script"),
      other => panic!("expected script element handle, got {other:?}"),
    }

    let text = handle
      .children
      .borrow()
      .iter()
      .filter_map(|child| match &child.data {
        NodeData::Text { contents } => Some(contents.borrow().to_string()),
        _ => None,
      })
      .collect::<String>();
    assert_eq!(text, expected_text);
  }

  #[test]
  fn yields_two_scripts_in_document_order_and_resumes() {
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };

    let mut parser = PausableHtml5everParser::new_document(RcDom::default(), opts);
    parser.push_str("<!doctype html><script>a</script><p>x</p><script>b</script>");
    parser.set_eof();

    let h1 = match parser.pump() {
      Html5everPump::Script(h) => h,
      _ => panic!("expected first pump to yield Script"),
    };
    let h2 = match parser.pump() {
      Html5everPump::Script(h) => h,
      _ => panic!("expected second pump to yield Script"),
    };
    let dom = match parser.pump() {
      Html5everPump::Finished(dom) => dom,
      _ => panic!("expected third pump to finish"),
    };

    assert_script_element_with_text(&h1, "a");
    assert_script_element_with_text(&h2, "b");

    // Ensure both handles are associated with the returned DOM.
    // `RcDom::document` points at the root document node; walking the tree should
    // find both script handles.
    fn contains_handle(root: &Handle, needle: &Handle) -> bool {
      if std::rc::Rc::ptr_eq(root, needle) {
        return true;
      }
      for child in root.children.borrow().iter() {
        if contains_handle(child, needle) {
          return true;
        }
      }
      false
    }

    assert!(contains_handle(&dom.document, &h1));
    assert!(contains_handle(&dom.document, &h2));
  }

  #[test]
  fn finishes_without_yielding_script() {
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };

    let mut parser = PausableHtml5everParser::new_document(RcDom::default(), opts);
    parser.push_str("<!doctype html><p>x</p>");
    parser.set_eof();

    match parser.pump() {
      Html5everPump::Finished(_) => {}
      _ => panic!("expected parser to finish without yielding Script"),
    };
  }
}
