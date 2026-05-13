use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::model::{Row, TableSchema};
use tracing::warn;

pub(crate) mod backend;
pub(crate) mod channel_reader;
pub(crate) mod config;
pub(crate) mod decoders;
pub(crate) mod embedded;
pub(crate) mod glob;
pub(crate) mod notify;
pub(crate) mod redis_events;
pub(crate) mod s3;
pub(crate) mod sql_client;
pub(crate) mod stats_worker;
pub(crate) mod streaming;
pub(crate) mod termination_guard;
pub(crate) mod upload_token;
pub(crate) mod ws;

mod directory;
mod file_stream;
mod glob_stream;
mod table_function;

pub(crate) enum Fs9Mode {
    Directory {
        path: String,
        recursive: bool,
        exclude: Option<String>,
    },
    File {
        path: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
    },
    Glob {
        pattern: String,
        format: Option<String>,
        delimiter: Option<char>,
        header: Option<bool>,
        exclude: Option<String>,
    },
}

pub(crate) const MAX_BYTES_PER_FILE: usize = 100 * 1024 * 1024;
pub(crate) const MAX_FILES_PER_GLOB: usize = 10_000;
pub(crate) const MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;

pub(crate) use directory::list_directory_entries;
pub(crate) use file_stream::start_file_stream;
pub(crate) use glob_stream::start_glob_stream;
pub(crate) use table_function::{execute_table_function, infer_table_function_schema};

#[cfg(test)]
pub(crate) use file_stream::start_file_stream_for_test_backend;
#[cfg(test)]
pub(crate) use glob_stream::start_glob_stream_with_budget_for_test_backend;
#[cfg(test)]
pub(crate) use table_function::execute_table_function_with_budget_for_test_backend;

#[cfg(test)]
mod tests;
