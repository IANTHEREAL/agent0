//! `eval_expr_with_sequences` -- entry point for evaluating expressions that may
//! contain sequence functions or user-defined functions.

use crate::storage::TikvStore;
use crate::types::{Row, TableSchema};
use anyhow::Result;
use sqlparser::ast::Expr;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use super::{eval_seq_expr, expr_needs_async_eval, replace::replace_sequence_functions};

pub(crate) async fn eval_expr_with_sequences(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    last_sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    expr: &Expr,
    row: Option<&Row>,
    schema: Option<&TableSchema>,
) -> Result<crate::types::Value> {
    if !expr_needs_async_eval(expr) {
        return eval_seq_expr(expr, row, schema);
    }
    let rewritten = replace_sequence_functions(
        store,
        txn,
        db_id,
        last_sequence_values,
        search_path,
        expr,
        row,
        schema,
    )
    .await?;
    eval_seq_expr(&rewritten, row, schema)
}
