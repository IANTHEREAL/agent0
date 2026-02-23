//! Parquet file import extension.
//!
//! Provides `read_parquet()` table function and `COPY FROM ... WITH (FORMAT parquet)` support.
//! Reads Parquet files from HTTP/HTTPS URLs using streaming row-group-at-a-time processing.

pub(crate) mod fs9_reader;
pub(crate) mod http_reader;
pub(crate) mod types;

pub(crate) mod limits;
pub(crate) mod reader;
