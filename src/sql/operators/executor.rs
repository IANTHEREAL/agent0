use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tikv_client::Transaction;

use super::{BoxedOperator, ExecutionContext};
use crate::storage::TikvStore;
use crate::types::Row;

pub async fn execute_operator_tree(
    operator: &mut BoxedOperator,
    txn: &mut Transaction,
    store: Arc<TikvStore>,
    search_path: &[String],
    sequence_values: &mut HashMap<String, i64>,
) -> Result<Vec<Row>> {
    let mut ctx = ExecutionContext::new(txn, store, search_path, sequence_values);

    operator.open(&mut ctx).await?;

    let mut rows = Vec::new();
    while let Some(row) = operator.next(&mut ctx).await? {
        rows.push(row);
    }

    operator.close(&mut ctx).await?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use crate::sql::operators::OperatorBuilder;
    use crate::types::{ColumnDef, DataType, TableSchema};
    use sqlparser::ast::{BinaryOperator, Expr, Ident, OrderByExpr};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[test]
    fn test_operator_builder_creates_valid_tree() {
        let schema = test_schema();
        let predicate = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "5".to_string(),
                false,
            ))),
        };
        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("id")),
            asc: Some(true),
            nulls_first: None,
        }];

        let op = OperatorBuilder::scan(schema)
            .filter(predicate)
            .sort(order_by)
            .limit(Some(10), 0)
            .build();

        assert_eq!(op.name(), "Limit");
        assert_eq!(op.children()[0].name(), "Sort");
        assert_eq!(op.children()[0].children()[0].name(), "Filter");
        assert_eq!(
            op.children()[0].children()[0].children()[0].name(),
            "TableScan"
        );
    }
}
