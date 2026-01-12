use std::path::PathBuf;

#[test]
fn cargo_toml_has_no_parse_js_dependency() {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let path = manifest_dir.join("Cargo.toml");
  let source =
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
  let cargo: toml::Value = source
    .parse()
    .unwrap_or_else(|e| panic!("parse {} as toml: {e}", path.display()));

  let mut offenders = Vec::<String>::new();

  for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
    if cargo
      .get(section)
      .and_then(|v| v.as_table())
      .is_some_and(|table| table.contains_key("parse-js"))
    {
      offenders.push(format!("[{section}] contains parse-js"));
    }
  }

  if let Some(features) = cargo.get("features").and_then(|v| v.as_table()) {
    for (name, feature_value) in features {
      let Some(list) = feature_value.as_array() else {
        continue;
      };
      for item in list {
        let Some(item) = item.as_str() else {
          continue;
        };
        if item == "parse-js" || item == "dep:parse-js" {
          offenders.push(format!("[features.{name}] includes {item}"));
        }
      }
    }
  }

  assert!(
    offenders.is_empty(),
    "js-wpt-dom-runner should not depend on parse-js directly (vm-js already pulls it in as needed)\n\
offenders:\n  - {}\n\
path: {}",
    offenders.join("\n  - "),
    path.display()
  );
}
