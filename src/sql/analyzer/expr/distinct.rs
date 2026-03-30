//! `IS [NOT] DISTINCT FROM` expression analysis.

use sqlparser::ast::Expr;

use crate::model::DataType;
use crate::sql::types::coercion::{comparison_target_type, is_oid_alias_type};

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
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

    /// Analyze `IS [NOT] DISTINCT FROM` with the same parameter-inference and
    /// coercion logic as comparison operators (`=`, `<>`).
    ///
    /// Mirrors the three-phase flow in `analyze_binary_op`:
    /// 1. Both-unknown → resolve to Text (PG UNKNOWN rule)
    /// 2. Contextual parameter typing from the concrete side
    /// 3. Implicit cast insertion via `comparison_target_type`
    pub(super) fn analyze_is_distinct_from(
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
