use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::ColorScheme;
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn hostile_css_does_not_panic_smoke_test() {
  let dom = dom::parse_html(
    r#"
      <div id="root" class="root">
        <div id="parent" class="foo parent">
          <div
            id="target"
            class="foo item"
            style="transform: translate(); background-image: linear-gradient();"
          >
            Target
          </div>
          <span id="sibling" class="item"></span>
        </div>
        <div id="other" class="bar">
          <span id="nested" class="foo bar"></span>
        </div>
      </div>
    "#,
  )
  .expect("parse html");

  let stylesheet = parse_stylesheet(
    r#"
      #target {
        font-size: 16px;
        opacity: 0.5;
        color: rgba(10, 20, 30, 0.75);
        background-color: rgba(40, 50, 60, 0.25);
      }

      #target {
        color: rgb();
        background-color: hsl();
        font-size: calc();
        opacity: calc();

        border-top-color: rgb();
        border-right-color: color-mix(in srgb, red,);
        border-bottom-color: lab();
        border-left-color: color(from rgb(0 0 0) srgb r g b /);

        outline-color: rgba(0, 0, 0, 2);
        caret-color: color-mix(in oklab, black 50%,);
        accent-color: rgb(10 20);

        width: calc(1px +);
        height: min(10px,);
        min-width: max();
        max-height: clamp(, 1px, 2px);

        margin: 1px 2px 3px 4px 5px;
        padding: calc(1px *);
        inset: 10px / 20px;

        border-width: 1px 2px 3px 4px 5px;
        border-radius: 10px / / 20px;
        outline-width: -1px;
        letter-spacing: calc(1em /);
        word-spacing: calc(/ 1em);
        line-height: calc(1 / 0);

        background-image: linear-gradient();
        background-image: radial-gradient(circle at, red, blue);
        background-image: conic-gradient(from, red);
        mask-image: url();
        mask-image: image-set(url(a.png) 1x,);
        border-image-source: url();
        list-style-image: linear-gradient();
        cursor: url(), auto;

        background: url() no-repeat left top / /;
        background-repeat: repeat-x repeat-y repeat;
        background-position: left top 10px 20px 30px;
        background-size: contain cover;

        transform: rotate();
        transform: translate(10px,);
        transform: matrix(1, 2, 3);
        transform-origin: calc() calc() calc();
        translate: 10px 20px 30px 40px;
        rotate: 10deg 1 0;
        scale: 1 2 3 4;
        perspective: calc();
        perspective-origin: left top / right;

        filter: blur();
        filter: drop-shadow(1px 2px);
        filter: hue-rotate();
        backdrop-filter: url(#);
        backdrop-filter: saturate();

        display: grid;
        grid-template-columns: repeat(, 1fr);
        grid-template-rows: minmax(, 1fr);
        grid-template-areas: "a" "b" /;
        grid-auto-flow: row dense dense;
        grid-auto-columns: minmax(, 1fr);
        grid-auto-rows: minmax(, 1fr);
        grid-column: span;
        grid-row: 1 / / 2;
        grid-area: a / b / c / d / e;
        gap: normal calc();
        row-gap: calc();
        column-gap: calc();
        place-items: safe safe center;
        place-content: stretch /;

        display: flex;
        flex: 1 1;
        flex-basis: calc();
        flex-direction: row column;
        flex-wrap: wrap nowrap;
        align-items: baseline baseline;
        justify-content: left left;
        align-content: safe unsafe center;
        order: calc();

        animation: spin 1s infinite linear;
        animation-name: spin,;
        animation-duration: calc();
        animation-timing-function: cubic-bezier(1, 2, 3);
        animation-iteration-count: -1;
        animation-direction: reverse reverse;
        animation-fill-mode: both both;
        animation-play-state: running running;

        transition: opacity;
        transition-property: opacity,;
        transition-duration: calc();
        transition-timing-function: steps();
        transition-delay: calc();

        box-shadow: inset inset 1px 2px 3px red;
        box-shadow: 1px 2px 3px 4px 5px 6px 7px;
        text-shadow: 1px 2px 3px 4px 5px;

        content: counter();
        counter-reset: mycounter;
        counter-increment: mycounter +;

        clip-path: circle();
        shape-outside: polygon();
        offset-path: path();
        offset-distance: calc();

        writing-mode: sideways;
        text-orientation: sideways;
        direction: sideways;

        overflow: scroll scroll;
        overflow-x: ;
        overflow-y: ;

        --empty: ;
        --broken: var(--missing,);
        color: var(--empty);
        background-color: var(--broken);
      }

      .foo {
        background-image: linear-gradient(to right, red,);
        transform: translateX();
      }
      #parent .item { padding: clamp(, , ); }
      #parent > .item { transform: translateX(); }

      #target,,.foo { width: 1px; }
      :not() { width: 2px; }
      :is(#does-not-exist, ) { height: 3px; }

      @media screen and (min-width: ) {
        #target { width: 123px; }
      }
      @media (max-width: 999999999999999999999999px) {
        #target { height: calc(); }
      }
      @supports (display: ) {
        #target { border: 1px solid; }
      }
      @supports selector(:is(#target,)) {
        #target { font-size: calc(); }
      }
      @layer foo {
        #target { color: rgb(); }
        @media not all and (min-width: ) {
          #target { opacity: calc(); }
        }
      }
      @font-face {
        font-family: ;
        src: url();
        font-weight: 100 900 1000;
      }
      @keyframes spin {
        from { transform: rotate(); }
        50% { opacity: calc(); }
        to { transform: translate(10px,); }
      }
      @unknown-at-rule ??? { #target { color: red; } }
    "#,
  )
  .expect("parse stylesheet");

  let media =
    MediaContext::screen(800.0, 600.0).with_color_scheme(ColorScheme::Light);
  let styled = apply_styles_with_media_target_and_imports(
    &dom, &stylesheet, &media, None, None, None, None, None, None,
  );

  let target = find_by_id(&styled, "target").expect("target node");
  assert_eq!(target.styles.font_size, 16.0);
  assert_eq!(target.styles.opacity, 0.5);

  assert!(target.styles.font_size.is_finite());
  assert!(target.styles.opacity.is_finite());
  assert!(target.styles.color.a.is_finite());
  assert!(target.styles.background_color.a.is_finite());
  assert!((0.0..=1.0).contains(&target.styles.opacity));
  assert!((0.0..=1.0).contains(&target.styles.color.a));
  assert!((0.0..=1.0).contains(&target.styles.background_color.a));
}
