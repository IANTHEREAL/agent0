//! Type inference system
//!
//! This module provides a modular type inference system with:
//! - `TypeContext`: Multi-table column resolution
//! - `FunctionRegistry`: Function signature lookup
//! - `TypeInferrer`: Core type inference logic
//! - Compatibility API: Drop-in replacement for old `infer_expr_type`

pub(crate) mod cast;
mod coercion;
mod context;
mod error;
mod infer;
mod mapping;
mod registry;

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

use crate::types::{DataType, TableSchema};
use sqlparser::ast::Expr;

pub fn infer_expr_type(expr: &Expr, schema: &TableSchema) -> Result<DataType, TypeError> {
    let ctx = TypeContext::single(schema);
    let mut inferrer = TypeInferrer::new(ctx);
    inferrer.infer(expr)
}

#[cfg(test)]
mod tests;
