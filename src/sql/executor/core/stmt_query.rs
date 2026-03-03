//! Query statement sub-dispatcher

use super::*;

impl Executor {
    pub(super) async fn execute_query_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        stmt: &Statement,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::Query(query) => {
                self.execute_query(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    current_role,
                )
                .await
            }
            Statement::ShowTables { .. } => self.execute_show_tables(txn, db_id, search_path).await,
            Statement::Explain {
                statement,
                analyze,
                verbose,
                ..
            } => {
                self.execute_explain(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    statement,
                    *analyze,
                    *verbose,
                    current_role,
                )
                .await
            }
            _ => unreachable!("Query dispatcher received non-query statement"),
        }
    }

    async fn execute_show_tables(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
    ) -> Result<ExecuteResult> {
        let current_schema = names::default_schema(search_path);
        let mut tables = Vec::new();
        for full_name in self.store.list_tables(txn, db_id).await? {
            match names::parse_full_name(&full_name) {
                Ok((schema, name)) => {
                    if schema == current_schema {
                        tables.push(name);
                    }
                }
                Err(_) => tables.push(full_name),
            }
        }
        Ok(ExecuteResult::ShowTables { tables })
    }

    async fn execute_explain(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        statement: &Statement,
        analyze: bool,
        _verbose: bool,
        current_role: Option<&str>,
    ) -> Result<ExecuteResult> {
        let (actual_rows, execution_time_ms, kv_stats) = if analyze {
            match statement {
                Statement::Query(query) => {
                    let start = Instant::now();
                    let (result, kv_stats) = with_kv_read_stats(async {
                        self.execute_query(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            query,
                            current_role,
                        )
                        .await
                    })
                    .await;
                    let result = result?;
                    let elapsed = start.elapsed();
                    let actual_rows = match result {
                        ExecuteResult::Select { rows, .. } => rows.len(),
                        _ => 0,
                    };
                    (
                        Some(actual_rows),
                        Some(elapsed.as_secs_f64() * 1000.0),
                        Some(kv_stats),
                    )
                }
                _ => {
                    return Err(anyhow!(
                        "EXPLAIN (ANALYZE) is only supported for SELECT/WITH statements"
                    ));
                }
            }
        } else {
            (None, None, None)
        };

        // For SELECT/WITH queries, EXPLAIN shares the same semantic path as execution:
        // analyze_then_rewrite_query -> optimizer -> physical plan.
        //
        // No runtime fallback is allowed here: analysis/planning errors must propagate
        // directly instead of trying an alternate planner path.
        let plan = if let Statement::Query(query) = statement {
            let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
            let analyzed = self
                .analyze_then_rewrite_query(
                    txn,
                    db_id,
                    search_path,
                    query,
                    &empty_ctes,
                    current_role,
                )
                .await?;

            // Always use the optimizer pipeline — single execution path.
            let mut planning_ctx = crate::sql::optimizer::PlanningContext::empty();
            self.prepare_planning_context(txn, db_id, &analyzed, &empty_ctes, &mut planning_ctx)
                .await?;
            let physical = crate::sql::optimizer::optimize(&analyzed, &planning_ctx)?;
            explain::physical_plan_to_plan_node(&physical)
        } else {
            // Intentional: non-SELECT EXPLAIN (DDL/DML/etc.) does not run through
            // Analyzer/Optimizer. It returns a trivial Result plan node.
            explain::PlanNode::Result {
                cost: explain::PlanCost::default(),
            }
        };
        let mut plan_text = explain::format_plan_text(&plan, 0);
        if let (Some(actual_rows), Some(execution_time_ms)) = (actual_rows, execution_time_ms) {
            use std::fmt::Write;
            writeln!(&mut plan_text, "Actual Rows: {}", actual_rows).unwrap();
            writeln!(
                &mut plan_text,
                "Execution Time: {:.3} ms",
                execution_time_ms
            )
            .unwrap();
        }
        if let Some(KvReadStatsSnapshot {
            table_scan_pairs,
            index_scan_pairs,
            batch_get_keys,
            batch_get_calls,
        }) = kv_stats
        {
            use std::fmt::Write;
            writeln!(&mut plan_text, "KV Table Scan Pairs: {}", table_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Index Scan Pairs: {}", index_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Batch Get Keys: {}", batch_get_keys).unwrap();
            writeln!(&mut plan_text, "KV Batch Get Calls: {}", batch_get_calls).unwrap();
        }

        let lines: Vec<Row> = plan_text
            .lines()
            .map(|line| Row::new(vec![Value::Text(line.to_string())]))
            .collect();

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: vec!["QUERY PLAN".to_string()],
            rows: lines,
            timezone: session_context::current_timezone(),
        })
    }
}
