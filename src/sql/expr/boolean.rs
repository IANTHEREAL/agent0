use anyhow::{anyhow, Result};
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator, Value as SqlValue};

use crate::sql::error::SqlError;
use crate::types::{DataType, TableSchema};

fn unwrap_nested_expr(expr: &Expr) -> &Expr {
    let mut current = expr;
    while let Expr::Nested(inner) = current {
        current = inner;
    }
    current
}

fn string_literal_expr(expr: &Expr) -> Option<&str> {
    match unwrap_nested_expr(expr) {
        Expr::Value(
            SqlValue::SingleQuotedString(s)
            | SqlValue::DoubleQuotedString(s)
            | SqlValue::EscapedStringLiteral(s),
        ) => Some(s.as_str()),
        _ => None,
    }
}

fn resolve_column_type<'a>(schema: &'a TableSchema, expr: &Expr) -> Option<&'a DataType> {
    match expr {
        Expr::Identifier(ident) => schema
            .column_index(&ident.value)
            .and_then(|idx| schema.columns.get(idx))
            .map(|col| &col.data_type),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let table_part = &parts[parts.len() - 2].value;
            let col_name = &parts[parts.len() - 1].value;
            let qualified_name = format!("{}.{}", table_part, col_name);
            schema
                .column_index(&qualified_name)
                .or_else(|| schema.column_index(col_name))
                .and_then(|idx| schema.columns.get(idx))
                .map(|col| &col.data_type)
        }
        _ => None,
    }
}

pub(crate) fn validate_bool_expr_in_boolean_context(
    expr: &Expr,
    schema: &TableSchema,
    err_msg: &'static str,
) -> Result<()> {
    match expr {
        Expr::Nested(inner) => validate_bool_expr_in_boolean_context(inner, schema, err_msg),

        Expr::Value(SqlValue::Boolean(_)) | Expr::Value(SqlValue::Null) => Ok(()),

        Expr::Value(
            SqlValue::SingleQuotedString(s)
            | SqlValue::DoubleQuotedString(s)
            | SqlValue::EscapedStringLiteral(s),
        ) => {
            if super::parse_bool_pg(s).is_some() {
                Ok(())
            } else {
                Err(SqlError::InvalidInputSyntax {
                    type_name: "boolean".into(),
                    value: s.clone(),
                }
                .into())
            }
        }

        Expr::Identifier(ident) => match resolve_column_type(schema, expr) {
            Some(DataType::Boolean) => Ok(()),
            Some(_) => Err(anyhow!(err_msg)),
            None => Err(anyhow!("Column '{}' not found", ident.value)),
        },

        Expr::CompoundIdentifier(parts) => match resolve_column_type(schema, expr) {
            Some(DataType::Boolean) => Ok(()),
            Some(_) => Err(anyhow!(err_msg)),
            None => {
                let col_name = parts.last().map(|p| p.value.as_str()).unwrap_or("");
                Err(anyhow!("Column '{}' not found", col_name))
            }
        },

        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: inner,
        } => validate_bool_expr_in_boolean_context(inner, schema, err_msg),

        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And | BinaryOperator::Or => {
                validate_bool_expr_in_boolean_context(left, schema, err_msg)?;
                validate_bool_expr_in_boolean_context(right, schema, err_msg)
            }

            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
            | BinaryOperator::PGRegexMatch
            | BinaryOperator::PGRegexIMatch
            | BinaryOperator::PGRegexNotMatch
            | BinaryOperator::PGRegexNotIMatch
            | BinaryOperator::PGOverlap => Ok(()),

            // PostgreSQL JSONB existence operator: `jsonb ? text`
            BinaryOperator::Custom(op) if op == "?" => Ok(()),
            BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "?" => Ok(()),
            // PostgreSQL FTS match operator: `tsvector @@ tsquery`
            BinaryOperator::PGCustomBinaryOperator(op) if op.len() == 1 && op[0] == "@@" => Ok(()),

            _ => Err(anyhow!(err_msg)),
        },

        Expr::AnyOp { .. } | Expr::AllOp { .. } => Ok(()),

        Expr::JsonAccess {
            operator, right, ..
        } => {
            use sqlparser::ast::JsonOperator;
            match operator {
                JsonOperator::AtArrow | JsonOperator::ArrowAt | JsonOperator::AtAt => Ok(()),
                // sqlparser-rs precedence quirk: expressions like `col ->> 'k' = 'v'` can be
                // parsed as `JsonAccess(col, ->>, BinaryOp('k', =, 'v'))`. Our evaluator
                // handles this form, so treat it as boolean in WHERE/FILTER contexts.
                JsonOperator::Arrow
                | JsonOperator::LongArrow
                | JsonOperator::HashArrow
                | JsonOperator::HashLongArrow => match right.as_ref() {
                    Expr::InList { .. } => Ok(()),
                    Expr::BinaryOp { op, .. } => match op {
                        BinaryOperator::And
                        | BinaryOperator::Or
                        | BinaryOperator::Eq
                        | BinaryOperator::NotEq
                        | BinaryOperator::Gt
                        | BinaryOperator::Lt
                        | BinaryOperator::GtEq
                        | BinaryOperator::LtEq
                        | BinaryOperator::PGRegexMatch
                        | BinaryOperator::PGRegexIMatch
                        | BinaryOperator::PGRegexNotMatch
                        | BinaryOperator::PGRegexNotIMatch
                        | BinaryOperator::PGOverlap => Ok(()),
                        BinaryOperator::Custom(op) if op == "?" => Ok(()),
                        BinaryOperator::PGCustomBinaryOperator(op)
                            if op.len() == 1 && op[0] == "?" =>
                        {
                            Ok(())
                        }
                        _ => Err(anyhow!(err_msg)),
                    },
                    _ => Err(anyhow!(err_msg)),
                },
                _ => Err(anyhow!(err_msg)),
            }
        }

        Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::SimilarTo { .. }
        | Expr::Between { .. }
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::Exists { .. }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_) => Ok(()),

        Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner) => validate_bool_expr_in_boolean_context(inner, schema, err_msg),

        Expr::Cast { data_type, .. } => match data_type {
            sqlparser::ast::DataType::Boolean => Ok(()),
            _ => Err(anyhow!(err_msg)),
        },

        Expr::TypedString { data_type, .. } => match data_type {
            sqlparser::ast::DataType::Boolean => Ok(()),
            _ => Err(anyhow!(err_msg)),
        },

        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if operand.is_none() {
                for condition in conditions {
                    validate_bool_expr_in_boolean_context(condition, schema, err_msg)?;
                }
            }

            for result in results {
                if string_literal_expr(result).is_some() {
                    return Err(anyhow!(err_msg));
                }
                validate_bool_expr_in_boolean_context(result, schema, err_msg)?;
            }

            if let Some(else_expr) = else_result {
                if string_literal_expr(else_expr).is_some() {
                    return Err(anyhow!(err_msg));
                }
                validate_bool_expr_in_boolean_context(else_expr, schema, err_msg)?;
            }

            Ok(())
        }

        // Allow unknown function return types; runtime evaluation will enforce boolean.
        Expr::Function(_) => Ok(()),

        _ => Err(anyhow!(err_msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ColumnDef;
    use sqlparser::ast::{Ident, JsonOperator};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "b".to_string(),
                    data_type: DataType::Boolean,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "t".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "j".to_string(),
                    data_type: DataType::Jsonb,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    #[test]
    fn test_validate_rejects_text_column_in_boolean_context() {
        let schema = test_schema();
        let expr = Expr::Identifier(Ident::new("t"));

        let err = validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required")
            .unwrap_err()
            .to_string();
        assert_eq!(err, "boolean required");
    }

    #[test]
    fn test_validate_accepts_boolean_column_in_boolean_context() {
        let schema = test_schema();
        let expr = Expr::Identifier(Ident::new("b"));
        validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap();
    }

    #[test]
    fn test_validate_accepts_parseable_boolean_string_literal() {
        let schema = test_schema();
        let expr = Expr::Value(SqlValue::SingleQuotedString("true".to_string()));
        validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap();
    }

    #[test]
    fn test_validate_rejects_non_parseable_boolean_string_literal() {
        let schema = test_schema();
        let expr = Expr::Value(SqlValue::SingleQuotedString("notabool".to_string()));
        let err =
            validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input syntax for type boolean"));
    }

    #[test]
    fn test_validate_accepts_json_access_containment_operators_in_boolean_context() {
        let schema = test_schema();

        for operator in [JsonOperator::AtArrow, JsonOperator::ArrowAt] {
            let expr = Expr::JsonAccess {
                left: Box::new(Expr::Identifier(Ident::new("j"))),
                operator,
                right: Box::new(Expr::Value(SqlValue::SingleQuotedString("{}".to_string()))),
            };
            validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap();
        }
    }

    #[test]
    fn test_validate_rejects_non_boolean_json_access_in_boolean_context() {
        let schema = test_schema();

        for operator in [
            JsonOperator::Arrow,
            JsonOperator::HashArrow,
            JsonOperator::LongArrow,
            JsonOperator::HashLongArrow,
        ] {
            let expr = Expr::JsonAccess {
                left: Box::new(Expr::Identifier(Ident::new("j"))),
                operator,
                right: Box::new(Expr::Value(SqlValue::SingleQuotedString("k".to_string()))),
            };
            let err = validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required")
                .unwrap_err()
                .to_string();
            assert_eq!(err, "boolean required");
        }
    }

    #[test]
    fn test_validate_accepts_any_all_ops_in_boolean_context() {
        let schema = test_schema();

        let array_expr = Expr::Array(sqlparser::ast::Array {
            elem: vec![Expr::Value(SqlValue::SingleQuotedString("x".to_string()))],
            named: true,
        });

        let any_expr = Expr::AnyOp {
            left: Box::new(Expr::Identifier(Ident::new("t"))),
            compare_op: BinaryOperator::Eq,
            right: Box::new(array_expr.clone()),
        };
        validate_bool_expr_in_boolean_context(&any_expr, &schema, "boolean required").unwrap();

        let all_expr = Expr::AllOp {
            left: Box::new(Expr::Identifier(Ident::new("t"))),
            compare_op: BinaryOperator::Eq,
            right: Box::new(array_expr),
        };
        validate_bool_expr_in_boolean_context(&all_expr, &schema, "boolean required").unwrap();
    }

    #[test]
    fn test_validate_rejects_case_with_text_literal_results_in_boolean_context() {
        let schema = test_schema();
        let expr = Expr::Case {
            operand: None,
            conditions: vec![Expr::Value(SqlValue::Boolean(true))],
            results: vec![Expr::Value(SqlValue::SingleQuotedString(
                "true".to_string(),
            ))],
            else_result: Some(Box::new(Expr::Value(SqlValue::SingleQuotedString(
                "false".to_string(),
            )))),
        };

        let err =
            validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap_err();
        assert_eq!(err.to_string(), "boolean required");
    }

    #[test]
    fn test_validate_accepts_case_with_boolean_results_in_boolean_context() {
        let schema = test_schema();
        let expr = Expr::Case {
            operand: None,
            conditions: vec![Expr::Value(SqlValue::Boolean(true))],
            results: vec![Expr::Value(SqlValue::Boolean(true))],
            else_result: Some(Box::new(Expr::Value(SqlValue::Boolean(false)))),
        };

        validate_bool_expr_in_boolean_context(&expr, &schema, "boolean required").unwrap();
    }
}
