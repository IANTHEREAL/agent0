//! SQL execution engine
//!
//! This module contains the main executor and all execution-related submodules.

mod core;
mod cte;
mod database;
mod ddl;
mod dml;
mod extensions;
mod join;
mod operators;
mod procedure;
mod select;
mod subquery;
pub(crate) mod triggers;
mod udt;

pub use core::*;
