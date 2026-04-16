//! AST-aware variable binding for PL/pgSQL embedded SQL.
//!
//! Replaces the text-based `substitute_variables()` / `replace_identifier()` approach
//! with an AST-level binder that:
//! 1. Parses embedded SQL first
//! 2. Walks expression positions in the AST
//! 3. Replaces bare `Expr::Identifier` nodes that match PL/pgSQL variables
//!    with typed literal expressions (preserving type semantics)
//!
//! This avoids the structural fragility of text substitution, particularly:
//! - jsonb variables being substituted as raw JSON (B2 in capability matrix)
//! - variable/column name collisions in DML contexts

use anyhow::Result;
use sqlparser::ast::{
    self as ast, DataType as AstDataType, Expr, FunctionArgExpr, GroupByExpr, Ident, ObjectName,
    Value as AstValue,
};

use crate::model::{DataType, Value};
use crate::sql::parse_sql;

use super::PlpgsqlContext;

// ── Typed literal builder ──────────────────────────────────────────

/// Convert a runtime PL/pgSQL `Value` + declared `DataType` into a typed
/// `sqlparser::ast::Expr`.
///
/// For types that can be represented as a native SQL literal (text, int, bool,
/// null) we emit the literal directly. For types that require explicit casting
/// (jsonb, timestamptz, etc.) we wrap the literal in `CAST(... AS <type>)`.
pub(super) fn value_to_ast_expr(value: &Value, data_type: &DataType) -> Expr {
    match value {
        Value::Null => Expr::Value(AstValue::Null),

        Value::Boolean(b) => Expr::Value(AstValue::Boolean(*b)),

        Value::Int32(i) => {
            if *i < 0 {
                Expr::Nested(Box::new(Expr::Value(number_literal(&i.to_string()))))
            } else {
                Expr::Value(number_literal(&i.to_string()))
            }
        }

        Value::Int64(i) => {
            if *i < 0 {
                Expr::Nested(Box::new(Expr::Value(number_literal(&i.to_string()))))
            } else {
                Expr::Value(number_literal(&i.to_string()))
            }
        }

        Value::Float64(f) => {
            if *f < 0.0 {
                Expr::Nested(Box::new(Expr::Value(number_literal(&f.to_string()))))
            } else {
                Expr::Value(number_literal(&f.to_string()))
            }
        }

        Value::Numeric(d) => {
            if d.is_sign_negative() {
                Expr::Nested(Box::new(Expr::Value(number_literal(&d.to_string()))))
            } else {
                Expr::Value(number_literal(&d.to_string()))
            }
        }

        Value::Text(t) => match data_type {
            DataType::Text | DataType::Varchar(_) | DataType::Name | DataType::Unknown => {
                Expr::Value(AstValue::SingleQuotedString(t.clone()))
            }
            _ => cast_string_literal(t, data_type),
        },

        // Json/Jsonb: stored as String internally — wrap in cast
        Value::Json(s) => Expr::Cast {
            expr: Box::new(Expr::Value(AstValue::SingleQuotedString(s.clone()))),
            data_type: custom_ast_type("json"),
            format: None,
        },

        Value::Jsonb(s) => Expr::Cast {
            expr: Box::new(Expr::Value(AstValue::SingleQuotedString(s.clone()))),
            data_type: custom_ast_type("jsonb"),
            format: None,
        },

        Value::Timestamp(ts) => {
            // Format epoch millis as ISO 8601 string (not raw number).
            let ts_str = match chrono::DateTime::from_timestamp_millis(*ts) {
                Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                None => ts.to_string(),
            };
            let target_type = match data_type {
                DataType::Timestamp => AstDataType::Timestamp(None, ast::TimezoneInfo::None),
                _ => AstDataType::Timestamp(None, ast::TimezoneInfo::WithTimeZone),
            };
            Expr::Cast {
                expr: Box::new(Expr::Value(AstValue::SingleQuotedString(ts_str))),
                data_type: target_type,
                format: None,
            }
        }

        Value::Date(d) => {
            // Format days-since-epoch as YYYY-MM-DD string (not raw number).
            let date_str =
                crate::model::date::format_date_days(*d).unwrap_or_else(|_| d.to_string());
            Expr::Cast {
                expr: Box::new(Expr::Value(AstValue::SingleQuotedString(date_str))),
                data_type: AstDataType::Date,
                format: None,
            }
        }

        Value::Time(t) => Expr::Cast {
            expr: Box::new(Expr::Value(AstValue::SingleQuotedString(t.to_string()))),
            data_type: custom_ast_type("time"),
            format: None,
        },

        Value::Uuid(u) => {
            let uuid_str = uuid::Uuid::from_bytes(*u).to_string();
            Expr::Cast {
                expr: Box::new(Expr::Value(AstValue::SingleQuotedString(uuid_str))),
                data_type: AstDataType::Uuid,
                format: None,
            }
        }

        Value::Bytes(b) => {
            let hex = hex::encode(b);
            Expr::Cast {
                expr: Box::new(Expr::Value(AstValue::EscapedStringLiteral(format!(
                    "\\x{hex}"
                )))),
                data_type: AstDataType::Bytea,
                format: None,
            }
        }

        Value::Interval(iv) => {
            let interval_str = iv.to_string();
            Expr::Cast {
                expr: Box::new(Expr::Value(AstValue::SingleQuotedString(interval_str))),
                data_type: AstDataType::Interval,
                format: None,
            }
        }

        Value::Array(elements) => {
            let element_type = match data_type {
                DataType::Array(inner) => inner.as_ref(),
                _ => &DataType::Text,
            };
            let ast_elements: Vec<Expr> = elements
                .iter()
                .map(|e| value_to_ast_expr(e, element_type))
                .collect();
            Expr::Array(ast::Array {
                elem: ast_elements,
                named: true,
            })
        }

        Value::Vector(v) => {
            // Vectors serialize as '[1.0, 2.0, ...]' string
            let vec_str = format!(
                "[{}]",
                v.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            Expr::Value(AstValue::SingleQuotedString(vec_str))
        }

        Value::Tsvector(s) | Value::Tsquery(s) => Expr::Cast {
            expr: Box::new(Expr::Value(AstValue::SingleQuotedString(s.clone()))),
            data_type: custom_ast_type(if matches!(value, Value::Tsvector(_)) {
                "tsvector"
            } else {
                "tsquery"
            }),
            format: None,
        },
    }
}

// ── Expression-level variable binder ────────────────────────────────

/// Walk an `Expr` AST and replace bare `Identifier` nodes whose name matches
/// a PL/pgSQL variable with the variable's typed literal expression.
///
/// Qualified identifiers (`table.column`, `schema.table.column`) are never
/// replaced — they are always treated as column/object references.
pub(super) fn bind_variables_in_expr(expr: &mut Expr, ctx: &PlpgsqlContext) {
    match expr {
        Expr::Identifier(ident) => {
            let name_lower = ident.value.to_lowercase();
            if ident.quote_style.is_none() {
                if let Some(value) = ctx.variables.get(&name_lower) {
                    let data_type = ctx
                        .variable_types
                        .get(&name_lower)
                        .cloned()
                        .unwrap_or(DataType::Text);
                    *expr = value_to_ast_expr(value, &data_type);
                }
            }
        }

        // Qualified identifiers: first try record field access (`rec.field`),
        // then PostgreSQL's `function_name.param_name` disambiguation syntax.
        Expr::CompoundIdentifier(parts) if parts.len() == 2 && parts[0].quote_style.is_none() => {
            let qualifier = parts[0].value.to_lowercase();
            let field = parts[1].value.to_lowercase();
            let full_key = format!("{}.{}", qualifier, field);

            // Record field access: FOR-loop variables are stored as "rec.field".
            // Only resolve when the qualifier is a known record variable (has
            // at least one "qualifier.*" entry) to avoid shadowing SQL aliases.
            let is_record = ctx
                .variables
                .keys()
                .any(|k| k.starts_with(&format!("{}.", qualifier)));
            if is_record {
                if let Some(value) = ctx.variables.get(&full_key) {
                    let data_type = ctx
                        .variable_types
                        .get(&full_key)
                        .cloned()
                        .unwrap_or(DataType::Text);
                    *expr = value_to_ast_expr(value, &data_type);
                }
            } else if !ctx.function_name.is_empty() && qualifier == ctx.function_name {
                // function_name.param_name disambiguation
                if let Some(value) = ctx.variables.get(&field) {
                    let data_type = ctx
                        .variable_types
                        .get(&field)
                        .cloned()
                        .unwrap_or(DataType::Text);
                    *expr = value_to_ast_expr(value, &data_type);
                }
            }
        }

        Expr::BinaryOp { left, right, .. } => {
            bind_variables_in_expr(left, ctx);
            bind_variables_in_expr(right, ctx);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            bind_variables_in_expr(inner, ctx);
        }
        Expr::Nested(inner) => {
            bind_variables_in_expr(inner, ctx);
        }
        Expr::Cast { expr: inner, .. }
        | Expr::TryCast { expr: inner, .. }
        | Expr::SafeCast { expr: inner, .. } => {
            bind_variables_in_expr(inner, ctx);
        }
        Expr::Function(func) => {
            for arg in &mut func.args {
                match arg {
                    ast::FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        bind_variables_in_expr(e, ctx)
                    }
                    ast::FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(e),
                        ..
                    } => bind_variables_in_expr(e, ctx),
                    _ => {}
                }
            }
            if let Some(filter) = func.filter.as_mut() {
                bind_variables_in_expr(filter, ctx);
            }
        }
        Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner) => {
            bind_variables_in_expr(inner, ctx);
        }
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            bind_variables_in_expr(a, ctx);
            bind_variables_in_expr(b, ctx);
        }
        Expr::InList { expr: e, list, .. } => {
            bind_variables_in_expr(e, ctx);
            for item in list {
                bind_variables_in_expr(item, ctx);
            }
        }
        Expr::InSubquery {
            expr: e, subquery, ..
        } => {
            bind_variables_in_expr(e, ctx);
            bind_variables_in_query(subquery, ctx);
        }
        Expr::Between {
            expr: e, low, high, ..
        } => {
            bind_variables_in_expr(e, ctx);
            bind_variables_in_expr(low, ctx);
            bind_variables_in_expr(high, ctx);
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                bind_variables_in_expr(op, ctx);
            }
            for cond in conditions {
                bind_variables_in_expr(cond, ctx);
            }
            for res in results {
                bind_variables_in_expr(res, ctx);
            }
            if let Some(else_r) = else_result {
                bind_variables_in_expr(else_r, ctx);
            }
        }
        Expr::Exists { subquery, .. } => {
            bind_variables_in_query(subquery, ctx);
        }
        Expr::Subquery(query) => {
            bind_variables_in_query(query, ctx);
        }
        Expr::ArrayIndex { obj, indexes } => {
            bind_variables_in_expr(obj, ctx);
            for idx in indexes {
                bind_variables_in_expr(idx, ctx);
            }
        }
        Expr::JsonAccess { left, right, .. } => {
            bind_variables_in_expr(left, ctx);
            bind_variables_in_expr(right, ctx);
        }
        Expr::CompositeAccess { expr: inner, .. } => {
            bind_variables_in_expr(inner, ctx);
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            bind_variables_in_expr(left, ctx);
            bind_variables_in_expr(right, ctx);
        }
        Expr::Like {
            expr: e, pattern, ..
        }
        | Expr::ILike {
            expr: e, pattern, ..
        }
        | Expr::SimilarTo {
            expr: e, pattern, ..
        } => {
            bind_variables_in_expr(e, ctx);
            bind_variables_in_expr(pattern, ctx);
        }
        Expr::Trim {
            expr: e, trim_what, ..
        } => {
            bind_variables_in_expr(e, ctx);
            if let Some(tw) = trim_what {
                bind_variables_in_expr(tw, ctx);
            }
        }
        Expr::Substring {
            expr: e,
            substring_from,
            substring_for,
            ..
        } => {
            bind_variables_in_expr(e, ctx);
            if let Some(f) = substring_from {
                bind_variables_in_expr(f, ctx);
            }
            if let Some(f) = substring_for {
                bind_variables_in_expr(f, ctx);
            }
        }
        Expr::Extract { expr: e, .. } => {
            bind_variables_in_expr(e, ctx);
        }
        Expr::Position { expr: e, r#in: i } => {
            bind_variables_in_expr(e, ctx);
            bind_variables_in_expr(i, ctx);
        }
        Expr::Array(arr) => {
            for e in &mut arr.elem {
                bind_variables_in_expr(e, ctx);
            }
        }
        Expr::Tuple(elems) => {
            for e in elems {
                bind_variables_in_expr(e, ctx);
            }
        }
        Expr::AtTimeZone { timestamp, .. } => {
            bind_variables_in_expr(timestamp, ctx);
        }

        // Leaf nodes / complex nodes we don't recurse into in Phase 1.
        // TODO(phase-2): add tracing/warn for unhandled Expr variants that may contain
        // variable identifiers (silent miss risk for shapes outside Plan A coverage).
        _ => {}
    }
}

// ── Statement-level variable binder ─────────────────────────────────

/// Bind variables in all expression positions of a parsed SQL statement.
/// Walks the statement AST and calls `bind_variables_in_expr` on each
/// expression node in an "expression position" (VALUES, WHERE, SET rhs,
/// function args, RETURNING, etc.).
///
/// Identifier positions (column definitions, table names) are NOT touched.
pub(super) fn bind_variables_in_statement(stmt: &mut ast::Statement, ctx: &PlpgsqlContext) {
    match stmt {
        ast::Statement::Insert {
            source,
            on,
            returning,
            ..
        } => {
            if let Some(src) = source {
                bind_variables_in_query(src, ctx);
            }
            if let Some(on_insert) = on {
                bind_variables_in_on_insert(on_insert, ctx);
            }
            if let Some(ret) = returning {
                for item in ret {
                    bind_variables_in_select_item(item, ctx);
                }
            }
        }

        ast::Statement::Update {
            assignments,
            selection,
            returning,
            from,
            ..
        } => {
            for assign in assignments {
                bind_variables_in_expr(&mut assign.value, ctx);
            }
            if let Some(sel) = selection {
                bind_variables_in_expr(sel, ctx);
            }
            if let Some(ret) = returning {
                for item in ret {
                    bind_variables_in_select_item(item, ctx);
                }
            }
            if let Some(from_clause) = from {
                bind_variables_in_table_with_joins(from_clause, ctx);
            }
        }

        ast::Statement::Delete {
            selection,
            returning,
            ..
        } => {
            if let Some(sel) = selection {
                bind_variables_in_expr(sel, ctx);
            }
            if let Some(ret) = returning {
                for item in ret {
                    bind_variables_in_select_item(item, ctx);
                }
            }
        }

        ast::Statement::Query(query) => {
            bind_variables_in_query(query, ctx);
        }

        // DDL and other statements: no variable binding needed
        _ => {}
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn bind_variables_in_query(query: &mut ast::Query, ctx: &PlpgsqlContext) {
    bind_variables_in_set_expr(&mut query.body, ctx);
    for ob in &mut query.order_by {
        bind_variables_in_expr(&mut ob.expr, ctx);
    }
    if let Some(limit) = &mut query.limit {
        bind_variables_in_expr(limit, ctx);
    }
    if let Some(offset) = &mut query.offset {
        bind_variables_in_expr(&mut offset.value, ctx);
    }
}

fn bind_variables_in_set_expr(set_expr: &mut ast::SetExpr, ctx: &PlpgsqlContext) {
    match set_expr {
        ast::SetExpr::Select(select) => {
            for item in &mut select.projection {
                bind_variables_in_select_item(item, ctx);
            }
            for table in &mut select.from {
                bind_variables_in_table_with_joins(table, ctx);
            }
            if let Some(sel) = &mut select.selection {
                bind_variables_in_expr(sel, ctx);
            }
            if let GroupByExpr::Expressions(exprs) = &mut select.group_by {
                for e in exprs {
                    bind_variables_in_expr(e, ctx);
                }
            }
            if let Some(having) = &mut select.having {
                bind_variables_in_expr(having, ctx);
            }
        }
        ast::SetExpr::Values(values) => {
            for row in &mut values.rows {
                for expr in row {
                    bind_variables_in_expr(expr, ctx);
                }
            }
        }
        ast::SetExpr::SetOperation { left, right, .. } => {
            bind_variables_in_set_expr(left, ctx);
            bind_variables_in_set_expr(right, ctx);
        }
        ast::SetExpr::Query(query) => {
            bind_variables_in_query(query, ctx);
        }
        _ => {}
    }
}

fn bind_variables_in_select_item(item: &mut ast::SelectItem, ctx: &PlpgsqlContext) {
    match item {
        ast::SelectItem::UnnamedExpr(e) => bind_variables_in_expr(e, ctx),
        ast::SelectItem::ExprWithAlias { expr, .. } => bind_variables_in_expr(expr, ctx),
        _ => {}
    }
}

fn bind_variables_in_table_with_joins(twj: &mut ast::TableWithJoins, ctx: &PlpgsqlContext) {
    bind_variables_in_table_factor(&mut twj.relation, ctx);
    for join in &mut twj.joins {
        bind_variables_in_table_factor(&mut join.relation, ctx);
        match &mut join.join_operator {
            ast::JoinOperator::Inner(c)
            | ast::JoinOperator::LeftOuter(c)
            | ast::JoinOperator::RightOuter(c)
            | ast::JoinOperator::FullOuter(c)
            | ast::JoinOperator::LeftSemi(c)
            | ast::JoinOperator::RightSemi(c)
            | ast::JoinOperator::LeftAnti(c)
            | ast::JoinOperator::RightAnti(c) => {
                if let ast::JoinConstraint::On(expr) = c {
                    bind_variables_in_expr(expr, ctx);
                }
            }
            ast::JoinOperator::CrossJoin
            | ast::JoinOperator::CrossApply
            | ast::JoinOperator::OuterApply => {}
        }
    }
}

fn bind_variables_in_table_factor(tf: &mut ast::TableFactor, ctx: &PlpgsqlContext) {
    match tf {
        ast::TableFactor::Derived { subquery, .. } => {
            bind_variables_in_query(subquery, ctx);
        }
        ast::TableFactor::TableFunction { expr, .. } => {
            bind_variables_in_expr(expr, ctx);
        }
        ast::TableFactor::UNNEST { array_exprs, .. } => {
            for e in array_exprs {
                bind_variables_in_expr(e, ctx);
            }
        }
        _ => {}
    }
}

fn bind_variables_in_on_insert(on: &mut ast::OnInsert, ctx: &PlpgsqlContext) {
    match on {
        ast::OnInsert::OnConflict(oc) => match &mut oc.action {
            ast::OnConflictAction::DoUpdate(du) => {
                for assign in &mut du.assignments {
                    bind_variables_in_expr(&mut assign.value, ctx);
                }
                if let Some(sel) = &mut du.selection {
                    bind_variables_in_expr(sel, ctx);
                }
            }
            ast::OnConflictAction::DoNothing => {}
        },
        ast::OnInsert::DuplicateKeyUpdate(assignments) => {
            for assign in assignments {
                bind_variables_in_expr(&mut assign.value, ctx);
            }
        }
        _ => {}
    }
}

// ── literal constructors ────────────────────────────────────────────

fn number_literal(s: &str) -> AstValue {
    AstValue::Number(s.to_string(), false)
}

fn custom_ast_type(name: &str) -> AstDataType {
    AstDataType::Custom(ObjectName(vec![Ident::new(name)]), vec![])
}

fn cast_string_literal(s: &str, data_type: &DataType) -> Expr {
    let ast_type = model_type_to_ast_type(data_type);
    Expr::Cast {
        expr: Box::new(Expr::Value(AstValue::SingleQuotedString(s.to_string()))),
        data_type: ast_type,
        format: None,
    }
}

fn model_type_to_ast_type(dt: &DataType) -> AstDataType {
    match dt {
        DataType::Text | DataType::Unknown => AstDataType::Text,
        DataType::Varchar(len) => {
            AstDataType::Varchar(Some(sqlparser::ast::CharacterLength::IntegerLength {
                length: *len,
                unit: None,
            }))
        }
        DataType::Int32 => AstDataType::Integer(None),
        DataType::Int64 => AstDataType::BigInt(None),
        DataType::Oid => custom_ast_type("oid"),
        DataType::Float64 => AstDataType::DoublePrecision,
        DataType::Numeric { .. } => custom_ast_type("numeric"),
        DataType::Boolean => AstDataType::Boolean,
        DataType::Timestamp => AstDataType::Timestamp(None, ast::TimezoneInfo::None),
        DataType::TimestampTz => AstDataType::Timestamp(None, ast::TimezoneInfo::WithTimeZone),
        DataType::Date => AstDataType::Date,
        DataType::Uuid => AstDataType::Uuid,
        DataType::Bytes => AstDataType::Bytea,
        DataType::Json => custom_ast_type("json"),
        DataType::Jsonb => custom_ast_type("jsonb"),
        DataType::Interval => AstDataType::Interval,
        DataType::Name => custom_ast_type("name"),
        DataType::Time => custom_ast_type("time"),
        DataType::Array(inner) => {
            let _ = inner;
            AstDataType::Text
        }
        DataType::Tsvector | DataType::Tsquery => custom_ast_type("text"),
        DataType::Vector(_) => custom_ast_type("text"),
        DataType::UserDefined(name) => custom_ast_type(name),
    }
}

// ── Public entry points for executor integration ───────────────────

/// Parse raw SQL, bind PL/pgSQL variables in the AST, and return bound statements.
///
/// Returns `Ok(statements)` if parsing + binding succeeds, or `Err` on parse failure.
pub(super) fn bind_sql_statements(
    raw_sql: &str,
    ctx: &PlpgsqlContext,
) -> Result<Vec<ast::Statement>> {
    let mut stmts = parse_sql(raw_sql)?;
    for stmt in &mut stmts {
        bind_variables_in_statement(stmt, ctx);
    }
    Ok(stmts)
}

/// Parse a PL/pgSQL expression (wrapping in `SELECT <expr>`), bind variables,
/// and return the bound SQL string ready for execution.
///
/// Returns the full `SELECT <bound_expr>` as a string.
pub(super) fn bind_expression(expr_str: &str, ctx: &PlpgsqlContext) -> Result<String> {
    let sql = format!("SELECT {}", expr_str);
    let mut stmts = parse_sql(&sql)?;
    if let Some(ast::Statement::Query(ref mut query)) = stmts.first_mut() {
        if let ast::SetExpr::Select(ref mut select) = *query.body {
            for item in &mut select.projection {
                match item {
                    ast::SelectItem::UnnamedExpr(ref mut expr) => {
                        bind_variables_in_expr(expr, ctx);
                    }
                    ast::SelectItem::ExprWithAlias { ref mut expr, .. } => {
                        bind_variables_in_expr(expr, ctx);
                    }
                    _ => {}
                }
            }
        }
    }
    // Reconstruct the SQL from the bound AST
    Ok(stmts
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("; "))
}
