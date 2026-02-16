//! Schema-resolution helpers: identifier normalization, column lookup, object-name parsing.

use crate::types::DataType;
use sqlparser::ast::{Expr, Ident, ObjectName};

pub(super) fn normalize_sql_ident(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_lowercase()
    }
}

pub(super) fn split_object_name_for_catalog(name: &ObjectName) -> Option<(Option<String>, String)> {
    match name.0.len() {
        1 => Some((None, normalize_sql_ident(&name.0[0]))),
        2 => Some((
            Some(normalize_sql_ident(&name.0[0])),
            normalize_sql_ident(&name.0[1]),
        )),
        _ => None,
    }
}

pub(super) fn expr_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(normalize_sql_ident(ident)),
        Expr::CompoundIdentifier(parts) => parts.last().map(normalize_sql_ident),
        _ => None,
    }
}

pub(super) fn expr_referenced_column_type<'a>(
    schema: &'a crate::types::TableSchema,
    expr: &Expr,
) -> Option<&'a DataType> {
    let col_name = expr_column_name(expr)?;
    schema
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&col_name))
        .map(|c| &c.data_type)
}
