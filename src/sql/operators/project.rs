use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{BoxedOperator, ExecutionContext, PhysicalOperator};
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::classify::needs_async;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;

/// Identifies set-returning function kinds that expand one input row to many.
#[derive(Copy, Clone, Debug)]
pub(crate) enum SrfKind {
    Unnest,
    RegexpSplitToTable,
    RegexpMatches,
    EvalFunctionArray,
    GenerateSubscripts,
}

pub(crate) fn detect_srf(expr: &TypedExpr) -> Option<SrfKind> {
    let TypedExprKind::FunctionCall { func, .. } = &expr.kind else {
        return None;
    };
    match func.name.to_ascii_uppercase().as_str() {
        "UNNEST" => Some(SrfKind::Unnest),
        "REGEXP_SPLIT_TO_TABLE" => Some(SrfKind::RegexpSplitToTable),
        "REGEXP_MATCHES" => Some(SrfKind::RegexpMatches),
        "JSON_OBJECT_KEYS"
        | "JSONB_OBJECT_KEYS"
        | "JSON_ARRAY_ELEMENTS"
        | "JSONB_ARRAY_ELEMENTS"
        | "JSON_ARRAY_ELEMENTS_TEXT"
        | "JSONB_ARRAY_ELEMENTS_TEXT"
        | "JSON_EACH"
        | "JSONB_EACH"
        | "JSON_EACH_TEXT"
        | "JSONB_EACH_TEXT" => Some(SrfKind::EvalFunctionArray),
        "GENERATE_SUBSCRIPTS" => Some(SrfKind::GenerateSubscripts),
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

fn value_to_i64_strict(v: &Value, arg_name: &str) -> Result<i64> {
    match v {
        Value::Int32(n) => Ok(*n as i64),
        Value::Int64(n) => Ok(*n),
        other => Err(anyhow!(
            "generate_subscripts: {} argument must be integer, got {}",
            arg_name,
            other
        )),
    }
}

fn value_to_bool_strict(v: &Value, arg_name: &str) -> Result<bool> {
    match v {
        Value::Boolean(b) => Ok(*b),
        other => Err(anyhow!(
            "generate_subscripts: {} argument must be boolean, got {}",
            arg_name,
            other
        )),
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
            let (Some(arg0), Some(arg1)) = (args.first(), args.get(1)) else {
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
            let (Some(arg0), Some(arg1)) = (args.first(), args.get(1)) else {
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
        SrfKind::GenerateSubscripts => {
            // generate_subscripts(array, dim [, reverse])
            // db9 only supports 1D arrays; PG returns empty for dim != 1.
            let Some(arg0) = args.first() else {
                return Ok(Vec::new());
            };
            // PostgreSQL vector catalog types use 0-based subscripts.
            let lower_bound = match &arg0.data_type {
                DataType::UserDefined(name)
                    if name.eq_ignore_ascii_case("int2vector")
                        || name.eq_ignore_ascii_case("oidvector") =>
                {
                    0_i32
                }
                _ => 1_i32,
            };

            let arr_val = eval_typed_expr(arg0, input, query_ctx)?;
            let len = match &arr_val {
                Value::Array(arr) => arr.len(),
                Value::Null => return Ok(Vec::new()),
                _ => {
                    return Err(anyhow!(
                        "generate_subscripts: first argument must be an array"
                    ))
                }
            };
            if len == 0 {
                return Ok(Vec::new());
            }

            // PostgreSQL strict function semantics: ANY NULL argument → return no rows (empty set)
            let dim = if let Some(dim_expr) = args.get(1) {
                let dim_val = eval_typed_expr(dim_expr, input, query_ctx)?;
                if dim_val == Value::Null {
                    return Ok(Vec::new()); // NULL dim → empty set
                }
                value_to_i64_strict(&dim_val, "dimension")?
            } else {
                1 // Default when not provided
            };
            if dim != 1 {
                return Ok(Vec::new());
            }

            let reverse = if let Some(arg2) = args.get(2) {
                let reverse_val = eval_typed_expr(arg2, input, query_ctx)?;
                if reverse_val == Value::Null {
                    return Ok(Vec::new()); // NULL reverse → empty set
                }
                value_to_bool_strict(&reverse_val, "reverse")?
            } else {
                false
            };
            let start = lower_bound;
            let end = lower_bound + (len as i32) - 1;
            let indices: Vec<Value> = if reverse {
                (start..=end).rev().map(Value::Int32).collect()
            } else {
                (start..=end).map(Value::Int32).collect()
            };
            Ok(indices)
        }
    }
}

#[derive(Debug)]
pub struct ProjectOperator {
    child: BoxedOperator,
    expressions: Vec<TypedExpr>,
    #[allow(dead_code)] // framework: preserved for EXPLAIN output
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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
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
    use crate::model::{ColumnDef, Value};
    use crate::sql::analyzer::types::{
        BinaryOp as TypedBinaryOp, FunctionKind, ResolvedFunction, TypedExpr, TypedExprKind,
    };
    use crate::sql::expr::typed_eval::eval_typed_expr;
    use crate::sql::query_context::QueryContext;

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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
                ColumnDef {
                    name: "age".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                    is_dropped: false,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
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

    fn function_call(name: &str, args: Vec<TypedExpr>, return_type: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: name.to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: return_type.clone(),
                },
                args,
                order_by: vec![],
                filter: None,
            },
            data_type: return_type,
        }
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
        use crate::model::Value;

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
        use crate::model::Value;

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

    #[test]
    fn generate_subscripts_uses_zero_based_ordinals_for_int2vector() {
        let expr = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int64(1),
                        Value::Int64(0),
                    ])),
                    data_type: DataType::UserDefined("int2vector".to_string()),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
            ],
            DataType::Int32,
        );

        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &expr,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out, vec![Value::Int32(0), Value::Int32(1)]);
    }

    #[test]
    fn generate_subscripts_uses_zero_based_ordinals_for_oidvector() {
        let expr = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int64(100),
                        Value::Int64(101),
                    ])),
                    data_type: DataType::UserDefined("oidvector".to_string()),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
            ],
            DataType::Int32,
        );

        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &expr,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out, vec![Value::Int32(0), Value::Int32(1)]);
    }

    #[test]
    fn generate_subscripts_keeps_one_based_ordinals_for_regular_arrays() {
        let expr = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int32(10),
                        Value::Int32(20),
                    ])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
            ],
            DataType::Int32,
        );

        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &expr,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out, vec![Value::Int32(1), Value::Int32(2)]);
    }

    #[test]
    fn generate_subscripts_rejects_non_integer_dim() {
        let expr = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int32(10),
                        Value::Int32(20),
                    ])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("x".to_string())),
                    data_type: DataType::Text,
                },
            ],
            DataType::Int32,
        );

        let err = eval_srf(
            SrfKind::GenerateSubscripts,
            &expr,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("generate_subscripts: dimension argument must be integer"));
    }

    #[test]
    fn generate_subscripts_rejects_non_boolean_reverse() {
        let expr = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int32(10),
                        Value::Int32(20),
                    ])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
            ],
            DataType::Int32,
        );

        let err = eval_srf(
            SrfKind::GenerateSubscripts,
            &expr,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("generate_subscripts: reverse argument must be boolean"));
    }

    #[test]
    fn detect_srf_covers_supported_and_unsupported_functions() {
        let unnest = function_call("UNNEST", vec![], DataType::Text);
        assert!(matches!(detect_srf(&unnest), Some(SrfKind::Unnest)));

        let obj_keys = function_call("jsonb_object_keys", vec![], DataType::Text);
        assert!(matches!(
            detect_srf(&obj_keys),
            Some(SrfKind::EvalFunctionArray)
        ));

        let unknown = function_call("now", vec![], DataType::Timestamp);
        assert!(detect_srf(&unknown).is_none());
    }

    #[test]
    fn eval_srf_unnest_handles_missing_scalar_and_null() {
        let no_args = function_call("UNNEST", vec![], DataType::Text);
        let out = eval_srf(
            SrfKind::Unnest,
            &no_args,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());

        let scalar = function_call(
            "UNNEST",
            vec![TypedExpr {
                kind: TypedExprKind::Constant(Value::Int32(5)),
                data_type: DataType::Int32,
            }],
            DataType::Int32,
        );
        let out = eval_srf(
            SrfKind::Unnest,
            &scalar,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out, vec![Value::Int32(5)]);

        let null_arg = function_call(
            "UNNEST",
            vec![TypedExpr {
                kind: TypedExprKind::Constant(Value::Null),
                data_type: DataType::Array(Box::new(DataType::Int32)),
            }],
            DataType::Int32,
        );
        let out = eval_srf(
            SrfKind::Unnest,
            &null_arg,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn eval_srf_regexp_split_validates_arguments_and_flags() {
        let missing_args = function_call("REGEXP_SPLIT_TO_TABLE", vec![], DataType::Text);
        let err = eval_srf(
            SrfKind::RegexpSplitToTable,
            &missing_args,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("regexp_split_to_table requires at least 2 arguments"));

        let ci = function_call(
            "REGEXP_SPLIT_TO_TABLE",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("AbC".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("b".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("i".to_string())),
                    data_type: DataType::Text,
                },
            ],
            DataType::Text,
        );
        let out = eval_srf(
            SrfKind::RegexpSplitToTable,
            &ci,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(
            out,
            vec![Value::Text("A".to_string()), Value::Text("C".to_string())]
        );

        let invalid = function_call(
            "REGEXP_SPLIT_TO_TABLE",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("abc".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("[".to_string())),
                    data_type: DataType::Text,
                },
            ],
            DataType::Text,
        );
        let err = eval_srf(
            SrfKind::RegexpSplitToTable,
            &invalid,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid regex pattern"));
    }

    #[test]
    fn eval_srf_regexp_matches_supports_global_and_capture_groups() {
        let global = function_call(
            "REGEXP_MATCHES",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("ab12cd34".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("([a-z]+)(\\d+)".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("g".to_string())),
                    data_type: DataType::Text,
                },
            ],
            DataType::Array(Box::new(DataType::Text)),
        );
        let out = eval_srf(
            SrfKind::RegexpMatches,
            &global,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            Value::Array(vec![
                Value::Text("ab".to_string()),
                Value::Text("12".to_string())
            ])
        );
        assert_eq!(
            out[1],
            Value::Array(vec![
                Value::Text("cd".to_string()),
                Value::Text("34".to_string())
            ])
        );

        let no_match = function_call(
            "REGEXP_MATCHES",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("abc".to_string())),
                    data_type: DataType::Text,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Text("\\d+".to_string())),
                    data_type: DataType::Text,
                },
            ],
            DataType::Array(Box::new(DataType::Text)),
        );
        let out = eval_srf(
            SrfKind::RegexpMatches,
            &no_match,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn eval_srf_eval_function_array_non_function_expr_returns_empty() {
        let scalar = TypedExpr {
            kind: TypedExprKind::Constant(Value::Text("k".to_string())),
            data_type: DataType::Text,
        };
        let out = eval_srf(
            SrfKind::EvalFunctionArray,
            &scalar,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());

        let null = TypedExpr {
            kind: TypedExprKind::Constant(Value::Null),
            data_type: DataType::Text,
        };
        let out = eval_srf(
            SrfKind::EvalFunctionArray,
            &null,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn generate_subscripts_reverse_and_null_argument_semantics() {
        let reverse = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![
                        Value::Int32(10),
                        Value::Int32(20),
                        Value::Int32(30),
                    ])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Boolean(true)),
                    data_type: DataType::Boolean,
                },
            ],
            DataType::Int32,
        );
        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &reverse,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert_eq!(out, vec![Value::Int32(3), Value::Int32(2), Value::Int32(1)]);

        let null_dim = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![Value::Int32(10)])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Null),
                    data_type: DataType::Int32,
                },
            ],
            DataType::Int32,
        );
        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &null_dim,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());

        let null_reverse = function_call(
            "GENERATE_SUBSCRIPTS",
            vec![
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Array(vec![Value::Int32(10)])),
                    data_type: DataType::Array(Box::new(DataType::Int32)),
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Int32(1)),
                    data_type: DataType::Int32,
                },
                TypedExpr {
                    kind: TypedExprKind::Constant(Value::Null),
                    data_type: DataType::Boolean,
                },
            ],
            DataType::Int32,
        );
        let out = eval_srf(
            SrfKind::GenerateSubscripts,
            &null_reverse,
            &Row::new(vec![]),
            &QueryContext::from_task_locals(),
        )
        .unwrap();
        assert!(out.is_empty());
    }
}
