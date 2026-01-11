pub mod dataflow;
pub mod dataflow_edge;
pub mod defs;
pub mod effect;
pub mod find_conds;
pub mod find_loops;
pub mod interference;
pub mod alias;
pub mod liveness;
pub mod loop_info;
pub mod nullability;
pub mod registers;
pub mod single_use_insts;

#[cfg(test)]
mod tests;
