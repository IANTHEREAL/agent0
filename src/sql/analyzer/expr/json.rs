//! JSON access expression analysis (`->`, `->>`, `#>`, `#>>`, `#-`, `@>`, `<@`, `@@`).

use sqlparser::ast::{self as ast, Expr};

use crate::model::{DataType, Value};

use super::super::error::AnalyzerError;
use super::super::types::*;
use super::super::Analyzer;

impl<'a> Analyzer<'a> {
    /// Analyze a JSON access expression (`->`, `->>`, `#>`, `#>>`, `#-`,
    /// `@>`, `<@`, `@@`).
    ///
    /// Handles chained access reassociation, precedence fixups for comparison
    /// operators nested inside the RHS by sqlparser, and type validation.
    pub(super) fn analyze_json_access(
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
                // Validate left operand. PostgreSQL defines `#-` for jsonb,
                // while the read-only accessors support both json and jsonb.
                let left_supported = match operator {
                    ast::JsonOperator::HashMinus => matches!(&l.data_type, DataType::Jsonb),
                    _ => matches!(&l.data_type, DataType::Json | DataType::Jsonb),
                };
                if !left_supported {
                    let other = &l.data_type;
                    let op_str = match operator {
                        ast::JsonOperator::Arrow => "->",
                        ast::JsonOperator::LongArrow => "->>",
                        ast::JsonOperator::HashArrow => "#>",
                        ast::JsonOperator::HashLongArrow => "#>>",
                        ast::JsonOperator::HashMinus => "#-",
                        _ => "json_op",
                    };
                    // PG reports bare string literals as type "unknown".
                    let right_type = if matches!(&r.kind, TypedExprKind::Constant(Value::Text(_))) {
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
                let json_op = match operator {
                    ast::JsonOperator::Arrow => JsonAccessOp::Arrow,
                    ast::JsonOperator::LongArrow => JsonAccessOp::LongArrow,
                    ast::JsonOperator::HashArrow => JsonAccessOp::HashArrow,
                    ast::JsonOperator::HashLongArrow => JsonAccessOp::HashLongArrow,
                    ast::JsonOperator::HashMinus => JsonAccessOp::HashMinus,
                    _ => unreachable!(),
                };
                let dt = match json_op {
                    JsonAccessOp::Arrow | JsonAccessOp::HashArrow => l.data_type.clone(),
                    JsonAccessOp::HashMinus => DataType::Jsonb,
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
}
