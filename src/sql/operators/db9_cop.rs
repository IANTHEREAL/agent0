use super::{ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema};
use crate::sql::optimizer::physical_plan::{Db9CopOp, Db9CopScan};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

#[derive(Debug)]
pub struct Db9CopOperator {
    table_schema: TableSchema,
    output_schema: TableSchema,
    scan: Db9CopScan,
    ops: Vec<Db9CopOp>,
    rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl Db9CopOperator {
    pub fn new(
        table_schema: TableSchema,
        output_schema: TableSchema,
        scan: Db9CopScan,
        ops: Vec<Db9CopOp>,
    ) -> Self {
        Self {
            table_schema,
            output_schema,
            scan,
            ops,
            rows: Vec::new(),
            position: 0,
            opened: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for Db9CopOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        let store = ctx.store.clone();
        let db_id = ctx.db_id;
        self.position = 0;
        self.rows = store
            .cop_select(
                ctx.txn,
                db_id,
                &self.table_schema,
                &self.scan,
                &self.ops,
                &self.output_schema,
            )
            .await?;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.rows.len() {
            let row = self.rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.rows.clear();
        self.position = 0;
        self.opened = false;
        Ok(())
    }

    #[cfg(test)]
    fn name(&self) -> &'static str {
        "Db9Cop"
    }

    #[cfg(test)]
    fn explain_info(&self) -> Option<String> {
        Some(format!("table={}", self.table_schema.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType};

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name.to_string(),
            1,
            vec![ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
                is_dropped: false,
            }],
            vec![0],
        )
    }

    #[test]
    fn db9_cop_operator_reports_output_schema() {
        let base_schema = schema("public.t");
        let output_schema = schema("db9_cop_output");
        let operator =
            Db9CopOperator::new(base_schema, output_schema.clone(), Db9CopScan::Seq, vec![]);

        assert_eq!(operator.name(), "Db9Cop");
        assert_eq!(operator.schema().name, output_schema.name);
        assert_eq!(operator.schema().columns.len(), 1);
    }
}
