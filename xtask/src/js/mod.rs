use anyhow::Result;
use clap::{Args, Subcommand};

mod test262;

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
}

pub fn run_js(args: JsArgs) -> Result<()> {
  match args.command {
    JsCommand::Test262(args) => test262::run_test262(args),
  }
}
