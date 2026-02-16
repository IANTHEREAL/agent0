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

fn resolve_sequence_name_from_typed_args<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    args: &'a [TypedExpr],
    qctx: &'a QueryContext,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        if args.is_empty() {
            return Err(anyhow!("sequence function requires at least 1 argument"));
        }
        let name_val = eval_static_typed_expr(&args[0], qctx)?;
        sequences::resolve_sequence_full_name_from_value(store, txn, db_id, search_path, name_val)
            .await
    })
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
        match &expr.kind {
            TypedExprKind::FunctionCall {
                func,
                args,
                order_by,
                filter,
            } => {
                let name_upper = func.name.to_uppercase();
                match name_upper.as_str() {
                    "NEXTVAL" => {
                        let seq_name = resolve_sequence_name_from_typed_args(
                            store,
                            txn,
                            db_id,
                            search_path,
                            args,
                            qctx,
                        )
                        .await?;
                        let val = store.nextval_sequence(txn, db_id, &seq_name).await?;
                        sequence_values.insert(seq_name, val);
                        Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Int64(val)),
                            expr.data_type.clone(),
                        ))
                    }
                    "CURRVAL" => {
                        let seq_name = resolve_sequence_name_from_typed_args(
                            store,
                            txn,
                            db_id,
                            search_path,
                            args,
                            qctx,
                        )
                        .await?;
                        let val = sequence_values.get(&seq_name).copied().ok_or_else(|| {
                            anyhow!(
                                "currval of sequence \"{}\" is not yet defined in this session",
                                seq_name
                            )
                        })?;
                        Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Int64(val)),
                            expr.data_type.clone(),
                        ))
                    }
                    "SETVAL" => {
                        let seq_name = resolve_sequence_name_from_typed_args(
                            store,
                            txn,
                            db_id,
                            search_path,
                            args,
                            qctx,
                        )
                        .await?;
                        if args.len() < 2 {
                            return Err(anyhow!("setval requires at least 2 arguments"));
                        }
                        let set_val = match eval_static_typed_expr(&args[1], qctx)? {
                            Value::Int32(n) => n as i64,
                            Value::Int64(n) => n,
                            other => {
                                return Err(anyhow!(
                                    "setval: value must be integer, got {}",
                                    other
                                ));
                            }
                        };
                        let is_called = if args.len() >= 3 {
                            match eval_static_typed_expr(&args[2], qctx)? {
                                Value::Boolean(b) => b,
                                Value::Text(s) => {
                                    matches!(
                                        s.to_lowercase().as_str(),
                                        "true" | "t" | "1" | "yes" | "y"
                                    )
                                }
                                other => {
                                    return Err(anyhow!(
                                        "setval: is_called must be boolean, got {}",
                                        other
                                    ));
                                }
                            }
                        } else {
                            true
                        };
                        store
                            .setval_sequence(txn, db_id, &seq_name, set_val, is_called)
                            .await?;
                        sequence_values.insert(seq_name, set_val);
                        Ok(TypedExpr::new(
                            TypedExprKind::Constant(Value::Int64(set_val)),
                            expr.data_type.clone(),
                        ))
                    }
                    _ => {
                        let mut new_args = Vec::with_capacity(args.len());
                        for a in args {
                            new_args.push(
                                materialize_sequences_in_typed_expr(
                                    store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    a,
                                    qctx,
                                )
                                .await?,
                            );
                        }
                        let mut new_order_by = Vec::with_capacity(order_by.len());
                        for ob in order_by {
                            new_order_by.push(TypedOrderByExpr {
                                expr: materialize_sequences_in_typed_expr(
                                    store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &ob.expr,
                                    qctx,
                                )
                                .await?,
                                asc: ob.asc,
                                nulls_first: ob.nulls_first,
                            });
                        }
                        let new_filter = match filter {
                            Some(f) => Some(Box::new(
                                materialize_sequences_in_typed_expr(
                                    store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    f,
                                    qctx,
                                )
                                .await?,
                            )),
                            None => None,
                        };
                        Ok(TypedExpr {
                            kind: TypedExprKind::FunctionCall {
                                func: func.clone(),
                                args: new_args,
                                order_by: new_order_by,
                                filter: new_filter,
                            },
                            data_type: expr.data_type.clone(),
                        })
                    }
                }
            }
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                order_by,
                filter,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            a,
                            qctx,
                        )
                        .await?,
                    );
                }
                let mut new_order_by = Vec::with_capacity(order_by.len());
                for ob in order_by {
                    new_order_by.push(TypedOrderByExpr {
                        expr: materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &ob.expr,
                            qctx,
                        )
                        .await?,
                        asc: ob.asc,
                        nulls_first: ob.nulls_first,
                    });
                }
                let new_filter = match filter {
                    Some(f) => Some(Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            f,
                            qctx,
                        )
                        .await?,
                    )),
                    None => None,
                };
                Ok(TypedExpr {
                    kind: TypedExprKind::AggregateCall {
                        func: func.clone(),
                        args: new_args,
                        distinct: *distinct,
                        order_by: new_order_by,
                        filter: new_filter,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::WindowCall {
                func,
                args,
                partition_by,
                order_by,
                window_frame,
            } => {
                let mut new_args = Vec::with_capacity(args.len());
                for a in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            a,
                            qctx,
                        )
                        .await?,
                    );
                }

                let mut new_partition_by = Vec::with_capacity(partition_by.len());
                for p in partition_by {
                    new_partition_by.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            p,
                            qctx,
                        )
                        .await?,
                    );
                }

                let mut new_order_by = Vec::with_capacity(order_by.len());
                for ob in order_by {
                    new_order_by.push(TypedOrderByExpr {
                        expr: materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &ob.expr,
                            qctx,
                        )
                        .await?,
                        asc: ob.asc,
                        nulls_first: ob.nulls_first,
                    });
                }

                let new_window_frame = match window_frame {
                    Some(frame) => {
                        let new_start = match &frame.start {
                            WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
                            WindowFrameBound::Preceding(v) => {
                                WindowFrameBound::Preceding(match v {
                                    Some(e) => Some(Box::new(
                                        materialize_sequences_in_typed_expr(
                                            store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            e,
                                            qctx,
                                        )
                                        .await?,
                                    )),
                                    None => None,
                                })
                            }
                            WindowFrameBound::Following(v) => {
                                WindowFrameBound::Following(match v {
                                    Some(e) => Some(Box::new(
                                        materialize_sequences_in_typed_expr(
                                            store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            e,
                                            qctx,
                                        )
                                        .await?,
                                    )),
                                    None => None,
                                })
                            }
                        };

                        let new_end = match &frame.end {
                            Some(bound) => Some(match bound {
                                WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
                                WindowFrameBound::Preceding(v) => {
                                    WindowFrameBound::Preceding(match v {
                                        Some(e) => Some(Box::new(
                                            materialize_sequences_in_typed_expr(
                                                store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                e,
                                                qctx,
                                            )
                                            .await?,
                                        )),
                                        None => None,
                                    })
                                }
                                WindowFrameBound::Following(v) => {
                                    WindowFrameBound::Following(match v {
                                        Some(e) => Some(Box::new(
                                            materialize_sequences_in_typed_expr(
                                                store,
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                e,
                                                qctx,
                                            )
                                            .await?,
                                        )),
                                        None => None,
                                    })
                                }
                            }),
                            None => None,
                        };

                        Some(WindowFrame {
                            units: frame.units,
                            start: new_start,
                            end: new_end,
                        })
                    }
                    None => None,
                };

                Ok(TypedExpr {
                    kind: TypedExprKind::WindowCall {
                        func: func.clone(),
                        args: new_args,
                        partition_by: new_partition_by,
                        order_by: new_order_by,
                        window_frame: new_window_frame,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::BinaryOp { left, right, op } => Ok(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            qctx,
                        )
                        .await?,
                    ),
                    op: op.clone(),
                    right: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            qctx,
                        )
                        .await?,
                    ),
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::UnaryOp { op, operand } => Ok(TypedExpr {
                kind: TypedExprKind::UnaryOp {
                    op: *op,
                    operand: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            operand,
                            qctx,
                        )
                        .await?,
                    ),
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::Cast {
                expr: inner,
                target_type,
                cast_context,
            } => Ok(TypedExpr {
                kind: TypedExprKind::Cast {
                    expr: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            qctx,
                        )
                        .await?,
                    ),
                    target_type: target_type.clone(),
                    cast_context: *cast_context,
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::IsTest {
                expr: inner,
                test,
                negated,
            } => Ok(TypedExpr {
                kind: TypedExprKind::IsTest {
                    expr: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            qctx,
                        )
                        .await?,
                    ),
                    test: *test,
                    negated: *negated,
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::Between {
                expr: inner,
                low,
                high,
                negated,
            } => Ok(TypedExpr {
                kind: TypedExprKind::Between {
                    expr: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            qctx,
                        )
                        .await?,
                    ),
                    low: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            low,
                            qctx,
                        )
                        .await?,
                    ),
                    high: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            high,
                            qctx,
                        )
                        .await?,
                    ),
                    negated: *negated,
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::InList {
                expr: inner,
                list,
                negated,
            } => {
                let mut new_list = Vec::with_capacity(list.len());
                for item in list {
                    new_list.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            item,
                            qctx,
                        )
                        .await?,
                    );
                }
                Ok(TypedExpr {
                    kind: TypedExprKind::InList {
                        expr: Box::new(
                            materialize_sequences_in_typed_expr(
                                store,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                inner,
                                qctx,
                            )
                            .await?,
                        ),
                        list: new_list,
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::Like {
                expr: inner,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => {
                let new_escape = match escape {
                    Some(e) => Some(Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            qctx,
                        )
                        .await?,
                    )),
                    None => None,
                };
                Ok(TypedExpr {
                    kind: TypedExprKind::Like {
                        expr: Box::new(
                            materialize_sequences_in_typed_expr(
                                store,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                inner,
                                qctx,
                            )
                            .await?,
                        ),
                        pattern: Box::new(
                            materialize_sequences_in_typed_expr(
                                store,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                pattern,
                                qctx,
                            )
                            .await?,
                        ),
                        escape: new_escape,
                        case_insensitive: *case_insensitive,
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::SimilarTo {
                expr: inner,
                pattern,
                escape,
                negated,
            } => {
                let new_escape = match escape {
                    Some(e) => Some(Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            qctx,
                        )
                        .await?,
                    )),
                    None => None,
                };
                Ok(TypedExpr {
                    kind: TypedExprKind::SimilarTo {
                        expr: Box::new(
                            materialize_sequences_in_typed_expr(
                                store,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                inner,
                                qctx,
                            )
                            .await?,
                        ),
                        pattern: Box::new(
                            materialize_sequences_in_typed_expr(
                                store,
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                pattern,
                                qctx,
                            )
                            .await?,
                        ),
                        escape: new_escape,
                        negated: *negated,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => {
                let new_operand = match operand {
                    Some(op) => Some(Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            op,
                            qctx,
                        )
                        .await?,
                    )),
                    None => None,
                };
                let mut new_when_clauses = Vec::with_capacity(when_clauses.len());
                for (w, t) in when_clauses {
                    new_when_clauses.push((
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            w,
                            qctx,
                        )
                        .await?,
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            t,
                            qctx,
                        )
                        .await?,
                    ));
                }
                let new_else = match else_result {
                    Some(e) => Some(Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            qctx,
                        )
                        .await?,
                    )),
                    None => None,
                };
                Ok(TypedExpr {
                    kind: TypedExprKind::Case {
                        operand: new_operand,
                        when_clauses: new_when_clauses,
                        else_result: new_else,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::Coalesce(args) => {
                let mut new_args = Vec::with_capacity(args.len());
                for arg in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            arg,
                            qctx,
                        )
                        .await?,
                    );
                }
                Ok(TypedExpr {
                    kind: TypedExprKind::Coalesce(new_args),
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::NullIf(a, b) => Ok(TypedExpr {
                kind: TypedExprKind::NullIf(
                    Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            a,
                            qctx,
                        )
                        .await?,
                    ),
                    Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            b,
                            qctx,
                        )
                        .await?,
                    ),
                ),
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::MinMax { args, is_greatest } => {
                let mut new_args = Vec::with_capacity(args.len());
                for arg in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            arg,
                            qctx,
                        )
                        .await?,
                    );
                }
                Ok(TypedExpr {
                    kind: TypedExprKind::MinMax {
                        args: new_args,
                        is_greatest: *is_greatest,
                    },
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::ArrayLiteral(args) => {
                let mut new_args = Vec::with_capacity(args.len());
                for arg in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            arg,
                            qctx,
                        )
                        .await?,
                    );
                }
                Ok(TypedExpr {
                    kind: TypedExprKind::ArrayLiteral(new_args),
                    data_type: expr.data_type.clone(),
                })
            }
            TypedExprKind::ArrayIndex { array, index } => Ok(TypedExpr {
                kind: TypedExprKind::ArrayIndex {
                    array: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            array,
                            qctx,
                        )
                        .await?,
                    ),
                    index: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            index,
                            qctx,
                        )
                        .await?,
                    ),
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::JsonAccess {
                expr: inner,
                path,
                operator,
            } => Ok(TypedExpr {
                kind: TypedExprKind::JsonAccess {
                    expr: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            qctx,
                        )
                        .await?,
                    ),
                    path: Box::new(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            path,
                            qctx,
                        )
                        .await?,
                    ),
                    operator: *operator,
                },
                data_type: expr.data_type.clone(),
            }),
            TypedExprKind::Row(args) => {
                let mut new_args = Vec::with_capacity(args.len());
                for arg in args {
                    new_args.push(
                        materialize_sequences_in_typed_expr(
                            store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            arg,
                            qctx,
                        )
                        .await?,
                    );
                }
                Ok(TypedExpr {
                    kind: TypedExprKind::Row(new_args),
                    data_type: expr.data_type.clone(),
                })
            }
            _ => Ok(expr.clone()),
        }
    })
}
