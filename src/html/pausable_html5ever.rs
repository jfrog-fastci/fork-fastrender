use html5ever::driver::Parser;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::BufferQueue;
use html5ever::tree_builder::TreeSink;
use html5ever::ParseOpts;
use markup5ever::interface::TokenizerResult;
use smallvec::SmallVec;

use crate::error::{Error, ParseError, RenderStage, Result};
use crate::render_control::check_active_periodic;

const HTML5EVER_INPUT_MAX_TENDRIL_BYTES: usize = 16 * 1024;
const HTML5EVER_PUMP_DEADLINE_STRIDE: usize = 64;
const HTML5EVER_REMAINDER_MAX_TENDRILS: usize = 64;

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
    let Some(parser) = self.parser.as_ref() else {
      return;
    };

    let mut start = 0usize;
    while start < chunk.len() {
      let mut end = (start + HTML5EVER_INPUT_MAX_TENDRIL_BYTES).min(chunk.len());
      while end < chunk.len() && !chunk.is_char_boundary(end) {
        end -= 1;
      }
      parser
        .input_buffer
        .push_back(StrTendril::from_slice(&chunk[start..end]));
      start = end;
    }
  }

  /// Like `document.write`: inject text before any buffered “remaining input”.
  pub fn push_front_str(&self, chunk: &str) {
    if chunk.is_empty() {
      return;
    }
    let Some(parser) = self.parser.as_ref() else {
      return;
    };

    let mut end = chunk.len();
    while end > 0 {
      let mut start = end.saturating_sub(HTML5EVER_INPUT_MAX_TENDRIL_BYTES);
      while start < end && !chunk.is_char_boundary(start) {
        start += 1;
      }
      parser
        .input_buffer
        .push_front(StrTendril::from_slice(&chunk[start..end]));
      end = start;
    }
  }

  /// Signal no more input will arrive.
  pub fn set_eof(&mut self) {
    self.eof = true;
  }

  /// Whether the parser has finished and can no longer be pumped or inspected.
  pub fn is_finished(&self) -> bool {
    self.parser.is_none()
  }

  /// Execute `f` with mutable access to the underlying `TreeSink`.
  ///
  /// This can be used between [`pump`](Self::pump) calls to inspect or mutate the
  /// live DOM / base-url state when html5ever yields `TokenizerResult::Script`.
  pub fn with_sink<R>(&mut self, f: impl FnOnce(&mut Sink) -> R) -> Option<R> {
    self
      .parser
      .as_mut()
      .map(|parser| f(&mut parser.tokenizer.sink.sink))
  }

  /// Borrow the underlying tree sink.
  ///
  pub fn sink(&self) -> Option<&Sink> {
    self
      .parser
      .as_ref()
      .map(|parser| &parser.tokenizer.sink.sink)
  }

  /// Mutably borrow the underlying tree sink.
  ///
  pub fn sink_mut(&mut self) -> Option<&mut Sink> {
    self
      .parser
      .as_mut()
      .map(|parser| &mut parser.tokenizer.sink.sink)
  }

  /// Run the tokenizer/tree-builder until it either needs a script, needs more
  /// input, or finishes.
  pub fn pump(&mut self) -> Result<Html5everPump<Sink::Handle, Sink::Output>> {
    let mut deadline_counter = HTML5EVER_PUMP_DEADLINE_STRIDE - 1;
    loop {
      let Some(parser) = self.parser.take() else {
        return Ok(Html5everPump::NeedMoreInput);
      };

      // If there's no buffered input left, either yield for more or finish if EOF was signalled.
      let Some(next_input) = parser.input_buffer.pop_front() else {
        if !self.eof {
          self.parser = Some(parser);
          return Ok(Html5everPump::NeedMoreInput);
        }

        // EOF: finalize the tokenizer/tree builder.
        let end_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          parser.tokenizer.end();
        }));
        if let Err(panic_payload) = end_result {
          return Err(Error::Parse(ParseError::InvalidHtml {
            message: format!("html5ever panicked while finalizing tokenizer: {}", panic_message(panic_payload)),
            line: 0,
          }));
        }

        let finish_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          parser.tokenizer.sink.sink.finish()
        }));
        match finish_result {
          Ok(output) => return Ok(Html5everPump::Finished(output)),
          Err(panic_payload) => {
            return Err(Error::Parse(ParseError::InvalidHtml {
              message: format!(
                "html5ever panicked while finishing tree sink: {}",
                panic_message(panic_payload)
              ),
              line: 0,
            }))
          }
        }
      };

      // Ensure timeouts/cancellation are observed periodically even when a large amount of input has
      // already been buffered into `parser.input_buffer`.
      //
      // We intentionally drive html5ever in small chunks so a single `tokenizer.feed` call can't run
      // arbitrarily long without returning to FastRender's cooperative deadline checks.
      if let Err(err) = check_active_periodic(
        &mut deadline_counter,
        HTML5EVER_PUMP_DEADLINE_STRIDE,
        RenderStage::DomParse,
      ) {
        // Avoid dropping the buffered input we just pulled.
        parser.input_buffer.push_front(next_input);
        self.parser = Some(parser);
        return Err(Error::Render(err));
      }

      let chunk = BufferQueue::default();
      let initial_len = next_input.len();
      chunk.push_back(next_input);

      // Drive html5ever directly so `TokenizerResult::Script` can be observed.
      let feed_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parser.tokenizer.feed(&chunk)
      }));
      let result = match feed_result {
        Ok(r) => r,
        Err(panic_payload) => {
          // Parser state may be inconsistent after unwinding; drop it by leaving `self.parser = None`.
          return Err(Error::Parse(ParseError::InvalidHtml {
            message: format!("html5ever panicked while parsing: {}", panic_message(panic_payload)),
            line: 0,
          }));
        }
      };

      // Return any unconsumed remainder back to the shared input queue.
      let mut remaining_len = 0usize;
      let mut remaining_tendrils = 0usize;
      let mut remainder: SmallVec<[StrTendril; HTML5EVER_REMAINDER_MAX_TENDRILS]> =
        SmallVec::new();
      while let Some(t) = chunk.pop_front() {
        if remainder.len() >= HTML5EVER_REMAINDER_MAX_TENDRILS {
          // Defensive: an unexpectedly fragmented remainder would force heap allocation; treat it as
          // a fatal parse error rather than risking unbounded memory growth.
          self.parser = Some(parser);
          return Err(Error::Parse(ParseError::InvalidHtml {
            message: format!(
              "html5ever produced too many remainder tendrils while parsing: max={HTML5EVER_REMAINDER_MAX_TENDRILS}"
            ),
            line: 0,
          }));
        }
        remaining_len = remaining_len.saturating_add(t.len());
        remainder.push(t);
      }
      remaining_tendrils = remainder.len();
      for t in remainder.into_iter().rev() {
        parser.input_buffer.push_front(t);
      }

      // Defensive: html5ever should always consume input when `feed` returns.
      // If it reports `Done`/`Script` without consuming anything, we'd spin forever re-feeding the
      // same tendril.
      if initial_len == 0 || remaining_len >= initial_len {
        // Put the parser back so callers can inspect state if needed.
        self.parser = Some(parser);
        return Err(Error::Parse(ParseError::InvalidHtml {
          message: format!(
            "html5ever made no progress while parsing: initial_bytes={initial_len} remaining_bytes={remaining_len} remaining_tendrils={remaining_tendrils}"
          ),
          line: 0,
        }));
      }

      match result {
        TokenizerResult::Script(handle) => {
          self.parser = Some(parser);
          return Ok(Html5everPump::Script(handle));
        }
        TokenizerResult::Done => {
          // Continue pumping buffered input (or return NeedMoreInput/Finished once it drains).
        }
      }

      self.parser = Some(parser);
    }
  }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
  if let Some(s) = payload.downcast_ref::<&str>() {
    (*s).to_string()
  } else if let Some(s) = payload.downcast_ref::<String>() {
    s.clone()
  } else {
    "unknown panic payload".to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::{Html5everPump, PausableHtml5everParser};
  use crate::render_control::{with_deadline, RenderDeadline};
  use html5ever::tendril::StrTendril;
  use html5ever::tree_builder::TreeBuilderOpts;
  use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as HtmlQuirksMode, TreeSink};
  use html5ever::ParseOpts;
  use markup5ever::interface::Attribute;
  use markup5ever::QualName;
  use markup5ever_rcdom::{Handle, NodeData, RcDom};
  use std::borrow::Cow;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use std::time::Duration;

  // `RcDom::document` points at the root document node; walking the tree should
  // find any handles inserted so far.
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

  #[derive(Default)]
  struct PanicOnParseErrorSink {
    inner: RcDom,
  }

  impl TreeSink for PanicOnParseErrorSink {
    type Handle = <RcDom as TreeSink>::Handle;
    type Output = <RcDom as TreeSink>::Output;
    type ElemName<'a>
      = <RcDom as TreeSink>::ElemName<'a>
    where
      Self: 'a;

    fn finish(self) -> Self::Output {
      self.inner.finish()
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
      panic!("parse_error: {msg}");
    }

    fn get_document(&self) -> Self::Handle {
      self.inner.get_document()
    }

    fn set_quirks_mode(&self, mode: HtmlQuirksMode) {
      self.inner.set_quirks_mode(mode);
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
      self.inner.same_node(x, y)
    }

    fn allow_declarative_shadow_roots(&self, intended_parent: &Self::Handle) -> bool {
      self.inner.allow_declarative_shadow_roots(intended_parent)
    }

    fn attach_declarative_shadow(
      &self,
      location: &Self::Handle,
      template: &Self::Handle,
      attrs: &[Attribute],
    ) -> bool {
      self.inner.attach_declarative_shadow(location, template, attrs)
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
      self.inner.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Self::Handle {
      self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> Self::Handle {
      self.inner.create_comment(text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Self::Handle {
      self.inner.create_pi(target, data)
    }

    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
      self.inner.append(parent, child)
    }

    fn append_before_sibling(&self, sibling: &Self::Handle, child: NodeOrText<Self::Handle>) {
      self.inner.append_before_sibling(sibling, child)
    }

    fn append_based_on_parent_node(
      &self,
      element: &Self::Handle,
      prev_element: &Self::Handle,
      child: NodeOrText<Self::Handle>,
    ) {
      self
        .inner
        .append_based_on_parent_node(element, prev_element, child)
    }

    fn append_doctype_to_document(
      &self,
      name: StrTendril,
      public_id: StrTendril,
      system_id: StrTendril,
    ) {
      self
        .inner
        .append_doctype_to_document(name, public_id, system_id)
    }

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
      self.inner.get_template_contents(target)
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
      self.inner.remove_from_parent(target)
    }

    fn reparent_children(&self, node: &Self::Handle, new_parent: &Self::Handle) {
      self.inner.reparent_children(node, new_parent)
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
      self.inner.add_attrs_if_missing(target, attrs)
    }

    fn mark_script_already_started(&self, node: &Self::Handle) {
      self.inner.mark_script_already_started(node)
    }

    fn is_mathml_annotation_xml_integration_point(&self, node: &Self::Handle) -> bool {
      self.inner.is_mathml_annotation_xml_integration_point(node)
    }

    fn pop(&self, node: &Self::Handle) {
      self.inner.pop(node)
    }
  }

  #[test]
  fn pump_returns_err_when_sink_panics_on_malformed_input() {
    // U+0000 is a parse error in many tokenizer states; html5ever reports it via `TreeSink::parse_error`.
    // Ensure our pausable driver converts panics from the underlying html5ever stack into `Result::Err`.
    let mut parser =
      PausableHtml5everParser::new_document(PanicOnParseErrorSink::default(), ParseOpts::default());
    parser.push_str("<p>\0</p>");
    parser.set_eof();

    let result = parser.pump();
    assert!(result.is_err());
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

    let h1 = match parser.pump().unwrap() {
      Html5everPump::Script(h) => h,
      _ => panic!("expected first pump to yield Script"),
    };

    {
      let sink = parser.sink().unwrap();
      assert!(
        contains_handle(&sink.document, &h1),
        "expected yielded script handle to be present in the in-progress DOM"
      );
    }

    let h2 = match parser.pump().unwrap() {
      Html5everPump::Script(h) => h,
      _ => panic!("expected second pump to yield Script"),
    };
    let dom = match parser.pump().unwrap() {
      Html5everPump::Finished(dom) => dom,
      _ => panic!("expected third pump to finish"),
    };

    assert_script_element_with_text(&h1, "a");
    assert_script_element_with_text(&h2, "b");

    // Ensure both handles are associated with the returned DOM.
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

    match parser.pump().unwrap() {
      Html5everPump::Finished(_) => {}
      _ => panic!("expected parser to finish without yielding Script"),
    };
  }

  #[test]
  fn sink_accessor_returns_none_after_finished() {
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

    match parser.pump().unwrap() {
      Html5everPump::Finished(_) => {}
      _ => panic!("expected parser to finish without yielding Script"),
    };

    assert!(parser.with_sink(|_| ()).is_none());
  }

  #[test]
  fn sink_returns_none_after_finished() {
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

    match parser.pump().unwrap() {
      Html5everPump::Finished(_) => {}
      _ => panic!("expected parser to finish without yielding Script"),
    };

    assert!(parser.sink().is_none());
  }

  #[test]
  fn sink_mut_returns_none_after_finished() {
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

    match parser.pump().unwrap() {
      Html5everPump::Finished(_) => {}
      _ => panic!("expected parser to finish without yielding Script"),
    };

    assert!(parser.sink_mut().is_none());
  }

  #[test]
  fn chunks_large_push_str_into_multiple_tendrils() {
    let mut parser = PausableHtml5everParser::new_document(RcDom::default(), ParseOpts::default());
    let big = "a".repeat(33 * 1024);
    parser.push_str(&big);

    let input = &parser.parser.as_ref().unwrap().input_buffer;
    let mut saw = 0usize;
    while let Some(t) = input.pop_front() {
      assert!(t.len() <= super::HTML5EVER_INPUT_MAX_TENDRIL_BYTES);
      saw += 1;
    }
    assert!(saw >= 3, "expected multiple input tendrils, saw {saw}");
  }

  #[test]
  fn pump_aborts_on_expired_deadline() {
    let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
    let result = with_deadline(Some(&deadline), || {
      let mut parser =
        PausableHtml5everParser::new_document(RcDom::default(), ParseOpts::default());
      parser.push_str("<!doctype html><p>x</p>");
      parser.set_eof();
      parser.pump()
    });

    match result {
      Err(crate::error::Error::Render(crate::error::RenderError::Timeout {
        stage: crate::error::RenderStage::DomParse,
        ..
      })) => {}
      Ok(_) => panic!("expected dom_parse timeout, got Ok"),
      Err(err) => panic!("expected dom_parse timeout, got {err}"),
    }
  }

  #[test]
  fn pump_aborts_on_cancel_mid_parse() {
    // The cancel callback returns false the first time it is queried, then true on the next
    // deadline check. This ensures we only abort if the parser performs multiple periodic deadline
    // checks while consuming a large buffered document.
    let calls: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let calls_for_cb = Arc::clone(&calls);
    let deadline = RenderDeadline::new(
      None,
      Some(Arc::new(move || {
        let prev = calls_for_cb.fetch_add(1, Ordering::Relaxed);
        prev >= 1
      })),
    );

    let big = "a".repeat(
      super::HTML5EVER_INPUT_MAX_TENDRIL_BYTES * (super::HTML5EVER_PUMP_DEADLINE_STRIDE + 1),
    );
    let result = with_deadline(Some(&deadline), || {
      let mut parser =
        PausableHtml5everParser::new_document(RcDom::default(), ParseOpts::default());
      parser.push_str(&big);
      parser.set_eof();
      parser.pump()
    });

    match result {
      Err(crate::error::Error::Render(crate::error::RenderError::Timeout {
        stage: crate::error::RenderStage::DomParse,
        ..
      })) => {}
      Ok(_) => panic!("expected dom_parse timeout, got Ok"),
      Err(err) => panic!("expected dom_parse timeout, got {err}"),
    }

    assert!(
      calls.load(Ordering::Relaxed) >= 2,
      "expected cancel callback to be consulted multiple times"
    );
  }
}
