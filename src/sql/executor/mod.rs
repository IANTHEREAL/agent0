//! SQL execution engine
//!
//! This module contains the main executor and all execution-related submodules.

mod core;
mod cte;
mod database;
mod default_privileges;
mod ddl;
mod dml;
mod extensions;
mod operators;
mod table_utils;
mod procedure;
mod select;
pub(crate) mod subquery;
pub(crate) mod triggers;
mod udt;
mod user_function;

pub use core::*;
