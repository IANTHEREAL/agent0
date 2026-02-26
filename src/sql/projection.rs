//! Projection helpers for SELECT items and default value handling

use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::model::{Row, TableSchema, Value};

use super::expr::bridge::eval_const_ast_expr;

/// Get the name of a SELECT item (column name or alias)
#[cfg(test)]
pub fn get_select_item_name(item: &sqlparser::ast::SelectItem) -> String {
    match item {
        sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        sqlparser::ast::SelectItem::UnnamedExpr(expr) => get_expr_name(expr),
        sqlparser::ast::SelectItem::Wildcard(_) => "*".to_string(),
        _ => "?column?".to_string(),
    }
}

/// Get the name of an expression for column naming
#[cfg(test)]
pub fn get_expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|p| p.value.clone())
            .unwrap_or_else(|| "?column?".to_string()),
        Expr::Function(f) => {
            if let Some(last_ident) = f.name.0.last() {
                last_ident.value.to_lowercase()
            } else {
                "?column?".to_string()
            }
        }
        Expr::ArrayAgg(_) => "array_agg".to_string(),
        Expr::Case { .. } => "case".to_string(),
        Expr::Cast { data_type, .. } => {
            use sqlparser::ast::DataType;
            match data_type {
                DataType::Int(_) | DataType::Integer(_) => "int4".to_string(),
                DataType::BigInt(_) => "int8".to_string(),
                DataType::SmallInt(_) => "int2".to_string(),
                DataType::Text => "text".to_string(),
                DataType::Varchar(_) | DataType::CharVarying(_) => "varchar".to_string(),
                DataType::Boolean => "bool".to_string(),
                DataType::Float(_) | DataType::Real => "float4".to_string(),
                DataType::Double | DataType::DoublePrecision => "float8".to_string(),
                DataType::Numeric(_) | DataType::Decimal(_) => "numeric".to_string(),
                DataType::Timestamp(_, _) => "timestamp".to_string(),
                DataType::Date => "date".to_string(),
                DataType::Uuid => "uuid".to_string(),
                DataType::JSON => "json".to_string(),
                _ => data_type.to_string().to_lowercase(),
            }
        }
        Expr::Substring { .. } => "substring".to_string(),
        Expr::Trim { .. } => "btrim".to_string(),
        Expr::Position { .. } => "position".to_string(),
        Expr::Extract { .. } => "extract".to_string(),
        Expr::Subquery(_) => "subquery".to_string(),
        Expr::Nested(inner) => get_expr_name(inner),
        _ => "?column?".to_string(),
    }
}

#[cfg(test)]
use crate::model::DataType;
#[cfg(test)]
use sqlparser::ast::Expr;

/// Fill default values for missing columns in a row
pub fn fill_row_defaults(row: &mut Row, schema: &TableSchema) -> Result<()> {
    if row.values.len() < schema.columns.len() {
        for i in row.values.len()..schema.columns.len() {
            let col = &schema.columns[i];
            let val = if let Some(expr_str) = &col.default_expr {
                eval_default_expr(expr_str)?
            } else {
                Value::Null
            };
            row.values.push(val);
        }
    }
    Ok(())
}

/// Evaluate a default expression string
pub fn eval_default_expr(expr_str: &str) -> Result<Value> {
    let sql = format!("SELECT {}", expr_str);
    let dialect = PostgreSqlDialect {};
    let ast = Parser::parse_sql(&dialect, &sql)
        .map_err(|e| anyhow!("Failed to parse default expr: {}", e))?;

    if let Some(sqlparser::ast::Statement::Query(q)) = ast.into_iter().next() {
        if let sqlparser::ast::SetExpr::Select(s) = *q.body {
            if let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) =
                s.projection.into_iter().next()
            {
                return eval_const_ast_expr(&e);
            }
        }
    }
    Ok(Value::Text(expr_str.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnDef;
    use crate::sql::analyzer::{Analyzer, NullCatalog, Scope};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn analyze_expr_type(expr: &Expr, scope: Scope) -> DataType {
        let catalog = NullCatalog;
        let typed = Analyzer::analyze_expr_with_scope(&catalog, scope, expr).unwrap();
        typed.data_type
    }

    fn analyze_first_expr(sql: &str, scope: Scope) -> DataType {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        analyze_expr_type(expr, scope)
    }

    #[test]
    fn test_version_function_name() {
        let dialect = PostgreSqlDialect {};
        let sql = "SELECT version()";
        let statements = Parser::parse_sql(&dialect, sql).unwrap();

        if let sqlparser::ast::Statement::Query(query) = &statements[0] {
            if let sqlparser::ast::SetExpr::Select(select) = &*query.body {
                let item = &select.projection[0];
                let name = get_select_item_name(item);
                assert_eq!(
                    name, "version",
                    "Function name should be 'version', got '{}'",
                    name
                );
            }
        }
    }

    #[test]
    fn test_infer_expr_type_compound_identifier_prefers_full_name() {
        let schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "total".to_string(),
                data_type: DataType::Float64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT o.total").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        let mut scope = Scope::from_table_schema("o", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Float64);
    }

    #[test]
    fn test_infer_expr_type_coalesce_sum_float() {
        let schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "total".to_string(),
                data_type: DataType::Float64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT COALESCE(SUM(o.total), 0)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        let mut scope = Scope::from_table_schema("o", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Float64);
    }

    #[test]
    fn test_infer_expr_type_sum_int32_returns_int64() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT SUM(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Int64);
    }

    #[test]
    fn test_infer_expr_type_sum_int64_returns_numeric() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int64,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT SUM(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(
            analyze_expr_type(expr, scope),
            DataType::Numeric {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn test_infer_expr_type_grouping_returns_int32() {
        let schema = TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, "SELECT GROUPING(x)").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };

        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Int32);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_timestamp_to_timestamptz() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(
            &dialect,
            "SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC'",
        )
        .unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::TimestampTz);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_chain_returns_timestamp() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(
            &dialect,
            "SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE 'UTC' AT TIME ZONE 'America/New_York'",
        )
        .unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Timestamp);
    }

    #[test]
    fn test_infer_expr_type_at_time_zone_now_returns_timestamp() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements =
            Parser::parse_sql(&dialect, "SELECT NOW() AT TIME ZONE 'America/New_York'").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Timestamp);
    }

    #[test]
    fn test_infer_expr_type_json_access_contains_returns_boolean() {
        let schema = TableSchema::default();
        let dialect = PostgreSqlDialect {};
        let statements =
            Parser::parse_sql(&dialect, "SELECT '{\"a\":1}'::jsonb @> '{\"a\":1}'::jsonb").unwrap();
        let sqlparser::ast::Statement::Query(query) = statements.into_iter().next().unwrap() else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            panic!("expected select");
        };
        let sqlparser::ast::SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
            panic!("expected unnamed expr");
        };
        let mut scope = Scope::from_table_schema("t", &schema);
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(analyze_expr_type(expr, scope), DataType::Boolean);
    }

    #[test]
    fn test_infer_expr_type_bytea_builtins() {
        let mut scope = Scope::new();
        scope.allow_aggregates = true;
        scope.allow_windows = true;
        assert_eq!(
            analyze_first_expr("SELECT int8send(0::bigint)", scope.clone()),
            DataType::Bytes
        );
        assert_eq!(
            analyze_first_expr(r"SELECT get_bit(E'\\x80'::bytea, 0)", scope.clone()),
            DataType::Int32
        );
        assert_eq!(
            analyze_first_expr(r"SELECT set_bit(E'\\x00'::bytea, 0, 1)", scope.clone()),
            DataType::Bytes
        );
        assert_eq!(
            analyze_first_expr(
                r"SELECT substring(E'\\x0102030405060708'::bytea from 3)",
                scope.clone(),
            ),
            DataType::Bytes
        );
        assert_eq!(
            analyze_first_expr(
                r"SELECT overlay('\x00001122'::bytea placing '\xaabb'::bytea from 2 for 2)",
                scope,
            ),
            DataType::Bytes
        );
    }
}
