use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlparser::ast::Expr;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::expr::eval_expr;
use crate::types::{ColumnDef, DataType, Row, TableSchema};

#[derive(Debug)]
pub struct ProjectOperator {
    child: BoxedOperator,
    expressions: Vec<Expr>,
    output_names: Vec<String>,
    output_schema: TableSchema,
    opened: bool,
}

impl ProjectOperator {
    pub fn new(
        child: BoxedOperator,
        expressions: Vec<Expr>,
        output_names: Vec<String>,
        output_types: Vec<DataType>,
    ) -> Self {
        let output_schema = TableSchema {
            name: "projection".to_string(),
            table_id: 0,
            columns: output_names
                .iter()
                .zip(output_types.iter())
                .map(|(name, dt)| ColumnDef {
                    name: name.clone(),
                    data_type: dt.clone(),
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        Self {
            child,
            expressions,
            output_names,
            output_schema,
            opened: false,
        }
    }

    fn project_row(&self, input: &Row) -> Result<Row> {
        let child_schema = self.child.schema();
        let mut values = Vec::with_capacity(self.expressions.len());

        for expr in &self.expressions {
            let value = eval_expr(expr, Some(input), Some(child_schema))?;
            values.push(value);
        }

        Ok(Row::new(values))
    }
}

#[async_trait]
impl PhysicalOperator for ProjectOperator {
    fn schema(&self) -> &TableSchema {
        &self.output_schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.open(ctx).await?;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if let Some(row) = self.child.next(ctx).await? {
            let projected = self.project_row(&row)?;
            Ok(Some(projected))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.opened = false;
        Ok(())
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.child.as_ref()]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![self.child.as_mut()]
    }

    fn name(&self) -> &'static str {
        "Project"
    }

    fn explain_info(&self) -> Option<String> {
        let cols = self.output_names.join(", ");
        Some(format!("columns=[{}]", cols))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ColumnDef;
    use sqlparser::ast::Ident;

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
                ColumnDef {
                    name: "age".to_string(),
                    data_type: DataType::Int32,
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
    fn test_project_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let expressions = vec![
            Expr::Identifier(Ident::new("id")),
            Expr::Identifier(Ident::new("name")),
        ];
        let output_names = vec!["id".to_string(), "name".to_string()];
        let output_types = vec![DataType::Int32, DataType::Text];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        assert_eq!(project.name(), "Project");
        assert_eq!(project.schema().columns.len(), 2);
        assert_eq!(project.schema().columns[0].name, "id");
        assert_eq!(project.schema().columns[1].name, "name");
    }

    #[test]
    fn test_project_explain_info() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let expressions = vec![
            Expr::Identifier(Ident::new("id")),
            Expr::Identifier(Ident::new("name")),
        ];
        let output_names = vec!["id".to_string(), "name".to_string()];
        let output_types = vec![DataType::Int32, DataType::Text];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        assert_eq!(
            project.explain_info(),
            Some("columns=[id, name]".to_string())
        );
    }
}
