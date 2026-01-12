use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::media::MediaContext;

#[test]
fn cascade_and_styled_node_clone_do_not_panic_on_internal_invariants() {
  const DEPTH: usize = 128;

  let mut shadow_deep = String::new();
  for i in 0..DEPTH {
    shadow_deep.push_str(&format!(r#"<div class="shadow-depth-{i}">"#));
  }
  shadow_deep.push_str(r#"<span id="shadow-leaf">shadow leaf</span>"#);
  for _ in 0..DEPTH {
    shadow_deep.push_str("</div>");
  }

  let mut light_deep = String::new();
  for i in 0..DEPTH {
    light_deep.push_str(&format!(r#"<div class="light-depth-{i}">"#));
  }
  light_deep.push_str(r#"<span id="light-leaf">light leaf</span>"#);
  for _ in 0..DEPTH {
    light_deep.push_str("</div>");
  }

  let html = format!(
    r#"
      <div id="outer-host">
        <template shadowroot="open">
          <style>
            :host {{ color: rgb(1, 2, 3); }}
            slot {{ font-size: 20px; }}
          </style>
          <div id="shadow-wrapper">
            <slot name="outer-slot"></slot>
            <div id="shadow-deep">{shadow_deep}</div>
            <div id="inner-host">
              <template shadowroot="open">
                <style>:host {{ color: rgb(4, 5, 6); }}</style>
                <div class="inner-shadow">
                  <slot name="inner-slot"></slot>
                </div>
              </template>
              <span id="inner-slotted" slot="inner-slot">inner slotted</span>
            </div>
          </div>
        </template>
        <div id="outer-slotted" slot="outer-slot">{light_deep}</div>
      </div>
    "#
  );

  let dom = dom::parse_html(&html).expect("parse html");
  let stylesheet = parse_stylesheet("div { color: rgb(7, 8, 9); }").expect("parse stylesheet");
  let media_ctx = MediaContext::screen(1200.0, 800.0);

  let styled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    apply_styles_with_media_target_and_imports(
      &dom,
      &stylesheet,
      &media_ctx,
      None,
      None,
      None,
      None,
      None,
      None,
    )
  }));
  assert!(
    styled.is_ok(),
    "apply_styles_with_media_target_and_imports panicked on a deep tree with shadow roots + slots"
  );

  let styled = styled.unwrap();
  let cloned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| styled.clone()));
  assert!(cloned.is_ok(), "StyledNode::clone panicked");
}

