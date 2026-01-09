use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_line_y_containing_text(root: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(node: &FragmentNode, needle: &str, current_line_y: Option<f32>) -> Option<f32> {
    let current_line_y = match node.content {
      FragmentContent::Line { .. } => Some(node.bounds.y()),
      _ => current_line_y,
    };

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return current_line_y;
      }
    }

    for child in node.children.iter() {
      if let Some(found) = walk(child, needle, current_line_y) {
        return Some(found);
      }
    }
    None
  }

  walk(root, needle, None)
}

#[test]
fn right_float_does_not_force_all_lines_below_it_when_text_fits_beside() {
  // Regression for DailyMail "DON'T MISS" puff list: the headline text should start at the top of
  // the puff (next to the right-floated thumbnail), not be pushed down by the float's full height.
  //
  // This encodes a minimal version of the markup:
  //   <img style="float:right" width=154 height=115>
  //   <span class=pufftext style="display:block; width:140px; padding-left:7px">
  //     <span class=arrow-small-r style="float:left">...</span>
  //     <strong style="display:block">headline text</strong>
  //   </span>
  //
  // The headline is short enough to fit in the remaining horizontal space (308 - 154 = 154px),
  // so it must not be "cleared" below the float.
  let html = r##"
     <style>
       body { margin: 0; font-family: "DejaVu Sans", sans-serif; font-size: 16px; line-height: 1; }
       .puff { width: 308px; }
       .puff a { display: block; min-height: 115px; }
       .puff img.thumb { float: right; width: 154px; height: 115px; }
      .puff .pufftext { display: block; padding: 5px 5px 5px 7px; width: 140px; }
      .puff .pufftext span { margin-right: 3px; }
      .puff strong { display: block; }
      /* Equivalent to the reset in the DailyMail fixture. */
      .arrow-small-r { float: left; border-left: 5px solid currentcolor; border-top: 5px solid transparent; border-bottom: 5px solid transparent; height: 0; }
    </style>
    <div class="puff">
      <a href="#">
        <img class="thumb" alt="" />
        <span class="pufftext">
          <span class="arrow-small-r"></span>
          <strong>Headline fits next to float</strong>
          <br />
       </span>
     </a>
    </div>
  "##;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let y = find_line_y_containing_text(&fragments.root, "Headline fits")
    .expect("expected to find headline line fragment");

  assert!(
    y < 50.0,
    "expected headline line to start near the top of its container, got y={y}"
  );
}
