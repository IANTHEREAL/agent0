//! Expression analysis: single-pass name resolution + type checking.
//!
//! This module implements `Analyzer::analyze_expr`, the heart of the semantic
//! analysis. It walks a `sqlparser::ast::Expr` and produces a `TypedExpr` with:
//! - All names resolved to positional `ColumnRef` nodes
//! - All types checked and annotated
//! - All syntax sugar normalized to canonical forms (FunctionCall)
//! - Implicit casts inserted where needed
//!
//! ## Module structure
//!
//! The `analyze_expr` match dispatch lives here, with each expression category
//! extracted into its own submodule:
//!
//! - `casts` — CAST, typed string literals, interval literals
//! - `coercion` — implicit type coercion and type unification
//! - `collate` — COLLATE expressions
//! - `collections` — ARRAY literals, array indexing, ARRAY_AGG, tuple/row
//! - `comparisons` — BETWEEN, IN list, SIMILAR TO
//! - `distinct` — IS [NOT] DISTINCT FROM
//! - `functions` — function call analysis (built-in + UDF)
//! - `json` — JSON access operators (->, ->>, #>, etc.)
//! - `literals` — identifiers, compound identifiers, value literals
//! - `operators` — binary and unary operators
//! - `quantified` — ANY/ALL quantified comparisons
//! - `subquery` — scalar subqueries, EXISTS, IN subquery, ARRAY subquery
//! - `sugar` — syntax sugar normalization (SUBSTRING, TRIM, POSITION, etc.)
//!
//! Shared helpers (type predicates, coercion contracts, SQL type resolution)
//! remain here because they are used by multiple submodules.

mod casts;
mod coercion;
mod collate;
mod collections;
mod comparisons;
mod distinct;
mod functions;
mod json;
mod literals;
mod operators;
mod quantified;
mod subquery;
mod sugar;

use sqlparser::ast::{self as ast, Expr};

use crate::model::{DataType, Value};
use crate::sql::types::coercion::{common_type, comparison_target_type};

use super::error::AnalyzerError;
use super::types::*;
use super::Analyzer;

impl<'a> Analyzer<'a> {
    pub(super) fn is_text_like_type(data_type: &DataType) -> bool {
        matches!(
            data_type,
            DataType::Text | DataType::Name | DataType::Varchar(_) | DataType::Unknown
        )
    }

    pub(super) fn is_untyped_text_literal(expr: &TypedExpr) -> bool {
        matches!(
            (&expr.kind, &expr.data_type),
            (TypedExprKind::Constant(Value::Text(_)), DataType::Unknown)
        )
    }

    pub(super) fn any_rhs_elem_is_semantically_unknown(&self, expr: &TypedExpr) -> bool {
        expr.is_null_constant()
            || Self::is_untyped_text_literal(expr)
            || self.is_unresolved_param(expr)
    }

    pub(super) fn unknown_lhs_target_for_text_like_any(right_type: &DataType) -> DataType {
        match right_type {
            // PG keeps NAME when unknown-like LHS compares against NAME[]
            // (e.g. `$1 = ANY(ARRAY[]::name[])`).
            DataType::Name => DataType::Name,
            // PG resolves unknown-like LHS to TEXT for text/varchar families,
            // including typmod-bearing varchar(n) element types.
            DataType::Text | DataType::Varchar(_) => DataType::Text,
            _ => right_type.clone(),
        }
    }

    /// `ANY/ALL` coercion contract:
    /// - untyped NULL adopts the RHS element type
    /// - for text-like -> non-text, only UNKNOWN text literals / unresolved params may coerce
    /// - for unknown-like LHS vs text-like RHS, infer NAME only for NAME; otherwise TEXT
    /// - text-like vs text-like otherwise uses normal string common-type resolution
    pub(super) fn comparison_target_type_for_any(
        &self,
        left_expr: &TypedExpr,
        right_type: &DataType,
    ) -> Option<DataType> {
        if left_expr.is_null_constant() {
            return Some(right_type.clone());
        }

        // Unknown adapts to any type (PG's UNKNOWNOID semantics).
        if matches!(right_type, DataType::Unknown) {
            return Some(left_expr.data_type.clone());
        }
        if left_expr.data_type == DataType::Unknown {
            return Some(right_type.clone());
        }

        let lhs_text_like = Self::is_text_like_type(&left_expr.data_type);
        let rhs_text_like = Self::is_text_like_type(right_type);

        if lhs_text_like && !rhs_text_like {
            return if Self::is_untyped_text_literal(left_expr)
                || self.is_unresolved_param(left_expr)
            {
                Some(right_type.clone())
            } else {
                None
            };
        }

        if lhs_text_like && rhs_text_like {
            // PG: unknown/text-seeded LHS in scalar-array comparison should
            // infer NAME only for NAME RHS; text/varchar RHS infer TEXT.
            if Self::is_untyped_text_literal(left_expr) || self.is_unresolved_param(left_expr) {
                return Some(Self::unknown_lhs_target_for_text_like_any(right_type));
            }
            return common_type(&left_expr.data_type, right_type);
        }

        if !lhs_text_like && rhs_text_like {
            return None;
        }

        comparison_target_type(&left_expr.data_type, right_type)
    }

    fn resolve_catalog_custom_type(
        &self,
        name: &ast::ObjectName,
    ) -> Result<Option<DataType>, AnalyzerError> {
        let (schema_opt, type_name) = crate::sql::names::split_object_name(name)
            .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
        let resolved = self
            .catalog
            .resolve_type(&type_name, schema_opt.as_deref())
            .map_err(|e| AnalyzerError::Internal(e.to_string()))?;
        Ok(resolved.map(|udt| DataType::UserDefined(format!("{}.{}", udt.schema, udt.name))))
    }

    fn resolve_sql_data_type(&self, data_type: &ast::DataType) -> Result<DataType, AnalyzerError> {
        match data_type {
            ast::DataType::Array(inner) => {
                let inner_type = match inner {
                    ast::ArrayElemTypeDef::AngleBracket(inner_type)
                    | ast::ArrayElemTypeDef::SquareBracket(inner_type) => self
                        .resolve_sql_data_type(inner_type)
                        .map_err(|err| match err {
                            AnalyzerError::Unsupported(message)
                                if crate::sql::types::normalized_sql_type_name(data_type)
                                    .is_some()
                                    && message.starts_with("type \"")
                                    && message.ends_with(" does not exist") =>
                            {
                                let type_name =
                                    crate::sql::types::normalized_sql_type_name(data_type)
                                        .expect("checked above");
                                AnalyzerError::Unsupported(format!(
                                    "type \"{}\" does not exist",
                                    type_name
                                ))
                            }
                            other => other,
                        })?,
                    ast::ArrayElemTypeDef::None => DataType::Text,
                };
                Ok(DataType::Array(Box::new(inner_type)))
            }
            ast::DataType::Custom(name, modifiers) => {
                // Catalog lookup for UDTs (step 2), then unified pipeline.
                let catalog_resolved = self.resolve_catalog_custom_type(name)?;
                let (dt, _is_serial) = crate::sql::types::resolve_custom_type(
                    crate::sql::types::TypeResolutionContext::NonDdl,
                    name,
                    modifiers,
                    catalog_resolved,
                )
                .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                Ok(dt)
            }
            _ => crate::sql::types::mapping::sql_datatype_to_internal(data_type)
                .map_err(|e| AnalyzerError::Unsupported(e.to_string())),
        }
    }

    fn resolve_sql_type_text(&self, type_text: &str) -> Result<DataType, AnalyzerError> {
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let sql = format!("SELECT CAST(NULL AS {})", type_text);
        let stmt = sqlparser::parser::Parser::parse_sql(&dialect, &sql)
            .map_err(|e| {
                AnalyzerError::Unsupported(format!("unsupported type '{}': {}", type_text, e))
            })?
            .into_iter()
            .next()
            .ok_or_else(|| {
                AnalyzerError::Internal("empty parsed statement for type resolution".to_string())
            })?;

        let ast::Statement::Query(query) = stmt else {
            return Err(AnalyzerError::Internal(
                "type resolution parse did not produce a query".to_string(),
            ));
        };
        let ast::SetExpr::Select(select) = &*query.body else {
            return Err(AnalyzerError::Internal(
                "type resolution parse did not produce a SELECT".to_string(),
            ));
        };
        let Some(ast::SelectItem::UnnamedExpr(ast::Expr::Cast { data_type, .. })) =
            select.projection.first()
        else {
            return Err(AnalyzerError::Internal(
                "type resolution parse did not produce a CAST".to_string(),
            ));
        };

        self.resolve_sql_data_type(data_type)
    }

    /// Analyze an expression, producing a fully-typed IR node.
    ///
    /// This is the core single-pass analysis: name resolution and type checking
    /// happen simultaneously during one recursive walk (PostgreSQL model).
    pub fn analyze_expr(&mut self, expr: &Expr) -> Result<TypedExpr, AnalyzerError> {
        match expr {
            // -- Leaf nodes --
            Expr::Identifier(ident) => self.analyze_identifier(ident),

            Expr::CompoundIdentifier(parts) => self.analyze_compound_identifier(parts),

            Expr::Value(val) => self.analyze_value(val),

            // -- Parenthesized expression --
            Expr::Nested(inner) => self.analyze_expr(inner),

            // -- Tuple / row constructor syntax --
            Expr::Tuple(items) => self.analyze_tuple(items),

            // -- Binary operators --
            Expr::BinaryOp { left, op, right } => {
                // Schema-qualified JSON access: OPERATOR(pg_catalog.->) etc.
                // Rewrite to JsonAccess before regular binary-op analysis.
                if let Some(json_op) = Self::binary_op_to_json_access_op(op) {
                    let json_access = Expr::JsonAccess {
                        left: left.clone(),
                        operator: json_op,
                        right: right.clone(),
                    };
                    return self.analyze_expr(&json_access);
                }
                self.analyze_binary_op(left, op, right)
            }

            // -- Unary operators --
            Expr::UnaryOp { op, expr } => self.analyze_unary_op(op, expr),

            // -- Cast (see casts.rs) --
            Expr::Cast {
                expr, data_type, ..
            } => self.analyze_cast(expr, data_type),

            // -- Typed string literal (see casts.rs) --
            Expr::TypedString { data_type, value } => self.analyze_typed_string(data_type, value),

            // -- IS tests --
            Expr::IsNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, false),
            Expr::IsNotNull(expr) => self.analyze_is_test(expr, IsTestKind::Null, true),
            Expr::IsTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, false),
            Expr::IsNotTrue(expr) => self.analyze_is_test(expr, IsTestKind::True, true),
            Expr::IsFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, false),
            Expr::IsNotFalse(expr) => self.analyze_is_test(expr, IsTestKind::False, true),
            Expr::IsUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, false),
            Expr::IsNotUnknown(expr) => self.analyze_is_test(expr, IsTestKind::Unknown, true),

            // -- BETWEEN --
            Expr::Between { .. } => self.analyze_between(expr),

            // -- IN list --
            Expr::InList { .. } => self.analyze_in_list(expr),

            // -- LIKE / ILIKE --
            Expr::Like {
                negated,
                expr,
                pattern,
                escape_char,
            } => self.analyze_like(expr, pattern, escape_char, false, *negated),

            Expr::ILike {
                negated,
                expr,
                pattern,
                escape_char,
            } => self.analyze_like(expr, pattern, escape_char, true, *negated),

            // -- SIMILAR TO --
            Expr::SimilarTo { .. } => self.analyze_similar_to(expr),

            // -- CASE --
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => self.analyze_case(operand, conditions, results, else_result),

            // -- Functions --
            Expr::Function(func) => self.analyze_function(func),

            // -- Subqueries (see subquery.rs) --
            Expr::Subquery(query) => self.analyze_subquery_expr(query),

            Expr::Exists { subquery, negated } => self.analyze_exists(subquery, *negated),

            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => self.analyze_in_subquery(expr, subquery, *negated),

            // -- Array subquery: ARRAY(SELECT ...) --
            Expr::ArraySubquery(query) => self.analyze_array_subquery(query),

            // -- Array / ArrayIndex (see collections.rs) --
            Expr::Array(arr) => self.analyze_array_literal(arr),

            Expr::ArrayIndex { obj, indexes } => self.analyze_array_index(obj, indexes),

            // -- JSON access --
            Expr::JsonAccess {
                left,
                operator,
                right,
            } => self.analyze_json_access(left, operator, right),

            // -- Syntax sugar -> normalized to FunctionCall (see sugar.rs) --
            Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Position { .. }
            | Expr::Extract { .. }
            | Expr::AtTimeZone { .. }
            | Expr::Overlay { .. }
            | Expr::Ceil { .. }
            | Expr::Floor { .. } => self.analyze_syntax_sugar(expr),

            // -- Interval literal (see casts.rs) --
            Expr::Interval(interval) => self.analyze_interval(interval),

            // -- ArrayAgg -> AggregateCall (see collections.rs) --
            Expr::ArrayAgg(agg) => self.analyze_array_agg(agg),

            // -- ANY / ALL --
            Expr::AnyOp {
                left,
                compare_op,
                right,
            } => self.analyze_any_op(left, compare_op, right),

            // ALL: `x = ALL(ARRAY[a, b, c])` -> x = a AND x = b AND x = c
            // ALL with column reference: not yet supported.
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => self.analyze_all_op(left, compare_op, right),

            // -- COLLATE (see collate.rs) --
            Expr::Collate { expr, collation } => self.analyze_collate(expr, collation),

            // -- IS [NOT] DISTINCT FROM --
            Expr::IsDistinctFrom(left, right) => self.analyze_is_distinct_from(left, right, false),
            Expr::IsNotDistinctFrom(left, right) => {
                self.analyze_is_distinct_from(left, right, true)
            }

            // -- Catch-all for unsupported expressions --
            other => Err(AnalyzerError::Unsupported(format!(
                "expression type not yet supported: {:?}",
                std::mem::discriminant(other),
            ))),
        }
    }
}
