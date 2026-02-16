//! TypedExpr rewrite helpers.
//!
//! Rewrites typed expression trees for executor-time materialization tasks.

use crate::sql::analyzer::types::{
    TypedExpr, TypedExprKind, TypedOrderByExpr, WindowFrame, WindowFrameBound,
};
use crate::sql::expr::static_eval::eval_static_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::storage::TikvStore;
use crate::types::Value;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

type RewriteFuture<'a> = Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceFunction {
    NextVal,
    CurrVal,
    SetVal,
}

struct SequenceMaterializeCtx<'a> {
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    qctx: &'a QueryContext,
}

impl<'a> SequenceMaterializeCtx<'a> {
    fn rewrite_expr<'b>(&'b mut self, expr: &'b TypedExpr) -> RewriteFuture<'b> {
        Box::pin(async move {
            let kind = match &expr.kind {
                TypedExprKind::Constant(_)
                | TypedExprKind::ColumnRef { .. }
                | TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::Default => return Ok(expr.clone()),
                TypedExprKind::FunctionCall {
                    func,
                    args,
                    order_by,
                    filter,
                } => match sequence_function_kind(&func.name) {
                    Some(SequenceFunction::NextVal) => {
                        let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                        let val = self
                            .store
                            .nextval_sequence(self.txn, self.db_id, &seq_name)
                            .await?;
                        self.sequence_values.insert(seq_name, val);
                        TypedExprKind::Constant(Value::Int64(val))
                    }
                    Some(SequenceFunction::CurrVal) => {
                        let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                        let val =
                            self.sequence_values
                                .get(&seq_name)
                                .copied()
                                .ok_or_else(|| {
                                    anyhow!(
                                "currval of sequence \"{}\" is not yet defined in this session",
                                seq_name
                            )
                                })?;
                        TypedExprKind::Constant(Value::Int64(val))
                    }
                    Some(SequenceFunction::SetVal) => {
                        let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                        if args.len() < 2 {
                            return Err(anyhow!("setval requires at least 2 arguments"));
                        }

                        let set_val = match eval_static_typed_expr(&args[1], self.qctx)? {
                            Value::Int32(n) => i64::from(n),
                            Value::Int64(n) => n,
                            other => {
                                return Err(anyhow!(
                                    "setval: value must be integer, got {}",
                                    other
                                ));
                            }
                        };

                        let is_called = if args.len() >= 3 {
                            parse_setval_is_called(eval_static_typed_expr(&args[2], self.qctx)?)?
                        } else {
                            true
                        };

                        self.store
                            .setval_sequence(self.txn, self.db_id, &seq_name, set_val, is_called)
                            .await?;
                        self.sequence_values.insert(seq_name, set_val);
                        TypedExprKind::Constant(Value::Int64(set_val))
                    }
                    None => TypedExprKind::FunctionCall {
                        func: func.clone(),
                        args: self.rewrite_exprs(args).await?,
                        order_by: self.rewrite_order_by(order_by).await?,
                        filter: self.rewrite_opt_expr(filter).await?,
                    },
                },
                TypedExprKind::AggregateCall {
                    func,
                    args,
                    distinct,
                    order_by,
                    filter,
                } => TypedExprKind::AggregateCall {
                    func: func.clone(),
                    args: self.rewrite_exprs(args).await?,
                    distinct: *distinct,
                    order_by: self.rewrite_order_by(order_by).await?,
                    filter: self.rewrite_opt_expr(filter).await?,
                },
                TypedExprKind::WindowCall {
                    func,
                    args,
                    partition_by,
                    order_by,
                    window_frame,
                } => TypedExprKind::WindowCall {
                    func: func.clone(),
                    args: self.rewrite_exprs(args).await?,
                    partition_by: self.rewrite_exprs(partition_by).await?,
                    order_by: self.rewrite_order_by(order_by).await?,
                    window_frame: self.rewrite_window_frame(window_frame).await?,
                },
                TypedExprKind::BinaryOp { left, op, right } => TypedExprKind::BinaryOp {
                    left: Box::new(self.rewrite_expr(left).await?),
                    op: op.clone(),
                    right: Box::new(self.rewrite_expr(right).await?),
                },
                TypedExprKind::UnaryOp { op, operand } => TypedExprKind::UnaryOp {
                    op: *op,
                    operand: Box::new(self.rewrite_expr(operand).await?),
                },
                TypedExprKind::Cast {
                    expr: inner,
                    target_type,
                    cast_context,
                } => TypedExprKind::Cast {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    target_type: target_type.clone(),
                    cast_context: *cast_context,
                },
                TypedExprKind::IsTest {
                    expr: inner,
                    test,
                    negated,
                } => TypedExprKind::IsTest {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    test: *test,
                    negated: *negated,
                },
                TypedExprKind::Between {
                    expr: inner,
                    low,
                    high,
                    negated,
                } => TypedExprKind::Between {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    low: Box::new(self.rewrite_expr(low).await?),
                    high: Box::new(self.rewrite_expr(high).await?),
                    negated: *negated,
                },
                TypedExprKind::InList {
                    expr: inner,
                    list,
                    negated,
                } => TypedExprKind::InList {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    list: self.rewrite_exprs(list).await?,
                    negated: *negated,
                },
                TypedExprKind::Like {
                    expr: inner,
                    pattern,
                    escape,
                    case_insensitive,
                    negated,
                } => TypedExprKind::Like {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    pattern: Box::new(self.rewrite_expr(pattern).await?),
                    escape: self.rewrite_opt_expr(escape).await?,
                    case_insensitive: *case_insensitive,
                    negated: *negated,
                },
                TypedExprKind::SimilarTo {
                    expr: inner,
                    pattern,
                    escape,
                    negated,
                } => TypedExprKind::SimilarTo {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    pattern: Box::new(self.rewrite_expr(pattern).await?),
                    escape: self.rewrite_opt_expr(escape).await?,
                    negated: *negated,
                },
                TypedExprKind::Case {
                    operand,
                    when_clauses,
                    else_result,
                } => {
                    let operand = self.rewrite_opt_expr(operand).await?;
                    let mut rewritten_when = Vec::with_capacity(when_clauses.len());
                    for (when_expr, then_expr) in when_clauses {
                        rewritten_when.push((
                            self.rewrite_expr(when_expr).await?,
                            self.rewrite_expr(then_expr).await?,
                        ));
                    }
                    TypedExprKind::Case {
                        operand,
                        when_clauses: rewritten_when,
                        else_result: self.rewrite_opt_expr(else_result).await?,
                    }
                }
                TypedExprKind::Coalesce(args) => {
                    TypedExprKind::Coalesce(self.rewrite_exprs(args).await?)
                }
                TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
                    Box::new(self.rewrite_expr(a).await?),
                    Box::new(self.rewrite_expr(b).await?),
                ),
                TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
                    args: self.rewrite_exprs(args).await?,
                    is_greatest: *is_greatest,
                },
                TypedExprKind::InSubquery {
                    expr: inner,
                    subquery,
                    negated,
                } => TypedExprKind::InSubquery {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    subquery: subquery.clone(),
                    negated: *negated,
                },
                TypedExprKind::AnyAll {
                    expr: inner,
                    op,
                    subquery,
                    is_all,
                } => TypedExprKind::AnyAll {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    op: op.clone(),
                    subquery: subquery.clone(),
                    is_all: *is_all,
                },
                TypedExprKind::ArrayLiteral(args) => {
                    TypedExprKind::ArrayLiteral(self.rewrite_exprs(args).await?)
                }
                TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
                    array: Box::new(self.rewrite_expr(array).await?),
                    index: Box::new(self.rewrite_expr(index).await?),
                },
                TypedExprKind::JsonAccess {
                    expr: inner,
                    path,
                    operator,
                } => TypedExprKind::JsonAccess {
                    expr: Box::new(self.rewrite_expr(inner).await?),
                    path: Box::new(self.rewrite_expr(path).await?),
                    operator: *operator,
                },
                TypedExprKind::Row(args) => TypedExprKind::Row(self.rewrite_exprs(args).await?),
            };

            Ok(with_data_type(expr, kind))
        })
    }

    fn resolve_sequence_name_from_typed_args<'b>(
        &'b mut self,
        args: &'b [TypedExpr],
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'b>> {
        Box::pin(async move {
            if args.is_empty() {
                return Err(anyhow!("sequence function requires at least 1 argument"));
            }

            let name_val = eval_static_typed_expr(&args[0], self.qctx)?;
            sequences::resolve_sequence_full_name_from_value(
                self.store,
                self.txn,
                self.db_id,
                self.search_path,
                name_val,
            )
            .await
        })
    }

    async fn rewrite_exprs(&mut self, exprs: &[TypedExpr]) -> Result<Vec<TypedExpr>> {
        let mut rewritten = Vec::with_capacity(exprs.len());
        for expr in exprs {
            rewritten.push(self.rewrite_expr(expr).await?);
        }
        Ok(rewritten)
    }

    async fn rewrite_opt_expr(
        &mut self,
        expr: &Option<Box<TypedExpr>>,
    ) -> Result<Option<Box<TypedExpr>>> {
        match expr {
            Some(inner) => Ok(Some(Box::new(self.rewrite_expr(inner).await?))),
            None => Ok(None),
        }
    }

    async fn rewrite_order_by(
        &mut self,
        order_by: &[TypedOrderByExpr],
    ) -> Result<Vec<TypedOrderByExpr>> {
        let mut rewritten = Vec::with_capacity(order_by.len());
        for ob in order_by {
            rewritten.push(TypedOrderByExpr {
                expr: self.rewrite_expr(&ob.expr).await?,
                asc: ob.asc,
                nulls_first: ob.nulls_first,
            });
        }
        Ok(rewritten)
    }

    async fn rewrite_window_frame(
        &mut self,
        frame: &Option<WindowFrame>,
    ) -> Result<Option<WindowFrame>> {
        match frame {
            Some(f) => Ok(Some(WindowFrame {
                units: f.units,
                start: self.rewrite_window_frame_bound(&f.start).await?,
                end: match &f.end {
                    Some(bound) => Some(self.rewrite_window_frame_bound(bound).await?),
                    None => None,
                },
            })),
            None => Ok(None),
        }
    }

    async fn rewrite_window_frame_bound(
        &mut self,
        bound: &WindowFrameBound,
    ) -> Result<WindowFrameBound> {
        match bound {
            WindowFrameBound::CurrentRow => Ok(WindowFrameBound::CurrentRow),
            WindowFrameBound::Preceding(v) => match v {
                Some(expr) => Ok(WindowFrameBound::Preceding(Some(Box::new(
                    self.rewrite_expr(expr).await?,
                )))),
                None => Ok(WindowFrameBound::Preceding(None)),
            },
            WindowFrameBound::Following(v) => match v {
                Some(expr) => Ok(WindowFrameBound::Following(Some(Box::new(
                    self.rewrite_expr(expr).await?,
                )))),
                None => Ok(WindowFrameBound::Following(None)),
            },
        }
    }
}

fn sequence_function_kind(name: &str) -> Option<SequenceFunction> {
    match name.to_ascii_uppercase().as_str() {
        "NEXTVAL" => Some(SequenceFunction::NextVal),
        "CURRVAL" => Some(SequenceFunction::CurrVal),
        "SETVAL" => Some(SequenceFunction::SetVal),
        _ => None,
    }
}

fn parse_setval_is_called(v: Value) -> Result<bool> {
    match v {
        Value::Boolean(b) => Ok(b),
        Value::Text(s) => Ok(matches!(
            s.to_lowercase().as_str(),
            "true" | "t" | "1" | "yes" | "y"
        )),
        other => Err(anyhow!("setval: is_called must be boolean, got {}", other)),
    }
}

fn with_data_type(expr: &TypedExpr, kind: TypedExprKind) -> TypedExpr {
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

/// Rewrite sequence function calls (`nextval`/`currval`/`setval`) into constants.
pub fn materialize_sequences_in_typed_expr<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut HashMap<String, i64>,
    search_path: &'a [String],
    expr: &'a TypedExpr,
    qctx: &'a QueryContext,
) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
    Box::pin(async move {
        let mut ctx = SequenceMaterializeCtx {
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            qctx,
        };
        ctx.rewrite_expr(expr).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_function_kind_is_case_insensitive() {
        assert_eq!(
            sequence_function_kind("nextval"),
            Some(SequenceFunction::NextVal)
        );
        assert_eq!(
            sequence_function_kind("CURRVAL"),
            Some(SequenceFunction::CurrVal)
        );
        assert_eq!(
            sequence_function_kind("SetVal"),
            Some(SequenceFunction::SetVal)
        );
        assert_eq!(sequence_function_kind("abs"), None);
    }

    #[test]
    fn parse_setval_is_called_accepts_bool_and_text() {
        assert!(parse_setval_is_called(Value::Boolean(true)).unwrap());
        assert!(!parse_setval_is_called(Value::Boolean(false)).unwrap());
        assert!(parse_setval_is_called(Value::Text("YES".to_string())).unwrap());
        assert!(!parse_setval_is_called(Value::Text("n".to_string())).unwrap());
    }

    #[test]
    fn parse_setval_is_called_rejects_non_bool() {
        let err = parse_setval_is_called(Value::Int32(1)).unwrap_err();
        assert!(err.to_string().contains("is_called must be boolean"));
    }
}
