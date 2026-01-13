use fastrender_ipc::{
  BrowserToRenderer, DiscoveredSubframe, FrameId, IframeNavigation, NavigationContext, Rect,
  SiteKey, SiteKeyFactory, SubframeToken,
};
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RendererProcessId(u64);

#[derive(Debug, Default)]
struct RendererProcessRegistry {
  next_id: u64,
  by_site: HashMap<SiteKey, RendererProcessId>,
  sent: HashMap<RendererProcessId, Vec<BrowserToRenderer>>,
}

impl RendererProcessRegistry {
  fn get_or_spawn(&mut self, site: SiteKey) -> RendererProcessId {
    if let Some(&existing) = self.by_site.get(&site) {
      return existing;
    }
    self.next_id = self.next_id.saturating_add(1);
    let id = RendererProcessId(self.next_id);
    self.by_site.insert(site, id);
    id
  }

  fn send(&mut self, process: RendererProcessId, msg: BrowserToRenderer) {
    self.sent.entry(process).or_default().push(msg);
  }

  fn messages(&self, process: RendererProcessId) -> &[BrowserToRenderer] {
    self
      .sent
      .get(&process)
      .map(|v| v.as_slice())
      .unwrap_or(&[])
  }
}

#[derive(Debug)]
struct FrameNode {
  site: SiteKey,
  process: RendererProcessId,
  subframes: HashMap<SubframeToken, SubframeSlot>,
}

#[derive(Debug, Clone)]
struct SubframeSlot {
  frame_id: FrameId,
  last_navigation: IframeNavigation,
  rect: Rect,
  opaque_origin: bool,
}

#[derive(Debug)]
struct BrowserFrameTree {
  site_keys: SiteKeyFactory,
  processes: RendererProcessRegistry,
  next_frame_id: u64,
  frames: HashMap<FrameId, FrameNode>,
}

impl BrowserFrameTree {
  fn new(site_keys: SiteKeyFactory) -> Self {
    Self {
      site_keys,
      processes: RendererProcessRegistry::default(),
      next_frame_id: 1,
      frames: HashMap::new(),
    }
  }

  fn alloc_frame_id(&mut self) -> FrameId {
    let id = FrameId::new(self.next_frame_id);
    self.next_frame_id = self.next_frame_id.saturating_add(1);
    id
  }

  fn create_root(&mut self, url: &str) -> FrameId {
    let url = normalize_url(url);
    let site = self.site_keys.site_key_for_navigation(&url, None);
    let process = self.processes.get_or_spawn(site.clone());
    let frame_id = self.alloc_frame_id();
    self.frames.insert(
      frame_id,
      FrameNode {
        site: site.clone(),
        process,
        subframes: HashMap::new(),
      },
    );
    self.processes.send(process, BrowserToRenderer::CreateFrame { frame_id });
    self.processes.send(
      process,
      BrowserToRenderer::Navigate {
        frame_id,
        url: url.clone(),
        context: NavigationContext {
          site_key: site,
          ..Default::default()
        },
      },
    );
    frame_id
  }

  fn handle_subframes_discovered(&mut self, parent_frame_id: FrameId, subframes: &[DiscoveredSubframe]) {
    let Some((parent_site, parent_process)) = self.frames.get(&parent_frame_id).map(|p| (p.site.clone(), p.process)) else {
      return;
    };

    for subframe in subframes {
      let token = subframe.token;
      let navigation = normalize_iframe_navigation(&subframe.navigation);
      let target_site = self.site_keys.site_key_for_subframe_navigation(
        navigation.effective_url(),
        Some(&parent_site),
        subframe.opaque_origin,
      );
      let isolate = target_site != parent_site;

      let existing = self
        .frames
        .get(&parent_frame_id)
        .and_then(|p| p.subframes.get(&token))
        .cloned();

      match existing {
        Some(existing) => {
          if existing.last_navigation != navigation {
            self.navigate_existing_subframe(
              parent_process,
              existing.frame_id,
              target_site,
              isolate,
              &navigation,
              subframe,
            );
          }

          let rect_changed = existing.rect != subframe.rect;
          let nav_changed = existing.last_navigation != navigation;
          if rect_changed || nav_changed || existing.opaque_origin != subframe.opaque_origin {
            if let Some(parent) = self.frames.get_mut(&parent_frame_id) {
              if let Some(slot) = parent.subframes.get_mut(&token) {
                slot.last_navigation = navigation.clone();
                slot.rect = subframe.rect;
                slot.opaque_origin = subframe.opaque_origin;
              }
            }
          }
        }
        None => {
          let child_frame_id = self.alloc_frame_id();
          let process = if isolate {
            self.processes.get_or_spawn(target_site.clone())
          } else {
            parent_process
          };
          self.frames.insert(
            child_frame_id,
            FrameNode {
              site: target_site.clone(),
              process,
              subframes: HashMap::new(),
            },
          );
          if let Some(parent) = self.frames.get_mut(&parent_frame_id) {
            parent.subframes.insert(
              token,
              SubframeSlot {
                frame_id: child_frame_id,
                last_navigation: navigation.clone(),
                rect: subframe.rect,
                opaque_origin: subframe.opaque_origin,
              },
            );
          }
          self
            .processes
            .send(process, BrowserToRenderer::CreateFrame { frame_id: child_frame_id });
          self.send_navigate(process, child_frame_id, &navigation, target_site, subframe);
        }
      }
    }
  }

  fn navigate_existing_subframe(
    &mut self,
    parent_process: RendererProcessId,
    child_frame_id: FrameId,
    target_site: SiteKey,
    isolate: bool,
    navigation: &IframeNavigation,
    subframe: &DiscoveredSubframe,
  ) {
    let Some(child_node) = self.frames.get(&child_frame_id) else {
      return;
    };
    let current_process = child_node.process;

    let next_process = if isolate {
      self.processes.get_or_spawn(target_site.clone())
    } else {
      parent_process
    };

    if next_process != current_process {
      // Process swap: destroy + recreate the frame in the new process but keep the FrameId stable.
      self.processes.send(
        current_process,
        BrowserToRenderer::DestroyFrame {
          frame_id: child_frame_id,
        },
      );
      self.processes.send(
        next_process,
        BrowserToRenderer::CreateFrame {
          frame_id: child_frame_id,
        },
      );
    }

    if let Some(child_mut) = self.frames.get_mut(&child_frame_id) {
      child_mut.site = target_site.clone();
      child_mut.process = next_process;
    }

    self.send_navigate(next_process, child_frame_id, navigation, target_site, subframe);
  }

  fn send_navigate(
    &mut self,
    process: RendererProcessId,
    frame_id: FrameId,
    navigation: &IframeNavigation,
    site_key: SiteKey,
    discovered: &DiscoveredSubframe,
  ) {
    let url = navigation.effective_url().to_string();
    self.processes.send(
      process,
      BrowserToRenderer::Navigate {
        frame_id,
        url,
        context: NavigationContext {
          site_key,
          sandbox_flags: discovered.sandbox_flags,
          opaque_origin: discovered.opaque_origin,
          referrer_policy: discovered.referrer_policy.unwrap_or_default(),
          ..Default::default()
        },
      },
    );
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn normalize_url(url: &str) -> String {
  let trimmed = trim_ascii_whitespace(url);
  Url::parse(trimmed)
    .map(|u| u.to_string())
    .unwrap_or_else(|_| trimmed.to_string())
}

fn normalize_iframe_navigation(nav: &IframeNavigation) -> IframeNavigation {
  match nav {
    IframeNavigation::Url(url) => IframeNavigation::Url(normalize_url(url)),
    IframeNavigation::AboutBlank => IframeNavigation::AboutBlank,
    IframeNavigation::Srcdoc { content_hash } => IframeNavigation::Srcdoc {
      content_hash: *content_hash,
    },
  }
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
  Rect {
    x,
    y,
    width,
    height,
  }
}

fn discovered(token: u64, nav: IframeNavigation, rect: Rect) -> DiscoveredSubframe {
  DiscoveredSubframe {
    token: SubframeToken(token),
    navigation: nav,
    rect,
    hit_testable: true,
    referrer_policy: None,
    sandbox_flags: fastrender_ipc::SandboxFlags::NONE,
    opaque_origin: false,
  }
}

#[test]
fn same_token_url_change_triggers_navigate() {
  let mut browser = BrowserFrameTree::new(SiteKeyFactory::new_with_seed(1));
  let parent = browser.create_root("https://parent.test/");
  let parent_process = browser.frames.get(&parent).unwrap().process;

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      1,
      IframeNavigation::Url("https://child.test/a".to_string()),
      rect(0.0, 0.0, 100.0, 100.0),
    )],
  );

  let child_frame_id = browser
    .frames
    .get(&parent)
    .unwrap()
    .subframes
    .get(&SubframeToken(1))
    .unwrap()
    .frame_id;
  let child_process = browser.frames.get(&child_frame_id).unwrap().process;
  assert_ne!(child_process, parent_process);

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      1,
      IframeNavigation::Url("https://child.test/b".to_string()),
      rect(0.0, 0.0, 100.0, 100.0),
    )],
  );

  let msgs = browser.processes.messages(child_process);
  assert!(
    msgs.iter().any(|msg| matches!(
      msg,
      BrowserToRenderer::Navigate { frame_id, url, .. }
        if *frame_id == child_frame_id && url == "https://child.test/b"
    )),
    "expected Navigate to be sent for updated URL; got {msgs:?}"
  );
}

#[test]
fn same_token_rect_change_does_not_trigger_navigate() {
  let mut browser = BrowserFrameTree::new(SiteKeyFactory::new_with_seed(1));
  let parent = browser.create_root("https://parent.test/");

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      1,
      IframeNavigation::Url("https://child.test/a".to_string()),
      rect(0.0, 0.0, 100.0, 100.0),
    )],
  );

  let child_frame_id = browser
    .frames
    .get(&parent)
    .unwrap()
    .subframes
    .get(&SubframeToken(1))
    .unwrap()
    .frame_id;
  let child_process = browser.frames.get(&child_frame_id).unwrap().process;

  let msgs_before = browser.processes.messages(child_process).len();

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      1,
      IframeNavigation::Url("https://child.test/a".to_string()),
      rect(10.0, 10.0, 80.0, 80.0),
    )],
  );

  let msgs_after = browser.processes.messages(child_process);
  assert!(
    msgs_after[msgs_before..]
      .iter()
      .all(|msg| !matches!(msg, BrowserToRenderer::Navigate { .. })),
    "expected no Navigate on geometry-only update; got {msgs_after:?}"
  );

  let stored = browser
    .frames
    .get(&parent)
    .unwrap()
    .subframes
    .get(&SubframeToken(1))
    .unwrap()
    .rect;
  assert_eq!(stored, rect(10.0, 10.0, 80.0, 80.0));
}

#[test]
fn cross_origin_url_change_causes_process_swap() {
  let mut browser = BrowserFrameTree::new(SiteKeyFactory::new_with_seed(1));
  let parent = browser.create_root("https://parent.test/");

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      2,
      IframeNavigation::Url("https://a.test/".to_string()),
      rect(0.0, 0.0, 100.0, 100.0),
    )],
  );

  let child_frame_id = browser
    .frames
    .get(&parent)
    .unwrap()
    .subframes
    .get(&SubframeToken(2))
    .unwrap()
    .frame_id;
  let old_process = browser.frames.get(&child_frame_id).unwrap().process;

  browser.handle_subframes_discovered(
    parent,
    &[discovered(
      2,
      IframeNavigation::Url("https://b.test/".to_string()),
      rect(0.0, 0.0, 100.0, 100.0),
    )],
  );

  let new_process = browser.frames.get(&child_frame_id).unwrap().process;
  assert_ne!(old_process, new_process, "expected process swap on cross-origin navigation");

  let old_msgs = browser.processes.messages(old_process);
  assert!(
    old_msgs.iter().any(|msg| matches!(
      msg,
      BrowserToRenderer::DestroyFrame { frame_id } if *frame_id == child_frame_id
    )),
    "expected DestroyFrame on old process; got {old_msgs:?}"
  );

  let new_msgs = browser.processes.messages(new_process);
  assert!(
    new_msgs.iter().any(|msg| matches!(
      msg,
      BrowserToRenderer::CreateFrame { frame_id } if *frame_id == child_frame_id
    )),
    "expected CreateFrame on new process; got {new_msgs:?}"
  );
  assert!(
    new_msgs.iter().any(|msg| matches!(
      msg,
      BrowserToRenderer::Navigate { frame_id, url, .. }
        if *frame_id == child_frame_id && url == "https://b.test/"
    )),
    "expected Navigate on new process; got {new_msgs:?}"
  );
}
