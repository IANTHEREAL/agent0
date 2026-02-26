//! EXPLAIN output formatting.
//!
//! Contains text formatting for `PlanNode` trees (PostgreSQL-compatible EXPLAIN output)
//! and helper functions for formatting typed expressions, SQL literals, and JSON values.

use std::fmt::Write;

use super::PlanNode;
use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExpr, TypedExprKind};

pub(super) fn quote_sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(super) fn format_json_value_compact(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let parts = map
                .iter()
                .map(|(k, val)| {
                    let key = serde_json::to_string(k).unwrap_or_else(|_| format!("\"{}\"", k));
                    format!("{}: {}", key, format_json_value_compact(val))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", parts.join(", "))
        }
        serde_json::Value::Array(arr) => {
            let parts = arr
                .iter()
                .map(format_json_value_compact)
                .collect::<Vec<_>>();
            format!("[{}]", parts.join(", "))
        }
        serde_json::Value::String(s) => {
            serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s))
        }
        _ => v.to_string(),
    }
}

pub(super) fn format_json_literal_for_explain(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => format_json_value_compact(&v),
        Err(_) => raw.to_string(),
    }
}

fn format_typed_constant_for_explain(
    value: &crate::model::Value,
    data_type: &crate::model::DataType,
) -> String {
    use crate::model::{DataType, Value};

    match (value, data_type) {
        (Value::Text(s), DataType::Jsonb) => {
            let json = format_json_literal_for_explain(s);
            format!("{}::jsonb", quote_sql_literal(&json))
        }
        (Value::Text(s), DataType::Json) => {
            let json = format_json_literal_for_explain(s);
            format!("{}::json", quote_sql_literal(&json))
        }
        (Value::Text(s), DataType::Tsquery) => format!("{}::tsquery", quote_sql_literal(s)),
        (Value::Text(s), DataType::Tsvector) => format!("{}::tsvector", quote_sql_literal(s)),
        (Value::Text(s), DataType::Text) | (Value::Text(s), DataType::Varchar(_)) => {
            format!("{}::text", quote_sql_literal(s))
        }
        (Value::Jsonb(s), DataType::Jsonb) => {
            let json = format_json_literal_for_explain(s);
            format!("{}::jsonb", quote_sql_literal(&json))
        }
        (Value::Json(s), DataType::Json) => {
            let json = format_json_literal_for_explain(s);
            format!("{}::json", quote_sql_literal(&json))
        }
        (Value::Tsquery(s), DataType::Tsquery) => format!("{}::tsquery", quote_sql_literal(s)),
        (Value::Tsvector(s), DataType::Tsvector) => format!("{}::tsvector", quote_sql_literal(s)),
        _ => format!("{}", value),
    }
}

/// Format a typed expression for EXPLAIN output using PG-like literal/cast style.
pub(crate) fn format_typed_expr(expr: &TypedExpr) -> String {
    match &expr.kind {
        TypedExprKind::Constant(v) => format_typed_constant_for_explain(v, &expr.data_type),
        TypedExprKind::ColumnRef { column_name, .. } => column_name.clone(),
        TypedExprKind::BinaryOp { left, op, right } => {
            let right_text = if matches!(op, TypedBinaryOp::JsonContains) {
                match &right.kind {
                    TypedExprKind::Constant(crate::model::Value::Text(s)) => {
                        let json = format_json_literal_for_explain(s);
                        format!("{}::jsonb", quote_sql_literal(&json))
                    }
                    TypedExprKind::Constant(crate::model::Value::Jsonb(s)) => {
                        let json = format_json_literal_for_explain(s);
                        format!("{}::jsonb", quote_sql_literal(&json))
                    }
                    _ => format_typed_expr(right),
                }
            } else {
                format_typed_expr(right)
            };
            format!("({} {} {})", format_typed_expr(left), op, right_text)
        }
        TypedExprKind::UnaryOp { op, operand } => {
            format!("({}{})", op, format_typed_expr(operand))
        }
        TypedExprKind::FunctionCall { func, args, .. } => {
            let arg_text = args
                .iter()
                .map(format_typed_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", func.name.to_lowercase(), arg_text)
        }
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            ..
        } => {
            let cast_ty = format!("{}", target_type).to_lowercase();
            format!("({})::{}", format_typed_expr(inner), cast_ty)
        }
        _ => format!("{}", expr),
    }
}

/// Format a `PlanNode` tree as PostgreSQL-compatible EXPLAIN text output.
pub fn format_plan_text(plan: &PlanNode, indent: usize) -> String {
    let mut output = String::new();
    format_plan_node(&mut output, plan, indent, true);
    output
}

fn format_relation_display(table_name: &str, alias: Option<&str>) -> String {
    let short_name = table_name.rsplit('.').next().unwrap_or(table_name);
    let relation_name = if table_name.strip_prefix("public.").is_some() {
        short_name.to_string()
    } else {
        table_name.to_string()
    };
    match alias {
        Some(a) if !a.eq_ignore_ascii_case(short_name) => format!("{} {}", relation_name, a),
        _ => relation_name,
    }
}

fn format_plan_node(output: &mut String, plan: &PlanNode, indent: usize, is_first: bool) {
    let prefix = if is_first {
        " ".repeat(indent)
    } else {
        format!("{}->  ", " ".repeat(indent.saturating_sub(4)))
    };

    match plan {
        PlanNode::SeqScan {
            table_name,
            alias,
            filter,
            cost,
        } => {
            let table_display = format_relation_display(table_name, alias.as_deref());
            writeln!(
                output,
                "{}Seq Scan on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, table_display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(f) = filter {
                writeln!(output, "{}  Filter: {}", " ".repeat(indent), f).unwrap();
            }
        }
        PlanNode::IndexScan {
            table_name,
            alias,
            index_name,
            index_cond,
            filter,
            cost,
        } => {
            let table_display = format_relation_display(table_name, alias.as_deref());
            writeln!(
                output,
                "{}Index Scan using {} on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, index_name, table_display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(cond) = index_cond {
                writeln!(output, "{}  Index Cond: {}", " ".repeat(indent), cond).unwrap();
            }
            if let Some(f) = filter {
                writeln!(output, "{}  Filter: {}", " ".repeat(indent), f).unwrap();
            }
        }
        PlanNode::NestedLoop {
            join_type,
            cost,
            children,
        } => {
            writeln!(
                output,
                "{}Nested Loop {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, join_type, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            for (i, child) in children.iter().enumerate() {
                format_plan_node(output, child, indent + 6, i == 0);
            }
        }
        PlanNode::HashJoin {
            join_type,
            hash_cond,
            cost,
            children,
        } => {
            writeln!(
                output,
                "{}Hash Join {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, join_type, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(cond) = hash_cond {
                writeln!(output, "{}  Hash Cond: {}", " ".repeat(indent), cond).unwrap();
            }
            for (i, child) in children.iter().enumerate() {
                format_plan_node(output, child, indent + 6, i == 0);
            }
        }
        PlanNode::SemiJoin {
            anti,
            hash_cond,
            cost,
            children,
        } => {
            let join_name = if *anti {
                "Hash Anti Join"
            } else {
                "Hash Semi Join"
            };
            writeln!(
                output,
                "{}{}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, join_name, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(cond) = hash_cond {
                writeln!(output, "{}  Hash Cond: {}", " ".repeat(indent), cond).unwrap();
            }
            for (i, child) in children.iter().enumerate() {
                format_plan_node(output, child, indent + 6, i == 0);
            }
        }
        PlanNode::Sort {
            sort_key,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Sort  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            writeln!(
                output,
                "{}  Sort Key: {}",
                " ".repeat(indent),
                sort_key.join(", ")
            )
            .unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Limit {
            count: _,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Limit  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Aggregate {
            strategy,
            keys,
            cost,
            child,
        } => {
            let key_display = if keys.is_empty() {
                String::new()
            } else {
                format!("  Group Key: {}", keys.join(", "))
            };
            writeln!(
                output,
                "{}{}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, strategy, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if !key_display.is_empty() {
                writeln!(output, "{}{}", " ".repeat(indent), key_display).unwrap();
            }
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Filter {
            condition,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Filter  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            writeln!(output, "{}  Filter: {}", " ".repeat(indent), condition).unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::TableFunctionScan {
            function_name,
            alias,
            cost,
        } => {
            let display = if let Some(a) = alias {
                format!("{} {}", function_name, a)
            } else {
                function_name.clone()
            };
            writeln!(
                output,
                "{}Function Scan on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
        }
        PlanNode::Result { cost } => {
            writeln!(
                output,
                "{}Result  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
        }
    }
}
