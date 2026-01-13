use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::multiprocess::{
  ProcessHandle, ProcessSpawner, RendererProcessId, RendererProcessRegistry,
};
use fastrender::site_isolation::{SiteKey, SiteKeyFactory};
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, IframeSandboxAttribute, ReplacedType};

#[derive(Debug)]
struct FakeHandle {
  id: RendererProcessId,
}

impl ProcessHandle for FakeHandle {
  fn id(&self) -> RendererProcessId {
    self.id
  }

  fn terminate(&mut self) {}
}

#[derive(Debug)]
struct FakeSpawner {
  next_id: u64,
}

impl ProcessSpawner for FakeSpawner {
  type Handle = FakeHandle;

  fn spawn(&mut self, _site: &SiteKey) -> Self::Handle {
    let id = RendererProcessId::new(self.next_id);
    self.next_id += 1;
    FakeHandle { id }
  }
}

fn collect_iframes<'a>(node: &'a BoxNode, out: &mut Vec<&'a ReplacedType>) {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::Iframe { .. } = &replaced.replaced_type {
      out.push(&replaced.replaced_type);
    }
  }
  if let Some(body) = node.footnote_body.as_deref() {
    collect_iframes(body, out);
  }
  for child in &node.children {
    collect_iframes(child, out);
  }
}

#[test]
fn sandboxed_srcdoc_iframe_forces_opaque_site_key_and_separate_process() {
  // `srcdoc` would normally inherit the parent origin. Sandbox without `allow-same-origin`
  // forces a unique opaque origin, so site isolation must not reuse the parent process.
  let html = r#"<!doctype html>
<html>
  <body>
    <iframe srcdoc='<p>inherit</p>'></iframe>
    <iframe sandbox srcdoc='<p>opaque 1</p>'></iframe>
    <iframe sandbox srcdoc='<p>opaque 2</p>'></iframe>
    <iframe sandbox='allow-same-origin' srcdoc='<p>inherit again</p>'></iframe>
  </body>
</html>"#;

  let dom = dom::parse_html(html).expect("parse html");
  let styled = fastrender::style::cascade::apply_styles(&dom, &StyleSheet::new());
  let tree = generate_box_tree(&styled).expect("box tree");

  let mut iframes = Vec::new();
  collect_iframes(&tree.root, &mut iframes);
  assert_eq!(iframes.len(), 4, "expected four iframe replaced boxes");

  let factory = SiteKeyFactory::new_with_seed(1);
  let root_site = factory.site_key_for_navigation("https://a.test/", None, false);

  let mut registry = RendererProcessRegistry::new(FakeSpawner { next_id: 1 });
  let root_pid = registry.get_or_spawn(root_site.clone());

  let mut same_origin_pids = Vec::new();
  let mut opaque_pids = Vec::new();

  for iframe in iframes {
    let ReplacedType::Iframe {
      src,
      srcdoc,
      sandbox,
      ..
    } = iframe
    else {
      continue;
    };

    let sandbox = *sandbox;
    let nav_url = if srcdoc.is_some() { "about:srcdoc" } else { src.as_str() };
    let site = factory.site_key_for_navigation(nav_url, Some(&root_site), sandbox.opaque_origin());
    let pid = registry.get_or_spawn(site);

    match sandbox {
      IframeSandboxAttribute::OpaqueOrigin => opaque_pids.push(pid),
      IframeSandboxAttribute::None | IframeSandboxAttribute::AllowSameOrigin => {
        same_origin_pids.push(pid)
      }
    }
  }

  assert!(!same_origin_pids.is_empty());
  assert_eq!(opaque_pids.len(), 2);

  for pid in same_origin_pids {
    assert_eq!(
      pid, root_pid,
      "expected srcdoc iframes that keep origin to share the parent process"
    );
  }

  assert_ne!(opaque_pids[0], root_pid);
  assert_ne!(opaque_pids[1], root_pid);
  assert_ne!(
    opaque_pids[0], opaque_pids[1],
    "expected sandboxed opaque-origin iframes to get unique processes"
  );

  assert_eq!(
    registry.process_count(),
    3,
    "expected root + 2 sandboxed opaque-origin iframe processes"
  );
}
