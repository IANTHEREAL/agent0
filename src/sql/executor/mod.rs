//! SQL execution engine
//!
//! This module contains the main executor and all execution-related submodules.

mod core;
mod cte;
mod database;
mod ddl;
mod default_privileges;
mod dml_analyzed;
mod extensions;
mod procedure;
mod select;
mod table_utils;
pub(crate) mod triggers;
mod udt;
mod user_function;

pub use core::*;
