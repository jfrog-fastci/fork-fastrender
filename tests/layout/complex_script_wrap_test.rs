use fastrender::api::{FastRender, FastRenderConfig};
use fastrender::debug::inspect::{inspect, InspectQuery};
use fastrender::style::media::MediaType;
use fastrender::text::font_db::FontConfig;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn count_line_fragments(fragment: &FragmentNode) -> usize {
  let mut count = usize::from(matches!(fragment.content, FragmentContent::Line { .. }));

  match &fragment.content {
    FragmentContent::RunningAnchor { snapshot, .. } | FragmentContent::FootnoteAnchor { snapshot } => {
      count += count_line_fragments(snapshot.as_ref());
    }
    _ => {}
  }

  for child in fragment.children.iter() {
    count += count_line_fragments(child);
  }

  count
}

#[test]
fn thai_text_wraps_in_narrow_container_without_overflow_wrap_anywhere() {
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let config = FastRenderConfig::new().with_font_sources(FontConfig::bundled_only());
      let mut renderer = FastRender::with_config(config).expect("renderer");

      let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; font-size: 16px; line-height: 20px; font-family: "Noto Sans Thai"; }
      #target { width: 60px; margin: 0; padding: 0; border: 0; white-space: normal; }
    </style>
  </head>
  <body>
    <div id="target">ภาษาไทยทดสอบการตัดคำ</div>
  </body>
</html>"#;

      let dom = renderer.parse_html(html).expect("parse html");
      let intermediates = renderer
        .layout_document_for_media_intermediates(&dom, 200, 200, MediaType::Screen)
        .expect("layout intermediates");

      let inspected = inspect(
        &intermediates.dom,
        &intermediates.styled_tree,
        &intermediates.box_tree.root,
        &intermediates.fragment_tree,
        InspectQuery::Id("target".to_string()),
      )
      .expect("inspect target element");
      assert_eq!(inspected.len(), 1, "expected exactly one #target match");

      let target_box_ids: Vec<usize> = inspected[0].boxes.iter().map(|b| b.box_id).collect();
      assert!(
        !target_box_ids.is_empty(),
        "expected inspection to return at least one box id for #target"
      );

      let mut line_count = 0usize;
      for fragment in intermediates.fragment_tree.iter_fragments() {
        if fragment.box_id().is_some_and(|id| target_box_ids.contains(&id)) {
          line_count += count_line_fragments(fragment);
        }
      }

      assert!(
        line_count >= 2,
        "expected Thai text to wrap into multiple line fragments without overflow-wrap:anywhere; got {line_count} line fragments"
      );
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}

