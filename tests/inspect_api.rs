use fastrender::{FastRender, FastRenderConfig, InspectQuery, ResourceFetcher};
use fastrender::resource::FetchedResource;
use fastrender::style::media::MediaType;
use std::sync::Arc;

#[test]
fn inspect_reports_style_and_layout_for_selector() {
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <html>
          <head>
            <style>
              body { margin: 0; padding: 0; }
              #container { margin: 0; padding: 0; }
              .item { display: block; width: 50px; height: 10px; margin: 0; padding: 0; border: 0 solid transparent; }
            </style>
          </head>
          <body>
            <div id="container">
              <div class="item" id="first"></div>
              <div class="item" id="second"></div>
              <div class="item"></div>
            </div>
          </body>
        </html>
      "#;
      let dom = renderer.parse_html(html).expect("parse");
      let results = renderer
        .inspect(
          &dom,
          200,
          200,
          InspectQuery::Selector("#container .item:nth-child(2)".to_string()),
        )
        .expect("inspection results");

      assert_eq!(results.len(), 1);
      let snapshot = &results[0];
      assert_eq!(snapshot.node.id.as_deref(), Some("second"));
      assert_eq!(snapshot.node.tag_name.as_deref(), Some("div"));
      assert_eq!(snapshot.style.display, "block");
      assert_eq!(snapshot.style.width, Some(50.0));
      assert_eq!(snapshot.style.height, Some(10.0));

      let block_fragment = snapshot
        .fragments
        .iter()
        .find(|f| f.kind == "block")
        .expect("block fragment");
      assert!((block_fragment.bounds.width - 50.0).abs() < f32::EPSILON);
      assert!((block_fragment.bounds.height - 10.0).abs() < f32::EPSILON);
      assert!((block_fragment.bounds.y - 10.0).abs() < f32::EPSILON);
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}

#[test]
fn inspect_selector_respects_document_quirks_mode() {
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");

      // Missing doctype triggers quirks mode; class selectors should match ASCII case-insensitively.
      let quirks_html = "<html><body><div class='item'></div></body></html>";
      let quirks_dom = renderer.parse_html(quirks_html).expect("parse quirks");
      let quirks_results = renderer
        .inspect(
          &quirks_dom,
          100,
          100,
          InspectQuery::Selector(".ITEM".to_string()),
        )
        .expect("quirks inspection");
      assert_eq!(
        quirks_results.len(),
        1,
        "quirks mode should match case-insensitive classes"
      );

      // Standards mode keeps class matching case-sensitive.
      let standards_html = "<!doctype html><html><body><div class='item'></div></body></html>";
      let standards_dom = renderer
        .parse_html(standards_html)
        .expect("parse standards");
      let standards_results = renderer
        .inspect(
          &standards_dom,
          100,
          100,
          InspectQuery::Selector(".ITEM".to_string()),
        )
        .expect("standards inspection");
      assert!(
        standards_results.is_empty(),
        "standards mode should keep class matching case-sensitive"
      );
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}

#[test]
fn inspect_respects_resource_policy_for_stylesheets() {
  #[derive(Clone, Default)]
  struct PanicFetcher;

  impl ResourceFetcher for PanicFetcher {
    fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
      panic!("unexpected fetch for {url}");
    }
  }

  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let fetcher = Arc::new(PanicFetcher) as Arc<dyn ResourceFetcher>;
      let config = FastRenderConfig::new()
        .with_base_url("https://origin.test/page.html")
        .with_same_origin_subresources(true);
      let mut renderer =
        FastRender::with_config_and_fetcher(config, Some(fetcher)).expect("renderer");

      let html = r#"
        <!doctype html>
        <html>
          <head>
            <link rel="stylesheet" href="https://example.com/blocked.css">
          </head>
          <body>
            <div id="target">Inspect</div>
          </body>
        </html>
      "#;
      let dom = renderer.parse_html(html).expect("parse");
      let results = renderer
        .inspect(
          &dom,
          100,
          100,
          InspectQuery::Selector("#target".to_string()),
        )
        .expect("inspect");
      assert_eq!(results.len(), 1);
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}

#[test]
fn inspect_includes_fragments_from_running_element_snapshots() {
  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <!doctype html>
        <html>
          <head>
            <style>
              @page { size: 200px 100px; margin: 0; }
              body { margin: 0; font-size: 10px; line-height: 10px; }
              #run { position: running(header); }
            </style>
          </head>
          <body>
            <div id="run">Run</div>
            <div>Body</div>
          </body>
        </html>
      "#;

      let dom = renderer.parse_html(html).expect("parse");
      let intermediates = renderer
        .layout_document_for_media_intermediates(&dom, 200, 100, MediaType::Print)
        .expect("layout intermediates");

      let results = fastrender::debug::inspect::inspect(
        &intermediates.dom,
        &intermediates.styled_tree,
        &intermediates.box_tree.root,
        &intermediates.fragment_tree,
        InspectQuery::Id("run".to_string()),
      )
      .expect("inspect");
      assert_eq!(results.len(), 1);
      let snapshot = &results[0];
      assert_eq!(snapshot.node.id.as_deref(), Some("run"));
      let box_id = snapshot
        .boxes
        .first()
        .map(|b| b.box_id)
        .expect("expected at least one box snapshot");
      assert!(
        snapshot.fragments.iter().any(|f| f.box_id == Some(box_id)),
        "expected inspect to traverse running-element snapshots so fragments exist for the running element's boxes"
      );
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}
