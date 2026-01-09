use anyhow::Result;
use clap::{Args, Subcommand};

mod test262;
mod test262_parser;
mod wpt_dom;

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
pub struct JsArgs {
  #[command(subcommand)]
  command: JsCommand,
}

#[derive(Subcommand, Debug)]
enum JsCommand {
  /// Run a curated subset of tc39/test262 language semantics tests.
  Test262(test262::Test262Args),
  /// Run the test262 parser harness (tc39/test262-parser-tests via ecma-rs).
  #[command(name = "test262-parser")]
  Test262Parser(test262_parser::Test262ParserArgs),
  /// Run a curated subset of WPT `testharness.js` DOM/event-loop tests.
  #[command(name = "wpt-dom")]
  WptDom(wpt_dom::WptDomArgs),
}

pub fn run_js(args: JsArgs) -> Result<()> {
  match args.command {
    JsCommand::Test262(args) => test262::run_test262(args),
    JsCommand::Test262Parser(args) => test262_parser::run_test262_parser(args),
    JsCommand::WptDom(args) => wpt_dom::run_wpt_dom(args),
  }
}
