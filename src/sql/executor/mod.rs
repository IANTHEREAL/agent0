//! SQL execution engine
//!
//! This module contains the main executor and all execution-related submodules.

mod core;
mod cte;
mod database;
mod ddl;
mod default_privileges;
mod dml;
mod extensions;
mod operators;
mod procedure;
mod select;
pub(crate) mod subquery;
mod table_utils;
pub(crate) mod triggers;
mod udt;
mod user_function;

pub use core::*;
