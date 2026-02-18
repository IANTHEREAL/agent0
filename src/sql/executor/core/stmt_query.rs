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

        let tables = self.store.list_tables(txn, db_id).await?;
        let mut schemas_by_full: HashMap<String, TableSchema> = HashMap::new();
        let mut schemas_by_short: HashMap<String, Option<TableSchema>> = HashMap::new();
        for table_name in &tables {
            if let Ok(Some(schema)) = self.store.get_schema(txn, db_id, table_name).await {
                schemas_by_full.insert(table_name.clone(), schema.clone());

                // EXPLAIN queries often refer to tables without schema qualification.
                // Provide a short-name lookup when the name is unambiguous.
                let short = table_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(table_name.as_str())
                    .to_string();
                match schemas_by_short.get(&short) {
                    None => {
                        schemas_by_short.insert(short, Some(schema));
                    }
                    Some(Some(_)) => {
                        schemas_by_short.insert(short, None);
                    }
                    Some(None) => {}
                }
            }
        }

        let schema_lookup = |table_name: &str| -> Option<TableSchema> {
            if let Some(schema) = schemas_by_full.get(table_name) {
                return Some(schema.clone());
            }
            schemas_by_short.get(table_name).and_then(|s| s.clone())
        };

        let row_count_lookup = |_table_name: &str| -> usize { 1000 };

        // For SELECT/WITH queries, use the exact same analyze+rewrite entry as
        // execution. Non-SELECT statements use the AST trivial-plan path.
        let plan = if let Statement::Query(query) = statement {
            let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
            let (_expanded, analyzed) = self
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
            // Build PlanningContext with real table statistics and schemas,
            // identical to execution path.
            let mut planning_ctx = crate::sql::optimizer::PlanningContext::empty();
            {
                let table_refs = crate::sql::optimizer::collect_query_table_refs(&analyzed);
                let cte_names: HashSet<String> = analyzed
                    .ctes
                    .iter()
                    .map(|c| c.name.to_lowercase())
                    .collect();
                let mut stats_attempted = HashSet::new();
                for (name, schema, alias) in &table_refs {
                    let ctx_key = crate::sql::optimizer::schema_map_key(name, *alias);
                    let tid = schema.table_id;
                    let stats = if stats_attempted.insert(tid) {
                        self.get_or_load_stats(txn, db_id, tid).await?
                    } else {
                        self.stats_cache().get_full_stats(db_id, tid)
                    };
                    if let Some(stats) = stats {
                        planning_ctx.table_stats.insert(ctx_key.clone(), stats);
                    }
                    let cte_key = name.to_lowercase();
                    if !cte_names.contains(&cte_key) {
                        if let Some(table_schema) =
                            self.store().get_schema(txn, db_id, name).await?
                        {
                            planning_ctx.table_schemas.insert(ctx_key, table_schema);
                        }
                    }
                }
            }
            let physical = crate::sql::optimizer::optimize(&analyzed, &planning_ctx)?;
            explain::physical_plan_to_plan_node(&physical)
        } else {
            // Non-SELECT statements (DDL, DML) — use AST path
            // (these produce a trivial Result node)
            explain::generate_plan(statement, &schema_lookup, &row_count_lookup)
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
        }) = kv_stats
        {
            use std::fmt::Write;
            writeln!(&mut plan_text, "KV Table Scan Pairs: {}", table_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Index Scan Pairs: {}", index_scan_pairs).unwrap();
            writeln!(&mut plan_text, "KV Batch Get Keys: {}", batch_get_keys).unwrap();
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
