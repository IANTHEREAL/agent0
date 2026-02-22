use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::classify::needs_async;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

/// Identifies set-returning function kinds that expand one input row to many.
#[derive(Copy, Clone, Debug)]
pub(crate) enum SrfKind {
    Unnest,
    RegexpSplitToTable,
    RegexpMatches,
    EvalFunctionArray,
}

pub(crate) fn detect_srf(expr: &TypedExpr) -> Option<SrfKind> {
    let TypedExprKind::FunctionCall { func, .. } = &expr.kind else {
        return None;
    };
    match func.name.to_ascii_uppercase().as_str() {
        "UNNEST" => Some(SrfKind::Unnest),
        "REGEXP_SPLIT_TO_TABLE" => Some(SrfKind::RegexpSplitToTable),
        "REGEXP_MATCHES" => Some(SrfKind::RegexpMatches),
        "JSONB_OBJECT_KEYS"
        | "JSONB_ARRAY_ELEMENTS"
        | "JSONB_ARRAY_ELEMENTS_TEXT"
        | "JSONB_EACH"
        | "JSONB_EACH_TEXT" => Some(SrfKind::EvalFunctionArray),
        _ => None,
    }
}

fn regexp_captures_to_values(caps: &regex::Captures<'_>) -> Vec<Value> {
    if caps.len() > 1 {
        (1..caps.len())
            .map(|idx| match caps.get(idx) {
                Some(m) => Value::Text(m.as_str().to_string()),
                None => Value::Null,
            })
            .collect()
    } else {
        caps.get(0)
            .map(|m| vec![Value::Text(m.as_str().to_string())])
            .unwrap_or_default()
    }
}

/// Evaluate a set-returning function and return the expanded values.
pub(crate) fn eval_srf(
    kind: SrfKind,
    expr: &TypedExpr,
    input: &Row,
    query_ctx: &QueryContext,
) -> Result<Vec<Value>> {
    let TypedExprKind::FunctionCall { args, .. } = &expr.kind else {
        return Ok(Vec::new());
    };
    match kind {
        SrfKind::Unnest => {
            if let Some(arg_expr) = args.first() {
                match eval_typed_expr(arg_expr, input, query_ctx)? {
                    Value::Array(arr) => Ok(arr),
                    Value::Null => Ok(Vec::new()),
                    other => Ok(vec![other]),
                }
            } else {
                Ok(Vec::new())
            }
        }
        SrfKind::RegexpSplitToTable => {
            let (Some(arg0), Some(arg1)) = (args.get(0), args.get(1)) else {
                return Err(anyhow!(
                    "regexp_split_to_table requires at least 2 arguments"
                ));
            };

            let source_val = eval_typed_expr(arg0, input, query_ctx)?;
            let source = match source_val {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            };

            let pattern_val = eval_typed_expr(arg1, input, query_ctx)?;
            let pattern = match pattern_val {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            };

            match (source, pattern) {
                (Some(source), Some(pattern)) => {
                    let flags = if let Some(arg2) = args.get(2) {
                        match eval_typed_expr(arg2, input, query_ctx)? {
                            Value::Text(s) => s,
                            Value::Null => String::new(),
                            v => v.to_string(),
                        }
                    } else {
                        String::new()
                    };
                    let case_insensitive = flags.to_ascii_lowercase().contains('i');
                    let regex_pattern = if case_insensitive {
                        format!("(?i){}", pattern)
                    } else {
                        pattern
                    };
                    let re = regex::Regex::new(&regex_pattern)
                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;

                    let mut parts = Vec::new();
                    let mut last_end = 0usize;
                    for m in re.find_iter(&source) {
                        parts.push(Value::Text(source[last_end..m.start()].to_string()));
                        last_end = m.end();
                    }
                    parts.push(Value::Text(source[last_end..].to_string()));
                    Ok(parts)
                }
                _ => Ok(Vec::new()),
            }
        }
        SrfKind::RegexpMatches => {
            let (Some(arg0), Some(arg1)) = (args.get(0), args.get(1)) else {
                return Err(anyhow!("regexp_matches requires at least 2 arguments"));
            };

            let source_val = eval_typed_expr(arg0, input, query_ctx)?;
            let source = match source_val {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            };

            let pattern_val = eval_typed_expr(arg1, input, query_ctx)?;
            let pattern = match pattern_val {
                Value::Text(s) => Some(s),
                Value::Null => None,
                v => Some(v.to_string()),
            };

            match (source, pattern) {
                (Some(source), Some(pattern)) => {
                    let flags = if let Some(arg2) = args.get(2) {
                        match eval_typed_expr(arg2, input, query_ctx)? {
                            Value::Text(s) => s,
                            Value::Null => String::new(),
                            v => v.to_string(),
                        }
                    } else {
                        String::new()
                    };
                    let global = flags.to_ascii_lowercase().contains('g');
                    let case_insensitive = flags.to_ascii_lowercase().contains('i');
                    let regex_pattern = if case_insensitive {
                        format!("(?i){}", pattern)
                    } else {
                        pattern
                    };
                    let re = regex::Regex::new(&regex_pattern)
                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;

                    let mut out = Vec::new();
                    if global {
                        for caps in re.captures_iter(&source) {
                            out.push(Value::Array(regexp_captures_to_values(&caps)));
                        }
                    } else if let Some(caps) = re.captures(&source) {
                        out.push(Value::Array(regexp_captures_to_values(&caps)));
                    }
                    Ok(out)
                }
                _ => Ok(Vec::new()),
            }
        }
        SrfKind::EvalFunctionArray => {
            // JSONB_OBJECT_KEYS, JSONB_ARRAY_ELEMENTS, etc. — eval_typed_expr already
            // returns the array; we just need to unpack it.
            match eval_typed_expr(expr, input, query_ctx)? {
                Value::Array(arr) => Ok(arr),
                Value::Null => Ok(Vec::new()),
                other => Ok(vec![other]),
            }
        }
    }
}

#[derive(Debug)]
pub struct ProjectOperator {
    child: BoxedOperator,
    expressions: Vec<TypedExpr>,
    #[allow(dead_code)] // preserved for EXPLAIN output
    output_names: Vec<String>,
    output_schema: TableSchema,
    opened: bool,
    /// Pre-computed SRF info: (expression_index, SrfKind) for each SRF in the projection.
    srf_indices: Vec<(usize, SrfKind)>,
    /// Buffer for SRF-expanded rows waiting to be yielded.
    srf_buffer: Vec<Row>,
}

impl ProjectOperator {
    pub fn new(
        child: BoxedOperator,
        expressions: Vec<TypedExpr>,
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
                    collation: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let srf_indices: Vec<(usize, SrfKind)> = expressions
            .iter()
            .enumerate()
            .filter_map(|(i, expr)| detect_srf(expr).map(|kind| (i, kind)))
            .collect();

        Self {
            child,
            expressions,
            output_names,
            output_schema,
            opened: false,
            srf_indices,
            srf_buffer: Vec::new(),
        }
    }

    async fn eval_expr_in_ctx(
        &self,
        expr: &TypedExpr,
        input: &Row,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<Value> {
        if needs_async(expr) {
            let materialized = ctx
                .executor
                .materialize_expr_for_row(
                    expr,
                    input,
                    ctx.outer_row.as_ref(),
                    Some(self.child.schema()),
                    ctx.txn,
                    ctx.db_id,
                    ctx.sequence_values,
                    ctx.search_path,
                    ctx.cte_tables,
                    ctx.query_ctx,
                )
                .await?;
            eval_typed_expr(&materialized, input, ctx.query_ctx)
        } else {
            eval_typed_expr(expr, input, ctx.query_ctx)
        }
    }

    async fn project_row(&self, input: &Row, ctx: &mut ExecutionContext<'_>) -> Result<Row> {
        let mut values = Vec::with_capacity(self.expressions.len());

        for expr in &self.expressions {
            let value = self.eval_expr_in_ctx(expr, input, ctx).await?;
            values.push(value);
        }

        Ok(Row::new(values))
    }

    /// Project a row that contains SRFs, returning a vector of expanded rows.
    async fn project_row_with_srf(
        &self,
        input: &Row,
        ctx: &mut ExecutionContext<'_>,
    ) -> Result<Vec<Row>> {
        // First, evaluate all expressions and collect SRF outputs.
        let mut base_values = Vec::with_capacity(self.expressions.len());
        let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

        for (i, expr) in self.expressions.iter().enumerate() {
            if let Some(&(_, kind)) = self.srf_indices.iter().find(|(idx, _)| *idx == i) {
                let outputs = eval_srf(kind, expr, input, ctx.query_ctx)?;
                srf_outputs.push((i, outputs));
                base_values.push(Value::Null); // placeholder
            } else {
                let value = self.eval_expr_in_ctx(expr, input, ctx).await?;
                base_values.push(value);
            }
        }

        if srf_outputs.is_empty() {
            return Ok(vec![Row::new(base_values)]);
        }

        // Expand: find max SRF length, create one row per position.
        let max_len = srf_outputs
            .iter()
            .map(|(_, out)| out.len())
            .max()
            .unwrap_or(0);

        if max_len == 0 {
            // All SRFs returned empty -> produce no rows (like PostgreSQL).
            return Ok(Vec::new());
        }

        let mut expanded = Vec::with_capacity(max_len);
        for i in 0..max_len {
            let mut row_values = base_values.clone();
            for (col_idx, out) in &srf_outputs {
                row_values[*col_idx] = out.get(i).cloned().unwrap_or(Value::Null);
            }
            expanded.push(Row::new(row_values));
        }

        Ok(expanded)
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
        self.srf_buffer.clear();
        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        // If we have buffered SRF-expanded rows, yield from buffer first.
        if !self.srf_buffer.is_empty() {
            return Ok(Some(self.srf_buffer.remove(0)));
        }

        if self.srf_indices.is_empty() {
            // No SRFs — fast path, same as before.
            if let Some(row) = self.child.next(ctx).await? {
                let projected = self.project_row(&row, ctx).await?;
                Ok(Some(projected))
            } else {
                Ok(None)
            }
        } else {
            // SRF path: keep fetching child rows until we get at least one expanded row.
            loop {
                let Some(row) = self.child.next(ctx).await? else {
                    return Ok(None);
                };
                let mut expanded = self.project_row_with_srf(&row, ctx).await?;
                if expanded.is_empty() {
                    continue; // SRFs returned empty, skip this input row.
                }
                if expanded.len() == 1 {
                    return Ok(Some(expanded.remove(0)));
                }
                // Buffer remaining rows, return first.
                let first = expanded.remove(0);
                self.srf_buffer = expanded;
                return Ok(Some(first));
            }
        }
    }

    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.child.close(ctx).await?;
        self.opened = false;
        self.srf_buffer.clear();
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
    use crate::sql::analyzer::types::{BinaryOp as TypedBinaryOp, TypedExpr, TypedExprKind};
    use crate::sql::expr::typed_eval::eval_typed_expr;
    use crate::sql::query_context::QueryContext;
    use crate::types::ColumnDef;

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
                    collation: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "age".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    /// Helper to create a ColumnRef TypedExpr.
    fn col_ref(index: usize, name: &str, data_type: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type,
        }
    }

    fn project_row_sync(project: &ProjectOperator, input: &Row) -> Row {
        let query_ctx = QueryContext::from_task_locals();
        let values = project
            .expressions
            .iter()
            .map(|expr| eval_typed_expr(expr, input, &query_ctx).unwrap())
            .collect();
        Row::new(values)
    }

    #[test]
    fn test_project_creation() {
        use super::super::scan::TableScanOperator;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let expressions = vec![
            col_ref(0, "id", DataType::Int32),
            col_ref(1, "name", DataType::Text),
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
            col_ref(0, "id", DataType::Int32),
            col_ref(1, "name", DataType::Text),
        ];
        let output_names = vec!["id".to_string(), "name".to_string()];
        let output_types = vec![DataType::Int32, DataType::Text];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        assert_eq!(
            project.explain_info(),
            Some("columns=[id, name]".to_string())
        );
    }

    #[test]
    fn test_project_row_column_subset() {
        use super::super::scan::TableScanOperator;
        use crate::types::Value;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let expressions = vec![col_ref(1, "name", DataType::Text)];
        let output_names = vec!["name".to_string()];
        let output_types = vec![DataType::Text];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        let input = Row::new(vec![
            Value::Int32(1),
            Value::Text("Alice".to_string()),
            Value::Int32(30),
        ]);
        let result = project_row_sync(&project, &input);

        assert_eq!(result.values.len(), 1);
        assert_eq!(result.values[0], Value::Text("Alice".to_string()));
    }

    #[test]
    fn test_project_row_arithmetic_expression() {
        use super::super::scan::TableScanOperator;
        use crate::types::Value;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let id_ref = col_ref(0, "id", DataType::Int32);
        let ten = TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(10)),
            data_type: DataType::Int32,
        };

        let expressions = vec![TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(id_ref),
                op: TypedBinaryOp::Add,
                right: Box::new(ten),
            },
            data_type: DataType::Int32,
        }];
        let output_names = vec!["id_plus_10".to_string()];
        let output_types = vec![DataType::Int32];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        let input = Row::new(vec![
            Value::Int32(5),
            Value::Text("Alice".to_string()),
            Value::Int32(30),
        ]);
        let result = project_row_sync(&project, &input);

        assert_eq!(result.values.len(), 1);
        assert_eq!(result.values[0], Value::Int32(15));
    }

    #[test]
    fn test_project_row_null_propagation() {
        use super::super::scan::TableScanOperator;
        use crate::types::Value;

        let schema = test_schema();
        let child = Box::new(TableScanOperator::new(schema));

        let expressions = vec![
            col_ref(0, "id", DataType::Int32),
            col_ref(1, "name", DataType::Text),
        ];
        let output_names = vec!["id".to_string(), "name".to_string()];
        let output_types = vec![DataType::Int32, DataType::Text];

        let project = ProjectOperator::new(child, expressions, output_names, output_types);

        let input = Row::new(vec![Value::Int32(1), Value::Null, Value::Int32(30)]);
        let result = project_row_sync(&project, &input);

        assert_eq!(result.values.len(), 2);
        assert_eq!(result.values[0], Value::Int32(1));
        assert_eq!(result.values[1], Value::Null);
    }
}
