//! The individual passes. Each module's `run` function (or functions) takes a
//! [`walrus::Module`] and returns counts of what it changed. [`crate::Pass`]
//! is usually the easier way to call them.

pub mod addr_cse;
pub mod branches;
pub mod const_hoist;
pub mod dedupe;
pub mod inline;
pub mod locals;
pub mod memory_packing;
pub mod merge_similar;
pub mod tails;
pub mod thunks;
