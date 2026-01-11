pub mod dataflow;
pub mod dataflow_edge;
pub mod defs;
pub mod driver;
pub mod effect;
pub mod escape;
pub mod encoding;
pub mod find_conds;
pub mod find_loops;
pub mod interference;
pub mod alias;
pub mod liveness;
pub mod loop_info;
pub mod nullability;
pub mod ownership;
pub mod purity;
pub mod range;
pub mod registers;
pub mod single_use_insts;

pub use driver::{analyze_program, annotate_program, FunctionKey, ProgramAnalyses};

#[cfg(test)]
mod tests;
