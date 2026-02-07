use anyhow::{anyhow, Result};
use sqlparser::ast::Query;

mod aggregate;
mod lateral;
mod operator_tree;
mod table_factor;
pub(super) mod using_merge;
mod window;

pub(in crate::sql::executor::select) const JOIN_LOCKING_CLAUSE_UNSUPPORTED: &str =
    "SELECT ... FOR UPDATE/SHARE with JOIN or multiple FROM items is not supported yet";

pub(in crate::sql::executor::select) fn ensure_no_locking_clauses_for_join(
    query: &Query,
) -> Result<()> {
    if query.locks.is_empty() {
        return Ok(());
    }
    Err(anyhow!(JOIN_LOCKING_CLAUSE_UNSUPPORTED))
}
