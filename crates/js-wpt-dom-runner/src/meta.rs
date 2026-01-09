use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaDirective {
  Script(String),
  TimeoutLong,
  TimeoutShort,
  Unknown(String),
}

#[derive(Debug, Clone)]
pub struct MetaParseResult {
  pub directives: Vec<MetaDirective>,
  pub timeout: Option<Duration>,
  pub scripts: Vec<String>,
}

pub fn parse_leading_meta(source: &str) -> MetaParseResult {
  let mut directives = Vec::new();
  let mut scripts = Vec::new();
  let mut timeout = None;

  for line in source.lines() {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("// META:") {
      break;
    }
    let rest = trimmed.trim_start_matches("// META:").trim();
    if let Some((key, value)) = rest.split_once('=') {
      let key = key.trim();
      let value = value.trim();
      match key {
        "script" => {
          directives.push(MetaDirective::Script(value.to_string()));
          scripts.push(value.to_string());
        }
        "timeout" => match value {
          "long" => {
            directives.push(MetaDirective::TimeoutLong);
            timeout = Some(Duration::from_secs(30));
          }
          "short" => {
            directives.push(MetaDirective::TimeoutShort);
            timeout = Some(Duration::from_secs(5));
          }
          _ => directives.push(MetaDirective::Unknown(rest.to_string())),
        },
        _ => directives.push(MetaDirective::Unknown(rest.to_string())),
      }
    } else {
      directives.push(MetaDirective::Unknown(rest.to_string()));
    }
  }

  MetaParseResult {
    directives,
    timeout,
    scripts,
  }
}
