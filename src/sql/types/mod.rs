//! Type inference system
//!
//! This module provides a modular type inference system with:
//! - `TypeContext`: Multi-table column resolution
//! - `FunctionRegistry`: Function signature lookup
//! - `TypeInferrer`: Core type inference logic

pub(crate) mod cast;
pub(crate) mod coercion;
mod context;
mod error;
mod infer;
pub(crate) mod mapping;
pub(crate) mod registry;

pub use context::TypeContext;
pub use error::TypeError;
pub use infer::TypeInferrer;

pub(crate) use cast::CastContext;
pub(crate) use mapping::{sql_datatype_to_internal, sql_datatype_to_internal_strict};

// Re-exports for tests
#[cfg(test)]
pub(crate) use coercion::{binary_op_result_type, is_numeric, unify_types};
#[cfg(test)]
pub(crate) use registry::global_registry;

#[cfg(test)]
mod tests;
