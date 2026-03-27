//! Expression analysis: single-pass name resolution + type checking.
//!
//! This module implements `Analyzer::analyze_expr`, the heart of the semantic
//! analysis. It walks a `sqlparser::ast::Expr` and produces a `TypedExpr` with:
//! - All names resolved to positional `ColumnRef` nodes
//! - All types checked and annotated
//! - All syntax sugar normalized to canonical forms (FunctionCall)
//! - Implicit casts inserted where needed

mod coercion;
mod functions;
mod literals;
mod operators;

use sqlparser::ast::{self as ast, BinaryOperator, Expr, TrimWhereField};

use crate::model::{DataType, Value};
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::{
    common_type, comparison_target_type, is_oid_alias_type, unify_types,
};

use super::error::AnalyzerError;
use super::types::*;
use super::Analyzer;

use coercion::extract_array_literal_elems;

impl<'a> Analyzer<'a> {
    fn is_text_like_type(data_type: &DataType) -> bool {
        matches!(
            data_type,
            DataType::Text | DataType::Name | DataType::Varchar(_) | DataType::Unknown
        )
    }

    fn is_untyped_text_literal(expr: &TypedExpr) -> bool {
        matches!(
            (&expr.kind, &expr.data_type),
            (TypedExprKind::Constant(Value::Text(_)), DataType::Unknown)
        )
    }

    fn any_rhs_elem_is_semantically_unknown(&self, expr: &TypedExpr) -> bool {
        expr.is_null_constant()
            || Self::is_untyped_text_literal(expr)
            || self.is_unresolved_param(expr)
    }

    fn implicit_text_array_coercion_source(expr: &TypedExpr) -> Option<&TypedExpr> {
        match &expr.kind {
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context: CastContext::Implicit,
            } if Self::is_text_like_type(target_type)
                && !Self::is_text_like_type(&inner.data_type) =>
            {
                Some(inner.as_ref())
            }
            _ => None,
        }
    }

    fn has_explicit_text_like_array_cast(expr: &TypedExpr) -> bool {
        match &expr.kind {
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context: CastContext::Explicit,
            } => {
                matches!(target_type, DataType::Array(elem) if Self::is_text_like_type(elem))
                    || Self::has_explicit_text_like_array_cast(inner)
            }
            TypedExprKind::Cast { expr: inner, .. } => {
                Self::has_explicit_text_like_array_cast(inner)
            }
            _ => false,
        }
    }

    /// ARRAY literal typing contract:
    /// - Treat untyped string literals / NULL / unresolved params as semantically UNKNOWN
    /// - If concrete explicit text-like members and concrete non-text members are both
    ///   present, fail ARRAY type resolution (PG parity)
    /// - If UNKNOWNs caused widening to text-like but concrete members are non-text,
    ///   recover element type from the concrete non-text members
    /// - Respect explicit text-like members/casts (do not recover past them)
    fn array_literal_elem_type(&self, elems: &[TypedExpr]) -> Result<DataType, AnalyzerError> {
        // PG resolves ARRAY['1','2'] to text[] at construction time — it does
        // NOT keep unknown[].  When all elements are Unknown, unify_expr_types
        // filters them out and returns Text (the PG default for unresolved
        // unknown literals).  This means `1 = ANY(ARRAY['1','2'])` correctly
        // errors with "operator does not exist: integer = text", matching PG.
        let refs: Vec<&TypedExpr> = elems.iter().collect();
        let mut elem_type = self.unify_expr_types(&refs, "ARRAY")?;

        if Self::is_text_like_type(&elem_type) {
            let mut concrete_non_text = Vec::new();
            let mut has_concrete_text_like = false;

            for elem in elems {
                if self.any_rhs_elem_is_semantically_unknown(elem) {
                    continue;
                }

                if Self::is_text_like_type(&elem.data_type) {
                    has_concrete_text_like = true;
                } else {
                    concrete_non_text.push(elem.data_type.clone());
                }
            }

            if has_concrete_text_like && !concrete_non_text.is_empty() {
                return Err(AnalyzerError::TypesCannotBeMatched {
                    types: elems.iter().map(|e| e.data_type.clone()).collect(),
                    context: "ARRAY".to_string(),
                });
            }

            if !has_concrete_text_like && !concrete_non_text.is_empty() {
                elem_type = unify_types(&concrete_non_text).ok_or_else(|| {
                    AnalyzerError::TypesCannotBeMatched {
                        types: elems.iter().map(|e| e.data_type.clone()).collect(),
                        context: "ARRAY".to_string(),
                    }
                })?;
            }
        }

        Ok(elem_type)
    }

    /// For `ANY/ALL` over non-empty literal arrays, recover RHS element type for
    /// operator resolution from concrete non-text members when array-level
    /// unification widened to text due UNKNOWN literals.
    ///
    /// Example (PostgreSQL): `1 = ANY(ARRAY[1, '2'])` resolves as integer
    /// comparison, while `1 = ANY(ARRAY['1', '2'])` remains integer vs text error.
    fn comparison_target_type_for_any_literal_array(
        &self,
        left_expr: &TypedExpr,
        elems: &[TypedExpr],
        fallback_elem_type: &DataType,
        allow_text_recovery: bool,
    ) -> Option<DataType> {
        let mut rhs_elem_type = fallback_elem_type.clone();

        if allow_text_recovery && !elems.is_empty() && Self::is_text_like_type(fallback_elem_type) {
            let mut concrete_non_text = Vec::new();
            let mut has_non_unknown_text_like = false;

            for elem in elems {
                if self.any_rhs_elem_is_semantically_unknown(elem) {
                    continue;
                }

                if let Some(inner) = Self::implicit_text_array_coercion_source(elem) {
                    concrete_non_text.push(inner.data_type.clone());
                    continue;
                }

                if Self::is_text_like_type(&elem.data_type) {
                    has_non_unknown_text_like = true;
                } else {
                    concrete_non_text.push(elem.data_type.clone());
                }
            }

            if !has_non_unknown_text_like && !concrete_non_text.is_empty() {
                rhs_elem_type = unify_types(&concrete_non_text)?;
            }
        }

        self.comparison_target_type_for_any(left_expr, &rhs_elem_type)
    }

    fn unknown_lhs_target_for_text_like_any(right_type: &DataType) -> DataType {
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

    /// Normalize the context type for parameter inference in IS DISTINCT FROM.
    ///
    /// PG applies three normalizations when inferring an unresolved param:
    /// - text-like: varchar(n) → text, name → name (PG drops typmod)
    /// - OID alias: regclass/regtype → oid (Int64 in db9; PG infers the base type)
    /// - everything else: use as-is
    fn param_inference_target(was_unresolved: bool, context_type: &DataType) -> DataType {
        if !was_unresolved {
            return context_type.clone();
        }
        if Self::is_text_like_type(context_type) {
            return Self::unknown_lhs_target_for_text_like_any(context_type);
        }
        if is_oid_alias_type(context_type) {
            // PG infers the unresolved param as `oid`, not as the alias type.
            // db9 stores OIDs as Int64.
            return DataType::Int64;
        }
        context_type.clone()
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
            Expr::Tuple(items) => {
                let analyzed_items: Vec<TypedExpr> = items
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;
                Ok(TypedExpr::new(
                    TypedExprKind::Row(analyzed_items),
                    DataType::UserDefined("record".to_string()),
                ))
            }

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

            // -- Cast --
            Expr::Cast {
                expr, data_type, ..
            } => {
                let inner = self.analyze_expr(expr)?;
                let target = self.resolve_sql_data_type(data_type)?;
                // Explicit cast resolves parameter type: `$1::int4`
                // Always record the inferred type for finalize_param_types,
                // but always emit a Cast node so the runtime converts the
                // value even if the wire decoder produces a different Value
                // variant (e.g. parse_pg_array yields Value::Text for UUID
                // strings — the Cast node converts them at eval time).
                if let TypedExprKind::Parameter { index } = &inner.kind {
                    self.resolve_param_type(*index, &target)?;
                }
                Ok(TypedExpr::new(
                    TypedExprKind::Cast {
                        expr: Box::new(inner),
                        target_type: target.clone(),
                        cast_context: CastContext::Explicit,
                    },
                    target,
                ))
            }

            // -- Typed string literal (DATE '...', TIMESTAMP '...') --
            Expr::TypedString { data_type, value } => {
                let target = self.resolve_sql_data_type(data_type)?;
                let parsed = self.parse_typed_literal(value, &target)?;
                Ok(TypedExpr::new(TypedExprKind::Constant(parsed), target))
            }

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
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let e = self.analyze_expr(expr)?;
                let lo = self.analyze_expr(low)?;
                let hi = self.analyze_expr(high)?;
                // All three operands must be type-compatible (PostgreSQL semantics).
                let common = self.unify_expr_types(&[&e, &lo, &hi], "BETWEEN")?;
                let e = self.coerce_if_needed(e, &common)?;
                let lo = self.coerce_if_needed(lo, &common)?;
                let hi = self.coerce_if_needed(hi, &common)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Between {
                        expr: Box::new(e),
                        low: Box::new(lo),
                        high: Box::new(hi),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // -- IN list --
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let e = self.analyze_expr(expr)?;
                let analyzed_list: Vec<TypedExpr> = list
                    .iter()
                    .map(|item| self.analyze_expr(item))
                    .collect::<Result<_, _>>()?;
                // All elements must be type-compatible with the expression.
                let mut in_refs: Vec<&TypedExpr> = vec![&e];
                in_refs.extend(analyzed_list.iter());
                let common = self.unify_expr_types(&in_refs, "IN list")?;
                let e = self.coerce_if_needed(e, &common)?;
                let analyzed_list = analyzed_list
                    .into_iter()
                    .map(|item| self.coerce_if_needed(item, &common))
                    .collect::<Result<_, _>>()?;
                Ok(TypedExpr::new(
                    TypedExprKind::InList {
                        expr: Box::new(e),
                        list: analyzed_list,
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

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
            Expr::SimilarTo {
                negated,
                expr,
                pattern,
                escape_char,
            } => {
                let e = self.analyze_expr(expr)?;
                let p = self.analyze_expr(pattern)?;
                let esc = escape_char.map(|c| {
                    Box::new(TypedExpr::new(
                        TypedExprKind::Constant(Value::Text(c.to_string())),
                        DataType::Text,
                    ))
                });
                Ok(TypedExpr::new(
                    TypedExprKind::SimilarTo {
                        expr: Box::new(e),
                        pattern: Box::new(p),
                        escape: esc,
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // -- CASE --
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => self.analyze_case(operand, conditions, results, else_result),

            // -- Functions --
            Expr::Function(func) => self.analyze_function(func),

            // -- Subqueries --
            Expr::Subquery(query) => {
                let analyzed = self.analyze_query(query)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                let dt = analyzed.output_schema[0].1.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::ScalarSubquery(Box::new(analyzed)),
                    dt,
                ))
            }

            Expr::Exists { subquery, negated } => {
                let analyzed = self.analyze_query(subquery)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Exists {
                        subquery: Box::new(analyzed),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                // Tuple form: (col1, col2, ...) [NOT] IN (SELECT ...)
                if let Expr::Tuple(tuple_exprs) = expr.as_ref() {
                    let mut analyzed_exprs: Vec<TypedExpr> = tuple_exprs
                        .iter()
                        .map(|e| self.analyze_expr(e))
                        .collect::<Result<Vec<_>, _>>()?;
                    let analyzed = self.analyze_query(subquery)?;
                    if analyzed.output_schema.len() != analyzed_exprs.len() {
                        return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                            got: analyzed.output_schema.len(),
                        });
                    }
                    // Pairwise coercion: coerce each tuple element to be
                    // comparison-compatible with the corresponding subquery
                    // output column, matching the scalar InSubquery path.
                    for (i, (_, sub_type, _)) in analyzed.output_schema.iter().enumerate() {
                        let elem = &analyzed_exprs[i];
                        if elem.data_type != *sub_type {
                            if elem.is_null_constant() {
                                analyzed_exprs[i] = TypedExpr::null(sub_type.clone());
                            } else if let Some(target) =
                                crate::sql::types::coercion::comparison_target_type(
                                    &elem.data_type,
                                    sub_type,
                                )
                            {
                                analyzed_exprs[i] =
                                    self.coerce_if_needed(analyzed_exprs[i].clone(), &target)?;
                            }
                        }
                    }
                    return Ok(TypedExpr::new(
                        TypedExprKind::TupleInSubquery {
                            exprs: analyzed_exprs,
                            subquery: Box::new(analyzed),
                            negated: *negated,
                        },
                        DataType::Boolean,
                    ));
                }

                let mut e = self.analyze_expr(expr)?;
                let analyzed = self.analyze_query(subquery)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                // Coerce LHS to be comparison-compatible with subquery output.
                // We cannot easily wrap the subquery output in a cast, so coerce
                // the LHS to the comparison target type (mirrors analyze_any_all_subquery).
                let sub_type = &analyzed.output_schema[0].1;
                if e.data_type != *sub_type {
                    if e.is_null_constant() {
                        e = TypedExpr::null(sub_type.clone());
                    } else if let Some(target) = comparison_target_type(&e.data_type, sub_type) {
                        e = self.coerce_if_needed(e, &target)?;
                    }
                }
                Ok(TypedExpr::new(
                    TypedExprKind::InSubquery {
                        expr: Box::new(e),
                        subquery: Box::new(analyzed),
                        negated: *negated,
                    },
                    DataType::Boolean,
                ))
            }

            // -- Array subquery: ARRAY(SELECT ...) --
            Expr::ArraySubquery(query) => {
                let analyzed = self.analyze_query(query)?;
                if analyzed.output_schema.len() != 1 {
                    return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                        got: analyzed.output_schema.len(),
                    });
                }
                let elem_type = analyzed.output_schema[0].1.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::ArraySubquery(Box::new(analyzed)),
                    DataType::Array(Box::new(elem_type)),
                ))
            }

            // -- Array --
            Expr::Array(arr) => {
                let elems: Vec<TypedExpr> = arr
                    .elem
                    .iter()
                    .map(|e| self.analyze_expr(e))
                    .collect::<Result<_, _>>()?;

                let elem_type = if elems.is_empty() {
                    // Empty array literal: element type unknown, default to Text
                    // (PostgreSQL: `SELECT ARRAY[]::text[]` requires cast for empty)
                    DataType::Text
                } else {
                    self.array_literal_elem_type(&elems)?
                };

                // Insert implicit casts for elements that don't match the unified type.
                let elems = elems
                    .into_iter()
                    .map(|e| self.coerce_if_needed(e, &elem_type))
                    .collect::<Result<_, _>>()?;

                Ok(TypedExpr::new(
                    TypedExprKind::ArrayLiteral(elems),
                    DataType::Array(Box::new(elem_type)),
                ))
            }

            Expr::ArrayIndex { obj, indexes } => {
                let arr = self.analyze_expr(obj)?;

                // Multiple dimensions: chain ArrayIndex nodes, unwrapping one
                // Array layer per subscript.
                let mut result = arr;
                for idx_expr in indexes {
                    let idx = self.analyze_expr(idx_expr)?;
                    let elem_type = match &result.data_type {
                        DataType::Array(inner) => inner.as_ref().clone(),
                        other => {
                            return Err(AnalyzerError::OperatorTypeMismatch {
                                operator: "[]".to_string(),
                                left: other.pg_display_name(),
                                right: idx.data_type.pg_display_name(),
                            });
                        }
                    };
                    result = TypedExpr::new(
                        TypedExprKind::ArrayIndex {
                            array: Box::new(result),
                            index: Box::new(idx),
                        },
                        elem_type,
                    );
                }
                Ok(result)
            }

            // -- JSON access --
            Expr::JsonAccess {
                left,
                operator,
                right,
            } => self.analyze_json_access(left, operator, right),

            // -- Syntax sugar -> normalized to FunctionCall --
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                let mut args = vec![self.analyze_expr(expr)?];
                if let Some(from) = substring_from {
                    args.push(self.analyze_expr(from)?);
                }
                if let Some(len) = substring_for {
                    args.push(self.analyze_expr(len)?);
                }
                self.make_function_call("SUBSTRING", args)
            }

            Expr::Trim {
                expr,
                trim_where,
                trim_what,
                ..
            } => {
                let func_name = match trim_where {
                    Some(TrimWhereField::Leading) => "LTRIM",
                    Some(TrimWhereField::Trailing) => "RTRIM",
                    Some(TrimWhereField::Both) | None => "BTRIM",
                };
                let mut args = vec![self.analyze_expr(expr)?];
                if let Some(what) = trim_what {
                    args.push(self.analyze_expr(what)?);
                }
                self.make_function_call(func_name, args)
            }

            Expr::Position { expr, r#in } => {
                // POSITION(substr IN str) -> STRPOS(str, substr)
                let substr = self.analyze_expr(expr)?;
                let string = self.analyze_expr(r#in)?;
                self.make_function_call("STRPOS", vec![string, substr])
            }

            Expr::Extract { field, expr } => {
                let field_str = format!("{}", field);
                let field_const = TypedExpr::new(
                    TypedExprKind::Constant(Value::Text(field_str)),
                    DataType::Text,
                );
                let date_expr = self.analyze_expr(expr)?;
                self.make_function_call("DATE_PART", vec![field_const, date_expr])
            }

            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                let ts = self.analyze_expr(timestamp)?;
                let tz = TypedExpr::new(
                    TypedExprKind::Constant(Value::Text(time_zone.clone())),
                    DataType::Text,
                );
                // AT TIME ZONE: tz is first arg per PG convention for timezone()
                self.make_function_call("TIMEZONE", vec![tz, ts])
            }

            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                let mut args = vec![
                    self.analyze_expr(expr)?,
                    self.analyze_expr(overlay_what)?,
                    self.analyze_expr(overlay_from)?,
                ];
                if let Some(len) = overlay_for {
                    args.push(self.analyze_expr(len)?);
                }
                self.make_function_call("OVERLAY", args)
            }

            Expr::Ceil { expr, .. } => {
                let arg = self.analyze_expr(expr)?;
                self.make_function_call("CEIL", vec![arg])
            }

            Expr::Floor { expr, .. } => {
                let arg = self.analyze_expr(expr)?;
                self.make_function_call("FLOOR", vec![arg])
            }

            // -- Interval literal --
            Expr::Interval(interval) => {
                let iv = self.parse_interval(&interval.value)?;
                Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Interval(iv)),
                    DataType::Interval,
                ))
            }

            // -- ArrayAgg -> AggregateCall --
            Expr::ArrayAgg(agg) => {
                let inner = self.analyze_expr(&agg.expr)?;
                let elem_type = inner.data_type.clone();
                let return_type = DataType::Array(Box::new(elem_type));
                let func = ResolvedFunction {
                    name: "ARRAY_AGG".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: return_type.clone(),
                };

                let order_by = if let Some(ob) = &agg.order_by {
                    self.analyze_order_by_exprs(ob, &[])?
                } else {
                    vec![]
                };

                Ok(TypedExpr::new(
                    TypedExprKind::AggregateCall {
                        func,
                        args: vec![inner],
                        distinct: agg.distinct,
                        order_by,
                        filter: None,
                    },
                    return_type,
                ))
            }

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

            // -- COLLATE --
            Expr::Collate { expr, collation } => {
                let analyzed_expr = self.analyze_expr(expr)?;
                // Validate that the expression is a text type
                match &analyzed_expr.data_type {
                    DataType::Text | DataType::Varchar(_) | DataType::Unknown => {}
                    other => {
                        return Err(AnalyzerError::Unsupported(format!(
                            "COLLATE can only be applied to text types, got {}",
                            other
                        )));
                    }
                }
                // ObjectName is a Vec<Ident>. PostgreSQL allows at most schema.collation
                // (2-part); 3+ parts are "cross-database references" and rejected.
                // Only pg_catalog is accepted as schema for built-in collations.
                let collation_name = match collation.0.len() {
                    1 => crate::sql::names::normalize_ident(&collation.0[0]),
                    2 => {
                        let schema = crate::sql::names::normalize_ident(&collation.0[0]);
                        if schema != "pg_catalog" {
                            // PostgreSQL distinguishes: known schema → "collation not found" (42704),
                            // unknown schema → "schema does not exist" (3F000).
                            let known = matches!(schema.as_str(), "public" | "information_schema")
                                || self.catalog.search_path().iter().any(|s| s == &schema);
                            if known {
                                let coll = crate::sql::names::normalize_ident(&collation.0[1]);
                                return Err(AnalyzerError::CollationNotFound(format!(
                                    "{}.{}",
                                    schema, coll
                                )));
                            }
                            return Err(AnalyzerError::SchemaNotFound(schema));
                        }
                        crate::sql::names::normalize_ident(&collation.0[1])
                    }
                    _ => {
                        return Err(AnalyzerError::CrossDatabaseReference(collation.to_string()));
                    }
                };
                // COLLATE "default" = use the database default collation, which is
                // the engine's default compare_text_pg path. Skip the Collate node.
                if collation_name.to_lowercase() == "default" {
                    return Ok(analyzed_expr);
                }

                // Resolve the collation at analysis time (catalog first, then registry)
                let resolved = self
                    .resolve_collation(&collation_name)
                    .map_err(|e| AnalyzerError::Unsupported(e.to_string()))?;
                // COLLATE preserves the input expression's type (P1-7 fix)
                let result_type = analyzed_expr.data_type.clone();
                Ok(TypedExpr::new(
                    TypedExprKind::Collate {
                        expr: Box::new(analyzed_expr),
                        collation: collation_name,
                        resolved,
                    },
                    result_type,
                ))
            }

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

    /// Analyze a JSON access expression (`->`, `->>`, `#>`, `#>>`, `#-`,
    /// `@>`, `<@`, `@@`).
    ///
    /// Handles chained access reassociation, precedence fixups for comparison
    /// operators nested inside the RHS by sqlparser, and type validation.
    fn analyze_json_access(
        &mut self,
        left: &Expr,
        operator: &ast::JsonOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        // Chained JSON access can arrive as:
        //   JsonAccess(left, op, JsonAccess(path1, op2, path2))
        // Reassociate to preserve JSON semantics:
        //   JsonAccess(JsonAccess(left, op, path1), op2, path2)
        if let Expr::JsonAccess {
            left: chained_left,
            operator: chained_op,
            right: chained_right,
        } = right
        {
            let left_json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: chained_left.clone(),
            };
            let reassociated = Expr::JsonAccess {
                left: Box::new(left_json_access),
                operator: *chained_op,
                right: chained_right.clone(),
            };
            return self.analyze_expr(&reassociated);
        }

        // sqlparser gives JSON operators lower precedence than comparison
        // operators, so `metadata->>'level' = 'senior'` is parsed as:
        //   JsonAccess(metadata, LongArrow, BinaryOp('level', Eq, 'senior'))
        // We must unwrap the nested operator: analyze the JSON access with
        // only the key, then wrap the result in the outer comparison.
        if let Expr::BinaryOp {
            left: bin_left,
            op: bin_op,
            right: bin_right,
        } = right
        {
            // Chained JSON access can arrive as:
            //   JsonAccess(left, op, BinaryOp(path1, json_op, path2))
            // Reassociate to preserve JSON access semantics:
            //   JsonAccess(JsonAccess(left, op, path1), json_op, path2)
            if let Some(chained_json_op) = Self::binary_op_to_json_access_op(bin_op) {
                let left_json_access = Expr::JsonAccess {
                    left: Box::new(left.clone()),
                    operator: *operator,
                    right: bin_left.clone(),
                };
                let reassociated = Expr::JsonAccess {
                    left: Box::new(left_json_access),
                    operator: chained_json_op,
                    right: bin_right.clone(),
                };
                return self.analyze_expr(&reassociated);
            }

            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: bin_left.clone(),
            };
            let outer = Expr::BinaryOp {
                left: Box::new(json_access),
                op: bin_op.clone(),
                right: bin_right.clone(),
            };
            return self.analyze_expr(&outer);
        }
        if let Expr::InList {
            expr: in_expr,
            list,
            negated,
        } = right
        {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: in_expr.clone(),
            };
            let outer = Expr::InList {
                expr: Box::new(json_access),
                list: list.clone(),
                negated: *negated,
            };
            return self.analyze_expr(&outer);
        }
        if let Expr::IsNull(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsNull(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsNotNull(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsNotNull(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsTrue(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsTrue(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsNotTrue(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsNotTrue(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsFalse(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsFalse(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsNotFalse(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsNotFalse(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsUnknown(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsUnknown(Box::new(json_access));
            return self.analyze_expr(&outer);
        }
        if let Expr::IsNotUnknown(inner) = right {
            let json_access = Expr::JsonAccess {
                left: Box::new(left.clone()),
                operator: *operator,
                right: inner.clone(),
            };
            let outer = Expr::IsNotUnknown(Box::new(json_access));
            return self.analyze_expr(&outer);
        }

        let l = self.analyze_expr(left)?;
        let r = self.analyze_expr(right)?;
        match operator {
            // Access operators -> JsonAccess IR node
            ast::JsonOperator::Arrow
            | ast::JsonOperator::LongArrow
            | ast::JsonOperator::HashArrow
            | ast::JsonOperator::HashLongArrow
            | ast::JsonOperator::HashMinus => {
                // Validate left operand is JSON/JSONB (PG compat)
                match &l.data_type {
                    DataType::Json | DataType::Jsonb => {}
                    other => {
                        let op_str = match operator {
                            ast::JsonOperator::Arrow => "->",
                            ast::JsonOperator::LongArrow => "->>",
                            ast::JsonOperator::HashArrow => "#>",
                            ast::JsonOperator::HashLongArrow => "#>>",
                            ast::JsonOperator::HashMinus => "#-",
                            _ => "json_op",
                        };
                        // PG reports bare string literals as type "unknown".
                        let right_type =
                            if matches!(&r.kind, TypedExprKind::Constant(Value::Text(_))) {
                                "unknown".to_string()
                            } else {
                                r.data_type.pg_display_name()
                            };
                        return Err(AnalyzerError::OperatorTypeMismatch {
                            operator: op_str.to_string(),
                            left: other.pg_display_name(),
                            right: right_type,
                        });
                    }
                }
                let json_op = match operator {
                    ast::JsonOperator::Arrow => JsonAccessOp::Arrow,
                    ast::JsonOperator::LongArrow => JsonAccessOp::LongArrow,
                    ast::JsonOperator::HashArrow => JsonAccessOp::HashArrow,
                    ast::JsonOperator::HashLongArrow => JsonAccessOp::HashLongArrow,
                    ast::JsonOperator::HashMinus => JsonAccessOp::HashMinus,
                    _ => unreachable!(),
                };
                let dt = match json_op {
                    JsonAccessOp::Arrow | JsonAccessOp::HashArrow | JsonAccessOp::HashMinus => {
                        DataType::Jsonb
                    }
                    JsonAccessOp::LongArrow | JsonAccessOp::HashLongArrow => DataType::Text,
                };
                Ok(TypedExpr::new(
                    TypedExprKind::JsonAccess {
                        expr: Box::new(l),
                        path: Box::new(r),
                        operator: json_op,
                    },
                    dt,
                ))
            }
            // Containment operators -> BinaryOp IR node (returns Boolean)
            ast::JsonOperator::AtArrow => Ok(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(l),
                    op: BinaryOp::JsonContains,
                    right: Box::new(r),
                },
                DataType::Boolean,
            )),
            ast::JsonOperator::ArrowAt => Ok(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(l),
                    op: BinaryOp::JsonContainedBy,
                    right: Box::new(r),
                },
                DataType::Boolean,
            )),
            // @@ (full-text search match) -- sqlparser 0.40 routes this
            // through JsonAccess, but it's a boolean operator.
            ast::JsonOperator::AtAt => Ok(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(l),
                    op: BinaryOp::TsMatch,
                    right: Box::new(r),
                },
                DataType::Boolean,
            )),
            other => Err(AnalyzerError::Unsupported(format!(
                "JSON operator {:?}",
                other
            ))),
        }
    }

    /// Analyze `x <op> ANY(rhs)` expressions.
    ///
    /// Handles subquery forms, literal array optimizations (`= ANY(ARRAY[...])` to
    /// `IN (...)`), `<> ANY` via `ScalarArrayCmp`, column-reference arrays via
    /// `ARRAY_POSITION`, and parameter type inference for Prisma-style `= ANY($1)`.
    fn analyze_any_op(
        &mut self,
        left: &Expr,
        compare_op: &BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        if let Expr::Subquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, false);
        }
        if let Expr::ArraySubquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, false);
        }

        let mut left_expr = self.analyze_expr(left)?;
        let mut right_expr = self.analyze_expr(right)?;

        // Optimize: `x = ANY(ARRAY[a, b, c])` -> `x IN (a, b, c)`
        if matches!(compare_op, BinaryOperator::Eq) {
            if let Some(elems) = extract_array_literal_elems(&right_expr) {
                // Empty array: type-check LHS against the array element type,
                // then return InList so the LHS is still evaluated at runtime
                // (catches errors like 1/0). Result is FALSE (= ANY of empty set).
                if elems.is_empty() {
                    let elem_type = match &right_expr.data_type {
                        DataType::Array(inner) => inner.as_ref().clone(),
                        _ => left_expr.data_type.clone(),
                    };
                    let common = self
                        .comparison_target_type_for_any(&left_expr, &elem_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: compare_op.to_string(),
                            left: left_expr.data_type.pg_display_name(),
                            right: elem_type.pg_display_name(),
                        })?;
                    let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                    return Ok(TypedExpr::new(
                        TypedExprKind::InList {
                            expr: Box::new(left_coerced),
                            list: vec![],
                            negated: false,
                        },
                        DataType::Boolean,
                    ));
                }
                let elem_type = match &right_expr.data_type {
                    DataType::Array(inner) => inner.as_ref().clone(),
                    _ => left_expr.data_type.clone(),
                };
                let allow_text_recovery = !Self::has_explicit_text_like_array_cast(&right_expr);
                let common = self
                    .comparison_target_type_for_any_literal_array(
                        &left_expr,
                        &elems,
                        &elem_type,
                        allow_text_recovery,
                    )
                    .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                        operator: compare_op.to_string(),
                        left: left_expr.data_type.pg_display_name(),
                        right: elem_type.pg_display_name(),
                    })?;
                let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                let list = elems
                    .into_iter()
                    .map(|e| self.coerce_if_needed(e, &common))
                    .collect::<Result<_, _>>()?;
                return Ok(TypedExpr::new(
                    TypedExprKind::InList {
                        expr: Box::new(left_coerced),
                        list,
                        negated: false,
                    },
                    DataType::Boolean,
                ));
            }
        }

        // `x <> ANY(ARRAY[a, b, c])` -> ScalarArrayCmp (OR of inequalities).
        // NOT the same as NOT IN which uses AND semantics.
        // ScalarArrayCmp evaluates LHS exactly once and handles empty arrays
        // correctly (LHS is still evaluated for side effects).
        if matches!(compare_op, BinaryOperator::NotEq) {
            if let Some(elems) = extract_array_literal_elems(&right_expr) {
                // Empty array: derive element type from right_expr.data_type
                // to enforce type compatibility. ScalarArrayCmp with empty
                // elems already evaluates LHS and returns FALSE at runtime.
                if elems.is_empty() {
                    let elem_type = match &right_expr.data_type {
                        DataType::Array(inner) => inner.as_ref().clone(),
                        _ => left_expr.data_type.clone(),
                    };
                    let common = self
                        .comparison_target_type_for_any(&left_expr, &elem_type)
                        .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                            operator: compare_op.to_string(),
                            left: left_expr.data_type.pg_display_name(),
                            right: elem_type.pg_display_name(),
                        })?;
                    let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                    let op = self.any_all_compare_op(compare_op)?;
                    return Ok(TypedExpr::new(
                        TypedExprKind::ScalarArrayCmp {
                            expr: Box::new(left_coerced),
                            elems: vec![],
                            op,
                            use_or: true,
                        },
                        DataType::Boolean,
                    ));
                }
                let elem_type = match &right_expr.data_type {
                    DataType::Array(inner) => inner.as_ref().clone(),
                    _ => left_expr.data_type.clone(),
                };
                let allow_text_recovery = !Self::has_explicit_text_like_array_cast(&right_expr);
                let common = self
                    .comparison_target_type_for_any_literal_array(
                        &left_expr,
                        &elems,
                        &elem_type,
                        allow_text_recovery,
                    )
                    .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                        operator: compare_op.to_string(),
                        left: left_expr.data_type.pg_display_name(),
                        right: elem_type.pg_display_name(),
                    })?;
                let left_coerced = self.coerce_if_needed(left_expr, &common)?;
                let coerced_elems = elems
                    .into_iter()
                    .map(|e| self.coerce_if_needed(e, &common))
                    .collect::<Result<_, _>>()?;
                let op = self.any_all_compare_op(compare_op)?;
                return Ok(TypedExpr::new(
                    TypedExprKind::ScalarArrayCmp {
                        expr: Box::new(left_coerced),
                        elems: coerced_elems,
                        op,
                        use_or: true,
                    },
                    DataType::Boolean,
                ));
            }
        }

        // General case: `x = ANY(array_col)` where array_col is a column reference
        // or other non-literal array expression.
        // Convert to: ARRAY_POSITION(array_col, x) IS NOT NULL
        if matches!(compare_op, BinaryOperator::Eq)
            && (matches!(right_expr.data_type, DataType::Array(_))
                || matches!(
                    &right_expr.data_type,
                    DataType::UserDefined(s)
                        if s.eq_ignore_ascii_case("int2vector")
                            || s.eq_ignore_ascii_case("oidvector")
                ))
        {
            // Coerce element types so LHS and array elements are compatible.
            // e.g. uuid_col = ANY(text_array) → cast array to uuid[], matching
            // how analyze_binary_op uses comparison_target_type for plain `=`.
            if let DataType::Array(elem_type) = &right_expr.data_type {
                if **elem_type != left_expr.data_type {
                    use crate::sql::types::coercion::comparison_target_type;
                    if let Some(target) = comparison_target_type(&left_expr.data_type, elem_type) {
                        if **elem_type != target {
                            right_expr = TypedExpr::new(
                                TypedExprKind::Cast {
                                    expr: Box::new(right_expr),
                                    target_type: DataType::Array(Box::new(target.clone())),
                                    cast_context: crate::sql::types::CastContext::Implicit,
                                },
                                DataType::Array(Box::new(target.clone())),
                            );
                        }
                        if left_expr.data_type != target {
                            left_expr = self.coerce_if_needed(left_expr, &target)?;
                        }
                    }
                }
            }

            let array_pos =
                self.make_function_call("ARRAY_POSITION", vec![right_expr, left_expr])?;
            return Ok(TypedExpr::new(
                TypedExprKind::IsTest {
                    expr: Box::new(array_pos),
                    test: IsTestKind::Null,
                    negated: true, // IS NOT NULL
                },
                DataType::Boolean,
            ));
        }

        // Infer array type for unresolved parameters in = ANY() context.
        // Prisma schema engine sends `WHERE col = ANY($1)` with OID=0;
        // PostgreSQL infers $1 as array(col_type) from context.
        if matches!(compare_op, BinaryOperator::Eq) {
            if let TypedExprKind::Parameter { index } = &right_expr.kind {
                if self.is_unresolved_param(&right_expr) {
                    let array_type = DataType::Array(Box::new(left_expr.data_type.clone()));
                    self.resolve_param_type(*index, &array_type)?;
                    let right_fixed =
                        TypedExpr::new(TypedExprKind::Parameter { index: *index }, array_type);
                    let array_pos =
                        self.make_function_call("ARRAY_POSITION", vec![right_fixed, left_expr])?;
                    return Ok(TypedExpr::new(
                        TypedExprKind::IsTest {
                            expr: Box::new(array_pos),
                            test: IsTestKind::Null,
                            negated: true,
                        },
                        DataType::Boolean,
                    ));
                }
            }
        }

        Err(AnalyzerError::Unsupported(format!(
            "ANY: right operand type {:?}, compare_op {:?}",
            right_expr.data_type, compare_op,
        )))
    }

    /// Analyze `x <op> ALL(rhs)` expressions.
    ///
    /// Handles subquery forms and literal array expansion
    /// (`= ALL(ARRAY[a, b, c])` to `a AND b AND c` chain).
    fn analyze_all_op(
        &mut self,
        left: &Expr,
        compare_op: &BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        if let Expr::Subquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, true);
        }
        if let Expr::ArraySubquery(subquery) = right {
            return self.analyze_any_all_subquery(left, compare_op, subquery, true);
        }

        let left_expr = self.analyze_expr(left)?;
        let right_expr = self.analyze_expr(right)?;

        // `x = ALL(ARRAY[a, b, c])` -> x = a AND x = b AND x = c
        if let Some(elems) = extract_array_literal_elems(&right_expr) {
            if elems.is_empty() {
                // ALL of empty array is TRUE by SQL standard
                return Ok(TypedExpr::new(
                    TypedExprKind::Constant(Value::Boolean(true)),
                    DataType::Boolean,
                ));
            }
            let op = self.any_all_compare_op(compare_op)?;
            // Build: (left op elem[0]) AND (left op elem[1]) AND ...
            // Coerce each element to be comparison-compatible with the LHS.
            let comparisons: Vec<TypedExpr> = elems
                .into_iter()
                .map(|elem| {
                    let (l, r) = self.ensure_comparison_compatible(left_expr.clone(), elem)?;
                    Ok(TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(l),
                            op: op.clone(),
                            right: Box::new(r),
                        },
                        DataType::Boolean,
                    ))
                })
                .collect::<Result<Vec<_>, AnalyzerError>>()?;
            let mut result = comparisons.into_iter();
            let first = result.next().unwrap();
            let combined = result.fold(first, |acc, next| {
                TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(acc),
                        op: BinaryOp::And,
                        right: Box::new(next),
                    },
                    DataType::Boolean,
                )
            });
            return Ok(combined);
        }

        Err(AnalyzerError::Unsupported(format!(
            "ALL with non-array operand: {:?}",
            compare_op,
        )))
    }

    /// Analyze `IS [NOT] DISTINCT FROM` with the same parameter-inference and
    /// coercion logic as comparison operators (`=`, `<>`).
    ///
    /// Mirrors the three-phase flow in `analyze_binary_op`:
    /// 1. Both-unknown → resolve to Text (PG UNKNOWN rule)
    /// 2. Contextual parameter typing from the concrete side
    /// 3. Implicit cast insertion via `comparison_target_type`
    fn analyze_is_distinct_from(
        &mut self,
        left: &Expr,
        right: &Expr,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut l = self.analyze_expr(left)?;
        let mut r = self.analyze_expr(right)?;

        // Phase 1: Both-unknown → resolve to Text, but NOT when either side
        // is NULL.  IS [NOT] DISTINCT FROM is a grammar-level construct in PG,
        // not an operator, so it does NOT have the "default to text" fallback
        // that `=` / `<>` get via operator resolution.  PG rejects:
        //   PREPARE p AS SELECT $1 IS DISTINCT FROM NULL;   -- 42P18
        // but accepts:
        //   PREPARE p AS SELECT $1 IS DISTINCT FROM $2;     -- both → text
        //   PREPARE p AS SELECT $1 IS DISTINCT FROM 'x';    -- $1 → text
        if self.is_semantically_unknown(&l)
            && self.is_semantically_unknown(&r)
            && !l.is_null_constant()
            && !r.is_null_constant()
        {
            if let TypedExprKind::Parameter { index } = &l.kind {
                if self.is_unresolved_param(&l) {
                    self.resolve_param_type(*index, &DataType::Text)?;
                    l = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Text);
                }
            }
            if let TypedExprKind::Parameter { index } = &r.kind {
                if self.is_unresolved_param(&r) {
                    self.resolve_param_type(*index, &DataType::Text)?;
                    r = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Text);
                }
            }
            // Resolve remaining Unknown literals to Text (bare string constants).
            if l.data_type == DataType::Unknown {
                l = TypedExpr::new(l.kind.clone(), DataType::Text);
            }
            if r.data_type == DataType::Unknown {
                r = TypedExpr::new(r.kind.clone(), DataType::Text);
            }
        }

        // Phase 2: Contextual parameter typing.
        //
        // Resolve an unresolved parameter from the concrete type on the other
        // side.  Unlike `analyze_binary_op` (which blocks ALL param-vs-param),
        // we allow a *resolved* parameter (client-specified OID) to serve as
        // context for an unresolved one — matching PostgreSQL:
        //   PREPARE p(text) AS SELECT $1 IS DISTINCT FROM $2;  -- {text,text}
        //
        // For text-like types, PG normalizes: varchar(n) → text, name → name.
        if let TypedExprKind::Parameter { index } = &l.kind {
            let rhs_blocks_param_inference = r.is_null_constant()
                || self.is_unresolved_param(&r)
                // PG has no `json = json` operator; don't infer param as Json.
                || (self.is_unresolved_param(&l) && r.data_type == DataType::Json);
            if !rhs_blocks_param_inference {
                let was_unresolved = self.is_unresolved_param(&l);
                let target = Self::param_inference_target(was_unresolved, &r.data_type);
                self.resolve_param_type(*index, &target)?;
                if was_unresolved {
                    l = TypedExpr::new(TypedExprKind::Parameter { index: *index }, target);
                }
            }
        }
        if let TypedExprKind::Parameter { index } = &r.kind {
            let lhs_blocks_param_inference = l.is_null_constant()
                || self.is_unresolved_param(&l)
                || (self.is_unresolved_param(&r) && l.data_type == DataType::Json);
            if !lhs_blocks_param_inference {
                let was_unresolved = self.is_unresolved_param(&r);
                let target = Self::param_inference_target(was_unresolved, &l.data_type);
                self.resolve_param_type(*index, &target)?;
                if was_unresolved {
                    r = TypedExpr::new(TypedExprKind::Parameter { index: *index }, target);
                }
            }
        }

        // Phase 3: Implicit cast insertion.
        //
        // IS [NOT] DISTINCT FROM short-circuits on NULL — the runtime evaluator
        // returns true/false directly without calling compare_values().  So when
        // either side is a NULL constant, no type coercion or compatibility check
        // is needed; skip Phase 3 entirely.  PG confirms:
        //   SELECT NULL IS DISTINCT FROM '{}'::json;  -- t  (no operator needed)
        //
        // For non-NULL operands:
        // - Numeric/temporal promotion always applies (Int32↔Int64, Date↔Timestamp).
        // - Text→typed coercion only for semantically unknown (bare literal, param).
        // - Explicitly typed `'1'::text IS DISTINCT FROM 1` must error.
        // - PG has no `json = json` operator: unknown → Json is rejected;
        //   unknown → Jsonb is allowed.
        // PG has no `json = json` operator.  Reject same-type Json here
        // (same-type pairs normally skip Phase 3 since no coercion is needed).
        // NULL short-circuits, so NULL IS DISTINCT FROM '{}'::json is fine.
        if l.data_type == DataType::Json
            && r.data_type == DataType::Json
            && !l.is_null_constant()
            && !r.is_null_constant()
        {
            return Err(AnalyzerError::OperatorTypeMismatch {
                operator: "=".to_string(),
                left: "json".to_string(),
                right: "json".to_string(),
            });
        }

        if l.data_type != r.data_type && !l.is_null_constant() && !r.is_null_constant() {
            let l_text = Self::is_text_like_type(&l.data_type);
            let r_text = Self::is_text_like_type(&r.data_type);
            // "unknown value" = bare literal or unresolved param (NOT null — handled above).
            let l_unknown_val = Self::is_untyped_text_literal(&l) || self.is_unresolved_param(&l);
            let r_unknown_val = Self::is_untyped_text_literal(&r) || self.is_unresolved_param(&r);

            let target = if l_text && !r_text {
                if l_unknown_val && !matches!(r.data_type, DataType::Json) {
                    // unknown → typed: allowed for most types including Jsonb.
                    Some(r.data_type.clone())
                } else if l_unknown_val {
                    // unknown vs Json → PG error (no = operator for json)
                    return Err(AnalyzerError::OperatorTypeMismatch {
                        operator: "=".to_string(),
                        left: "unknown".to_string(),
                        right: r.data_type.pg_display_name(),
                    });
                } else {
                    // Explicit text vs typed → PG error
                    return Err(AnalyzerError::OperatorTypeMismatch {
                        operator: "=".to_string(),
                        left: l.data_type.pg_display_name(),
                        right: r.data_type.pg_display_name(),
                    });
                }
            } else if r_text && !l_text {
                if r_unknown_val && !matches!(l.data_type, DataType::Json) {
                    Some(l.data_type.clone())
                } else if r_unknown_val {
                    return Err(AnalyzerError::OperatorTypeMismatch {
                        operator: "=".to_string(),
                        left: l.data_type.pg_display_name(),
                        right: "unknown".to_string(),
                    });
                } else {
                    return Err(AnalyzerError::OperatorTypeMismatch {
                        operator: "=".to_string(),
                        left: l.data_type.pg_display_name(),
                        right: r.data_type.pg_display_name(),
                    });
                }
            } else {
                // Neither side is Text-like, or both are: use standard rules
                // (numeric promotion, temporal promotion, etc.)
                comparison_target_type(&l.data_type, &r.data_type)
            };
            if let Some(mut target) = target {
                // OID alias types (regclass, regtype) are integers at the
                // storage level.  comparison_target_type returns the alias,
                // but the runtime cast layer doesn't support Int64→alias.
                // Use Int64 instead — it's the actual storage type for OIDs.
                if is_oid_alias_type(&target) {
                    target = DataType::Int64;
                }
                l = self.coerce_if_needed(l, &target)?;
                r = self.coerce_if_needed(r, &target)?;
            } else {
                // No coercion path found — types are incompatible.
                // PG rejects at analysis time; don't defer to runtime.
                return Err(AnalyzerError::OperatorTypeMismatch {
                    operator: "=".to_string(),
                    left: l.data_type.pg_display_name(),
                    right: r.data_type.pg_display_name(),
                });
            }
        }

        Ok(TypedExpr::new(
            TypedExprKind::IsDistinctFrom {
                left: Box::new(l),
                right: Box::new(r),
                negated,
            },
            DataType::Boolean,
        ))
    }
}
