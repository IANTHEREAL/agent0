//! Row Level Security (RLS) — core module.
//!
//! Provides:
//! - `RlsPolicy` / `RlsCommand`: policy data model (in `crate::model`)
//! - Policy combination: permissive OR + restrictive AND (PG semantics)
//! - Bypass logic: superuser, table owner (unless FORCE)
//! - Post-Analyzer predicate injection into `AnalyzedQuery` (SELECT path)
//! - DML enforcement: INSERT WITH CHECK, UPDATE USING + WITH CHECK,
//!   DELETE USING, RETURNING error 42501
//!
//! Design: see db9-server#1810.

pub(crate) mod dml;
pub(crate) mod inject;
mod policy;

#[cfg(test)]
mod tests;

pub(crate) use inject::inject_rls_predicates;
pub(crate) use policy::should_bypass_rls;
