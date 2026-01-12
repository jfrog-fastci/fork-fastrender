use crate::tree::fragment_tree::FragmentNode;
use crate::{FastRender, FontConfig, Rgba};

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }

  node
    .children
    .iter()
    .find_map(|child| find_fragment_by_background(child, color))
}

#[test]
fn flex_nested_container_align_self_baseline_does_not_expand_height() {
  // Regression: a flex item that is itself a (nested) flex container should not gain extra
  // cross-axis size solely because it uses `align-self: baseline` in its parent.
  //
  // This pattern is common for button rows: the parent aligns items on text baselines, but each
  // button is an internal flex container (e.g. icon + label) with `align-items: center`.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let left_bg = Rgba::rgb(247, 247, 253);
  let right_bg = Rgba::rgb(88, 101, 242);

  let html = r#"
    <style>
      * { box-sizing: border-box; }
      .row { display: flex; }

      /* Webflow embed clearfix (used by many marketing pages). */
      .w-embed::before, .w-embed::after { content: " "; display: table; }
      .w-embed::after { clear: both; }

      .btn {
        background: rgb(247, 247, 253);
        display: flex;
        align-items: center;
        justify-content: center;
        min-height: 56px;
        padding: 16.7px 24px;
        font-size: 18px;
        line-height: 24px;
      }

      .btn.other {
        background: rgb(88, 101, 242);
      }

      .btn.baseline {
        /* Align the button itself as a flex item. */
        align-self: baseline;
      }

      .icon {
        width: 24px;
        height: 24px;
        margin-right: 8.5px;
        display: block;
      }
    </style>
    <div class="row">
      <a class="btn baseline">
        <div class="icon w-embed">
          <svg width="100%" height="100%" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"></svg>
        </div>
        <div>Download</div>
      </a>
      <a class="btn other">Other</a>
    </div>
  "#;

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 864, 200)
    .expect("layout document");

  let left = find_fragment_by_background(&fragments.root, left_bg).expect("left button fragment");
  assert!(
    find_fragment_by_background(&fragments.root, right_bg).is_some(),
    "expected right button fragment"
  );
  let height = left.bounds.height();

  // 24px line-height + 16.7px top/bottom padding = 57.4px border box.
  assert!(
    (height - 57.4).abs() < 0.5,
    "expected baseline-aligned nested flex button to size to content+padding; got height={height} bounds={:?}",
    left.bounds
  );
}
