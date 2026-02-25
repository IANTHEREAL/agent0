use std::collections::HashSet;
use std::ops::ControlFlow;

use anyhow::Result;
use sqlparser::ast::{
    visit_expressions_mut, BinaryOperator, DataType as SqlDataType, Expr as AstExpr, ObjectName,
};

use super::rename::{parse_sql_expr, parse_stored_query};
use crate::model::{DataType, TableSchema};
use crate::sql::names::normalize_ident;

pub(super) fn datatype_has_unqualified_type_name(
    data_type: &SqlDataType,
    target_name: &str,
) -> bool {
    match data_type {
        SqlDataType::Custom(type_name, _) => {
            type_name.0.len() == 1 && normalize_ident(&type_name.0[0]) == target_name
        }
        SqlDataType::Array(elem) => match elem {
            sqlparser::ast::ArrayElemTypeDef::None => false,
            sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)
            | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner) => {
                datatype_has_unqualified_type_name(inner, target_name)
            }
        },
        SqlDataType::Struct(fields) => fields
            .iter()
            .any(|f| datatype_has_unqualified_type_name(&f.field_type, target_name)),
        _ => false,
    }
}

pub(super) fn expr_has_unqualified_type_cast(expr_sql: &str, target_name: &str) -> Result<bool> {
    let mut expr = parse_sql_expr(expr_sql)?;
    let mut found = false;
    let _ = visit_expressions_mut(&mut expr, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if datatype_has_unqualified_type_name(data_type, target_name) {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    Ok(found)
}

pub(super) fn query_has_unqualified_type_cast(query_sql: &str, target_name: &str) -> Result<bool> {
    let mut query = parse_stored_query(query_sql)?;
    let mut found = false;
    let _ = visit_expressions_mut(&mut query, |e| {
        match e {
            AstExpr::Cast { data_type, .. }
            | AstExpr::TryCast { data_type, .. }
            | AstExpr::SafeCast { data_type, .. }
            | AstExpr::TypedString { data_type, .. } => {
                if datatype_has_unqualified_type_name(data_type, target_name) {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    Ok(found)
}

pub(super) fn enum_column_names(schema: &TableSchema, enum_full_name: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for col in &schema.columns {
        if matches!(&col.data_type, DataType::UserDefined(t) if t == enum_full_name) {
            out.insert(col.name.clone());
            out.insert(col.name.rsplit('.').next().unwrap_or(&col.name).to_string());
        }
    }
    out
}

pub(super) fn expr_is_target_enum_context(
    expr: &AstExpr,
    enum_columns: &HashSet<String>,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Identifier(ident) => enum_columns.contains(&normalize_ident(ident)),
        AstExpr::CompoundIdentifier(parts) => parts
            .last()
            .map(normalize_ident)
            .map(|n| enum_columns.contains(&n))
            .unwrap_or(false),
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_context(
                inner,
                enum_columns,
                enum_full_name,
                allow_unqualified_type_match,
            )
        }
        _ => expr_is_target_enum_cast_context(expr, enum_full_name, allow_unqualified_type_match),
    }
}

pub(super) fn expr_is_target_enum_cast_context(
    expr: &AstExpr,
    enum_full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match expr {
        AstExpr::Cast { data_type, .. }
        | AstExpr::TryCast { data_type, .. }
        | AstExpr::SafeCast { data_type, .. }
        | AstExpr::TypedString { data_type, .. } => {
            data_type_matches_target(data_type, enum_full_name, allow_unqualified_type_match)
        }
        AstExpr::Nested(inner) | AstExpr::Collate { expr: inner, .. } => {
            expr_is_target_enum_cast_context(inner, enum_full_name, allow_unqualified_type_match)
        }
        _ => false,
    }
}

pub(super) fn is_comparison_op(op: &BinaryOperator) -> bool {
    matches!(
        op,
        &BinaryOperator::Eq
            | &BinaryOperator::NotEq
            | &BinaryOperator::Lt
            | &BinaryOperator::LtEq
            | &BinaryOperator::Gt
            | &BinaryOperator::GtEq
    )
}

pub(super) fn data_type_matches_target(
    data_type: &SqlDataType,
    full_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    let Ok((schema, name)) = crate::sql::names::parse_full_name(full_name) else {
        return false;
    };
    match data_type {
        SqlDataType::Custom(type_name, _) => {
            object_name_matches_target(type_name, &schema, &name, allow_unqualified_type_match)
        }
        _ => false,
    }
}

pub(super) fn object_name_matches_target(
    type_name: &ObjectName,
    target_schema: &str,
    target_name: &str,
    allow_unqualified_type_match: bool,
) -> bool {
    match type_name.0.as_slice() {
        [single] => allow_unqualified_type_match && normalize_ident(single) == target_name,
        parts if parts.len() >= 2 => {
            let schema = normalize_ident(&parts[parts.len() - 2]);
            let name = normalize_ident(parts.last().expect("parts.len() >= 2"));
            schema == target_schema && name == target_name
        }
        _ => false,
    }
}
