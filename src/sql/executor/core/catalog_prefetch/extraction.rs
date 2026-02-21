//! AST visitor extractors: table names, table-valued function calls, and scalar
//! function names from sqlparser AST nodes.

use crate::sql::names;
use crate::sql::table_functions::table_function_key;
use sqlparser::ast::{ObjectName, Query, Statement, TableFactor, Visit, Visitor};
use std::collections::HashSet;
use std::ops::ControlFlow;

/// Extract all table names referenced in a DML statement.
///
/// Uses the sqlparser Visitor to walk the entire statement AST, capturing
/// table references from FROM, USING, WHERE, SET, VALUES, and subqueries.
pub(super) fn extract_dml_table_names(stmt: &Statement) -> HashSet<String> {
    let mut collector = TableNameCollector {
        names: HashSet::new(),
    };
    let _ = stmt.visit(&mut collector);
    collector.names
}

/// Extract all table names from a sqlparser Query AST using the Visitor pattern.
///
/// Returns a deduplicated set of table names as they appear in FROM/JOIN clauses.
/// Names may be bare (`users`) or schema-qualified (`public.users`).
pub(super) fn extract_table_names(query: &Query) -> HashSet<String> {
    let mut collector = TableNameCollector {
        names: HashSet::new(),
    };
    let _ = query.visit(&mut collector);
    collector.names
}

/// Visitor that collects table names from all relation `ObjectName` nodes.
///
/// Uses `pre_visit_relation` instead of `pre_visit_table_factor` because
/// DML target tables (INSERT INTO t, UPDATE t, DELETE FROM t) are stored as
/// bare `ObjectName` in the AST, not wrapped in `TableFactor::Table`.
/// `pre_visit_relation` visits ALL relation references uniformly.
struct TableNameCollector {
    names: HashSet<String>,
}

impl Visitor for TableNameCollector {
    type Break = ();

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<()> {
        let parts: Vec<String> = relation.0.iter().map(names::normalize_ident).collect();
        let full_name = parts.join(".");
        self.names.insert(full_name);
        ControlFlow::Continue(())
    }
}

#[derive(Debug, Clone)]
pub(super) struct TableFunctionCall {
    pub key: String,
    pub name: ObjectName,
    pub name_parts: Vec<String>,
    pub args: Vec<sqlparser::ast::FunctionArg>,
}

/// Extract table-valued function calls from a sqlparser Query AST.
pub(super) fn extract_table_function_calls(query: &Query) -> Vec<TableFunctionCall> {
    let mut collector = TableFunctionCollector { calls: Vec::new() };
    let _ = query.visit(&mut collector);
    collector.calls
}

/// Visitor that collects `TableFactor::Table` nodes that have `args` (i.e. function-in-FROM).
struct TableFunctionCollector {
    calls: Vec<TableFunctionCall>,
}

impl Visitor for TableFunctionCollector {
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = table_factor
        {
            let key = table_function_key(name, args);
            let name_parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();
            self.calls.push(TableFunctionCall {
                key,
                name: name.clone(),
                name_parts,
                args: args.clone(),
            });
        }
        ControlFlow::Continue(())
    }
}

#[derive(Default)]
struct ScalarFunctionCollector {
    names: Vec<ObjectName>,
    seen: HashSet<String>,
}

impl Visitor for ScalarFunctionCollector {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &sqlparser::ast::Expr) -> ControlFlow<()> {
        if let sqlparser::ast::Expr::Function(func) = expr {
            let key = func
                .name
                .0
                .iter()
                .map(names::normalize_ident)
                .collect::<Vec<_>>()
                .join(".");
            if self.seen.insert(key) {
                self.names.push(func.name.clone());
            }
        }
        ControlFlow::Continue(())
    }
}

pub(super) fn extract_scalar_function_names(query: &Query) -> Vec<ObjectName> {
    let mut collector = ScalarFunctionCollector::default();
    let _ = query.visit(&mut collector);
    collector.names
}

pub(super) fn extract_scalar_function_names_from_statement(stmt: &Statement) -> Vec<ObjectName> {
    let mut collector = ScalarFunctionCollector::default();
    let _ = stmt.visit(&mut collector);
    collector.names
}
