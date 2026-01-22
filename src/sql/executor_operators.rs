use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, GroupByExpr, OrderByExpr, Query, SelectItem, SetExpr};
use tikv_client::Transaction;

use super::expr::eval_expr;
use super::helpers::infer_expr_type;
use super::operators::{
    execute_operator_tree, AggregateExpr, BoxedOperator, FilterOperator, HashAggregateOperator,
    JoinType, LimitOperator, NestedLoopJoinOperator, PhysicalPlanner, SortOperator,
    TableScanOperator,
};
use super::{ExecuteResult, Executor};
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

pub fn use_operator_execution() -> bool {
    static USE_OPERATORS: OnceLock<bool> = OnceLock::new();
    *USE_OPERATORS.get_or_init(|| {
        std::env::var("PGTIKV_USE_OPERATORS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub fn extract_limit(query: &Query) -> Option<usize> {
    if let Some(limit_expr) = &query.limit {
        if let Ok(v) = eval_expr(limit_expr, None, None) {
            return match v {
                Value::Int64(n) if n >= 0 => Some(n as usize),
                Value::Int32(n) if n >= 0 => Some(n as usize),
                _ => None,
            };
        }
    }
    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                return match v {
                    Value::Int64(n) if n >= 0 => Some(n as usize),
                    Value::Int32(n) if n >= 0 => Some(n as usize),
                    _ => Some(1),
                };
            }
        }
        return Some(1);
    }
    None
}

pub fn extract_offset(query: &Query) -> usize {
    if let Some(offset) = &query.offset {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            return match v {
                Value::Int64(n) if n >= 0 => n as usize,
                Value::Int32(n) if n >= 0 => n as usize,
                _ => 0,
            };
        }
    }
    0
}

fn expr_has_function_call(expr: &Expr) -> bool {
    match expr {
        Expr::Function(_) => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_function_call(left) || expr_has_function_call(right)
        }
        Expr::UnaryOp { expr, .. } => expr_has_function_call(expr),
        Expr::Nested(e) => expr_has_function_call(e),
        Expr::Between { expr, low, high, .. } => {
            expr_has_function_call(expr) || expr_has_function_call(low) || expr_has_function_call(high)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_function_call(expr) || list.iter().any(expr_has_function_call)
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => expr_has_function_call(e),
        Expr::IsFalse(e) | Expr::IsTrue(e) | Expr::IsNotFalse(e) | Expr::IsNotTrue(e) => {
            expr_has_function_call(e)
        }
        Expr::Cast { expr, .. } => expr_has_function_call(expr),
        Expr::Case { operand, conditions, results, else_result, .. } => {
            operand.as_ref().map_or(false, |e| expr_has_function_call(e))
                || conditions.iter().any(expr_has_function_call)
                || results.iter().any(expr_has_function_call)
                || else_result.as_ref().map_or(false, |e| expr_has_function_call(e))
        }
        _ => false,
    }
}

impl Executor {
    pub(crate) fn is_simple_operator_query(query: &Query, select: &sqlparser::ast::Select) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if !select.from[0].joins.is_empty() {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }

        if let Some(ref sel) = select.selection {
            if expr_has_function_call(sel) {
                return false;
            }
        }

        let has_function_or_agg = select.projection.iter().any(|item| {
            match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    matches!(
                        e,
                        Expr::Function(_)
                            | Expr::AggregateExpressionWithFilter { .. }
                            | Expr::ArrayAgg(_)
                    )
                }
                _ => false,
            }
        });

        if has_function_or_agg {
            return false;
        }

        let is_wildcard_only = select.projection.iter().all(|item| {
            matches!(
                item,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
            )
        });

        if !is_wildcard_only {
            return false;
        }

        if !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        ) {
            return false;
        }

        if select.having.is_some() {
            return false;
        }

        if select.distinct.is_some() {
            return false;
        }

        true
    }

    pub(crate) fn is_aggregate_operator_query(query: &Query, select: &sqlparser::ast::Select) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if !select.from[0].joins.is_empty() {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }
        if select.distinct.is_some() {
            return false;
        }

        let has_window_funcs = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if has_window_funcs {
            return false;
        }

        let group_by_exprs: Vec<String> = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs
                .iter()
                .filter_map(|e| {
                    if let Expr::Identifier(id) = e {
                        Some(id.value.to_lowercase())
                    } else {
                        None
                    }
                })
                .collect(),
            GroupByExpr::All => Vec::new(),
        };

        let is_simple_agg_func = |f: &sqlparser::ast::Function| {
            if f.filter.is_some() {
                return false;
            }
            let func_name = f
                .name
                .0
                .last()
                .map(|n| n.value.to_uppercase())
                .unwrap_or_default();
            matches!(
                func_name.as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
        };

        let mut has_aggregates = false;
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.filter.is_some() {
                        return false;
                    }
                    if is_simple_agg_func(f) {
                        has_aggregates = true;
                    }
                }
                SelectItem::UnnamedExpr(Expr::Identifier(id))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Identifier(id),
                    ..
                } => {
                    if !group_by_exprs.contains(&id.value.to_lowercase()) {
                        return false;
                    }
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    return false;
                }
                _ => {
                    return false;
                }
            }
        }

        has_aggregates
    }

    fn extract_aggregate_info(
        projection: &[SelectItem],
        schema: &TableSchema,
    ) -> (Vec<AggregateExpr>, Vec<String>, Vec<DataType>) {
        let mut agg_exprs = Vec::new();
        let mut agg_names = Vec::new();
        let mut agg_types = Vec::new();

        for item in projection {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(e) => (e, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                _ => continue,
            };

            if let Expr::Function(f) = expr {
                let func_name = f
                    .name
                    .0
                    .last()
                    .map(|n| n.value.to_uppercase())
                    .unwrap_or_default();

                if matches!(
                    func_name.as_str(),
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                ) {
                    let arg = f.args.first().and_then(|arg| match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
                        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                        _ => None,
                    });

                    let name = alias.unwrap_or_else(|| func_name.to_lowercase());

                    let data_type = match func_name.as_str() {
                        "COUNT" => DataType::Int64,
                        "SUM" => {
                            if let Some(ref a) = arg {
                                match infer_expr_type(a, schema) {
                                    DataType::Int32 | DataType::Int64 => DataType::Int64,
                                    DataType::Float64 | DataType::Numeric { .. } => {
                                        DataType::Numeric {
                                            precision: None,
                                            scale: None,
                                        }
                                    }
                                    _ => DataType::Numeric {
                                        precision: None,
                                        scale: None,
                                    },
                                }
                            } else {
                                DataType::Int64
                            }
                        }
                        "AVG" => DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        "MIN" | "MAX" => {
                            if let Some(ref a) = arg {
                                infer_expr_type(a, schema)
                            } else {
                                DataType::Text
                            }
                        }
                        "STRING_AGG" => DataType::Text,
                        "ARRAY_AGG" => DataType::Json,
                        _ => DataType::Text,
                    };

                    agg_exprs.push(AggregateExpr {
                        func_name: func_name.clone(),
                        arg,
                        distinct: f.distinct,
                    });
                    agg_names.push(name);
                    agg_types.push(data_type);
                }
            }
        }

        (agg_exprs, agg_names, agg_types)
    }

    fn extract_group_by_info(
        group_by: &GroupByExpr,
        schema: &TableSchema,
    ) -> (Vec<Expr>, Vec<String>, Vec<DataType>) {
        let exprs = match group_by {
            GroupByExpr::Expressions(exprs) => exprs.clone(),
            GroupByExpr::All => Vec::new(),
        };

        let mut names = Vec::new();
        let mut types = Vec::new();

        for expr in &exprs {
            let name = match expr {
                Expr::Identifier(id) => id.value.clone(),
                _ => format!("{}", expr),
            };
            let data_type = infer_expr_type(expr, schema);
            names.push(name);
            types.push(data_type);
        }

        (exprs, names, types)
    }

    pub(crate) async fn execute_aggregate_with_operators(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: TableSchema,
        filter: Option<&Expr>,
        group_by: &GroupByExpr,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
    ) -> Result<ExecuteResult> {
        let (group_by_exprs, group_by_names, group_by_types) =
            Self::extract_group_by_info(group_by, &schema);
        let (agg_exprs, agg_names, agg_types) = Self::extract_aggregate_info(projection, &schema);

        let mut root: BoxedOperator = Box::new(TableScanOperator::new(schema.clone()));

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        root = Box::new(HashAggregateOperator::new(
            root,
            group_by_exprs,
            agg_exprs,
            group_by_names.clone(),
            group_by_types.clone(),
            agg_names.clone(),
            agg_types.clone(),
        ));

        if !order_by.is_empty() {
            root = Box::new(SortOperator::new(root, order_by.to_vec()));
        }

        if limit.is_some() || offset > 0 {
            root = Box::new(LimitOperator::new(root, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut root,
            txn,
            self.store(),
            search_path,
            sequence_values,
        )
        .await?;

        let columns: Vec<String> = projection
            .iter()
            .map(super::helpers::get_select_item_name)
            .collect();

        #[derive(Clone, Copy, Debug)]
        enum ProjectionSource {
            Group(usize),
            Agg(usize),
        }

        let group_len = group_by_names.len();
        let mut sources: Vec<ProjectionSource> = Vec::with_capacity(projection.len());
        let mut column_types: Vec<DataType> = Vec::with_capacity(projection.len());

        let mut agg_idx = 0usize;
        for item in projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(_))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(_),
                    ..
                } => {
                    sources.push(ProjectionSource::Agg(agg_idx));
                    column_types.push(
                        agg_types
                            .get(agg_idx)
                            .cloned()
                            .ok_or_else(|| anyhow!("Aggregate column out of bounds"))?,
                    );
                    agg_idx += 1;
                }
                SelectItem::UnnamedExpr(Expr::Identifier(id))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Identifier(id),
                    ..
                } => {
                    let group_idx = group_by_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&id.value))
                        .ok_or_else(|| anyhow!("GROUP BY column '{}' not found", id.value))?;
                    sources.push(ProjectionSource::Group(group_idx));
                    column_types.push(
                        group_by_types
                            .get(group_idx)
                            .cloned()
                            .ok_or_else(|| anyhow!("Group column out of bounds"))?,
                    );
                }
                _ => {
                    return Err(anyhow!(
                        "Unsupported projection for aggregate operator execution"
                    ));
                }
            }
        }

        let mut projected_rows: Vec<Row> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values: Vec<Value> = Vec::with_capacity(sources.len());
            for source in &sources {
                let idx = match *source {
                    ProjectionSource::Group(i) => i,
                    ProjectionSource::Agg(i) => group_len + i,
                };
                values.push(row.values.get(idx).cloned().unwrap_or(Value::Null));
            }
            projected_rows.push(Row::new(values));
        }

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
        })
    }

    pub(crate) fn is_simple_join_operator_query(
        query: &Query,
        select: &sqlparser::ast::Select,
    ) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if select.from[0].joins.len() != 1 {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }
        if select.distinct.is_some() {
            return false;
        }

        let has_aggregates = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                let func_name = f
                    .name
                    .0
                    .last()
                    .map(|n| n.value.to_uppercase())
                    .unwrap_or_default();
                matches!(
                    func_name.as_str(),
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                )
            } else {
                false
            }
        });

        if has_aggregates {
            return false;
        }

        let has_window_funcs = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if has_window_funcs {
            return false;
        }

        if !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        ) {
            return false;
        }

        if select.having.is_some() {
            return false;
        }

        true
    }

    pub(crate) async fn execute_join_with_operators(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        left_schema: TableSchema,
        left_rows: Vec<Row>,
        right_schema: TableSchema,
        right_rows: Vec<Row>,
        join_type: JoinType,
        join_condition: Option<Expr>,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
    ) -> Result<ExecuteResult> {
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &left_schema.columns {
            combined_columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }
        for col in &right_schema.columns {
            combined_columns.push(ColumnDef {
                name: col.name.clone(),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let combined_schema = TableSchema {
            name: "join_result".to_string(),
            table_id: 0,
            columns: combined_columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let left_op: BoxedOperator =
            Box::new(TableScanOperator::new_with_rows(left_schema, left_rows));
        let right_op: BoxedOperator =
            Box::new(TableScanOperator::new_with_rows(right_schema, right_rows));

        let mut root: BoxedOperator = Box::new(NestedLoopJoinOperator::new(
            left_op,
            right_op,
            join_type,
            join_condition,
        ));

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        if !order_by.is_empty() {
            root = Box::new(SortOperator::new(root, order_by.to_vec()));
        }

        if limit.is_some() || offset > 0 {
            root = Box::new(LimitOperator::new(root, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut root,
            txn,
            self.store(),
            search_path,
            sequence_values,
        )
        .await?;

        let columns: Vec<String> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
                SelectItem::UnnamedExpr(_) => {
                    vec![super::helpers::get_select_item_name(item)]
                }
                SelectItem::ExprWithAlias { alias, .. } => vec![alias.value.clone()],
                SelectItem::QualifiedWildcard(_, _) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
            })
            .collect();

        let column_types: Vec<DataType> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.data_type.clone())
                    .collect(),
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    vec![infer_expr_type(expr, &combined_schema)]
                }
            })
            .collect();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
        })
    }

    pub(crate) async fn execute_with_operators(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
    ) -> Result<ExecuteResult> {
        let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());

        let estimated_rows = 1000;
        let mut operator = planner.plan_simple_select(
            schema.clone(),
            filter,
            order_by.to_vec(),
            limit,
            offset,
            estimated_rows,
        )?;

        let rows = execute_operator_tree(
            &mut operator,
            txn,
            self.store(),
            search_path,
            sequence_values,
        )
        .await?;

        let columns: Vec<String> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) => {
                    schema.columns.iter().map(|c| c.name.clone()).collect()
                }
                SelectItem::UnnamedExpr(_) => {
                    vec![super::helpers::get_select_item_name(item)]
                }
                SelectItem::ExprWithAlias { alias, .. } => vec![alias.value.clone()],
                SelectItem::QualifiedWildcard(_, _) => {
                    schema.columns.iter().map(|c| c.name.clone()).collect()
                }
            })
            .collect();

        let column_types: Vec<DataType> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    schema.columns.iter().map(|c| c.data_type.clone()).collect()
                }
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    vec![infer_expr_type(expr, &schema)]
                }
            })
            .collect();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_query(sql: &str) -> Query {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        match statements.into_iter().next().unwrap() {
            sqlparser::ast::Statement::Query(q) => *q,
            _ => panic!("Expected query"),
        }
    }

    fn get_select(query: &Query) -> &sqlparser::ast::Select {
        match &*query.body {
            SetExpr::Select(s) => s,
            _ => panic!("Expected select"),
        }
    }

    #[test]
    fn test_is_simple_query_basic_select() {
        let query = parse_query("SELECT * FROM users WHERE id > 5");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_column_projection() {
        let query = parse_query("SELECT id, name FROM users WHERE id > 5");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_query_with_order_limit() {
        let query = parse_query("SELECT * FROM users ORDER BY id LIMIT 10 OFFSET 5");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_join() {
        let query = parse_query("SELECT * FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_aggregate() {
        let query = parse_query("SELECT COUNT(*) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_group_by() {
        let query = parse_query("SELECT status, COUNT(*) FROM users GROUP BY status");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_window() {
        let query = parse_query("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_cte() {
        let query = parse_query("WITH t AS (SELECT * FROM users) SELECT * FROM t");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_distinct() {
        let query = parse_query("SELECT DISTINCT name FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_multiple_tables() {
        let query = parse_query("SELECT * FROM users, orders");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_count() {
        let query = parse_query("SELECT COUNT(*) FROM users");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_with_group_by() {
        let query = parse_query("SELECT status, COUNT(*) FROM users GROUP BY status");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_sum_avg() {
        let query = parse_query("SELECT SUM(amount), AVG(amount) FROM orders");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_simple_select() {
        let query = parse_query("SELECT id, name FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_join() {
        let query = parse_query("SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_window() {
        let query = parse_query("SELECT id, SUM(amount) OVER (ORDER BY id) FROM orders");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_join_query() {
        let query = parse_query("SELECT * FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_join_query_left_join() {
        let query =
            parse_query("SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_multiple_joins() {
        let query = parse_query(
            "SELECT * FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id",
        );
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_with_aggregate() {
        let query =
            parse_query("SELECT u.id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.id");
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_no_join() {
        let query = parse_query("SELECT * FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_bool_and() {
        let query = parse_query("SELECT BOOL_AND(flag) FROM flags");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_function_call() {
        let query = parse_query("SELECT BOOL_AND(flag) FROM flags");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_upper_function() {
        let query = parse_query("SELECT UPPER(name) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_array_agg() {
        let query = parse_query("SELECT ARRAY_AGG(value ORDER BY value) FROM test");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_filter_clause() {
        let query = parse_query("SELECT COUNT(*) FILTER (WHERE status = 'completed') FROM orders");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }
}
