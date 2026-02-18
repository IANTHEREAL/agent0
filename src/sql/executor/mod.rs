//! SQL execution engine
//!
//! This module contains the main executor and all execution-related submodules.

mod bg_sql;
mod core;
mod cron;
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
pub(crate) use bg_sql::{execute_bg_sql_function, is_bg_sql_function, try_execute_bg_sql_function};
pub(crate) use cron::{
    execute_cron_scalar_function, split_cron_scalar_function_name, try_execute_cron_scalar_function,
};
