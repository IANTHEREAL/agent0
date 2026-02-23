//! Type system utilities
//!
//! This module provides:
//! - `FunctionRegistry`: Function signature lookup
//! - Type coercion and cast logic
//! - SQL ↔ internal type mapping

pub(crate) mod cast;
pub(crate) mod coercion;
pub(crate) mod mapping;
pub(crate) mod registry;

pub(crate) use cast::CastContext;
pub(crate) use mapping::sql_datatype_to_internal_strict;

// Re-exports for tests
#[cfg(test)]
pub(crate) use coercion::{binary_op_result_type, is_numeric, unify_types};
#[cfg(test)]
pub(crate) use registry::global_registry;

#[cfg(test)]
mod tests;
