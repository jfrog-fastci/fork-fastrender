use clap::{Parser, Subcommand};
use diagnostics::FileId;
use effect_js::{
  detect_signals, recognize_patterns_best_effort_untyped, resolve_api_call,
  resolve_api_call_best_effort_untyped, ApiId, RecognizedPattern, SemanticSignal,
};
use hir_js::{ExprId, ExprKind, FileKind, ObjectKey};
use knowledge_base::{ApiKind, KnowledgeBase, TargetEnv, WebPlatform};
use parse_js::{parse_with_options, ParseOptions};
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Cli {
  /// Load the knowledge base from an on-disk `knowledge-base/` directory instead of the embedded
  /// bundle.
  ///
  /// This is useful when iterating on YAML/TOML files without recompiling the crate.
  #[arg(long, global = true, value_name = "DIR")]
  kb_dir: Option<PathBuf>,

  /// Target Node.js version to use when selecting versioned KB entries.
  ///
  /// Accepts either `vMAJOR.MINOR.PATCH` or a lenient form like `20` / `20.3` / `v20`.
  #[arg(long, global = true, value_name = "VERSION", conflicts_with = "web_platform")]
  node_version: Option<String>,

  /// Target web platform to use when selecting platform-specific KB entries.
  #[arg(long, global = true, value_name = "PLATFORM", value_enum, conflicts_with = "node_version")]
  web_platform: Option<WebPlatformArg>,

  #[command(subcommand)]
  command: Command,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum WebPlatformArg {
  Generic,
  Chrome,
  Firefox,
  Safari,
}

impl From<WebPlatformArg> for WebPlatform {
  fn from(value: WebPlatformArg) -> Self {
    match value {
      WebPlatformArg::Generic => WebPlatform::Generic,
      WebPlatformArg::Chrome => WebPlatform::Chrome,
      WebPlatformArg::Firefox => WebPlatform::Firefox,
      WebPlatformArg::Safari => WebPlatform::Safari,
    }
  }
}

#[derive(Subcommand, Debug)]
enum Command {
  /// Inspect the semantic knowledge base.
  Kb {
    #[command(subcommand)]
    command: KbCommand,
  },
  /// Parse and analyze a single source file.
  Analyze {
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// Parse as TypeScript (default).
    #[arg(long, conflicts_with = "tsx")]
    ts: bool,

    /// Parse as TSX.
    #[arg(long)]
    tsx: bool,

    /// Print semantic signals (`Promise.all`, `async` bodies without await, etc.).
    #[arg(long)]
    signals: bool,
  },
}

#[derive(Subcommand, Debug)]
enum KbCommand {
  /// Print canonical API names (sorted).
  List,
  /// Show a single entry by canonical name or alias.
  Show {
    #[arg(value_name = "NAME")]
    name: String,
  },
}

fn main() {
  let Cli {
    kb_dir,
    node_version,
    web_platform,
    command,
  } = Cli::parse();
  let kb_dir = kb_dir.as_deref();
  let target = parse_target_env(node_version.as_deref(), web_platform);

  match command {
    Command::Kb { command } => run_kb(kb_dir, &target, command),
    Command::Analyze {
      file,
      ts,
      tsx,
      signals,
    } => run_analyze(kb_dir, &target, file, ts, tsx, signals),
  }
}

fn run_kb(kb_dir: Option<&Path>, target: &TargetEnv, command: KbCommand) {
  let kb = load_kb_or_exit(kb_dir);
  match command {
    KbCommand::List => {
      for (name, _) in kb.iter() {
        println!("{name}");
      }
    }
    KbCommand::Show { name } => {
      let Some(entry) = kb.api_for_target(&name, target) else {
        eprintln!("unknown API entry: {name}");
        exit(2);
      };

      println!("name: {}", entry.name);
      println!("id: 0x{:x}", entry.id.raw());
      if let Some(src) = kb.source_for_target(&name, target) {
        println!("source: {src}");
      }
      if !entry.aliases.is_empty() {
        println!("aliases:");
        for alias in entry.aliases.iter() {
          println!("  - {alias}");
        }
      }
      if let Some(semantics) = entry.semantics.as_deref() {
        println!("semantics: {semantics}");
      }
      if let Some(signature) = entry.signature.as_deref() {
        println!("signature: {signature}");
      }
      if let Some(since) = entry.since.as_deref() {
        println!("since: {since}");
      }
      if let Some(until) = entry.until.as_deref() {
        println!("until: {until}");
      }
      if entry.kind != ApiKind::Function {
        let kind = match entry.kind {
          ApiKind::Function => "function",
          ApiKind::Constructor => "constructor",
          ApiKind::Getter => "getter",
          ApiKind::Setter => "setter",
          ApiKind::Value => "value",
        };
        println!("kind: {kind}");
      }
      if let Some(async_) = entry.async_ {
        println!("async: {async_}");
      }
      if let Some(idempotent) = entry.idempotent {
        println!("idempotent: {idempotent}");
      }
      if let Some(deterministic) = entry.deterministic {
        println!("deterministic: {deterministic}");
      }
      if let Some(parallelizable) = entry.parallelizable {
        println!("parallelizable: {parallelizable}");
      }
      println!("effects: {:?}", entry.effects);
      println!("effect_summary: {:?}", entry.effect_summary);
      println!("purity: {:?}", entry.purity);
      if !entry.properties.is_empty() {
        println!("properties:");
        for (k, v) in entry.properties.iter() {
          if let Some(s) = v.as_str() {
            println!("  {k}: {s}");
          } else {
            println!("  {k}: {v}");
          }
        }
      }
    }
  }
}

fn run_analyze(
  kb_dir: Option<&Path>,
  target: &TargetEnv,
  file: PathBuf,
  ts: bool,
  tsx: bool,
  signals: bool,
) {
  let source = fs::read_to_string(&file).unwrap_or_else(|err| {
    eprintln!("failed to read {}: {err}", file.display());
    std::process::exit(2);
  });

  let dialect = if tsx {
    parse_js::Dialect::Tsx
  } else if ts {
    parse_js::Dialect::Ts
  } else {
    // Default.
    parse_js::Dialect::Ts
  };

  let parsed = parse_with_options(
    &source,
    ParseOptions {
      dialect,
      source_type: parse_js::SourceType::Module,
    },
  )
  .unwrap_or_else(|err| {
    eprintln!("parse error: {err}");
    std::process::exit(1);
  });

  let file_kind = match dialect {
    parse_js::Dialect::Tsx => FileKind::Tsx,
    _ => FileKind::Ts,
  };

  let lowered = hir_js::lower_file(FileId(0), file_kind, &parsed);
  let kb = load_kb_or_exit(kb_dir);

  let kb_calls = resolve_calls_kb(&kb, &lowered);
  println!("== API resolution (knowledge-base) ==");
  if kb_calls.is_empty() {
    println!("(no resolved calls)");
  } else {
    for call in kb_calls {
      let Some(entry) = kb.api_for_target(&call.api, target) else {
        println!(
          "[{}..{}] {} => {}",
          call.span.start, call.span.end, call.call_text, call.api
        );
        continue;
      };
      let source = kb.source_for_target(&call.api, target).unwrap_or("<unknown>");
      println!(
          "[{}..{}] {} => {} (source={source}, effects={:?}, purity={:?})",
          call.span.start,
          call.span.end,
          call.call_text,
          call.api,
          entry.effects,
          entry.purity,
      );
    }
  }

  let builtin_calls = resolve_calls_best_effort_builtin(&lowered);
  println!();
  println!("== API resolution (best-effort builtins) ==");
  if builtin_calls.is_empty() {
    println!("(no resolved calls)");
  } else {
    for call in builtin_calls {
      // Show KB details when available; otherwise fall back to the stable ApiId.
      if let Some(entry_any) = kb.get_by_id(call.api) {
        let entry = kb.api_for_target(&entry_any.name, target).unwrap_or(entry_any);
        let source = kb.source_for_target(&entry_any.name, target).unwrap_or("<unknown>");
        println!(
          "[{}..{}] {} => {} (source={source}, effects={:?}, purity={:?})",
          call.span.start,
          call.span.end,
          call.call_text,
          entry.name,
          entry.effects,
          entry.purity,
        );
      } else {
        println!(
          "[{}..{}] {} => id: 0x{:x}",
          call.span.start,
          call.span.end,
          call.call_text,
          call.api.raw()
        );
      }
    }
  }

  let patterns = recognize_patterns_best_effort(&lowered);
  println!();
  println!("== Patterns ==");
  if patterns.is_empty() {
    println!("(no patterns)");
  } else {
    for pattern in patterns {
      println!("{}", pattern);
    }
  }

  if signals {
    let signals = detect_signals_best_effort(&lowered);
    println!();
    println!("== Semantic Signals ==");
    if signals.is_empty() {
      println!("(no semantic signals)");
    } else {
      for signal in signals {
        println!("{signal}");
      }
    }
  }
}

fn load_kb_or_exit(kb_dir: Option<&Path>) -> KnowledgeBase {
  let kb = match kb_dir {
    Some(dir) => KnowledgeBase::load_from_dir(dir).unwrap_or_else(|err| {
      eprintln!("failed to load knowledge base from {}: {err}", dir.display());
      exit(1);
    }),
    None => KnowledgeBase::load_default().unwrap_or_else(|err| {
      eprintln!("failed to load default knowledge base: {err}");
      exit(1);
    }),
  };
  kb.validate().unwrap_or_else(|err| {
    eprintln!("invalid knowledge base: {err}");
    exit(1);
  });
  kb
}

fn parse_target_env(node_version: Option<&str>, web_platform: Option<WebPlatformArg>) -> TargetEnv {
  if let Some(raw) = node_version {
    let Some(version) = parse_lenient_version(raw) else {
      eprintln!("invalid --node-version: {raw}");
      exit(2);
    };
    return TargetEnv::Node { version };
  }

  if let Some(platform) = web_platform {
    return TargetEnv::Web {
      platform: platform.into(),
    };
  }

  TargetEnv::Unknown
}

fn parse_lenient_version(raw: &str) -> Option<Version> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let raw = raw.strip_prefix('v').unwrap_or(raw);

  if let Ok(v) = Version::parse(raw) {
    return Some(v);
  }

  let mut it = raw.split('.');
  let major_str = it.next()?;
  let minor_str = it.next();
  let patch_str = it.next();
  if it.next().is_some() {
    return None;
  }

  let major = major_str.parse::<u64>().ok()?;
  let minor = minor_str.map(|s| s.parse::<u64>().ok()).unwrap_or(Some(0))?;
  let patch = patch_str.map(|s| s.parse::<u64>().ok()).unwrap_or(Some(0))?;
  Some(Version::new(major, minor, patch))
}

#[derive(Debug, Clone)]
struct ResolvedCall {
  span: diagnostics::TextRange,
  call_text: String,
  api: ApiId,
}

#[derive(Debug, Clone)]
struct KbResolvedCall {
  span: diagnostics::TextRange,
  call_text: String,
  api: String,
}

fn resolve_calls_kb(kb: &KnowledgeBase, lowered: &hir_js::LowerResult) -> Vec<KbResolvedCall> {
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Call(_)) {
        continue;
      }
      let expr_id = ExprId(idx as u32);
      let Some(api) = resolve_api_call(kb, lowered, *body_id, expr_id) else {
        continue;
      };
      out.push(KbResolvedCall {
        span: expr.span,
        call_text: format_expr_path(body, &lowered.names, expr_id, 8),
        api: api.to_string(),
      });
    }
  }

  out.sort_by(|a, b| {
    (a.span.start, a.span.end, a.api.as_str(), a.call_text.as_str()).cmp(&(
      b.span.start,
      b.span.end,
      b.api.as_str(),
      b.call_text.as_str(),
    ))
  });
  out
}

fn resolve_calls_best_effort_builtin(lowered: &hir_js::LowerResult) -> Vec<ResolvedCall> {
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };
    for (idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Call(_)) {
        continue;
      }
      let expr_id = ExprId(idx as u32);
      let Some(api) = resolve_api_call_best_effort_untyped(lowered, *body_id, expr_id) else {
        continue;
      };
      let call_text = format_expr_path(body, &lowered.names, expr_id, 8);
      out.push(ResolvedCall {
        span: expr.span,
        call_text,
        api,
      });
    }
  }
  out.sort_by(|a, b| {
    (a.span.start, a.span.end, a.api.raw(), a.call_text.as_str()).cmp(&(
      b.span.start,
      b.span.end,
      b.api.raw(),
      b.call_text.as_str(),
    ))
  });
  out
}

fn recognize_patterns_best_effort(lowered: &hir_js::LowerResult) -> Vec<PatternLine> {
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };
    for pat in recognize_patterns_best_effort_untyped(lowered, *body_id) {
      match pat {
        RecognizedPattern::CanonicalCall { .. } => {}
        RecognizedPattern::MapFilterReduce {
          base,
          map_call,
          filter_call,
          reduce_call,
        } => {
          let span = diagnostics::TextRange::new(
            body.exprs[base.0 as usize].span.start,
            body.exprs[reduce_call.0 as usize].span.end,
          );
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] MapFilterReduce: base={} map={} filter={} reduce={}",
              span.start, span.end, base.0, map_call.0, filter_call.0, reduce_call.0
            ),
          });
        }
        RecognizedPattern::ArrayChain { base, ops, terminal } => {
          let span = body.exprs[base.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] ArrayChain: base={} ops={} terminal={terminal:?}",
              span.start,
              span.end,
              base.0,
              ops.len(),
            ),
          });
        }
        RecognizedPattern::PromiseAllFetch {
          promise_all_call,
          fetch_call_count,
          ..
        } => {
          let span = body.exprs[promise_all_call.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] PromiseAllFetch: Promise.all + fetch (fetch_calls={fetch_call_count})",
              span.start,
              span.end,
            ),
          });
        }
        RecognizedPattern::AsyncIterator { stmt, .. } => {
          let span = body.stmts[stmt.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] AsyncIterator: for await (... of ...)",
              span.start, span.end,
            ),
          });
        }
        RecognizedPattern::StringTemplate { expr, span_count } => {
          let span = body.exprs[expr.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] StringTemplate: spans={span_count}",
              span.start, span.end,
            ),
          });
        }
        RecognizedPattern::ObjectSpread {
          expr,
          spread_count,
        } => {
          let span = body.exprs[expr.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] ObjectSpread: spreads={spread_count}",
              span.start, span.end,
            ),
          });
        }
        RecognizedPattern::ArrayDestructure { stmt, arity, .. } => {
          let span = body.stmts[stmt.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] ArrayDestructure: arity={arity}",
              span.start, span.end,
            ),
          });
        }
        RecognizedPattern::GuardClause { stmt, kind, .. } => {
          let span = body.stmts[stmt.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] GuardClause: {:?}",
              span.start, span.end, kind,
            ),
          });
        }
        RecognizedPattern::MapGetOrDefault {
          conditional,
          map,
          key,
          default,
        } => {
          let span = body.exprs[conditional.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] MapGetOrDefault: conditional={} map={} key={} default={}",
              span.start, span.end, conditional.0, map.0, key.0, default.0
            ),
          });
        }
        RecognizedPattern::JsonParseTyped { call, target } => {
          let span = body.exprs[call.0 as usize].span;
          out.push(PatternLine {
            span,
            text: format!(
              "[{}..{}] JsonParseTyped: call={} target_type={}",
              span.start, span.end, call.0, target.0
            ),
          });
        }
      }
    }
  }
  out.sort_by(|a, b| (a.span.start, a.span.end, a.text.as_str()).cmp(&(b.span.start, b.span.end, b.text.as_str())));
  out
}

fn detect_signals_best_effort(lowered: &hir_js::LowerResult) -> Vec<SignalLine> {
  let mut out = Vec::new();

  for (body_id, _) in lowered.body_index.iter() {
    let Some(body) = lowered.body(*body_id) else {
      continue;
    };

    for signal in detect_signals(&lowered.hir, body, &lowered.names) {
      let span = semantic_signal_span(&signal, &lowered.hir, body);
      out.push(SignalLine {
        span,
        text: format!("[{}..{}] {signal:?}", span.start, span.end),
      });
    }
  }

  out.sort_by(|a, b| {
    (a.span.start, a.span.end, a.text.as_str()).cmp(&(b.span.start, b.span.end, b.text.as_str()))
  });
  out
}

fn semantic_signal_span(
  signal: &SemanticSignal,
  file: &hir_js::HirFile,
  body: &hir_js::Body,
) -> diagnostics::TextRange {
  match *signal {
    SemanticSignal::PromiseAll { expr }
    | SemanticSignal::AsConstAssertion { expr }
    | SemanticSignal::TypeAssertion { expr }
    | SemanticSignal::NonNullAssertion { expr }
    | SemanticSignal::PrivateFieldAccess { expr } => body.exprs[expr.0 as usize].span,
    SemanticSignal::ConstBinding { stmt, .. } | SemanticSignal::ForAwaitOf { stmt } => {
      body.stmts[stmt.0 as usize].span
    }
    SemanticSignal::AsyncFunctionWithoutAwait { body: body_id, .. } => file
      .span_map
      .body_span(body_id)
      .unwrap_or(body.span),
    SemanticSignal::ReadonlyTypePosition { type_expr } => file
      .span_map
      .type_expr_span(body.owner, type_expr)
      .unwrap_or(body.span),
  }
}

#[derive(Debug, Clone)]
struct PatternLine {
  span: diagnostics::TextRange,
  text: String,
}

impl std::fmt::Display for PatternLine {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.text)
  }
}

#[derive(Debug, Clone)]
struct SignalLine {
  span: diagnostics::TextRange,
  text: String,
}

impl std::fmt::Display for SignalLine {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.text)
  }
}

fn format_expr_path(
  body: &hir_js::Body,
  names: &hir_js::NameInterner,
  expr: ExprId,
  budget: usize,
) -> String {
  if budget == 0 {
    return "…".to_string();
  }
  let Some(expr) = body.exprs.get(expr.0 as usize) else {
    return "<missing>".to_string();
  };
  match &expr.kind {
    ExprKind::Ident(id) => names.resolve(*id).unwrap_or("<unknown>").to_string(),
    ExprKind::This => "this".to_string(),
    ExprKind::Super => "super".to_string(),
    ExprKind::Member(member) => {
      let object = format_expr_path(body, names, member.object, budget.saturating_sub(1));
      let prop = match &member.property {
        ObjectKey::Ident(id) => names.resolve(*id).unwrap_or("<unknown>"),
        ObjectKey::String(s) => s.as_str(),
        ObjectKey::Number(n) => n.as_str(),
        ObjectKey::Computed(_) => "[computed]",
      };
      format!("{object}.{prop}")
    }
    ExprKind::Call(call) => {
      let callee = format_expr_path(body, names, call.callee, budget.saturating_sub(1));
      format!("{callee}(...)")
    }
    _ => "<expr>".to_string(),
  }
}
