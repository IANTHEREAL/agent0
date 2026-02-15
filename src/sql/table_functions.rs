use sqlparser::ast::{FunctionArg, FunctionArgExpr, ObjectName};

use crate::sql::names;

/// Build a stable signature key for a table-valued function call in FROM.
///
/// The Analyzer is synchronous, so any dynamic table-function schemas must be
/// pre-fetched asynchronously. This key is used as the lookup handle between
/// the prefetch phase and the Analyzer.
///
/// Notes:
/// - Identifiers are normalized with `names::normalize_ident` (PostgreSQL-like
///   case-folding for unquoted idents).
/// - Expression strings are trimmed and concatenated without extra whitespace
///   to keep the key stable.
pub(crate) fn table_function_key(name: &ObjectName, args: &[FunctionArg]) -> String {
    let parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();
    let full_name = parts.join(".");

    let mut rendered_args = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                rendered_args.push(format!("{}", e).trim().to_string());
            }
            FunctionArg::Named { name, arg, .. } => {
                let param = names::normalize_ident(name);
                let expr = match arg {
                    FunctionArgExpr::Expr(e) => format!("{}", e).trim().to_string(),
                    other => format!("{}", other).trim().to_string(),
                };
                rendered_args.push(format!("{param}=>{expr}"));
            }
            other => {
                rendered_args.push(format!("{}", other).trim().to_string());
            }
        }
    }

    format!("{}({})", full_name, rendered_args.join(","))
}
