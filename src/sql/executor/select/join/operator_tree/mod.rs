use super::super::*;
use super::table_factor::{
    collect_visible_aliases_in_table_with_joins, duplicate_column_names_lowercase,
    extract_virtual_table_filter, TransparentNestedJoinInfo,
};
use super::using_merge::{
    build_coalesce_for_merge, rewrite_for_using_join, UsingMergeColumn,
};

impl Executor {
    pub(in crate::sql::executor::select) async fn try_execute_simple_join_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Option<ExecuteResult>> {
        use crate::types::ColumnDef;

        let virtual_filter = select
            .selection
            .as_ref()
            .map(extract_virtual_table_filter)
            .unwrap_or_default();

        let resolved_selection = if let Some(sel) = &select.selection {
            Some(
                self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes)
                    .await?,
            )
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                ctes,
            )
            .await?;

        let has_lateral = select.from.iter().any(|from_item| {
            from_item
                .joins
                .iter()
                .any(|j| matches!(&j.relation, TableFactor::Derived { lateral: true, .. }))
        });

        if has_lateral {
            return self
                .execute_lateral_join(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    &resolved_projection,
                    resolved_selection.as_ref(),
                    ctes,
                )
                .await
                .map(Some);
        }

        let is_implicit_join = select.from.len() > 1;

        struct TableInfo {
            alias: String,
            schema: TableSchema,
            preloaded_rows: Option<Vec<Row>>,
        }

        let mut tables: Vec<TableInfo> = Vec::new();
        struct JoinStep {
            right_idx: usize,
            join_type: JoinType,
            condition: Option<Expr>,
        }
        let mut join_steps: Vec<JoinStep> = Vec::new();
        let mut merge_columns: Vec<UsingMergeColumn> = Vec::new();

        // Unified path: process all FROM items and their explicit JOINs.
        // For `FROM a, b JOIN c ON ...`, sqlparser gives:
        //   from[0] = {relation: a, joins: []}
        //   from[1] = {relation: b, joins: [{relation: c, ON: b.id = c.id}]}
        //
        // Explicit JOINs bind tighter than comma: `FROM a, b RIGHT JOIN c`
        // means `a CROSS (b RIGHT JOIN c)`. We process FROM items with explicit
        // JOINs first, then cross-join the standalone FROM items.
        // Phase 1: process FROM items that have explicit JOINs (they bind tighter)
        let mut transparent_nested_joins: Vec<TransparentNestedJoinInfo> = Vec::new();
        for from_item in &select.from {
            if from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                schema,
                preloaded_rows,
            });

            for join in &from_item.joins {
                let resolved_join = self
                    .resolve_join_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                        &virtual_filter,
                    )
                    .await?;
                let (alias, right_schema, right_preloaded) = match resolved_join {
                    Some(t) => t,
                    None => return Ok(None),
                };
                if let TableFactor::NestedJoin {
                    table_with_joins,
                    alias: nested_alias,
                } = &join.relation
                {
                    if nested_alias.is_none() {
                        let inner_aliases =
                            collect_visible_aliases_in_table_with_joins(table_with_joins);
                        if !inner_aliases.is_empty() {
                            transparent_nested_joins.push(TransparentNestedJoinInfo {
                                derived_alias: alias.clone(),
                                inner_aliases,
                                duplicate_cols_lower: duplicate_column_names_lowercase(
                                    &right_schema,
                                ),
                            });
                        }
                    }
                }
                let jt = JoinType::from(&join.join_operator);
                let condition = match &join.join_operator {
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(expr.clone())
                    }
                    sqlparser::ast::JoinOperator::CrossJoin => None,
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                        let mut conditions: Vec<Expr> = Vec::new();
                        for col in cols {
                            let col_name = names::normalize_ident(col);
                            let left_expr = if let Some(idx) = merge_columns
                                .iter()
                                .position(|mc| mc.col_name.eq_ignore_ascii_case(&col_name))
                            {
                                // Chained USING JOIN must compare against the *merged* key from
                                // the left relation (COALESCE semantics), not any single prior
                                // table alias. Important for OUTER JOIN chains.
                                let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                merge_columns[idx].source_aliases.push(alias.clone());
                                expr
                            } else {
                                let left_alias = tables
                                    .iter()
                                    .rev()
                                    .find(|t| {
                                        t.schema
                                            .columns
                                            .iter()
                                            .any(|c| c.name.eq_ignore_ascii_case(&col_name))
                                    })
                                    .map(|t| t.alias.clone());
                                let left_alias = match left_alias {
                                    Some(a) => a,
                                    None => return Ok(None),
                                };
                                merge_columns.push(UsingMergeColumn {
                                    col_name: col_name.clone(),
                                    source_aliases: vec![left_alias.clone(), alias.clone()],
                                });
                                Expr::CompoundIdentifier(vec![
                                    Ident::new(left_alias),
                                    Ident::new(col_name.clone()),
                                ])
                            };
                            conditions.push(Expr::BinaryOp {
                                left: Box::new(left_expr),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(alias.clone()),
                                    Ident::new(col_name),
                                ])),
                            });
                        }
                        conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                            left: Box::new(a),
                            op: BinaryOperator::And,
                            right: Box::new(b),
                        })
                    }
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Natural) => {
                        let mut common_idents: Vec<Ident> = Vec::new();
                        let mut seen = HashSet::new();
                        for right_col in &right_schema.columns {
                            let col_lower = right_col.name.to_lowercase();
                            if seen.contains(&col_lower) {
                                continue;
                            }
                            for left_table in &tables {
                                if left_table
                                    .schema
                                    .columns
                                    .iter()
                                    .any(|c| c.name.eq_ignore_ascii_case(&col_lower))
                                {
                                    common_idents.push(Ident::new(col_lower.clone()));
                                    seen.insert(col_lower.clone());
                                    break;
                                }
                            }
                        }
                        if common_idents.is_empty() {
                            None
                        } else {
                            let mut conditions: Vec<Expr> = Vec::new();
                            for col_ident in &common_idents {
                                let col_name = &col_ident.value;
                                let left_expr = if let Some(idx) = merge_columns
                                    .iter()
                                    .position(|mc| mc.col_name.eq_ignore_ascii_case(col_name))
                                {
                                    let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                    merge_columns[idx].source_aliases.push(alias.clone());
                                    expr
                                } else {
                                    let left_alias = tables
                                        .iter()
                                        .rev()
                                        .find(|t| {
                                            t.schema
                                                .columns
                                                .iter()
                                                .any(|c| c.name.eq_ignore_ascii_case(col_name))
                                        })
                                        .map(|t| t.alias.clone())
                                        .unwrap_or_else(|| tables[0].alias.clone());
                                    merge_columns.push(UsingMergeColumn {
                                        col_name: col_name.clone(),
                                        source_aliases: vec![left_alias.clone(), alias.clone()],
                                    });
                                    Expr::CompoundIdentifier(vec![
                                        Ident::new(left_alias),
                                        Ident::new(col_name.clone()),
                                    ])
                                };
                                conditions.push(Expr::BinaryOp {
                                    left: Box::new(left_expr),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(alias.clone()),
                                        Ident::new(col_name.clone()),
                                    ])),
                                });
                            }
                            conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                        }
                    }
                    _ => return Ok(None),
                };
                let idx = tables.len();
                tables.push(TableInfo {
                    alias,
                    schema: right_schema,
                    preloaded_rows: right_preloaded,
                });
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: jt,
                    condition,
                });
            }
        }

        // Phase 2: add standalone FROM items (no explicit JOINs) as CROSS JOINs
        for from_item in &select.from {
            if !from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            let idx = tables.len();
            let need_cross = idx > 0;
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                schema,
                preloaded_rows,
            });
            if need_cross {
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: JoinType::Cross,
                    condition: None,
                });
            }
        }

        if tables.len() < 2 {
            return Ok(None);
        }

        // Resolve JOIN-condition scalar subqueries.
        //
        // - Uncorrelated subqueries are resolved eagerly to literals.
        // - Correlated scalar subqueries are materialized as hidden computed columns on the
        //   referenced outer table (preloading rows if needed), and the JOIN condition is
        //   rewritten to reference that column instead of `Expr::Subquery`.
        let mut correlated_subquery_counter: usize = 0;

        fn rewrite_join_condition_subqueries<'a>(
            exec: &'a Executor,
            txn: &'a mut Transaction,
            db_id: u64,
            sequence_values: &'a mut HashMap<String, i64>,
            search_path: &'a [String],
            expr: &'a Expr,
            ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
            tables: &'a mut Vec<TableInfo>,
            counter: &'a mut usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>>
        {
            Box::pin(async move {
                match expr {
                    Expr::Subquery(subquery) => {
                        let mut referenced_aliases: Vec<String> = Vec::new();
                        for t in tables.iter() {
                            if crate::sql::executor::subquery::query_has_outer_reference(subquery, &t.alias) {
                                referenced_aliases.push(t.alias.clone());
                            }
                        }

                        if referenced_aliases.is_empty() {
                            return exec
                                .resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    ctes,
                                )
                                .await;
                        }

                        if referenced_aliases.len() > 1 {
                            return Err(anyhow!(
                                "Correlated scalar subquery in JOIN condition references multiple outer tables: {:?}",
                                referenced_aliases
                            ));
                        }

                        let outer_alias = referenced_aliases
                            .pop()
                            .unwrap_or_else(|| "outer".to_string());

                        let table_idx = tables
                            .iter()
                            .position(|t| t.alias.eq_ignore_ascii_case(&outer_alias))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Correlated scalar subquery references unknown outer table alias '{}'",
                                    outer_alias
                                )
                            })?;

                        if tables[table_idx].preloaded_rows.is_none() {
                            let (schema, rows) = exec
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &tables[table_idx].schema.name,
                                    ctes,
                                )
                                .await?;
                            tables[table_idx].schema = schema;
                            tables[table_idx].preloaded_rows = Some(rows);
                        }

                        let computed_col = format!("__tipg_subquery_{}", *counter);
                        *counter = counter.saturating_add(1);

                        let outer_schema = tables[table_idx].schema.clone();
                        let outer_rows =
                            tables[table_idx].preloaded_rows.take().unwrap_or_default();

                        let mut inferred_type: Option<DataType> = None;
                        let mut new_rows: Vec<Row> = Vec::with_capacity(outer_rows.len());
                        for row in outer_rows {
                            let val = exec
                                .eval_correlated_subquery(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    subquery,
                                    &outer_alias,
                                    &outer_schema,
                                    &row,
                                )
                                .await?;
                            if inferred_type.is_none() {
                                inferred_type = val.data_type();
                            }
                            let mut values = row.values;
                            values.push(val);
                            new_rows.push(Row::new(values));
                        }

                        tables[table_idx]
                            .schema
                            .columns
                            .push(crate::types::ColumnDef {
                                name: computed_col.clone(),
                                data_type: inferred_type.unwrap_or(DataType::Text),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            });
                        tables[table_idx].preloaded_rows = Some(new_rows);

                        Ok(Expr::CompoundIdentifier(vec![
                            Ident::new(outer_alias),
                            Ident::new(computed_col),
                        ]))
                    }
                    Expr::BinaryOp { left, op, right } => {
                        let left = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        let right = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::BinaryOp {
                            left: Box::new(left),
                            op: op.clone(),
                            right: Box::new(right),
                        })
                    }
                    Expr::UnaryOp { op, expr: inner } => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::UnaryOp {
                            op: op.clone(),
                            expr: Box::new(inner),
                        })
                    }
                    Expr::Nested(inner) => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::Nested(Box::new(inner)))
                    }
                    Expr::Function(f) => {
                        let mut args = Vec::with_capacity(f.args.len());
                        for arg in &f.args {
                            let rewritten = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        rewrite_join_condition_subqueries(
                                            exec,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            e,
                                            ctes,
                                            tables,
                                            counter,
                                        )
                                        .await?,
                                    ))
                                }
                                other => other.clone(),
                            };
                            args.push(rewritten);
                        }
                        Ok(Expr::Function(Function {
                            name: f.name.clone(),
                            args,
                            filter: f.filter.clone(),
                            null_treatment: f.null_treatment.clone(),
                            over: f.over.clone(),
                            distinct: f.distinct,
                            special: f.special,
                            order_by: f.order_by.clone(),
                        }))
                    }
                    _ => Ok(expr.clone()),
                }
            })
        }

        for step in &mut join_steps {
            if let Some(cond) = &step.condition {
                step.condition = Some(
                    rewrite_join_condition_subqueries(
                        self,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        cond,
                        ctes,
                        &mut tables,
                        &mut correlated_subquery_counter,
                    )
                    .await?,
                );
            }
        }

        let has_aggregates = projection_has_non_window_aggregate(&resolved_projection);
        let has_group_by = !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        );
        let needs_aggregation = has_aggregates || has_group_by || select.having.is_some();

        let has_distinct = select.distinct.is_some();

        // Build (alias, schema) pairs for expression rewriting, with optional alias-less NestedJoin
        // transparency mapping (Sequelize relies on inner aliases remaining visible).
        let mut table_aliases: Vec<(String, TableSchema)> = tables
            .iter()
            .map(|t| (t.alias.clone(), t.schema.clone()))
            .collect();

        let mut requires_transparent_nested_join_mapping = false;
        if !transparent_nested_joins.is_empty() {
            #[derive(Debug, Clone)]
            struct InnerAliasTarget {
                inner_alias: String,
                derived_alias: String,
                duplicate_cols_lower: HashSet<String>,
            }

            let mut inner_alias_targets: HashMap<String, InnerAliasTarget> = HashMap::new();
            let mut ambiguous_inner_aliases: HashSet<String> = HashSet::new();
            for info in &transparent_nested_joins {
                for inner_alias in &info.inner_aliases {
                    let key = inner_alias.to_lowercase();
                    if let Some(existing) = inner_alias_targets.get(&key) {
                        if !existing
                            .derived_alias
                            .eq_ignore_ascii_case(&info.derived_alias)
                        {
                            ambiguous_inner_aliases.insert(key);
                        }
                    } else {
                        inner_alias_targets.insert(
                            key,
                            InnerAliasTarget {
                                inner_alias: inner_alias.clone(),
                                derived_alias: info.derived_alias.clone(),
                                duplicate_cols_lower: info.duplicate_cols_lower.clone(),
                            },
                        );
                    }
                }
            }

            // Guard-rail: Qualified wildcard (inner_alias.*) is not transparent today because it is
            // expanded from `tables` rather than `table_aliases`.
            for item in &resolved_projection {
                if let SelectItem::QualifiedWildcard(obj, _) = item {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    if inner_alias_targets.contains_key(&qualifier.to_lowercase()) {
                        return Err(SqlError::Unsupported(format!(
                            "Qualified wildcard {}.* over nested join grouping is not supported",
                            qualifier
                        ))
                        .into());
                    }
                }
            }

            let mut used_inner_aliases: HashSet<String> = HashSet::new();

            fn visit_expr_for_inner_alias_refs(
                expr: &Expr,
                targets: &HashMap<String, InnerAliasTarget>,
                ambiguous: &HashSet<String>,
                used: &mut HashSet<String>,
            ) -> Result<()> {
                match expr {
                    Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                        let table_ref = parts[0].value.to_lowercase();
                        if targets.contains_key(&table_ref) {
                            if ambiguous.contains(&table_ref) {
                                return Err(SqlError::Unsupported(format!(
                                    "Ambiguous nested join inner alias {}",
                                    parts[0].value
                                ))
                                .into());
                            }
                            used.insert(table_ref.clone());

                            let Some(target) = targets.get(&table_ref) else {
                                return Ok(());
                            };
                            let col_lower = parts[1].value.to_lowercase();
                            if target.duplicate_cols_lower.contains(&col_lower) {
                                return Err(SqlError::Unsupported(format!(
                                    "NestedJoin materialization cannot safely resolve {}.{} because derived output contains duplicate column name {}",
                                    parts[0].value,
                                    parts[1].value,
                                    parts[1].value
                                ))
                                .into());
                            }
                        }
                        Ok(())
                    }
                    Expr::BinaryOp { left, right, .. } => {
                        visit_expr_for_inner_alias_refs(left, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(right, targets, ambiguous, used)
                    }
                    Expr::UnaryOp { expr: inner, .. }
                    | Expr::Nested(inner)
                    | Expr::IsNull(inner)
                    | Expr::IsNotNull(inner) => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::Function(f) => {
                        for arg in &f.args {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                                visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                            }
                        }
                        Ok(())
                    }
                    Expr::Cast { expr: inner, .. } | Expr::TryCast { expr: inner, .. } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::InList {
                        expr: inner, list, ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        for item in list {
                            visit_expr_for_inner_alias_refs(item, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Between {
                        expr: inner,
                        low,
                        high,
                        ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(low, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(high, targets, ambiguous, used)
                    }
                    Expr::Case {
                        operand,
                        conditions,
                        results,
                        else_result,
                    } => {
                        if let Some(op) = operand.as_deref() {
                            visit_expr_for_inner_alias_refs(op, targets, ambiguous, used)?;
                        }
                        for c in conditions {
                            visit_expr_for_inner_alias_refs(c, targets, ambiguous, used)?;
                        }
                        for r in results {
                            visit_expr_for_inner_alias_refs(r, targets, ambiguous, used)?;
                        }
                        if let Some(e) = else_result.as_deref() {
                            visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Subquery(_) | Expr::Exists { .. } => Ok(()),
                    _ => Ok(()),
                }
            }

            for step in &join_steps {
                if let Some(cond) = &step.condition {
                    visit_expr_for_inner_alias_refs(
                        cond,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(sel) = resolved_selection.as_ref() {
                visit_expr_for_inner_alias_refs(
                    sel,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            for item in &resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        visit_expr_for_inner_alias_refs(
                            expr,
                            &inner_alias_targets,
                            &ambiguous_inner_aliases,
                            &mut used_inner_aliases,
                        )?;
                    }
                    _ => {}
                }
            }
            for o in &query.order_by {
                visit_expr_for_inner_alias_refs(
                    &o.expr,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let GroupByExpr::Expressions(exprs) = &select.group_by {
                for e in exprs {
                    visit_expr_for_inner_alias_refs(
                        e,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(having) = select.having.as_ref() {
                visit_expr_for_inner_alias_refs(
                    having,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let Some(qualify) = select.qualify.as_ref() {
                visit_expr_for_inner_alias_refs(
                    qualify,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }

            for inner_key in &used_inner_aliases {
                if let Some(target) = inner_alias_targets.get(inner_key) {
                    table_aliases.push((
                        target.derived_alias.clone(),
                        TableSchema {
                            name: target.inner_alias.clone(),
                            table_id: 0,
                            columns: Vec::new(),
                            version: 1,
                            pk_constraint_name: None,
                            pk_indices: Vec::new(),
                            indexes: Vec::new(),
                            check_constraints: Vec::new(),
                            foreign_keys: Vec::new(),
                            owner: String::new(),
                        },
                    ));
                }
            }
            requires_transparent_nested_join_mapping = !used_inner_aliases.is_empty();
        }

        if tables.len() == 2
            && !is_implicit_join
            && !needs_aggregation
            && !has_distinct
            && !requires_transparent_nested_join_mapping
            && merge_columns.is_empty()
            && tables[0].preloaded_rows.is_none()
            && tables[1].preloaded_rows.is_none()
        {
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            let left_preloaded = tables[0].preloaded_rows.take();
            let right_preloaded = tables[1].preloaded_rows.take();
            let result = self
                .execute_simple_join_with_operators(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    tables[0].schema.clone(),
                    tables[1].schema.clone(),
                    &tables[0].alias,
                    &tables[1].alias,
                    join_steps[0].join_type,
                    join_steps[0].condition.clone(),
                    resolved_selection.as_ref(),
                    &query.order_by,
                    limit,
                    offset,
                    &resolved_projection,
                    left_preloaded,
                    right_preloaded,
                )
                .await?;
            return Ok(Some(result));
        }

        // Multi-JOIN: build a left-deep operator tree
        // Build combined schema incrementally
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &tables[0].schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", tables[0].alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let mut running_op: BoxedOperator = if let Some(rows) = tables[0].preloaded_rows.take() {
            Box::new(TableScanOperator::new_with_rows(
                tables[0].schema.clone(),
                rows,
            ))
        } else {
            Box::new(TableScanOperator::new(tables[0].schema.clone()))
        };

        for step in &join_steps {
            let right_preloaded_rows = tables[step.right_idx].preloaded_rows.take();
            let right = &tables[step.right_idx];
            for col in &right.schema.columns {
                combined_columns.push(ColumnDef {
                    name: format!("{}.{}", right.alias, col.name),
                    data_type: col.data_type.clone(),
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                });
            }

            let combined_schema = TableSchema {
                name: "join_result".to_string(),
                table_id: 0,
                columns: combined_columns.clone(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
            };

            let right_op: BoxedOperator = if let Some(rows) = right_preloaded_rows {
                Box::new(TableScanOperator::new_with_rows(right.schema.clone(), rows))
            } else {
                Box::new(TableScanOperator::new(right.schema.clone()))
            };

            let rewritten_condition = match &step.condition {
                Some(cond) => Some(rewrite_expr_for_multi_join(cond, &table_aliases)?),
                None => None,
            };

            // Try hash join for equi-join conditions
            let join_algo = choose_join_algorithm(
                step.condition.as_ref(),
                running_op.schema(),
                &right.schema,
                1000,
                1000,
                &HashJoinConfig::default(),
            );

            running_op = match join_algo {
                JoinAlgorithmChoice::HashJoin {
                    left_is_build,
                    left_key_indices,
                    right_key_indices,
                } => {
                    let hash_join_type = match step.join_type {
                        JoinType::Inner => HashJoinType::Inner,
                        JoinType::Left => HashJoinType::Left,
                        JoinType::Right => HashJoinType::Right,
                        JoinType::Full => HashJoinType::Full,
                        JoinType::Cross => HashJoinType::Inner,
                    };
                    Box::new(
                        HashJoinOperator::new(
                            running_op,
                            right_op,
                            hash_join_type,
                            left_key_indices,
                            right_key_indices,
                            left_is_build,
                            None,
                            HashJoinConfig::default(),
                        )
                        .with_output_schema(combined_schema),
                    )
                }
                JoinAlgorithmChoice::NestedLoop => Box::new(NestedLoopJoinOperator::with_schema(
                    running_op,
                    right_op,
                    step.join_type,
                    rewritten_condition,
                    combined_schema,
                )),
            };
        }

        if let Some(filter) = &resolved_selection {
            let rewritten = rewrite_for_using_join(filter, &table_aliases, &merge_columns)?;
            running_op = Box::new(FilterOperator::new(running_op, rewritten));
        }

        if needs_aggregation {
            return self
                .execute_join_aggregate_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    select,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let has_window_funcs = resolved_projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if has_window_funcs {
            return self
                .execute_join_window_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let final_schema = running_op.schema().clone();
        let mut rewritten_order_by: Vec<sqlparser::ast::OrderByExpr> = Vec::new();

        let wildcard_plan = if !merge_columns.is_empty() {
            let source_schemas: Vec<&TableSchema> = tables.iter().map(|t| &t.schema).collect();
            build_join_wildcard_plan(select, &source_schemas)
        } else {
            None
        };

        let source_offsets: Vec<usize> = {
            let mut offsets = Vec::with_capacity(tables.len());
            let mut offset = 0;
            for t in &tables {
                offsets.push(offset);
                offset += t.schema.columns.len();
            }
            offsets
        };

        let mut projection_exprs: Vec<Expr> = Vec::new();
        let mut alias_exprs: HashMap<String, Expr> = HashMap::new();
        for item in &resolved_projection {
            match item {
                SelectItem::Wildcard(_) => {
                    if let Some(ref plan) = wildcard_plan {
                        for wc in &plan.columns {
                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                projection_exprs.push(build_coalesce_for_merge(mc));
                            } else {
                                let combined_idx = source_offsets[wc.source_idx] + wc.col_idx;
                                projection_exprs.push(Expr::Identifier(Ident::new(
                                    final_schema.columns[combined_idx].name.clone(),
                                )));
                            }
                        }
                    } else {
                        for col in &final_schema.columns {
                            projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                        }
                    }
                }
                SelectItem::QualifiedWildcard(obj, _) => {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    let (table_idx, table) = tables
                        .iter()
                        .enumerate()
                        .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                        .ok_or_else(|| {
                            anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            )
                        })?;

                    for (col_idx, col) in table.schema.columns.iter().enumerate() {
                        if col.name.starts_with("__tipg_subquery_") {
                            continue;
                        }
                        let mc = merge_columns
                            .iter()
                            .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                        let is_merge_source = mc.as_ref().map_or(false, |mc| {
                            mc.source_aliases
                                .iter()
                                .any(|a| a.eq_ignore_ascii_case(&table.alias))
                        });
                        if let Some(mc) = mc.filter(|_| is_merge_source) {
                            projection_exprs.push(build_coalesce_for_merge(mc));
                        } else {
                            let combined_idx = source_offsets[table_idx] + col_idx;
                            projection_exprs.push(Expr::Identifier(Ident::new(
                                final_schema.columns[combined_idx].name.clone(),
                            )));
                        }
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs.push(rewrite_for_using_join(
                        expr,
                        &table_aliases,
                        &merge_columns,
                    )?);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                    alias_exprs.insert(alias.value.to_lowercase(), rewritten.clone());
                    projection_exprs.push(rewritten);
                }
            }
        }

        for o in &query.order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(e) = alias_exprs.get(&ident.value.to_lowercase()) {
                    e.clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs[pos - 1].clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else {
                rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
            };
            rewritten_order_by.push(sqlparser::ast::OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        if !rewritten_order_by.is_empty() {
            running_op = Box::new(SortOperator::new(running_op, rewritten_order_by));
        }

        let limit = extract_limit(query);
        let offset = extract_offset(query);
        if limit.is_some() || offset > 0 {
            running_op = Box::new(LimitOperator::new(running_op, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut running_op,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let has_unqualified_wildcard = resolved_projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        let has_qualified_wildcard = resolved_projection
            .iter()
            .any(|p| matches!(p, SelectItem::QualifiedWildcard(_, _)));
        let has_wildcard = has_unqualified_wildcard || has_qualified_wildcard;

        enum ProjectionSource {
            ColumnIndex(usize),
            Expr(Expr),
            CoalesceColumn(Vec<usize>),
        }

        let (columns, column_types, projected_rows) = if has_unqualified_wildcard
            && wildcard_plan.is_some()
            && !has_qualified_wildcard
        {
            let plan = wildcard_plan.as_ref().unwrap();
            let cols: Vec<String> = plan.columns.iter().map(|c| c.name.clone()).collect();
            let types: Vec<DataType> = plan.columns.iter().map(|c| c.data_type.clone()).collect();
            let projected: Vec<Row> = rows
                .iter()
                .map(|row| {
                    let values: Vec<Value> = plan
                        .columns
                        .iter()
                        .map(|wc| {
                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                for sa in &mc.source_aliases {
                                    if let Some(ti) =
                                        tables.iter().position(|t| t.alias.eq_ignore_ascii_case(sa))
                                    {
                                        if let Some(ci) = tables[ti]
                                            .schema
                                            .columns
                                            .iter()
                                            .position(|c| c.name.eq_ignore_ascii_case(&wc.name))
                                        {
                                            let idx = source_offsets[ti] + ci;
                                            if let Some(val) = row.values.get(idx) {
                                                if *val != Value::Null {
                                                    return val.clone();
                                                }
                                            }
                                        }
                                    }
                                }
                                Value::Null
                            } else {
                                let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                row.values.get(idx).cloned().unwrap_or(Value::Null)
                            }
                        })
                        .collect();
                    Row::new(values)
                })
                .collect();
            (cols, types, projected)
        } else if has_wildcard && merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                            if unqualified.starts_with("__tipg_subquery_") {
                                continue;
                            }
                            cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                            types.push(c.data_type.clone());
                            sources.push(ProjectionSource::ColumnIndex(idx));
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                        if !matched {
                            return Err(anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            ));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        if let Some(plan) = wildcard_plan.as_ref() {
                            for wc in &plan.columns {
                                cols.push(wc.name.clone());
                                types.push(wc.data_type.clone());

                                let mc = merge_columns
                                    .iter()
                                    .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                                let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                    let wc_alias = tables
                                        .get(wc.source_idx)
                                        .map(|t| t.alias.as_str())
                                        .unwrap_or("");
                                    mc.source_aliases
                                        .iter()
                                        .any(|a| a.eq_ignore_ascii_case(wc_alias))
                                });

                                if let Some(mc) = mc.filter(|_| is_merge_source) {
                                    let mut indices: Vec<usize> =
                                        Vec::with_capacity(mc.source_aliases.len());
                                    for sa in &mc.source_aliases {
                                        if let Some(ti) = tables
                                            .iter()
                                            .position(|t| t.alias.eq_ignore_ascii_case(sa))
                                        {
                                            if let Some(ci) =
                                                tables[ti].schema.columns.iter().position(|c| {
                                                    c.name.eq_ignore_ascii_case(&wc.name)
                                                })
                                            {
                                                indices.push(source_offsets[ti] + ci);
                                            }
                                        }
                                    }
                                    sources.push(ProjectionSource::CoalesceColumn(indices));
                                } else {
                                    let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                    sources.push(ProjectionSource::ColumnIndex(idx));
                                }
                            }
                        } else {
                            for (idx, c) in final_schema.columns.iter().enumerate() {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                cols.push(unqualified.to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(unqualified.to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                        if !matched {
                            return Err(anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            ));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard && !has_unqualified_wildcard && !merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let (table_idx, table) = tables
                            .iter()
                            .enumerate()
                            .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Qualified wildcard {}.* not found in join output",
                                    qualifier
                                )
                            })?;

                        for (col_idx, col) in table.schema.columns.iter().enumerate() {
                            if col.name.starts_with("__tipg_subquery_") {
                                continue;
                            }

                            cols.push(col.name.clone());
                            types.push(col.data_type.clone());

                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(&table.alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                sources.push(ProjectionSource::Expr(build_coalesce_for_merge(mc)));
                            } else {
                                let idx = source_offsets[table_idx] + col_idx;
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::Wildcard(_) => {
                        return Err(anyhow!(
                            "internal error: expected qualified wildcard handling only"
                        ));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_unqualified_wildcard {
            let cols: Vec<String> = final_schema
                .columns
                .iter()
                .map(|c| c.name.split('.').last().unwrap_or(&c.name).to_string())
                .collect();
            let types: Vec<DataType> = final_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, rows)
        } else {
            let mut rewritten_projection: Vec<SelectItem> =
                Vec::with_capacity(resolved_projection.len());
            for item in &resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        let rewritten_name =
                            get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                        if rewritten_name != original_name {
                            rewritten_projection.push(SelectItem::ExprWithAlias {
                                expr: rewritten,
                                alias: Ident::new(original_name),
                            });
                        } else {
                            rewritten_projection.push(SelectItem::UnnamedExpr(rewritten));
                        }
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        rewritten_projection.push(SelectItem::ExprWithAlias {
                            expr: rewrite_for_using_join(expr, &table_aliases, &merge_columns)?,
                            alias: alias.clone(),
                        });
                    }
                    other => rewritten_projection.push(other.clone()),
                }
            }

            let cols: Vec<String> = rewritten_projection
                .iter()
                .map(|item| get_select_item_name(item))
                .collect();

            let types: Vec<DataType> = rewritten_projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        infer_expr_type(expr, &final_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();

            fn unnest_arg_expr<'a>(expr: &'a Expr) -> Option<&'a Expr> {
                match expr {
                    Expr::Function(f) => {
                        let Some(name) = f.name.0.last() else {
                            return None;
                        };
                        if !name.value.eq_ignore_ascii_case("UNNEST") {
                            return None;
                        }
                        f.args.first().and_then(|arg| match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                            _ => None,
                        })
                    }
                    Expr::Nested(inner) => unnest_arg_expr(inner),
                    _ => None,
                }
            }

            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                let mut row_values = Vec::with_capacity(rewritten_projection.len());
                let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };

                    if let Some(arg_expr) = unnest_arg_expr(expr) {
                        let outputs = match eval_expr(arg_expr, Some(&row), Some(&final_schema))? {
                            Value::Array(arr) => arr,
                            Value::Null => Vec::new(),
                            other => vec![other],
                        };
                        srf_outputs.push((row_values.len(), outputs));
                        row_values.push(Value::Null);
                        continue;
                    }

                    let val = eval_expr(expr, Some(&row), Some(&final_schema))?;
                    row_values.push(val);
                }

                if srf_outputs.is_empty() {
                    projected.push(Row::new(row_values));
                    continue;
                }

                let max_len = srf_outputs
                    .iter()
                    .map(|(_, outputs)| outputs.len())
                    .max()
                    .unwrap_or(0);
                for idx in 0..max_len {
                    let mut expanded = row_values.clone();
                    for (col_idx, outputs) in &srf_outputs {
                        expanded[*col_idx] = outputs.get(idx).cloned().unwrap_or(Value::Null);
                    }
                    projected.push(Row::new(expanded));
                }
            }
            (cols, types, projected)
        };

        let projected_rows = if matches!(&select.distinct, Some(Distinct::Distinct)) {
            dedup_rows(projected_rows)
        } else {
            projected_rows
        };

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }

}
