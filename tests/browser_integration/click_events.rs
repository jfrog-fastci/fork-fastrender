use fastrender::{BrowserTab, RenderOptions, Result, VmJsBrowserTabExecutor};

#[test]
fn click_prevent_default_blocks_link_navigation() -> Result<()> {
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
<script>
  var link = document.getElementById("link");
  link.addEventListener("click", function (ev) { ev.preventDefault(); });
</script>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved, None);
  Ok(())
}

#[test]
fn click_default_action_resolves_link_when_not_canceled() -> Result<()> {
  let html = r#"<!doctype html>
<a id="link" href="https://example.com/next">next</a>
"#;

  let executor = VmJsBrowserTabExecutor::new();
  let mut tab = BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

  let link = tab
    .dom()
    .get_element_by_id("link")
    .expect("expected <a id=link> to be present");

  let resolved = tab.resolve_navigation_for_click(link)?;
  assert_eq!(resolved.as_deref(), Some("https://example.com/next"));
  Ok(())
}
