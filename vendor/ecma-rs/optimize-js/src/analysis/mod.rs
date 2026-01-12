pub mod call_summary;
pub mod callsite;
pub mod dataflow;
pub mod dataflow_edge;
pub mod defs;
pub mod driver;
pub mod effect;
pub mod escape;
pub mod interproc_escape;
pub mod encoding;
pub mod facts;
pub mod find_conds;
pub mod find_loops;
pub mod interference;
pub mod alias;
pub mod async_elision;
pub mod consume;
pub mod liveness;
pub mod loop_info;
pub mod nullability;
pub mod ownership;
pub mod parallelize;
pub mod purity;
pub mod range;
pub mod registers;
#[cfg(feature = "serde")]
pub(crate) mod serde;
pub mod single_use_insts;
pub mod value_types;

pub use driver::{
  analyze_cfg, analyze_program, analyze_program_function, annotate_escape_and_ownership,
  annotate_program, FunctionAnalyses, FunctionKey, ProgramAnalyses,
};

#[cfg(feature = "typed")]
pub use driver::{analyze_cfg_typed, analyze_program_function_typed, annotate_program_typed};

#[cfg(test)]
mod tests;
