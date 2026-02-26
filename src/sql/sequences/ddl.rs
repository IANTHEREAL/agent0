//! CREATE/DROP SEQUENCE DDL handlers.

use crate::model::{SequenceBacking, SequenceDef, SequenceState};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use sqlparser::ast::{MinMaxValue, ObjectName, SequenceOptions};
use std::sync::Arc;
use tikv_client::Transaction;

use super::{eval_i64, normalize_sequence_name, ExecuteResult};

fn parse_minmax(value: &MinMaxValue) -> Result<Option<i64>> {
    match value {
        MinMaxValue::Empty => Ok(None),
        MinMaxValue::None => Ok(None),
        MinMaxValue::Some(expr) => Ok(Some(eval_i64(expr)?)),
    }
}

pub(crate) async fn execute_create_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    if_not_exists: bool,
    sequence_options: &[SequenceOptions],
) -> Result<ExecuteResult> {
    let (schema, seq_name, full_name) = normalize_sequence_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &schema).await? {
        return Err(anyhow!("schema '{}' does not exist", schema));
    }

    if if_not_exists && store.get_sequence(txn, db_id, &full_name).await?.is_some() {
        return Ok(ExecuteResult::CommandComplete {
            tag: "CREATE SEQUENCE",
        });
    }

    let mut start_value: i64 = 1;
    let mut increment: i64 = 1;
    let mut min_value: i64 = 1;
    let mut max_value: i64 = i64::MAX;
    let mut cache_size: i64 = 1;
    let mut is_cycled: bool = false;

    for opt in sequence_options {
        match opt {
            SequenceOptions::StartWith(expr, _) => start_value = eval_i64(expr)?,
            SequenceOptions::IncrementBy(expr, _) => increment = eval_i64(expr)?,
            SequenceOptions::MinValue(v) => {
                if let Some(val) = parse_minmax(v)? {
                    min_value = val;
                }
            }
            SequenceOptions::MaxValue(v) => {
                if let Some(val) = parse_minmax(v)? {
                    max_value = val;
                }
            }
            SequenceOptions::Cycle(no_cycle) => {
                is_cycled = !*no_cycle;
            }
            SequenceOptions::Cache(expr) => cache_size = eval_i64(expr)?,
        }
    }

    if increment == 0 {
        return Err(anyhow!("Sequence '{}' has invalid INCREMENT 0", full_name));
    }
    if min_value > max_value {
        return Err(anyhow!(
            "Sequence '{}' has invalid MINVALUE/MAXVALUE ({}/{})",
            full_name,
            min_value,
            max_value
        ));
    }
    if start_value < min_value || start_value > max_value {
        return Err(anyhow!(
            "Sequence '{}' START value {} is out of bounds ({}, {})",
            full_name,
            start_value,
            min_value,
            max_value
        ));
    }

    let def = SequenceDef {
        oid: 0,
        schema,
        name: seq_name,
        start_value,
        increment,
        min_value,
        max_value,
        cache_size,
        is_cycled,
        owned_by: None,
        owner: "postgres".to_string(),
        backing: SequenceBacking::Standalone(SequenceState {
            last_value: start_value,
            is_called: false,
        }),
    };

    store.create_sequence(txn, db_id, def).await?;
    Ok(ExecuteResult::CommandComplete {
        tag: "CREATE SEQUENCE",
    })
}

pub(crate) async fn execute_drop_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    for name in names {
        let resolved =
            names::resolve_existing_sequence_name(store.as_ref(), txn, db_id, name, search_path)
                .await?;
        let Some(resolved) = resolved else {
            if !if_exists {
                return Err(SqlError::SequenceNotFound(name.to_string()).into());
            }
            continue;
        };
        let existed = store.drop_sequence(txn, db_id, &resolved.full).await?;
        if !existed && !if_exists {
            return Err(SqlError::SequenceNotFound(resolved.full.clone()).into());
        }
    }
    Ok(ExecuteResult::CommandComplete {
        tag: "DROP SEQUENCE",
    })
}
