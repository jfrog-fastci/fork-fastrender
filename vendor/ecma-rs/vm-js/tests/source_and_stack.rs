use std::sync::Arc;
use vm_js::Heap;
use vm_js::HeapLimits;
use vm_js::format_stack_trace;
use vm_js::SourceText;
use vm_js::StackFrame;

#[test]
fn source_text_line_col_handles_newlines_tabs_and_utf8() {
  let text = "a\té\nb🙂c\n";
  let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  let source = SourceText::new_charged(&mut heap, "<inline>", text).unwrap();

  // Byte offsets are used as the input; reported columns are 1-based UTF-8
  // byte offsets from the start of the line (exact for ASCII).
  let offset_e = text.find('é').unwrap() as u32;
  assert_eq!(source.line_col(offset_e), (1, 3));

  let offset_b = text.find('b').unwrap() as u32;
  assert_eq!(source.line_col(offset_b), (2, 1));

  let offset_c = text.find('c').unwrap() as u32;
  assert_eq!(source.line_col(offset_c), (2, 6));

  // Clamp offsets that point inside a UTF-8 sequence.
  let offset_emoji = text.find('🙂').unwrap();
  assert_eq!(source.line_col((offset_emoji + 1) as u32), (2, 2));
}

#[test]
fn source_text_line_col_is_dos_safe_on_huge_single_line_sources() {
  const SIZE: usize = 5 * 1024 * 1024;
  let text = String::from_utf8(vec![b'a'; SIZE]).unwrap();
  let mut heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  let source = SourceText::new_charged(&mut heap, "<inline>", text).unwrap();

  let end_offset = SIZE as u32;

  // This loop is intentionally large: the implementation must not scan the
  // source linearly per call (which would make this test prohibitively slow and
  // allow O(n^2) behavior for stack trace / diagnostic mapping on huge single-
  // line sources).
  for _ in 0..50_000 {
    assert_eq!(source.line_col(end_offset), (1, end_offset + 1));
  }

  // Offsets outside the text are clamped.
  assert_eq!(source.line_col(end_offset.saturating_add(123)), (1, end_offset + 1));
}

#[test]
fn stack_trace_formatting_is_stable() {
  let frames = vec![
    StackFrame {
      function: Some(Arc::<str>::from("foo")),
      source: Arc::<str>::from("script.js"),
      line: 1,
      col: 5,
    },
    StackFrame {
      function: None,
      source: Arc::<str>::from("script.js"),
      line: 2,
      col: 1,
    },
  ];

  let rendered = format_stack_trace(&frames);
  let expected = "at foo (script.js:1:5)\nat script.js:2:1";
  assert_eq!(rendered, expected);
}
