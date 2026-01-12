use effect_model::{EffectTemplate, PurityTemplate};
use knowledge_base::{ApiDatabase, ApiId, TargetEnv, WebPlatform};
use semver::Version;

#[test]
fn get_by_id_for_target_selects_target_specific_entry() {
  let node_yaml = r#"
- name: x
  effects: Io
  purity: Impure
"#;
  let web_yaml = r#"
- name: x
  effects: Pure
  purity: Pure
"#;

  let kb = ApiDatabase::load_from_sources(&[("node/x.yaml", node_yaml), ("web/x.yaml", web_yaml)])
    .expect("load KB");

  // Simulate downstream tooling storing only the canonical, content-addressed ID.
  let id = ApiId::from_name("x");

  let node_target = TargetEnv::Node {
    version: Version::new(20, 0, 0),
  };
  let web_target = TargetEnv::Web {
    platform: WebPlatform::Generic,
  };

  let node = kb
    .get_by_id_for_target(id, &node_target)
    .expect("Node entry");
  assert_eq!(node.effects, EffectTemplate::Io);
  assert_eq!(node.purity, PurityTemplate::Impure);

  let web = kb
    .get_by_id_for_target(id, &web_target)
    .expect("Web entry");
  assert_eq!(web.effects, EffectTemplate::Pure);
  assert_eq!(web.purity, PurityTemplate::Pure);

  // Existing behavior: `get_by_id` uses `TargetEnv::Unknown` and picks a deterministic entry.
  let unknown = kb.get_by_id(id).expect("Unknown entry");
  assert_eq!(unknown.effects, EffectTemplate::Io);
  assert_eq!(unknown.purity, PurityTemplate::Impure);

  assert_eq!(
    kb.source_for_id_for_target(id, &node_target),
    Some("node/x.yaml")
  );
  assert_eq!(
    kb.source_for_id_for_target(id, &web_target),
    Some("web/x.yaml")
  );
}

