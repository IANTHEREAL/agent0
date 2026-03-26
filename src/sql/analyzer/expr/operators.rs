//! Binary operator, unary operator, IS test, LIKE, and ANY/ALL analysis.
//!
//! Contains `analyze_binary_op`, `analyze_unary_op`, `analyze_is_test`,
//! `analyze_like`, `analyze_any_all_subquery`, `any_all_compare_op`,
//! and related helpers.

use sqlparser::ast::{self as ast, BinaryOperator, Expr, UnaryOperator};

use crate::model::{DataType, Value};
use crate::sql::types::coercion::{
    binary_op_result_type, common_type, comparison_target_type, is_numeric,
};

use crate::sql::analyzer::error::AnalyzerError;
use crate::sql::analyzer::types::*;
use crate::sql::analyzer::Analyzer;

impl<'a> Analyzer<'a> {
    // -- Helper: binary operators --

    pub(super) fn analyze_binary_op(
        &mut self,
        left: &Expr,
        op: &ast::BinaryOperator,
        right: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut l = self.analyze_expr(left)?;
        let mut r = self.analyze_expr(right)?;
        let typed_op = self.convert_binary_op(op)?;

        // Contextual NULL typing: if one side is NULL, adopt the other's type
        if l.is_null_constant() && !r.is_null_constant() {
            l = TypedExpr::null(r.data_type.clone());
        } else if r.is_null_constant() && !l.is_null_constant() {
            r = TypedExpr::null(l.data_type.clone());
        }

        // Both-unknown ambiguity check (PG UNKNOWN type rule).
        // Must run BEFORE contextual parameter typing to catch mixed-unknown
        // cases like `$1 + '1'`, `NULL + '1'`, `NULL + NULL`.
        if self.is_semantically_unknown(&l) && self.is_semantically_unknown(&r) {
            if Self::resolves_unknown_pair_to_text(&typed_op) {
                // Resolve unresolved params to Text; literals are already Text.
                if let TypedExprKind::Parameter { index } = &l.kind {
                    if self.is_unresolved_param(&l) {
                        self.resolve_param_type(*index, &DataType::Text)?;
                        l = TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            DataType::Text,
                        );
                    }
                }
                if let TypedExprKind::Parameter { index } = &r.kind {
                    if self.is_unresolved_param(&r) {
                        self.resolve_param_type(*index, &DataType::Text)?;
                        r = TypedExpr::new(
                            TypedExprKind::Parameter { index: *index },
                            DataType::Text,
                        );
                    }
                }
                // Resolve any remaining Unknown to Text for downstream processing.
                if l.data_type == DataType::Unknown {
                    l = TypedExpr::new(l.kind.clone(), DataType::Text);
                }
                if r.data_type == DataType::Unknown {
                    r = TypedExpr::new(r.kind.clone(), DataType::Text);
                }
            } else if Self::is_ambiguous_unknown_pair_op(&typed_op) {
                return Err(AnalyzerError::AmbiguousOperator {
                    operator: typed_op.to_string(),
                    left: "unknown".to_string(),
                    right: "unknown".to_string(),
                });
            }
        }

        // One-concrete-one-unknown resolution: coerce Unknown to match the concrete side,
        // with operator-specific overrides where PG resolves Unknown differently:
        //   - Concat (||) with non-jsonb: Unknown → Text (string concatenation).
        //     Concat with jsonb keeps Unknown → Jsonb (jsonb merge).
        //   - Sub (-) with jsonb LHS: Unknown → Text (delete-by-text-key).
        if l.data_type == DataType::Unknown
            && r.data_type != DataType::Unknown
            && !matches!(r.kind, TypedExprKind::Parameter { .. })
        {
            let resolve_type = if typed_op == BinaryOp::Concat && r.data_type != DataType::Jsonb {
                DataType::Text
            } else {
                r.data_type.clone()
            };
            l = self.coerce_if_needed(l, &resolve_type)?;
        }
        if r.data_type == DataType::Unknown
            && l.data_type != DataType::Unknown
            && !matches!(l.kind, TypedExprKind::Parameter { .. })
        {
            let resolve_type = if typed_op == BinaryOp::Concat && l.data_type != DataType::Jsonb {
                DataType::Text
            } else if typed_op == BinaryOp::Sub && l.data_type == DataType::Jsonb {
                // jsonb - text (delete key), jsonb - int (delete by index)
                // Unknown string literals should resolve to Text, not Jsonb.
                DataType::Text
            } else {
                l.data_type.clone()
            };
            r = self.coerce_if_needed(r, &resolve_type)?;
        }

        // Contextual parameter typing (mirrors NULL typing above).
        // Resolve parameter from the concrete type on the other side.
        // Always call resolve_param_type for conflict detection, even for
        // already-resolved params (enables InconsistentParameterTypes).
        if let TypedExprKind::Parameter { index } = &l.kind {
            if !r.is_null_constant() && !matches!(&r.kind, TypedExprKind::Parameter { .. }) {
                let was_unresolved = self.is_unresolved_param(&l);
                self.resolve_param_type(*index, &r.data_type)?;
                if was_unresolved {
                    l = TypedExpr::new(
                        TypedExprKind::Parameter { index: *index },
                        r.data_type.clone(),
                    );
                }
            }
        }
        if let TypedExprKind::Parameter { index } = &r.kind {
            if !l.is_null_constant() && !matches!(&l.kind, TypedExprKind::Parameter { .. }) {
                let was_unresolved = self.is_unresolved_param(&r);
                self.resolve_param_type(*index, &l.data_type)?;
                if was_unresolved {
                    r = TypedExpr::new(
                        TypedExprKind::Parameter { index: *index },
                        l.data_type.clone(),
                    );
                }
            }
        }

        // AND/OR: both sides must be boolean -- resolve params (including
        // already-resolved ones for conflict detection)
        if matches!(typed_op, BinaryOp::And | BinaryOp::Or) {
            if let TypedExprKind::Parameter { index } = &l.kind {
                let was_unresolved = self.is_unresolved_param(&l);
                self.resolve_param_type(*index, &DataType::Boolean)?;
                if was_unresolved {
                    l = TypedExpr::new(
                        TypedExprKind::Parameter { index: *index },
                        DataType::Boolean,
                    );
                }
            }
            if let TypedExprKind::Parameter { index } = &r.kind {
                let was_unresolved = self.is_unresolved_param(&r);
                self.resolve_param_type(*index, &DataType::Boolean)?;
                if was_unresolved {
                    r = TypedExpr::new(
                        TypedExprKind::Parameter { index: *index },
                        DataType::Boolean,
                    );
                }
            }
        }

        // PostgreSQL UNKNOWN literal rule (partial):
        //
        // String literals are untyped (UNKNOWN) in PostgreSQL and can be coerced
        // to match a numeric operator context. In db9, string literals are
        // initially typed as TEXT, which would otherwise reject `TEXT + INT`.
        //
        // We only apply this for *literal* text constants (not TEXT columns, and
        // not explicitly typed TEXT via `::text`), matching the desired contract:
        //
        //   SELECT '100' + 50  -> OK (coerce literal to INT)
        //   SELECT '100'::text + 50 -> ERROR
        //   SELECT text_col + 50 -> ERROR
        if matches!(
            typed_op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::Exp
        ) {
            if is_numeric(&r.data_type) && matches!(l.kind, TypedExprKind::Constant(Value::Text(_)))
            {
                l = self.coerce_if_needed(l, &r.data_type)?;
            } else if is_numeric(&l.data_type)
                && matches!(r.kind, TypedExprKind::Constant(Value::Text(_)))
            {
                r = self.coerce_if_needed(r, &l.data_type)?;
            }
        }

        // PostgreSQL UNKNOWN literal rule (jsonb concat):
        //
        // In PostgreSQL, an untyped string literal on either side of `jsonb ||`
        // is coerced to JSONB via the type input function (so invalid JSON
        // errors, rather than silently falling back to text concatenation).
        //
        //   SELECT '{"a":1}'::jsonb || '{"b":2}'  -> jsonb merge
        //   SELECT '{"a":1}'::jsonb || 'x'        -> ERROR (invalid JSON)
        //
        // We only apply this to semantically-unknown expressions (bare string
        // literals, NULL, unresolved params). Explicit TEXT (`::text`) should
        // remain eligible for text concatenation (PG anynonarray||text).
        if typed_op == BinaryOp::Concat {
            if l.data_type == DataType::Jsonb && self.is_semantically_unknown(&r) {
                r = self.coerce_if_needed(r, &DataType::Jsonb)?;
            } else if r.data_type == DataType::Jsonb && self.is_semantically_unknown(&l) {
                l = self.coerce_if_needed(l, &DataType::Jsonb)?;
            }
        }

        // Use our BinaryOp Display impl (outputs "+", "-", "=", etc.)
        // which maps directly to the operator symbols in binary_op_result_type.
        let op_display = typed_op.to_string();
        let result_type = binary_op_result_type(&op_display, &l.data_type, &r.data_type)
            .or_else(|| {
                // Bitwise operators return the common numeric type
                match &typed_op {
                    BinaryOp::BitwiseAnd
                    | BinaryOp::BitwiseOr
                    | BinaryOp::BitwiseXor
                    | BinaryOp::ShiftLeft
                    | BinaryOp::ShiftRight => common_type(&l.data_type, &r.data_type),
                    BinaryOp::Custom(_) => common_type(&l.data_type, &r.data_type),
                    _ => None,
                }
            })
            .ok_or_else(|| AnalyzerError::OperatorTypeMismatch {
                operator: op_display.clone(),
                left: l.data_type.pg_display_name(),
                right: r.data_type.pg_display_name(),
            })?;

        // Insert implicit casts when operand types differ and a target type exists.
        // Comparisons use comparison_target_type (non-Text side wins) while
        // arithmetic/other operators continue to use common_type.
        if l.data_type != r.data_type {
            let target_type = match typed_op {
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => comparison_target_type(&l.data_type, &r.data_type),
                // PostgreSQL jsonb subtraction is heterogeneous:
                //   jsonb - text / int
                // Do not force both sides to a common type (e.g. Text), which
                // would break operator dispatch at runtime.
                BinaryOp::Sub
                    if matches!(
                        (&l.data_type, &r.data_type),
                        (DataType::Jsonb, DataType::Text)
                            | (DataType::Jsonb, DataType::Int32)
                            | (DataType::Jsonb, DataType::Int64)
                            | (DataType::Jsonb, DataType::Array(_))
                    ) =>
                {
                    None
                }
                _ => common_type(&l.data_type, &r.data_type),
            };

            if let Some(target) = target_type {
                l = self.coerce_if_needed(l, &target)?;
                r = self.coerce_if_needed(r, &target)?;
            }
        }

        Ok(TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(l),
                op: typed_op,
                right: Box::new(r),
            },
            result_type,
        ))
    }

    fn resolves_unknown_pair_to_text(op: &BinaryOp) -> bool {
        matches!(
            op,
            BinaryOp::Concat
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        )
    }

    fn is_ambiguous_unknown_pair_op(op: &BinaryOp) -> bool {
        matches!(
            op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::BitwiseAnd
                | BinaryOp::BitwiseOr
                | BinaryOp::BitwiseXor
                | BinaryOp::ShiftLeft
                | BinaryOp::ShiftRight
                | BinaryOp::Exp
        )
    }

    /// Returns true if `expr` is "semantically unknown" in PostgreSQL's sense:
    /// an unresolved parameter, an expression with DataType::Unknown (bare
    /// string literals and NULL constants), or a NULL constant that was
    /// contextually retyped (its data_type changed but it's still NULL).
    pub(super) fn is_semantically_unknown(&self, expr: &TypedExpr) -> bool {
        match &expr.kind {
            TypedExprKind::Parameter { .. } => self.is_unresolved_param(expr),
            _ => expr.data_type == DataType::Unknown || expr.is_null_constant(),
        }
    }

    pub(super) fn convert_binary_op(
        &self,
        op: &ast::BinaryOperator,
    ) -> Result<BinaryOp, AnalyzerError> {
        use ast::BinaryOperator as SqlOp;
        Ok(match op {
            SqlOp::Plus => BinaryOp::Add,
            SqlOp::Minus => BinaryOp::Sub,
            SqlOp::Multiply => BinaryOp::Mul,
            SqlOp::Divide => BinaryOp::Div,
            SqlOp::Modulo => BinaryOp::Mod,
            SqlOp::Eq => BinaryOp::Eq,
            SqlOp::NotEq => BinaryOp::NotEq,
            SqlOp::Lt => BinaryOp::Lt,
            SqlOp::LtEq => BinaryOp::LtEq,
            SqlOp::Gt => BinaryOp::Gt,
            SqlOp::GtEq => BinaryOp::GtEq,
            SqlOp::And => BinaryOp::And,
            SqlOp::Or => BinaryOp::Or,
            SqlOp::StringConcat => BinaryOp::Concat,
            SqlOp::BitwiseAnd => BinaryOp::BitwiseAnd,
            SqlOp::BitwiseOr => BinaryOp::BitwiseOr,
            SqlOp::BitwiseXor => BinaryOp::BitwiseXor,
            SqlOp::PGBitwiseShiftLeft => BinaryOp::ShiftLeft,
            SqlOp::PGBitwiseShiftRight => BinaryOp::ShiftRight,
            SqlOp::PGRegexMatch => BinaryOp::RegexMatch,
            SqlOp::PGRegexIMatch => BinaryOp::RegexIMatch,
            SqlOp::PGRegexNotMatch => BinaryOp::RegexNotMatch,
            SqlOp::PGRegexNotIMatch => BinaryOp::RegexNotIMatch,
            SqlOp::PGOverlap => BinaryOp::ArrayOverlap,
            SqlOp::PGExp => BinaryOp::Exp,
            SqlOp::PGCustomBinaryOperator(parts) => {
                // OPERATOR(pg_catalog.~) → parts = ["pg_catalog", "~"]
                // OPERATOR(~)            → parts = ["~"]
                // ?|                     → parts = ["?|"]
                let op_symbol = match parts.len() {
                    0 => {
                        return Err(AnalyzerError::Unsupported(
                            "empty OPERATOR() reference".to_string(),
                        ));
                    }
                    1 => parts[0].as_str(),
                    2 => {
                        if !parts[0].eq_ignore_ascii_case("pg_catalog") {
                            return Err(AnalyzerError::Unsupported(format!(
                                "schema-qualified operator: {}.{}",
                                parts[0], parts[1]
                            )));
                        }
                        parts[1].as_str()
                    }
                    _ => {
                        return Err(AnalyzerError::Unsupported(format!(
                            "cross-schema operator reference: {}",
                            parts.join(".")
                        )));
                    }
                };
                Self::resolve_custom_op_symbol(op_symbol)
            }
            other => {
                return Err(AnalyzerError::Unsupported(format!(
                    "binary operator {:?}",
                    other
                )));
            }
        })
    }

    /// Map a custom operator symbol string to the corresponding `BinaryOp`.
    ///
    /// Handles both operators that sqlparser routes through `PGCustomBinaryOperator`
    /// (e.g. `?|`, `@@`) and standard PostgreSQL operators that arrive via
    /// schema-qualified `OPERATOR(pg_catalog.~)` syntax.
    fn resolve_custom_op_symbol(symbol: &str) -> BinaryOp {
        match symbol {
            // Regex
            "~" => BinaryOp::RegexMatch,
            "~*" => BinaryOp::RegexIMatch,
            "!~" => BinaryOp::RegexNotMatch,
            "!~*" => BinaryOp::RegexNotIMatch,
            // Array
            "&&" => BinaryOp::ArrayOverlap,
            "@>" => BinaryOp::ArrayContains,
            "<@" => BinaryOp::ArrayContainedBy,
            // JSON existence
            "?|" => BinaryOp::JsonExistsAny,
            "?&" => BinaryOp::JsonExistsAll,
            // Full-text search
            "@@" => BinaryOp::TsMatch,
            // Standard operators (for OPERATOR(pg_catalog.=) etc.)
            "+" => BinaryOp::Add,
            "-" => BinaryOp::Sub,
            "*" => BinaryOp::Mul,
            "/" => BinaryOp::Div,
            "%" => BinaryOp::Mod,
            "=" => BinaryOp::Eq,
            "<>" | "!=" => BinaryOp::NotEq,
            "<" => BinaryOp::Lt,
            "<=" => BinaryOp::LtEq,
            ">" => BinaryOp::Gt,
            ">=" => BinaryOp::GtEq,
            "||" => BinaryOp::Concat,
            // Fallback
            other => BinaryOp::Custom(other.to_string()),
        }
    }

    pub(super) fn binary_op_to_json_access_op(
        op: &ast::BinaryOperator,
    ) -> Option<ast::JsonOperator> {
        match op {
            ast::BinaryOperator::PGCustomBinaryOperator(parts) => {
                // Strip optional pg_catalog schema prefix
                let op_symbol = match parts.len() {
                    1 => parts[0].as_str(),
                    2 if parts[0].eq_ignore_ascii_case("pg_catalog") => parts[1].as_str(),
                    _ => return None,
                };
                match op_symbol {
                    "->" => Some(ast::JsonOperator::Arrow),
                    "->>" => Some(ast::JsonOperator::LongArrow),
                    "#>" => Some(ast::JsonOperator::HashArrow),
                    "#>>" => Some(ast::JsonOperator::HashLongArrow),
                    "#-" => Some(ast::JsonOperator::HashMinus),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    // -- Helper: unary operators --

    pub(super) fn analyze_unary_op(
        &mut self,
        op: &UnaryOperator,
        expr: &Expr,
    ) -> Result<TypedExpr, AnalyzerError> {
        let operand = self.analyze_expr(expr)?;
        let (typed_op, result_type) = match op {
            UnaryOperator::Not => (UnaryOp::Not, DataType::Boolean),
            UnaryOperator::Plus => (UnaryOp::Plus, operand.data_type.clone()),
            UnaryOperator::Minus => (UnaryOp::Minus, operand.data_type.clone()),
            UnaryOperator::PGBitwiseNot => (UnaryOp::BitwiseNot, operand.data_type.clone()),
            _ => {
                return Err(AnalyzerError::Unsupported(format!(
                    "unary operator {:?}",
                    op,
                )));
            }
        };

        Ok(TypedExpr::new(
            TypedExprKind::UnaryOp {
                op: typed_op,
                operand: Box::new(operand),
            },
            result_type,
        ))
    }

    // -- Helper: IS tests --

    pub(super) fn analyze_is_test(
        &mut self,
        expr: &Expr,
        test: IsTestKind,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let inner = self.analyze_expr(expr)?;
        Ok(TypedExpr::new(
            TypedExprKind::IsTest {
                expr: Box::new(inner),
                test,
                negated,
            },
            DataType::Boolean,
        ))
    }

    // -- Helper: LIKE --

    pub(super) fn analyze_like(
        &mut self,
        expr: &Expr,
        pattern: &Expr,
        escape: &Option<char>,
        case_insensitive: bool,
        negated: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut e = self.analyze_expr(expr)?;
        if let TypedExprKind::Parameter { index } = &e.kind {
            let was_unresolved = self.is_unresolved_param(&e);
            self.resolve_param_type(*index, &DataType::Text)?;
            if was_unresolved {
                e = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Text);
            }
        }

        let mut p = self.analyze_expr(pattern)?;
        if let TypedExprKind::Parameter { index } = &p.kind {
            let was_unresolved = self.is_unresolved_param(&p);
            self.resolve_param_type(*index, &DataType::Text)?;
            if was_unresolved {
                p = TypedExpr::new(TypedExprKind::Parameter { index: *index }, DataType::Text);
            }
        }
        let esc = escape.map(|c| {
            Box::new(TypedExpr::new(
                TypedExprKind::Constant(Value::Text(c.to_string())),
                DataType::Text,
            ))
        });

        Ok(TypedExpr::new(
            TypedExprKind::Like {
                expr: Box::new(e),
                pattern: Box::new(p),
                escape: esc,
                case_insensitive,
                negated,
            },
            DataType::Boolean,
        ))
    }

    // -- Helper: ANY/ALL subquery --

    pub(super) fn analyze_any_all_subquery(
        &mut self,
        left: &Expr,
        compare_op: &BinaryOperator,
        subquery: &ast::Query,
        is_all: bool,
    ) -> Result<TypedExpr, AnalyzerError> {
        let mut left_expr = self.analyze_expr(left)?;
        let analyzed = self.analyze_query(subquery)?;
        if analyzed.output_schema.len() != 1 {
            return Err(AnalyzerError::ScalarSubqueryMultipleColumns {
                got: analyzed.output_schema.len(),
            });
        }

        let right_type = analyzed.output_schema[0].1.clone();
        if left_expr.is_null_constant() {
            left_expr = TypedExpr::null(right_type.clone());
        } else if left_expr.data_type != right_type || self.is_unresolved_param(&left_expr) {
            // Unresolved `$n` parameters are initially seeded as Text.
            // Even when RHS is also Text, we must still run through
            // coerce_if_needed() so resolve_param_type() records the inferred
            // type and finalize_param_types() doesn't raise 42P18.
            let Some(target) = self.comparison_target_type_for_any(&left_expr, &right_type) else {
                return Err(AnalyzerError::OperatorTypeMismatch {
                    operator: compare_op.to_string(),
                    left: left_expr.data_type.pg_display_name(),
                    right: right_type.pg_display_name(),
                });
            };
            left_expr = self.coerce_if_needed(left_expr, &target)?;
        }

        let op = self.any_all_compare_op(compare_op)?;
        Ok(TypedExpr::new(
            TypedExprKind::AnyAll {
                expr: Box::new(left_expr),
                op,
                subquery: Box::new(analyzed),
                is_all,
            },
            DataType::Boolean,
        ))
    }

    pub(super) fn any_all_compare_op(
        &self,
        compare_op: &BinaryOperator,
    ) -> Result<BinaryOp, AnalyzerError> {
        match compare_op {
            BinaryOperator::Eq => Ok(BinaryOp::Eq),
            BinaryOperator::NotEq => Ok(BinaryOp::NotEq),
            BinaryOperator::Lt => Ok(BinaryOp::Lt),
            BinaryOperator::LtEq => Ok(BinaryOp::LtEq),
            BinaryOperator::Gt => Ok(BinaryOp::Gt),
            BinaryOperator::GtEq => Ok(BinaryOp::GtEq),
            other => Err(AnalyzerError::Unsupported(format!(
                "ANY/ALL with operator: {:?}",
                other,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_custom_op_symbol_regex() {
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("~"),
            BinaryOp::RegexMatch
        );
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("~*"),
            BinaryOp::RegexIMatch
        );
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("!~"),
            BinaryOp::RegexNotMatch
        );
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("!~*"),
            BinaryOp::RegexNotIMatch
        );
    }

    #[test]
    fn resolve_custom_op_symbol_json_array_fts() {
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("?|"),
            BinaryOp::JsonExistsAny
        );
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("?&"),
            BinaryOp::JsonExistsAll
        );
        assert_eq!(Analyzer::resolve_custom_op_symbol("@@"), BinaryOp::TsMatch);
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("@>"),
            BinaryOp::ArrayContains
        );
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("<@"),
            BinaryOp::ArrayContainedBy
        );
    }

    #[test]
    fn resolve_custom_op_symbol_standard() {
        assert_eq!(Analyzer::resolve_custom_op_symbol("="), BinaryOp::Eq);
        assert_eq!(Analyzer::resolve_custom_op_symbol("<>"), BinaryOp::NotEq);
        assert_eq!(Analyzer::resolve_custom_op_symbol("<"), BinaryOp::Lt);
        assert_eq!(Analyzer::resolve_custom_op_symbol("<="), BinaryOp::LtEq);
        assert_eq!(Analyzer::resolve_custom_op_symbol(">"), BinaryOp::Gt);
        assert_eq!(Analyzer::resolve_custom_op_symbol(">="), BinaryOp::GtEq);
        assert_eq!(Analyzer::resolve_custom_op_symbol("||"), BinaryOp::Concat);
    }

    #[test]
    fn resolve_custom_op_symbol_unknown_falls_through() {
        assert_eq!(
            Analyzer::resolve_custom_op_symbol("???"),
            BinaryOp::Custom("???".to_string())
        );
    }
}
